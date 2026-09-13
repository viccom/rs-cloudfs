//! RED-phase tests for the web volume management plan's P2 batch
//! (plan `docs/plans/2026-09-13-web-volume-management.md` §1.3 R1–R6,
//! §3-P2): the control channel's `REBUILD <name>` as a BACKGROUND task
//! — single-flight per volume (R1), the drained-queue gate (R2),
//! immediate acceptance that never parks the command queue (R3), the
//! bounded pass with idempotent abort (R4), the checkpoint that aborts
//! on REMOVE/shutdown (R5), and the synchronous scope gates (R6) —
//! plus the `rebuilding` marker on LIST/CONFIGS and the
//! `cydrive rebuild` forward to a live instance.
//!
//! The rebuild executor runs behind the injection seam
//! ([`cloudkit_cli::RuntimeRebuild`]): fakes park on a watch channel,
//! count their calls, and flag their own cancellation (the R5
//! observable — the supervisor ABORTS the future, so a Drop guard
//! inside it is the deterministic witness).

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cloudkit_cli::control::read_control_addr;
use cloudkit_cli::{
    rebuild_forward_live, run_multi_with_transports_and_commands, MultiVolumeHandle, RebuildTuning,
    RunOptions, RuntimeRebuild, RuntimeVolumeCommands,
};
use cloudkit_core::config::{CyDriveConfig, VolumeConfig};
use cloudkit_core::transport::mock::{MockTransport, UploadAction};
use cloudkit_core::transport::{CloudTransport, StorageError};
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

/// The process-level config for multi-volume mode (the
/// `runtime_volumes.rs` baseline).
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

/// A minimal REBUILDABLE volume file body: the local backend (no
/// telegram shadow-index refusal, no credentials) — the boot injects a
/// mock transport, so the backend key only matters to the REBUILD
/// gates.
fn local_toml() -> String {
    "backend = \"local\"\n".to_string()
}

/// A telegram-flavoured volume file body (the R6 refusal target).
fn telegram_toml() -> String {
    "backend = \"telegram\"\nbot_token = \"111:AAA\"\nchat_id = 111111\n".to_string()
}

/// A pre-connected mock transport.
async fn mock_transport() -> Arc<MockTransport> {
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect mock transport");
    mock
}

/// The boot over the volume files on disk with the injected command
/// extras (the rebuild seam + tuning ride along).
async fn boot(
    cfg: CyDriveConfig,
    commands: RuntimeVolumeCommands,
) -> (MultiVolumeHandle, Vec<VolumeConfig>) {
    let specs =
        cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
    let mut mocks = Vec::new();
    for _ in &specs {
        mocks.push(mock_transport().await);
    }
    let injections = specs
        .clone()
        .into_iter()
        .zip(mocks)
        .map(|(spec, mock)| (spec, RunOptions::default(), mock as Arc<dyn CloudTransport>))
        .collect();
    let handle = run_multi_with_transports_and_commands(&cfg, injections, commands)
        .await
        .expect("multi-volume boot");
    (handle, specs)
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

// ------------------------------------------------- the rebuild seam fake ---

/// The parking rebuild fake's observable state: the call count, the
/// cancellation flag (a Drop guard inside the future sets it — the R5
/// witness), the entered signal (per-name), and the release gate.
struct RebuildProbe {
    calls: AtomicUsize,
    cancelled: Arc<AtomicBool>,
    entered: tokio::sync::mpsc::UnboundedSender<String>,
    go: tokio::sync::watch::Sender<bool>,
}

impl RebuildProbe {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Releases the parked future (and every later one — the watch
    /// stays true, so a re-REBUILD completes immediately).
    fn release(&self) {
        self.go.send_replace(true);
    }
}

/// Builds the injection seam + its probe: every invocation counts,
/// signals its volume name, parks on the watch gate, and flags its own
/// cancellation when the supervisor aborts it. Settled runs answer a
/// small non-empty outcome.
fn rebuild_probe() -> (
    RuntimeRebuild,
    Arc<RebuildProbe>,
    tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    let (entered_tx, entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (go_tx, go_rx) = tokio::sync::watch::channel(false);
    let cancelled = Arc::new(AtomicBool::new(false));
    let probe = Arc::new(RebuildProbe {
        calls: AtomicUsize::new(0),
        cancelled: Arc::clone(&cancelled),
        entered: entered_tx,
        go: go_tx,
    });
    let seam: RuntimeRebuild = {
        let probe = Arc::clone(&probe);
        let go_rx = go_rx;
        Arc::new(move |name: &str, _settings: &CyDriveConfig| {
            let name = name.to_string();
            let probe = Arc::clone(&probe);
            let mut go_rx = go_rx.clone();
            probe.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let _ = probe.entered.send(name.clone());
                /// The cancellation witness: dropping the future (the
                /// supervisor's abort) flips the flag.
                struct CancelOnDrop(Arc<AtomicBool>);
                impl Drop for CancelOnDrop {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::SeqCst);
                    }
                }
                let _witness = CancelOnDrop(Arc::clone(&probe.cancelled));
                while !*go_rx.borrow_and_update() {
                    if go_rx.changed().await.is_err() {
                        break;
                    }
                }
                Ok(cloudkit_core::rebuild::RebuildOutcome { files: 1, dirs: 1 })
            })
        })
    };
    (seam, probe, entered_rx)
}

