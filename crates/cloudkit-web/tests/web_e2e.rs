//! E2E contract tests for the web dashboard (offline).
//!
//! Each test spins up a fresh environment — temp SQLite + cache tree +
//! `MockTransport` + Vfs + `WebUiServer` on an ephemeral loopback port
//! (`127.0.0.1:0`; production's 127.0.0.1:8088 contract is the caller's
//! concern) — and drives it with hand-rolled HTTP/1.1 over a raw
//! `TcpStream`, mirroring the `cloudkit-webdav` smoke-test harness. The
//! scenarios pin the Python `cydrive/web_ui/app.py` wire contract:
//! the six routes' JSON shapes field by field (the zero-change frontend
//! `static/js/app.js` consumes exactly these), the multipart upload
//! seam into `Vfs::put`, the single-call delete fix, hydrated downloads
//! and the Python 404 semantics for unknown files.
//!
//! The three M4 incremental routes (no Python baseline; frozen by the
//! Rust design doc) follow: `/api/list` (flat directory listing with
//! 400/404 semantics), single-range `Range` support on `/api/download`
//! (206/416, lenient fallback to 200) and `/api/queue` (queue counters
//! plus the DB pending tally).

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::{MockTransport, OpenRangeAction, UploadAction};
use cloudkit_core::transport::{CloudTransport, StorageError, UploadJob, UploadReceipt};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cloudkit_web::{WebUiConfig, WebUiServer};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

// ------------------------------------------------------------- helpers ---

/// VfsConfig for tests: tiny chunks, one worker, fast retry.
fn base_cfg() -> VfsConfig {
    VfsConfig {
        chunk_size_bytes: 64,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: None,
        encryption_scheme: cloudkit_core::config::EncryptionScheme::Gcm,
        hydrate_timeout: Duration::from_secs(180),
    }
}

/// The dashboard knobs: the `/api/stats` extra fields the frontend
/// reads (`drive_letter`) plus the URL glue Python derives from config.
/// The identity fields carry the test transport's own semantics
/// (`backend = "mock"` — the offline harness's MockTransport; no
/// volume on the CloudTransport face; the mock declares
/// `remote_delete = false`; no quota concept) — the backend-identity
/// test overrides them with a baidu-shaped config.
fn ui_cfg() -> WebUiConfig {
    WebUiConfig {
        drive_letter: Some("Y:".to_string()),
        webdav_url: "http://127.0.0.1:8080".to_string(),
        chat_id: 123456789,
        is_configured: true,
        backend: "mock".to_string(),
        volume: None,
        remote_delete: false,
        quota: None,
    }
}

/// One fresh offline environment per test: real SQLite, mirrored cache
/// tree, mock remote, Vfs and the server under test on an ephemeral
/// port.
struct Env {
    _dir: tempfile::TempDir,
    server: WebUiServer,
    db: Arc<MetaDatabase>,
    mock: Arc<MockTransport>,
    vfs: Arc<Vfs>,
}

/// Environment with caller-owned knobs (chunk split / retry policy /
/// scripted mock), pre-connecting the transport.
async fn env_with(cfg: VfsConfig, mock: Arc<MockTransport>) -> Env {
    env_with_ui(cfg, mock, ui_cfg()).await
}

/// [`env_with`] with caller-owned dashboard knobs — the backend-identity
/// contract test drives a baidu-shaped config through the same stack.
async fn env_with_ui(cfg: VfsConfig, mock: Arc<MockTransport>, ui: WebUiConfig) -> Env {
    let dir = tempfile::tempdir().expect("create temp dir");
    let cache_root = dir.path().join("cache");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    mock.connect().await.expect("pre-connect mock transport");
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Arc::new(Vfs::new(
        db.clone(),
        CacheManager::new(cache_root, u64::MAX),
        transport,
        cfg,
    ));
    let server = WebUiServer::serve(vfs.clone(), ui, SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("serve on an ephemeral loopback port");
    Env {
        _dir: dir,
        server,
        db,
        mock,
        vfs,
    }
}

/// Default environment (tiny chunks, always-succeeding mock).
async fn test_env() -> Env {
    env_with(base_cfg(), Arc::new(MockTransport::new())).await
}

/// Sends one raw HTTP/1.1 request (`Connection: close`) and reads the
/// full response bytes until the server closes the connection.
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

/// Builds an HTTP/1.1 request with Host, Connection: close and an exact
/// Content-Length for `body`.
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

/// Hand-rolled `multipart/form-data` body with exactly one field named
/// `file` (the only shape the frontend's `FormData.append("file", ...)`
/// produces, and the only one the Python handler reads).
fn multipart_body(boundary: &str, filename: &str, content: &str) -> String {
    format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n\
         {content}\r\n\
         --{boundary}--\r\n"
    )
}

