//! RED-phase tests for Phase 2.5 / MV0: the CLI config-discovery wiring
//! for multi-volume mode (K19/K28).
//!
//! Contract under test: `docs/plans/2026-09-08-phase2-5-multivolume.md`
//! §3-MV0 — `discover_config_with_volumes_and_store` returns the process
//! config plus the volume manifest in multi-volume mode (validation
//! branches surfaced as actionable errors), keeps the single-volume
//! chain byte-identical otherwise, and ignores `CYDRIVE_*` overrides in
//! volume mode with a tracing note (K28). (MV1 replaced the MV0
//! `ensure_single_volume` gate with the real volume assembly — that
//! gate's tests were retired with it; the assembly itself is covered by
//! `multivolume_e2e.rs`.)

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use cloudkit_cli::{
    bootstrap_first_run_cwd, discover_config_with_store, discover_config_with_volumes_and_store,
    discover_first_run_config, DiscoveredConfig,
};
use cloudkit_core::config::Backend;
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

// ------------------- web first-run bootstrap (FR1: red 1/2/5) ---

/// The pinned first-run template (web first-run plan FR1 / D2): the pin
/// is byte-exact, so any template drift fails this test and the author
/// consciously re-pins. Header declares the first run; ports 8485/8486
/// are the program defaults (负责人 2026-09-23 裁决); `auto_mount_drive`
/// is process-scoped so the K19 mixing guard accepts it.
const FIRST_RUN_TEMPLATE_PIN: &str = "\
# cydrive process config — generated on first run: no config.toml or
# config.json was found in this directory, so this minimal configuration
# was written and the instance started so a first volume can be added
# through the web dashboard. Process-level keys only — each volume's own
# settings live in volumes/<name>.toml.
volumes_dir = \"volumes\"

webdav_host = \"127.0.0.1\"
webdav_port = 8485
enable_web_ui = true
web_ui_host = \"127.0.0.1\"
web_ui_port = 8486
auto_mount_drive = true
";

/// RED 1 (happy arm): in an empty cwd the bootstrap generates the
/// minimal init config (every key, the 8485/8486 ports and the
/// "first run" header) and creates the `volumes/` directory.
#[test]
fn bootstrap_first_run_generates_minimal_config_in_an_empty_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = chdir(dir.path());

    let generated = bootstrap_first_run_cwd().expect("first-run bootstrap");
    assert!(generated, "an empty cwd must generate the config");

    let text = fs::read_to_string("config.toml").expect("config.toml exists");
    for needle in [
        "volumes_dir = \"volumes\"",
        "webdav_host = \"127.0.0.1\"",
        "webdav_port = 8485",
        "enable_web_ui = true",
        "web_ui_host = \"127.0.0.1\"",
        "web_ui_port = 8486",
        "auto_mount_drive = true",
        "first run",
    ] {
        assert!(text.contains(needle), "template carries `{needle}`: {text}");
    }
    assert!(
        Path::new("volumes").is_dir(),
        "the volumes directory is created"
    );
}

/// RED 1 (guard arm): an existing `config.toml` (or a legacy
/// `config.json`) disables the bootstrap — the probe runs first, so an
/// existing configuration is byte-identical after the call.
#[test]
fn bootstrap_first_run_never_touches_an_existing_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let existing = "bot_token = \"123456:ABC-DEF\"\nchat_id = 123456789\n";
    write_file(&dir.path().join("config.toml"), existing);
    let _guard = chdir(dir.path());

    let generated = bootstrap_first_run_cwd().expect("bootstrap probe");
    assert!(
        !generated,
        "an existing config.toml must disable the bootstrap"
    );
    assert_eq!(
        fs::read_to_string("config.toml").expect("read back"),
        existing,
        "the existing file stays byte-identical"
    );
    assert!(
        !Path::new("volumes").exists(),
        "no volumes directory appears over an existing config"
    );
}

/// RED 1 (guard arm, legacy shape): a legacy Python `config.json` alone
/// also disables the bootstrap — nothing is generated over it.
#[test]
fn bootstrap_first_run_respects_a_legacy_json_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.json"),
        r#"{ "bot_token": "1:x", "chat_id": 1 }"#,
    );
    let _guard = chdir(dir.path());

    let generated = bootstrap_first_run_cwd().expect("bootstrap probe");
    assert!(!generated, "a legacy config.json disables the bootstrap");
    assert!(
        !Path::new("config.toml").exists(),
        "no config.toml is generated over a legacy config"
    );
    assert!(!Path::new("volumes").exists());
}

/// RED 2: right after the bootstrap, the first-run discovery loads the
/// generated config.toml (mixing guard + validate) and returns
/// `Multi` with an EMPTY volume manifest — gate A (the empty
/// volumes-directory error) is bypassed without touching core.
#[test]
fn discover_first_run_returns_multi_with_empty_volumes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = chdir(dir.path());
    bootstrap_first_run_cwd().expect("bootstrap");

    let discovered = discover_first_run_config().expect("first-run discovery");
    match discovered {
        DiscoveredConfig::Multi { process, volumes } => {
            assert!(volumes.is_empty(), "no volumes exist yet: {volumes:?}");
            assert_eq!(process.volumes_dir.as_deref(), Some("volumes"));
            assert_eq!(process.webdav_port, 8485);
            assert_eq!(process.web_ui_port, 8486);
            assert!(process.enable_web_ui);
            assert!(process.auto_mount_drive);
        }
        DiscoveredConfig::Single(_) => {
            panic!("the first-run config carries volumes_dir — Multi it is")
        }
    }
}

/// RED 5: the generated file is the pinned template, byte for byte.
#[test]
fn first_run_template_pin_keys_ports_and_header() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = chdir(dir.path());
    bootstrap_first_run_cwd().expect("bootstrap");

    let text = fs::read_to_string("config.toml").expect("config.toml");
    assert_eq!(
        text, FIRST_RUN_TEMPLATE_PIN,
        "the first-run template is pinned byte for byte"
    );
}
