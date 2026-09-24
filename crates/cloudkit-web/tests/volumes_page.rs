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

/// Builds an HTTP/1.1 request with an arbitrary Host header and an exact
/// Content-Length for `body` (the DNS-rebinding shape puts the attacker's
/// authority in the Host itself — the shared helper pins the legal
/// loopback spelling).
fn request_with_host(
    method: &str,
    target: &str,
    host: &str,
    extra: &[(&str, &str)],
    body: &str,
) -> String {
    let mut req = format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (name, value) in extra {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    req.push_str(body);
    req
}

/// Hand-rolled `multipart/form-data` body with exactly one field named
/// `file` (the `web_e2e.rs` shape — the only form the frontend's
/// `FormData.append("file", ...)` produces).
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
/// the four stat cards, the Add Volume button (live since P3 — the
/// placeholder pin retired with the placeholder) with its form card,
/// and the seven-column volume table, wired to its own script.
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
        // The P3 form card: the volume form with its backend radio, the
        // credential groups and the advanced section.
        "id=\"volume-form-card\"",
        "id=\"volume-form\"",
        "name=\"vf-backend\"",
        "id=\"vf-group-telegram\"",
        "id=\"vf-group-baidu\"",
        "id=\"vf-group-local\"",
        "id=\"vf-name\"",
        "id=\"vf-drive\"",
        "id=\"vf-enabled\"",
        "id=\"vf-advanced\"",
        "id=\"vf-error\"",
        "id=\"volumes-tbody\"",
        // The table headers keep their English text (the i18n contract);
        // each now carries its data-i18n hook (mechanical adaptation).
        "<th data-i18n=\"table.name\">Name</th>",
        "<th data-i18n=\"table.backend\">Backend</th>",
        "<th data-i18n=\"table.status\">Status</th>",
        "<th data-i18n=\"table.drive\">Drive</th>",
        "<th data-i18n=\"table.pending\">Pending</th>",
        "<th data-i18n=\"table.size\">Size</th>",
        "<th data-i18n=\"table.actions\">Actions</th>",
        "/static/js/volumes.js",
    ] {
        assert!(body.contains(marker), "skeleton needs `{marker}`: {body}");
    }
}

/// UI consistency batch (负责人 fix): the config section's mid-sidebar
/// block is the dashboard's views-section twin — the same nav-menu
/// vocabulary (the small section label + flat nav-item entries), the
/// config-submenu special block gone wholesale; the exact-path active
/// entry is `/volumes` here and the read-only badge lives inside the
/// system entry.
#[tokio::test]
async fn volumes_page_carries_the_config_section_as_nav_items() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/volumes", addr, &[])).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let body = body_of(&resp);
    assert!(
        !body.contains("config-sub"),
        "the config-submenu special block is gone wholesale: {body}"
    );
    let section = body
        .split("nav-section-title")
        .nth(1)
        .and_then(|rest| rest.split("</nav>").next())
        .expect("the config section block");
    assert!(
        section.contains("data-i18n=\"sidebar.section_config\""),
        "the section label shares the views section's small-label style: {section}"
    );
    assert!(
        section.contains("href=\"/volumes\" class=\"nav-item active\""),
        "the volume entry is the exact-path active nav-item on /volumes: {section}"
    );
    assert!(
        !section.contains("href=\"/volumes/system\" class=\"nav-item active\""),
        "exact-path matching: the system entry stays inactive here: {section}"
    );
    let system_item = section
        .split("href=\"/volumes/system\"")
        .nth(1)
        .and_then(|rest| rest.split("</a>").next())
        .expect("the system entry");
    assert!(
        system_item.contains("class=\"nav-item\""),
        "the system entry is a flat nav-item: {system_item}"
    );
    assert!(
        system_item.contains("ro-badge") && system_item.contains("data-i18n=\"config.readonly\""),
        "the read-only badge lives inside the system entry: {system_item}"
    );
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

/// UI polish round 2, item 4: `GET /volumes/system` (multi-volume mode)
/// serves the read-only system-parameters skeleton — the system title +
/// its read-only subtitle, the instance section's four faces, the
/// collapsible volume-parameter cards host, the config section in the
/// shared nav-item vocabulary (system entry exact-path active,
/// read-only badge), NO primary action button, and its own script
/// behind i18n.js.
#[tokio::test]
async fn system_page_serves_the_readonly_skeleton() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/volumes/system", addr, &[])).await;
    assert_eq!(status_of(&resp), 200, "the page answers: {resp}");
    assert!(resp.contains("text/html"), "html: {resp}");
    let body = body_of(&resp);
    for marker in [
        // The page skeleton: the i18n-keyed title and its read-only
        // subtitle.
        "data-i18n=\"system.title\"",
        "data-i18n=\"system.subtitle\"",
        // The instance section's four faces (JS-filled).
        "id=\"sys-dashboard-url\"",
        "id=\"sys-webdav-endpoints\"",
        "id=\"sys-volume-files\"",
        "id=\"sys-running\"",
        // The per-volume collapsible cards host.
        "id=\"sys-volume-cards\"",
        // The config section in the views section's nav-item vocabulary:
        // the small section label, this page's entry exact-path active,
        // both pages listed, the read-only badge.
        "data-i18n=\"sidebar.section_config\"",
        "href=\"/volumes/system\" class=\"nav-item active\"",
        "href=\"/volumes\" class=\"nav-item\"",
        "ro-badge",
        // The page pill's configuration segment is the active one.
        "data-i18n=\"pages.config\"",
    ] {
        assert!(body.contains(marker), "skeleton needs `{marker}`: {body}");
    }
    assert!(
        !body.contains("config-sub"),
        "the config-submenu special block is gone here too: {body}"
    );
    // Read-only by construction: no primary action button in the topbar
    // (the language pill and the round refresh are the only controls).
    let topbar = body
        .split("<header class=\"topbar\">")
        .nth(1)
        .and_then(|rest| rest.split("</header>").next())
        .expect("the topbar block")
        .to_string();
    assert!(
        !topbar.contains("btn-primary"),
        "a read-only page carries no primary action: {topbar}"
    );
    assert!(
        topbar.contains("lang-pill") && topbar.contains("btn-icon"),
        "the shared topbar shape (lang pill + refresh): {topbar}"
    );
    // i18n.js loads before the page script (the overlay contract).
    assert!(
        body.find("<script src=\"/static/js/i18n.js\"").unwrap()
            < body.find("<script src=\"/static/js/system.js\"").unwrap(),
        "i18n.js loads before system.js: {body}"
    );
}

/// UI polish round 2, item 4: a single-volume instance's
/// `/volumes/system` is the explanation page (its parameters ARE its
/// config.toml — the cards host has nothing to iterate), not the
/// multi-volume skeleton.
#[tokio::test]
async fn system_page_in_single_volume_mode_is_the_explanation_page() {
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

    let resp = send(addr, &request("GET", "/volumes/system", addr, &[])).await;
    assert_eq!(
        status_of(&resp),
        200,
        "the explanation page answers: {resp}"
    );
    let body = body_of(&resp);
    assert!(
        body.contains("id=\"system-single-note\""),
        "the single-volume note marker: {body}"
    );
    assert!(
        body.contains("config.toml"),
        "the note names the defining file: {body}"
    );
    assert!(
        !body.contains("id=\"sys-volume-cards\""),
        "no parameters host in single-volume mode: {body}"
    );
}

