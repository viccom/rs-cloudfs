//! Read-through 按需逐层索引原语（Phase 8 / RT2，D1–D10）。
//!
//! 读路径从「本地 db 闸门（miss 即 404）」改为「按需逐层回源 + 物化
//! 缓存」：[`read_dir_fresh`] 每次调用现查后端并调和本地行（D5 强制
//! revalidate），[`stat_fresh`] 靠父目录 TTL 窗把 Explorer 的 stat 风暴
//! 归并成一次父 list（D6）。机制面：
//!
//! - **门**（D2）：`authoritative_index && as_driver().is_some()` 才回
//!   源；否则逐字退化为 `db.list_dir`/`db.get_file`（telegram 窄面零行
//!   为变化，A6）；
//! - **单飞 + 归并**（D5/D6）：per-dir tokio Mutex 串行化回源（锁不跨
//!   注册表，RV1 纪律）；闸内重查「到达世代」——排队等待期间同目录已
//!   有完成趟的并发调用直读 db，恰一次 list；
//! - **物化**（D4）：[`crate::materialize::materialize_entry`] 唯一映射；
//! - **删除双确认**（D7）：候选缺失行逐条 `driver.stat` 复核，NotFound
//!   才删；单目录超限整批跳过；in-flight 行两侧豁免（D7/D10）；
//! - **stale-if-error**（R2）：回源瞬态错绝不落 404，有旧行照常服务；
//! - [`DirCache`]：TTL 窗（stat_fresh 快路径）+ 单飞闸 + 世代计数 +
//!   写侧就近失效（pan115 先例）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use cloudkit_storage::{Entry, RelPath as VolRel, StorageDriver, StorageError};

use crate::cache::CacheManager;
use crate::database::{FileRecord, MetaDatabase};
use crate::materialize::{list_all_pages, materialize_entry};
use crate::rebuild::RebuildError;
use crate::rel_path::RelPath;
use crate::transport::CloudTransport;
use crate::vfs::VfsError;

/// 单目录 prune 候选上限（D7）：超过视为疑似残缺列表，整批跳过删除 +
/// 告警；权威批量清账归 rebuild 完成趟 sweep。
const PRUNE_CANDIDATE_CAP: usize = 32;

/// stat_fresh 的父目录 TTL 窗（D6）。实测定值，真机矩阵后可调
/// （`with_ttl` 测试缝；挂账预登记）。
const DEFAULT_DIR_TTL: Duration = Duration::from_secs(5);

/// 目录级缓存状态：TTL 窗 + 单飞闸 + 世代计数。
///
/// 内部 `std::sync::Mutex` 短临界区（查/标记/取闸即还）；唯一跨 await
/// 的 [`tokio::sync::Mutex`] 从闸里取出后在锁外持有——注册表锁纪律
/// （RV1「锁不跨 await」）自动满足。
pub struct DirCache {
    ttl: Duration,
    inner: Mutex<DirCacheInner>,
}

