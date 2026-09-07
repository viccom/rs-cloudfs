//! RED-phase spec tests for the Entry-metadata half of E-4: the
//! `files.encryption_scheme` column (schema-only additive, red line R6),
//! the `put`-time scheme write, and the sync payload's optional `scheme`
//! field (interfaces §4 wire compatibility: `None` = byte-identical with
//! the pre-E-4 payload; old consumers ignore the new key; old payloads
//! decode with `None`).

use std::sync::Arc;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::config::{EncryptionScheme, SCHEME_AEAD_V2, SCHEME_GCM};
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::sync::{apply_pulled_rows, deserialize_row, serialize_row, SyncPulledRow};
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::CloudTransport;
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use rusqlite::Connection;

// ------------------------------------------------------------- database ---

/// The exact pre-E-4 `files` DDL (the frozen Python contract shape) used
/// to hand-build a legacy database the new code must adopt.
const LEGACY_FILES_DDL: &str = "CREATE TABLE files (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    rel_path TEXT UNIQUE NOT NULL,
    name TEXT NOT NULL,
    parent_dir TEXT NOT NULL,
    size INTEGER DEFAULT 0,
    mtime REAL DEFAULT 0,
    sha256 TEXT,
    is_dir INTEGER DEFAULT 0,
    telegram_msg_id INTEGER,
    is_uploaded INTEGER DEFAULT 0,
    is_cached INTEGER DEFAULT 1,
    is_encrypted INTEGER DEFAULT 0,
    chunk_count INTEGER DEFAULT 1,
    mime_type TEXT,
    created_at REAL,
    updated_at REAL
);";

/// Whether the `files` table currently has the `encryption_scheme` column.
fn has_scheme_column(conn: &Connection) -> bool {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info('files')")
        .expect("pragma query");
    let names: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .expect("pragma rows")
        .map(|r| r.expect("name"))
        .collect();
    names.iter().any(|n| n == "encryption_scheme")
}

#[test]
fn fresh_database_gains_the_scheme_column() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open");
    drop(db);
    let conn = Connection::open(dir.path().join("meta.db")).expect("reopen raw");
    assert!(
        has_scheme_column(&conn),
        "a database created by this build carries encryption_scheme"
    );
}

#[test]
fn legacy_database_is_adopted_with_default_gcm_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("legacy.db");
    {
        let conn = Connection::open(&db_path).expect("legacy connection");
        conn.execute_batch("PRAGMA foreign_keys = OFF;")
            .expect("python parity: fk off");
        conn.execute_batch(LEGACY_FILES_DDL).expect("legacy schema");
        // One row exactly the way the Python baseline would write it (no
        // scheme column in the INSERT).
        conn.execute(
            "INSERT INTO files (rel_path, name, parent_dir, size, mtime, is_dir, \
             is_uploaded, is_cached, is_encrypted, chunk_count) \
             VALUES ('/old.txt', 'old.txt', '/', 3, 1.0, 0, 1, 0, 1, 1)",
            [],
        )
        .expect("legacy insert");
    }

    let db = MetaDatabase::open(&db_path).expect("adopt the legacy database");
    let row = db.get_file("/old.txt").expect("read").expect("row");
    assert_eq!(
        row.encryption_scheme, "gcm",
        "pre-existing rows default to the frozen v1 scheme (= current behavior)"
    );
    // The adopting connection also gained the column: a Python-shaped
    // INSERT (column omitted) still works and reads back 'gcm'.
    let conn = Connection::open(&db_path).expect("raw reopen");
    assert!(has_scheme_column(&conn), "migration added the column");
    conn.execute(
        "INSERT INTO files (rel_path, name, parent_dir, size, mtime, is_dir, \
         is_uploaded, is_cached, is_encrypted, chunk_count) \
         VALUES ('/pynew.txt', 'pynew.txt', '/', 4, 1.0, 0, 1, 0, 0, 1)",
        [],
    )
    .expect("python-shaped insert after migration");
    drop(conn);
    let row = db.get_file("/pynew.txt").expect("read").expect("row");
    assert_eq!(row.encryption_scheme, "gcm");
}

/// A canonical [`FileUpsert`] for scheme tests.
fn scheme_upsert(rel: &str, is_encrypted: bool) -> FileUpsert {
    let rel_path = RelPath::new(rel).expect("valid rel path");
    FileUpsert {
        rel_path: rel_path.as_str().to_string(),
        name: rel_path.name().to_string(),
        parent_dir: rel_path.parent().expect("parent").as_str().to_string(),
        size: 10,
        mtime: 1.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: Some(7),
        is_uploaded: true,
        is_cached: false,
        is_encrypted,
        chunk_count: 1,
        mime_type: None,
    }
}