/// The embedded static tree picked up the system page's script
/// (rust-embed embeds at compile time — a miss would leave the page's
/// script tag pointing at a 404).
#[tokio::test]
async fn static_tree_serves_the_system_script() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/static/js/system.js", addr, &[])).await;
    assert_eq!(status_of(&resp), 200, "the script serves: {resp}");
    assert!(resp.contains("javascript"), "a JS content type: {resp}");
    assert!(
        body_of(&resp).contains("loadSystemPage"),
        "the real script body, not a stub: {resp}"
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

/// Phase 8-B EB4 (B5 反分裂): since EB3 the backend accepts REBUILD on
/// encrypted volumes — the web UI must stop hiding/disabling those
/// controls (gate narrows to telegram only) and the "refuses encrypted"
/// copy must go. Content pins over the served scripts (no JS harness
/// exists; these are the assertions): all five assertions are RED until
/// the four static-file spots are synced.
#[tokio::test]
async fn rebuild_controls_gate_on_telegram_only_after_phase8b() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();

    let fetch = |path: &'static str| async move {
        let resp = send(addr, &request("GET", path, addr, &[])).await;
        assert_eq!(status_of(&resp), 200, "{path} serves: {resp}");
        body_of(&resp)
    };

    // i18n (both languages): the rebuild tooltip names telegram only,
    // and the dead "refresh refuses encrypted" key is gone.
    let i18n = fetch("/static/js/i18n.js").await;
    assert!(
        !i18n.contains("(telegram/encrypted)"),
        "EN rebuild tooltip narrowed to telegram: {i18n}"
    );
    assert!(
        !i18n.contains("telegram/加密卷"),
        "ZH rebuild tooltip narrowed to telegram: {i18n}"
    );
    assert!(
        i18n.contains("(telegram):") && i18n.contains("（telegram）"),
        "both languages keep the narrowed (telegram) spelling: {i18n}"
    );
    assert!(
        !i18n.contains("refresh_encrypted")
            && !i18n.contains("Refresh refuses encrypted")
            && !i18n.contains("加密实例拒绝刷新"),
        "the misleading encrypted-refresh note key is gone: {i18n}"
    );

    // volumes.js: the Refresh control no longer mutes encrypted rows.
    let volumes_js = fetch("/static/js/volumes.js").await;
    assert!(
        !volumes_js.contains("v.encrypted"),
        "encrypted volumes render the real Refresh button: {volumes_js}"
    );
    assert!(
        volumes_js.contains("v.backend === 'telegram'"),
        "the telegram gate itself stays: {volumes_js}"
    );

    // app.js: the index rebuild button is disabled for telegram only.
    let app_js = fetch("/static/js/app.js").await;
    assert!(
        !app_js.contains("row.encrypted"),
        "encrypted rows no longer disable the rebuild button: {app_js}"
    );
}

/// The i18n/layout batch (2026-09-13 UX follow-up): the `/volumes` page
/// carries the `data-i18n` hooks and the dual-segment language pill,
/// loads `i18n.js` before its page script, anchors the bottom page
/// switcher, and the external links (GitHub / Cynet Security) are gone.
/// Static element text stays English — the raw-HTML contract the tests
/// above pin is untouched; zh is a JS overlay.
#[tokio::test]
async fn volumes_page_is_bilingual_ready_and_prunes_external_links() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/volumes", addr, &[])).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let body = body_of(&resp);
    for marker in [
        "data-i18n=",
        "data-i18n-placeholder=",
        "data-i18n-title=",
        // The dual-segment pill (`[中|EN]`) in the topbar actions.
        "lang-pill",
        "data-lang=\"zh\"",
        "data-lang=\"en\"",
        // i18n.js loads before the page script.
        "/static/js/i18n.js",
        // The bottom-anchored page switcher (Files / Volumes) — since
        // the ui-polish batch a single two-segment pill (the sidebar
        // twin of the language pill).
        "sidebar-pages",
        "page-switch",
        "page-seg",
        "href=\"/\"",
        "href=\"/volumes\"",
    ] {
        assert!(
            body.contains(marker),
            "i18n/layout needs `{marker}`: {body}"
        );
    }
    assert!(
        body.contains("<script src=\"/static/js/i18n.js\"></script>"),
        "i18n.js is a loaded script, not a stray string: {body}"
    );
    assert!(
        body.find("<script src=\"/static/js/i18n.js\"").unwrap()
            < body.find("<script src=\"/static/js/volumes.js\"").unwrap(),
        "i18n.js loads before volumes.js: {body}"
    );
    for gone in ["github.com", "cynetx.ir", "Cynet Security"] {
        assert!(
            !body.contains(gone),
            "the external link `{gone}` is pruned: {body}"
        );
    }
    // The sidebar storage card is gone; its aggregate moved into the
    // stats grid's fifth card.
    assert!(
        !body.contains("class=\"storage-card\""),
        "the sidebar storage card is gone: {body}"
    );
    assert!(
        body.contains("id=\"stat-total-storage\""),
        "the Total Storage stat card: {body}"
    );
    // This page's switcher pill: Volumes is the active segment.
    let pages = body.split("sidebar-pages").nth(1).expect("the pages block");
    assert!(
        pages.contains("page-seg active") && pages.contains("href=\"/volumes\""),
        "Volumes is the active page segment: {pages}"
    );
    // The topbar keeps its unified shape: the title block on the left
    // (title + subtitle), actions on the right.
    assert!(
        body.contains("class=\"topbar-title\""),
        "the topbar's title block: {body}"
    );
}

