//! RED-phase tests for `cydrive_core::config`.
//!
//! Contract under test: Python `cydrive/config.py` (field set, defaults and
//! legacy `config.json` handling — unknown keys ignored, missing fields
//! defaulted) plus the Rust design layering from `docs/rust-rewrite-design.md`:
//! `config.toml` as canonical format, `CYDRIVE_*` environment overrides with
//! precedence env > file > defaults, and upfront [`CyDriveConfig::validate`].

use std::fs;
use std::sync::{Mutex, MutexGuard};

use cydrive_core::config::{ConfigError, CyDriveConfig};

// ------------------------------------------------------------- helpers ---

/// [`CyDriveConfig::default`] with `bot_token`/`chat_id` replaced — the
/// minimal "configured" shape used as expectation baseline everywhere.
fn default_with(bot_token: &str, chat_id: i64) -> CyDriveConfig {
    CyDriveConfig {
        bot_token: bot_token.to_string(),
        chat_id,
        ..CyDriveConfig::default()
    }
}

/// Default config with `chunk_size_mb` replaced (single-field variants keep
/// clippy's `field_reassign_with_default` happy).
fn with_chunk_size(chunk_size_mb: u64) -> CyDriveConfig {
    CyDriveConfig {
        chunk_size_mb,
        ..CyDriveConfig::default()
    }
}

/// Default config with `drive_letter` replaced.
fn with_drive_letter(drive_letter: &str) -> CyDriveConfig {
    CyDriveConfig {
        drive_letter: drive_letter.to_string(),
        ..CyDriveConfig::default()
    }
}

/// Encryption enabled, carrying the given password.
fn with_encryption(encryption_password: Option<&str>) -> CyDriveConfig {
    CyDriveConfig {
        enable_encryption: true,
        encryption_password: encryption_password.map(str::to_string),
        ..CyDriveConfig::default()
    }
}

/// Every environment key [`CyDriveConfig::with_env_overrides`] recognises.
const ENV_KEYS: &[&str] = &[
    "CYDRIVE_BOT_TOKEN",
    "CYDRIVE_CHAT_ID",
    "CYDRIVE_WEBDAV_PORT",
    "CYDRIVE_WEB_UI_PORT",
    "CYDRIVE_DRIVE_LETTER",
    "CYDRIVE_CHUNK_SIZE_MB",
    "CYDRIVE_ENABLE_ENCRYPTION",
];

/// Serialises every test that touches process-global environment state
/// (tests in one binary share one process; `set_var`/`remove_var` race).
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

// ------------------------------------------------------------ defaults ---

#[test]
fn default_matches_python_config() {
    let cfg = CyDriveConfig::default();
    assert_eq!(cfg.bot_token, "");
    assert_eq!(cfg.chat_id, 0);
    assert_eq!(cfg.api_id, 6);
    assert_eq!(cfg.api_hash, "eb06d4abfb49dc3eeb1aeb98ae0f581e");
    assert_eq!(cfg.storage_path, "./Telegram_Drive");
    assert_eq!(cfg.cache_path, "./Telegram_Cache");
    assert_eq!(cfg.db_path, "./cydrive_meta.db");
    assert_eq!(cfg.webdav_host, "127.0.0.1");
    assert_eq!(cfg.webdav_port, 8080);
    assert_eq!(cfg.web_ui_host, "127.0.0.1");
    assert_eq!(cfg.web_ui_port, 8088);
    assert!(cfg.enable_web_ui);
    assert_eq!(cfg.drive_letter, "Y:");
    assert!(cfg.auto_mount_drive);
    assert_eq!(cfg.chunk_size_mb, 1900);
    assert_eq!(cfg.cache_limit_gb, 20);
    assert_eq!(cfg.encryption_password, None);
    assert!(!cfg.enable_encryption);
}

// ------------------------------------------------------- legacy config ---

#[test]
fn load_legacy_json_reads_example_shape() {
    // Exact shape of the Python repo's config.example.json: token + chat_id.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    fs::write(
        &path,
        r#"{ "bot_token": "123456:ABC-DEF", "chat_id": 123456789 }"#,
    )
    .expect("write config.json");

    let cfg = CyDriveConfig::load_legacy_json(&path).expect("example shape loads");
    assert_eq!(cfg, default_with("123456:ABC-DEF", 123456789));
}

#[test]
fn load_legacy_json_ignores_unknown_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    fs::write(
        &path,
        r#"{
            "bot_token": "111:AA",
            "chat_id": -1001234567890,
            "hack": 42,
            "chunk_size_mb": 500
        }"#,
    )
    .expect("write config.json");

    let cfg = CyDriveConfig::load_legacy_json(&path).expect("unknown keys are ignored");
    let mut expected = default_with("111:AA", -1001234567890);
    expected.chunk_size_mb = 500;
    assert_eq!(cfg, expected);
}

