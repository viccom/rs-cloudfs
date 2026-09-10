//! WF4: the mount lifecycle — runtime detection (K40's decision input),
//! mount-point normalization, `FileSystemHost` bring-up, the readiness /
//! disappearance polls and the teardown the CLI's stop sequence drives.
//!
//! ## The pieces
//!
//! * [`winfsp_status`] answers "can this process mount through WinFsp?"
//!   from the registry `InstallDir` + the runtime DLL's existence, then a
//!   `LoadLibraryW` of the DLL **by absolute path** (the bare-name load
//!   fails on a stock install — see the spike's finding) and finally
//!   `winfsp::winfsp_init()`. Every failure is a *reason string*, never a
//!   panic and never a process exit: that reason is what the CLI reports
//!   and degrades from (K40's fallback to WebDAV), and it is why the CI
//!   box (no WinFsp at all) can run this code path.
//! * [`normalize_mount_point`] is the only accepted mount-point shape: a
//!   drive letter (`"Q:"`). The FUSE-era `\\?\` device forms are refused
//!   (K38 — this adapter is not the compat layer), as is everything that
//!   is not exactly one ASCII letter, and [`volume_label`] truncates the
//!   volume label to WinFsp's 32 wide chars *explicitly* so what the
//!   banner logs equals what the FSD shows.
//! * [`mount`] / [`mount_with`] bring one volume up and [`MountHandle`]
//!   tears it down (unmount + dispatcher stop + a disappearance poll).
//!
//! ## Threading (the integration-critical fact, measured 2026-09-10)
//!
//! `FileSystemHost` carries an explicit `unsafe impl Send` (winfsp-rs
//! 0.13.1, `src/host/fshost.rs:560`) bounded on `T: Send` and
//! `T::FileContext: Send` — and **no** `Sync` impl (the inner
//! `NonNull<FSP_FILE_SYSTEM>` is `!Sync`; `assert_sync` on the host type
//! is a hard `E0277`). Measured both ways in this batch: the `Send`
//! assertion compiles, the `Sync` one does not.
//!
//! Consequences, which is why this module needs no worker thread and no
//! channels:
//!
//! * the host may be **moved** between threads (created here, parked in
//!   the CLI's process handle, torn down by the stop sequence), so
//!   [`MountHandle`] owns it directly and stays `Send` (pinned by a
//!   test);
//! * it may **not** be shared by reference, so every lifecycle call
//!   takes `&mut self` and nothing hands the host to two owners;
//! * `unmount()` / `stop()` are blocking WinFsp calls and the readiness
//!   poll sleeps — the CLI therefore runs mount *and* teardown on the
//!   blocking pool (`spawn_blocking`), never on a runtime worker.
//!
//! ## The readiness verdict travels *with* the handle
//!
//! [`mount`] returns `Err` only when nothing ended up mounted (runtime
//! unavailable, the letter is taken, the host could not be created, the
//! mount point could not be claimed, the dispatcher refused to start —
//! the last one rolls its claimed mount point back, because nothing is
//! being served). A mount that started but whose drive letter did not
//! *appear* inside the tolerance is **not** an `Err`: the mount is left
//! in place and the verdict comes back as [`Mounted::readiness`], the
//! caller keeps the handle and reports it. Rationale: the host is
//! already serving — WinFsp's own mount+start are synchronous and
//! sub-millisecond (spike measurement), so a missing drive letter is a
//! *probe* problem (the letter is shadowed, the shell is slow, an
//! antivirus holds the device), and tearing down a working mount on a
//! probe timeout would destroy the thing the user asked for. A late
//! arrival keeps working; a broken one is visible on the banner and the
//! stop sequence still releases it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cloudkit_core::vfs::Vfs;
use winfsp::host::{FileSystemHost, FineGuard, VolumeParams};

use crate::fs::CloudFs;

