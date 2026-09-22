//! RED-phase tests for `cloudkit_core::rebuild` (Phase 2 / K11,
//! docs/plans/2026-09-08-phase2-execution.md §6): the authoritative
//! backend bootstrap of the metadata DB.
//!
//! Contract under test:
//!
//! - `rebuild_from_backend(driver, db, root)`: recursive `list` from
//!   `root` → one `files` row per backend entry (upsert keyed on the
//!   vpath-shaped `rel_path`), with the authoritative-index row shape:
//!   `is_uploaded = 1` (the bytes exist in the backend — nothing is
//!   pending), `chunk_count = 1` (whole-file view: the driver's
//!   chunking is invisible above L2), `telegram_msg_id = fs_id`-shaped
//!   handle parsed as i64 (path-shaped local handles degrade to the K6
//!   `0` placeholder), `mtime = Entry.mtime`. Directory rows follow the
//!   `create_dir` parity (size 0, `chunk_count = 0`, `msg_id = NULL`,
//!   `is_cached = 1`). `sha256`/`mime_type` stay `NULL` (coalescing
//!   keeps any stored value on re-rebuild). Every file row also writes
//!   one single-container `chunks` row (index 0, the row's msg_id, the
//!   whole size, no sha — K11 bookkeeping parity with an upload
//!   persist's one-element receipt); directory rows write none.
//! - Plaintext-only semantics (K11): an instance with
//!   `enable_encryption = true` is refused up front with an actionable
//!   error pointing at `cydrive sync` — the backend only sees
//!   ciphertext containers under plaintext names, so a rebuilt row
//!   would mislabel encrypted payloads as plaintext. The gate is a
//!   pure config check (`ensure_plaintext_instance`), the CLI layer
//!   calls it before anything else.
//! - Empty backend root: `Ok` with zero rows (a fresh app dir is a
//!   legitimate state, not an error).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use cloudkit_core::config::CyDriveConfig;
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::rebuild::{
    ensure_plaintext_instance, rebuild_from_backend, rebuild_from_backend_with, RebuildInterrupted,
    RebuildLimits, RebuildOutcome,
};
use cloudkit_storage::{
    BackendHandle, ByteStream, Capabilities, Entry, EntryId, Listing, MockStorageDriver, Page,
    Quota, Range, RelPath, StorageDriver, StorageError, UploadStager, VolumeId, WriteHint,
};

// ------------------------------------------------------------- helpers ---

/// A mock driver over a scratch volume, plus the handle of its writer
/// seeding helper's last Entry (assertions key on the handle digits).
fn seeded_driver() -> MockStorageDriver {
    MockStorageDriver::new(VolumeId::parse("baidu:123456789").expect("volume id"))
}

/// Seeds one backend file of `data` at the volume-relative `path`
/// (writer + write + close; parents auto-created by the mock).
async fn seed_file(driver: &MockStorageDriver, path: &str, data: &[u8]) {
    let rel = RelPath::new(path).expect("seed path");
    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..Default::default()
    };
    let mut stager = driver.writer(&rel, &hint).await.expect("seed writer");
    stager.write(data).await.expect("seed write");
    stager.close().await.expect("seed close");
}

/// The entry id (numeric mock handle) the backend assigned to `path`.
async fn handle_of(driver: &MockStorageDriver, path: &str) -> i64 {
    driver
        .stat(&RelPath::new(path).expect("stat path"))
        .await
        .expect("seeded stat")
        .id
        .handle
        .as_str()
        .parse()
        .expect("mock handles are numeric")
}

// -------------------------------------------------------------- tests ---

