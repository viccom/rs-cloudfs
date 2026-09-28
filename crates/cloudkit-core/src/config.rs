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
use std::io::Write as _;
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
    "allow_remote_admin",
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
    "sftp_host",
    "sftp_port",
    "sftp_username",
    "sftp_password",
    "sftp_private_key_path",
    "sftp_private_key_passphrase",
    "sftp_host_fingerprint",
    "sftp_root",
    "pan115_client_id",
    "pan115_access_token",
    "pan115_refresh_token",
    "pan115_root",
    "pan123_token",
    "pan123_root",
    "webdav_url",
    "webdav_username",
    "webdav_password",
    "webdav_auth",
    "webdav_vendor",
    "webdav_accept_invalid_certs",
    "volumes_dir",
    "enabled",
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
    "sftp_host",
    "sftp_port",
    "sftp_username",
    "sftp_password",
    "sftp_private_key_path",
    "sftp_private_key_passphrase",
    "sftp_host_fingerprint",
    "sftp_root",
    "pan115_client_id",
    "pan115_access_token",
    "pan115_refresh_token",
    "pan115_root",
    "pan123_token",
    "pan123_root",
    // Phase 7 / WD1b: the six webdav keys are Rust-added (no Python
    // dataclass ever carried them), so a legacy config.json must reject
    // them like the other driver keys above.
    "webdav_url",
    "webdav_username",
    "webdav_password",
    "webdav_auth",
    "webdav_vendor",
    "webdav_accept_invalid_certs",
    "volumes_dir",
    "enabled",
    "allow_remote_admin",
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
    /// v1 whole-file GCM (frozen legacy format; reads stay supported).
    Gcm,
    /// v2 chunked AEAD (streaming + random access) — the default for
    /// new encrypted volumes.
    #[default]
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
    /// SSH/SFTP drive (ck-sftp, Phase 4 / SF1 — one server account per
    /// volume; driver wiring lands in SF3).
    Sftp,
    /// 115 open-platform drive (ck-pan115, Phase 5 / 115-1 — one 115
    /// account per volume; driver wiring lands in 115-4).
    Pan115,
    /// 123 cloud-drive via the web API (ck-pan123, Phase 6 / 123-1 — one
    /// 123pan account per volume; driver wiring lands in 123-4).
    Pan123,
    /// WebDAV share drive (ck-webdav, Phase 7 / WD1a — one server base
    /// URL per volume, sub-path = the volume root; the assembly wiring
    /// lands in WD1b, client verbs in WD2/3).
    Webdav,
}

impl Backend {
    /// Stable `config.toml` spelling of this backend.
    pub const fn as_str(self) -> &'static str {
        match self {
            Backend::Telegram => "telegram",
            Backend::Baidu => "baidu",
            Backend::Local => "local",
            Backend::Sftp => "sftp",
            Backend::Pan115 => "pan115",
            Backend::Pan123 => "pan123",
            Backend::Webdav => "webdav",
        }
    }
}

/// How a volume is exposed as a Windows drive letter (Phase 3; default
/// flipped by the 负责人 2026-09-24 裁决 / K87).
///
/// The wire names are the stable `config.toml` spellings
/// (`mount_backend = "webdav" | "winfsp"`). The default —
/// [`MountBackend::Winfsp`] — mounts drive letters in-process out of the
/// box; the legacy `net use` mapping (the Python baseline behavior) is
/// now an explicit opt-in that needs a one-time elevated
/// `cydrive fix-reg` on fresh machines and otherwise fails the first
/// claim with system error 67. Value validation is exhaustive at parse
/// time (the field is a typed enum — an unknown variant is a
/// [`ConfigError::Parse`] naming all accepted values and never reaches
/// `validate`, which therefore adds no rule for the key).
/// *Availability* is deliberately not a config concern: `"winfsp"` on a
/// machine without WinFsp (or in a binary built without the feature) is
/// a legal config value — the mount flow reports unavailability at
/// mount time (`MountBackendDecision::WinfspUnavailable`) instead of
/// rejecting the config, and a winfsp mount that fails does NOT fall
/// back to webdav (the volume stays reachable through the dashboard/
/// WebDAV; opt into `net use` by setting the key explicitly).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountBackend {
    /// In-process WinFsp native mount (Phase 3, K38/K39) — needs a
    /// `--features winfsp` build and an installed WinFsp runtime.
    #[default]
    Winfsp,
    /// `net use` drive mapping onto the process WebDAV endpoint (the
    /// Python baseline behavior; now an explicit opt-in).
    Webdav,
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
    // The dashboard's remote-administration opt-in (web volume
    // management §1.5): one switch for the ONE process dashboard.
    "allow_remote_admin",
];