/// The index page's half of the i18n/layout batch: the same i18n hooks
/// and pill, the pruned external links, and the sidebar storage card's
/// content migrated into the stats grid (the `storage-*` ids survive —
/// app.js keeps filling them — but the sidebar chassis is gone).
///
/// The ui-polish batch (2026-09-12 feedback) adds: the two-segment
/// page-switch pill, the topbar in the same shape as /volumes (title
/// left, actions right — the search lives in the files section-title
/// row now), no cross-volume summary card (five stat cards), the
/// WebDAV card's demoted URL line, and the pagination host under the
/// files table.
#[tokio::test]
async fn index_page_is_bilingual_ready_and_storage_card_moved() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/", addr, &[])).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let body = body_of(&resp);
    for marker in [
        "data-i18n=",
        "lang-pill",
        "data-lang=\"zh\"",
        "data-lang=\"en\"",
        "<script src=\"/static/js/i18n.js\"></script>",
        // The two-segment page switch pill at the sidebar's bottom.
        "sidebar-pages",
        "page-switch",
        "page-seg",
        // The migrated storage card keeps its ids inside the stats grid.
        "id=\"storage-percent\"",
        "id=\"storage-detail\"",
        "id=\"drive-letter\"",
        // The unified topbar's title block.
        "class=\"topbar-title\"",
        // The WebDAV card: the short headline, the URL demoted to a
        // muted mono line the app.js wiring still fills.
        "data-i18n=\"webdav.running\"",
        "id=\"webdav-endpoint\"",
        "class=\"webdav-url\"",
        // The pagination controls are JS-rendered into this static host.
        "id=\"pagination\"",
    ] {
        assert!(
            body.contains(marker),
            "i18n/layout needs `{marker}`: {body}"
        );
    }
    for gone in ["github.com", "cynetx.ir", "Cynet Security"] {
        assert!(
            !body.contains(gone),
            "the external link `{gone}` is pruned: {body}"
        );
    }
    assert!(
        !body.contains("class=\"storage-card\""),
        "the sidebar storage card chassis is gone: {body}"
    );
    // The cross-volume aggregate card is gone for good (负责人 feedback:
    // the stats grid keeps exactly five cards).
    assert!(
        !body.contains("summary-card"),
        "the cross-volume summary card is gone from the index: {body}"
    );
    // Cloud Drive left the middle nav (merged into the bottom switcher);
    // the two filter items stay as the views section.
    assert!(
        !body.contains("filterType('all')"),
        "the Cloud Drive filter item is gone: {body}"
    );
    assert!(
        body.contains("filterType('media', event)")
            && body.contains("filterType('documents', event)"),
        "the view filter items stay: {body}"
    );
    // This page's switcher pill: Files is the active segment.
    let pages = body.split("sidebar-pages").nth(1).expect("the pages block");
    assert!(
        pages.contains("page-seg active") && pages.contains("href=\"/\""),
        "Files is the active page segment: {pages}"
    );
    // UI consistency: the usage-state page keeps the views section and
    // carries NO config section — the config vocabulary is the
    // config-side pages' affordance alone.
    assert!(
        !body.contains("config-sub") && !body.contains("sidebar.section_config"),
        "the config section stays off the index page: {body}"
    );
    assert!(
        body.contains("data-i18n=\"sidebar.views\"") && body.contains("class=\"nav-item\""),
        "the views section stays in the shared nav-item vocabulary: {body}"
    );
    // UI polish round 2 (item 1): the topbar is the main-content's FIRST
    // child — the volume tabs row follows it (one visual order across
    // the pages: title row on top, the page's secondary elements after).
    let main_at = body
        .find("<main class=\"main-content\">")
        .expect("the main block");
    let topbar_at = body
        .find("<header class=\"topbar\">")
        .expect("the topbar somewhere");
    let tabs_at = body.find("id=\"volume-tabs\"").expect("the tabs row");
    assert!(
        main_at < topbar_at && topbar_at < tabs_at,
        "the topbar leads the main content, the tabs follow it: \
         main {main_at} < topbar {topbar_at} < tabs {tabs_at}"
    );
    // The search box moved out of the topbar into the files section's
    // title row (title left, search right) — the topbar block itself
    // carries no search input, and the input sits after the section.
    let topbar = body
        .split("<header class=\"topbar\">")
        .nth(1)
        .and_then(|rest| rest.split("</header>").next())
        .expect("the topbar block")
        .to_string();
    assert!(
        !topbar.contains("search-input"),
        "the search left the topbar: {topbar}"
    );
    let search_at = body
        .find("id=\"search-input\"")
        .expect("the search input somewhere");
    let section_at = body
        .find("class=\"section-title\"")
        .expect("the files section title");
    assert!(
        search_at > section_at,
        "the search lives in the section-title row: search {search_at} vs section {section_at}"
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

/// M3: the config endpoint answers by FILE, not registry membership — a
/// volume file with no registry entry (disabled at boot, or REMOVE'd
/// with the file kept, K49) still serves its masked SHOW reply; that is
/// what keeps the Edit repair loop open for stopped/disabled volumes.
/// The seam's own ERR text stays the authority on names with no file.
#[tokio::test]
async fn volume_config_endpoint_serves_unregistered_volume_files() {
    let dir = tempfile::tempdir().expect("temp dir");
    let volumes_dir = dir.path().join("volumes");
    write_volume_file(&volumes_dir, "shelved");
    // The registry is EMPTY: the file exists, the runtime does not.
    let server = multi_server_with_commands(vec![], show_seam(volumes_dir)).await;
    let addr = server.local_addr();

    let resp = send(
        addr,
        &request("GET", "/api/volumes/shelved/config", addr, &[]),
    )
    .await;
    assert_eq!(
        status_of(&resp),
        200,
        "a file-backed volume answers SHOW regardless of registry membership: {resp}"
    );
    let raw = body_of(&resp);
    assert!(
        !raw.contains("FAKE-TOKEN-MARKER"),
        "the production masking still rides the reply: {raw}"
    );
    let value: serde_json::Value = serde_json::from_str(&raw).expect("parse the config json");
    assert_eq!(value["name"], "shelved");
    assert_eq!(value["bot_token"], serde_json::json!({"set": true}));
}

/// The endpoint maps the seam's answers: an `ERR:` reply carries its
/// actionable text as a 404, and a name the seam refuses (K58-M3: the
/// registry pre-gate is gone — the seam's own ERR text is the authority
/// on unknown names, the cli's SHOW naming the volumes_dir) 404s the
/// same way.
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
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|msg| msg.contains("no volume file")),
        "the seam's ERR text is the unknown-name answer (K58-M3: no registry list): {body}"
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

// ------------------------------------------- the Host allowlist (K58-H5) ---

/// H5: the guard's second verdict. Comparing Origin against Host alone
/// cannot catch DNS rebinding — the attacker's page is served from a
/// domain that later resolves to the dashboard's own address, so Host
/// and Origin AGREE on the attacker's authority and the §1.5 verdict
/// waves the forged request through into the handler. The Host
/// allowlist (derived from the bound address) refuses that shape: an
/// authority naming no bound spelling answers 403 naming the rebinding
/// suspicion — BEFORE the seam is reached.
#[tokio::test]
async fn guard_refuses_the_dns_rebinding_shape_on_the_management_family() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server_with_commands(
        vec![entry],
        canned_seam("ERR: no volume registered under `a`\n".to_string()),
    )
    .await;
    let addr = server.local_addr();

    let evil = format!("evil.example:{}", addr.port());
    let origin = format!("http://{evil}");
    let resp = send(
        addr,
        &request_with_host(
            "POST",
            "/api/volumes/a/destroy",
            &evil,
            &[("Origin", origin.as_str())],
            "",
        ),
    )
    .await;
    assert_eq!(
        status_of(&resp),
        403,
        "Host == Origin == the attacker's authority must be refused before the seam: {resp}"
    );
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|msg| msg.contains("rebinding")),
        "the refusal names the suspicion: {body}"
    );
}

/// H5 stock-closing: `/api/upload` joins the guard — it had NO Origin
/// protection at all, and a multipart POST is a simple request (no
/// preflight), so a cross-site page can drive it. The same rebinding
/// shape 403s BEFORE any multipart parsing or volume routing.
#[tokio::test]
async fn upload_route_refuses_the_dns_rebinding_shape() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();

    let evil = format!("evil.example:{}", addr.port());
    let origin = format!("http://{evil}");
    let body = multipart_body("k58rebind", "forged.txt", "forged");
    let content_type = "multipart/form-data; boundary=k58rebind";
    let resp = send(
        addr,
        &request_with_host(
            "POST",
            "/api/upload?volume=a",
            &evil,
            &[("Origin", origin.as_str()), ("Content-Type", content_type)],
            &body,
        ),
    )
    .await;
    assert_eq!(
        status_of(&resp),
        403,
        "the forged-Host upload must be refused before the multipart parse: {resp}"
    );
}

