//! CyDrive platform layer (unit D): Windows WebDAV drive mapping and
//! WebClient registry tuning, mirroring the Python baseline
//! (`cydrive/platform/windows.py`, class `WindowsMounter`) behaviour for
//! behaviour by behaviour.
//!
//! The layout follows the repo rule inherited from the Python side
//! (precedent `feaac0b`: `platform/windows.py` must stay importable on
//! Linux/macOS): everything in this root module is pure, cross-platform
//! logic that any target can unit-test; the side-effecting [`windows`]
//! module carries the real implementation under `cfg(windows)` and
//! compile-only `Unsupported` stubs elsewhere, so the whole workspace
//! keeps building on non-Windows targets.

/// Fallback probe order once the preferred drive letter is occupied —
/// a direct lift of the Python baseline list ("from Z backwards").
pub const FALLBACK_DRIVE_LETTERS: [&str; 8] = ["Z:", "Y:", "X:", "W:", "V:", "U:", "T:", "S:"];

/// WebClient service `Parameters` subkey under `HKEY_LOCAL_MACHINE`.
pub const WEBCLIENT_REG_PATH: &str = r"SYSTEM\CurrentControlSet\Services\WebClient\Parameters";

/// `FileSizeLimitInBytes` value written by [`windows::optimize_webdav_registry`]
/// — 4 GiB−1, the largest DWORD WebDAV's WebClient accepts.
pub const FILE_SIZE_LIMIT_BYTES: u32 = 4294967295;

/// `BasicAuthLevel` value written by [`windows::optimize_webdav_registry`]
/// — 2 = basic auth allowed on both HTTP and HTTPS.
pub const BASIC_AUTH_LEVEL: u32 = 2;

/// Errors from the platform layer's side-effecting operations.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// The operation only exists on another platform (non-Windows stubs).
    #[error("not supported on this platform: {0}")]
    Unsupported(&'static str),
    /// An external command (`net use`, `sc`, …) exited non-zero.
    #[error("command failed ({command}): {message}")]
    Command {
        /// The argv joined for display (e.g. `net use Y: http://…`).
        command: String,
        /// The command's stderr, or stdout when stderr is empty — the
        /// Python baseline's `result.stderr or result.stdout` choice.
        message: String,
    },
    /// A registry open/write failed (permissions map to a clear
    /// run-as-administrator message).
    #[error("registry error: {0}")]
    Registry(String),
    /// Spawning a command or probing the filesystem failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Lenient canonicalisation shared by every entry point: trim, uppercase,
/// ensure the trailing colon. Unlike [`normalize_drive_letter`] this never
/// rejects — the Python baseline fed whatever it got straight to `net use`
/// and let the command report the mistake.
fn canonical_letter(input: &str) -> String {
    let trimmed = input.trim().to_uppercase();
    if trimmed.ends_with(':') {
        trimmed
    } else {
        format!("{trimmed}:")
    }
}

/// Canonicalise a drive letter: `"y"`, `"y:"` and `" Y: "` all become
/// `Some("Y:")`. Anything that is not exactly one ASCII alphabetic
/// character (multi-letter, empty, digits, …) is `None`.
pub fn normalize_drive_letter(input: &str) -> Option<String> {
    let canonical = canonical_letter(input);
    let core = &canonical[..canonical.len() - 1]; // the colon is guaranteed ASCII
    let mut chars = core.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => Some(canonical),
        _ => None,
    }
}

/// Pick the drive letter to mount, with the Python baseline's semantics:
/// a free preferred letter wins; otherwise the first free letter along
/// [`FALLBACK_DRIVE_LETTERS`]; when everything is occupied the preferred
/// letter itself is returned (the mount will most likely fail, but the
/// choice stays deterministic and identical to the baseline).
///
/// An unparseable `preferred` degrades to the baseline's lenient
/// canonicalisation instead of failing here — `net use` reports it.
pub fn pick_drive_letter(preferred: &str, used: &[String]) -> String {
    let preferred =
        normalize_drive_letter(preferred).unwrap_or_else(|| canonical_letter(preferred));
    if !used.iter().any(|l| l == &preferred) {
        return preferred;
    }
    for letter in FALLBACK_DRIVE_LETTERS {
        if !used.iter().any(|l| l == letter) {
            return letter.to_string();
        }
    }
    preferred
}

/// Decode a `GetLogicalDrives`-style bitmask: bit *i* set means drive
/// `char::from(b'A' + i)` exists, reported as `"A:"`, `"B:"`, …
pub fn used_letters_from_bitmask(mask: u32) -> Vec<String> {
    (0..26)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| format!("{}:", char::from(b'A' + bit as u8)))
        .collect()
}

/// Argv mapping `letter` to the WebDAV endpoint at `url`:
/// `["net", "use", "Y:", url, "/persistent:no"]`.
pub fn mount_command(letter: &str, url: &str) -> Vec<String> {
    vec![
        "net".to_string(),
        "use".to_string(),
        letter.to_string(),
        url.to_string(),
        "/persistent:no".to_string(),
    ]
}

/// Argv releasing `letter`:
/// `["net", "use", "Y:", "/delete", "/y"]`.
pub fn unmount_command(letter: &str) -> Vec<String> {
    vec![
        "net".to_string(),
        "use".to_string(),
        letter.to_string(),
        "/delete".to_string(),
        "/y".to_string(),
    ]
}

/// Windows implementation of the side-effecting mount/registry surface.
#[cfg(windows)]
pub mod windows;

/// Compile-only stubs so the workspace builds on non-Windows targets
/// (repo rule; Python precedent `feaac0b`).
#[cfg(not(windows))]
#[path = "windows_stub.rs"]
pub mod windows;
