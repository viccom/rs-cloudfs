//! RED-phase tests for the quasi-realtime sync wiring (doorbell model,
//! approved design): the periodic sync task becomes a three-source
//! `select!` — the fallback interval tick, the VFS's local-change
//! doorbell (`vfs.sync_notifier()`), and the SSE doorbell task — so a
//! pass runs within seconds of any local or remote change instead of
//! waiting for the 300s fallback.
//!
//! Covered:
//! - end-to-end quasi-realtime: instance A (fallback interval 3600s —
//!   only a doorbell can trigger a pass) applies another machine's push
//!   within seconds of the push;
//! - upload success pushes: a WebDAV PUT through the running stack
//!   reaches the server with the uploaded payload while the fallback
//!   tick is 3600s away (the enqueue wake and the upload-success wake
//!   together guarantee a post-upload pass);
//! - SSE resilience: a secret-gated server against a secretless client
//!   only warns — the stack stays healthy (PROPFIND 207) and shuts down
//!   cleanly;
//! - the doorbell task itself against a scripted raw endpoint: it rings
//!   on every successful connection (covering rings missed while
//!   offline), skips self-origin frames, survives a hung handshake
//!   (bounded by its client's request timeout), reconnects with the
//!   injected backoff, and exits cleanly on the shutdown gate.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cloudkit_core::config::CyDriveConfig;
use cloudkit_core::database::{FileRecord, FileUpsert, MetaDatabase};
use cloudkit_core::sync::namespace_key;
use cloudkit_core::sync::{serialize_row, SyncClient, SyncRowUpdate};
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::CloudTransport;
use cloudkit_sync_server::router::router;
use cloudkit_sync_server::store::SyncStore;
use cydrive_cli::sync_client::HttpSyncClient;
use cydrive_cli::{run_with_transport, spawn_sync_doorbell, RunHandle, ShutdownWatch};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::sleep;

// ------------------------------------------------------------- helpers ---

/// The shared token/chat pair of "instance A" (and its namespace).
const TOKEN: &str = "123456:ABC-DEF";
const CHAT: i64 = 42;

fn test_namespace() -> String {
    namespace_key(TOKEN, &CHAT.to_string())
}

/// Spawns the real sync server (in-memory store, default heartbeat) on
/// an ephemeral loopback port.
async fn spawn_sync_server(secret: Option<&str>) -> (SocketAddr, Arc<SyncStore>) {
    let store = Arc::new(SyncStore::open_in_memory().expect("in-memory sync store"));
    let app = router(Arc::clone(&store), secret.map(str::to_string));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral loopback port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("sync accept loop");
    });
    (addr, store)
}

/// A config anchored in `dir` with the sync fields injected and the
/// fallback interval pushed far away — within a test's lifetime, only a
/// doorbell can trigger a pass.
fn realtime_config(dir: &Path, sync_url: String) -> CyDriveConfig {
    CyDriveConfig {
        bot_token: TOKEN.to_string(),
        chat_id: CHAT,
        db_path: dir.join("meta.db").to_string_lossy().into_owned(),
        cache_path: dir.join("cache").to_string_lossy().into_owned(),
        webdav_port: 0,
        auto_mount_drive: false,
        enable_web_ui: false,
        sync_url: Some(sync_url),
        sync_interval_secs: 3600,
        ..CyDriveConfig::default()
    }
}

/// Seeds one uploaded file row (chunk-0 message id included).
fn seed_file(db: &MetaDatabase, rel_path: &str, msg_id: i64) {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    db.upsert_file(&FileUpsert {
        rel_path: rel_path.to_string(),
        name: name.to_string(),
        parent_dir: "/".to_string(),
        size: 10,
        mtime: 1_700_000_000.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: Some(msg_id),
        is_uploaded: true,
        is_cached: false,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: None,
    })
    .expect("seed file row");
}

/// A pre-connected mock transport.
async fn mock_transport() -> Arc<MockTransport> {
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect mock transport");
    mock
}

