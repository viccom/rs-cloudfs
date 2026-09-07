//! The HTTP [`SyncClient`] for the cydrive-sync server (sync-lite
//! Batch B4).
//!
//! One thin transport: POST JSON to `{sync_url}/v1/push` and
//! `{sync_url}/v1/pull`, speaking the frozen wire types of the
//! `cydrive-sync` crate and mapping them field-for-field onto core's
//! sync-engine types (core deliberately does not depend on the wire
//! crate; this module is the mapping layer the plan assigns to the CLI).
//!
//! Error model: a non-2xx answer becomes [`SyncError::Client`] carrying
//! the status and a truncated response body (the server's
//! `{"error": ...}` JSON stays readable); a failed connect/send
//! becomes [`SyncError::Client`] with the URL and the underlying
//! error chain spelled out in plain text.
//!
//! Transport security: `https://` sync URLs terminate TLS in rustls
//! against the Mozilla root store (public-CA certificates trusted,
//! self-signed not — tunnel for those); `http://` is unchanged.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use cloudkit_core::sync::{SyncClient, SyncError, SyncPullResult, SyncPulledRow, SyncRowUpdate};
use cydrive_sync::wire::{
    PullRequest, PullResponse, PushRequest, PushResponse, PushRow, SubscribeEvent, SubscribeRequest,
};
use http_body_util::{BodyExt, Full};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

/// The environment variable carrying the optional family-level shared
/// secret (`None` = send none; only a server configured with
/// `SYNC_SECRET` requires one).
pub const SYNC_SECRET_ENV: &str = "CYDRIVE_SYNC_SECRET";

/// Per-request budget: 300s, covering the whole exchange — sending the
/// request, receiving the response headers and collecting the complete
/// response body (see [`HttpSyncClient::post_json`]). Wide on purpose —
/// the first full push of a large drive is one big request (tens of
/// thousands of rows), and a pull of the whole namespace likewise;
/// family-scale sync favors completing over failing fast. Anything
/// slower than this is a dead peer, not a big drive.
pub const SYNC_HTTP_TIMEOUT: Duration = Duration::from_secs(300);

/// Longest the doorbell reader waits for ANY frame — a data event, a
/// keepalive comment, even a bare blank line — before declaring the
/// stream dead: warn, close the channel, and let the caller's
/// reconnect chain heal. 90s = 4.5× the server's default 20s heartbeat,
/// so any liveness the server advertises arrives well inside the
/// budget. The heartbeat is server-configurable; if one is raised past
/// this budget the reader merely reconnects a bit more often than
/// strictly needed — an over-tight budget costs extra reconnects,
/// never correctness (events interrupt an idle window at any moment).
/// Without this deadline a half-open connection (NAT expiry, sleep
/// wake, Wi-Fi switch) parks the read forever: TCP never notices, and
/// the reconnect chain never runs (review High-3).
pub const FRAME_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Cap (characters) for response bodies quoted into error messages —
/// the truncated server answer stays diagnosable without flooding logs.
pub const ERROR_BODY_MAX_CHARS: usize = 512;

/// Depth of the channel [`HttpSyncClient::subscribe_stream`] hands out:
/// a doorbell is tiny and idempotent, so a briefly slow consumer loses
/// nothing by a full channel… except the doorbell task blocks on the
/// send, and the periodic fallback covers a consumer that stays stuck.
/// A handful of frames of slack is all the merging the model needs.
const SUBSCRIBE_EVENT_CAPACITY: usize = 16;

/// Hard cap on the doorbell reader's frame-assembly buffer (review
/// Med-1): a stream that never reaches ANY terminator — a malicious
/// slow drip, or (before this fix) a compliant CRLF stream fed to the
/// LF-only splitter — would grow `pending` without bound. Past the
/// cap the reader warns, drops the stream and lets the caller's
/// reconnect chain heal, the same self-healing exit the idle budget
/// uses. 64 KiB leaves several hundred times the headroom a doorbell
/// frame needs (a hundred-odd bytes of JSON), so the cap can only
/// trip on a broken or hostile stream, never on a healthy one.
const MAX_PENDING_FRAME_BYTES: usize = 64 * 1024;

