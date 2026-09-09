//! E2E contract tests for the multi-volume dashboard (Phase 2.5 / MV3,
//! offline).
//!
//! Each test assembles two (or three) independent volume environments —
//! real SQLite + cache tree + `MockTransport` + Vfs each, the same
//! construction `web_e2e.rs` uses per volume — behind ONE
//! `WebUiServer::serve_multi` on an ephemeral loopback port, and pins the
//! K23/K24 wire contract:
//!
//! - `GET /api/volumes` — the registry listing: per-volume identity,
//!   status (failed volumes carry their reason) and the DB metadata
//!   numbers (never a backend scan — the PCFS anti-lesson);
//! - K23 volume routing: every volume-scoped API takes an optional
//!   `?volume=<name>`; multi-volume mode without the parameter answers
//!   400 + the actionable volume list, an unknown name answers 404, a
//!   known-but-failed volume answers 409 with its reason, and a running
//!   volume answers exactly the frozen per-route shapes (the 16-key
//!   `/api/stats` contract included, with the per-volume identity keys);
//! - `GET /api/stats/summary` — the cross-volume aggregate (Σ files /
//!   bytes / quota over the running volumes);
//! - single-volume mode: the `?volume` parameter is ignored (no
//!   registry to route through) and the two registry routes stay absent
//!   — the frozen nine-route table and every existing behavior are
//!   pinned zero-drift by `web_e2e.rs`.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::{CloudTransport, UploadJob, UploadReceipt};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cloudkit_web::{QuotaSnapshot, VolumeUiEntry, VolumeUiStatus, WebUiConfig, WebUiServer};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

// ------------------------------------------------------------- helpers ---

/// VfsConfig for tests: tiny chunks, one worker, fast retry (the
/// `web_e2e.rs` baseline).
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

/// One volume's dashboard knobs (the per-volume `WebUiConfig` face the
/// cli assembly builds from the registry: per-volume drive letter,
/// `/vol/<name>` WebDAV URL, backend identity, boot quota snapshot).
// Every argument is an independent dashboard knob the tests toggle
// independently; a bundled options struct would just move the same
// breadth one level deeper.
#[allow(clippy::too_many_arguments)]
fn volume_cfg(
    backend: &str,
    letter: &str,
    url_suffix: &str,
    chat_id: i64,
    is_configured: bool,
    volume_id: Option<&str>,
    remote_delete: bool,
    quota: Option<QuotaSnapshot>,
) -> WebUiConfig {
    WebUiConfig {
        drive_letter: Some(letter.to_string()),
        webdav_url: format!("http://127.0.0.1:8080/vol/{url_suffix}"),
        chat_id,
        is_configured,
        backend: backend.to_string(),
        volume: volume_id.map(str::to_string),
        remote_delete,
        quota,
    }
}

/// One assembled volume: its own db/cache/mock/VFS plus the registry
/// entry handed to the server.
struct VolumeEnv {
    db: Arc<MetaDatabase>,
    mock: Arc<MockTransport>,
    vfs: Arc<Vfs>,
    entry: VolumeUiEntry,
}

/// Builds one running volume environment inside `dir` (unique file names
/// per volume name — two volumes never share a db or cache tree).
async fn volume_env(dir: &Path, name: &str, cfg: WebUiConfig) -> VolumeEnv {
    let db = Arc::new(MetaDatabase::open(&dir.join(format!("{name}.db"))).expect("open volume db"));
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect mock transport");
    let vfs = Arc::new(Vfs::new(
        db.clone(),
        CacheManager::new(dir.join(format!("{name}-cache")), u64::MAX),
        mock.clone(),
        base_cfg(),
    ));
    VolumeEnv {
        db,
        mock,
        vfs: vfs.clone(),
        entry: VolumeUiEntry {
            name: name.to_string(),
            status: VolumeUiStatus::Running,
            config: cfg,
            vfs: Some(vfs),
        },
    }
}

