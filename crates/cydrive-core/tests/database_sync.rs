//! Capability test: `Arc<MetaDatabase>` must be shareable across `tokio`
//! tasks that hold it across `.await` points (the upload-queue worker will).
//!
//! Before the interior-mutability refactor, `MetaDatabase` held a bare
//! `rusqlite::Connection` (Send but not Sync), so `Arc<MetaDatabase>` was
//! not Send and a future holding one failed to compile under
//! `tokio::spawn`. The RED phase of this test was that compile error.

use std::sync::Arc;

use cydrive_core::database::{FileUpsert, MetaDatabase};

#[tokio::test]
async fn arc_database_shares_one_db_across_spawned_tasks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("sync.db")).expect("open"));

    let spawn_worker = |db: Arc<MetaDatabase>, prefix: &'static str| {
        tokio::spawn(async move {
            for i in 0..50i64 {
                let uploaded = i % 2 == 0;
                db.upsert_file(&FileUpsert {
                    rel_path: format!("/{prefix}{i}.txt"),
                    name: format!("{prefix}{i}.txt"),
                    parent_dir: "/".to_string(),
                    size: i,
                    mtime: 1_700_000_000.0 + i as f64,
                    sha256: None,
                    is_dir: false,
                    telegram_msg_id: None,
                    is_uploaded: uploaded,
                    is_cached: !uploaded,
                    is_encrypted: false,
                    chunk_count: 1,
                    mime_type: Some("text/plain".to_string()),
                })
                .expect("upsert");
                // Hold `db` across a yield: the exact pattern the upload
                // worker needs, and it interleaves the two workers for real.
                tokio::task::yield_now().await;
            }
            for i in 0..50i64 {
                let rec = db
                    .get_file(&format!("/{prefix}{i}.txt"))
                    .expect("query")
                    .expect("row");
                assert_eq!(rec.size, i);
                assert_eq!(rec.is_uploaded, i % 2 == 0);
                assert_eq!(rec.parent_dir, "/");
            }
        })
    };

    let a = spawn_worker(db.clone(), "a");
    let b = spawn_worker(db.clone(), "b");
    a.await.expect("worker a panicked");
    b.await.expect("worker b panicked");

    assert_eq!(db.list_all_files().expect("list").len(), 100);
}
