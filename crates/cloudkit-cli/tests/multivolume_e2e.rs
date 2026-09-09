//! RED-phase tests for Phase 2.5 / MV1: the VolumeRegistry assembly.
//!
//! Contract under test: `docs/plans/2026-09-08-phase2-5-multivolume.md`
//! §3-MV1 — `run_multi_with_transports` boots several volumes in one
//! process with per-volume isolation (K21 home directories, independent
//! dbs/caches/queues), degrades visibly on a failed volume while the rest
//! keep running (K22), stops every volume through ONE stop gate and the
//! process-level control file (K25), and the transport session/state
//! paths follow the injected base directory (telegram session, baidu
//! sessions_dir). Single-volume behavior is pinned zero-drift by the
//! existing `run_e2e.rs` suite.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cloudkit_cli::control::{control_file_path, read_control_addr, send_stop};
use cloudkit_cli::{
    baidu_params, run_multi_with_transports, transport_config_from, BaiduEndpoints, RunOptions,
    VolumeStatus,
};
use cloudkit_core::config::{load_volumes, CyDriveConfig, VolumeConfig};
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::CloudTransport;
use tokio::time::timeout;

// ------------------------------------------------------------- helpers ---

/// Serialises every test that changes the process-wide working directory
/// (tests in one binary share one process; `set_current_dir` races).
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

/// The process-level config for multi-volume mode: `volumes_dir` set,
/// everything else default (the process file may not carry volume keys).
fn process_config() -> CyDriveConfig {
    CyDriveConfig {
        volumes_dir: Some("volumes".to_string()),
        ..CyDriveConfig::default()
    }
}

/// A minimal legal telegram-flavoured volume file body (the injected
/// `MockTransport` replaces the real connect; the keys just keep the
/// file a legal volume shape).
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

/// Loads the volume manifest from `volumes/` under the current cwd (the
/// real MV0 discovery surface — same parse, same ordering).
fn load_specs() -> Vec<VolumeConfig> {
    load_volumes(Path::new("volumes")).expect("load volume specs")
}

/// The multi-volume boot under test: one `RunOptions` per volume, the
/// mock transports injected through the same seam the production
/// dispatch uses.
async fn boot_multi(
    specs: Vec<VolumeConfig>,
    mocks: Vec<Arc<MockTransport>>,
) -> cloudkit_cli::MultiVolumeHandle {
    assert_eq!(specs.len(), mocks.len(), "one transport per volume");
    let injections = specs
        .into_iter()
        .zip(mocks)
        .map(|(spec, mock)| (spec, RunOptions::default(), mock as Arc<dyn CloudTransport>))
        .collect();
    run_multi_with_transports(&process_config(), injections)
        .await
        .expect("multi-volume boot")
}

// ------------------------------------------------- 1. per-volume isolation ---