/// Polls `probe`'s observable until `cond` holds (bounded — a hung
/// assertion surface must fail, not park the suite).
async fn until<F: Fn(&Arc<RebuildProbe>) -> bool>(probe: &Arc<RebuildProbe>, cond: F) -> bool {
    for _ in 0..200 {
        if cond(probe) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cond(probe)
}

/// Polls LIST until the volume's row carries (or loses) the
/// `rebuilding` marker.
async fn list_shows_rebuilding(addr: SocketAddr, name: &str, rebuilding: bool) -> bool {
    for _ in 0..200 {
        let reply = send_cmd(addr, "LIST").await;
        let marked = list_rows(&reply)
            .iter()
            .any(|row| row.starts_with(&format!("{name} ")) && row.ends_with("rebuilding"));
        if marked == rebuilding {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

// ------------------------------------------------- 1. R1 + R3 + the marker ---

/// The background lifecycle: `REBUILD a` answers acceptance
/// IMMEDIATELY (the parked fake proves the handler did not wait — R3),
/// the volume's LIST/CONFIGS rows carry the `rebuilding` marker, a
/// second REBUILD on the same volume is refused with the start time
/// (R1), and once the fake settles the marker falls off and a fresh
/// REBUILD is accepted again (the state machine resets — R1).
#[tokio::test]
async fn rebuild_runs_in_background_marks_list_and_resets_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(&dir.path().join("volumes").join("a.toml"), &local_toml());
    write_file(&dir.path().join("volumes").join("t.toml"), &telegram_toml());
    let _guard = chdir(dir.path());

    let (seam, probe, mut entered) = rebuild_probe();
    let (handle, _specs) = boot(
        process_config(),
        RuntimeVolumeCommands {
            rebuild: Some(seam),
            rebuild_tuning: RebuildTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();

    // Acceptance: immediate, actionably worded, and the background body
    // is already parked inside the fake (the handler returned first).
    let started = std::time::Instant::now();
    let reply = send_cmd(addr, "REBUILD a").await;
    assert!(
        reply.starts_with("OK:")
            && reply.contains("rebuild of `a` started in background")
            && reply.contains("LIST"),
        "the acceptance reply: {reply}"
    );
    let reply2 = send_cmd(addr, "LIST").await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the acceptance did not wait for the rebuild: {:?}",
        started.elapsed()
    );
    let _ = reply2;
    timeout(Duration::from_secs(5), entered.recv())
        .await
        .expect("the background body started")
        .expect("the entered signal carries the volume name");

    // R1: the marker is visible while the fake parks.
    assert!(
        list_shows_rebuilding(addr, "a", true).await,
        "LIST carries the rebuilding marker while the rebuild runs"
    );
    let reply = send_cmd(addr, "CONFIGS").await;
    assert!(
        reply.lines().any(|line| {
            line.starts_with("a local enabled=true running") && line.ends_with("rebuilding")
        }),
        "CONFIGS carries the rebuilding marker: {reply}"
    );
    let reply = send_cmd(addr, "REBUILD a").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("already running"),
        "the single-flight refusal: {reply}"
    );
    assert!(
        reply.contains("started") && reply.contains(':'),
        "the refusal names the start time (HH:MM:SS): {reply}"
    );
    assert!(
        reply.contains("LIST"),
        "the refusal points at LIST: {reply}"
    );

    // Settle: the marker falls off, exactly one call ran.
    probe.release();
    assert!(
        list_shows_rebuilding(addr, "a", false).await,
        "the marker falls off once the rebuild settles"
    );
    assert_eq!(probe.calls(), 1, "exactly one executor call");

    // The state machine reset: a fresh REBUILD is accepted and settles
    // immediately (the watch stays released).
    let reply = send_cmd(addr, "REBUILD a").await;
    assert!(
        reply.starts_with("OK:"),
        "a settled volume accepts a fresh rebuild: {reply}"
    );
    assert!(
        until(&probe, |probe| probe.calls() == 2).await,
        "the second background body ran"
    );
    assert!(
        list_shows_rebuilding(addr, "a", false).await,
        "the marker falls off after the second settle"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// ------------------------------------------------------------- 2. R2 ---

/// The drained-queue gate: a volume with an upload in flight
/// (RateLimited{3600s} — the held-queue device) refuses REBUILD
/// BEFORE acceptance, naming the pending count and the LIST aid; the
/// volume never enters the rebuilding state.
#[tokio::test]
async fn rebuild_refuses_a_volume_with_uploads_in_flight() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("stuck.toml"),
        &local_toml(),
    );
    let _guard = chdir(dir.path());

    // The boot injects the HOLD transport directly (no dispatch): the
    // worker honors the authoritative RateLimited wait, so the enqueued
    // job stays outstanding for the whole test.
    let hold = Arc::new(
        MockTransport::builder()
            .upload_action(UploadAction::Fail {
                error: StorageError::RateLimited {
                    retry_after: Some(Duration::from_secs(3600)),
                },
            })
            .build(),
    );
    hold.connect().await.expect("connect the hold mock");
    let specs =
        cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
    let spec = specs.into_iter().next().expect("the stuck volume spec");
    let handle = run_multi_with_transports_and_commands(
        &process_config(),
        vec![(spec, RunOptions::default(), hold as Arc<dyn CloudTransport>)],
        RuntimeVolumeCommands::default(),
    )
    .await
    .expect("boot with the held volume");
    let addr = control_addr();

    let source = dir.path().join("hold.txt");
    fs::write(&source, b"hold me in flight").expect("write source");
    handle
        .volume("stuck")
        .expect("volume stuck")
        .vfs()
        .expect("vfs stuck")
        .ingest_file(
            &cloudkit_core::rel_path::RelPath::new("/hold.txt").expect("valid rel path"),
            &source,
            1.0,
        )
        .await
        .expect("ingest into stuck");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        list_rows(&reply)
            .iter()
            .any(|row| row.starts_with("stuck running ") && row.contains("pending=1")),
        "the upload is outstanding: {reply}"
    );

    let reply = send_cmd(addr, "REBUILD stuck").await;
    assert!(
        reply.starts_with("ERR:")
            && reply.contains("1 upload(s) in flight")
            && reply.contains("drained queue"),
        "the undrained refusal names the count and the requirement: {reply}"
    );
    assert!(
        reply.contains("LIST"),
        "the refusal points at the LIST pending aid: {reply}"
    );
    assert!(
        !list_rows(&send_cmd(addr, "LIST").await)
            .iter()
            .any(|row| row.ends_with("rebuilding")),
        "a refused rebuild never marks the volume: {reply}"
    );

    // Deliberate leak (the held worker sleeps 3600s; the runtime drop
    // cancels the task).
    drop(handle);
}

// ------------------------------------------------------------- 3. R4 ---

/// The bounded pass: a rebuild that outlives its (injected, tiny)
/// budget is aborted, the state resets (a fresh REBUILD is accepted),
/// and the abort leaves the idempotent-merge world intact — the row
/// marker is gone, the volume still serves.
#[tokio::test]
async fn rebuild_timeout_resets_state_and_is_reacceptable() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(&dir.path().join("volumes").join("a.toml"), &local_toml());
    let _guard = chdir(dir.path());

    let (seam, probe, _entered) = rebuild_probe();
    let (handle, _specs) = boot(
        process_config(),
        RuntimeVolumeCommands {
            rebuild: Some(seam),
            // fast() = 300ms budget / 50ms cadence: the parking fake
            // (never released) must hit the budget, not park forever.
            rebuild_tuning: RebuildTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();

    let reply = send_cmd(addr, "REBUILD a").await;
    assert!(reply.starts_with("OK:"), "accepted: {reply}");
    assert!(
        list_shows_rebuilding(addr, "a", true).await,
        "the marker is up while the fake parks"
    );
    // Never released: the budget must abort it and reset the state.
    assert!(
        list_shows_rebuilding(addr, "a", false).await,
        "the timeout takes the marker down"
    );
    assert_eq!(probe.calls(), 1, "exactly one executor call");

    // Re-acceptable after the timeout.
    let reply = send_cmd(addr, "REBUILD a").await;
    assert!(
        reply.starts_with("OK:"),
        "a timed-out rebuild is re-acceptable: {reply}"
    );
    assert_eq!(probe.calls(), 2, "the second call ran");

    // The second pass also parks (the watch stays closed) — let the
    // shutdown's own checkpoint (R5) clean it up.
    handle.request_stop();
    assert!(
        until(&probe, |probe| probe.cancelled()).await,
        "the shutdown checkpoint aborted the parked body"
    );
    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// ------------------------------------------------------------- 4. R5 ---

/// The REMOVE checkpoint: a background rebuild whose volume is REMOVEd
/// mid-flight self-aborts (the cancellation witness flips, the volume
/// is gone from the registry, no panic) — the removal never waits for
/// the rebuild and the rebuild never outlives its volume.
#[tokio::test]
async fn rebuild_aborts_when_its_volume_is_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(&dir.path().join("volumes").join("a.toml"), &local_toml());
    let _guard = chdir(dir.path());

    let (seam, probe, _entered) = rebuild_probe();
    let (handle, _specs) = boot(
        process_config(),
        RuntimeVolumeCommands {
            rebuild: Some(seam),
            rebuild_tuning: RebuildTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();

    let reply = send_cmd(addr, "REBUILD a").await;
    assert!(reply.starts_with("OK:"), "accepted: {reply}");
    assert!(
        list_shows_rebuilding(addr, "a", true).await,
        "the rebuild is in flight"
    );

    // REMOVE completes without waiting for the rebuild (queue idle, no
    // mount) — then the checkpoint aborts the parked body.
    let reply = send_cmd(addr, "REMOVE a").await;
    assert!(reply.starts_with("OK:"), "REMOVE is not blocked: {reply}");
    assert!(
        until(&probe, |probe| probe.cancelled()).await,
        "the checkpoint aborted the rebuild after the removal"
    );
    assert!(handle.volume("a").is_none(), "the volume is gone");
    assert_eq!(probe.calls(), 1, "no re-invocation after the abort");
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        !reply.contains("rebuilding"),
        "no marker for a removed volume: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// The shutdown checkpoint (the watch arm of R5): a stop-gate fire
/// mid-rebuild aborts the parked body — the same observation point the
/// REMOVE path uses, driven by the production stop trigger.
#[tokio::test]
async fn rebuild_aborts_when_the_shutdown_gate_fires() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(&dir.path().join("volumes").join("a.toml"), &local_toml());
    let _guard = chdir(dir.path());

    let (seam, probe, _entered) = rebuild_probe();
    let (handle, _specs) = boot(
        process_config(),
        RuntimeVolumeCommands {
            rebuild: Some(seam),
            rebuild_tuning: RebuildTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;
    let addr = control_addr();

    let reply = send_cmd(addr, "REBUILD a").await;
    assert!(reply.starts_with("OK:"), "accepted: {reply}");
    assert!(
        list_shows_rebuilding(addr, "a", true).await,
        "the rebuild is in flight"
    );

    handle.request_stop();
    assert!(
        until(&probe, |probe| probe.cancelled()).await,
        "the shutdown checkpoint aborted the parked body"
    );
    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// ------------------------------------------------------------- 5. R6 ---

/// The synchronous scope gates (all BEFORE acceptance, all
/// actionable): an encrypted volume gets the K11 sync guidance
/// verbatim, a telegram volume the shadow-index refusal, an unknown
/// name the LIST pointer, the malformed shapes the usage line — and
/// once the stop gate has fired, REBUILD joins the mutating family's
/// shutdown refusal.
#[tokio::test]
async fn rebuild_scope_gates_refuse_synchronously() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("enc.toml"),
        "backend = \"local\"\nenable_encryption = true\nencryption_password = \"probe-pass\"\n",
    );
    write_file(&dir.path().join("volumes").join("t.toml"), &telegram_toml());
    let _guard = chdir(dir.path());

    let (handle, _specs) = boot(process_config(), RuntimeVolumeCommands::default()).await;
    let addr = control_addr();

    // The encrypted refusal: the K11 text verbatim (sync guidance).
    let reply = send_cmd(addr, "REBUILD enc").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("rebuild refuses encrypted instances"),
        "the K11 refusal text: {reply}"
    );
    assert!(
        reply.contains("`cydrive sync`"),
        "the refusal points at sync: {reply}"
    );
    // The file's encryption is visible on the config face (the web
    // button gating's data source).
    let reply = send_cmd(addr, "CONFIGS").await;
    assert!(
        reply
            .lines()
            .any(|line| line.starts_with("enc local enabled=true running encrypted")),
        "CONFIGS marks the encrypted volume: {reply}"
    );

    // The telegram refusal (the shadow-index wording).
    let reply = send_cmd(addr, "REBUILD t").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("not supported for the telegram backend"),
        "the telegram refusal: {reply}"
    );

    // Unknown name: the LIST pointer.
    let reply = send_cmd(addr, "REBUILD ghost").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("no volume registered"),
        "the unknown-volume refusal: {reply}"
    );

    // Malformed shapes: the usage line teaching REBUILD.
    for line in ["REBUILD", "REBUILD a b"] {
        let reply = send_cmd(addr, line).await;
        assert!(
            reply.starts_with("ERR:") && reply.contains("usage"),
            "{line} gets the usage line: {reply}"
        );
        assert!(
            reply.contains("REBUILD <name>"),
            "the usage line teaches REBUILD: {reply}"
        );
    }

    // Nothing was accepted: no marker anywhere.
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        !reply.contains("rebuilding"),
        "the refusals accepted nothing: {reply}"
    );

    // H1: after the stop gate fires, REBUILD joins the mutating
    // refusal (a shutting-down instance accepts no new rebuilds).
    let reply = send_cmd(addr, "STOP").await;
    assert!(reply.starts_with("OK:"), "STOP acknowledges: {reply}");
    let reply = send_cmd(addr, "REBUILD enc").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("shutting down"),
        "REBUILD after the gate is refused: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// ------------------------------------------------- 6. the CLI forward ---

/// `cydrive rebuild`'s multi-volume forward: with a live instance in
/// the working directory, one `REBUILD <name>` per non-telegram volume
/// rides the control channel (the instance's background task does the
/// work); a telegram volume keeps the offline refusal wording. Without
/// an instance (no control file yet, or a dead one) the forward yields
/// `None` — the caller's offline pass runs unchanged.
#[tokio::test]
async fn rebuild_forward_reports_per_volume_replies() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(&dir.path().join("volumes").join("a.toml"), &local_toml());
    write_file(&dir.path().join("volumes").join("t.toml"), &telegram_toml());
    let _guard = chdir(dir.path());

    let process = process_config();
    let specs =
        cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");

    // Before any boot: no control file — the forward defers to the
    // offline pass (zero drift for the not-running case).
    let forward = rebuild_forward_live(&process, &specs).await.expect("probe");
    assert!(forward.is_none(), "no live instance → the offline path");

    // A dead control file is the same verdict (a stale port file must
    // not wedge the subcommand).
    write_file(&dir.path().join("cydrive.control"), "127.0.0.1:1\n");
    let forward = rebuild_forward_live(&process, &specs)
        .await
        .expect("probe the stale file");
    assert!(
        forward.is_none(),
        "a dead control address → the offline path"
    );
    fs::remove_file(dir.path().join("cydrive.control")).expect("drop the stale file");

    // The live instance: per-volume replies, the telegram refusal
    // wording matching the offline pass.
    let (seam, probe, _entered) = rebuild_probe();
    probe.release(); // settle immediately
    let (handle, _specs) = boot(
        process_config(),
        RuntimeVolumeCommands {
            rebuild: Some(seam),
            rebuild_tuning: RebuildTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;

    let rows = rebuild_forward_live(&process, &specs)
        .await
        .expect("the forward")
        .expect("the live instance answers");
    let row_of = |name: &str| {
        rows.iter()
            .find(|(volume, _)| volume == name)
            .unwrap_or_else(|| panic!("row {name}: {rows:?}"))
            .1
            .clone()
    };
    assert!(
        row_of("a").starts_with("OK: rebuild of `a` started in background"),
        "the local volume's acceptance rides the row: {:?}",
        rows
    );
    assert!(
        row_of("t").starts_with("NOT rebuilt —")
            && row_of("t").contains("not supported for the telegram backend"),
        "the telegram volume keeps the offline refusal wording: {:?}",
        rows
    );
    assert_eq!(rows.len(), 2, "one row per volume: {rows:?}");
    assert!(
        until(&probe, |probe| probe.calls() == 1).await,
        "the instance's background rebuild actually ran"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}
