//! E2E contract tests for the web volume-management surface's P0 batch
//! (plan `docs/plans/2026-09-13-web-volume-management.md` §1.4/§1.5/§3):
//! the `/volumes` page (multi-volume skeleton, single-volume
//! explanation page), the dashboard's command seam and its
//! `GET /api/volumes/{name}/config` route, the `pending` column on
//! `/api/volumes`, and the same-origin guard mounted on the volume
//! management family. The masking itself (values never leaving the
//! backend) is pinned against the REAL serializer — the seam handler
//! calls `cloudkit_core::config::volume_show_json` exactly the way the
//! cli composition root's SHOW does; the full real-seam path (the cli
//! boot injecting its control-channel handler) lives in the cli test
//! suite (`web_volume_mgmt.rs`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::{MockTransport, UploadAction};
use cloudkit_core::transport::{CloudTransport, StorageError};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cloudkit_web::{
    QuotaSnapshot, RegistryHandle, VolumeCommandClient, VolumeUiEntry, VolumeUiStatus, WebUiConfig,
    WebUiServer,
};
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

/// One volume's dashboard knobs (the `multivolume.rs` baseline).
fn volume_cfg(backend: &str, letter: &str) -> WebUiConfig {
    WebUiConfig {
        drive_letter: Some(letter.to_string()),
        webdav_url: format!("http://127.0.0.1:8080/vol/{letter}"),
        chat_id: 0,
        is_configured: false,
        backend: backend.to_string(),
        volume: None,
        remote_delete: false,
        quota: Some(QuotaSnapshot {
            used: 5,
            total: Some(10),
        }),
    }
}

/// One assembled volume: its own db/mock/VFS plus the registry entry.
async fn volume_env(
    dir: &Path,
    name: &str,
    backend: &str,
    mock: Arc<MockTransport>,
) -> VolumeUiEntry {
    let db = Arc::new(MetaDatabase::open(&dir.join(format!("{name}.db"))).expect("open volume db"));
    let vfs = Arc::new(Vfs::new(
        db,
        CacheManager::new(dir.join(format!("{name}-cache")), u64::MAX),
        mock,
        base_cfg(),
    ));
    VolumeUiEntry {
        name: name.to_string(),
        status: VolumeUiStatus::Running,
        config: volume_cfg(backend, "V:"),
        vfs: Some(vfs),
    }
}

/// A mock transport whose uploads park on an authoritative
/// RateLimited{3600s} — the queue worker honors the wait, so an
/// enqueued job stays outstanding for the whole test (the runtime
/// volumes suite's "stuck queue" device).
async fn held_mock() -> Arc<MockTransport> {
    let mock = Arc::new(
        MockTransport::builder()
            .upload_action(UploadAction::Fail {
                error: StorageError::RateLimited {
                    retry_after: Some(Duration::from_secs(3600)),
                },
            })
            .build(),
    );
    mock.connect().await.expect("connect the held mock");
    mock
}

/// A healthy mock transport.
async fn idle_mock() -> Arc<MockTransport> {
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("connect the idle mock");
    mock
}

