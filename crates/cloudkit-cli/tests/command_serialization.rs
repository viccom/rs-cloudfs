//! RED-phase tests for the K58 review-fix batch FB: every volume-command
//! entrypoint serializes again (H2 — the web seam used to run its
//! commands on the axum handler task, OUTSIDE the control channel's
//! serialized loop), and a command's execution no longer dies with the
//! caller that requested it (H1 — the dashboard's route budget used to
//! DROP the handler future mid-K50, wedging a volume whose live entry
//! was already taken out of the table).
//!
//! The repro shape is the production one: a real boot (dashboard on,
//! control channel on), a REMOVE parked in its drain window by a
//! RateLimited upload (the same hold technique the M5 characterization
//! test uses), and the web route as the misbehaving caller — abandoned
//! (H1) or duplicated concurrently (H2). The panic-isolation leg pins
//! the moved protection layer: a panicking execution answers the
//! actionable internal-error ERR on the WEB seam too (previously only
//! the control channel's catch_unwind contained it).

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cloudkit_cli::control::read_control_addr;
use cloudkit_cli::{
    run_multi_with_transports_and_commands, MultiVolumeHandle, RemoveTuning, RunOptions,
    RuntimeVolumeCommands, VolumeTransportDispatch,
};
use cloudkit_core::config::{CyDriveConfig, VolumeConfig};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::{MockTransport, UploadAction};
use cloudkit_core::transport::CloudTransport;
use cloudkit_storage::StorageError;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::timeout;

// ------------------------------------------------------------- helpers ---

/// Serialises every test that changes the process-wide working directory
/// (the `runtime_volumes.rs` convention — tests in one binary share one
/// process; `set_current_dir` races).
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
/// duration of the guard. Declare the guard *after* the owning `TempDir`
/// so the cwd is restored before the directory is deleted.
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

/// The process-level config for multi-volume mode with the dashboard ON
/// (an ephemeral web port): both command faces (control channel + web
/// seam) live in the same instance.
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

/// A minimal legal telegram-flavoured volume file body (the injected
/// mock replaces the real connect; the keys keep the file a legal
/// volume shape).
fn volume_toml(token: &str, chat_id: i64) -> String {
    format!("backend = \"telegram\"\nbot_token = \"{token}\"\nchat_id = {chat_id}\n")
}

/// A pre-connected mock transport.
async fn mock_transport() -> Arc<MockTransport> {
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect mock transport");
    mock
}

/// The drain-hold dispatch (the M5 characterization test's technique):
/// the ONE upload of the volume added through this dispatch fails with
/// an authoritative RateLimited{5s} — the worker honors the wait exactly
/// (the job stays in flight for five seconds), then the exhausted script
/// serves the retry as Ok. A REMOVE therefore parks in its drain loop
/// for a bounded, observable window and then COMPLETES.
fn hold_dispatch() -> VolumeTransportDispatch {
    Arc::new(move |_spec: &VolumeConfig| {
        Box::pin(async {
            let mock = Arc::new(
                MockTransport::builder()
                    .upload_action(UploadAction::Fail {
                        error: StorageError::RateLimited {
                            retry_after: Some(Duration::from_secs(5)),
                        },
                    })
                    .build(),
            );
            mock.connect().await.expect("connect hold mock");
            Ok(Some((
                RunOptions::default(),
                mock as Arc<dyn CloudTransport>,
            )))
        })
    })
}

/// A dispatch whose connect leg panics — the panic-isolation leg's
/// injection point (the deliberate test panic).
fn panicking_dispatch() -> VolumeTransportDispatch {
    Arc::new(move |_spec: &VolumeConfig| {
        Box::pin(async {
            panic!("the deliberate dispatch panic (K58-FB test)");
        })
    })
}

/// The drain windows wide enough to cover the held upload's 5s retry
/// (the `a_slow_remove_holds_the_control_channel` tuning).
fn hold_tuning() -> RemoveTuning {
    RemoveTuning {
        drain_timeout: Duration::from_secs(15),
        poll_interval: Duration::from_millis(50),
    }
}

