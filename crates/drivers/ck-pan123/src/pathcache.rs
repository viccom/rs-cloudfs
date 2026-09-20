//! 路径 ↔ file_id 解析层（Phase 6 / 123-2）。
//!
//! 123 的寻址是 **file_id**，而 [`StorageDriver`] 契约是 [`RelPath`]——
//! 本模块是两面之间的桥：每个卷根（`pan123_root`，D3 缺省 `"0"`）逐级
//! 下行，`/a/b` 的 file_id 由「a 的 file_id + 子条目名匹配」经 list 得出
//! （**list-walk 起步——简单正确**；123 的 info 端点按 fileId 查、无法
//! 解析路径）。
//!
//! 设计取舍（对齐 ck-pan115 pathcache 先例——任务 B 的裁决：list-walk
//! 为主，缓存作为优化按先例加上）：
//!
//! - **缓存 = 父 file_id → (名字 → 子条目) 的单层映射**，由 `list`/
//!   `stat` 成功路径写入（[`PathCache::put_dir`]）；`resolve` 命中缓存
//!   零网络，未命中才 list 该层下行；
//! - **TTL 10 分钟 + 容量上限 1024 目录**（M-S5 同款纪律）：本进程外
//!   的变更（123 官方端/网页删改）由 TTL 兜底；超出容量按最旧驱逐
//!   （rebuild 大树的内存上界）；
//! - **就近失效 + 反查索引**：`mkdir`/`delete`/`rename` 成功后就近失效
//!   受影响目录。delete 只有 EntryId（句柄 = 裸 file_id，任务 A 契约）
//!   不知父目录——**反查索引** `file_id → 父 file_id`（put_dir 顺带
//!   维护）让 [`PathCache::invalidate_owner`] 找到该失效的目录（K73
//!   M-S1 的教训：ghost 行驻留缓存）；
//! - **stat 末级新鲜查询**（[`PathCache::resolve_with`] 的
//!   `fresh_last`）：末段绕过缓存现列父目录——后端错误必须能被 stat
//!   观察到（M-S1：stat 缓存吞后端错误）。
//!
//! [`StorageDriver`]: cloudkit_storage::StorageDriver
//! [`RelPath`]: cloudkit_storage::RelPath

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cloudkit_storage::{RelPath, StorageError};
use tokio::sync::RwLock;

use crate::api::Pan123Client;
use crate::models::FileEntry;

/// 单目录的子条目索引：名字 → 条目。
type DirIndex = HashMap<String, FileEntry>;

/// 目录索引的 TTL（M-S5）：外部（123 官方端）删改对长挂载进程的可见
/// 性上界——过期即 miss（重列自愈）。本进程内的变更走就近失效，不
/// 依赖 TTL。
const DIR_TTL: Duration = Duration::from_secs(10 * 60);
/// 缓存目录数上限（M-S5）：rebuild 扫大树时的内存上界——超出按最旧
/// 驱逐（`put_dir` 刷新时间戳 = 「最近用过」的近似）。
const DIR_CAP: usize = 1024;
/// list 翻页页长（spike 探测形态的保守值；123 的 limit 上限未采样——
/// 100 与 spike 一致）。
pub(crate) const PAGE: u32 = 100;

/// 路径解析缓存（并发共享；`&self` 方法族安全）。
pub struct PathCache {
    ttl: Duration,
    /// 父 file_id → (该目录已列出的子条目, 本次落缓存时刻)。
    dirs: RwLock<HashMap<String, (DirIndex, Instant)>>,
    /// file_id → 父 file_id 反查索引（delete 的就近失效面——句柄是裸
    /// file_id 不带父，K73 M-S1 形态的解）。
    owners: RwLock<HashMap<String, String>>,
}

impl Default for PathCache {
    fn default() -> Self {
        PathCache::new()
    }
}

impl PathCache {
    pub fn new() -> Self {
        PathCache::with_ttl(DIR_TTL)
    }

    /// 测试缝：注入 TTL（毫秒级窗口——`LimiterConfig::fast` 先例形态：
    /// 结构注入而非时钟替身）。
    pub fn with_ttl(ttl: Duration) -> Self {
        PathCache {
            ttl,
            dirs: RwLock::new(HashMap::new()),
            owners: RwLock::new(HashMap::new()),
        }
    }

