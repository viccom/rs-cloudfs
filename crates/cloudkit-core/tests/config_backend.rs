//! RED-phase tests for the multi-backend config keys of
//! `cloudkit_core::config` (Phase 2 / K17, docs/plans/
//! 2026-09-08-phase2-execution.md §6).
//!
//! Contract under test — three-place key sync (interfaces §4):
//!
//! - `backend` (`"telegram"` | `"baidu"` | `"local"`, default
//!   `"telegram"`): the default keeps every pre-Phase-2 config
//!   byte-compatible (a config without the key behaves exactly as
//!   before). An unknown value is rejected with an actionable error
//!   naming the accepted values; cross-field rules fire per backend
//!   (`backend = "baidu"` requires the K14 credential keys,
//!   `backend = "local"` requires an absolute `local_root`).
//! - `baidu_root` (`String`, default `/apps/cloudfs`): backend-absolute
//!   path, must start with `'/'`.
//! - K14 credential keys `baidu_app_key` / `baidu_app_secret` /
//!   `baidu_access_token` / `baidu_refresh_token` (`Option<String>`,
//!   default `None`): env overrides
//!   `CYDRIVE_BAIDU_APP_KEY/APP_SECRET/ACCESS_TOKEN/REFRESH_TOKEN`
//!   apply env > file; a set-but-empty variable clears the value back
//!   to `None` (the proxy_url/sync_url precedent).
//! - `local_root` (`Option<String>`, default `None`): the local
//!   backend's volume root; must be present and absolute when
//!   `backend = "local"`.
//!
//! All seven keys ride the canonical `config.toml` (KNOWN_TOML_KEYS
//! grows) and are **rejected** by the legacy `config.json`
//! (LEGACY_REJECTED_KEYS grows — tier-1 precedent: a key the legacy
//! loader cannot honour must fail loudly).

use std::fs;
use std::sync::{Mutex, MutexGuard};

use cloudkit_core::config::{Backend, ConfigError, CyDriveConfig};

// ------------------------------------------------------------- helpers ---

/// Every credential env key these tests touch (isolated against ambient env):
/// the four CYDRIVE_BAIDU_* overrides, the two CYDRIVE_SFTP_* ones
/// (review fix: the sftp keys ride the same `with_env_overrides` chain, so
/// the K28 multi-volume skip covers them — volumes never see env values),
/// the two CYDRIVE_PAN115_* ones (Phase 5) and CYDRIVE_PAN123_TOKEN
/// (Phase 6 / 123-1 — same chain).
const BAIDU_ENV_KEYS: &[&str] = &[
    "CYDRIVE_BAIDU_APP_KEY",
    "CYDRIVE_BAIDU_APP_SECRET",
    "CYDRIVE_BAIDU_ACCESS_TOKEN",
    "CYDRIVE_BAIDU_REFRESH_TOKEN",
    "CYDRIVE_SFTP_PASSWORD",
    "CYDRIVE_SFTP_PRIVATE_KEY_PASSPHRASE",
    "CYDRIVE_PAN115_ACCESS_TOKEN",
    "CYDRIVE_PAN115_REFRESH_TOKEN",
    "CYDRIVE_PAN123_TOKEN",
    "CYDRIVE_WEBDAV_PASSWORD",
];

/// Serialises env-touching tests (same precedent as `tests/config.rs`).
static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Holds [`ENV_MUTEX`] and clears every baidu env key on drop.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for key in BAIDU_ENV_KEYS {
            std::env::remove_var(key);
        }
    }
}

fn env_guard() -> EnvGuard {
    let _lock = ENV_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for key in BAIDU_ENV_KEYS {
        std::env::remove_var(key);
    }
    EnvGuard { _lock }
}

/// A minimal valid baidu config: backend set + all four K14 keys present
/// (short dummy values — the scanner gate only matches 20+ char literals).
fn baidu_config() -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Baidu,
        baidu_app_key: Some("test-key".to_string()),
        baidu_app_secret: Some("test-secret".to_string()),
        baidu_access_token: Some("test-access".to_string()),
        baidu_refresh_token: Some("test-refresh".to_string()),
        ..CyDriveConfig::default()
    }
}

/// A minimal valid local config (tempdir roots are absolute).
fn local_config(local_root: &str) -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Local,
        local_root: Some(local_root.to_string()),
        ..CyDriveConfig::default()
    }
}

// -------------------------------------------------------------- tests ---

#[test]
fn defaults_are_telegram_and_unchanged() {
    // The compatibility contract (K17): a config that predates Phase 2
    // keeps validating and carries the exact pre-Phase-2 semantics —
    // backend = telegram, baidu keys unset, local_root unset.
    let cfg = CyDriveConfig::default();
    assert_eq!(cfg.backend, Backend::Telegram, "default backend");
    assert_eq!(cfg.baidu_root, "/apps/cloudfs", "default baidu_root");
    assert_eq!(cfg.baidu_app_key, None);
    assert_eq!(cfg.baidu_app_secret, None);
    assert_eq!(cfg.baidu_access_token, None);
    assert_eq!(cfg.baidu_refresh_token, None);
    assert_eq!(cfg.local_root, None);
    cfg.validate().expect("default config still validates");
}

#[test]
fn load_toml_reads_all_new_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        concat!(
            "backend = \"baidu\"\n",
            "baidu_root = \"/apps/privatefs\"\n",
            "baidu_app_key = \"k\"\n",
            "baidu_app_secret = \"s\"\n",
            "baidu_access_token = \"a\"\n",
            "baidu_refresh_token = \"r\"\n",
        ),
    )
    .expect("write config.toml");

    let cfg = CyDriveConfig::load_toml(&path).expect("config with backend keys loads");
    assert_eq!(cfg.backend, Backend::Baidu);
    assert_eq!(cfg.baidu_root, "/apps/privatefs");
    assert_eq!(cfg.baidu_app_key.as_deref(), Some("k"));
    assert_eq!(cfg.baidu_app_secret.as_deref(), Some("s"));
    assert_eq!(cfg.baidu_access_token.as_deref(), Some("a"));
    assert_eq!(cfg.baidu_refresh_token.as_deref(), Some("r"));
    cfg.validate().expect("complete baidu config validates");

    // local_root rides the TOML too (None default round-trips as absent).
    // The absolute-path shape is platform-dependent (`Path::is_absolute`
    // semantics: a drive prefix on Windows, a leading '/' on Unix).
    let good_root = if cfg!(windows) {
        "C:\\data\\cloudfs"
    } else {
        "/srv/cloudfs"
    };
    fs::write(
        &path,
        // TOML literal string (single quotes): a Windows root's
        // backslashes pass through unescaped.
        format!("backend = \"local\"\nlocal_root = '{good_root}'\n"),
    )
    .expect("write config.toml");
    let cfg = CyDriveConfig::load_toml(&path).expect("local config loads");
    assert_eq!(cfg.backend, Backend::Local);
    assert_eq!(cfg.local_root.as_deref(), Some(good_root));
    cfg.validate()
        .expect("local config with absolute root validates");
}

