//! End-to-end sync wiring over real HTTP (sync-lite Batch B4): the real
//! axum sync server on an ephemeral loopback port driven through the
//! CLI's own seams — [`run_sync_command`] (the `cydrive sync` body) and
//! [`run_with_transport`] (the `run` boot with the periodic sync task).
//!
//! Covered:
//! - dual instances converge: a seeded drive (directory + multi-chunk
//!   file + pending ghost) syncs A -> server -> B; the ghost row does
//!   not propagate to B (no local bytes there); a second pass pushes
//!   nothing (idempotency);
//! - `run` boots the periodic sync task: the server sees a push while
//!   WebDAV keeps serving (PROPFIND 207);
//! - an unreachable sync_url only warns: the stack serves and shuts
//!   down cleanly;
//! - the server-side secret: a matching client secret passes and the
//!   server stores the rows; a mismatching one fails the command with
//!   the HTTP 403 surfacing in the error chain;
//! - the config-sourced secret: a `config.toml` carrying `sync_secret`
//!   (env unset) powers a full gated pass through the same chain
//!   `cydrive sync` runs — pull and push both clear the 403 gate.
//! - https (ignored, real-network): a live public-CA endpoint completes
//!   the full TLS chain and answers with a real HTTP status.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use cydrive_cli::sync_client::{HttpSyncClient, SYNC_SECRET_ENV};
use cydrive_cli::{resolve_sync_secret, run_sync_command, run_with_transport, RunHandle};
use cydrive_core::config::CyDriveConfig;
use cydrive_core::database::{FileUpsert, MetaDatabase};
use cydrive_core::sync::namespace_key;
use cydrive_core::sync::SyncClient;
use cydrive_core::sync::SyncError;
use cydrive_core::transport::mock::MockTransport;
use cydrive_core::transport::CloudTransport;
use cydrive_sync::router::router;
use cydrive_sync::store::SyncStore;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::sleep;

// ------------------------------------------------------------- helpers ---

/// Serialises every test that touches the process-wide `CYDRIVE_SYNC_SECRET`
/// variable (tests in one binary share one process; env vars race).
static SECRET_ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Holds [`SECRET_ENV_MUTEX`] and removes the secret env var on drop —
/// including on panic, so a failing assertion cannot poison later runs.
struct SecretEnvGuard {
    _lock: MutexGuard<'static, ()>,
}

impl Drop for SecretEnvGuard {
    fn drop(&mut self) {
        std::env::remove_var(SYNC_SECRET_ENV);
    }
}

/// Locks [`SECRET_ENV_MUTEX`] and starts with the secret env var unset.
fn lock_secret_env() -> SecretEnvGuard {
    let lock = SECRET_ENV_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::remove_var(SYNC_SECRET_ENV);
    SecretEnvGuard { _lock: lock }
}

/// The shared token/chat pair: same values on both instances = the same
/// namespace key = the same drive.
const TOKEN: &str = "123456:ABC-DEF";
const CHAT: i64 = 42;

/// The namespace key both instances derive from [`TOKEN`]/[`CHAT`].
fn test_namespace() -> String {
    namespace_key(TOKEN, &CHAT.to_string())
}

/// Spawns the real sync server (in-memory store) on `127.0.0.1:0`,
/// mirroring cydrive-sync's own e2e assembly.
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

/// A config anchored in `dir` with the sync fields injected: ephemeral
/// WebDAV port, temp DB and cache, no web UI (the config default `true`
/// would contend on the fixed 8088 port across parallel tests), no
/// auto-mount (the offline gate must never map a real drive).
fn sync_config(dir: &Path, sync_url: Option<String>) -> CyDriveConfig {
    CyDriveConfig {
        bot_token: TOKEN.to_string(),
        chat_id: CHAT,
        db_path: dir.join("meta.db").to_string_lossy().into_owned(),
        cache_path: dir.join("cache").to_string_lossy().into_owned(),
        webdav_port: 0,
        auto_mount_drive: false,
        enable_web_ui: false,
        sync_url,
        ..CyDriveConfig::default()
    }
}

/// Final path segment of a virtual path.
fn basename(rel_path: &str) -> &str {
    rel_path.rsplit('/').next().unwrap_or(rel_path)
}

