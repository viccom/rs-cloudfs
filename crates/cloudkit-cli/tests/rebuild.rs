//! RED-phase tests for the `cydrive rebuild` CLI body (Phase 2 / K11):
//! `cloudkit_cli::run_rebuild_with_driver` (the injected-driver seam the
//! production `run_rebuild_command` feeds with the backend-key driver
//! assembly) and its gates.
//!
//! Contract under test:
//!
//! - **Happy path**: a seeded driver + a local-backend instance config
//!   → rows land in the instance db (the cwd's `db_path`), the command
//!   reports the rebuilt counts.
//! - **Encrypted instance**: refused with the K11 sync guidance
//!   (before any backend access).
//! - **Telegram backend**: refused — telegram's remote store is
//!   message-shaped (no list face); the local db IS the authoritative
//!   index (shadow index). The refusal points at `cydrive sync`.

use std::path::PathBuf;

use cloudkit_cli::{run_rebuild_with_driver, TELEGRAM_REBUILD_REFUSAL};
use cloudkit_core::config::{Backend, CyDriveConfig};
use cloudkit_core::database::MetaDatabase;
use cloudkit_storage::{MockStorageDriver, RelPath, StorageDriver, VolumeId, WriteHint};

// ------------------------------------------------------------- helpers ---

/// An instance config for `tag` living entirely inside `dir`
/// (local backend + platform-absolute local_root, plaintext).
fn local_instance_config(dir: &std::path::Path) -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Local,
        local_root: Some(
            PathBuf::from(dir)
                .join("root")
                .to_string_lossy()
                .into_owned(),
        ),
        db_path: dir.join("meta.db").to_string_lossy().into_owned(),
        cache_path: dir.join("cache").to_string_lossy().into_owned(),
        ..CyDriveConfig::default()
    }
}

/// A mock driver over a scratch volume with one seeded file.
async fn seeded_driver() -> MockStorageDriver {
    let driver = MockStorageDriver::new(VolumeId::parse("baidu:123456789").expect("volume id"));
    let rel = RelPath::new("hello.txt").expect("seed path");
    let hint = WriteHint {
        size: Some(5),
        ..Default::default()
    };
    let mut stager = driver.writer(&rel, &hint).await.expect("seed writer");
    stager.write(b"hello").await.expect("seed write");
    stager.close().await.expect("seed close");
    driver
}

// -------------------------------------------------------------- tests ---

#[tokio::test]
async fn rebuild_writes_rows_into_the_instance_db() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = local_instance_config(dir.path());
    let driver = seeded_driver().await;

    let outcome = run_rebuild_with_driver(&cfg, &driver)
        .await
        .expect("rebuild against the seeded backend");
    assert_eq!(outcome.files, 1, "one backend file rebuilt");

    // The rows live in the instance db the config points at.
    let db = MetaDatabase::open(std::path::Path::new(&cfg.db_path)).expect("open instance db");
    let row = db
        .get_file("/hello.txt")
        .expect("read row")
        .expect("row exists");
    assert_eq!(row.size, 5);
    assert!(row.is_uploaded);
}

#[tokio::test]
async fn encrypted_instance_is_refused_with_sync_guidance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = local_instance_config(dir.path());
    cfg.enable_encryption = true;
    cfg.encryption_password = Some("pw".to_string());
    let driver = seeded_driver().await;

    let err = run_rebuild_with_driver(&cfg, &driver)
        .await
        .expect_err("encrypted instance must be refused");
    let message = err.to_string();
    assert!(
        message.contains("sync"),
        "the refusal must point at cydrive sync, got: {message}"
    );
}

#[tokio::test]
async fn telegram_backend_is_refused_as_the_shadow_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = CyDriveConfig {
        db_path: dir.path().join("meta.db").to_string_lossy().into_owned(),
        cache_path: dir.path().join("cache").to_string_lossy().into_owned(),
        ..CyDriveConfig::default() // backend = telegram (the default)
    };
    let driver = seeded_driver().await;

    let err = run_rebuild_with_driver(&cfg, &driver)
        .await
        .expect_err("telegram must be refused (no authoritative index face)");
    assert!(
        err.to_string().contains("sync"),
        "the refusal must point at sync, got: {err}"
    );
    // The canonical refusal text is shared with the driver assembly
    // (build_driver bails with the same constant).
    assert!(
        TELEGRAM_REBUILD_REFUSAL.contains("telegram"),
        "the shared refusal names the backend"
    );
}
