//! materialize_entry = Entry → files 行的**唯一**物化映射（Phase 8 / D4）
//! ——rebuild 与 read-through 共用。本文件钉死三个不变量：
//!
//! 1. 路径形句柄（local/sftp/webdav 形态）→ K6 `0` 占位落库，行经词汇
//!    转行键（"/docs/a.txt"）可读回；
//! 2. 数字形句柄（fs_id/msg_id 形态）直进 msg_id，且落 K11 单容器
//!    chunk 行（index 0、同 msg_id、整 size、无 sha）；
//! 3. 重物化刷新 updated_at（D8③ sweep 的「扫描期再触碰」判据根基），
//!    同 rel_path 冲突键归同一条行。

use cloudkit_core::database::MetaDatabase;
use cloudkit_core::materialize::{list_all_pages, materialize_entry, MaterializedRow};
use cloudkit_storage::{
    BackendHandle, ByteStream, Capabilities, Entry, EntryId, EntryKind, Listing, Page, PageCursor,
    Quota, Range, RelPath, StorageDriver, StorageError, UploadStager, VolumeId, WriteHint,
};

/// 构造一条后端条目（`handle` 形态即被测分叉：路径形 vs 数字形）。
fn entry(path: &str, kind: EntryKind, handle: &str, size: u64) -> Entry {
    Entry {
        id: EntryId::new(
            VolumeId::parse("baidu:123456789").expect("volume id"),
            BackendHandle::new(handle),
        ),
        path: RelPath::new(path).expect("合法卷内相对路径"),
        kind,
        size,
        mtime: 1_700_000_000.0,
    }
}

/// 真 sqlite 的临时库（rebuild 测试同款形态）。
fn db() -> (tempfile::TempDir, MetaDatabase) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open db");
    (dir, db)
}

#[test]
fn path_shaped_handle_degrades_to_the_k6_placeholder_and_reads_back() {
    let (_dir, db) = db();
    let e = entry("docs/a.txt", EntryKind::File, "/dav/docs/a.txt", 5);
    let row: MaterializedRow = materialize_entry(&db, &e).expect("materialize");
    assert_eq!(row.telegram_msg_id, Some(0), "路径形句柄 → K6 0 占位");
    assert!(row.is_uploaded, "权威索引行：字节已在后端");
    assert!(!row.is_encrypted, "明文语义行");
    let stored = db
        .get_file("/docs/a.txt")
        .expect("get_file")
        .expect("行已落库");
    assert_eq!(stored.size, 5, "词汇转行键后可按 \"/docs/a.txt\" 读回");
}

#[test]
fn numeric_handle_lands_as_the_msg_id_with_a_single_container_chunk() {
    let (_dir, db) = db();
    let e = entry("a.bin", EntryKind::File, "12345", 9);
    let row = materialize_entry(&db, &e).expect("materialize");
    assert_eq!(row.telegram_msg_id, Some(12345));
    let chunks = db.get_chunks_by_file_id(row.id).expect("chunks");
    assert_eq!(chunks.len(), 1, "K11 单容器 chunk 行");
    assert_eq!(chunks[0].chunk_index, 0);
    assert_eq!(chunks[0].telegram_msg_id, Some(12345));
    assert_eq!(chunks[0].size, 9);
    assert_eq!(chunks[0].sha256, None, "单容器行不带 sha");
}

#[test]
fn re_materializing_bumps_updated_at_and_keeps_the_coalesce_columns() {
    // sweep 保护（D8③）靠 updated_at 刷新——本测试钉住该不变量。
    let (_dir, db) = db();
    let e = entry("a.bin", EntryKind::File, "1", 1);
    let first = materialize_entry(&db, &e).expect("first materialize");
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = materialize_entry(&db, &e).expect("second materialize");
    assert!(
        second.updated_at.unwrap() > first.updated_at.unwrap(),
        "重物化必须**严格**刷新 updated_at（L1：>= 放过同一毫秒的假刷新）"
    );
    assert!(second.is_uploaded);
    assert_eq!(second.id, first.id, "rel_path 冲突键：同键归同一条行");
}

// ============================== M2（Phase 8 审查批）：分页归集双上限 ===

/// 自指游标的故障驱动（M2 注入点）：`list` 恒回 `per_page` 条目 + 恒有
/// 续读游标——读路径绝不允许被它挂死。
struct CursorLoopDriver {
    volume: VolumeId,
    per_page: usize,
}

impl CursorLoopDriver {
    fn new(per_page: usize) -> Self {
        Self {
            volume: VolumeId::parse("baidu:123456789").expect("volume id"),
            per_page,
        }
    }

    fn make_entry(&self, index: usize) -> Entry {
        Entry {
            id: EntryId::new(self.volume.clone(), BackendHandle::new(index.to_string())),
            path: RelPath::new(&format!("loop/f{index}.txt")).expect("path"),
            kind: EntryKind::File,
            size: 1,
            mtime: 1.0,
        }
    }
}

#[async_trait::async_trait]
impl StorageDriver for CursorLoopDriver {
    fn volume(&self) -> &VolumeId {
        &self.volume
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::none()
    }

    async fn list(&self, _dir: &RelPath, _page: Page) -> Result<Listing, StorageError> {
        Ok(Listing {
            entries: (0..self.per_page).map(|i| self.make_entry(i)).collect(),
            next: Some(PageCursor::Next("loop".to_string())),
        })
    }

    async fn stat(&self, _path: &RelPath) -> Result<Entry, StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn mkdir(&self, _path: &RelPath) -> Result<(), StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn delete(&self, _id: &EntryId) -> Result<(), StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn rename(&self, _from: &RelPath, _to: &RelPath) -> Result<(), StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn reader(
        &self,
        _id: &EntryId,
        _range: Option<Range>,
    ) -> Result<ByteStream, StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn writer(
        &self,
        _path: &RelPath,
        _hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn quota(&self) -> Result<Quota, StorageError> {
        Err(StorageError::Unsupported)
    }
}

// M2 红：恒 Some(next) 的自指游标必须在页数上限处报错返回（绝不挂死
// 读路径 / winfsp FSD 线程），错误消息含目录与计数。
#[tokio::test]
async fn a_self_feeding_cursor_is_capped_by_the_page_limit_instead_of_hanging() {
    let driver = CursorLoopDriver::new(0);
    let dir = RelPath::new("docs").expect("dir");
    let err = list_all_pages(&driver, &dir)
        .await
        .expect_err("the page cap must fire on a self-feeding cursor");
    let message = err.to_string();
    assert!(
        message.contains("docs"),
        "错误消息必须含目录，got: {message}"
    );
    assert!(
        message.contains("1024") || message.contains("page"),
        "错误消息必须点名页数上限，got: {message}"
    );
}

// M2 红：小页大流的自指游标在条目上限处报错（页数上限不误触）。
#[tokio::test]
async fn an_endless_entry_stream_is_capped_by_the_entry_limit() {
    let driver = CursorLoopDriver::new(128);
    let dir = RelPath::new("docs").expect("dir");
    let err = list_all_pages(&driver, &dir)
        .await
        .expect_err("the entry cap must fire on an endless entry stream");
    let message = err.to_string();
    assert!(
        message.contains("docs"),
        "错误消息必须含目录，got: {message}"
    );
    assert!(
        message.contains("65536") || message.contains("entries"),
        "错误消息必须点名条目上限，got: {message}"
    );
}
