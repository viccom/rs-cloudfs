//! E2E contract tests for the web dashboard (offline).
//!
//! Each test spins up a fresh environment — temp SQLite + cache tree +
//! `MockTransport` + Vfs + `WebUiServer` on an ephemeral loopback port
//! (`127.0.0.1:0`; production's 127.0.0.1:8088 contract is the caller's
//! concern) — and drives it with hand-rolled HTTP/1.1 over a raw
//! `TcpStream`, mirroring the `cydrive-webdav` smoke-test harness. The
//! scenarios pin the Python `cydrive/web_ui/app.py` wire contract:
//! the six routes' JSON shapes field by field (the zero-change frontend
//! `static/js/app.js` consumes exactly these), the multipart upload
//! seam into `Vfs::put`, the single-call delete fix, hydrated downloads
//! and the Python 404 semantics for unknown files.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use cydrive_core::cache::CacheManager;
use cydrive_core::database::{FileUpsert, MetaDatabase};
use cydrive_core::rel_path::RelPath;
use cydrive_core::transport::mock::{MockTransport, UploadAction};
use cydrive_core::transport::{CloudTransport, TransportError, UploadJob, UploadReceipt};
use cydrive_core::upload_queue::RetryPolicy;
use cydrive_core::vfs::{Vfs, VfsConfig};
use cydrive_web::{WebUiConfig, WebUiServer};
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
        hydrate_timeout: Duration::from_secs(180),
    }
}

/// The dashboard knobs: the four `/api/stats` extra fields the frontend
/// reads (`drive_letter`) plus the URL glue Python derives from config.
fn ui_cfg() -> WebUiConfig {
    WebUiConfig {
        drive_letter: "Y:".to_string(),
        webdav_url: "http://127.0.0.1:8080".to_string(),
        chat_id: 123456789,
        is_configured: true,
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
    let server = WebUiServer::serve(vfs.clone(), ui_cfg(), SocketAddr::from(([127, 0, 0, 1], 0)))
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
            telegram_msg_id: Some(i64::from(receipt.first_msg_id)),
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
        db.upsert_chunk(file_id, index, i64::from(msg_id), chunk_row_size, None)
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
/// fields the handler glues on (`webdav_host`/`webdav_port` included —
/// the Python response carries them even though app.js never reads
/// them; shape parity is the contract).
const STATS_KEYS: [&str; 11] = [
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
                error: TransportError::Remote("scripted first-attempt failure".to_string()),
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
        env.mock
            .message(i32::try_from(msg_id).expect("narrow msg id")),
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