#[test]
fn toml_roundtrip_preserves_new_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");

    let cfg = baidu_config();
    cfg.save_toml(&path).expect("save_toml");
    let loaded = CyDriveConfig::load_toml(&path).expect("load_toml");
    assert_eq!(loaded, cfg, "backend keys must survive the TOML round-trip");
}

#[test]
fn unknown_backend_value_rejected_with_actionable_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(&path, "backend = \"dropbox\"\n").expect("write config.toml");

    let err = CyDriveConfig::load_toml(&path)
        .expect_err("an unknown backend value must be rejected at parse time");
    // The encryption_scheme precedent: the serde enum error names every
    // accepted value and echoes the rejected one (the field name itself
    // is not part of the message — accepted per that adjudication).
    assert!(
        matches!(err, ConfigError::Parse { ref message, .. }
                 if message.contains("telegram")
                     && message.contains("baidu")
                     && message.contains("local")
                     && message.contains("dropbox")),
        "expected a Parse error naming all accepted backends and the rejected value, got: {err:?}"
    );
}

#[test]
fn baidu_backend_requires_the_four_credential_keys() {
    // All four present → valid (baidu_config covers that); each key
    // missing or empty on its own → actionable Invalid naming the key
    // and its env route.
    for (name, clear) in [
        ("baidu_app_key", 0usize),
        ("baidu_app_secret", 1),
        ("baidu_access_token", 2),
        ("baidu_refresh_token", 3),
    ] {
        let mut cfg = baidu_config();
        match clear {
            0 => cfg.baidu_app_key = None,
            1 => cfg.baidu_app_secret = Some(String::new()),
            2 => cfg.baidu_access_token = None,
            _ => cfg.baidu_refresh_token = Some(String::new()),
        }
        let err = cfg
            .validate()
            .expect_err("backend=baidu without {name} must be invalid");
        assert!(
            matches!(err, ConfigError::Invalid(ref message) if message.contains(name)
                     && message.contains("CYDRIVE")),
            "expected Invalid naming {name} and its env route, got: {err:?}"
        );
    }
    // A telegram config carries no baidu requirement (compatibility).
    CyDriveConfig::default()
        .validate()
        .expect("telegram config never needs baidu keys");
}

#[test]
fn baidu_root_must_be_backend_absolute_for_baidu() {
    let mut cfg = baidu_config();
    cfg.baidu_root = "apps/cloudfs".to_string(); // missing leading '/'
    let err = cfg
        .validate()
        .expect_err("a relative baidu_root must be rejected for backend=baidu");
    assert!(
        matches!(err, ConfigError::Invalid(ref message) if message.contains("baidu_root")),
        "expected Invalid naming baidu_root, got: {err:?}"
    );

    // The same value under the default telegram backend is inert (the
    // key only feeds the baidu driver) — no validation rule fires.
    let cfg = CyDriveConfig {
        baidu_root: "apps/cloudfs".to_string(),
        ..CyDriveConfig::default()
    };
    cfg.validate()
        .expect("telegram backend ignores baidu_root shape");
}

#[test]
fn local_backend_requires_absolute_local_root() {
    // Missing / empty / relative roots are each rejected with the key
    // named; an absolute one validates (unix and windows shapes).
    for bad in [None, Some(""), Some("relative/root"), Some("./here")] {
        let cfg = CyDriveConfig {
            backend: Backend::Local,
            local_root: bad.map(str::to_string),
            ..CyDriveConfig::default()
        };
        let err = cfg
            .validate()
            .expect_err("backend=local without an absolute local_root must be invalid");
        assert!(
            matches!(err, ConfigError::Invalid(ref message) if message.contains("local_root")
                     && message.contains("absolute")),
            "expected Invalid naming local_root and the absolute requirement, got: {err:?}"
        );
    }
    // Absolute means platform-absolute (`Path::is_absolute`): a drive
    // prefix on Windows, a leading '/' on Unix — the other platform's
    // shape is deliberately NOT absolute here (a Unix path on Windows
    // has no prefix and vice versa), so each platform checks its own.
    let good_root = if cfg!(windows) {
        "C:\\data\\cloudfs"
    } else {
        "/srv/cloudfs"
    };
    local_config(good_root)
        .validate()
        .unwrap_or_else(|e| panic!("local_root={good_root:?} must be valid: {e}"));
}

#[test]
fn env_overrides_apply_env_over_file_and_empty_clears() {
    let _env = env_guard();

    // env > file: set variables win over config.toml values.
    std::env::set_var("CYDRIVE_BAIDU_APP_KEY", "env-key");
    std::env::set_var("CYDRIVE_BAIDU_APP_SECRET", "env-secret");
    std::env::set_var("CYDRIVE_BAIDU_ACCESS_TOKEN", "env-access");
    std::env::set_var("CYDRIVE_BAIDU_REFRESH_TOKEN", "env-refresh");
    let cfg = baidu_config().with_env_overrides();
    assert_eq!(cfg.baidu_app_key.as_deref(), Some("env-key"));
    assert_eq!(cfg.baidu_app_secret.as_deref(), Some("env-secret"));
    assert_eq!(cfg.baidu_access_token.as_deref(), Some("env-access"));
    assert_eq!(cfg.baidu_refresh_token.as_deref(), Some("env-refresh"));

    // An empty variable explicitly clears the file value (sync_url
    // precedent) — validate must then name the missing key.
    std::env::set_var("CYDRIVE_BAIDU_APP_KEY", "");
    let cfg = baidu_config().with_env_overrides();
    assert_eq!(cfg.baidu_app_key, None, "empty env clears to None");
}

/// The two sftp credential keys ride the same `with_env_overrides` chain
/// as the baidu ones (review fix, K28 alignment): env > file on the
/// single-volume load path, empty clears, and — because the multi-volume
/// discovery path never calls `with_env_overrides` — volume files are
/// immune to cross-volume env bleed by construction.
#[test]
fn sftp_credential_env_overrides_apply_and_empty_clears() {
    let _env = env_guard();

    std::env::set_var("CYDRIVE_SFTP_PASSWORD", "env-password");
    std::env::set_var("CYDRIVE_SFTP_PRIVATE_KEY_PASSPHRASE", "env-phrase");
    let cfg = sftp_config().with_env_overrides();
    assert_eq!(cfg.sftp_password.as_deref(), Some("env-password"));
    assert_eq!(
        cfg.sftp_private_key_passphrase.as_deref(),
        Some("env-phrase")
    );

    // Empty clears to None (baidu/sync_url precedent); a key-path-only
    // config stays valid without the password.
    std::env::set_var("CYDRIVE_SFTP_PASSWORD", "");
    let cfg = sftp_config().with_env_overrides();
    assert_eq!(cfg.sftp_password, None, "empty env clears to None");
}