/// Volume-scoped keys (Phase 2.5 / K19): everything a single storage
/// volume owns — the backend selector, all driver parameters and
/// credentials, the encryption group, db/cache/queue tuning, the drive
/// letter/mount keys, the sync group and the `enabled` switch (Phase 3.6
/// / RV0 — the persistent disable form). Legal in a volume file; a
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
    "sftp_host",
    "sftp_port",
    "sftp_username",
    "sftp_password",
    "sftp_private_key_path",
    "sftp_private_key_passphrase",
    "sftp_host_fingerprint",
    "sftp_root",
    "pan115_client_id",
    "pan115_access_token",
    "pan115_refresh_token",
    "pan115_root",
    "pan123_token",
    "pan123_root",
    // Phase 7 / WD1b: all six webdav keys are volume-scoped — one WebDAV
    // server base URL (with its credentials and tuning keys) identifies
    // exactly one storage volume.
    "webdav_url",
    "webdav_username",
    "webdav_password",
    "webdav_auth",
    "webdav_vendor",
    "webdav_accept_invalid_certs",
    "enabled",
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
/// Public (web volume management P3) so the CREATE precheck and the
/// dashboard's slug field enforce the ONE rule the volume files do.
pub fn is_valid_volume_name(name: &str) -> bool {
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

/// Keys whose VALUES are credentials — the review-M3 redaction targets.
/// A toml/serde parse-error message that mentions one of them carries
/// value text on its face (the embedded offending source line, or the
/// serde `invalid type: ... \`value\`` quote), and the message travels
/// into the boot error, the control-channel ADD reply and the tracing
/// log — so the values are masked at the `ConfigError::Parse`
/// construction points (the credential-values-never-enter-logs red
/// line). Key names and positions always stay; only values go.
///
/// Review M2 (K58): the URL keys are credential-valued too — a proxy
/// URL carries userinfo (`socks5://user:pass@host`) and a sync URL may
/// — so `proxy_url`/`sync_url` join the list (both the SHOW folding and
/// this redaction follow the one list).
const SECRET_VALUED_KEYS: &[&str] = &[
    "bot_token",
    "encryption_password",
    "sync_secret",
    "proxy_url",
    "sync_url",
    "baidu_app_key",
    "baidu_app_secret",
    "baidu_access_token",
    "baidu_refresh_token",
    // Phase 4 / SF1 (D1): both sftp credential keys — the password and
    // the private-key passphrase — ride the same masking funnel as the
    // baidu secrets (parse-error redaction + the SHOW `{"set": bool}`
    // write-only folding).
    "sftp_password",
    "sftp_private_key_passphrase",
    // Phase 5 / 115-1: both 115 token keys (the driver self-refreshes
    // the pair and persists rotations back into these keys — the K13
    // ConfigTokenStore precedent rides the baidu keys the same way).
    "pan115_access_token",
    "pan115_refresh_token",
    // Phase 6 / 123-1: the single 123pan login token (web API has no
    // refresh — the key is written once per acquisition via the K13
    // first-save chain).
    "pan123_token",
    // Phase 7 / WD1b: the WebDAV password credential (the username is
    // not secret — it labels the volume identity). Rides the same
    // masking funnel as the sftp passphrase (parse-error redaction +
    // the SHOW `{"set": bool}` write-only folding).
    "webdav_password",
];

/// Masks a credential value span: length-preserving, first and last two
/// characters visible when the span is long enough to hide anything
/// (shorter spans are all stars — a two-character head/tail of a tiny
/// value would leak most of it).
fn mask_secret_value(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    match chars.len() {
        0 => String::new(),
        n if n >= 8 => {
            let head: String = chars[..2].iter().collect();
            let tail: String = chars[n - 2..].iter().collect();
            format!("{head}{}{tail}", "*".repeat(n - 4))
        }
        n => "*".repeat(n),
    }
}

/// Byte offset of the value in a line that assigns a credential key
/// (just past the `=` that follows the key) — `None` on lines that do
/// not carry a `credential-key ... =` shape.
fn credential_assignment_value_offset(line: &str) -> Option<usize> {
    let key = SECRET_VALUED_KEYS
        .iter()
        .copied()
        .find(|key| line.contains(key))?;
    let key_end = line.find(key)? + key.len();
    let equals = line[key_end..].find('=')? + key_end;
    Some(equals + 1)
}

/// Redacts credential values from a parse-error message (review M3 —
/// the single funnel EVERY config parse error passes through: both
/// `load_volume_config` construction points AND the process-level
/// `CyDriveConfig::load_toml_with_keys` pair, so boot, the ADD reply
/// and the log share one scrubbed text). Two surfaces carry values:
///
/// - toml embeds the offending source line (`bot_token = "xxx` — a
///   broken quote runs the raw value to the line end): every line with
///   a `credential-key ... =` shape has its value span masked; an
///   unterminated multi-line string opener (`"""` / `'''`) masks
///   through the end of the message, because the credential's
///   continuation lines are indistinguishable from the tail;
/// - serde quotes the offending value in backticks on a line that need
///   not repeat the key (`invalid type: integer \`123…\``): every
///   backtick span that is long enough to be a value and is not itself
///   a known key name (``in `bot_token` `` keeps the key) is masked.
///
/// A message naming no credential key is returned verbatim — ordinary
/// syntax errors keep their full diagnostic text.
///
/// K58-M5: PUBLIC because it is the ONE scrubber — the single-funnel
/// rule. A toml parse-error surface outside this module (the cli
/// UPDATE re-read is the first) must route through this function
/// rather than grow a second redaction implementation that can drift
/// from this one.
pub fn redact_credential_values(message: &str) -> String {
    if !SECRET_VALUED_KEYS.iter().any(|key| message.contains(key)) {
        return message.to_string();
    }
    let masked_assignments = mask_credential_assignment_lines(message);
    let mut masked = mask_backtick_value_spans(&masked_assignments);
    if message.ends_with('\n') && !masked.ends_with('\n') {
        masked.push('\n');
    }
    masked
}

/// Pass 1 of [`redact_credential_values`]: mask the value span on every
/// line assigning a credential key (see the funnel's doc comment for
/// the multi-line-opener rule).
fn mask_credential_assignment_lines(message: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut masked_to_end = false;
    for line in message.lines() {
        if masked_to_end {
            lines.push(mask_secret_value(line));
            continue;
        }
        match credential_assignment_value_offset(line) {
            Some(offset) => {
                let (prefix, value) = line.split_at(offset);
                let trimmed = value.trim_start();
                if trimmed.starts_with("\"\"\"") || trimmed.starts_with("'''") {
                    masked_to_end = true;
                }
                lines.push(format!("{prefix}{}", mask_secret_value(value)));
            }
            None => lines.push(line.to_string()),
        }
    }
    lines.join("\n")
}

/// Pass 2 of [`redact_credential_values`]: mask backtick spans that
/// carry values (at least 3 characters — punctuation hints like
/// `` `"` `` survive — and not a [`KNOWN_TOML_KEYS`] spelling, so
/// ``in `bot_token` `` keeps naming the key).
fn mask_backtick_value_spans(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(open) = rest.find('`') {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + '`'.len_utf8()..];
        match after_open.find('`') {
            Some(close) => {
                let content = &after_open[..close];
                out.push('`');
                if content.chars().count() >= 3 && !KNOWN_TOML_KEYS.contains(&content) {
                    out.push_str(&mask_secret_value(content));
                } else {
                    out.push_str(content);
                }
                out.push('`');
                rest = &after_open[close + '`'.len_utf8()..];
            }
            None => {
                // An unpaired backtick (prose fragment): keep the tail.
                out.push_str(rest);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The invalid-name refusal shared by [`load_volume_config`]'s early
/// check and [`parse_volume_toml`] (one construction point — the two
/// cannot drift apart).
fn invalid_volume_name_error(name: &str, path: &Path) -> ConfigError {
    ConfigError::Invalid(format!(
        "invalid volume name `{name}`: volume names must match \
         ^[a-z][a-z0-9_-]{{0,31}}$ (a lowercase letter first, then lowercase \
         letters/digits/`_`/`-`, at most 32 characters) — rename the volume \
         file {}",
        path.display()
    ))
}

/// Parses and validates one volume-file BODY against the strict
/// volume-file surface — the extracted parse segment of
/// [`load_volume_config`] (web volume management P3: the loader now
/// reads the file and delegates here, zero drift — the existing
/// loader suite pins the shared texts). Direct callers are the
/// CREATE/UPDATE pre-write validation: `render_volume_toml` output is
/// checked through this funnel BEFORE anything touches the disk (the
/// name rules, the strict key surface with its `config.toml`
/// guidance, the typed parse — the full cross-field
/// [`CyDriveConfig::validate`] stays the caller's second step, exactly
/// like the assembly layer runs it after a file load). `path` is the
/// CALLER's target (a CREATE's not-yet-written file): it rides the
/// returned [`VolumeConfig`] and the error texts, and anchors
/// `base_dir`; it need not exist.
pub fn parse_volume_toml(path: &Path, name: &str, text: &str) -> Result<VolumeConfig, ConfigError> {
    if !is_valid_volume_name(name) {
        return Err(invalid_volume_name_error(name, path));
    }
    let path_str = path_as_str(path);
    let table: toml::Table = toml::from_str(text).map_err(|err| ConfigError::Parse {
        path: path_str.clone(),
        // Review M3: the toml error embeds the offending source line —
        // a broken-quote credential line would leak its value into the
        // ADD reply and the log otherwise.
        message: redact_credential_values(&err.to_string()),
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
        // Review M3: the serde error quotes the offending value in
        // backticks — a wrong-typed credential value must not ride the
        // message into the ADD reply or the log.
        message: redact_credential_values(&err.to_string()),
    })?;
    let base_dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    Ok(VolumeConfig {
        name: name.to_string(),
        file_path: path.to_path_buf(),
        base_dir,
        explicit_drive_letter,
        settings,
    })
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
    // The name check stays BEFORE the read (the pre-refactor error
    // ordering: a bad name reports the name even when the file is also
    // missing); parse_volume_toml re-checks it in its own funnel.
    if !is_valid_volume_name(&name) {
        return Err(invalid_volume_name_error(&name, path));
    }
    let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path_str,
        source,
    })?;
    parse_volume_toml(path, &name, &text)
}

/// Serializes one volume file's EXPLICIT configuration as the compact
/// single-line JSON the control channel's `SHOW <name>` command answers
/// with (web volume management plan §1.2 / K49 修订): the validated
/// volume name plus every key the file explicitly sets, verbatim —
/// EXCEPT the credential-valued keys ([`SECRET_VALUED_KEYS`]), which
/// collapse to `{"set": true}` / `{"set": false}` markers (write-only
/// principle: the value never leaves the backend — the reply is what
/// the dashboard's edit form prefills from). Keys the file leaves
/// unset appear nowhere (the reply shows the file, not the parsed
/// config with its defaults).
///
/// Validation is [`load_volume_config`]'s whole funnel (name rules,
/// strict key surface, typed parse) — a half-validated file never
/// serializes. The file is parsed twice (validated config + raw table
/// for the explicit keys); a SHOW is a rare, human-scale command, so
/// the double read is free.
pub fn volume_show_json(path: &Path) -> Result<String, ConfigError> {
    let spec = load_volume_config(path)?;
    let path_str = path_as_str(path);
    let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path_str.clone(),
        source,
    })?;
    let table: toml::Table = toml::from_str(&text).map_err(|err| ConfigError::Parse {
        path: path_str,
        message: redact_credential_values(&err.to_string()),
    })?;
    let mut body = serde_json::Map::new();
    body.insert("name".to_string(), serde_json::json!(spec.name));
    for (key, value) in &table {
        // Credential keys serialize once below, uniformly (present or
        // absent) — never through their values.
        if SECRET_VALUED_KEYS.contains(&key.as_str()) {
            continue;
        }
        body.insert(key.clone(), toml_value_json(value));
    }
    for key in SECRET_VALUED_KEYS {
        body.insert(
            (*key).to_string(),
            serde_json::json!({ "set": table.contains_key(*key) }),
        );
    }
    serde_json::to_string(&serde_json::Value::Object(body)).map_err(|error| {
        ConfigError::Invalid(format!(
            "serializing the volume configuration for SHOW failed: {error}"
        ))
    })
}

