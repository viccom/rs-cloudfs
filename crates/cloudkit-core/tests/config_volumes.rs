//! RED-phase tests for Phase 2.5 / MV0: multi-volume configuration
//! (K19 volume files, discovery and validation).
//!
//! Contract under test: `docs/plans/2026-09-08-phase2-5-multivolume.md`
//! §1-K19/K27 and §2 — the `volumes_dir` process key, the key partition
//! (process-scoped vs volume-scoped, a total bipartition of
//! [`KNOWN_TOML_KEYS`]), per-volume `<name>.toml` files parsed through
//! the same strict schema surface as `config.toml` (unknown keys
//! rejected, process-level keys rejected with guidance), directory
//! discovery (stable sort, missing/empty dir errors) and the
//! multi-volume validation branches (process/volume key mixing,
//! drive-letter conflicts).

use std::fs;
use std::path::Path;

use cloudkit_core::config::{
    discover_volumes, ensure_no_volume_keys_in_process, load_volume_config, load_volumes,
    volume_show_json, Backend, ConfigError, CyDriveConfig, KNOWN_TOML_KEYS, PROCESS_SCOPED_KEYS,
    VOLUME_SCOPED_KEYS,
};

// ------------------------------------------------------------- helpers ---

/// Writes `text` to `path`, creating missing parent directories.
fn write_file(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(path, text).expect("write file");
}

/// A minimal legal local-backend volume file body.
const LOCAL_VOLUME_TOML: &str =
    "backend = \"local\"\nlocal_root = \"root\"\ndrive_letter = \"V\"\n";

// ---------------------------------------------------- key partitioning ---

#[test]
fn key_partition_bipartitions_known_toml_keys_exactly() {
    // The two scoped sets must be a total, non-overlapping bipartition
    // of KNOWN_TOML_KEYS: every known key is in exactly one set.
    let mut union: Vec<&str> = PROCESS_SCOPED_KEYS
        .iter()
        .chain(VOLUME_SCOPED_KEYS)
        .copied()
        .collect();
    union.sort_unstable();
    let union_len = union.len();
    union.dedup();
    assert_eq!(
        union.len(),
        union_len,
        "the scoped key sets must not repeat a key"
    );

    let mut known: Vec<&str> = KNOWN_TOML_KEYS.to_vec();
    known.sort_unstable();
    let known_len = known.len();
    known.dedup();
    assert_eq!(known.len(), known_len, "KNOWN_TOML_KEYS must not repeat");

    assert_eq!(
        union, known,
        "process-scoped + volume-scoped keys must cover KNOWN_TOML_KEYS exactly"
    );

    for key in PROCESS_SCOPED_KEYS {
        assert!(
            !VOLUME_SCOPED_KEYS.contains(key),
            "`{key}` must not be in both scoped sets"
        );
    }
}

#[test]
fn process_scoped_keys_are_the_process_globals() {
    // K19/§2: process-level keys are the web endpoints, the dashboard
    // switch, the (new) volumes directory and the mount pair — the
    // master switch and the backend selector (both govern the whole
    // multi-volume mount gate, so a per-volume spelling would leave
    // sibling mounts ungoverned by two different policies with no single
    // place to reason about them) — everything a single process shares
    // across volumes.
    assert_eq!(
        PROCESS_SCOPED_KEYS.to_vec(),
        vec![
            "volumes_dir",
            "webdav_host",
            "webdav_port",
            "web_ui_host",
            "web_ui_port",
            "enable_web_ui",
            "auto_mount_drive",
            "mount_backend",
        ],
        "the process-scoped key set must stay deliberate — new keys join \
         exactly one side of the partition"
    );
}

// ------------------------------------------------- volumes_dir key -------

#[test]
fn volumes_dir_absent_means_none_single_volume_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    write_file(&path, "bot_token = \"1:a\"\nchat_id = 7\n");

    let cfg = CyDriveConfig::load_toml(&path).expect("toml without volumes_dir loads");
    assert_eq!(cfg.volumes_dir, None, "absent key means single-volume mode");
    assert_eq!(
        CyDriveConfig::default().volumes_dir,
        None,
        "the default is single-volume mode (byte-compatible pre-Phase-2.5)"
    );
}