/// H5 no-false-positive pin: the upload guard keeps the contract route
/// working exactly as before — a non-browser client (curl's shape: legal
/// Host, no Origin/Referer) and the dashboard's own same-origin uploads
/// both pass, and the Host comparison is case-insensitive (`LOCALHOST`
/// is the same authority as `localhost`).
#[tokio::test]
async fn upload_guard_keeps_legitimate_uploads_working() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();

    // One upload per shape, asserted inline (the label rides the failure
    // messages).
    async fn upload(label: &str, addr: SocketAddr, host: &str, extra: &[(&str, &str)]) {
        let content_type = "multipart/form-data; boundary=k58legit";
        let mut headers = vec![("Content-Type", content_type)];
        headers.extend(extra);
        let body = multipart_body("k58legit", "legit.txt", "legitimate");
        let resp = send(
            addr,
            &request_with_host("POST", "/api/upload?volume=a", host, &headers, &body),
        )
        .await;
        assert_eq!(status_of(&resp), 200, "{label} must keep uploading: {resp}");
        assert!(
            body_of(&resp).contains("\"success\":true"),
            "{label} — the handler itself must have run: {resp}"
        );
    }

    let same_origin = format!("http://127.0.0.1:{}", addr.port());
    let local_referer = format!("http://localhost:{}/volumes", addr.port());
    let ip_host = format!("127.0.0.1:{}", addr.port());
    let name_host = format!("localhost:{}", addr.port());

    // The curl shape: legal Host, no Origin/Referer (the "no headers =
    // not a browser" verdict stays).
    upload("curl shape", addr, &ip_host, &[]).await;
    // The dashboard's own same-origin uploads.
    upload(
        "same-origin Origin",
        addr,
        &ip_host,
        &[("Origin", same_origin.as_str())],
    )
    .await;
    // The localhost spelling reaches the same loopback listener.
    upload(
        "localhost Host + matching Referer",
        addr,
        &name_host,
        &[("Referer", local_referer.as_str())],
    )
    .await;
    // Host comparison is case-insensitive (LOCALHOST == localhost).
    let upper_host = name_host.to_uppercase();
    let upper_origin = format!("http://{name_host}");
    upload(
        "case-insensitive Host spelling",
        addr,
        &upper_host,
        &[("Origin", upper_origin.as_str())],
    )
    .await;
}

// -------------------------- the P1 write routes + configs endpoint (§1.2/§1.5) ---

/// A seam that records every command it received and answers the canned
/// reply — the write routes' transport.
fn recording_seam(reply: String) -> (VolumeCommandClient, Arc<std::sync::Mutex<Vec<String>>>) {
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let seen_for_closure = Arc::clone(&seen);
    let client: VolumeCommandClient = Arc::new(move |line: &str| {
        seen_for_closure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(line.to_string());
        let reply = reply.clone();
        Box::pin(async move { reply })
    });
    (client, seen)
}

/// The three write routes forward their commands through the seam and
/// answer the pinned success shape: `{"ok": true, "reply": "..."}` with
/// the `OK: ` prefix stripped (the dashboard toasts the reply verbatim).
#[tokio::test]
async fn write_routes_forward_their_commands_through_the_seam() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, seen) = recording_seam("OK: removed volume `a`\n".to_string());
    let server = multi_server_with_commands(vec![entry], seam).await;
    let addr = server.local_addr();

    for target in [
        "/api/volumes/a/remove",
        "/api/volumes/a/disable",
        "/api/volumes/a/enable",
    ] {
        let resp = send(addr, &request("POST", target, addr, &[])).await;
        assert_eq!(status_of(&resp), 200, "{target} ok: {resp}");
        assert!(resp.contains("application/json"), "json: {resp}");
        let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse body");
        assert_eq!(
            body,
            serde_json::json!({ "ok": true, "reply": "removed volume `a`" }),
            "the pinned success shape ({target}): {body}"
        );
    }
    assert_eq!(
        *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec!["REMOVE a", "DISABLE a", "ENABLE a"],
        "each route sends its own command line"
    );
}

/// The write routes map the seam's `ERR:` replies to 409 with the
/// actionable text verbatim (a refused mutation is a conflict, and the
/// dashboard toasts the text as-is).
#[tokio::test]
async fn write_routes_map_err_replies_to_409_with_the_text() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server_with_commands(
        vec![entry],
        canned_seam(
            "ERR: no volume registered under `ghost` — `LIST` shows the current set\n".to_string(),
        ),
    )
    .await;
    let addr = server.local_addr();

    let resp = send(
        addr,
        &request("POST", "/api/volumes/ghost/remove", addr, &[]),
    )
    .await;
    assert_eq!(status_of(&resp), 409, "ERR maps to 409: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|msg| msg.contains("no volume registered")),
        "the ERR text rides the error verbatim: {body}"
    );
}

/// Without the command seam the write routes answer their actionable
/// 503 (the read-only dashboard boot).
#[tokio::test]
async fn write_routes_require_the_seam() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();

    for target in [
        "/api/volumes/a/remove",
        "/api/volumes/a/disable",
        "/api/volumes/a/enable",
    ] {
        let resp = send(addr, &request("POST", target, addr, &[])).await;
        assert_eq!(status_of(&resp), 503, "no seam ({target}): {resp}");
        let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
        assert!(
            body["error"].as_str().is_some_and(|msg| !msg.is_empty()),
            "actionable error: {body}"
        );
    }
}

/// The P0 Origin middleware covers the write routes (裁决③: the
/// state-changing family is what it exists for): a foreign Origin 403s
/// BEFORE the seam is reached.
#[tokio::test]
async fn write_routes_inherit_the_origin_guard() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, seen) = recording_seam("OK: removed volume `a`\n".to_string());
    let server = multi_server_with_commands(vec![entry], seam).await;
    let addr = server.local_addr();

    let resp = send(
        addr,
        &request(
            "POST",
            "/api/volumes/a/remove",
            addr,
            &[("Origin", "http://evil.example")],
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 403, "cross-origin POST refused: {resp}");
    assert!(
        seen.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty(),
        "the guard refuses before the seam is reached"
    );
}

/// §1.5's non-loopback degrade: binding a non-loopback address withholds
/// the management WRITE family (403 naming `allow_remote_admin`) while
/// the reads — including the configs listing — stay open; the explicit
/// opt-in restores the writes.
#[tokio::test]
async fn non_loopback_binding_forbids_write_routes_without_the_opt_in() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, _seen) =
        recording_seam("OK: 1 volume file(s)\na local enabled=true running\n".to_string());
    let server = WebUiServer::serve_multi_with_remote_admin(
        RegistryHandle::new(vec![entry]),
        // 0.0.0.0 is the standard "listen everywhere" binding the guard
        // exists for; the client reaches it over the loopback interface.
        "0.0.0.0:0".parse().expect("parse the bind address"),
        Some(seam),
        false,
    )
    .await
    .expect("serve on a non-loopback binding");
    // Windows refuses to CONNECT to 0.0.0.0 (AddrNotAvailable): the
    // client reaches the wildcard binding over the loopback interface.
    let addr = SocketAddr::from(([127, 0, 0, 1], server.local_addr().port()));

    for target in [
        "/api/volumes/a/remove",
        "/api/volumes/a/disable",
        "/api/volumes/a/enable",
    ] {
        let resp = send(addr, &request("POST", target, addr, &[])).await;
        assert_eq!(status_of(&resp), 403, "write withheld ({target}): {resp}");
        let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|msg| msg.contains("allow_remote_admin")),
            "the refusal names the opt-in key: {body}"
        );
    }
    let resp = send(addr, &request("GET", "/api/volumes", addr, &[])).await;
    assert_eq!(
        status_of(&resp),
        200,
        "the read listing stays open (read-only degrade): {resp}"
    );
    let resp = send(addr, &request("GET", "/api/volumes/configs", addr, &[])).await;
    assert_eq!(
        status_of(&resp),
        200,
        "the configs listing is a read: {resp}"
    );
    server.shutdown().await;

    // The opt-in restores the writes.
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, _seen) = recording_seam("OK: removed volume `a`\n".to_string());
    let server = WebUiServer::serve_multi_with_remote_admin(
        RegistryHandle::new(vec![entry]),
        "0.0.0.0:0".parse().expect("parse the bind address"),
        Some(seam),
        true,
    )
    .await
    .expect("serve with the opt-in");
    let addr = SocketAddr::from(([127, 0, 0, 1], server.local_addr().port()));
    let resp = send(addr, &request("POST", "/api/volumes/a/remove", addr, &[])).await;
    assert_eq!(
        status_of(&resp),
        200,
        "allow_remote_admin = true restores the write family: {resp}"
    );
    server.shutdown().await;
}

