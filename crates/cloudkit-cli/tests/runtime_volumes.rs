//! RED-phase tests for Phase 3.6 / RV2: runtime volume ADD/REMOVE/LIST
//! over the control channel (plan `docs/plans/2026-09-10-runtime-volumes.md`
//! §3-RV2; rulings K48/K49/K50).
//!
//! Contract under test: a running multi-volume instance answers
//! `ADD <name>` / `REMOVE <name>` / `LIST` on the loopback control
//! channel; ADD assembles one volume at runtime through the SAME
//! per-volume assembly the boot loop uses and registers it into all
//! three faces (master registry, WebDAV dispatch, dashboard); REMOVE
//! runs the K50 safe sequence (drain → unmount → unregister) and any
//! failed step aborts the removal with the volume fully intact; LIST
//! reports name × status × letter × backend × pending per volume.

use std::fs;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cloudkit_cli::control::read_control_addr;
use cloudkit_cli::{
    run_multi_with_transports, run_multi_with_transports_and_commands, DriveRelease,
    MountedBackend, MountedVolume, MultiVolumeHandle, RemoveTuning, RunOptions, RuntimeMount,
    RuntimeVolumeCommands, VolumeMounts, VolumeStatus, VolumeTransportDispatch,
};
use cloudkit_core::config::{CyDriveConfig, VolumeConfig};
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::{MockTransport, UploadAction};
use cloudkit_core::transport::CloudTransport;
use cloudkit_storage::StorageError;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::timeout;

// ------------------------------------------------------------- helpers ---

/// Serialises every test that changes the process-wide working directory
/// (the `multivolume_e2e.rs` convention — tests in one binary share one
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

/// The process-level config for multi-volume mode (the `multivolume_e2e`
/// shape): `volumes_dir` set, ephemeral ports, no auto-mount (the offline
/// gate never maps a real network drive), dashboard off.
fn process_config() -> CyDriveConfig {
    CyDriveConfig {
        volumes_dir: Some("volumes".to_string()),
        webdav_port: 0,
        enable_web_ui: false,
        web_ui_port: 0,
        auto_mount_drive: false,
        ..CyDriveConfig::default()
    }
}

/// [`process_config`] with the auto-mount switch ON — the ADD mount arm's
/// precondition (the H3 tests; the mount itself is stubbed, so no real
/// drive is ever touched).
fn mount_process_config() -> CyDriveConfig {
    CyDriveConfig {
        auto_mount_drive: true,
        ..process_config()
    }
}

/// A minimal legal telegram-flavoured volume file body (the injected
/// mock replaces the real connect; the keys keep the file a legal
/// volume shape).
fn volume_toml(token: &str, chat_id: i64) -> String {
    format!("backend = \"telegram\"\nbot_token = \"{token}\"\nchat_id = {chat_id}\n")
}

/// A pre-connected mock transport (the seam the production dispatch
/// substitutes real transports for).
async fn mock_transport() -> Arc<MockTransport> {
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect mock transport");
    mock
}

/// The RV2 dispatch seam under test: the runtime ADD reads the volume
/// file, then builds the transport through this closure (the production
/// `run` passes its real dispatch; tests inject mocks).
fn mock_dispatch() -> VolumeTransportDispatch {
    Arc::new(move |_spec: &VolumeConfig| Box::pin(async { mock_dispatch_result().await }))
}

/// One dispatched mock (the closure body, spelled out so failure-closing
/// dispatches can reuse the shape).
async fn mock_dispatch_result() -> anyhow::Result<Option<(RunOptions, Arc<dyn CloudTransport>)>> {
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect mock transport");
    Ok(Some((
        RunOptions::default(),
        mock as Arc<dyn CloudTransport>,
    )))
}

