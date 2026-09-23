//! RED-phase tests for the web volume-management P5 batch (plan
//! `docs/plans/2026-09-13-web-volume-management.md` §1.2/§2.2/§3-P5):
//! the control channel's two-leg `DESTROY` protocol. The first leg
//! (`DESTROY <name>`) executes NOTHING — it answers the preview
//! (what a confirm would do, what stays by default, what is never
//! touched). The second leg (`DESTROY <name> confirm` [/ `purge_local`])
//! unloads a running volume through the SAME K50 sequence REMOVE runs
//! (a drain refusal aborts the WHOLE destroy — the file survives),
//! then deletes the volume file (idempotent on a missing file), and
//! only with the explicit `purge_local` third word also deletes the
//! volume's local data directory (K49 分档: remote data is NEVER
//! touched, the local directory stays by default).

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cloudkit_cli::control::read_control_addr;
use cloudkit_cli::{
    run_multi_with_transports_and_commands, RemoveTuning, RunOptions, RuntimeVolumeCommands,
    VolumeTransportDispatch,
};
use cloudkit_core::config::{CyDriveConfig, VolumeConfig};
use cloudkit_core::transport::mock::{MockTransport, UploadAction};
use cloudkit_core::transport::{CloudTransport, StorageError};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::timeout;

// ------------------------------------------------------------- helpers ---
// (the `volume_create_update.rs` conventions, verbatim in shape)

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
/// duration of the guard. Declare the guard *after* the owning `TempDir`.
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

/// The process-level config for multi-volume mode: ephemeral ports, no
/// dashboard, no auto-mount.
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

/// A telegram-flavoured volume file body (the injected mock replaces the
/// real connect; the keys keep the file a legal volume shape).
fn volume_toml(token: &str, chat_id: i64) -> String {
    format!("backend = \"telegram\"\nbot_token = \"{token}\"\nchat_id = {chat_id}\n")
}

/// A pre-connected mock transport.
async fn mock_transport() -> Arc<MockTransport> {
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect mock transport");
    mock
}

/// The runtime-ADD dispatch seam: every dispatched volume gets a fresh
/// healthy mock.
fn mock_dispatch() -> VolumeTransportDispatch {
    Arc::new(move |_spec: &VolumeConfig| {
        Box::pin(async {
            let mock = Arc::new(MockTransport::new());
            mock.connect().await.expect("pre-connect mock transport");
            Ok(Some((
                RunOptions::default(),
                mock as Arc<dyn CloudTransport>,
            )))
        })
    })
}