/// Serves the multi-volume dashboard on an ephemeral loopback port.
async fn multi_server(entries: Vec<VolumeUiEntry>) -> WebUiServer {
    WebUiServer::serve_multi(entries, SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("serve the multi-volume dashboard on an ephemeral port")
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
/// `file` (the only shape the frontend sends).
fn multipart_body(boundary: &str, filename: &str, content: &str) -> String {
    format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n\
         {content}\r\n\
         --{boundary}--\r\n"
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
/// chunk), byte-wise on purpose.
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

/// Writes a row of any shape straight into a volume's DB (the
/// `web_e2e.rs` seeding helper).
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

/// Pushes `bytes` to a volume's mock remote as `rel` (one chunk).
async fn seed_remote(mock: &Arc<MockTransport>, rel: &str, bytes: &[u8]) -> UploadReceipt {
    let dir = tempfile::tempdir().expect("seed scratch dir");
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let local_path = dir.path().join(rel_path.name());
    std::fs::write(&local_path, bytes).expect("write seed scratch file");
    mock.upload(&UploadJob {
        rel_path,
        local_path,
        size: bytes.len() as u64,
        chunk_count: 1,
        chunk_size: 64,
    })
    .await
    .expect("seed upload to the mock remote")
}

/// Inserts an uploaded `files` row for `rel` plus its chunk row (the
/// common hydrate precondition, single-chunk shape).
fn seed_uploaded_row(db: &Arc<MetaDatabase>, rel: &str, bytes: &[u8], receipt: &UploadReceipt) {
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let parent = rel_path.parent().expect("non-root path");
    let file_id = db
        .upsert_file(&FileUpsert {
            rel_path: rel_path.as_str().to_string(),
            name: rel_path.name().to_string(),
            parent_dir: parent.as_str().to_string(),
            size: bytes.len() as i64,
            mtime: 1_700_000_000.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: Some(receipt.first_msg_id),
            is_uploaded: true,
            is_cached: false,
            is_encrypted: false,
            chunk_count: receipt.chunk_msg_ids.len() as i64,
            mime_type: None,
        })
        .expect("seed files row");
    db.upsert_chunk(file_id, 0, receipt.first_msg_id, bytes.len() as i64, None)
        .expect("seed chunk row");
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

/// The exact 16-key `/api/stats` contract (the frozen Python-parity set
/// plus the five backend-identity keys) — identical in multi-volume
/// mode's per-volume answers.
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

/// The common two-volume fixture: volume `a` (local-flavoured identity,
/// quota snapshot, remote_delete on) and volume `b` (baidu-flavoured,
/// no quota), each with distinct seeded rows so cross-volume leakage
/// fails a number assertion, not just a key assertion.
async fn two_volume_env() -> (tempfile::TempDir, WebUiServer, VolumeEnv, VolumeEnv) {
    let dir = tempfile::tempdir().expect("temp dir");
    let a = volume_env(
        dir.path(),
        "a",
        volume_cfg(
            "local",
            "V:",
            "a",
            0,
            false,
            Some("local:aa11"),
            true,
            Some(QuotaSnapshot {
                used: 5,
                total: Some(10),
            }),
        ),
    )
    .await;
    let b = volume_env(
        dir.path(),
        "b",
        volume_cfg("baidu", "Z:", "b", 0, false, Some("baidu:42"), false, None),
    )
    .await;

    // Volume a: one uploaded file (100 bytes) + one directory.
    seed_row(&a.db, "/docs", true, 0, true);
    seed_row(&a.db, "/docs/only-a.txt", false, 100, true);
    // Volume b: two pending files (50 + 60 bytes) — different numbers in
    // every dimension.
    seed_row(&b.db, "/b1.bin", false, 50, false);
    seed_row(&b.db, "/b2.bin", false, 60, false);

    let server = multi_server(vec![a.entry.clone(), b.entry.clone()]).await;
    (dir, server, a, b)
}

// ------------------------------------------------------------ scenarios ---

/// K24: `GET /api/volumes` lists every registry entry with its identity,
/// per-volume mount URL, status and the DB metadata numbers — the two
/// volumes' numbers are their own (isolation), and no field leaks across.
#[tokio::test]
async fn volumes_endpoint_lists_both_volumes() {
    let (_dir, server, _a, _b) = two_volume_env().await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/api/volumes", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    assert!(
        resp.contains("application/json"),
        "json content type: {resp}"
    );

    let list: serde_json::Value =
        serde_json::from_str(&body_of(&resp)).expect("parse volumes array");
    let entries = list.as_array().expect("array body");
    assert_eq!(entries.len(), 2, "one entry per volume");

    let a_entry = entries
        .iter()
        .find(|e| e["name"] == "a")
        .expect("volume a entry");
    assert_eq!(a_entry["backend"], "local", "the volume's backend");
    assert_eq!(a_entry["volume_id"], "local:aa11", "dispatched identity");
    assert_eq!(a_entry["drive_letter"], "V:", "the volume's letter");
    assert_eq!(
        a_entry["webdav_url"], "http://127.0.0.1:8080/vol/a",
        "the per-volume mount URL (K27)"
    );
    assert_eq!(a_entry["status"], "running", "assembled volume");
    assert!(
        a_entry.get("status_reason").is_none(),
        "no failure reason on a running volume: {a_entry}"
    );
    assert_eq!(a_entry["quota_used"], 5, "boot quota snapshot, used");
    assert_eq!(a_entry["quota_total"], 10, "boot quota snapshot, total");
    assert_eq!(a_entry["total_files"], 1, "db metadata: non-dir rows");
    assert_eq!(a_entry["total_bytes"], 100, "db metadata: sum of sizes");

    let b_entry = entries
        .iter()
        .find(|e| e["name"] == "b")
        .expect("volume b entry");
    assert_eq!(b_entry["backend"], "baidu");
    assert_eq!(b_entry["volume_id"], "baidu:42");
    assert_eq!(b_entry["drive_letter"], "Z:");
    assert_eq!(b_entry["webdav_url"], "http://127.0.0.1:8080/vol/b");
    assert_eq!(b_entry["status"], "running");
    assert!(b_entry["quota_used"].is_null(), "no quota concept: null");
    assert!(b_entry["quota_total"].is_null(), "no quota concept: null");
    assert_eq!(b_entry["total_files"], 2, "b's own rows, not a's");
    assert_eq!(b_entry["total_bytes"], 110, "b's own bytes, not a's");
}

/// K23: `GET /api/stats?volume=a` answers the frozen 16-key contract
/// with volume a's own values — db aggregates from a's db and the
/// identity keys (drive letter, per-volume WebDAV URL, chat id,
/// configured flag, backend, volume, remote_delete, quota) from a's
/// registry entry.
#[tokio::test]
async fn stats_with_volume_param_returns_sixteen_keys() {
    let (_dir, server, _a, _b) = two_volume_env().await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/api/stats?volume=a", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");

    let stats: serde_json::Value =
        serde_json::from_str(&body_of(&resp)).expect("parse stats object");
    let expected: BTreeSet<String> = STATS_KEYS.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        keys_of(&stats).into_iter().collect::<BTreeSet<_>>(),
        expected,
        "exact 16-key set, per-volume flavour: {stats}"
    );

    assert_eq!(stats["total_files"], 1, "volume a's files");
    assert_eq!(stats["total_bytes"], 100, "volume a's bytes");
    assert_eq!(stats["total_dirs"], 1, "volume a's dirs");
    assert_eq!(stats["uploaded_files"], 1);
    assert_eq!(stats["pending_uploads"], 0);
    assert_eq!(stats["drive_letter"], "V:", "volume a's letter");
    assert_eq!(stats["webdav_host"], "127.0.0.1");
    assert_eq!(stats["webdav_port"], 8080);
    assert_eq!(
        stats["webdav_url"], "http://127.0.0.1:8080/vol/a",
        "volume a's per-volume WebDAV URL"
    );
    assert_eq!(stats["chat_id"], 0);
    assert_eq!(stats["is_configured"], false);
    assert_eq!(stats["backend"], "local", "volume a's backend");
    assert_eq!(stats["volume"], "local:aa11", "volume a's identity");
    assert_eq!(stats["remote_delete"], true, "volume a's K4 bit");
    assert_eq!(stats["quota_used"], 5, "volume a's quota snapshot");
    assert_eq!(stats["quota_total"], 10);

    // The same call against volume b answers b's own numbers — the
    // parameter routes, it does not decorate.
    let resp = send(addr, &request("GET", "/api/stats?volume=b", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let stats: serde_json::Value =
        serde_json::from_str(&body_of(&resp)).expect("parse stats object");
    assert_eq!(stats["total_files"], 2, "volume b's files");
    assert_eq!(stats["total_bytes"], 110, "volume b's bytes");
    assert_eq!(stats["pending_uploads"], 2, "b's rows are pending");
    assert_eq!(stats["backend"], "baidu");
    assert_eq!(stats["drive_letter"], "Z:");
}

/// K23: in multi-volume mode a volume-scoped API without the parameter
/// answers 400 with the actionable volume list (no PCFS-style fallback
/// to an arbitrary volume).
#[tokio::test]
async fn stats_without_volume_400_lists_volumes() {
    let (_dir, server, ..) = two_volume_env().await;
    let addr = server.local_addr();

    for target in ["/api/stats", "/api/files", "/api/queue", "/api/list"] {
        let resp = send(addr, &request("GET", target, addr, &[], "")).await;
        assert_eq!(
            status_of(&resp),
            400,
            "no default volume for {target}: {resp}"
        );
        let body: serde_json::Value =
            serde_json::from_str(&body_of(&resp)).expect("parse error body");
        assert_eq!(
            body["volumes"],
            serde_json::json!(["a", "b"]),
            "the body names the addressable volumes: {body}"
        );
        assert!(
            body["error"].as_str().is_some_and(|msg| !msg.is_empty()),
            "uniform error message: {body}"
        );
    }
}

/// K23: a `?volume=` naming no registry entry answers 404 with the same
/// actionable body.
#[tokio::test]
async fn unknown_volume_404() {
    let (_dir, server, _a, _b) = two_volume_env().await;
    let addr = server.local_addr();

    let resp = send(
        addr,
        &request("GET", "/api/stats?volume=nope", addr, &[], ""),
    )
    .await;
    assert_eq!(status_of(&resp), 404, "unknown volume: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse error body");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|msg| msg.contains("nope")),
        "the error names the unknown volume: {body}"
    );
    assert_eq!(body["volumes"], serde_json::json!(["a", "b"]));

    // The empty-string spelling is the missing-parameter case (400), not
    // an unknown-volume lookup.
    let resp = send(addr, &request("GET", "/api/stats?volume=", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 400, "empty volume = absent: {resp}");
}

/// K22/K24: a failed volume is listed with `status: "failed"` and its
/// reason (no numbers — there is no db to read), and addressing it
/// answers 409 Conflict carrying the reason.
#[tokio::test]
async fn failed_volume_listed_and_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let a = volume_env(
        dir.path(),
        "a",
        volume_cfg("telegram", "Y:", "a", 111111, true, None, false, None),
    )
    .await;
    let failed = VolumeUiEntry {
        name: "broken".to_string(),
        status: VolumeUiStatus::Failed {
            reason: "opening metadata db failed".to_string(),
        },
        config: volume_cfg("baidu", "Z:", "broken", 0, false, None, false, None),
        vfs: None,
    };
    let server = multi_server(vec![a.entry, failed]).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/api/volumes", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "the listing still answers: {resp}");
    let list: serde_json::Value =
        serde_json::from_str(&body_of(&resp)).expect("parse volumes array");
    let broken = list
        .as_array()
        .expect("array")
        .iter()
        .find(|e| e["name"] == "broken")
        .expect("the failed volume is listed, never silently absent");
    assert_eq!(broken["status"], "failed");
    assert_eq!(
        broken["status_reason"], "opening metadata db failed",
        "the assembly failure reason is visible: {broken}"
    );
    assert!(broken["total_files"].is_null(), "no db to read: null");
    assert!(broken["total_bytes"].is_null(), "no db to read: null");
    assert!(broken["quota_used"].is_null());

    // Addressing the failed volume answers 409 with the reason.
    let resp = send(
        addr,
        &request("GET", "/api/stats?volume=broken", addr, &[], ""),
    )
    .await;
    assert_eq!(status_of(&resp), 409, "failed volume: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse error body");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|msg| msg.contains("opening metadata db failed")),
        "the refusal carries the reason: {body}"
    );
}