#[test]
fn legacy_json_rejects_all_new_keys() {
    // The legacy key set stays frozen at the Python dataclass fields: a
    // Phase-2 key smuggled into a legacy config.json must be rejected,
    // not silently ignored (tier-1 2026-09-03 precedent).
    for key in [
        "backend",
        "baidu_root",
        "baidu_app_key",
        "baidu_app_secret",
        "baidu_access_token",
        "baidu_refresh_token",
        "local_root",
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            format!(r#"{{ "bot_token": "111:AA", "chat_id": 5, "{key}": "x" }}"#),
        )
        .expect("write config.json");

        let err = CyDriveConfig::load_legacy_json(&path)
            .expect_err("legacy json must reject the {key} key");
        assert!(
            matches!(err, ConfigError::Parse { ref message, .. } if message.contains(key)),
            "expected Parse error naming {key}, got: {err:?}"
        );
    }
}

#[test]
fn backend_enum_round_trips_wire_names() {
    // The TOML wire names are stable identifiers (sync docs, E2E configs
    // hand-write them); pin the spellings both ways.
    assert_eq!(Backend::Telegram.as_str(), "telegram");
    assert_eq!(Backend::Baidu.as_str(), "baidu");
    assert_eq!(Backend::Local.as_str(), "local");
    assert_eq!(Backend::Sftp.as_str(), "sftp");
    assert_eq!(Backend::Pan115.as_str(), "pan115");
    assert_eq!(Backend::Pan123.as_str(), "pan123");
}

// -------------------------------------------------- sftp keys (Phase 4 SF1) ---
//
// Contract under test — three-place key sync (interfaces §4) for the
// eight `sftp_*` keys plus the `sftp` backend's cross-field rules:
//
// - `backend = "sftp"` requires `sftp_host` and `sftp_username` set and
//   non-empty, and at least one of `sftp_password` /
//   `sftp_private_key_path` (D1: password OR key — an actionable error
//   names both when neither is present).
// - `sftp_port`: an explicit value must be in 1..=65535 (a `0` is the
//   only rejectable value left after the typed `u16` parse).
// - `sftp_host_fingerprint`: optional (D2 — the driver refuses to
//   connect without it, which is a driver concern, not a config rule).
// - `sftp_root`: optional; when present it must be a backend-absolute
//   path starting with `'/'` (the baidu_root rule, mirrored).
// - The telegram default triggers none of these rules — pre-Phase-4
//   configs validate unchanged.

/// A minimal valid sftp config: backend set + host/username + password
/// auth (short dummy values — the scanner gate only matches 20+ char
/// literals).
fn sftp_config() -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Sftp,
        sftp_host: Some("server.example.org".to_string()),
        sftp_username: Some("user".to_string()),
        sftp_password: Some("test-password".to_string()),
        ..CyDriveConfig::default()
    }
}

#[test]
fn sftp_defaults_are_unset_and_telegram_stays_inert() {
    // The compatibility contract: a config that predates Phase 4 keeps
    // validating — every sftp key defaults to None and the telegram
    // backend fires no sftp rule even when sftp keys are dirty.
    let cfg = CyDriveConfig::default();
    assert_eq!(cfg.backend, Backend::Telegram, "default backend");
    assert_eq!(cfg.sftp_host, None);
    assert_eq!(cfg.sftp_port, None);
    assert_eq!(cfg.sftp_username, None);
    assert_eq!(cfg.sftp_password, None);
    assert_eq!(cfg.sftp_private_key_path, None);
    assert_eq!(cfg.sftp_private_key_passphrase, None);
    assert_eq!(cfg.sftp_host_fingerprint, None);
    assert_eq!(cfg.sftp_root, None);
    cfg.validate().expect("default config still validates");

    let dirty = CyDriveConfig {
        sftp_host: Some(String::new()),
        sftp_username: None,
        sftp_port: Some(0),
        sftp_root: Some("relative/root".to_string()),
        ..CyDriveConfig::default()
    };
    dirty
        .validate()
        .expect("the telegram backend ignores every sftp rule");
}

#[test]
fn load_toml_reads_all_sftp_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        concat!(
            "backend = \"sftp\"\n",
            "sftp_host = \"nas.lan\"\n",
            "sftp_port = 2222\n",
            "sftp_username = \"cloudfs\"\n",
            "sftp_password = \"pw\"\n",
            "sftp_private_key_path = \"C:/keys/id_ed25519\"\n",
            "sftp_private_key_passphrase = \"phrase\"\n",
            "sftp_host_fingerprint = \"SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n",
            "sftp_root = \"/srv/cloudfs\"\n",
        ),
    )
    .expect("write config.toml");

    let cfg = CyDriveConfig::load_toml(&path).expect("config with sftp keys loads");
    assert_eq!(cfg.backend, Backend::Sftp);
    assert_eq!(cfg.sftp_host.as_deref(), Some("nas.lan"));
    assert_eq!(cfg.sftp_port, Some(2222));
    assert_eq!(cfg.sftp_username.as_deref(), Some("cloudfs"));
    assert_eq!(cfg.sftp_password.as_deref(), Some("pw"));
    assert_eq!(
        cfg.sftp_private_key_path.as_deref(),
        Some("C:/keys/id_ed25519")
    );
    assert_eq!(cfg.sftp_private_key_passphrase.as_deref(), Some("phrase"));
    assert_eq!(
        cfg.sftp_host_fingerprint.as_deref(),
        Some("SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
    );
    assert_eq!(cfg.sftp_root.as_deref(), Some("/srv/cloudfs"));
    cfg.validate().expect("complete sftp config validates");
}

#[test]
fn toml_roundtrip_preserves_sftp_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");

    let mut cfg = sftp_config();
    cfg.sftp_port = Some(2222);
    cfg.sftp_private_key_path = Some("/keys/id_ed25519".to_string());
    cfg.sftp_host_fingerprint =
        Some("SHA256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".to_string());
    cfg.sftp_root = Some("/srv/data".to_string());
    cfg.save_toml(&path).expect("save_toml");
    let loaded = CyDriveConfig::load_toml(&path).expect("load_toml");
    assert_eq!(loaded, cfg, "sftp keys must survive the TOML round-trip");
}

#[test]
fn sftp_backend_requires_host_and_username() {
    for (name, clear_host) in [("sftp_host", true), ("sftp_username", false)] {
        let mut cfg = sftp_config();
        if clear_host {
            cfg.sftp_host = Some(String::new());
        } else {
            cfg.sftp_username = None;
        }
        let err = cfg
            .validate()
            .expect_err("backend=sftp without {name} must be invalid");
        assert!(
            matches!(err, ConfigError::Invalid(ref message) if message.contains(name)),
            "expected Invalid naming {name}, got: {err:?}"
        );
    }
}