/// Parent virtual path (`"/"` for top level).
fn parent_dir(rel_path: &str) -> String {
    match rel_path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => rel_path[..index].to_string(),
    }
}

/// Seeds one directory row.
fn seed_dir(db: &MetaDatabase, rel_path: &str) {
    db.upsert_file(&FileUpsert {
        rel_path: rel_path.to_string(),
        name: basename(rel_path).to_string(),
        parent_dir: parent_dir(rel_path),
        size: 0,
        mtime: 1_700_000_000.0,
        sha256: None,
        is_dir: true,
        telegram_msg_id: None,
        is_uploaded: true,
        is_cached: false,
        is_encrypted: false,
        chunk_count: 0,
        mime_type: None,
    })
    .expect("seed directory row");
}

/// Seeds one file row with its chunk sequence (`uploaded` rows carry
/// message ids; a pending ghost has none).
fn seed_file(db: &MetaDatabase, rel_path: &str, uploaded: bool, msg_ids: &[i64]) {
    let file_id = db
        .upsert_file(&FileUpsert {
            rel_path: rel_path.to_string(),
            name: basename(rel_path).to_string(),
            parent_dir: parent_dir(rel_path),
            size: (msg_ids.len() as i64) * 10,
            mtime: 1_700_000_123.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: msg_ids.first().copied(),
            is_uploaded: uploaded,
            is_cached: false,
            is_encrypted: false,
            chunk_count: msg_ids.len() as i64,
            mime_type: None,
        })
        .expect("seed file row");
    for (index, msg) in msg_ids.iter().enumerate() {
        db.upsert_chunk(file_id, index as i64, *msg, 10, None)
            .expect("seed chunk row");
    }
}

/// Sorted rel_path set of a db's files table.
fn rel_paths(db: &MetaDatabase) -> Vec<String> {
    let mut paths: Vec<String> = db
        .list_all_files()
        .expect("list files")
        .into_iter()
        .map(|row| row.rel_path)
        .collect();
    paths.sort();
    paths
}

/// A pre-connected mock transport (the seam the production `run`
/// substitutes `GrammersTransport` for).
async fn mock_transport() -> Arc<MockTransport> {
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect mock transport");
    mock
}

/// Boots the stack from `cfg` with `mock` injected, panicking on failure.
async fn boot(cfg: &CyDriveConfig, mock: Arc<MockTransport>) -> RunHandle {
    let transport: Arc<dyn CloudTransport> = mock;
    run_with_transport(cfg, transport)
        .await
        .expect("boot the stack")
}

/// Sends one raw HTTP/1.1 request (`Connection: close`) and reads the
/// response bytes until the server closes the connection.
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
/// exact Content-Length.
fn request(method: &str, target: &str, addr: SocketAddr, extra: &[(&str, &str)]) -> String {
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
    req.push_str("Content-Length: 0\r\n\r\n");
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

/// Polls the server store until the test namespace has seen a push
/// (its counter moved past 0), failing the test on the deadline.
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

/// Spawns a slow-drip HTTP endpoint: every connection gets a complete
/// response header block plus one body byte of the promised 256, and
/// then the connection just hangs — the "headers arrived, body
/// trickles forever" failure mode. Raw TCP like the silent SOCKS5
/// proxy in `connect_deadline.rs`: no HTTP machinery is needed to
/// betray the client. Returns the bound port.
async fn spawn_drip_body_endpoint() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the drip endpoint");
    let port = listener
        .local_addr()
        .expect("read the drip endpoint port")
        .port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                // Swallow the request (a small POST) so its bytes never
                // fill the socket buffer and stall the response side.
                let mut sink = [0u8; 4096];
                let _ = tokio::time::timeout(Duration::from_secs(1), socket.read(&mut sink)).await;
                let head = "HTTP/1.1 200 OK\r\n\
                            content-type: application/json\r\n\
                            content-length: 256\r\n\r\n";
                if socket.write_all(head.as_bytes()).await.is_ok() {
                    let _ = socket.write_all(b"{").await;
                }
                // Never deliver the remaining 255 bytes and never
                // close: anything waiting for the complete body blocks
                // forever. Closing would let the reader error out
                // immediately instead of hanging, defeating the point;
                // the task dies with the test runtime.
                std::future::pending::<()>().await;
            });
        }
    });
    port
}

// ------------------------------------------------------------- scenarios ---

