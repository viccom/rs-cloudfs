//! RED-phase tests for `cloudkit_core::config`.
//!
//! Contract under test: Python `cydrive/config.py` (field set, defaults and
//! legacy `config.json` handling — unknown keys ignored, missing fields
//! defaulted) plus the Rust design layering from `docs/rust-rewrite-design.md`:
//! `config.toml` as canonical format, `CYDRIVE_*` environment overrides with
//! precedence env > file > defaults, and upfront [`CyDriveConfig::validate`].

use std::fs;
use std::sync::{Mutex, MutexGuard};

use cloudkit_core::config::{Backend, ConfigError, CyDriveConfig, EncryptionScheme, MountBackend};

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
        allow_remote_admin: true,
        drive_letter: "Z:".to_string(),
        auto_mount_drive: false,
        // Phase 3 / K40: the mount backend round-trips too.
        mount_backend: MountBackend::Winfsp,
        mount_point: None,
        chunk_size_mb: 2000,
        cache_limit_gb: 7,
        upload_workers: 2,
        queue_capacity: 256,
        hydrate_timeout_secs: 180,
        encryption_password: None,
        enable_encryption: false,
        encryption_scheme: EncryptionScheme::Gcm,
        proxy_url: None,
        sync_url: None,
        sync_secret: None,
        sync_interval_secs: 300,
        // Phase 2 / K17: the multi-backend keys (defaults keep this
        // round-trip byte-compatible with the pre-Phase-2 shape).
        backend: Backend::Telegram,
        baidu_root: "/apps/cloudfs".to_string(),
        baidu_app_key: None,
        baidu_app_secret: None,
        baidu_access_token: None,
        baidu_refresh_token: None,
        local_root: None,
        // Phase 4 / SF1: the sftp key group round-trips too (all unset —
        // the exhaustive-field shape of this test).
        sftp_host: None,
        sftp_port: None,
        sftp_username: None,
        sftp_password: None,
        sftp_private_key_path: None,
        sftp_private_key_passphrase: None,
        sftp_host_fingerprint: None,
        sftp_root: None,
        // Phase 5 / 115-1: the pan115 key group round-trips too (all
        // unset — the exhaustive-field shape of this test).
        pan115_client_id: None,
        pan115_access_token: None,
        pan115_refresh_token: None,
        pan115_root: None,
        // Phase 6 / 123-1: the pan123 key group round-trips too (all
        // unset — the exhaustive-field shape of this test).
        pan123_token: None,
        pan123_root: None,
        // Phase 7 / WD1b: the webdav key group round-trips too (all
        // unset — the exhaustive-field shape of this test).
        webdav_url: None,
        webdav_username: None,
        webdav_password: None,
        webdav_auth: None,
        webdav_vendor: None,
        webdav_accept_invalid_certs: None,
        volumes_dir: None,
        // Phase 3.6 / RV0: the volume enable switch round-trips too.
        enabled: true,
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

/// Review M3 follow-up (fix(core) extension): the single-volume
/// `config.toml` loader shares the volume-file redaction contract — its
/// parse errors ride the same `ConfigError::Parse` message into the
/// boot error path (the log and the console), so a TOML syntax error on
/// a credential-carrying line must not leak the value either. The key
/// name and the line/column position stay for diagnosis.
#[test]
fn load_toml_parse_error_redacts_credential_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(&path, "bot_token = \"SECRET-MARKER-456\nchat_id = 1\n").expect("write config.toml");

    let err =
        CyDriveConfig::load_toml(&path).expect_err("the broken-quote line must fail to parse");
    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "expected Parse error, got: {err:?}"
    );
    let message = err.to_string();
    assert!(
        !message.contains("SECRET-MARKER-456"),
        "the credential value must be redacted from the parse error: {message}"
    );
    assert!(
        message.contains("bot_token"),
        "the key name stays for diagnosis: {message}"
    );
    assert!(
        message.contains("line"),
        "the position stays for diagnosis: {message}"
    );
}

/// Review M3 follow-up, serde construction point: a wrong-typed
/// credential value in `config.toml` fails in `try_into`, and the serde
/// error quotes the value (the embedded source line and the `invalid
/// type: integer \`...\`` reason) — both occurrences must be masked.
#[test]
fn load_toml_type_error_redacts_credential_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(&path, "encryption_password = 9999999999012345\n").expect("write config.toml");

    let err = CyDriveConfig::load_toml(&path).expect_err("an integer password must fail to parse");
    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "expected Parse error, got: {err:?}"
    );
    let message = err.to_string();
    assert!(
        !message.contains("9999999999012345"),
        "the credential value must be redacted from the type error: {message}"
    );
    assert!(
        message.contains("encryption_password"),
        "the key name stays for diagnosis: {message}"
    );
}