#[test]
fn sftp_backend_requires_password_or_private_key() {
    // Neither auth credential → one actionable error naming BOTH keys
    // (D1: password OR key — the message must offer both routes).
    let mut cfg = sftp_config();
    cfg.sftp_password = None;
    let err = cfg
        .validate()
        .expect_err("backend=sftp with neither password nor key must be invalid");
    assert!(
        matches!(err, ConfigError::Invalid(ref message)
                 if message.contains("sftp_password")
                     && message.contains("sftp_private_key_path")),
        "expected Invalid naming both auth keys, got: {err:?}"
    );

    // Key-only auth (no password) validates — D1's second form.
    let mut key_only = sftp_config();
    key_only.sftp_password = None;
    key_only.sftp_private_key_path = Some("/keys/id_ed25519".to_string());
    key_only
        .validate()
        .expect("private-key-only auth validates");

    // An EMPTY private-key path does not count as the key form (same
    // empty-means-unset semantics as the baidu keys).
    let mut empty_key = key_only;
    empty_key.sftp_private_key_path = Some(String::new());
    let err = empty_key
        .validate()
        .expect_err("an empty private key path must not satisfy the auth rule");
    assert!(
        matches!(err, ConfigError::Invalid(ref message) if message.contains("sftp_private_key_path")),
        "expected Invalid naming sftp_private_key_path, got: {err:?}"
    );
}

#[test]
fn sftp_port_must_be_in_range_when_present() {
    let mut cfg = sftp_config();
    cfg.sftp_port = Some(0);
    let err = cfg
        .validate()
        .expect_err("sftp_port = 0 must be rejected for backend=sftp");
    assert!(
        matches!(err, ConfigError::Invalid(ref message)
                 if message.contains("sftp_port") && message.contains("1")),
        "expected Invalid naming sftp_port and the range, got: {err:?}"
    );

    // The boundaries pass: the minimum, the maximum, and absent (the
    // driver's own default 22).
    for good in [None, Some(1u16), Some(65535)] {
        let mut cfg = sftp_config();
        cfg.sftp_port = good;
        cfg.validate()
            .unwrap_or_else(|e| panic!("sftp_port={good:?} must be valid: {e}"));
    }
}

#[test]
fn sftp_root_must_be_backend_absolute_for_sftp() {
    let mut cfg = sftp_config();
    cfg.sftp_root = Some("srv/cloudfs".to_string()); // missing leading '/'
    let err = cfg
        .validate()
        .expect_err("a relative sftp_root must be rejected for backend=sftp");
    assert!(
        matches!(err, ConfigError::Invalid(ref message) if message.contains("sftp_root")),
        "expected Invalid naming sftp_root, got: {err:?}"
    );
}

#[test]
fn legacy_json_rejects_all_sftp_keys() {
    for key in [
        "sftp_host",
        "sftp_port",
        "sftp_username",
        "sftp_password",
        "sftp_private_key_path",
        "sftp_private_key_passphrase",
        "sftp_host_fingerprint",
        "sftp_root",
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            format!(r#"{{ "bot_token": "111:AA", "chat_id": 5, "{key}": "x" }}"#),
        )
        .expect("write config.json");

        let err = CyDriveConfig::load_legacy_json(&path)
            .expect_err("legacy json must reject the {key} key");
        assert!(
            matches!(err, ConfigError::Parse { ref message, .. } if message.contains(key)),
            "expected Parse error naming {key}, got: {err:?}"
        );
    }
}

#[test]
fn volume_file_accepts_the_sftp_key_group() {
    // The eight keys are volume-scoped (K19 partition — a volume file
    // carries its own server identity/credentials); a minimal sftp
    // volume file loads through the strict volume surface.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nas.toml");
    fs::write(
        &path,
        concat!(
            "backend = \"sftp\"\n",
            "sftp_host = \"nas.lan\"\n",
            "sftp_username = \"cloudfs\"\n",
            "sftp_password = \"pw\"\n",
            "sftp_host_fingerprint = \"SHA256:CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC\"\n",
        ),
    )
    .expect("write volume file");

    let spec = cloudkit_core::config::load_volume_config(&path)
        .expect("a volume file carrying the sftp key group loads");
    assert_eq!(spec.settings.backend, Backend::Sftp);
    assert_eq!(spec.settings.sftp_host.as_deref(), Some("nas.lan"));
    spec.settings
        .validate()
        .expect("the parsed volume settings validate");
}

// ------------------------------------------------- pan115 keys (Phase 5 115-1) ---
//
// Contract under test — three-place key sync (interfaces §4) for the
// four `pan115_*` keys plus the `pan115` backend's cross-field rules
// (K69: route A decided — the open platform with a token pair the
// driver self-refreshes):
//
// - `backend = "pan115"` requires `pan115_access_token` AND
//   `pan115_refresh_token` set and non-empty (each error names its
//   CYDRIVE_PAN115_* env route; the initial pair comes from the setup
//   QR scan or a hand-filled token pair — the message says so).
// - `pan115_root`: optional; when present it must be all digits (a
//   115 folder id — "0" is the drive root, the D3 default).
// - `pan115_client_id`: optional non-secret (the app identity the QR
//   scan binds to; the driver injects the K69 default 100197303 — no
//   config rule fires on it).
// - The telegram default triggers none of these rules — pre-Phase-5
//   configs validate unchanged.

/// A minimal valid pan115 config: backend set + the token pair (short
/// dummy values — the scanner gate only matches 20+ char literals).
fn pan115_config() -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Pan115,
        pan115_access_token: Some("mock-access-0".to_string()),
        pan115_refresh_token: Some("mock-refresh-0".to_string()),
        ..CyDriveConfig::default()
    }
}

#[test]
fn pan115_defaults_are_unset_and_telegram_stays_inert() {
    // The compatibility contract: a config that predates Phase 5 keeps
    // validating — every pan115 key defaults to None and the telegram
    // backend fires no pan115 rule even when pan115 keys are dirty.
    let cfg = CyDriveConfig::default();
    assert_eq!(cfg.backend, Backend::Telegram, "default backend");
    assert_eq!(cfg.pan115_client_id, None);
    assert_eq!(cfg.pan115_access_token, None);
    assert_eq!(cfg.pan115_refresh_token, None);
    assert_eq!(cfg.pan115_root, None);
    cfg.validate().expect("default config still validates");

    let dirty = CyDriveConfig {
        pan115_access_token: Some(String::new()),
        pan115_root: Some("not-a-folder-id".to_string()),
        ..CyDriveConfig::default()
    };
    dirty
        .validate()
        .expect("the telegram backend ignores every pan115 rule");
}

#[test]
fn load_toml_reads_all_pan115_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        concat!(
            "backend = \"pan115\"\n",
            "pan115_client_id = \"100197303\"\n",
            "pan115_access_token = \"mock-access-0\"\n",
            "pan115_refresh_token = \"mock-refresh-0\"\n",
            "pan115_root = \"1234567890\"\n",
        ),
    )
    .expect("write config.toml");

    let cfg = CyDriveConfig::load_toml(&path).expect("config with pan115 keys loads");
    assert_eq!(cfg.backend, Backend::Pan115);
    assert_eq!(cfg.pan115_client_id.as_deref(), Some("100197303"));
    assert_eq!(cfg.pan115_access_token.as_deref(), Some("mock-access-0"));
    assert_eq!(cfg.pan115_refresh_token.as_deref(), Some("mock-refresh-0"));
    assert_eq!(cfg.pan115_root.as_deref(), Some("1234567890"));
    cfg.validate().expect("complete pan115 config validates");
}

