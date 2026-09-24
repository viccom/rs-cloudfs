//! End-to-end offline tests for the first-run bootstrap boot chain (web
//! first-run plan FR1): an empty directory bootstraps into init mode —
//! the generated config loads as `Multi` with an EMPTY volume manifest,
//! the boot passes gate B (`first_run = true`), the dashboard binds the
//! empty registry and the first volume is created through the existing
//! web `POST /api/volumes` route (the real local-backend dispatch).
//!
//! The `run` orchestration itself (bootstrap probe ahead of discovery)
//! lives in main.rs — these tests drive the lib seams it composes.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::Context as _;
use cloudkit_cli::{
    bootstrap_first_run_cwd, discover_first_run_config, run_multi_with_transports_and_commands,
    DiscoveredConfig, RunOptions, RuntimeVolumeCommands, VolumeTransportDispatch,
};
use cloudkit_core::config::{CyDriveConfig, VolumeConfig};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

// ------------------------------------------------------------- helpers ---

/// Serialises every test that changes the process-wide working directory
/// (the `web_volume_mgmt.rs` convention).
static CWD_MUTEX: Mutex<()> = Mutex::new(());

/// Holds [`CWD_MUTEX`] and restores the previous working directory on
/// drop — including on panic.
struct CwdGuard {
    _lock: MutexGuard<'static, ()>,
    prev: PathBuf,
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.prev).expect("restore previous cwd");
    }
}

/// Locks [`CWD_MUTEX`] and moves the process cwd into `dir` for the
/// duration of the guard.
fn chdir(dir: &Path) -> CwdGuard {
    let lock = CWD_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let prev = std::env::current_dir().expect("current dir");
    std::env::set_current_dir(dir).expect("chdir into temp dir");
    CwdGuard { _lock: lock, prev }
}

/// Suppresses the post-bind browser spawn for one test (the same
/// headless kill-switch a server deployment would set; the spawn itself
/// is deliberately NOT asserted anywhere — see `open_browser`).
fn suppress_browser() {
    std::env::set_var("CYDRIVE_NO_OPEN_BROWSER", "1");
}

/// The real dispatch a first-run boot carries (main.rs's
/// `dispatch_runtime_volume`, non-telegram arm): a web-created volume
/// assembles through the SAME unified-backend seam the runtime ADD
/// uses — here the offline local backend, so the e2e is real end to end.
fn local_dispatch() -> VolumeTransportDispatch {
    Arc::new(|spec: &VolumeConfig| {
        Box::pin(async move {
            let home = cloudkit_cli::volume_home(spec)?;
            let settings = cloudkit_cli::resolve_volume_settings(spec)?;
            settings
                .validate()
                .with_context(|| format!("invalid configuration for volume `{}`", spec.name))?;
            let mut run_options = RunOptions::default();
            let transport = cloudkit_cli::dispatch_unified_backend_volume(
                spec,
                &settings,
                &home,
                &mut run_options,
            )
            .await?;
            Ok(Some((run_options, transport)))
        })
    })
}

/// RED 3/4 boot: bootstrap → first-run discovery → the init boot with an
/// empty injection set. The dashboard moves to an ephemeral port so
/// parallel test binaries never contend on 8486 (the WebDAV port never
/// binds: an empty face set is a structural `None`).
async fn boot_first_run() -> (cloudkit_cli::MultiVolumeHandle, SocketAddr) {
    assert!(bootstrap_first_run_cwd().expect("bootstrap"), "empty cwd");
    let DiscoveredConfig::Multi {
        mut process,
        volumes,
    } = discover_first_run_config().expect("first-run discovery")
    else {
        panic!("the first-run discovery returns Multi");
    };
    assert!(volumes.is_empty(), "no volumes right after the bootstrap");
    process.web_ui_port = 0;
    // The WebDAV listener moves to an ephemeral port too (nothing dials
    // it in these tests) — its PRESENCE is the assertion (FR1 fix: the
    // first-run boot must bind it even with zero volumes, because the
    // runtime ADD of a drive-letter volume mounts through it).
    process.webdav_port = 0;

    let handle = run_multi_with_transports_and_commands(
        &process,
        Vec::new(),
        RuntimeVolumeCommands {
            dispatch: Some(local_dispatch()),
            ..RuntimeVolumeCommands::default()
        },
        true,
    )
    .await
    .expect("the first-run boot must succeed with zero volumes");
    let web = handle.web_ui_addr().expect("the dashboard binds");
    (handle, web)
}