/// Parses one complete SSE frame (its lines, without the blank-line
/// terminator) into the joined `data:` payload, per the SSE
/// field-processing rules the doorbell server speaks:
///
/// - every `data:` line contributes its value (one optional leading
///   space is stripped), multiple data lines join with `\n`;
/// - lines beginning with `:` are comments (`: keepalive`) and other
///   field names (`event:` / `id:` / `retry:`) are ignored;
/// - a frame without any data line yields `None`;
/// - a trailing `\r` is stripped (CRLF-safe).
pub fn sse_frame_data(frame: &str) -> Option<String> {
    let mut data: Vec<&str> = Vec::new();
    for line in frame.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
        // comments and other fields are ignored
    }
    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
    }
}

/// Glues the configured sync base URL and an endpoint path with exactly
/// one slash: a trailing slash on the config value (or a path prefix
/// behind a reverse proxy) must not produce `//v1/push`.
pub fn endpoint_url(base: &str, endpoint: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        endpoint.trim_start_matches('/')
    )
}

/// Maps core push updates onto wire rows, field for field.
pub fn push_wire_rows(updates: &[SyncRowUpdate]) -> Vec<PushRow> {
    updates
        .iter()
        .map(|update| PushRow {
            rel_path: update.rel_path.clone(),
            deleted: update.deleted,
            payload: update.payload.clone(),
        })
        .collect()
}

/// Maps a wire pull response onto core's pull result, field for field.
pub fn pull_core_result(response: PullResponse) -> SyncPullResult {
    SyncPullResult {
        rows: response
            .rows
            .into_iter()
            .map(|row| SyncPulledRow {
                rel_path: row.rel_path,
                version: row.version,
                deleted: row.deleted,
                payload: row.payload,
            })
            .collect(),
        max_version: response.max_version,
    }
}

/// Lossy-decodes `bytes` and caps it at `max_chars` characters, marking
/// a truncation with a trailing `"..."`.
pub fn truncate_for_log(bytes: &[u8], max_chars: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.chars().count() <= max_chars {
        text.into_owned()
    } else {
        let mut cut: String = text.chars().take(max_chars).collect();
        cut.push_str("...");
        cut
    }
}

/// Renders an error chain in plain text (`top: source: source...`) —
/// hyper-util's top-level errors say only "client error (Connect)",
/// the actionable detail (refused, DNS, timeout) lives in the sources.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// The CLI's [`SyncClient`] over real HTTP(S): one reusable hyper-util
/// legacy client pointed at the configured sync base URL. `https://`
/// URLs go through rustls with the Mozilla (webpki) root store — a
/// certificate from a public CA is verified and trusted; a self-signed
/// certificate is rejected (the documented answer for that shape is a
/// tunnel), and plain `http://` behaves exactly as before.
pub struct HttpSyncClient {
    /// Base URL with any trailing slash trimmed (see [`endpoint_url`]).
    base_url: String,
    /// The legacy client (cheap to keep for the process lifetime).
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    /// Per-request budget (see [`HttpSyncClient::with_request_timeout`]).
    request_timeout: Duration,
    /// Doorbell reader idle budget (see [`FRAME_IDLE_TIMEOUT`]) — how
    /// long one `frame()` read may park before the stream is declared
    /// dead and the channel closes for a reconnect.
    frame_idle_timeout: Duration,
    /// This instance's stable sync identity (from
    /// [`cloudkit_core::database::MetaDatabase::sync_client_id`]): rides
    /// on push/pull (the access log's client tag) and on subscribe (the
    /// doorbell's origin-skip). An empty string means "no identity" and
    /// serializes away — the anonymous pre-doorbell wire form.
    client_id: String,
}

