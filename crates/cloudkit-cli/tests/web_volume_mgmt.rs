//! RED-phase tests for the web volume-management P0 batch (plan
//! `docs/plans/2026-09-13-web-volume-management.md` §1.1/§1.2/§3-P0):
//! the control channel's `SHOW <name>` command (single-line masked
//! JSON — credential values never leave the backend) and the cli
//! assembly's injection of the SAME volume-command handler into the
//! dashboard (the in-process callback seam: web-sent commands and
//! control-channel commands serialize behind one queue, and the web
//! config endpoint follows the dynamic registry).

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cloudkit_cli::control::read_control_addr;
use cloudkit_cli::{
    run_multi_with_transports, MultiVolumeHandle, RemoveTuning, RunOptions, RuntimeVolumeCommands,
    VolumeTransportDispatch,
};
use cloudkit_core::config::{CyDriveConfig, VolumeConfig};
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::CloudTransport;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::timeout;

// ------------------------------------------------------------- helpers ---

/// Serialises every test that changes the process-wide working directory
/// (the `runtime_volumes.rs` convention).
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

/// Writes `text` to `path`, creating missing parent directories.
fn write_file(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(path, text).expect("write file");
}

/// The process-level config for multi-volume mode with the dashboard
/// ON (an ephemeral web port): the seam's e2e boot.
fn web_process_config() -> CyDriveConfig {
    CyDriveConfig {
        volumes_dir: Some("volumes".to_string()),
        webdav_port: 0,
        enable_web_ui: true,
        web_ui_port: 0,
        auto_mount_drive: false,
        ..CyDriveConfig::default()
    }
}

/// A telegram-flavoured volume file with an obviously FAKE credential —
/// the marker the masking assertions grep for.
fn volume_toml(token: &str, chat_id: i64) -> String {
    format!("backend = \"telegram\"\nbot_token = \"{token}\"\nchat_id = {chat_id}\n")
}

/// A pre-connected mock transport.
async fn mock_transport() -> Arc<MockTransport> {
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect mock transport");
    mock
}

/// The plain multi-volume boot over the volumes on disk (the runtime
/// command surface installs with the control channel; SHOW needs no
/// dispatch).
async fn boot(cfg: CyDriveConfig) -> MultiVolumeHandle {
    let specs =
        cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
    let mut mocks = Vec::new();
    for _ in &specs {
        mocks.push(mock_transport().await);
    }
    let injections = specs
        .into_iter()
        .zip(mocks)
        .map(|(spec, mock)| (spec, RunOptions::default(), mock as Arc<dyn CloudTransport>))
        .collect();
    run_multi_with_transports(&cfg, injections)
        .await
        .expect("multi-volume boot")
}

/// Sends one control-channel line and returns the reply (read to EOF).
async fn send_cmd(addr: SocketAddr, line: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect to control");
    stream
        .write_all(format!("{line}\n").as_bytes())
        .await
        .expect("send control line");
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .expect("read control reply");
    String::from_utf8_lossy(&raw).into_owned()
}

/// The running instance's control address (the port file in the cwd).
fn control_addr() -> SocketAddr {
    read_control_addr(&web_process_config()).expect("the control file parses into an address")
}

/// One raw HTTP/1.1 request (`Connection: close`), response read to EOF.
async fn send_http(addr: SocketAddr, target: &str) -> String {
    send_http_method("GET", addr, target).await
}

/// [`send_http`] with the method injectable (the P1 write routes POST).
async fn send_http_method(method: &str, addr: SocketAddr, target: &str) -> String {
    send_http_with_body(method, addr, target, "").await
}

/// [`send_http`] with a request body (the P3 create route's JSON).
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
    stream.read_to_end(&mut raw).await.expect("read to EOF");
    String::from_utf8_lossy(&raw).into_owned()
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

