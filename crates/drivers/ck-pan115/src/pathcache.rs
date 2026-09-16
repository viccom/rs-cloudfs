//! 路径 ↔ folder id 解析层（Phase 5 / 115-2）。
//!
//! 115 的寻址是 **folder id（cid）**，而 [`StorageDriver`] 契约是
//! [`RelPath`]——本模块是两面之间的桥。每个卷根（`pan115_root`，D3
//! 缺省 `"0"`）解析为 cid 后，逐级下行：`/a/b` 的 cid 由「a 的 cid +
//! 子条目名匹配」得出。
//!
//! 设计取舍（对齐 ck-baidu 的 `handles` 句柄缓存先例）：
//!
//! - **缓存 = 父 cid → (名字 → 子条目) 的单层映射**，由 `list`/`stat`
//!   成功路径写入（`put_dir`）；`resolve` 命中缓存零网络，未命中才
//!   走 `list` 下行。K44 先例（Explorer 列目录风暴走 db 行零网络）的
//!   驱动侧形态：同一目录反复解析只花一次网络。
//! - **失效**：`mkdir`/`delete`/`rename`/写路径成功后就近失效受影响
//!   目录（`invalidate`），不做全树清扫——错的缓存比没有缓存更坏。
//!   本进程外的变更（经 115 官方端/网页的删改）由 **TTL** 兜底：目录
//!   索引 10 分钟过期（M-S5——长挂载进程的外部可见性上界；`stat` 的
//!   末级新鲜查询不受影响）。**容量上限** 1024 目录：超出按最旧驱逐
//!   （rebuild 大树的内存上界；`put_dir` 即「最近用过」）。
//! - **凭证与配额纪律**：本层只读/写内存映射，**不持久化**（跨进程
//!   重启重新解析；115 的 cid 在账号内稳定但无必要落盘）。
//!
//! [`StorageDriver`]: cloudkit_storage::StorageDriver
//! [`RelPath`]: cloudkit_storage::RelPath

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cloudkit_storage::{RelPath, StorageError};
use tokio::sync::RwLock;

use crate::api::{ListRow, Pan115Client};

/// 单目录的子条目索引：名字 → 条目。
type DirIndex = HashMap<String, ListRow>;

/// 目录索引的 TTL（M-S5）：外部（115 官方端）删改对长挂载进程的可见
/// 性上界——过期即 miss（重列自愈）。本进程内的变更走就近失效，不
/// 依赖 TTL。
const DIR_TTL: Duration = Duration::from_secs(10 * 60);
/// 缓存目录数上限（M-S5）：rebuild 扫大树时的内存上界——超出按最旧
/// 驱逐（`put_dir` 刷新时间戳 = 「最近用过」的近似）。
const DIR_CAP: usize = 1024;

/// 路径解析缓存（并发共享；`&self` 方法族安全）。
pub struct PathCache {
    ttl: Duration,
    /// 父 cid → (该目录已列出的子条目, 本次落缓存时刻)。
    /// `list`/`stat`/`resolve` 成功路径写入；结构变更后就近失效；TTL
    /// 过期在读取面判（miss 即重列——过期条目由下一次 put_dir 覆盖）。
    dirs: RwLock<HashMap<String, (DirIndex, Instant)>>,
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
        }
    }

    /// 记下一层目录列表（list/stat 成功后调用——顺带喂缓存，baidu
    /// `handles.put_batch` 的解析层等价物）。容量满时先驱逐最旧条目。
    pub async fn put_dir(&self, cid: &str, rows: &[ListRow]) {
        let mut dirs = self.dirs.write().await;
        if !dirs.contains_key(cid) && dirs.len() >= DIR_CAP {
            if let Some(oldest) = dirs
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, _)| k.clone())
            {
                dirs.remove(&oldest);
            }
        }
        if let Some((index, at)) = dirs.get_mut(cid) {
            index.clear();
            *at = Instant::now();
        } else {
            dirs.insert(cid.to_string(), (HashMap::new(), Instant::now()));
        }
        let Some((index, _)) = dirs.get_mut(cid) else {
            unreachable!("the entry was just inserted")
        };
        for row in rows {
            index.insert(row.fname.clone(), row.clone());
        }
    }

    /// 命中的子条目（零网络）；未命中（含 TTL 过期）→ None（调用方走
    /// list 下行）。
    pub async fn get_child(&self, cid: &str, name: &str) -> Option<ListRow> {
        let dirs = self.dirs.read().await;
        let (index, at) = dirs.get(cid)?;
        if at.elapsed() >= self.ttl {
            return None;
        }
        index.get(name).cloned()
    }

    /// 失效一个目录（结构变更后就近调用；未在缓存中 → 无操作）。
    pub async fn invalidate(&self, cid: &str) {
        let mut dirs = self.dirs.write().await;
        dirs.remove(cid);
    }

    /// 失效全部（`rename` 跨目录移动的保守形态：源父与目标父都动过，
    /// 但更深的已缓存路径可能含旧结构——115 的 cid 不随 move 变化，
    /// 只有被移动条目**自身**的父关系变了，故只需失效两个父级；本
    /// 方法留给未来的全树操作（`rebuild` 等））。
    pub async fn clear(&self) {
        let mut dirs = self.dirs.write().await;
        dirs.clear();
    }

    /// 解析 `path` 到 folder id。
    ///
    /// - 根（`/`）→ `root_cid`（卷根，D3）；
    /// - 逐级下行：命中缓存零网络；未命中 `list` 该层并喂缓存；
    /// - 中途不存在 → `NotFound`（契约：list/stat 的目录不存在语义）；
    /// - **末段是文件**：`want_dir = false` 时返回 (父 cid, Some(文件
    ///   行))；`want_dir = true` 时文件占位 → `NotFound`（目录语义）。
    pub async fn resolve(
        &self,
        client: &Pan115Client,
        root_cid: &str,
        path: &RelPath,
        want_dir: bool,
    ) -> Result<Resolved, StorageError> {
        self.resolve_with(client, root_cid, path, want_dir, false)
            .await
    }

    /// [`PathCache::resolve`] 的变体：`fresh_last = true` 时**末级组件
    /// 绕过缓存**（现列父目录）——`stat` 走此形态：stat 是新鲜度查询，
    /// 且 conformance ⑤ 要求后端错误能被 stat 观察到（缓存命中会把
    /// 注入的后端错误吞掉）。中间层仍走缓存（目录链稳定）。
    pub async fn resolve_with(
        &self,
        client: &Pan115Client,
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
                    // 未命中：list 该层一次（分页取全，1000/页沿 115
                    // 上限之下——limit 用 200 保守值 + 翻页）。
                    let rows = list_all(client, &cid).await?;
                    self.put_dir(&cid, &rows).await;
                    match self.get_child(&cid, comp).await {
                        Some(row) => row,
                        None => return Err(StorageError::NotFound),
                    }
                }
            };
            let is_dir = child.fc == "0";
            if i == last {
                if want_dir && !is_dir {
                    return Err(StorageError::NotFound); // 目录语义遇文件占位
                }
                return Ok(Resolved {
                    parent_cid: cid.clone(),
                    cid: if is_dir { child.fid.clone() } else { cid },
                    row: Some(child),
                });
            }
            if !is_dir {
                return Err(StorageError::NotFound); // 中途遇文件
            }
            cid = child.fid.clone();
        }
        unreachable!("components() is non-empty and the loop returns on the last item")
    }
}

