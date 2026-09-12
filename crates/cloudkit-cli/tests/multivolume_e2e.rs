//! RED-phase tests for Phase 2.5 / MV1: the volume registry assembly.
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
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cloudkit_cli::control::{control_file_path, read_control_addr, send_stop};
// Baidu-gated surface (FT2): the params mapping and the endpoint set
// exist only with the `baidu` feature.
#[cfg(feature = "telegram")]
use cloudkit_cli::transport_config_from;
#[cfg(feature = "baidu")]
use cloudkit_cli::{baidu_params, BaiduEndpoints};
use cloudkit_cli::{run_multi_with_transports, volume_mount_url, RunOptions, VolumeStatus};
use cloudkit_core::config::{load_volumes, CyDriveConfig, VolumeConfig};
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::CloudTransport;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
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
/// The WebDAV port is ephemeral (parallel boots must not contend on the
/// 8080 default) and auto-mount stays OFF so the offline gate never maps
/// a real network drive — same reasons as `run_e2e.rs`'s `temp_config`.
/// `enable_web_ui` is OFF here for the same parallel-port reason (the
/// 8088 default); the MV3 dashboard tests flip it on with an ephemeral
/// port through [`process_config_with_web_ui`].
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
    boot_multi_with(process_config(), specs, mocks).await
}

/// [`boot_multi`] with a caller-owned process config (the MV3 dashboard
/// tests enable the web UI on an ephemeral port; the degraded-bind test
/// points it at a squatted one).
async fn boot_multi_with(
    cfg: CyDriveConfig,
    specs: Vec<VolumeConfig>,
    mocks: Vec<Arc<MockTransport>>,
) -> cloudkit_cli::MultiVolumeHandle {
    assert_eq!(specs.len(), mocks.len(), "one transport per volume");
    let injections = specs
        .into_iter()
        .zip(mocks)
        .map(|(spec, mock)| (spec, RunOptions::default(), mock as Arc<dyn CloudTransport>))
        .collect();
    run_multi_with_transports(&cfg, injections)
        .await
        .expect("multi-volume boot")
}