#[test]
fn upsert_file_scheme_roundtrips_and_plain_upsert_preserves() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open");

    db.upsert_file_scheme(&scheme_upsert("/v2.bin", true), SCHEME_AEAD_V2)
        .expect("scheme upsert");
    let row = db.get_file("/v2.bin").expect("read").expect("row");
    assert_eq!(row.encryption_scheme, SCHEME_AEAD_V2);

    // Plain upserts (the queue's success write, hydrate's cached flip)
    // must preserve the scheme — only the two scheme-aware origins
    // (put path, sync apply) ever write it.
    let mut flip = scheme_upsert("/v2.bin", true);
    flip.is_cached = true;
    db.upsert_file(&flip).expect("plain upsert");
    let row = db.get_file("/v2.bin").expect("read").expect("row");
    assert_eq!(
        row.encryption_scheme, SCHEME_AEAD_V2,
        "plain upsert_file preserves the scheme column"
    );

    // Conflict-update through upsert_file_scheme DOES move the value
    // (the put path re-writing a row whose config changed).
    db.upsert_file_scheme(&scheme_upsert("/v2.bin", true), SCHEME_GCM)
        .expect("scheme upsert");
    let row = db.get_file("/v2.bin").expect("read").expect("row");
    assert_eq!(row.encryption_scheme, SCHEME_GCM);
}

// ------------------------------------------------------------ put path ---

/// Vfs over a pre-connected mock transport with the given scheme and a
/// password; returns (dir, db, vfs).
async fn vfs_env(scheme: EncryptionScheme) -> (tempfile::TempDir, Arc<MetaDatabase>, Arc<Vfs>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("db"));
    let cache = CacheManager::new(dir.path().join("cache"), 1 << 20);
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect");
    let transport: Arc<dyn CloudTransport> = mock;
    let cfg = VfsConfig {
        chunk_size_bytes: 4096,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: std::time::Duration::from_millis(1),
            max_backoff: std::time::Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: Some("pw".to_string()),
        encryption_scheme: scheme,
        hydrate_timeout: std::time::Duration::from_secs(60),
    };
    let vfs = Arc::new(Vfs::new(Arc::clone(&db), cache, transport, cfg));
    (dir, db, vfs)
}

#[tokio::test]
async fn put_records_the_configured_scheme_on_encrypted_rows() {
    // aead_v2-configured put flags the row encrypted with scheme aead_v2.
    let (_dir, db, vfs) = vfs_env(EncryptionScheme::AeadV2).await;
    let rel = RelPath::new("/a.bin").expect("rel");
    vfs.put(&rel, b"payload", 1.0).await.expect("put");
    let row = db.get_file("/a.bin").expect("read").expect("row");
    assert!(row.is_encrypted);
    assert_eq!(row.encryption_scheme, SCHEME_AEAD_V2);
    vfs.shutdown().await;

    // Default (gcm) put keeps the v1 scheme value.
    let (_dir, db, vfs) = vfs_env(EncryptionScheme::Gcm).await;
    let rel = RelPath::new("/b.bin").expect("rel");
    vfs.put(&rel, b"payload", 1.0).await.expect("put");
    let row = db.get_file("/b.bin").expect("read").expect("row");
    assert!(row.is_encrypted);
    assert_eq!(row.encryption_scheme, SCHEME_GCM);
    vfs.shutdown().await;

    // Unencrypted put (no password) leaves the scheme at the gcm default.
    let dir = tempfile::tempdir().expect("tempdir");
    let db2 = Arc::new(MetaDatabase::open(&dir.path().join("m.db")).expect("db"));
    let cache = CacheManager::new(dir.path().join("cache"), 1 << 20);
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("connect");
    let transport: Arc<dyn CloudTransport> = mock;
    let cfg = VfsConfig {
        // Scheme without password: the key must stay dormant.
        encryption_scheme: EncryptionScheme::AeadV2,
        ..VfsConfig::default()
    };
    let vfs = Vfs::new(Arc::clone(&db2), cache, transport, cfg);
    let rel = RelPath::new("/c.bin").expect("rel");
    vfs.put(&rel, b"payload", 1.0).await.expect("put");
    let row = db2.get_file("/c.bin").expect("read").expect("row");
    assert!(!row.is_encrypted, "no password -> plaintext row");
    assert_eq!(
        row.encryption_scheme, SCHEME_GCM,
        "the scheme only rides rows the password actually flags encrypted"
    );
    vfs.shutdown().await;
}