#[test]
fn load_legacy_json_missing_file_is_read_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("does-not-exist.json");

    let err = CyDriveConfig::load_legacy_json(&path).expect_err("missing file must fail");
    assert!(
        matches!(err, ConfigError::Read { .. }),
        "expected Read error, got: {err:?}"
    );
}

#[test]
fn load_legacy_json_invalid_json_is_parse_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    fs::write(&path, "this is { not json").expect("write config.json");

    let err = CyDriveConfig::load_legacy_json(&path).expect_err("malformed JSON must fail");
    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "expected Parse error, got: {err:?}"
    );
}

#[test]
fn load_legacy_json_type_mismatch_is_parse_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    fs::write(&path, r#"{ "bot_token": 12345, "chat_id": 1 }"#).expect("write config.json");

    let err = CyDriveConfig::load_legacy_json(&path).expect_err("wrong-typed value must fail");
    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "expected Parse error, got: {err:?}"
    );
}

// ----------------------------------------------------------- toml i/o ---

#[test]
fn toml_roundtrip_preserves_full_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Parent dir absent on purpose: save_toml must create it
    // (Python save() os.makedirs(dirname) parity).
    let path = dir.path().join("nested").join("config.toml");

    let cfg = CyDriveConfig {
        bot_token: "987:zyx-wvu".to_string(),
        chat_id: -100200300,
        api_id: 2040,
        api_hash: "deadbeefdeadbeef".to_string(),
        storage_path: "/srv/storage".to_string(),
        cache_path: "/srv/cache".to_string(),
        db_path: "/srv/meta.db".to_string(),
        webdav_host: "0.0.0.0".to_string(),
        webdav_port: 9090,
        web_ui_host: "192.168.1.10".to_string(),
        web_ui_port: 9443,
        enable_web_ui: false,
        drive_letter: "Z:".to_string(),
        auto_mount_drive: false,
        chunk_size_mb: 2000,
        cache_limit_gb: 7,
        encryption_password: None,
        enable_encryption: false,
    };

    cfg.save_toml(&path).expect("save_toml");
    let loaded = CyDriveConfig::load_toml(&path).expect("load_toml");
    assert_eq!(loaded, cfg);
}

#[test]
fn toml_roundtrip_preserves_optional_password() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");

    let cfg = CyDriveConfig {
        encryption_password: Some("hunter2-secret".to_string()),
        enable_encryption: true,
        ..default_with("10:AA", 10)
    };

    cfg.save_toml(&path).expect("save_toml");
    let loaded = CyDriveConfig::load_toml(&path).expect("load_toml");
    assert_eq!(loaded, cfg);
}

#[test]
fn toml_missing_fields_fall_back_to_defaults() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    // Hand-written minimal TOML: only two keys present, everything else
    // must come from #[serde(default)].
    fs::write(&path, "bot_token = \"31337:tok\"\nchat_id = 31337\n").expect("write config.toml");

    let cfg = CyDriveConfig::load_toml(&path).expect("minimal toml loads");
    assert_eq!(cfg, default_with("31337:tok", 31337));
}

#[test]
fn toml_unknown_key_is_parse_error() {
    // Canonical format is schema-strict (catches typos), unlike legacy JSON.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        "bot_token = \"1:a\"\nchat_id = 1\nnot_a_field = true\n",
    )
    .expect("write config.toml");

    let err = CyDriveConfig::load_toml(&path).expect_err("unknown toml key must fail");
    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "expected Parse error, got: {err:?}"
    );
}

// ---------------------------------------------------- env overrides ---

#[test]
fn env_overrides_apply_known_keys() {
    let _env = env_guard();
    std::env::set_var("CYDRIVE_BOT_TOKEN", "42:ENV");
    std::env::set_var("CYDRIVE_WEBDAV_PORT", "9911");
    std::env::set_var("CYDRIVE_CHUNK_SIZE_MB", "512");
    std::env::set_var("CYDRIVE_DRIVE_LETTER", "Z:");

    let cfg = CyDriveConfig::default().with_env_overrides();
    let mut expected = default_with("42:ENV", 0);
    expected.webdav_port = 9911;
    expected.chunk_size_mb = 512;
    expected.drive_letter = "Z:".to_string();
    assert_eq!(cfg, expected);
}

#[test]
fn env_override_non_numeric_chat_id_is_ignored() {
    let _env = env_guard();
    std::env::set_var("CYDRIVE_CHAT_ID", "not-a-number");

    let cfg = default_with("1:a", 777).with_env_overrides();
    assert_eq!(
        cfg.chat_id, 777,
        "unparseable CYDRIVE_CHAT_ID must be ignored"
    );
}

#[test]
fn env_override_invalid_numbers_are_ignored_per_key() {
    let _env = env_guard();
    std::env::set_var("CYDRIVE_WEBDAV_PORT", "70000"); // > u16::MAX
    std::env::set_var("CYDRIVE_WEB_UI_PORT", "9090"); // valid sibling still applies
    std::env::set_var("CYDRIVE_CHUNK_SIZE_MB", "-5"); // negative, not a u64

    let cfg = CyDriveConfig::default().with_env_overrides();
    assert_eq!(
        cfg.webdav_port, 8080,
        "out-of-range port must keep file/default value"
    );
    assert_eq!(cfg.web_ui_port, 9090);
    assert_eq!(cfg.chunk_size_mb, 1900);
}