/// How long the drive letter may take to appear after `start()` (and to
/// disappear after `unmount`). rclone polls mount/unmount readiness with
/// 10s windows for exactly this race (research report §三-生命周期); the
/// spike measured the real appearance at well under a millisecond, so
/// this is a tolerance, not an expectation.
pub const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Polling cadence inside the tolerances above (25ms — rclone's order).
pub const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// WinFsp truncates a volume label to 32 wide chars (`set_volume_label`
/// does it inside the DLL); we truncate explicitly so the label the log
/// and the banner name is the label the FSD shows.
pub const MAX_VOLUME_LABEL_CHARS: usize = 32;

/// The filesystem name Explorer and `fsutil` show for the volume.
pub const FILESYSTEM_NAME: &str = "cydrive";

/// Volume serial number: `"CYDR"` as a u32 — a stable, recognisable
/// marker (the spike's fixed value was arbitrary; uniqueness matters only
/// between simultaneously mounted volumes, and each mount has its own
/// letter).
const VOLUME_SERIAL: u32 = 0x4359_4452;

// ------------------------------------------------------------- errors ---

/// Everything [`mount`] and [`MountHandle::unmount`] can refuse with. The
/// messages are user-facing: each one names what failed and what the
/// operator can do about it (K40's "visible, never silent").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountError {
    /// The mount point is not a single drive letter.
    InvalidMountPoint {
        /// The refused input, echoed back.
        input: String,
        /// Why it was refused (one sentence, actionable).
        reason: &'static str,
    },
    /// Something already answers on that drive letter.
    LetterInUse {
        /// The canonical letter (`"Q:"`).
        letter: String,
    },
    /// The WinFsp runtime is missing or unusable; the string is
    /// [`winfsp_status`]'s reason.
    Unavailable(String),
    /// WinFsp refused to create/mount/start the filesystem.
    Host(String),
    /// The mount point did not appear inside the tolerance (the mount is
    /// deliberately left in place — see the module docs).
    NotVisible {
        /// The canonical letter.
        letter: String,
        /// The tolerance that expired.
        timeout: Duration,
    },
    /// The mount point was still visible after a successful teardown (a
    /// process may hold an open handle on the volume).
    StillVisible {
        /// The canonical letter.
        letter: String,
        /// The tolerance that expired.
        timeout: Duration,
    },
}

impl std::fmt::Display for MountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MountError::InvalidMountPoint { input, reason } => {
                write!(f, "invalid mount point {input:?}: {reason}")
            }
            MountError::LetterInUse { letter } => write!(
                f,
                "drive letter {letter} is already in use; refusing to mount over it — pick a \
                 free letter or release the existing mapping first"
            ),
            MountError::Unavailable(reason) => write!(f, "WinFsp is unusable: {reason}"),
            MountError::Host(reason) => write!(f, "{reason}"),
            MountError::NotVisible { letter, timeout } => write!(
                f,
                "the {letter} drive did not appear within {timeout:?}; the mount was left in \
                 place (a late arrival still works) — check whether the letter is already \
                 mapped, and the WinFsp/Windows event log"
            ),
            MountError::StillVisible { letter, timeout } => write!(
                f,
                "the {letter} drive is still visible {timeout:?} after unmounting; a process \
                 may hold an open handle on it — close the programs using that drive"
            ),
        }
    }
}

impl std::error::Error for MountError {}

// -------------------------------------------------- runtime detection ---

/// A located WinFsp installation: the registry `InstallDir` plus the
/// runtime DLL inside it. The DLL is what gets preloaded — a bare-name
/// `LoadLibraryW("winfsp-x64.dll")` fails on a stock install (the DLL
/// lives in `<InstallDir>\bin`, which is not on the DLL search path); the
/// spike proved the absolute-path preload is the load-bearing step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinFspInstall {
    /// `HKLM\SOFTWARE\WOW6432Node\WinFsp` (or the native key) `InstallDir`.
    pub install_dir: PathBuf,
    /// `<install_dir>\bin\winfsp-x64.dll`.
    pub dll: PathBuf,
}