/// One explicit toml value as JSON. The strict key surface only admits
/// scalars, so the structural toml variants are unreachable in practice
/// — they degrade to their toml text rather than failing the reply.
fn toml_value_json(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(text) => serde_json::Value::String(text.clone()),
        toml::Value::Integer(number) => serde_json::json!(number),
        toml::Value::Float(number) if number.is_finite() => serde_json::json!(number),
        toml::Value::Boolean(flag) => serde_json::Value::Bool(*flag),
        other => serde_json::Value::String(other.to_string()),
    }
}

/// One JSON payload value as a toml value — [`toml_value_json`]'s
/// inverse (web volume management P3: the CREATE/UPDATE payload
/// conversion; pinned by a round-trip test through the SHOW
/// serializer). `Ok(None)` is the **unset** affordance (K49 分档): JSON
/// `null` and the empty string mean "this key is not being set" — a
/// CREATE writes no such key (no paved defaults) and an UPDATE
/// overlays nothing over the file's value, which is exactly the
/// write-only credential rule (`present and non-empty = overwrite,
/// missing or empty = keep`). Errors are actionable and NEVER echo the
/// offending value (the M3 discipline — the payload may carry
/// credentials): they name the key and the JSON shape it got; a float
/// passes through as a toml float and is rejected by the caller's
/// typed parse (no config key is float-shaped).
pub fn json_value_to_toml(
    key: &str,
    value: &serde_json::Value,
) -> Result<Option<toml::Value>, ConfigError> {
    use serde_json::Value as Json;
    let converted = match value {
        Json::Null => None,
        Json::String(text) if text.is_empty() => None,
        Json::String(text) => Some(toml::Value::String(text.clone())),
        Json::Bool(flag) => Some(toml::Value::Boolean(*flag)),
        Json::Number(number) => {
            if let Some(int) = number.as_i64() {
                Some(toml::Value::Integer(int))
            } else if number.as_u64().is_some() {
                // Past i64::MAX there is no toml integer to hold it (the
                // u64 range toml deliberately lacks); refuse with the key
                // named rather than silently rounding through f64.
                return Err(ConfigError::Invalid(format!(
                    "key `{key}`: the value is an integer too large for toml \
                     (i64 range at most)"
                )));
            } else {
                match number.as_f64() {
                    Some(float) => Some(toml::Value::Float(float)),
                    None => {
                        return Err(ConfigError::Invalid(format!(
                            "key `{key}`: the number has no toml representation"
                        )));
                    }
                }
            }
        }
        other => {
            return Err(ConfigError::Invalid(format!(
                "key `{key}`: the value must be a toml scalar (a string, a whole \
                 number or a boolean), not a JSON {}",
                json_shape_name(other),
            )));
        }
    };
    Ok(converted)
}