#[test]
fn volumes_dir_present_parses_and_roundtrips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    write_file(&path, "volumes_dir = \"volumes\"\nwebdav_port = 8080\n");

    let cfg = CyDriveConfig::load_toml(&path).expect("volumes_dir parses");
    assert_eq!(cfg.volumes_dir.as_deref(), Some("volumes"));

    let roundtrip = dir.path().join("roundtrip.toml");
    cfg.save_toml(&roundtrip).expect("save_toml");
    let loaded = CyDriveConfig::load_toml(&roundtrip).expect("reload");
    assert_eq!(loaded, cfg, "volumes_dir must round-trip through toml");
}

#[test]
fn volumes_dir_is_rejected_by_legacy_json() {
    // Legacy Python config.json is single-volume history; the Rust-added
    // `volumes_dir` key rides LEGACY_REJECTED_KEYS like every other
    // tuning key the legacy loader cannot honour.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    fs::write(&path, r#"{ "volumes_dir": "volumes" }"#).expect("write config.json");

    let err = CyDriveConfig::load_legacy_json(&path)
        .expect_err("legacy json must reject the volumes_dir key");
    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "expected Parse error, got: {err:?}"
    );
    let message = err.to_string();
    assert!(
        message.contains("volumes_dir"),
        "error must name the rejected key: {message}"
    );
}

// ------------------------------------------------- volume file loading ---

#[test]
fn load_volume_config_parses_a_legal_volume_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("local.toml");
    write_file(&path, LOCAL_VOLUME_TOML);

    let volume = load_volume_config(&path).expect("legal volume file loads");
    assert_eq!(volume.name, "local", "the volume name is the file stem");
    assert_eq!(
        volume.base_dir,
        dir.path(),
        "base dir is the volume file's directory"
    );
    assert_eq!(volume.file_path, path);
    assert_eq!(volume.settings.backend, Backend::Local);
    // Relative paths stay raw at this layer — no absolutisation; the
    // assembly layer (MV1/K21) resolves them against the volume dir.
    assert_eq!(volume.settings.local_root.as_deref(), Some("root"));
    assert_eq!(volume.settings.drive_letter, "V");
}

#[test]
fn load_volume_config_rejects_process_level_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("local.toml");
    write_file(
        &path,
        "backend = \"local\"\nlocal_root = \"root\"\nwebdav_port = 9090\n",
    );

    let err = load_volume_config(&path).expect_err("process-level keys must be rejected");
    let message = err.to_string();
    assert!(
        message.contains("webdav_port"),
        "error must name the offending key: {message}"
    );
    assert!(
        message.contains("process-level"),
        "error must say the key is process-level: {message}"
    );
    assert!(
        message.contains("config.toml"),
        "error must point at config.toml as the key's home: {message}"
    );
}

#[test]
fn load_volume_config_rejects_unknown_keys_like_config_toml() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("local.toml");
    write_file(&path, "backend = \"local\"\nfrobnicate = true\n");

    let err = load_volume_config(&path).expect_err("unknown keys must be rejected");
    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "expected Parse error (same surface as config.toml), got: {err:?}"
    );
    let message = err.to_string();
    assert!(
        message.contains("unknown key"),
        "error wording must match the config.toml surface: {message}"
    );
    assert!(
        message.contains("frobnicate"),
        "error must name the key: {message}"
    );
}

#[test]
fn load_volume_config_rejects_invalid_volume_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A `/` (path separator) cannot occur inside a file name on Windows
    // or Unix, so that shape is unreachable by construction; the name
    // pattern is enforced for everything a stem can actually be.
    let bad_names = [
        "Bad.toml",    // uppercase first letter
        "1abc.toml",   // digit first
        "-abc.toml",   // hyphen first
        "a_bc X.toml", // space inside
        "aBC.toml",    // uppercase inside
        // 33 characters — one past the 32-char cap:
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.toml",
    ];
    for name in bad_names {
        let path = dir.path().join(name);
        write_file(&path, LOCAL_VOLUME_TOML);
        let err = match load_volume_config(&path) {
            Ok(_) => panic!("volume name `{name}` must be rejected"),
            Err(err) => err,
        };
        let message = err.to_string();
        assert!(
            message.contains("volume name"),
            "error must talk about the volume name ({name}): {message}"
        );
    }

    // Empty stem shape: a file literally named `.toml` has the stem
    // `.toml` (a leading dot, not a lowercase letter).
    let dot = dir.path().join(".toml");
    write_file(&dot, LOCAL_VOLUME_TOML);
    let err = load_volume_config(&dot).expect_err("empty/`.toml` stem must be rejected");
    assert!(
        err.to_string().contains("volume name"),
        "error must talk about the volume name: {err}"
    );

    // The legal boundary: exactly 32 characters passes.
    let legal = format!("{}.toml", "a".repeat(32));
    let path = dir.path().join(legal);
    write_file(&path, LOCAL_VOLUME_TOML);
    let volume =
        load_volume_config(&path).expect("a 32-character all-lowercase name is the legal boundary");
    assert_eq!(volume.name.len(), 32);
}

