//! Application configuration: field model, legacy `config.json` loading,
//! canonical `config.toml` round-tripping and `CYDRIVE_*` environment
//! overrides.
//!
//! Contract source: Python `cydrive/config.py` — the field set, the default
//! values and the legacy-JSON behaviour (unknown keys filtered out, missing
//! fields falling back to dataclass defaults) mirror it. Mandated Rust-side
//! design changes (see `docs/rust-rewrite-design.md`, «配置»):
//!
//! * The legacy `config.json` stays **read-only** compatibility; the
//!   canonical on-disk format is `config.toml` ([`CyDriveConfig::save_toml`]
//!   / [`CyDriveConfig::load_toml`]).
//! * Precedence is environment (`CYDRIVE_*`) > file > defaults, applied by
//!   [`CyDriveConfig::with_env_overrides`].
//! * Python `load()` never fails (it degrades to an interactive wizard or a
//!   `NOT_CONFIGURED` placeholder). The Rust loader reports problems via
//!   [`ConfigError`] instead; interactivity belongs to the CLI layer, and
//!   "configured or not" is answered by [`CyDriveConfig::is_configured`].
//! * Paths stay exactly as written (e.g. `"./Telegram_Drive"`); unlike the
//!   Python defaults they are *not* passed through `os.path.abspath`.
//! * `save_toml` writes the file only (creating missing parent directories,
//!   Python `os.makedirs(dirname, exist_ok=True)` parity); it does **not**
//!   create `storage_path`/`cache_path` working directories — that is a
//!   runtime concern elsewhere.

use std::fs;
use std::path::{Path, PathBuf};

use crate::credentials::{CredentialStore, BOT_TOKEN, ENCRYPTION_PASSWORD, SERVICE};

/// Errors produced while loading, saving or validating configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The config file could not be read (missing, permission, ...).
    #[error("failed to read config file {path}: {source}")]
    Read {
        /// The file path that failed to read, stringified as given.
        path: String,
        /// Underlying IO failure.
        #[source]
        source: std::io::Error,
    },
    /// The config file was read but its content is not valid configuration
    /// (malformed syntax or a value of the wrong type).
    #[error("failed to parse config file {path}: {message}")]
    Parse {
        /// The file path that failed to parse, stringified as given.
        path: String,
        /// Human-readable description of the parse/type failure.
        message: String,
    },
    /// The configuration parsed fine but violates a validation rule of
    /// [`CyDriveConfig::validate`].
    #[error("invalid config: {0}")]
    Invalid(String),
    /// Bare IO error outside the read/parse file flow.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Every field name [`CyDriveConfig`] accepts in `config.toml` — the
/// schema-strict key set [`CyDriveConfig::load_toml`] enforces (an unknown
/// key is a [`ConfigError::Parse`], catching typos in the canonical format).
/// Public (Phase 2.5 / MV0) so the key-partition tests can pin the exact
/// bipartition into [`PROCESS_SCOPED_KEYS`] and [`VOLUME_SCOPED_KEYS`].
pub const KNOWN_TOML_KEYS: &[&str] = &[
    "bot_token",
    "chat_id",
    "api_id",
    "api_hash",
    "storage_path",
    "cache_path",
    "db_path",
    "webdav_host",
    "webdav_port",
    "web_ui_host",
    "web_ui_port",
    "enable_web_ui",
    "drive_letter",
    "auto_mount_drive",
    "mount_backend",
    "mount_point",
    "chunk_size_mb",
    "cache_limit_gb",
    "upload_workers",
    "queue_capacity",
    "hydrate_timeout_secs",
    "encryption_password",
    "enable_encryption",
    "encryption_scheme",
    "proxy_url",
    "sync_url",
    "sync_secret",
    "sync_interval_secs",
    "backend",
    "baidu_root",
    "baidu_app_key",
    "baidu_app_secret",
    "baidu_access_token",
    "baidu_refresh_token",
    "local_root",
    "volumes_dir",
];

/// Numeric fields for which legacy JSON additionally accepts a numeric
/// *string* (Python's `chat_id: int or str` union and the wizard's
/// `int(input(...))`-or-raw-string fallback).
const NUMERIC_JSON_KEYS: &[&str] = &[
    "chat_id",
    "api_id",
    "webdav_port",
    "web_ui_port",
    "chunk_size_mb",
    "cache_limit_gb",
];

/// Rust-added tuning keys that a legacy `config.json` must **reject**
/// (unlike the other unknown keys, which stay silently ignored per the
/// Python filter semantics): accepting them would make the user believe a
/// setting takes effect when the legacy loader cannot honour it. The
/// canonical `config.toml` accepts them all.
const LEGACY_REJECTED_KEYS: &[&str] = &[
    "upload_workers",
    "queue_capacity",
    "hydrate_timeout_secs",
    "mount_point",
    "mount_backend",
    "encryption_scheme",
    "sync_url",
    "sync_secret",
    "sync_interval_secs",
    "backend",
    "baidu_root",
    "baidu_app_key",
    "baidu_app_secret",
    "baidu_access_token",
    "baidu_refresh_token",
    "local_root",
    "volumes_dir",
];

/// Client-side encryption container scheme (Batch E / E-4, foundation D7).
///
/// Chooses the format new encrypted uploads are sealed with:
///
/// - [`EncryptionScheme::Gcm`] — v1 whole-file AES-256-GCM, Python
///   `CyCrypto` wire compatible (the default; existing behavior, red line
///   R6). Requires buffering the whole plaintext, kept for compatibility.
/// - [`EncryptionScheme::AeadV2`] — v2 chunked STREAM-construction AEAD:
///   streaming upload (zero `.enc.tmp`) and streaming hydration.
///
/// The read path dispatches on the per-row scheme recorded in the
/// metadata DB, not on this key — switching the key never breaks reads of
/// already-stored files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncryptionScheme {
    /// v1 whole-file GCM (Python compatible, frozen; the default).
    #[default]
    Gcm,
    /// v2 chunked AEAD (streaming + random access).
    AeadV2,
}

impl EncryptionScheme {
    /// Stable identifier as stored in the `files.encryption_scheme`
    /// column and compared on the read path.
    pub const fn as_str(self) -> &'static str {
        match self {
            EncryptionScheme::Gcm => "gcm",
            EncryptionScheme::AeadV2 => "aead_v2",
        }
    }
}

/// Value of [`EncryptionScheme::as_str`] for rows/files sealed with the
/// frozen v1 format (also the DB column default).
pub const SCHEME_GCM: &str = "gcm";
/// Value of [`EncryptionScheme::as_str`] for v2 chunked-AEAD payloads.
pub const SCHEME_AEAD_V2: &str = "aead_v2";