    /// 记下一层目录列表（list/stat 成功后调用——顺带喂缓存）。容量满
    /// 时先驱逐最旧条目；反查索引同步重建（该目录的行全部指向它）。
    pub async fn put_dir(&self, cid: &str, rows: &[FileEntry]) {
        let mut dirs = self.dirs.write().await;
        if !dirs.contains_key(cid) && dirs.len() >= DIR_CAP {
            if let Some(oldest) = dirs
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, _)| k.clone())
            {
                dirs.remove(&oldest);
                self.owners.write().await.retain(|_, v| v != &oldest);
            }
        }
        let mut index = DirIndex::new();
        let mut owners = self.owners.write().await;
        owners.retain(|_, v| v != cid);
        for row in rows {
            index.insert(row.file_name.clone(), row.clone());
            owners.insert(row.file_id.to_string(), cid.to_string());
        }
        dirs.insert(cid.to_string(), (index, Instant::now()));
    }

    /// 命中的子条目（零网络）；未命中（含 TTL 过期）→ None（调用方走
    /// list 下行）。
    pub async fn get_child(&self, cid: &str, name: &str) -> Option<FileEntry> {
        let dirs = self.dirs.read().await;
        let (index, at) = dirs.get(cid)?;
        if at.elapsed() >= self.ttl {
            return None;
        }
        index.get(name).cloned()
    }

    /// 失效一个目录（结构变更后就近调用；未在缓存中 → 无操作；反查
    /// 索引同步清理）。
    pub async fn invalidate(&self, cid: &str) {
        self.dirs.write().await.remove(cid);
        self.owners.write().await.retain(|_, v| v != cid);
    }

    /// 按条目反查并失效其父目录（delete 的面——句柄是裸 file_id）。
    /// 返回被失效的父 file_id（缓存不知道该条目 → None，无操作）。
    pub async fn invalidate_owner(&self, fid: i64) -> Option<String> {
        let owner = self.owners.write().await.remove(&fid.to_string())?;
        self.dirs.write().await.remove(&owner);
        // 同目录其余行的反查项一并清（目录整体失效）。
        self.owners.write().await.retain(|_, v| v != &owner);
        Some(owner)
    }

    /// 解析 `path` 到条目（末级目录走缓存）。
    ///
    /// - 根（`/`）→ 空 Resolved（cid = 卷根）；
    /// - 逐级下行：命中缓存零网络；未命中 list 该层并喂缓存；
    /// - 中途不存在 → `NotFound`；中途遇文件 → `NotFound`；
    /// - **末段是文件**：`want_dir = false` 时返回 (父 cid, Some(文件
    ///   行))；`want_dir = true` 时文件占位 → `NotFound`（目录语义）。
    pub async fn resolve(
        &self,
        client: &Pan123Client,
        root_cid: &str,
        path: &RelPath,
        want_dir: bool,
    ) -> Result<Resolved, StorageError> {
        self.resolve_with(client, root_cid, path, want_dir, false)
            .await
    }

    /// [`PathCache::resolve`] 的变体：`fresh_last = true` 时**末级组件
    /// 绕过缓存**（现列父目录）——`stat` 走此形态：stat 是新鲜度查询，
    /// 且后端错误必须能被 stat 观察到（缓存命中会把注入的后端错误吞掉，
    /// M-S1）。中间层仍走缓存（目录链稳定）。
    pub async fn resolve_with(
        &self,
        client: &Pan123Client,
        root_cid: &str,
        path: &RelPath,
        want_dir: bool,
        fresh_last: bool,
    ) -> Result<Resolved, StorageError> {
        let comps: Vec<String> = path.components().map(str::to_string).collect();
        let mut cid = root_cid.to_string();
        if comps.is_empty() {
            return Ok(Resolved {
                parent_cid: cid.clone(),
                cid: cid.clone(),
                row: None,
            });
        }
        let last = comps.len() - 1;
        for (i, comp) in comps.iter().enumerate() {
            let want_fresh = fresh_last && i == last;
            let cached = if want_fresh {
                None
            } else {
                self.get_child(&cid, comp).await
            };
            let child = match cached {
                Some(row) => row,
                None => {
                    // 未命中：list 该层全量并喂缓存（翻页到尽）。
                    let rows = list_all(client, &cid).await?;
                    self.put_dir(&cid, &rows).await;
                    match self.get_child(&cid, comp).await {
                        Some(row) => row,
                        None => return Err(StorageError::NotFound),
                    }
                }
            };
            let is_dir = child.is_dir();
            if i == last {
                if want_dir && !is_dir {
                    return Err(StorageError::NotFound); // 目录语义遇文件占位
                }
                return Ok(Resolved {
                    parent_cid: cid.clone(),
                    cid: if is_dir {
                        child.file_id.to_string()
                    } else {
                        cid
                    },
                    row: Some(child),
                });
            }
            if !is_dir {
                return Err(StorageError::NotFound); // 中途遇文件
            }
            cid = child.file_id.to_string();
        }
        unreachable!("components() is non-empty and the loop returns on the last item")
    }
}