/// `GET /api/volumes/configs` (the config page's data source): one
/// `CONFIGS` through the seam, the row lines parsed into JSON — good
/// rows as `{name, backend, enabled, running}`, broken files as
/// `{name, invalid, reason}`. An `ERR:` reply carries its text as a 404
/// (the P0 config endpoint's mapping) and a missing seam the 503.
#[tokio::test]
async fn configs_endpoint_serves_the_parsed_listing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, seen) = recording_seam(
        "OK: 3 volume file(s)\n\
         a telegram enabled=true running\n\
         b local enabled=false absent\n\
         broken invalid (unknown key `no_such_key`)\n"
            .to_string(),
    );
    let server = multi_server_with_commands(vec![entry], seam).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/api/volumes/configs", addr, &[])).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    assert!(
        resp.contains("application/json"),
        "json content type: {resp}"
    );
    let rows: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse rows");
    assert_eq!(
        rows,
        serde_json::json!([
            { "name": "a", "backend": "telegram", "enabled": true, "running": true },
            { "name": "b", "backend": "local", "enabled": false, "running": false },
            { "name": "broken", "invalid": true, "reason": "unknown key `no_such_key`" },
        ]),
        "the CONFIGS rows parsed into the pinned JSON shapes: {rows}"
    );
    assert_eq!(
        *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec!["CONFIGS".to_string()],
        "the endpoint sends exactly the CONFIGS command"
    );
    server.shutdown().await;

    // An ERR reply maps to 404 with its text.
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server_with_commands(
        vec![entry],
        canned_seam("ERR: this instance runs single-volume mode\n".to_string()),
    )
    .await;
    let addr = server.local_addr();
    let resp = send(addr, &request("GET", "/api/volumes/configs", addr, &[])).await;
    assert_eq!(status_of(&resp), 404, "ERR maps to 404: {resp}");
    server.shutdown().await;

    // No seam: the family's 503.
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();
    let resp = send(addr, &request("GET", "/api/volumes/configs", addr, &[])).await;
    assert_eq!(status_of(&resp), 503, "no seam: {resp}");
}

// ------------------- the P6 rebuild route + the sparse config markers (§3-P6) ---

/// `POST /api/volumes/{name}/rebuild` (P6): the Refresh button's
/// endpoint — one `REBUILD <name>` through the seam (the same
/// serialized surface the control channel runs; the acceptance comes
/// back immediately, the walk is the instance's background task), the
/// pinned write-family success shape, and the seam's `ERR:` (an
/// already-running rebuild, the drained-queue refusal, the K11/telegram
/// gates) as a 409 with its actionable text verbatim.
#[tokio::test]
async fn rebuild_route_forwards_the_command_and_maps_replies() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, seen) = recording_seam(
        "OK: rebuild of `a` started in background — `LIST` shows progress; the result \
         logs when done\n"
            .to_string(),
    );
    let server = multi_server_with_commands(vec![entry], seam).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("POST", "/api/volumes/a/rebuild", addr, &[])).await;
    assert_eq!(status_of(&resp), 200, "the rebuild route answers: {resp}");
    assert!(resp.contains("application/json"), "json: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse body");
    assert_eq!(
        body,
        serde_json::json!({
            "ok": true,
            "reply": "rebuild of `a` started in background — `LIST` shows progress; the \
                      result logs when done"
        }),
        "the pinned write-family success shape: {body}"
    );
    assert_eq!(
        *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec!["REBUILD a".to_string()],
        "the route sends exactly the REBUILD command"
    );
    server.shutdown().await;

    // An ERR reply (the gates' refusals) maps to 409 with the text
    // verbatim — the dashboard toasts the actionable wording as-is.
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server_with_commands(
        vec![entry],
        canned_seam(
            "ERR: rebuild already running on `a` (started 14:23:07) — `LIST` shows its \
             progress\n"
                .to_string(),
        ),
    )
    .await;
    let addr = server.local_addr();
    let resp = send(addr, &request("POST", "/api/volumes/a/rebuild", addr, &[])).await;
    assert_eq!(status_of(&resp), 409, "ERR maps to 409: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|msg| msg.contains("already running")),
        "the refusal text rides the error verbatim: {body}"
    );
    server.shutdown().await;
}

/// The rebuild route joins the write family's gates wholesale: a
/// foreign Origin 403s BEFORE the seam, a non-loopback binding without
/// `allow_remote_admin` 403s naming the key, and a seam-less dashboard
/// 503s — the P1 middleware/budget family inherited untouched.
#[tokio::test]
async fn rebuild_route_inherits_origin_remote_and_seam_gates() {
    // Origin: refused before the seam is reached.
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, seen) = recording_seam("OK: rebuild of `a` started in background\n".to_string());
    let server = multi_server_with_commands(vec![entry], seam).await;
    let addr = server.local_addr();
    let resp = send(
        addr,
        &request(
            "POST",
            "/api/volumes/a/rebuild",
            addr,
            &[("Origin", "http://evil.example")],
        ),
    )
    .await;
    assert_eq!(
        status_of(&resp),
        403,
        "cross-origin rebuild refused: {resp}"
    );
    assert!(
        seen.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty(),
        "the guard refuses before the seam is reached"
    );
    server.shutdown().await;

    // Remote degrade: a non-loopback binding withholds the write.
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, _seen) = recording_seam("OK: rebuild of `a` started in background\n".to_string());
    let server = WebUiServer::serve_multi_with_remote_admin(
        RegistryHandle::new(vec![entry]),
        "0.0.0.0:0".parse().expect("parse the bind address"),
        Some(seam),
        false,
    )
    .await
    .expect("serve on a non-loopback binding");
    let addr = SocketAddr::from(([127, 0, 0, 1], server.local_addr().port()));
    let resp = send(addr, &request("POST", "/api/volumes/a/rebuild", addr, &[])).await;
    assert_eq!(status_of(&resp), 403, "remote rebuild withheld: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|msg| msg.contains("allow_remote_admin")),
        "the refusal names the opt-in key: {body}"
    );
    server.shutdown().await;

    // No seam: the family's 503.
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();
    let resp = send(addr, &request("POST", "/api/volumes/a/rebuild", addr, &[])).await;
    assert_eq!(status_of(&resp), 503, "no seam: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"].as_str().is_some_and(|msg| !msg.is_empty()),
        "actionable error: {body}"
    );
}