/// K23: `POST /api/upload?volume=a` lands the row in volume a only —
/// volume b's db never sees it.
#[tokio::test]
async fn upload_routes_to_the_named_volume() {
    let (_dir, server, a, b) = two_volume_env().await;
    let addr = server.local_addr();

    let resp = send(
        addr,
        &request(
            "POST",
            "/api/upload?volume=a",
            addr,
            &[(
                "Content-Type",
                "multipart/form-data; boundary=multi-boundary",
            )],
            &multipart_body("multi-boundary", "routed.txt", "a's bytes"),
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "accepted: {resp}");
    let json: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse response");
    assert_eq!(
        json,
        serde_json::json!({"success": true, "filename": "routed.txt"}),
        "the success shape is the frozen one"
    );

    assert!(
        a.db.get_file("/routed.txt").expect("db read a").is_some(),
        "volume a sees the uploaded row"
    );
    assert!(
        b.db.get_file("/routed.txt").expect("db read b").is_none(),
        "volume b must not see volume a's upload"
    );

    // The queue counters route the same way: a's queue took the job,
    // b's stayed empty.
    let resp = send(addr, &request("GET", "/api/queue?volume=a", addr, &[], "")).await;
    let queue: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse queue");
    assert_eq!(queue["enqueued"], 1, "volume a's queue has the job");
    let resp = send(addr, &request("GET", "/api/queue?volume=b", addr, &[], "")).await;
    let queue: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse queue");
    assert_eq!(queue["enqueued"], 0, "volume b's queue stays empty");

    server.shutdown().await;
    a.vfs.shutdown().await;
    b.vfs.shutdown().await;
}

/// K23: `/api/files`, `/api/list` and `POST /api/delete` all route by
/// the volume parameter — a's rows never leak into b's listing, and a
/// delete on a touches only a's db.
#[tokio::test]
async fn files_list_delete_route_by_volume() {
    let (_dir, server, a, b) = two_volume_env().await;
    let addr = server.local_addr();
    seed_row(&b.db, "/docs/only-b.txt", false, 7, true);
    seed_row(&a.db, "/doomed.txt", false, 9, true);
    seed_row(&b.db, "/doomed.txt", false, 9, true);

    // files: each volume sees only its own rows.
    let resp = send(addr, &request("GET", "/api/files?volume=a", addr, &[], "")).await;
    let rows: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse files");
    let paths: BTreeSet<String> = rows
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["rel_path"].as_str().expect("str").to_string())
        .collect();
    assert_eq!(
        paths,
        BTreeSet::from([
            "/docs".to_string(),
            "/docs/only-a.txt".to_string(),
            "/doomed.txt".to_string()
        ]),
        "volume a's listing carries only a's rows"
    );

    // list: the sub-directory listing routes through the volume too.
    let resp = send(
        addr,
        &request("GET", "/api/list?path=/docs&volume=a", addr, &[], ""),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "list ok: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse list");
    let names: BTreeSet<&str> = body["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["rel_path"].as_str().expect("str"))
        .collect();
    assert_eq!(
        names,
        BTreeSet::from(["/docs/only-a.txt"]),
        "volume a's /docs lists only a's child"
    );

    // delete: removing /doomed.txt on volume a leaves b's copy alone.
    let resp = send(
        addr,
        &request(
            "POST",
            "/api/delete?volume=a",
            addr,
            &[("Content-Type", "application/json")],
            r#"{"filename":"doomed.txt"}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "delete ok: {resp}");
    assert!(
        a.db.get_file("/doomed.txt").expect("db read a").is_none(),
        "a's row is gone"
    );
    assert!(
        b.db.get_file("/doomed.txt").expect("db read b").is_some(),
        "b's same-named row survives"
    );
}

/// K23: `/api/download/{filename}?volume=a` hydrates through volume a's
/// VFS — a's bytes answer, and a name that exists only on b 404s the
/// Python way.
#[tokio::test]
async fn download_routes_by_volume() {
    let (_dir, server, a, _b) = two_volume_env().await;
    let addr = server.local_addr();
    let receipt = seed_remote(&a.mock, "/a-blob.bin", b"volume a payload").await;
    seed_uploaded_row(&a.db, "/a-blob.bin", b"volume a payload", &receipt);

    let resp = send(
        addr,
        &request("GET", "/api/download/a-blob.bin?volume=a", addr, &[], ""),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    assert_eq!(body_of(&resp), "volume a payload", "volume a's bytes");

    // The same name on volume b (nothing seeded there) 404s.
    let resp = send(
        addr,
        &request("GET", "/api/download/a-blob.bin?volume=b", addr, &[], ""),
    )
    .await;
    assert_eq!(status_of(&resp), 404, "b has no such row: {resp}");
    assert_eq!(
        body_of(&resp),
        "File not found in CyDrive cloud",
        "the Python 404 body is the frozen one"
    );
}

/// K24: `GET /api/stats/summary` aggregates over the RUNNING volumes —
/// Σ files / Σ bytes / Σ dirs / Σ uploaded / Σ pending from the dbs and
/// the quota sums over the snapshots that exist.
#[tokio::test]
async fn stats_summary_aggregates_running_volumes() {
    let (_dir, server, _a, _b) = two_volume_env().await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/api/stats/summary", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let summary: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse summary");

    assert_eq!(summary["volumes"], 2, "every registry entry counted");
    assert_eq!(summary["running"], 2, "both volumes assembled");
    assert_eq!(summary["total_files"], 3, "1 (a) + 2 (b)");
    assert_eq!(summary["total_bytes"], 210, "100 (a) + 110 (b)");
    assert_eq!(summary["total_dirs"], 1, "a's directory");
    assert_eq!(summary["uploaded_files"], 1, "a's uploaded row");
    assert_eq!(summary["pending_uploads"], 2, "b's pending rows");
    assert_eq!(summary["quota_used"], 5, "only a has a snapshot");
    assert_eq!(summary["quota_total"], 10, "only a has a ceiling");

    // A failed volume contributes neither numbers nor the running count.
    let dir = tempfile::tempdir().expect("temp dir");
    let a = volume_env(
        dir.path(),
        "a",
        volume_cfg("local", "V:", "a", 0, false, None, false, None),
    )
    .await;
    seed_row(&a.db, "/x.bin", false, 40, true);
    let failed = VolumeUiEntry {
        name: "broken".to_string(),
        status: VolumeUiStatus::Failed {
            reason: "boom".to_string(),
        },
        config: volume_cfg("baidu", "Z:", "broken", 0, false, None, false, None),
        vfs: None,
    };
    let server = multi_server(vec![a.entry, failed]).await;
    let resp = send(
        addr_for(&server),
        &request("GET", "/api/stats/summary", addr_for(&server), &[], ""),
    )
    .await;
    let summary: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse summary");
    assert_eq!(summary["volumes"], 2, "listed but not running");
    assert_eq!(summary["running"], 1);
    assert_eq!(summary["total_files"], 1, "only the running volume counts");
    assert_eq!(summary["total_bytes"], 40);
}

/// Small helper: the bound address of a server (test readability).
fn addr_for(server: &WebUiServer) -> SocketAddr {
    server.local_addr()
}

/// K23 single-volume face: the `?volume` parameter is IGNORED in
/// single-volume mode (there is one volume — the frozen behavior answers
/// unchanged), and the two registry routes stay absent (404) so the
/// single-volume route table is byte-identical to the pre-MV3 one.
#[tokio::test]
async fn single_volume_mode_ignores_volume_param_and_has_no_registry_routes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("single.db")).expect("open db"));
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("connect mock");
    let vfs = Arc::new(Vfs::new(
        db.clone(),
        CacheManager::new(dir.path().join("cache"), u64::MAX),
        mock.clone(),
        base_cfg(),
    ));
    seed_row(&db, "/only.txt", false, 42, true);
    let server = WebUiServer::serve(
        vfs.clone(),
        volume_cfg("telegram", "Y:", "x", 123456, true, None, false, None),
        SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .await
    .expect("serve single-volume dashboard");
    let addr = server.local_addr();

    // The frozen no-parameter behavior.
    let resp = send(addr, &request("GET", "/api/stats", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let plain: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse");

    // The same call with a stray volume parameter answers the identical
    // body (parameter ignored, not an error — there is no registry).
    let resp = send(addr, &request("GET", "/api/stats?volume=a", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "ignored, not refused: {resp}");
    let with_param: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse");
    assert_eq!(plain, with_param, "byte-identical stats body");
    assert_eq!(with_param["total_files"], 1);
    assert_eq!(with_param["total_bytes"], 42);

    // The registry routes do not exist in single-volume mode.
    for target in ["/api/volumes", "/api/stats/summary"] {
        let resp = send(addr, &request("GET", target, addr, &[], "")).await;
        assert_eq!(status_of(&resp), 404, "{target} absent: {resp}");
    }
}

/// A volume that claimed no drive letter reports `null` — the config
/// default "Y:" is a placeholder, not a mount claim, and surfacing it
/// would tell the dashboard the volume mounts as Y: when it mounts
/// nothing (K24 honesty; the cli assembly passes `None` for volumes
/// without an explicit `drive_letter`).
#[tokio::test]
async fn unclaimed_drive_letter_reports_null_not_the_default() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut cfg = volume_cfg("local", "V:", "free", 0, false, None, false, None);
    cfg.drive_letter = None;
    let env = volume_env(dir.path(), "free", cfg).await;
    seed_row(&env.db, "/f.txt", false, 7, true);
    let server = multi_server(vec![env.entry.clone()]).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/api/volumes", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let rows: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse");
    assert!(rows[0]["drive_letter"].is_null(), "row: {rows}");

    let resp = send(
        addr,
        &request("GET", "/api/stats?volume=free", addr, &[], ""),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let stats: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse");
    assert!(stats["drive_letter"].is_null(), "stats: {stats}");
}
