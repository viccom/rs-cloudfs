//! BaiduDriver——百度网盘 StorageDriver（Phase 2 Batch B1 元数据面 +
//! B2 上传/下载面）。
//!
//! 后端模型：卷根 [`crate::BaiduParams::root`]（后端绝对路径，缺省
//! `/apps/cloudfs`）下的网盘树；[`RelPath`] 相对路径 ↔ 后端绝对路径的
//! 换算归本驱动（root 前缀不泄漏到聚合层，R1）。
//!
//! ## 语义声明（trait 契约的驱动侧选择）
//!
//! - **句柄**：fs_id 十进制字符串（K5；跨 rename 稳定——PCFS
//!   api.go:170-171 先例）；他卷句柄 → `NotFound`（ck-local 同款契约）；
//! - **meta 停用**（真网 31300/31023 实证，2026-09-08 第四轮返工——见
//!   [`crate::api`] 模块文档）：stat = **list 父目录 + path 精确匹配**；
//!   fs_id → 条目解析（delete 句柄 / reader 的 dlink 签发路径）= **句柄
//!   缓存**（[`HandleCache`]，list/stat/Entry 流量批量填充，容量 4096）
//!   → 未命中**递归 list 扫描**（卷根深度优先，`scan_dir`）；
//! - **delete 幂等形态**：删除不存在句柄 → `NotFound` 恒定（conformance
//!   断言④钉死——不可解析 fs_id 视同不存在句柄；可解析但全树扫描无
//!   亦 `NotFound`）；filemanager delete **只支持 path 形态**（fs_id
//!   形态 errno=12 不删，真网实证 #3）——path 由两级解析供给；缓存陈旧
//!   （rename 后条目指向旧路径）由**纠偏腿**兜底：delete 撞 -9 → 条目
//!   失效 + 重扫 + 重试一次（防删错/删空）；xpan 删除走回收站 10 天是
//!   后端已知限制，非驱动语义）；
//! - **list**：depth-1、按 [`RelPath`] 字典序稳定有序、内部 offset 游标
//!   切 [`Page`]（后端无分页参数——spike api.rs:131-148 实证；游标
//!   `off:{end}` 形态，ck-local/mock 先例）；卷外路径条目（后端异常回显）
//!   跳过不透出；每次 list 顺带**批量填充句柄缓存**（一次 list 一次锁）；
//! - **mtime**：读 `server_mtime`（api.rs 模块文档注源）；
//! - **目录 size 恒 0**：后端对目录返回的实现值不透出（ck-local 同款）；
//! - **mkdir**：逐级隐式创建（xpan create 不自动建父目录，mock 严格语义
//!   钉死）+ **list 预检**（真网实证 2026-09-08 第五轮：目录 create 撞
//!   已存在 ≠ -8——errno=0 成功假象 + `<名>_<时间戳>` 空副本垃圾，见
//!   `tests/dir_create_preflight.rs`）：每层先 list 父目录，已存在 → 跳过
//!   create（最终层已存在 → `Exists`）；-8 分支保留为防御语义；
//! - **ensure_parents**（writer 隐式建父）：同款逐级 list 预检（上传到
//!   已有目录路径不产空副本垃圾；预检流量顺带喂句柄缓存）；
//! - **rename**：目标已存在 → **覆盖**（`ondup=overwrite`——PCFS
//!   api.go:829-845 钉死的表单形态即覆盖语义，mock/黄金参照一致；与
//!   trait「目标已存在 → Exists」的驱动侧偏离在此显式声明；conformance
//!   断言⑥复核结论：套件的 rename 断言**不覆盖**目标已存在形态
//!   （mv-dst 为新名），覆盖语义未被套件否定，维持声明）；
//! - **writer**：三步曲 stager（`upload.rs`——precreate rtype=3 /
//!   superfile2 4MiB 分片串行落定 / create + K7 差集续传会话）；目标
//!   是已存在目录 → `Invalid`（trait 契约）；
//! - **reader**：dlink 缓存 + 4MiB 有界 Range 分片流（`download.rs`
//!   ——K8/K9；Range>4MiB 驱动内拼接，对上层透明）；
//! - **capabilities**：B2 点亮六位（range_read/resume/multipart/
//!   server_side_move/rapid_upload/authoritative_index），逐位注码见
//!   [`StorageDriver::capabilities`] 实现。
//!
//! 错误映射表（errno 逐码注源）见 [`crate::api`] 模块文档。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use cloudkit_storage::{
    BackendHandle, ByteStream, Capabilities, Entry, EntryId, EntryKind, Listing, Page, PageCursor,
    Quota, Range, RelPath, StorageDriver, StorageError, UploadStager, VolumeId, WriteHint,
};

