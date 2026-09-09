//! RED-phase tests for Phase 2.5 / MV4: the CLI ops surface over the
//! multi-volume manifest — the `volumes` listing, doctor's per-volume
//! checks, `status`'s per-volume db stats, the `setup --multi` skeleton
//! and the K25 `stop` path pin.
//!
//! Contract under test: `docs/plans/2026-09-08-phase2-5-multivolume.md`
//! §3-MV4. Single-volume behaviour is pinned zero-drift by the existing
//! suites (ops.rs doctor, status.rs) — this file only adds the
//! multi-volume faces. The aggregated multi-volume stop itself is proven
//! by `multivolume_e2e.rs::one_control_stop_stops_every_volume`; here it
//! is only pinned that the `cydrive stop` discovery chain resolves the
//! same cwd-anchored control file the multi-volume boot binds.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use cloudkit_cli::control::control_file_path;
use cloudkit_cli::doctor::{run_doctor_multi, CheckResult, CheckStatus};
use cloudkit_cli::setup::run_setup_multi;
use cloudkit_cli::volumes::{
    collect_volume_rows, collect_volume_stats, render_volume_stats, volumes_report, VolumeDbOutcome,
};
use cloudkit_cli::{
    discover_config, discover_config_with_volumes_and_store, resolve_volume_settings,
    DiscoveredConfig,
};
use cloudkit_core::config::VolumeConfig;
use cloudkit_core::credentials::InMemoryStore;
use cloudkit_core::database::{FileUpsert, MetaDatabase};

// ------------------------------------------------------------- helpers ---

/// `CYDRIVE_*` keys cleared on guard drop so a developer shell cannot
/// skew the tests (same list as `multivolume_config.rs`).
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

/// A port nothing holds: bind `:0`, read the port, drop the listener.
fn free_port() -> u16 {
    let probe = TcpListener::bind("127.0.0.1:0").expect("probe a free port");
    probe.local_addr().expect("local addr").port()
}

/// A multi-volume process config.toml body with free probe ports.
fn multi_process_toml() -> String {
    format!(
        "volumes_dir = \"volumes\"\nwebdav_port = {}\nweb_ui_port = {}\nenable_web_ui = false\n",
        free_port(),
        free_port()
    )
}

/// Discover in the cwd and unwrap the `Multi` arm.
fn discover_multi() -> (cloudkit_core::config::CyDriveConfig, Vec<VolumeConfig>) {
    match discover_config_with_volumes_and_store(&InMemoryStore::new()).expect("discovery") {
        DiscoveredConfig::Multi { process, volumes } => (process, volumes),
        DiscoveredConfig::Single(_) => panic!("volumes_dir set means multi-volume discovery"),
    }
}

/// Finds the check named `name` in a doctor run's results.
fn find_result<'a>(results: &'a [CheckResult], name: &str) -> &'a CheckResult {
    results
        .iter()
        .find(|result| result.name == name)
        .unwrap_or_else(|| panic!("no check named {name:?} in {results:?}"))
}

/// A plain file row (the ops.rs shape) with the upload flag injectable —
/// one uploaded + one pending row exercises both stats columns.
fn file_entry(rel: &str, size: i64, uploaded: bool) -> FileUpsert {
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
        is_uploaded: uploaded,
        is_cached: false,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: None,
    }
}

// ------------------------------------------------------- volumes listing ---

