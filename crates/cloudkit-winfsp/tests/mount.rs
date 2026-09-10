//! Spec tests for the WF4 mount lifecycle (`cloudkit_winfsp::mount`).
//!
//! Two halves, split by what a WinFsp-free machine can prove:
//!
//! - **CI half (this file's non-ignored tests)**: the mount-point
//!   normalizer (drive letters only — `\\?\` device paths and every
//!   non-letter form are refused), the K40 runtime-status classifier
//!   (registry/DLL/load/init failures become an *actionable unavailable
//!   reason*, never a panic — the K40 fallback's whole input), the
//!   readiness/disappearance pollers over a scripted visibility probe,
//!   the refusal order of the mount funnel (bad letter / occupied
//!   letter / unavailable runtime — all three return before any WinFsp
//!   call is attempted) and the `Send` shape the CLI's stop sequence
//!   needs. None of these touch the DLL: they run on a box without
//!   WinFsp installed, which is exactly the CI box.
//! - **real-machine half (`#[ignore]`)**: one actual in-process mount of
//!   a temp Vfs on a free letter (`Q:`), verified from a *separate*
//!   process (`cmd /c dir /b Q:\`), then unmounted and verified gone.
//!   Never runs by default (`cargo test -- --ignored` on a machine with
//!   WinFsp installed and Q: free).
#![cfg(all(windows, feature = "winfsp"))]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::Capabilities;
use cloudkit_core::transport::CloudTransport;
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cloudkit_winfsp::fs::CloudFs;
use cloudkit_winfsp::mount::{
    mount_with, normalize_mount_point, volume_label, wait_for_visibility, winfsp_status,
    winfsp_status_with, DriveRoot, MountError, MountHandle, MountTuning, MountVisibility, Mounted,
    WinFspInstall, WinFspRuntime, WinFspStatus, MAX_VOLUME_LABEL_CHARS,
};

// ------------------------------------------------------------- test seams ---

/// A scripted [`WinFspRuntime`]: every stage's answer is injected and
/// every call is recorded, so the classifier's decision tree (and the
/// mount funnel's refusal order) can be driven without a WinFsp install.
struct FakeRuntime {
    install: Option<WinFspInstall>,
    load: Result<(), String>,
    init: Result<(), String>,
    calls: Mutex<Vec<&'static str>>,
}