/// Review M3 follow-up, over-redaction guard: a `config.toml` syntax
/// error on a line with NO credential key keeps its message verbatim —
/// the diagnostic value of the embedded source line survives the
/// redaction pass untouched (pin, green on both sides of the fix).
#[test]
fn load_toml_parse_error_without_credentials_stays_verbatim() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(&path, "local_root = \"plain-offending-value\n").expect("write config.toml");

    let err =
        CyDriveConfig::load_toml(&path).expect_err("the broken-quote line must fail to parse");
    let message = err.to_string();
    assert!(
        message.contains("local_root = \"plain-offending-value"),
        "a message without credential keys is not redacted: {message}"
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

// ------------------------------------------------ tuning keys (C6) ---
//
// Tier-1 plan (`docs/plans/2026-09-03-tier1-utilities.md`, contract C6):
// three new `config.toml` tuning keys — `upload_workers` (`u32`, default 2,
// valid `1..=32`), `queue_capacity` (`u32`, default 256, must be
// `>= upload_workers` and `<= 100_000`) and `hydrate_timeout_secs` (`u64`,
// default 1800 — raised from the original 180 by the review-followup BUG②
// contract change: real-machine downstream bandwidth through a local proxy
// measured ~0.45 MB/s (decisions.md 2026-09-03, Tier-1 真机端到端 发现①),
// so the 180s default timed out every file above ~80 MB; valid range
// `1..=86_400`, an explicit 180 stays legal). Violations are
// [`ConfigError::Invalid`] via [`CyDriveConfig::validate`], styled after the
// existing rules. The legacy `config.json` key set stays frozen at the
// Python dataclass fields — it must not grow the new keys.

#[test]
fn toml_new_tuning_keys_parse_with_defaults_when_absent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    // Minimal TOML without any of the three tuning keys — the shape every
    // pre-tier1 config file has; the keys must fall back to defaults.
    fs::write(&path, "bot_token = \"tune:def\"\nchat_id = 42\n").expect("write config.toml");

    let cfg = CyDriveConfig::load_toml(&path).expect("toml without tuning keys loads");
    assert_eq!(cfg.upload_workers, 2, "upload_workers default must be 2");
    assert_eq!(
        cfg.queue_capacity, 256,
        "queue_capacity default must be 256"
    );
    assert_eq!(
        cfg.hydrate_timeout_secs, 1800,
        "hydrate_timeout_secs default must be 1800 (BUG②: raised from 180 — real-machine bandwidth, decisions.md 2026-09-03)"
    );
}

#[test]
fn hydrate_timeout_default_is_1800_across_the_chain() {
    // BUG② pin: every definition point of the default must stay aligned
    // at 1800 — the serde default function (key absent from a partial
    // config.toml), the `CyDriveConfig::default` impl, and the
    // `VfsConfig::default` fallback at the end of the
    // config -> vfs_config conversion chain.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(&path, "bot_token = \"1:a\"\nchat_id = 1\n").expect("write config.toml");

    let loaded = CyDriveConfig::load_toml(&path).expect("toml without the key loads");
    assert_eq!(
        loaded.hydrate_timeout_secs, 1800,
        "key absent -> serde default 1800"
    );
    assert_eq!(
        CyDriveConfig::default().hydrate_timeout_secs,
        1800,
        "Default impl aligned with the serde default"
    );
    assert_eq!(
        cloudkit_core::vfs::VfsConfig::default().hydrate_timeout,
        std::time::Duration::from_secs(1800),
        "VfsConfig::default (the chain's fallback when no config maps over) aligned"
    );
}

#[test]
fn hydrate_timeout_explicit_180_is_still_valid() {
    // The default change must not over-reach: an explicit 180 remains a
    // legal, load-bearing value (loads verbatim and passes validate).
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        "bot_token = \"1:a\"\nchat_id = 1\nhydrate_timeout_secs = 180\n",
    )
    .expect("write config.toml");

    let cfg = CyDriveConfig::load_toml(&path).expect("explicit 180 loads");
    assert_eq!(cfg.hydrate_timeout_secs, 180);
    cfg.validate().expect("explicit 180 passes validation");
}