#[test]
fn toml_roundtrip_preserves_pan115_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");

    let mut cfg = pan115_config();
    cfg.pan115_client_id = Some("100197303".to_string());
    cfg.pan115_root = Some("0".to_string());
    cfg.save_toml(&path).expect("save_toml");
    let loaded = CyDriveConfig::load_toml(&path).expect("load_toml");
    assert_eq!(loaded, cfg, "pan115 keys must survive the TOML round-trip");
}

#[test]
fn pan115_backend_requires_the_token_pair() {
    for (name, clear_access) in [
        ("pan115_access_token", true),
        ("pan115_refresh_token", false),
    ] {
        let mut cfg = pan115_config();
        if clear_access {
            cfg.pan115_access_token = Some(String::new());
        } else {
            cfg.pan115_refresh_token = None;
        }
        let err = cfg
            .validate()
            .expect_err("backend=pan115 without {name} must be invalid");
        assert!(
            matches!(err, ConfigError::Invalid(ref message) if message.contains(name)),
            "expected Invalid naming {name}, got: {err:?}"
        );
    }

    // Empty-means-unset also rejects the pair form (the baidu keys'
    // semantics, mirrored).
    let mut cfg = pan115_config();
    cfg.pan115_refresh_token = Some(String::new());
    assert!(
        cfg.validate().is_err(),
        "an empty refresh token must not count"
    );
}

#[test]
fn pan115_token_error_names_the_setup_route() {
    // Actionable-text contract: with no pair at all the error offers the
    // ways to obtain one (the setup QR scan / a hand-filled pair) and
    // names the env routes.
    let cfg = CyDriveConfig {
        backend: Backend::Pan115,
        ..CyDriveConfig::default()
    };
    let err = cfg
        .validate()
        .expect_err("backend=pan115 without tokens must be invalid");
    match err {
        ConfigError::Invalid(message) => {
            assert!(
                message.contains("pan115_access_token"),
                "names the key: {message}"
            );
            assert!(
                message.contains("CYDRIVE_PAN115_ACCESS_TOKEN"),
                "names the env route: {message}"
            );
            assert!(
                message.contains("setup"),
                "offers the setup route: {message}"
            );
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn pan115_root_must_be_numeric_when_present() {
    for bad in ["abc", "12x45", "-1", "", "0x10"] {
        let mut cfg = pan115_config();
        cfg.pan115_root = Some(bad.to_string());
        assert!(
            cfg.validate().is_err(),
            "pan115_root={bad:?} must be rejected for backend=pan115"
        );
    }

    // The good forms: the drive root "0" (D3 default), a real folder id,
    // and absent (the driver's own default "0").
    for good in [None, Some("0"), Some("1234567890")] {
        let mut cfg = pan115_config();
        cfg.pan115_root = good.map(str::to_string);
        cfg.validate()
            .unwrap_or_else(|e| panic!("pan115_root={good:?} must be valid: {e}"));
    }
}

#[test]
fn legacy_json_rejects_all_pan115_keys() {
    for key in [
        "pan115_client_id",
        "pan115_access_token",
        "pan115_refresh_token",
        "pan115_root",
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            format!(r#"{{ "bot_token": "111:AA", "chat_id": 5, "{key}": "x" }}"#),
        )
        .expect("write config.json");

        let err = CyDriveConfig::load_legacy_json(&path)
            .expect_err("legacy json must reject the {key} key");
        assert!(
            matches!(err, ConfigError::Parse { ref message, .. } if message.contains(key)),
            "expected Parse error naming {key}, got: {err:?}"
        );
    }
}

#[test]
fn volume_file_accepts_the_pan115_key_group() {
    // The four keys are volume-scoped (K19 partition — a volume file
    // carries its own drive credentials); a minimal pan115 volume file
    // loads through the strict volume surface.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("net115.toml");
    fs::write(
        &path,
        concat!(
            "backend = \"pan115\"\n",
            "pan115_access_token = \"mock-access-0\"\n",
            "pan115_refresh_token = \"mock-refresh-0\"\n",
            "pan115_root = \"0\"\n",
        ),
    )
    .expect("write volume file");

    let spec = cloudkit_core::config::load_volume_config(&path)
        .expect("a volume file carrying the pan115 key group loads");
    assert_eq!(spec.settings.backend, Backend::Pan115);
    assert_eq!(spec.settings.pan115_root.as_deref(), Some("0"));
    spec.settings
        .validate()
        .expect("the parsed volume settings validate");
}

// ------------------------------------------------- pan123 keys (Phase 6 123-1) ---
//
// Contract under test — three-place key sync (interfaces §4) for the two
// `pan123_*` keys plus the `pan123` backend's cross-field rules (K64: web
// API route with a single 90-day token — no refresh exists, K76.4):
//
// - `backend = "pan123"` requires `pan123_token` set and non-empty (the
//   error names the CYDRIVE_PAN123_TOKEN env route; the initial token
//   comes from the setup QR scan or a password sign_in — the message
//   says so).
// - `pan123_root`: optional; when present it must be all digits (a
//   123pan folder id — "0" is the netdisk root, the D3-on-Phase-5
//   default).
// - The telegram default triggers none of these rules — pre-Phase-6
//   configs validate unchanged.

/// A minimal valid pan123 config: backend set + the token (short dummy
/// value — the scanner gate only matches 20+ char literals).
fn pan123_config() -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Pan123,
        pan123_token: Some("mock-token-0".to_string()),
        ..CyDriveConfig::default()
    }
}

#[test]
fn pan123_defaults_are_unset_and_telegram_stays_inert() {
    // The compatibility contract: a config that predates Phase 6 keeps
    // validating — every pan123 key defaults to None and the telegram
    // backend fires no pan123 rule even when pan123 keys are dirty.
    let cfg = CyDriveConfig::default();
    assert_eq!(cfg.backend, Backend::Telegram, "default backend");
    assert_eq!(cfg.pan123_token, None);
    assert_eq!(cfg.pan123_root, None);
    cfg.validate().expect("default config still validates");

    let dirty = CyDriveConfig {
        pan123_token: Some(String::new()),
        pan123_root: Some("not-a-folder-id".to_string()),
        ..CyDriveConfig::default()
    };
    dirty
        .validate()
        .expect("the telegram backend ignores every pan123 rule");
}

#[test]
fn pan123_backend_requires_the_token() {
    let mut cfg = pan123_config();
    cfg.pan123_token = None;
    let err = cfg.validate().expect_err("token required");
    assert!(
        matches!(&err, ConfigError::Invalid(msg)
            if msg.contains("pan123_token")
                && msg.contains("CYDRIVE_PAN123_TOKEN")
                && (msg.contains("QR") || msg.contains("sign_in"))),
        "actionable text naming the key, the env route and the acquisition path: {err:?}"
    );
    // Empty string is as good as absent (empty-means-unset).
    let mut empty = pan123_config();
    empty.pan123_token = Some(String::new());
    assert!(
        empty.validate().is_err(),
        "an empty token must not satisfy the requirement"
    );
    // The valid minimal config passes.
    pan123_config()
        .validate()
        .expect("minimal pan123 config validates");
}

#[test]
fn pan123_root_must_be_numeric_when_present() {
    let mut cfg = pan123_config();
    cfg.pan123_root = Some("12x45".to_string());
    let err = cfg.validate().expect_err("non-numeric root rejected");
    assert!(
        matches!(&err, ConfigError::Invalid(msg) if msg.contains("pan123_root")),
        "names the offending key: {err:?}"
    );
    // "0" (the netdisk root) and any digit string pass.
    let mut root = pan123_config();
    root.pan123_root = Some("0".to_string());
    root.validate().expect("the netdisk root is legal");
    let mut deep = pan123_config();
    deep.pan123_root = Some("64379791".to_string());
    deep.validate().expect("a folder id is legal");
}

#[test]
fn load_toml_reads_both_pan123_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        concat!(
            "backend = \"pan123\"\n",
            "pan123_token = \"mock-token-0\"\n",
            "pan123_root = \"64379791\"\n",
        ),
    )
    .expect("write config");

    let cfg = CyDriveConfig::load_toml(&path).expect("the pan123 key group loads");
    assert_eq!(cfg.backend, Backend::Pan123);
    assert_eq!(cfg.pan123_token.as_deref(), Some("mock-token-0"));
    assert_eq!(cfg.pan123_root.as_deref(), Some("64379791"));
    cfg.validate().expect("the loaded config validates");
}