impl Default for FakeRuntime {
    /// The bare machine: nothing installed (the CI box's normal state).
    fn default() -> Self {
        Self {
            install: None,
            load: Ok(()),
            init: Ok(()),
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl FakeRuntime {
    /// The happy machine: installed, the DLL loads, `winfsp_init` is Ok.
    fn ready() -> Self {
        let dir = PathBuf::from(r"C:\Program Files (x86)\WinFsp");
        Self {
            install: Some(WinFspInstall {
                dll: dir.join("bin").join("winfsp-x64.dll"),
                install_dir: dir,
            }),
            load: Ok(()),
            init: Ok(()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().expect("call log").clone()
    }
}

impl WinFspRuntime for FakeRuntime {
    fn install(&self) -> Option<WinFspInstall> {
        self.calls.lock().expect("call log").push("install");
        self.install.clone()
    }

    fn load(&self, _dll: &Path) -> Result<(), String> {
        self.calls.lock().expect("call log").push("load");
        self.load.clone()
    }

    fn init(&self) -> Result<(), String> {
        self.calls.lock().expect("call log").push("init");
        self.init.clone()
    }
}

/// A scripted visibility probe: answers from a queue (popping one answer
/// per call) and falls back to `default` once the script is exhausted —
/// "invisible for three polls, then visible" is one constructor call.
struct ScriptedVisibility {
    script: Mutex<VecDeque<bool>>,
    default: bool,
    calls: Mutex<usize>,
}

impl ScriptedVisibility {
    fn new(script: impl IntoIterator<Item = bool>, default: bool) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            default,
            calls: Mutex::new(0),
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().expect("call count")
    }
}

impl MountVisibility for ScriptedVisibility {
    fn visible(&self, _letter: &str) -> bool {
        *self.calls.lock().expect("call count") += 1;
        let mut script = self.script.lock().expect("script");
        script.pop_front().unwrap_or(self.default)
    }
}

// ------------------------------------------------- mount point normalizer ---

#[test]
fn drive_letters_are_canonicalised() {
    for (input, expected) in [("Q", "Q:"), ("q:", "Q:"), ("  Q:  ", "Q:"), ("y", "Y:")] {
        assert_eq!(
            normalize_mount_point(input).expect("a drive letter must normalize"),
            expected,
            "input {input:?}"
        );
    }
}

#[test]
fn device_namespace_paths_are_refused() {
    // K38 / rclone's血泪: the FUSE-compat `\\?\` forms are not mount
    // points this adapter accepts — a drive letter is the only shape.
    for input in [r"\\?\Q:", r"\\.\Q:", r"\\?\Volume{1}\"] {
        let error = normalize_mount_point(input).expect_err("device path must be refused");
        match error {
            MountError::InvalidMountPoint { input: echoed, .. } => {
                assert_eq!(echoed, input, "the refusal echoes the input");
            }
            other => panic!("expected InvalidMountPoint, got {other:?}"),
        }
    }
}

#[test]
fn non_letter_forms_are_refused() {
    for input in ["", "  ", "QQ", "1:", "Q:\\", "C:/temp", "Q:folder", "共享"] {
        match normalize_mount_point(input) {
            Err(MountError::InvalidMountPoint { .. }) => {}
            other => panic!("input {input:?} must be refused, got {other:?}"),
        }
    }
    // The legal letter that looks like the refused shapes above.
    assert_eq!(normalize_mount_point("Q:").expect("Q: is legal"), "Q:");
}

#[test]
fn normalization_is_idempotent() {
    for input in ["q", "Q:", " y "] {
        let once = normalize_mount_point(input).expect("normalizes");
        let twice = normalize_mount_point(&once).expect("normalizes again");
        assert_eq!(once, twice, "input {input:?}");
    }
}

#[test]
fn volume_labels_truncate_to_32_chars_idempotently() {
    assert_eq!(MAX_VOLUME_LABEL_CHARS, 32);
    let long = "v".repeat(40);
    let truncated = volume_label(&long);
    assert_eq!(truncated.chars().count(), 32, "truncated to 32 wide chars");
    assert_eq!(volume_label(&truncated), truncated, "idempotent");
    let short = "tg";
    assert_eq!(volume_label(short), "tg", "short labels pass through");
    // Cuts by char, never by byte: a multi-byte name must stay valid UTF-8.
    let wide = "卷".repeat(40);
    assert_eq!(volume_label(&wide).chars().count(), 32);
}

// ------------------------------------------------------ runtime status (K40) ---

#[test]
fn status_without_an_install_is_actionable_and_total() {
    let runtime = FakeRuntime::default();
    let status = winfsp_status_with(&runtime);
    match status {
        WinFspStatus::Unavailable(reason) => {
            assert!(
                reason.contains("not installed") || reason.contains("InstallDir"),
                "the reason names the missing install, got: {reason}"
            );
            assert!(
                reason.contains("webdav"),
                "the reason must point at the webdav fallback, got: {reason}"
            );
        }
        other => panic!("a machine without WinFsp must classify as unavailable, got {other:?}"),
    }
    assert_eq!(
        runtime.calls(),
        vec!["install"],
        "without an install nothing is loaded or initialized"
    );
}

#[test]
fn status_reports_load_failures_with_the_stage() {
    let mut runtime = FakeRuntime::ready();
    runtime.load = Err("LoadLibraryW failed: 126".to_string());
    match winfsp_status_with(&runtime) {
        WinFspStatus::Unavailable(reason) => {
            assert!(
                reason.contains("LoadLibraryW failed: 126"),
                "the stage's own error is carried verbatim, got: {reason}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert_eq!(runtime.calls(), vec!["install", "load"]);
}

#[test]
fn status_reports_init_failures_with_the_stage() {
    let mut runtime = FakeRuntime::ready();
    runtime.init = Err("winfsp_init failed".to_string());
    match winfsp_status_with(&runtime) {
        WinFspStatus::Unavailable(reason) => {
            assert!(
                reason.contains("winfsp_init failed"),
                "the stage's own error is carried verbatim, got: {reason}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert_eq!(runtime.calls(), vec!["install", "load", "init"]);
}

#[test]
fn status_ready_after_install_load_and_init() {
    let runtime = FakeRuntime::ready();
    match winfsp_status_with(&runtime) {
        WinFspStatus::Ready { install_dir, dll } => {
            assert!(install_dir.ends_with("WinFsp"), "{install_dir:?}");
            assert!(dll.ends_with("winfsp-x64.dll"), "{dll:?}");
        }
        other => panic!("a prepared machine must classify as ready, got {other:?}"),
    }
    assert_eq!(runtime.calls(), vec!["install", "load", "init"]);
}

/// The production probe on *this* machine, whatever that machine is: the
/// call must be total (a status, never a panic) and, when it reports
/// unavailable, the reason must be non-empty and actionable. This is the
/// test that would have caught a `winfsp_init_or_die`-shaped panic (the
/// rclone "DLL missing → crash" lesson) on the CI box.
#[test]
fn production_status_is_total_on_any_machine() {
    match winfsp_status() {
        WinFspStatus::Ready { dll, .. } => {
            assert!(
                std::fs::metadata(&dll).is_ok(),
                "a ready status names a DLL that exists: {dll:?}"
            );
        }
        WinFspStatus::Unavailable(reason) => {
            assert!(!reason.is_empty(), "an unavailable status carries a reason");
        }
    }
}

// -------------------------------------------------------- visibility polls ---

#[test]
fn readiness_poll_succeeds_after_a_slow_appearance() {
    // "invisible, invisible, then visible" — the mount raced the probe,
    // exactly what the 10s tolerance exists for.
    let probe = ScriptedVisibility::new([false, false, true], true);
    assert!(wait_for_visibility(
        &probe,
        "Q:",
        true,
        Duration::from_millis(200),
        Duration::from_millis(1)
    ));
    assert_eq!(
        probe.calls(),
        3,
        "it stops probing the moment it is visible"
    );
}

#[test]
fn readiness_poll_times_out_when_the_mount_never_appears() {
    let probe = ScriptedVisibility::new([], false);
    assert!(!wait_for_visibility(
        &probe,
        "Q:",
        true,
        Duration::from_millis(30),
        Duration::from_millis(5)
    ));
    assert!(
        probe.calls() > 1,
        "the poll retries until the tolerance ends"
    );
}

#[test]
fn disappearance_poll_succeeds_and_times_out() {
    let slow = ScriptedVisibility::new([true, true, false], false);
    assert!(wait_for_visibility(
        &slow,
        "Q:",
        false,
        Duration::from_millis(200),
        Duration::from_millis(1)
    ));
    let stuck = ScriptedVisibility::new([], true);
    assert!(!wait_for_visibility(
        &stuck,
        "Q:",
        false,
        Duration::from_millis(30),
        Duration::from_millis(5)
    ));
}

// --------------------------------------------------------- mount funnel ---

/// The test Vfs (tiny chunks / one worker — the metadata-file harness).
fn test_vfs(rt: &tokio::runtime::Runtime, dir: &Path) -> Arc<Vfs> {
    let db = Arc::new(MetaDatabase::open(&dir.join("meta.db")).expect("open temp db"));
    let cache = CacheManager::new(dir.join("cache"), 1 << 30);
    let mock = Arc::new(
        MockTransport::builder()
            .capabilities(Capabilities {
                range_read: true,
                inbound: true,
                chat: true,
                ..Capabilities::none()
            })
            .build(),
    );
    let _guard = rt.enter();
    rt.block_on(mock.connect()).expect("pre-connect mock");
    let transport: Arc<dyn CloudTransport> = mock;
    Arc::new(Vfs::new(
        db,
        cache,
        transport,
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
        },
    ))
}

#[test]
fn mount_refuses_a_bad_letter_before_touching_winfsp() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("test runtime");
    let dir = tempfile::tempdir().expect("tempdir");
    let vfs = test_vfs(&rt, dir.path());
    let runtime = FakeRuntime::ready();
    let visibility = ScriptedVisibility::new([], false);

    let error = mount_with(
        &runtime,
        Arc::new(visibility),
        MountTuning::fast(),
        vfs,
        rt.handle().clone(),
        r"\\?\Q:",
        "vol",
    )
    .expect_err("a device path must be refused");
    assert!(
        matches!(error, MountError::InvalidMountPoint { .. }),
        "{error:?}"
    );
    assert_eq!(
        runtime.calls(),
        Vec::<&'static str>::new(),
        "the letter is validated before any WinFsp call"
    );
}

#[test]
fn mount_refuses_an_occupied_letter() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("test runtime");
    let dir = tempfile::tempdir().expect("tempdir");
    let vfs = test_vfs(&rt, dir.path());
    let runtime = FakeRuntime::ready();
    // The letter already answers: mounting over it would silently shadow
    // whatever is there (the K22 "visible, never silent" rule).
    let visibility = ScriptedVisibility::new([true], true);

    let error = mount_with(
        &runtime,
        Arc::new(visibility),
        MountTuning::fast(),
        vfs,
        rt.handle().clone(),
        "Q:",
        "vol",
    )
    .expect_err("an occupied letter must be refused");
    match error {
        MountError::LetterInUse { letter } => assert_eq!(letter, "Q:"),
        other => panic!("expected LetterInUse, got {other:?}"),
    }
    assert!(
        runtime.calls().is_empty(),
        "the occupancy check runs before the WinFsp runtime is touched"
    );
}

#[test]
fn mount_reports_an_unavailable_runtime_instead_of_attempting() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("test runtime");
    let dir = tempfile::tempdir().expect("tempdir");
    let vfs = test_vfs(&rt, dir.path());
    let runtime = FakeRuntime::default(); // not installed
    let visibility = ScriptedVisibility::new([], false);

    let error = mount_with(
        &runtime,
        Arc::new(visibility),
        MountTuning::fast(),
        vfs,
        rt.handle().clone(),
        "Q:",
        "vol",
    )
    .expect_err("an unavailable runtime must not be attempted");
    match error {
        MountError::Unavailable(reason) => {
            assert!(!reason.is_empty(), "the reason survives to the caller");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert!(
        !runtime.calls().contains(&"init"),
        "an uninstalled runtime is never initialized"
    );
}

/// The CLI's stop sequence holds the handle across `.await` points, so
/// the handle must be `Send` — which it is, because `FileSystemHost`
/// itself carries an explicit `unsafe impl Send` (winfsp-rs 0.13.1,
/// `src/host/fshost.rs:560`) while having **no** `Sync` impl (see
/// `mount.rs`'s module docs for the measured record).
#[test]
fn mount_handle_is_send_and_sync_enough_for_the_async_stop_sequence() {
    fn assert_send<T: Send>() {}
    assert_send::<MountHandle>();
    assert_send::<Mounted>();
    assert_send::<MountError>();
    // The adapter crosses into the mount thread and is called from the
    // WinFsp dispatcher threads: `Send + Sync` is the hand-off contract.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CloudFs>();
}

/// The production visibility probe answers for a letter that certainly
/// exists (`C:` on a Windows box) and one that cannot (`%` is not a
/// drive) — no mounting involved.
#[test]
fn drive_root_probe_reads_the_filesystem() {
    let probe = DriveRoot;
    assert!(probe.visible("C:"), "C:\\ exists on a Windows box");
    assert!(!probe.visible("X:"), "X:\\ is not expected to exist here");
}

// ------------------------------------------------------- real machine ---

/// The WF4 real-machine acceptance in miniature: a temp Vfs mounted on a
/// free letter is visible to a *separate* process, and gone after the
/// unmount. Ignored by default (needs WinFsp installed and `Q:` free).
#[test]
#[ignore = "real machine: mounts Q: through WinFsp (WinFsp runtime required, Q: must be free)"]
fn real_machine_mount_is_visible_cross_process_and_unmounts() {
    const LETTER: &str = "Q:";
    // Refuse to run if anything already answers on Q: (never mount over
    // a live mapping — the same guard the funnel applies).
    assert!(
        !Path::new("Q:\\").exists(),
        "Q:\\ is occupied; free the letter before running this test"
    );
    match winfsp_status() {
        WinFspStatus::Ready { install_dir, dll } => {
            println!(
                "winfsp ready: install={} dll={}",
                install_dir.display(),
                dll.display()
            );
        }
        WinFspStatus::Unavailable(reason) => panic!("WinFsp unavailable: {reason}"),
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let cache = CacheManager::new(dir.path().join("cache"), 1 << 30);
    let mock = Arc::new(
        MockTransport::builder()
            .capabilities(Capabilities {
                range_read: true,
                inbound: true,
                chat: true,
                ..Capabilities::none()
            })
            .build(),
    );
    let _guard = rt.enter();
    rt.block_on(mock.connect()).expect("pre-connect mock");
    let transport: Arc<dyn CloudTransport> = mock;
    let vfs = Arc::new(Vfs::new(
        db.clone(),
        cache,
        transport,
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
        },
    ));
    let rel = RelPath::new("/hello.txt").expect("rel path");
    db.upsert_file(&FileUpsert {
        rel_path: rel.as_str().to_string(),
        name: rel.name().to_string(),
        parent_dir: "/".to_string(),
        size: 12,
        mtime: 1_700_000_000.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: Some(1),
        is_uploaded: true,
        is_cached: false,
        is_encrypted: false,
        chunk_count: 0,
        mime_type: None,
    })
    .expect("seed row");

    let mut mounted =
        cloudkit_winfsp::mount::mount(vfs.clone(), rt.handle().clone(), LETTER, "wf4")
            .expect("mounting on Q: must succeed");
    println!("mount readiness: {:?}", mounted.readiness);
    assert!(
        mounted.readiness.is_ok(),
        "the mount point must be visible: {:?}",
        mounted.readiness
    );

    // Cross-process view: a fresh cmd.exe, argv entries passed apart (the
    // spike's quoting lesson).
    let out = std::process::Command::new("cmd")
        .arg("/c")
        .args(["dir", "/b", "Q:\\"])
        .output()
        .expect("run cmd /c dir");
    let listing = String::from_utf8_lossy(&out.stdout);
    println!("cmd /c dir /b Q:\\ -> {listing:?}");
    assert!(
        out.status.success(),
        "dir must succeed; stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        listing.contains("hello.txt"),
        "the mounted volume must list the seeded row, got: {listing:?}"
    );

    mounted
        .handle
        .unmount()
        .expect("a clean unmount must succeed");
    assert!(
        !Path::new("Q:\\").exists(),
        "Q:\\ must be gone after the unmount"
    );
}