#[tokio::test]
async fn rebuild_walks_the_backend_tree_into_db_rows() {
    let driver = seeded_driver();
    // A tree: two top-level files, one directory with a nested file
    // (the writer auto-creates the parent dir in the mock backend).
    seed_file(&driver, "hello.txt", b"hello").await;
    seed_file(&driver, "big.bin", &[7u8; 100]).await;
    seed_file(&driver, "docs/readme.md", b"# doc").await;
    let hello_id = handle_of(&driver, "hello.txt").await;
    let big_id = handle_of(&driver, "big.bin").await;
    let readme_id = handle_of(&driver, "docs/readme.md").await;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open db");
    let outcome = rebuild_from_backend(&driver, &db, &RelPath::root())
        .await
        .expect("rebuild walks the tree");
    assert_eq!(
        outcome,
        RebuildOutcome {
            files: 3,
            dirs: 1,
            ..Default::default()
        },
        "three files and the docs/ directory"
    );

    // File rows: authoritative-index shape.
    let hello = db.get_file("/hello.txt").expect("read hello").expect("row");
    assert_eq!(hello.size, 5);
    assert!(hello.is_uploaded, "backend bytes exist → is_uploaded=1");
    assert_eq!(hello.chunk_count, 1, "whole-file view above L2");
    assert_eq!(hello.telegram_msg_id, Some(hello_id), "msg_id = handle");
    assert!(!hello.is_encrypted);
    assert!(hello.mtime > 0.0, "mtime from the Entry");
    assert_eq!(hello.name, "hello.txt");
    assert_eq!(hello.parent_dir, "/");

    // Chunks rows (K11 single-container parity): index 0, the row's
    // msg_id, the whole size, no sha — the upload-persist shape for a
    // one-element receipt.
    let hello_chunks = db
        .get_chunks_by_file_id(hello.id)
        .expect("read hello chunks");
    assert_eq!(hello_chunks.len(), 1, "one single-container chunk row");
    assert_eq!(hello_chunks[0].chunk_index, 0);
    assert_eq!(hello_chunks[0].telegram_msg_id, Some(hello_id));
    assert_eq!(hello_chunks[0].size, 5);
    assert_eq!(hello_chunks[0].sha256, None);

    let big = db.get_file("/big.bin").expect("read big").expect("row");
    assert_eq!(big.size, 100);
    assert_eq!(big.telegram_msg_id, Some(big_id));

    // Nested file carries the directory parent_dir.
    let readme = db
        .get_file("/docs/readme.md")
        .expect("read readme")
        .expect("row");
    assert_eq!(readme.size, 5, "b\"# doc\" is five bytes");
    assert_eq!(readme.telegram_msg_id, Some(readme_id));
    assert_eq!(readme.parent_dir, "/docs");

    // Directory row: create_dir parity (0-size, 0 chunks, NULL msg_id)
    // — and no chunks rows.
    let docs = db.get_file("/docs").expect("read docs").expect("row");
    assert!(docs.is_dir);
    assert_eq!(docs.size, 0);
    assert_eq!(docs.chunk_count, 0);
    assert_eq!(docs.telegram_msg_id, None);
    assert!(docs.is_uploaded);
    assert!(
        db.get_chunks_by_file_id(docs.id)
            .expect("read docs chunks")
            .is_empty(),
        "directories carry no chunks rows"
    );
}

#[tokio::test]
async fn rebuild_upserts_over_stale_rows_and_reports_counts() {
    let driver = seeded_driver();
    seed_file(&driver, "a.txt", b"aaa").await;
    let a_id = handle_of(&driver, "a.txt").await;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open db");
    // A stale pending row (crash-staged upload that never drained) and
    // a ghost row for a file the backend no longer has — the rebuild
    // refreshes the first; the second is the completion sweep's target
    // (Phase 8 / D8③: the K11 no-pruning scope ended — a COMPLETING
    // pass prunes uploaded rows the scan never re-touched, while the
    // pending row is sweep-exempt by is_uploaded=0).
    let stale_id = db
        .upsert_file(&cloudkit_core::database::FileUpsert {
            rel_path: "/a.txt".to_string(),
            name: "a.txt".to_string(),
            parent_dir: "/".to_string(),
            size: 3,
            mtime: 1.0,
            sha256: Some("stale-hash".to_string()),
            is_dir: false,
            telegram_msg_id: Some(999),
            is_uploaded: false,
            is_cached: true,
            is_encrypted: false,
            chunk_count: 1,
            mime_type: None,
        })
        .expect("seed stale row");
    // A stale chunk row rides the stale files row (msg_id 999) — the
    // refresh must rewrite it to the backend's handle.
    db.upsert_chunk(stale_id, 0, 999, 3, None)
        .expect("seed stale chunk");
    db.upsert_file(&cloudkit_core::database::FileUpsert {
        rel_path: "/ghost.txt".to_string(),
        name: "ghost.txt".to_string(),
        parent_dir: "/".to_string(),
        size: 1,
        mtime: 1.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: Some(555),
        is_uploaded: true,
        is_cached: false,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: None,
    })
    .expect("seed ghost row");

    let outcome = rebuild_from_backend(&driver, &db, &RelPath::root())
        .await
        .expect("rebuild");
    assert_eq!(outcome.files, 1, "one backend file");

    // The stale row is refreshed to the backend truth: uploaded, the
    // backend's handle — while the stored sha256 survives (coalesce).
    let a = db.get_file("/a.txt").expect("read a").expect("row");
    assert!(a.is_uploaded, "pending row refreshed to uploaded");
    assert_eq!(a.telegram_msg_id, Some(a_id), "msg_id refreshed");
    assert_eq!(
        a.sha256.as_deref(),
        Some("stale-hash"),
        "coalesce keeps sha"
    );
    // The stale chunk row is refreshed too (same upsert semantics).
    let a_chunks = db.get_chunks_by_file_id(a.id).expect("read a chunks");
    assert_eq!(a_chunks.len(), 1);
    assert_eq!(a_chunks[0].chunk_index, 0);
    assert_eq!(
        a_chunks[0].telegram_msg_id,
        Some(a_id),
        "stale chunk msg_id refreshed to the backend handle"
    );
    assert_eq!(a_chunks[0].size, 3);

    // Ghost row pruned by the completion sweep (D8③): uploaded,
    // pre-scan (updated_at < the scan's anchor), absent from the
    // backend — exactly the "seen by no pass of this scan" shape.
    assert!(
        db.get_file("/ghost.txt").expect("read ghost").is_none(),
        "the ghost row is pruned by the completing pass"
    );
}

