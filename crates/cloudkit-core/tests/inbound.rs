//! RED-phase tests for `cloudkit_core::inbound` (M2 inbound indexing
//! unit). All bodies are expected to panic with "not yet implemented"
//! until the GREEN phase lands.
//!
//! Contract under test (Python baseline `telegram_client.py:57-85`,
//! `_process_incoming_file`): a remote file arriving through
//! `transport.incoming()` is indexed metadata-only — `rel_path` is
//! `"/" + filename` built verbatim (a filename containing `/` forms a
//! nested virtual path, mirroring the Python no-sanitization behavior;
//! an empty/backslash/polluted name falls back to
//! `Telegram_File_{first_msg_id}.bin`), the row carries
//! `parent_dir = "/"`, `size = handle.total_size`, `mtime = now`,
//! `telegram_msg_id = first_msg_id`, `is_uploaded = true`,
//! `is_cached = false`, `is_encrypted = false`, `chunk_count = 1`
//! (single-message media) and no sha256. A same-named file overwrites
//! the row (rel_path unique key). The worker consumes the incoming
//! stream indefinitely: `Err` events warn-and-continue (never kill the
//! worker) and `Command` events log-and-skip (bot command unit pending).
//!
//! Known gap vs the Python baseline (frozen spec): `InboundFile` carries
//! no mime field, so `mime_type` is always `None` where Python
//! transparently stored `msg.file.mime_type`.

use std::sync::Arc;
use std::time::Duration;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::inbound::spawn_inbound_worker;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::{
    CloudTransport, InboundFile, IncomingEvent, RemoteHandle, StorageError,
};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};

/// VfsConfig for the inbound tests: default chunking, one queue worker,
/// tiny capacity — the queue is never exercised here, it only has to
/// spawn cleanly inside `Vfs::new`.
fn test_cfg() -> VfsConfig {
    VfsConfig {
        chunk_size_bytes: 1024 * 1024,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: None,
        encryption_scheme: cloudkit_core::config::EncryptionScheme::Gcm,
        hydrate_timeout: Duration::from_secs(180),
    }
}

/// Builds a real temp environment (SQLite db + cache tree + `Vfs` over
/// a plain mock transport). The first tuple item keeps the temp dir
/// alive.
async fn test_env() -> (tempfile::TempDir, Arc<MetaDatabase>, Arc<Vfs>) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let vfs = Arc::new(Vfs::new(
        Arc::clone(&db),
        CacheManager::new(dir.path().join("cache"), 64 * 1024 * 1024),
        Arc::new(MockTransport::new()),
        test_cfg(),
    ));
    (dir, db, vfs)
}

/// Same shape as [`test_env`], but the mock transport is scripted with
/// `events` and handed back as the trait object so the same `Arc` backs
/// both the `Vfs` and the inbound worker.
async fn worker_env(
    events: Vec<Result<IncomingEvent, StorageError>>,
) -> (
    tempfile::TempDir,
    Arc<MetaDatabase>,
    Arc<Vfs>,
    Arc<dyn CloudTransport>,
) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let transport: Arc<dyn CloudTransport> =
        Arc::new(MockTransport::builder().incoming_results(events).build());
    let vfs = Arc::new(Vfs::new(
        Arc::clone(&db),
        CacheManager::new(dir.path().join("cache"), 64 * 1024 * 1024),
        Arc::clone(&transport),
        test_cfg(),
    ));
    (dir, db, vfs, transport)
}

/// Builds a single-message `File` event for the scripted transports.
fn file_event(filename: &str, msg_id: i64, size: u64) -> Result<IncomingEvent, StorageError> {
    Ok(IncomingEvent::File(InboundFile {
        filename: filename.to_string(),
        handle: RemoteHandle {
            first_msg_id: msg_id,
            chunk_msg_ids: vec![msg_id],
            total_size: size,
            path: None,
        },
    }))
}