/// Storage backend selector (Phase 2 / K17): which driver the
/// composition root wires behind the transport seam.
///
/// The wire names are the stable `config.toml` spellings
/// (`backend = "telegram" | "baidu" | "local"`). The default —
/// [`Backend::Telegram`] — is the full-compatibility contract: a config
/// without the `backend` key behaves exactly as the pre-Phase-2 build.
/// Value validation is exhaustive at parse time (the field is a typed
/// enum — an unknown variant is a [`ConfigError::Parse`] naming all
/// accepted values and never reaches `validate`, which therefore adds
/// no rule for the value itself; the backend's *cross-field*
/// requirements — baidu credentials, an absolute `local_root` — are
/// `validate` rules, see [`CyDriveConfig::validate`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// Telegram MTProto drive (the original CyDrive, default).
    #[default]
    Telegram,
    /// Baidu netapp drive (ck-baidu, authoritative index).
    Baidu,
    /// Local filesystem drive (ck-local, one root directory).
    Local,
}

impl Backend {
    /// Stable `config.toml` spelling of this backend.
    pub const fn as_str(self) -> &'static str {
        match self {
            Backend::Telegram => "telegram",
            Backend::Baidu => "baidu",
            Backend::Local => "local",
        }
    }
}

/// How a volume is exposed as a Windows drive letter (Phase 3 / K40).
///
/// The wire names are the stable `config.toml` spellings
/// (`mount_backend = "webdav" | "winfsp"`). The default —
/// [`MountBackend::Webdav`] — is the full-compatibility contract: a
/// config without the key keeps the Python baseline's `net use` drive
/// mapping, byte for byte. Value validation is exhaustive at parse time
/// (the field is a typed enum — an unknown variant is a
/// [`ConfigError::Parse`] naming all accepted values and never reaches
/// `validate`, which therefore adds no rule for the key). *Availability*
/// is deliberately not a config concern: `"winfsp"` on a machine without
/// WinFsp (or in a binary built without the feature) is a legal config
/// that the mount flow degrades visibly from (K40's fallback back to
/// WebDAV — never a parse or validation error, never a refusal to boot).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountBackend {
    /// `net use` drive mapping onto the process WebDAV endpoint (the
    /// Python baseline behavior; the default, and the fallback arm every
    /// degraded winfsp mount lands on).
    #[default]
    Webdav,
    /// In-process WinFsp native mount (Phase 3, K38/K39) — needs a
    /// `--features winfsp` build and an installed WinFsp runtime.
    Winfsp,
}

impl MountBackend {
    /// Stable `config.toml` spelling of this backend.
    pub const fn as_str(self) -> &'static str {
        match self {
            MountBackend::Webdav => "webdav",
            MountBackend::Winfsp => "winfsp",
        }
    }
}

/// Process-scoped keys (Phase 2.5 / K19): the settings one process
/// shares across all volumes — the WebDAV/Web UI endpoints, the
/// dashboard switch and the `volumes_dir` itself. Together with
/// [`VOLUME_SCOPED_KEYS`] this bipartitions [`KNOWN_TOML_KEYS`] exactly
/// (no overlap, full coverage — pinned by tests).
pub const PROCESS_SCOPED_KEYS: &[&str] = &[
    "volumes_dir",
    "webdav_host",
    "webdav_port",
    "web_ui_host",
    "web_ui_port",
    "enable_web_ui",
    // A process-level switch by usage: the multi-volume mount gate reads
    // it from the process config (per-volume override would leave the
    // other volumes' mounts ungoverned). Single-volume mode keeps it in
    // config.toml either way — the partition only governs multi mode.
    "auto_mount_drive",
    // Same ruling for the mount backend (Phase 3 / K40): the mount
    // policy is read once per process and applied to every volume's
    // claim, so a per-volume spelling would leave sibling mounts under a
    // different backend with no single place to reason about them.
    "mount_backend",
];

/// Volume-scoped keys (Phase 2.5 / K19): everything a single storage
/// volume owns — the backend selector, all driver parameters and
/// credentials, the encryption group, db/cache/queue tuning, the drive
/// letter/mount keys and the sync group. Legal in a volume file; a
/// multi-volume process-level `config.toml` carrying any of them is a
/// mixing error (see [`ensure_no_volume_keys_in_process`]).
pub const VOLUME_SCOPED_KEYS: &[&str] = &[
    "bot_token",
    "chat_id",
    "api_id",
    "api_hash",
    "storage_path",
    "cache_path",
    "db_path",
    "drive_letter",
    "mount_point",
    "chunk_size_mb",
    "cache_limit_gb",
    "upload_workers",
    "queue_capacity",
    "hydrate_timeout_secs",
    "encryption_password",
    "enable_encryption",
    "encryption_scheme",
    "proxy_url",
    "sync_url",
    "sync_secret",
    "sync_interval_secs",
    "backend",
    "baidu_root",
    "baidu_app_key",
    "baidu_app_secret",
    "baidu_access_token",
    "baidu_refresh_token",
    "local_root",
];

/// One discovered volume: a `<name>.toml` file under the volumes
/// directory (Phase 2.5 / K19), parsed through the same strict schema
/// surface as `config.toml` but restricted to [`VOLUME_SCOPED_KEYS`].
///
/// Relative paths inside `settings` (db/cache/local_root/...) stay
/// **raw** at this layer — no absolutisation happens here. The assembly
/// layer (MV1 / K21) resolves them against the per-volume directory;
/// [`Self::base_dir`] (the volume file's own directory, i.e. the
/// volumes dir) is carried so that resolution has its anchor.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeConfig {
    /// Volume name — the file stem, validated against
    /// `^[a-z][a-z0-9_-]{0,31}$`.
    pub name: String,
    /// The volume file's path on disk.
    pub file_path: PathBuf,
    /// The volume file's directory (the volumes dir) — the anchor for
    /// the MV1/K21 per-volume path resolution.
    pub base_dir: PathBuf,
    /// The volume-scoped key subset, parsed with the same surface and
    /// defaults as `config.toml` (process-scoped keys are rejected).
    pub settings: CyDriveConfig,
    /// Whether the volume file **explicitly** set `drive_letter`
    /// (presence semantics — the parsed `settings.drive_letter` always
    /// carries the single-volume `"Y:"` default and cannot distinguish
    /// an explicit letter from a defaulted one). The cross-volume
    /// conflict check (K27) considers explicit letters only: volumes
    /// that left the letter unset mount nothing by default and never
    /// collide; what an unset letter means at assembly time is MV2's
    /// mount decision, not this layer's.
    pub explicit_drive_letter: bool,
}