#[tokio::test]
async fn rebuild_of_an_empty_backend_is_ok_with_zero_rows() {
    let driver = seeded_driver();
    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open db");
    let outcome = rebuild_from_backend(&driver, &db, &RelPath::root())
        .await
        .expect("empty backend is a legitimate state");
    assert_eq!(
        outcome,
        RebuildOutcome {
            files: 0,
            dirs: 0,
            ..Default::default()
        }
    );
}

#[test]
fn encrypted_instances_are_refused_with_sync_guidance() {
    // A mock config with encryption on: the pure gate refuses with an
    // actionable message pointing at cydrive sync (K11 plaintext-only).
    let encrypted = CyDriveConfig {
        enable_encryption: true,
        encryption_password: Some("pw".to_string()),
        ..CyDriveConfig::default()
    };
    let err =
        ensure_plaintext_instance(&encrypted).expect_err("encrypted instance must be refused");
    let message = err.to_string();
    assert!(
        message.contains("sync"),
        "the refusal must point at cydrive sync, got: {message}"
    );
    assert!(
        message.contains("encrypt"),
        "the refusal must name encryption as the reason, got: {message}"
    );

    // Plaintext instances (the default) pass the gate.
    ensure_plaintext_instance(&CyDriveConfig::default())
        .expect("plaintext instance passes the gate");
}

// ============================================================== RT4 ===
// Phase 8 / D8 三件套：迭代工作队列 + `rebuild_state` 持久化续跑 +
// `max_entries` 总上限 + 完成趟 sweep prune（三重保护）。红测试先于实现
// 写就；断言口径对齐计划 Task 4 与验收 A4/A5。

// ------------------------------------------------- RT4 harness 裥料 ---

/// list 调用计数的薄包装驱动（形态照 `readthrough.rs` 的
/// `CountingDriver`——mock 本体无 list 计数，「续跑不重扫已完成目录」
/// 的断言（A4）全靠它；per-dir 计数与页间停顿服务 M7 的 DeadlineStop
/// 断言）。
struct CountingDriver {
    inner: Arc<MockStorageDriver>,
    list_calls: AtomicUsize,
    /// per-dir list 计数（M7：「该目录被重列」的断言面）。
    per_dir: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    /// 每次 list 前的确定性停顿（M7：真实网络的页间耗时形态在此注入，
    /// 让墙钟预算恰在页间耗尽）。
    page_delay: Duration,
}

impl CountingDriver {
    fn new(inner: Arc<MockStorageDriver>) -> Self {
        Self {
            inner,
            list_calls: AtomicUsize::new(0),
            per_dir: std::sync::Mutex::new(std::collections::HashMap::new()),
            page_delay: Duration::ZERO,
        }
    }