/// The K40 decision input: can this process mount through WinFsp *now*?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WinFspStatus {
    /// Installed, loaded and initialized — a mount may be attempted.
    Ready {
        /// The installation directory found in the registry.
        install_dir: PathBuf,
        /// The preloaded runtime DLL.
        dll: PathBuf,
    },
    /// Not usable; the reason is actionable (it is what the banner and the
    /// `tracing::error!` line carry before the WebDAV fallback runs).
    Unavailable(String),
}

impl WinFspStatus {
    /// `true` for [`WinFspStatus::Ready`].
    pub fn is_ready(&self) -> bool {
        matches!(self, WinFspStatus::Ready { .. })
    }

    /// The unavailability reason, `None` when ready.
    pub fn reason(&self) -> Option<&str> {
        match self {
            WinFspStatus::Ready { .. } => None,
            WinFspStatus::Unavailable(reason) => Some(reason),
        }
    }
}

/// The three side-effecting steps of the detection, behind a trait so the
/// classifier is testable on a machine without WinFsp (the CI box) — the
/// tests script install/load/init outcomes and pin the resulting reason.
pub trait WinFspRuntime: Send + Sync {
    /// Registry `InstallDir` + the runtime DLL's existence (pure reads).
    fn install(&self) -> Option<WinFspInstall>;
    /// `LoadLibraryW` of the absolute DLL path.
    fn load(&self, dll: &Path) -> Result<(), String>;
    /// `winfsp::winfsp_init()`.
    fn init(&self) -> Result<(), String>;
}

/// The real machine (Windows registry + the WinFsp DLL).
pub struct SystemWinFsp;

/// Registry subkeys, in probe order: the 32-bit installer view
/// (`WOW6432Node`) first, then the native one (ARM64 installers write
/// there; the winfsp-rs `system` feature probes in the same order).
const WINFSP_REG_KEYS: [&str; 2] = [r"SOFTWARE\WOW6432Node\WinFsp", r"SOFTWARE\WinFsp"];

/// The x64 runtime DLL name. The workspace ships x86_64 Windows only; the
/// other architectures' names are `winfsp-x86.dll` / `winfsp-a64.dll`.
const WINFSP_DLL: &str = "winfsp-x64.dll";

impl SystemWinFsp {
    /// Registry `InstallDir` → `<dir>\bin\winfsp-x64.dll`, `None` when the
    /// key or the DLL is missing (both mean "not installed", which is the
    /// CI box's normal state).
    fn locate() -> Option<WinFspInstall> {
        use windows::core::w;
        use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

        for subkey in WINFSP_REG_KEYS {
            let key: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
            let mut buf = [0u16; 260];
            let mut size = (buf.len() * std::mem::size_of::<u16>()) as u32;
            let status = unsafe {
                RegGetValueW(
                    HKEY_LOCAL_MACHINE,
                    windows::core::PCWSTR(key.as_ptr()),
                    w!("InstallDir"),
                    RRF_RT_REG_SZ,
                    None,
                    Some(buf.as_mut_ptr().cast()),
                    Some(&mut size),
                )
            };
            if status.is_err() {
                continue;
            }
            // `size` counts bytes including the NUL terminator.
            let len = (size as usize / std::mem::size_of::<u16>()).saturating_sub(1);
            let dir = PathBuf::from(String::from_utf16_lossy(&buf[..len]));
            let dll = dir.join("bin").join(WINFSP_DLL);
            if dll.exists() {
                return Some(WinFspInstall {
                    install_dir: dir,
                    dll,
                });
            }
        }
        None
    }

    /// One full probe: locate → preload → init.
    fn probe() -> Result<WinFspInstall, String> {
        let Some(install) = Self::locate() else {
            return Err(format!(
                "WinFsp is not installed (no `InstallDir` with a {WINFSP_DLL} under {}) — \
                 install it from https://winfsp.dev/install/ to use `mount_backend = \
                 \"winfsp\"`; the webdav mount backend needs nothing installed",
                WINFSP_REG_KEYS.join(" or ")
            ));
        };
        let machine = SystemWinFsp;
        machine.load(&install.dll)?;
        machine.init()?;
        Ok(install)
    }
}