/// Two mock volumes in one process are fully isolated (K21): each volume
/// gets its own home directory (the default db file name lands inside
/// it), a write enqueued on volume A never shows up in volume B's db or
/// queue, and the shutdown drain uploads A's file through A's transport
/// only.
#[tokio::test]
async fn two_mock_volumes_isolate_db_cache_and_queues() {
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

    let specs = load_specs();
    assert_eq!(specs.len(), 2, "a and b discovered");
    let mock_a = mock_transport().await;
    let mock_b = mock_transport().await;
    let handle = boot_multi(specs, vec![mock_a.clone(), mock_b.clone()]).await;

    assert_eq!(
        handle.volume("a").expect("volume a").status(),
        VolumeStatus::Running,
        "volume a runs"
    );
    assert_eq!(
        handle.volume("b").expect("volume b").status(),
        VolumeStatus::Running,
        "volume b runs"
    );

    // K21: the default db file names land inside each volume's home dir.
    assert!(
        dir.path()
            .join("volumes")
            .join("a")
            .join("cydrive_meta.db")
            .exists(),
        "volume a's db lives in volumes/a/"
    );
    assert!(
        dir.path()
            .join("volumes")
            .join("b")
            .join("cydrive_meta.db")
            .exists(),
        "volume b's db lives in volumes/b/"
    );

    // One file into volume A only.
    let source = dir.path().join("source.txt");
    fs::write(&source, b"volume-a payload").expect("write source");
    let rel = RelPath::new("/file.txt").expect("valid rel path");
    handle
        .volume("a")
        .expect("volume a")
        .vfs()
        .expect("running volume has a vfs")
        .ingest_file(&rel, &source, 1.0)
        .await
        .expect("ingest into volume a");

    // Queues are per-volume: A's queue saw the job, B's did not.
    let stats_a = handle
        .volume("a")
        .expect("volume a")
        .vfs()
        .expect("vfs a")
        .queue_stats();
    let stats_b = handle
        .volume("b")
        .expect("volume b")
        .vfs()
        .expect("vfs b")
        .queue_stats();
    assert_eq!(stats_a.enqueued, 1, "volume a's queue has the job");
    assert_eq!(stats_b.enqueued, 0, "volume b's queue stays empty");

    // Db isolation: the row is visible in A's db, absent from B's.
    assert!(
        handle
            .volume("a")
            .expect("volume a")
            .vfs()
            .expect("vfs a")
            .db()
            .get_file("/file.txt")
            .expect("db read a")
            .is_some(),
        "volume a sees the file row"
    );
    assert!(
        handle
            .volume("b")
            .expect("volume b")
            .vfs()
            .expect("vfs b")
            .db()
            .get_file("/file.txt")
            .expect("db read b")
            .is_none(),
        "volume b must not see volume a's file row"
    );

    // One stop drains A's queue to uploaded through A's transport only.
    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("the aggregated shutdown completes")
        .expect("shutdown joins cleanly");
    let db_a = MetaDatabase::open(&dir.path().join("volumes").join("a").join("cydrive_meta.db"))
        .expect("reopen volume a's db");
    let row = db_a
        .get_file("/file.txt")
        .expect("db read a")
        .expect("row exists");
    assert!(row.is_uploaded, "the drain marked volume a's row uploaded");
    assert!(
        !mock_a.upload_calls().is_empty(),
        "volume a's transport saw the upload job"
    );
    assert!(
        mock_b.upload_calls().is_empty(),
        "volume b's transport must stay untouched"
    );
}

// ------------------------------------------------------- 2. K22 degradation ---

/// One failed volume (its db_path points at the volume toml file itself
/// — SQLite refuses it as not-a-database) degrades visibly: the volume
/// is `Failed{reason}` in the registry, its siblings keep running, and
/// the boot still returns a handle.
#[tokio::test]
async fn one_failed_volume_degrades_while_the_rest_run() {
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
        // The poisoned volume: db_path resolves to the volume toml file
        // itself (relative to the volume home dir).
        "backend = \"telegram\"\nbot_token = \"222:BBB\"\nchat_id = 222222\ndb_path = \"../b.toml\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("c.toml"),
        &volume_toml("333:CCC", 333333),
    );
    let _guard = chdir(dir.path());

    let specs = load_specs();
    let mocks = vec![
        mock_transport().await,
        mock_transport().await,
        mock_transport().await,
    ];
    let handle = boot_multi(specs, mocks).await; // must NOT be an Err

    assert_eq!(
        handle.volume("a").expect("volume a").status(),
        VolumeStatus::Running,
        "volume a still runs"
    );
    assert_eq!(
        handle.volume("c").expect("volume c").status(),
        VolumeStatus::Running,
        "volume c still runs"
    );
    match handle.volume("b").expect("volume b").status() {
        VolumeStatus::Failed { reason } => assert!(
            reason.contains("metadata db"),
            "the failure reason names the db assembly step: {reason}"
        ),
        other => panic!("volume b must be Failed, got {other:?}"),
    }

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("the degraded boot still stops cleanly")
        .expect("shutdown joins cleanly");
}

/// Every volume failing is a process-level error (non-zero exit
/// semantics): the boot returns `Err` naming the volumes.
#[tokio::test]
async fn every_volume_failing_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        "backend = \"telegram\"\nchat_id = 1\ndb_path = \"../a.toml\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("b.toml"),
        "backend = \"telegram\"\nchat_id = 2\ndb_path = \"../b.toml\"\n",
    );
    let _guard = chdir(dir.path());

    let mut injections = Vec::new();
    for spec in load_specs() {
        let mock: Arc<dyn CloudTransport> = mock_transport().await;
        injections.push((spec, RunOptions::default(), mock));
    }
    let error = run_multi_with_transports(&process_config(), injections)
        .await
        .expect_err("all volumes failing must fail the boot");
    let message = format!("{error:#}");
    assert!(
        message.contains("volume"),
        "the error names the volume problem: {message}"
    );
}

