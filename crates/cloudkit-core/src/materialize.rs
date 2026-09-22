//! Entry → files 行的**唯一**物化映射（Phase 8 / D4）。
//!
//! 平移自 `rebuild.rs::upsert_entry`（K6 句柄降级 / K11 单容器 chunk
//! 形态原样保留）；rebuild 与 read-through 共用——映射语义只许在这一处
//! 维护。行形状契约（`is_uploaded = 1`、`chunk_count = 1`、目录行
//! `create_dir` 同形、`sha256`/`mime_type` 留 `NULL` 走 coalesce）见
//! [`crate::rebuild`] 模块文档，两消费方逐字同一契约。

use cloudkit_storage::{Entry, EntryKind, Page, PageCursor, RelPath, StorageDriver, StorageError};

use crate::database::{DbError, FileRecord, FileUpsert, MetaDatabase};
use crate::rebuild::RebuildError;

/// `list_all_pages` 的页数上限（M2 / Phase 8 审查批）：自指游标的故障
/// 驱动（cursor 恒 `Some(next)`）不得把读路径挂死（winfsp 面 = FSD 线
/// 程挂死）。1024 页 × 每页 512 = 52.4 万条目，远超任何合法单目录的真
/// 实规模——超限只会是驱动故障，归 `Unavailable` 带上下文上抛。
pub const LIST_MAX_PAGES: usize = 1024;

/// `list_all_pages` 的累计条目上限（M2 同款防线）：65 536 = 2^16，按
/// 512/页即 128 页——先于页数上限触发的真值闸（小页大流形态）；百万级
/// 全树遍历是 rebuild 的活（有 max_entries 闸），单目录读穿绝不走量。
pub const LIST_MAX_ENTRIES: usize = 65_536;

/// 物化产物（调用方拿行做后续判定，如 read-through 的 TTL 标记）。
pub type MaterializedRow = FileRecord;

/// Upserts one backend entry as a `files` row and returns the surviving
/// row (see the [`crate::rebuild`] module doc for the row-shape contract;
/// K6/K11 shapes live here from now on).
pub fn materialize_entry(
    db: &MetaDatabase,
    entry: &Entry,
) -> Result<MaterializedRow, RebuildError> {
    // vocab (volume-relative, "docs/readme.md") → vpath row key
    // ("/docs/readme.md"): the files table's rel_path convention every
    // other writer uses.
    let rel_path = format!("/{}", entry.path.as_str());
    let name = entry.path.file_name().unwrap_or_default().to_string();
    let parent_dir = match entry.path.parent() {
        Some(parent) => format!("/{}", parent.as_str()),
        None => "/".to_string(),
    };
    match entry.kind {
        EntryKind::File => {
            // msg_id = the handle: fs_id-shaped handles (baidu, mock)
            // parse directly; path-shaped local handles cannot occupy
            // the i64 column and degrade to the K6 0 placeholder.
            let msg_id = entry.id.handle.as_str().parse::<i64>().unwrap_or(0);
            let file_id = db.upsert_file(&FileUpsert {
                rel_path,
                name,
                parent_dir,
                size: entry.size as i64,
                mtime: entry.mtime,
                sha256: None,
                is_dir: false,
                telegram_msg_id: Some(msg_id),
                is_uploaded: true,
                is_cached: false,
                is_encrypted: false,
                chunk_count: 1,
                mime_type: None,
            })?;
            // Single-container chunks row (K11): index 0 carrying the
            // row's msg_id and the whole size — the exact shape an
            // upload persist writes for a one-element receipt, so a
            // rebuilt file is row/chunks-equivalent to a sync-copied
            // one.
            db.upsert_chunk(file_id, 0, msg_id, entry.size as i64, None)?;
        }
        EntryKind::Dir => {
            // create_dir parity: zero-sized, zero chunks, NULL msg_id,
            // born uploaded + cached.
            db.upsert_file(&FileUpsert {
                rel_path,
                name,
                parent_dir,
                size: 0,
                mtime: entry.mtime,
                sha256: None,
                is_dir: true,
                telegram_msg_id: None,
                is_uploaded: true,
                is_cached: true,
                is_encrypted: false,
                chunk_count: 0,
                mime_type: None,
            })?;
        }
    }
    db.get_file(&format!("/{}", entry.path.as_str()))?
        .ok_or_else(|| {
            // Unreachable in practice (upsert then read of the same key
            // under the same lock); surfaces through the existing
            // `RebuildError::Db` shape — the SELECT came back empty —
            // rather than a new variant (Phase 8: rebuild's error shape
            // stays untouched).
            RebuildError::from(DbError::from(rusqlite::Error::QueryReturnedNoRows))
        })
}

/// depth-1 全页归集（read-through 用；rebuild 的流式页循环保持原样——
/// 20 万级单目录内存 ≈ 数 MB，可整目录归集）。双上限（[`LIST_MAX_PAGES`]
/// / [`LIST_MAX_ENTRIES`]，M2）：自指游标在闸处报 `Unavailable`（消息含
/// 目录与计数），绝不挂死。
pub async fn list_all_pages(
    driver: &dyn StorageDriver,
    dir: &RelPath,
) -> Result<Vec<Entry>, StorageError> {
    let mut out = Vec::new();
    let mut cursor = PageCursor::Start;
    let mut pages = 0usize;
    loop {
        pages += 1;
        if pages > LIST_MAX_PAGES {
            return Err(StorageError::Unavailable(format!(
                "directory listing for `{dir}` exceeded the {LIST_MAX_PAGES}-page \
                 read-through cap after {pages} pages; a self-feeding cursor is suspected"
            )));
        }
        if out.len() > LIST_MAX_ENTRIES {
            return Err(StorageError::Unavailable(format!(
                "directory listing for `{dir}` exceeded the {LIST_MAX_ENTRIES}-entry \
                 read-through cap at {pages} pages ({entries} entries); \
                 a self-feeding cursor is suspected",
                entries = out.len()
            )));
        }
        let listing = driver.list(dir, Page { limit: 512, cursor }).await?;
        out.extend(listing.entries);
        match listing.next {
            Some(next) => cursor = next,
            None => return Ok(out),
        }
    }
}