#[test]
fn toml_new_tuning_keys_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");

    let cfg = CyDriveConfig {
        upload_workers: 4,
        queue_capacity: 512,
        hydrate_timeout_secs: 300,
        ..default_with("tune:rt", 42)
    };

    cfg.save_toml(&path).expect("save_toml");
    let loaded = CyDriveConfig::load_toml(&path).expect("load_toml");
    assert_eq!(loaded, cfg, "tuning keys must round-trip through toml");
    assert_eq!(loaded.upload_workers, 4);
    assert_eq!(loaded.queue_capacity, 512);
    assert_eq!(loaded.hydrate_timeout_secs, 300);
}

#[test]
fn toml_upload_workers_out_of_range_rejected() {
    // Contract C6: `upload_workers` must be in `1..=32`.
    for bad in [0u32, 33] {
        let cfg = CyDriveConfig {
            upload_workers: bad,
            ..CyDriveConfig::default()
        };
        assert!(
            matches!(cfg.validate(), Err(ConfigError::Invalid(_))),
            "upload_workers={bad} must be invalid"
        );
    }
}

#[test]
fn toml_queue_capacity_below_workers_rejected() {
    // Contract C6: `queue_capacity` must be `>= upload_workers`.
    let cfg = CyDriveConfig {
        upload_workers: 2,
        queue_capacity: 1,
        ..CyDriveConfig::default()
    };
    assert!(
        matches!(cfg.validate(), Err(ConfigError::Invalid(_))),
        "queue_capacity below upload_workers must be invalid"
    );
}

#[test]
fn toml_hydrate_timeout_zero_rejected() {
    // Contract C6: `hydrate_timeout_secs` must be in `1..=86_400` — zero
    // and one past the upper bound are both rejected.
    for bad in [0u64, 86_401] {
        let cfg = CyDriveConfig {
            hydrate_timeout_secs: bad,
            ..CyDriveConfig::default()
        };
        assert!(
            matches!(cfg.validate(), Err(ConfigError::Invalid(_))),
            "hydrate_timeout_secs={bad} must be invalid"
        );
    }
}

#[test]
fn legacy_json_unknown_new_key_rejected() {
    // The legacy key set stays frozen at the Python dataclass fields: a
    // tuning key smuggled into a legacy `config.json` must be rejected, not
    // silently ignored (the user would believe it takes effect).
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    fs::write(
        &path,
        r#"{
            "bot_token": "111:AA",
            "chat_id": 5,
            "upload_workers": 8
        }"#,
    )
    .expect("write config.json");

    let err = CyDriveConfig::load_legacy_json(&path)
        .expect_err("legacy json must reject new tuning keys");
    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "expected Parse error, got: {err:?}"
    );
}

/// Tier-3: the optional Linux `mount_point` key (C4) — defaults to None,
/// parses verbatim, must be absolute when present, and the legacy json
/// rejects it like the other Rust-added tuning keys.
#[test]
fn toml_mount_point_parses_and_defaults() {
    let dir = tempfile::tempdir().expect("tempdir");
    let minimal = dir.path().join("minimal.toml");
    std::fs::write(&minimal, "bot_token = \"1:a\"\nchat_id = 7\n").expect("write");
    let cfg = CyDriveConfig::load_toml(&minimal).expect("minimal loads");
    assert_eq!(cfg.mount_point, None, "absent key means the default");

    let explicit = dir.path().join("explicit.toml");
    std::fs::write(
        &explicit,
        "bot_token = \"1:a\"\nchat_id = 7\nmount_point = \"/mnt/cydrive\"\n",
    )
    .expect("write");
    let cfg = CyDriveConfig::load_toml(&explicit).expect("explicit loads");
    assert_eq!(cfg.mount_point.as_deref(), Some("/mnt/cydrive"));
    cfg.validate().expect("absolute mount_point validates");
}

#[test]
fn toml_mount_point_requires_absolute() {
    let cfg = CyDriveConfig {
        mount_point: Some("relative/path".to_string()),
        ..CyDriveConfig::default()
    };
    assert!(matches!(cfg.validate(), Err(ConfigError::Invalid(_))));
}

#[test]
fn legacy_json_rejects_mount_point() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    std::fs::write(
        &path,
        r#"{ "bot_token": "1:a", "chat_id": 7, "mount_point": "/mnt/x" }"#,
    )
    .expect("write");
    let err = CyDriveConfig::load_legacy_json(&path).expect_err("legacy rejects mount_point");
    assert!(matches!(err, ConfigError::Parse { .. }), "got: {err:?}");
}
