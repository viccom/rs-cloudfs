//! RED-phase tests for the M5-2 operations subcommands: `stats` report
//! formatting, the offline `doctor` check suite and the `setup` wizard's
//! pure core (see `docs/rust-rewrite-design.md`, «平台层与 CLI» —
//! doctor = "端口占用/注册表/WebClient/Telegram 连通性一键诊断").
//!
//! Contract under test:
//!
//! * [`cydrive_cli::format_stats_report`] — a comfy-table report with the
//!   Python `/stats` rows (Total Files/Folders/Cloud Storage/Synced/Pending)
//!   plus Drive/URL, sizes human-readable via the bot's `size_gb >= 1`
//!   branch semantics;
//! * `run_doctor` — offline checks (config/credentials/db/cache/ports)
//!   driven by an injected [`DoctorContext`], and `render_report`'s
//!   per-line `[OK]/[WARN]/[FAIL] name — detail` + summary counts;
//! * `evaluate_webclient_params` — the pure three-state registry verdict;
//! * the setup wizard's pure core — token validation mirroring the Python
//!   wizard texts, `apply_wizard` field filling + letter normalisation,
//!   `persist_setup` scrubbing + store roundtrip through
//!   [`cydrive_cli::discover_config_with_store`].
//!
//! The interactive dialoguer layer and `platform_checks` (real registry /
//! `sc query` / live Telegram) are compile-verified only — a doctor run
//! against the real OS surfaces is a manual, online concern by design.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use cloudkit_core::config::CyDriveConfig;
use cloudkit_core::credentials::{CredentialError, CredentialStore, InMemoryStore, BOT_TOKEN};
use cloudkit_core::database::{FileUpsert, MetaDatabase, Stats};
use cydrive_cli::doctor::{
    evaluate_webclient_params, render_report, run_doctor, CheckResult, CheckStatus, DoctorContext,
};
use cydrive_cli::setup::{apply_wizard, persist_setup, validate_token, WizardAnswers};
use cydrive_cli::{discover_config_with_store, format_stats_report};

// ------------------------------------------------------------- helpers ---

/// Every `CYDRIVE_*` override key `with_env_overrides` recognises; the
/// roundtrip test clears them so a developer shell cannot skew results
/// (same list as `tests/migrate.rs`).
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

/// A stats snapshot with every field pinned to a distinct known value.
fn known_stats() -> Stats {
    Stats {
        total_files: 12,
        total_bytes: 5 * 1024 * 1024 * 1024, // 5 GiB → the GB branch
        total_dirs: 3,
        uploaded_files: 10,
        pending_uploads: 2,
    }
}

/// Finds the check named `name` in a doctor run's results.
fn find_result<'a>(results: &'a [CheckResult], name: &str) -> &'a CheckResult {
    results
        .iter()
        .find(|result| result.name == name)
        .unwrap_or_else(|| panic!("no check named {name:?} in {results:?}"))
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

// ---------------------------------------------------------------- stats ---

#[test]
fn stats_report_contains_all_rows() {
    let report = format_stats_report(&known_stats(), "Y:", "http://127.0.0.1:8080");
    // Every row label, its known value, and the two extras.
    for expected in [
        "Total Files",
        "12",
        "Total Folders",
        "3",
        "Total Cloud Storage",
        "5.00 GB",
        "Synced Files",
        "10",
        "Pending Uploads",
        "2",
        "Y:",
        "http://127.0.0.1:8080",
    ] {
        assert!(
            report.contains(expected),
            "missing {expected:?} in:\n{report}"
        );
    }
}

#[test]
fn stats_formats_large_sizes_gb() {
    // Bot semantics (`bot.rs`): GB branch when size_gb >= 1, two decimals.
    let large = Stats {
        total_bytes: 3 * 1024 * 1024 * 1024,
        ..known_stats()
    };
    assert!(
        format_stats_report(&large, "Y:", "u").contains("3.00 GB"),
        "3 GiB formats as GB"
    );
    let small = Stats {
        total_bytes: 1024 * 1024, // 1 MiB → the MB branch
        ..known_stats()
    };
    let report = format_stats_report(&small, "Y:", "u");
    assert!(report.contains("1.00 MB"), "1 MiB formats as MB: {report}");
    assert!(
        !report.contains("GB"),
        "no GB token in the MB branch: {report}"
    );
}