impl WinFspRuntime for SystemWinFsp {
    fn install(&self) -> Option<WinFspInstall> {
        Self::locate()
    }

    fn load(&self, dll: &Path) -> Result<(), String> {
        use windows::core::HSTRING;
        use windows::Win32::System::LibraryLoader::LoadLibraryW;

        let wide = HSTRING::from(dll.as_os_str());
        unsafe { LoadLibraryW(&wide) }
            .map(|_module| ())
            .map_err(|error| {
                format!(
                    "preloading the WinFsp runtime failed: LoadLibraryW({}) → {error} — \
                     reinstall WinFsp from https://winfsp.dev/install/",
                    dll.display()
                )
            })
    }

    fn init(&self) -> Result<(), String> {
        winfsp::winfsp_init()
            .map(|_token| ())
            .map_err(|error| format!("winfsp_init() failed: {error}"))
    }
}

/// The process-level one-shot: the probe runs once and its verdict is
/// cached (mounting is a process-lifetime decision — an operator who
/// installs WinFsp mid-process restarts the process). Deliberately the
/// *only* cached path: the injectable [`winfsp_status_with`] stays
/// uncached for tests.
static INIT: OnceLock<Result<WinFspInstall, String>> = OnceLock::new();

/// The K40 gate for the real machine: registry + DLL + preload + init,
/// probed once per process (see [`INIT`]). Never panics, never exits the
/// process — a machine without WinFsp gets an actionable
/// [`WinFspStatus::Unavailable`] and the caller falls back to WebDAV.
pub fn winfsp_status() -> WinFspStatus {
    match INIT.get_or_init(SystemWinFsp::probe) {
        Ok(install) => WinFspStatus::Ready {
            install_dir: install.install_dir.clone(),
            dll: install.dll.clone(),
        },
        Err(reason) => WinFspStatus::Unavailable(reason.clone()),
    }
}

/// [`winfsp_status`] over an injected runtime (the test seam — scripted
/// install/load/init outcomes pin the classifier and its reasons).
pub fn winfsp_status_with(runtime: &dyn WinFspRuntime) -> WinFspStatus {
    let Some(install) = runtime.install() else {
        return WinFspStatus::Unavailable(format!(
            "WinFsp is not installed (no `InstallDir` with a {WINFSP_DLL} under {}) — \
             install it from https://winfsp.dev/install/ to use `mount_backend = \"winfsp\"`; \
             the webdav mount backend needs nothing installed",
            WINFSP_REG_KEYS.join(" or ")
        ));
    };
    if let Err(reason) = runtime.load(&install.dll) {
        return WinFspStatus::Unavailable(reason);
    }
    if let Err(reason) = runtime.init() {
        return WinFspStatus::Unavailable(reason);
    }
    WinFspStatus::Ready {
        install_dir: install.install_dir,
        dll: install.dll,
    }
}

// ------------------------------------------------- mount-point normalizer ---

/// `"q"`, `"q:"` and `" Q: "` all become `Some("Q:")`; everything else is
/// refused. This is `cloudkit_platform`'s lenient canonicalisation
/// (`canonical_letter` + `normalize_drive_letter`) with the mount-point
/// additions WinFsp needs, kept *here* on purpose: `cloudkit-winfsp` and
/// `cloudkit-platform` are both L5 and same-layer crates must not depend
/// on each other (architecture §1), so the twelve-line canonicalisation is
/// duplicated rather than reached across the layer.
fn canonical_letter(input: &str) -> String {
    let trimmed = input.trim().to_uppercase();
    if trimmed.ends_with(':') {
        trimmed
    } else {
        format!("{trimmed}:")
    }
}

