//! RED-phase tests for the `cydrive rebuild` CLI body (Phase 2 / K11 +
//! Phase 8-B / EB3):
//! `cloudkit_cli::run_rebuild_with_driver` (the injected-driver seam the
//! production `run_rebuild_command` feeds with the backend-key driver
//! assembly) and its gates.
//!
//! Contract under test:
//!
//! - **Happy path**: a seeded driver + a local-backend instance config
//!   → rows land in the instance db (the cwd's `db_path`), the command
//!   reports the rebuilt counts.
//! - **Encrypted instance** (B5, EB3): rebuilds through the same seam —
//!   the walk carries the production `CipherCtx` into
//!   `rebuild_from_backend_with_ctx`, so file rows land with
//!   `is_encrypted = true`, the configured scheme and the plaintext
//!   size closed-form back-solved from the container length.
//! - **Encrypted local volume integration** (EB3, feature `local`): a
//!   real `LocalDriver` over a TempDir backend pre-seeded with freshly
//!   encrypted v2 containers → full-tree rebuild through the production
//!   seam → every row's cipher fields exact → at least one file reads
//!   back through the Vfs read path byte-for-byte (the EB4 leg-3 probe).
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
    // K11 single-container chunks row (bookkeeping parity with an
    // upload persist's one-element receipt).
    let chunks = db.get_chunks_by_file_id(row.id).expect("read chunks");
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].chunk_index, 0);
    assert_eq!(chunks[0].telegram_msg_id, row.telegram_msg_id);
    assert_eq!(chunks[0].size, 5);
}

