//! RED-phase tests for Phase 2.5 / MV0: the CLI config-discovery wiring
//! for multi-volume mode (K19/K28).
//!
//! Contract under test: `docs/plans/2026-09-08-phase2-5-multivolume.md`
//! §3-MV0 — `discover_config_with_volumes_and_store` returns the process
//! config plus the volume manifest in multi-volume mode (validation
//! branches surfaced as actionable errors), keeps the single-volume
//! chain byte-identical otherwise, ignores `CYDRIVE_*` overrides in
//! volume mode with a tracing note (K28), and `ensure_single_volume`
//! refuses multi-volume configs with an explicit "MV1" error instead of
//! panicking in `run`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use cloudkit_cli::{
    discover_config_with_store, discover_config_with_volumes_and_store, ensure_single_volume,
    DiscoveredConfig,
};
use cloudkit_core::config::{Backend, CyDriveConfig};
use cloudkit_core::credentials::InMemoryStore;

// ------------------------------------------------------------- helpers ---

/// `CYDRIVE_*` keys the K28 test toggles; cleared on guard drop so a
/// developer shell cannot skew the other tests.
const ENV_KEYS: &[&str] = &[
    "CYDRIVE_BOT_TOKEN",
    "CYDRIVE_CHAT_ID",
    "CYDRIVE_WEBDAV_PORT",
    "CYDRIVE_WEB_UI_PORT",
    "CYDRIVE_DRIVE_LETTER",
    "CYDRIVE_CHUNK_SIZE_MB",
    "CYDRIVE_ENABLE_ENCRYPTION",
];

/// Serialises every test that changes the process-wide working directory.
static CWD_MUTEX: Mutex<()> = Mutex::new(());

/// Holds [`CWD_MUTEX`] and restores the previous working directory (and a
/// clean `CYDRIVE_*` environment) on drop — including on panic.
struct CwdGuard {
    _lock: MutexGuard<'static, ()>,
    prev: PathBuf,
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        for key in ENV_KEYS {
            std::env::remove_var(key);
        }
        std::env::set_current_dir(&self.prev).expect("restore previous cwd");
    }
}

/// Locks [`CWD_MUTEX`], clears `CYDRIVE_*` overrides and moves the process
/// cwd into `dir` for the duration of the guard.
fn chdir(dir: &Path) -> CwdGuard {
    let lock = CWD_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for key in ENV_KEYS {
        std::env::remove_var(key);
    }
    let prev = std::env::current_dir().expect("current dir");
    std::env::set_current_dir(dir).expect("chdir into temp dir");
    CwdGuard { _lock: lock, prev }
}

/// Writes `text` to `path`, creating missing parent directories.
fn write_file(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(path, text).expect("write file");
}

/// A multi-volume process config.toml body (process-scoped keys only).
const MULTI_PROCESS_TOML: &str =
    "volumes_dir = \"volumes\"\nwebdav_host = \"127.0.0.1\"\nwebdav_port = 8080\nenable_web_ui = false\n";

/// A minimal legal local volume file.
const LOCAL_VOLUME_TOML: &str =
    "backend = \"local\"\nlocal_root = \"root\"\ndrive_letter = \"V\"\n";

/// A temp cwd already holding a multi-volume process config and one
/// local volume file.
fn multi_volume_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(&dir.path().join("config.toml"), MULTI_PROCESS_TOML);
    write_file(
        &dir.path().join("volumes").join("local.toml"),
        LOCAL_VOLUME_TOML,
    );
    dir
}

// ------------------------------------------------------------- tests -----

#[test]
fn discover_multi_returns_process_config_and_volume_manifest() {
    let dir = multi_volume_dir();
    let _guard = chdir(dir.path());

    let discovered =
        discover_config_with_volumes_and_store(&InMemoryStore::new()).expect("multi discovery");
    match discovered {
        DiscoveredConfig::Multi { process, volumes } => {
            assert_eq!(process.volumes_dir.as_deref(), Some("volumes"));
            assert_eq!(process.webdav_port, 8080);
            assert_eq!(volumes.len(), 1, "one volume file discovered");
            assert_eq!(volumes[0].name, "local");
            assert_eq!(volumes[0].settings.backend, Backend::Local);
            assert_eq!(volumes[0].settings.drive_letter, "V");
        }
        DiscoveredConfig::Single(_) => panic!("volumes_dir set means multi-volume discovery"),
    }
}