/// The JSON shape word for [`json_value_to_toml`]'s refusals ("array" /
/// "object" — the only shapes that reach it; null, strings, numbers and
/// booleans all convert).
fn json_shape_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
        _ => "value",
    }
}

/// Renders a volume file's EXPLICIT keys back as toml — the controlled
/// rewrite primitive behind ENABLE/DISABLE (web volume management plan
/// §1.2's narrow UPDATE form; CREATE/UPDATE in P3/P4 reuse it). The
/// explicit keys render in the file's own order (one `key = value` per
/// line, values through toml's own value serialization), an overrides
/// key REPLACES its explicit twin in place or APPENDS when the file
/// never set it, and **no default keys are paved** — the same philosophy
/// as the setup hand-written template ([`CyDriveConfig::save_toml`]'s
/// 29-key paving would drown a hand-maintained volume file). Comments
/// are lost (裁决①: accepted, callers must surface it); the output
/// re-parses through [`load_volume_config`] (pinned by tests).
pub fn render_volume_toml(explicit: &toml::Table, overrides: &toml::Table) -> String {
    let mut text = String::new();
    for (key, value) in explicit {
        let value = overrides.get(key).unwrap_or(value);
        text.push_str(&format!("{key} = {value}\n"));
    }
    for (key, value) in overrides {
        if !explicit.contains_key(key) {
            text.push_str(&format!("{key} = {value}\n"));
        }
    }
    text
}

/// Writes a config file (volume file or `config.toml`) with no torn
/// intermediate state on disk (K58-H3): a plain `fs::write` truncates
/// the target in place, so a crash or power loss mid-write leaves a
/// truncated file — for a volume file that means either invalid TOML
/// (every instance of the volume refuses to boot) or, worse, a tear at
/// a line boundary that silently DROPS trailing keys (`enable_encryption`
/// / `enabled` / a credential flipped away — the file re-parses but with
/// different semantics). The atomic sequence instead writes the full
/// content to a sibling temp file (the fixed `.<name>.tmp` pattern, a
/// stale leftover from a crashed earlier attempt removed first),
/// fsyncs it, then `std::fs::rename`s it over the target — on Windows
/// (`MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`) and POSIX alike the
/// rename is an atomic replace, so a concurrent reader sees either the
/// whole previous file or the whole new one, never a middle. A failure
/// before or at the rename leaves the ORIGINAL untouched and removes
/// the temp file this call created (a crash between write and rename
/// leaves at most the temp file, which the next attempt's pre-delete
/// clears).
///
/// The one atomic-write primitive every config write goes through:
/// [`write_volume_enabled`], the cli's CREATE/UPDATE volume writes and
/// [`CyDriveConfig::save_toml`]/[`CyDriveConfig::save_toml_scrubbed`]
/// (the review's 顺带 convergence — a torn `config.toml` is the same
/// boot-refusing failure).
pub fn write_config_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("config path {} has no file name", path.display()),
        )
    })?;
    let temp_path = path.with_file_name(format!(".{}.tmp", name.to_string_lossy()));
    // Best-effort pre-delete of a crashed attempt's leftover: a stubborn
    // leftover resurfaces as the create error below (never silently
    // ignored past that point).
    let _ = fs::remove_file(&temp_path);
    let mut file = fs::File::create(&temp_path)?;
    let written = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    match fs::rename(&temp_path, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp_path);
            Err(error)
        }
    }
}

/// The ENABLE/DISABLE file write (web volume management §1.2): rewrite
/// one volume file with the `enabled` flag overlaid through
/// [`render_volume_toml`]. The file is validated through the FULL
/// [`load_volume_config`] funnel first — a half-validated file is never
/// "repaired" by a rewrite that cannot know what it would destroy — and
/// the raw table is then re-read for the render (the SHOW serializer's
/// double-read precedent; a toggle is a rare, human-scale command).
pub fn write_volume_enabled(path: &Path, enabled: bool) -> Result<(), ConfigError> {
    load_volume_config(path)?;
    let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path_as_str(path),
        source,
    })?;
    let explicit: toml::Table = toml::from_str(&text).map_err(|err| ConfigError::Parse {
        path: path_as_str(path),
        message: redact_credential_values(&err.to_string()),
    })?;
    let mut overrides = toml::Table::new();
    overrides.insert("enabled".to_string(), toml::Value::Boolean(enabled));
    write_config_atomically(path, &render_volume_toml(&explicit, &overrides))?;
    Ok(())
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
/// default `"Y:"` is a placeholder, not a mount claim. A volume carrying
/// `enabled = false` (Phase 3.6 / RV0) is skipped with an info note — it
/// joins neither the conflict check nor the returned set, which is the
/// persistent disable form (K49; re-enabling is hand-editing the file).
/// The returned order is the stable file-name order of
/// [`discover_volumes`].
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
        if !volume.settings.enabled {
            tracing::info!(
                volume = %volume.name,
                file = %volume.file_path.display(),
                "volume is disabled (enabled = false in its volume file); skipping it — \
                 no drive-letter claim, no assembly, no /vol route"
            );
            continue;
        }
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

/// Default `enabled` (Phase 3.6 / RV0): absent key = the volume takes
/// part in assembly — the semantic natural, not a compatibility
/// consideration.
fn default_enabled() -> bool {
    true
}

/// `skip_serializing_if` predicate for `enabled`: the default (`true`)
/// is omitted from serialized files — a process config written by
/// `setup` must never carry the volume-scoped key (the K19 mixing guard
/// would reject its own output in multi-volume mode) — while an
/// explicit `false` (the disable) survives the round-trip.
fn is_enabled(value: &bool) -> bool {
    *value
}