use crate::api;
use crate::client::BaiduClient;
use crate::{download, upload, BaiduParams};

/// 句柄缓存容量上限（真网 31300 裁决，2026-09-08）：fs_id → 后端条目的
/// 解析层缓存。超容**全清**（取舍：条目可由 list/扫描流量重建，代价是
/// 一次性冷启动重扫；逐出最旧的 LRU 需要额外簿记且「最旧」在批量填充
/// 形态下无实效——填充源本就是整目录列举，收益不抵复杂度）。
const HANDLE_CACHE_CAP: usize = 4096;

/// fs_id → 后端条目句柄缓存（真网 31300 裁决的解析层，2026-09-08）。
///
/// meta 端点在此 appkey 下全废（31300/31023），delete 句柄解析与 reader
/// 的 dlink 签发路径需要 fs_id → path/isdir/size 的解析面——由本缓存 +
/// 递归扫描（[`BaiduDriver::scan_dir`]）两级供给。**缓存只是解析层**：
/// K5（handle=fs_id 跨 rename 稳定）不受影响，陈旧条目（rename 后指向
/// 旧路径）由 delete 的纠偏腿（-9 → 失效重扫重试）兜底。
///
/// 填充点：`list`（批量，一次 list 一次锁）/ `stat`（父目录批量）/
/// `entry_for`（父目录批量）/ `scan_dir`（逐目录批量）/ writer 预检 /
/// mkdir 与 ensure_parents 的逐级 list 预检（2026-09-08 第五轮真网实证
/// 连带——预检流量是常态解析来源）。
pub(crate) struct HandleCache {
    map: Mutex<HashMap<i64, api::RemoteEntry>>,
}

impl HandleCache {
    pub(crate) fn new() -> Self {
        HandleCache {
            map: Mutex::new(HashMap::new()),
        }
    }

    fn get(&self, fs_id: i64) -> Option<api::RemoteEntry> {
        self.map.lock().unwrap().get(&fs_id).cloned()
    }

    fn invalidate(&self, fs_id: i64) {
        self.map.lock().unwrap().remove(&fs_id);
    }

    /// 批量填充（单次锁临界区；upload.rs 的 entry_for/预检填充点共用，
    /// pub(crate)）。超容全清后重灌（见 [`HANDLE_CACHE_CAP`] 取舍注码）
    /// ——插入项随后照常进入，保证本次填充原子可见。
    pub(crate) fn put_batch(&self, entries: &[api::RemoteEntry]) {
        if entries.is_empty() {
            return;
        }
        let mut map = self.map.lock().unwrap();
        if map.len() + entries.len() > HANDLE_CACHE_CAP {
            map.clear();
        }
        for e in entries {
            map.insert(e.fs_id, e.clone());
        }
    }
}