/// The full-cycle boot: the RV2-extended entry with the mock dispatch.
async fn boot_with_commands(
    cfg: CyDriveConfig,
    specs: Vec<VolumeConfig>,
    mocks: Vec<Arc<MockTransport>>,
    commands: RuntimeVolumeCommands,
) -> MultiVolumeHandle {
    assert_eq!(specs.len(), mocks.len(), "one transport per volume");
    let injections = specs
        .into_iter()
        .zip(mocks)
        .map(|(spec, mock)| (spec, RunOptions::default(), mock as Arc<dyn CloudTransport>))
        .collect();
    run_multi_with_transports_and_commands(&cfg, injections, commands)
        .await
        .expect("multi-volume boot with runtime commands")
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

/// The running instance's control address (the port file in the cwd —
/// the process config's default db_path anchors it there).
fn control_addr() -> SocketAddr {
    read_control_addr(&process_config()).expect("the control file parses into an address")
}

/// The LIST reply's data rows (everything after the `OK: N volume(s)`
/// header), one per volume.
fn list_rows(reply: &str) -> Vec<String> {
    reply
        .lines()
        .skip(1)
        .map(|line| line.trim().to_owned())
        .collect()
}

/// One raw HTTP/1.1 request (`Connection: close`), response read to EOF.
async fn send_http(addr: SocketAddr, request: &str) -> String {
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

/// Builds an HTTP/1.1 request with Host and `Connection: close`.
fn http_request(method: &str, target: &str, addr: SocketAddr) -> String {
    format!(
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\
         Content-Length: 0\r\n\r\n",
        addr.port()
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

// ------------------------------------------------- 1. ADD / REMOVE / LIST full cycle ---

/// The whole runtime cycle over the control channel (mock transports):
/// ADD assembles `b` through the runtime dispatch and registers it into
/// the master registry AND the WebDAV face (`/vol/b` answers) at once;
/// REMOVE unregisters it from both, its upload queue drains to the
/// terminal state, and re-ADD after a REMOVE works (clean teardown).
#[tokio::test]
async fn add_remove_list_full_cycle_over_the_control_channel() {
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
        &dir.path().join("volumes").join("b.toml"),
        &volume_toml("222:BBB", 222222),
    );
    let _guard = chdir(dir.path());

    let specs = {
        let mut v =
            cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
        // Boot volume `a` only; `b` arrives at runtime through ADD.
        v.retain(|spec| spec.name == "a");
        v
    };
    let handle = boot_with_commands(
        process_config(),
        specs,
        vec![mock_transport().await],
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();
    let webdav = handle.webdav_addr().expect("webdav bound");

    // LIST at boot: one volume, no letter, no mount backend — the
    // volume file's backend fills the column.
    let reply = send_cmd(addr, "LIST").await;
    assert!(reply.starts_with("OK: 1 volume(s)"), "LIST header: {reply}");
    assert_eq!(
        list_rows(&reply),
        vec!["a running - telegram pending=0".to_string()],
        "LIST row format: name status letter backend pending=N: {reply}"
    );

    // ADD b: assembled at runtime, registered everywhere at once.
    let reply = send_cmd(addr, "ADD b").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("added volume `b`"),
        "ADD reply: {reply}"
    );
    assert_eq!(
        handle.volume("b").expect("volume b registered").status(),
        VolumeStatus::Running,
        "the runtime-added volume is running"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        reply.starts_with("OK: 2 volume(s)"),
        "both volumes listed: {reply}"
    );
    assert!(
        list_rows(&reply)
            .iter()
            .any(|row| row.starts_with("b running ")),
        "volume b has its row: {reply}"
    );

    // The data plane follows: /vol/b routes on the SAME port, no
    // listener restart (K51 liveness through the RV1 dispatch).
    let resp = send_http(webdav, &http_request("PROPFIND", "/vol/b/", webdav)).await;
    assert_eq!(
        status_of(&resp),
        207,
        "runtime-added volume reachable: {resp}"
    );

    // REMOVE b: unregistered, unroutable, queue drained to the terminal
    // state before the reply comes back.
    let source = dir.path().join("b-source.txt");
    fs::write(&source, b"remove-drains-me").expect("write source");
    let rel = RelPath::new("/b.txt").expect("valid rel path");
    handle
        .volume("b")
        .expect("volume b")
        .vfs()
        .expect("vfs b")
        .ingest_file(&rel, &source, 1.0)
        .await
        .expect("ingest into volume b");

    let reply = send_cmd(addr, "REMOVE b").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("removed volume `b`"),
        "REMOVE reply: {reply}"
    );
    assert!(
        handle.volume("b").is_none(),
        "the removed volume is gone from the master registry"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert_eq!(
        list_rows(&reply),
        vec!["a running - telegram pending=0".to_string()],
        "only the surviving volume remains: {reply}"
    );
    let resp = send_http(webdav, &http_request("PROPFIND", "/vol/b/", webdav)).await;
    assert_eq!(status_of(&resp), 404, "removed volume unroutable: {resp}");

    // The queue drained: the row reached the uploaded terminal state.
    let db_b = MetaDatabase::open(&dir.path().join("volumes").join("b").join("cydrive_meta.db"))
        .expect("reopen volume b's db");
    let row = db_b
        .get_file("/b.txt")
        .expect("db read b")
        .expect("row exists");
    assert!(row.is_uploaded, "REMOVE drained volume b's queue");

    // Rejections: REMOVE of a missing volume, and re-ADD after a REMOVE
    // (the teardown left nothing behind that blocks a fresh cycle).
    let reply = send_cmd(addr, "REMOVE b").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("no volume registered"),
        "REMOVE of a missing volume is refused actionably: {reply}"
    );
    let reply = send_cmd(addr, "ADD b").await;
    assert!(
        reply.starts_with("OK:"),
        "re-ADD after REMOVE works (clean teardown): {reply}"
    );
    let reply = send_cmd(addr, "REMOVE b").await;
    assert!(reply.starts_with("OK:"), "cleanup REMOVE: {reply}");

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// Refusals that must not touch the running set: ADD of an already
/// registered name (pointed at REMOVE), ADD of a file that does not
/// exist, ADD of a disabled volume (K49: `enabled = false` is the
/// persistent-disable form), and REMOVE of a *failed* boot entry (the
/// runtime garbage a broken assembly left — removable, nothing to drain).
#[tokio::test]
async fn add_and_remove_rejections_answer_actionably() {
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
        &dir.path().join("volumes").join("bad.toml"),
        // The poisoned volume (the K22 shape from multivolume_e2e): its
        // db_path points at the volume toml itself, so the assembly fails.
        "backend = \"telegram\"\nbot_token = \"222:BBB\"\nchat_id = 222222\ndb_path = \"../bad.toml\"\n",
    );
    let _guard = chdir(dir.path());

    let specs = cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load specs");
    let mocks = vec![mock_transport().await, mock_transport().await];
    let handle = boot_with_commands(
        process_config(),
        specs,
        mocks,
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();

    // ADD of a registered name.
    let reply = send_cmd(addr, "ADD a").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("already registered"),
        "duplicate ADD refused: {reply}"
    );
    assert!(
        reply.contains("REMOVE"),
        "the refusal names the way out: {reply}"
    );

    // ADD of a file that does not exist.
    let reply = send_cmd(addr, "ADD nosuch").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("nosuch"),
        "missing-file ADD names the volume: {reply}"
    );

    // ADD of a disabled volume: the persistent-disable form (K49) must
    // not be overrideable at runtime.
    write_file(
        &dir.path().join("volumes").join("dis.toml"),
        &format!("{}enabled = false\n", volume_toml("333:CCC", 333333)),
    );
    let reply = send_cmd(addr, "ADD dis").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("enabled = false"),
        "disabled ADD refused with the key named: {reply}"
    );

    // None of the refusals changed the running set.
    let reply = send_cmd(addr, "LIST").await;
    let rows = list_rows(&reply);
    assert_eq!(rows.len(), 2, "a (running) + bad (failed) listed: {reply}");
    assert!(
        rows.iter().any(|row| row.starts_with("a running ")),
        "volume a unaffected: {reply}"
    );
    assert!(
        rows.iter().any(|row| row.starts_with("bad failed ")),
        "the failed boot entry is listed failed: {reply}"
    );

    // A failed entry is runtime garbage REMOVE can clear: no queue, no
    // mount — the registry (and the dashboard face) just drop it.
    let reply = send_cmd(addr, "REMOVE bad").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("removed volume `bad`"),
        "REMOVE of a failed entry succeeds: {reply}"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert_eq!(
        list_rows(&reply),
        vec!["a running - telegram pending=0".to_string()],
        "only a remains: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// ADD failure isolation (K22's runtime twin): a volume whose transport
/// dispatch fails is refused with the reason, and the sibling keeps
/// serving — nothing partially registered.
#[tokio::test]
async fn add_failure_leaves_siblings_untouched() {
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
        &dir.path().join("volumes").join("bad.toml"),
        &volume_toml("222:BBB", 222222),
    );
    let _guard = chdir(dir.path());

    let specs = {
        let mut v =
            cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
        v.retain(|spec| spec.name == "a");
        v
    };
    let failing_dispatch: VolumeTransportDispatch = Arc::new(move |spec: &VolumeConfig| {
        Box::pin(async move {
            if spec.name == "bad" {
                anyhow::bail!("credentials rejected by the backend");
            }
            mock_dispatch_result().await
        })
    });
    let handle = boot_with_commands(
        process_config(),
        specs,
        vec![mock_transport().await],
        RuntimeVolumeCommands {
            dispatch: Some(failing_dispatch),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();
    let webdav = handle.webdav_addr().expect("webdav bound");

    let reply = send_cmd(addr, "ADD bad").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("credentials rejected"),
        "the dispatch failure surfaces in the refusal: {reply}"
    );

    // The sibling is untouched and `bad` is not half-registered anywhere.
    assert_eq!(
        handle.volumes(),
        vec![("a".to_string(), VolumeStatus::Running)],
        "the failed ADD left the registry unchanged"
    );
    let resp = send_http(webdav, &http_request("PROPFIND", "/vol/a/", webdav)).await;
    assert_eq!(status_of(&resp), 207, "sibling still serving: {resp}");
    let reply = send_cmd(addr, "REMOVE bad").await;
    assert!(
        reply.starts_with("ERR:"),
        "nothing to remove after the failed ADD: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// --------------------------------------------------- 2. K50 safe removal ---

/// The K50 drain step: a volume with an upload held in flight (the mock
/// transport answers `RateLimited{3600s}`, which the queue honors exactly
/// and never degrades) refuses REMOVE with a pending count, and the
/// abort leaves the volume fully intact — registered, routable, nothing
/// torn down.
#[tokio::test]
async fn remove_with_undrained_queue_aborts_and_keeps_the_volume() {
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

    let specs = {
        let mut v =
            cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
        v.retain(|spec| spec.name != "stuck");
        v
    };
    // The ADD dispatch hands out a transport whose uploads fail with an
    // authoritative RateLimited{3600s}: the worker honors the wait
    // exactly (never clamped, never degraded), so the job stays
    // in-flight for the whole test.
    let hold_dispatch: VolumeTransportDispatch = Arc::new(move |_spec: &VolumeConfig| {
        Box::pin(async {
            let mock = Arc::new(
                MockTransport::builder()
                    .upload_action(UploadAction::Fail {
                        error: StorageError::RateLimited {
                            retry_after: Some(Duration::from_secs(3600)),
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
    });
    let handle = boot_with_commands(
        process_config(),
        specs,
        vec![mock_transport().await],
        RuntimeVolumeCommands {
            dispatch: Some(hold_dispatch),
            remove_tuning: RemoveTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();
    let webdav = handle.webdav_addr().expect("webdav bound");

    let reply = send_cmd(addr, "ADD stuck").await;
    assert!(reply.starts_with("OK:"), "stuck adds cleanly: {reply}");

    let source = dir.path().join("hold.txt");
    fs::write(&source, b"hold me in flight").expect("write source");
    let rel = RelPath::new("/hold.txt").expect("valid rel path");
    handle
        .volume("stuck")
        .expect("volume stuck")
        .vfs()
        .expect("vfs stuck")
        .ingest_file(&rel, &source, 1.0)
        .await
        .expect("ingest into stuck");

    // LIST shows the pending count (the §4 risk-table aid): the healthy
    // volume reads 0, the held one reads 1.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reply = send_cmd(addr, "LIST").await;
    let rows = list_rows(&reply);
    assert!(
        rows.iter()
            .any(|row| row.starts_with("a running ") && row.ends_with("pending=0")),
        "healthy volume shows pending=0: {reply}"
    );
    assert!(
        rows.iter()
            .any(|row| row.starts_with("stuck running ") && row.ends_with("pending=1")),
        "held upload shows pending=1: {reply}"
    );

    // REMOVE aborts on the drain timeout, actionably.
    let reply = send_cmd(addr, "REMOVE stuck").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("pending"),
        "the drain timeout names the pending count: {reply}"
    );
    assert!(
        handle.volume("stuck").expect("still registered").status() == VolumeStatus::Running,
        "the aborted removal left the volume registered"
    );
    let resp = send_http(webdav, &http_request("PROPFIND", "/vol/stuck/", webdav)).await;
    assert_eq!(
        status_of(&resp),
        207,
        "the aborted removal kept the data plane: {resp}"
    );

    // Deliberate leak: this handle is dropped WITHOUT shutdown — the
    // held worker sleeps 3600s (the RateLimited wait) and a drain-join
    // would hang the test. The runtime drop cancels the task.
    drop(handle);
}

/// Review M2: a drain that keeps being fed is not a drain. While REMOVE
/// parks in its drain window (one upload held by RateLimited{3600s}), a
/// SECOND upload arrives — the enqueued count grows between polls, so a
/// client is still writing the volume and the 60s budget can never be
/// enough (the queue is being re-fed). The drain must detect the growth
/// and abort EARLY with the writes-still-arriving wording (close the
/// programs using the volume), not wait the budget out and answer the
/// generic retry-once-drained advice that this scenario can never
/// satisfy. The volume stays registered either way (K50).
#[tokio::test]
async fn remove_drain_aborts_early_when_new_uploads_keep_arriving() {
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

    let specs = {
        let mut v =
            cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
        v.retain(|spec| spec.name != "stuck");
        v
    };
    // Same hold transport as the undrained-queue test, with the Fail
    // action scripted TWICE (upload scripts are consumed one entry per
    // call; an exhausted script serves Ok): both the first and the
    // second upload fail with an authoritative RateLimited{3600s} and
    // stay in flight for the whole test.
    let hold_dispatch: VolumeTransportDispatch = Arc::new(move |_spec: &VolumeConfig| {
        Box::pin(async {
            let mock = Arc::new(
                MockTransport::builder()
                    .upload_action(UploadAction::Fail {
                        error: StorageError::RateLimited {
                            retry_after: Some(Duration::from_secs(3600)),
                        },
                    })
                    .upload_action(UploadAction::Fail {
                        error: StorageError::RateLimited {
                            retry_after: Some(Duration::from_secs(3600)),
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
    });
    // A 5s drain budget: the red behaviour would wait it out fully
    // (proving the mis-diagnosis); the growth detection must answer
    // within a couple of poll ticks of the second upload.
    let handle = boot_with_commands(
        process_config(),
        specs,
        vec![mock_transport().await],
        RuntimeVolumeCommands {
            dispatch: Some(hold_dispatch),
            remove_tuning: RemoveTuning {
                drain_timeout: Duration::from_secs(5),
                poll_interval: Duration::from_millis(50),
            },
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();
    let webdav = handle.webdav_addr().expect("webdav bound");

    let reply = send_cmd(addr, "ADD stuck").await;
    assert!(reply.starts_with("OK:"), "stuck adds cleanly: {reply}");

    // The first upload parks the queue at outstanding=1 (the drain's
    // enqueued baseline).
    let source = dir.path().join("hold.txt");
    fs::write(&source, b"hold me in flight").expect("write source");
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

    // REMOVE parks in its drain loop; 300ms later a second upload
    // arrives — the enqueued count grows mid-drain (a client still
    // writes the volume).
    let started = std::time::Instant::now();
    let remove_task = tokio::spawn(async move { send_cmd(addr, "REMOVE stuck").await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let source2 = dir.path().join("hold2.txt");
    fs::write(&source2, b"another write while draining").expect("write source 2");
    let rel2 = RelPath::new("/hold2.txt").expect("valid rel path 2");
    handle
        .volume("stuck")
        .expect("volume stuck")
        .vfs()
        .expect("vfs stuck")
        .ingest_file(&rel2, &source2, 1.0)
        .await
        .expect("second ingest into stuck");

    let reply = timeout(Duration::from_secs(10), remove_task)
        .await
        .expect("REMOVE settles")
        .expect("the REMOVE task joins");
    let elapsed = started.elapsed();
    assert!(
        reply.starts_with("ERR:") && reply.contains("still arriving"),
        "the fed drain answers the writes-still-arriving abort: {reply}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the growth detection aborts well inside the 5s budget: {elapsed:?}"
    );
    assert!(
        handle.volume("stuck").expect("still registered").status() == VolumeStatus::Running,
        "the aborted removal left the volume registered"
    );
    let resp = send_http(webdav, &http_request("PROPFIND", "/vol/stuck/", webdav)).await;
    assert_eq!(
        status_of(&resp),
        207,
        "the aborted removal kept the data plane: {resp}"
    );

    // Deliberate leak (same reason as the undrained-queue test): the
    // held workers sleep 3600s; the runtime drop cancels the task.
    drop(handle);
}

/// The K50 unmount abort (the injected-probe path): a volume whose drive
/// release reports failure stops the removal — the volume stays
/// registered and its mount entry untouched; only a release that
/// succeeds lets the removal complete. The probe is injected through the
/// live-volume table seam, so this runs in the default (winfsp-off)
/// build against the same code the winfsp unmount plugs into.
#[tokio::test]
async fn unmount_failure_aborts_the_removal_with_the_volume_intact() {
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

    let specs = cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load specs");
    let handle = run_multi_with_transports(&process_config(), {
        vec![(
            specs.into_iter().next().expect("volume a"),
            RunOptions::default(),
            mock_transport().await as Arc<dyn CloudTransport>,
        )]
    })
    .await
    .expect("boot");
    let addr = control_addr();

    // Inject the never-succeeding release (the "letter never disappears"
    // probe — the winfsp unmount is the real counterpart behind the same
    // trait).
    let attempts = Arc::new(AtomicUsize::new(0));
    let released = Arc::new(AtomicBool::new(false));
    assert!(
        handle.live_table().set_release(
            "a",
            Box::new(FakeRelease {
                attempts: Arc::clone(&attempts),
                released: Arc::clone(&released),
                succeed: false,
            }),
        ),
        "the release injects into the live volume"
    );

    let reply = send_cmd(addr, "REMOVE a").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("unmount"),
        "the unmount failure aborts the removal actionably: {reply}"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "exactly one release attempt (no retry storm behind the abort)"
    );
    assert!(
        !released.load(Ordering::SeqCst),
        "the (fake) drive letter was never released — nothing was half-unmounted"
    );
    assert_eq!(
        handle.volumes(),
        vec![("a".to_string(), VolumeStatus::Running)],
        "the volume stays registered through the abort"
    );

    // A succeeding release lets the same removal complete.
    assert!(
        handle.live_table().set_release(
            "a",
            Box::new(FakeRelease {
                attempts: Arc::new(AtomicUsize::new(0)),
                released: Arc::clone(&released),
                succeed: true,
            }),
        ),
        "the replacement release injects"
    );
    let reply = send_cmd(addr, "REMOVE a").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("removed volume `a`"),
        "the removal completes once the drive releases: {reply}"
    );
    assert!(
        released.load(Ordering::SeqCst),
        "the succeeding release ran"
    );
    assert!(
        handle.volume("a").is_none(),
        "the volume is gone after the completed removal"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// The injectable K50 release probe: succeeds or fails on demand, with
/// attempt/release observation for the abort assertions.
struct FakeRelease {
    attempts: Arc<AtomicUsize>,
    released: Arc<AtomicBool>,
    succeed: bool,
}

impl DriveRelease for FakeRelease {
    fn release(
        &mut self,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        let attempts = Arc::clone(&self.attempts);
        let released = Arc::clone(&self.released);
        let succeed = self.succeed;
        Box::pin(async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            // The disappearance poll's stand-in: the winfsp unmount waits
            // for the letter to vanish and reports when it does not.
            tokio::time::sleep(Duration::from_millis(20)).await;
            if succeed {
                released.store(true, Ordering::SeqCst);
                Ok(())
            } else {
                Err(
                    "the drive letter did not disappear within the unmount window \
                     (a program is holding it open)"
                        .to_string(),
                )
            }
        })
    }
}

// ------------------------------------------------------- 3. LIST data face ---

/// LIST answers on every multi-volume instance (commands are process
/// surfaces, not options): a plain boot without a runtime dispatch still
/// lists, and its ADD refusal names the missing dispatch instead of
/// failing silently.
#[tokio::test]
async fn list_works_and_add_without_dispatch_refuses() {
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

    let specs = cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load specs");
    let handle = run_multi_with_transports(&process_config(), {
        vec![(
            specs.into_iter().next().expect("volume a"),
            RunOptions::default(),
            mock_transport().await as Arc<dyn CloudTransport>,
        )]
    })
    .await
    .expect("boot");
    let addr = control_addr();

    let reply = send_cmd(addr, "LIST").await;
    assert_eq!(
        list_rows(&reply),
        vec!["a running - telegram pending=0".to_string()],
        "LIST works without a runtime dispatch: {reply}"
    );

    // A valid, unregistered volume file with no dispatch behind ADD:
    // the refusal names the missing dispatch (the file checks pass —
    // they precede it — so this is the dispatch gate's own wording).
    write_file(
        &dir.path().join("volumes").join("b.toml"),
        &volume_toml("222:BBB", 222222),
    );
    let reply = send_cmd(addr, "ADD b").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("dispatch"),
        "ADD without a runtime dispatch is refused actionably: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// ------------------------------- 4. shutdown gate vs in-flight commands (H1) ---

/// H1 (review fix): a REMOVE caught mid-drain by the shutdown gate —
/// Ctrl+C / `cydrive stop` firing while the K50 drain step waits on a
/// held upload — must observe the gate within one poll tick, put the
/// entry BACK into the live table (so the stop task's idle barrier plus
/// `take_all` sees it and the volume drains through the shutdown
/// sequence), and answer a shutdown-specific ERR well before the drain
/// budget expires. The `FakeRelease` probe, planted while the entry
/// still sits in the live table (REMOVE's first move takes it out),
/// travels with the entry and proves the hand-back end to end: after
/// the shutdown joins, its one and only attempt ran — the stop task's
/// teardown pass released the handed-back entry.
#[tokio::test]
async fn remove_drain_observes_the_shutdown_gate_and_hands_the_entry_back() {
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

    let specs = cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load specs");
    assert_eq!(
        specs
            .iter()
            .map(|spec| spec.name.clone())
            .collect::<Vec<_>>(),
        vec!["a".to_string(), "stuck".to_string()],
        "the mocks pair up with the specs by position"
    );
    // stuck's boot transport: one authoritative RateLimited{12s} — the
    // worker honors the wait exactly (the job stays in flight, REMOVE
    // parks in its drain loop), and the exhausted script then serves
    // the retry as Ok, so the shutdown's own queue drain finishes
    // shortly after the sleep instead of hanging the test.
    let stuck_mock = Arc::new(
        MockTransport::builder()
            .upload_action(UploadAction::Fail {
                error: StorageError::RateLimited {
                    retry_after: Some(Duration::from_secs(12)),
                },
            })
            .build(),
    );
    stuck_mock.connect().await.expect("pre-connect stuck mock");
    let handle = boot_with_commands(
        process_config(),
        specs,
        vec![mock_transport().await, stuck_mock],
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            // A 10s drain budget: the red behaviour would wait it out
            // fully; the gate observation must win far earlier.
            remove_tuning: RemoveTuning {
                drain_timeout: Duration::from_secs(10),
                poll_interval: Duration::from_millis(50),
            },
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();

    // Hold one real upload in flight (the RateLimited sleep).
    let source = dir.path().join("hold.txt");
    fs::write(&source, b"hold me in flight").expect("write source");
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

    // The release probe goes in BEFORE the REMOVE takes the entry out:
    // it then travels with the entry (take → hand-back → take_all), and
    // only the stop task's teardown pass calls it.
    let attempts = Arc::new(AtomicUsize::new(0));
    let released = Arc::new(AtomicBool::new(false));
    assert!(
        handle.live_table().set_release(
            "stuck",
            Box::new(FakeRelease {
                attempts: Arc::clone(&attempts),
                released: Arc::clone(&released),
                succeed: true,
            }),
        ),
        "the probe injects into the live volume"
    );

    // REMOVE parks in its drain loop, THEN the gate fires — the Ctrl+C
    // topology (a control-channel STOP would queue behind the in-flight
    // REMOVE on the accept loop; the signal arm fires the gate directly
    // through `request_stop`, which is what races the command). The
    // reply wording pins that the drain observation point answered, not
    // the entry refusal.
    let started = std::time::Instant::now();
    let remove_task = tokio::spawn(async move { send_cmd(addr, "REMOVE stuck").await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    handle.request_stop();
    let reply = timeout(Duration::from_secs(8), remove_task)
        .await
        .expect("REMOVE settles after the gate fire")
        .expect("the REMOVE task joins");
    let elapsed = started.elapsed();
    assert!(
        reply.starts_with("ERR:")
            && reply.contains("shutting down")
            && reply.contains("removal aborted"),
        "REMOVE answers the shutdown-specific drain abort (took {elapsed:?}): {reply}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the gate observation wins well inside the 10s drain budget: {elapsed:?}"
    );

    // The real stop task now owns the handed-back entry: its idle
    // barrier waits out the in-flight REMOVE, `take_all` sees the entry
    // (the hand-back is what the release probe proves), and the queue
    // drain joins once the held worker's 12s sleep expires.
    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "exactly one release ran — the shutdown's teardown pass picked up the handed-back entry"
    );
    assert!(
        released.load(Ordering::SeqCst),
        "the handed-back entry's release step actually ran"
    );
}

/// H1 (review fix): once the stop gate has fired, the command surface
/// refuses new ADD/REMOVE (an answer must still come back — the
/// connection task parks on the reply one-shot) while the read-only
/// LIST keeps answering; nothing mutates the tables the shutdown
/// sequence is about to drain. Red: the ADD runs to completion (an OK)
/// and the REMOVE runs into the already-drained live state.
#[tokio::test]
async fn volume_commands_are_refused_after_the_stop_gate_fires() {
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
        &dir.path().join("volumes").join("b.toml"),
        &volume_toml("333:CCC", 333333),
    );
    let _guard = chdir(dir.path());

    let specs = {
        let mut v =
            cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
        v.retain(|spec| spec.name == "a");
        v
    };
    let handle = boot_with_commands(
        process_config(),
        specs,
        vec![mock_transport().await],
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();

    // Fire the gate the production way (a real `cydrive stop` walks
    // this exact path); the accept loop keeps serving connections, so a
    // late command still reaches the handler.
    let reply = send_cmd(addr, "STOP").await;
    assert!(reply.starts_with("OK:"), "STOP acknowledges: {reply}");

    let reply = send_cmd(addr, "ADD b").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("shutting down"),
        "ADD after the gate is refused with the shutdown reason: {reply}"
    );
    let reply = send_cmd(addr, "REMOVE a").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("shutting down"),
        "REMOVE after the gate is refused with the shutdown reason: {reply}"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("a running"),
        "the read-only LIST still answers on a shutting-down instance: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// ------------------------- 5. publish-after-mount ordering (H3) ---

/// A mount stub that parks until released: reports "started" over the
/// channel the moment the ADD enters its mount step, then holds the ADD
/// inside the mount window until the watch flips — the deterministic
/// stand-in for the seconds-long real mount (review H3's seam).
fn blocked_mount_stub(
    started: tokio::sync::mpsc::Sender<()>,
    go: tokio::sync::watch::Receiver<bool>,
) -> RuntimeMount {
    Arc::new(move |name: &str, letter: &str| {
        let volume = name.to_string();
        // Canonical display form (the real mounter's own): "Q" -> "Q:".
        let letter = format!("{}:", letter.trim_end_matches(':').to_ascii_uppercase());
        let started = started.clone();
        let mut go = go.clone();
        Box::pin(async move {
            let _ = started.send(()).await;
            while !*go.borrow_and_update() {
                if go.changed().await.is_err() {
                    break;
                }
            }
            VolumeMounts {
                mounted: vec![MountedVolume {
                    volume,
                    letter,
                    backend: MountedBackend::WebDav,
                }],
                winfsp: Default::default(),
            }
        })
    })
}

/// H3 (review fix): an ADD's faces publish only after its mount settles.
/// The old order registered the volume into all three faces BEFORE the
/// (seconds-long) mount pass — `/vol/<name>` routed and the dashboard
/// tab appeared while the drive did not exist yet, and a mount failure
/// rolled the faces back under in-flight requests (axum request tasks
/// run concurrently with the control loop; "same task, serialized" only
/// holds between commands). With the stub blocking mid-mount, a PROPFIND
/// during the window must NOT route; after the stub succeeds and the ADD
/// answers, the volume (and its mount) is visible everywhere.
#[tokio::test]
async fn add_publishes_its_faces_only_after_the_mount_settles() {
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
        &dir.path().join("volumes").join("m.toml"),
        &format!("{}drive_letter = \"Q\"\n", volume_toml("222:BBB", 222222)),
    );
    let _guard = chdir(dir.path());

    let specs = {
        let mut v =
            cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
        v.retain(|spec| spec.name == "a");
        v
    };
    let (started_tx, mut started_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (go_tx, go_rx) = tokio::sync::watch::channel(false);
    let handle = boot_with_commands(
        mount_process_config(),
        specs,
        vec![mock_transport().await],
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            mount: Some(blocked_mount_stub(started_tx, go_rx)),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();
    let webdav = handle.webdav_addr().expect("webdav bound");

    // The ADD parks inside the (stubbed) mount step.
    let add_task = tokio::spawn(async move { send_cmd(addr, "ADD m").await });
    timeout(Duration::from_secs(5), started_rx.recv())
        .await
        .expect("the ADD reaches its mount step within the safety window")
        .expect("the started signal arrives");

    // During the mount window the volume is invisible on every face —
    // this PROPFIND is exactly the in-flight request the old order
    // served off a not-yet-mounted (and, on failure, rolled-back)
    // volume.
    let resp = send_http(webdav, &http_request("PROPFIND", "/vol/m/", webdav)).await;
    assert_eq!(
        status_of(&resp),
        404,
        "not routable while the mount is in flight: {resp}"
    );
    assert!(
        handle.volume("m").is_none(),
        "the registry face is still empty during the mount window"
    );

    // Release the stub: the ADD completes and publishes everywhere.
    go_tx.send(true).expect("release the mount stub");
    let reply = timeout(Duration::from_secs(5), add_task)
        .await
        .expect("the ADD settles once the stub releases")
        .expect("the ADD task joins");
    assert!(
        reply.starts_with("OK:") && reply.contains("mounted Q:"),
        "the ADD reports the mounted letter: {reply}"
    );
    let resp = send_http(webdav, &http_request("PROPFIND", "/vol/m/", webdav)).await;
    assert_eq!(
        status_of(&resp),
        207,
        "routable after the mount settled: {resp}"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        list_rows(&reply)
            .iter()
            .any(|row| row.starts_with("m running Q: webdav")),
        "LIST shows the volume with its mount: {reply}"
    );

    // Cleanup: swap the (would-be net-use) release for the probe so the
    // REMOVE never touches a real drive, then remove and stop.
    let released = Arc::new(AtomicBool::new(false));
    let released_for_cleanup = Arc::clone(&released);
    assert!(
        handle.live_table().set_release(
            "m",
            Box::new(FakeRelease {
                attempts: Arc::new(AtomicUsize::new(0)),
                released: released_for_cleanup,
                succeed: true,
            }),
        ),
        "the probe injects into the mounted volume"
    );
    let reply = send_cmd(addr, "REMOVE m").await;
    assert!(reply.starts_with("OK:"), "cleanup REMOVE: {reply}");
    assert!(released.load(Ordering::SeqCst), "the probe release ran");

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// H3 (review fix): a runtime ADD whose drive mount fails answers ERR
/// and leaves NOTHING behind — no registry entry, no `/vol/<name>`
/// route, no live entry — so a later REMOVE of the name answers the
/// plain "no volume registered" refusal, and the sibling kept serving
/// throughout. (The pre-fix rollback produced the same post-state; this
/// pins the new never-registered teardown path.)
#[tokio::test]
async fn add_whose_mount_fails_leaves_no_residue_anywhere() {
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
        &dir.path().join("volumes").join("m.toml"),
        &format!("{}drive_letter = \"Q\"\n", volume_toml("222:BBB", 222222)),
    );
    let _guard = chdir(dir.path());

    let specs = {
        let mut v =
            cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
        v.retain(|spec| spec.name == "a");
        v
    };
    let failing_mount: RuntimeMount =
        Arc::new(|_name: &str, _letter: &str| Box::pin(async { VolumeMounts::default() }));
    let handle = boot_with_commands(
        mount_process_config(),
        specs,
        vec![mock_transport().await],
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            mount: Some(failing_mount),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();
    let webdav = handle.webdav_addr().expect("webdav bound");

    let reply = send_cmd(addr, "ADD m").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("rolled back"),
        "the failed mount fails the ADD actionably: {reply}"
    );
    assert!(
        handle.volume("m").is_none(),
        "no registry entry survives the failed ADD"
    );
    let resp = send_http(webdav, &http_request("PROPFIND", "/vol/m/", webdav)).await;
    assert_eq!(status_of(&resp), 404, "no route survives: {resp}");
    let reply = send_cmd(addr, "LIST").await;
    assert_eq!(
        list_rows(&reply),
        vec!["a running - telegram pending=0".to_string()],
        "only the sibling is listed: {reply}"
    );
    let reply = send_cmd(addr, "REMOVE m").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("no volume registered"),
        "nothing to remove after the rolled-back ADD: {reply}"
    );
    let resp = send_http(webdav, &http_request("PROPFIND", "/vol/a/", webdav)).await;
    assert_eq!(status_of(&resp), 207, "the sibling kept serving: {resp}");

    // The unpublished teardown is bounded: the stop task joins cleanly.
    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// ----------------------------- 6. the production net-use release (M1-3) ---

/// M1-3 (review fix): the production net-use release object (now on the
/// blocking pool with a 30s budget) rides the same K50 abort path the
/// FakeRelease pins — injected over a live volume with an unmapped
/// letter, its Err must abort the REMOVE with the volume intact and the
/// net-use failure surfacing verbatim. The budget branch (a hung
/// provider) is untestable offline; its cover is the constant's comment
/// plus review.
#[tokio::test]
async fn the_real_net_use_release_aborts_the_removal_when_it_fails() {
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

    let specs = cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load specs");
    let handle = run_multi_with_transports(&process_config(), {
        vec![(
            specs.into_iter().next().expect("volume a"),
            RunOptions::default(),
            mock_transport().await as Arc<dyn CloudTransport>,
        )]
    })
    .await
    .expect("boot");
    let addr = control_addr();
    let webdav = handle.webdav_addr().expect("webdav bound");

    // A letter nothing maps: the production release's real net use must
    // fail on it (deterministic — picked off this machine's mount set).
    let used = cloudkit_platform::windows::used_drive_letters();
    let letter = (b'A'..=b'Z')
        .map(|byte| format!("{}:", char::from(byte)))
        .find(|letter| {
            used.iter()
                .all(|mounted| !mounted.eq_ignore_ascii_case(letter))
        })
        .expect("at least one unmapped drive letter exists");
    assert!(
        handle
            .live_table()
            .set_release("a", Box::new(cloudkit_cli::WebDavRelease { letter })),
        "the production release injects into the live volume"
    );

    let reply = send_cmd(addr, "REMOVE a").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("unmounting volume `a` failed"),
        "the release failure aborts the removal actionably: {reply}"
    );
    assert!(
        reply.contains("net use"),
        "the net-use failure surfaces verbatim in the abort reason: {reply}"
    );
    assert_eq!(
        handle.volumes(),
        vec![("a".to_string(), VolumeStatus::Running)],
        "the volume stays registered through the abort (K50)"
    );
    let resp = send_http(webdav, &http_request("PROPFIND", "/vol/a/", webdav)).await;
    assert_eq!(status_of(&resp), 207, "the data plane survives: {resp}");

    // A healthy release then completes the same removal.
    assert!(
        handle.live_table().set_release(
            "a",
            Box::new(FakeRelease {
                attempts: Arc::new(AtomicUsize::new(0)),
                released: Arc::new(AtomicBool::new(false)),
                succeed: true,
            }),
        ),
        "the replacement release injects"
    );
    let reply = send_cmd(addr, "REMOVE a").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("removed volume `a`"),
        "the removal completes once the drive releases: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// --------------------- 7. M5 characterization: serialization / empty table ---

/// M5 test gap 1 (characterization, expected green — pins control.rs's
/// documented concurrency contract: "volume commands queue behind the
/// one in flight"). The accept loop awaits the serialized handler on
/// its own task, so while a REMOVE parks in its K50 drain window the
/// whole channel is held:
///
/// - a second connection's `LIST` is not answered until the REMOVE
///   settles — its command queues behind the in-flight one — and then
///   answers with the correct post-removal list;
/// - a `PING` connection is likewise not answered while the REMOVE
///   holds the loop. PING never enters the command queue (the
///   connection task answers it inline; the handler-record tests in
///   `control_channel.rs` pin that STOP and PING never reach the
///   handler), but the serialized accept loop cannot even ACCEPT the
///   connection until the handler returns — so the reply lands right
///   after the REMOVE settles, not before. This is the documented
///   queueing ("a later command, STOP included, queues behind the one
///   in flight"), not a PING regression.
///
/// The STOP arm is deliberately not constructed here: it shares the
/// same accept-loop serialization (its reply would equally wait out
/// the in-flight REMOVE), and firing the shutdown callback mid-test
/// would race the shutdown sequence for no extra pin.
#[tokio::test]
async fn a_slow_remove_holds_the_control_channel_until_it_settles() {
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

    let specs = {
        let mut v =
            cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
        v.retain(|spec| spec.name == "a");
        v
    };
    // The ADD dispatch hands out a transport whose ONE upload fails
    // with an authoritative RateLimited{5s}: the worker honors the wait
    // exactly (the job stays in flight for five seconds), then the
    // exhausted script serves the retry as Ok — so the REMOVE parks in
    // its drain loop for a bounded, observable window and then
    // COMPLETES (the drain sees outstanding reach zero well inside the
    // 15s budget).
    let hold_dispatch: VolumeTransportDispatch = Arc::new(move |_spec: &VolumeConfig| {
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
    });
    let handle = boot_with_commands(
        process_config(),
        specs,
        vec![mock_transport().await],
        RuntimeVolumeCommands {
            dispatch: Some(hold_dispatch),
            remove_tuning: RemoveTuning {
                drain_timeout: Duration::from_secs(15),
                poll_interval: Duration::from_millis(50),
            },
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();

    let reply = send_cmd(addr, "ADD stuck").await;
    assert!(reply.starts_with("OK:"), "stuck adds cleanly: {reply}");

    // Park one real upload in flight (the RateLimited sleep) — the
    // REMOVE will wait on exactly this in its drain loop.
    let source = dir.path().join("hold.txt");
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

    // The REMOVE parks in its drain loop; PING and LIST arrive on
    // their own connections while it is in flight.
    let remove_task = tokio::spawn(async move { send_cmd(addr, "REMOVE stuck").await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut ping_task = tokio::spawn(async move { send_cmd(addr, "PING").await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut list_task = tokio::spawn(async move { send_cmd(addr, "LIST").await });

    // While the REMOVE is in flight, neither connection is answered —
    // the LIST queues behind the in-flight command, and the PING
    // connection cannot even be accepted by the serialized loop.
    assert!(
        timeout(Duration::from_millis(400), &mut ping_task)
            .await
            .is_err(),
        "PING is not answered while the in-flight REMOVE holds the accept loop"
    );
    assert!(
        timeout(Duration::from_millis(400), &mut list_task)
            .await
            .is_err(),
        "LIST is not answered while the in-flight REMOVE holds the accept loop"
    );

    // The REMOVE settles (the held upload retries to Ok, the drain sees
    // zero) — and only then do the parked connections get served.
    let reply = timeout(Duration::from_secs(30), remove_task)
        .await
        .expect("REMOVE settles within the budget")
        .expect("the REMOVE task joins");
    assert!(
        reply.starts_with("OK:") && reply.contains("removed volume `stuck`"),
        "the held upload drains and the REMOVE completes: {reply}"
    );
    let reply = timeout(Duration::from_secs(5), ping_task)
        .await
        .expect("PING is served once the REMOVE settles")
        .expect("the PING task joins");
    assert!(
        reply.starts_with("OK: cydrive"),
        "the parked PING answers the version line once the loop is free: {reply}"
    );
    let reply = timeout(Duration::from_secs(5), list_task)
        .await
        .expect("LIST is served once the REMOVE settles")
        .expect("the LIST task joins");
    assert_eq!(
        list_rows(&reply),
        vec!["a running - telegram pending=0".to_string()],
        "the queued LIST answers with the correct post-removal list: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// M5 test gap 2 (characterization, expected green): removing every
/// volume down to the empty registry leaves the instance serving —
/// `LIST` answers `OK: 0 volume(s)` with no volume rows, the WebDAV
/// listener still accepts NEW connections (a PROPFIND on a removed
/// volume's prefix is the routed 404, not a refused connection), and
/// PING still answers (K51's empty-table liveness + K50's
/// remove-to-empty never taking the process down).
#[tokio::test]
async fn removing_every_volume_leaves_the_empty_instance_serving() {
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
        &dir.path().join("volumes").join("b.toml"),
        &volume_toml("222:BBB", 222222),
    );
    let _guard = chdir(dir.path());

    let specs = cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load specs");
    let handle = boot_with_commands(
        process_config(),
        specs,
        vec![mock_transport().await, mock_transport().await],
        RuntimeVolumeCommands::default(),
    )
    .await;
    let addr = control_addr();
    let webdav = handle.webdav_addr().expect("webdav bound");

    // Both volumes answer at boot.
    let resp = send_http(webdav, &http_request("PROPFIND", "/vol/a/", webdav)).await;
    assert_eq!(status_of(&resp), 207, "volume a at boot: {resp}");
    let resp = send_http(webdav, &http_request("PROPFIND", "/vol/b/", webdav)).await;
    assert_eq!(status_of(&resp), 207, "volume b at boot: {resp}");

    // Remove down to the empty table.
    let reply = send_cmd(addr, "REMOVE a").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("removed volume `a`"),
        "REMOVE a: {reply}"
    );
    let reply = send_cmd(addr, "REMOVE b").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("removed volume `b`"),
        "REMOVE b: {reply}"
    );
    assert_eq!(handle.volumes(), Vec::<(String, VolumeStatus)>::new());

    // The empty registry: LIST answers the zero-volume header with no
    // volume rows behind it.
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        reply.starts_with("OK: 0 volume(s)"),
        "the empty table's LIST header: {reply}"
    );
    assert!(
        list_rows(&reply).is_empty(),
        "no volume rows behind the zero-volume header: {reply}"
    );

    // The WebDAV listener keeps accepting new connections: the removed
    // volumes' prefixes are the routed 404 (the dispatch table read per
    // request), not a refused/dropped connection.
    let resp = send_http(webdav, &http_request("PROPFIND", "/vol/a/", webdav)).await;
    assert_eq!(
        status_of(&resp),
        404,
        "a fresh connection gets the routed 404 for removed volume a: {resp}"
    );
    let resp = send_http(webdav, &http_request("PROPFIND", "/vol/b/", webdav)).await;
    assert_eq!(
        status_of(&resp),
        404,
        "a fresh connection gets the routed 404 for removed volume b: {resp}"
    );

    // The instance is still up and answering on the control channel.
    let reply = send_cmd(addr, "PING").await;
    assert!(
        reply.starts_with("OK: cydrive"),
        "PING still answers on the emptied instance: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// --------------------- 6. ENABLE / DISABLE / CONFIGS (web volume P1) ---

/// An instant mount stub (the blocked stub's success arm without the
/// parking): the DISABLE test only needs a mount present in the live
/// table, not its timing.
fn instant_mount_stub() -> RuntimeMount {
    Arc::new(move |name: &str, letter: &str| {
        let volume = name.to_string();
        let letter = format!("{}:", letter.trim_end_matches(':').to_ascii_uppercase());
        Box::pin(async move {
            VolumeMounts {
                mounted: vec![MountedVolume {
                    volume,
                    letter,
                    backend: MountedBackend::WebDav,
                }],
                winfsp: Default::default(),
            }
        })
    })
}

/// `DISABLE <name>` (web volume management P1): the file write lands
/// FIRST (the persistent source of truth — a failed write must never
/// touch the runtime), then the volume comes down through the SAME K50
/// sequence REMOVE runs: the drive release executes, the volume leaves
/// every face (LIST, the WebDAV route) and `enabled = false` is what the
/// file on disk says.
#[tokio::test]
async fn disable_writes_the_file_releases_the_drive_and_unregisters_everywhere() {
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
        &dir.path().join("volumes").join("m.toml"),
        &format!("{}drive_letter = \"Q\"\n", volume_toml("222:BBB", 222222)),
    );
    let _guard = chdir(dir.path());

    let specs = {
        let mut v =
            cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
        v.retain(|spec| spec.name == "a");
        v
    };
    let handle = boot_with_commands(
        mount_process_config(),
        specs,
        vec![mock_transport().await],
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            mount: Some(instant_mount_stub()),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();
    let webdav = handle.webdav_addr().expect("webdav bound");

    // Bring `m` up at runtime with a mounted (stubbed) drive.
    let reply = send_cmd(addr, "ADD m").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("mounted Q:"),
        "the runtime ADD mounts the stubbed drive: {reply}"
    );

    // Swap the (would-be net-use) release for the probe, then DISABLE.
    let released = Arc::new(AtomicBool::new(false));
    let released_for_probe = Arc::clone(&released);
    assert!(
        handle.live_table().set_release(
            "m",
            Box::new(FakeRelease {
                attempts: Arc::new(AtomicUsize::new(0)),
                released: released_for_probe,
                succeed: true,
            })
        ),
        "the probe injects into the mounted volume"
    );

    let reply = send_cmd(addr, "DISABLE m").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("disabled volume `m`"),
        "the DISABLE acknowledges and names the volume: {reply}"
    );
    assert!(
        released.load(Ordering::SeqCst),
        "the drive release ran (the same K50 step-2 REMOVE runs)"
    );
    let file = fs::read_to_string(dir.path().join("volumes").join("m.toml"))
        .expect("read the volume file");
    assert!(
        file.contains("enabled = false"),
        "the file carries the persistent disable: {file}"
    );
    assert!(
        file.contains("bot_token = \"222:BBB\""),
        "the explicit keys survive the rewrite: {file}"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert_eq!(
        list_rows(&reply),
        vec!["a running - telegram pending=0".to_string()],
        "the registry face dropped m: {reply}"
    );
    let resp = send_http(webdav, &http_request("PROPFIND", "/vol/m/", webdav)).await;
    assert_eq!(status_of(&resp), 404, "the WebDAV face dropped m: {resp}");

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// A volume that was not running (already stopped, or REMOVEd) still
/// DISABLEs: the file write is the whole action, the reply says the
/// runtime was untouched.
#[tokio::test]
async fn disable_of_a_stopped_volume_only_writes_the_file() {
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
        &dir.path().join("volumes").join("off.toml"),
        &volume_toml("222:BBB", 222222),
    );
    let _guard = chdir(dir.path());

    let specs = {
        let mut v =
            cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
        v.retain(|spec| spec.name == "a");
        v
    };
    let handle = boot_with_commands(
        process_config(),
        specs,
        vec![mock_transport().await],
        RuntimeVolumeCommands::default(),
    )
    .await;
    let addr = control_addr();

    let reply = send_cmd(addr, "DISABLE off").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("was not running"),
        "a stopped volume disables file-only: {reply}"
    );
    let file = fs::read_to_string(dir.path().join("volumes").join("off.toml"))
        .expect("read the volume file");
    assert!(
        file.contains("enabled = false"),
        "the file carries the disable: {file}"
    );
    // Idempotent: a second DISABLE of the now-disabled file answers OK.
    let reply = send_cmd(addr, "DISABLE off").await;
    assert!(
        reply.starts_with("OK:"),
        "second DISABLE is idempotent: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// The K50 philosophy on the write side: a failed file write must not
/// touch the runtime (the persistent file is the source of truth — a
/// runtime disable the file contradicts would resurrect on the next
/// boot). A read-only volume file forces the write failure.
#[tokio::test]
#[allow(clippy::permissions_set_readonly_false)] // restoring the write bit a Windows-readonly test file cleared; the TempDir's scratch file carries no permission meaning on any platform
async fn disable_write_failure_leaves_the_runtime_untouched() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    let file = dir.path().join("volumes").join("a.toml");
    write_file(&file, &volume_toml("111:AAA", 111111));
    let _guard = chdir(dir.path());

    let specs =
        cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
    let handle = boot_with_commands(
        process_config(),
        specs,
        vec![mock_transport().await],
        RuntimeVolumeCommands::default(),
    )
    .await;
    let addr = control_addr();

    // Make the volume file unwritable (the Windows readonly attribute
    // denies fs::write).
    let mut perms = fs::metadata(&file).expect("file metadata").permissions();
    perms.set_readonly(true);
    fs::set_permissions(&file, perms).expect("make the file read-only");

    let reply = send_cmd(addr, "DISABLE a").await;
    let mut perms = fs::metadata(&file).expect("file metadata").permissions();
    perms.set_readonly(false);
    let _ = fs::set_permissions(&file, perms); // restore for the TempDir drop
    assert!(
        reply.starts_with("ERR:") && reply.contains("writing"),
        "the write failure is the refusal: {reply}"
    );
    assert!(
        reply.contains("nothing was changed"),
        "the refusal says the runtime is untouched: {reply}"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert_eq!(
        list_rows(&reply),
        vec!["a running - telegram pending=0".to_string()],
        "the volume keeps running exactly as before: {reply}"
    );
    let text = fs::read_to_string(&file).expect("read the file");
    assert!(
        !text.contains("enabled = false"),
        "the file was not rewritten: {text}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// `ENABLE <name>` re-assembles a disabled volume through the runtime
/// ADD path and flips the file back — the DISABLE twin. The refusals
/// keep ADD's shape: a name without a file names the volumes_dir, and an
/// already-registered volume is refused.
#[tokio::test]
async fn enable_reassembles_the_disabled_volume() {
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
        &dir.path().join("volumes").join("off.toml"),
        &format!("enabled = false\n{}", volume_toml("222:BBB", 222222)),
    );
    let _guard = chdir(dir.path());

    let specs = {
        let mut v =
            cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
        v.retain(|spec| spec.name == "a");
        v
    };
    let handle = boot_with_commands(
        process_config(),
        specs,
        vec![mock_transport().await],
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();

    // ENABLE of the never-booted disabled file: the file flips and the
    // volume assembles.
    let reply = send_cmd(addr, "ENABLE off").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("added volume `off`"),
        "the ENABLE assembles through the ADD path: {reply}"
    );
    let file = fs::read_to_string(dir.path().join("volumes").join("off.toml"))
        .expect("read the volume file");
    assert!(
        file.contains("enabled = true"),
        "the file flipped back: {file}"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert_eq!(
        list_rows(&reply),
        vec![
            "a running - telegram pending=0".to_string(),
            "off running - telegram pending=0".to_string(),
        ],
        "both volumes run: {reply}"
    );

    // Refusals keep ADD's shapes.
    let reply = send_cmd(addr, "ENABLE ghost").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("no volume file"),
        "a missing file names the fact: {reply}"
    );
    let reply = send_cmd(addr, "ENABLE a").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("already registered"),
        "an already-running volume is refused: {reply}"
    );
    let reply = send_cmd(addr, "ENABLE").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("usage") && reply.contains("ENABLE <name>"),
        "malformed shapes get the usage line: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// `CONFIGS` is the configuration FULL set (the config page's data
/// source — LIST is the runtime registry and cannot see disabled
/// volumes): every `*.toml` under the volumes_dir, one row per file,
/// with the runtime join (running/absent), and a schema-broken file
/// reporting `invalid` on its row without failing the whole listing.
#[tokio::test]
async fn configs_lists_running_disabled_and_invalid_rows() {
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

    let specs =
        cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
    let handle = boot_with_commands(
        process_config(),
        specs,
        vec![mock_transport().await],
        RuntimeVolumeCommands::default(),
    )
    .await;
    let addr = control_addr();

    // Late-arriving files: a disabled volume and a schema-broken one
    // (written after the boot — CONFIGS scans the directory live).
    write_file(
        &dir.path().join("volumes").join("broken.toml"),
        "backend = \"local\"\nno_such_key = 1\n",
    );
    write_file(
        &dir.path().join("volumes").join("off.toml"),
        &format!("enabled = false\n{}", volume_toml("222:BBB", 222222)),
    );

    let reply = send_cmd(addr, "CONFIGS").await;
    assert!(
        reply.starts_with("OK: 3 volume file(s)"),
        "the header counts every file: {reply}"
    );
    let rows = list_rows(&reply);
    assert_eq!(
        rows[0], "a telegram enabled=true running",
        "the running volume row: {reply}"
    );
    assert!(
        rows[1].starts_with("broken invalid ("),
        "the broken file reports invalid on its row: {reply}"
    );
    assert!(
        rows[1].contains("no_such_key"),
        "the invalid row carries the refusal reason: {reply}"
    );
    assert_eq!(
        rows[2], "off telegram enabled=false absent",
        "the disabled volume row (absent from the runtime): {reply}"
    );

    // Usage: CONFIGS takes no argument.
    let reply = send_cmd(addr, "CONFIGS a").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("usage"),
        "CONFIGS with an argument gets the usage line: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// The H1 entry gate covers the new mutations: after the stop gate
/// fires, ENABLE and DISABLE are refused with the shutdown reason while
/// the read-only CONFIGS keeps answering (the LIST twin).
#[tokio::test]
async fn enable_disable_refused_after_the_stop_gate_fires() {
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
        &dir.path().join("volumes").join("b.toml"),
        &volume_toml("333:CCC", 333333),
    );
    let _guard = chdir(dir.path());

    let specs = {
        let mut v =
            cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
        v.retain(|spec| spec.name == "a");
        v
    };
    let handle = boot_with_commands(
        process_config(),
        specs,
        vec![mock_transport().await],
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();

    let reply = send_cmd(addr, "STOP").await;
    assert!(reply.starts_with("OK:"), "STOP acknowledges: {reply}");

    let reply = send_cmd(addr, "ENABLE b").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("shutting down"),
        "ENABLE after the gate is refused: {reply}"
    );
    let reply = send_cmd(addr, "DISABLE a").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("shutting down"),
        "DISABLE after the gate is refused: {reply}"
    );
    let file = fs::read_to_string(dir.path().join("volumes").join("a.toml"))
        .expect("read the volume file");
    assert!(
        !file.contains("enabled = false"),
        "the gate refusal left the file untouched: {file}"
    );
    let reply = send_cmd(addr, "CONFIGS").await;
    assert!(
        reply.starts_with("OK:"),
        "the read-only CONFIGS still answers: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}
