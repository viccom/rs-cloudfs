//! Windows WebDAV drive mapping and WebClient tuning — the real
//! implementation, compiled only on `cfg(windows)`.
//!
//! Each function mirrors one `WindowsMounter` classmethod from the Python
//! baseline (`cydrive/platform/windows.py`): ensure the WebClient service,
//! probe used letters, pick a drive letter, clear stale mappings, map and
//! release with `net use`, and tune the WebClient registry for 4 GiB
//! transfers plus basic auth.

use std::path::Path;
use std::process::Output;

use crate::{
    canonical_letter, mount_command, pick_drive_letter, unmount_command, PlatformError,
    WinFspInstall, BASIC_AUTH_LEVEL, FILE_SIZE_LIMIT_BYTES, WEBCLIENT_REG_PATH,
};

/// Occupied drive letters, probed as `A:\` … `Z:\` root-path existence.
///
/// Equivalent to the Win32 `GetLogicalDrives` bitmask the Python baseline
/// read through `ctypes.windll.kernel32`: a letter counts as used exactly
/// when its root path exists. Probing with `std::fs` avoids pulling in a
/// `windows-sys` dependency for a single API call whose result is fully
/// derivable from the filesystem.
pub fn used_drive_letters() -> Vec<String> {
    (b'A'..=b'Z')
        .map(|byte| format!("{}:", char::from(byte)))
        .filter(|letter| Path::new(&format!("{letter}\\")).exists())
        .collect()
}

/// Ensures the Windows WebClient service (the WebDAV client `net use`
/// relies on) is running: `sc query webclient` shows no `RUNNING` →
/// `sc start webclient`. A failing query or start surfaces as
/// [`PlatformError::Command`].
pub fn ensure_webclient_service() -> Result<(), PlatformError> {
    let query = run_argv(&["sc", "query", "webclient"])?;
    if String::from_utf8_lossy(&query.stdout).contains("RUNNING") {
        return Ok(());
    }
    run_argv(&["sc", "start", "webclient"])?;
    Ok(())
}

/// Tunes the WebClient registry for 4 GiB WebDAV transfers and basic
/// auth: writes `FileSizeLimitInBytes` = [`FILE_SIZE_LIMIT_BYTES`] and
/// `BasicAuthLevel` = [`BASIC_AUTH_LEVEL`] as DWORDs under
/// `HKLM\{WEBCLIENT_REG_PATH}`, then restarts the service so the limits
/// take effect (`net stop` failing — e.g. the service already stopped —
/// is ignored, mirroring the baseline; a failing `net start` is an error).
///
/// Opening the key without administrator rights maps to a clear
/// [`PlatformError::Registry`] message asking for an elevated terminal.
pub fn optimize_webdav_registry() -> Result<(), PlatformError> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE};

    let hklm = winreg::RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm
        .open_subkey_with_flags(WEBCLIENT_REG_PATH, KEY_READ | KEY_SET_VALUE)
        .map_err(|e| registry_error("opening WebClient Parameters", &e))?;
    key.set_value("FileSizeLimitInBytes", &FILE_SIZE_LIMIT_BYTES)
        .map_err(|e| registry_error("writing FileSizeLimitInBytes", &e))?;
    key.set_value("BasicAuthLevel", &BASIC_AUTH_LEVEL)
        .map_err(|e| registry_error("writing BasicAuthLevel", &e))?;
    drop(key); // release the handle before restarting the service

    let _ = run_argv(&["net", "stop", "webclient"]); // already-stopped is fine
    run_argv(&["net", "start", "webclient"])?;
    Ok(())
}

/// Reads the two WebClient tuning values this crate writes
/// (`FileSizeLimitInBytes`, `BasicAuthLevel`) as `(limit, auth)` — the
/// read-only leg of the `doctor` subcommand (M5-2). `None` when the
/// `Parameters` key or either value is missing/unreadable; reading is a
/// diagnosis, so callers report the absence instead of erroring.
pub fn read_webclient_params() -> Option<(u32, u32)> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};

    let hklm = winreg::RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm
        .open_subkey_with_flags(WEBCLIENT_REG_PATH, KEY_READ)
        .ok()?;
    let limit: u32 = key.get_value("FileSizeLimitInBytes").ok()?;
    let auth: u32 = key.get_value("BasicAuthLevel").ok()?;
    Some((limit, auth))
}