/// The mock dispatch for the runtime ADD path (the seam e2e re-ADDs a
/// removed volume through the same surface the control channel uses).
fn mock_dispatch() -> VolumeTransportDispatch {
    Arc::new(move |_spec: &VolumeConfig| {
        Box::pin(async {
            let mock = Arc::new(MockTransport::new());
            mock.connect().await.expect("connect mock");
            Ok(Some((
                RunOptions::default(),
                mock as Arc<dyn CloudTransport>,
            )))
        })
    })
}

// ----------------------------------------------------------- SHOW (§1.2) ---

/// `SHOW <name>` answers one `OK: <single-line JSON>` line: the volume's
/// EXPLICIT keys verbatim, credential keys collapsed to `{"set": bool}`
/// markers — the fake token's VALUE must not ride the reply in any form.
#[tokio::test]
async fn show_reports_masked_volume_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("media.toml"),
        &volume_toml("111:FAKE-TOKEN-DO-NOT-LEAK", 4242),
    );
    let _guard = chdir(dir.path());

    let handle = boot(web_process_config()).await;
    let reply = send_cmd(control_addr(), "SHOW media").await;

    assert!(reply.starts_with("OK: "), "ok reply: {reply}");
    let payload = reply.strip_prefix("OK: ").expect("the OK prefix");
    let payload = payload.trim_end();
    assert!(!payload.contains('\n'), "single-line JSON: {reply}");
    let value: serde_json::Value = serde_json::from_str(payload).expect("valid json: {reply}");
    assert_eq!(value["name"], "media");
    assert_eq!(value["backend"], "telegram");
    assert_eq!(value["chat_id"], 4242);
    assert_eq!(
        value["bot_token"],
        serde_json::json!({"set": true}),
        "the credential collapses to its set marker: {reply}"
    );
    assert_eq!(
        value["encryption_password"],
        serde_json::json!({"set": false}),
        "an absent credential reports unset: {reply}"
    );
    assert!(
        !reply.contains("FAKE-TOKEN-DO-NOT-LEAK"),
        "the credential VALUE must never leave the backend: {reply}"
    );
    assert!(
        value.get("drive_letter").is_none(),
        "no defaulted keys — the reply shows the file: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// SHOW's refusal states are actionable: a volume with no file names
/// the volumes_dir, a path-unsafe name is refused before it becomes a
/// file name, and the malformed shapes get the usage line.
#[tokio::test]
async fn show_refusals_are_actionable() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("media.toml"),
        &volume_toml("111:AAA", 111111),
    );
    let _guard = chdir(dir.path());

    let handle = boot(web_process_config()).await;
    let addr = control_addr();

    // A valid name with no file under the volumes_dir.
    let reply = send_cmd(addr, "SHOW ghost").await;
    assert!(reply.starts_with("ERR:"), "missing file refuses: {reply}");
    assert!(
        reply.contains("ghost") && reply.contains("volumes"),
        "the refusal names the volume and the directory: {reply}"
    );

    // A path-unsafe name never becomes a file name ("a b" is the
    // usage arm's two-argument case, further below).
    for line in ["SHOW ../config", "SHOW ..\\config", "SHOW media.toml"] {
        let reply = send_cmd(addr, line).await;
        assert!(reply.starts_with("ERR:"), "{line} refuses: {reply}");
        assert!(
            reply.contains("not a volume name"),
            "{line} names the name rules: {reply}"
        );
    }

    // Malformed shapes get the usage line (SHOW listed) — "a b" is two
    // arguments, exactly like ADD's extra-argument case.
    for line in ["SHOW", "SHOW a b", "SHOW a b c"] {
        let reply = send_cmd(addr, line).await;
        assert!(
            reply.starts_with("ERR:") && reply.contains("usage"),
            "{line} gets the usage line: {reply}"
        );
        assert!(
            reply.contains("SHOW <name>"),
            "the usage line teaches SHOW: {reply}"
        );
    }

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// --------------------------------------------- the web seam e2e (§1.1) ---