    fn list_calls(&self) -> usize {
        self.list_calls.load(Ordering::SeqCst)
    }

    /// 某目录（卷相对）的累计 list 次数。
    fn lists_of(&self, dir: &str) -> usize {
        self.per_dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(dir)
            .copied()
            .unwrap_or(0)
    }
}

#[async_trait]
impl StorageDriver for CountingDriver {
    fn volume(&self) -> &VolumeId {
        self.inner.volume()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn list(&self, dir: &RelPath, page: Page) -> Result<Listing, StorageError> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        *self
            .per_dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(dir.as_str().to_string())
            .or_insert(0) += 1;
        if !self.page_delay.is_zero() {
            tokio::time::sleep(self.page_delay).await;
        }
        self.inner.list(dir, page).await
    }

    async fn stat(&self, path: &RelPath) -> Result<Entry, StorageError> {
        self.inner.stat(path).await
    }

    async fn mkdir(&self, path: &RelPath) -> Result<(), StorageError> {
        self.inner.mkdir(path).await
    }

    async fn delete(&self, id: &EntryId) -> Result<(), StorageError> {
        self.inner.delete(id).await
    }

    async fn rename(&self, from: &RelPath, to: &RelPath) -> Result<(), StorageError> {
        self.inner.rename(from, to).await
    }

    async fn reader(&self, id: &EntryId, range: Option<Range>) -> Result<ByteStream, StorageError> {
        self.inner.reader(id, range).await
    }

    async fn writer(
        &self,
        path: &RelPath,
        hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        self.inner.writer(path, hint).await
    }

    async fn quota(&self) -> Result<Quota, StorageError> {
        self.inner.quota().await
    }
}

// `CountingDriver` 的 BackendHandle 进口仅服务 trait 签名的可读性；钉
// 一处使用避免 unused 导入（reader 不展开句柄构造）。
#[allow(dead_code)]
fn _handle_type_witness(_: BackendHandle) {}

/// A deterministic multi-directory tree: 10 root files plus five
/// directories `d1..d5` with 10 files each — 6 listable directories,
/// 65 entries, 60 file rows.
async fn seed_wide_tree(driver: &MockStorageDriver) {
    for i in 0..10 {
        seed_file(driver, &format!("f{i}.txt"), b"x").await;
    }
    for d in 1..=5 {
        for i in 0..10 {
            seed_file(driver, &format!("d{d}/f{i}.txt"), b"x").await;
        }
    }
}

/// Seeds one pre-scan UPLOADED row (`is_uploaded = 1`) with its K11
/// single-container chunk — the shape a completed rebuild or an upload
/// persist leaves behind. `updated_at` lands before any later scan's
/// start time, i.e. the exact "seen by a previous scan?" candidate the
/// completion sweep judges.
fn seed_uploaded_row(db: &MetaDatabase, path: &str, msg_id: i64) -> i64 {
    let (parent, name) = match path.rfind('/') {
        Some(0) => ("/", &path[1..]),
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("/", path),
    };
    let id = db
        .upsert_file(&cloudkit_core::database::FileUpsert {
            rel_path: path.to_string(),
            name: name.to_string(),
            parent_dir: parent.to_string(),
            size: 1,
            mtime: 1.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: Some(msg_id),
            is_uploaded: true,
            is_cached: false,
            is_encrypted: false,
            chunk_count: 1,
            mime_type: None,
        })
        .expect("seed uploaded row");
    db.upsert_chunk(id, 0, msg_id, 1, None).expect("seed chunk");
    id
}

/// A pre-scan PENDING row (`is_uploaded = 0`) — the in-flight shape the
/// sweep must never touch (D8③ protection d / D10).
fn seed_pending_row(db: &MetaDatabase, path: &str) {
    let (parent, name) = match path.rfind('/') {
        Some(0) => ("/", &path[1..]),
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("/", path),
    };
    db.upsert_file(&cloudkit_core::database::FileUpsert {
        rel_path: path.to_string(),
        name: name.to_string(),
        parent_dir: parent.to_string(),
        size: 1,
        mtime: 1.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: None,
        is_uploaded: false,
        is_cached: true,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: None,
    })
    .expect("seed pending row");
}

