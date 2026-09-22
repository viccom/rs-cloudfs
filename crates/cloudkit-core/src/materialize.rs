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
/// 20 万级单目录内存 ≈ 数 MB，可整目录归集）。
pub async fn list_all_pages(
    driver: &dyn StorageDriver,
    dir: &RelPath,
) -> Result<Vec<Entry>, StorageError> {
    let mut out = Vec::new();
    let mut cursor = PageCursor::Start;
    loop {
        let listing = driver.list(dir, Page { limit: 512, cursor }).await?;
        out.extend(listing.entries);
        match listing.next {
            Some(next) => cursor = next,
            None => return Ok(out),
        }
    }
}
