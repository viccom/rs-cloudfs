//! End-to-end for the SSE doorbell (`POST /v1/subscribe`): the server
//! notifies subscribers only THAT something changed (`max_version` +
//! the pusher's `client_id` as `origin`) — subscribers pull the data
//! themselves, so push/pull stays the single source of truth.
//!
//! Covered (approved doorbell contract):
//! - subscribe -> another client pushes -> the subscriber reads a
//!   `data: {"max_version":N,"origin":...}` frame; a push without a
//!   client_id delivers `origin: null`
//! - origin skip: a subscriber with the SAME client_id as the pusher
//!   receives nothing (a positive-control subscriber proves the push
//!   really did broadcast — the assertion window opens only after the
//!   control subscriber's frame arrived)
//! - a subscriber without a client_id is never skipped
//! - secret gate: missing/wrong secret -> 403 JSON whose response
//!   *completes* (no hung stream); malformed JSON -> 400
//! - heartbeat: `: keepalive` comment frames at the injected interval
//! - disconnect: dropping the connection reaps the per-namespace
//!   broadcast registry entry (no leak)
//! - dual-active client_id (the copied-db accident, review Med-2):
//!   two subscribers sharing one id BOTH receive a push with that
//!   shared origin (delivered with `origin: null` so the client-side
//!   skip cannot re-silence it), the subscription that creates the
//!   duplicate logs a warn, and the self-origin skip returns the
//!   moment one of the two disconnects
//! - response headers: text/event-stream + no-cache +
//!   X-Accel-Buffering: no (nginx/openresty must not buffer SSE)

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use bytes::Bytes;
use futures_util::{StreamExt as _, TryStreamExt as _};
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use cydrive_sync::events::EventHub;
use cydrive_sync::router::router_with_hub;
use cydrive_sync::store::SyncStore;

/// A heartbeat long enough that keepalive frames never interleave with
/// the data frames a test asserts on.
const QUIET_HEARTBEAT: Duration = Duration::from_secs(60);

/// Spawns the real router on `127.0.0.1:0` with an injectable
/// heartbeat and an observable broadcast hub; returns (address, hub).
async fn spawn(secret: Option<&str>, heartbeat: Duration) -> (SocketAddr, Arc<EventHub>) {
    let store = Arc::new(SyncStore::open_in_memory().expect("in-memory store"));
    let hub = Arc::new(EventHub::new());
    let app = router_with_hub(store, secret.map(str::to_string), hub.clone(), heartbeat);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind random loopback port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("accept loop");
    });
    (addr, hub)
}

/// POSTs raw JSON and collects the complete response — for push, and
/// for subscribe answers that must NOT turn into streams (403/400).
async fn post_collect(addr: SocketAddr, path: &str, body: &str) -> (StatusCode, HeaderMap, Bytes) {
    let client: Client<_, http_body_util::Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build_http();
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{addr}{path}"))
        .header("content-type", "application/json")
        .body(http_body_util::Full::new(Bytes::from(body.to_string())))
        .expect("request build");
    let response = client.request(request).await.expect("http roundtrip");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes();
    (status, headers, bytes)
}

/// Pushes one row batch and asserts it was accepted.
async fn push_ok(addr: SocketAddr, body: &str) {
    let (status, _, bytes) = post_collect(addr, "/v1/push", body).await;
    assert_eq!(status, StatusCode::OK, "push failed: {bytes:?}");
}

/// Reads complete SSE frames (delimited by the blank-line terminator)
/// out of a streaming response body, buffering across chunk
/// boundaries — TCP may split or coalesce frames arbitrarily.
struct SseReader {
    stream: Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, String>> + Send>>,
    buf: Vec<u8>,
}

