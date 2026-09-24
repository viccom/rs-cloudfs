//! RED-phase tests for Phase 2.5 / MV2: single-port multi-volume routing.
//!
//! Contract under test: `docs/plans/2026-09-08-phase2-5-multivolume.md`
//! §1-K20 + §3-MV2 — [`WebDavServer::serve_volumes`] serves several
//! [`CyDriveFs`] volumes on ONE port, dispatching every request by its
//! `/vol/<name>/` URL prefix (per request, so keep-alive connections can
//! address different volumes on the same connection):
//!
//! - the prefix is a process-level concept and never reaches the volume
//!   (R1: the db row and the transport's upload job see `/x.txt`, not
//!   `/vol/a/x.txt`);
//! - `/vol/<name>` and `/vol/<name>/` both address the volume root;
//! - anything else — no prefix, `/vol/`, an unknown volume — is a 404
//!   and touches no volume;
//! - each volume keeps its own DavHandler (its own FakeLs), so the
//!   single-volume lockdown semantics ride through unchanged.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::sleep;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::{CloudTransport, UploadJob};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cloudkit_webdav::{CyDriveFs, RegistryHandle, WebDavServer};

// ------------------------------------------------------------- helpers ---

/// VfsConfig for integration tests: tiny chunks, one worker, fast retry
/// (same shape as `fs_adapter.rs`'s config).
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
        encryption_scheme: cloudkit_core::config::EncryptionScheme::Gcm,
        hydrate_timeout: Duration::from_secs(180),
    }
}

/// One isolated volume: temp db + cache tree + pre-connected mock
/// transport + VFS + the adapter. The `TempDir` stays alive in the
/// struct; tests drop it after [`VolumeEnv::shutdown`].
struct VolumeEnv {
    _dir: tempfile::TempDir,
    db: Arc<MetaDatabase>,
    mock: Arc<MockTransport>,
    vfs: Arc<Vfs>,
    fs: CyDriveFs,
}

async fn volume_env() -> VolumeEnv {
    let dir = tempfile::tempdir().expect("create temp dir");
    let cache_root = dir.path().join("cache");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let cache = CacheManager::new(cache_root.clone(), u64::MAX);
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect mock transport");
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Arc::new(Vfs::new(db.clone(), cache, transport, test_cfg()));
    let fs = CyDriveFs::new(
        vfs.clone(),
        db.clone(),
        CacheManager::new(cache_root, u64::MAX),
    );
    VolumeEnv {
        _dir: dir,
        db,
        mock,
        vfs,
        fs,
    }
}

impl VolumeEnv {
    /// Drains the volume's upload queue (uploads finish into the mock).
    async fn shutdown(self) {
        self.vfs.shutdown().await;
    }
}

/// Boots `serve_volumes` with two volumes named `a` and `b` on an
/// ephemeral loopback port (RV1 mechanical adaptation: the static vec
/// became the shared [`RegistryHandle`]; assertions unchanged).
async fn serve_two(a: CyDriveFs, b: CyDriveFs) -> WebDavServer {
    WebDavServer::serve_volumes(
        RegistryHandle::new(vec![("a".to_string(), a), ("b".to_string(), b)]),
        SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .await
    .expect("serve_volumes binds the ephemeral port")
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

/// A keep-alive PUT (no `Connection: close`) for the per-request
/// dispatch test.
fn keepalive_put(target: &str, addr: SocketAddr, body: &str) -> String {
    format!(
        "PUT {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\n\r\n{body}",
        addr.port(),
        body.len()
    )
}

/// A keep-alive `Depth: 1` PROPFIND (no `Connection: close`) for the
/// same-connection removal test — the multistatus reply comes back
/// chunk-framed, so [`read_response`] peels exactly one reply off the
/// connection.
fn keepalive_propfind(target: &str, addr: SocketAddr) -> String {
    format!(
        "PROPFIND {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nDepth: 1\r\n\
         Content-Length: 0\r\n\r\n",
        addr.port()
    )
}

/// Reads one CRLF/LF-terminated line off `stream` (terminator included)
/// — the chunked-framing helper of [`read_response`].
async fn read_line(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).await.expect("read line byte");
        buf.push(byte[0]);
        if buf.ends_with(b"\n") {
            break;
        }
    }
    buf
}

