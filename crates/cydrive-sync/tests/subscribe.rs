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
//! - response headers: text/event-stream + no-cache +
//!   X-Accel-Buffering: no (nginx/openresty must not buffer SSE)

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
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