impl SseReader {
    /// Next complete frame including its `\n\n` terminator; `None`
    /// once the server closes the stream. Cancellation-safe: bytes
    /// read before an outer timeout stay buffered.
    async fn next_frame(&mut self) -> Option<String> {
        loop {
            if let Some(pos) = self.buf.windows(2).position(|window| window == b"\n\n") {
                let frame: Vec<u8> = self.buf.drain(..pos + 2).collect();
                return Some(String::from_utf8_lossy(&frame).into_owned());
            }
            match self.stream.next().await {
                Some(Ok(chunk)) => self.buf.extend_from_slice(&chunk),
                Some(Err(error)) => panic!("sse body read failed: {error}"),
                None => return None,
            }
        }
    }
}

/// Opens a subscription and returns once the response HEADERS have
/// arrived — the handler registers the hub receiver before returning,
/// so a returned subscription is guaranteed to be receiving.
async fn subscribe(addr: SocketAddr, body: &str) -> (StatusCode, HeaderMap, SseReader) {
    let client: Client<_, http_body_util::Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build_http();
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{addr}/v1/subscribe"))
        .header("content-type", "application/json")
        .body(http_body_util::Full::new(Bytes::from(body.to_string())))
        .expect("request build");
    let response = client.request(request).await.expect("subscribe roundtrip");
    let status = response.status();
    let headers = response.headers().clone();
    let stream = response
        .into_body()
        .into_data_stream()
        .map_err(|error| error.to_string());
    let reader = SseReader {
        stream: Box::pin(stream),
        buf: Vec::new(),
    };
    (status, headers, reader)
}

/// A push with a client_id rings every subscriber with that origin;
/// a push without one rings with `origin: null`. The SSE response
/// headers must also carry the proxy-proof trio.
#[tokio::test]
async fn subscribe_receives_doorbell_events_after_push() {
    let (addr, _hub) = spawn(None, QUIET_HEARTBEAT).await;
    let (status, headers, mut reader) =
        subscribe(addr, r#"{"key":"ns","client_id":"watcher"}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("content-type"),
        Some(&HeaderValue::from_static("text/event-stream")),
        "headers: {headers:?}"
    );
    assert_eq!(
        headers.get("cache-control"),
        Some(&HeaderValue::from_static("no-cache")),
        "headers: {headers:?}"
    );
    assert_eq!(
        headers.get("x-accel-buffering"),
        Some(&HeaderValue::from_static("no")),
        "nginx/openresty buffer proxied SSE unless told not to — \
         deployment-critical header: {headers:?}"
    );

    // another client pushes one row -> the subscriber hears the doorbell
    push_ok(
        addr,
        r#"{"key":"ns","client_id":"pusher","rows":[
            {"rel_path":"/a","deleted":false,"payload":"x"}]}"#,
    )
    .await;
    let frame = reader
        .next_frame()
        .await
        .expect("doorbell frame after push");
    assert_eq!(frame, "data: {\"max_version\":1,\"origin\":\"pusher\"}\n\n");

    // a push without a client_id doorbells with origin: null
    push_ok(
        addr,
        r#"{"key":"ns","rows":[
            {"rel_path":"/b","deleted":false,"payload":"y"}]}"#,
    )
    .await;
    let frame = reader
        .next_frame()
        .await
        .expect("doorbell frame after clientless push");
    assert_eq!(frame, "data: {\"max_version\":2,\"origin\":null}\n\n");
}