/// Polls the metadata db (bounded: 200 x 10ms) until it holds at least
/// `want` rows; panics with the observed rows otherwise. Polling, not a
/// fixed sleep: on a current-thread runtime the worker only progresses
/// across the sleep await points.
async fn wait_for_rows(db: &MetaDatabase, want: usize) {
    for _ in 0..200 {
        let rows = db.list_all_files().expect("list files");
        if rows.len() >= want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let rows = db.list_all_files().expect("list files");
    panic!(
        "timed out waiting for {want} indexed rows; db holds: {:?}",
        rows.iter().map(|r| r.rel_path.clone()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn inbound_file_indexes_at_root() {
    let (_dir, db, vfs) = test_env().await;

    let rel = vfs
        .index_inbound(InboundFile {
            filename: "photo.jpg".to_string(),
            handle: RemoteHandle {
                first_msg_id: 10,
                chunk_msg_ids: vec![10],
                total_size: 42,
                path: None,
            },
        })
        .await
        .expect("index the inbound file");

    assert_eq!(rel.as_str(), "/photo.jpg");
    let row = db
        .get_file("/photo.jpg")
        .expect("db read")
        .expect("row exists");
    assert_eq!(row.rel_path, "/photo.jpg");
    assert_eq!(row.name, "photo.jpg");
    assert_eq!(row.parent_dir, "/");
    assert_eq!(row.size, 42);
    assert!(row.mtime > 0.0, "mtime is now(), not 0");
    assert_eq!(row.telegram_msg_id, Some(10));
    assert!(row.is_uploaded);
    assert!(!row.is_cached);
    assert!(!row.is_encrypted);
    assert_eq!(row.chunk_count, 1);
    assert_eq!(row.sha256, None);
    // Frozen gap: InboundFile carries no mime field (see module docs).
    assert_eq!(row.mime_type, None);
    vfs.shutdown().await;
}

#[tokio::test]
async fn same_filename_overwrites() {
    let (_dir, db, vfs) = test_env().await;

    vfs.index_inbound(InboundFile {
        filename: "doc.pdf".to_string(),
        handle: RemoteHandle {
            first_msg_id: 11,
            chunk_msg_ids: vec![11],
            total_size: 100,
            path: None,
        },
    })
    .await
    .expect("index first copy");
    vfs.index_inbound(InboundFile {
        filename: "doc.pdf".to_string(),
        handle: RemoteHandle {
            first_msg_id: 12,
            chunk_msg_ids: vec![12],
            total_size: 200,
            path: None,
        },
    })
    .await
    .expect("index second copy");

    let rows = db.list_all_files().expect("list files");
    assert_eq!(rows.len(), 1, "rel_path unique key: one row, overwritten");
    let row = &rows[0];
    assert_eq!(row.size, 200, "second event wins");
    assert_eq!(row.telegram_msg_id, Some(12));
    vfs.shutdown().await;
}

#[tokio::test]
async fn nested_name_forms_nested_path() {
    let (_dir, _db, vfs) = test_env().await;

    let rel = vfs
        .index_inbound(InboundFile {
            filename: "a/b.txt".to_string(),
            handle: RemoteHandle {
                first_msg_id: 13,
                chunk_msg_ids: vec![13],
                total_size: 7,
                path: None,
            },
        })
        .await
        .expect("index the nested-named file");

    // Python baseline equivalence: no sanitization, "/" in the name
    // simply forms a nested virtual path.
    assert_eq!(rel.as_str(), "/a/b.txt");
    vfs.shutdown().await;
}

#[tokio::test]
async fn invalid_name_falls_back() {
    let (_dir, db, vfs) = test_env().await;

    // Empty filename: "/{name}" would degenerate to the root "/".
    let rel = vfs
        .index_inbound(InboundFile {
            filename: String::new(),
            handle: RemoteHandle {
                first_msg_id: 10,
                chunk_msg_ids: vec![10],
                total_size: 5,
                path: None,
            },
        })
        .await
        .expect("index the fallback-named file");
    assert_eq!(rel.as_str(), "/Telegram_File_10.bin");
    assert!(db
        .get_file("/Telegram_File_10.bin")
        .expect("db read")
        .is_some());

    // Backslash: rejected by RelPath (would smuggle a Windows separator
    // into the namespace), so the same fallback applies.
    let rel = vfs
        .index_inbound(InboundFile {
            filename: "a\\b.txt".to_string(),
            handle: RemoteHandle {
                first_msg_id: 14,
                chunk_msg_ids: vec![14],
                total_size: 5,
                path: None,
            },
        })
        .await
        .expect("index the backslash-named file");
    assert_eq!(rel.as_str(), "/Telegram_File_14.bin");
    vfs.shutdown().await;
}

#[tokio::test]
async fn worker_indexes_scripted_events() {
    let (_dir, db, vfs, transport) = worker_env(vec![
        file_event("one.bin", 21, 1),
        file_event("two.bin", 22, 2),
    ])
    .await;

    let handle = spawn_inbound_worker(Arc::clone(&vfs), transport, "Y:".to_string());
    wait_for_rows(&db, 2).await;
    assert!(
        db.get_file("/one.bin").expect("db read").is_some(),
        "the first scripted file landed"
    );
    assert!(
        db.get_file("/two.bin").expect("db read").is_some(),
        "the second scripted file landed"
    );
    handle.shutdown().await;
    vfs.shutdown().await;
}

#[tokio::test]
async fn worker_survives_error_events() {
    let (_dir, db, vfs, transport) = worker_env(vec![
        Err(StorageError::Unavailable("scripted failure".into())),
        file_event("after-error.bin", 31, 9),
    ])
    .await;

    let handle = spawn_inbound_worker(Arc::clone(&vfs), transport, "Y:".to_string());
    // The error must not kill the worker: the file behind it is still
    // indexed once the stream is consumed.
    wait_for_rows(&db, 1).await;
    assert!(db.get_file("/after-error.bin").expect("db read").is_some());
    handle.shutdown().await;
    vfs.shutdown().await;
}

#[tokio::test]
async fn worker_logs_and_skips_commands() {
    let (_dir, db, vfs, transport) = worker_env(vec![
        Ok(IncomingEvent::Command {
            text: "/stats".to_string(),
        }),
        file_event("file.bin", 41, 16),
    ])
    .await;

    let handle = spawn_inbound_worker(Arc::clone(&vfs), transport, "Y:".to_string());
    wait_for_rows(&db, 1).await;
    // The command produced no row: only the file is indexed.
    let rows = db.list_all_files().expect("list files");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].rel_path, "/file.bin");
    handle.shutdown().await;
    vfs.shutdown().await;
}
