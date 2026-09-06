//! RED-phase tests for the db-layer files-table change doorbell (wake
//! chokepoint batch, 2026-09-06): the quasi-realtime sync wake moves from
//! hand-placed `wake_sync()` call sites to a single chokepoint — a rusqlite
//! `update_hook` on the `MetaDatabase` connection that rings the shared
//! sync [`tokio::sync::Notify`] for every `files` table row change.
//!
//! Contract under test:
//!
//! - a write to the `files` table that bypasses every VFS/queue method
//!   (a direct `db.upsert_file`) still rings the doorbell the CLI holds
//!   through `Vfs::sync_notifier()` — the hook makes a forgotten manual
//!   ring structurally impossible;
//! - both row-change shapes ring: a fresh INSERT and the upsert's
//!   conflict-update path (UPDATE);
//! - the suppression guard silences the hook for its lifetime and
//!   restores it on drop — the sync engine's own apply writes use it so
//!   remote applications never trigger a redundant local pass;
//! - writes to the `chunks` table (or any non-`files` table) must NOT
//!   ring: chunk bookkeeping follows an uploaded row's own ring and is
//!   not a sync payload change of its own.
//!
//! Determinism: each test builds its own database (fresh `Notify`), and
//! negative assertions use the documented anti-lost-wakeup pattern —
//! `Notified::enable()` BEFORE the trigger, then a short timeout window.
//! Where earlier writes may have parked a permit in the `Notify` (the
//! hook's `notify_one` stores one when nobody waits), the test drains it
//! first; `enable()` returning `false` proves the drain succeeded.

use std::sync::Arc;
use std::time::Duration;

use cydrive_core::cache::CacheManager;
use cydrive_core::database::{FileUpsert, MetaDatabase};
use cydrive_core::transport::mock::MockTransport;
use cydrive_core::transport::CloudTransport;
use cydrive_core::vfs::{Vfs, VfsConfig};

// ------------------------------------------------------------- helpers ---

/// A VfsConfig with one worker and fast retry (the established pattern
/// of `tests/sync_wake.rs`; the queue itself is idle in these tests).
fn test_cfg() -> VfsConfig {
    VfsConfig::default()
}

/// Real temp environment: SQLite db + cache tree + default mock
/// transport, assembled into a Vfs. The doorbell under test is the one
/// `Vfs::sync_notifier()` hands out — the same handle the CLI's sync
/// task holds, so the tests pin the end-to-end wiring, not a private
/// test seam.
async fn test_vfs() -> (tempfile::TempDir, Vfs, Arc<MetaDatabase>) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let cache = CacheManager::new(dir.path().join("cache"), 1 << 20);
    let mock = Arc::new(MockTransport::builder().build());
    mock.connect().await.expect("pre-connect mock transport");
    let transport: Arc<dyn CloudTransport> = mock;
    let vfs = Vfs::new(Arc::clone(&db), cache, transport, test_cfg());
    (dir, vfs, db)
}

/// A minimal `files` row upsert at `path`.
fn row_upsert(path: &str) -> FileUpsert {
    FileUpsert {
        rel_path: path.to_string(),
        name: path.rsplit('/').next_back().unwrap_or(path).to_string(),
        parent_dir: "/".to_string(),
        size: 10,
        mtime: 1_700_000_000.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: None,
        is_uploaded: false,
        is_cached: false,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: None,
    }
}

// ------------------------------------------------------ positive hooks ---

/// A direct `db.upsert_file` — bypassing every VFS/queue method — rings
/// the doorbell, for both row-change shapes: the fresh INSERT, and the
/// conflict-update path of a second upsert at the same rel_path
/// (UPDATE). This is the chokepoint property itself: any `files` write
/// wakes sync, however it was made.
#[tokio::test]
async fn direct_db_write_rings_the_vfs_doorbell() {
    let (_dir, vfs, db) = test_vfs().await;

    let notifier = vfs.sync_notifier();

    // Fresh INSERT.
    let insert_wake = notifier.notified();
    tokio::pin!(insert_wake);
    insert_wake.as_mut().enable();
    db.upsert_file(&row_upsert("/direct.txt"))
        .expect("direct insert");
    tokio::time::timeout(Duration::from_secs(1), insert_wake.as_mut())
        .await
        .expect("a direct files insert must ring the sync doorbell");

    // Conflict-update (ON CONFLICT DO UPDATE) at the same rel_path.
    let update_wake = notifier.notified();
    tokio::pin!(update_wake);
    update_wake.as_mut().enable();
    db.upsert_file(&row_upsert("/direct.txt"))
        .expect("conflict update");
    tokio::time::timeout(Duration::from_secs(1), update_wake.as_mut())
        .await
        .expect("a direct files conflict-update must ring the sync doorbell");
}

// --------------------------------------------------- suppression guard ---

/// While the suppression guard is alive, `files` writes do not ring;
/// dropping it restores the doorbell. The sync engine's apply path (and
/// the hydrate `is_cached` flips) rely on both halves.
#[tokio::test]
async fn suppression_guard_silences_writes_and_restores() {
    let (_dir, vfs, db) = test_vfs().await;

    let notifier = vfs.sync_notifier();

    let guard = db.suppress_files_hook();

    // Suppressed: the write lands but the doorbell stays silent.
    let suppressed_wake = notifier.notified();
    tokio::pin!(suppressed_wake);
    suppressed_wake.as_mut().enable();
    db.upsert_file(&row_upsert("/quiet.txt"))
        .expect("suppressed write lands");
    assert!(
        tokio::time::timeout(Duration::from_millis(250), suppressed_wake.as_mut())
            .await
            .is_err(),
        "a write under the suppression guard must not ring"
    );

    drop(guard);

    // Restored: the same kind of write rings again.
    let restored_wake = notifier.notified();
    tokio::pin!(restored_wake);
    restored_wake.as_mut().enable();
    db.upsert_file(&row_upsert("/loud.txt"))
        .expect("restored write lands");
    tokio::time::timeout(Duration::from_secs(1), restored_wake.as_mut())
        .await
        .expect("after the guard drops, files writes must ring again");
}

// ------------------------------------------------------ negative hooks ---

/// A `chunks` table write must NOT ring — the doorbell is scoped to
/// `files` row changes; chunk bookkeeping rides along with its row's
/// own change (and is payload-internal anyway).
#[tokio::test]
async fn chunk_writes_do_not_ring() {
    let (_dir, vfs, db) = test_vfs().await;

    let notifier = vfs.sync_notifier();

    // Seed the row the chunk belongs to; a permit may have been parked
    // by the seed's own ring — drain it so the window below is clean.
    let file_id = db
        .upsert_file(&row_upsert("/chunked.bin"))
        .expect("seed files row");
    let drain = notifier.notified();
    tokio::pin!(drain);
    drain.as_mut().enable();
    let _ = tokio::time::timeout(Duration::from_millis(100), drain.as_mut()).await;

    let wake = notifier.notified();
    tokio::pin!(wake);
    assert!(
        !wake.as_mut().enable(),
        "the seed's permit must have been drained before the window opens"
    );

    db.upsert_chunk(file_id, 0, 7, 10, None)
        .expect("chunk row write");
    assert!(
        tokio::time::timeout(Duration::from_millis(250), wake.as_mut())
            .await
            .is_err(),
        "a chunks table write must not ring the sync doorbell"
    );
}