/// The boot over the volumes on disk plus the command surface.
async fn boot(
    cfg: CyDriveConfig,
    commands: RuntimeVolumeCommands,
) -> (cloudkit_cli::MultiVolumeHandle, SocketAddr) {
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
    let handle = run_multi_with_transports_and_commands(&cfg, injections, commands, false)
        .await
        .expect("multi-volume boot with runtime commands");
    let addr = read_control_addr(&process_config()).expect("control address");
    (handle, addr)
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

/// The LIST reply's data rows (everything after the header line).
fn list_rows(reply: &str) -> Vec<String> {
    reply
        .lines()
        .skip(1)
        .map(|line| line.trim().to_owned())
        .collect()
}

/// Seeds a volume's home directory with the furniture a live volume
/// would have (db / cache / local root), so the keep-vs-purge
/// assertions have real bytes on the line.
fn seed_home(dir: &Path, name: &str) {
    let home = dir.join("volumes").join(name);
    fs::create_dir_all(home.join("cache")).expect("create the cache dir");
    fs::create_dir_all(home.join("root")).expect("create the local root");
    fs::write(home.join("meta.db"), b"fake db bytes").expect("write the db");
    fs::write(home.join("cache").join("chunk-0"), b"fake cache bytes").expect("write cache");
    fs::write(home.join("root").join("file.txt"), b"local data").expect("write root file");
}

async fn shutdown(handle: cloudkit_cli::MultiVolumeHandle) {
    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// --------------------------------------------- P5: the preview leg (§1.2) ---

/// `DESTROY <name>` (no `confirm`) executes NOTHING: the volume file
/// stays, the running volume stays registered, and the reply is the
/// PREVIEW — an `OK:` (a confirmation request, not an error) naming
/// what a confirm would do (the K50 unmount for a running volume, the
/// file deletion), what stays by default (the local data directory,
/// with its path) and what is never touched (remote data), plus the
/// exact confirmation command.
#[tokio::test]
async fn destroy_preview_executes_nothing_and_previews() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:AAA", 111111),
    );
    seed_home(dir.path(), "a");
    let _guard = chdir(dir.path());

    let (handle, addr) = boot(
        process_config(),
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;

    let reply = send_cmd(addr, "DESTROY a").await;
    assert!(
        reply.starts_with("OK:"),
        "the preview is a confirmation request, not an error: {reply}"
    );
    assert!(
        reply.to_lowercase().contains("nothing was executed"),
        "the preview says nothing ran: {reply}"
    );
    // The preview teaches all three facts of 裁决②: the file path to be
    // deleted, the local data directory KEPT (with its path and the
    // purge_local way out), and the remote-data guarantee.
    assert!(
        reply.contains("volumes") && reply.contains("a.toml"),
        "the preview names the volume file: {reply}"
    );
    assert!(
        reply.to_lowercase().contains("local data directory")
            && reply.to_lowercase().contains("kept"),
        "the preview states the local data directory stays by default: {reply}"
    );
    assert!(
        reply.contains("purge_local"),
        "the preview names the purge_local escape hatch: {reply}"
    );
    assert!(
        reply.to_lowercase().contains("never touched") && reply.to_lowercase().contains("remote"),
        "the preview promises remote data is never touched: {reply}"
    );
    assert!(
        reply.contains("DESTROY a confirm"),
        "the preview teaches the confirmation command: {reply}"
    );

    // Zero side effects: file, home and registration all intact.
    assert!(
        dir.path().join("volumes").join("a.toml").is_file(),
        "the preview deleted no file"
    );
    assert!(
        dir.path()
            .join("volumes")
            .join("a")
            .join("meta.db")
            .is_file(),
        "the home directory is untouched"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        list_rows(&reply)
            .iter()
            .any(|row| row.starts_with("a running")),
        "the volume stays registered and running: {reply}"
    );

    shutdown(handle).await;
}

/// The preview's refusal states: a name with no volume file points at
/// CONFIGS (the full set), a path-unsafe name never becomes a path, and
/// the usage shapes teach the two legs.
#[tokio::test]
async fn destroy_preview_refusals_are_actionable() {
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

    let (handle, addr) = boot(
        process_config(),
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;

    // A valid name with no file: point at CONFIGS.
    let reply = send_cmd(addr, "DESTROY ghost").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("ghost"),
        "the missing file refuses naming the volume: {reply}"
    );
    assert!(
        reply.contains("CONFIGS"),
        "the refusal points at CONFIGS: {reply}"
    );

    // A path-unsafe name never becomes a path.
    for line in ["DESTROY ../config", "DESTROY a.toml"] {
        let reply = send_cmd(addr, line).await;
        assert!(
            reply.starts_with("ERR:") && reply.contains("not a volume name"),
            "{line} names the name rules: {reply}"
        );
    }

    // The usage shapes: no name at all, and the confirm/purge_local
    // misuse (the purge word only rides a confirm).
    for line in [
        "DESTROY",
        "DESTROY a extra",
        "DESTROY a purge_local",
        "DESTROY a confirm now",
    ] {
        let reply = send_cmd(addr, line).await;
        assert!(
            reply.starts_with("ERR:") && reply.contains("usage"),
            "{line} gets the usage line: {reply}"
        );
        assert!(
            reply.contains("confirm"),
            "the usage line teaches the two legs ({line}): {reply}"
        );
    }

    shutdown(handle).await;
}

// ------------------------------------- P5: the confirm leg (§1.2 / K49) ---

/// The full chain on a RUNNING volume: the K50 unmount (drain →
/// release → unregister), then the file deletion — and the OK spells
/// out 裁决②'s two guarantees: the local data directory is KEPT (with
/// its path) and remote data was never touched. The home directory's
/// bytes survive verbatim.
#[tokio::test]
async fn destroy_confirm_unmounts_then_deletes_the_file_and_keeps_the_home() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:AAA", 111111),
    );
    seed_home(dir.path(), "a");
    let _guard = chdir(dir.path());

    let (handle, addr) = boot(
        process_config(),
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            remove_tuning: RemoveTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;

    let reply = send_cmd(addr, "DESTROY a confirm").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("destroyed volume `a`"),
        "the confirm acknowledges and names the volume: {reply}"
    );
    assert!(
        reply.to_lowercase().contains("local data directory")
            && reply.to_lowercase().contains("kept"),
        "the OK states the local data directory was kept: {reply}"
    );
    assert!(
        reply.to_lowercase().contains("never touched") && reply.to_lowercase().contains("remote"),
        "the OK states remote data was never touched: {reply}"
    );

    // The file is gone; the home directory survives with its bytes.
    assert!(
        !dir.path().join("volumes").join("a.toml").exists(),
        "the volume file was deleted"
    );
    let db = fs::read(dir.path().join("volumes").join("a").join("meta.db"))
        .expect("the home directory survived");
    assert_eq!(db, b"fake db bytes", "the home's bytes are intact");
    assert!(
        dir.path()
            .join("volumes")
            .join("a")
            .join("root")
            .join("file.txt")
            .is_file(),
        "the local root data survived"
    );

    // The runtime: unregistered everywhere.
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        reply.starts_with("OK: 0 volume(s)"),
        "the volume is unregistered: {reply}"
    );
    assert!(
        send_cmd(addr, "SHOW a").await.starts_with("ERR:"),
        "no file left to SHOW"
    );

    shutdown(handle).await;
}