/// 百度网盘驱动。
pub struct BaiduDriver {
    volume: VolumeId,
    /// 规范化卷根（构造时去尾部 `/`；`/` 本身保留原样）。
    root: String,
    /// 共享 HTTP 面（stager/下载流任务经 Arc 持有——ByteStream 与
    /// UploadStager 的生命周期独立于 `&self`）。
    client: Arc<BaiduClient>,
    /// K7 上传会话表（内存 + 可选磁盘；与 stager 经 Arc 共享）。
    sessions: upload::SessionStore,
    /// K8 dlink 缓存（与下载流任务经 Arc 共享）。
    dlinks: Arc<download::DlinkCache>,
    /// fs_id → 条目句柄缓存（与 stager 经 Arc 共享；解析层，见结构体文档）。
    handles: Arc<HandleCache>,
}

impl BaiduDriver {
    /// 连接构造：uinfo 取 uid → VolumeId `baidu:<uid>`（K5）。
    ///
    /// token 齐备性校验在 [`BaiduClient::new`]；uinfo 失败按 errno 映射
    /// 表归一（110 会在 client 层自救一次——见 `oauth.rs` 状态机）。
    pub(crate) async fn connect(params: &BaiduParams) -> Result<Self, StorageError> {
        let client = Arc::new(BaiduClient::new(params)?);
        let uid = api::uinfo(&client).await?;
        let volume = VolumeId::new("baidu", &uid.to_string())?;
        // 根规范化：去尾部 `/`（`/apps/cloudfs/` 与 `/apps/cloudfs` 同义；
        // 根卷 `/` 本身会清成空串，还原为 `/`）。
        let mut root = params.root.trim_end_matches('/').to_string();
        if root.is_empty() {
            root.push('/');
        }
        let ttl = Duration::from_secs(
            params
                .dlink_ttl_secs
                .unwrap_or(crate::DEFAULT_DLINK_TTL_SECS),
        );
        Ok(BaiduDriver {
            volume,
            root,
            client,
            sessions: upload::SessionStore::new(params.sessions_dir.clone()),
            dlinks: Arc::new(download::DlinkCache::new(ttl)),
            handles: Arc::new(HandleCache::new()),
        })
    }

    /// RelPath → 后端绝对路径（root 前缀拼接；根目录即 root 本身）。
    ///
    /// B3b 起委派 upload::abs_of 共享函数（stager/transport 面/driver
    /// 三处同源，杜绝前缀拼接逻辑漂移）。
    fn abs_path(&self, rel: &RelPath) -> String {
        upload::abs_of(&self.root, rel)
    }

    /// transport 面整文件上传（B3b 段一）：委派 upload::upload_whole_file
    ///（[`UPLOAD_WORKERS`] 并发 superfile2 + K7 会话表与 stager 共用）。
    pub(crate) async fn upload_whole_file(
        &self,
        rel: &RelPath,
        data: bytes::Bytes,
    ) -> Result<Entry, StorageError> {
        upload::upload_whole_file(
            &self.client,
            &self.volume,
            &self.root,
            rel,
            data,
            &self.sessions,
            &self.handles,
        )
        .await
    }

    /// 后端绝对路径 → RelPath（root 前缀剥离；不可剥离/非法形态 → None
    /// ——list 对之跳过，防御后端异常回显）。
    fn rel_from_abs(&self, abs: &str) -> Option<RelPath> {
        let stripped = if abs == self.root.as_str() {
            ""
        } else {
            let prefix = if self.root == "/" {
                "/".to_string()
            } else {
                format!("{}/", self.root)
            };
            abs.strip_prefix(&prefix)?
        };
        RelPath::new(stripped).ok()
    }