/// A POST /api/upload request carrying `multipart_body`.
fn upload_request(addr: SocketAddr, boundary: &str, filename: &str, content: &str) -> String {
    request(
        "POST",
        "/api/upload",
        addr,
        &[(
            "Content-Type",
            &format!("multipart/form-data; boundary={boundary}"),
        )],
        &multipart_body(boundary, filename, content),
    )
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

/// Case-insensitive header lookup (headers end at the first empty line).
fn header<'a>(resp: &'a str, name: &str) -> Option<&'a str> {
    resp.lines().take_while(|l| !l.is_empty()).find_map(|l| {
        let (key, value) = l.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// The response body (everything after the blank line), dechunked when
/// the server answered `Transfer-Encoding: chunked`.
fn body_of(resp: &str) -> String {
    let (head, body) = resp
        .split_once("\r\n\r\n")
        .map_or(("", ""), |(head, body)| (head, body));
    if head
        .lines()
        .any(|l| l.eq_ignore_ascii_case("transfer-encoding: chunked"))
    {
        dechunk(body)
    } else {
        body.to_string()
    }
}

/// Decodes a chunked body (size-line / chunk / CRLF ... until a 0
/// chunk). Byte-wise on purpose: chunk boundaries may split multi-byte
/// UTF-8 sequences.
fn dechunk(mut body: &str) -> String {
    let mut out = Vec::new();
    while let Some((size_line, rest)) = body.split_once("\r\n") {
        let Ok(size) = usize::from_str_radix(size_line.trim(), 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let bytes = rest.as_bytes();
        if bytes.len() < size {
            break;
        }
        out.extend_from_slice(&bytes[..size]);
        body = &rest[size..];
        body = body.strip_prefix("\r\n").unwrap_or(body);
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The sorted key set of a JSON object.
fn keys_of(value: &serde_json::Value) -> Vec<String> {
    value
        .as_object()
        .expect("json object")
        .keys()
        .map(|key| key.to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Writes a row of any shape straight into the DB.
fn seed_row(db: &Arc<MetaDatabase>, rel: &str, is_dir: bool, size: i64, is_uploaded: bool) {
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let parent_dir = match rel_path.parent() {
        Some(parent) => parent.as_str().to_string(),
        None => "/".to_string(),
    };
    db.upsert_file(&FileUpsert {
        rel_path: rel_path.as_str().to_string(),
        name: rel_path.name().to_string(),
        parent_dir,
        size,
        mtime: 1_700_000_123.0,
        sha256: None,
        is_dir,
        telegram_msg_id: None,
        is_uploaded,
        is_cached: false,
        is_encrypted: false,
        chunk_count: if is_dir { 0 } else { 1 },
        mime_type: None,
    })
    .expect("seed row");
}

/// A directory row born uploaded + cached carrying a backend handle
/// (`telegram_msg_id` = the remote object id — the shape the K4
/// collection gate consumes; mirrors the fs_adapter helper).
fn seed_dir_row_with_handle(db: &Arc<MetaDatabase>, rel: &str, msg_id: i64) {
    let rel_path = RelPath::new(rel).expect("valid rel path");
    db.upsert_file(&FileUpsert {
        rel_path: rel_path.as_str().to_string(),
        name: rel_path.name().to_string(),
        parent_dir: rel_path
            .parent()
            .map(|parent| parent.as_str().to_string())
            .unwrap_or_else(|| "/".to_string()),
        size: 0,
        mtime: 1_700_000_000.0,
        sha256: None,
        is_dir: true,
        telegram_msg_id: Some(msg_id),
        is_uploaded: true,
        is_cached: true,
        is_encrypted: false,
        chunk_count: 0,
        mime_type: None,
    })
    .expect("seed dir row with handle");
}

/// Pushes `bytes` to the mock remote as `rel` (split at `chunk_size`).
async fn seed_remote(
    mock: &Arc<MockTransport>,
    rel: &str,
    bytes: &[u8],
    chunk_count: u32,
    chunk_size: u64,
) -> UploadReceipt {
    let dir = tempfile::tempdir().expect("seed scratch dir");
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let local_path = dir.path().join(rel_path.name());
    std::fs::write(&local_path, bytes).expect("write seed scratch file");
    mock.upload(&UploadJob {
        rel_path,
        local_path,
        size: bytes.len() as u64,
        chunk_count,
        chunk_size,
    })
    .await
    .expect("seed upload to the mock remote")
}

/// Inserts an uploaded `files` row for `rel` plus one chunk row per
/// receipt message (the common hydrate precondition).
fn seed_uploaded_row(
    db: &Arc<MetaDatabase>,
    rel: &str,
    bytes: &[u8],
    receipt: &UploadReceipt,
    chunk_size: u64,
) {
    let size = bytes.len() as i64;
    let chunk_count = receipt.chunk_msg_ids.len() as i64;
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let parent = rel_path.parent().expect("non-root path");
    let file_id = db
        .upsert_file(&FileUpsert {
            rel_path: rel_path.as_str().to_string(),
            name: rel_path.name().to_string(),
            parent_dir: parent.as_str().to_string(),
            size,
            mtime: 1_700_000_000.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: Some(receipt.first_msg_id),
            is_uploaded: true,
            is_cached: false,
            is_encrypted: false,
            chunk_count,
            mime_type: None,
        })
        .expect("seed files row");
    for (index, &msg_id) in receipt.chunk_msg_ids.iter().enumerate() {
        let index = index as i64;
        let chunk_row_size = if index + 1 < chunk_count {
            chunk_size as i64
        } else {
            size - (chunk_size as i64) * (chunk_count - 1)
        };
        db.upsert_chunk(file_id, index, msg_id, chunk_row_size, None)
            .expect("seed chunk row");
    }
}

/// Remote bytes plus the matching uploaded row in one shot.
async fn seed_remote_file(
    db: &Arc<MetaDatabase>,
    mock: &Arc<MockTransport>,
    rel: &str,
    bytes: &[u8],
    chunk_size: u64,
) {
    let chunk_count = bytes.len().div_ceil(chunk_size as usize).max(1) as u32;
    let receipt = seed_remote(mock, rel, bytes, chunk_count, chunk_size).await;
    seed_uploaded_row(db, rel, bytes, &receipt, chunk_size);
}

/// The exact key set of one `/api/files` row: Python's
/// `SELECT * FROM files` column order, key by key.
const FILE_ROW_KEYS: [&str; 16] = [
    "id",
    "rel_path",
    "name",
    "parent_dir",
    "size",
    "mtime",
    "sha256",
    "is_dir",
    "telegram_msg_id",
    "is_uploaded",
    "is_cached",
    "is_encrypted",
    "chunk_count",
    "mime_type",
    "created_at",
    "updated_at",
];

/// The exact key set of `/api/stats`: Python `get_stats()` plus the six
/// handler-glued dashboard fields (`webdav_host`/`webdav_port` included —
/// the Python response carries them even though app.js never reads
/// them; shape parity is the contract) and the five multi-backend
/// identity keys the dashboard adapter added on top (`webdav_url` was
/// already in the frozen set; it is now actually rendered).
const STATS_KEYS: [&str; 16] = [
    "total_files",
    "total_bytes",
    "total_dirs",
    "uploaded_files",
    "pending_uploads",
    "drive_letter",
    "webdav_host",
    "webdav_port",
    "webdav_url",
    "chat_id",
    "is_configured",
    "backend",
    "volume",
    "remote_delete",
    "quota_used",
    "quota_total",
];

// ------------------------------------------------------------ scenarios ---

/// 1. GET / serves the real index.html skeleton (the template is pure
///    static markup — no server-side variables — filled by app.js).
#[tokio::test]
async fn index_served_with_html() {
    let env = test_env().await;
    let addr = env.server.local_addr();

    let resp = send(addr, &request("GET", "/", addr, &[], "")).await;

    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let content_type = header(&resp, "content-type").expect("content-type");
    assert!(
        content_type.to_ascii_lowercase().starts_with("text/html"),
        "html content type: {resp}"
    );
    let body = body_of(&resp);
    assert!(
        body.contains("<title>CyDrive • Infinite Cloud Virtual Drive</title>"),
        "real index.html title: {body}"
    );
    assert!(body.contains("id=\"files-tbody\""), "files table anchor");
    assert!(body.contains("/static/js/app.js"), "frontend script wired");
}

/// 2. /static/* serves the embedded assets verbatim.
#[tokio::test]
async fn static_asset_served() {
    let env = test_env().await;
    let addr = env.server.local_addr();

    let resp = send(addr, &request("GET", "/static/js/app.js", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "js asset: {resp}");
    let content_type = header(&resp, "content-type").expect("content-type");
    assert!(
        content_type.to_ascii_lowercase().contains("javascript"),
        "js content type: {resp}"
    );
    let body = body_of(&resp);
    assert!(
        body.contains("loadDriveData"),
        "real app.js content: {body}"
    );
    assert!(body.contains("fetch(\"/api/stats\")"), "stats polling");

    let resp = send(
        addr,
        &request("GET", "/static/css/style.css", addr, &[], ""),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "css asset: {resp}");
    assert!(
        header(&resp, "content-type")
            .expect("content-type")
            .to_ascii_lowercase()
            .contains("text/css"),
        "css content type: {resp}"
    );
}

/// 3. GET /api/files mirrors Python's `SELECT *` row shape key by key,
///    with the exact fields app.js consumes (name / is_dir / size /
///    mtime / is_uploaded) carrying sane values.
#[tokio::test]
async fn api_files_shape_matches_frontend() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_row(&env.db, "/docs", true, 0, true);
    seed_row(&env.db, "/docs/hello.txt", false, 11, true);
    seed_row(&env.db, "/pending.txt", false, 150, false);

    let resp = send(addr, &request("GET", "/api/files", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    assert!(
        header(&resp, "content-type")
            .expect("content-type")
            .starts_with("application/json"),
        "json content type: {resp}"
    );

    let rows: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse files array");
    let rows = rows.as_array().expect("array body");
    assert_eq!(rows.len(), 3, "one row per seeded entry");

    let expected: BTreeSet<String> = FILE_ROW_KEYS.iter().map(|s| s.to_string()).collect();
    for row in rows {
        assert_eq!(
            keys_of(row).into_iter().collect::<BTreeSet<_>>(),
            expected,
            "row key set mirrors SELECT *: {row}"
        );
    }

    let hello = rows
        .iter()
        .find(|r| r["rel_path"] == "/docs/hello.txt")
        .expect("hello row");
    assert_eq!(hello["name"], "hello.txt", "app.js renders name");
    assert_eq!(hello["is_dir"], 0, "int flag like sqlite");
    assert_eq!(hello["size"], 11, "app.js formats size");
    assert_eq!(hello["mtime"], 1_700_000_123.0, "epoch seconds float");
    assert_eq!(hello["is_uploaded"], 1, "app.js badges Synced");

    let docs = rows
        .iter()
        .find(|r| r["rel_path"] == "/docs")
        .expect("docs row");
    assert_eq!(docs["is_dir"], 1, "directory row");
    let pending = rows
        .iter()
        .find(|r| r["rel_path"] == "/pending.txt")
        .expect("pending row");
    assert_eq!(pending["is_uploaded"], 0, "app.js badges Syncing");
}

/// 4. GET /api/stats carries the full key set — Python `get_stats()`
///    plus the extra dashboard fields — with correct aggregate values.
#[tokio::test]
async fn api_stats_shape_with_extra_fields() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_row(&env.db, "/photos", true, 0, true);
    seed_row(&env.db, "/photos/a.bin", false, 100, true);
    seed_row(&env.db, "/b.bin", false, 50, false);

    let resp = send(addr, &request("GET", "/api/stats", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");

    let stats: serde_json::Value =
        serde_json::from_str(&body_of(&resp)).expect("parse stats object");
    let expected: BTreeSet<String> = STATS_KEYS.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        keys_of(&stats).into_iter().collect::<BTreeSet<_>>(),
        expected,
        "exact key set: {stats}"
    );

    assert_eq!(stats["total_files"], 2, "non-dir rows");
    assert_eq!(stats["total_bytes"], 150, "sum over non-dir rows");
    assert_eq!(stats["total_dirs"], 1, "dir rows");
    assert_eq!(stats["uploaded_files"], 1, "uploaded non-dir rows");
    assert_eq!(stats["pending_uploads"], 1, "total - uploaded");
    assert_eq!(stats["drive_letter"], "Y:", "app.js drive badge");
    assert_eq!(stats["webdav_host"], "127.0.0.1");
    assert_eq!(stats["webdav_port"], 8080);
    assert_eq!(stats["webdav_url"], "http://127.0.0.1:8080");
    assert_eq!(stats["chat_id"], 123456789);
    assert_eq!(stats["is_configured"], true, "driven by WebUiConfig");
}

/// 4b. GET /api/stats exposes the active backend's identity — the
///     `backend` / `volume` / `remote_delete` / `quota_used` /
///     `quota_total` keys the multi-backend dashboard renders (the
///     frozen eleven keys above are untouched; this is the adapter's
///     additive contract). The baidu shape here is test-fixed values
///     through the mock transport — no live baidu connection; the web
///     layer reports the config verbatim (the per-backend assembly is
///     the run flow's concern, cli side).
#[tokio::test]
async fn stats_exposes_backend_identity_and_quota() {
    let ui = WebUiConfig {
        backend: "baidu".to_string(),
        volume: Some("baidu:42".to_string()),
        remote_delete: true,
        quota: Some(cloudkit_web::QuotaSnapshot {
            used: 5,
            total: Some(10),
        }),
        ..ui_cfg()
    };
    let env = env_with_ui(base_cfg(), Arc::new(MockTransport::new()), ui).await;
    let addr = env.server.local_addr();

    let resp = send(addr, &request("GET", "/api/stats", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");

    let stats: serde_json::Value =
        serde_json::from_str(&body_of(&resp)).expect("parse stats object");
    assert_eq!(
        stats["backend"], "baidu",
        "the active backend's stable config spelling"
    );
    assert_eq!(
        stats["volume"], "baidu:42",
        "the dispatched volume identity"
    );
    assert_eq!(
        stats["remote_delete"], true,
        "the K4 bit the delete-confirm UX gates on"
    );
    assert_eq!(stats["quota_used"], 5, "boot quota snapshot, used bytes");
    assert_eq!(stats["quota_total"], 10, "boot quota snapshot, total bytes");
    assert_eq!(
        stats["webdav_url"], "http://127.0.0.1:8080",
        "frozen key, now actually rendered by the WebDAV card"
    );
}

/// 4c. The absent-identity face of the same contract: a config without
///     volume and quota serializes those as JSON `null` (not missing
///     keys — the exact key set above stays stable across backends), so
///     the frontend's "unlimited" branch keys off real nulls.
#[tokio::test]
async fn stats_volume_and_quota_null_when_absent() {
    let env = test_env().await;
    let addr = env.server.local_addr();

    let resp = send(addr, &request("GET", "/api/stats", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");

    let stats: serde_json::Value =
        serde_json::from_str(&body_of(&resp)).expect("parse stats object");
    assert_eq!(stats["backend"], "mock", "test transport's own semantics");
    assert!(
        stats["volume"].is_null(),
        "no volume on the CloudTransport face: null, not missing"
    );
    assert_eq!(stats["remote_delete"], false, "mock declares the bit off");
    assert!(stats["quota_used"].is_null(), "no snapshot: null");
    assert!(stats["quota_total"].is_null(), "no snapshot: null");
}

/// 5. POST /api/upload (multipart field `file`) answers the Python
///    success shape, lands a pending row (accepted ≠ uploaded), and the
///    shutdown drain pushes it through the queue to the mock remote.
///
///    The pending window is made deterministic by scripting the mock's
///    first upload attempt to fail with a 500ms retry backoff: the row
///    cannot flip to uploaded before the backoff elapses, while the
///    drain after `Vfs::shutdown` waits out the retry and succeeds.
#[tokio::test]
async fn upload_multipart_roundtrip() {
    let mut cfg = base_cfg();
    cfg.retry = RetryPolicy {
        initial_backoff: Duration::from_millis(500),
        max_backoff: Duration::from_millis(600),
        max_attempts: 3,
    };
    let mock = Arc::new(
        MockTransport::builder()
            .upload_action(UploadAction::Fail {
                error: StorageError::Unavailable("scripted first-attempt failure".to_string()),
            })
            .build(),
    );
    let env = env_with(cfg, mock).await;
    let addr = env.server.local_addr();

    let resp = send(
        addr,
        &upload_request(addr, "cydrive-boundary", "hello.txt", "web upload body"),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "accepted: {resp}");
    let json: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse response");
    assert_eq!(
        json,
        serde_json::json!({"success": true, "filename": "hello.txt"}),
        "Python success shape"
    );

    let row = env
        .db
        .get_file("/hello.txt")
        .expect("db read")
        .expect("row exists");
    assert!(!row.is_uploaded, "row is pending the instant we return");
    assert_eq!(row.size, 15, "staged size");
    assert!(row.is_cached, "cache copy staged");

    env.server.shutdown().await;
    env.vfs.shutdown().await; // waits out the 500ms retry, then uploads

    let row = env
        .db
        .get_file("/hello.txt")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "drain uploaded the file");
    assert_eq!(
        env.mock.upload_calls().len(),
        2,
        "one failed attempt plus one successful retry"
    );
    let msg_id = row.telegram_msg_id.expect("chunk-0 msg id recorded");
    assert_eq!(
        env.mock.message(msg_id),
        Some(b"web upload body".to_vec()),
        "mock remote stores the exact bytes"
    );
}

/// 6. A >2MB upload body succeeds — proving DefaultBodyLimit was raised
///    past axum's 2MB default (the 1900MB chunk threshold needs it).
#[tokio::test]
async fn upload_exceeds_axum_default_limit() {
    let mut cfg = base_cfg();
    cfg.chunk_size_bytes = 8 * 1024 * 1024; // one chunk for a 3MB body
    let env = env_with(cfg, Arc::new(MockTransport::new())).await;
    let addr = env.server.local_addr();

    let content = "0123456789abcdef".repeat(3 * 1024 * 1024 / 16); // 3 MiB
    let resp = send(
        addr,
        &upload_request(addr, "big-boundary", "big.bin", &content),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "3MiB body accepted: {resp}");

    env.server.shutdown().await;
    env.vfs.shutdown().await;

    let row = env
        .db
        .get_file("/big.bin")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "drain uploaded the big file");
    assert_eq!(row.size, 3 * 1024 * 1024);
    assert_eq!(env.mock.upload_calls().len(), 1, "one upload job");
}

/// 7. POST /api/delete removes the row and never touches the remote
///    (baseline mirror) — with a single `delete_file` call, fixing the
///    Python double-call defect per the design doc.
#[tokio::test]
async fn delete_removes_row_single_call() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_row(&env.db, "/doomed.txt", false, 7, true);

    let resp = send(
        addr,
        &request(
            "POST",
            "/api/delete",
            addr,
            &[("Content-Type", "application/json")],
            r#"{"filename":"doomed.txt"}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let json: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse response");
    assert_eq!(
        json,
        serde_json::json!({"success": true, "deleted": "doomed.txt"}),
        "Python success shape echoes the raw filename"
    );

    assert!(
        env.db.get_file("/doomed.txt").expect("db read").is_none(),
        "row gone after one call"
    );
    assert!(
        env.mock.deleted().is_empty(),
        "baseline mirror: delete never propagates to the remote"
    );
    assert!(
        env.mock.upload_calls().is_empty(),
        "delete enqueues no upload"
    );
}

/// 7a (K4 / B3b 段二b): with the transport declaring `remote_delete`,
/// the dashboard's file delete propagates to the remote object (through
/// the shared `Vfs::remove_file` gate) before the row dies.
#[tokio::test]
async fn delete_remote_gated_removes_remote_object() {
    let mock = Arc::new(
        MockTransport::builder()
            .capabilities(cloudkit_core::transport::Capabilities {
                remote_delete: true,
                ..cloudkit_core::transport::Capabilities::none()
            })
            .build(),
    );
    let env = env_with(base_cfg(), mock).await;
    let addr = env.server.local_addr();
    let receipt = seed_remote(&env.mock, "/gated.txt", b"payload", 1, 64).await;
    seed_uploaded_row(&env.db, "/gated.txt", b"payload", &receipt, 64);

    let resp = send(
        addr,
        &request(
            "POST",
            "/api/delete",
            addr,
            &[("Content-Type", "application/json")],
            r#"{"filename":"gated.txt"}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    assert!(
        env.db.get_file("/gated.txt").expect("db read").is_none(),
        "row gone"
    );
    assert_eq!(
        env.mock.deleted(),
        vec![receipt.first_msg_id],
        "the remote object died with the row (remote_delete=true)"
    );

    env.server.shutdown().await;
    env.vfs.shutdown().await;
}

/// 7b (K4): the dashboard's DIRECTORY delete fallback (the baseline's
/// unconditional dir-row delete) gates the same way — a dir row with a
/// backend handle has its remote object deleted before the row dies; a
/// refused remote delete answers 500 with the row kept.
#[tokio::test]
async fn delete_dir_remote_gated_and_refusal_keeps_row() {
    let mock = Arc::new(
        MockTransport::builder()
            .capabilities(cloudkit_core::transport::Capabilities {
                remote_delete: true,
                ..cloudkit_core::transport::Capabilities::none()
            })
            .build(),
    );
    let env = env_with(base_cfg(), mock).await;
    let addr = env.server.local_addr();

    // Gated success leg: a dir row carrying a remote handle.
    let receipt = seed_remote(&env.mock, "/docs", b"dir-marker", 1, 64).await;
    seed_dir_row_with_handle(&env.db, "/docs", receipt.first_msg_id);
    let resp = send(
        addr,
        &request(
            "POST",
            "/api/delete",
            addr,
            &[("Content-Type", "application/json")],
            r#"{"filename":"docs"}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "dir delete ok: {resp}");
    assert_eq!(
        env.mock.deleted(),
        vec![receipt.first_msg_id],
        "the dir's remote object died before the row"
    );
    assert!(
        env.db.get_file("/docs").expect("db read").is_none(),
        "the dir row is gone"
    );

    // Refused leg: two scripted refusals (the gate retries once) keep
    // the row and answer 500.
    let refused = || {
        Err(cloudkit_core::transport::StorageError::Unavailable(
            "backend down".into(),
        ))
    };
    let mock2 = Arc::new(
        MockTransport::builder()
            .capabilities(cloudkit_core::transport::Capabilities {
                remote_delete: true,
                ..cloudkit_core::transport::Capabilities::none()
            })
            .delete_action(refused())
            .delete_action(refused())
            .build(),
    );
    let env2 = env_with(base_cfg(), mock2).await;
    let addr2 = env2.server.local_addr();
    let receipt2 = seed_remote(&env2.mock, "/stuck", b"dir-marker", 1, 64).await;
    seed_dir_row_with_handle(&env2.db, "/stuck", receipt2.first_msg_id);

    let resp = send(
        addr2,
        &request(
            "POST",
            "/api/delete",
            addr2,
            &[("Content-Type", "application/json")],
            r#"{"filename":"stuck"}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 500, "the refusal answers 500: {resp}");
    assert!(
        env2.db.get_file("/stuck").expect("db read").is_some(),
        "the dir row survives the refused remote delete"
    );

    env.server.shutdown().await;
    env.vfs.shutdown().await;
    env2.server.shutdown().await;
    env2.vfs.shutdown().await;
}

/// 8. GET /api/download hydrates through the VFS and streams the exact
///    bytes with Python's inline disposition header.
#[tokio::test]
async fn download_returns_hydrated_bytes() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_remote_file(&env.db, &env.mock, "/blob.bin", b"payload", 64).await;

    let resp = send(
        addr,
        &request("GET", "/api/download/blob.bin", addr, &[], ""),
    )
    .await;

    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    assert_eq!(body_of(&resp), "payload", "hydrated bytes");
    assert_eq!(header(&resp, "content-length"), Some("7"));
    assert_eq!(
        header(&resp, "content-disposition"),
        Some("inline; filename=\"blob.bin\""),
        "Python inline disposition: {resp}"
    );
    assert_eq!(
        header(&resp, "content-type"),
        Some("application/octet-stream"),
        "mime from the filename like mimetypes.guess_type"
    );
}

/// 9. GET /api/download of an unknown file answers Python's 404 body.
#[tokio::test]
async fn unknown_download_404() {
    let env = test_env().await;
    let addr = env.server.local_addr();

    let resp = send(
        addr,
        &request("GET", "/api/download/nope.txt", addr, &[], ""),
    )
    .await;

    assert_eq!(status_of(&resp), 404, "not found: {resp}");
    assert_eq!(
        body_of(&resp),
        "File not found in CyDrive cloud",
        "Python 404 body verbatim"
    );
}

/// The exact key set of `GET /api/queue`: the four queue counters plus
/// the DB pending tally.
const QUEUE_KEYS: [&str; 5] = ["enqueued", "succeeded", "degraded", "retries", "pending"];

/// 10. GET /api/list without a `path` defaults to the root and answers
///     the root's direct children flat (the frontend builds the tree),
///     each entry shaped exactly like an `/api/files` row.
#[tokio::test]
async fn list_root_default_and_shape() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_row(&env.db, "/docs", true, 0, true);
    seed_row(&env.db, "/readme.txt", false, 11, true);

    let resp = send(addr, &request("GET", "/api/list", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    assert!(
        header(&resp, "content-type")
            .expect("content-type")
            .starts_with("application/json"),
        "json content type: {resp}"
    );

    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse list body");
    assert_eq!(body["path"], "/", "default path is the root");
    let entries = body["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 2, "one entry per direct root child");

    let expected: BTreeSet<String> = FILE_ROW_KEYS.iter().map(|s| s.to_string()).collect();
    for entry in entries {
        assert_eq!(
            keys_of(entry).into_iter().collect::<BTreeSet<_>>(),
            expected,
            "entry key set mirrors /api/files rows: {entry}"
        );
    }
    let paths: BTreeSet<&str> = entries
        .iter()
        .map(|e| e["rel_path"].as_str().expect("rel_path string"))
        .collect();
    assert_eq!(
        paths,
        BTreeSet::from(["/docs", "/readme.txt"]),
        "direct children only, flat"
    );

    // An explicit `path=/` answers the same listing.
    let resp = send(addr, &request("GET", "/api/list?path=/", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "explicit root ok: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse list body");
    assert_eq!(body["path"], "/");
    assert_eq!(body["entries"].as_array().expect("entries").len(), 2);
}

/// 11. GET /api/list?path=/docs lists only /docs's direct children; the
///     leading-slash guarantee and the backslash fold normalize the
///     query value first.
#[tokio::test]
async fn list_subdir_entries() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_row(&env.db, "/docs", true, 0, true);
    seed_row(&env.db, "/docs/a.txt", false, 3, true);
    seed_row(&env.db, "/docs/nested", true, 0, true);
    seed_row(&env.db, "/other/b.txt", false, 4, true);
    seed_row(&env.db, "/root.txt", false, 5, true);

    let resp = send(addr, &request("GET", "/api/list?path=/docs", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse list body");
    assert_eq!(body["path"], "/docs", "normalized path echoed");
    let entries = body["entries"].as_array().expect("entries array");
    let paths: BTreeSet<&str> = entries
        .iter()
        .map(|e| e["rel_path"].as_str().expect("rel_path string"))
        .collect();
    assert_eq!(
        paths,
        BTreeSet::from(["/docs/a.txt", "/docs/nested"]),
        "only /docs children — no grandchildren, no siblings, not /docs itself"
    );

    // No leading slash: normalized onto `/docs`.
    let resp = send(addr, &request("GET", "/api/list?path=docs", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "leading slash guaranteed: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse list body");
    assert_eq!(body["path"], "/docs");
    assert_eq!(body["entries"].as_array().expect("entries").len(), 2);

    // Backslash query (`%5C`) folds onto the separator.
    let resp = send(
        addr,
        &request("GET", "/api/list?path=%5Cdocs", addr, &[], ""),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "backslash folded: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse list body");
    assert_eq!(body["path"], "/docs");
}

/// 12. Directory existence semantics: a path with neither a row nor any
///     child 404s, while an existing-but-empty directory answers 200
///     with an empty `entries` array.
#[tokio::test]
async fn list_missing_dir_404_but_empty_dir_ok() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_row(&env.db, "/empty", true, 0, true);

    let resp = send(
        addr,
        &request("GET", "/api/list?path=/missing", addr, &[], ""),
    )
    .await;
    assert_eq!(status_of(&resp), 404, "no row and no children: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse error body");
    assert_eq!(body, serde_json::json!({"error": "Directory not found"}));

    let resp = send(
        addr,
        &request("GET", "/api/list?path=/empty", addr, &[], ""),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "empty but existing: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse list body");
    assert_eq!(body["path"], "/empty");
    assert_eq!(body["entries"], serde_json::json!([]), "empty entry list");
}

/// 13. Path pollution in `?path=` answers 400 with the uniform error
///     shape (`..` traversal, empty segments).
#[tokio::test]
async fn list_invalid_path_400() {
    let env = test_env().await;
    let addr = env.server.local_addr();

    for bad in ["/../etc", "/a//b"] {
        let resp = send(
            addr,
            &request("GET", &format!("/api/list?path={bad}"), addr, &[], ""),
        )
        .await;
        assert_eq!(status_of(&resp), 400, "rejected {bad}: {resp}");
        let body: serde_json::Value =
            serde_json::from_str(&body_of(&resp)).expect("parse error body");
        assert_eq!(body, serde_json::json!({"error": "Invalid path"}));
    }
}

/// 14. GET /api/download honors a single-byte `Range`: 206 with an exact
///     slice and `Content-Range`, including the open-ended and suffix
///     forms (standard single-range semantics, aiohttp FileResponse
///     baseline).
#[tokio::test]
async fn download_range_serves_206_slice() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_remote_file(&env.db, &env.mock, "/blob.bin", b"payload", 64).await;

    let resp = send(
        addr,
        &request(
            "GET",
            "/api/download/blob.bin",
            addr,
            &[("Range", "bytes=2-4")],
            "",
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 206, "partial content: {resp}");
    assert_eq!(header(&resp, "content-range"), Some("bytes 2-4/7"));
    assert_eq!(header(&resp, "content-length"), Some("3"));
    assert_eq!(body_of(&resp), "ylo", "exact slice bytes[2..=4]");

    // Open-ended `a-` runs to the last byte.
    let resp = send(
        addr,
        &request(
            "GET",
            "/api/download/blob.bin",
            addr,
            &[("Range", "bytes=5-")],
            "",
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 206, "open-ended range: {resp}");
    assert_eq!(header(&resp, "content-range"), Some("bytes 5-6/7"));
    assert_eq!(body_of(&resp), "ad");

    // Suffix `-n` covers the final n bytes.
    let resp = send(
        addr,
        &request(
            "GET",
            "/api/download/blob.bin",
            addr,
            &[("Range", "bytes=-3")],
            "",
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 206, "suffix range: {resp}");
    assert_eq!(header(&resp, "content-range"), Some("bytes 4-6/7"));
    assert_eq!(body_of(&resp), "oad");
}

/// 15. Range edge cases: a start past the end answers 416 with
///     `Content-Range: bytes */size`, while malformed or multi-range
///     headers are ignored and the full body is served with 200.
#[tokio::test]
async fn download_range_out_of_bounds_416_and_malformed_ignored() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_remote_file(&env.db, &env.mock, "/blob.bin", b"payload", 64).await;

    let resp = send(
        addr,
        &request(
            "GET",
            "/api/download/blob.bin",
            addr,
            &[("Range", "bytes=7-9")],
            "",
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 416, "unsatisfiable range: {resp}");
    assert_eq!(header(&resp, "content-range"), Some("bytes */7"));

    for ignored in ["bytes=abc", "bytes=0-1,3-4"] {
        let resp = send(
            addr,
            &request(
                "GET",
                "/api/download/blob.bin",
                addr,
                &[("Range", ignored)],
                "",
            ),
        )
        .await;
        assert_eq!(status_of(&resp), 200, "ignored Range {ignored}: {resp}");
        assert_eq!(
            header(&resp, "content-range"),
            None,
            "no Content-Range on a full body"
        );
        assert_eq!(body_of(&resp), "payload", "full body on ignored Range");
        assert_eq!(header(&resp, "content-length"), Some("7"));
    }
}

/// 15a. SR2 streaming: a single-range GET on a streamable row answers
///      206 straight out of one bounded `open_range` window — the mock
///      never sees a full `open`, and the window it does see is exactly
///      the requested slice (the transport slices further on its own;
///      the web layer asks for the whole range).
#[tokio::test]
async fn download_range_streams_the_window_not_the_file() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    let content = ascii_pattern(100);
    seed_remote_file(&env.db, &env.mock, "/stream.bin", &content, 64).await;

    let resp = send(
        addr,
        &request(
            "GET",
            "/api/download/stream.bin",
            addr,
            &[("Range", "bytes=10-29")],
            "",
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 206, "partial content: {resp}");
    assert_eq!(header(&resp, "content-range"), Some("bytes 10-29/100"));
    assert_eq!(header(&resp, "content-length"), Some("20"));
    assert_eq!(header(&resp, "accept-ranges"), Some("bytes"));
    assert_eq!(
        header(&resp, "content-disposition"),
        Some("inline; filename=\"stream.bin\"")
    );
    assert_eq!(
        body_of(&resp),
        String::from_utf8_lossy(&content[10..30]).into_owned(),
        "exact slice bytes[10..=29]"
    );
    assert!(
        env.mock.open_calls().is_empty(),
        "the streaming path never asks for a full open"
    );
    assert_eq!(
        env.mock.open_range_calls(),
        vec![(10, 20)],
        "one window: exactly the requested range"
    );
}

/// 15b. SR2 streaming: a plain GET streams the whole file as one
///      `(0, total)` window with an explicit Content-Length
///      (`Body::from_stream` carries no length of its own).
#[tokio::test]
async fn download_without_range_streams_full_window() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    let content = ascii_pattern(100);
    seed_remote_file(&env.db, &env.mock, "/stream.bin", &content, 64).await;

    let resp = send(
        addr,
        &request("GET", "/api/download/stream.bin", addr, &[], ""),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    assert_eq!(header(&resp, "content-length"), Some("100"));
    assert_eq!(header(&resp, "accept-ranges"), Some("bytes"));
    assert_eq!(
        body_of(&resp),
        String::from_utf8_lossy(&content).into_owned(),
        "full body streamed"
    );
    assert!(env.mock.open_calls().is_empty(), "no full open");
    assert_eq!(
        env.mock.open_range_calls(),
        vec![(0, 100)],
        "one full-length window"
    );
}

/// 15c. SR2 streaming: an unsatisfiable Range answers 416 with the
///      `bytes */size` Content-Range and never reaches the remote —
///      neither a full open nor a window.
#[tokio::test]
async fn download_unsatisfiable_range_416_touches_no_remote() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    let content = ascii_pattern(100);
    seed_remote_file(&env.db, &env.mock, "/stream.bin", &content, 64).await;

    let resp = send(
        addr,
        &request(
            "GET",
            "/api/download/stream.bin",
            addr,
            &[("Range", "bytes=100-")],
            "",
        ),
    )
    .await;
    assert_eq!(
        status_of(&resp),
        416,
        "start == size is unsatisfiable: {resp}"
    );
    assert_eq!(header(&resp, "content-range"), Some("bytes */100"));
    assert!(
        env.mock.open_calls().is_empty() && env.mock.open_range_calls().is_empty(),
        "the 416 is decided off the row size alone — zero remote calls"
    );
}

/// 15c2. SR2 hardening: a body longer than one streaming window is
///        served through MULTIPLE bounded `open_range` calls (at most
///        4 MiB each), never one whole-length call — the open-ended
///        `Range: bytes=0-` shape browsers and players send for video
///        playback included. Bytes stay exact; the 206/Content-Range/
///        Content-Length contract is unchanged.
#[tokio::test]
async fn download_open_ended_range_over_one_window_streams_bounded_windows() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    // 5 MiB: strictly more than one 4 MiB streaming window.
    let content = ascii_pattern(5 * 1024 * 1024);
    seed_remote_file(&env.db, &env.mock, "/big.bin", &content, 1024 * 1024).await;

    let resp = send(
        addr,
        &request(
            "GET",
            "/api/download/big.bin",
            addr,
            &[("Range", "bytes=0-")],
            "",
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 206, "partial content: {resp}");
    assert_eq!(
        header(&resp, "content-range"),
        Some("bytes 0-5242879/5242880")
    );
    assert_eq!(header(&resp, "content-length"), Some("5242880"));
    assert_eq!(header(&resp, "accept-ranges"), Some("bytes"));
    assert_eq!(
        body_of(&resp),
        String::from_utf8_lossy(&content).into_owned(),
        "byte-exact full body across the window seams"
    );
    assert!(env.mock.open_calls().is_empty(), "no full open");
    assert_eq!(
        env.mock.open_range_calls(),
        vec![(0, 4 * 1024 * 1024), (4 * 1024 * 1024, 1024 * 1024)],
        "one bounded ≤4 MiB window per open_range call"
    );
}

/// 15d. SR2 fallback (R-5): an encrypted row never streams — the
///      download serves through the full hydrate path, and the mock's
///      observation face shows the contrast with 15a/15b: a full `open`,
///      never a window.
#[tokio::test]
async fn download_encrypted_row_hydrates_through_full_open() {
    let mut cfg = base_cfg();
    cfg.encryption_password = Some("secret".to_string());
    let env = env_with(cfg, Arc::new(MockTransport::new())).await;
    let addr = env.server.local_addr();
    let rel = RelPath::new("/enc.bin").expect("valid rel path");
    env.vfs
        .put(&rel, b"plaintext secret", 1_700_000_000.0)
        .await
        .expect("stage the encrypted upload");
    // Drain the queue: the row uploads, and the successful upload drops
    // the local plaintext copy, so the download must hydrate remotely.
    env.vfs.shutdown().await;

    let resp = send(
        addr,
        &request("GET", "/api/download/enc.bin", addr, &[], ""),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "decrypted ok: {resp}");
    assert_eq!(body_of(&resp), "plaintext secret", "decrypted plaintext");
    assert_eq!(
        env.mock.open_calls().len(),
        1,
        "encrypted row hydrates through one full open"
    );
    assert!(
        env.mock.open_range_calls().is_empty(),
        "encrypted row never serves through a window"
    );
}

/// 15e. SR2 fallback: a 0-byte row (hydrate materializes the empty
///      copy) answers 200 with an empty body — and never touches the
///      remote at all.
#[tokio::test]
async fn download_zero_byte_row_hydrates_empty_200() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_remote_file(&env.db, &env.mock, "/empty.bin", b"", 64).await;

    let resp = send(
        addr,
        &request("GET", "/api/download/empty.bin", addr, &[], ""),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    assert_eq!(header(&resp, "content-length"), Some("0"));
    assert_eq!(body_of(&resp), "", "empty body");
    assert!(
        env.mock.open_calls().is_empty() && env.mock.open_range_calls().is_empty(),
        "a 0-byte row has no remote bytes to fetch"
    );
}

/// 15f. SR2 streaming failure mode: a mid-stream transport error (the
///      mock's `FailAfterBytes` script) truncates the body — the head
///      and the leading bytes were already served, so HTTP cannot
///      change the status; the connection just ends short of the
///      promised Content-Length.
#[tokio::test]
async fn download_mid_stream_error_truncates_the_body() {
    let mock = Arc::new(
        MockTransport::builder()
            .open_range_action(OpenRangeAction::FailAfterBytes {
                bytes: 3,
                error: StorageError::Unavailable("scripted mid-stream disconnect".to_string()),
            })
            .build(),
    );
    let env = env_with(base_cfg(), mock).await;
    let addr = env.server.local_addr();
    let content = ascii_pattern(100);
    seed_remote_file(&env.db, &env.mock, "/stream.bin", &content, 64).await;

    let resp = send(
        addr,
        &request("GET", "/api/download/stream.bin", addr, &[], ""),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "the head was already sent: {resp}");
    assert_eq!(
        header(&resp, "content-length"),
        Some("100"),
        "the promise stands at the total"
    );
    // What arrives is a strict prefix of the file — 0..=3 bytes; hyper
    // may or may not flush the buffered prefix frame before the abort,
    // and both are the same contract: truncated, never fabricated.
    let body = body_of(&resp);
    assert!(
        body.len() < 100
            && body
                .as_bytes()
                .iter()
                .zip(content.iter())
                .all(|(got, want)| got == want),
        "body truncated short of the promised 100 bytes (got {}), prefix-consistent",
        body.len()
    );
    assert_eq!(env.mock.open_range_calls(), vec![(0, 100)]);
    assert!(env.mock.open_calls().is_empty());
}

/// A deterministic ASCII pattern (`a`-z cycling): every byte survives
/// the harness's String-based body decoding losslessly, unlike a binary
/// pattern whose high bytes would round-trip through lossy UTF-8.
fn ascii_pattern(len: usize) -> Vec<u8> {
    (0..len).map(|index| b'a' + (index % 26) as u8).collect()
}

/// 16. GET /api/queue reports the four upload-queue counters plus the
///     DB pending tally; after one upload drains, the numbers agree with
///     the library reads.
#[tokio::test]
async fn queue_endpoint_counters() {
    let env = test_env().await;
    let addr = env.server.local_addr();

    let resp = send(
        addr,
        &upload_request(addr, "queue-boundary", "hello.txt", "queued body"),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "upload accepted: {resp}");

    // The queue drains asynchronously; poll until the job succeeded.
    // (persist_success flips the DB row before `succeeded` increments,
    // so a >=1 reading implies the DB is settled too.)
    let mut queue = None;
    for _ in 0..200 {
        let resp = send(addr, &request("GET", "/api/queue", addr, &[], "")).await;
        assert_eq!(status_of(&resp), 200, "queue ok: {resp}");
        let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse queue");
        if body["succeeded"].as_u64().unwrap_or(0) >= 1 {
            queue = Some(body);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let queue = queue.expect("upload drained within the poll window");

    let expected: BTreeSet<String> = QUEUE_KEYS.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        keys_of(&queue).into_iter().collect::<BTreeSet<_>>(),
        expected,
        "exact key set: {queue}"
    );
    assert_eq!(queue["enqueued"], 1, "one job accepted");
    assert_eq!(queue["succeeded"], 1, "one job uploaded");
    assert_eq!(queue["degraded"], 0, "always-succeeding mock");
    assert_eq!(queue["retries"], 0, "no scripted failures");
    let db_pending = env.db.get_stats().expect("db stats").pending_uploads;
    assert_eq!(
        queue["pending"].as_i64(),
        Some(db_pending),
        "pending mirrors the DB"
    );
    assert_eq!(db_pending, 0, "drained upload is no longer pending");
}

/// 17 (plan F2 / review H2): deleting a pending upload whose local
///     cache copy still exists is the dashboard's face of the three-
///     surface guard (core `VfsError::UploadPending`, WebDAV
///     `FsError::Forbidden`): the route answers 409 Conflict with the
///     uniform `{"error": ...}` body and the row — whose cache copy is
///     the only copy of the bytes — survives; a ghost pending row (copy
///     already vanished, bytes nowhere) deletes normally.
#[tokio::test]
async fn delete_pending_upload_conflict() {
    let env = test_env().await;
    let addr = env.server.local_addr();

    // Pending row + its local copy in the mirrored cache tree (the same
    // tree `env_with` hands the Vfs): the only copy of the bytes.
    seed_row(&env.db, "/conflict.bin", false, 7, false);
    let paths = CacheManager::new(env._dir.path().join("cache"), u64::MAX);
    let rel = RelPath::new("/conflict.bin").expect("valid rel path");
    let local = paths.local_path(&rel);
    std::fs::create_dir_all(local.parent().expect("cache parent dir")).expect("create cache dirs");
    std::fs::write(&local, b"only local copy").expect("write the only local copy");

    let resp = send(
        addr,
        &request(
            "POST",
            "/api/delete",
            addr,
            &[("Content-Type", "application/json")],
            r#"{"filename":"conflict.bin"}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 409, "pending upload conflicts: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse error body");
    assert!(
        body["error"].as_str().is_some_and(|msg| !msg.is_empty()),
        "uniform error body explains the refusal: {body}"
    );
    assert!(
        env.db
            .get_file("/conflict.bin")
            .expect("db read")
            .is_some_and(|row| !row.is_uploaded),
        "the pending row survives the refused delete"
    );
    assert!(
        local.exists(),
        "the only copy of the bytes survives the refused delete"
    );

    // Ghost pending row: no local copy — deletes normally.
    seed_row(&env.db, "/ghost.bin", false, 7, false);
    let resp = send(
        addr,
        &request(
            "POST",
            "/api/delete",
            addr,
            &[("Content-Type", "application/json")],
            r#"{"filename":"ghost.bin"}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "ghost row deletes: {resp}");
    assert!(
        env.db.get_file("/ghost.bin").expect("db read").is_none(),
        "the ghost row is gone"
    );
}

/// Drain any wake permit stored by seeding writes (`notify_one` keeps a
/// permit when no waiter is enabled), so the assertion can only pass on
/// the operation under test — not on seeding echoes.
async fn drain_stale_wake_permits(notifier: &tokio::sync::Notify) {
    while tokio::time::timeout(Duration::from_millis(50), notifier.notified())
        .await
        .is_ok()
    {}
}

/// 18 (review High-2): the dashboard's delete rings the sync wake —
///     a deletion is the tombstone's origin; the route must not
///     bypass the VFS layer (and its doorbell ring) with a direct db
///     write. Asserted on an uploaded row (the deletable shape).
#[tokio::test]
async fn delete_rings_the_sync_wake() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_row(&env.db, "/wake-doomed.txt", false, 5, true);

    let notifier = env.vfs.sync_notifier();
    drain_stale_wake_permits(&notifier).await;
    let wake = notifier.notified();
    tokio::pin!(wake);
    wake.as_mut().enable();

    let resp = send(
        addr,
        &request(
            "POST",
            "/api/delete",
            addr,
            &[("Content-Type", "application/json")],
            r#"{"filename":"wake-doomed.txt"}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");

    tokio::time::timeout(Duration::from_secs(1), wake.as_mut())
        .await
        .expect("delete must ring the sync wake");
}
