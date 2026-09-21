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