/// A drain refusal aborts the WHOLE destroy (K50's never-half-remove,
/// carried into the file deletion): a stuck upload keeps the volume
/// running AND the file on disk — the reply is the refusal naming the
/// destroy retry.
#[tokio::test]
async fn destroy_confirm_drain_refusal_keeps_the_file_and_the_volume() {
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

    // The boot injects only the healthy `a`; `stuck` joins through the
    // runtime ADD, which the hold dispatch serves with the HELD mock
    // (its uploads park on RateLimited{3600s}).
    let specs = {
        let mut v =
            cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
        v.retain(|spec| spec.name == "a");
        v
    };
    let hold_dispatch: VolumeTransportDispatch = Arc::new(move |spec: &VolumeConfig| {
        let _stuck = spec.name == "stuck"; // force the HRTB closure shape
        Box::pin(async move {
            let mock = Arc::new(
                MockTransport::builder()
                    .upload_action(UploadAction::Fail {
                        error: StorageError::RateLimited {
                            retry_after: Some(Duration::from_secs(3600)),
                        },
                    })
                    .build(),
            );
            mock.connect().await.expect("connect mock");
            Ok(Some((
                RunOptions::default(),
                mock as Arc<dyn CloudTransport>,
            )))
        })
    });
    let injections = vec![(
        specs[0].clone(),
        RunOptions::default(),
        mock_transport().await as Arc<dyn CloudTransport>,
    )];
    let handle = run_multi_with_transports_and_commands(
        &process_config(),
        injections,
        RuntimeVolumeCommands {
            dispatch: Some(hold_dispatch),
            remove_tuning: RemoveTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
        false,
    )
    .await
    .expect("boot");
    let addr = read_control_addr(&process_config()).expect("control address");

    // `stuck`'s file sits on disk but the boot injects only `a`, so the
    // held entry joins through the runtime ADD:
    let reply = send_cmd(addr, "ADD stuck").await;
    assert!(reply.starts_with("OK:"), "stuck adds cleanly: {reply}");
    let source = dir.path().join("hold.txt");
    fs::write(&source, b"hold me in flight").expect("write source");
    let rel = cloudkit_core::rel_path::RelPath::new("/hold.txt").expect("valid rel path");
    handle
        .volume("stuck")
        .expect("volume stuck")
        .vfs()
        .expect("vfs stuck")
        .ingest_file(&rel, &source, 1.0)
        .await
        .expect("ingest into stuck");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let reply = send_cmd(addr, "DESTROY stuck confirm").await;
    assert!(
        reply.starts_with("ERR:"),
        "the drain refusal aborts the destroy: {reply}"
    );
    assert!(
        reply.contains("in flight") || reply.contains("aborted"),
        "the refusal carries the K50 reason: {reply}"
    );
    assert!(
        reply.contains("DESTROY stuck confirm"),
        "the refusal names the destroy retry: {reply}"
    );
    assert!(
        dir.path().join("volumes").join("stuck.toml").is_file(),
        "the file was NOT deleted (never half-destroyed)"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        list_rows(&reply)
            .iter()
            .any(|row| row.starts_with("stuck running")),
        "the volume stays registered and running: {reply}"
    );

    // Deliberate leak (the remove_with_undrained_queue precedent): the
    // held worker sleeps 3600s; a drain-join would hang the test.
    drop(handle);
}

/// A volume whose file exists but is NOT registered (disabled at boot,
/// or unmounted by hand) takes the direct path: no unmount attempt,
/// the file deletes, the home stays, and the OK says the volume was
/// not running.
#[tokio::test]
async fn destroy_confirm_on_an_unregistered_volume_deletes_the_file_directly() {
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
        &format!("{}\nenabled = false\n", volume_toml("333:CCC", 333333)),
    );
    seed_home(dir.path(), "off");
    let _guard = chdir(dir.path());

    let (handle, addr) = boot(
        process_config(),
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            remove_tuning: RemoveTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        !reply.contains("off"),
        "the disabled volume never booted: {reply}"
    );

    let reply = send_cmd(addr, "DESTROY off confirm").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("destroyed volume `off`"),
        "the disabled volume destroys cleanly: {reply}"
    );
    assert!(
        reply.to_lowercase().contains("not running"),
        "the OK says the volume was not running: {reply}"
    );
    assert!(
        !dir.path().join("volumes").join("off.toml").exists(),
        "the file is gone"
    );
    assert!(
        dir.path()
            .join("volumes")
            .join("off")
            .join("meta.db")
            .is_file(),
        "the home directory stays by default"
    );

    shutdown(handle).await;
}