/// `skip_serializing_if` predicate for `allow_remote_admin`: the default
/// (`false`, the safe stance) is omitted — a `setup`-written process
/// config stays byte-identical to the pre-key form; an explicit `true`
/// (the remote-administration opt-in) survives the round-trip.
fn is_default_allow_remote_admin(value: &bool) -> bool {
    !*value
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
    /// Whether the dashboard's volume-management WRITE routes
    /// (`POST /api/volumes/<name>/{remove,enable,disable}`) stay enabled
    /// when the web UI binds a non-loopback `web_ui_host` (web volume
    /// management plan §1.5). A **process-scoped** key (K19 partition:
    /// one switch for the ONE process dashboard). The default — `false`
    /// — is the safe stance: a remotely reachable dashboard refuses
    /// volume mutations (403 naming this key) unless the operator
    /// explicitly opts in, so binding `0.0.0.0` for read-only browsing
    /// never silently opens volume administration to the network.
    /// `validate` adds no rule — a bool has no invalid value.
    /// Serialization emits the key only when `true` (a `setup`-written
    /// process config stays byte-identical to the pre-key form).
    #[serde(default, skip_serializing_if = "is_default_allow_remote_admin")]
    pub allow_remote_admin: bool,
    /// Windows drive letter to mount, canonical form `"X:"`.
    pub drive_letter: String,
    /// Whether to auto-mount the drive on startup.
    pub auto_mount_drive: bool,
    /// How to expose the drive on Windows (Phase 3; default flipped by
    /// the 负责人 2026-09-24 裁决 / K87): [`MountBackend::Winfsp`] (the
    /// default — in-process native mount; K89 makes it a default-feature
    /// build, still needs an installed WinFsp runtime) or
    /// [`MountBackend::Webdav`] (the `net use` mapping — needs the WebDAV
    /// endpoint up and a one-time elevated `cydrive fix-reg` on fresh
    /// machines, system error 67 otherwise). A winfsp mount that cannot
    /// run or fails does NOT fall back to webdav: the volume stays
    /// reachable through the dashboard/WebDAV and the failure is
    /// announced with actionable hints
    /// (`MountBackendDecision::WinfspUnavailable`). A **process-scoped**
    /// key (K19 partition): it governs every volume's mount in one
    /// process. Value validation is exhaustive at parse time (typed
    /// enum — see [`MountBackend`]).
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
    /// `"aead_v2"` (chunked streaming v2 — the default; supports Range
    /// streaming reads, K47) or `"gcm"` (frozen v1 whole-file legacy,
    /// reads only). Read paths dispatch on the per-row scheme in the
    /// metadata DB, never on this key. Value validation is exhaustive
    /// at parse time (the field is a typed enum — an unknown variant is
    /// a [`ConfigError::Parse`] naming both accepted values and never
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
    /// SSH server hostname / address (ck-sftp, Phase 4 / SF1). Required
    /// when `backend = "sftp"`; inert otherwise.
    pub sftp_host: Option<String>,
    /// SSH server TCP port; `None` = the SSH default 22 (the driver's
    /// `DEFAULT_PORT`). `validate` rejects an explicit `0` (the
    /// 1..=65535 rule — values above 65535 cannot parse into `u16` and
    /// fail earlier, at the typed parse).
    pub sftp_port: Option<u16>,
    /// SSH login user name. Required when `backend = "sftp"`; inert
    /// otherwise.
    pub sftp_username: Option<String>,
    /// SSH password credential (D1: password OR private key — at least
    /// one). A [`SECRET_VALUED_KEYS`] member (R3): masked in parse
    /// errors, folded to `{"set": bool}` in SHOW replies.
    pub sftp_password: Option<String>,
    /// Filesystem path of the SSH private key (D1's second auth form;
    /// the path itself is not a credential, only the key file's content
    /// is).
    pub sftp_private_key_path: Option<String>,
    /// Passphrase unlocking an encrypted [`Self::sftp_private_key_path`]
    /// (D1: an encrypted key without its passphrase is half support).
    /// A [`SECRET_VALUED_KEYS`] member (R3).
    pub sftp_private_key_passphrase: Option<String>,
    /// Server host-key fingerprint (D2: explicit accept + pinned
    /// fingerprint). `SHA256:...` OpenSSH form (the connecting error
    /// message spells the server's actual fingerprint to copy here);
    /// `None` = not yet accepted — the driver REFUSES to connect and
    /// the error names the acceptance path (never a silent TOFU, never
    /// an unconditional accept).
    pub sftp_host_fingerprint: Option<String>,
    /// SFTP drive root as a backend-absolute path (must start with `'/'`
    /// when the backend is `sftp`; `None` = the server filesystem root
    /// `/`). Mirrors the `baidu_root`/`local_root` naming convention.
    pub sftp_root: Option<String>,
    /// 115 open-platform app identity for the QR scan (ck-pan115,
    /// Phase 5 / 115-1). **Non-secret** (K65 route C / K69.1: the
    /// device-code PKCE flow needs no app secret; the default is the
    /// public OpenList-managed client_id `100197303`, injected by the
    /// driver when this key is absent — hence NOT in
    /// [`SECRET_VALUED_KEYS`]). Overriding it is the recovery path when
    /// the bound app identity gets banned (re-scan under a new identity).
    pub pan115_client_id: Option<String>,
    /// 115 access token (ck-pan115, Phase 5 / 115-1). Required when
    /// `backend = "pan115"`; inert otherwise. A [`SECRET_VALUED_KEYS`]
    /// member (R3). The driver refreshes and rotates the pair at
    /// runtime, persisting rotations back into this key and
    /// [`Self::pan115_refresh_token`] (the K13 ConfigTokenStore
    /// precedent).
    pub pan115_access_token: Option<String>,
    /// 115 refresh token — the self-sustaining half of the pair (a
    /// refresh response returns a NEW pair and the old refresh token
    /// dies immediately; K69.1). A [`SECRET_VALUED_KEYS`] member (R3).
    pub pan115_refresh_token: Option<String>,
    /// 115 drive root as a numeric folder id (ck-pan115, Phase 5 /
    /// 115-1; D3: settable, `None` = the netdisk root `"0"`, the
    /// driver's own default). Must be all digits when present —
    /// `validate` rejects anything else.
    pub pan115_root: Option<String>,
    /// 123 cloud-drive login token (ck-pan123, Phase 6 / 123-1). Required
    /// when `backend = "pan123"`; inert otherwise. A
    /// [`SECRET_VALUED_KEYS`] member (R3). The web API has **no refresh
    /// mechanism** (K76.4) — the token (90 days) comes from the setup QR
    /// scan or a password sign-in and is only re-persisted on a fresh
    /// acquisition (the "first save" of the K13 chain).
    pub pan123_token: Option<String>,
    /// 123 drive root as a numeric folder id (ck-pan123, Phase 6 / 123-1;
    /// D3沿用 Phase 5 拍板形态: settable, `None` = the netdisk root
    /// `"0"`, the driver's own default). Must be all digits when
    /// present — `validate` rejects anything else.
    pub pan123_root: Option<String>,
    /// WebDAV server base URL (ck-webdav, Phase 7 / WD1a). Required when
    /// `backend = "webdav"`; inert otherwise. The full http(s) URL of the
    /// share — the sub-path becomes the volume root (there is no separate
    /// root key); a trailing slash is tolerated (the driver normalises).
    /// No filesystem meaning: it is never rebased against the volume home.
    pub webdav_url: Option<String>,
    /// WebDAV auth user name; must be set as a pair with
    /// [`Self::webdav_password`] (both absent = anonymous access).
    pub webdav_username: Option<String>,
    /// WebDAV auth password (D1: Basic pre-empt + Digest 401 negotiation).
    /// A [`SECRET_VALUED_KEYS`] member (R3): masked in parse errors,
    /// folded to `{"set": bool}` in SHOW replies; env route
    /// `CYDRIVE_WEBDAV_PASSWORD`.
    pub webdav_password: Option<String>,
    /// Auth shape selector: `"auto"` (the default — Basic first, then
    /// Digest negotiation on 401), `"basic"` or `"digest"`. `validate`
    /// enforces the value domain (lenient casing/whitespace) when the
    /// backend is webdav; the driver re-checks on its map face.
    pub webdav_auth: Option<String>,
    /// Server flavour: `"generic"` (the default — read-only mtime, the
    /// D2 downgrade) or `"nextcloud"` (PUT carries `X-OC-Mtime`).
    /// Only tunes the mtime write strategy; `validate` enforces the two
    /// values when the backend is webdav.
    pub webdav_vendor: Option<String>,
    /// Accept self-signed TLS certificates (D3; default `false` = the
    /// rustls strict posture). A typed bool — the value domain is
    /// exhaustive at parse time (a non-bool toml value is a `Parse`
    /// error), so `validate` adds no rule for it; the driver's map face
    /// does the lenient `"true"/"false"` string parsing (trim + case).
    pub webdav_accept_invalid_certs: Option<bool>,
    /// Volumes directory (Phase 2.5 / K19), relative to the process
    /// working directory: one `<name>.toml` file per storage volume
    /// (see [`load_volume_config`]). `None` — the default — means
    /// single-volume mode: every pre-Phase-2.5 behaviour is
    /// byte-compatible. When set, the process-level `config.toml` may
    /// carry only [`PROCESS_SCOPED_KEYS`] (mixing is a validation
    /// error) and the directory must exist and hold at least one
    /// volume file.
    pub volumes_dir: Option<String>,
    /// Whether this storage volume takes part in assembly (Phase 3.6 /
    /// RV0, K49). A **volume-scoped** key (K19 partition): legal in a
    /// volume file, rejected in the process `config.toml` by the mixing
    /// guard. The default — `true` — is the semantic natural (absent
    /// key = enabled), so volume files written before RV0 behave exactly
    /// as before. `false` is the persistent form of "disabled":
    /// [`load_volumes`] skips the volume at discovery with an info note —
    /// it joins no drive-letter conflict check, no assembly, no banner,
    /// no `/vol/<name>` route; re-enabling is hand-editing the key back
    /// (runtime add/remove is the RV2 control channel's orthogonal
    /// mechanism and never touches the file). `validate` adds no rule —
    /// a bool has no invalid value. Serialization emits the key only
    /// when `false`, so a process config saved by `setup` never carries
    /// it (the K19 mixing guard would reject its own output otherwise).
    #[serde(default = "default_enabled", skip_serializing_if = "is_enabled")]
    pub enabled: bool,
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
            webdav_port: 8485,
            web_ui_host: "127.0.0.1".to_string(),
            web_ui_port: 8486,
            enable_web_ui: true,
            allow_remote_admin: false,
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
            sftp_host: None,
            sftp_port: None,
            sftp_username: None,
            sftp_password: None,
            sftp_private_key_path: None,
            sftp_private_key_passphrase: None,
            sftp_host_fingerprint: None,
            sftp_root: None,
            pan115_client_id: None,
            pan115_access_token: None,
            pan115_refresh_token: None,
            pan115_root: None,
            pan123_token: None,
            pan123_root: None,
            webdav_url: None,
            webdav_username: None,
            webdav_password: None,
            webdav_auth: None,
            webdav_vendor: None,
            webdav_accept_invalid_certs: None,
            volumes_dir: None,
            enabled: default_enabled(),
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
            // Review M3 follow-up: the same redaction the volume-file
            // loader applies — the toml error embeds the offending
            // source line, and a broken-quote credential line would
            // leak its value into the boot error otherwise.
            message: redact_credential_values(&err.to_string()),
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
            // Review M3 follow-up: the serde error quotes the offending
            // value in backticks — a wrong-typed credential value must
            // not ride the message into the boot error.
            message: redact_credential_values(&err.to_string()),
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
        write_config_atomically(path, &text)?;
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
        write_config_atomically(path, &text)?;
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
                message: redact_credential_values(&err.to_string()),
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
        // 复审 M2（凭据泄漏）：serde_json 的类型错误会原样引用值
        // （「invalid type: integer `…`」）且不携带键名——脱敏漏斗按键名
        // 触发、无从命中。凭据键的值只允许字符串/null：错误类型提前
        // 拒绝，消息只报键名，值不进入。
        if let serde_json::Value::Object(map) = &root {
            for key in SECRET_VALUED_KEYS {
                if let Some(value) = map.get(*key) {
                    if !value.is_string() && !value.is_null() {
                        return Err(ConfigError::Parse {
                            path: path_str.clone(),
                            message: format!(
                                "credential key `{key}` must be a string (the supplied \
                                 value is redacted)"
                            ),
                        });
                    }
                }
            }
        }
        // Derived `Deserialize` on `CyDriveConfig`: unknown keys ignored,
        // missing fields defaulted (`#[serde(default)]`), wrong types error.
        serde_json::from_value(root).map_err(|err| ConfigError::Parse {
            path: path_str,
            message: redact_credential_values(&err.to_string()),
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
    /// | `CYDRIVE_SFTP_PASSWORD` | `sftp_password` | same empty-clears rule (review fix: rides this chain so the K28 multi-volume skip applies — volumes never see env credentials) |
    /// | `CYDRIVE_SFTP_PRIVATE_KEY_PASSPHRASE` | `sftp_private_key_passphrase` | same empty-clears rule |
    /// | `CYDRIVE_PAN115_ACCESS_TOKEN` | `pan115_access_token` | same empty-clears rule (Phase 5 / 115-1: the K28 multi-volume skip applies the same way) |
    /// | `CYDRIVE_PAN115_REFRESH_TOKEN` | `pan115_refresh_token` | same empty-clears rule |
    /// | `CYDRIVE_PAN123_TOKEN` | `pan123_token` | same empty-clears rule (Phase 6 / 123-1: the K28 multi-volume skip applies the same way) |
    /// | `CYDRIVE_WEBDAV_PASSWORD` | `webdav_password` | same empty-clears rule (Phase 7 / WD1b: the credential rides this chain — the assembly NEVER reads env directly, B-M1) |
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
        // Review fix (K28 alignment): the two sftp credential keys ride the
        // same env > file chain as the baidu ones — the multi-volume
        // discovery path never calls this method, so volume files are
        // immune to cross-volume env bleed by construction.
        if let Some(value) = env_string("CYDRIVE_SFTP_PASSWORD") {
            config.sftp_password = (!value.is_empty()).then_some(value);
        }
        if let Some(value) = env_string("CYDRIVE_SFTP_PRIVATE_KEY_PASSPHRASE") {
            config.sftp_private_key_passphrase = (!value.is_empty()).then_some(value);
        }
        // Phase 5 / 115-1: the two 115 token keys ride the same env >
        // file chain as the baidu/sftp ones — the multi-volume
        // discovery path never calls this method, so volume files are
        // immune to cross-volume env bleed by construction (K28).
        if let Some(value) = env_string("CYDRIVE_PAN115_ACCESS_TOKEN") {
            config.pan115_access_token = (!value.is_empty()).then_some(value);
        }
        if let Some(value) = env_string("CYDRIVE_PAN115_REFRESH_TOKEN") {
            config.pan115_refresh_token = (!value.is_empty()).then_some(value);
        }
        // Phase 6 / 123-1: the 123pan token rides the same env > file
        // chain as the baidu/sftp/pan115 ones — the multi-volume
        // discovery path never calls this method, so volume files are
        // immune to cross-volume env bleed by construction (K28).
        if let Some(value) = env_string("CYDRIVE_PAN123_TOKEN") {
            config.pan123_token = (!value.is_empty()).then_some(value);
        }
        // Phase 7 / WD1b: the webdav password rides the same env > file
        // chain as the baidu/sftp/pan115/pan123 credentials — the
        // multi-volume discovery path never calls this method, so volume
        // files are immune to cross-volume env bleed by construction
        // (K28); the assembly reads it off the resolved config only
        // (B-M1: no direct env read at the assembly point).
        if let Some(value) = env_string("CYDRIVE_WEBDAV_PASSWORD") {
            config.webdav_password = (!value.is_empty()).then_some(value);
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
    ///   requires `local_root` set and absolute; `backend = "sftp"`
    ///   requires `sftp_host` and `sftp_username` non-empty, at least
    ///   one of `sftp_password` / `sftp_private_key_path` (D1), an
    ///   explicit `sftp_port` in `1..=65535` and — when present — an
    ///   `sftp_root` starting with `'/'` (`sftp_host_fingerprint` is
    ///   deliberately optional: refusing to connect until the operator
    ///   pins a fingerprint is the driver's D2 job, not a config rule).
    ///   The telegram default triggers none of these rules —
    ///   pre-Phase-2 configs validate unchanged.
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
                // M3（复审）同族：sync_url 无 userinfo 拒收臂，内嵌凭据
                // 形态可穿过 host 检查——两臂同样不回显原文（R3）。
                return Err(ConfigError::Invalid(
                    "sync_url must start with http:// or https://".to_string(),
                ));
            };
            // Minimal authority parse (no url crate in core): the
            // authority runs to the first '/', '?' or '#', the host to
            // the first ':' (the port) or the authority's end. A scheme
            // with no host — "http://", "http://:8290/x", "http:///path"
            // — is not a reachable server and must fail loudly here.
            let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
            let host = authority.split(':').next().unwrap_or_default();
            if host.is_empty() {
                return Err(ConfigError::Invalid(
                    "sync_url needs a host, e.g. \"http://sync.example.org:8290\"".to_string(),
                ));
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
        // Phase 4 / SF1 cross-field rules (gated on the backend like the
        // baidu/local blocks above — the telegram default never fires).
        if self.backend == Backend::Sftp {
            for (name, value) in [
                ("sftp_host", &self.sftp_host),
                ("sftp_username", &self.sftp_username),
            ] {
                if value.as_deref().is_none_or(str::is_empty) {
                    return Err(ConfigError::Invalid(format!(
                        "backend = \"sftp\" requires {name}: set it in config.toml \
                         (the SSH server address and the login user)"
                    )));
                }
            }
            let has_password = self.sftp_password.as_deref().is_some_and(|v| !v.is_empty());
            let has_key = self
                .sftp_private_key_path
                .as_deref()
                .is_some_and(|v| !v.is_empty());
            if !has_password && !has_key {
                return Err(ConfigError::Invalid(
                    "backend = \"sftp\" requires an auth credential: set sftp_password or \
                     sftp_private_key_path in config.toml (password or private key — at least \
                     one; the optional sftp_private_key_passphrase unlocks an encrypted key)"
                        .to_string(),
                ));
            }
            if let Some(port) = self.sftp_port {
                if !(1..=65535).contains(&port) {
                    return Err(ConfigError::Invalid(format!(
                        "sftp_port must be in 1..=65535, got {port}"
                    )));
                }
            }
            if let Some(root) = &self.sftp_root {
                if !root.starts_with('/') {
                    return Err(ConfigError::Invalid(format!(
                        "sftp_root must be a backend-absolute path starting with '/', e.g. \
                         \"/srv/sftp\", got {root:?}"
                    )));
                }
            }
        }
        // Phase 5 / 115-1 cross-field rules (gated on the backend like
        // the baidu/local/sftp blocks above — the telegram default
        // never fires them).
        if self.backend == Backend::Pan115 {
            for (name, value, env) in [
                (
                    "pan115_access_token",
                    &self.pan115_access_token,
                    "CYDRIVE_PAN115_ACCESS_TOKEN",
                ),
                (
                    "pan115_refresh_token",
                    &self.pan115_refresh_token,
                    "CYDRIVE_PAN115_REFRESH_TOKEN",
                ),
            ] {
                if value.as_deref().is_none_or(str::is_empty) {
                    return Err(ConfigError::Invalid(format!(
                        "backend = \"pan115\" requires {name}: obtain the initial pair via the \
                         setup QR scan (or a hand-filled token pair), then set it in config.toml \
                         or export {env}"
                    )));
                }
            }
            if let Some(root) = &self.pan115_root {
                if root.is_empty() || !root.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(ConfigError::Invalid(format!(
                        "pan115_root must be a numeric 115 folder id (\"0\" is the netdisk \
                         root), got {root:?}"
                    )));
                }
            }
        }
        // Phase 6 / 123-1 cross-field rules (gated on the backend like
        // the baidu/local/sftp/pan115 blocks above — the telegram
        // default never fires them).
        if self.backend == Backend::Pan123 {
            if self.pan123_token.as_deref().is_none_or(str::is_empty) {
                return Err(ConfigError::Invalid(
                    "backend = \"pan123\" requires pan123_token: obtain the initial token via \
                     the setup QR scan (or a password sign_in), then set it in config.toml or \
                     export CYDRIVE_PAN123_TOKEN"
                        .to_string(),
                ));
            }
            if let Some(root) = &self.pan123_root {
                if root.is_empty() || !root.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(ConfigError::Invalid(format!(
                        "pan123_root must be a numeric 123pan folder id (\"0\" is the netdisk \
                         root), got {root:?}"
                    )));
                }
            }
        }
        // Phase 7 / WD1b cross-field rules (gated on the backend like the
        // baidu/local/sftp/pan115/pan123 blocks above — the telegram
        // default never fires them). The rules mirror
        // `ck_webdav::config::parse_from_map` (the driver's second gate)
        // at the string level core can check without the url crate: the
        // full URL normalisation (trailing slash, percent forms) stays on
        // the driver side — a config that passes here must parse there.
        if self.backend == Backend::Webdav {
            let url = self.webdav_url.as_deref().unwrap_or("").trim();
            if url.is_empty() {
                return Err(ConfigError::Invalid(
                    "backend = \"webdav\" requires webdav_url: set the full http(s) URL of the \
                     WebDAV share in config.toml (the sub-path becomes the volume root), e.g. \
                     https://nas.lan:5006/dav/"
                        .to_string(),
                ));
            }
            // Minimal authority parse (the sync_url precedent — no url
            // crate in core): scheme, non-empty host, no query/fragment.
            let Some(rest) = url
                .strip_prefix("http://")
                .or_else(|| url.strip_prefix("https://"))
            else {
                // M3（复审 2026-09-25）：四臂文案一律不回显原文——URL 可
                // 内嵌凭据，而 Invalid 不经 redact_credential_values 漏斗
                //（脱敏只挂 Parse 构造点），回显即密码进错误链（R3）。
                return Err(ConfigError::Invalid(
                    "webdav_url must start with http:// or https:// (a WebDAV share is a \
                     plain http(s) URL, e.g. https://nas.lan:5006/dav/)"
                        .to_string(),
                ));
            };
            if url.contains(['?', '#']) {
                return Err(ConfigError::Invalid(
                    "webdav_url must not carry a query or fragment (the share URL is a plain \
                     http(s) path, e.g. https://nas.lan:5006/dav/)"
                        .to_string(),
                ));
            }
            let authority = rest.split('/').next().unwrap_or_default();
            // WD4 挂账①：userinfo 形态（`https://user:pass@host/`）会把
            // 凭据带进 SHOW 回显与 sync namespace（卷身份携带完整 base
            // URL）——第一道漏斗拒收并指路凭据键（驱动 parse_from_map
            // 是第二道）。
            if authority.contains('@') {
                return Err(ConfigError::Invalid(
                    "webdav_url must not embed credentials as userinfo (user:pass@host): set \
                     webdav_username and webdav_password instead — the separate keys keep the \
                     credentials out of the volume URL"
                        .to_string(),
                ));
            }
            let host = authority.split(':').next().unwrap_or_default();
            if host.is_empty() {
                return Err(ConfigError::Invalid(
                    "webdav_url needs a host, e.g. \"https://nas.lan:5006/dav/\"".to_string(),
                ));
            }
            // Credentials arrive as a pair (both absent = anonymous).
            let has_username = self
                .webdav_username
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty());
            let has_password = self
                .webdav_password
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty());
            if has_username != has_password {
                return Err(ConfigError::Invalid(
                    "webdav_username and webdav_password must be set as a pair: only one of \
                     them is set — add the other, or remove both for anonymous access"
                        .to_string(),
                ));
            }
            // Enum keys: lenient casing/whitespace (the driver's map face
            // parses the same trio). Empty values read as unset.
            if let Some(value) = self.webdav_auth.as_deref().filter(|v| !v.trim().is_empty()) {
                if !matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "auto" | "basic" | "digest"
                ) {
                    return Err(ConfigError::Invalid(format!(
                        "webdav_auth must be one of auto, basic, digest, got {value:?} \
                         (auto = Basic first, then Digest negotiation on 401)"
                    )));
                }
            }
            if let Some(value) = self
                .webdav_vendor
                .as_deref()
                .filter(|v| !v.trim().is_empty())
            {
                if !matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "generic" | "nextcloud"
                ) {
                    return Err(ConfigError::Invalid(format!(
                        "webdav_vendor must be one of generic, nextcloud, got {value:?} \
                         (vendor only tunes the mtime write strategy)"
                    )));
                }
            }
            // webdav_accept_invalid_certs: a typed bool — the serde parse
            // is exhaustive (a non-bool value is a Parse error), so no
            // rule here (the enable_encryption precedent).
        }
        Ok(())
    }
}
