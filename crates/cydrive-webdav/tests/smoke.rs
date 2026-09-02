//! Smoke tests for the `WebDavServer` hyper assembly (offline).
//!
//! Each test spins up a fresh environment — temp SQLite + cache tree +
//! pre-connected `MockTransport` + Vfs + `CyDriveFs` — serves it on an
//! ephemeral loopback port (`127.0.0.1:0`, production's 127.0.0.1:8080
//! contract is the caller's concern) and drives it with hand-rolled
//! HTTP/1.1 over a raw `TcpStream` (no HTTP client dependency). The
//! scenarios pin the Python baseline's observable wire behavior:
//! PROPFIND listings off the DB, hydrated GETs with Range support,
//! staged PUTs that drain into the upload queue (0-byte PUTs never
//! touch the transport), MKCOL/DELETE/MOVE semantics and the OPTIONS
//! advertisement Windows Explorer probes before mounting.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cydrive_core::cache::CacheManager;
use cydrive_core::database::{FileUpsert, MetaDatabase};
use cydrive_core::rel_path::RelPath;
use cydrive_core::transport::mock::MockTransport;
use cydrive_core::transport::{CloudTransport, UploadJob, UploadReceipt};
use cydrive_core::upload_queue::RetryPolicy;
use cydrive_core::vfs::{Vfs, VfsConfig};
use cydrive_webdav::{CyDriveFs, WebDavServer};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// VfsConfig for integration tests: tiny chunks, one worker, fast retry.
fn test_cfg() -> VfsConfig {
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

/// One fresh offline environment per test: real SQLite, mirrored cache
/// tree, mock remote, Vfs and the server under test on an ephemeral
/// port.
struct Env {
    _dir: tempfile::TempDir,
    server: WebDavServer,
    db: Arc<MetaDatabase>,
    mock: Arc<MockTransport>,
    vfs: Arc<Vfs>,
    #[allow(dead_code)]
    cache_root: PathBuf,
}

async fn test_env() -> Env {
    let dir = tempfile::tempdir().expect("create temp dir");
    let cache_root = dir.path().join("cache");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect mock transport");
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Arc::new(Vfs::new(
        db.clone(),
        CacheManager::new(cache_root.clone(), u64::MAX),
        transport,
        test_cfg(),
    ));
    let fs = CyDriveFs::new(
        vfs.clone(),
        db.clone(),
        CacheManager::new(cache_root.clone(), u64::MAX),
    );
    let server = WebDavServer::serve(fs, SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("serve on an ephemeral loopback port");
    Env {
        _dir: dir,
        server,
        db,
        mock,
        vfs,
        cache_root,
    }
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
/// the server answered `Transfer-Encoding: chunked` (PROPFIND streams
/// its multistatus XML).
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

/// Writes a row of any shape straight into the DB.
fn seed_row(db: &Arc<MetaDatabase>, rel: &str, is_dir: bool, size: i64) {
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
        is_uploaded: true,
        is_cached: false,
        is_encrypted: false,
        chunk_count: if is_dir { 0 } else { 1 },
        mime_type: None,
    })
    .expect("seed row");
}

/// 1. PROPFIND Depth:1 answers 207 with a multistatus body naming the
///    collection and its child file (listings come straight off the DB).
#[tokio::test]
async fn propfind_root_lists_entries() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_row(&env.db, "/docs", true, 0);
    seed_remote_file(&env.db, &env.mock, "/docs/hello.txt", b"hello world", 64).await;

    let resp = send(
        addr,
        &request("PROPFIND", "/docs/", addr, &[("Depth", "1")], ""),
    )
    .await;

    assert_eq!(status_of(&resp), 207, "multistatus: {resp}");
    let body = body_of(&resp);
    assert!(body.contains("docs"), "collection listed: {body}");
    assert!(body.contains("hello.txt"), "file listed: {body}");
}

/// 2. GET hydrates through the VFS and returns the exact bytes with an
///    explicit Content-Length.
#[tokio::test]
async fn get_returns_bytes_and_headers() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_remote_file(&env.db, &env.mock, "/docs/hello.txt", b"hello world", 64).await;

    let resp = send(addr, &request("GET", "/docs/hello.txt", addr, &[], "")).await;

    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    assert_eq!(header(&resp, "content-length"), Some("11"));
    assert_eq!(body_of(&resp), "hello world");
}

/// 3. GET with a single Range returns 206, the sliced bytes and a
///    correct Content-Range.
#[tokio::test]
async fn get_range_partial() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_remote_file(&env.db, &env.mock, "/docs/hello.txt", b"hello world", 64).await;

    let resp = send(
        addr,
        &request(
            "GET",
            "/docs/hello.txt",
            addr,
            &[("Range", "bytes=2-4")],
            "",
        ),
    )
    .await;

    assert_eq!(status_of(&resp), 206, "partial content: {resp}");
    assert_eq!(
        header(&resp, "content-range"),
        Some("bytes 2-4/11"),
        "inclusive slice bounds over the full length: {resp}"
    );
    assert_eq!(body_of(&resp), "llo");
}