/// Sends one raw HTTP/1.1 request (`Connection: close`) and reads the
/// response bytes until the server closes the connection (same shape as
/// `run_e2e.rs`'s helper).
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
fn request(method: &str, target: &str, addr: SocketAddr, body: &str) -> String {
    format!(
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\
         Content-Length: {}\r\n\r\n{body}",
        addr.port(),
        body.len()
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
/// the server answered `Transfer-Encoding: chunked` (the `web_e2e.rs`
/// helper).
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

/// Review M4: RV0 made the empty-boot path reachable — every volume
/// file carrying `enabled = false` leaves [`load_volumes`] with an
/// empty set (each skip is only an info line in the log), and the boot
/// must refuse with the disabled-specific message: the volume-file
/// count, the directory, and the two ways out (flip a key back to
/// true, or drop `volumes_dir`). The pre-fix generic "no volumes to
/// assemble" bail named none of that.
#[tokio::test]
async fn every_volume_disabled_is_an_actionable_boot_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        "backend = \"local\"\nlocal_root = \"root-a\"\nenabled = false\n",
    );
    write_file(
        &dir.path().join("volumes").join("b.toml"),
        "backend = \"local\"\nlocal_root = \"root-b\"\nenabled = false\n",
    );
    let _guard = chdir(dir.path());

    let specs = load_specs();
    assert!(
        specs.is_empty(),
        "both disabled volumes are skipped at discovery (the RV0 skip)"
    );

    let error = run_multi_with_transports(&process_config(), Vec::new())
        .await
        .expect_err("a boot with every volume disabled must fail");
    let message = format!("{error:#}");
    assert!(
        message.contains("disabled"),
        "the error names the disabled cause: {message}"
    );
    assert!(
        message.contains("2 volume file"),
        "the error counts the skipped volume files: {message}"
    );
    assert!(
        message.contains("volumes"),
        "the error names the volumes directory: {message}"
    );
    assert!(
        message.contains("single-volume"),
        "the error offers the single-volume way out: {message}"
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
#[cfg(feature = "telegram")]
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
#[cfg(feature = "baidu")]
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

// --------------------------------------------- 5. MV2: single-port WebDAV routing ---

/// The multi-volume boot serves ONE WebDAV port routing `/vol/<name>/`
/// (K20): `handle.webdav_addr()` is bound, a PUT through `/vol/a/...`
/// lands in volume a's db only (prefix stripped, R1), a PUT without a
/// volume segment is a 404 that touches no volume, and no volume
/// claimed a drive letter so nothing mounted (the offline gate never
/// maps a real network drive).
#[tokio::test]
async fn multi_boot_serves_single_port_volume_routing() {
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

    let addr = handle
        .webdav_addr()
        .expect("the multi-volume WebDAV port is bound");
    assert_ne!(addr.port(), 0, ":0 must resolve to the real bound port");

    // PUT through the volume-a prefix lands in volume a only.
    let resp = send(addr, &request("PUT", "/vol/a/x.txt", addr, "via-prefix")).await;
    assert_eq!(status_of(&resp), 201, "PUT created through /vol/a/: {resp}");
    assert!(
        handle
            .volume("a")
            .expect("volume a")
            .vfs()
            .expect("vfs a")
            .db()
            .get_file("/x.txt")
            .expect("db read a")
            .is_some(),
        "volume a sees the prefix-stripped row"
    );
    assert!(
        handle
            .volume("b")
            .expect("volume b")
            .vfs()
            .expect("vfs b")
            .db()
            .get_file("/x.txt")
            .expect("db read b")
            .is_none(),
        "volume b must not see volume a's row"
    );

    // PROPFIND the volume root through the prefix.
    let resp = send(addr, &request("PROPFIND", "/vol/a/", addr, "")).await;
    assert_eq!(status_of(&resp), 207, "volume a root multistatus: {resp}");
    assert!(resp.contains("x.txt"), "volume a lists x.txt: {resp}");

    // No volume segment → 404, and neither db ever sees the row.
    let resp = send(addr, &request("PUT", "/no-prefix.txt", addr, "no-prefix")).await;
    assert_eq!(status_of(&resp), 404, "unrouted PUT must 404: {resp}");
    for name in ["a", "b"] {
        assert!(
            handle
                .volume(name)
                .expect("volume")
                .vfs()
                .expect("vfs")
                .db()
                .get_file("/no-prefix.txt")
                .expect("db read")
                .is_none(),
            "volume {name} must not see the unrouted PUT"
        );
    }

    // No volume claimed a drive letter: nothing mounted.
    assert!(
        handle.mounted_letters().is_empty(),
        "volumes without an explicit drive_letter mount nothing"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// A WebDAV bind failure degrades visibly (K22): the volumes' data
/// planes keep running (`webdav_addr()` is `None`, the boot still
/// returns a handle) — a taken port is not a volume failure.
#[tokio::test]
async fn webdav_bind_failure_degrades_without_failing_volumes() {
    // Occupy a port first, then boot the multi-volume stack against it.
    let squatter = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind squatter");
    let taken = squatter.local_addr().expect("squatter addr");

    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        &format!(
            "volumes_dir = \"volumes\"\nwebdav_port = {}\n",
            taken.port()
        ),
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:AAA", 111111),
    );
    let _guard = chdir(dir.path());

    let mut cfg = process_config();
    cfg.webdav_port = taken.port();

    let specs = load_specs();
    let mock: Arc<dyn CloudTransport> = mock_transport().await;
    let injections = vec![(
        specs.into_iter().next().expect("volume a"),
        RunOptions::default(),
        mock,
    )];
    let handle = run_multi_with_transports(&cfg, injections)
        .await
        .expect("the boot must survive the WebDAV bind failure (K22)");

    assert_eq!(
        handle.volume("a").expect("volume a").status(),
        VolumeStatus::Running,
        "the volume itself is NOT failed by the WebDAV bind error"
    );
    assert_eq!(
        handle.webdav_addr(),
        None,
        "no WebDAV address is reported after the bind failure"
    );
    assert!(
        handle.mounted_letters().is_empty(),
        "nothing to mount without WebDAV"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("the degraded boot still stops cleanly")
        .expect("shutdown joins cleanly");
}

// ------------------------------------------------ 6. MV2: per-volume mount URL ---

/// K27: the per-volume mount URL glues the process WebDAV host/port
/// with the `/vol/<name>` segment (pure URL construction — real mounts
/// are the Windows MiniRedir probe's job, never the offline gate's).
#[test]
fn volume_mount_url_glues_the_per_volume_path() {
    let mut cfg = process_config();
    cfg.webdav_host = "127.0.0.1".to_string();
    cfg.webdav_port = 8080;

    assert_eq!(
        volume_mount_url(&cfg, "tg"),
        "http://127.0.0.1:8080/vol/tg",
        "the mount URL is the WebDAV root plus the volume segment"
    );
    assert_eq!(
        volume_mount_url(&cfg, "baidu"),
        "http://127.0.0.1:8080/vol/baidu",
        "every volume gets its own segment"
    );
}

/// With `auto_mount_drive` off (every offline test config), even a
/// volume with an EXPLICIT drive_letter mounts nothing — the explicit
/// letter is a claim, the process switch is the gate.
#[tokio::test]
async fn explicit_drive_letter_mounts_only_with_the_auto_mount_switch() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        "backend = \"telegram\"\nbot_token = \"111:AAA\"\nchat_id = 111111\ndrive_letter = \"V\"\n",
    );
    let _guard = chdir(dir.path());

    let specs = load_specs();
    assert!(
        specs[0].explicit_drive_letter,
        "the volume file claims a drive letter"
    );
    let handle = boot_multi(specs, vec![mock_transport().await]).await;
    assert!(
        handle.mounted_letters().is_empty(),
        "auto_mount_drive=false gates the mount even for an explicit letter"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// ------------------------------------------- 7. MV3: multi-volume dashboard ---

/// A process config with the dashboard enabled on an ephemeral port
/// (the `run_e2e.rs` `web_ui_enabled_serves_dashboard` convention).
fn process_config_with_web_ui() -> CyDriveConfig {
    CyDriveConfig {
        enable_web_ui: true,
        web_ui_port: 0,
        ..process_config()
    }
}

/// The dashboard port is absent when the web UI is off (the default
/// offline gate).
#[tokio::test]
async fn web_ui_disabled_reports_no_dashboard_address() {
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

    let handle = boot_multi(load_specs(), vec![mock_transport().await]).await;
    assert_eq!(
        handle.web_ui_addr(),
        None,
        "enable_web_ui=false boots no dashboard"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// MV3 / K24: `enable_web_ui` in multi-volume mode boots ONE dashboard
/// port serving the volume registry — `/api/volumes` lists both volumes
/// with their per-volume `/vol/<name>` WebDAV URLs, the volume-scoped
/// APIs route by `?volume=`, and the aggregated shutdown drains the
/// dashboard with everything else.
#[tokio::test]
async fn multi_boot_serves_volume_aware_dashboard() {
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
    let handle = boot_multi_with(
        process_config_with_web_ui(),
        specs,
        vec![mock_transport().await, mock_transport().await],
    )
    .await;

    let addr = handle
        .web_ui_addr()
        .expect("the multi-volume dashboard port is bound");
    assert_ne!(addr.port(), 0, ":0 must resolve to the real bound port");

    // K24: the registry listing — both volumes, per-volume mount URLs.
    let resp = send(addr, &request("GET", "/api/volumes", addr, "")).await;
    assert_eq!(status_of(&resp), 200, "volumes listing: {resp}");
    let list: serde_json::Value =
        serde_json::from_str(body_of(&resp).trim()).expect("parse volumes array");
    let entries = list.as_array().expect("array body");
    assert_eq!(entries.len(), 2, "one entry per volume");
    let a = entries
        .iter()
        .find(|e| e["name"] == "a")
        .expect("volume a entry");
    assert_eq!(a["backend"], "telegram", "the volume file's backend");
    assert_eq!(a["status"], "running", "assembled volume");
    assert!(
        a["webdav_url"]
            .as_str()
            .is_some_and(|url| url.ends_with("/vol/a")),
        "the per-volume mount URL (K27): {a}"
    );

    // K23: a volume-scoped call without the parameter is refused with
    // the actionable volume list; with it, the frozen 16-key shape.
    let resp = send(addr, &request("GET", "/api/stats", addr, "")).await;
    assert_eq!(status_of(&resp), 400, "no default volume: {resp}");
    let body: serde_json::Value =
        serde_json::from_str(body_of(&resp).trim()).expect("parse error body");
    assert_eq!(
        body["volumes"],
        serde_json::json!(["a", "b"]),
        "the body names the addressable volumes: {body}"
    );

    let resp = send(addr, &request("GET", "/api/stats?volume=a", addr, "")).await;
    assert_eq!(status_of(&resp), 200, "volume a stats: {resp}");
    let stats: serde_json::Value =
        serde_json::from_str(body_of(&resp).trim()).expect("parse stats object");
    assert_eq!(stats["backend"], "telegram", "volume a's identity");
    assert!(
        stats["webdav_url"]
            .as_str()
            .is_some_and(|url| url.ends_with("/vol/a")),
        "the stats card carries the per-volume mount URL: {stats}"
    );

    // The summary aggregate answers over both volumes.
    let resp = send(addr, &request("GET", "/api/stats/summary", addr, "")).await;
    assert_eq!(status_of(&resp), 200, "summary: {resp}");
    let summary: serde_json::Value =
        serde_json::from_str(body_of(&resp).trim()).expect("parse summary");
    assert_eq!(summary["volumes"], 2, "both volumes counted");

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// K22 semantics for the dashboard bind: a taken port degrades with an
/// error (the volumes keep running, `web_ui_addr()` reports `None`) —
/// a broken dashboard must not take the data planes down.
#[tokio::test]
async fn web_ui_bind_failure_degrades_without_failing_volumes() {
    // Occupy a port first, then boot the multi-volume stack against it.
    let squatter = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind squatter");
    let taken = squatter.local_addr().expect("squatter addr");

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

    let cfg = process_config_with_web_ui();
    let cfg = CyDriveConfig {
        web_ui_port: taken.port(),
        ..cfg
    };
    let handle = boot_multi_with(cfg, load_specs(), vec![mock_transport().await]).await;

    assert_eq!(
        handle.volume("a").expect("volume a").status(),
        VolumeStatus::Running,
        "the volume is NOT failed by the dashboard bind error"
    );
    assert_eq!(
        handle.web_ui_addr(),
        None,
        "no dashboard address is reported after the bind failure"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("the degraded boot still stops cleanly")
        .expect("shutdown joins cleanly");
}

// ------------------------------------------ 7. RV1: the live registry handle ---

/// RV1 (K51): the boot registry is a live shared handle, not a frozen
/// Vec snapshot — removing a volume from it is immediately visible to
/// the status queries the banner and doctor read (`status_list` /
/// `volume`), while the surviving volume keeps its entry. RV2's ADD/
/// REMOVE control commands own the mutation surface; this pins the seam
/// they act through and the liveness the banner inherits.
#[tokio::test]
async fn registry_removal_is_immediately_visible_to_status_queries() {
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

    let registry = handle.registry();
    assert_eq!(
        handle.volumes().len(),
        2,
        "both volumes are registered at boot"
    );

    // The removal: the K50 seam the runtime unload acts through.
    assert!(
        registry.remove("a"),
        "removing a registered volume reports it"
    );
    assert!(
        !registry.remove("a"),
        "removing an already-removed volume reports the miss"
    );

    assert!(
        handle.volume("a").is_none(),
        "the removed volume is gone from name lookups"
    );
    assert_eq!(
        handle.volumes(),
        vec![("b".to_string(), VolumeStatus::Running)],
        "the status list reflects the removal immediately (the banner's live query)"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}