/// Review M3: a TOML syntax error on a line that carries a credential
/// value (the broken-quote `bot_token` line) must not leak the value —
/// the parse error's Display embeds the offending source line, and the
/// message flows into the control-channel ADD reply and the tracing
/// log (the credential-values-never-enter-logs red line). The message
/// keeps the key name and the line/column position (still diagnosable)
/// but the value is masked.
#[test]
fn load_volume_config_parse_error_redacts_credential_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("leak.toml");
    write_file(
        &path,
        "backend = \"telegram\"\nbot_token = \"SECRET-MARKER-123\nchat_id = 111111\n",
    );

    let err = load_volume_config(&path).expect_err("the broken-quote line must fail to parse");
    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "expected Parse error, got: {err:?}"
    );
    let message = err.to_string();
    assert!(
        !message.contains("SECRET-MARKER-123"),
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

/// Review M3, serde construction point: a credential key with a
/// wrong-typed value fails in `try_into`, and the serde error quotes
/// the value twice (the embedded source line and the `invalid type:
/// integer \`...\`` reason) — both occurrences must be masked.
#[test]
fn load_volume_config_type_error_redacts_credential_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("leak.toml");
    write_file(
        &path,
        "backend = \"local\"\nlocal_root = \"root\"\nencryption_password = 9999999999012345\n",
    );

    let err = load_volume_config(&path).expect_err("an integer password must fail to parse");
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

/// Review M3, over-redaction guard: a syntax error on a line with NO
/// credential key keeps its message verbatim — the diagnostic value of
/// the embedded source line (the raw offending text) survives the
/// redaction pass untouched.
#[test]
fn load_volume_config_parse_error_without_credentials_stays_verbatim() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("plain.toml");
    write_file(
        &path,
        "backend = \"local\"\nlocal_root = \"plain-offending-value\n",
    );

    let err = load_volume_config(&path).expect_err("the broken-quote line must fail to parse");
    let message = err.to_string();
    assert!(
        message.contains("local_root = \"plain-offending-value"),
        "a message without credential keys is not redacted: {message}"
    );
}

// ------------------------------------------------------- discovery -------

#[test]
fn discover_volumes_lists_toml_files_in_stable_name_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Create out of order; the result must be sorted by file name.
    for name in ["c.toml", "a.toml", "b.toml"] {
        write_file(&dir.path().join(name), LOCAL_VOLUME_TOML);
    }
    // Non-toml files and directories (even ones ending in .toml) are
    // ignored — discovery is a non-recursive *.toml file listing.
    write_file(&dir.path().join("notes.txt"), "not a volume\n");
    fs::create_dir_all(dir.path().join("sub.toml")).expect("create decoy directory");

    let found = discover_volumes(dir.path()).expect("discovery lists the volumes");
    let names: Vec<String> = found
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["a.toml", "b.toml", "c.toml"]);
}

#[test]
fn discover_volumes_missing_directory_is_an_actionable_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("no-such-volumes");

    let err = discover_volumes(&missing).expect_err("missing directory must fail");
    let message = err.to_string();
    assert!(
        message.contains("volumes"),
        "error must point at the volumes directory: {message}"
    );
    assert!(
        message.contains("volumes_dir") && message.contains("single-volume"),
        "error must offer both actions (create files or drop volumes_dir): {message}"
    );
}

#[test]
fn discover_volumes_empty_directory_is_an_actionable_error() {
    let dir = tempfile::tempdir().expect("tempdir");

    let empty = dir.path().join("empty");
    fs::create_dir_all(&empty).expect("create empty dir");
    let err = discover_volumes(&empty).expect_err("empty directory must fail");
    let message = err.to_string();
    assert!(
        message.contains("volumes") && message.contains("volumes_dir"),
        "error must be actionable: {message}"
    );

    // A directory with only non-toml files counts as empty too.
    let notoml = dir.path().join("notoml");
    fs::create_dir_all(&notoml).expect("create dir");
    write_file(&notoml.join("readme.md"), "hi\n");
    let err = discover_volumes(&notoml).expect_err("no *.toml files must fail");
    assert!(
        err.to_string().contains("volumes"),
        "error must be actionable: {err}"
    );
}