// --------------------------------------------------- 3. K25 aggregated stop ---

/// One control-channel STOP stops every volume (K25): the multi-volume
/// boot writes the process-level control file in the cwd, a STOP fires
/// the shared gate (wait_for_stop_request resolves), the aggregated
/// shutdown drains each volume's queue, and the control file is removed
/// by the stop chain.
#[tokio::test]
async fn one_control_stop_stops_every_volume() {
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

    let specs = load_specs();
    let mocks = vec![mock_transport().await, mock_transport().await];
    let handle = boot_multi(specs, mocks).await;

    // The control file is process-level and lives in the cwd (the
    // process config's default db_path anchors it there).
    let control_file = control_file_path(&process_config());
    assert!(
        control_file.exists(),
        "the multi-volume boot writes the process control file {}",
        control_file.display()
    );

    // A queued file on volume a: the stop must drain it (the shutdown
    // semantics carry over per volume).
    let source = dir.path().join("source.txt");
    fs::write(&source, b"drain me").expect("write source");
    let rel = RelPath::new("/drain.txt").expect("valid rel path");
    handle
        .volume("a")
        .expect("volume a")
        .vfs()
        .expect("vfs a")
        .ingest_file(&rel, &source, 1.0)
        .await
        .expect("ingest into volume a");

    let control_addr =
        read_control_addr(&process_config()).expect("the control file parses into an address");
    let reply = send_stop(control_addr)
        .await
        .expect("STOP over the control channel");
    assert!(
        reply.contains("OK: shutting down"),
        "control reply: {reply}"
    );

    // The ONE gate fires for every volume.
    timeout(Duration::from_secs(15), handle.wait_for_stop_request())
        .await
        .expect("the stop gate fires after the control STOP");

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("the aggregated shutdown completes")
        .expect("shutdown joins cleanly");
    assert!(
        !control_file.exists(),
        "the stop chain removes the control file"
    );
    let db_a = MetaDatabase::open(&dir.path().join("volumes").join("a").join("cydrive_meta.db"))
        .expect("reopen volume a's db");
    let row = db_a
        .get_file("/drain.txt")
        .expect("db read a")
        .expect("row exists");
    assert!(row.is_uploaded, "the stop drained volume a's queue");
}

// --------------------------------------- 4. per-volume transport state paths ---

/// The telegram session path follows the injected base directory (the
/// `cwd` parameter IS the per-volume home in multi-volume mode; two
/// different bases yield two different session files).
#[test]
fn telegram_session_path_follows_the_volume_home() {
    let dir_a = tempfile::tempdir().expect("tempdir a");
    let dir_b = tempfile::tempdir().expect("tempdir b");
    let cfg = CyDriveConfig::default();

    let tc_a = transport_config_from(&cfg, dir_a.path());
    let tc_b = transport_config_from(&cfg, dir_b.path());

    assert_eq!(
        tc_a.session_path,
        dir_a.path().join("cynet_bot_session.session"),
        "volume a's session lands in its home dir"
    );
    assert_eq!(
        tc_b.session_path,
        dir_b.path().join("cynet_bot_session.session"),
        "volume b's session lands in its home dir"
    );
    assert_ne!(
        tc_a.session_path, tc_b.session_path,
        "two volumes never share a session file"
    );
}

/// The baidu upload-session directory follows the injected state base
/// (the per-volume home in multi-volume mode; "." keeps the single-volume
/// cwd behaviour).
#[test]
fn baidu_sessions_dir_follows_the_state_base() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = CyDriveConfig::default();

    let params = baidu_params(&cfg, &BaiduEndpoints::default(), None, dir.path());
    assert_eq!(
        params.sessions_dir,
        Some(dir.path().to_path_buf()),
        "the state base becomes the sessions dir"
    );

    let cwd_params = baidu_params(&cfg, &BaiduEndpoints::default(), None, Path::new("."));
    assert_eq!(
        cwd_params.sessions_dir,
        Some(PathBuf::from(".")),
        "the single-volume default stays the cwd"
    );
}