/// 1. 续跑（A4）：max_entries=50 的首趟在完成 root+d1..d4（55 条 ≥ 50）
///    后优雅停，d5 留在持久化队列；二趟从游标继续恰补 1 次 list——
///    总 list 次数 = 可列目录数（6），无任何已完成目录被重扫。
#[tokio::test]
async fn an_interrupted_pass_resumes_without_relisting_completed_directories() {
    let mock = Arc::new(seeded_driver());
    seed_wide_tree(&mock).await;
    let driver = CountingDriver::new(Arc::clone(&mock));

    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open db");
    let limits = RebuildLimits {
        max_entries: 50,
        time_budget: None,
    };

    // Pass 1: root (15 entries: 10 files + 5 dir rows) then d1..d4 (40)
    // → 55 ≥ 50 → stops with d5 still pending.
    let first = rebuild_from_backend_with(&driver, &db, &RelPath::root(), limits)
        .await
        .expect("first pass");
    assert_eq!(
        first.interrupted,
        Some(RebuildInterrupted::EntriesBudget),
        "the pass stops gracefully on the entry cap"
    );
    assert_eq!((first.files, first.dirs), (50, 5), "root + d1..d4 walked");
    assert_eq!(
        driver.list_calls(),
        5,
        "root and d1..d4 listed; d5 still pending"
    );
    // The checkpoint survives on disk (D8①).
    assert!(
        db.rebuild_state_get("scan_started_at")
            .expect("kv read")
            .is_some(),
        "the scan anchor is persisted by the first pass"
    );
    assert!(
        db.rebuild_state_get("pending").expect("kv read").is_some(),
        "the remaining queue is persisted"
    );

    // Pass 2: resumes from the persisted cursor — exactly ONE new list.
    let second = rebuild_from_backend_with(&driver, &db, &RelPath::root(), limits)
        .await
        .expect("second pass");
    assert_eq!(
        second.interrupted, None,
        "the resumed pass drains the queue and completes"
    );
    assert_eq!(
        (second.files, second.dirs),
        (10, 0),
        "only d5's files materialize in the resumed pass"
    );
    assert_eq!(
        driver.list_calls(),
        6,
        "total lists = listable directories: no completed directory re-listed"
    );
    // The full tree is indexed exactly once.
    for d in 1..=5 {
        for i in 0..10 {
            assert!(
                db.get_file(&format!("/d{d}/f{i}.txt"))
                    .expect("read row")
                    .is_some(),
                "d{d}/f{i}.txt must be indexed"
            );
        }
    }
    assert!(db.get_file("/f0.txt").expect("read row").is_some());
    // Completion cleared the checkpoint (all three keys).
    for key in ["pending", "scan_started_at", "entries_done"] {
        assert!(
            db.rebuild_state_get(key).expect("kv read").is_none(),
            "key {key:?} must be cleared by the completing pass"
        );
    }
}

/// 2. 总上限（A4）：中断态的 outcome 明示「entries budget ran out;
///    rerun to continue」；未完成趟绝不 sweep——预先播种的 stale 行
///    （含 chunks）原样幸存，检查点三键仍在。
#[tokio::test]
async fn an_entry_budget_stop_reports_rerun_and_never_sweeps() {
    let mock = seeded_driver();
    // Backend: a.txt + d1/b.txt — a pass capped at 1 entry finishes root
    // (2 entries ≥ 1) and stops with d1 pending.
    seed_file(&mock, "a.txt", b"a").await;
    seed_file(&mock, "d1/b.txt", b"b").await;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open db");
    // A pre-scan uploaded row for a path the backend no longer carries:
    // a COMPLETING pass would prune it — the interrupted pass must leave
    // it (and its chunks) untouched.
    let ghost = seed_uploaded_row(&db, "/gone.txt", 555);
    let limits = RebuildLimits {
        max_entries: 1,
        time_budget: None,
    };

    let first = rebuild_from_backend_with(&mock, &db, &RelPath::root(), limits)
        .await
        .expect("first pass");
    assert_eq!(first.interrupted, Some(RebuildInterrupted::EntriesBudget));
    let reason = first.interrupted.unwrap().to_string();
    assert!(
        reason.contains("entries budget ran out"),
        "the interruption must name its budget, got: {reason}"
    );
    assert!(
        reason.contains("rerun to continue"),
        "the interruption must carry the rerun semantics, got: {reason}"
    );
    // The checkpoint is on disk for the rerun.
    assert!(
        db.rebuild_state_get("pending").expect("kv read").is_some(),
        "d1 stays queued"
    );
    assert!(
        db.rebuild_state_get("scan_started_at")
            .expect("kv read")
            .is_some(),
        "the anchor is kept for the rerun"
    );
    // An incomplete pass NEVER sweeps.
    assert!(
        db.get_file("/gone.txt").expect("read row").is_some(),
        "the stale row survives an interrupted pass"
    );
    assert_eq!(
        db.get_chunks_by_file_id(ghost).expect("read chunks").len(),
        1,
        "the stale row's chunk survives too"
    );
}