/// Origin skip: the subscriber whose client_id equals the push origin
/// hears nothing. The positive-control subscriber (different
/// client_id, subscribed before the push) proves the broadcast really
/// happened — only then does the "heard nothing" window open.
#[tokio::test]
async fn origin_skip_same_client_id_hears_nothing() {
    let (addr, _hub) = spawn(None, QUIET_HEARTBEAT).await;

    // the subscriber under test, then the positive control
    let (status, _, mut echo_subscriber) =
        subscribe(addr, r#"{"key":"ns","client_id":"laptop-1"}"#).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, mut control) =
        subscribe(addr, r#"{"key":"ns","client_id":"other-machine"}"#).await;
    assert_eq!(status, StatusCode::OK);

    push_ok(
        addr,
        r#"{"key":"ns","client_id":"laptop-1","rows":[
            {"rel_path":"/a","deleted":false,"payload":"x"}]}"#,
    )
    .await;

    // positive control: the push really did broadcast
    let frame = control
        .next_frame()
        .await
        .expect("control subscriber must hear the push");
    assert!(
        frame.contains("\"origin\":\"laptop-1\""),
        "control frame should carry the pusher's client_id: {frame:?}"
    );

    // the same-client_id subscriber must hear nothing in the window
    let nothing =
        tokio::time::timeout(Duration::from_millis(500), echo_subscriber.next_frame()).await;
    assert!(
        nothing.is_err(),
        "a subscriber with the pusher's own client_id must not receive its own \
         event (self-origin skip), got: {nothing:?}"
    );
}

/// A subscriber that did not identify itself is never skipped — the
/// skip compares client_ids, and an anonymous subscriber has none to
/// match (receiving a foreign-origin doorbell is harmless: the pull
/// that follows is idempotent).
#[tokio::test]
async fn subscriber_without_client_id_is_not_skipped() {
    let (addr, _hub) = spawn(None, QUIET_HEARTBEAT).await;
    let (status, _, mut anonymous) = subscribe(addr, r#"{"key":"ns"}"#).await;
    assert_eq!(status, StatusCode::OK);

    push_ok(
        addr,
        r#"{"key":"ns","client_id":"someone","rows":[
            {"rel_path":"/a","deleted":false,"payload":"x"}]}"#,
    )
    .await;

    let frame = anonymous
        .next_frame()
        .await
        .expect("a client_id-less subscriber must still hear every event");
    assert!(
        frame.contains("\"origin\":\"someone\""),
        "frame should carry the pusher's client_id: {frame:?}"
    );
}

/// The secret gate covers subscribe exactly like push/pull: missing or
/// wrong secret -> 403 `{"error": ...}` — and the response COMPLETES
/// (`collect()` finishes), proving no stream was opened. Malformed
/// JSON -> 400 like the other endpoints.
#[tokio::test]
async fn secret_gate_rejects_subscribe_without_hanging() {
    let (addr, _hub) = spawn(Some("s3cret"), QUIET_HEARTBEAT).await;

    for body in [r#"{"key":"ns"}"#, r#"{"key":"ns","secret":"nope"}"#] {
        let (status, headers, bytes) = post_collect(addr, "/v1/subscribe", body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}: {bytes:?}");
        let error: serde_json::Value = serde_json::from_slice(&bytes).expect("403 body is JSON");
        assert!(
            error.get("error").is_some_and(|v| v.is_string()),
            "{body}: {bytes:?}"
        );
        assert_ne!(
            headers.get("content-type"),
            Some(&HeaderValue::from_static("text/event-stream")),
            "a rejected subscribe must not answer with an event stream: {headers:?}"
        );
    }

    // malformed body -> 400 like the other endpoints
    let (status, _, bytes) = post_collect(addr, "/v1/subscribe", "{not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{bytes:?}");
}

