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
    discover_volumes, ensure_no_volume_keys_in_process, load_volume_config, load_volumes, Backend,
    ConfigError, CyDriveConfig, KNOWN_TOML_KEYS, PROCESS_SCOPED_KEYS, VOLUME_SCOPED_KEYS,
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
    // switch and the (new) volumes directory — everything a single
    // process shares across volumes.
    assert_eq!(
        PROCESS_SCOPED_KEYS.to_vec(),
        vec![
            "volumes_dir",
            "webdav_host",
            "webdav_port",
            "web_ui_host",
            "web_ui_port",
            "enable_web_ui",
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