/// Serves the multi-volume dashboard WITHOUT the command seam (the
/// read-only boot).
async fn multi_server(entries: Vec<VolumeUiEntry>) -> WebUiServer {
    WebUiServer::serve_multi(
        RegistryHandle::new(entries),
        SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .await
    .expect("serve the multi-volume dashboard on an ephemeral port")
}

/// Serves the multi-volume dashboard WITH the command seam.
async fn multi_server_with_commands(
    entries: Vec<VolumeUiEntry>,
    commands: VolumeCommandClient,
) -> WebUiServer {
    WebUiServer::serve_multi_with_commands(
        RegistryHandle::new(entries),
        SocketAddr::from(([127, 0, 0, 1], 0)),
        Some(commands),
    )
    .await
    .expect("serve the multi-volume dashboard with the command seam")
}

/// A seam handler shaped like the cli composition root's: `SHOW <name>`
/// serializes the named volume file through the REAL core serializer —
/// the write-only masking this suite asserts against is the production
/// one, not a hand-rolled twin.
fn show_seam(volumes_dir: PathBuf) -> VolumeCommandClient {
    Arc::new(move |line: &str| {
        let path = line
            .strip_prefix("SHOW ")
            .map(|name| volumes_dir.join(format!("{name}.toml")));
        Box::pin(async move {
            match path.map(|path| cloudkit_core::config::volume_show_json(&path)) {
                Some(Ok(json)) => format!("OK: {json}"),
                Some(Err(error)) => format!("ERR: {error}"),
                None => "ERR: usage: SHOW <name>\n".to_string(),
            }
        })
    })
}

/// A seam handler that always answers the canned reply (the web-side
/// mapping tests: the endpoint's job is the HTTP shape, not the SHOW
/// semantics).
fn canned_seam(reply: String) -> VolumeCommandClient {
    Arc::new(move |line: &str| {
        let reply = reply.clone();
        Box::pin(async move {
            let _echo = line; // tie the future to the command's lifetime
            reply
        })
    })
}

/// Sends one raw HTTP/1.1 request (`Connection: close`) and reads the
/// response bytes to EOF.
async fn send(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect to server");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("send request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read to EOF");
    String::from_utf8_lossy(&raw).into_owned()
}

/// Builds an HTTP/1.1 request with Host, `Connection: close` and the
/// given extra headers.
fn request(method: &str, target: &str, addr: SocketAddr, extra: &[(&str, &str)]) -> String {
    let mut req = format!(
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
        addr.port()
    );
    for (name, value) in extra {
        req.push_str(&format!("{name}: {value}\r\n"));
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

/// The response body (everything after the blank line), dechunked when
/// the server answered chunked.
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

/// Decodes a chunked body (the `multivolume.rs` helper).
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

/// Writes a legal telegram-flavoured volume file with a FAKE credential
/// marker into `dir` (the seam's SHOW reads it back masked).
fn write_volume_file(dir: &Path, name: &str) {
    std::fs::create_dir_all(dir).expect("create the volumes dir");
    std::fs::write(
        dir.join(format!("{name}.toml")),
        format!(
            "backend = \"telegram\"\n\
             bot_token = \"111:FAKE-TOKEN-MARKER-{name}\"\n\
             chat_id = 4242\n"
        ),
    )
    .expect("write the volume file");
}

// ------------------------------------------------------------ the page ---

/// §1.4: the multi-volume `/volumes` page serves the management
/// skeleton — the sidebar nav (Cloud Drive back-link, Volumes active),
/// the four stat cards, the Add Volume button (disabled until P3) and
/// the seven-column volume table, wired to its own script.
#[tokio::test]
async fn volumes_page_serves_the_management_skeleton() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/volumes", addr, &[])).await;
    assert_eq!(status_of(&resp), 200, "the page answers: {resp}");
    assert!(resp.contains("text/html"), "html: {resp}");
    let body = body_of(&resp);
    for marker in [
        "href=\"/\"",
        "href=\"/volumes\"",
        "id=\"stat-volume-count\"",
        "id=\"stat-volume-running\"",
        "id=\"stat-volume-failed\"",
        "id=\"stat-volume-pending\"",
        "id=\"add-volume-btn\"",
        "coming in P3",
        "id=\"volumes-tbody\"",
        "<th>Name</th>",
        "<th>Backend</th>",
        "<th>Status</th>",
        "<th>Drive</th>",
        "<th>Pending</th>",
        "<th>Size</th>",
        "<th>Actions</th>",
        "/static/js/volumes.js",
    ] {
        assert!(body.contains(marker), "skeleton needs `{marker}`: {body}");
    }
}

/// 裁决④: a single-volume instance's `/volumes` is the explanation
/// page (single-volume volumes live in config.toml), not the management
/// table.
#[tokio::test]
async fn volumes_page_in_single_volume_mode_is_the_explanation_page() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("single.db")).expect("open db"));
    let mock = idle_mock().await;
    let vfs = Arc::new(Vfs::new(
        db,
        CacheManager::new(dir.path().join("cache"), u64::MAX),
        mock,
        base_cfg(),
    ));
    let server = WebUiServer::serve(
        vfs,
        volume_cfg("telegram", "Y:"),
        SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .await
    .expect("serve single-volume dashboard");
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/volumes", addr, &[])).await;
    assert_eq!(
        status_of(&resp),
        200,
        "the explanation page answers: {resp}"
    );
    let body = body_of(&resp);
    assert!(
        body.contains("id=\"volumes-single-note\""),
        "the single-volume note marker: {body}"
    );
    assert!(
        !body.contains("id=\"volumes-tbody\""),
        "no management table in single-volume mode: {body}"
    );
}