// --------------------------------------------------------------- doctor ---

#[test]
fn doctor_config_missing_fails() {
    let ctx = DoctorContext {
        config_present: false,
        db_path: None,
        cache_path: None,
        webdav_port: 8080,
        web_ui_port: 8088,
        bot_token: String::new(),
        credential_store: Arc::new(InMemoryStore::new()),
    };
    let results = run_doctor(&ctx);
    let config = find_result(&results, "config");
    assert_eq!(config.status, CheckStatus::Fail);
    assert!(
        config.detail.contains("setup"),
        "failure guidance names the setup command: {}",
        config.detail
    );
}

#[test]
fn doctor_db_opens_reports_count() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("meta.db");
    let db = MetaDatabase::open(&db_path).expect("open db");
    db.upsert_file(&file_entry("/a.txt", 100))
        .expect("insert a");
    db.upsert_file(&file_entry("/b.txt", 250))
        .expect("insert b");
    drop(db);

    let ctx = DoctorContext {
        config_present: true,
        db_path: Some(db_path),
        cache_path: None,
        webdav_port: 8080,
        web_ui_port: 8088,
        bot_token: String::new(),
        credential_store: Arc::new(InMemoryStore::new()),
    };
    let results = run_doctor(&ctx);
    let db = find_result(&results, "database");
    assert_eq!(db.status, CheckStatus::Ok);
    assert!(
        db.detail.contains("2 files"),
        "detail carries the row count: {}",
        db.detail
    );
}

#[test]
fn doctor_db_unreadable_fails() {
    // The path exists (it is a directory), so doctor attempts the open —
    // which cannot succeed against a directory.
    let dir = tempfile::tempdir().expect("tempdir");
    let ctx = DoctorContext {
        config_present: true,
        db_path: Some(dir.path().to_path_buf()),
        cache_path: None,
        webdav_port: 8080,
        web_ui_port: 8088,
        bot_token: String::new(),
        credential_store: Arc::new(InMemoryStore::new()),
    };
    let results = run_doctor(&ctx);
    assert_eq!(find_result(&results, "database").status, CheckStatus::Fail);
}

#[test]
fn doctor_cache_writable_ok() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ctx = DoctorContext {
        config_present: true,
        db_path: None,
        cache_path: Some(dir.path().join("Telegram_Cache")),
        webdav_port: 8080,
        web_ui_port: 8088,
        bot_token: String::new(),
        credential_store: Arc::new(InMemoryStore::new()),
    };
    let results = run_doctor(&ctx);
    let cache = find_result(&results, "cache directory");
    assert_eq!(cache.status, CheckStatus::Ok, "detail: {}", cache.detail);
    // The probe cleaned up after itself; create_dir_all left the root in
    // place, which is the runtime's own behaviour on first hydrate.
    assert!(dir.path().join("Telegram_Cache").is_dir());
    let probe_leftovers: Vec<_> = std::fs::read_dir(dir.path().join("Telegram_Cache"))
        .expect("read cache dir")
        .collect();
    assert!(
        probe_leftovers.is_empty(),
        "probe file removed after the check"
    );
}

#[test]
fn doctor_port_checks_bind_semantics() {
    // Hold one listener open → that port reports Warn; a second ephemeral
    // port released before the run reports Ok.
    let held = TcpListener::bind(("127.0.0.1", 0)).expect("bind held port");
    let held_port = held.local_addr().expect("local addr").port();
    let free_port = {
        let probe = TcpListener::bind(("127.0.0.1", 0)).expect("bind free port probe");
        probe.local_addr().expect("local addr").port()
    }; // dropped: the port is free again

    let ctx = DoctorContext {
        config_present: true,
        db_path: None,
        cache_path: None,
        webdav_port: held_port,
        web_ui_port: free_port,
        bot_token: String::new(),
        credential_store: Arc::new(InMemoryStore::new()),
    };
    let results = run_doctor(&ctx);
    let webdav = find_result(&results, "webdav port");
    assert_eq!(webdav.status, CheckStatus::Warn, "occupied → Warn");
    assert!(
        webdav.detail.contains("in use"),
        "occupied detail: {}",
        webdav.detail
    );
    let web_ui = find_result(&results, "web ui port");
    assert_eq!(web_ui.status, CheckStatus::Ok, "free → Ok");
    assert!(
        web_ui.detail.contains("available"),
        "free detail: {}",
        web_ui.detail
    );
}

