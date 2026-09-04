//! Linux WebDAV mount chain — the real implementation, compiled only on
//! `cfg(target_os = "linux")`.
//!
//! Each function mirrors the Linux legs of the Python baseline's
//! `UnixMounter` (`cydrive/platform/linux_mac.py`): gio first (desktop
//! sessions), davfs2 second, the baseline's exact argv and report
//! strings. One deliberate divergence: when *neither* backend is
//! installed the baseline's auto-mount path answered a headless "active
//! on Linux server" success — here [`mount_drive`] returns
//! [`PlatformError::MissingBackend`] instead, so an explicit `cydrive
//! mount` never reports a mount that did not happen; the run flow's
//! auto-mount wiring (status plan C5) treats that error as its
//! warn-and-continue headless mode.
//!
//! macOS is not handled here: `mount_webdav` stays stubbed per the
//! plan's YAGNI list.

use std::path::Path;

use crate::{
    davfs_mount_command, davfs_unmount_command, detect_mount_backend, fusermount_unmount_command,
    gio_mount_command, parse_proc_mounts_davfs, PlatformError,
};

/// Mounts the WebDAV endpoint at `url`, creating `mount_point` first
/// (the baseline's `os.makedirs(..., exist_ok=True)`).
///
/// Order: gio (`which gio`) → davfs2 (`which mount.davfs` — the
/// binary the davfs2 package actually installs, and what the baseline
/// probes). A *failing* gio attempt falls through to davfs2 exactly
/// like the baseline; the last failing command surfaces as
/// [`PlatformError::Command`]. Neither backend installed →
/// [`PlatformError::MissingBackend`] (see the module docs for the
/// divergence from the baseline's headless success).
pub fn mount_drive(mount_point: &Path, url: &str) -> Result<String, PlatformError> {
    std::fs::create_dir_all(mount_point)?;

    let gio = which("gio");
    let davfs2 = which("mount.davfs");
    if detect_mount_backend(gio, davfs2).is_none() {
        return Err(PlatformError::MissingBackend(
            "neither gio nor mount.davfs found on PATH — install davfs2 (or run \
             in a desktop session with gio); the WebDAV server itself keeps \
             running either way"
                .to_string(),
        ));
    }

    // gio first; a *failing* gio attempt falls through to davfs2, exactly
    // like the baseline's sequential tries. The failed gio error is kept:
    // it is what surfaces when nothing else can mount.
    let mut gio_failure: Option<PlatformError> = None;
    if gio {
        match run_argv(&gio_mount_command(url)) {
            Ok(_) => return Ok(format!("Mounted CyDrive via GNOME GIO: {url}")),
            Err(err) => gio_failure = Some(err),
        }
    }

    if davfs2 {
        run_argv(&davfs_mount_command(url, &mount_point.to_string_lossy()))?;
        return Ok(format!(
            "Mounted CyDrive via davfs2 to {}",
            mount_point.display()
        ));
    }

    // Reached when gio was available but failed and davfs2 is absent:
    // surface the actual gio error (the backend gate above rules out the
    // `None` case in practice; the fallback stays defensive, not a lie).
    match gio_failure {
        Some(err) => Err(err),
        None => Err(PlatformError::MissingBackend(
            "no mount backend available".to_string(),
        )),
    }
}

/// Releases whatever is mounted at `mount_point`, mirroring the
/// baseline's chain: a missing path is already "unmounted" (Ok report),
/// FUSE mounts (gio) go through `fusermount -u` best-effort, and plain
/// `umount` handles davfs2 mounts. Success of *either* release counts;
/// only when both fail does the `umount` error surface.
pub fn unmount_drive(mount_point: &Path) -> Result<String, PlatformError> {
    if !mount_point.exists() {
        return Ok("Mount path does not exist.".to_string());
    }

    let path = mount_point.to_string_lossy().into_owned();
    let fuse_released = run_argv(&fusermount_unmount_command(&path)).is_ok();
    match run_argv(&davfs_unmount_command(&path)) {
        Ok(_) => Ok(format!("Unmounted {path}")),
        Err(_) if fuse_released => Ok(format!("Unmounted {path}")),
        Err(err) => Err(err),
    }
}

/// `which <bin>` — the baseline's `shutil.which` availability probe.
fn which(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Releases every davfs mount the current `/proc/mounts` shows for
/// `url` — the run flow's stale cleanup before a fresh auto-mount
/// (status plan C5): a previous run's leftover mount must not keep
/// serving a dead server under the new one. Best-effort by design:
/// every `umount`'s output is swallowed and failures stay silent, which
/// makes repeated calls idempotent — a machine with no matching mounts
/// is a no-op, and a busy mount simply survives for [`unmount_drive`]
/// (or the operator) to release later. Each mount point is attempted at
/// most once per call, so an `umount` that does not stick cannot turn
/// the cleanup into a spin.
pub fn unmount_stale_for(url: &str) {
    let mut attempted: Vec<String> = Vec::new();
    while let Some(point) = std::fs::read_to_string("/proc/mounts")
        .ok()
        .and_then(|mounts| parse_proc_mounts_davfs(&mounts, url))
    {
        if attempted.iter().any(|p| p == &point) {
            break;
        }
        let _ = run_argv(&davfs_unmount_command(&point));
        attempted.push(point);
    }
}

/// Runs one external command to completion. Non-zero exit maps to
/// [`PlatformError::Command`] with stderr, falling back to stdout when
/// stderr is empty (the baseline's `result.stderr or result.stdout`) —
/// the same contract as the Windows module's runner.
fn run_argv(argv: &[String]) -> Result<std::process::Output, PlatformError> {
    let output = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()?;
    if !output.status.success() {
        let message = if output.stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout).into_owned()
        } else {
            String::from_utf8_lossy(&output.stderr).into_owned()
        };
        return Err(PlatformError::Command {
            command: argv.join(" "),
            message,
        });
    }
    Ok(output)
}