/// [`PathCache::resolve`] 的产出：末段条目的形态。
#[derive(Debug, Clone)]
pub struct Resolved {
    /// 末段的父目录 cid（mkdir/delete/move 的 `pid`/`parent_id` 面）。
    pub parent_cid: String,
    /// 末段自身若是目录 = 其 cid；若是文件 = 父 cid（未使用）。
    pub cid: String,
    /// 末段条目（根解析时为 None）。
    pub row: Option<ListRow>,
}

/// 列一个目录的全部条目（`list_files_page` 翻页到尽）。
///
/// 终止规则（OpenList 115_open `driver.go:90-97` 同形态）：累计行数
/// ≥ 服务端 `count`，或本页短于请求 limit（防 count 抖动死循环——
/// 页短即尽为硬终止）。
pub async fn list_all(client: &Pan115Client, cid: &str) -> Result<Vec<ListRow>, StorageError> {
    // limit 200：K69.3 限流纪律下的保守页长（大目录多翻几页，每页
    // 都过限流器；1000+ 的单页巨读不换页数少换的是突发）。
    const PAGE: i64 = 200;
    let mut out: Vec<ListRow> = Vec::new();
    loop {
        let offset = out.len() as i64;
        let (mut rows, count) = client.list_files_page(cid, PAGE, offset).await?;
        let short = (rows.len() as i64) < PAGE;
        out.append(&mut rows);
        if short || (count >= 0 && out.len() as i64 >= count) {
            return Ok(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str) -> ListRow {
        ListRow {
            fid: format!("fid-{name}"),
            fc: "1".to_string(),
            fs: 1,
            fname: name.to_string(),
            pc: format!("pc-{name}"),
            sha1: String::new(),
            upt: 0,
        }
    }

    /// M-S5：目录索引按 TTL 过期——过期即 miss（外部删改的可见性
    /// 上界），miss 后由调用方重列自愈。
    #[tokio::test]
    async fn entries_expire_after_the_ttl() {
        let cache = PathCache::with_ttl(Duration::from_millis(30));
        cache.put_dir("c1", &[row("a")]).await;
        assert!(
            cache.get_child("c1", "a").await.is_some(),
            "a fresh entry hits"
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            cache.get_child("c1", "a").await.is_none(),
            "an expired entry misses (the caller re-lists)"
        );
    }

    /// M-S5：容量上限——超出 DIR_CAP 时最旧目录被驱逐（rebuild 大树的
    /// 内存上界），新近条目保留。
    #[tokio::test]
    async fn capacity_evicts_the_oldest_directory() {
        let cache = PathCache::new();
        for i in 0..DIR_CAP {
            cache.put_dir(&format!("d{i}"), &[row("x")]).await;
        }
        assert!(
            cache.get_child("d0", "x").await.is_some(),
            "within capacity everything stays"
        );
        cache.put_dir(&format!("d{DIR_CAP}"), &[row("x")]).await;
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

    /// 就近失效不被 TTL 语义影响（invalidate 显式移除）。
    #[tokio::test]
    async fn invalidate_removes_immediately() {
        let cache = PathCache::with_ttl(Duration::from_secs(600));
        cache.put_dir("c1", &[row("a")]).await;
        cache.invalidate("c1").await;
        assert!(cache.get_child("c1", "a").await.is_none());
    }
}