impl HttpSyncClient {
    /// Builds the client for `sync_url` (the config layer has already
    /// required the `http://`/`https://` prefix) and `client_id` (the
    /// per-database stable identity; empty = anonymous) with the default
    /// [`SYNC_HTTP_TIMEOUT`] budget. The connector is the same plain
    /// hyper-util `HttpConnector` the http-only client used, wrapped for
    /// TLS so `https` handshakes via rustls/webpki-roots while `http`
    /// keeps the previous behavior. The inner connector drops its own
    /// scheme check (`enforce_http(false)`, mirroring hyper-rustls'
    /// `build()`) — the scheme gate lives in the wrapper, which routes
    /// https to TLS and everything else straight through.
    pub fn new(sync_url: &str, client_id: String) -> Self {
        Self::with_request_timeout(sync_url, SYNC_HTTP_TIMEOUT, client_id)
    }

    /// [`HttpSyncClient::new`] with an explicit per-request budget — the
    /// seam the slow-endpoint regression tests use to keep their drip
    /// windows seconds instead of minutes; production callers use
    /// [`HttpSyncClient::new`] and keep the wide 300s default.
    pub fn with_request_timeout(
        sync_url: &str,
        request_timeout: Duration,
        client_id: String,
    ) -> Self {
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        let connector = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        Self {
            base_url: sync_url.trim_end_matches('/').to_string(),
            client: Client::builder(TokioExecutor::new()).build(connector),
            request_timeout,
            frame_idle_timeout: FRAME_IDLE_TIMEOUT,
            client_id,
        }
    }

    /// Overrides the doorbell reader's idle budget (see
    /// [`FRAME_IDLE_TIMEOUT`]) — the seam the half-open-stream
    /// regression tests use to keep their windows sub-second;
    /// production callers keep the 90s default.
    pub fn with_frame_idle_timeout(mut self, idle: Duration) -> Self {
        self.frame_idle_timeout = idle;
        self
    }

    /// The wire form of this client's identity: `None` when anonymous
    /// (the empty string), so the field serializes away entirely — the
    /// old-server compatibility shape the `secret` field established.
    fn wire_client_id(&self) -> Option<String> {
        (!self.client_id.is_empty()).then(|| self.client_id.clone())
    }

    /// The configured base URL (trailing slash trimmed) — the doorbell
    /// task's log lines name the server they are talking to.
    pub fn url(&self) -> &str {
        &self.base_url
    }