/// The cli assembly injects the SAME handler the control channel runs
/// into the dashboard: the web config endpoint answers the masked SHOW
/// JSON, follows the dynamic registry (a REMOVEd volume 404s — the
/// file stays, the registry governs), and the `/volumes` page serves
/// its skeleton on the dashboard port.
#[tokio::test]
async fn web_seam_serves_config_and_follows_the_registry() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:FAKE-TOKEN-WEB-SEAM", 111111),
    );
    write_file(
        &dir.path().join("volumes").join("b.toml"),
        &volume_toml("222:BBB", 222222),
    );
    let _guard = chdir(dir.path());

    let cfg = web_process_config();
    let specs =
        cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
    let injections = vec![
        (
            specs[0].clone(),
            RunOptions::default(),
            mock_transport().await as Arc<dyn CloudTransport>,
        ),
        (
            specs[1].clone(),
            RunOptions::default(),
            mock_transport().await as Arc<dyn CloudTransport>,
        ),
    ];
    let handle = cloudkit_cli::run_multi_with_transports_and_commands(
        &cfg,
        injections,
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            remove_tuning: RemoveTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await
    .expect("boot with the command surface");
    let web = handle
        .web_ui_addr()
        .expect("the dashboard bound (enable_web_ui = true)");
    let addr = control_addr();

    // The listing carries the pending column (P0's /api/volumes row).
    let resp = send_http(web, "/api/volumes").await;
    assert_eq!(status_of(&resp), 200, "listing ok: {resp}");
    let rows: serde_json::Value = serde_json::from_str(body_of(&resp)).expect("parse rows");
    assert_eq!(rows.as_array().expect("array").len(), 2, "both volumes");
    for row in rows.as_array().expect("array") {
        assert!(
            row.get("pending").is_some_and(|pending| pending.is_u64()),
            "every running row carries a numeric pending: {row}"
        );
    }

    // The config endpoint answers the masked SHOW JSON over the seam.
    let resp = send_http(web, "/api/volumes/a/config").await;
    assert_eq!(status_of(&resp), 200, "config ok: {resp}");
    assert!(
        !resp.contains("FAKE-TOKEN-WEB-SEAM"),
        "the credential value never rides the HTTP response: {resp}"
    );
    let value: serde_json::Value = serde_json::from_str(body_of(&resp)).expect("parse config");
    assert_eq!(value["name"], "a");
    assert_eq!(value["bot_token"], serde_json::json!({"set": true}));

    // The /volumes page skeleton is served on the dashboard port.
    let resp = send_http(web, "/volumes").await;
    assert_eq!(status_of(&resp), 200, "page ok: {resp}");
    assert!(
        resp.contains("id=\"volumes-tbody\""),
        "the management skeleton: {resp}"
    );

    // A volume outside the registry 404s even with a file on disk.
    let resp = send_http(web, "/api/volumes/ghost/config").await;
    assert_eq!(status_of(&resp), 404, "unknown volume: {resp}");

    // REMOVE a over the control channel: the seam follows the dynamic
    // registry — the endpoint 404s (the toml stays, the registry governs)
    // and the listing shrinks.
    let reply = send_cmd(addr, "REMOVE a").await;
    assert!(reply.starts_with("OK:"), "remove ok: {reply}");
    let resp = send_http(web, "/api/volumes/a/config").await;
    assert_eq!(status_of(&resp), 404, "removed volume 404s: {resp}");
    let resp = send_http(web, "/api/volumes").await;
    let rows: serde_json::Value = serde_json::from_str(body_of(&resp)).expect("parse rows");
    assert_eq!(rows.as_array().expect("array").len(), 1, "one row left");

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// ------------------------------------------- the P1 write family e2e ---

/// The P1 write routes drive the REAL command surface end to end (the
/// seam's other side is the cli composition root's own handler): an
/// unmount through the web endpoint changes the configs listing's
/// runtime column, a disable writes the volume file and takes the
/// volume down, an enable brings it back — the file, the registry and
/// the configs row all agreeing at every step. Green-since-birth pin:
/// the components were each red→green in their own suites; this test
/// pins their composition.
#[tokio::test]
async fn web_write_routes_drive_the_real_command_surface() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:FAKE-TOKEN-WEB-WRITE", 111111),
    );
    write_file(
        &dir.path().join("volumes").join("b.toml"),
        &volume_toml("222:BBB", 222222),
    );
    let _guard = chdir(dir.path());

    let cfg = web_process_config();
    let specs =
        cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
    let injections = vec![
        (
            specs[0].clone(),
            RunOptions::default(),
            mock_transport().await as Arc<dyn CloudTransport>,
        ),
        (
            specs[1].clone(),
            RunOptions::default(),
            mock_transport().await as Arc<dyn CloudTransport>,
        ),
    ];
    let handle = cloudkit_cli::run_multi_with_transports_and_commands(
        &cfg,
        injections,
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            remove_tuning: RemoveTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await
    .expect("boot with the command surface");
    let web = handle
        .web_ui_addr()
        .expect("the dashboard bound (enable_web_ui = true)");
    let addr = control_addr();

    // The configs row lookup helper over the endpoint's JSON.
    async fn configs_row(web: SocketAddr, name: &str) -> serde_json::Value {
        let resp = send_http(web, "/api/volumes/configs").await;
        assert_eq!(status_of(&resp), 200, "configs ok: {resp}");
        let rows: serde_json::Value =
            serde_json::from_str(body_of(&resp)).expect("parse the configs rows");
        rows.as_array()
            .expect("array body")
            .iter()
            .find(|row| row["name"] == name)
            .unwrap_or_else(|| panic!("row {name}: {rows}"))
            .clone()
    }

    // Boot state: both volumes running.
    let row = configs_row(web, "a").await;
    assert_eq!(
        row,
        serde_json::json!({ "name": "a", "backend": "telegram", "enabled": true, "running": true }),
        "the boot row: {row}"
    );

    // Unmount `a` through the web endpoint: the reply rides the pinned
    // shape and the configs row flips to absent (the FILE stays — K49).
    let resp = send_http_method("POST", web, "/api/volumes/a/remove").await;
    assert_eq!(status_of(&resp), 200, "remove ok: {resp}");
    let body: serde_json::Value = serde_json::from_str(body_of(&resp)).expect("parse body");
    assert_eq!(body["ok"], serde_json::json!(true), "the ok flag: {body}");
    assert!(
        body["reply"]
            .as_str()
            .is_some_and(|r| r.contains("removed")),
        "the reply text rides: {body}"
    );
    let row = configs_row(web, "a").await;
    assert_eq!(
        row,
        serde_json::json!({ "name": "a", "backend": "telegram", "enabled": true, "running": false }),
        "the unmounted volume reads absent with its file intact: {row}"
    );

    // Disable `b`: the file flips, the volume comes down.
    let resp = send_http_method("POST", web, "/api/volumes/b/disable").await;
    assert_eq!(status_of(&resp), 200, "disable ok: {resp}");
    let file = fs::read_to_string(dir.path().join("volumes").join("b.toml"))
        .expect("read the volume file");
    assert!(
        file.contains("enabled = false"),
        "the web disable wrote the file: {file}"
    );
    assert!(
        !file.contains("FAKE-TOKEN-WEB-WRITE"),
        "the other volume's credential never rides sibling files: {file}"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        reply.starts_with("OK: 0 volume(s)"),
        "both volumes down (a unmounted, b disabled): {reply}"
    );
    let row = configs_row(web, "b").await;
    assert_eq!(
        row,
        serde_json::json!({ "name": "b", "backend": "telegram", "enabled": false, "running": false }),
        "the disabled row: {row}"
    );

    // Enable `b`: the file flips back and the volume re-assembles.
    let resp = send_http_method("POST", web, "/api/volumes/b/enable").await;
    assert_eq!(status_of(&resp), 200, "enable ok: {resp}");
    let file = fs::read_to_string(dir.path().join("volumes").join("b.toml"))
        .expect("read the volume file");
    assert!(
        file.contains("enabled = true"),
        "the web enable wrote the file: {file}"
    );
    let row = configs_row(web, "b").await;
    assert_eq!(
        row["running"],
        serde_json::json!(true),
        "the volume runs again: {row}"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        reply.starts_with("OK: 1 volume(s)") && reply.contains("b running"),
        "LIST agrees with the configs row: {reply}"
    );

    // An ERR reply keeps its actionable text on the 409 (remove a
    // volume that is not registered).
    let resp = send_http_method("POST", web, "/api/volumes/ghost/remove").await;
    assert_eq!(status_of(&resp), 409, "ERR maps to 409: {resp}");
    let body: serde_json::Value = serde_json::from_str(body_of(&resp)).expect("error json");
    assert!(
        body["error"].as_str().is_some_and(|m| m.contains("ghost")),
        "the refusal names the volume: {body}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// ------------------------------------------- the P3 create route e2e ---

/// The P3 create route drives the REAL command surface end to end (the
/// seam's other side is the cli composition root's own handler): the
/// form's JSON creates the volume file under volumes_dir, assembles the
/// volume through the runtime ADD path, and the reply carries the mount
/// state — with a credential VALUE never riding the HTTP response.
/// Green-since-birth pin: the components were each red→green in their
/// own suites; this test pins their composition.
#[tokio::test]
async fn web_create_route_drives_the_real_command_surface() {
    let dir = tempfile::tempdir().expect("temp dir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:AAA", 111111),
    );
    let _guard = chdir(dir.path());

    let cfg = web_process_config();
    let specs =
        cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
    let injections = vec![(
        specs[0].clone(),
        RunOptions::default(),
        mock_transport().await as Arc<dyn CloudTransport>,
    )];
    let handle = cloudkit_cli::run_multi_with_transports_and_commands(
        &cfg,
        injections,
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            remove_tuning: RemoveTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await
    .expect("boot with the command surface");
    let web = handle
        .web_ui_addr()
        .expect("the dashboard bound (enable_web_ui = true)");
    let addr = control_addr();

    // Create `made` through the web route: file + runtime + reply all
    // agree.
    let resp = send_http_with_body(
        "POST",
        web,
        "/api/volumes",
        "{\"name\":\"made\",\"backend\":\"telegram\",\"bot_token\":\"222:FAKE-CREATED-VIA-WEB\",\"chat_id\":222222}",
    )
    .await;
    assert_eq!(status_of(&resp), 200, "create ok: {resp}");
    assert!(
        !resp.contains("FAKE-CREATED-VIA-WEB"),
        "the credential value never rides the HTTP response: {resp}"
    );
    let body: serde_json::Value = serde_json::from_str(body_of(&resp)).expect("parse body");
    assert_eq!(body["ok"], serde_json::json!(true), "the ok flag: {body}");
    assert!(
        body["reply"]
            .as_str()
            .is_some_and(|r| r.contains("created volume `made`")),
        "the reply names the created volume: {body}"
    );

    let file = fs::read_to_string(dir.path().join("volumes").join("made.toml"))
        .expect("the created volume file");
    assert!(
        file.contains("bot_token = \"222:FAKE-CREATED-VIA-WEB\"")
            && file.contains("chat_id = 222222"),
        "the file carries the payload's keys: {file}"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        reply.contains("made running"),
        "the created volume is registered: {reply}"
    );

    // A duplicate create answers the 409 with the actionable refusal.
    let resp = send_http_with_body(
        "POST",
        web,
        "/api/volumes",
        "{\"name\":\"made\",\"backend\":\"telegram\",\"bot_token\":\"9:9\",\"chat_id\":9}",
    )
    .await;
    assert_eq!(status_of(&resp), 409, "duplicate create conflicts: {resp}");
    let body: serde_json::Value = serde_json::from_str(body_of(&resp)).expect("error json");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|m| m.contains("already exists")),
        "the refusal is the command's own text: {body}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}
