//! RED-phase tests for the `migrate` subcommand and store-aware config
//! discovery (M5-1: credential vault + legacy import).
//!
//! Contract under test: `cydrive migrate` moves a legacy Python
//! `config.json` into a scrubbed canonical `config.toml` with the two
//! secrets relocated to the OS credential store (injected here as an
//! [`InMemoryStore`]), adopts Python-default data artifacts found in the
//! working directory and reports what it did; `discover_config_with_store`
//! backfills store secrets into a config whose file left them empty, with
//! `CYDRIVE_*` env still on top (see `docs/rust-rewrite-design.md`,
//! «平台层与 CLI» migrate 行).

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use cloudkit_cli::{discover_config_with_store, run_migrate};
use cloudkit_core::config::CyDriveConfig;
use cloudkit_core::credentials::{CredentialStore, InMemoryStore, BOT_TOKEN, ENCRYPTION_PASSWORD};
use cloudkit_core::database::{FileUpsert, MetaDatabase};

// ------------------------------------------------------------- helpers ---

/// Every `CYDRIVE_*` override key `with_env_overrides` recognises; the
/// discovery test clears them so a developer shell cannot skew results.
const ENV_KEYS: &[&str] = &[
    "CYDRIVE_BOT_TOKEN",
    "CYDRIVE_CHAT_ID",
    "CYDRIVE_WEBDAV_PORT",
    "CYDRIVE_WEB_UI_PORT",
    "CYDRIVE_DRIVE_LETTER",
    "CYDRIVE_CHUNK_SIZE_MB",
    "CYDRIVE_ENABLE_ENCRYPTION",
];

/// Serialises every test that changes the process-wide working directory
/// (tests in one binary share one process; `set_current_dir` races).
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
/// cwd into `dir` for the duration of the guard. Declare the guard *after*
/// the owning `TempDir` so the cwd is restored before the directory is
/// deleted (Windows refuses to remove the cwd).
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

/// A plain uploaded file row (same shape as the core database tests use).
fn file_entry(rel: &str, size: i64) -> FileUpsert {
    let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
    let parent_dir = match rel.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => rel[..i].to_string(),
    };
    FileUpsert {
        rel_path: rel.to_string(),
        name,
        parent_dir,
        size,
        mtime: 1_700_000_000.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: None,
        is_uploaded: true,
        is_cached: false,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: None,
    }
}

// -------------------------------------------------------------- migrate ---

#[test]
fn migrate_legacy_json_moves_secrets_to_store_and_scrubs_toml() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = chdir(dir.path());
    fs::write(
        "config.json",
        r#"{
            "bot_token": "123456:ABC-DEF",
            "chat_id": 123456789,
            "enable_encryption": true,
            "encryption_password": "legacy-secret"
        }"#,
    )
    .expect("write legacy config.json");

    let store = InMemoryStore::new();
    let report = run_migrate(&store).expect("migration succeeds");

    // Secrets moved into the store.
    assert_eq!(
        store.get(BOT_TOKEN).expect("get bot_token"),
        Some("123456:ABC-DEF".to_string())
    );
    assert_eq!(
        store.get(ENCRYPTION_PASSWORD).expect("get password"),
        Some("legacy-secret".to_string())
    );

    // config.toml written, scrubbed and loadable.
    let text = fs::read_to_string("config.toml").expect("config.toml written");
    let cfg =
        CyDriveConfig::load_toml(Path::new("config.toml")).expect("scrubbed config.toml loads");
    assert_eq!(cfg.bot_token, "", "token scrubbed to an empty string");
    assert_eq!(cfg.encryption_password, None, "password scrubbed to None");
    assert!(cfg.enable_encryption, "encryption flag survives as a bool");
    assert_eq!(cfg.chat_id, 123456789);

    // Raw text: no password key, no secret material, but a pointer at the
    // credential store for humans.
    assert!(
        !text.contains("encryption_password"),
        "no encryption_password key in: {text}"
    );
    assert!(!text.contains("ABC-DEF"), "no bot token secret in: {text}");
    assert!(
        !text.contains("legacy-secret"),
        "no password secret in: {text}"
    );
    assert!(
        text.contains("credential"),
        "comment explains where the secrets live: {text}"
    );

    // The legacy file is kept (irreversible deletion is the user's call).
    assert!(Path::new("config.json").exists(), "config.json kept");

    // Report carries the two key facts: which credentials moved and where
    // the config landed.
    assert!(report.contains("config.toml"), "report: {report}");
    assert!(
        report.contains(BOT_TOKEN) && report.contains(ENCRYPTION_PASSWORD),
        "report names both migrated credentials: {report}"
    );
}