#[test]
fn toml_roundtrip_preserves_pan123_keys() {
    let mut cfg = pan123_config();
    cfg.pan123_root = Some("0".to_string());
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    cfg.save_toml(&path).expect("save");
    let reloaded = CyDriveConfig::load_toml(&path).expect("reload");
    assert_eq!(reloaded, cfg, "both keys round-trip verbatim");
}

#[test]
fn legacy_json_rejects_the_pan123_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    for key in ["pan123_token", "pan123_root"] {
        fs::write(
            &path,
            format!("{{\"bot_token\": \"t\", \"chat_id\": 1, \"{key}\": \"x\"}}"),
        )
        .expect("write legacy json");
        let err = CyDriveConfig::load_legacy_json(&path)
            .expect_err("legacy config.json must reject the pan123 keys");
        assert!(
            matches!(err, ConfigError::Parse { ref message, .. } if message.contains(key)),
            "expected Parse error naming {key}, got: {err:?}"
        );
    }
}

#[test]
fn pan123_token_env_override_applies_and_empty_clears() {
    let _guard = env_guard(); // clears CYDRIVE_PAN123_TOKEN on drop too
    let mut cfg = pan123_config();
    cfg.pan123_token = None;

    std::env::set_var("CYDRIVE_PAN123_TOKEN", "mock-token-from-env");
    let overridden = cfg.clone().with_env_overrides();
    assert_eq!(
        overridden.pan123_token.as_deref(),
        Some("mock-token-from-env"),
        "env > file for the pan123 token"
    );

    std::env::set_var("CYDRIVE_PAN123_TOKEN", "");
    let cleared = cfg.with_env_overrides();
    assert_eq!(
        cleared.pan123_token, None,
        "a set-but-empty variable clears back to None"
    );
    std::env::remove_var("CYDRIVE_PAN123_TOKEN");
}

#[test]
fn volume_file_accepts_the_pan123_key_group() {
    // The two keys are volume-scoped (K19 partition — a volume file
    // carries its own drive credentials); a minimal pan123 volume file
    // loads through the strict volume surface.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pan123.toml");
    fs::write(
        &path,
        concat!(
            "backend = \"pan123\"\n",
            "pan123_token = \"mock-token-0\"\n",
            "pan123_root = \"0\"\n",
        ),
    )
    .expect("write volume file");

    let spec = cloudkit_core::config::load_volume_config(&path)
        .expect("a volume file carrying the pan123 key group loads");
    assert_eq!(spec.settings.backend, Backend::Pan123);
    assert_eq!(spec.settings.pan123_root.as_deref(), Some("0"));
    spec.settings
        .validate()
        .expect("the parsed volume settings validate");
}

// ------------------------------------------------- webdav keys (Phase 7 WD1b) ---
//
// Contract under test — three-place key sync (interfaces §4) for the
// six `webdav_*` keys plus the `webdav` backend's cross-field rules
// (the plan §4.2 table; core checks the string-level subset, the
// driver's parse_from_map re-checks on its map face):
//
// - `backend = "webdav"` requires `webdav_url` set and non-empty, with
//   an http(s) scheme, a non-empty host and no query/fragment (the
//   sync_url minimal-authority precedent — no url crate in core).
// - `webdav_username` / `webdav_password` arrive as a PAIR (only one
//   set = invalid; both absent = anonymous access).
// - `webdav_auth` ∈ {auto, basic, digest} and `webdav_vendor` ∈
//   {generic, nextcloud} — lenient casing/whitespace, empty = unset.
// - `webdav_accept_invalid_certs` is a typed bool (the serde parse is
//   the value domain — no validate rule, the enable_encryption form).
// - The telegram default triggers none of these rules — pre-Phase-7
//   configs validate unchanged.

/// A minimal valid webdav config: backend set + url + the credential
/// pair (short dummy values — the scanner gate only matches 20+ char
/// literals).
fn webdav_config() -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Webdav,
        webdav_url: Some("https://nas.lan:5006/dav".to_string()),
        webdav_username: Some("spike".to_string()),
        webdav_password: Some("pw".to_string()),
        ..CyDriveConfig::default()
    }
}

#[test]
fn webdav_defaults_are_unset_and_telegram_stays_inert() {
    // The compatibility contract: a config that predates Phase 7 keeps
    // validating — every webdav key defaults to None and the telegram
    // backend fires no webdav rule even when webdav keys are dirty.
    let cfg = CyDriveConfig::default();
    assert_eq!(cfg.backend, Backend::Telegram, "default backend");
    assert_eq!(cfg.webdav_url, None);
    assert_eq!(cfg.webdav_username, None);
    assert_eq!(cfg.webdav_password, None);
    assert_eq!(cfg.webdav_auth, None);
    assert_eq!(cfg.webdav_vendor, None);
    assert_eq!(cfg.webdav_accept_invalid_certs, None);
    cfg.validate().expect("default config still validates");

    let dirty = CyDriveConfig {
        webdav_url: Some("not a url".to_string()),
        webdav_username: Some("only-username".to_string()),
        webdav_auth: Some("ntlm".to_string()),
        webdav_accept_invalid_certs: Some(true),
        ..CyDriveConfig::default()
    };
    dirty
        .validate()
        .expect("the telegram backend ignores every webdav rule");
}