/// Boots the stack, panicking on failure.
async fn boot(cfg: &CyDriveConfig, mock: Arc<MockTransport>) -> RunHandle {
    let transport: Arc<dyn CloudTransport> = mock;
    run_with_transport(cfg, transport)
        .await
        .expect("boot the stack")
}

/// Sends one raw HTTP/1.1 request (`Connection: close`) and reads the
/// response to EOF.
async fn send(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect to server");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("send request");
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .expect("read response to EOF");
    String::from_utf8_lossy(&raw).into_owned()
}

/// Builds a minimal HTTP/1.1 request with Host, Connection: close and an
/// exact Content-Length for `body`.
fn request(
    method: &str,
    target: &str,
    addr: SocketAddr,
    extra: &[(&str, &str)],
    body: &str,
) -> String {
    let mut req = format!(
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
        addr.port()
    );
    for (name, value) in extra {
        req.push_str(name);
        req.push_str(": ");
        req.push_str(value);
        req.push_str("\r\n");
    }
    req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    req.push_str("\r\n");
    req.push_str(body);
    req
}

/// Parses the numeric status code off the status line.
fn status_of(resp: &str) -> u16 {
    let line = resp.lines().next().expect("status line");
    line.split_whitespace()
        .nth(1)
        .expect("status code token")
        .parse()
        .expect("numeric status code")
}

