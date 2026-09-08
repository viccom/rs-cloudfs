//! BaiduDriver——百度网盘 StorageDriver 元数据面（Phase 2 Batch B1）。
//!
//! 后端模型：卷根 [`crate::BaiduParams::root`]（后端绝对路径，缺省
//! `/apps/cloudfs`）下的网盘树；[`RelPath`] 相对路径 ↔ 后端绝对路径的
//! 换算归本驱动（root 前缀不泄漏到聚合层，R1）。
//!
//! ## 语义声明（trait 契约的驱动侧选择）
//!
//! - **句柄**：fs_id 十进制字符串（K5；跨 rename 稳定——PCFS
//!   api.go:170-171 先例）；他卷句柄 → `NotFound`（ck-local 同款契约）；
//!   空/不可解析句柄 → `Invalid`；
//! - **delete 幂等形态**：删除不存在句柄 → `NotFound`（解析走
//!   `method=meta&fs_ids=[<id>]` 直查——PCFS api.go:176-179 姊妹形态，
//!   相对「本地缓存 path」方案少一份驱动内状态；B2 conformance 断言④按
//!   此跑；xpan 删除走回收站 10 天是后端已知限制，非驱动语义）；
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
//!   trait「目标已存在 → Exists」的驱动侧偏离在此显式声明，B2
//!   conformance 断言⑥复核后定稿）；
//! - **writer/reader**：B1 恒 `Unsupported`（B2 接三步曲上传/下载器）；
//! - **capabilities**：B1 骨架 `Capabilities::none()`——逐位点亮与注码
//!   归 B2（R4：只声明经 conformance 验证的位）。
//!
//! 错误映射表（errno 逐码注源）见 [`crate::api`] 模块文档。

use async_trait::async_trait;

use cloudkit_storage::{
    BackendHandle, ByteStream, Capabilities, Entry, EntryId, EntryKind, Listing, Page, PageCursor,
    Quota, Range, RelPath, StorageDriver, StorageError, UploadStager, VolumeId, WriteHint,
};

use crate::api;
use crate::client::BaiduClient;
use crate::BaiduParams;

/// 百度网盘驱动。
pub struct BaiduDriver {
    volume: VolumeId,
    /// 规范化卷根（构造时去尾部 `/`；`/` 本身保留原样）。
    root: String,
    client: BaiduClient,
}

impl BaiduDriver {
    /// 连接构造：uinfo 取 uid → VolumeId `baidu:<uid>`（K5）。
    ///
    /// token 齐备性校验在 [`BaiduClient::new`]；uinfo 失败按 errno 映射
    /// 表归一（110 会在 client 层自救一次——见 `oauth.rs` 状态机）。
    pub(crate) async fn connect(params: &BaiduParams) -> Result<Self, StorageError> {
        let client = BaiduClient::new(params)?;
        let uid = api::uinfo(&client).await?;
        let volume = VolumeId::new("baidu", &uid.to_string())?;
        // 根规范化：去尾部 `/`（`/apps/cloudfs/` 与 `/apps/cloudfs` 同义；
        // 根卷 `/` 本身会清成空串，还原为 `/`）。
        let mut root = params.root.trim_end_matches('/').to_string();
        if root.is_empty() {
            root.push('/');
        }
        Ok(BaiduDriver {
            volume,
            root,
            client,
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
        // B1 骨架：全 false。逐位点亮（authoritative_index/server_side_move/
        // range_read/resume/multipart/rapid_upload）与注码归 B2——B2
        // conformance 全绿后逐位点亮（R4：未经套件验证不声明）。
        Capabilities::none()
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
        // 句柄 = fs_id 十进制字符串（K5）；空/不可解析 → Invalid
        if id.handle.as_str().parse::<i64>().is_err() {
            return Err(StorageError::Invalid);
        }
        // fs_id → path 解析：meta fs_ids 直查（实现裁决见模块文档「delete
        // 幂等形态」；-9 → NotFound）
        let remote = api::meta_by_fs_id(&self.client, id.handle.as_str()).await?;
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
        api::filemanager_move(&self.client, &self.abs_path(from), &dest_dir, new_name).await
    }

    async fn reader(&self, id: &EntryId, range: Option<Range>) -> Result<ByteStream, StorageError> {
        let _ = (id, range);
        Err(StorageError::Unsupported) // B1 阶段永久占位：B2 接 download.rs（dlink 缓存 + 4MiB 分片）
    }

    async fn writer(
        &self,
        path: &RelPath,
        hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        let _ = (path, hint);
        Err(StorageError::Unsupported) // B1 阶段永久占位：B2 接 upload.rs（三步曲 + 差集续传）
    }

    async fn quota(&self) -> Result<Quota, StorageError> {
        let (used, total) = api::quota(&self.client).await?;
        Ok(Quota {
            total: Some(total.max(0) as u64),
            used: used.max(0) as u64,
        })
    }
}