    /// 递归删除目录子树（深度优先：子文件/子目录 → 自身；单条
    /// filemanager delete 语义 × N 次；Box::pin 引入间接层支持递归）。
    fn delete_tree<'a>(
        &'a self,
        abs_dir: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            let children = api::list(&self.client, abs_dir).await?;
            for child in children {
                if child.isdir != 0 {
                    self.delete_tree(&child.path).await?;
                } else {
                    api::filemanager_delete(&self.client, &child.path).await?;
                }
            }
            api::filemanager_delete(&self.client, abs_dir).await
        })
    }

    /// 收集目录子树的后端绝对路径清单（rename 目录腿用；move 前快照）。
    fn collect_subtree<'a>(
        &'a self,
        abs_dir: &'a str,
        out: &'a mut Vec<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            let children = api::list(&self.client, abs_dir).await?;
            for child in children {
                let path = child.path.clone();
                if child.isdir != 0 {
                    self.collect_subtree(&path, out).await?;
                }
                out.push(path);
            }
            Ok(())
        })
    }

    /// 按解析条目执行删除（delete 的操作相；陈旧纠偏重试共用）：
    /// 目录 = 客户端递归（trait 契约「目录删除为递归」——深度优先删子树
    /// 再删自身；mock 的 filemanager 钉了单条语义，真实后端的单调用递归
    /// 形态留待真机窗口复核，多几次调用无语义差异）。
    async fn delete_resolved(&self, remote: &api::RemoteEntry) -> Result<(), StorageError> {
        if remote.isdir != 0 {
            return self.delete_tree(&remote.path).await;
        }
        api::filemanager_delete(&self.client, &remote.path).await
    }

    /// fs_id → 后端条目解析（delete 句柄 / reader dlink 签发共用；真网
    /// 31300/31023 裁决——meta fs_ids 直查停用）：句柄缓存命中 → 未命中
    /// 递归扫描卷根。
    async fn resolve_handle(&self, fs_id: i64) -> Result<api::RemoteEntry, StorageError> {
        if let Some(hit) = self.handles.get(fs_id) {
            return Ok(hit);
        }
        self.scan_dir(&self.root.clone(), fs_id).await
    }

    /// 递归 list 扫描（卷根深度优先；冷句柄的解析兜底）：逐目录列举
    /// （顺带批量填充句柄缓存）→ 本层 fs_id 匹配即返回 → 目录下沉递归。
    /// 只扫本卷子树（卷外回显经 rel_from_abs 过滤——防御）；全树无 →
    /// `NotFound`。
    ///
    /// 子目录列举撞 `NotFound`（并发删除竞态）跳过继续；其他错误（网络/
    /// 鉴权）上抛——扫描语义不吞真故障。
    fn scan_dir<'a>(
        &'a self,
        dir: &'a str,
        fs_id: i64,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<api::RemoteEntry, StorageError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let children = api::list(&self.client, dir).await?;
            self.handles.put_batch(&children);
            for child in children {
                if child.fs_id == fs_id {
                    return Ok(child);
                }
                if child.isdir != 0 && self.rel_from_abs(&child.path).is_some() {
                    match self.scan_dir(&child.path, fs_id).await {
                        Ok(hit) => return Ok(hit),
                        Err(StorageError::NotFound) => continue, // 并发删除竞态
                        Err(e) => return Err(e),
                    }
                }
            }
            Err(StorageError::NotFound)
        })
    }

    /// RemoteEntry + RelPath → Entry（list/stat 共用换算面）。
    ///
    /// - id.handle = fs_id 十进制字符串（K5）；
    /// - 目录 size 恒 0（模块文档语义声明）；
    /// - mtime = server_mtime（f64 epoch 秒，interfaces §5）。
    fn entry_from_remote(&self, remote: &api::RemoteEntry, rel: RelPath) -> Entry {
        let is_dir = remote.isdir != 0;
        Entry {
            id: EntryId::new(
                self.volume.clone(),
                BackendHandle::new(remote.fs_id.to_string()),
            ),
            path: rel,
            kind: if is_dir {
                EntryKind::Dir
            } else {
                EntryKind::File
            },
            size: if is_dir { 0 } else { remote.size.max(0) as u64 },
            mtime: remote.server_mtime as f64,
        }
    }
}