/// The confirm leg is idempotent on a missing file (a hand-deleted
/// file): nothing to unload, nothing to delete — the OK acknowledges
/// without pretending a file went away.
#[tokio::test]
async fn destroy_confirm_is_idempotent_when_the_file_is_already_gone() {
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
        &dir.path().join("volumes").join("gone.toml"),
        &volume_toml("444:DDD", 444444),
    );
    let _guard = chdir(dir.path());

    let (handle, addr) = boot(
        process_config(),
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            remove_tuning: RemoveTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;

    // The boot registered `gone` (its file existed); take it down by
    // hand (REMOVE), then delete the file by hand — the confirm leg
    // lands on a registered-nothing, file-nothing state.
    let reply = send_cmd(addr, "REMOVE gone").await;
    assert!(reply.starts_with("OK:"), "REMOVE unloads it: {reply}");
    fs::remove_file(dir.path().join("volumes").join("gone.toml")).expect("hand-delete the file");

    let reply = send_cmd(addr, "DESTROY gone confirm").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("destroyed volume `gone`"),
        "the idempotent confirm still acknowledges: {reply}"
    );
    assert!(
        reply.to_lowercase().contains("already"),
        "the OK says the file was already gone: {reply}"
    );

    shutdown(handle).await;
}

// ------------------------------------------- P5: purge_local (裁决②) ---

