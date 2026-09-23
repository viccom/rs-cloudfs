//! Entry → files 行的**唯一**物化映射（Phase 8 / D4；cipher 真相语义 =
//! Phase 8-B / B1+B3+B4）。
//!
//! 平移自 `rebuild.rs::upsert_entry`（K6 句柄降级 / K11 单容器 chunk
//! 形态原样保留）；rebuild 与 read-through 共用——映射语义只许在这一处
//! 维护。行形状契约（`is_uploaded = 1`、`chunk_count = 1`、目录行
//! `create_dir` 同形、`sha256`/`mime_type` 留 `NULL` 走 coalesce）见
//! [`crate::rebuild`] 模块文档，两消费方逐字同一契约。
//!
//! Cipher 真相三层优先（B1，随 [`CipherCtx`] 入参生效）：既有行的
//! cipher 真相（`is_encrypted`/scheme/其尺寸推导）> 实例配置初值；列举
//! 只填空、永不降级 `is_encrypted = 1` 的行，歧义交首读内容校验（B2，
//! EB2）与 sync 出路（B6）。目录行永明文化（`is_encrypted = false`）。

use cloudkit_crypto::v2::{DEFAULT_CHUNK_SIZE, HEADER_SIZE, TAG_SIZE};
use cloudkit_storage::{Entry, EntryKind, Page, PageCursor, RelPath, StorageDriver, StorageError};

use crate::database::{DbError, FileRecord, FileUpsert, MetaDatabase};
use crate::rebuild::RebuildError;

/// v1（冻结）容器开销 = 16B 盐 + 12B nonce + 16B GCM tag = 44 —— 尺寸闭式
/// `pt = ct − 44`（v1 无 magic，格式冻结自 Python 契约；空件 ct = 44 → 0）。
const V1_OVERHEAD: i64 = 44;

/// v2 容器头尺寸（`CKCRYPT2` 自描述头，34B = v2.rs [`HEADER_SIZE`]）。
const V2_HEADER: i64 = HEADER_SIZE as i64;

/// v2 每块尾随 GCM tag 尺寸（v2.rs [`TAG_SIZE`]）。
const V2_TAG: i64 = TAG_SIZE as i64;

/// 容器分块大小 = `AeadV2::new()` 的默认（vfs.rs `hydrate_v2` 无参构造 =
/// v2.rs [`DEFAULT_CHUNK_SIZE`] 1MiB 同源）；应用无容器分块配置缝
/// （`chunk_size_mb` 是上传队列分段，与容器分块无关，计划 §0.3）——给定
/// 方案后尺寸反推闭式精确。
const V2_CHUNK: i64 = DEFAULT_CHUNK_SIZE as i64;

/// 物化的 cipher 上下文（Phase 8-B B1/B3）：**实例配置初值**——`enabled`
/// = 加密实例（密码在），`scheme` = 配置的容器方案。真相三层优先的第
/// 二层：既有行 cipher 真相恒高于本初值，无既有真相（新行）才用它填空
/// （B1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CipherCtx {
    /// 实例是否加密——与上传行 vfs.rs:500 `is_encrypted =
    /// encryption_password.is_some()` 同源判据。
    pub enabled: bool,
    /// 配置的容器方案（新行/无真相行的 scheme 初值）。
    pub scheme: &'static str,
}

impl CipherCtx {
    /// 与上传行 vfs.rs:500-506 同源构造：密码在 = 加密实例；scheme =
    /// `cfg.encryption_scheme`。
    pub fn from_cfg(cfg: &crate::vfs::VfsConfig) -> Self {
        Self {
            enabled: cfg.encryption_password.is_some(),
            scheme: cfg.encryption_scheme.as_str(),
        }
    }
}

/// 从**密文容器尺寸**反推明文尺寸（Phase 8-B B3/B4：列举期零内容嗅探的
/// 闭式）。
///
/// - v1（冻结，无 magic）：`ct − 44`（盐 16 + nonce 12 + tag 16）；
/// - v2（34B 自描述头 + 分块流，每块密文 + 16B tag，恒默认 1MiB 分块）：
///   `body = ct − 34`；`n−1 = (body−16) / (chunk+16)`；`last = (body−16) %
///   (chunk+16)`；`pt = n−1 · chunk + last`（空件 body = 16 → 0；恰整倍数
///   → 满尾块——v2.rs 头部格式文档的自 delimiting 语义）。
///
/// 防御臂（B2 首读内容校验会纠正）：结构违例（短于头/尾块超 chunk）或
/// 未知方案 → 保守返回 `ct` 并 `warn`。
pub fn plaintext_len_from_container(ct: i64, scheme: &str) -> i64 {
    if scheme == crate::config::SCHEME_GCM {
        if ct < V1_OVERHEAD {
            tracing::warn!(
                ct,
                scheme,
                "ciphertext shorter than the v1 container overhead; \
                 returning the raw length (first-read validation will repair)"
            );
            return ct;
        }
        ct - V1_OVERHEAD
    } else if scheme == crate::config::SCHEME_AEAD_V2 {
        match plaintext_len_with_chunk(ct, V2_CHUNK) {
            Some(plain) => plain,
            None => {
                tracing::warn!(
                    ct,
                    "v2 container structure violated (first-read validation will repair); \
                     returning the raw length"
                );
                ct
            }
        }
    } else {
        tracing::warn!(
            scheme,
            ct,
            "unknown encryption scheme in the size back-solve; \
             returning the raw length (first-read validation will repair)"
        );
        ct
    }
}