#[test]
fn discover_single_keeps_the_existing_chain() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "bot_token = \"123456:ABC-DEF\"\nchat_id = 123456789\n",
    );
    let _guard = chdir(dir.path());

    let discovered =
        discover_config_with_volumes_and_store(&InMemoryStore::new()).expect("single discovery");
    let cfg = match discovered {
        DiscoveredConfig::Single(cfg) => cfg,
        DiscoveredConfig::Multi { .. } => panic!("no volumes_dir means single-volume mode"),
    };
    let legacy_chain =
        discover_config_with_store(&InMemoryStore::new()).expect("existing discovery");
    assert_eq!(
        cfg, legacy_chain,
        "the single-volume chain must be unchanged"
    );
}

#[test]
fn discover_multi_with_missing_volumes_dir_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(&dir.path().join("config.toml"), MULTI_PROCESS_TOML);
    // No `volumes/` directory on purpose.
    let _guard = chdir(dir.path());

    let err = discover_config_with_volumes_and_store(&InMemoryStore::new())
        .expect_err("missing volumes directory must fail");
    let message = format!("{err:#}");
    assert!(
        message.contains("volumes"),
        "error must name the volumes directory: {message}"
    );
}

#[test]
fn discover_multi_with_volume_keys_in_process_config_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Mixing: volumes_dir plus an explicit (volume-scoped) backend key.
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\nwebdav_port = 8080\nbackend = \"local\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("local.toml"),
        LOCAL_VOLUME_TOML,
    );
    let _guard = chdir(dir.path());

    let err = discover_config_with_volumes_and_store(&InMemoryStore::new())
        .expect_err("mixing process and volume keys must fail");
    let message = format!("{err:#}");
    assert!(
        message.contains("backend"),
        "error must name the offending key: {message}"
    );
    assert!(
        message.contains("volume"),
        "error must direct the key into a volume file: {message}"
    );
}

#[test]
fn discover_multi_ignores_cydrive_env_overrides() {
    // K28: in multi-volume mode CYDRIVE_* overrides are ignored (they
    // would cross-wire volume settings across volumes); the process
    // config keeps its file values and a tracing note is emitted.
    let dir = multi_volume_dir();
    let _guard = chdir(dir.path());
    std::env::set_var("CYDRIVE_WEBDAV_PORT", "9999");

    let discovered =
        discover_config_with_volumes_and_store(&InMemoryStore::new()).expect("multi discovery");
    match discovered {
        DiscoveredConfig::Multi { process, .. } => assert_eq!(
            process.webdav_port, 8080,
            "CYDRIVE_WEBDAV_PORT must NOT override the process config in volume mode (K28)"
        ),
        DiscoveredConfig::Single(_) => panic!("volumes_dir set means multi-volume discovery"),
    }
}

#[test]
fn ensure_single_volume_passes_single_mode_through() {
    let cfg = CyDriveConfig {
        bot_token: "1:a".to_string(),
        chat_id: 7,
        ..CyDriveConfig::default()
    };
    let out = ensure_single_volume(DiscoveredConfig::Single(cfg.clone()))
        .expect("single-volume configs run as before");
    assert_eq!(out, cfg);
}

#[test]
fn ensure_single_volume_rejects_multi_mode_with_mv1_error() {
    let dir = multi_volume_dir();
    let _guard = chdir(dir.path());

    let discovered =
        discover_config_with_volumes_and_store(&InMemoryStore::new()).expect("multi discovery");
    let err = ensure_single_volume(discovered)
        .expect_err("multi-volume configs must fail loudly in this build, not panic");
    let message = format!("{err:#}");
    assert!(
        message.contains("MV1"),
        "error must say multi-volume assembly arrives in MV1: {message}"
    );
    assert!(
        message.contains("volumes_dir"),
        "error must point at the config key to remove: {message}"
    );
}

#[test]
fn discover_multi_surfaces_drive_letter_conflicts() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(&dir.path().join("config.toml"), MULTI_PROCESS_TOML);
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        "backend = \"local\"\nlocal_root = \"a\"\ndrive_letter = \"V\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("b.toml"),
        "backend = \"local\"\nlocal_root = \"b\"\ndrive_letter = \"V\"\n",
    );
    let _guard = chdir(dir.path());

    let err = discover_config_with_volumes_and_store(&InMemoryStore::new())
        .expect_err("two volumes on one letter must fail discovery");
    let message = format!("{err:#}");
    assert!(
        message.contains("drive_letter"),
        "error must name drive_letter: {message}"
    );
}