/// Polls until the test namespace has seen a push, failing on the
/// deadline.
async fn wait_for_push(store: &SyncStore, key: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Some(version)) = store.namespace_version(key) {
            if version > 0 {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the sync server never saw a push for namespace {key}"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

/// A foreign machine's logical row payload for `rel_path` (uploaded,
/// single chunk at message `msg_id`).
fn foreign_payload(rel_path: &str, msg_id: i64) -> String {
    serialize_row(
        &FileRecord {
            id: 1,
            rel_path: rel_path.to_string(),
            name: rel_path.rsplit('/').next().unwrap_or(rel_path).to_string(),
            parent_dir: "/".to_string(),
            size: 10,
            mtime: 1_700_000_999.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: Some(msg_id),
            is_uploaded: true,
            is_cached: false,
            is_encrypted: false,
            chunk_count: 1,
            mime_type: None,
            created_at: None,
            updated_at: None,
        },
        &[],
    )
    .expect("serialize the foreign row")
}

// -------------------------------------------------------------- e2e ---

/// End-to-end quasi-realtime: A boots (boot pass pushes its seed row),
/// then "another machine" pushes a new row into the shared namespace. A
/// must apply the foreign row within seconds — with the fallback tick an
/// hour away, only the SSE doorbell can have triggered the pass.
#[tokio::test]
async fn foreign_doorbell_triggers_pass_within_seconds() {
    let (addr, _store) = spawn_sync_server(None).await;
    let url = format!("http://{addr}");
    let dir = tempfile::tempdir().expect("instance A dir");
    {
        let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("seed A's db");
        seed_file(&db, "/seed.txt", 7);
    }
    let cfg = realtime_config(dir.path(), url.clone());
    let handle = boot(&cfg, mock_transport().await).await;
    let ns = test_namespace();

    // Give the boot pass (and the doorbell connection) a moment; the
    // seeded row reaching the server proves the first pass completed.
    // (No store handle here — the push is asserted through the foreign
    // row arriving; a boot without sync would fail the deadline below.)

    // Another machine pushes a fresh row into the same namespace.
    let other = HttpSyncClient::new(&url, "other-machine".to_string());
    other
        .push(
            &ns,
            None,
            &[SyncRowUpdate {
                rel_path: "/from-b.txt".to_string(),
                deleted: false,
                payload: foreign_payload("/from-b.txt", 99),
            }],
        )
        .await
        .expect("the foreign push is accepted");

    // A applies the foreign row within seconds.
    let deadline = Instant::now() + Duration::from_secs(10);
    let applied = loop {
        let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("read A's db");
        if let Some(row) = db.get_file("/from-b.txt").expect("db read") {
            break row;
        }
        assert!(
            Instant::now() < deadline,
            "the foreign row never reached instance A — the doorbell-triggered pass \
             did not happen within 10s"
        );
        sleep(Duration::from_millis(100)).await;
    };
    assert!(applied.is_uploaded, "the applied row keeps its state");
    assert_eq!(applied.telegram_msg_id, Some(99));

    handle.shutdown().await;
}

/// Upload success pushes: a WebDAV PUT through the running stack lands
/// on the server carrying the UPLOADED payload while the fallback tick
/// is an hour away. The enqueue wake may push the pending row first;
/// the upload queue's success persist wake then guarantees a later pass
/// that pushes the uploaded state (pinned separately at core level).
#[tokio::test]
async fn upload_success_drives_push_to_server_without_tick() {
    let (addr, store) = spawn_sync_server(None).await;
    let url = format!("http://{addr}");
    let dir = tempfile::tempdir().expect("instance dir");
    {
        let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("seed db");
        seed_file(&db, "/seed.txt", 8);
    }
    let cfg = realtime_config(dir.path(), url);
    let handle = boot(&cfg, mock_transport().await).await;
    let ns = test_namespace();

    // The boot pass must complete (its push visible) BEFORE the local
    // change, so the later server movement can only come from a
    // doorbell-triggered pass.
    wait_for_push(&store, &ns).await;

    let webdav = handle.local_addr();
    let resp = send(
        webdav,
        &request("PUT", "/fresh.txt", webdav, &[], "fresh bytes"),
    )
    .await;
    assert_eq!(status_of(&resp), 201, "PUT accepted: {resp}");

    // The server eventually holds the row with its uploaded payload.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (rows, _max) = store.pull(&ns, 0).expect("server pull");
        let uploaded = rows.iter().any(|row| {
            row.rel_path == "/fresh.txt" && row.payload.contains("\"is_uploaded\":true")
        });
        if uploaded {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the uploaded payload never reached the server — no pass ran after the \
             upload success within 10s"
        );
        sleep(Duration::from_millis(100)).await;
    }

    handle.shutdown().await;
}

/// SSE resilience: a secret-gated server against a secretless client —
/// the doorbell task's 403 loop only warns; the stack stays healthy and
/// shuts down cleanly (the `unreachable_sync_url_only_warns` precedent,
/// now with a live server rejecting the subscribe).
#[tokio::test]
async fn secret_gated_subscribe_keeps_service_healthy() {
    std::env::remove_var("CYDRIVE_SYNC_SECRET");
    let (addr, _store) = spawn_sync_server(Some("s3cret")).await;
    let dir = tempfile::tempdir().expect("instance dir");
    let cfg = realtime_config(dir.path(), format!("http://{addr}"));

    let handle = boot(&cfg, mock_transport().await).await;
    let webdav = handle.local_addr();

    // Let the doomed doorbell attempt (and the equally doomed pass)
    // fail; both must only warn.
    sleep(Duration::from_millis(700)).await;

    let resp = send(
        webdav,
        &request("PROPFIND", "/", webdav, &[("Depth", "1")], ""),
    )
    .await;
    assert_eq!(status_of(&resp), 207, "service unaffected: {resp}");

    handle.shutdown().await;
}

// ------------------------------------------------- doorbell task unit ---

/// One scripted connection of the raw doorbell endpoint.
struct ConnPlan {
    /// Silence before any response bytes (a hung handshake).
    delay: Duration,
    /// Raw frame bytes written after the SSE head.
    frames: &'static str,
    /// Hold the connection open after the frames (false = drop it).
    hold: bool,
}

/// The scripted raw SSE endpoint of [`doorbell_task_reconnects`]: one
/// listener, connections answered from `plans` in order (extra
/// connections are held open silently).
async fn spawn_scripted_doorbell_endpoint(plans: Vec<ConnPlan>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind scripted endpoint");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let mut plans = plans.into_iter();
        while let Ok((mut socket, _)) = listener.accept().await {
            let Some(plan) = plans.next() else {
                // No plan left: hold silently.
                tokio::spawn(async move {
                    std::future::pending::<()>().await;
                    drop(socket);
                });
                continue;
            };
            tokio::spawn(async move {
                // Swallow the request so its bytes never stall us.
                let mut sink = [0u8; 4096];
                let _ = tokio::time::timeout(Duration::from_secs(1), socket.read(&mut sink)).await;
                if !plan.delay.is_zero() {
                    sleep(plan.delay).await;
                    return; // hung handshake: close without a word
                }
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
                if socket.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                let _ = socket.write_all(plan.frames.as_bytes()).await;
                let _ = socket.flush().await;
                if plan.hold {
                    std::future::pending::<()>().await;
                }
                // else: drop = stream EOF, the doorbell reconnects
            });
        }
    });
    addr
}