#[test]
fn webdav_as_str_is_stable() {
    assert_eq!(Backend::Webdav.as_str(), "webdav");
}

#[test]
fn webdav_key_lists_carry_the_six_keys() {
    use cloudkit_core::config::{KNOWN_TOML_KEYS, VOLUME_SCOPED_KEYS};
    for key in [
        "webdav_url",
        "webdav_username",
        "webdav_password",
        "webdav_auth",
        "webdav_vendor",
        "webdav_accept_invalid_certs",
    ] {
        assert!(KNOWN_TOML_KEYS.contains(&key), "{key} rides config.toml");
        assert!(
            VOLUME_SCOPED_KEYS.contains(&key),
            "{key} is volume-scoped (one share = one volume)"
        );
    }
}

#[test]
fn load_toml_reads_all_webdav_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        concat!(
            "backend = \"webdav\"\n",
            "webdav_url = \"https://nas.lan:5006/dav\"\n",
            "webdav_username = \"spike\"\n",
            "webdav_password = \"pw\"\n",
            "webdav_auth = \"digest\"\n",
            "webdav_vendor = \"nextcloud\"\n",
            "webdav_accept_invalid_certs = true\n",
        ),
    )
    .expect("write config.toml");

    let cfg = CyDriveConfig::load_toml(&path).expect("config with webdav keys loads");
    assert_eq!(cfg.backend, Backend::Webdav);
    assert_eq!(cfg.webdav_url.as_deref(), Some("https://nas.lan:5006/dav"));
    assert_eq!(cfg.webdav_username.as_deref(), Some("spike"));
    assert_eq!(cfg.webdav_password.as_deref(), Some("pw"));
    assert_eq!(cfg.webdav_auth.as_deref(), Some("digest"));
    assert_eq!(cfg.webdav_vendor.as_deref(), Some("nextcloud"));
    assert_eq!(cfg.webdav_accept_invalid_certs, Some(true));
    cfg.validate().expect("complete webdav config validates");
}

#[test]
fn toml_roundtrip_preserves_webdav_keys() {
    let mut cfg = webdav_config();
    cfg.webdav_auth = Some("digest".to_string());
    cfg.webdav_vendor = Some("nextcloud".to_string());
    cfg.webdav_accept_invalid_certs = Some(true);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    cfg.save_toml(&path).expect("save");
    let reloaded = CyDriveConfig::load_toml(&path).expect("reload");
    assert_eq!(reloaded, cfg, "the webdav key group round-trips verbatim");
}

#[test]
fn webdav_backend_requires_the_url() {
    for (label, url) in [
        ("absent", None),
        ("empty", Some(String::new())),
        ("blank", Some("   ".to_string())),
    ] {
        let mut cfg = webdav_config();
        cfg.webdav_url = url;
        let err = cfg
            .validate()
            .err()
            .unwrap_or_else(|| panic!("{label} url must be invalid"));
        assert!(
            matches!(&err, ConfigError::Invalid(msg)
                     if msg.contains("webdav_url") && msg.contains("http")),
            "{label}: names the key and the expected shape: {err:?}"
        );
    }
    // Anonymous access (no credentials) validates — the pair rule has a
    // both-absent exit.
    let mut anonymous = webdav_config();
    anonymous.webdav_username = None;
    anonymous.webdav_password = None;
    anonymous
        .validate()
        .expect("anonymous access validates with just the url");
}

#[test]
fn webdav_url_must_be_http_with_a_host_and_no_query() {
    for (label, url, needle) in [
        ("ftp scheme", "ftp://nas.lan/dav", "http"),
        ("no scheme", "nas.lan:5006/dav", "http"),
        ("empty host", "https://:5006/dav", "host"),
        ("bare scheme", "https://", "host"),
        ("query", "https://nas.lan/dav?x=1", "query"),
        ("fragment", "https://nas.lan/dav#f", "query"),
    ] {
        let mut cfg = webdav_config();
        cfg.webdav_url = Some(url.to_string());
        let err = cfg
            .validate()
            .err()
            .unwrap_or_else(|| panic!("{label} must be rejected"));
        assert!(
            matches!(&err, ConfigError::Invalid(msg)
                     if msg.contains("webdav_url") && msg.contains(needle)),
            "{label}: names webdav_url and {needle}: {err:?}"
        );
    }
}

/// WD4 挂账①裁决：userinfo 形态（`https://user:pass@host/`）会把凭据
/// 带进 SHOW 回显与 sync namespace（卷身份携带完整 base URL）——core
/// validate 第一道漏斗拒收并指路凭据键（驱动 parse_from_map 是第二道）。
#[test]
fn webdav_url_must_not_embed_userinfo_credentials() {
    for (label, url) in [
        ("user:pass", "https://spike:pw@nas.lan:5006/dav/"),
        ("username only", "https://spike@nas.lan:5006/dav/"),
    ] {
        let mut cfg = webdav_config();
        cfg.webdav_url = Some(url.to_string());
        let err = cfg
            .validate()
            .err()
            .unwrap_or_else(|| panic!("{label} userinfo must be rejected"));
        assert!(
            matches!(&err, ConfigError::Invalid(msg)
                     if msg.contains("webdav_url")
                         && msg.contains("webdav_username")
                         && msg.contains("webdav_password")),
            "{label}: routes the credentials to their keys: {err:?}"
        );
    }
}

/// 复审 M3（2026-09-25）：校验失败文案绝不回显 webdav_url 原文。嵌入
/// 凭据的误用形态命中**任一臂**都不得把密码带进错误链——
/// `ConfigError::Invalid` 不经 `redact_credential_values` 漏斗（脱敏只挂
/// Parse 构造点），回显即凭据随 boot 错误/ADD 回复/tracing 落日志（R3）。
/// 最坏路径 = query/fragment 臂先于 userinfo 臂裁决：userinfo+query 形态
/// 的密码从 query 臂整串漏出。driver 侧同纪律镜像钉 = ck-webdav lib.rs
/// 的 M4 断言组。
#[test]
fn webdav_url_validation_errors_never_echo_the_raw_url() {
    for (label, url, marker) in [
        // scheme 臂：非 http(s) 形态整串回显（含密码）。
        ("scheme arm", "ftp://spike:SECRET9@nas.lan/dav/", "SECRET9"),
        // query/fragment 臂先于 userinfo 臂——userinfo+query 形态的
        // 密码从这里漏（最坏路径）。
        (
            "query arm",
            "https://spike:SECRET9@nas.lan/dav/?x=1",
            "SECRET9",
        ),
        (
            "fragment arm",
            "https://spike:SECRET9@nas.lan/dav/#f",
            "SECRET9",
        ),
        // userinfo 臂自身同样回显原文。
        (
            "userinfo arm",
            "https://spike:SECRET9@nas.lan:5006/dav/",
            "SECRET9",
        ),
        // host 臂（无凭据可达形态）同纪律不回显——统一去原文。
        ("host arm", "https://:5006/HOSTMARK9", "HOSTMARK9"),
    ] {
        let mut cfg = webdav_config();
        cfg.webdav_url = Some(url.to_string());
        let err = cfg
            .validate()
            .err()
            .unwrap_or_else(|| panic!("{label} must be rejected"));
        assert!(
            matches!(&err, ConfigError::Invalid(msg) if !msg.contains(marker)),
            "{label}: the refusal must not echo the raw url ({marker} leaked): {err:?}"
        );
    }
}