/// At an injected 100 ms heartbeat the stream emits `: keepalive`
/// comment frames — byte-exact, repeatedly.
#[tokio::test]
async fn heartbeat_sends_keepalive_comment_frames() {
    let (addr, _hub) = spawn(None, Duration::from_millis(100)).await;
    let (status, _, mut reader) = subscribe(addr, r#"{"key":"ns"}"#).await;
    assert_eq!(status, StatusCode::OK);

    for attempt in 0..2 {
        let frame = tokio::time::timeout(Duration::from_secs(2), reader.next_frame())
            .await
            .unwrap_or_else(|_| panic!("keepalive #{attempt} within 2s at a 100ms heartbeat"))
            .expect("stream stays open");
        assert_eq!(frame, ": keepalive\n\n", "keepalive #{attempt}");
    }
}

/// Registry hygiene: a subscribe connection registers exactly one
/// entry; dropping the connection (raw TCP, so the disconnect is
/// unambiguous — a pooled client would muddy who hung up) reaps it,
/// and a later push into the reaped namespace must not resurrect an
/// entry (publishing with no receivers is a no-op, not an error).
#[tokio::test]
async fn dropping_the_connection_reaps_the_registry_entry() {
    let (addr, hub) = spawn(None, QUIET_HEARTBEAT).await;

    let mut socket = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let body = r#"{"key":"ns"}"#;
    let request = format!(
        "POST /v1/subscribe HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(request.as_bytes())
        .await
        .expect("write subscribe request");
    let mut head = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let read = socket.read(&mut chunk).await.expect("read response head");
        assert!(read > 0, "eof before response headers arrived");
        head.extend_from_slice(&chunk[..read]);
        if head.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "expected the subscribe stream to open, head: {head}"
    );
    assert!(head.contains("text/event-stream"), "head: {head}");

    // headers received => the receiver is registered
    assert_eq!(hub.receiver_count("ns"), 1);
    assert_eq!(hub.channel_count(), 1);

    // abrupt disconnect
    drop(socket);

    // the entry must be reaped once hyper notices the eof and drops
    // the stream (whose guard removes the idle entry)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while hub.channel_count() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "registry entry was not reaped after the subscriber disconnected"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(hub.receiver_count("ns"), 0);

    // publishing into a reaped namespace is a no-op broadcast
    push_ok(
        addr,
        r#"{"key":"ns","rows":[
            {"rel_path":"/a","deleted":false,"payload":"x"}]}"#,
    )
    .await;
    assert_eq!(
        hub.channel_count(),
        0,
        "publishing must not resurrect a reaped entry"
    );
}

// === Dual-active client_id (review P3, Med-2) =========================
//
// A client_id lives in the client's db; copying that db to a second
// machine makes BOTH machines subscribe with the same id, and the
// plain origin skip — server pump AND client — then silences the
// doorbell in both directions (every push carries the shared id as
// its origin, so each side skips). The tests below pin the
// server-side ruling: while two live subscriptions share an id, its
// self-origin events ARE delivered (origin rewritten to `null` so
// the client-side skip cannot re-silence them), the anomaly is
// warned about, and the single-active skip returns as soon as one of
// the two disconnects.

/// Shared buffer the warn-capture subscriber writes into (installed
/// at most once per test process — `set_global_default` is
/// process-global).
static CAPTURED_LOGS: OnceLock<Arc<Mutex<String>>> = OnceLock::new();

/// A `MakeWriter` that appends into [`CAPTURED_LOGS`].
struct LogWriter(Arc<Mutex<String>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log capture buffer")
            .push_str(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Installs (once) a WARN-and-above capturing subscriber and returns
/// its buffer — the dual-active warn assertion reads it. In-process
/// capture instead of spawning the real binary (the `logging.rs`
/// form) because these tests must drive several long-lived SSE
/// streams against one hub; the other tests never assert on logs, so
/// a process-global default is harmless to them.
fn captured_warns() -> Arc<Mutex<String>> {
    CAPTURED_LOGS
        .get_or_init(|| {
            let buffer: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
            let writer_buffer = Arc::clone(&buffer);
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || LogWriter(writer_buffer.clone()))
                .try_init()
                .expect("install the warn-capture subscriber once per test process");
            buffer
        })
        .clone()
}

/// Med-2: two subscribers share one client_id (the copied-db
/// accident); a push with that shared id as origin must reach BOTH —
/// with `origin` rewritten to `null`, because the client-side skip
/// compares the origin against its own id and would otherwise
/// re-silence exactly this delivery.
#[tokio::test]
async fn duplicate_client_id_subscribers_both_receive_doorbell() {
    let (addr, _hub) = spawn(None, QUIET_HEARTBEAT).await;

    let (status, _, mut first) = subscribe(addr, r#"{"key":"ns","client_id":"laptop-1"}"#).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, mut second) = subscribe(addr, r#"{"key":"ns","client_id":"laptop-1"}"#).await;
    assert_eq!(status, StatusCode::OK);

    // either machine pushing carries the shared id as the origin
    push_ok(
        addr,
        r#"{"key":"ns","client_id":"laptop-1","rows":[
            {"rel_path":"/a","deleted":false,"payload":"x"}]}"#,
    )
    .await;

    for (who, reader) in [("first", &mut first), ("second", &mut second)] {
        let frame = tokio::time::timeout(Duration::from_secs(2), reader.next_frame())
            .await
            .unwrap_or_else(|_| {
                panic!("dual-active subscriber ({who}) must receive the shared-origin doorbell")
            })
            .expect("stream stays open");
        assert_eq!(
            frame, "data: {\"max_version\":1,\"origin\":null}\n\n",
            "the origin must be rewritten to null so the client-side skip \
             cannot re-silence a dual-active id"
        );
    }
}

