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
    BASIC_AUTH_LEVEL, FILE_SIZE_LIMIT_BYTES, WEBCLIENT_REG_PATH,
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