#[test]
fn migrate_adopts_existing_db_and_reports_count() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = chdir(dir.path());
    // db_path deliberately points somewhere else: adopting the
    // Python-default artifact in the cwd must win over the loaded value.
    fs::write(
        "config.json",
        r#"{ "bot_token": "1:a", "chat_id": 2, "db_path": "./fresh.db" }"#,
    )
    .expect("write legacy config.json");

    // A real Python-default-named metadata DB with two rows.
    let db = MetaDatabase::open(Path::new("cydrive_meta.db")).expect("open db");
    db.upsert_file(&file_entry("/a.txt", 100))
        .expect("insert a");
    db.upsert_file(&file_entry("/b.txt", 250))
        .expect("insert b");
    drop(db);

    let store = InMemoryStore::new();
    let report = run_migrate(&store).expect("migration succeeds");

    assert!(
        report.contains("2 files"),
        "report carries the adopted DB's row count: {report}"
    );

    let cfg = CyDriveConfig::load_toml(Path::new("config.toml")).expect("load config.toml");
    assert_eq!(cfg.db_path, "./cydrive_meta.db", "adopted the existing DB");
}

#[test]
fn migrate_missing_config_is_actionable_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = chdir(dir.path());

    let err = run_migrate(&InMemoryStore::new()).expect_err("no source config");
    let message = format!("{err:#}");
    assert!(
        message.contains("config.json") && message.contains("config.toml"),
        "error guides towards both possible sources: {message}"
    );
}

#[test]
fn migrate_idempotent_second_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = chdir(dir.path());
    fs::write("config.json", r#"{ "bot_token": "9:ZZ", "chat_id": 99 }"#)
        .expect("write legacy config.json");

    let store = InMemoryStore::new();
    let _first = run_migrate(&store).expect("first migration");
    let second = run_migrate(&store).expect("second migration must not error");

    // Store values survived the overwrite; the toml stays scrubbed.
    assert_eq!(
        store.get(BOT_TOKEN).expect("get bot_token"),
        Some("9:ZZ".to_string())
    );
    let cfg = CyDriveConfig::load_toml(Path::new("config.toml")).expect("second config.toml loads");
    assert_eq!(cfg.bot_token, "");
    assert!(
        second.contains("config.toml"),
        "second report still reports: {second}"
    );
}

// ------------------------------------------------------------ discover ---

#[test]
fn discover_with_store_backfills_configured() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = chdir(dir.path());
    // Scrubbed config: chat_id but no token — exactly what migrate leaves
    // behind.
    fs::write("config.toml", "chat_id = 42\n").expect("write config.toml");

    let store = InMemoryStore::new().with(BOT_TOKEN, "777:KEY");

    let cfg = discover_config_with_store(&store).expect("discovery succeeds");
    assert_eq!(cfg.bot_token, "777:KEY", "empty file token backfilled");
    assert!(cfg.is_configured(), "backfilled discovery is configured");

    // Env still outranks the store.
    std::env::set_var("CYDRIVE_BOT_TOKEN", "999:ENV");
    let cfg = discover_config_with_store(&store).expect("discovery with env");
    assert_eq!(
        cfg.bot_token, "999:ENV",
        "CYDRIVE_BOT_TOKEN beats the store"
    );
}
