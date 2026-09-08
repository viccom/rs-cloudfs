//! RED-phase tests for the sync-lite config keys of `cloudkit_core::config`.
//!
//! Contract under test (docs/plans/2026-09-04-sync-lite.md, «客户端»):
//! `CyDriveConfig` gains `sync_url` (`Option<String>`, `None` = feature
//! off, default) and `sync_interval_secs` (`u64`, default 300, valid
//! `1..=86_400`). Both keys are accepted by the canonical `config.toml`
//! (KNOWN_TOML_KEYS grows) and **rejected** by the legacy `config.json`
//! (tier-1 precedent: a tuning key the legacy loader cannot honour must
//! fail loudly, not be silently swallowed). `CYDRIVE_SYNC_URL` overrides
//! the file value env > file; an empty value clears it back to `None`
//! (proxy_url precedent). When set, `sync_url` must start with `http://`
//! or `https://` and carry a non-empty host — the bare scheme, a
//! port-only authority and a path-only URL are all rejected with an
//! actionable [`ConfigError::Invalid`] message; `None` skips the check.
//!
//! The companion `sync_secret` key (`Option<String>`, default `None`)
//! carries the optional family-level shared secret: accepted by the
//! canonical TOML (no format constraint), rejected by the legacy JSON
//! (same precedent) and scrubbed from programmatic writes. Its env route
//! is the CLI-layer `CYDRIVE_SYNC_SECRET`, deliberately outside the core
//! env-override set — covered by the CLI tests, not here.

use std::fs;
use std::sync::{Mutex, MutexGuard};

use cloudkit_core::config::{ConfigError, CyDriveConfig};

// ------------------------------------------------------------- helpers ---

/// [`CyDriveConfig::default`] with `bot_token`/`chat_id` replaced — the
/// minimal "configured" shape (mirrors the helper in `tests/config.rs`).
fn default_with(bot_token: &str, chat_id: i64) -> CyDriveConfig {
    CyDriveConfig {
        bot_token: bot_token.to_string(),
        chat_id,
        ..CyDriveConfig::default()
    }
}

/// Every environment key [`CyDriveConfig::with_env_overrides`] recognises
/// (the full set, so the guard also isolates against ambient env state).
const ENV_KEYS: &[&str] = &[
    "CYDRIVE_BOT_TOKEN",
    "CYDRIVE_CHAT_ID",
    "CYDRIVE_WEBDAV_PORT",
    "CYDRIVE_WEB_UI_PORT",
    "CYDRIVE_DRIVE_LETTER",
    "CYDRIVE_CHUNK_SIZE_MB",
    "CYDRIVE_ENABLE_ENCRYPTION",
    "CYDRIVE_PROXY_URL",
    "CYDRIVE_SYNC_URL",
];

/// Serialises every test that touches process-global environment state
/// (tests in one binary share one process; `set_var`/`remove_var` race) —
/// same precedent as `tests/config.rs`.
static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Holds [`ENV_MUTEX`] and clears all override keys on drop — including on
/// panic — so no env state leaks between tests.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for key in ENV_KEYS {
            std::env::remove_var(key);
        }
    }
}

/// Locks [`ENV_MUTEX`] and starts from a clean env (all override keys unset).
fn env_guard() -> EnvGuard {
    let _lock = ENV_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for key in ENV_KEYS {
        std::env::remove_var(key);
    }
    EnvGuard { _lock }
}

// -------------------------------------------------------------- tests ---

#[test]
fn default_sync_keys_are_off_and_300() {
    let cfg = CyDriveConfig::default();
    assert_eq!(cfg.sync_url, None, "default config must have sync off");
    assert_eq!(
        cfg.sync_interval_secs, 300,
        "sync_interval_secs default must be 300"
    );
}

#[test]
fn load_toml_reads_both_sync_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        concat!(
            "bot_token = \"123456:ABC-DEF\"\n",
            "chat_id = 123456789\n",
            "sync_url = \"https://sync.example.internal:8290\"\n",
            "sync_interval_secs = 60\n",
        ),
    )
    .expect("write config.toml");

    let cfg = CyDriveConfig::load_toml(&path).expect("config with sync keys loads");
    assert_eq!(
        cfg.sync_url.as_deref(),
        Some("https://sync.example.internal:8290"),
        "load_toml must pick up the sync_url key"
    );
    assert_eq!(cfg.sync_interval_secs, 60, "sync_interval_secs must parse");
    cfg.validate().expect("set sync keys validate");
}

#[test]
fn toml_roundtrip_preserves_sync_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");

    let cfg = CyDriveConfig {
        sync_url: Some("http://127.0.0.1:8290".to_string()),
        sync_interval_secs: 86_400,
        ..default_with("sync:rt", 42)
    };

    cfg.save_toml(&path).expect("save_toml");
    let loaded = CyDriveConfig::load_toml(&path).expect("load_toml");
    assert_eq!(loaded, cfg, "sync keys must survive the TOML round-trip");
}

