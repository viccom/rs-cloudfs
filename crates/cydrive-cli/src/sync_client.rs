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
use cydrive_core::sync::{SyncClient, SyncError, SyncPullResult, SyncPulledRow, SyncRowUpdate};
use cydrive_sync::wire::{PullRequest, PullResponse, PushRequest, PushResponse, PushRow};
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

/// Cap (characters) for response bodies quoted into error messages —
/// the truncated server answer stays diagnosable without flooding logs.
pub const ERROR_BODY_MAX_CHARS: usize = 512;

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
}

impl HttpSyncClient {
    /// Builds the client for `sync_url` (the config layer has already
    /// required the `http://`/`https://` prefix) with the default
    /// [`SYNC_HTTP_TIMEOUT`] budget. The connector is the
    /// same plain hyper-util `HttpConnector` the http-only client used,
    /// wrapped for TLS so `https` handshakes via rustls/webpki-roots
    /// while `http` keeps the previous behavior. The inner connector
    /// drops its own scheme check (`enforce_http(false)`, mirroring
    /// hyper-rustls' `build()`) — the scheme gate lives in the wrapper,
    /// which routes https to TLS and everything else straight through.
    pub fn new(sync_url: &str) -> Self {
        Self::with_request_timeout(sync_url, SYNC_HTTP_TIMEOUT)
    }

    /// [`HttpSyncClient::new`] with an explicit per-request budget —
    /// the seam the slow-endpoint regression tests use to keep their
    /// drip windows seconds instead of minutes; production callers use
    /// [`HttpSyncClient::new`] and keep the wide 300s default.
    pub fn with_request_timeout(sync_url: &str, request_timeout: Duration) -> Self {
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
        }
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
            // The SSE doorbell batch made client_id optional on the
            // wire; this client does not send one yet (client-side
            // subscribing is a later batch).
            client_id: None,
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
            // Same doorbell-batch optional field as push; not sent yet.
            client_id: None,
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
