//! LocalStager——本地文件系统上传暂存器（Batch L 红阶段骨架）。
//!
//! commit-on-close 语义（interfaces §2）绿阶段落地：`write` 暂存、
//! `close` 固化为远端（目标路径）对象、`abort`/Drop 清理暂存不留垃圾。
//! 当前三方法全部 `Unsupported` 占位。

use async_trait::async_trait;

use cloudkit_storage::{Entry, StorageError, UploadStager};

/// 本地文件系统上传暂存器（绿阶段持有目标路径与暂存状态）。
pub struct LocalStager;

#[async_trait]
impl UploadStager for LocalStager {
    async fn write(&mut self, _data: &[u8]) -> Result<(), StorageError> {
        Err(StorageError::Unsupported) // 断言①⑦
    }

    async fn close(self: Box<Self>) -> Result<Entry, StorageError> {
        Err(StorageError::Unsupported) // 断言①
    }

    async fn abort(self: Box<Self>) -> Result<(), StorageError> {
        Err(StorageError::Unsupported)
    }
}