/// 3. sweep 三重保护（A5）：完成趟只删「扫描期间未再触碰且
///    is_uploaded=1」的行——(a) 扫描前存在且远端已删 → 行删 +
///    chunks 级联清空；(b) 本趟物化的行 → 保留；(d) in-flight 行
///    （is_uploaded=0）→ 保留。
#[tokio::test]
async fn the_completion_sweep_prunes_unseen_rows_and_honors_the_protections() {
    let mock = seeded_driver();
    seed_file(&mock, "a.txt", b"a").await;
    seed_file(&mock, "d1/x.txt", b"x").await;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open db");
    // (a) pre-scan uploaded row, remote-deleted → the sweep's target.
    let gone = seed_uploaded_row(&db, "/gone.txt", 555);
    // (d) pre-scan in-flight row → sweep-exempt by is_uploaded=0.
    seed_pending_row(&db, "/pending.bin");

    let outcome = rebuild_from_backend(&mock, &db, &RelPath::root())
        .await
        .expect("completing pass");
    assert_eq!(outcome.interrupted, None, "the pass completes");

    // (a) pruned, with its chunks cascaded away.
    assert!(
        db.get_file("/gone.txt").expect("read row").is_none(),
        "the remote-deleted row is pruned by the completion sweep"
    );
    assert!(
        db.get_chunks_by_file_id(gone)
            .expect("read chunks")
            .is_empty(),
        "chunks cascade with the swept row"
    );
    assert!(
        outcome.pruned >= 1,
        "the outcome reports the sweep's deletion count, got {}",
        outcome.pruned
    );

    // (b) rows upserted by this very pass survive.
    let a = db.get_file("/a.txt").expect("read row").expect("row");
    assert!(a.is_uploaded, "the freshly materialized row is intact");
    assert!(
        db.get_file("/d1/x.txt").expect("read row").is_some(),
        "nested fresh rows survive"
    );

    // (d) the in-flight row survives.
    let pending = db.get_file("/pending.bin").expect("read row").expect("row");
    assert!(
        !pending.is_uploaded,
        "the in-flight row is untouched by the sweep"
    );

    // Completion cleared the checkpoint.
    assert!(
        db.rebuild_state_get("scan_started_at")
            .expect("kv read")
            .is_none(),
        "the anchor is cleared once the sweep ran"
    );
}

