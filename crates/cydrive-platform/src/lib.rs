//! CyDrive platform layer (unit D): Windows WebDAV drive mapping and
//! WebClient registry tuning, mirroring the Python baseline
//! (`cydrive/platform/windows.py`, class `WindowsMounter`) behaviour for
//! behaviour by behaviour — plus the Linux mount chain (gio → davfs2,
//! `cydrive/platform/linux_mac.py`, class `UnixMounter`) added in the
//! service-lifecycle batch (contract C4).
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
    /// No Linux mount backend (neither `gio` nor `mount.davfs`) was
    /// found on `PATH`.
    #[error("no Linux mount backend available: {0}")]
    MissingBackend(String),
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

// ---------------------------------------------------- Linux mount chain ---
//
// Everything below mirrors the Python baseline's Unix mounter
// (`cydrive/platform/linux_mac.py`, class `UnixMounter`). The plan's
// drafted shapes (a `gio mount -d` form, `sudo`-prefixed davfs2/umount)
// were placeholders: the baseline runs gio without a mount point and
// davfs2/umount without sudo, and sudo/credentials semantics are
// explicitly deferred to the next batch by the plan's C4 scope ruling.

/// Pick the Linux mount backend by availability: gio wins when both are
/// installed, davfs2 alone carries the mount, neither → `None`
/// (design doc :161 order; the baseline probes `gio` first and only
/// consults davfs2 when gio is missing or fails).
pub fn detect_mount_backend(gio: bool, davfs2: bool) -> Option<&'static str> {
    if gio {
        Some("gio")
    } else if davfs2 {
        Some("davfs2")
    } else {
        None
    }
}

/// Argv mounting `url` through GNOME's gio — the baseline's
/// `["gio", "mount", webdav_url]`: no mount point and no `-d` flag (gio
/// chooses where the mount lands).
pub fn gio_mount_command(url: &str) -> Vec<String> {
    vec!["gio".to_string(), "mount".to_string(), url.to_string()]
}

/// Argv mounting `url` at `mount_point` via davfs2 — the baseline's
/// `["mount", "-t", "davfs", webdav_url, mount_path]`, run without sudo
/// (root or a configured davfs2 group membership is assumed, exactly
/// like the baseline).
pub fn davfs_mount_command(url: &str, mount_point: &str) -> Vec<String> {
    vec![
        "mount".to_string(),
        "-t".to_string(),
        "davfs".to_string(),
        url.to_string(),
        mount_point.to_string(),
    ]
}

/// Argv releasing `mount_point` with plain `umount` — the baseline's
/// second Linux unmount attempt.
pub fn davfs_unmount_command(mount_point: &str) -> Vec<String> {
    vec!["umount".to_string(), mount_point.to_string()]
}

/// Argv releasing a FUSE mount at `mount_point` — the baseline's *first*
/// Linux unmount attempt (`["fusermount", "-u", mount_path]`); its
/// failure is ignored when the mount is not FUSE-backed.
pub fn fusermount_unmount_command(mount_point: &str) -> Vec<String> {
    vec![
        "fusermount".to_string(),
        "-u".to_string(),
        mount_point.to_string(),
    ]
}

/// Default Linux mount point: `~/CyDrive` (the baseline's
/// `get_default_mount_point`, minus the directory creation, which is the
/// side-effecting [`linux`] layer's job).
pub fn default_mount_point(home: &std::path::Path) -> std::path::PathBuf {
    home.join("CyDrive")
}

// ------------------------------------------ mount status parsers (status C2) ---
//
// Read-only mounts-state scans behind `cydrive status`: pure text
// parsers any target can unit-test, plus one cfg-gated shell that asks
// the current machine. Windows answers "which letter maps url" by
// scanning `net use`'s listing; Linux answers "which mount point carries
// url" by scanning `/proc/mounts`.