/// `cydrive volumes` in multi-volume mode: one row per volume file in
/// stable name order — name, backend, the EXPLICIT drive letter (an
/// unclaimed volume shows the placeholder, never the parsed "Y:" default
/// — the same presence semantics K27 pinned for mounts), the K21-resolved
/// db path inside the volume home, and the volume file itself.
#[test]
fn volumes_report_lists_both_volumes_with_placeholder_letters() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(&dir.path().join("config.toml"), &multi_process_toml());
    write_file(
        &dir.path().join("volumes").join("alpha.toml"),
        "backend = \"local\"\nlocal_root = \"root-a\"\ndrive_letter = \"V\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("beta.toml"),
        "backend = \"telegram\"\n",
    );
    let _guard = chdir(dir.path());

    let discovered = discover_multi();

    let rows = collect_volume_rows(&discovered.1).expect("listing rows");
    assert_eq!(rows.len(), 2, "one row per volume file");
    assert_eq!(rows[0].name, "alpha");
    assert_eq!(rows[0].backend, "local");
    assert_eq!(
        rows[0].drive_letter.as_deref(),
        Some("V:"),
        "an explicit letter renders canonicalised"
    );
    assert_eq!(
        rows[0].db_path,
        dir.path()
            .join("volumes")
            .join("alpha")
            .join("cydrive_meta.db"),
        "the default db_path lands inside the K21 volume home"
    );
    assert_eq!(
        rows[0].file_path,
        Path::new("volumes").join("alpha.toml"),
        "the volume file reports the discovered path verbatim"
    );
    assert_eq!(rows[1].name, "beta");
    assert_eq!(rows[1].backend, "telegram");
    assert_eq!(
        rows[1].drive_letter, None,
        "an unclaimed letter is None, not the Y: placeholder default"
    );

    let report = volumes_report(&DiscoveredConfig::Multi {
        process: discovered.0,
        volumes: discovered.1,
    })
    .expect("report");
    let expected_db = dir
        .path()
        .join("volumes")
        .join("alpha")
        .join("cydrive_meta.db")
        .to_string_lossy()
        .into_owned();
    for expected in ["alpha", "beta", "local", "telegram", &expected_db] {
        assert!(
            report.contains(expected),
            "missing {expected:?} in:\n{report}"
        );
    }
    assert!(
        !report.contains("Y:"),
        "an unclaimed volume must not report the placeholder default letter:\n{report}"
    );
}

/// `cydrive volumes` in single-volume mode: the command says so and hands
/// out the one-line migration hint (set volumes_dir + move the
/// volume-scoped keys into volume files).
#[test]
fn volumes_report_single_mode_prints_the_migration_hint() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "bot_token = \"123456:ABC-DEF\"\nchat_id = 123456789\n",
    );
    let _guard = chdir(dir.path());

    let discovered =
        discover_config_with_volumes_and_store(&InMemoryStore::new()).expect("discovery");
    let report = volumes_report(&discovered).expect("report");
    assert!(
        report.contains("Single-volume"),
        "names the current mode: {report}"
    );
    assert!(
        report.contains("volumes_dir"),
        "the migration hint names the key: {report}"
    );
}

/// A `volumes_dir` with no directory behind it is the discoverer's
/// actionable error — the listing must not degrade to an empty table.
#[test]
fn volumes_listing_errors_when_the_volumes_directory_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    // No `volumes/` directory on purpose.
    let _guard = chdir(dir.path());

    let err = discover_config_with_volumes_and_store(&InMemoryStore::new())
        .expect_err("a missing volumes directory must fail discovery");
    let message = format!("{err:#}");
    assert!(
        message.contains("does not exist"),
        "actionable wording naming the directory: {message}"
    );
}

// -------------------------------------------------------------- doctor ---

/// The multi-volume doctor happy path: process-level config + port
/// checks, then per volume the config legality, the writable home, the
/// backend checks (local root) and the db probe — an existing db with
/// rows reads Ok with the count, a fresh volume's db is the same Warn
/// the single-volume doctor reports.
#[test]
fn doctor_multi_volume_checks_happy_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(&dir.path().join("config.toml"), &multi_process_toml());
    write_file(
        &dir.path().join("volumes").join("alpha.toml"),
        "backend = \"local\"\nlocal_root = \"root-a\"\ndb_path = \"meta.db\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("beta.toml"),
        "backend = \"local\"\nlocal_root = \"root-b\"\n",
    );
    // alpha's db pre-exists with two rows; beta's stays fresh.
    let alpha_db = dir.path().join("volumes").join("alpha").join("meta.db");
    let db = MetaDatabase::open(&alpha_db).expect("open alpha's db");
    db.upsert_file(&file_entry("/a.txt", 512 * 1024, true))
        .expect("insert a");
    db.upsert_file(&file_entry("/b.txt", 512 * 1024, true))
        .expect("insert b");
    drop(db);
    let _guard = chdir(dir.path());

    let (process, volumes) = discover_multi();
    let results = run_doctor_multi(&process, &volumes);

    // Process level: config + both ports.
    assert_eq!(find_result(&results, "config").status, CheckStatus::Ok);
    assert!(
        find_result(&results, "config")
            .detail
            .contains("2 volume(s)"),
        "the config line carries the volume count: {}",
        find_result(&results, "config").detail
    );
    assert_eq!(find_result(&results, "webdav port").status, CheckStatus::Ok);
    assert_eq!(find_result(&results, "web ui port").status, CheckStatus::Ok);

    // alpha: every check Ok — the db exists and reads with its count.
    for name in [
        "volume alpha: config",
        "volume alpha: home",
        "volume alpha: database",
        "volume alpha: local root",
    ] {
        assert_eq!(
            find_result(&results, name).status,
            CheckStatus::Ok,
            "{name}: {}",
            find_result(&results, name).detail
        );
    }
    assert!(
        find_result(&results, "volume alpha: database")
            .detail
            .contains("2 files"),
        "the db detail carries the row count: {}",
        find_result(&results, "volume alpha: database").detail
    );

    // beta: healthy config/home/root; the fresh db is the single-volume
    // doctor's "not created yet" Warn, not a Fail.
    for name in [
        "volume beta: config",
        "volume beta: home",
        "volume beta: local root",
    ] {
        assert_eq!(
            find_result(&results, name).status,
            CheckStatus::Ok,
            "{name}: {}",
            find_result(&results, name).detail
        );
    }
    let beta_db = find_result(&results, "volume beta: database");
    assert_eq!(beta_db.status, CheckStatus::Warn);
    assert!(
        beta_db.detail.contains("not created yet"),
        "a fresh volume's db is a Warn: {}",
        beta_db.detail
    );
}

