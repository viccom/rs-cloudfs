//! 上传暂存器（interfaces §2：commit-on-close 语义）。
//!
//! **写入只是 staging**：`write` 阶段不产生任何远端可见对象；只有
//! [`UploadStager::close`]（commit）才在目标路径产生远端对象——与
//! WebDAV flush=PUT、PCFS create/close 同构，统一于此。

use async_trait::async_trait;

use crate::error::StorageError;
use crate::vocab::Entry;

/// 暂存写入器（由 [`crate::driver::StorageDriver::writer`] 创建）。
///
/// 语义契约：
/// - **commit-on-close**：`close()` 前目标路径对 `stat`/`list` 不可见
///   （conformance 断言①钉死）；
/// - **staging 可丢弃不留垃圾**：`abort()` 显式放弃暂存，驱动必须保证
///   远端无孤儿对象；不带 `close`/`abort` 的 Drop 等价于放弃——对声明了
///   RESUME 能力的驱动，Drop 允许保留**上传会话**（已传分片，供差集续传，
///   不是远端可见对象）；未声明 RESUME 的驱动 Drop 即全量丢弃；
/// - `write` 按到达顺序追加，调用方负责顺序性；单 stager 不要求并发写。
///
/// 错误语义：中途任何 `Err` 后 stager 进入未定义状态，调用方应 `abort`
/// 或 Drop，不得继续 `close`。
///
/// 并发语义：同一 stager 串行使用；不同 stager（不同路径）可并行。
///
/// 生命周期：stager 持有驱动侧会话资源直到 close/abort/Drop。
#[async_trait]
pub trait UploadStager: Send {
    /// 追加一段数据到暂存区。
    async fn write(&mut self, data: &[u8]) -> Result<(), StorageError>;

    /// 提交：把暂存内容固化为远端对象并返回其 [`Entry`]。
    /// 若 [`crate::vocab::WriteHint::size`] 曾给出且与实际不符，
    /// 驱动应返回 `Invalid`。
    async fn close(self: Box<Self>) -> Result<Entry, StorageError>;

    /// 放弃暂存：不留远端垃圾（含 RESUME 会话一并清除）。
    async fn abort(self: Box<Self>) -> Result<(), StorageError>;
}