/// `true` for exactly `^[a-z][a-z0-9_-]{0,31}$` — a lowercase ASCII
/// letter first, then lowercase letters/digits/`_`/`-`, at most 32
/// characters (K19; the name doubles as the URL segment, the mount
/// label and the dashboard tab, so it stays a conservative slug).
fn is_valid_volume_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    match bytes.first() {
        Some(&first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    bytes.len() <= 32
        && bytes[1..]
            .iter()
            .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Loads a single volume file (Phase 2.5 / K19). The volume name is the
/// file stem; the body accepts exactly the [`VOLUME_SCOPED_KEYS`] subset
/// of the strict `config.toml` surface (unknown keys rejected with the
/// same wording) — a process-scoped key is rejected with guidance
/// pointing back at `config.toml`.
///
/// No path absolutisation happens here (see [`VolumeConfig`]); the
/// settings validate through the same parse-time type checks as
/// `config.toml` (wrong-typed values, unknown enum variants), while the
/// full cross-field [`CyDriveConfig::validate`] run belongs to the
/// assembly layer, after the credential chain has resolved.
pub fn load_volume_config(path: &Path) -> Result<VolumeConfig, ConfigError> {
    let path_str = path_as_str(path);
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| {
            ConfigError::Invalid(format!(
                "invalid volume file {path_str}: the file name must be valid UTF-8"
            ))
        })?
        .to_string();
    if !is_valid_volume_name(&name) {
        return Err(ConfigError::Invalid(format!(
            "invalid volume name `{name}`: volume names must match \
             ^[a-z][a-z0-9_-]{{0,31}}$ (a lowercase letter first, then lowercase \
             letters/digits/`_`/`-`, at most 32 characters) — rename the volume \
             file {path_str}"
        )));
    }
    let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path_str.clone(),
        source,
    })?;
    let table: toml::Table = toml::from_str(&text).map_err(|err| ConfigError::Parse {
        path: path_str.clone(),
        message: err.to_string(),
    })?;
    for key in table.keys() {
        if PROCESS_SCOPED_KEYS.contains(&key.as_str()) {
            return Err(ConfigError::Parse {
                path: path_str,
                message: format!(
                    "key `{key}` is a process-level setting and belongs in config.toml, \
                     not in a volume file — remove it from this file"
                ),
            });
        }
        if !KNOWN_TOML_KEYS.contains(&key.as_str()) {
            return Err(ConfigError::Parse {
                path: path_str,
                message: format!("unknown key `{key}`"),
            });
        }
    }
    let explicit_drive_letter = table.contains_key("drive_letter");
    let settings: CyDriveConfig = table.try_into().map_err(|err| ConfigError::Parse {
        path: path_str,
        message: err.to_string(),
    })?;
    let base_dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    Ok(VolumeConfig {
        name,
        file_path: path.to_path_buf(),
        base_dir,
        explicit_drive_letter,
        settings,
    })
}

/// Lists the `*.toml` volume files in `dir` (non-recursive), sorted
/// stably by file name (K19). A missing directory and a directory with
/// no volume files are both actionable errors — an empty multi-volume
/// process has nothing to serve and must not silently degrade.
pub fn discover_volumes(dir: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    if !dir.is_dir() {
        return Err(ConfigError::Invalid(format!(
            "volumes directory `{dir}` does not exist (or is not a directory): create it \
             and add one `<name>.toml` file per volume, or remove `volumes_dir` from \
             config.toml to run single-volume",
            dir = dir.display(),
        )));
    }
    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue; // directories (even `*.toml`-named) and links are not volumes
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        entries.push((entry.file_name().to_string_lossy().into_owned(), path));
    }
    if entries.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "volumes directory `{dir}` contains no *.toml volume files: add one \
             `<name>.toml` per volume, or remove `volumes_dir` from config.toml to \
             run single-volume",
            dir = dir.display(),
        )));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries.into_iter().map(|(_, path)| path).collect())
}

/// Canonical comparison form of a drive letter: uppercase without the
/// optional trailing `':'` (`"v"`, `"V"` and `"V:"` all collide — the
/// mounter canonicalises the same way). Shape validation stays with
/// [`CyDriveConfig::validate`]; this only groups spellings for the
/// conflict check.
fn normalize_drive_letter(letter: &str) -> String {
    letter.trim_end_matches(':').to_ascii_uppercase()
}

/// Loads every volume under `dir` (discovery + per-file parse) and
/// validates the cross-volume rules: no two volumes may explicitly mount
/// the same `drive_letter` (K27 — spelling differences like `"V"` vs
/// `"v:"` normalise to the same letter and still collide). A volume that
/// left `drive_letter` unset participates in no conflict: the parsed
/// default `"Y:"` is a placeholder, not a mount claim. The returned order
/// is the stable file-name order of [`discover_volumes`].
///
/// Duplicate volume names cannot occur with the single `.toml`
/// extension: two files in one directory cannot share a stem. Should a
/// second extension ever be accepted, name uniqueness needs its own
/// guard here.
pub fn load_volumes(dir: &Path) -> Result<Vec<VolumeConfig>, ConfigError> {
    let mut volumes = Vec::new();
    let mut mounted: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for path in discover_volumes(dir)? {
        let volume = load_volume_config(&path)?;
        if volume.explicit_drive_letter {
            let letter = normalize_drive_letter(&volume.settings.drive_letter);
            if let Some(other) = mounted.get(&letter) {
                return Err(ConfigError::Invalid(format!(
                    "drive_letter conflict: volumes `{other}` and `{}` both mount drive \
                     `{letter}` — give each volume its own drive_letter in its volume file \
                     ({})",
                    volume.name,
                    volume.file_path.display(),
                )));
            }
            mounted.insert(letter, volume.name.clone());
        }
        volumes.push(volume);
    }
    Ok(volumes)
}