/// Dual instances converge through the real server: A's seeded drive
/// (directory + multi-chunk file + pending ghost) reaches B intact —
/// except the ghost, which has no bytes on B and must not appear there.
/// A's own ghost row stays local (it is A's pending upload), so the
/// convergence assertion compares the sets with that one local row
/// excluded. A second pass on either side pushes nothing.
#[tokio::test]
async fn dual_instances_converge_over_real_http() {
    let (addr, _store) = spawn_sync_server(None).await;
    let url = format!("http://{addr}");
    let dir_a = tempfile::tempdir().expect("instance A dir");
    let dir_b = tempfile::tempdir().expect("instance B dir");

    {
        let db = MetaDatabase::open(&dir_a.path().join("meta.db")).expect("seed A's db");
        seed_dir(&db, "/docs");
        seed_file(&db, "/docs/big.bin", true, &[10, 11, 12]);
        seed_file(&db, "/pending.txt", false, &[]); // ghost: no cache copy anywhere
    }
    let cfg_a = sync_config(dir_a.path(), Some(url.clone()));
    let cfg_b = sync_config(dir_b.path(), Some(url));

    let outcome_a = run_sync_command(&cfg_a, None)
        .await
        .expect("A's first sync pass");
    assert_eq!(
        outcome_a.pushed, 3,
        "A pushes dir + file + ghost row: {outcome_a:?}"
    );
    assert_eq!(outcome_a.pushed_tombstones, 0);

    let outcome_b = run_sync_command(&cfg_b, None)
        .await
        .expect("B's first sync pass");
    assert_eq!(outcome_b.pulled, 3);
    assert_eq!(outcome_b.applied, 2, "directory + multi-chunk file apply");
    assert_eq!(
        outcome_b.skipped_ghost, 1,
        "the pending ghost has no bytes on B"
    );
    assert_eq!(outcome_b.pushed, 0, "B has nothing new to push");

    let db_b = MetaDatabase::open(&dir_b.path().join("meta.db")).expect("reopen B's db");
    assert_eq!(
        rel_paths(&db_b),
        vec!["/docs", "/docs/big.bin"],
        "the ghost row must not appear on B"
    );

    let db_a = MetaDatabase::open(&dir_a.path().join("meta.db")).expect("reopen A's db");
    let paths_a = rel_paths(&db_a);
    assert!(
        paths_a.contains(&"/pending.txt".to_string()),
        "A keeps its local pending ghost: {paths_a:?}"
    );
    let non_ghost_a: Vec<String> = paths_a
        .into_iter()
        .filter(|path| path != "/pending.txt")
        .collect();
    assert_eq!(
        non_ghost_a,
        rel_paths(&db_b),
        "everything but A's local ghost converges on both sides"
    );

    // The multi-chunk sequence survives the trip (chunks-inside-payload).
    let row = db_b
        .get_file("/docs/big.bin")
        .expect("B's db read")
        .expect("big.bin synced to B");
    assert!(row.is_uploaded);
    assert_eq!(row.chunk_count, 3);
    let msg_ids: Vec<Option<i64>> = db_b
        .get_chunks_by_file_id(row.id)
        .expect("B's chunks read")
        .into_iter()
        .map(|chunk| chunk.telegram_msg_id)
        .collect();
    assert_eq!(msg_ids, vec![Some(10), Some(11), Some(12)]);

    // Idempotency: a second pass on either side pushes nothing.
    let outcome_a2 = run_sync_command(&cfg_a, None)
        .await
        .expect("A's second sync pass");
    assert_eq!(outcome_a2.pushed, 0, "mirror now matches the server");
    assert_eq!(outcome_a2.pushed_tombstones, 0);
    let outcome_b2 = run_sync_command(&cfg_b, None)
        .await
        .expect("B's second sync pass");
    assert_eq!(outcome_b2.pushed, 0);
    assert_eq!(outcome_b2.pushed_tombstones, 0);
}