// ---------------------------------------------------- volume set load ----

#[test]
fn load_volumes_loads_all_volumes_in_name_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("b.toml"),
        "backend = \"local\"\nlocal_root = \"b\"\ndrive_letter = \"Y\"\n",
    );
    write_file(&dir.path().join("a.toml"), LOCAL_VOLUME_TOML);

    let volumes = load_volumes(dir.path()).expect("the volume set loads");
    let names: Vec<&str> = volumes.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, vec!["a", "b"], "volumes arrive in file-name order");
    assert_eq!(volumes[0].settings.drive_letter, "V");
    assert_eq!(volumes[1].settings.drive_letter, "Y");
}

#[test]
fn load_volumes_rejects_drive_letter_conflicts() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("a.toml"),
        "backend = \"local\"\nlocal_root = \"a\"\ndrive_letter = \"V\"\n",
    );
    // Same letter, different spelling — normalisation must catch it.
    write_file(
        &dir.path().join("b.toml"),
        "backend = \"local\"\nlocal_root = \"b\"\ndrive_letter = \"v\"\n",
    );

    let err = load_volumes(dir.path()).expect_err("two volumes on one letter must fail");
    let message = err.to_string();
    assert!(
        message.contains("drive_letter"),
        "error must name drive_letter: {message}"
    );
    assert!(
        message.contains('`') || message.contains("conflict"),
        "error must name the conflicting volumes: {message}"
    );
}

// ---------------------------------------------- process-key mixing (K19) --

#[test]
fn process_keys_with_backend_present_is_rejected() {
    // `backend` defaults to telegram, so presence must be judged on the
    // raw toml keys, never on the parsed value.
    let err = ensure_no_volume_keys_in_process(&[
        "volumes_dir".to_string(),
        "webdav_port".to_string(),
        "backend".to_string(),
    ])
    .expect_err("an explicit `backend` in the process config must be rejected");
    let message = err.to_string();
    assert!(
        message.contains("backend"),
        "error must name the offending key: {message}"
    );
    assert!(
        message.contains("volume"),
        "error must tell the user to move it into a volume file: {message}"
    );
}

#[test]
fn process_keys_with_db_path_present_is_rejected() {
    let err = ensure_no_volume_keys_in_process(&["volumes_dir".to_string(), "db_path".to_string()])
        .expect_err("an explicit `db_path` in the process config must be rejected");
    assert!(
        err.to_string().contains("db_path"),
        "error must name the offending key: {}",
        err
    );
}

#[test]
fn process_keys_process_only_set_passes() {
    ensure_no_volume_keys_in_process(&[
        "volumes_dir".to_string(),
        "webdav_host".to_string(),
        "webdav_port".to_string(),
        "web_ui_host".to_string(),
        "web_ui_port".to_string(),
        "enable_web_ui".to_string(),
    ])
    .expect("a pure process-level key set is legal");
}

// -------------------------------------------- volume enabled key (RV0) ---
// Phase 3.6 / RV0 + K49: a per-volume `enabled` boolean (default true) is
// the persistent form of "this volume is disabled". Discovery-side policy:
// `load_volumes` skips disabled volumes with an info note — they join no
// drive-letter conflict check, no assembly, no banner, no `/vol/<name>`.
// The process-level `config.toml` keeps rejecting the key (K19 partition:
// volume-scoped).

#[test]
fn enabled_key_is_a_volume_scoped_known_key() {
    assert!(
        KNOWN_TOML_KEYS.contains(&"enabled"),
        "the strict toml surface must accept the key"
    );
    assert!(
        VOLUME_SCOPED_KEYS.contains(&"enabled"),
        "the enable switch is per-volume state (K19 partition)"
    );
    assert!(
        !PROCESS_SCOPED_KEYS.contains(&"enabled"),
        "a volume-level switch must not sit on the process side"
    );
}

#[test]
fn load_volumes_skips_disabled_volumes() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("a.toml"),
        "backend = \"local\"\nlocal_root = \"a\"\nenabled = false\n",
    );
    write_file(&dir.path().join("b.toml"), LOCAL_VOLUME_TOML);

    let volumes = load_volumes(dir.path()).expect("the enabled volume loads");
    let names: Vec<&str> = volumes.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["b"],
        "the disabled volume must be skipped at discovery (RV0)"
    );
}