#[test]
fn sync_interval_secs_bounds_are_inclusive() {
    // 1 and 86 400 are the inclusive bounds — both must validate.
    for good in [1u64, 86_400] {
        let cfg = CyDriveConfig {
            sync_interval_secs: good,
            ..CyDriveConfig::default()
        };
        cfg.validate()
            .unwrap_or_else(|e| panic!("sync_interval_secs={good} must be valid: {e}"));
    }
}

#[test]
fn sync_interval_secs_out_of_range_rejected() {
    // 0 and 86 401 are one step past each inclusive bound.
    for bad in [0u64, 86_401] {
        let cfg = CyDriveConfig {
            sync_interval_secs: bad,
            ..CyDriveConfig::default()
        };
        let err = cfg
            .validate()
            .expect_err("sync_interval_secs={bad} must be invalid");
        assert!(
            matches!(err, ConfigError::Invalid(ref message) if message.contains("sync_interval_secs")),
            "expected actionable Invalid error naming the key, got: {err:?}"
        );
    }
}

#[test]
fn sync_url_must_be_http_s_when_set() {
    // None skips the rule entirely (feature off).
    CyDriveConfig::default()
        .validate()
        .expect("sync_url None validates");

    // Both schemes are accepted.
    for good in ["http://127.0.0.1:8290", "https://sync.example.org"] {
        let cfg = CyDriveConfig {
            sync_url: Some(good.to_string()),
            ..CyDriveConfig::default()
        };
        cfg.validate()
            .unwrap_or_else(|e| panic!("sync_url={good:?} must be valid: {e}"));
    }

    // A non-HTTP scheme (or bare host) is rejected with an actionable
    // message naming the field and the accepted schemes.
    for bad in ["socks5://127.0.0.1:1080", "sync.example.org", "ftp://x"] {
        let cfg = CyDriveConfig {
            sync_url: Some(bad.to_string()),
            ..CyDriveConfig::default()
        };
        let err = cfg
            .validate()
            .expect_err("sync_url={bad:?} must be invalid");
        assert!(
            matches!(err, ConfigError::Invalid(ref message) if message.contains("sync_url")
                     && message.contains("http")),
            "expected actionable Invalid error naming sync_url and the schemes, got: {err:?}"
        );
    }
}

#[test]
fn sync_url_with_empty_host_rejected() {
    // Three malformed shapes that carry the right scheme prefix but no
    // host: the bare scheme (empty authority), a port-only authority and
    // a path-only URL — the prefix check alone waves them through.
    for bad in ["http://", "http://:8290/x", "http:///path"] {
        let cfg = CyDriveConfig {
            sync_url: Some(bad.to_string()),
            ..CyDriveConfig::default()
        };
        let err = cfg
            .validate()
            .expect_err("sync_url={bad:?} must be invalid");
        assert!(
            matches!(err, ConfigError::Invalid(ref message) if message.contains("sync_url")
                     && message.contains("host")),
            "expected actionable Invalid error naming sync_url and the missing host, got: {err:?}"
        );
    }

    // A host with a port and a bare host both stay valid.
    for good in ["http://sync.example.org:8290", "https://sync.example.org"] {
        let cfg = CyDriveConfig {
            sync_url: Some(good.to_string()),
            ..CyDriveConfig::default()
        };
        cfg.validate()
            .unwrap_or_else(|e| panic!("sync_url={good:?} must be valid: {e}"));
    }
}

#[test]
fn env_sync_url_overrides_file_and_empty_clears_it() {
    let _env = env_guard();

    // env > file: a set variable wins over the value from the config file.
    std::env::set_var("CYDRIVE_SYNC_URL", "https://env.example.org:8290");
    let cfg = default_with("1:a", 1).with_env_overrides();
    assert_eq!(
        cfg.sync_url.as_deref(),
        Some("https://env.example.org:8290"),
        "CYDRIVE_SYNC_URL must override the file value"
    );

    // An empty env value explicitly clears the sync URL (back to None).
    std::env::set_var("CYDRIVE_SYNC_URL", "");
    let cfg = default_with("1:a", 1).with_env_overrides();
    assert_eq!(
        cfg.sync_url, None,
        "empty CYDRIVE_SYNC_URL must clear the file value"
    );
}