#[test]
fn webdav_credentials_must_arrive_as_a_pair() {
    for (label, drop_username) in [("username only", true), ("password only", false)] {
        let mut cfg = webdav_config();
        if drop_username {
            cfg.webdav_username = None;
        } else {
            cfg.webdav_password = None;
        }
        let err = cfg
            .validate()
            .err()
            .unwrap_or_else(|| panic!("{label} must be rejected"));
        assert!(
            matches!(&err, ConfigError::Invalid(msg)
                     if msg.contains("webdav_username") && msg.contains("webdav_password")),
            "{label}: names BOTH keys: {err:?}"
        );
    }
    // An EMPTY value does not count as the pair's other half
    // (empty-means-unset).
    let mut cfg = webdav_config();
    cfg.webdav_password = Some(String::new());
    let err = cfg
        .validate()
        .expect_err("an empty password leaves a lone username");
    assert!(
        matches!(&err, ConfigError::Invalid(msg) if msg.contains("webdav_password")),
        "{err:?}"
    );
}

#[test]
fn webdav_auth_and_vendor_enumerate_their_values_leniently() {
    let mut bad_auth = webdav_config();
    bad_auth.webdav_auth = Some("ntlm".to_string());
    let err = bad_auth.validate().expect_err("ntlm rejected");
    assert!(
        matches!(&err, ConfigError::Invalid(msg)
                 if msg.contains("webdav_auth") && msg.contains("auto, basic, digest")),
        "lists the legal auth values: {err:?}"
    );

    let mut bad_vendor = webdav_config();
    bad_vendor.webdav_vendor = Some("owncloud".to_string());
    let err = bad_vendor.validate().expect_err("owncloud rejected");
    assert!(
        matches!(&err, ConfigError::Invalid(msg)
                 if msg.contains("webdav_vendor") && msg.contains("generic, nextcloud")),
        "lists the legal vendor values: {err:?}"
    );

    // Lenient casing/whitespace passes; empty reads as unset.
    let mut lenient = webdav_config();
    lenient.webdav_auth = Some("  Digest ".to_string());
    lenient.webdav_vendor = Some("NextCloud".to_string());
    lenient.validate().expect("lenient enum values validate");

    let mut empty = webdav_config();
    empty.webdav_auth = Some(String::new());
    empty.webdav_vendor = Some(String::new());
    empty.validate().expect("empty enum keys read as unset");
}

#[test]
fn legacy_json_rejects_the_webdav_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    for key in [
        "webdav_url",
        "webdav_username",
        "webdav_password",
        "webdav_auth",
        "webdav_vendor",
        "webdav_accept_invalid_certs",
    ] {
        fs::write(
            &path,
            format!("{{\"bot_token\": \"t\", \"chat_id\": 1, \"{key}\": \"x\"}}"),
        )
        .expect("write legacy json");
        let err = CyDriveConfig::load_legacy_json(&path)
            .expect_err("legacy config.json must reject the webdav keys");
        assert!(
            matches!(err, ConfigError::Parse { ref message, .. } if message.contains(key)),
            "expected Parse error naming {key}, got: {err:?}"
        );
    }
}

#[test]
fn webdav_password_env_override_applies_and_empty_clears() {
    let _guard = env_guard(); // clears CYDRIVE_WEBDAV_PASSWORD on drop too
    let mut cfg = webdav_config();
    cfg.webdav_password = None;
    // A lone username would not validate, but with_env_overrides does
    // not validate — the env value completes the pair at assembly time.
    std::env::set_var("CYDRIVE_WEBDAV_PASSWORD", "env-password");
    let overridden = cfg.clone().with_env_overrides();
    assert_eq!(
        overridden.webdav_password.as_deref(),
        Some("env-password"),
        "env > file for the webdav password"
    );
    overridden
        .validate()
        .expect("the env value completes the credential pair");

    std::env::set_var("CYDRIVE_WEBDAV_PASSWORD", "");
    let cleared = cfg.with_env_overrides();
    assert_eq!(
        cleared.webdav_password, None,
        "a set-but-empty variable clears back to None"
    );
    std::env::remove_var("CYDRIVE_WEBDAV_PASSWORD");
}

#[test]
fn volume_file_accepts_the_webdav_key_group() {
    // The six keys are volume-scoped (K19 partition — a volume file
    // carries its own share credentials); a minimal webdav volume file
    // loads through the strict volume surface.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nas.toml");
    fs::write(
        &path,
        concat!(
            "backend = \"webdav\"\n",
            "webdav_url = \"https://nas.lan:5006/dav\"\n",
            "webdav_username = \"spike\"\n",
            "webdav_password = \"pw\"\n",
            "webdav_auth = \"digest\"\n",
            "webdav_vendor = \"nextcloud\"\n",
            "webdav_accept_invalid_certs = false\n",
        ),
    )
    .expect("write volume file");

    let spec = cloudkit_core::config::load_volume_config(&path)
        .expect("a volume file carrying the webdav key group loads");
    assert_eq!(spec.settings.backend, Backend::Webdav);
    assert_eq!(
        spec.settings.webdav_url.as_deref(),
        Some("https://nas.lan:5006/dav")
    );
    spec.settings
        .validate()
        .expect("the parsed volume settings validate");
}

#[test]
fn webdav_is_a_sync_participant_like_pan115_and_pan123() {
    // Phase 7 / WD1b: the sync gate is open for webdav (the pan115/
    // pan123 ruling; `is_sync_supported` uses the !matches! form, so no
    // code arm was needed — this pin keeps it that way).
    assert!(cloudkit_core::sync::is_sync_supported(&Backend::Webdav));
    assert!(!cloudkit_core::sync::is_sync_supported(&Backend::Local));
    assert!(!cloudkit_core::sync::is_sync_supported(&Backend::Sftp));
}

#[test]
fn webdav_password_parse_errors_are_redacted() {
    // R3 / M3 funnel: a broken-quote toml line for a SECRET_VALUED_KEYS
    // member must not carry the value into the parse error (the WD1b
    // addition of webdav_password rides the same redaction).
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        concat!(
            "backend = \"webdav\"\n",
            "webdav_url = \"https://nas.lan:5006/dav\"\n",
            "webdav_password = \"broken-quote-value\n",
        ),
    )
    .expect("write config.toml");
    let err = CyDriveConfig::load_toml(&path).expect_err("the broken toml line must fail the load");
    let message = err.to_string();
    assert!(
        message.contains("webdav_password"),
        "the key name stays for diagnosis: {message}"
    );
    assert!(
        !message.contains("broken-quote-value"),
        "the credential VALUE never rides the error: {message}"
    );
}