#[test]
fn doctor_render_summary_counts() {
    let results = vec![
        CheckResult {
            name: "a".to_string(),
            status: CheckStatus::Ok,
            detail: "d1".to_string(),
        },
        CheckResult {
            name: "b".to_string(),
            status: CheckStatus::Ok,
            detail: "d2".to_string(),
        },
        CheckResult {
            name: "c".to_string(),
            status: CheckStatus::Warn,
            detail: "d3".to_string(),
        },
        CheckResult {
            name: "d".to_string(),
            status: CheckStatus::Fail,
            detail: "d4".to_string(),
        },
    ];
    let report = render_report(&results);
    assert!(report.contains("[OK] a — d1"), "per-line format: {report}");
    assert!(
        report.contains("[WARN] c — d3"),
        "per-line format: {report}"
    );
    assert!(
        report.contains("[FAIL] d — d4"),
        "per-line format: {report}"
    );
    assert!(
        report.contains("2 ok, 1 warn, 1 fail"),
        "summary counts: {report}"
    );
}

#[test]
fn doctor_evaluate_webclient_params_three_states() {
    // Missing registry values → Warn pointing at fix-reg.
    let missing = evaluate_webclient_params(None);
    assert_eq!(missing.status, CheckStatus::Warn);
    assert!(
        missing.detail.contains("fix-reg"),
        "guidance names fix-reg: {}",
        missing.detail
    );
    // Present but off-contract → Warn with the current values.
    let off = evaluate_webclient_params(Some((1_073_741_824, 1)));
    assert_eq!(off.status, CheckStatus::Warn);
    assert!(
        off.detail.contains("1073741824") && off.detail.contains('1'),
        "detail carries the current values: {}",
        off.detail
    );
    // The contract pair → Ok.
    let contract = evaluate_webclient_params(Some((0xFFFF_FFFF, 2)));
    assert_eq!(contract.status, CheckStatus::Ok);
}

// -------------------------------------------------- doctor: credentials ---

/// A [`CredentialStore`] whose backend is unreachable: every operation
/// answers [`CredentialError::Unavailable`], the shape a headless
/// session (systemd service with no Secret Service / keyring) produces.
struct UnreachableKeyring;

impl CredentialStore for UnreachableKeyring {
    fn get(&self, _key: &str) -> Result<Option<String>, CredentialError> {
        Err(CredentialError::Unavailable(
            "no secret service in this session".to_string(),
        ))
    }

    fn set(&self, _key: &str, _value: &str) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable(
            "no secret service in this session".to_string(),
        ))
    }

    fn delete(&self, _key: &str) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable(
            "no secret service in this session".to_string(),
        ))
    }
}

#[test]
fn doctor_warns_headless_keyring_with_empty_token() {
    fn ctx(token: &str, store: Arc<dyn CredentialStore>) -> DoctorContext {
        DoctorContext {
            config_present: true,
            db_path: None,
            cache_path: None,
            webdav_port: 8080,
            web_ui_port: 8088,
            bot_token: token.to_string(),
            credential_store: store,
        }
    }

    // Empty token + unreachable keyring → Warn naming the headless
    // remedy (the env var).
    let results = run_doctor(&ctx("", Arc::new(UnreachableKeyring)));
    let check = find_result(&results, "credentials");
    assert_eq!(check.status, CheckStatus::Warn, "detail: {}", check.detail);
    assert!(
        check.detail.contains("CYDRIVE_BOT_TOKEN"),
        "guidance names the env var: {}",
        check.detail
    );

    // Token resolved from file/env → no warning even with the keyring
    // down: the credential exists for headless boot.
    let results = run_doctor(&ctx("123456789:ABCdef", Arc::new(UnreachableKeyring)));
    assert_eq!(
        find_result(&results, "credentials").status,
        CheckStatus::Ok,
        "token present outranks the keyring state"
    );

    // Keyring reachable → no warning either: `cydrive setup` can
    // persist the token there.
    let results = run_doctor(&ctx("", Arc::new(InMemoryStore::new())));
    assert_eq!(
        find_result(&results, "credentials").status,
        CheckStatus::Ok,
        "reachable keyring with no token yet is not a warning"
    );
}

// ---------------------------------------------------------------- setup ---