/// [`PathCache::resolve`] 的产出：末段条目的形态。
#[derive(Debug, Clone)]
pub struct Resolved {
    /// 末段的父目录 file_id（mkdir/delete/rename 的 parentFileId 面）。
    pub parent_cid: String,
    /// 末段自身若是目录 = 其 file_id；若是文件 = 父 cid（未使用）。
    pub cid: String,
    /// 末段条目（根解析时为 None）。
    pub row: Option<FileEntry>,
}

/// 列一个目录的全部条目（`Page` 1 基分页翻到尽）。
///
/// 终止规则（123-0 真机实证：Page 分页 page1/page2 零重叠、`Total`
/// 全量计数）：本页短于请求 limit（页短即尽为硬终止——防 Total 抖动
/// 死循环），或累计行数 ≥ `Total`。
pub async fn list_all(client: &Pan123Client, cid: &str) -> Result<Vec<FileEntry>, StorageError> {
    let mut out: Vec<FileEntry> = Vec::new();
    let mut page = 1u32;
    loop {
        let (rows, total) = client
            .list_page(cid.parse().unwrap_or(0), page, PAGE)
            .await?;
        let short = (rows.len() as u32) < PAGE;
        out.extend(rows);
        if short || (total >= 0 && out.len() as i64 >= total) {
            return Ok(out);
        }
        page += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, fid: i64, is_dir: bool) -> FileEntry {
        FileEntry {
            file_id: fid,
            file_name: name.to_string(),
            entry_type: if is_dir { 1 } else { 0 },
            size: 7,
            ..FileEntry::default()
        }
    }

    /// M-S5：目录索引按 TTL 过期——过期即 miss（外部删改的可见性
    /// 上界），miss 后由调用方重列自愈。
    #[tokio::test]
    async fn entries_expire_after_the_ttl() {
        let cache = PathCache::with_ttl(Duration::from_millis(30));
        cache.put_dir("10", &[row("a", 1, true)]).await;
        assert!(
            cache.get_child("10", "a").await.is_some(),
            "a fresh entry hits"
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            cache.get_child("10", "a").await.is_none(),
            "an expired entry misses (the caller re-lists)"
        );
    }

    /// M-S5：容量上限——超出 DIR_CAP 时最旧目录被驱逐，新近条目保留。
    #[tokio::test]
    async fn capacity_evicts_the_oldest_directory() {
        let cache = PathCache::new();
        for i in 0..DIR_CAP {
            cache
                .put_dir(&format!("d{i}"), &[row("x", 1000 + i as i64, false)])
                .await;
        }
        assert!(
            cache.get_child("d0", "x").await.is_some(),
            "within capacity everything stays"
        );
        cache
            .put_dir(&format!("d{DIR_CAP}"), &[row("x", 9999, false)])
            .await;
        assert!(
            cache.get_child("d0", "x").await.is_none(),
            "the oldest entry was evicted"
        );
        assert!(cache
            .get_child(&format!("d{}", DIR_CAP - 1), "x")
            .await
            .is_some());
        assert!(cache.get_child(&format!("d{DIR_CAP}"), "x").await.is_some());
    }

    /// 反查索引（M-S1 的 123 形态）：delete 只有裸 file_id——按行反查
    /// 父目录并整体失效；同目录其余行的反查项同步清理。
    #[tokio::test]
    async fn invalidate_owner_finds_and_drops_the_parent_directory() {
        let cache = PathCache::new();
        cache
            .put_dir("10", &[row("a", 1, false), row("b", 2, false)])
            .await;
        cache.put_dir("20", &[row("c", 3, false)]).await;
        let owner = cache.invalidate_owner(1).await;
        assert_eq!(
            owner.as_deref(),
            Some("10"),
            "the parent directory is found"
        );
        assert!(
            cache.get_child("10", "a").await.is_none(),
            "the whole parent dir is invalidated"
        );
        assert!(
            cache.get_child("10", "b").await.is_none(),
            "sibling rows leave with the directory"
        );
        assert!(
            cache.get_child("20", "c").await.is_some(),
            "unrelated directories survive"
        );
        // 已失效后的第二次反查 → None（幂等无操作）。
        assert_eq!(cache.invalidate_owner(1).await, None);
    }
}