/// v2 闭式反推的**结构敏感**核心（Phase 8-B EB2 / B2 首读校验用）：给定
/// 密文总长 `ct` 与**容器头自述的分块大小** `chunk`（头内真值参数——
/// `AeadV2Window` 解析所得，权威于应用默认 1MiB），返回明文长度；
/// 结构违例（短于 34B 头 + 16B tag、尾块超 chunk 上界）返回 `None`
/// ——调用方**不回写**（[`plaintext_len_from_container`] 的防御臂则按
/// 既有契约保守回 `ct`，供列举期使用，两者语义刻意不同）。
pub fn plaintext_len_with_chunk(ct: i64, chunk: i64) -> Option<i64> {
    let body = ct - V2_HEADER;
    if body < V2_TAG {
        return None;
    }
    if chunk <= 0 {
        return None;
    }
    let payload = body - V2_TAG;
    let stride = chunk + V2_TAG;
    let n_minus_1 = payload / stride;
    let last = payload % stride;
    if last > chunk {
        return None;
    }
    Some(n_minus_1 * chunk + last)
}

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
///
/// `cipher` = the instance's [`CipherCtx`] (Phase 8-B B1+B3):
/// `None` keeps today's verbatim behavior (`upsert_file`, plaintext
/// shapes — the rebuild default road); `Some(ctx)` resolves the file
/// row's cipher truth three-layer-first and writes through
/// [`MetaDatabase::upsert_materialized`]:
///
/// 1. an existing row with `is_encrypted = 1` — its scheme (and hence
///    its size back-solve) survives untouched, on a plaintext instance
///    too (never downgraded);
/// 2. otherwise the config initial (`ctx.enabled` / `ctx.scheme`) fills
///    the absent truth — size closed-form-derived under that scheme
///    while flagged encrypted, raw `entry.size` while plaintext;
/// 3. Dir rows are never ciphered (`is_encrypted = false`, today's
///    shape) regardless of the context.
pub fn materialize_entry(
    db: &MetaDatabase,
    entry: &Entry,
    cipher: Option<&CipherCtx>,
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
            let mut upsert = FileUpsert {
                rel_path: rel_path.clone(),
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
            };
            let file_id = match cipher {
                // 缺省（rebuild 缺省路）：今日逐字行为。
                None => db.upsert_file(&upsert)?,
                // 真相三层优先（B1）：既有 is_encrypted=1 行的 scheme >
                // 实例配置初值；尺寸按**真相 scheme** 从密文长闭式反推
                // （明文真相则 entry.size 原样）。写入经
                // `upsert_materialized`——flag/scheme 的保留由 ON CONFLICT
                // 保留集在 SQL 侧兜底（读-写窗内的并发写也不降级）。
                Some(ctx) => {
                    let existing = db.get_file(&rel_path)?;
                    let (truth_encrypted, truth_scheme): (bool, &str) = match &existing {
                        Some(row) if row.is_encrypted => (true, row.encryption_scheme.as_str()),
                        _ => (ctx.enabled, ctx.scheme),
                    };
                    upsert.is_encrypted = truth_encrypted;
                    upsert.size = if truth_encrypted {
                        plaintext_len_from_container(entry.size as i64, truth_scheme)
                    } else {
                        entry.size as i64
                    };
                    db.upsert_materialized(&upsert, truth_scheme)?
                }
            };
            // Single-container chunks row (K11): index 0 carrying the
            // row's msg_id and the whole size — the exact shape an
            // upload persist writes for a one-element receipt, so a
            // rebuilt file is row/chunks-equivalent to a sync-copied
            // one. `entry.size` stays the backend's own length (the
            // container length for cipher instances — the encrypted
            // upload receipt records ciphertext chunk sizes the same
            // way; reads consume only the msg id, never chunk.size).
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
