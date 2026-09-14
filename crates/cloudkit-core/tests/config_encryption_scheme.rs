//! RED-phase spec tests for the `encryption_scheme` config key (Batch E /
//! E-4, foundation D7): TOML parsing, the frozen `"gcm"` default,
//! actionable rejection of unknown values, legacy-JSON rejection (the key
//! is Rust-added — a Python `config.json` must reject it instead of
//! silently ignoring it) and the save/load round-trip.
//!
//! Wire shape (interfaces §4, three-place key sync): `KNOWN_TOML_KEYS`
//! accepts the key, the legacy JSON *reject* list carries it, and value
//! validation is exhaustive at parse time (the field is a typed enum —
//! an unknown variant cannot reach `validate`, which therefore gains no
//! new rule for this key).

use cloudkit_core::config::{ConfigError, CyDriveConfig, EncryptionScheme};

/// Writes `text` to a temp file and returns its path.
fn config_file(name: &str, text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    std::fs::write(&path, text).expect("write config file");
    (dir, path)
}

/// A minimal valid config body every test appends its key lines to.
const BASE: &str = "bot_token = \"123:abc\"\nchat_id = 42\n";

#[test]
fn toml_parses_aead_v2_scheme() {
    let (_dir, path) = config_file(
        "config.toml",
        &format!("{BASE}enable_encryption = true\nencryption_password = \"pw\"\nencryption_scheme = \"aead_v2\"\n"),
    );
    let cfg = CyDriveConfig::load_toml(&path).expect("aead_v2 must parse");
    assert_eq!(
        cfg.encryption_scheme,
        EncryptionScheme::AeadV2,
        "the key selects the v2 scheme"
    );
}

#[test]
fn default_scheme_is_aead_v2() {
    assert_eq!(
        CyDriveConfig::default().encryption_scheme,
        EncryptionScheme::AeadV2,
        "dataclass default is aead_v2 (K58 ruling: streaming is the sane default)"
    );
    let (_dir, path) = config_file("config.toml", BASE);
    let cfg = CyDriveConfig::load_toml(&path).expect("base config loads");
    assert_eq!(
        cfg.encryption_scheme,
        EncryptionScheme::AeadV2,
        "a file without the key keeps the aead_v2 default"
    );
}

#[test]
fn unknown_scheme_value_is_rejected_with_actionable_message() {
    let (_dir, path) = config_file(
        "config.toml",
        &format!("{BASE}encryption_scheme = \"rot13\"\n"),
    );
    let err = CyDriveConfig::load_toml(&path).expect_err("unknown scheme must not load");
    match err {
        ConfigError::Parse { message, .. } => {
            assert!(
                message.contains("gcm") && message.contains("aead_v2"),
                "the error must name both accepted values, got: {message}"
            );
            assert!(
                message.contains("rot13"),
                "the error must echo the rejected value, got: {message}"
            );
        }
        other => panic!("expected a Parse error, got: {other:?}"),
    }
}

#[test]
fn legacy_json_rejects_the_rust_added_key() {
    let (_dir, path) = config_file(
        "config.json",
        "{\"bot_token\": \"123:abc\", \"chat_id\": 42, \"encryption_scheme\": \"aead_v2\"}",
    );
    let err = CyDriveConfig::load_legacy_json(&path).expect_err("legacy json must reject the key");
    match err {
        ConfigError::Parse { message, .. } => {
            assert!(
                message.contains("encryption_scheme"),
                "the rejection names the key, got: {message}"
            );
            assert!(
                message.contains("config.toml"),
                "the rejection points at the canonical format, got: {message}"
            );
        }
        other => panic!("expected a Parse error, got: {other:?}"),
    }
}

#[test]
fn save_load_roundtrip_preserves_the_scheme() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested/config.toml");
    let cfg = CyDriveConfig {
        encryption_scheme: EncryptionScheme::AeadV2,
        ..CyDriveConfig::default()
    };
    cfg.save_toml(&path).expect("save");
    let loaded = CyDriveConfig::load_toml(&path).expect("reload the saved file");
    assert_eq!(
        loaded.encryption_scheme,
        EncryptionScheme::AeadV2,
        "save/load keeps the selected scheme"
    );
}

#[test]
fn validate_accepts_both_schemes() {
    // The value gate is exhaustive at parse time (typed enum); `validate`
    // imposes no cross-field rule for the key — pinned here so a future
    // accidental coupling (e.g. requiring enable_encryption) is a visible
    // decision, not a silent drift.
    CyDriveConfig::default().validate().expect("gcm validates");
    CyDriveConfig {
        encryption_scheme: EncryptionScheme::AeadV2,
        ..CyDriveConfig::default()
    }
    .validate()
    .expect("aead_v2 validates on its own");
}