#[test]
fn token_validation_mirrors_python() {
    // Python wizard: empty → "Token cannot be empty."; no ':' → the
    // '123456789:ABCdef...' warning text.
    assert_eq!(
        validate_token("  "),
        Err("Token cannot be empty".to_string())
    );
    assert_eq!(
        validate_token("abcdef"),
        Err("Bot tokens usually follow format '123456789:ABCdef...'".to_string())
    );
    assert_eq!(validate_token("123456789:ABCdef"), Ok(()));
}

#[test]
fn apply_wizard_fills_fields_and_normalizes_letter() {
    let answers = WizardAnswers {
        bot_token: "123456789:ABCdef".to_string(),
        chat_id: 42,
        drive_letter: "y".to_string(),
    };
    let cfg = apply_wizard(CyDriveConfig::default(), &answers);
    assert_eq!(cfg.bot_token, "123456789:ABCdef");
    assert_eq!(cfg.chat_id, 42);
    assert_eq!(cfg.drive_letter, "Y:", "letter normalized");
    // Everything else keeps the incoming (default) values.
    assert_eq!(cfg.webdav_port, 8080);
    assert_eq!(cfg.webdav_host, "127.0.0.1");
    assert!(cfg.is_configured(), "wizard output counts as configured");
}

#[test]
fn persist_setup_scrubs_and_stores() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = chdir(dir.path());
    let answers = WizardAnswers {
        bot_token: "123456789:ABCdef".to_string(),
        chat_id: 42,
        drive_letter: "y".to_string(),
    };
    let cfg = apply_wizard(CyDriveConfig::default(), &answers);

    let store = InMemoryStore::new();
    persist_setup(&cfg, Some(&store)).expect("persist setup");

    // The token lives in the store, not in the file.
    assert_eq!(
        store.get(BOT_TOKEN).expect("get token"),
        Some("123456789:ABCdef".to_string())
    );
    let text = fs::read_to_string("config.toml").expect("config.toml written");
    let on_disk = CyDriveConfig::load_toml(Path::new("config.toml")).expect("toml loads");
    assert_eq!(on_disk.bot_token, "", "token scrubbed from the file");
    assert!(
        !text.contains("ABCdef"),
        "no token secret in the file: {text}"
    );
    assert_eq!(on_disk.chat_id, 42);
    assert_eq!(on_disk.drive_letter, "Y:");
}

#[test]
fn persist_setup_roundtrips_discover() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = chdir(dir.path());
    let answers = WizardAnswers {
        bot_token: "999:xyz".to_string(),
        chat_id: -100200,
        drive_letter: "z".to_string(),
    };
    let cfg = apply_wizard(CyDriveConfig::default(), &answers);

    let store = InMemoryStore::new();
    persist_setup(&cfg, Some(&store)).expect("persist setup");

    let discovered = discover_config_with_store(&store).expect("discover after setup");
    assert!(
        discovered.is_configured(),
        "store backfill restores the token"
    );
    assert_eq!(discovered.bot_token, "999:xyz");
    assert_eq!(discovered.chat_id, -100200);
    assert_eq!(discovered.drive_letter, "Z:");
}

/// Headless fix (2026-09-04): when no credential store exists (WSL /
/// servers without Secret Service), `persist_setup(None)` must keep the
/// secrets by writing them INTO config.toml — the old flow stored them
/// in a volatile in-memory fallback and lost them while claiming success.
#[test]
fn persist_setup_headless_writes_secrets_into_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = chdir(dir.path());
    let answers = WizardAnswers {
        bot_token: "123456789:HEADLESS".to_string(),
        chat_id: 42,
        drive_letter: "y".to_string(),
    };
    let cfg = apply_wizard(CyDriveConfig::default(), &answers);

    persist_setup(&cfg, None).expect("persist headless setup");

    let on_disk = CyDriveConfig::load_toml(Path::new("config.toml")).expect("toml loads");
    assert_eq!(
        on_disk.bot_token, "123456789:HEADLESS",
        "the token survives in the file"
    );
    assert!(on_disk.is_configured());
    // Discovery against an empty (unreachable) keyring still works: the
    // file value wins over an empty backfill.
    let discovered =
        discover_config_with_store(&InMemoryStore::new()).expect("discover headless setup");
    assert_eq!(discovered.bot_token, "123456789:HEADLESS");
    assert!(discovered.is_configured());
}