/// Boots the instance with one running volume (`a`, a plain mock) and
/// the given runtime dispatch behind it.
async fn boot(cfg: CyDriveConfig, dispatch: VolumeTransportDispatch) -> MultiVolumeHandle {
    let specs = {
        let mut v =
            cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
        v.retain(|spec| spec.name == "a");
        v
    };
    let injections = vec![(
        specs.into_iter().next().expect("volume a on disk"),
        RunOptions::default(),
        mock_transport().await as Arc<dyn CloudTransport>,
    )];
    run_multi_with_transports_and_commands(
        &cfg,
        injections,
        RuntimeVolumeCommands {
            dispatch: Some(dispatch),
            remove_tuning: hold_tuning(),
            ..RuntimeVolumeCommands::default()
        },
        false,
    )
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
async fn send_http_method(method: &str, addr: SocketAddr, target: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect to server");
    let request = format!(
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\
         Content-Length: 0\r\n\r\n",
        addr.port()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("send request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read to EOF");
    String::from_utf8_lossy(&raw).into_owned()
}

/// Parks one held upload on the volume (the drain window's payload) and
/// lets the worker reach its RateLimited wait.
async fn park_held_upload(handle: &MultiVolumeHandle, dir: &Path) {
    let source = dir.join("hold.txt");
    fs::write(&source, b"hold me five seconds").expect("write source");
    let rel = RelPath::new("/hold.txt").expect("valid rel path");
    handle
        .volume("stuck")
        .expect("volume stuck")
        .vfs()
        .expect("vfs stuck")
        .ingest_file(&rel, &source, 1.0)
        .await
        .expect("ingest into stuck");
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Polls LIST until `name` disappears from the registry (bounded by
/// `budget`); `false` when the volume is still registered at the end.
async fn wait_until_unregistered(addr: SocketAddr, name: &str, budget: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let reply = send_cmd(addr, "LIST").await;
        if !reply.contains(name) {
            return Some(reply);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ------------------------------------------------ 1. H1: cancellation ---

/// K58-H1's main proof: a web caller that ABANDONS its request mid-drain
/// must not kill the command. The dashboard's route wraps the command
/// future in a budget (`tokio::time::timeout`), and the web client can
/// equally drop the connection — either way the route's future goes
/// away. Pre-fix, that future WAS the command: dropping it mid-K50 took
/// the volume's live entry with it and left the volume registered
/// forever (wedged: `REMOVE` would never complete, yet `LIST` keeps
/// listing it). Post-fix the execution runs on its own task and runs to
/// its natural end — the abandoned caller simply never hears the reply.
///
/// Test form (honest note): the route's 120s budget is a constant, so
/// the abandonment is produced by aborting the spawned HTTP client task
/// — the client's TCP connection closes and hyper drops the in-flight
/// handler task, the same future-drop the route budget produces. The
/// command is parked in its drain window when the abandonment lands.
#[tokio::test]
async fn an_abandoned_web_remove_runs_to_its_natural_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:AAA", 111111),
    );
    write_file(
        &dir.path().join("volumes").join("stuck.toml"),
        &volume_toml("222:BBB", 222222),
    );
    let _guard = chdir(dir.path());

    let handle = boot(web_process_config(), hold_dispatch()).await;
    let web = handle
        .web_ui_addr()
        .expect("the dashboard bound (enable_web_ui = true)");
    let addr = control_addr();

    let reply = send_cmd(addr, "ADD stuck").await;
    assert!(reply.starts_with("OK:"), "stuck adds cleanly: {reply}");
    park_held_upload(&handle, dir.path()).await;

    // The web client abandons the REMOVE while its drain is parked.
    let client =
        tokio::spawn(
            async move { send_http_method("POST", web, "/api/volumes/stuck/remove").await },
        );
    tokio::time::sleep(Duration::from_millis(300)).await;
    client.abort();

    // The command must still run to completion: the held upload retries
    // to Ok inside the 15s drain budget, the K50 sequence commits, and
    // LIST stops listing the volume. Pre-fix the removal dies with the
    // dropped future and the volume stays registered forever.
    let final_list = wait_until_unregistered(addr, "stuck", Duration::from_secs(20)).await;
    assert!(
        final_list.is_some(),
        "the abandoned web REMOVE must run to its natural end — the volume is still registered \
         (the drop of the web future took the live entry mid-K50: the command is dead, the \
         registry entry is not)"
    );
    assert!(
        dir.path().join("volumes").join("stuck.toml").is_file(),
        "REMOVE is runtime-only (K49) — the volume file stays either way"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// --------------------------------------------- 2. H2: serialization ---

/// K58-H2's main proof: two concurrent web REMOVEs of the same volume
/// must serialize behind the one command execution — the first completes
/// the K50 sequence, the second then finds the volume already gone and
/// answers the ordinary `no volume registered` refusal. Pre-fix both
/// handlers ran concurrently on their own axum tasks: the second raced
/// INTO the first's mid-K50 window and hit the structural
/// `no live runtime state` arm (an invariant-violation text no client
/// should ever see), or worse, interleaved with it.
#[tokio::test]
async fn concurrent_web_removes_serialize_one_completes_one_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:AAA", 111111),
    );
    write_file(
        &dir.path().join("volumes").join("stuck.toml"),
        &volume_toml("222:BBB", 222222),
    );
    let _guard = chdir(dir.path());

    let handle = boot(web_process_config(), hold_dispatch()).await;
    let web = handle
        .web_ui_addr()
        .expect("the dashboard bound (enable_web_ui = true)");
    let addr = control_addr();

    let reply = send_cmd(addr, "ADD stuck").await;
    assert!(reply.starts_with("OK:"), "stuck adds cleanly: {reply}");
    park_held_upload(&handle, dir.path()).await;

    // Two concurrent web REMOVEs: one K50 completion, one ordinary
    // refusal — never a race into the mid-K50 window.
    let (first, second) = tokio::join!(
        send_http_method("POST", web, "/api/volumes/stuck/remove"),
        send_http_method("POST", web, "/api/volumes/stuck/remove"),
    );
    let mut completions = 0;
    let mut refusals = 0;
    for (label, resp) in [("first", &first), ("second", &second)] {
        assert!(
            !resp.contains("no live runtime state"),
            "the {label} reply must never be the structural mid-K50 invariant text (the \
             concurrent REMOVE raced past the serialization): {resp}"
        );
        if resp.contains("removed volume `stuck`") {
            completions += 1;
        }
        if resp.contains("no volume registered under `stuck`") {
            refusals += 1;
        }
    }
    assert_eq!(
        completions, 1,
        "exactly one REMOVE completes the K50 sequence: first={first} second={second}"
    );
    assert_eq!(
        refusals, 1,
        "exactly one REMOVE meets the ordinary unknown-volume refusal: first={first} \
         second={second}"
    );

    let reply = send_cmd(addr, "LIST").await;
    assert!(
        !reply.contains("stuck"),
        "the registry agrees the volume is gone: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// --------------------------------------- 3. panic isolation on the web ---

/// The panic protection moved to where the execution actually runs (the
/// detached task): a panicking command answers the actionable
/// internal-error ERR on the WEB seam too — pre-fix only the control
/// channel's catch_unwind contained it, and a panicking web handler
/// killed the axum connection task (the browser saw a bare reset). Both
/// faces keep serving after the panic.
#[tokio::test]
async fn a_panicking_execution_answers_the_err_on_the_web_seam_and_both_faces_survive() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:AAA", 111111),
    );
    let _guard = chdir(dir.path());

    let handle = boot(web_process_config(), panicking_dispatch()).await;
    let web = handle
        .web_ui_addr()
        .expect("the dashboard bound (enable_web_ui = true)");
    let addr = control_addr();

    // Take `a` down first so the ENABLE leg re-assembles it (through
    // the panicking dispatch).
    let reply = send_cmd(addr, "REMOVE a").await;
    assert!(reply.starts_with("OK:"), "remove ok: {reply}");

    // The web ENABLE hits the panicking dispatch: the route answers the
    // shared internal-error ERR (not a connection reset), with its 409.
    let resp = send_http_method("POST", web, "/api/volumes/a/enable").await;
    assert!(
        resp.contains("ERR") || resp.contains("HTTP/1.1 409"),
        "the web route answered (not a bare connection reset): {resp}"
    );
    assert!(
        resp.contains("internal error while executing the command"),
        "the panicking execution's actionable text rides the web reply: {resp}"
    );

    // Both faces keep serving: the control channel answers commands, a
    // control-side ADD through the same panicking dispatch gets the
    // contained ERR reply, and the web seam still executes commands.
    let reply = send_cmd(addr, "PING").await;
    assert!(
        reply.starts_with("OK: cydrive"),
        "the control channel answers PING after the panic: {reply}"
    );
    let reply = send_cmd(addr, "ADD a").await;
    assert!(
        reply.contains("internal error while executing the command"),
        "the control-side ADD through the panicking dispatch answers the contained ERR: {reply}"
    );
    let resp = send_http_method("POST", web, "/api/volumes/ghost/remove").await;
    assert!(
        resp.contains("HTTP/1.1 409"),
        "the web seam still executes commands after the panic: {resp}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}
