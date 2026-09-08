//! Concurrency gate on push/pull body buffering (the inherited
//! rs-CyDrive sync-review Medium/Low item): every push/pull request
//! buffers its whole body in memory — up to 64 MB — so unbounded
//! concurrent batches could OOM a family-grade box. The router bounds
//! this with a semaphore ([`MAX_CONCURRENT_BODY_BUFFERS`] permits,
//! 2 × 64 MB = 128 MB peak budget): a request arriving while every
//! permit is taken is shed with 503 + an actionable "retry shortly"
//! body, permits are released by Drop alone, and subscribe (an SSE
//! doorbell that holds no batch body) deliberately stays outside the
//! gate.
//!
//! Covered:
//! - exhausted gate: push AND pull answer 503 `{"error": ...}` whose
//!   text says busy + retry — actionable and distinct from the 403
//!   secret failure and the 413 oversized batch
//! - release determinism: dropping one held permit lets the very next
//!   push through, and that push's own permit comes back (a following
//!   pull succeeds while the test still holds one)
//! - idle gate: normal paths unchanged (push/pull 200, malformed JSON
//!   still 400 — the gate distorts nothing)
//! - subscribe stays ungated: under a fully exhausted gate a
//!   secret-rejected subscribe still answers its complete 403, and a
//!   valid subscribe still opens the text/event-stream

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{Request, StatusCode};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use cloudkit_sync_server::events::EventHub;
use cloudkit_sync_server::router::{router_with_gate, MAX_CONCURRENT_BODY_BUFFERS};
use cloudkit_sync_server::store::SyncStore;

/// One valid minimal push batch.
const PUSH_BODY: &str =
    r#"{"key":"ns","rows":[{"rel_path":"/a","deleted":false,"payload":"x"}]}"#;
/// One valid minimal pull request.
const PULL_BODY: &str = r#"{"key":"ns","since":0}"#;

/// Spawns the real router on `127.0.0.1:0` with an injectable
/// body-buffering gate at the production permit count; returns
/// (address, gate).
async fn spawn(secret: Option<&str>, heartbeat: Duration) -> (SocketAddr, Arc<Semaphore>) {
    let store = Arc::new(SyncStore::open_in_memory().expect("in-memory store"));
    let gate = Arc::new(Semaphore::new(MAX_CONCURRENT_BODY_BUFFERS));
    let app = router_with_gate(
        store,
        secret.map(str::to_string),
        Arc::new(EventHub::new()),
        heartbeat,
        gate.clone(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind random loopback port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("accept loop");
    });
    (addr, gate)
}

/// POSTs `body` to `path`, returning status and raw response bytes.
async fn post(addr: SocketAddr, path: &str, body: &str) -> (StatusCode, Bytes) {
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{addr}{path}"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("request build");
    let response = client.request(request).await.expect("http roundtrip");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes();
    (status, bytes)
}

/// Takes every permit out of `gate` — the test-side stand-in for an
/// equal number of in-flight body reads, no racing real requests.
/// Dropping a returned guard releases its permit.
fn exhaust(gate: &Arc<Semaphore>) -> Vec<OwnedSemaphorePermit> {
    let mut held = Vec::new();
    while let Ok(permit) = gate.clone().try_acquire_owned() {
        held.push(permit);
    }
    assert!(
        !held.is_empty(),
        "the gate must start with at least one permit"
    );
    held
}

/// A push or pull arriving while every body-buffering permit is taken
/// must be shed: 503 with a JSON error whose text is actionable — it
/// says the server is busy and to retry (clearly distinct from a 403
/// secret failure or a 413 oversized batch).
#[tokio::test]
async fn exhausted_gate_sheds_push_and_pull_with_actionable_503() {
    let (addr, gate) = spawn(None, Duration::from_secs(60)).await;
    let _held = exhaust(&gate);

    for (path, body) in [("/v1/push", PUSH_BODY), ("/v1/pull", PULL_BODY)] {
        let (status, bytes) = post(addr, path, body).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {bytes:?}");
        let error: serde_json::Value =
            serde_json::from_slice(&bytes).expect("503 body is JSON");
        let text = error
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        assert!(
            text.contains("busy") && text.contains("retry"),
            "503 text must tell the client to retry shortly: {text}"
        );
    }
}

/// Release determinism: permits come back via Drop alone. Dropping one
/// held permit lets the very next push through, and the served push
/// returns its own permit — the following pull succeeds even though
/// the test still holds one permit hostage. No manual release, no
/// stuck gate.
#[tokio::test]
async fn dropped_permit_lets_the_next_push_and_pull_through() {
    let (addr, gate) = spawn(None, Duration::from_secs(60)).await;
    let mut held = exhaust(&gate);

    drop(held.pop().expect("at least one permit held"));
    let (status, bytes) = post(addr, "/v1/push", PUSH_BODY).await;
    assert_eq!(status, StatusCode::OK, "body: {bytes:?}");

    // one permit is still held by the test, so this pull succeeding
    // proves the push's own permit was released server-side
    let (status, bytes) = post(addr, "/v1/pull", PULL_BODY).await;
    assert_eq!(status, StatusCode::OK, "body: {bytes:?}");

    drop(held);
    let (status, bytes) = post(addr, "/v1/push", PUSH_BODY).await;
    assert_eq!(status, StatusCode::OK, "body: {bytes:?}");
}

/// An idle gate must be invisible: normal push/pull answer 200 and
/// malformed JSON still gets its 400 — the gate distorts nothing on
/// the happy and validation paths.
#[tokio::test]
async fn idle_gate_changes_nothing_on_normal_and_error_paths() {
    let (addr, _gate) = spawn(None, Duration::from_secs(60)).await;

    let (status, bytes) = post(addr, "/v1/push", PUSH_BODY).await;
    assert_eq!(status, StatusCode::OK, "body: {bytes:?}");
    let (status, bytes) = post(addr, "/v1/pull", PULL_BODY).await;
    assert_eq!(status, StatusCode::OK, "body: {bytes:?}");
    let (status, bytes) = post(addr, "/v1/push", "{not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {bytes:?}");
}

/// Subscribe holds no batch body (SSE doorbell) and must stay outside
/// the gate: under a fully exhausted gate a secret-rejected subscribe
/// still answers its complete 403, and a valid subscribe still opens
/// the text/event-stream.
#[tokio::test]
async fn subscribe_stays_outside_the_gate() {
    let (addr, gate) = spawn(Some("s3cret"), Duration::from_millis(100)).await;
    let _held = exhaust(&gate);

    // secret rejection completes with 403 — not the gate's 503
    let (status, bytes) = post(addr, "/v1/subscribe", r#"{"key":"ns"}"#).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {bytes:?}");

    // a valid subscribe still opens the stream while every gate
    // permit is held by the test (raw TCP, so the never-ending SSE
    // body cannot hang the assertion)
    let mut socket = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let body = r#"{"key":"ns","secret":"s3cret"}"#;
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
        let read = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut chunk))
            .await
            .expect("subscribe headers within 2s")
            .expect("read response head");
        assert!(read > 0, "eof before response headers arrived");
        head.extend_from_slice(&chunk[..read]);
        if head.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "subscribe must open while the gate is exhausted: {head}"
    );
    assert!(head.contains("text/event-stream"), "head: {head}");
}