/// The one mount-point shape this adapter accepts: a Windows drive letter
/// (K38 — no `\\?\` device paths, which the FUSE compatibility layer
/// needed and the native API does not).
///
/// Refused with an actionable reason: the `\\?\` / `\\.\` forms, UNC
/// paths, directory paths, multi-character letters, digits, and the empty
/// string.
pub fn normalize_mount_point(input: &str) -> Result<String, MountError> {
    let trimmed = input.trim();
    if trimmed.starts_with(r"\\") {
        return Err(MountError::InvalidMountPoint {
            input: input.to_string(),
            reason: "device-namespace paths (\\\\?\\ / \\\\.\\) are not mount points here — \
                     pass a drive letter such as \"Q:\" (K38: the native adapter mounts drive \
                     letters only)",
        });
    }
    let canonical = canonical_letter(trimmed);
    // `canonical_letter` guarantees a trailing ASCII colon; the core is
    // exactly one ASCII alphabetic character for a drive letter.
    let core = &canonical[..canonical.len() - 1];
    let mut chars = core.chars();
    match (chars.next(), chars.next()) {
        (Some(letter), None) if letter.is_ascii_alphabetic() => Ok(canonical),
        _ => Err(MountError::InvalidMountPoint {
            input: input.to_string(),
            reason: "a Windows drive-letter mount needs exactly one ASCII letter with an \
                     optional trailing ':' — e.g. \"Q:\"",
        }),
    }
}

/// The volume label handed to WinFsp: truncated to
/// [`MAX_VOLUME_LABEL_CHARS`] wide chars (char-based, so a multi-byte name
/// cannot be cut into invalid UTF-8), idempotent.
pub fn volume_label(label: &str) -> String {
    label.chars().take(MAX_VOLUME_LABEL_CHARS).collect()
}

// --------------------------------------------------------- visibility ---

/// "Does this drive letter answer?" — the probe behind the readiness and
/// disappearance polls, injected so both are testable without mounting
/// anything.
pub trait MountVisibility: Send + Sync {
    /// `true` when the drive letter exists as far as the OS is concerned.
    fn visible(&self, letter: &str) -> bool;
}

/// The production probe: `<letter>\` exists. This is the same signal
/// `cloudkit_platform::used_drive_letters` reads (it probes `A:\`…`Z:\`
/// the same way) — a second, independent enumeration would answer
/// identically, so the single path probe is the honest "dual-path" here.
pub struct DriveRoot;

impl MountVisibility for DriveRoot {
    fn visible(&self, letter: &str) -> bool {
        Path::new(&format!("{letter}\\")).exists()
    }
}

/// Polls `probe` until it answers `expect_visible`, at most for
/// `timeout` with `interval` sleeps between probes. Returns `true` when
/// the expected state was observed (the first probe counts: an
/// already-visible letter needs no wait).
pub fn wait_for_visibility(
    probe: &dyn MountVisibility,
    letter: &str,
    expect_visible: bool,
    timeout: Duration,
    interval: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if probe.visible(letter) == expect_visible {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(interval);
    }
}

// --------------------------------------------------------------- mount ---

/// The two tolerance knobs a mount carries (production:
/// [`MountTuning::default`]; tests shrink them so "inside the window" and
/// "after it" cost milliseconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountTuning {
    /// Readiness and disappearance tolerance.
    pub timeout: Duration,
    /// Cadence inside the tolerance.
    pub interval: Duration,
}

impl Default for MountTuning {
    fn default() -> Self {
        Self {
            timeout: READY_TIMEOUT,
            interval: POLL_INTERVAL,
        }
    }
}

impl MountTuning {
    /// Millisecond-scale windows for tests and for a caller that wants a
    /// loud, fast failure (never production).
    pub fn fast() -> Self {
        Self {
            timeout: Duration::from_millis(50),
            interval: Duration::from_millis(1),
        }
    }
}