/// Reads exactly one HTTP response off `stream`: headers to the blank
/// line, then the body framed either by `Content-Length` (the empty
/// 201/404 replies) or — the dav-server multistatus shape, whose
/// streamed body carries no length — by `Transfer-Encoding: chunked`
/// (decoded and concatenated). Either way the connection is left at
/// the exact start of the next response.
async fn read_response(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream
            .read_exact(&mut byte)
            .await
            .expect("read header byte");
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let header = |name: &str| {
        head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    };
    let mut body = Vec::new();
    if let Some(len) = header("content-length") {
        let len: usize = len.parse().expect("numeric content-length");
        body = vec![0u8; len];
        stream.read_exact(&mut body).await.expect("read body");
    } else if header("transfer-encoding").is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        loop {
            let size_line = read_line(stream).await;
            let size = usize::from_str_radix(
                String::from_utf8_lossy(&size_line)
                    .trim()
                    .split(';')
                    .next()
                    .expect("chunk size token"),
                16,
            )
            .expect("hex chunk size");
            if size == 0 {
                // The terminating CRLF (an empty trailer section) ends
                // the response body.
                read_line(stream).await;
                break;
            }
            let mut chunk = vec![0u8; size];
            stream.read_exact(&mut chunk).await.expect("read chunk");
            body.extend_from_slice(&chunk);
            // The CRLF after each chunk's data.
            read_line(stream).await;
        }
    }
    format!("{head}{}", String::from_utf8_lossy(&body))
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
fn header_of<'a>(resp: &'a str, name: &str) -> Option<&'a str> {
    resp.lines().take_while(|l| !l.is_empty()).find_map(|l| {
        let (key, value) = l.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// The response body (everything after the blank line; `Connection:
/// close` framing serves it till EOF).
fn body_of(resp: &str) -> &str {
    resp.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}

/// A `Depth: 1` PROPFIND (the `Connection: close` shape).
async fn propfind(addr: SocketAddr, target: &str) -> String {
    send(
        addr,
        &request("PROPFIND", target, addr, &[("Depth", "1")], ""),
    )
    .await
}

// ------------------------------------------------------ 1. prefix routing ---

/// PUT `/vol/a/x.txt` lands in volume a only (the prefix stripped, R1):
/// the row exists in a's db, not in b's; PROPFIND lists it under
/// `/vol/a/` but not `/vol/b/`; GET `/vol/b/x.txt` is a 404.
#[tokio::test]
async fn put_routes_to_the_named_volume_only() {
    let vol_a = volume_env().await;
    let vol_b = volume_env().await;
    let server = serve_two(vol_a.fs.clone(), vol_b.fs.clone()).await;
    let addr = server.local_addr();
    assert_ne!(addr.port(), 0, ":0 must resolve to the real bound port");

    let resp = send(
        addr,
        &request("PUT", "/vol/a/x.txt", addr, &[], "a-payload"),
    )
    .await;
    assert_eq!(status_of(&resp), 201, "PUT created: {resp}");

    // R1: the prefix never reaches the volume — the row is at /x.txt.
    assert!(
        vol_a.db.get_file("/x.txt").expect("db read a").is_some(),
        "volume a sees the prefix-stripped row"
    );
    assert!(
        vol_b.db.get_file("/x.txt").expect("db read b").is_none(),
        "volume b must not see volume a's row"
    );

    let resp = propfind(addr, "/vol/a/").await;
    assert_eq!(status_of(&resp), 207, "volume a root multistatus: {resp}");
    assert!(resp.contains("x.txt"), "volume a lists x.txt: {resp}");

    let resp = propfind(addr, "/vol/b/").await;
    assert_eq!(status_of(&resp), 207, "volume b root multistatus: {resp}");
    assert!(
        !resp.contains("x.txt"),
        "volume b must not list volume a's file: {resp}"
    );

    let resp = send(addr, &request("GET", "/vol/b/x.txt", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 404, "GET off the wrong volume: {resp}");

    server.shutdown().await;
    vol_a.shutdown().await;
    vol_b.shutdown().await;
}

/// `/vol/<name>` (no trailing slash) and `/vol/<name>/` both address the
/// volume root and answer PROPFIND with 207.
#[tokio::test]
async fn volume_root_both_spellings_answer_207() {
    let vol_a = volume_env().await;
    let vol_b = volume_env().await;
    let server = serve_two(vol_a.fs.clone(), vol_b.fs.clone()).await;
    let addr = server.local_addr();

    let resp = propfind(addr, "/vol/a").await;
    assert_eq!(status_of(&resp), 207, "no-trailing-slash root: {resp}");
    let resp = propfind(addr, "/vol/a/").await;
    assert_eq!(status_of(&resp), 207, "trailing-slash root: {resp}");

    server.shutdown().await;
    vol_a.shutdown().await;
    vol_b.shutdown().await;
}

// ---------------------------------------------------- 2. unrouted requests ---

/// Requests without a legal volume segment are a 404 that touches no
/// volume: the bare root `/`, `/vol/`, `/vol` and an unknown volume name
/// all fail, and neither db ever sees a row.
#[tokio::test]
async fn requests_without_a_volume_segment_are_404_everywhere() {
    let vol_a = volume_env().await;
    let vol_b = volume_env().await;
    let server = serve_two(vol_a.fs.clone(), vol_b.fs.clone()).await;
    let addr = server.local_addr();

    for target in ["/x.txt", "/vol/unknown/x", "/vol/", "/vol", "/"] {
        let method = if target == "/" { "GET" } else { "PUT" };
        let resp = send(addr, &request(method, target, addr, &[], "p")).await;
        assert_eq!(
            status_of(&resp),
            404,
            "unrouted target {target} must 404: {resp}"
        );
    }
    for db in [&vol_a.db, &vol_b.db] {
        assert!(
            db.list_dir("/").expect("list root").is_empty(),
            "no unrouted request may touch a volume"
        );
    }

    server.shutdown().await;
    vol_a.shutdown().await;
    vol_b.shutdown().await;
}

// ------------------------------------------------------- 3. deep hierarchies ---

/// MKCOL + nested PUT through the prefix: `MKCOL /vol/a/d/` then
/// `PUT /vol/a/d/f` build `/d` and `/d/f` inside volume a only.
#[tokio::test]
async fn mkcol_and_nested_put_route_through_the_prefix() {
    let vol_a = volume_env().await;
    let vol_b = volume_env().await;
    let server = serve_two(vol_a.fs.clone(), vol_b.fs.clone()).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("MKCOL", "/vol/a/d/", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 201, "MKCOL created: {resp}");

    let resp = send(addr, &request("PUT", "/vol/a/d/f", addr, &[], "nested")).await;
    assert_eq!(status_of(&resp), 201, "nested PUT created: {resp}");

    let resp = propfind(addr, "/vol/a/d/").await;
    assert_eq!(status_of(&resp), 207, "collection multistatus: {resp}");
    assert!(resp.contains("f"), "the collection lists f: {resp}");

    // The rows land prefix-stripped in volume a (R1), never in b.
    assert!(
        vol_a.db.get_file("/d/f").expect("db read a").is_some(),
        "volume a has the nested row at /d/f"
    );
    assert!(
        vol_b.db.get_file("/d").expect("db read b").is_none(),
        "volume b must stay empty"
    );

    server.shutdown().await;
    vol_a.shutdown().await;
    vol_b.shutdown().await;
}

// ------------------------------------------- 4. per-request dispatch (keep-alive) ---

/// WebDAV rides keep-alive connections: one connection, two PUTs to two
/// different volumes — the second request must re-dispatch on its own
/// path (a connection-level split would send both to whichever volume
/// won the first request).
#[tokio::test]
async fn keep_alive_connection_redispatches_each_request() {
    let vol_a = volume_env().await;
    let vol_b = volume_env().await;
    let server = serve_two(vol_a.fs.clone(), vol_b.fs.clone()).await;
    let addr = server.local_addr();

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(keepalive_put("/vol/a/ka.txt", addr, "for-a").as_bytes())
        .await
        .expect("send first PUT");
    let resp = read_response(&mut stream).await;
    assert_eq!(status_of(&resp), 201, "first PUT on the connection: {resp}");

    stream
        .write_all(keepalive_put("/vol/b/ka.txt", addr, "for-b").as_bytes())
        .await
        .expect("send second PUT on the SAME connection");
    let resp = read_response(&mut stream).await;
    assert_eq!(status_of(&resp), 201, "second PUT re-dispatched: {resp}");
    drop(stream);

    assert!(
        vol_a.db.get_file("/ka.txt").expect("db read a").is_some(),
        "volume a got the first PUT"
    );
    assert!(
        vol_b.db.get_file("/ka.txt").expect("db read b").is_some(),
        "volume b got the second PUT on the same connection"
    );

    server.shutdown().await;
    vol_a.shutdown().await;
    vol_b.shutdown().await;
}

// ------------------------------------------------- 5. the prefix stays hidden ---

/// R1: after `PUT /vol/a/noleak.txt` drains, the transport's upload job
/// carries `/noleak.txt` — no `vol` segment ever reaches the storage
/// face.
#[tokio::test]
async fn the_volume_prefix_never_reaches_the_transport() {
    let vol_a = volume_env().await;
    let vol_b = volume_env().await;
    let server = serve_two(vol_a.fs.clone(), vol_b.fs.clone()).await;
    let addr = server.local_addr();

    let resp = send(
        addr,
        &request("PUT", "/vol/a/noleak.txt", addr, &[], "payload"),
    )
    .await;
    assert_eq!(status_of(&resp), 201, "PUT created: {resp}");

    // Drain through shutdown, then inspect the mocks' upload jobs
    // (Arc clones — shutdown() consumes the env structs).
    let mock_a = vol_a.mock.clone();
    let mock_b = vol_b.mock.clone();
    server.shutdown().await;
    vol_a.shutdown().await;
    vol_b.shutdown().await;

    let jobs = mock_a.upload_calls();
    assert!(!jobs.is_empty(), "volume a's upload job ran");
    for job in jobs {
        assert_eq!(
            job.rel_path.as_str(),
            "/noleak.txt",
            "the upload job sees the prefix-stripped path"
        );
    }
    assert!(
        mock_b.upload_calls().is_empty(),
        "volume b's transport must stay untouched"
    );
}

/// SR1 regression: a Range GET under the `/vol` prefix serves the exact
/// 206 slice through the streaming path — the volume's transport sees
/// one bounded `open_range` window (never a whole-file `open`) and the
/// other volume stays untouched (RangeFile works behind the prefix
/// router).
#[tokio::test]
async fn volume_get_range_streams_bounded_window() {
    let vol_a = volume_env().await;
    let vol_b = volume_env().await;
    let server = serve_two(vol_a.fs.clone(), vol_b.fs.clone()).await;
    let addr = server.local_addr();

    // Remote bytes + uploaded row in volume a (single-message row: the
    // handle falls back to the row's telegram_msg_id).
    let scratch = tempfile::tempdir().expect("seed scratch dir");
    let rel = RelPath::new("/stream.txt").expect("valid rel path");
    let local_path = scratch.path().join("stream.txt");
    std::fs::write(&local_path, b"hello world").expect("write seed file");
    let receipt = vol_a
        .mock
        .upload(&UploadJob {
            rel_path: rel.clone(),
            local_path,
            size: 11,
            chunk_count: 1,
            chunk_size: 64,
        })
        .await
        .expect("seed upload to volume a's remote");
    vol_a
        .db
        .upsert_file(&FileUpsert {
            rel_path: rel.as_str().to_string(),
            name: rel.name().to_string(),
            parent_dir: "/".to_string(),
            size: 11,
            mtime: 1_700_000_000.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: Some(receipt.first_msg_id),
            is_uploaded: true,
            is_cached: false,
            is_encrypted: false,
            chunk_count: 1,
            mime_type: None,
        })
        .expect("seed files row in volume a");

    let resp = send(
        addr,
        &request(
            "GET",
            "/vol/a/stream.txt",
            addr,
            &[("Range", "bytes=2-4")],
            "",
        ),
    )
    .await;

    assert_eq!(status_of(&resp), 206, "partial content: {resp}");
    assert_eq!(
        header_of(&resp, "content-range"),
        Some("bytes 2-4/11"),
        "slice bounds over the row size: {resp}"
    );
    assert_eq!(body_of(&resp), "llo");
    assert!(
        vol_a.mock.open_calls().is_empty(),
        "the streaming path never asks for a whole-file open"
    );
    assert_eq!(
        vol_a.mock.open_range_calls(),
        vec![(2, 9)],
        "one bounded window at the range start"
    );
    assert!(
        vol_b.mock.upload_calls().is_empty()
            && vol_b.mock.open_calls().is_empty()
            && vol_b.mock.open_range_calls().is_empty(),
        "volume b's transport stays untouched"
    );

    server.shutdown().await;
    vol_a.shutdown().await;
    vol_b.shutdown().await;
}

// ------------------------------------------------- 7. the dynamic table (RV1) ---

/// RV1 (K51): the route table is a live shared handle — removing a
/// volume from the registry makes its `/vol/<name>` routes answer 404
/// immediately (per-request table lookup, no route rebuild, no listener
/// restart) while the surviving volume keeps serving untouched.
#[tokio::test]
async fn removed_volume_answers_404_while_survivors_keep_serving() {
    let vol_a = volume_env().await;
    let vol_b = volume_env().await;
    let registry = RegistryHandle::new(vec![
        ("a".to_string(), vol_a.fs.clone()),
        ("b".to_string(), vol_b.fs.clone()),
    ]);
    let server =
        WebDavServer::serve_volumes(registry.clone(), SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("serve_volumes binds the ephemeral port");
    let addr = server.local_addr();

    // Boot sanity: both volumes are reachable before the removal.
    let resp = send(addr, &request("PUT", "/vol/a/pre.txt", addr, &[], "a")).await;
    assert_eq!(status_of(&resp), 201, "volume a at boot: {resp}");
    let resp = send(addr, &request("PUT", "/vol/b/pre.txt", addr, &[], "b")).await;
    assert_eq!(status_of(&resp), 201, "volume b at boot: {resp}");

    // The removal: the K50 seam the runtime unload acts through.
    assert!(
        registry.remove("a"),
        "removing a registered volume reports it"
    );

    // The removed volume's routes are gone — the same URL that answered
    // 201 above now answers the plain 404, without touching any volume.
    let resp = send(addr, &request("PUT", "/vol/a/late.txt", addr, &[], "late")).await;
    assert_eq!(status_of(&resp), 404, "removed volume: {resp}");
    assert!(
        vol_a.db.get_file("/late.txt").expect("db read a").is_none(),
        "the removed volume's db must not see requests"
    );

    // The surviving volume is unaffected: it still routes, writes and
    // lists, and the removal never touched its rows.
    let resp = send(
        addr,
        &request("PUT", "/vol/b/late.txt", addr, &[], "b-late"),
    )
    .await;
    assert_eq!(status_of(&resp), 201, "surviving volume: {resp}");
    let resp = propfind(addr, "/vol/b/").await;
    assert_eq!(status_of(&resp), 207, "surviving volume listing: {resp}");
    assert!(
        resp.contains("late.txt") && resp.contains("pre.txt"),
        "the surviving volume lists its boot and post-removal rows: {resp}"
    );
    assert!(
        vol_b.db.get_file("/late.txt").expect("db read b").is_some(),
        "the surviving volume took the write"
    );

    server.shutdown().await;
    vol_a.shutdown().await;
    vol_b.shutdown().await;
}

/// RV1 (K51): inserting a volume into the registry after boot makes it
/// immediately routable on the SAME port — no server restart, no route
/// rebuild; and it never existed for requests before the insert.
#[tokio::test]
async fn dynamically_inserted_volume_is_immediately_routable() {
    let vol_a = volume_env().await;
    let vol_c = volume_env().await;
    let registry = RegistryHandle::new(vec![("a".to_string(), vol_a.fs.clone())]);
    let server =
        WebDavServer::serve_volumes(registry.clone(), SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("serve_volumes binds the ephemeral port");
    let addr = server.local_addr();

    // Before the insert the name is an unknown volume: 404.
    let resp = send(addr, &request("PUT", "/vol/c/first.txt", addr, &[], "x")).await;
    assert_eq!(status_of(&resp), 404, "unregistered volume: {resp}");

    // The insert: the K50/RV2 seam the runtime load acts through.
    registry.insert("c", vol_c.fs.clone());

    // The route is live at once — prefix stripped (R1), row in c's db.
    let resp = send(
        addr,
        &request("PUT", "/vol/c/first.txt", addr, &[], "c-payload"),
    )
    .await;
    assert_eq!(status_of(&resp), 201, "dynamically inserted volume: {resp}");
    assert!(
        vol_c
            .db
            .get_file("/first.txt")
            .expect("db read c")
            .is_some(),
        "the inserted volume sees the prefix-stripped row"
    );

    server.shutdown().await;
    vol_a.shutdown().await;
    vol_c.shutdown().await;
}

/// RV1 (K51) keep-alive companion, M5 test gap 3 (characterization,
/// expected green): the per-request dispatch is not cached per
/// connection. ONE keep-alive connection PROPFINDs volume a to a 207,
/// then `registry.remove("a")` runs, and the VERY SAME connection
/// asks the same URL again — it must now get the 404 (the request
/// re-reads the live table; a connection-level route would keep
/// serving the removed volume), while the surviving volume b keeps
/// answering both on that same connection and on a fresh one.
#[tokio::test]
async fn keep_alive_connection_sees_a_volume_removal_mid_connection() {
    let vol_a = volume_env().await;
    let vol_b = volume_env().await;
    let registry = RegistryHandle::new(vec![
        ("a".to_string(), vol_a.fs.clone()),
        ("b".to_string(), vol_b.fs.clone()),
    ]);
    let server =
        WebDavServer::serve_volumes(registry.clone(), SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("serve_volumes binds the ephemeral port");
    let addr = server.local_addr();

    // Before the removal: the keep-alive connection routes to a.
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(keepalive_propfind("/vol/a/", addr).as_bytes())
        .await
        .expect("send first PROPFIND");
    let resp = read_response(&mut stream).await;
    assert_eq!(
        status_of(&resp),
        207,
        "volume a answers before the removal: {resp}"
    );

    // The removal through the shared registry handle (the K50 seam).
    assert!(
        registry.remove("a"),
        "removing a registered volume reports it"
    );

    // The SAME connection, the SAME URL: the plain 404 now — the
    // request re-reads the dispatch table instead of reusing whatever
    // route answered the first request on this connection.
    stream
        .write_all(keepalive_propfind("/vol/a/", addr).as_bytes())
        .await
        .expect("send second PROPFIND on the SAME connection");
    let resp = read_response(&mut stream).await;
    assert_eq!(
        status_of(&resp),
        404,
        "the same connection sees the removal: {resp}"
    );

    // The connection stays usable and the survivor is reachable on it.
    stream
        .write_all(keepalive_propfind("/vol/b/", addr).as_bytes())
        .await
        .expect("send the survivor PROPFIND on the same connection");
    let resp = read_response(&mut stream).await;
    assert_eq!(
        status_of(&resp),
        207,
        "the survivor answers the old connection: {resp}"
    );
    drop(stream);

    // And on a fresh connection: volume b is unaffected by a's removal.
    let resp = propfind(addr, "/vol/b/").await;
    assert_eq!(
        status_of(&resp),
        207,
        "the survivor on a fresh connection: {resp}"
    );

    server.shutdown().await;
    vol_a.shutdown().await;
    vol_b.shutdown().await;
}

// ------------------------------------------------------- 6. graceful stop ---

/// `serve_volumes` stops like `serve`: after shutdown the listener
/// refuses new connections.
#[tokio::test]
async fn serve_volumes_stops_gracefully() {
    let vol_a = volume_env().await;
    let vol_b = volume_env().await;
    let server = serve_two(vol_a.fs.clone(), vol_b.fs.clone()).await;
    let addr = server.local_addr();

    server.shutdown().await;
    // Give the accept loop a moment to unwind (shutdown joins it, but
    // the OS may lag the refusal).
    let mut refused = false;
    for _ in 0..50 {
        if TcpStream::connect(addr).await.is_err() {
            refused = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(refused, "new connections must be refused after shutdown");

    vol_a.shutdown().await;
    vol_b.shutdown().await;
}

// ------------------------------------------- BUG-FIX: MOVE under the prefix ---

/// BUG-FIX（Phase 7 真机 e2e，pan115 2026-09-24 实证）：`/vol/<name>/`
/// 前缀路由下的 **MOVE 必须落在正确的命名空间**。
///
/// 缺陷形态（修复前）：`VolumeRouter::dispatch` 只重写了请求 URI，未
/// 同步改写 `Destination` 头——dav-server 拿到的 Destination 仍是
/// `/vol/a/...`，与它内部的卷内命名空间错位，`has_parent(&dest)` 判定
/// 失败 → **409 Conflict**，改名在多卷模式下恒不可用（真机实测：单卷
/// 201 假成功 / 多卷 409）。
///
/// 契约：Destination 的 `/vol/a` 前缀必须与请求 URI 同样被剥掉；改名
/// 后源路径消失、目标路径可读，且**不触碰兄弟卷 b**。
#[tokio::test]
async fn move_through_the_volume_prefix_rewrites_the_destination() {
    let vol_a = volume_env().await;
    let vol_b = volume_env().await;
    let server = serve_two(vol_a.fs.clone(), vol_b.fs.clone()).await;
    let addr = server.local_addr();

    // Seed one row in volume a (same shape as the GET-range test above).
    let rel = RelPath::new("/src.txt").expect("valid rel path");
    let scratch = tempfile::tempdir().expect("seed scratch dir");
    let local_path = scratch.path().join("src.txt");
    std::fs::write(&local_path, b"payload").expect("write seed file");
    let receipt = vol_a
        .mock
        .upload(&UploadJob {
            rel_path: rel.clone(),
            local_path,
            size: 7,
            chunk_count: 1,
            chunk_size: 64,
        })
        .await
        .expect("seed upload to volume a's remote");
    vol_a
        .db
        .upsert_file(&FileUpsert {
            rel_path: rel.as_str().to_string(),
            name: rel.name().to_string(),
            parent_dir: "/".to_string(),
            size: 7,
            mtime: 1_700_000_000.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: Some(receipt.first_msg_id),
            is_uploaded: true,
            is_cached: false,
            is_encrypted: false,
            chunk_count: 1,
            mime_type: None,
        })
        .expect("seed files row in volume a");

    // MOVE /vol/a/src.txt -> /vol/a/dst.txt (the mounted-volume shape:
    // the client addresses both ends through the volume prefix).
    let dest = format!("http://127.0.0.1:{}/vol/a/dst.txt", addr.port());
    let resp = send(
        addr,
        &request(
            "MOVE",
            "/vol/a/src.txt",
            addr,
            &[("Destination", &dest)],
            "",
        ),
    )
    .await;

    let status = status_of(&resp);
    assert!(
        (200..300).contains(&status),
        "MOVE under the prefix must succeed, got {status}: {resp}"
    );
    // The row moved: the old path is gone, the new one resolves.
    let old = send(
        addr,
        &request("PROPFIND", "/vol/a/src.txt", addr, &[("Depth", "0")], ""),
    )
    .await;
    assert_eq!(status_of(&old), 404, "old path gone: {old}");
    let new = send(
        addr,
        &request("PROPFIND", "/vol/a/dst.txt", addr, &[("Depth", "0")], ""),
    )
    .await;
    assert_eq!(status_of(&new), 207, "new path resolves: {new}");
    // The sibling volume is untouched.
    assert!(
        vol_b.mock.upload_calls().is_empty() && vol_b.db.list_dir("/").expect("list b").is_empty(),
        "volume b stays untouched by volume a's rename"
    );

    server.shutdown().await;
    vol_a.shutdown().await;
    vol_b.shutdown().await;
}