/// The configs rows' P2 sparse markers parse into the JSON the Refresh
/// button gates on: `rebuilding` and `encrypted` ride as `true`, and a
/// quiet row carries NEITHER key (the P1 row shape stays
/// byte-for-byte — the frontend's `=== true` reads absence as false).
#[tokio::test]
async fn configs_endpoint_parses_the_sparse_rebuild_markers() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, _seen) = recording_seam(
        "OK: 3 volume file(s)\n\
         a local enabled=true running rebuilding\n\
         enc local enabled=true running encrypted\n\
         busy baidu enabled=true running encrypted rebuilding\n"
            .to_string(),
    );
    let server = multi_server_with_commands(vec![entry], seam).await;
    let addr = server.local_addr();

    let resp = send(addr, &request("GET", "/api/volumes/configs", addr, &[])).await;
    assert_eq!(status_of(&resp), 200, "ok: {resp}");
    let rows: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse rows");
    assert_eq!(
        rows,
        serde_json::json!([
            { "name": "a", "backend": "local", "enabled": true, "running": true,
              "rebuilding": true },
            { "name": "enc", "backend": "local", "enabled": true, "running": true,
              "encrypted": true },
            { "name": "busy", "backend": "baidu", "enabled": true, "running": true,
              "encrypted": true, "rebuilding": true },
        ]),
        "the sparse markers ride their rows: {rows}"
    );
    server.shutdown().await;
}

// --------------------------- the P3 create route (web volume §1.2/§1.5) ---