/// The usage-state page links to the management page (nav item + the
/// tabs-row ＋ chip target).
#[tokio::test]
async fn index_links_to_the_volumes_page() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/", addr, &[])).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let body = body_of(&resp);
    assert!(
        body.contains("href=\"/volumes\""),
        "the nav carries the Volumes item: {body}"
    );
}

/// The embedded static tree picked up the new script (rust-embed embeds
/// at compile time — a miss would leave the page's script tag pointing
/// at a 404). A pin, green since birth: the asset route predates it.
#[tokio::test]
async fn static_tree_serves_the_volumes_script() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/static/js/volumes.js", addr, &[])).await;
    assert_eq!(status_of(&resp), 200, "the script serves: {resp}");
    assert!(resp.contains("javascript"), "a JS content type: {resp}");
    assert!(
        body_of(&resp).contains("loadVolumesPage"),
        "the real script body, not a stub: {resp}"
    );
}

// ------------------------------------------------------ the pending column ---

/// The `/api/volumes` rows carry `pending` — the queue's outstanding
/// count for a running volume (a held upload reads 1, an idle one 0)
/// and `null` for a failed volume (no queue exists).
#[tokio::test]
async fn volumes_listing_carries_pending() {
    let dir = tempfile::tempdir().expect("temp dir");
    let held = volume_env(dir.path(), "held", "telegram", held_mock().await).await;
    let idle = volume_env(dir.path(), "idle", "local", idle_mock().await).await;
    let failed = VolumeUiEntry {
        name: "broken".to_string(),
        status: VolumeUiStatus::Failed {
            reason: "boom".to_string(),
        },
        config: volume_cfg("baidu", "Z:"),
        vfs: None,
    };

    // Enqueue one upload on the held volume: the worker parks on the
    // RateLimited wait, so the job stays outstanding.
    held.vfs
        .as_ref()
        .expect("running volume")
        .put(
            &RelPath::new("/held.bin").expect("valid rel path"),
            b"held bytes",
            1.0,
        )
        .await
        .expect("enqueue the held upload");

    let server = multi_server(vec![held, idle, failed]).await;
    let addr = server.local_addr();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let resp = send(addr, &request("GET", "/api/volumes", addr, &[])).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let rows: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse rows");
    let rows = rows.as_array().expect("array body");
    let pending_of = |name: &str| {
        rows.iter()
            .find(|row| row["name"] == name)
            .unwrap_or_else(|| panic!("row {name}: {rows:?}"))
            .get("pending")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    assert_eq!(
        pending_of("held"),
        serde_json::json!(1),
        "held upload outstanding"
    );
    assert_eq!(pending_of("idle"), serde_json::json!(0), "idle queue");
    assert!(
        pending_of("broken").is_null(),
        "a failed volume has no queue: null"
    );
}

// ------------------------------------------------ the config endpoint (seam) ---

/// Without the command seam the config endpoint answers its actionable
/// 503 (the dashboard booted without volume management).
#[tokio::test]
async fn volume_config_endpoint_requires_the_seam() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/api/volumes/a/config", addr, &[])).await;
    assert_eq!(status_of(&resp), 503, "no seam: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"].as_str().is_some_and(|msg| !msg.is_empty()),
        "actionable error: {body}"
    );
}

/// Through the seam the endpoint serves the SHOW reply's JSON with the
/// production masking intact: the fake credential's VALUE must not ride
/// the response in any form (write-only — the plan §1.2 red line).
#[tokio::test]
async fn volume_config_endpoint_serves_the_masked_show_reply() {
    let dir = tempfile::tempdir().expect("temp dir");
    let volumes_dir = dir.path().join("volumes");
    write_volume_file(&volumes_dir, "a");
    let entry = volume_env(dir.path(), "a", "telegram", idle_mock().await).await;
    let server = multi_server_with_commands(vec![entry], show_seam(volumes_dir)).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/api/volumes/a/config", addr, &[])).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    assert!(resp.contains("application/json"), "json: {resp}");
    let raw = body_of(&resp);
    assert!(
        !raw.contains("FAKE-TOKEN-MARKER"),
        "a credential value must never leave the backend: {raw}"
    );
    let value: serde_json::Value = serde_json::from_str(&raw).expect("parse the config json");
    assert_eq!(value["name"], "a");
    assert_eq!(value["backend"], "telegram");
    assert_eq!(value["bot_token"], serde_json::json!({"set": true}));
}