#[async_trait]
impl StorageDriver for BaiduDriver {
    fn volume(&self) -> &VolumeId {
        &self.volume
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // 断言②全套绿（半开/钳制/空窗口/start>=size=空流）——
            // download.rs 的 4MiB 有界窗口拼接对上层透明。
            range_read: true,
            // K7 差集可观测（到齐 drop → 再 writer 0 满块重传，仅补尾块
            // ——`tests/upload_resume.rs` 差集断言钉死）；upload.rs 会话表
            // （内存 + sessions_dir 磁盘双层）。注：conformance ⑦ 现行
            // 场景（**未到齐**部分写 drop）在「到齐即传」（真网 31363
            // 裁决）下首段零上传、次段全量，撞 ⑦ 的按 staging 对齐差集
            // 上界——待 harness 场景适配裁决（2026-09-08 B2 返工挂账）。
            resume: true,
            // superfile2 4MiB 分片后端原生分块（三步曲主体）。
            multipart: true,
            // filemanager opera=move 后端单侧搬移（ondup=overwrite；不
            // 轮询 taskid——两源一致）；断言⑥文件+目录搬移绿。
            server_side_move: true,
            // precreate return_type=2 秒传路径已实现并有测试钉死
            // （`tests/upload_resume.rs` rapid 用例）；spike §4 实证此
            // appkey 桶不触发——声明不依赖（正确性不建立在秒传上）。
            rapid_upload: true,
            // list 即网盘真相（无影子索引，D4；断言③）。
            authoritative_index: true,
            // 百度网盘无变更推送通道（拉取式后端）。
            change_feed: false,
            // 无 bot 入站通道（telegram 族独有形态）。
            inbound: false,
            // 无对话通道。
            chat: false,
            // K4（B3b transport 面）：delete_remote 经本驱动 delete 真删
            // 网盘对象（fs_id 解析 + filemanager delete，transport_face.rs
            // 薄壳委派）；StorageDriver.delete 的真删语义不变。
            remote_delete: true,
        }
    }

    async fn list(&self, dir: &RelPath, page: Page) -> Result<Listing, StorageError> {
        let remote = api::list(&self.client, &self.abs_path(dir)).await?;
        // 顺带批量填充句柄缓存（一次 list 一次锁——真网 31300 裁决的
        // 解析层资产；卷外回显条目一并入缓存无害：本卷签发的句柄空间
        // 恒在卷内）。
        self.handles.put_batch(&remote);
        let mut entries: Vec<Entry> = remote
            .iter()
            .filter_map(|e| {
                // 卷外路径/非法形态跳过（模块文档语义声明）
                self.rel_from_abs(&e.path)
                    .map(|rel| self.entry_from_remote(e, rel))
            })
            .collect();
        // RelPath 字典序稳定排序（trait 契约；后端返回序是实现细节）
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        // 内部 offset 游标切 Page（后端无分页参数——模块文档注源）；
        // 游标 `off:{end}` 不透明令牌（ck-local/mock 先例），伪令牌回退 0。
        let total = entries.len();
        let offset = match page.cursor {
            PageCursor::Start => 0usize,
            PageCursor::Next(tok) => tok
                .strip_prefix("off:")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
        };
        let offset = offset.min(total); // 伪超大 offset 钳制
        let end = offset.saturating_add(page.limit).min(total);
        let next = (end < total).then(|| PageCursor::Next(format!("off:{end}")));
        Ok(Listing {
            entries: entries.drain(offset..end).collect(),
            next,
        })
    }

    async fn stat(&self, path: &RelPath) -> Result<Entry, StorageError> {
        // 真网 31300 实证驱动（2026-09-08 第四轮返工）：meta&path 在此
        // appkey 下持续无权限（"stream type is not authorized"，轮询 10s
        // 不可见——非延迟）；list 全程即时可用。stat = **list 父目录 +
        // path 精确匹配**（父目录不存在 → list -9 → NotFound 语义保持；
        // conformance ⑤ 的注入回放语义在新主路径下自动成立——注入顶
        // 「下一个业务请求」即此 list）。
        let abs = self.abs_path(path);
        let entries = api::list(&self.client, &api::parent_abs(&abs)).await?;
        self.handles.put_batch(&entries);
        let remote = entries
            .into_iter()
            .find(|e| e.path == abs)
            .ok_or(StorageError::NotFound)?;
        // Entry.path 用请求时的 RelPath（匹配条目的回显 path 与拼接 abs
        // 同值，直接复用入参省一次剥离）。
        Ok(self.entry_from_remote(&remote, path.clone()))
    }

    async fn mkdir(&self, path: &RelPath) -> Result<(), StorageError> {
        if path.is_root() {
            return Err(StorageError::Exists); // 卷根本就存在（ck-local/mock 同款语义）
        }
        // **list 预检**（真网实证 2026-09-08 第五轮）：目录 create 撞已存在
        // ≠ -8——返回 errno=0（成功假象）且远端生成 `<名>_<时间戳>` 空副本
        // （垃圾）。逐级隐式建父目录（trait 契约；xpan create 不自动建父
        // ——mock 严格语义钉死）必须每层「先 list 父目录，已存在 → 跳过
        // create（最终层 → Exists），不存在才 create」——这是不产垃圾的
        // 唯一途径；-8 分支保留为防御语义（真网未观察到但语义安全）。
        let comps: Vec<&str> = path.components().collect();
        let last = comps.len() - 1;
        let mut prefix = RelPath::root();
        for (i, comp) in comps.into_iter().enumerate() {
            prefix = prefix.join(comp)?;
            let abs = self.abs_path(&prefix);
            let siblings = api::list(&self.client, &api::parent_abs(&abs)).await?;
            self.handles.put_batch(&siblings); // 预检流量顺带批量喂句柄缓存
            if siblings.iter().any(|e| e.path == abs) {
                if i == last {
                    return Err(StorageError::Exists); // 目标名已被占用（目录/文件占位同斥）
                }
                continue; // 中间层已存在：零 create 下沉（ghost 免疫）
            }
            match api::create_dir(&self.client, &abs).await {
                Ok(()) => {}
                Err(StorageError::Exists) if i != last => {} // 防御（真网实证 errno=0 形态，-8 不触发）
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn delete(&self, id: &EntryId) -> Result<(), StorageError> {
        // 他卷句柄 → NotFound（trait 契约，ck-local 同款）
        if id.volume != self.volume {
            return Err(StorageError::NotFound);
        }
        // 句柄 = fs_id 十进制字符串（K5）；不可解析 → NotFound（conformance
        // 断言④：不存在的句柄恒 NotFound——非数字形态在本卷 fs_id 空间
        // 视角即不存在）
        let Ok(fs_id) = id.handle.as_str().parse::<i64>() else {
            return Err(StorageError::NotFound);
        };
        // fs_id → path 解析两级（真网实证 #3：filemanager delete 只支持
        // path 形态；#1/#2：meta 直查全废）：句柄缓存 → 递归扫描。
        let remote = self.resolve_handle(fs_id).await?;
        match self.delete_resolved(&remote).await {
            Err(StorageError::NotFound) => {
                // 缓存陈旧纠偏（decisions 2026-09-08 连带项）：rename 后
                // 缓存条目指向旧路径 → filemanager 撞 -9——失效 + 重扫 +
                // 重试一次（防陈旧缓存删错/删空）。扫描也无 → NotFound
                // （对象确已不在，幂等语义保持）。
                self.handles.invalidate(fs_id);
                let fresh = self.resolve_handle(fs_id).await?;
                self.delete_resolved(&fresh).await
            }
            res => res,
        }?;
        // 成功后失效本句柄缓存条目（防复删走陈旧路径多绕一轮纠偏；
        // 目录删除的子条目残留条目由各自纠偏腿自愈——有界且自修正）。
        self.handles.invalidate(fs_id);
        Ok(())
    }

    async fn rename(&self, from: &RelPath, to: &RelPath) -> Result<(), StorageError> {
        if from.is_root() || to.is_root() {
            return Err(StorageError::Invalid); // 卷根不可作为 rename 端点
        }
        // 目标是源的后代 → Invalid（trait 契约；目录搬进自身会成环）
        let from_prefix = format!("{}/", from.as_str());
        if to.as_str().starts_with(&from_prefix) {
            return Err(StorageError::Invalid);
        }
        let dest_dir = self.abs_path(&to.parent().expect("非根路径必有父"));
        let new_name = to.file_name().expect("非根路径必有文件名");
        let src_abs = self.abs_path(from);
        // 源不存在 → NotFound（trait 契约；kind 决定文件腿/目录腿）。
        let st = self.stat(from).await?;
        if st.kind == EntryKind::File {
            // 文件腿：单次 filemanager move（wire 契约钉死——metadata_ops
            // 断言恰一次调用）。
            return api::filemanager_move(&self.client, &src_abs, &dest_dir, new_name).await;
        }
        // 目录腿：**客户端递归搬移**（conformance ⑥ 钉死子树跟随语义）——
        // 自身 move 先行 + 子条目按新前缀补搬（move 前收集子树清单）。
        // 子条目 move 撞 `NotFound` = 旧路径已空 → 真机后端单调用自带
        // 递归（子树已被搬过）的形态，容错跳过——mock 单条语义与真机
        // 递归语义双兼容（mock：逐条 Ok；真机：自身 move 即全搬，补搬
        // 全 -9 跳过）。
        let mut subtree = Vec::new();
        self.collect_subtree(&src_abs, &mut subtree).await?;
        api::filemanager_move(&self.client, &src_abs, &dest_dir, new_name).await?;
        let dst_abs = self.abs_path(to);
        let old_prefix = format!("{src_abs}/");
        let new_prefix = format!("{dst_abs}/");
        for child_abs in subtree {
            let Some(rest) = child_abs.strip_prefix(&old_prefix) else {
                continue; // 防御：收集自 move 前的快照，理论上必含前缀
            };
            let new_abs = format!("{new_prefix}{rest}");
            let (parent, name) = new_abs
                .rsplit_once('/')
                .map(|(p, n)| (p.to_string(), n.to_string()))
                .expect("绝对路径必有分隔符");
            match api::filemanager_move(&self.client, &child_abs, &parent, &name).await {
                Ok(()) => {}
                Err(StorageError::NotFound) => {} // 后端递归已搬（真机形态）
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn reader(&self, id: &EntryId, range: Option<Range>) -> Result<ByteStream, StorageError> {
        // 委派 download.rs：fs_id 句柄解析（缓存→扫描——真网 31300 裁决，
        // meta 直查停用）→ dlink 缓存 → 4MiB 有界分片流（两段 fallback
        // 内嵌分片拉取路径）。
        if id.volume != self.volume {
            return Err(StorageError::NotFound); // 他卷句柄（trait 契约）
        }
        let fs_id: i64 = id
            .handle
            .as_str()
            .parse()
            .map_err(|_| StorageError::Invalid)?;
        let remote = self.resolve_handle(fs_id).await?; // 全树无 → NotFound
        download::open_range(&self.client, &self.dlinks, fs_id, &remote, range).await
    }

    async fn writer(
        &self,
        path: &RelPath,
        hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        // 委派 upload.rs：目标已存在目录在此判 `Invalid`（父目录 list 预检
        // ——真网 31300 裁决，meta 预检停用），三步曲/会话恢复延迟到首次
        // 网络动作（内容 md5 依赖数据到达）。
        upload::open_writer(
            &self.client,
            &self.volume,
            &self.root,
            &self.abs_path(path),
            path,
            hint,
            &self.sessions,
            &self.handles,
        )
        .await
    }

    async fn quota(&self) -> Result<Quota, StorageError> {
        let (used, total) = api::quota(&self.client).await?;
        Ok(Quota {
            total: Some(total.max(0) as u64),
            used: used.max(0) as u64,
        })
    }
}