/// The doorbell task against a scripted endpoint, with wake counting:
///
/// - connection 1 delivers a SELF-origin frame (skipped — no ring) and
///   a foreign frame (ring), then closes;
/// - connection 2 hangs past the client's 150s-handshake budget — the
///   subscribe fails, the task backs off (injected 50ms→100ms) and
///   retries;
/// - connection 3 opens clean and holds — the reconnect-success ring.
///
/// Exactly 3 rings in total (2 connect rings + 1 foreign frame), which
/// pins the self-origin skip as much as the ringing; the task survives,
/// stays alive, and exits cleanly on the shutdown gate.
#[tokio::test]
async fn doorbell_task_skips_self_rings_foreign_and_reconnects() {
    let addr = spawn_scripted_doorbell_endpoint(vec![
        // 1: self frame (skipped) + foreign frame (ring), then EOF
        ConnPlan {
            delay: Duration::ZERO,
            frames: concat!(
                "data: {\"max_version\":1,\"origin\":\"laptop-1\"}\n\n",
                "data: {\"max_version\":2,\"origin\":\"other\"}\n\n",
            ),
            hold: false,
        },
        // 2: hung handshake (closes at 400ms, after the 150ms budget)
        ConnPlan {
            delay: Duration::from_millis(400),
            frames: "",
            hold: false,
        },
        // 3: clean connect, silent hold
        ConnPlan {
            delay: Duration::ZERO,
            frames: "",
            hold: true,
        },
    ])
    .await;
    let url = format!("http://{addr}");

    // The wake counter: an always-waiting consumer turns every
    // notify_one into exactly one increment (permits cover the gaps).
    let wake = Arc::new(Notify::new());
    let rings = Arc::new(AtomicUsize::new(0));
    {
        let wake = Arc::clone(&wake);
        let rings = Arc::clone(&rings);
        tokio::spawn(async move {
            loop {
                wake.notified().await;
                rings.fetch_add(1, Ordering::SeqCst);
            }
        });
    }

    let client = Arc::new(HttpSyncClient::with_request_timeout(
        &url,
        Duration::from_millis(150),
        "laptop-1".to_string(),
    ));
    let watch = Arc::new(ShutdownWatch::new());
    let doorbell = spawn_sync_doorbell(
        client,
        "ns".to_string(),
        None,
        "laptop-1".to_string(),
        Arc::clone(&wake),
        Arc::clone(&watch),
        cydrive_cli::DoorbellBackoff {
            initial: Duration::from_millis(50),
            max: Duration::from_millis(200),
        },
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while rings.load(Ordering::SeqCst) < 3 {
        assert!(
            Instant::now() < deadline,
            "the doorbell never rang 3 times (connect, foreign frame, reconnect); \
             rings so far: {}",
            rings.load(Ordering::SeqCst)
        );
        sleep(Duration::from_millis(25)).await;
    }
    // Settle: no further rings may arrive (the held connection 3 is
    // silent, and the self frame was skipped — a 4th ring means the
    // skip is broken).
    sleep(Duration::from_millis(400)).await;
    assert_eq!(
        rings.load(Ordering::SeqCst),
        3,
        "exactly two connect rings + one foreign frame; a self-origin frame must not ring"
    );
    assert!(
        !doorbell.is_finished(),
        "the doorbell task stays alive on the held stream"
    );

    // Clean exit on the shutdown gate.
    watch.trigger();
    tokio::time::timeout(Duration::from_secs(1), doorbell)
        .await
        .expect("the doorbell task exits on the shutdown gate")
        .expect("the doorbell task did not panic");
}