/// The endpoint maps the seam's answers: an `ERR:` reply carries its
/// actionable text as a 404, and a name outside the registry 404s with
/// the addressable volume list (the registry link the runtime table
/// gives REMOVE).
#[tokio::test]
async fn volume_config_endpoint_maps_err_and_unknown_names() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server_with_commands(
        vec![entry],
        canned_seam("ERR: no volume file for `a` under the volumes_dir\n".to_string()),
    )
    .await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/api/volumes/a/config", addr, &[])).await;
    assert_eq!(status_of(&resp), 404, "ERR maps to 404: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|msg| msg.contains("no volume file")),
        "the ERR text rides the error: {body}"
    );

    let resp = send(
        addr,
        &request("GET", "/api/volumes/ghost/config", addr, &[]),
    )
    .await;
    assert_eq!(status_of(&resp), 404, "unknown volume: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert_eq!(
        body["volumes"],
        serde_json::json!(["a"]),
        "the addressable list names the registry: {body}"
    );
}

// ------------------------------------------------ the same-origin guard (§1.5) ---

/// The volume-management family rejects cross-origin requests (裁决③):
/// a foreign Origin, a foreign Referer and the `Origin: null` spelling
/// all 403 — the write routes this family grows in P1+ inherit the
/// guard.
#[tokio::test]
async fn volume_management_family_rejects_cross_origin() {
    let dir = tempfile::tempdir().expect("temp dir");
    let volumes_dir = dir.path().join("volumes");
    write_volume_file(&volumes_dir, "a");
    let entry = volume_env(dir.path(), "a", "telegram", idle_mock().await).await;
    let server = multi_server_with_commands(vec![entry], show_seam(volumes_dir)).await;
    let addr = server.local_addr();

    let foreign: &[(&str, &str)] = &[
        ("Origin", "http://evil.example"),
        ("Origin", "https://127.0.0.1"),
        ("Origin", "null"),
        ("Referer", "http://evil.example/attack.html"),
    ];
    for (name, value) in foreign {
        for target in ["/api/volumes", "/api/volumes/a/config"] {
            let resp = send(addr, &request("GET", target, addr, &[(name, value)])).await;
            assert_eq!(
                status_of(&resp),
                403,
                "cross-origin {name}: {value} on {target}: {resp}"
            );
        }
    }
}

/// What the guard lets through: no Origin/Referer at all (non-browser
/// clients, address-bar navigation), a same-origin Origin and a
/// same-origin Referer.
#[tokio::test]
async fn origin_guard_allows_same_origin_and_headerless_requests() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();
    let same_origin = format!("http://127.0.0.1:{}", addr.port());
    let same_referer = format!("http://127.0.0.1:{}/volumes", addr.port());

    for headers in [
        Vec::<(&str, &str)>::new(),
        vec![("Origin", same_origin.as_str())],
        vec![("Referer", same_referer.as_str())],
    ] {
        let resp = send(addr, &request("GET", "/api/volumes", addr, &headers)).await;
        assert_eq!(
            status_of(&resp),
            200,
            "legitimate request with {headers:?} must pass: {resp}"
        );
    }
}

/// The guard's scope is the volume-management family only: the read
/// routes (stats, downloads — linkable from anywhere) and the page
/// routes stay open even with a foreign Origin.
#[tokio::test]
async fn origin_guard_scoped_to_the_volume_management_family() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();

    for target in [
        "/api/stats?volume=a",
        "/api/queue?volume=a",
        "/volumes",
        "/",
    ] {
        let resp = send(
            addr,
            &request("GET", target, addr, &[("Origin", "http://evil.example")]),
        )
        .await;
        assert_eq!(
            status_of(&resp),
            200,
            "the read/page route {target} stays open to foreign origins: {resp}"
        );
    }
}