/// `run` wires the periodic sync task: with sync_url set, the server
/// sees a push right after boot (the first pass is immediate) while the
/// WebDAV service stays up (PROPFIND 207), and the stack shuts down
/// cleanly.
#[tokio::test]
async fn run_boot_syncs_and_keeps_serving() {
    let (addr, store) = spawn_sync_server(None).await;
    let dir = tempfile::tempdir().expect("instance dir");
    {
        let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("seed db");
        seed_file(&db, "/boot.txt", true, &[77]);
    }
    let cfg = sync_config(dir.path(), Some(format!("http://{addr}")));

    let handle = boot(&cfg, mock_transport().await).await;
    let webdav = handle.local_addr();

    wait_for_push(&store, &test_namespace()).await;

    let resp = send(webdav, &request("PROPFIND", "/", webdav, &[("Depth", "1")])).await;
    assert_eq!(status_of(&resp), 207, "service healthy after sync: {resp}");

    handle.shutdown().await;
}

/// Failure resilience: a sync_url pointing at a dead port only warns —
/// the stack boots, serves (PROPFIND 207 before and after the doomed
/// pass had time to fail) and shuts down cleanly.
#[tokio::test]
async fn unreachable_sync_url_only_warns() {
    // A guaranteed-dead port: bind an ephemeral listener, note the port,
    // then drop the listener so nothing can ever answer there.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a port to free it");
    let dead_port = listener.local_addr().expect("local addr").port();
    drop(listener);

    let dir = tempfile::tempdir().expect("instance dir");
    let cfg = sync_config(dir.path(), Some(format!("http://127.0.0.1:{dead_port}")));

    let handle = boot(&cfg, mock_transport().await).await;
    let webdav = handle.local_addr();

    // Give the doomed first pass time to fail (it must only warn).
    sleep(Duration::from_millis(500)).await;

    let resp = send(webdav, &request("PROPFIND", "/", webdav, &[("Depth", "1")])).await;
    assert_eq!(status_of(&resp), 207, "service unaffected: {resp}");

    handle.shutdown().await;
}

/// The server-side secret gate end to end: a matching client secret
/// passes and the server stores the rows; a mismatching secret fails
/// the command with the HTTP 403 visible in the error chain.
#[tokio::test]
async fn secret_gate_end_to_end() {
    let (addr, store) = spawn_sync_server(Some("s3cret")).await;
    let url = format!("http://{addr}");

    let dir_ok = tempfile::tempdir().expect("matching-secret instance dir");
    {
        let db = MetaDatabase::open(&dir_ok.path().join("meta.db")).expect("seed db");
        seed_file(&db, "/secret.txt", true, &[21]);
    }
    let cfg_ok = sync_config(dir_ok.path(), Some(url.clone()));
    run_sync_command(&cfg_ok, Some("s3cret"))
        .await
        .expect("matching secret passes");
    assert!(
        store
            .namespace_version(&test_namespace())
            .expect("server store read")
            .is_some_and(|version| version > 0),
        "the server stored the push"
    );

    let dir_bad = tempfile::tempdir().expect("mismatching-secret instance dir");
    {
        let db = MetaDatabase::open(&dir_bad.path().join("meta.db")).expect("seed db");
        seed_file(&db, "/other.txt", true, &[22]);
    }
    let cfg_bad = sync_config(dir_bad.path(), Some(url));
    let err = run_sync_command(&cfg_bad, Some("wrong"))
        .await
        .expect_err("mismatching secret must fail the command");
    let msg = format!("{err:#}");
    assert!(msg.contains("403"), "the HTTP status surfaces: {msg}");
}