/// Locates the installed WinFsp runtime (Phase 3 / WF4): the registry
/// `InstallDir` under either [`WINFSP_REG_KEYS`] subkey, plus the runtime
/// DLL when it is there.
///
/// Selection semantics (review cli-M1, RB4 — the exact shape the runtime
/// gate in `cloudkit-winfsp`'s mount probe follows, so the doctor can
/// never diagnose a half install while a mount would happily run): each
/// subkey's `InstallDir` is a *candidate*; a candidate whose runtime DLL
/// is missing is skipped and the next subkey probed, and only when no
/// candidate carries the DLL does the probe report the first readable
/// `InstallDir` with `dll: None` (the true half-install the doctor
/// warns about). Read-only by design (`doctor`'s leg): no load, no
/// `winfsp_init`, no feature flag — a machine without WinFsp simply gets
/// `None`, which is the `[WARN]` the doctor renders. `None` also covers
/// "the registry key is unreadable", which is indistinguishable from
/// "not installed" for diagnosis purposes.
pub fn winfsp_install() -> Option<WinFspInstall> {
    let candidates = crate::WINFSP_REG_KEYS
        .iter()
        .filter_map(|subkey| read_install_dir(subkey));
    pick_winfsp_install(candidates)
}

/// Reads one subkey's `InstallDir` (a pure registry read; `None` when
/// the key or the value is unreadable).
fn read_install_dir(subkey: &str) -> Option<std::path::PathBuf> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};

    let hklm = winreg::RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm.open_subkey_with_flags(subkey, KEY_READ).ok()?;
    key.get_value::<String, _>("InstallDir")
        .ok()
        .map(std::path::PathBuf::from)
}

/// The probe's selection seam (pure, so both sides of the cli-M1
/// alignment are testable without a real registry): the first candidate
/// whose runtime DLL exists — a DLL-less candidate never masks a good
/// later one.
fn pick_winfsp_install<I>(candidates: I) -> Option<WinFspInstall>
where
    I: IntoIterator<Item = std::path::PathBuf>,
{
    let mut first_readable = None;
    for install_dir in candidates {
        let dll = install_dir.join("bin").join(crate::WINFSP_X64_DLL);
        if dll.exists() {
            return Some(WinFspInstall {
                dll: Some(dll),
                install_dir,
            });
        }
        if first_readable.is_none() {
            first_readable = Some(install_dir);
        }
    }
    first_readable.map(|install_dir| WinFspInstall {
        dll: None,
        install_dir,
    })
}

/// Maps the WebDAV endpoint at `url` to a drive letter and returns the
/// letter actually mounted.
///
/// Baseline order: ensure WebClient runs → pick the target letter
/// (normalise `letter`, probe used letters, apply [`pick_drive_letter`]'s
/// fallback semantics) → release any stale mapping on that letter
/// (failures ignored — the letter may simply be free) →
/// `net use <letter> <url> /persistent:no`, whose non-zero exit becomes
/// [`PlatformError::Command`].
pub fn mount_drive(letter: &str, url: &str) -> Result<String, PlatformError> {
    ensure_webclient_service()?;
    let target = pick_drive_letter(letter, &used_drive_letters());

    // Remove a stale mapping first; on a free letter this fails harmlessly.
    let _ = run_argv(&clear_argv(&unmount_command(&target)));

    run_argv(&mount_argv(&mount_command(&target, url)))?;
    Ok(target)
}

/// Releases the network drive at `letter` (`net use <letter> /delete /y`).
/// The letter is canonicalised leniently, exactly like the baseline —
/// garbage input reaches `net use` and comes back as a `Command` error.
pub fn unmount_drive(letter: &str) -> Result<(), PlatformError> {
    let letter = canonical_letter(letter);
    run_argv(&clear_argv(&unmount_command(&letter)))?;
    Ok(())
}