/// 4. PUT lands the Explorer-style write (2xx), GET reads it back, and
///    after the queue drains the row is uploaded with exactly one
///    transport call.
#[tokio::test]
async fn put_then_get_roundtrip() {
    let env = test_env().await;
    let addr = env.server.local_addr();

    let resp = send(addr, &request("PUT", "/roundtrip.txt", addr, &[], "WebDAV")).await;
    assert_eq!(status_of(&resp), 201, "created: {resp}");

    let resp = send(addr, &request("GET", "/roundtrip.txt", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "read back: {resp}");
    assert_eq!(body_of(&resp), "WebDAV");

    env.server.shutdown().await;
    env.server.shutdown().await; // idempotent
    env.vfs.shutdown().await;

    let row = env
        .db
        .get_file("/roundtrip.txt")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "drain uploaded the file");
    assert_eq!(row.size, 6);
    assert_eq!(env.mock.upload_calls().len(), 1, "one upload job");
}

/// 5. A 0-byte PUT succeeds, never touches the transport, and leaves an
///    uploaded 0-size row (Explorer placeholder guard, contract 6).
#[tokio::test]
async fn put_zero_byte() {
    let env = test_env().await;
    let addr = env.server.local_addr();

    let resp = send(addr, &request("PUT", "/placeholder.txt", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 201, "created: {resp}");

    env.server.shutdown().await;
    env.vfs.shutdown().await;

    assert!(
        env.mock.upload_calls().is_empty(),
        "0-byte PUT never touches the transport"
    );
    let row = env
        .db
        .get_file("/placeholder.txt")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "0-byte counts as uploaded");
    assert_eq!(row.size, 0);
}

/// 6. MKCOL creates a collection visible to PROPFIND; a duplicate MKCOL
///    is rejected with 405.
#[tokio::test]
async fn mkcol_then_propfind() {
    let env = test_env().await;
    let addr = env.server.local_addr();

    let resp = send(addr, &request("MKCOL", "/up", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 201, "created: {resp}");

    let resp = send(addr, &request("PROPFIND", "/", addr, &[("Depth", "1")], "")).await;
    assert_eq!(status_of(&resp), 207, "multistatus: {resp}");
    assert!(body_of(&resp).contains("up"), "new collection listed");

    let resp = send(addr, &request("MKCOL", "/up", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 405, "duplicate mkcol: {resp}");
}

/// 7. DELETE answers 204, the file is gone (404), and the remote keeps
///    its messages (baseline mirror: delete never propagates).
#[tokio::test]
async fn delete_file_gone() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_remote_file(&env.db, &env.mock, "/doomed.bin", b"payload", 64).await;

    let resp = send(addr, &request("DELETE", "/doomed.bin", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 204, "no content: {resp}");

    let resp = send(addr, &request("GET", "/doomed.bin", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 404, "gone: {resp}");

    assert!(
        env.mock.deleted().is_empty(),
        "baseline mirror: delete never touches the remote"
    );
}

/// 8. MOVE renames: 2xx, the new path serves the bytes, the old one is
///    404.
#[tokio::test]
async fn move_renames() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_remote_file(&env.db, &env.mock, "/a.txt", b"moved!", 64).await;

    let resp = send(
        addr,
        &request("MOVE", "/a.txt", addr, &[("Destination", "/b.txt")], ""),
    )
    .await;
    assert!(
        (200..300).contains(&status_of(&resp)),
        "move succeeded: {resp}"
    );

    let resp = send(addr, &request("GET", "/b.txt", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "new path serves: {resp}");
    assert_eq!(body_of(&resp), "moved!");

    let resp = send(addr, &request("GET", "/a.txt", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 404, "old path gone: {resp}");
}

/// 9. OPTIONS advertises WebDAV class 1/2 (the DAV header) and an allow
///    set covering PROPFIND, PUT and LOCK (Windows Explorer probes this
///    before mounting; LOCK requires the FakeLs locksystem).
#[tokio::test]
async fn options_advertise_dav() {
    let env = test_env().await;
    let addr = env.server.local_addr();
    seed_remote_file(&env.db, &env.mock, "/docs/hello.txt", b"hello world", 64).await;

    // Root collection: PROPFIND and LOCK are advertised.
    let resp = send(addr, &request("OPTIONS", "/", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let dav = header(&resp, "dav").expect("DAV header");
    assert!(dav.contains("1,2"), "class 1 and 2: {resp}");
    let allow = header(&resp, "allow").expect("allow header");
    assert!(allow.contains("PROPFIND"), "root allow: {resp}");
    assert!(
        allow.contains("LOCK"),
        "lock advertised (FakeLs active): {resp}"
    );

    // On an existing file PUT appears in the allow set as well.
    let resp = send(addr, &request("OPTIONS", "/docs/hello.txt", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let allow = header(&resp, "allow").expect("allow header");
    assert!(
        allow.contains("PROPFIND") && allow.contains("PUT") && allow.contains("LOCK"),
        "file allow set: {resp}"
    );
}
