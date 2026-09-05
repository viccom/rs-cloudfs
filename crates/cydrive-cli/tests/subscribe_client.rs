//! RED-phase tests for the client half of the SSE doorbell
//! (quasi-realtime batch): [`HttpSyncClient`] gains the per-database
//! `client_id` (sent on push/pull — clearing the previous batch's
//! `client_id: None` anchors) and `subscribe_stream`, one long-lived
//! doorbell subscription:
//!
//! - POST `{url}/v1/subscribe` with `{key, secret, client_id}`;
//! - frames streamed and parsed incrementally — `data: {json}` lines
//!   become [`SubscribeEvent`]s on an mpsc receiver, `: keepalive`
//!   comment lines are ignored, a malformed data line warns and is
//!   skipped without dropping the connection;
//! - a non-2xx answer (403 with the server's secret gate) surfaces as
//!   [`SyncError::Client`] carrying the status in plain words.
//!
//! The client_id wiring is pinned compositionally through the server's
//! origin-skip: a subscriber whose client_id equals the pusher's own
//! hears NOTHING (so both the subscribe and the push must have carried
//! the same id), while a foreign pusher's doorbell arrives parsed.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use cydrive_cli::sync_client::{sse_frame_data, HttpSyncClient};
use cydrive_core::sync::{SyncClient, SyncRowUpdate, SyncError};
use cydrive_sync::events::EventHub;
use cydrive_sync::router::router_with_hub;
use cydrive_sync::store::SyncStore;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::time::{sleep, Instant};

/// A heartbeat long enough that keepalive frames never interleave with
/// the frames a test asserts on (the keepalive TOLERANCE has its own
/// short-heartbeat test).
const QUIET_HEARTBEAT: Duration = Duration::from_secs(60);

// ------------------------------------------------------------- helpers ---

/// Spawns the real sync router on `127.0.0.1:0` with an observable
/// broadcast hub; returns (address, hub).
async fn spawn_router(
    secret: Option<&str>,
    heartbeat: Duration,
) -> (SocketAddr, Arc<EventHub>) {
    let store = Arc::new(SyncStore::open_in_memory().expect("in-memory store"));
    let hub = Arc::new(EventHub::new());
    let app = router_with_hub(store, secret.map(str::to_string), Arc::clone(&hub), heartbeat);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral loopback port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("sync accept loop");
    });
    (addr, hub)
}

/// One opaque live row update (the server stores payloads verbatim).
fn row_update(rel_path: &str) -> SyncRowUpdate {
    SyncRowUpdate {
        rel_path: rel_path.to_string(),
        deleted: false,
        payload: "opaque-payload".to_string(),
    }
}

/// Polls until `cond` holds, failing on the deadline (seconds).
async fn wait_until<F: Fn() -> bool>(what: &str, cond: F) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

/// Spawns a raw TCP endpoint that answers ONE connection with a
/// `200 text/event-stream` head plus the literal `frames` bytes, then
/// keeps the connection open (silently). Raw TCP so the emitted frames
/// are exactly the (malformed) bytes the test wants — the real router
/// never sends a bad line.
async fn spawn_raw_sse_endpoint(frames: &[u8]) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw sse endpoint");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("one connection");
        // Swallow the request (a small POST) so its bytes never stall us.
        let mut sink = [0u8; 4096];
        let _ = tokio::time::timeout(Duration::from_secs(1), socket.read(&mut sink)).await;
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
        socket.write_all(head.as_bytes()).await.expect("sse head");
        socket.write_all(frames).await.expect("sse frames");
        socket.flush().await.expect("flush");
        // Hold the connection open: the stream must stay usable.
        std::future::pending::<()>().await;
    });
    addr
}

// ------------------------------------------------------- client id wire ---

/// push and subscribe must BOTH carry the client_id — proven through the
/// server's origin-skip: a push under the subscriber's own id rings
/// nothing, a foreign push rings with the foreign origin parsed.
#[tokio::test]
async fn push_and_subscribe_carry_the_client_id_end_to_end() {
    let (addr, hub) = spawn_router(None, QUIET_HEARTBEAT).await;
    let url = format!("http://{addr}");
    let ns = "identity-ns";

    let laptop = HttpSyncClient::new(&url, "laptop".to_string());
    let mut doorbell = laptop
        .subscribe_stream(ns, None)
        .await
        .expect("the doorbell subscription opens");
    wait_until("the hub to register the subscriber", || {
        hub.receiver_count(ns) == 1
    })
    .await;

    // The same machine pushes again (a second client instance sharing
    // the identity): the server must skip the subscriber's own bell.
    let laptop_elsewhere = HttpSyncClient::new(&url, "laptop".to_string());
    laptop_elsewhere
        .push(ns, None, &[row_update("/self.txt")])
        .await
        .expect("self push accepted");
    assert!(
        tokio::time::timeout(Duration::from_millis(500), doorbell.recv())
            .await
            .is_err(),
        "a push carrying the subscriber's own client_id must not ring it"
    );

    // Another machine pushes: the doorbell arrives, parsed.
    let other = HttpSyncClient::new(&url, "other-machine".to_string());
    other
        .push(ns, None, &[row_update("/foreign.txt")])
        .await
        .expect("foreign push accepted");
    let event = tokio::time::timeout(Duration::from_secs(2), doorbell.recv())
        .await
        .expect("the foreign doorbell must ring")
        .expect("the stream stays open");
    assert_eq!(event.max_version, 2, "the second batch's version");
    assert_eq!(
        event.origin.as_deref(),
        Some("other-machine"),
        "the event carries the foreign pusher's client_id"
    );
}