/// 3c. 跨续跑早趟物化的行 → 保留（scan_started_at 从首趟持久，绝不在
///     续跑时重置——本条即防回归：若续跑重置锚点，早趟行的 updated_at
///     会落在「未见」侧而被误删）。
#[tokio::test]
async fn early_pass_rows_survive_the_completion_sweep_after_a_resume() {
    let mock = Arc::new(seeded_driver());
    // Initial backend: one root file + d1 with 5 files. Root's listing
    // carries exactly 2 entries (f0 + the d1 dir row), so the 2-entry
    // cap stops pass 1 right after root with d1 still pending.
    seed_file(&mock, "f0.txt", b"0").await;
    for i in 0..5 {
        seed_file(&mock, &format!("d1/f{i}.txt"), b"x").await;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open db");
    let counting = CountingDriver::new(Arc::clone(&mock));
    let first = rebuild_from_backend_with(
        &counting,
        &db,
        &RelPath::root(),
        RebuildLimits {
            max_entries: 2,
            time_budget: None,
        },
    )
    .await
    .expect("first pass");
    assert_eq!(first.interrupted, Some(RebuildInterrupted::EntriesBudget));
    // The early-pass rows exist now (updated_at = pass-1 time).
    assert!(db.get_file("/f0.txt").expect("read row").is_some());
    assert!(db.get_file("/d1").expect("read row").is_some());

    // The remote grows mid-scan: d1 gains a subdirectory.
    seed_file(&mock, "d1/sub/deep.txt", b"d").await;

    // Pass 2 gets a fresh (larger) budget — the rerun shape — and
    // completes, which runs the sweep against pass 1's persisted anchor.
    let second = rebuild_from_backend_with(
        &counting,
        &db,
        &RelPath::root(),
        RebuildLimits {
            max_entries: 100,
            time_budget: None,
        },
    )
    .await
    .expect("second pass");
    assert_eq!(second.interrupted, None, "the resumed pass completes");
    assert!(
        db.get_file("/f0.txt").expect("read row").is_some(),
        "the early-pass file row survives the sweep"
    );
    assert!(
        db.get_file("/d1").expect("read row").is_some(),
        "the early-pass directory row survives the sweep"
    );
    assert!(
        db.get_file("/d1/sub/deep.txt").expect("read row").is_some(),
        "the mid-scan remote addition is indexed"
    );
    assert!(
        db.rebuild_state_get("scan_started_at")
            .expect("kv read")
            .is_none(),
        "the completed scan clears its anchor"
    );
}

/// 4. 未完成趟（时间预算中断）→ 绝不 sweep：零预算的趟在第一个目录
///    边界即停（确定性，无需睡眠），队列照播、stale 行带 chunks 幸存。
#[tokio::test]
async fn a_time_budget_stop_leaves_everything_and_never_sweeps() {
    let mock = seeded_driver();
    seed_file(&mock, "a.txt", b"a").await;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open db");
    let ghost = seed_uploaded_row(&db, "/gone.txt", 555);

    // A zero wall-clock budget: the deadline is already elapsed at the
    // first directory boundary — deterministic, no sleeping.
    let outcome = rebuild_from_backend_with(
        &mock,
        &db,
        &RelPath::root(),
        RebuildLimits {
            max_entries: usize::MAX,
            time_budget: Some(Duration::ZERO),
        },
    )
    .await
    .expect("the pass answers");
    assert_eq!(outcome.interrupted, Some(RebuildInterrupted::TimeBudget));
    assert_eq!(
        (outcome.files, outcome.dirs),
        (0, 0),
        "nothing was processed"
    );
    let reason = outcome.interrupted.unwrap().to_string();
    assert!(
        reason.contains("time budget ran out"),
        "the interruption must name its budget, got: {reason}"
    );
    assert!(
        reason.contains("rerun to continue"),
        "the interruption must carry the rerun semantics, got: {reason}"
    );
    // The seeded queue is the checkpoint; nothing was swept.
    assert!(
        db.rebuild_state_get("pending").expect("kv read").is_some(),
        "the queue stays persisted for the rerun"
    );
    assert!(
        db.get_file("/gone.txt").expect("read row").is_some(),
        "an incomplete pass never sweeps"
    );
    assert_eq!(
        db.get_chunks_by_file_id(ghost).expect("read chunks").len(),
        1,
        "the stale row's chunk survives"
    );
}

/// 5（M3 体量地板）：完成趟 sweep 的「静默空列表」防线——既有 uploaded
/// 行 > 100 且待删候选超过其半时，放弃**整个** sweep（0 行删除，含
/// chunks 级联），完成趟检查点照常清；幸存行由下一次完成趟重新裁决。
#[tokio::test]
async fn the_completion_sweep_abandons_a_majority_prune_on_a_sizable_index() {
    let mock = seeded_driver(); // 全空后端——「驱动谎报空树」的故障形态
    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open db");
    let mut ids = Vec::new();
    for i in 0..150 {
        ids.push(seed_uploaded_row(
            &db,
            &format!("/old{i}.txt"),
            3000 + i as i64,
        ));
    }

    let outcome = rebuild_from_backend(&mock, &db, &RelPath::root())
        .await
        .expect("the completing pass answers");
    assert_eq!(outcome.interrupted, None, "the pass completes");
    assert_eq!(
        outcome.pruned, 0,
        "M3：候选(150) 超基数(150)之半 → 整个 sweep 放弃，零删除"
    );
    for (i, id) in ids.iter().enumerate() {
        assert!(
            db.get_file(&format!("/old{i}.txt"))
                .expect("read row")
                .is_some(),
            "M3：行全保留（old{i}.txt）"
        );
        assert_eq!(
            db.get_chunks_by_file_id(*id).expect("read chunks").len(),
            1,
            "M3：chunks 级联同样不发生（old{i}.txt）"
        );
    }
    assert!(
        db.rebuild_state_get("scan_started_at")
            .expect("kv read")
            .is_none(),
        "完成趟检查点照常清（放弃 sweep 不改变趟的完成语义）"
    );
}

/// 6（M7）：DeadlineStop 臂——多页目录（600 条目跨 2 页，页限 512）+
/// 页间耗尽的微预算 → 页间中断（`interrupted = TimeBudget`，目录回持
/// 久化队列）；rerun（预算放宽）**从头重列该目录**（页界语义：半列的
/// 目录不算完成——首趟恰 1 页，二趟重列恰 2 页，per-dir 计数 1+2），
/// 子树行最终齐全、完成趟照常收口。
#[tokio::test]
async fn a_mid_directory_deadline_stop_requeues_and_the_rerun_relists_the_directory() {
    let mock = Arc::new(seeded_driver());
    // big/：600 个文件 → 2 页（512 + 88）；根只含 big 目录行。
    for i in 0..600 {
        seed_file(&mock, &format!("big/f{i:03}.txt"), b"x").await;
    }
    // 页间停顿 60ms：预算 100ms 让首趟恰在 big 的第 1、2 页之间耗尽
    // （根页检查 ~0ms < 100 ✓；big 页1检查 ~60ms < 100 ✓；页2检查
    // ~120ms ≥ 100 → DeadlineStop）。两趟共用同一计数器（M7 断言面 =
    // 该目录的跨趟累计 list 次数）。
    let mut counting = CountingDriver::new(Arc::clone(&mock));
    counting.page_delay = Duration::from_millis(60);

    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open db");

    // Pass 1：root 完成后 pop big，页间耗尽 → TimeBudget 中断。
    let first = rebuild_from_backend_with(
        &counting,
        &db,
        &RelPath::root(),
        RebuildLimits {
            max_entries: usize::MAX,
            time_budget: Some(Duration::from_millis(100)),
        },
    )
    .await
    .expect("first pass");
    assert_eq!(
        first.interrupted,
        Some(RebuildInterrupted::TimeBudget),
        "预算必须在 big 的页间耗尽"
    );
    assert_eq!((first.files, first.dirs), (512, 1), "根 + big 页1 已物化");
    assert_eq!(counting.lists_of(""), 1, "根恰列 1 次");
    assert_eq!(
        counting.lists_of("big"),
        1,
        "首趟 big 恰列 1 页（半列不算完成）"
    );
    // 检查点仍是「big 未列完」的形态：队列里留着 big（持久化语义）。
    assert!(
        db.rebuild_state_get("pending")
            .expect("kv read")
            .is_some_and(|raw| raw.contains("big")),
        "the interrupted directory stays persisted on the queue"
    );

    // Pass 2：预算放宽 → big 从头重列（2 页）→ 完成趟收口。
    let second = rebuild_from_backend_with(
        &counting,
        &db,
        &RelPath::root(),
        RebuildLimits {
            max_entries: usize::MAX,
            time_budget: None,
        },
    )
    .await
    .expect("second pass");
    assert_eq!(second.interrupted, None, "续跑完成");
    assert_eq!(
        (second.files, second.dirs),
        (600, 0),
        "重列物化全部 600 条（前 512 幂等重 upsert）"
    );
    assert_eq!(
        counting.lists_of("big"),
        3,
        "M7 核心：该目录被重列——首趟 1 页 + 二趟从头 2 页（页界重列语义）"
    );
    assert_eq!(
        db.get_stats().expect("stats").total_files,
        600,
        "子树行最终齐全"
    );
    for probe in [
        "big/f000.txt",
        "big/f511.txt",
        "big/f512.txt",
        "big/f599.txt",
    ] {
        assert!(
            db.get_file(&format!("/{probe}"))
                .expect("read row")
                .is_some(),
            "{probe} must be indexed"
        );
    }
    assert!(
        db.rebuild_state_get("scan_started_at")
            .expect("kv read")
            .is_none(),
        "完成趟检查点照常清"
    );
}