/// The config-sourced secret reaches a gated server end to end: a
/// `config.toml` carrying `sync_secret` (the hand-written-file route the
/// batch's ruling allows) powers a full `cydrive sync` pass — pull AND
/// push — against a server that 403s secretless traffic, with the env
/// variable unset so the config leg of the resolution chain is the one
/// under test. This mirrors exactly what `sync_cmd` does: load the file,
/// resolve the secret, run the pass.
#[tokio::test]
async fn config_toml_sync_secret_powers_a_gated_pass() {
    let (addr, store) = spawn_sync_server(Some("cfg-secret")).await;

    let dir = tempfile::tempdir().expect("config-holding instance dir");
    let toml_path = dir.path().join("config.toml");
    std::fs::write(
        &toml_path,
        format!(
            concat!(
                "bot_token = \"{}\"\n",
                "chat_id = {}\n",
                "db_path = {:?}\n",
                "cache_path = {:?}\n",
                "sync_url = \"http://{}\"\n",
                "sync_secret = \"cfg-secret\"\n",
            ),
            TOKEN,
            CHAT,
            dir.path().join("meta.db").to_string_lossy(),
            dir.path().join("cache").to_string_lossy(),
            addr,
        ),
    )
    .expect("write config.toml");

    // Seed the db the config points at, then run the real chain the
    // `cydrive sync` command runs (env unset → the config key governs).
    {
        let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("seed db");
        seed_file(&db, "/from-config.txt", true, &[31]);
    }
    let _guard = lock_secret_env();
    let cfg = CyDriveConfig::load_toml(&toml_path).expect("config.toml with sync_secret loads");
    let secret = resolve_sync_secret(&cfg);
    assert_eq!(
        secret.as_deref(),
        Some("cfg-secret"),
        "with the env var unset, the config.toml value must supply the secret"
    );

    let outcome = run_sync_command(&cfg, secret.as_deref())
        .await
        .expect("the config-sourced secret must pass the pull gate");
    assert_eq!(outcome.pushed, 1, "the seeded row uploads: {outcome:?}");
    assert!(
        store
            .namespace_version(&test_namespace())
            .expect("server store read")
            .is_some_and(|version| version > 0),
        "the gated server stored the push"
    );
}

/// Real-network https probe (ignored by default — needs outbound https
/// to a live public-CA site): proves the whole TLS chain against an
/// endpoint that is not a sync server. The probe namespace has no rows
/// on the remote, so any answer maps to [`SyncError::Client`] carrying
/// a real HTTP status — which can only exist if DNS, TCP, the rustls
/// handshake with webpki root verification, and the HTTP/1.1
/// request/response round trip all succeeded. An http-only connector
/// dies before any HTTP status exists ("scheme is not http"), which is
/// the failure this test pins.
#[tokio::test]
#[ignore = "real network: POSTs to a live public-CA https endpoint"]
async fn https_real_endpoint_handshakes_and_gets_http_status() {
    let client = HttpSyncClient::new("https://git.metme.top", "https-probe".to_string());
    let error = client
        .pull("rs-cydrive-https-probe", None, 0)
        .await
        .expect_err("the probe namespace has no rows on the remote");
    let SyncError::Client(message) = &error else {
        panic!("expected a transport-level Client error, got: {error:?}");
    };
    let status = message
        .split("answered HTTP ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_default();
    assert!(
        !status.is_empty() && status.chars().all(|c| c.is_ascii_digit()),
        "expected a real HTTP status code in the error, got: {message}"
    );
}

/// Regression (review Low, L1): the per-request budget must cover
/// collecting the response *body*, not only the response headers.
/// Against an endpoint that answers headers promptly and then drips
/// the body forever, both `pull` and `push` must return the timeout
/// error within the injected 2s budget instead of hanging on the body
/// read. The 15s outer guard turns a regression back into a test
/// failure rather than a hung suite.
#[tokio::test]
async fn slow_body_drip_is_bounded_by_the_request_timeout() {
    let port = spawn_drip_body_endpoint().await;
    let url = format!("http://127.0.0.1:{port}");
    let client = HttpSyncClient::with_request_timeout(
        &url,
        Duration::from_secs(2),
        "drip-probe".to_string(),
    );

    let guard = Duration::from_secs(15);
    let error = tokio::time::timeout(guard, client.pull("ns", None, 0))
        .await
        .expect(
            "pull must return within the request budget instead of hanging on the trickling body",
        )
        .expect_err("the drip endpoint never completes a body; pull must fail");
    let SyncError::Client(message) = &error else {
        panic!("expected a transport-level Client error, got: {error:?}");
    };
    assert!(
        message.contains("/v1/pull") && message.contains("did not answer within"),
        "the error must carry the timeout semantics: {message}"
    );

    // Same contract on the push path (both endpoints share post_json).
    let error = tokio::time::timeout(guard, client.push("ns", None, &[]))
        .await
        .expect(
            "push must return within the request budget instead of hanging on the trickling body",
        )
        .expect_err("the drip endpoint never completes a body; push must fail");
    let SyncError::Client(message) = &error else {
        panic!("expected a transport-level Client error, got: {error:?}");
    };
    assert!(
        message.contains("/v1/push") && message.contains("did not answer within"),
        "the error must carry the timeout semantics: {message}"
    );
}