    /// POSTs `body` (JSON) to `endpoint`, enforcing the wide request
    /// budget and returning the raw response bytes of a 2xx answer.
    ///
    /// The budget covers the *whole* exchange — sending the request,
    /// receiving the response headers, and collecting the complete
    /// response body. `client.request` resolves at the response
    /// headers; wrapping only that call left the body read unbounded,
    /// so a peer that answered headers and then trickled its body
    /// stalled a manual `cydrive sync` forever (review Low L1).
    async fn post_json(&self, endpoint: &str, body: Vec<u8>) -> Result<Bytes, SyncError> {
        let url = endpoint_url(&self.base_url, endpoint);
        let request = http::Request::builder()
            .method("POST")
            .uri(url.as_str())
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body)))
            .map_err(|error| {
                SyncError::Client(format!(
                    "building the {endpoint} request for {url}: {error}"
                ))
            })?;
        let exchange = async {
            let response = self.client.request(request).await.map_err(|error| {
                SyncError::Client(format!(
                    "reaching the sync server failed ({endpoint} to {url}): {}",
                    error_chain(&error)
                ))
            })?;
            let status = response.status();
            let bytes = response
                .into_body()
                .collect()
                .await
                .map_err(|error| {
                    SyncError::Client(format!(
                        "reading the {endpoint} response from {url}: {error}"
                    ))
                })?
                .to_bytes();
            Ok::<_, SyncError>((status, bytes))
        };
        let (status, bytes) = tokio::time::timeout(self.request_timeout, exchange)
            .await
            .map_err(|_elapsed| {
                SyncError::Client(format!(
                    "{endpoint} to {url} did not answer within {}s (the budget covers \
                     sending, the response headers and the complete response body)",
                    self.request_timeout.as_secs()
                ))
            })??;
        if !status.is_success() {
            return Err(SyncError::Client(format!(
                "{endpoint} to {url} answered HTTP {status}: {}",
                truncate_for_log(&bytes, ERROR_BODY_MAX_CHARS)
            )));
        }
        Ok(bytes)
    }

    /// Opens one SSE doorbell subscription against
    /// `POST {base}/v1/subscribe` and returns the receiver side of the
    /// parsed [`SubscribeEvent`] stream (the doorbell model: frames say
    /// only *that* something changed — the caller pulls the rows).
    /// NOT part of the core `SyncClient` trait: the core engine never
    /// subscribes, only the CLI's background task does.
    ///
    /// Lifecycle: the connect phase (request through response headers)
    /// is bounded by the same per-request budget as push/pull; a non-2xx
    /// answer (e.g. the secret gate's 403) is collected and returned as
    /// a plain-words [`SyncError::Client`]. On success a reader task
    /// owns the body: complete frames are split on the blank-line
    /// terminator (buffering across TCP chunk boundaries), `data:`
    /// payloads are JSON-parsed onto the channel, comment/keepalive
    /// frames are ignored, and a malformed data line warns and is
    /// SKIPPED — a bad frame must never drop the connection. Each
    /// read is bounded by the idle budget ([`FRAME_IDLE_TIMEOUT`]): a
    /// stream silent past it — the half-open shape TCP never notices —
    /// is declared dead and the reader exits like any stream error.
    /// The assembly buffer is likewise bounded (64 KiB, review Med-1):
    /// a stream with no frame terminator in sight is dropped the same
    /// way instead of growing the buffer without bound.
    /// The receiver closes when the stream ends or errors (body EOF,
    /// read error, idle budget) or when this side drops it (the next
    /// send then ends the reader task); the caller treats a closed
    /// channel as "reconnect".
    pub async fn subscribe_stream(
        &self,
        key: &str,
        secret: Option<&str>,
    ) -> Result<tokio::sync::mpsc::Receiver<SubscribeEvent>, SyncError> {
        let endpoint = "/v1/subscribe";
        let url = endpoint_url(&self.base_url, endpoint);
        let request = SubscribeRequest {
            key: key.to_string(),
            secret: secret.map(str::to_string),
            // Identify ourselves so the server never rings our own bell
            // (the origin-skip); anonymous subscribes hear everything,
            // which is merely a redundant harmless pass.
            client_id: self.wire_client_id(),
        };
        let body = serde_json::to_vec(&request).map_err(|error| {
            SyncError::Client(format!("serializing the subscribe request: {error}"))
        })?;
        let request = http::Request::builder()
            .method("POST")
            .uri(url.as_str())
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(Full::new(Bytes::from(body)))
            .map_err(|error| {
                SyncError::Client(format!(
                    "building the {endpoint} request for {url}: {error}"
                ))
            })?;

        // Headers-only budget: the body below is a long-lived stream and
        // must NOT fall under a timeout — only the handshake may hang.
        let response = tokio::time::timeout(self.request_timeout, self.client.request(request))
            .await
            .map_err(|_elapsed| {
                SyncError::Client(format!(
                    "{endpoint} to {url} did not answer within {}s (the budget covers the \
                     subscribe handshake, not the stream itself)",
                    self.request_timeout.as_secs()
                ))
            })?
            .map_err(|error| {
                SyncError::Client(format!(
                    "subscribing the doorbell at {url} failed: {}",
                    error_chain(&error)
                ))
            })?;
        let status = response.status();
        if !status.is_success() {
            let bytes = response
                .into_body()
                .collect()
                .await
                .map_err(|error| {
                    SyncError::Client(format!("reading the rejected {endpoint} answer: {error}"))
                })?
                .to_bytes();
            return Err(SyncError::Client(format!(
                "{endpoint} to {url} was rejected with HTTP {status}: {}",
                truncate_for_log(&bytes, ERROR_BODY_MAX_CHARS)
            )));
        }

        let mut stream = response.into_body();
        let idle = self.frame_idle_timeout;
        let (tx, rx) = tokio::sync::mpsc::channel(SUBSCRIBE_EVENT_CAPACITY);
        tokio::spawn(async move {
            // Frame assembly buffer: TCP may split or coalesce SSE frames
            // arbitrarily, so bytes accumulate until a blank-line
            // terminator completes a frame.
            let mut pending: Vec<u8> = Vec::new();
            loop {
                // Liveness deadline (review High-3): a half-open
                // connection never errors and never EOFs — without this
                // budget the read below parks forever and the reconnect
                // chain never runs. Any frame (data or keepalive
                // comment) resets the clock, so a heartbeat stream never
                // trips it; the deadline only ends streams that have
                // gone silent well past their heartbeat.
                let frame = match tokio::time::timeout(idle, stream.frame()).await {
                    Ok(Some(Ok(frame))) => frame,
                    Ok(Some(Err(error))) => {
                        tracing::warn!(
                            %error,
                            "the sync doorbell stream failed; the subscriber will reconnect"
                        );
                        break;
                    }
                    Ok(None) => {
                        tracing::debug!("the sync doorbell stream ended; reconnecting");
                        break;
                    }
                    Err(_elapsed) => {
                        tracing::warn!(
                            "no SSE frame within {}s (idle/dead stream), reconnecting",
                            idle.as_secs_f64()
                        );
                        break;
                    }
                };
                let Some(chunk) = frame.data_ref() else {
                    continue; // trailers and other non-data frames
                };
                pending.extend_from_slice(chunk);
                // Bound the assembly buffer (Med-1): without a cap a
                // stream that never reaches a terminator grows
                // `pending` forever; past the cap the stream is broken
                // or hostile — drop it and let the reconnect chain
                // heal, the same exit the idle budget above uses.
                if pending.len() > MAX_PENDING_FRAME_BYTES {
                    tracing::warn!(
                        bytes = pending.len(),
                        "no complete SSE frame within {} KiB, dropping the doorbell stream \
                         (the subscriber will reconnect)",
                        MAX_PENDING_FRAME_BYTES / 1024
                    );
                    break;
                }
                while let Some((content_len, term_len)) = find_frame_terminator(&pending) {
                    let frame_bytes: Vec<u8> = pending.drain(..content_len + term_len).collect();
                    let frame_text = String::from_utf8_lossy(&frame_bytes[..content_len]);
                    let Some(data) = sse_frame_data(&frame_text) else {
                        continue; // comment / non-data frame (keepalive)
                    };
                    match serde_json::from_str::<SubscribeEvent>(&data) {
                        Ok(event) => {
                            if tx.send(event).await.is_err() {
                                // Receiver dropped: the subscriber went
                                // away; end the reader (nothing to deliver).
                                return;
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                frame = %data,
                                "a malformed doorbell frame was skipped (the stream stays open)"
                            );
                        }
                    }
                }
            }
            // `tx` drops here: the receiver observes a closed channel and
            // reconnects.
        });
        Ok(rx)
    }
}