#[test]
fn legacy_json_rejects_both_sync_keys() {
    // The legacy key set stays frozen at the Python dataclass fields: a
    // sync key smuggled into a legacy `config.json` must be rejected, not
    // silently ignored (the user would believe it takes effect — tier-1
    // 2026-09-03 precedent).
    for key in ["sync_url", "sync_interval_secs"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");
        let body = match key {
            "sync_url" => "\"https://sync.example.org\"",
            _ => "60",
        };
        fs::write(
            &path,
            format!(r#"{{ "bot_token": "111:AA", "chat_id": 5, "{key}": {body} }}"#),
        )
        .expect("write config.json");

        let err = CyDriveConfig::load_legacy_json(&path)
            .expect_err("legacy json must reject the sync keys");
        assert!(
            matches!(err, ConfigError::Parse { .. }),
            "expected Parse error for {key}, got: {err:?}"
        );
    }
}

// ---------------------------------------------------- sync_secret key ---

/// The `sync_secret` contract (secret-batch): `Option<String>`, default
/// `None` (send none), no format constraint in `validate` (any non-empty
/// string is a legal secret — a value that trims to empty is treated as
/// unset at the CLI resolution layer, not a validation error). The key
/// rides the canonical `config.toml` (user hand-writing allowed) but is
/// rejected by the legacy JSON and scrubbed from programmatic writes.
#[test]
fn default_sync_secret_is_none() {
    let cfg = CyDriveConfig::default();
    assert_eq!(cfg.sync_secret, None, "default config sends no secret");
}

#[test]
fn load_toml_reads_sync_secret() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        concat!(
            "bot_token = \"123456:ABC-DEF\"\n",
            "chat_id = 123456789\n",
            "sync_url = \"https://sync.example.internal:8290\"\n",
            "sync_secret = \"family-secret\"\n",
        ),
    )
    .expect("write config.toml");

    let cfg = CyDriveConfig::load_toml(&path).expect("config with sync_secret loads");
    assert_eq!(
        cfg.sync_secret.as_deref(),
        Some("family-secret"),
        "load_toml must pick up the sync_secret key"
    );
    cfg.validate().expect("a plain secret needs no format");
}

#[test]
fn sync_secret_has_no_format_constraint() {
    // Any non-empty string is legal — spaces, punctuation, anything; and
    // an empty string is no validation error either (it reads as unset
    // at the resolution layer, matching the sync_url empty-clears rule).
    for value in ["with spaces and !@#", "ünïcödé", ""] {
        let cfg = CyDriveConfig {
            sync_secret: Some(value.to_string()),
            ..CyDriveConfig::default()
        };
        cfg.validate()
            .unwrap_or_else(|e| panic!("sync_secret={value:?} must be valid: {e}"));
    }
    CyDriveConfig {
        sync_secret: None,
        ..CyDriveConfig::default()
    }
    .validate()
    .expect("None (feature off) validates");
}

#[test]
fn toml_roundtrip_preserves_sync_secret() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");

    let cfg = CyDriveConfig {
        sync_url: Some("http://127.0.0.1:8290".to_string()),
        sync_secret: Some("family-secret".to_string()),
        ..default_with("sync:secret:rt", 42)
    };

    cfg.save_toml(&path).expect("save_toml");
    let loaded = CyDriveConfig::load_toml(&path).expect("load_toml");
    assert_eq!(loaded, cfg, "sync_secret must survive the TOML round-trip");
}

#[test]
fn legacy_json_rejects_sync_secret() {
    // Same tier-1 precedent as sync_url: a legacy config.json cannot
    // honour the key, so it must fail loudly instead of being silently
    // filtered away.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    fs::write(
        &path,
        r#"{ "bot_token": "111:AA", "chat_id": 5, "sync_secret": "x" }"#,
    )
    .expect("write config.json");

    let err =
        CyDriveConfig::load_legacy_json(&path).expect_err("legacy json must reject sync_secret");
    assert!(
        matches!(err, ConfigError::Parse { ref message, .. } if message.contains("sync_secret")),
        "expected Parse error naming sync_secret, got: {err:?}"
    );
}

#[test]
fn scrubbed_write_omits_sync_secret() {
    // Programmatic writes (setup/migrate) go through save_toml_scrubbed:
    // the secret must stay out of the file — neither parsed back nor
    // present in the raw bytes — while every other field round-trips.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");

    let cfg = CyDriveConfig {
        sync_url: Some("http://127.0.0.1:8290".to_string()),
        sync_secret: Some("family-secret".to_string()),
        ..default_with("sync:scrub", 7)
    };
    cfg.save_toml_scrubbed(&path).expect("scrubbed save");

    let raw = fs::read_to_string(&path).expect("read the scrubbed file");
    assert!(
        !raw.contains("family-secret"),
        "the secret must not appear in the written file: {raw}"
    );
    let loaded = CyDriveConfig::load_toml(&path).expect("scrubbed config loads");
    assert_eq!(loaded.sync_secret, None, "the secret is scrubbed to None");
    assert_eq!(
        loaded.sync_url,
        Some("http://127.0.0.1:8290".to_string()),
        "non-secret fields survive the scrubbed write"
    );
}
