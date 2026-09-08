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
//! - **delete 幂等形态**：删除不存在句柄 → `NotFound` 恒定（conformance
//!   断言④钉死——不可解析 fs_id 视同不存在句柄，从本卷 fs_id 空间
//!   视角即不存在）；解析走 `method=meta&fs_ids=[<id>]` 直查——PCFS
//!   api.go:176-179 姊妹形态，相对「本地缓存 path」方案少一份驱动内
//!   状态；xpan 删除走回收站 10 天是后端已知限制，非驱动语义）；
//! - **list**：depth-1、按 [`RelPath`] 字典序稳定有序、内部 offset 游标
//!   切 [`Page`]（后端无分页参数——spike api.rs:131-148 实证；游标
//!   `off:{end}` 形态，ck-local/mock 先例）；卷外路径条目（后端异常回显）
//!   跳过不透出；
//! - **mtime**：读 `server_mtime`（api.rs 模块文档注源）；
//! - **目录 size 恒 0**：后端对目录返回的实现值不透出（ck-local 同款）；
//! - **mkdir**：逐级隐式创建（xpan create 不自动建父目录，mock 严格语义
//!   钉死）；中间层已存在（-8）继续下沉，最终层已存在 → `Exists`；
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

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use cloudkit_storage::{
    BackendHandle, ByteStream, Capabilities, Entry, EntryId, EntryKind, Listing, Page, PageCursor,
    Quota, Range, RelPath, StorageDriver, StorageError, UploadStager, VolumeId, WriteHint,
};

use crate::api;
use crate::client::BaiduClient;
use crate::{download, upload, BaiduParams};

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
        })
    }

    /// RelPath → 后端绝对路径（root 前缀拼接；根目录即 root 本身）。
    fn abs_path(&self, rel: &RelPath) -> String {
        if rel.is_root() {
            return self.root.clone();
        }
        if self.root == "/" {
            format!("/{}", rel.as_str())
        } else {
            format!("{}/{}", self.root, rel.as_str())
        }
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
            // 断言⑦绿（K7 差集可观测：drop → 再 writer 只补缺失分片，
            // `backend_bytes_received` 上界断言过）——upload.rs 会话表
            // （内存 + sessions_dir 磁盘双层）。
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
        }
    }

    async fn list(&self, dir: &RelPath, page: Page) -> Result<Listing, StorageError> {
        let remote = api::list(&self.client, &self.abs_path(dir)).await?;
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
        let remote = api::meta_by_path(&self.client, &self.abs_path(path)).await?;
        // Entry.path 用请求时的 RelPath（后端回显 path 与拼接 abs 同值，
        // 直接复用入参省一次剥离；-9 已在 client 层归一 NotFound）
        Ok(self.entry_from_remote(&remote, path.clone()))
    }

    async fn mkdir(&self, path: &RelPath) -> Result<(), StorageError> {
        if path.is_root() {
            return Err(StorageError::Exists); // 卷根本就存在（ck-local/mock 同款语义）
        }
        // 逐级隐式建父目录（trait 契约；xpan create 不自动建父——mock 严格
        // 语义钉死）：浅层先行，中间层 -8（已存在）继续下沉，最终层 -8 →
        // Exists（模块文档语义声明）。
        let comps: Vec<&str> = path.components().collect();
        let last = comps.len() - 1;
        let mut prefix = RelPath::root();
        for (i, comp) in comps.into_iter().enumerate() {
            prefix = prefix.join(comp)?;
            match api::create_dir(&self.client, &self.abs_path(&prefix)).await {
                Ok(()) => {}
                Err(StorageError::Exists) if i != last => {}
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
        if id.handle.as_str().parse::<i64>().is_err() {
            return Err(StorageError::NotFound);
        }
        // fs_id → path 解析：meta fs_ids 直查（实现裁决见模块文档「delete
        // 幂等形态」；-9 → NotFound）
        let remote = api::meta_by_fs_id(&self.client, id.handle.as_str()).await?;
        if remote.isdir != 0 {
            // 目录删除 = 客户端递归（trait 契约「目录删除为递归」）：
            // 深度优先删子树再删自身——mock 的 filemanager 钉了单条
            // 语义，真实后端的单调用递归形态留待真机窗口复核（多几次
            // 调用无语义差异）。
            return self.delete_tree(&remote.path).await;
        }
        api::filemanager_delete(&self.client, &remote.path).await
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
        // 委派 download.rs：fs_id 句柄 → meta（path/size）→ dlink 缓存 →
        // 4MiB 有界分片流（两段 fallback 内嵌分片拉取路径）。
        download::open_range(&self.client, &self.dlinks, &self.volume, id, range).await
    }

    async fn writer(
        &self,
        path: &RelPath,
        hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        // 委派 upload.rs：目标已存在目录在此判 `Invalid`（meta 预检），
        // 三步曲/会话恢复延迟到首次网络动作（内容 md5 依赖数据到达）。
        upload::open_writer(
            &self.client,
            &self.volume,
            &self.root,
            &self.abs_path(path),
            path,
            hint,
            &self.sessions,
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