/// The first complete frame terminator in `buf`, as
/// `(content_len, term_len)`: the frame's lines are
/// `buf[..content_len]` and the blank-line terminator itself spans
/// `term_len` more bytes (consume `content_len + term_len` in total).
/// Per the SSE spec a line may end in CRLF, LF or CR, so a blank
/// line — the frame terminator — is any of `\r\n\r\n`, `\n\n` or
/// `\r\r` (review Med-1: an LF-only window never fires on the other
/// two, so a compliant CRLF stream buffered forever); the FIRST
/// complete one wins. `None` while no full frame has arrived.
/// Mixed terminators across the two lines (`\r\n\n`, `\n\r\n`) are
/// not recognized: the doorbell server — like every SSE stack in the
/// chain — emits one consistent line ending.
fn find_frame_terminator(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        match buf[i] {
            // LF line ending: the frame completes when the next line
            // is empty too (another bare LF).
            b'\n' => {
                if buf[i + 1] == b'\n' {
                    return Some((i, 2));
                }
                i += 1;
            }
            // A CR opening a CRLF line ending.
            b'\r' if buf[i + 1] == b'\n' => {
                if buf[i..].starts_with(b"\r\n\r\n") {
                    return Some((i, 4));
                }
                i += 2; // an in-frame CRLF line ending
            }
            // A bare-CR line ending: two in a row terminate a frame.
            b'\r' => {
                if buf[i + 1] == b'\r' {
                    return Some((i, 2));
                }
                i += 1; // an in-frame bare-CR line ending
            }
            _ => i += 1,
        }
    }
    None
}