/// Mixing guard (K19): in multi-volume mode the process-level
/// `config.toml` must carry **no** volume-scoped key. `keys` are the raw
/// TOML table keys of the process config (presence semantics — an
/// explicit `backend` counts even though its parsed value has a
/// default), so `Ok(())` means the process file is clean.
pub fn ensure_no_volume_keys_in_process(keys: &[String]) -> Result<(), ConfigError> {
    let offending: Vec<&str> = keys
        .iter()
        .map(String::as_str)
        .filter(|key| VOLUME_SCOPED_KEYS.contains(key))
        .collect();
    if offending.is_empty() {
        return Ok(());
    }
    let listed = offending
        .iter()
        .map(|key| format!("`{key}`"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(ConfigError::Invalid(format!(
        "multi-volume config.toml must not contain volume-scoped keys: found {listed} — \
         move them into the per-volume `<name>.toml` files under the volumes_dir directory"
    )))
}

/// Default `upload_workers` (tier-1 contract C6).
fn default_upload_workers() -> u32 {
    2
}

/// Default `queue_capacity` (tier-1 contract C6).
fn default_queue_capacity() -> u32 {
    256
}

/// Default `hydrate_timeout_secs` (tier-1 contract C6). Raised from 180
/// to 1800 (review follow-up BUG②, owner-approved default contract
/// change): real-machine downstream bandwidth through a local proxy
/// measured ~0.45 MB/s (decisions.md 2026-09-03, Tier-1 真机端到端
/// 发现①), so the original 180s — a mirror of the Python WebDAV
/// thread's `future.result(timeout=180)` cap — timed out every file
/// above ~80 MB on a fresh deployment. An explicit `hydrate_timeout_secs`
/// still overrides the default.
fn default_hydrate_timeout_secs() -> u64 {
    1800
}

/// Default `sync_interval_secs` (sync-lite plan, client side).
fn default_sync_interval_secs() -> u64 {
    300
}

/// Default `baidu_root` (Phase 2 / K17): the Baidu app-dir root
/// (ck-baidu `DEFAULT_ROOT` — the driver crate owns the value, this
/// mirrors it for the config default; the composition root passes the
/// key through to `BaiduParams.root`).
fn default_baidu_root() -> String {
    "/apps/cloudfs".to_string()
}

/// Stringifies a path the way [`ConfigError`] variants expect ("as given").
fn path_as_str(path: &Path) -> String {
    path.display().to_string()
}

/// Reads an environment variable as an owned string; absent or non-UTF-8
/// values yield `None`.
fn env_string(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// Reads an environment variable and parses it as `T`; absent, non-UTF-8 or
/// unparseable values yield `None` (caller keeps the previous value).
fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|raw| raw.parse().ok())
}

/// `true` for exactly one ASCII alphabetic letter, optionally followed by
/// `':'` (`"Y"`, `"y:"`, ...); the canonicalisation to uppercase `"X:"` is
/// the mounter's job, validation only checks the shape.
fn is_valid_drive_letter(value: &str) -> bool {
    match value.as_bytes() {
        [letter] => letter.is_ascii_alphabetic(),
        [letter, b':'] => letter.is_ascii_alphabetic(),
        _ => false,
    }
}

/// CyDrive configuration, field-for-field compatible with the Python
/// `CyDriveConfig` dataclass of `cydrive/config.py`.
///
/// All keys use the Python `snake_case` naming in both TOML and legacy JSON.
/// Deserialization applies [`Default`] per missing field (`#[serde(default)]`),
/// so partial config files are valid.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CyDriveConfig {
    /// Telegram bot token from @BotFather, `"<id>:<secret>"`.
    pub bot_token: String,
    /// Telegram user/chat ID.
    pub chat_id: i64,
    /// MTProto API ID (Cynet Android-client credential).
    pub api_id: i32,
    /// MTProto API hash (Cynet Android-client credential).
    pub api_hash: String,
    /// Root of the virtual drive as seen by upload/download.
    pub storage_path: String,
    /// Root of the local on-disk hydration cache.
    pub cache_path: String,
    /// SQLite metadata database file.
    pub db_path: String,
    /// WebDAV listen host.
    pub webdav_host: String,
    /// WebDAV listen port (must be non-zero).
    pub webdav_port: u16,
    /// Web dashboard listen host.
    pub web_ui_host: String,
    /// Web dashboard listen port (must be non-zero).
    pub web_ui_port: u16,
    /// Whether to start the web dashboard at all.
    pub enable_web_ui: bool,
    /// Windows drive letter to mount, canonical form `"X:"`.
    pub drive_letter: String,
    /// Whether to auto-mount the drive on startup.
    pub auto_mount_drive: bool,
    /// How to expose the drive on Windows (Phase 3 / K40):
    /// [`MountBackend::Webdav`] (the default — the `net use` mapping the
    /// Python baseline used) or [`MountBackend::Winfsp`] (the in-process
    /// WinFsp native mount; needs a `--features winfsp` build and an
    /// installed WinFsp runtime — when either is missing the mount flow
    /// logs, says so on the banner and falls back to WebDAV instead of
    /// refusing to start). A **process-scoped** key (K19 partition): it
    /// governs every volume's mount in one process. Value validation is
    /// exhaustive at parse time (typed enum — see [`MountBackend`]).
    #[serde(default)]
    pub mount_backend: MountBackend,
    /// Linux mount point for `cydrive mount` / startup auto-mount
    /// (absolute path; `None` = the `$HOME/CyDrive` default). Windows
    /// ignores this key — drive letters are its mount surface.
    pub mount_point: Option<String>,
    /// Upload chunk size in MB (Telegram per-message limit with margin,
    /// valid range 1..=2000).
    pub chunk_size_mb: u64,
    /// Cache capacity in GB.
    pub cache_limit_gb: u64,
    /// Number of concurrent workers draining the upload queue
    /// (valid range 1..=32).
    #[serde(default = "default_upload_workers")]
    pub upload_workers: u32,
    /// Bounded capacity of the upload queue; must be `>= upload_workers`
    /// and at most 100 000.
    #[serde(default = "default_queue_capacity")]
    pub queue_capacity: u32,
    /// Per-file hydration (download) timeout in seconds
    /// (valid range 1..=86 400).
    #[serde(default = "default_hydrate_timeout_secs")]
    pub hydrate_timeout_secs: u64,
    /// Client-side encryption password; must be set and non-empty when
    /// `enable_encryption` is `true`.
    pub encryption_password: Option<String>,
    /// Whether client-side encryption is enabled.
    pub enable_encryption: bool,
    /// Container scheme for NEW encrypted uploads (Batch E / E-4):
    /// `"gcm"` (default, Python-compatible v1) or `"aead_v2"` (streaming
    /// v2). Read paths dispatch on the per-row scheme in the metadata DB,
    /// never on this key. Value validation is exhaustive at parse time
    /// (the field is a typed enum — an unknown variant is a
    /// [`ConfigError::Parse`] naming both accepted values and never
    /// reaches `validate`, which therefore adds no rule for this key).
    pub encryption_scheme: EncryptionScheme,
    /// Optional SOCKS5 proxy URL for the Telegram MTProto connection
    /// (e.g. `"socks5://127.0.0.1:7897"` for a local Clash mixed port);
    /// `None` connects directly. `validate` imposes no rules on it — any
    /// string (or `None`) is accepted here, the transport layer owns the
    /// scheme semantics.
    pub proxy_url: Option<String>,
    /// Base URL of the sync-lite metadata server (e.g.
    /// `"https://sync.example.org:8290"`); `None` disables syncing
    /// entirely. When set, `validate` requires the `http://` or `https://`
    /// scheme — the sync client itself speaks plain HTTP, TLS is a
    /// reverse-proxy concern (sync-lite plan, client side).
    pub sync_url: Option<String>,
    /// Optional family-level shared secret the sync server may require
    /// (the server-side `SYNC_SECRET` gate); `None` = send none. The key
    /// is deliberately **not** part of the core env-override set: the
    /// secret's env route is the CLI-layer `CYDRIVE_SYNC_SECRET`
    /// (`resolve_sync_secret`), which outranks this file value. Unlike
    /// `sync_url` there is no format constraint — any non-empty string is
    /// a legal secret, and a value that trims to empty reads as unset at
    /// the CLI resolution layer. Hand-writing the key into config.toml is
    /// allowed (the file may already hold the bot token); programmatic
    /// writes go through [`CyDriveConfig::save_toml_scrubbed`], which
    /// keeps it out of the file.
    pub sync_secret: Option<String>,
    /// Sync polling interval in seconds (valid range 1..=86 400).
    #[serde(default = "default_sync_interval_secs")]
    pub sync_interval_secs: u64,
    /// Storage backend selector (Phase 2 / K17); the `telegram` default
    /// keeps every pre-Phase-2 config byte-compatible. Unknown values
    /// are rejected at parse time (typed enum — see [`Backend`]); the
    /// backend's cross-field requirements live in `validate`.
    #[serde(default)]
    pub backend: Backend,
    /// Baidu drive root as a backend-absolute path (must start with
    /// `'/'` when the backend is `baidu`; the default mirrors
    /// ck-baidu's `DEFAULT_ROOT`).
    #[serde(default = "default_baidu_root")]
    pub baidu_root: String,
    /// Baidu app key (K14 credential). No code default — the value
    /// comes from this key or `CYDRIVE_BAIDU_APP_KEY`; required (with
    /// the other three baidu keys) when `backend = "baidu"`.
    pub baidu_app_key: Option<String>,
    /// Baidu app secret (K14 credential); env route
    /// `CYDRIVE_BAIDU_APP_SECRET`. See [`Self::baidu_app_key`].
    pub baidu_app_secret: Option<String>,
    /// Baidu OAuth access token (K14 credential); env route
    /// `CYDRIVE_BAIDU_ACCESS_TOKEN`. Refreshed in place by the driver's
    /// refresh machine; the persisted rotation is a Batch B3b setup
    /// concern.
    pub baidu_access_token: Option<String>,
    /// Baidu OAuth refresh token (K14 credential); env route
    /// `CYDRIVE_BAIDU_REFRESH_TOKEN`.
    pub baidu_refresh_token: Option<String>,
    /// Local drive root (ck-local volume root). Required and absolute
    /// when `backend = "local"` (Windows `C:\...` and Unix `/...`
    /// shapes both pass — [`std::path::Path::is_absolute`] semantics);
    /// inert otherwise. No env route (K17 names env overrides for the
    /// four baidu keys only).
    pub local_root: Option<String>,
    /// Volumes directory (Phase 2.5 / K19), relative to the process
    /// working directory: one `<name>.toml` file per storage volume
    /// (see [`load_volume_config`]). `None` — the default — means
    /// single-volume mode: every pre-Phase-2.5 behaviour is
    /// byte-compatible. When set, the process-level `config.toml` may
    /// carry only [`PROCESS_SCOPED_KEYS`] (mixing is a validation
    /// error) and the directory must exist and hold at least one
    /// volume file.
    pub volumes_dir: Option<String>,
}

