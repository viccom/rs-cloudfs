//! RED-phase spec tests for the `mount_backend` config key (Phase 3 /
//! K40): which mechanism exposes a volume as a Windows drive letter.
//!
//! Wire shape (interfaces §4, three-place key sync):
//!
//! - `mount_backend` (`"webdav"` | `"winfsp"`, default `"webdav"`): the
//!   default keeps every pre-Phase-3 config byte-compatible — without
//!   the key the drive mapping is the `net use` WebDAV path, exactly as
//!   before. An unknown value is rejected at parse time with an
//!   actionable message naming both accepted values (the typed-enum
//!   precedent `backend` / `encryption_scheme` set).
//! - the key rides `KNOWN_TOML_KEYS` and the **process** side of the
//!   K19 partition (the mount policy governs the whole multi-volume
//!   mount gate — a per-volume spelling would leave sibling mounts
//!   ungoverned, the same ruling `auto_mount_drive` carries).
//! - the legacy `config.json` **rejects** it (LEGACY_REJECTED_KEYS
//!   grows — a key the legacy loader cannot honour must fail loudly).
//! - `validate` gains no rule for the value: the enum is exhaustive at
//!   parse time, and *availability* (feature compiled in / WinFsp
//!   installed) is a runtime decision — K40's fallback, not a config
//!   error. A config may therefore ask for `"winfsp"` on a machine
//!   without WinFsp and still validate.

use cloudkit_core::config::{
    ConfigError, CyDriveConfig, MountBackend, KNOWN_TOML_KEYS, PROCESS_SCOPED_KEYS,
    VOLUME_SCOPED_KEYS,
};

/// Writes `text` to a temp file and returns its path (the
/// `config_encryption_scheme` helper shape).
fn config_file(name: &str, text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    std::fs::write(&path, text).expect("write config file");
    (dir, path)
}

/// A minimal valid config body every test appends its key lines to.
const BASE: &str = "bot_token = \"123:abc\"\nchat_id = 42\n";

// ------------------------------------------------------- value & default ---

#[test]
fn default_mount_backend_is_winfsp() {
    assert_eq!(
        CyDriveConfig::default().mount_backend,
        MountBackend::Winfsp,
        "the default is winfsp (负责人 2026-09-24 裁决: in-process mounts out of          the box, no net use fallback)"
    );
    let (_dir, path) = config_file("config.toml", BASE);
    let cfg = CyDriveConfig::load_toml(&path).expect("base config loads");
    assert_eq!(
        cfg.mount_backend,
        MountBackend::Winfsp,
        "a file without the key keeps the winfsp default"
    );
}

#[test]
fn mount_backend_parses_both_spellings() {
    for (text, expected) in [
        ("mount_backend = \"webdav\"\n", MountBackend::Webdav),
        ("mount_backend = \"winfsp\"\n", MountBackend::Winfsp),
    ] {
        let (_dir, path) = config_file("config.toml", &format!("{BASE}{text}"));
        let cfg = CyDriveConfig::load_toml(&path).expect("both spellings parse");
        assert_eq!(cfg.mount_backend, expected, "wire spelling: {text:?}");
        assert_eq!(
            cfg.mount_backend.as_str(),
            expected.as_str(),
            "as_str round-trips the wire spelling"
        );
    }
}

#[test]
fn unknown_mount_backend_is_rejected_with_actionable_message() {
    let (_dir, path) = config_file("config.toml", &format!("{BASE}mount_backend = \"npfs\"\n"));
    let err = CyDriveConfig::load_toml(&path).expect_err("unknown backend must not load");
    match err {
        ConfigError::Parse { message, .. } => {
            assert!(
                message.contains("webdav") && message.contains("winfsp"),
                "the error must name both accepted values, got: {message}"
            );
            assert!(
                message.contains("npfs"),
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
        "{\"bot_token\": \"123:abc\", \"chat_id\": 42, \"mount_backend\": \"winfsp\"}",
    );
    let err = CyDriveConfig::load_legacy_json(&path).expect_err("legacy json must reject the key");
    match err {
        ConfigError::Parse { message, .. } => {
            assert!(
                message.contains("mount_backend"),
                "the rejection names the key, got: {message}"
            );
        }
        other => panic!("expected a Parse error, got: {other:?}"),
    }
}

// --------------------------------------------------- key-list integration ---

#[test]
fn mount_backend_is_a_process_scoped_known_key() {
    assert!(
        KNOWN_TOML_KEYS.contains(&"mount_backend"),
        "the strict toml surface must accept the key"
    );
    assert!(
        PROCESS_SCOPED_KEYS.contains(&"mount_backend"),
        "the mount policy is process-level (governs every volume's mount)"
    );
    assert!(
        !VOLUME_SCOPED_KEYS.contains(&"mount_backend"),
        "a volume file must not carry it: the partition stays exact"
    );
}

#[test]
fn volume_file_rejects_the_process_scoped_key_with_guidance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let volume = dir.path().join("tg.toml");
    std::fs::write(
        &volume,
        "bot_token = \"1:a\"\nchat_id = 7\nmount_backend = \"winfsp\"\n",
    )
    .expect("write volume file");
    let err = cloudkit_core::config::load_volume_config(&volume)
        .expect_err("a volume file must not carry a process-scoped key");
    match err {
        ConfigError::Parse { message, .. } => {
            assert!(
                message.contains("mount_backend") && message.contains("config.toml"),
                "the rejection names the key and points back at config.toml, got: {message}"
            );
        }
        other => panic!("expected a Parse error, got: {other:?}"),
    }
}

// ------------------------------------------------------------ validate ---

#[test]
fn asking_for_winfsp_never_fails_validation() {
    // Availability is a runtime fact (K40 falls back visibly); the config
    // layer validates the value's shape only — which the enum already
    // guarantees. This test pins that `validate` gained no availability
    // rule: a machine without WinFsp (the CI box) must still accept it.
    let (_dir, path) = config_file(
        "config.toml",
        &format!("{BASE}mount_backend = \"winfsp\"\n"),
    );
    let cfg = CyDriveConfig::load_toml(&path).expect("winfsp request loads");
    cfg.validate()
        .expect("mount_backend must add no cross-field rule");
}

#[test]
fn mount_backend_roundtrips_through_toml() {
    let (_dir, path) = config_file(
        "config.toml",
        &format!("{BASE}mount_backend = \"winfsp\"\n"),
    );
    let cfg = CyDriveConfig::load_toml(&path).expect("load");
    let dir = tempfile::tempdir().expect("tempdir");
    let roundtrip = dir.path().join("roundtrip.toml");
    cfg.save_toml(&roundtrip).expect("save_toml");
    let loaded = CyDriveConfig::load_toml(&roundtrip).expect("reload");
    assert_eq!(
        loaded.mount_backend,
        MountBackend::Winfsp,
        "save/load must preserve the key"
    );
    assert_eq!(loaded, cfg, "the whole config round-trips");
}