/// One raw HTTP/1.1 request (`Connection: close`), response read to EOF.
async fn send_http_with_body(method: &str, addr: SocketAddr, target: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect to server");
    let request = format!(
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        addr.port(),
        body.len()
    );
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

/// [`send_http_with_body`] without a body (the GET shapes).
async fn send_http(addr: SocketAddr, target: &str) -> String {
    send_http_with_body("GET", addr, target, "").await
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

/// The response body (everything after the blank line).
fn body_of(resp: &str) -> &str {
    resp.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}

// --------------------------------------------------------------- tests ---

/// RED 3 (true arm): `first_run = true` with an empty injection set
/// boots — the dashboard binds the empty registry and serves the
/// /volumes management page (the FR2 empty-state guidance lives there).
#[tokio::test]
async fn first_run_gate_boots_empty_and_serves_the_volumes_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = chdir(dir.path());
    suppress_browser();

    let (handle, web) = boot_first_run().await;
    assert_ne!(web.port(), 0, ":0 resolves to the real bound port");
    assert!(
        handle.webdav_addr().is_some(),
        "first-run binds the WebDAV listener over the EMPTY registry — the mount          endpoint runtime-added drive-letter volumes go through"
    );

    let resp = send_http(web, "/volumes").await;
    assert_eq!(status_of(&resp), 200, "the empty management page: {resp}");
    assert!(
        resp.contains("id=\"volumes-tbody\""),
        "the management skeleton is served: {resp}"
    );

    handle
        .shutdown()
        .await
        .expect("the first-run instance shuts down cleanly");
}

/// RED 3 (false arm): the very same empty assembly with
/// `first_run = false` keeps the existing gate-B refusal — the wording
/// of the pre-first-run error is pinned here so the new parameter can
/// never silently change the configured boot path.
#[tokio::test]
async fn non_first_run_empty_assembly_keeps_the_existing_bail() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = chdir(dir.path());
    // The same shape the bootstrap produces: volumes_dir set, volumes/
    // empty — but the boot is NOT a first run.
    std::fs::create_dir_all("volumes").expect("empty volumes dir");
    let cfg = CyDriveConfig {
        volumes_dir: Some("volumes".to_string()),
        webdav_port: 0,
        enable_web_ui: true,
        web_ui_port: 0,
        auto_mount_drive: false,
        ..CyDriveConfig::default()
    };

    let err = run_multi_with_transports_and_commands(
        &cfg,
        Vec::new(),
        RuntimeVolumeCommands::default(),
        false,
    )
    .await
    .expect_err("without first_run the empty assembly still refuses");
    let message = format!("{err:#}");
    assert!(
        message.contains("no enabled volumes to assemble"),
        "the existing gate-B text is unchanged: {message}"
    );
}

/// RED 4: the full first-run chain — bootstrap → init boot →
/// `POST /api/volumes` creates a LOCAL volume through the real dispatch
/// → the listing carries it and the volume file landed under `volumes/`.
#[tokio::test]
async fn first_run_e2e_creates_the_first_volume_through_the_web() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = chdir(dir.path());
    suppress_browser();

    let (handle, web) = boot_first_run().await;

    // The Add-Volume form's route: the body's `name` keys the command,
    // every other member rides as the CREATE payload (local backend —
    // the loopback write gate passes, no Origin header = non-browser).
    // local_root must be ABSOLUTE (the volume-settings contract); the
    // directory itself is created by the backend on first use.
    let root = dir.path().join("media-data");
    let body = serde_json::json!({
        "name": "media",
        "backend": "local",
        "local_root": root.to_string_lossy()
    })
    .to_string();
    let resp = send_http_with_body("POST", web, "/api/volumes", &body).await;
    assert_eq!(status_of(&resp), 200, "create ok: {resp}");

    // The aggregate listing carries the new volume.
    let resp = send_http(web, "/api/volumes").await;
    assert_eq!(status_of(&resp), 200, "listing ok: {resp}");
    let rows: serde_json::Value = serde_json::from_str(body_of(&resp)).expect("the listing parses");
    let names: Vec<&str> = rows
        .as_array()
        .expect("the listing is an array")
        .iter()
        .filter_map(|row| row.get("name").and_then(|name| name.as_str()))
        .collect();
    assert!(names.contains(&"media"), "the new volume is listed: {rows}");

    // The volume file landed under the volumes directory (the next
    // start re-discovers it through the ordinary path).
    assert!(
        Path::new("volumes").join("media.toml").is_file(),
        "the volume file is written under volumes/"
    );

    handle
        .shutdown()
        .await
        .expect("shutdown joins cleanly after the create");
}