// ---------------------------------------------------------- sync payload ---

#[test]
fn payload_omits_scheme_for_gcm_rows_byte_identical_with_legacy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("db");
    db.upsert_file_scheme(&scheme_upsert("/gcm.bin", true), SCHEME_GCM)
        .expect("seed");
    let row = db.get_file("/gcm.bin").expect("read").expect("row");
    let payload = serialize_row(&row, &[]).expect("serialize");
    assert!(
        !payload.contains("\"scheme\""),
        "gcm rows serialize exactly like pre-E-4 payloads: {payload}"
    );
}

#[test]
fn payload_carries_scheme_only_for_non_gcm_encrypted_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("db");
    db.upsert_file_scheme(&scheme_upsert("/v2.bin", true), SCHEME_AEAD_V2)
        .expect("seed");
    let row = db.get_file("/v2.bin").expect("read").expect("row");
    let payload = serialize_row(&row, &[]).expect("serialize");
    assert!(
        payload.contains("\"scheme\":\"aead_v2\""),
        "v2 rows carry the scheme: {payload}"
    );
    let parsed = deserialize_row(&payload).expect("decode");
    assert_eq!(parsed.scheme.as_deref(), Some(SCHEME_AEAD_V2));

    // An unencrypted row never carries a scheme even if the column held
    // a stray value (the mapping is gated on is_encrypted).
    let mut plain = scheme_upsert("/plain.bin", false);
    plain.rel_path = "/plain.bin".to_string();
    db.upsert_file_scheme(&plain, SCHEME_AEAD_V2).expect("seed");
    let row = db.get_file("/plain.bin").expect("read").expect("row");
    let payload = serialize_row(&row, &[]).expect("serialize");
    assert!(
        !payload.contains("\"scheme\""),
        "unencrypted rows never carry a scheme: {payload}"
    );
}

#[test]
fn old_form_payload_decodes_with_scheme_none() {
    // Hand-written pre-E-4 payload (no scheme key): must decode with
    // None (= gcm).
    let old = r#"{"rel_path":"/old.bin","name":"old.bin","parent_dir":"/","size":5,
        "mtime":1.0,"sha256":null,"is_dir":false,"telegram_msg_id":9,
        "is_uploaded":true,"is_encrypted":true,"chunk_count":1,
        "mime_type":null,"chunks":[]}"#;
    let parsed = deserialize_row(old).expect("old payload decodes");
    assert_eq!(parsed.scheme, None, "missing key reads as the gcm default");
}

#[test]
fn new_form_payload_does_not_break_old_consumers() {
    // A consumer compiled against the pre-E-4 struct (no scheme field)
    // tolerates the new key: serde drops unknown fields by default.
    #[derive(serde::Deserialize)]
    struct OldRowPayload {
        rel_path: String,
        is_encrypted: bool,
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("db");
    db.upsert_file_scheme(&scheme_upsert("/v2.bin", true), SCHEME_AEAD_V2)
        .expect("seed");
    let row = db.get_file("/v2.bin").expect("read").expect("row");
    let payload = serialize_row(&row, &[]).expect("serialize");
    let old_view: OldRowPayload = serde_json::from_str(&payload).expect("old consumer parses");
    assert_eq!(old_view.rel_path, "/v2.bin");
    assert!(old_view.is_encrypted);
}

#[test]
fn apply_pulled_rows_restores_the_row_scheme() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("db");
    let cache = CacheManager::new(dir.path().join("cache"), 1 << 20);

    // Build the payload of a v2-encrypted uploaded row.
    let mut up = scheme_upsert("/synced.bin", true);
    up.is_uploaded = true;
    let db2 = MetaDatabase::open(&dir.path().join("src.db")).expect("src db");
    db2.upsert_file_scheme(&up, SCHEME_AEAD_V2)
        .expect("seed src");
    let src_row = db2.get_file("/synced.bin").expect("read").expect("row");
    let payload = serialize_row(&src_row, &[]).expect("serialize");

    apply_pulled_rows(
        &db,
        &cache,
        &[SyncPulledRow {
            rel_path: "/synced.bin".to_string(),
            version: 5,
            deleted: false,
            payload,
        }],
        5,
    )
    .expect("apply");

    let row = db.get_file("/synced.bin").expect("read").expect("row");
    assert_eq!(
        row.encryption_scheme, SCHEME_AEAD_V2,
        "sync apply restores the pulled row's scheme"
    );
}