/// B5 (EB3): an encrypted instance rebuilds through the production seam
/// — the walk carries the production `CipherCtx` (password + configured
/// scheme, `CipherCtx::from_cfg` judgment) so the file row lands
/// cipher-correct: `is_encrypted`, the scheme, and the plaintext size
/// closed-form back-solved from the container length.
#[tokio::test]
async fn encrypted_instance_rebuilds_with_cipher_truth_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = local_instance_config(dir.path());
    cfg.enable_encryption = true;
    cfg.encryption_password = Some("pw".to_string());

    // Backend content = a real v2 container under a plaintext name.
    let plain = b"encrypted payload for the rebuild seam";
    let container = cloudkit_core::crypto::AeadV2::new().encrypt("pw", plain);
    let driver = MockStorageDriver::new(VolumeId::parse("baidu:123456789").expect("volume id"));
    let rel = RelPath::new("secret.bin").expect("seed path");
    let hint = WriteHint {
        size: Some(container.len() as u64),
        ..Default::default()
    };
    let mut stager = driver.writer(&rel, &hint).await.expect("seed writer");
    stager.write(&container).await.expect("seed write");
    stager.close().await.expect("seed close");

    let outcome = run_rebuild_with_driver(&cfg, &driver)
        .await
        .expect("encrypted instance rebuilds through the production seam (B5)");
    assert_eq!(outcome.files, 1, "one backend file rebuilt");

    let db = MetaDatabase::open(std::path::Path::new(&cfg.db_path)).expect("open instance db");
    let row = db
        .get_file("/secret.bin")
        .expect("read row")
        .expect("row exists");
    assert!(row.is_encrypted, "cipher flag from the instance config");
    assert_eq!(
        row.encryption_scheme, "aead_v2",
        "scheme from the instance config (default AeadV2)"
    );
    assert_eq!(
        row.size,
        plain.len() as i64,
        "size = closed-form back-solve of the container length"
    );
    let chunks = db.get_chunks_by_file_id(row.id).expect("read chunks");
    assert_eq!(chunks.len(), 1, "one single-container chunk row");
    assert_eq!(
        chunks[0].size,
        container.len() as i64,
        "the chunk keeps the backend container length"
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

/// EB3 integration leg (EB4 leg 3's probe): a REAL encrypted local
/// volume — a TempDir backend pre-seeded with freshly encrypted v2
/// containers — rebuilt through the production seam (`run_rebuild_with_driver`,
/// which opens a fresh db and drives `rebuild_from_backend_with_ctx`
/// with the production `CipherCtx`): every file row lands cipher-exact
/// (flag / config scheme / closed-form plaintext size, container length
/// on the chunk) and at least one file then reads back through the Vfs
/// read path byte-for-byte.
#[cfg(feature = "local")]
#[tokio::test]
async fn encrypted_local_volume_rebuilds_and_reads_through() {
    use std::sync::Arc;

    use cloudkit_core::cache::CacheManager;
    use cloudkit_core::crypto::AeadV2;
    use cloudkit_core::rel_path::RelPath as VfsRelPath;
    use cloudkit_core::transport::CloudTransport;
    use cloudkit_core::vfs::{StreamSource, Vfs};
    use futures_util::StreamExt as _;

    // Backend: a real local root pre-seeded with fresh v2 containers
    // under plaintext names (root file + nested file + empty file —
    // the closed-form size edge).
    let backend = tempfile::tempdir().expect("backend tempdir");
    let plain_a = b"alpha payload read back byte-for-byte";
    let ct_a = AeadV2::new().encrypt("pw", plain_a);
    std::fs::write(backend.path().join("alpha.txt"), &ct_a).expect("seed alpha");
    std::fs::create_dir_all(backend.path().join("docs")).expect("seed docs dir");
    let plain_b = b"nested beta payload";
    let ct_b = AeadV2::new().encrypt("pw", plain_b);
    std::fs::write(backend.path().join("docs").join("beta.bin"), &ct_b).expect("seed beta");
    let ct_empty = AeadV2::new().encrypt("pw", b"");
    assert_eq!(
        ct_empty.len(),
        50,
        "v2 empty container = 34B header + 16B tag"
    );
    std::fs::write(backend.path().join("empty.bin"), &ct_empty).expect("seed empty");

    // Instance: local backend over that root, encryption on, a fresh
    // instance dir the seam will create its db in.
    let inst = tempfile::tempdir().expect("instance tempdir");
    let cfg = CyDriveConfig {
        backend: Backend::Local,
        local_root: Some(backend.path().to_string_lossy().into_owned()),
        db_path: inst.path().join("meta.db").to_string_lossy().into_owned(),
        cache_path: inst.path().join("cache").to_string_lossy().into_owned(),
        enable_encryption: true,
        encryption_password: Some("pw".to_string()),
        ..CyDriveConfig::default()
    };

    let driver = ck_local::factory(&ck_local::LocalParams {
        root: backend.path().to_path_buf(),
    })
    .await
    .expect("local driver over the pre-seeded root");

    // The production seam: fresh db + `rebuild_from_backend_with_ctx`
    // driven by the production cipher context. The K11 gate refused
    // this call before EB3.
    let outcome = run_rebuild_with_driver(&cfg, driver.as_ref())
        .await
        .expect("encrypted local volume rebuilds through the production seam (B5)");
    assert_eq!(
        (outcome.files, outcome.dirs),
        (3, 1),
        "two root files + one nested + docs/"
    );

    // Every row's cipher fields are exact.
    let db = MetaDatabase::open(std::path::Path::new(&cfg.db_path)).expect("open instance db");
    for (path, plain, ct) in [
        ("/alpha.txt", plain_a.as_slice(), ct_a.as_slice()),
        ("/docs/beta.bin", plain_b.as_slice(), ct_b.as_slice()),
    ] {
        let row = db.get_file(path).expect("read row").expect("row exists");
        assert!(row.is_encrypted, "{path}: cipher flag");
        assert_eq!(row.encryption_scheme, "aead_v2", "{path}: config scheme");
        assert_eq!(
            row.size,
            plain.len() as i64,
            "{path}: size = closed-form back-solve"
        );
        let chunks = db.get_chunks_by_file_id(row.id).expect("chunks");
        assert_eq!(chunks.len(), 1, "{path}: single-container chunk");
        assert_eq!(
            chunks[0].size,
            ct.len() as i64,
            "{path}: chunk = container length"
        );
    }
    // The empty-file edge: 50-byte container → plaintext size 0.
    let empty = db
        .get_file("/empty.bin")
        .expect("read empty")
        .expect("row exists");
    assert!(empty.is_encrypted, "empty container row is encrypted");
    assert_eq!(empty.size, 0, "v2 empty container backsolves to size 0");
    // Directory rows never cipher.
    let docs = db.get_file("/docs").expect("read docs").expect("dir row");
    assert!(
        docs.is_dir && !docs.is_encrypted,
        "directory rows stay plaintext"
    );

    // Read-through: one file back through the Vfs read path over the
    // same local transport (K47 stream arm with first-read validation,
    // or the hydrate fallback — either serve must be byte-for-byte).
    let transport: Arc<dyn CloudTransport> = Arc::new(ck_local::LocalTransport::new(driver));
    let cache = CacheManager::new(inst.path().join("cache"), 1 << 20);
    let db = Arc::new(MetaDatabase::open(std::path::Path::new(&cfg.db_path)).expect("reopen db"));
    let vfs = Vfs::new(db, cache, transport, cloudkit_cli::vfs_config(&cfg));
    let rel = VfsRelPath::new("/alpha.txt").expect("valid vpath");
    match vfs
        .open_read(&rel)
        .await
        .expect("open_read admits the rebuilt encrypted row")
    {
        StreamSource::Stream {
            handle,
            total_size,
            transport,
        } => {
            assert_eq!(
                total_size,
                plain_a.len() as u64,
                "K35: stream total is the back-solved plaintext size"
            );
            let mut window = transport
                .open_range(&handle, 0, total_size)
                .await
                .expect("decrypting window");
            let mut out = Vec::new();
            while let Some(chunk) = window.next().await {
                out.extend_from_slice(&chunk.expect("window chunk"));
            }
            assert_eq!(out, plain_a, "stream arm decrypts byte-for-byte");
        }
        StreamSource::Hydrate => {
            let path = vfs.hydrate(&rel).await.expect("hydrate the row");
            assert_eq!(
                std::fs::read(path).expect("read hydrated file"),
                plain_a,
                "hydrate arm decrypts byte-for-byte"
            );
        }
    }
}