impl Default for CyDriveConfig {
    /// Defaults aligned field-by-field with the Python dataclass
    /// (`cydrive/config.py`), except that path defaults keep their literal
    /// relative form instead of being `os.path.abspath`-resolved.
    fn default() -> Self {
        Self {
            bot_token: String::new(),
            chat_id: 0,
            api_id: 6,
            api_hash: "eb06d4abfb49dc3eeb1aeb98ae0f581e".to_string(),
            storage_path: "./Telegram_Drive".to_string(),
            cache_path: "./Telegram_Cache".to_string(),
            db_path: "./cydrive_meta.db".to_string(),
            webdav_host: "127.0.0.1".to_string(),
            webdav_port: 8080,
            web_ui_host: "127.0.0.1".to_string(),
            web_ui_port: 8088,
            enable_web_ui: true,
            drive_letter: "Y:".to_string(),
            auto_mount_drive: true,
            mount_backend: MountBackend::default(),
            mount_point: None,
            chunk_size_mb: 1900,
            cache_limit_gb: 20,
            upload_workers: default_upload_workers(),
            queue_capacity: default_queue_capacity(),
            hydrate_timeout_secs: default_hydrate_timeout_secs(),
            encryption_password: None,
            enable_encryption: false,
            encryption_scheme: EncryptionScheme::default(),
            proxy_url: None,
            sync_url: None,
            sync_secret: None,
            sync_interval_secs: default_sync_interval_secs(),
            backend: Backend::default(),
            baidu_root: default_baidu_root(),
            baidu_app_key: None,
            baidu_app_secret: None,
            baidu_access_token: None,
            baidu_refresh_token: None,
            local_root: None,
            volumes_dir: None,
        }
    }
}

impl CyDriveConfig {
    /// Loads the canonical `config.toml` from `path`.
    ///
    /// * Unreadable file (missing, permissions, ...) → [`ConfigError::Read`].
    /// * Malformed TOML or a value of the wrong type → [`ConfigError::Parse`].
    /// * Fields absent from the file fall back to [`Default`] values
    ///   (`#[serde(default)]`); unknown extra keys are an error (TOML is
    ///   schema-strict, unlike the legacy JSON).
    /// * Creates nothing, mounts nothing — pure load.
    pub fn load_toml(path: &Path) -> Result<Self, ConfigError> {
        Self::load_toml_with_keys(path).map(|(config, _keys)| config)
    }