/// Builds an HTTP/1.1 request carrying a JSON body (the create/update
/// routes' transport).
fn request_with_body(
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
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str(&format!(
        "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ));
    req
}

/// `POST /api/volumes` (P3, the Add Volume form's transport): the body's
/// `name` keys the command line and the REST of the object rides as the
/// compact single-line JSON payload — `CREATE <name> <json>` through the
/// seam — with the family's write budget, the pinned success shape and
/// the ERR→409 mapping.
#[tokio::test]
async fn create_route_forwards_the_payload_through_the_seam() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, seen) =
        recording_seam("OK: created volume `newvol` (file at volumes/newvol.toml)\n".to_string());
    let server = multi_server_with_commands(vec![entry], seam).await;
    let addr = server.local_addr();

    let resp = send(
        addr,
        &request_with_body(
            "POST",
            "/api/volumes",
            addr,
            &[],
            r#"{"name": "newvol", "backend": "local", "local_root": "C:/data"}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "create ok: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse body");
    assert_eq!(
        body,
        serde_json::json!({
            "ok": true,
            "reply": "created volume `newvol` (file at volumes/newvol.toml)"
        }),
        "the pinned success shape: {body}"
    );
    assert_eq!(
        *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec!["CREATE newvol {\"backend\":\"local\",\"local_root\":\"C:/data\"}".to_string()],
        "the name keys the command; the remaining object rides as one compact JSON payload"
    );
    server.shutdown().await;
}

/// The create route maps the seam's `ERR:` replies to the family's 409
/// with the actionable text verbatim (a refused create — an existing
/// file, a validation refusal — is a conflict the form shows inline).
#[tokio::test]
async fn create_route_maps_err_replies_to_409() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server_with_commands(
        vec![entry],
        canned_seam(
            "ERR: a volume file for `a` already exists at volumes/a.toml — edit it instead\n"
                .to_string(),
        ),
    )
    .await;
    let addr = server.local_addr();

    let resp = send(
        addr,
        &request_with_body(
            "POST",
            "/api/volumes",
            addr,
            &[],
            r#"{"name": "a", "backend": "local", "local_root": "C:/data"}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 409, "ERR maps to 409: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|msg| msg.contains("already exists")),
        "the ERR text rides verbatim: {body}"
    );
    server.shutdown().await;
}

/// The create route's own shape checks (the seam is the authority on the
/// PAYLOAD's semantics; the route is the authority on the HTTP body): a
/// non-JSON body, a JSON non-object and a missing/non-string `name` each
/// answer an actionable 400 without reaching the seam.
#[tokio::test]
async fn create_route_rejects_broken_bodies_before_the_seam() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, seen) = recording_seam("OK: unreachable\n".to_string());
    let server = multi_server_with_commands(vec![entry], seam).await;
    let addr = server.local_addr();

    for (label, body) in [
        ("not JSON", "create me a volume"),
        ("a bare JSON array", "[1, 2]"),
        ("no name", r#"{"backend": "local"}"#),
        ("a non-string name", r#"{"name": 42}""#),
    ] {
        let resp = send(
            addr,
            &request_with_body("POST", "/api/volumes", addr, &[], body),
        )
        .await;
        assert_eq!(status_of(&resp), 400, "{label} refused: {resp}");
        let parsed: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("json");
        assert!(
            parsed["error"].as_str().is_some_and(|msg| !msg.is_empty()),
            "{label} refusal is actionable: {parsed}"
        );
    }
    assert!(
        seen.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty(),
        "no broken body reaches the seam"
    );
    server.shutdown().await;
}

/// The create route joins the write family's gates wholesale (P1's
/// middleware family): a foreign Origin 403s BEFORE the seam, a
/// non-loopback binding without `allow_remote_admin` 403s naming the
/// key, and a seam-less dashboard 503s.
#[tokio::test]
async fn create_route_inherits_origin_remote_and_seam_gates() {
    // Origin: refused before the seam is reached.
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, seen) = recording_seam("OK: created\n".to_string());
    let server = multi_server_with_commands(vec![entry], seam).await;
    let addr = server.local_addr();
    let resp = send(
        addr,
        &request_with_body(
            "POST",
            "/api/volumes",
            addr,
            &[("Origin", "http://evil.example")],
            r#"{"name": "x", "backend": "local", "local_root": "C:/d"}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 403, "cross-origin create refused: {resp}");
    assert!(
        seen.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty(),
        "the guard refuses before the seam is reached"
    );
    server.shutdown().await;

    // Remote degrade: a non-loopback binding withholds the write.
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, _seen) = recording_seam("OK: created\n".to_string());
    let server = WebUiServer::serve_multi_with_remote_admin(
        RegistryHandle::new(vec![entry]),
        "0.0.0.0:0".parse().expect("parse the bind address"),
        Some(seam),
        false,
    )
    .await
    .expect("serve on a non-loopback binding");
    let addr = SocketAddr::from(([127, 0, 0, 1], server.local_addr().port()));
    let resp = send(
        addr,
        &request_with_body(
            "POST",
            "/api/volumes",
            addr,
            &[],
            r#"{"name": "x", "backend": "local", "local_root": "C:/d"}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 403, "remote create withheld: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|msg| msg.contains("allow_remote_admin")),
        "the refusal names the opt-in key: {body}"
    );
    server.shutdown().await;

    // No seam: the family's 503.
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();
    let resp = send(
        addr,
        &request_with_body(
            "POST",
            "/api/volumes",
            addr,
            &[],
            r#"{"name": "x", "backend": "local", "local_root": "C:/d"}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 503, "no seam: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"].as_str().is_some_and(|msg| !msg.is_empty()),
        "actionable error: {body}"
    );
}

// --------------------------- the P4 update route (web volume §1.2/§1.5) ---

/// `POST /api/volumes/{name}` (P4, the edit form's transport): the body
/// is the fields object verbatim — the name rides the PATH — and the
/// route forwards one compact `UPDATE <name> <json>` through the seam
/// with the 180s assembly budget (REMOVE's drain + the ADD leg); the
/// reply mapping is the write family's (OK pinned shape, ERR 409 with
/// the actionable text — the mixed-state refusals included).
#[tokio::test]
async fn update_route_forwards_the_fields_through_the_seam() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, seen) = recording_seam(
        "OK: updated volume `a` (re-assembled: running; no drive letter claimed); note: the \
         volume file was rewritten — hand-written comments are lost\n"
            .to_string(),
    );
    let server = multi_server_with_commands(vec![entry], seam).await;
    let addr = server.local_addr();

    let resp = send(
        addr,
        &request_with_body(
            "POST",
            "/api/volumes/a",
            addr,
            &[],
            r#"{"chat_id": 999999, "bot_token": "", "drive_letter": "Q"}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "update ok: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse body");
    assert_eq!(body["ok"], serde_json::json!(true), "the ok flag: {body}");
    assert!(
        body["reply"]
            .as_str()
            .is_some_and(|r| r.contains("updated volume `a`")),
        "the reply rides verbatim (comments note included): {body}"
    );
    assert_eq!(
        *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec!["UPDATE a {\"bot_token\":\"\",\"chat_id\":999999,\"drive_letter\":\"Q\"}".to_string()],
        "the path's name keys the command; the body rides as one compact JSON payload \
         (serde_json's map order — alphabetical — is the deterministic wire form)"
    );
    server.shutdown().await;
}

/// The update route maps the seam's mixed-state `ERR:` replies to the
/// family's 409 verbatim — the edit form renders the drain guidance
/// inline (the file IS updated; the runtime is stale).
#[tokio::test]
async fn update_route_maps_mixed_state_err_to_409() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server_with_commands(
        vec![entry],
        canned_seam(
            "ERR: the volume file was updated (volumes/a.toml) but the volume still runs its \
             OLD configuration — drain the pending uploads and retry\n"
                .to_string(),
        ),
    )
    .await;
    let addr = server.local_addr();

    let resp = send(
        addr,
        &request_with_body("POST", "/api/volumes/a", addr, &[], r#"{"chat_id": 1}"#),
    )
    .await;
    assert_eq!(status_of(&resp), 409, "mixed-state ERR maps to 409: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|msg| msg.contains("OLD configuration")),
        "the mixed-state text rides verbatim: {body}"
    );
    server.shutdown().await;
}

/// The update route's body gate: a non-JSON body and a non-object body
/// answer actionable 400s without reaching the seam; the family's gates
/// (Origin, remote degrade, seam) answer before the command.
#[tokio::test]
async fn update_route_body_and_family_gates() {
    // The body gate: nothing broken reaches the seam.
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, seen) = recording_seam("OK: unreachable\n".to_string());
    let server = multi_server_with_commands(vec![entry], seam).await;
    let addr = server.local_addr();
    for (label, body) in [("not JSON", "just words"), ("a bare array", "[1]")] {
        let resp = send(
            addr,
            &request_with_body("POST", "/api/volumes/a", addr, &[], body),
        )
        .await;
        assert_eq!(status_of(&resp), 400, "{label} refused: {resp}");
    }
    // Origin: refused before the seam.
    let resp = send(
        addr,
        &request_with_body(
            "POST",
            "/api/volumes/a",
            addr,
            &[("Origin", "http://evil.example")],
            r#"{"chat_id": 1}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 403, "cross-origin update refused: {resp}");
    assert!(
        seen.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty(),
        "no broken or foreign request reaches the seam"
    );
    server.shutdown().await;

    // Remote degrade + the seam-less 503.
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, _seen) = recording_seam("OK: updated\n".to_string());
    let server = WebUiServer::serve_multi_with_remote_admin(
        RegistryHandle::new(vec![entry]),
        "0.0.0.0:0".parse().expect("parse the bind address"),
        Some(seam),
        false,
    )
    .await
    .expect("serve on a non-loopback binding");
    let addr = SocketAddr::from(([127, 0, 0, 1], server.local_addr().port()));
    let resp = send(
        addr,
        &request_with_body("POST", "/api/volumes/a", addr, &[], r#"{"chat_id": 1}"#),
    )
    .await;
    assert_eq!(status_of(&resp), 403, "remote update withheld: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|msg| msg.contains("allow_remote_admin")),
        "the refusal names the opt-in key: {body}"
    );
    server.shutdown().await;

    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();
    let resp = send(
        addr,
        &request_with_body("POST", "/api/volumes/a", addr, &[], r#"{"chat_id": 1}"#),
    )
    .await;
    assert_eq!(status_of(&resp), 503, "no seam: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"].as_str().is_some_and(|msg| !msg.is_empty()),
        "actionable error: {body}"
    );
}

// ------------------------- the P5 destroy route (§1.2 two-leg, plan §3) ---

/// The destroy route's FIRST leg: a body without `confirm: true`
/// forwards the bare `DESTROY <name>` (the preview — the command
/// executes nothing) and answers the pinned preview shape
/// `{"ok": true, "reply": ..., "confirm_required": true}` so the
/// frontend can render the preview panel without inventing its own
/// state machine.
#[tokio::test]
async fn destroy_route_preview_leg_forwards_and_flags_confirm_required() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, seen) = recording_seam(
        "OK: DESTROY preview for `a` — nothing was executed:\n- kept: the local data \
         directory stays\n- never touched: remote data\nconfirm with: `DESTROY a confirm`\n"
            .to_string(),
    );
    let server = multi_server_with_commands(vec![entry], seam).await;
    let addr = server.local_addr();

    // The explicit preview body, the empty-object body and the EMPTY
    // body all mean "not confirmed" (the safe default).
    for (label, body) in [
        ("explicit", r#"{"confirm": false}"#),
        ("empty object", "{}"),
        ("no body", ""),
    ] {
        let resp = send(
            addr,
            &request_with_body("POST", "/api/volumes/a/destroy", addr, &[], body),
        )
        .await;
        assert_eq!(status_of(&resp), 200, "preview ok ({label}): {resp}");
        assert!(resp.contains("application/json"), "json: {resp}");
        let value: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse body");
        assert_eq!(
            value,
            serde_json::json!({
                "ok": true,
                "reply": "DESTROY preview for `a` — nothing was executed:\n- kept: the local data directory stays\n- never touched: remote data\nconfirm with: `DESTROY a confirm`",
                "confirm_required": true,
            }),
            "the pinned preview shape ({label}): {value}"
        );
    }
    // Clone the commands out so the guard dies on its statement (the
    // clippy await-holding-lock rule does not credit the later drop).
    let commands = seen
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(
        commands.len(),
        3,
        "each preview leg sent the preview command"
    );
    assert!(
        commands.iter().all(|line| line == "DESTROY a"),
        "the preview command carries no confirm word: {commands:?}"
    );
    server.shutdown().await;
}

/// The destroy route's SECOND leg: `confirm: true` forwards
/// `DESTROY <name> confirm` — plus the literal `purge_local` third
/// word when the body asks for it — and answers the family's plain
/// success shape (no confirm_required flag: the destruction ran). The
/// seam's ERR: (a drain refusal) rides the family's 409 verbatim.
#[tokio::test]
async fn destroy_route_confirm_leg_forwards_purge_local_and_maps_the_reply() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, seen) = recording_seam(
        "OK: destroyed volume `a` (the volume was not running; volume file deleted)\n".to_string(),
    );
    let server = multi_server_with_commands(vec![entry], seam).await;
    let addr = server.local_addr();

    let resp = send(
        addr,
        &request_with_body(
            "POST",
            "/api/volumes/a/destroy",
            addr,
            &[],
            r#"{"confirm": true}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "confirm ok: {resp}");
    let value: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("parse body");
    assert_eq!(
        value,
        serde_json::json!({ "ok": true, "reply": "destroyed volume `a` (the volume was not running; volume file deleted)" }),
        "the family's plain success shape (no confirm_required): {value}"
    );

    let resp = send(
        addr,
        &request_with_body(
            "POST",
            "/api/volumes/a/destroy",
            addr,
            &[],
            r#"{"confirm": true, "purge_local": true}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "purge ok: {resp}");
    assert_eq!(
        *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec!["DESTROY a confirm", "DESTROY a confirm purge_local"],
        "the purge flag rides the command line as the literal third word"
    );
    server.shutdown().await;

    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server_with_commands(
        vec![entry],
        canned_seam(
            "ERR: destroying `a` aborted during the unmount: uploads are still arriving — \
             the volume file was NOT deleted\n"
                .to_string(),
        ),
    )
    .await;
    let addr = server.local_addr();
    let resp = send(
        addr,
        &request_with_body(
            "POST",
            "/api/volumes/a/destroy",
            addr,
            &[],
            r#"{"confirm": true}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 409, "ERR maps to 409: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|msg| msg.contains("was NOT deleted")),
        "the refusal text rides verbatim: {body}"
    );
    server.shutdown().await;
}

/// The destroy route's body gate and family gates: a non-JSON body, a
/// non-object body and wrong-typed flags answer actionable 400s without
/// reaching the seam; the Origin guard, the remote degrade and the
/// missing seam answer before the command is built.
#[tokio::test]
async fn destroy_route_body_and_family_gates() {
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, seen) = recording_seam("OK: unreachable\n".to_string());
    let server = multi_server_with_commands(vec![entry], seam).await;
    let addr = server.local_addr();
    for (label, body) in [
        ("not JSON", "just words"),
        ("a bare array", "[1]"),
        ("a string confirm", r#"{"confirm": "yes"}"#),
        ("a numeric purge", r#"{"confirm": true, "purge_local": 1}"#),
    ] {
        let resp = send(
            addr,
            &request_with_body("POST", "/api/volumes/a/destroy", addr, &[], body),
        )
        .await;
        assert_eq!(status_of(&resp), 400, "{label} refused: {resp}");
    }
    let resp = send(
        addr,
        &request_with_body(
            "POST",
            "/api/volumes/a/destroy",
            addr,
            &[("Origin", "http://evil.example")],
            r#"{"confirm": true}"#,
        ),
    )
    .await;
    assert_eq!(
        status_of(&resp),
        403,
        "cross-origin destroy refused: {resp}"
    );
    assert!(
        seen.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty(),
        "no broken or foreign request reaches the seam"
    );
    server.shutdown().await;

    // Remote degrade: the write family's 403 naming the opt-in key.
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let (seam, _seen) = recording_seam("OK: destroyed\n".to_string());
    let server = WebUiServer::serve_multi_with_remote_admin(
        RegistryHandle::new(vec![entry]),
        "0.0.0.0:0".parse().expect("parse the bind address"),
        Some(seam),
        false,
    )
    .await
    .expect("serve on a non-loopback binding");
    let addr = SocketAddr::from(([127, 0, 0, 1], server.local_addr().port()));
    let resp = send(
        addr,
        &request_with_body(
            "POST",
            "/api/volumes/a/destroy",
            addr,
            &[],
            r#"{"confirm": true}"#,
        ),
    )
    .await;
    assert_eq!(status_of(&resp), 403, "remote destroy withheld: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|msg| msg.contains("allow_remote_admin")),
        "the refusal names the opt-in key: {body}"
    );
    server.shutdown().await;

    // No seam: the family's actionable 503.
    let dir = tempfile::tempdir().expect("temp dir");
    let entry = volume_env(dir.path(), "a", "local", idle_mock().await).await;
    let server = multi_server(vec![entry]).await;
    let addr = server.local_addr();
    let resp = send(
        addr,
        &request_with_body("POST", "/api/volumes/a/destroy", addr, &[], "{}"),
    )
    .await;
    assert_eq!(status_of(&resp), 503, "no seam: {resp}");
    let body: serde_json::Value = serde_json::from_str(&body_of(&resp)).expect("error json");
    assert!(
        body["error"].as_str().is_some_and(|msg| !msg.is_empty()),
        "actionable error: {body}"
    );
}

/// 负责人实测反馈（2026-09-24）：建卷表单的加密方案**默认 aead_v2**——
/// 旧代码把下拉预选成 gcm（后端缺省其实早已是 aead_v2，前端预选把每个
/// 「不选择」的新卷都钉成了 gcm）；编辑预填只把**显式存储的 gcm** 显示为
/// gcm，缺省/空值一律显示 aead_v2。源码 pin（无 JS harness 的既有手法）。
#[tokio::test]
async fn volume_form_defaults_the_encryption_scheme_to_aead_v2() {
    let server = multi_server(vec![]).await;
    let addr = server.local_addr();
    let resp = send(addr, &request("GET", "/static/js/volumes.js", addr, &[])).await;
    assert_eq!(status_of(&resp), 200);
    let js = body_of(&resp);

    assert!(
        js.contains("scheme.value = 'aead_v2';"),
        "the create/edit form must default the scheme dropdown to aead_v2"
    );
    assert!(
        !js.contains("scheme.value = 'gcm'"),
        "the old gcm pre-selection must be gone"
    );
    assert!(
        js.contains("=== 'gcm' ? 'gcm' : 'aead_v2'"),
        "edit prefill maps only an EXPLICITLY stored gcm to gcm (absent/empty = default)"
    );
    server.shutdown().await;
}

/// Web 首启引导（FR2）：零卷空态文案是**引导语**——首启用户（无配置
/// bootstrap 进 init 模式，注册表为空）看到的不再是纯陈述，而是指向
/// 「＋ 添加卷」动作的指路文案。双语键对称；旧纯陈述文案删除。
#[tokio::test]
async fn empty_volume_state_copy_points_at_add_volume() {
    let server = multi_server(vec![]).await;
    let addr = server.local_addr();
    let resp = send(addr, &request("GET", "/static/js/i18n.js", addr, &[])).await;
    assert_eq!(status_of(&resp), 200);
    let i18n = body_of(&resp);

    assert!(
        i18n.contains("No volumes yet — use ＋ Add Volume above to create your first one."),
        "EN empty state must point at the Add Volume action"
    );
    assert!(
        i18n.contains("尚无存储卷——点击上方「＋ 添加卷」创建第一个卷。"),
        "ZH empty state must point at the Add Volume action"
    );
    assert!(
        !i18n.contains("No volume files are configured on this instance.")
            && !i18n.contains("本实例未配置任何卷文件。"),
        "the old statement-only copy is gone"
    );
    server.shutdown().await;
}
