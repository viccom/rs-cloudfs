//! End-to-end: the real axum router on a random loopback port, driven
//! over real HTTP by the hyper-util legacy client (the same wire path
//! a reverse proxy or curl would take).
//!
//! Covered (sync-lite Batch A HTTP contract):
//! - push -> pull roundtrip (rows, versions, tombstone flag, payloads)
//! - push answers {max_version}; it grows batch over batch
//! - incremental pull over HTTP (since filtering)
//! - configured secret: missing/wrong -> 403 {"error": ...}, right -> 200;
//!   no configured secret -> everyone passes
//! - malformed JSON -> 400 {"error": ...} on both endpoints
//! - unknown namespace pull -> 200 with empty rows and max_version 0

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::{Request, StatusCode};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use cydrive_sync::router::router;
use cydrive_sync::store::SyncStore;
use cydrive_sync::wire::{PullRequest, PullResponse, PushRequest, PushResponse, PushRow};

/// Spawns the real server (in-memory store) on `127.0.0.1:0` and
/// returns its address.
async fn spawn_server(secret: Option<&str>) -> SocketAddr {
    let store = Arc::new(SyncStore::open_in_memory().expect("in-memory store"));
    let app = router(store, secret.map(str::to_string));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind random loopback port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("accept loop");
    });
    addr
}

/// POSTs `body` to `path` on the spawned server, returning status and
/// raw response bytes.
async fn post(addr: SocketAddr, path: &str, body: String) -> (StatusCode, Bytes) {
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{addr}{path}"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
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

fn push_request(key: &str, secret: Option<&str>, rows: Vec<PushRow>) -> String {
    serde_json::to_string(&PushRequest {
        key: key.to_string(),
        secret: secret.map(str::to_string),
        rows,
    })
    .expect("serialize push request")
}

fn push_row(rel_path: &str, deleted: bool, payload: &str) -> PushRow {
    PushRow {
        rel_path: rel_path.to_string(),
        deleted,
        payload: payload.to_string(),
    }
}

async fn push(addr: SocketAddr, key: &str, secret: Option<&str>, rows: Vec<PushRow>) -> (StatusCode, Bytes) {
    post(addr, "/v1/push", push_request(key, secret, rows)).await
}

async fn pull(addr: SocketAddr, key: &str, since: i64) -> (StatusCode, Bytes) {
    let request = serde_json::to_string(&PullRequest {
        key: key.to_string(),
        since,
    })
    .expect("serialize pull request");
    post(addr, "/v1/pull", request).await
}

#[tokio::test]
async fn push_then_pull_roundtrip_over_http() {
    let addr = spawn_server(None).await;

    let (status, body) = push(
        addr,
        "ns1",
        None,
        vec![
            push_row("/a.txt", false, "{\"size\":1}"),
            push_row("/b", true, ""),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    let response: PushResponse = serde_json::from_slice(&body).expect("push response JSON");
    assert_eq!(response.max_version, 2);

    let (status, body) = pull(addr, "ns1", 0).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    let response: PullResponse = serde_json::from_slice(&body).expect("pull response JSON");
    assert_eq!(response.max_version, 2);
    assert_eq!(response.rows.len(), 2);

    let a = response.rows.iter().find(|r| r.rel_path == "/a.txt").unwrap();
    assert_eq!(a.version, 1);
    assert!(!a.deleted);
    assert_eq!(a.payload, "{\"size\":1}");
    let b = response.rows.iter().find(|r| r.rel_path == "/b").unwrap();
    assert!(b.deleted);
    assert_eq!(b.version, 2);
    assert_eq!(b.payload, "");
}

#[tokio::test]
async fn push_max_version_grows_and_incremental_pull_filters() {
    let addr = spawn_server(None).await;

    let (status, body) = push(addr, "ns", None, vec![push_row("/a", false, "a")]).await;
    assert_eq!(status, StatusCode::OK);
    let first: PushResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(first.max_version, 1);

    let (status, body) = push(
        addr,
        "ns",
        None,
        vec![push_row("/b", false, "b"), push_row("/c", false, "c")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let second: PushResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(second.max_version, 3, "a batch of N rows raises the counter by N");

    // only rows newer than the cursor come back
    let (status, body) = pull(addr, "ns", first.max_version).await;
    assert_eq!(status, StatusCode::OK);
    let response: PullResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(response.max_version, 3);
    let paths: Vec<&str> = response.rows.iter().map(|r| r.rel_path.as_str()).collect();
    assert_eq!(paths, vec!["/b", "/c"]);

    // an up-to-date cursor answers empty but still reports the counter
    let (status, body) = pull(addr, "ns", second.max_version).await;
    assert_eq!(status, StatusCode::OK);
    let response: PullResponse = serde_json::from_slice(&body).unwrap();
    assert!(response.rows.is_empty());
    assert_eq!(response.max_version, 3);
}

#[tokio::test]
async fn secret_gate_rejects_missing_and_wrong_but_accepts_right() {
    let addr = spawn_server(Some("s3cret")).await;

    // missing secret -> 403 with a JSON {"error": ...} body
    let (status, body) = push(addr, "ns", None, vec![push_row("/a", false, "a")]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body:?}");
    let error: serde_json::Value = serde_json::from_slice(&body).expect("403 body is JSON");
    assert!(
        error.get("error").is_some_and(|v| v.is_string()),
        "body: {body:?}"
    );

    // wrong secret -> 403
    let (status, body) = push(addr, "ns", Some("nope"), vec![push_row("/a", false, "a")]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body:?}");

    // the rejected pushes must not have written anything
    let (status, body) = pull(addr, "ns", 0).await;
    assert_eq!(status, StatusCode::OK);
    let response: PullResponse = serde_json::from_slice(&body).unwrap();
    assert!(response.rows.is_empty());
    assert_eq!(response.max_version, 0);

    // right secret -> 200
    let (status, body) = push(addr, "ns", Some("s3cret"), vec![push_row("/a", false, "a")]).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    let response: PushResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(response.max_version, 1);
}

#[tokio::test]
async fn no_configured_secret_allows_everyone() {
    let addr = spawn_server(None).await;

    let (status, body) = push(addr, "ns", None, vec![push_row("/a", false, "a")]).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    let (status, body) = pull(addr, "ns", 0).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    let response: PullResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(response.rows.len(), 1);
}

#[tokio::test]
async fn malformed_json_is_400_with_error_json() {
    let addr = spawn_server(None).await;

    for path in ["/v1/push", "/v1/pull"] {
        let (status, body) = post(addr, path, "{not json".to_string()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body:?}");
        let error: serde_json::Value = serde_json::from_slice(&body).expect("400 body is JSON");
        assert!(
            error.get("error").is_some_and(|v| v.is_string()),
            "{path}: {body:?}"
        );
    }
}

#[tokio::test]
async fn pull_unknown_namespace_is_empty_with_zero_max_version() {
    let addr = spawn_server(None).await;

    let (status, body) = pull(addr, "ghost", 0).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    let response: PullResponse = serde_json::from_slice(&body).unwrap();
    assert!(response.rows.is_empty());
    assert_eq!(response.max_version, 0);
}