#[async_trait]
impl SyncClient for HttpSyncClient {
    async fn push(
        &self,
        key: &str,
        secret: Option<&str>,
        rows: &[SyncRowUpdate],
    ) -> Result<i64, SyncError> {
        let request = PushRequest {
            key: key.to_string(),
            secret: secret.map(str::to_string),
            // Who is pushing — the doorbell's `origin`. The server skips
            // ringing the pusher's own subscription, so this same-machine
            // echo suppression keys on exactly this field.
            client_id: self.wire_client_id(),
            rows: push_wire_rows(rows),
        };
        let body = serde_json::to_vec(&request)
            .map_err(|error| SyncError::Client(format!("serializing the push request: {error}")))?;
        let bytes = self.post_json("/v1/push", body).await?;
        let response: PushResponse = serde_json::from_slice(&bytes).map_err(|error| {
            SyncError::Client(format!(
                "decoding the push response: {error}: {}",
                truncate_for_log(&bytes, ERROR_BODY_MAX_CHARS)
            ))
        })?;
        Ok(response.max_version)
    }

    async fn pull(
        &self,
        key: &str,
        secret: Option<&str>,
        since: i64,
    ) -> Result<SyncPullResult, SyncError> {
        let request = PullRequest {
            key: key.to_string(),
            since,
            // The server gates pull behind the same shared secret as
            // push; `None` serializes the field away entirely, keeping a
            // secretless pull byte-identical to the pre-secret wire form.
            secret: secret.map(str::to_string),
            // Same doorbell-batch optional field as push: access-log
            // correlation only, omitted entirely when anonymous so the
            // wire bytes stay the pre-doorbell form.
            client_id: self.wire_client_id(),
        };
        let body = serde_json::to_vec(&request)
            .map_err(|error| SyncError::Client(format!("serializing the pull request: {error}")))?;
        let bytes = self.post_json("/v1/pull", body).await?;
        let response: PullResponse = serde_json::from_slice(&bytes).map_err(|error| {
            SyncError::Client(format!(
                "decoding the pull response: {error}: {}",
                truncate_for_log(&bytes, ERROR_BODY_MAX_CHARS)
            ))
        })?;
        Ok(pull_core_result(response))
    }
}

#[cfg(test)]
mod terminator_tests {
    use super::find_frame_terminator;

    #[test]
    fn lf_frames_keep_the_previous_cut() {
        // The pre-Med-1 splitter cut at the first `\n` of a `\n\n`
        // window; (content_len, 2) keeps byte-identical behavior for
        // pure-LF streams.
        assert_eq!(find_frame_terminator(b"data: x\n\n"), Some((7, 2)));
        assert_eq!(find_frame_terminator(b"a\n\nb\n\n"), Some((1, 2)));
        assert_eq!(find_frame_terminator(b""), None);
        assert_eq!(find_frame_terminator(b"\n"), None);
        assert_eq!(find_frame_terminator(b"no terminator"), None);
        // a trailing lone LF waits for more bytes
        assert_eq!(find_frame_terminator(b"data: x\n"), None);
    }

    #[test]
    fn crlf_frames_terminate() {
        assert_eq!(find_frame_terminator(b"data: x\r\n\r\n"), Some((7, 4)));
        // in-frame CRLF line endings do not terminate
        assert_eq!(find_frame_terminator(b"data: a\r\ndata: b\r\n"), None);
        // a trailing CRLF waits for the blank second line
        assert_eq!(find_frame_terminator(b"data: x\r\n\r"), None);
        // the first complete terminator wins
        assert_eq!(find_frame_terminator(b"a\r\n\r\nb\r\n\r\n"), Some((1, 4)));
    }

    #[test]
    fn cr_only_frames_terminate() {
        assert_eq!(find_frame_terminator(b"data: x\r\r"), Some((7, 2)));
        // a single bare CR is an in-frame line ending
        assert_eq!(find_frame_terminator(b"data: a\rdata: b\r"), None);
    }
}
