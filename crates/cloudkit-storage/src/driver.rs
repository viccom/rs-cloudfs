//! StorageDriver——层边界主 trait（foundation D1 / interfaces §1-§2）。
//!
//! 层位置：L2（不依赖任何 workspace crate、不依赖任何驱动；运行时
//! 后端无关——R1/R2）。

use async_trait::async_trait;

use crate::capability::Capabilities;
use crate::error::StorageError;
use crate::ids::{EntryId, VolumeId};
use crate::stager::UploadStager;
use crate::vocab::{ByteStream, Entry, Listing, Page, Quota, Range, RelPath, WriteHint};

/// 多云存储统一驱动契约。
///
/// 通用规则（各方法另有个别契约）：
/// - **错误**：一切失败映射为 [`StorageError`]（R2）；未知后端错误保留
///   原始码与消息进 `Io`/`Unavailable` 载荷；
/// - **并发**：所有方法可并发调用（`&self` + 内部同步）；同一 stager
///   串行使用；
/// - **生命周期**：连接/会话/token 刷新归驱动自理（见
///   [`crate::optional::TokenEvents`] 的持久化回调）；调用方只负责
///   stager 的 close/abort；
/// - **路径语义**：[`RelPath`] 卷内相对路径；写入路径的缺失父目录由
///   驱动隐式创建（与 mkdir 的自动父目录一致）；
/// - **分块策略归驱动**：上层只见 [`Entry`] 与字节流。
#[async_trait]
pub trait StorageDriver: Send + Sync {
    /// 本驱动服务的卷身份（D6：ID 从第一天带卷）。
    fn volume(&self) -> &VolumeId;

    /// 能力位声明（R4：必须诚实——只声明经 conformance 验证的位）。
    fn capabilities(&self) -> Capabilities;

    /// 列目录（depth-1，仅直接子条目），按 [`RelPath`] 字典序稳定有序。
    ///
    /// 错误：目录不存在 → `NotFound`。分页语义统一在 [`Page`]；
    /// 游标/offset 型后端都归一到 [`Listing::next`] 回吐的不透明令牌。
    async fn list(&self, dir: &RelPath, page: Page) -> Result<Listing, StorageError>;

    /// 取条目元数据。
    ///
    /// 错误：不存在 → `NotFound`。
    async fn stat(&self, path: &RelPath) -> Result<Entry, StorageError>;

    /// 创建目录（含缺失父目录的隐式创建）；目标已存在 → `Exists`
    /// （conformance 断言④）。
    async fn mkdir(&self, path: &RelPath) -> Result<(), StorageError>;

    /// 按句柄删除条目；目录删除为递归。
    ///
    /// 幂等语义二选一并恒定（断言④）：删除不存在句柄 → `NotFound`，
    /// 或幂等 `Ok(())`；驱动必须在文档中声明所选形态。
    /// 他卷句柄 → `NotFound`。
    async fn delete(&self, id: &EntryId) -> Result<(), StorageError>;

    /// 移动/重命名（文件与目录）。
    ///
    /// 错误：源不存在 → `NotFound`；目标已存在 → `Exists`；
    /// 目标是源的后代 → `Invalid`。声明 `SERVER_SIDE_MOVE` 时为后端单侧
    /// 操作（无重传），否则允许降级 copy+delete。
    async fn rename(&self, from: &RelPath, to: &RelPath) -> Result<(), StorageError>;

    /// 打开读取流；`range = None` 整读，`Some` 时按半开区间语义
    /// （end 越界钳制到 EOF；`start >= size` 的行为与驱动声明一致，
    /// 见 conformance 断言②）。
    ///
    /// 错误：句柄不存在 → `NotFound`；句柄指向目录 → `Invalid`；
    /// 未声明 `RANGE_READ` 却收到 `Some(range)` → `Unsupported`。
    async fn reader(&self, id: &EntryId, range: Option<Range>) -> Result<ByteStream, StorageError>;

    /// 打开上传暂存器（commit-on-close，见 [`UploadStager`]）。
    ///
    /// 错误：目标路径是已存在目录 → `Invalid`。
    async fn writer(
        &self,
        path: &RelPath,
        hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError>;

    /// 卷配额视图；后端无配额概念时 `total = None`。
    async fn quota(&self) -> Result<Quota, StorageError>;
}