/// A live mount: the WinFsp host plus what the polls need.
///
/// `Send` but not `Sync` (the host's own auto-trait shape) — the CLI
/// parks it in the process handle and the stop sequence takes `&mut self`
/// to release it. Dropping the handle is a best-effort teardown
/// ([`MountHandle::unmount`]); the WinFsp host's own `Drop` is the
/// net below that (it deletes the filesystem), so a handle that was never
/// explicitly unmounted still leaves no WinFsp object behind.
pub struct MountHandle {
    /// `None` once unmounted — the idempotence flag and the host at once.
    host: Option<FileSystemHost<CloudFs, FineGuard>>,
    /// The canonical drive letter (`"Q:"`).
    mount_point: String,
    /// The truncated volume label (what the FSD shows).
    label: String,
    /// The drive-letter probe (readiness, disappearance, occupancy).
    visibility: Arc<dyn MountVisibility>,
    /// The tolerances this mount was created with.
    tuning: MountTuning,
}

impl MountHandle {
    /// The canonical mount point (`"Q:"`).
    pub fn mount_point(&self) -> &str {
        &self.mount_point
    }

    /// The volume label as handed to WinFsp (already truncated).
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether this handle still owns a live mount.
    pub fn is_mounted(&self) -> bool {
        self.host.is_some()
    }

    /// Releases the mount: `unmount()` (the drive letter goes away) +
    /// `stop()` (the dispatcher drains) + the host drop, then polls for
    /// the letter's disappearance for up to `tuning.timeout`.
    ///
    /// Idempotent: a second call (or the [`Drop`] after an explicit call)
    /// is a no-op. A letter that stays visible after a successful
    /// teardown is reported as [`MountError::StillVisible`] — the mount
    /// itself is gone, some process is holding a handle on it (rclone's
    /// "unmount has no force" reality), and the caller's job is to say so
    /// rather than retry.
    pub fn unmount(&mut self) -> Result<(), MountError> {
        let Some(mut host) = self.host.take() else {
            return Ok(());
        };
        host.unmount();
        host.stop();
        // The host's Drop deletes the FSP_FILE_SYSTEM; keeping it inside
        // this scope means the WinFsp objects are gone by the time the
        // poll starts, so a still-visible letter really is somebody
        // else's handle and not our teardown lagging.
        drop(host);
        if !wait_for_visibility(
            self.visibility.as_ref(),
            &self.mount_point,
            false,
            self.tuning.timeout,
            self.tuning.interval,
        ) {
            return Err(MountError::StillVisible {
                letter: self.mount_point.clone(),
                timeout: self.tuning.timeout,
            });
        }
        Ok(())
    }
}

impl Drop for MountHandle {
    fn drop(&mut self) {
        // Best-effort: a teardown failure at drop time has no caller to
        // report to (the process is usually exiting), and the reason is
        // already logged by whoever owned the handle.
        let _ = self.unmount();
    }
}

impl std::fmt::Debug for MountHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountHandle")
            .field("mount_point", &self.mount_point)
            .field("label", &self.label)
            .field("mounted", &self.is_mounted())
            .finish()
    }
}

/// The product of a successful bring-up: the live handle plus the
/// readiness verdict (see the module docs — a mount that started but has
/// not appeared yet is *not* an error, it is this `Err`).
#[derive(Debug)]
pub struct Mounted {
    /// The live mount.
    pub handle: MountHandle,
    /// `Ok` when the drive letter answered inside the tolerance; `Err`
    /// ([`MountError::NotVisible`]) when it did not — the mount is left
    /// running either way.
    pub readiness: Result<(), MountError>,
}

/// Brings one volume up on `letter` through WinFsp (the production
/// entry, real registry + DLL + a [`DriveRoot`] probe).
///
/// Blocking by design: WinFsp's bring-up and the polls are synchronous
/// (the CLI calls this from a blocking task, see the module docs).
pub fn mount(
    vfs: Arc<Vfs>,
    rt: tokio::runtime::Handle,
    letter: &str,
    label: &str,
) -> Result<Mounted, MountError> {
    mount_with(
        &SystemWinFsp,
        Arc::new(DriveRoot),
        MountTuning::default(),
        vfs,
        rt,
        letter,
        label,
    )
}