#[test]
fn env_override_enable_encryption_truthiness() {
    let _env = env_guard();
    for (raw, expected) in [("1", true), ("true", true), ("0", false), ("yes", false)] {
        std::env::set_var("CYDRIVE_ENABLE_ENCRYPTION", raw);
        let cfg = CyDriveConfig::default().with_env_overrides();
        assert_eq!(
            cfg.enable_encryption, expected,
            "CYDRIVE_ENABLE_ENCRYPTION={raw:?}"
        );
    }
}

#[test]
fn env_overrides_leave_unset_keys_alone() {
    let _env = env_guard();
    std::env::set_var("CYDRIVE_BOT_TOKEN", "9:ONLY");

    let cfg = default_with("from:file", 555).with_env_overrides();
    assert_eq!(cfg, default_with("9:ONLY", 555));
}

#[test]
fn with_env_overrides_without_env_returns_equivalent_config() {
    let _env = env_guard();
    let cfg = default_with("7:zz", 7);
    let snapshot = cfg.clone();

    let out = cfg.with_env_overrides();
    assert_eq!(out, snapshot);
}

// ------------------------------------------------------ is_configured ---

#[test]
fn is_configured_is_false_for_default() {
    assert!(!CyDriveConfig::default().is_configured());
}

#[test]
fn is_configured_true_when_token_has_colon_and_chat_id_nonzero() {
    assert!(default_with("123456:ABC-DEF", 123456789).is_configured());
}

#[test]
fn is_configured_false_without_colon_or_with_zero_chat_id() {
    // Token without ':': not a plausible bot token.
    assert!(!default_with("123456ABCDEF", 123456789).is_configured());
    // Token shape fine but chat_id unset.
    assert!(!default_with("123456:ABC-DEF", 0).is_configured());
    // Empty token.
    assert!(!default_with("", 123456789).is_configured());
}

// ------------------------------------------------------------ validate ---

#[test]
fn validate_accepts_default_config() {
    CyDriveConfig::default()
        .validate()
        .expect("defaults (empty token = not configured yet) must validate");
}

#[test]
fn validate_rejects_token_without_colon() {
    let err = default_with("no-colon-here", 1)
        .validate()
        .expect_err("non-empty token without ':' must be invalid");
    assert!(
        matches!(err, ConfigError::Invalid(_)),
        "expected Invalid, got: {err:?}"
    );
}

#[test]
fn validate_chunk_size_bounds() {
    for bad in [0u64, 2001] {
        let cfg = with_chunk_size(bad);
        assert!(
            matches!(cfg.validate(), Err(ConfigError::Invalid(_))),
            "chunk_size_mb={bad} must be invalid"
        );
    }
    for good in [1u64, 2000] {
        with_chunk_size(good)
            .validate()
            .unwrap_or_else(|e| panic!("chunk_size_mb={good} must be valid: {e}"));
    }
}

#[test]
fn validate_drive_letter_forms() {
    // Accepted: one ASCII letter, optional trailing ':', any case — the
    // mounter canonicalises to uppercase "X:".
    for ok in ["Y:", "y", "y:", "N"] {
        with_drive_letter(ok)
            .validate()
            .unwrap_or_else(|e| panic!("drive_letter={ok:?} must be valid: {e}"));
    }
    // Rejected: empty, multi-char, digits, Python-Linux "N/A" placeholder.
    for bad in ["", "YY", "5:", "Y::", "N/A"] {
        assert!(
            matches!(
                with_drive_letter(bad).validate(),
                Err(ConfigError::Invalid(_))
            ),
            "drive_letter={bad:?} must be invalid"
        );
    }
}

#[test]
fn validate_rejects_zero_ports() {
    let webdav = CyDriveConfig {
        webdav_port: 0,
        ..CyDriveConfig::default()
    };
    assert!(
        matches!(webdav.validate(), Err(ConfigError::Invalid(_))),
        "webdav_port=0 must be invalid"
    );

    let web_ui = CyDriveConfig {
        web_ui_port: 0,
        ..CyDriveConfig::default()
    };
    assert!(
        matches!(web_ui.validate(), Err(ConfigError::Invalid(_))),
        "web_ui_port=0 must be invalid"
    );
}

#[test]
fn validate_requires_non_empty_password_for_encryption() {
    assert!(
        matches!(
            with_encryption(None).validate(),
            Err(ConfigError::Invalid(_))
        ),
        "encryption without a password must be invalid"
    );

    assert!(
        matches!(
            with_encryption(Some("")).validate(),
            Err(ConfigError::Invalid(_))
        ),
        "encryption with an empty password must be invalid"
    );

    with_encryption(Some("secret"))
        .validate()
        .expect("Some non-empty password with encryption is fine");
}