/// At a short heartbeat the keepalive comment frames flow constantly —
/// the client must ignore them and still deliver the parsed data event.
#[tokio::test]
async fn subscribe_stream_parses_events_and_tolerates_keepalives() {
    let (addr, _hub) = spawn_router(None, Duration::from_millis(100)).await;
    let url = format!("http://{addr}");
    let ns = "keepalive-ns";

    let watcher = HttpSyncClient::new(&url, "watcher".to_string());
    let mut doorbell = watcher
        .subscribe_stream(ns, None)
        .await
        .expect("subscription opens");
    // Let a couple of keepalives pass through the parser first.
    sleep(Duration::from_millis(250)).await;

    let pusher = HttpSyncClient::new(&url, "pusher".to_string());
    pusher
        .push(ns, None, &[row_update("/hot.txt")])
        .await
        .expect("push accepted");
    let event = tokio::time::timeout(Duration::from_secs(2), doorbell.recv())
        .await
        .expect("the data event must survive the keepalive noise")
        .expect("the stream stays open");
    assert_eq!(event.max_version, 1);
    assert_eq!(event.origin.as_deref(), Some("pusher"));
}

// ------------------------------------------------------ frame parsing ---

/// Pure parser contract: one complete SSE frame (lines, blank-line
/// terminated) yields its joined data payload; comment lines and other
/// field names are ignored; `data:` without the space still parses; CRLF
/// line endings are tolerated.
#[test]
fn sse_frame_data_extracts_the_data_payload() {
    assert_eq!(
        sse_frame_data("data: {\"max_version\":1,\"origin\":null}\n"),
        Some("{\"max_version\":1,\"origin\":null}".to_string())
    );
    // no space after the colon is legal SSE
    assert_eq!(
        sse_frame_data("data:x\n"),
        Some("x".to_string())
    );
    // comment-only frame (the keepalive) carries no data
    assert_eq!(sse_frame_data(": keepalive\n"), None);
    // unrelated field names are ignored
    assert_eq!(sse_frame_data("event: ping\nid: 7\nretry: 1000\n"), None);
    // multiple data lines join with a newline (SSE spec)
    assert_eq!(
        sse_frame_data("data: a\ndata: b\n"),
        Some("a\nb".to_string())
    );
    // CRLF tolerance
    assert_eq!(
        sse_frame_data("data: crlf\r\n\r\n"),
        Some("crlf".to_string())
    );
    // a mixed frame: comment + data
    assert_eq!(
        sse_frame_data(": comment\ndata: mixed\n"),
        Some("mixed".to_string())
    );
}

/// A malformed data line must not kill the stream: the bad frame warns
/// and is skipped, the next good frame still arrives.
#[tokio::test]
async fn subscribe_stream_skips_malformed_data_frames() {
    let addr = spawn_raw_sse_endpoint(
        b"data: this is not json\n\ndata: {\"max_version\":9,\"origin\":null}\n\n",
    )
    .await;
    let client = HttpSyncClient::new(&format!("http://{addr}"), "parser-probe".to_string());
    let mut doorbell = client
        .subscribe_stream("any-ns", None)
        .await
        .expect("subscription opens against the raw endpoint");

    let event = tokio::time::timeout(Duration::from_secs(2), doorbell.recv())
        .await
        .expect("the good frame after the malformed one must arrive")
        .expect("the stream stays open");
    assert_eq!(event.max_version, 9);
    assert_eq!(event.origin, None);

    // And the stream is still alive (the endpoint holds it open): no
    // further events, but also no termination within a short window.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), doorbell.recv())
            .await
            .is_err(),
        "no further frames expected"
    );
}

// ------------------------------------------------------------ gating ---

/// A subscribe the server rejects (secret configured, client sent none)
/// must surface a plain-words Client error carrying the HTTP status —
/// not a hung stream.
#[tokio::test]
async fn subscribe_stream_maps_rejection_to_client_error() {
    let (addr, _hub) = spawn_router(Some("s3cret"), QUIET_HEARTBEAT).await;
    let client = HttpSyncClient::new(&format!("http://{addr}"), "gated".to_string());
    let error = client
        .subscribe_stream("ns", None)
        .await
        .expect_err("the gated subscribe must fail");
    let SyncError::Client(message) = &error else {
        panic!("expected a transport-level Client error, got: {error:?}");
    };
    assert!(
        message.contains("403"),
        "the HTTP status must surface in plain words: {message}"
    );
    assert!(
        message.contains("/v1/subscribe"),
        "the error must name the endpoint: {message}"
    );
}