/// [`mount`] with the runtime, the visibility probe and the tolerances
/// injected (the test seam: the refusal order and the polls are pinned
/// without a WinFsp install, and the real-machine test drives the same
/// path with the production probes).
///
/// Refusal order — cheapest and least side-effecting first, so nothing
/// external is touched before the request is known to be sane:
/// normalise the mount point → refuse an occupied letter → ask the
/// runtime (which is what loads the DLL).
pub fn mount_with(
    runtime: &dyn WinFspRuntime,
    visibility: Arc<dyn MountVisibility>,
    tuning: MountTuning,
    vfs: Arc<Vfs>,
    rt: tokio::runtime::Handle,
    letter: &str,
    label: &str,
) -> Result<Mounted, MountError> {
    let mount_point = normalize_mount_point(letter)?;
    // Mounting over a letter something already answers on would shadow
    // it (WinFsp's drive-letter mounts take the name) — refuse loudly
    // instead. A leftover mapping from a crashed process shows up here
    // too, which is the honest answer: release it first.
    if visibility.visible(&mount_point) {
        return Err(MountError::LetterInUse {
            letter: mount_point,
        });
    }
    let status = winfsp_status_with(runtime);
    let (install_dir, dll) = match status {
        WinFspStatus::Ready { install_dir, dll } => (install_dir, dll),
        WinFspStatus::Unavailable(reason) => return Err(MountError::Unavailable(reason)),
    };
    tracing::info!(
        mount_point = %mount_point,
        install_dir = %install_dir.display(),
        dll = %dll.display(),
        "winfsp runtime ready; mounting the volume"
    );

    let label = volume_label(label);
    // The volume label is the assembly-time snapshot CloudFs answers
    // `get_volume_info` from (K44) — the same string the handle reports.
    let fs = CloudFs::new(vfs, rt, label.clone());
    let mut host = FileSystemHost::<CloudFs, FineGuard>::new(volume_params(), fs)
        .map_err(|error| MountError::Host(format!("creating the WinFsp host failed: {error}")))?;
    if let Err(error) = host.mount(&mount_point) {
        return Err(MountError::Host(format!(
            "claiming the mount point {mount_point} failed: {error} — is the letter in use?"
        )));
    }
    if let Err(error) = host.start() {
        // The mount point was claimed but the dispatcher never came up:
        // nothing is served, so roll the claim back (the one place where
        // rolling back IS the right answer — there is no mount to keep).
        host.unmount();
        return Err(MountError::Host(format!(
            "starting the WinFsp dispatcher failed: {error}"
        )));
    }

    let handle = MountHandle {
        host: Some(host),
        mount_point: mount_point.clone(),
        label,
        visibility,
        tuning,
    };
    let readiness = if wait_for_visibility(
        handle.visibility.as_ref(),
        &mount_point,
        true,
        tuning.timeout,
        tuning.interval,
    ) {
        tracing::info!(mount_point = %mount_point, "winfsp mount point is visible");
        Ok(())
    } else {
        Err(MountError::NotVisible {
            letter: mount_point,
            timeout: tuning.timeout,
        })
    };
    Ok(Mounted { handle, readiness })
}

/// The volume parameters every CyDrive mount uses — the spike's proven
/// set (K39/K44): case-preserving names, wide-char on-disk names, a fixed
/// 512-byte sector with one-sector allocation units (NTFS-shaped, which
/// is what the shell and copy engines expect), 255-char components and a
/// stable creation timestamp.
///
/// `case_sensitive_search` is deliberately left at WinFsp's default
/// (false): the FSD performs case-insensitive name lookups itself, which
/// is what Explorer and Windows applications assume — the adapter's own
/// paths stay case-preserved.
fn volume_params() -> VolumeParams {
    let mut params = VolumeParams::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0);
    params
        .filesystem_name(FILESYSTEM_NAME)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .sector_size(512)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .volume_serial_number(VOLUME_SERIAL)
        .volume_creation_time(crate::fs::unix_to_filetime(now));
    params
}