/// The dual-active accident must be diagnosable: the subscription
/// that brings one (namespace, client_id) to two live subscribers
/// logs a WARN naming the likely cause — correlation fields are 8-char
/// prefixes, never whole identifiers.
#[tokio::test]
async fn duplicate_client_id_subscription_warns() {
    let captured = captured_warns();
    let (addr, _hub) = spawn(None, QUIET_HEARTBEAT).await;

    let (status, _, _first) =
        subscribe(addr, r#"{"key":"namespace-one","client_id":"laptop-one"}"#).await;
    assert_eq!(status, StatusCode::OK);
    let before = captured.lock().expect("capture buffer").len();
    let (status, _, _second) =
        subscribe(addr, r#"{"key":"namespace-one","client_id":"laptop-one"}"#).await;
    assert_eq!(status, StatusCode::OK);

    // the warn is emitted synchronously while registering the second
    // subscription, i.e. before its response headers — no polling
    let logs = captured.lock().expect("capture buffer").clone();
    let fresh = &logs[before..];
    assert!(
        fresh.contains("duplicate client_id detected"),
        "the second same-id subscribe must warn; captured since the first: {fresh:?}"
    );
    assert!(
        fresh.contains("doorbell self-skip disabled"),
        "the warn must say what changed for this id: {fresh:?}"
    );
    assert!(
        fresh.contains("ns=namespac") && fresh.contains("client=laptop-o"),
        "correlation fields are 8-char prefixes of ns and client_id: {fresh:?}"
    );
}

/// The dual-active delivery is only as wide as the anomaly: once one
/// of the two same-id subscribers disconnects (active count back to
/// one) the self-origin skip returns for the survivor — the
/// single-active optimization is untouched by the fix.
#[tokio::test]
async fn duplicate_client_id_back_to_single_resumes_self_skip() {
    let (addr, hub) = spawn(None, QUIET_HEARTBEAT).await;

    let (status, _, mut survivor) = subscribe(addr, r#"{"key":"ns","client_id":"laptop-1"}"#).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, departing) = subscribe(addr, r#"{"key":"ns","client_id":"laptop-1"}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hub.receiver_count("ns"), 2, "both same-id streams are live");

    // dropping the body closes the connection; the pump teardown then
    // releases its registration. Polling the (existing)
    // receiver-count observability until the survivor is alone —
    // the dual-active release happens before this is observable.
    drop(departing);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while hub.receiver_count("ns") != 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the departing subscription was not released after the disconnect"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // single-active again: the survivor must NOT hear its own id's push
    push_ok(
        addr,
        r#"{"key":"ns","client_id":"laptop-1","rows":[
            {"rel_path":"/a","deleted":false,"payload":"x"}]}"#,
    )
    .await;
    let nothing = tokio::time::timeout(Duration::from_millis(500), survivor.next_frame()).await;
    assert!(
        nothing.is_err(),
        "back to single-active, a self-origin push must be skipped again, got: {nothing:?}"
    );
}