#[test]
fn load_volumes_announces_the_disabled_skip_in_the_log() {
    // RV0: "info! 声明，不静默" — a skip is a visible, one-line
    // declaration naming the volume, never a silent drop. Captured
    // through a thread-local dispatcher (the tests/logging.rs pattern;
    // each integration test is its own process, so the interest-cache
    // rebuild cannot race another test's subscriber here).
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let buf: Arc<Mutex<Vec<u8>>> = Arc::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer({
            let buf = Arc::clone(&buf);
            move || Sink(Arc::clone(&buf))
        })
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);
    let _guard = tracing::dispatcher::set_default(&dispatch);
    tracing::callsite::rebuild_interest_cache();

    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("a.toml"),
        "backend = \"local\"\nlocal_root = \"a\"\nenabled = false\n",
    );
    assert!(
        load_volumes(dir.path())
            .expect("discovery still succeeds")
            .is_empty(),
        "the disabled volume is still skipped"
    );

    let captured = String::from_utf8(
        buf.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
    )
    .expect("the log is valid UTF-8");
    assert!(
        captured.contains("INFO") && captured.contains("volume is disabled"),
        "the skip must be announced at info level: {captured:?}"
    );
    assert!(
        captured.contains("a.toml") || captured.contains("\"a\""),
        "the announcement must name the skipped volume: {captured:?}"
    );
}

#[test]
fn load_volumes_disabled_volume_does_not_claim_drive_letter() {
    // Both volumes claim "V"; the disabled one must never reach the
    // conflict check, so the enabled one loads and mounts "V" alone.
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("a.toml"),
        "backend = \"local\"\nlocal_root = \"a\"\ndrive_letter = \"V\"\nenabled = false\n",
    );
    write_file(
        &dir.path().join("b.toml"),
        "backend = \"local\"\nlocal_root = \"b\"\ndrive_letter = \"V\"\n",
    );

    let volumes =
        load_volumes(dir.path()).expect("no conflict: the disabled volume is skipped first");
    let names: Vec<&str> = volumes.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, vec!["b"]);
    assert_eq!(volumes[0].settings.drive_letter, "V");
}

#[test]
fn process_config_with_enabled_key_is_rejected_by_mixing_guard() {
    // K19 partition: the process config.toml must reject `enabled` with
    // the actionable move-it-into-a-volume-file guidance.
    let err = ensure_no_volume_keys_in_process(&["volumes_dir".to_string(), "enabled".to_string()])
        .expect_err("an explicit `enabled` in the process config must be rejected");
    let message = err.to_string();
    assert!(
        message.contains("enabled"),
        "error must name the offending key: {message}"
    );
    assert!(
        message.contains("volume"),
        "error must tell the user to move it into a volume file: {message}"
    );
}

#[test]
fn process_config_file_with_enabled_key_still_rejected_end_to_end() {
    // Characterization + contract: before RV0 the strict schema rejected
    // `enabled` as an unknown key at load time; after RV0 the file parses
    // and the K19 mixing guard rejects it. Either way a process
    // config.toml carrying the key is refused — this pins the observable
    // outcome across both shapes.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    write_file(&path, "volumes_dir = \"volumes\"\nenabled = true\n");

    let rejected = match CyDriveConfig::load_toml_with_keys(&path) {
        Ok((_, keys)) => ensure_no_volume_keys_in_process(&keys)
            .expect_err("the mixing guard must reject `enabled`"),
        Err(err) => err, // pre-RV0 shape: unknown-key rejection at load
    };
    assert!(
        rejected.to_string().contains("enabled"),
        "rejection must name the key: {rejected}"
    );
}

#[test]
fn legacy_json_rejects_the_enabled_key() {
    // Legacy Python config.json is frozen history; a key it cannot honour
    // must fail loudly (the volumes_dir/mount_backend precedent, K19),
    // not sit silently ignored while the user believes a disable took
    // effect.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    fs::write(&path, r#"{ "enabled": false }"#).expect("write config.json");

    let err = CyDriveConfig::load_legacy_json(&path)
        .expect_err("legacy json must reject the enabled key");
    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "expected Parse error, got: {err:?}"
    );
    assert!(
        err.to_string().contains("enabled"),
        "error must name the rejected key: {err}"
    );
}