/// The `purge_local` third word deletes the volume's HOME directory —
/// db, cache and local root together — and the OK says so; without the
/// word the home stays (the previous tests' default-keep assertions).
#[tokio::test]
async fn destroy_confirm_purge_local_deletes_the_home_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:AAA", 111111),
    );
    seed_home(dir.path(), "a");
    let _guard = chdir(dir.path());

    let (handle, addr) = boot(
        process_config(),
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            remove_tuning: RemoveTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;

    let reply = send_cmd(addr, "DESTROY a confirm purge_local").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("destroyed volume `a`"),
        "the purging confirm acknowledges: {reply}"
    );
    assert!(
        reply.to_lowercase().contains("local data directory")
            && reply.to_lowercase().contains("deleted"),
        "the OK states the local data directory was deleted: {reply}"
    );
    assert!(
        reply.to_lowercase().contains("never touched") && reply.to_lowercase().contains("remote"),
        "the OK still promises remote data was never touched: {reply}"
    );
    assert!(
        !dir.path().join("volumes").join("a.toml").exists(),
        "the volume file is gone"
    );
    assert!(
        !dir.path().join("volumes").join("a").exists(),
        "the whole home directory is gone (db, cache, root)"
    );

    shutdown(handle).await;
}

/// A purge failure does NOT roll the destroy back (the volume file is
/// already deleted — there is nothing to restore), but the reply must
/// say the deletion failed and name the leftover path. The failure
/// device: the home path exists as a plain FILE, which no directory
/// deletion can consume (cross-platform, no permission games). The
/// volume is DISABLED so the boot never assembles it (the assembly's
/// own `create_dir_all` on the home would trip over the file — the
/// device must fail the PURGE, not the boot).
#[tokio::test]
async fn destroy_confirm_purge_failure_reports_the_leftover_without_rollback() {
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
        &format!("{}\nenabled = false\n", volume_toml("555:EEE", 555555)),
    );
    // The home path occupied by a plain file: `remove_dir_all` fails.
    write_file(&dir.path().join("volumes").join("b"), "not a directory");
    let _guard = chdir(dir.path());

    let (handle, addr) = boot(
        process_config(),
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            remove_tuning: RemoveTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;

    let reply = send_cmd(addr, "DESTROY b confirm purge_local").await;
    assert!(
        reply.starts_with("ERR:"),
        "the purge failure surfaces as an ERR: {reply}"
    );
    assert!(
        reply.contains("failed"),
        "the refusal says the deletion failed: {reply}"
    );
    // The refusal names the leftover path (actionable: what to clean
    // by hand) and acknowledges what already succeeded.
    assert!(
        reply.contains("volumes") && reply.contains("b"),
        "the refusal names the leftover path: {reply}"
    );
    assert!(
        !dir.path().join("volumes").join("b.toml").exists(),
        "no rollback: the volume file stays deleted"
    );
    assert!(
        dir.path().join("volumes").join("b").is_file(),
        "the leftover stays exactly where it was"
    );

    shutdown(handle).await;
}

// ------------------------------- P5: the mutating family (H1 gate) ---

/// DESTROY joins the mutating family on the shutdown gate (H1): after
/// a real STOP both legs are refused — a shutting-down instance
/// accepts no volume deletion (and no preview either: the family gate
/// is keyword-level, like CREATE/UPDATE's).
#[tokio::test]
async fn destroy_is_refused_after_the_stop_gate_fires() {
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

    let (handle, addr) = boot(
        process_config(),
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;

    let reply = send_cmd(addr, "STOP").await;
    assert!(reply.starts_with("OK:"), "STOP acknowledges: {reply}");
    for line in ["DESTROY a", "DESTROY a confirm"] {
        let reply = send_cmd(addr, line).await;
        assert!(
            reply.starts_with("ERR:") && reply.contains("shutting down"),
            "{line} after the gate is refused with the shutdown reason: {reply}"
        );
    }
    assert!(
        dir.path().join("volumes").join("a.toml").is_file(),
        "the refused DESTROY deleted no file"
    );

    shutdown(handle).await;
}