    /// [`Self::load_toml`] plus the raw key set the file actually
    /// contains (Phase 2.5 / K19). Presence semantics matter for keys
    /// whose parsed value carries a default (`backend`, `db_path`, ...):
    /// the multi-volume mixing guard must see an *explicit* key, which
    /// the deserialised config alone cannot distinguish from a default.
    pub fn load_toml_with_keys(path: &Path) -> Result<(Self, Vec<String>), ConfigError> {
        let path_str = path_as_str(path);
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path_str.clone(),
            source,
        })?;
        // Two-stage parse: value-tree first (so unknown keys can be detected
        // manually — serde would silently ignore them), then derive-based
        // deserialization with per-field `#[serde(default)]` fallback.
        let table: toml::Table = toml::from_str(&text).map_err(|err| ConfigError::Parse {
            path: path_str.clone(),
            message: err.to_string(),
        })?;
        for key in table.keys() {
            if !KNOWN_TOML_KEYS.contains(&key.as_str()) {
                return Err(ConfigError::Parse {
                    path: path_str,
                    message: format!("unknown key `{key}`"),
                });
            }
        }
        let keys: Vec<String> = table.keys().cloned().collect();
        let config: CyDriveConfig = table.try_into().map_err(|err| ConfigError::Parse {
            path: path_str,
            message: err.to_string(),
        })?;
        Ok((config, keys))
    }

    /// Serializes `self` to TOML and writes it to `path`, creating missing
    /// parent directories (Python `save()` `os.makedirs(dirname, ...)`
    /// parity). Unlike the Python `save`, this does **not** create the
    /// `storage_path`/`cache_path` working directories.
    ///
    /// Secrets are written as-is; use [`CyDriveConfig::save_toml_scrubbed`]
    /// for the credential-vault flow (M5) that keeps them out of the file.
    pub fn save_toml(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string_pretty(self).map_err(|err| ConfigError::Parse {
            path: path_as_str(path),
            message: err.to_string(),
        })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, text)?;
        Ok(())
    }

    /// Serializes `self` with the secret fields scrubbed and writes it
    /// to `path`: `bot_token` becomes an empty string, the optional
    /// `encryption_password` and `sync_secret` are omitted entirely, and
    /// a header comment points readers at the OS credential store
    /// (keyring service `"cydrive"`); the secrets re-enter the config at
    /// load time via [`CyDriveConfig::with_credential_backfill`]
    /// (the sync secret re-enters through its env variable or a
    /// hand-written file value — see the CLI resolution chain).
    ///
    /// This is the write side of the credential vault (M5): on-disk config
    /// files stay non-sensitive while `enable_encryption` and every other
    /// field round-trip unchanged. Directory creation matches
    /// [`CyDriveConfig::save_toml`]. Hand-writing `sync_secret` into the
    /// file stays legal (the ruling: family-level convenience beats file
    /// secrecy — the file may already hold the bot token); only
    /// programmatic writes go through this scrubbed path.
    pub fn save_toml_scrubbed(&self, path: &Path) -> Result<(), ConfigError> {
        let mut scrubbed = self.clone();
        scrubbed.bot_token = String::new();
        scrubbed.encryption_password = None;
        scrubbed.sync_secret = None;
        let body = toml::to_string_pretty(&scrubbed).map_err(|err| ConfigError::Parse {
            path: path_as_str(path),
            message: err.to_string(),
        })?;
        let text = format!(
            "# Secrets live in the OS credential manager (keyring service \"{SERVICE}\"), \
             not in this file:\n# the bot token and the encryption password are \
             intentionally empty here and return via the credential store.\n# The sync \
             shared secret is omitted here too; it returns via CYDRIVE_SYNC_SECRET or a \
             hand-written sync_secret value.\n{body}"
        );
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, text)?;
        Ok(())
    }

    /// Loads a legacy Python `config.json`.
    ///
    /// Python semantics (`CyDriveConfig.load`): parse JSON, keep only keys
    /// that are dataclass fields, use the dataclass defaults for the rest.
    /// Concretely:
    ///
    /// * Unknown keys are silently ignored (Python filters by
    ///   `cls.__dataclass_fields__`).
    /// * Missing fields fall back to [`Default`].
    /// * Divergence from Python: Python falls back to the interactive wizard
    ///   whenever the file is missing/unreadable/malformed or lacks a truthy
    ///   `bot_token`+`chat_id` pair; the Rust port has no wizard in the core,
    ///   so unreadable → [`ConfigError::Read`], malformed JSON or wrong-typed
    ///   values → [`ConfigError::Parse`], and a file without token/chat_id
    ///   loads fine (configured-ness is judged by
    ///   [`CyDriveConfig::is_configured`]).
    pub fn load_legacy_json(path: &Path) -> Result<Self, ConfigError> {
        let path_str = path_as_str(path);
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path_str.clone(),
            source,
        })?;
        let mut root: serde_json::Value =
            serde_json::from_str(&text).map_err(|err| ConfigError::Parse {
                path: path_str.clone(),
                message: err.to_string(),
            })?;
        // The legacy key set is frozen at the Python dataclass fields: the
        // Rust-added tuning keys are rejected instead of silently ignored
        // (Python filter semantics) so the user is not left believing a
        // setting takes effect when the legacy loader cannot honour it.
        if let serde_json::Value::Object(map) = &root {
            for key in LEGACY_REJECTED_KEYS {
                if map.contains_key(*key) {
                    return Err(ConfigError::Parse {
                        path: path_str.clone(),
                        message: format!(
                            "unknown key `{key}`; this tuning key is not part of the \
                             legacy config.json — use config.toml instead"
                        ),
                    });
                }
            }
        }
        // Python leniency for numeric fields: accept numeric strings by
        // normalising them to JSON numbers before the typed conversion.
        // Non-numeric strings are left in place and rejected as a type
        // mismatch below, matching the `Parse` contract.
        if let serde_json::Value::Object(map) = &mut root {
            for key in NUMERIC_JSON_KEYS {
                if let Some(value) = map.get_mut(*key) {
                    let as_number = value.as_str().and_then(|s| s.parse::<i64>().ok());
                    if let Some(number) = as_number {
                        *value = serde_json::Value::Number(number.into());
                    }
                }
            }
        }
        // Derived `Deserialize` on `CyDriveConfig`: unknown keys ignored,
        // missing fields defaulted (`#[serde(default)]`), wrong types error.
        serde_json::from_value(root).map_err(|err| ConfigError::Parse {
            path: path_str,
            message: err.to_string(),
        })
    }

    /// Returns a copy with `CYDRIVE_*` environment overrides applied
    /// (precedence: env > current value). Consumes `self`.
    ///
    /// Recognised keys — anything else is ignored:
    ///
    /// | Key | Field | Notes |
    /// |---|---|---|
    /// | `CYDRIVE_BOT_TOKEN` | `bot_token` | verbatim string |
    /// | `CYDRIVE_CHAT_ID` | `chat_id` | `i64`; unparseable value → key ignored |
    /// | `CYDRIVE_WEBDAV_PORT` | `webdav_port` | `u16`; unparseable → ignored |
    /// | `CYDRIVE_WEB_UI_PORT` | `web_ui_port` | `u16`; unparseable → ignored |
    /// | `CYDRIVE_DRIVE_LETTER` | `drive_letter` | verbatim string |
    /// | `CYDRIVE_CHUNK_SIZE_MB` | `chunk_size_mb` | `u64`; unparseable → ignored |
    /// | `CYDRIVE_ENABLE_ENCRYPTION` | `enable_encryption` | `"1"` or `"true"` (case-sensitive) → `true`; any other value → `false` |
    /// | `CYDRIVE_PROXY_URL` | `proxy_url` | verbatim string; an empty value clears the proxy (`None`) |
    /// | `CYDRIVE_SYNC_URL` | `sync_url` | verbatim string; an empty value clears the sync URL (`None`, feature off) |
    /// | `CYDRIVE_BAIDU_APP_KEY` | `baidu_app_key` | verbatim string; an empty value clears it (`None`) — the proxy/sync_url precedent |
    /// | `CYDRIVE_BAIDU_APP_SECRET` | `baidu_app_secret` | same empty-clears rule |
    /// | `CYDRIVE_BAIDU_ACCESS_TOKEN` | `baidu_access_token` | same empty-clears rule |
    /// | `CYDRIVE_BAIDU_REFRESH_TOKEN` | `baidu_refresh_token` | same empty-clears rule |
    pub fn with_env_overrides(self) -> Self {
        let mut config = self;
        if let Some(value) = env_string("CYDRIVE_BOT_TOKEN") {
            config.bot_token = value;
        }
        if let Some(value) = env_parse::<i64>("CYDRIVE_CHAT_ID") {
            config.chat_id = value;
        }
        if let Some(value) = env_parse::<u16>("CYDRIVE_WEBDAV_PORT") {
            config.webdav_port = value;
        }
        if let Some(value) = env_parse::<u16>("CYDRIVE_WEB_UI_PORT") {
            config.web_ui_port = value;
        }
        if let Some(value) = env_string("CYDRIVE_DRIVE_LETTER") {
            config.drive_letter = value;
        }
        if let Some(value) = env_parse::<u64>("CYDRIVE_CHUNK_SIZE_MB") {
            config.chunk_size_mb = value;
        }
        if let Some(value) = env_string("CYDRIVE_ENABLE_ENCRYPTION") {
            config.enable_encryption = value == "1" || value == "true";
        }
        if let Some(value) = env_string("CYDRIVE_PROXY_URL") {
            config.proxy_url = (!value.is_empty()).then_some(value);
        }
        if let Some(value) = env_string("CYDRIVE_SYNC_URL") {
            config.sync_url = (!value.is_empty()).then_some(value);
        }
        // K14 (Phase 2): the four baidu credential keys ride the same
        // env > file chain as the other CYDRIVE_* overrides; a
        // set-but-empty value explicitly clears the file value so a
        // shell export can neutralise a stale token without editing
        // config.toml.
        if let Some(value) = env_string("CYDRIVE_BAIDU_APP_KEY") {
            config.baidu_app_key = (!value.is_empty()).then_some(value);
        }
        if let Some(value) = env_string("CYDRIVE_BAIDU_APP_SECRET") {
            config.baidu_app_secret = (!value.is_empty()).then_some(value);
        }
        if let Some(value) = env_string("CYDRIVE_BAIDU_ACCESS_TOKEN") {
            config.baidu_access_token = (!value.is_empty()).then_some(value);
        }
        if let Some(value) = env_string("CYDRIVE_BAIDU_REFRESH_TOKEN") {
            config.baidu_refresh_token = (!value.is_empty()).then_some(value);
        }
        config
    }

    /// Returns a copy with the two secret fields backfilled from `store`
    /// — but only where the file left them empty or absent. Consumes
    /// `self`.
    ///
    /// This is the "file > store" leg of the M5 precedence chain
    /// **env > file > OS credential store** (the CLI applies it as
    /// `load → backfill → env-overrides`, so a `CYDRIVE_*` variable still
    /// outranks everything). A non-empty file value is never overwritten.
    ///
    /// Store failures degrade to a warning and keep the file value: a
    /// missing or locked credential vault must not refuse CyDrive from
    /// starting.
    pub fn with_credential_backfill(self, store: &dyn CredentialStore) -> Self {
        let mut config = self;
        if config.bot_token.is_empty() {
            match store.get(BOT_TOKEN) {
                Ok(Some(token)) => config.bot_token = token,
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    %error, key = BOT_TOKEN,
                    "credential backfill failed; keeping the empty file value"
                ),
            }
        }
        if config
            .encryption_password
            .as_deref()
            .is_none_or(str::is_empty)
        {
            match store.get(ENCRYPTION_PASSWORD) {
                Ok(Some(password)) => config.encryption_password = Some(password),
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    %error, key = ENCRYPTION_PASSWORD,
                    "credential backfill failed; keeping the file value"
                ),
            }
        }
        config
    }

    /// `true` when the config carries usable Telegram credentials:
    /// `bot_token` is non-empty **and** contains `':'`, **and** `chat_id != 0`
    /// (Python truthiness check plus the token-shape check from its setup
    /// wizard).
    pub fn is_configured(&self) -> bool {
        !self.bot_token.is_empty() && self.bot_token.contains(':') && self.chat_id != 0
    }

    /// Checks cross-field sanity (new in the Rust port; Python only had
    /// ad-hoc checks). Rules — all violations → [`ConfigError::Invalid`]:
    ///
    /// * `bot_token`: if non-empty, it must contain `':'` (empty means
    ///   "not configured yet" and is accepted).
    /// * `chunk_size_mb`: must be in `1..=2000` (Telegram single-message
    ///   hard limit with margin).
    /// * `drive_letter`: exactly one ASCII alphabetic letter optionally
    ///   followed by `':'`, any case (`"Y:"`, `"y"`, `"y:"` all pass — the
    ///   mounter canonicalises to uppercase `"X:"`); anything else (empty,
    ///   multi-character, digits, `"N/A"`, ...) is invalid.
    /// * `webdav_port` and `web_ui_port`: must be non-zero.
    /// * `enable_encryption == true` requires `encryption_password` to be
    ///   `Some` and non-empty.
    /// * `upload_workers`: must be in `1..=32`.
    /// * `queue_capacity`: must be `>= upload_workers` and at most `100_000`.
    /// * `hydrate_timeout_secs`: must be in `1..=86_400`.
    /// * `sync_interval_secs`: must be in `1..=86_400`.
    /// * `sync_url`: when `Some`, must start with `http://` or `https://`
    ///   and carry a non-empty host (`http://`, `http://:8290/x` and
    ///   `http:///path` are rejected; `None` means the sync feature is
    ///   off and skips the check).
    /// * `sync_secret`: no format constraint — any non-empty string is a
    ///   legal secret, and a value that trims to empty reads as unset at
    ///   the CLI resolution layer (never a validation error).
    /// * `mount_backend` value: no rule here either — the typed enum
    ///   rejects unknown values at parse time (see [`MountBackend`]), and
    ///   whether WinFsp is *usable* is a runtime fact the mount flow
    ///   degrades from (K40), never a validation error.
    /// * `backend` value: no rule here — the typed enum rejects unknown
    ///   values at parse time (see [`Backend`]). Cross-field rules do
    ///   live here: `backend = "baidu"` requires all four K14 credential
    ///   keys (`baidu_app_key` / `baidu_app_secret` /
    ///   `baidu_access_token` / `baidu_refresh_token`) set and non-empty
    ///   (each naming its `CYDRIVE_BAIDU_*` env route in the error), and
    ///   a `baidu_root` starting with `'/'`; `backend = "local"`
    ///   requires `local_root` set and absolute. The telegram default
    ///   triggers neither rule — pre-Phase-2 configs validate unchanged.
    ///
    /// Returns `Ok(())` when every rule holds.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.bot_token.is_empty() && !self.bot_token.contains(':') {
            return Err(ConfigError::Invalid(
                "bot_token is set but contains no ':' (expected \"<id>:<secret>\")".to_string(),
            ));
        }
        if !(1..=2000).contains(&self.chunk_size_mb) {
            return Err(ConfigError::Invalid(format!(
                "chunk_size_mb must be in 1..=2000, got {}",
                self.chunk_size_mb
            )));
        }
        if !is_valid_drive_letter(&self.drive_letter) {
            return Err(ConfigError::Invalid(format!(
                "drive_letter must be one ASCII letter with optional ':' , got {:?}",
                self.drive_letter
            )));
        }
        if self.webdav_port == 0 {
            return Err(ConfigError::Invalid(
                "webdav_port must be non-zero".to_string(),
            ));
        }
        if self.web_ui_port == 0 {
            return Err(ConfigError::Invalid(
                "web_ui_port must be non-zero".to_string(),
            ));
        }
        if self.enable_encryption
            && self
                .encryption_password
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err(ConfigError::Invalid(
                "enable_encryption requires a non-empty encryption_password".to_string(),
            ));
        }
        if !(1..=32).contains(&self.upload_workers) {
            return Err(ConfigError::Invalid(format!(
                "upload_workers must be in 1..=32, got {}",
                self.upload_workers
            )));
        }
        if self.queue_capacity < self.upload_workers || self.queue_capacity > 100_000 {
            return Err(ConfigError::Invalid(format!(
                "queue_capacity must be >= upload_workers ({}) and <= 100000, got {}",
                self.upload_workers, self.queue_capacity
            )));
        }
        if !(1..=86_400).contains(&self.hydrate_timeout_secs) {
            return Err(ConfigError::Invalid(format!(
                "hydrate_timeout_secs must be in 1..=86400, got {}",
                self.hydrate_timeout_secs
            )));
        }
        if let Some(point) = &self.mount_point {
            if !point.starts_with('/') {
                return Err(ConfigError::Invalid(format!(
                    "mount_point must be an absolute path starting with '/', got {point:?}"
                )));
            }
        }
        if !(1..=86_400).contains(&self.sync_interval_secs) {
            return Err(ConfigError::Invalid(format!(
                "sync_interval_secs must be in 1..=86400, got {}",
                self.sync_interval_secs
            )));
        }
        if let Some(url) = &self.sync_url {
            let rest = url
                .strip_prefix("http://")
                .or_else(|| url.strip_prefix("https://"));
            let Some(rest) = rest else {
                return Err(ConfigError::Invalid(format!(
                    "sync_url must start with http:// or https://, got {url:?}"
                )));
            };
            // Minimal authority parse (no url crate in core): the
            // authority runs to the first '/', '?' or '#', the host to
            // the first ':' (the port) or the authority's end. A scheme
            // with no host — "http://", "http://:8290/x", "http:///path"
            // — is not a reachable server and must fail loudly here.
            let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
            let host = authority.split(':').next().unwrap_or_default();
            if host.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "sync_url needs a host, e.g. \"http://sync.example.org:8290\", got {url:?}"
                )));
            }
        }
        // Phase 2 / K17 cross-field rules, gated on the backend so the
        // telegram default (every pre-Phase-2 config) never fires them.
        if self.backend == Backend::Baidu {
            for (name, value, env) in [
                (
                    "baidu_app_key",
                    &self.baidu_app_key,
                    "CYDRIVE_BAIDU_APP_KEY",
                ),
                (
                    "baidu_app_secret",
                    &self.baidu_app_secret,
                    "CYDRIVE_BAIDU_APP_SECRET",
                ),
                (
                    "baidu_access_token",
                    &self.baidu_access_token,
                    "CYDRIVE_BAIDU_ACCESS_TOKEN",
                ),
                (
                    "baidu_refresh_token",
                    &self.baidu_refresh_token,
                    "CYDRIVE_BAIDU_REFRESH_TOKEN",
                ),
            ] {
                if value.as_deref().is_none_or(str::is_empty) {
                    return Err(ConfigError::Invalid(format!(
                        "backend = \"baidu\" requires {name}: set it in config.toml or export \
                         {env} (no code default exists for credentials)"
                    )));
                }
            }
            if !self.baidu_root.starts_with('/') {
                return Err(ConfigError::Invalid(format!(
                    "baidu_root must be a backend-absolute path starting with '/', e.g. \
                     \"/apps/cloudfs\", got {:?}",
                    self.baidu_root
                )));
            }
        }
        if self.backend == Backend::Local {
            match self.local_root.as_deref() {
                Some(root) if !root.trim().is_empty() => {
                    if !Path::new(root).is_absolute() {
                        return Err(ConfigError::Invalid(format!(
                            "backend = \"local\" requires local_root to be an absolute path \
                             (e.g. \"/srv/cloudfs\" or \"C:\\data\\cloudfs\"), got {root:?}"
                        )));
                    }
                }
                _ => {
                    return Err(ConfigError::Invalid(
                        "backend = \"local\" requires local_root: set it to the drive's root \
                         directory as an absolute path in config.toml"
                            .to_string(),
                    ));
                }
            }
        }
        Ok(())
    }
}