#[test]
fn volume_file_without_enabled_defaults_to_enabled() {
    // The natural default (RV0): absent key = enabled — a volume file
    // written before RV0, or without the key, behaves exactly as before.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("local.toml");
    write_file(&path, LOCAL_VOLUME_TOML);

    let volume = load_volume_config(&path).expect("legal volume file loads");
    assert!(
        volume.settings.enabled,
        "an absent `enabled` key must default to true"
    );
    assert!(
        CyDriveConfig::default().enabled,
        "the struct default is enabled"
    );

    // An explicit `enabled = true` parses identically.
    let path = dir.path().join("on.toml");
    write_file(
        &path,
        "backend = \"local\"\nlocal_root = \"on\"\nenabled = true\n",
    );
    let volume = load_volume_config(&path).expect("explicit true parses");
    assert!(volume.settings.enabled);
}

#[test]
fn enabled_false_roundtrips_and_default_saves_stay_clean() {
    // save_toml of an enabled (default) config must NOT emit the key —
    // a process config.toml written by setup stays free of volume-scoped
    // keys (K19). The key appears only when explicitly false, so a
    // volume file's disable survives a programmatic round-trip.
    let dir = tempfile::tempdir().expect("tempdir");

    let path = dir.path().join("process.toml");
    CyDriveConfig::default()
        .save_toml(&path)
        .expect("save_toml");
    let text = fs::read_to_string(&path).expect("read saved toml");
    let table: toml::Table = toml::from_str(&text).expect("saved toml parses");
    assert!(
        !table.contains_key("enabled"),
        "a default save must not emit the volume-scoped `enabled` key:\n{text}"
    );

    let disabled = CyDriveConfig {
        enabled: false,
        ..CyDriveConfig::default()
    };
    let path = dir.path().join("volume.toml");
    disabled.save_toml(&path).expect("save_toml");
    let reloaded = CyDriveConfig::load_toml(&path).expect("reload");
    assert!(!reloaded.enabled, "the disable must survive the round-trip");
    assert_eq!(reloaded, disabled, "the whole config round-trips");
}

// ------------------------------------- SHOW serialization (web volume P0) ---

/// The SHOW serializer (web volume management plan §1.2): a volume
/// file's EXPLICIT keys as one compact JSON line — every
/// credential-valued key collapsed to `{"set": bool}` (write-only: the
/// value never leaves the backend), absent credentials reporting
/// `{"set": false}`, and nothing the file leaves unset appearing at
/// all (no defaults — the reply shows the file, not the parsed config).
#[test]
fn volume_show_json_reports_explicit_keys_and_masks_credentials() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("media.toml");
    write_file(
        &file,
        "backend = \"telegram\"\n\
         bot_token = \"111:FAKE-TOKEN-MARKER\"\n\
         chat_id = 4242\n\
         chunk_size_mb = 8\n",
    );

    let line = volume_show_json(&file).expect("serialize the volume file");
    assert!(!line.contains('\n'), "one compact line: {line}");
    let value: serde_json::Value = serde_json::from_str(&line).expect("valid json");
    assert_eq!(value["name"], "media", "the validated file stem");
    assert_eq!(value["backend"], "telegram");
    assert_eq!(value["chat_id"], 4242);
    assert_eq!(value["chunk_size_mb"], 8);
    // An explicit credential collapses to the set marker; an absent one
    // reports unset; the VALUE must not ride the reply in any form.
    assert_eq!(value["bot_token"], serde_json::json!({"set": true}));
    assert_eq!(
        value["encryption_password"],
        serde_json::json!({"set": false})
    );
    assert!(
        !line.contains("FAKE-TOKEN-MARKER"),
        "credential leaked: {line}"
    );
    // An ordinary key the file leaves unset stays absent — no defaults.
    assert!(
        value.get("drive_letter").is_none(),
        "no defaulted keys: {value}"
    );
}

/// SHOW rejects what `load_volume_config` rejects (the same validation
/// funnel — name rules, unknown keys, process-level keys): the
/// serializer never answers half-validated configuration.
#[test]
fn volume_show_json_reuses_the_volume_file_validation() {
    let dir = tempfile::tempdir().expect("tempdir");

    let unknown = dir.path().join("bad.toml");
    write_file(&unknown, "backend = \"local\"\nno_such_key = 1\n");
    assert!(
        volume_show_json(&unknown)
            .expect_err("unknown key must be refused")
            .to_string()
            .contains("no_such_key"),
        "the refusal names the key"
    );

    let process = dir.path().join("web.toml");
    write_file(&process, "backend = \"local\"\nweb_ui_port = 9\n");
    assert!(
        volume_show_json(&process).is_err(),
        "a process-level key must be refused"
    );
}