#[derive(Default)]
struct DirCacheInner {
    /// stat_fresh 快路径的 TTL 窗（D6）：目录 → 上次强制刷新完成时刻。
    marked: HashMap<String, Instant>,
    /// 单飞归并的到达世代（D5/D6）：目录 → 已完成的强制刷新次数。
    generations: HashMap<String, u64>,
    /// per-dir 单飞闸（临界区跨回源 await，故为 tokio Mutex）。
    flights: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

impl Default for DirCache {
    fn default() -> Self {
        Self::new()
    }
}

impl DirCache {
    /// 生产形态：默认 TTL 窗（[`DEFAULT_DIR_TTL`]）。
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_DIR_TTL)
    }

    /// 测试缝：显式 TTL（挂账预登记——真机后窗口值可调）。
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(DirCacheInner::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, DirCacheInner> {
        // 毒锁恢复统一模式（code-style §2）：缓存状态无不变量可破。
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// per-dir 单飞闸：同一目录的并发回源在此串行；调用方在闸内重查世
    /// 代（[`DirCache::generation`]），命中即归并为直读 db。
    pub fn flight(&self, dir: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.lock()
            .flights
            .entry(dir.to_string())
            .or_default()
            .clone()
    }

    /// 本目录已完成的强制刷新世代。
    pub fn generation(&self, dir: &str) -> u64 {
        self.lock().generations.get(dir).copied().unwrap_or(0)
    }

    /// 记一次强制刷新完成：世代 +1，并落 TTL 窗（stat_fresh 的零网络
    /// 判据）。
    pub fn mark(&self, dir: &str) {
        let mut inner = self.lock();
        *inner.generations.entry(dir.to_string()).or_default() += 1;
        inner.marked.insert(dir.to_string(), Instant::now());
    }

    /// TTL 窗内（`true` = 快路径可零网络直出）。顺手清出过期项。
    pub fn fresh(&self, dir: &str) -> bool {
        let mut inner = self.lock();
        match inner.marked.get(dir) {
            Some(at) if at.elapsed() < self.ttl => true,
            Some(_) => {
                inner.marked.remove(dir);
                false
            }
            None => false,
        }
    }

    /// 写侧就近失效（pan115 先例）：撤窗使下一次 stat_fresh 强制重列
    /// （D5 的 read_dir_fresh 本就每调必列，失效是快路径的双保险）。
    pub fn invalidate(&self, dir: &str) {
        self.lock().marked.remove(dir);
    }
}

/// vpath（VFS 绝对式，`/docs`）→ vocab（卷内相对，`docs`；根 → 卷根
/// 空串）。两类型并存的有意形态（L2 vpath.rs 模块文档），ck-local
/// transport_face 同款换算点；vpath 已校验，失败只可能是防御形态。
fn vocab_rel(path: &str) -> Result<VolRel, StorageError> {
    VolRel::new(path.trim_start_matches('/'))
}

/// 物化映射的读穿侧错误面：[`materialize_entry`] 说的还是 rebuild 的
/// 错误形状（D4 平移不动其契约），在此归一到 [`VfsError`]——`Db` 直
/// 转；`List` 的载荷是驱动错误（R2 分类学）；`EncryptedInstance` 只
/// 出自 K11 纯闸门、`Serde` 只出自 rebuild 的检查点序列化（materialize
/// 两者皆不产），防御臂归 `Invalid`。
fn materialize(db: &MetaDatabase, entry: &Entry) -> Result<FileRecord, VfsError> {
    materialize_entry(db, entry).map_err(|error| match error {
        RebuildError::Db(db_error) => VfsError::Db(db_error),
        RebuildError::List { source, .. } => VfsError::Transport(source),
        RebuildError::EncryptedInstance | RebuildError::Serde(_) => {
            VfsError::Transport(StorageError::Invalid)
        }
    })
}

/// Read-through 目录列表（D3/D5）：权威宽面卷每次调用现查后端（恰一
/// 次全页 list）并调和本地行；门不过的卷逐字退化为
/// [`MetaDatabase::list_dir`]。单飞语义见模块文档。
pub async fn read_dir_fresh(
    db: &MetaDatabase,
    transport: &dyn CloudTransport,
    cache: &CacheManager,
    dir_cache: &DirCache,
    encrypted_instance: bool,
    dir: &RelPath,
) -> Result<Vec<FileRecord>, VfsError> {
    // D2 门：宽面不在场 → 逐字退化（telegram 零行为变化，A6）。
    let Some(driver) = transport.as_driver() else {
        return Ok(db.list_dir(dir.as_str())?);
    };
    // 能力位不申报权威索引 → 同退化（R4：能力位诚实成为可执行约束）。
    if !transport.capabilities().authoritative_index {
        return Ok(db.list_dir(dir.as_str())?);
    }
    // D10 加密实例显式拒收（rebuild K11 闸门同款）：密文容器的尺寸是
    // 密文尺寸，物化成行会破坏「size = 明文」契约与 AEAD 预算数学。
    // 文案指路 cydrive sync（A7）。
    if encrypted_instance {
        return Err(VfsError::EncryptedInstance);
    }

    let dir_str = dir.as_str();
    let arrived = dir_cache.generation(dir_str);
    let flight = dir_cache.flight(dir_str);
    // per-dir 单飞（D5/D6）：临界区跨回源 await；不与任何注册表锁嵌套。
    let _guard = flight.lock().await;
    // 闸内重查世代：排队期间同目录已有完成趟 → 归并，直读 db（并发 N
    // 次 → 恰 1 次 list，用例 10）。顺序调用（前趟已完成才到达）世代
    // 相同 → 照常强制 revalidate（D5 的每视图现查语义不受影响，用例 4）。
    if dir_cache.generation(dir_str) != arrived {
        return Ok(db.list_dir(dir_str)?);
    }

    let vol_dir = vocab_rel(dir_str)?;
    match list_all_pages(driver, &vol_dir).await {
        Ok(entries) => {
            // upsert 侧物化：in-flight 行跳过（D7/D10）——上传 worker 的
            // 成功写回归属行，不得被回源物化冲掉。
            for entry in &entries {
                let vpath = format!("/{}", entry.path.as_str());
                if db
                    .get_file(&vpath)?
                    .is_some_and(|row| row_in_flight(&row, cache))
                {
                    tracing::debug!(
                        path = %vpath,
                        "in-flight row exempted from read-through materialization"
                    );
                    continue;
                }
                materialize(db, entry)?;
            }
            reconcile_dir(db, driver, cache, dir, &entries).await?;
            dir_cache.mark(dir_str);
            Ok(db.list_dir(dir_str)?)
        }
        // 驱动断言目录不存在（权威形态）：删本层行——直接子行 + 目录自
        // 身行，逐条 stat 双确认（与 prune 臂同一 helper；更深层行留库
        // 由 rebuild sweep 清账，RT4）——然后回 NotFound。
        Err(StorageError::NotFound) => {
            let rows = db.list_dir(dir_str)?;
            let mut removed = 0usize;
            for row in &rows {
                if delete_confirmed(driver, db, &row.rel_path).await? {
                    removed += 1;
                }
            }
            if !dir.is_root() && delete_confirmed(driver, db, dir_str).await? {
                removed += 1;
            }
            tracing::debug!(dir = %dir, removed, "driver reports the directory gone; local layer pruned");
            Err(VfsError::NotFound(dir_str.to_string()))
        }
        // stale-if-error（R2 / 风险表）：上游瞬断绝不清库——有旧行照常
        // 服务旧列表（warn 声明）；无旧行才上抛（且绝不落 NotFound，
        // 404 只留给确认不存在）。
        Err(error) => {
            let rows = db.list_dir(dir_str)?;
            if rows.is_empty() {
                Err(VfsError::Transport(error))
            } else {
                tracing::warn!(
                    dir = %dir,
                    %error,
                    "backend list failed; serving the stale index rows"
                );
                Ok(rows)
            }
        }
    }
}

/// reconcile 一层：物化远端条目（upsert 侧的 in-flight 豁免在
/// `read_dir_fresh` 体内），并对「本目录 `is_uploaded = 1` 且不在远端
/// 列表」的行做 stat 双确认 prune（D7）。
async fn reconcile_dir(
    db: &MetaDatabase,
    driver: &dyn StorageDriver,
    cache: &CacheManager,
    dir: &RelPath,
    entries: &[Entry],
) -> Result<(), VfsError> {
    // prune 候选：已上传（远端应有权威副本）却不在本次列表的行。
    // pending 行（is_uploaded = 0，in-flight/ghost）天然不是候选——
    // 它们没有远端副本，缺失是常态而非删除信号（D7/D10）；显式的
    // in-flight 免删由此筛选保证。
    let listed: std::collections::HashSet<&str> =
        entries.iter().map(|entry| entry.path.as_str()).collect();
    let rows = db.list_dir(dir.as_str())?;
    let candidates: Vec<&FileRecord> = rows
        .iter()
        .filter(|row| {
            row.is_uploaded
                && !listed.contains(row.rel_path.trim_start_matches('/'))
                && !row_in_flight(row, cache)
        })
        .collect();
    if candidates.len() > PRUNE_CANDIDATE_CAP {
        // D7 上限：大批缺失是「列表性故障」（瞬断/残缺页）的形态而非
        // 集中删除——整批跳过 + 告警，绝不基于一次可疑观测清库。
        tracing::warn!(
            dir = %dir,
            candidates = candidates.len(),
            cap = PRUNE_CANDIDATE_CAP,
            "too many prune candidates; skipping the whole batch (suspected partial listing; authoritative cleanup belongs to rebuild)"
        );
        return Ok(());
    }
    for row in candidates {
        delete_confirmed(driver, db, &row.rel_path).await?;
    }
    Ok(())
}

/// D7/D10 in-flight 判据（[`crate::sync::is_in_flight_row`] 单点同源，
/// 原 sync.rs:579-580 判定）：pending 且本地副本在盘。键不可解析的行按
/// 非 in-flight 处理（毒行走 rebuild 既有语义，物化覆盖）。
fn row_in_flight(row: &FileRecord, cache: &CacheManager) -> bool {
    match RelPath::new(&row.rel_path) {
        Ok(rel) => crate::sync::is_in_flight_row(Some(row), cache, &rel),
        Err(_) => false,
    }
}

/// 双确认删除（D7「删除永不基于单次观测」）：仅当 `driver.stat` 复报
/// NotFound 才删行（chunks 随 `delete_file` 事务级联）；Ok（谎报/已回
/// 归）或其他 Err 一律保留。返回是否确删。
async fn delete_confirmed(
    driver: &dyn StorageDriver,
    db: &MetaDatabase,
    rel_path: &str,
) -> Result<bool, VfsError> {
    let vol = vocab_rel(rel_path)?;
    match driver.stat(&vol).await {
        Err(StorageError::NotFound) => {
            db.delete_file(rel_path)?;
            Ok(true)
        }
        // 谎报 Ok / 瞬态错：保留（删除面绝不吸收错误为真）。
        _ => Ok(false),
    }
}

/// Read-through 单路径 stat（D6）：父目录 TTL 窗内行命中零网络直出；
/// 过期 / 缺行重列父目录恰一次（Explorer 的百项 stat 风暴归并为一次父
/// list）；仍无行再 `driver.stat` 兜底。根恒存（合成元数据）。
pub async fn stat_fresh(
    db: &MetaDatabase,
    transport: &dyn CloudTransport,
    cache: &CacheManager,
    dir_cache: &DirCache,
    encrypted_instance: bool,
    rel: &RelPath,
) -> Result<FileRecord, VfsError> {
    // D2 门：不过 → db.get_file 逐字退化（今日行为）。
    let degrade = || {
        db.get_file(rel.as_str())?
            .ok_or_else(|| VfsError::NotFound(rel.as_str().to_string()))
    };
    let Some(driver) = transport.as_driver() else {
        return degrade();
    };
    if !transport.capabilities().authoritative_index {
        return degrade();
    }
    // D10 加密实例显式拒收（read_dir_fresh 同款闸门，含根——拒绝先于
    // 一切合成/回源，A7 零网络）。
    if encrypted_instance {
        return Err(VfsError::EncryptedInstance);
    }

    // 根：恒存的目录，合成元数据（消费面 RowMetaData::root 同形：
    // 0 尺寸 / mtime 0 / 无 etag 素材）。
    if rel.is_root() {
        return Ok(root_record());
    }

    let parent = rel.parent().unwrap_or_else(RelPath::root);
    // 快路径（D6）：行命中且父目录在 TTL 窗内 → 零网络直出。
    if let Some(row) = db.get_file(rel.as_str())? {
        if dir_cache.fresh(parent.as_str()) {
            return Ok(row);
        }
    }
    // D6 风暴归并：重列父目录恰一次。回源错误在此容忍——最终结果由下
    // 面的 driver.stat 兜底决定（绝不因瞬态错回 404，R2）。
    let _ = read_dir_fresh(db, transport, cache, dir_cache, encrypted_instance, &parent).await;
    if let Some(row) = db.get_file(rel.as_str())? {
        return Ok(row);
    }
    // 兜底：逐路径 stat（父层被谎报/半残时深路径仍可寻址）。行在库则
    // 到不了这里，无 in-flight 可伤；直接物化。
    match driver.stat(&vocab_rel(rel.as_str())?).await {
        Ok(entry) => {
            materialize(db, &entry)?;
            db.get_file(rel.as_str())?
                .ok_or_else(|| VfsError::NotFound(rel.as_str().to_string()))
        }
        Err(StorageError::NotFound) => Err(VfsError::NotFound(rel.as_str().to_string())),
        Err(error) => Err(VfsError::Transport(error)),
    }
}

/// 合成根行（消费面 `RowMetaData::root` 同形的 `FileRecord` 投影：
/// 恒存目录、0 尺寸、mtime 0、无 sha/msg 素材）。
fn root_record() -> FileRecord {
    FileRecord {
        id: 0,
        rel_path: "/".to_string(),
        name: String::new(),
        parent_dir: "/".to_string(),
        size: 0,
        mtime: 0.0,
        sha256: None,
        is_dir: true,
        telegram_msg_id: None,
        is_uploaded: true,
        is_cached: true,
        is_encrypted: false,
        chunk_count: 0,
        mime_type: None,
        encryption_scheme: crate::config::SCHEME_GCM.to_string(),
        created_at: None,
        updated_at: None,
    }
}