/// One broken volume fails ONLY its own checks: beta's db path points at
/// an existing directory (an unopenable db location), which must surface
/// as a Fail while the healthy sibling alpha reports normally.
#[test]
fn doctor_multi_reports_a_failed_volume() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(&dir.path().join("config.toml"), &multi_process_toml());
    write_file(
        &dir.path().join("volumes").join("alpha.toml"),
        "backend = \"local\"\nlocal_root = \"root-a\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("beta.toml"),
        "backend = \"local\"\nlocal_root = \"root-b\"\ndb_path = \"blockdir\"\n",
    );
    // beta's resolved db path is this existing directory — open fails.
    fs::create_dir_all(dir.path().join("volumes").join("beta").join("blockdir"))
        .expect("create the blocking directory");
    let _guard = chdir(dir.path());

    let (process, volumes) = discover_multi();
    let results = run_doctor_multi(&process, &volumes);

    let beta_db = find_result(&results, "volume beta: database");
    assert_eq!(beta_db.status, CheckStatus::Fail);
    assert!(
        beta_db.detail.contains("cannot be opened") || beta_db.detail.contains("blockdir"),
        "the failure names the location: {}",
        beta_db.detail
    );
    // The healthy sibling is unaffected (fresh db → Warn, config Ok).
    assert_eq!(
        find_result(&results, "volume alpha: config").status,
        CheckStatus::Ok
    );
    assert_eq!(
        find_result(&results, "volume alpha: database").status,
        CheckStatus::Warn
    );
}

// -------------------------------------------------------------- status ---

/// The multi-volume status stats: one row per volume with the db read
/// read-only (exists-guarded — a fresh volume reports not-created, never
/// gets its db created by a status command), and the rendered table
/// carries the volume-listing header plus every volume's numbers.
#[test]
fn status_multi_volume_stats_carry_each_volumes_numbers() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(&dir.path().join("config.toml"), &multi_process_toml());
    write_file(
        &dir.path().join("volumes").join("alpha.toml"),
        "backend = \"local\"\nlocal_root = \"root-a\"\ndb_path = \"meta.db\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("beta.toml"),
        "backend = \"telegram\"\n",
    );
    let alpha_db = dir.path().join("volumes").join("alpha").join("meta.db");
    let db = MetaDatabase::open(&alpha_db).expect("open alpha's db");
    db.upsert_file(&file_entry("/a.txt", 512 * 1024, true))
        .expect("insert a (uploaded)");
    db.upsert_file(&file_entry("/b.txt", 512 * 1024, false))
        .expect("insert b (pending)");
    drop(db);
    let _guard = chdir(dir.path());

    let (_, volumes) = discover_multi();
    let rows = collect_volume_stats(&volumes).expect("stats rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "alpha");
    match &rows[0].outcome {
        VolumeDbOutcome::Stats(stats) => {
            assert_eq!(stats.total_files, 2);
            assert_eq!(stats.uploaded_files, 1);
            assert_eq!(stats.pending_uploads, 1);
        }
        other => panic!("alpha's db stats expected, got {other:?}"),
    }
    assert_eq!(rows[1].name, "beta");
    assert!(
        matches!(rows[1].outcome, VolumeDbOutcome::NotCreatedYet),
        "beta's fresh db reads as not-created: {:?}",
        rows[1].outcome
    );
    assert!(
        !rows[1].db_path.exists(),
        "a status read must not create the missing db"
    );

    let rendered = render_volume_stats(&rows);
    assert!(
        rendered.contains("volumes: 2 configured (alpha, beta)"),
        "the volume-listing header: {rendered}"
    );
    assert!(
        rendered.contains("1.00 MB"),
        "alpha's byte total (2 x 512 KiB): {rendered}"
    );
    assert!(
        rendered.contains("not created yet"),
        "beta's fresh db note: {rendered}"
    );
}

