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
//! - **delete 幂等形态**：删除不存在句柄 → `NotFound`（B2 conformance
//!   断言④按此跑；xpan 删除走回收站 10 天是后端已知限制，非驱动语义）；
//! - **list**：depth-1、按 [`RelPath`] 字典序稳定有序、内部 offset 游标
//!   切 [`Page`]（后端无分页参数——spike api.rs:131-148 实证）；
//! - **mtime**：读 `server_mtime`（api.rs 模块文档注源）；
//! - **writer/reader**：B1 恒 `Unsupported`（B2 接三步曲上传/下载器）；
//! - **capabilities**：B1 骨架 `Capabilities::none()`——逐位点亮与注码
//!   归 B2（R4：只声明经 conformance 验证的位）。
//!
//! 错误映射表（errno 逐码注源）见 [`crate::api`] 模块文档。

use async_trait::async_trait;

use cloudkit_storage::{
    ByteStream, Capabilities, Entry, EntryId, Listing, Page, Quota, Range, RelPath, StorageDriver,
    StorageError, UploadStager, VolumeId, WriteHint,
};

use crate::api;
use crate::client::BaiduClient;
use crate::BaiduParams;

/// 百度网盘驱动。
#[allow(dead_code)] // 红骨架：绿阶段元数据面读取 root/client 后移除
pub struct BaiduDriver {
    volume: VolumeId,
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
        Ok(BaiduDriver {
            volume,
            root: params.root.clone(),
            client,
        })
    }
}

#[async_trait]
impl StorageDriver for BaiduDriver {
    fn volume(&self) -> &VolumeId {
        &self.volume
    }

    fn capabilities(&self) -> Capabilities {
        // B1 骨架：全 false。逐位点亮（authoritative_index/server_side_move/
        // range_read/resume/multipart/rapid_upload）与注码归 B2（R4）。
        Capabilities::none()
    }

    async fn list(&self, dir: &RelPath, page: Page) -> Result<Listing, StorageError> {
        let _ = (dir, page);
        // B1 红 → 绿③：api::list + root 前缀剥离 + 字典序 + Page 切片；
        // Entry/BackendHandle/EntryKind 届时经 api::RemoteEntry 换算。
        Err(StorageError::Unsupported)
    }

    async fn stat(&self, path: &RelPath) -> Result<Entry, StorageError> {
        let _ = path;
        Err(StorageError::Unsupported) // B1 红 → 绿③：api::meta_by_path
    }

    async fn mkdir(&self, path: &RelPath) -> Result<(), StorageError> {
        let _ = path;
        Err(StorageError::Unsupported) // B1 红 → 绿③：api::create_dir（isdir=1）
    }

    async fn delete(&self, id: &EntryId) -> Result<(), StorageError> {
        let _ = id;
        Err(StorageError::Unsupported) // B1 红 → 绿③：句柄解析 + api::filemanager_delete（info[] 逐项）
    }

    async fn rename(&self, from: &RelPath, to: &RelPath) -> Result<(), StorageError> {
        let _ = (from, to);
        Err(StorageError::Unsupported) // B1 红 → 绿③：api::filemanager_move
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
        Err(StorageError::Unsupported) // B1 红 → 绿③：api::quota（total=Some）
    }
}