/// Maps the already-built `mount_command` argv (all owned `String`s) to
/// the borrowed slices [`run_argv`] takes.
fn mount_argv(parts: &[String]) -> Vec<&str> {
    parts.iter().map(String::as_str).collect()
}

/// Same for `unmount_command`'s argv.
fn clear_argv(parts: &[String]) -> Vec<&str> {
    parts.iter().map(String::as_str).collect()
}

/// Runs one external command to completion. Non-zero exit maps to
/// [`PlatformError::Command`] with stderr, falling back to stdout when
/// stderr is empty (the baseline's `result.stderr or result.stdout`).
fn run_argv(argv: &[&str]) -> Result<Output, PlatformError> {
    let output = std::process::Command::new(argv[0])
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

/// Shapes a winreg failure: `ERROR_ACCESS_DENIED` (5) becomes the
/// run-as-administrator guidance the Python baseline produced; anything
/// else keeps the underlying message.
fn registry_error(context: &str, err: &std::io::Error) -> PlatformError {
    if err.raw_os_error() == Some(5) {
        PlatformError::Registry(format!(
            "{context}: administrator privilege required to tune the WebDAV registry — \
             run the terminal as Admin"
        ))
    } else {
        PlatformError::Registry(format!("{context}: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake install tree: `<dir>/bin/winfsp-x64.dll`.
    fn install_tree(dir: &Path, with_dll: bool) -> std::path::PathBuf {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("create bin/");
        if with_dll {
            std::fs::write(bin.join(crate::WINFSP_X64_DLL), b"dll").expect("write dll");
        }
        dir.to_path_buf()
    }

    /// cli-M1 (review RB4): the selection seam — probe order wins when
    /// the first candidate is complete, a DLL-less candidate never masks
    /// a good later one, and a no-DLL-everywhere probe reports the first
    /// readable `InstallDir` as the half install (the doctor's Warn).
    #[test]
    fn pick_winfsp_install_prefers_the_first_complete_candidate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = install_tree(&dir.path().join("first"), true);
        let second = install_tree(&dir.path().join("second"), true);

        let picked = pick_winfsp_install([first.clone(), second.clone()]);
        assert_eq!(
            picked.expect("complete first candidate").install_dir,
            first,
            "probe order wins when the first candidate is complete"
        );
    }

    #[test]
    fn pick_winfsp_install_skips_a_dll_less_candidate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stale = install_tree(&dir.path().join("stale"), false);
        let good = install_tree(&dir.path().join("good"), true);

        let picked = pick_winfsp_install([stale.clone(), good.clone()])
            .expect("the second candidate is complete");
        assert_eq!(
            picked.install_dir, good,
            "a stale first install must not mask a good second one (cli-M1)"
        );
        assert!(picked.dll.is_some(), "the picked candidate carries its DLL");
        assert_eq!(
            picked
                .dll
                .as_ref()
                .expect("dll")
                .parent()
                .and_then(Path::parent),
            Some(good.as_path()),
            "the DLL path is <install_dir>/bin/winfsp-x64.dll"
        );
    }

    #[test]
    fn pick_winfsp_install_reports_the_half_install_when_no_dll_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stale = install_tree(&dir.path().join("stale"), false);

        let picked = pick_winfsp_install([stale.clone(), dir.path().join("missing")])
            .expect("a readable InstallDir without a DLL is still reported");
        assert_eq!(
            picked.install_dir, stale,
            "the first readable dir is reported"
        );
        assert!(
            picked.dll.is_none(),
            "no candidate carries the DLL: the half install the doctor warns about"
        );
    }

    #[test]
    fn pick_winfsp_install_answers_none_for_no_candidates() {
        assert!(
            pick_winfsp_install([]).is_none(),
            "no readable InstallDir at all: not installed"
        );
    }
}