// --------------------------------------------------------------- setup ---

/// `cydrive setup --multi` writes a loadable skeleton: a process-scoped
/// config.toml (volumes_dir + process keys, NO volume-scoped keys — the
/// K19 mixing guard would reject them) and one local-backend example
/// volume file with a commented-out drive_letter; the generated set must
/// discover as Multi and survive the boot-time K21 resolution + validate.
#[test]
fn setup_multi_writes_a_loadable_skeleton() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = chdir(dir.path());

    let report = run_setup_multi().expect("the skeleton write succeeds");
    assert!(
        report.contains("config.toml") && report.contains("volumes/local.toml"),
        "the report names both files: {report}"
    );

    let process = fs::read_to_string("config.toml").expect("read config.toml");
    assert!(
        process.contains("volumes_dir = \"volumes\""),
        "the process config points at the volumes dir: {process}"
    );
    assert!(
        !process.contains("bot_token"),
        "the process config must stay process-scoped (K19 mixing guard): {process}"
    );

    let volume =
        fs::read_to_string(dir.path().join("volumes").join("local.toml")).expect("read the volume");
    assert!(volume.contains("backend = \"local\""), "{volume}");
    assert!(volume.contains("local_root"), "{volume}");
    assert!(
        volume.contains("# drive_letter"),
        "the commented drive_letter example: {volume}"
    );

    // The skeleton is loadable end to end.
    let (process_cfg, volumes) = discover_multi();
    assert_eq!(volumes.len(), 1);
    assert_eq!(volumes[0].name, "local");
    let resolved = resolve_volume_settings(&volumes[0]).expect("K21 resolution");
    resolved
        .validate()
        .expect("the skeleton volume survives boot-time validation");
    assert!(
        process_cfg.volumes_dir.is_some(),
        "the discovered process config is multi-volume"
    );
}

/// `cydrive setup --multi` never overwrites: an existing config.toml (or
/// an already-present example volume file) refuses with the actionable
/// message instead.
#[test]
fn setup_multi_refuses_to_overwrite_an_existing_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "bot_token = \"123456:ABC-DEF\"\nchat_id = 123456789\n",
    );
    {
        let _guard = chdir(dir.path());
        let err = run_setup_multi().expect_err("an existing config must be refused");
        let message = format!("{err:#}");
        assert!(
            message.contains("already exists"),
            "the refusal names what exists: {message}"
        );
    }

    // A stray example volume file without a config.toml is refused too —
    // the skeleton must not clobber user work.
    let dir2 = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir2.path().join("volumes").join("local.toml"),
        "backend = \"local\"\nlocal_root = \"C:/data\"\n",
    );
    let _guard2 = chdir(dir2.path());
    assert!(
        run_setup_multi().is_err(),
        "an existing volumes/local.toml must be refused"
    );
}

// ---------------------------------------------------------------- stop ---

/// K25 pin: `cydrive stop` discovers through the plain single-volume
/// chain; the multi-volume boot binds its ONE control server against the
/// process config. Both must anchor the same `cydrive.control` in the
/// working directory, so the zero-change stop path cannot drift. (The
/// aggregated stop-every-volume semantics are proven by
/// `multivolume_e2e.rs::one_control_stop_stops_every_volume`.)
#[test]
fn stop_resolves_the_same_process_control_file_in_multi_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(&dir.path().join("config.toml"), &multi_process_toml());
    write_file(
        &dir.path().join("volumes").join("alpha.toml"),
        "backend = \"local\"\nlocal_root = \"root-a\"\n",
    );
    let _guard = chdir(dir.path());

    let single = discover_config().expect("the stop discovery chain");
    let (process, _) = discover_multi();
    assert_eq!(
        control_file_path(&single),
        control_file_path(&process),
        "the stop chain and the multi-volume boot anchor the same control file"
    );
    let control = control_file_path(&process);
    assert_eq!(
        control.file_name().and_then(|name| name.to_str()),
        Some("cydrive.control")
    );
    assert!(
        control
            .parent()
            .is_some_and(|parent| parent == Path::new(".")),
        "the control file anchors in the working directory: {}",
        control.display()
    );
}