/// Scan `net use`'s listing for the mapping line carrying `url` and
/// return its single-letter drive token as `"Y:"` (canonicalised through
/// [`normalize_drive_letter`], so `y`/`Y:`-shaped tokens both count); no
/// such line → `None`.
///
/// `net use` renders whitespace-aligned columns, so tokens split on
/// whitespace; the matched tokens (letter, URL) are locale-independent,
/// but the surrounding status words come localized on non-English
/// Windows installs — an accepted risk of this parser (documented on the
/// red test's English-format sample).
pub fn parse_net_use_mapping(output: &str, url: &str) -> Option<String> {
    for line in output.lines() {
        if !line.contains(url) {
            continue;
        }
        // e.g. "OK    Y:    http://127.0.0.1:8289    Web Client Network"
        // — the first single-letter token on the line is the mapping.
        for token in line.split_whitespace() {
            if let Some(letter) = normalize_drive_letter(token) {
                return Some(letter);
            }
        }
    }
    None
}

/// Scan `/proc/mounts` (or mount(8)'s rendering of it) for the davfs
/// line mounted from `url` and return its mount point; no such line →
/// `None`.
///
/// Both text forms lead with the mounted URL as their first field —
/// `/proc/mounts`: `<url> <mountpoint> fuse[.<subtype>] <opts> ...`;
/// mount(8): `<url> on <mountpoint> type fuse[.<subtype>] (opts)` — and
/// a FUSE-typed field (`fuse` plain, or `fuse.davfs2`/`fuse.sshfs`
/// subtypes) tells the mount apart from everything else in the file.
/// Anchoring the URL to the first field keeps unrelated FUSE mounts
/// from matching by a coincidental URL substring.
pub fn parse_proc_mounts_davfs(output: &str, url: &str) -> Option<String> {
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.first() != Some(&url) {
            continue;
        }
        if !fields.iter().any(|field| field.starts_with("fuse")) {
            continue;
        }
        let mountpoint = if fields.get(1) == Some(&"on") {
            fields.get(2) // mount(8): "<url> on <mountpoint> type ..."
        } else {
            fields.get(1) // /proc/mounts: "<url> <mountpoint> fuse ..."
        };
        if let Some(point) = mountpoint {
            return Some((*point).to_string());
        }
    }
    None
}

/// The status shell over the two parsers (status plan C2): what the
/// current machine maps `url` to — a `"Y:"`-shaped letter on Windows
/// (`net use` scan), the davfs mount point on Linux (`/proc/mounts`
/// scan), nothing on other platforms. Read-only probes; a failed probe
/// simply reads as "not mounted" (`None`), never as an error — `status`
/// is a diagnosis, not a gate. cfg discipline as everywhere in this
/// crate: exactly one branch survives per target, so all targets
/// compile.
pub fn current_mount_for(url: &str) -> Option<String> {
    #[cfg(windows)]
    {
        let output = std::process::Command::new("net").arg("use").output().ok()?;
        parse_net_use_mapping(&String::from_utf8_lossy(&output.stdout), url)
    }
    #[cfg(target_os = "linux")]
    {
        let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
        parse_proc_mounts_davfs(&mounts, url)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

/// Windows implementation of the side-effecting mount/registry surface.
#[cfg(windows)]
pub mod windows;

/// Compile-only stubs so the workspace builds on non-Windows targets
/// (repo rule; Python precedent `feaac0b`).
#[cfg(not(windows))]
#[path = "windows_stub.rs"]
pub mod windows;

/// Linux implementation of the side-effecting mount chain (gio →
/// davfs2), compiled only on `cfg(target_os = "linux")`. macOS stays
/// stubbed — `mount_webdav` is out of scope per the plan's YAGNI list.
#[cfg(target_os = "linux")]
pub mod linux;

/// Compile-only stubs so the workspace builds on non-Linux targets
/// (same cfg discipline as [`windows`]'s stubs).
#[cfg(not(target_os = "linux"))]
#[path = "linux_stub.rs"]
pub mod linux;
