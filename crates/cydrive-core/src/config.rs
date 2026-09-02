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
use std::path::Path;

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
const KNOWN_TOML_KEYS: &[&str] = &[
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
    "chunk_size_mb",
    "cache_limit_gb",
    "encryption_password",
    "enable_encryption",
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
    /// Upload chunk size in MB (Telegram per-message limit with margin,
    /// valid range 1..=2000).
    pub chunk_size_mb: u64,
    /// Cache capacity in GB.
    pub cache_limit_gb: u64,
    /// Client-side encryption password; must be set and non-empty when
    /// `enable_encryption` is `true`.
    pub encryption_password: Option<String>,
    /// Whether client-side encryption is enabled.
    pub enable_encryption: bool,
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
            chunk_size_mb: 1900,
            cache_limit_gb: 20,
            encryption_password: None,
            enable_encryption: false,
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
        let config: CyDriveConfig = table.try_into().map_err(|err| ConfigError::Parse {
            path: path_str,
            message: err.to_string(),
        })?;
        Ok(config)
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

    /// Serializes `self` with the two secret fields scrubbed and writes it
    /// to `path`: `bot_token` becomes an empty string, the optional
    /// `encryption_password` is omitted entirely, and a header comment
    /// points readers at the OS credential store (keyring service
    /// `"cydrive"`); the secrets re-enter the config at load time via
    /// [`CyDriveConfig::with_credential_backfill`].
    ///
    /// This is the write side of the credential vault (M5): on-disk config
    /// files stay non-sensitive while `enable_encryption` and every other
    /// field round-trip unchanged. Directory creation matches
    /// [`CyDriveConfig::save_toml`].
    pub fn save_toml_scrubbed(&self, path: &Path) -> Result<(), ConfigError> {
        let mut scrubbed = self.clone();
        scrubbed.bot_token = String::new();
        scrubbed.encryption_password = None;
        let body = toml::to_string_pretty(&scrubbed).map_err(|err| ConfigError::Parse {
            path: path_as_str(path),
            message: err.to_string(),
        })?;
        let text = format!(
            "# Secrets live in the OS credential manager (keyring service \"{SERVICE}\"), \
             not in this file:\n# the bot token and the encryption password are \
             intentionally empty here and return via the credential store.\n{body}"
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
        Ok(())
    }
}
