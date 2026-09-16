//! CloudTransport 面（Phase 5 / 115-4；照 ck-sftp/ck-local 薄壳形态）。
//!
//! pan115 的 transport 面是 [`Pan115Driver`] 的薄壳：与 StorageDriver 面
//! 共享同一驱动实现与 token/限流世界（同一 `Arc<Pan115Client>` 经
//! driver 持有），不重复任何协议逻辑。与 sftp/local 的路径寻址不同，
//! 115 是 **id 寻址**（folder id / 复合句柄），所以：
//!
//! - **句柄**：`RemoteHandle.path` 给的是 vpath（卷内路径），本面把它
//!   经驱动的解析层换成 [`EntryId`]（命中缓存零网络；未命中 list 下行）
//!   ——`path = None` → `Invalid`；`first_msg_id`/`chunk_msg_ids` 是
//!   不语义化的占位（receipt 恒 0 / `[0]`——K11 簿记与 rebuild 契约）；
//! - **connect**：`user/info` 一次（token 活力的探活；失败按
//!   `StorageError` 映射归一——401* → `Unauthorized{recoverable:true}`）；
//! - **删除幂等形态**：句柄缺失 → `NotFound`（驱动 delete 经
//!   `ufile/delete`，D2：进回收站语义）；
//! - **字节预算**：`open`/`open_range` 以 `total_size` 为窗口上界
//!   （E-5 预算帽语义）；EOF 钳制归驱动 reader（115 的 CDN 窗口流）。
//!
//! ByteStream 适配：storage 面（Send）与 transport 面（Send+Sync）是
//! 两个类型——经 [`SyncStream`] 的 Mutex 桥接（ck-local/ck-sftp 同款
//! 最小适配）。

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;

use cloudkit_storage::transport::{
    ByteStream, CloudTransport, RemoteHandle, StorageError, UploadJob, UploadReceipt,
};
use cloudkit_storage::vpath::RelPath as VPath;
use cloudkit_storage::{Capabilities, EntryId, Range, RelPath, StorageDriver, WriteHint};

use crate::Pan115Driver;

/// vpath（CyDrive VFS 绝对式，transport 载荷）→ vocab（卷内相对，
/// StorageDriver 词汇）RelPath 转换（ck-local/ck-sftp 同款私换算点）。
fn vocab_rel(path: &VPath) -> Result<RelPath, StorageError> {
    RelPath::new(path.as_str().trim_start_matches('/'))
}

/// 句柄寻址：vpath → 卷内 [`EntryId`]（115 是 id 寻址——解析层把路径
/// 换成复合句柄 `fid:pc:parent`；`stat` 的产出与 list 行同形）。
async fn entry_id(driver: &Pan115Driver, handle: &RemoteHandle) -> Result<EntryId, StorageError> {
    let path = handle.path.as_ref().ok_or(StorageError::Invalid)?;
    let rel = vocab_rel(path)?;
    let entry = driver.stat(&rel).await?;
    Ok(entry.id)
}

/// storage ByteStream（Send）→ transport ByteStream（Send + Sync）的
/// 最小适配（std Mutex 引入 Sync，poll 内短临界区、永不跨 await；
/// 毒锁恢复继续比卡死正确——ck-local/ck-sftp 同款）。
struct SyncStream {
    inner: std::sync::Mutex<cloudkit_storage::ByteStream>,
}

impl Stream for SyncStream {
    type Item = Result<Bytes, StorageError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut guard = self
            .get_mut()
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.as_mut().poll_next(cx)
    }
}

/// 把 storage 面流适配成 transport 面流。
fn sync_stream(inner: cloudkit_storage::ByteStream) -> ByteStream {
    Box::pin(SyncStream {
        inner: std::sync::Mutex::new(inner),
    })
}

/// transport 面空流（want == 0 窗口）。
fn empty_stream() -> ByteStream {
    let frames: Vec<Result<Bytes, StorageError>> = Vec::new();
    Box::pin(futures_util::stream::iter(frames))
}

/// CloudTransport 薄壳：持有驱动经 `Arc` 与 StorageDriver 装配共享。
pub struct Pan115Transport {
    driver: Arc<Pan115Driver>,
}

impl Pan115Transport {
    /// 以既有驱动实例构造 transport 面（两面共享同一后端状态）。
    pub fn new(driver: Arc<Pan115Driver>) -> Self {
        Pan115Transport { driver }
    }

    /// 底层驱动（两面互访）。
    pub fn driver(&self) -> &Pan115Driver {
        &self.driver
    }

    /// 归还驱动句柄（组合根双面装配共用一个 `Arc<Pan115Driver>`）。
    pub fn into_driver(self) -> Arc<Pan115Driver> {
        self.driver
    }
}

#[async_trait]
impl CloudTransport for Pan115Transport {
    /// 探活：`user/info` 一次（token 活力的探活；失败按驱动映射归一
    /// ——401* → `Unauthorized{recoverable:true}`，770004 → `RateLimited`）。
    async fn connect(&self) -> Result<(), StorageError> {
        self.driver.client().user_info().await.map(|_| ())
    }

    /// 整文件复制上传：读盘 → writer + write 全量 + close。receipt 遵循
    /// K6（`first_msg_id = 0` 占位、`chunk_msg_ids = [0]` 单 chunk 占位）。
    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        let rel = vocab_rel(&job.rel_path)?;
        if rel.is_root() {
            return Err(StorageError::Invalid); // 卷根不可作为上传目标
        }
        let data = tokio::fs::read(&job.local_path)
            .await
            .map_err(|e| StorageError::Io(format!("reading {}: {e}", job.local_path.display())))?;
        let hint = WriteHint {
            size: Some(job.size),
            ..Default::default()
        };
        let mut stager = self.driver.writer(&rel, &hint).await?;
        stager.write(&data).await?;
        let entry = stager.close().await?;
        Ok(receipt_of(entry.size))
    }

    /// 流式上传：字节逐帧进 stager；超计划拒绝、短流由 close 的承诺
    /// 校验给 `Invalid`（ck-local/ck-sftp 同款形态）。
    async fn upload_stream(
        &self,
        job: &UploadJob,
        data: ByteStream,
    ) -> Result<UploadReceipt, StorageError> {
        let rel = vocab_rel(&job.rel_path)?;
        if rel.is_root() {
            return Err(StorageError::Invalid);
        }
        let hint = WriteHint {
            size: Some(job.size),
            ..Default::default()
        };
        let mut stager = self.driver.writer(&rel, &hint).await?;
        let mut frames = data;
        let mut written: u64 = 0;
        while let Some(frame) = futures_util::StreamExt::next(&mut frames).await {
            let frame = frame?;
            written += frame.len() as u64;
            if written > job.size {
                return Err(StorageError::Unavailable(format!(
                    "stream exceeded the planned {} bytes",
                    job.size
                )));
            }
            stager.write(&frame).await?;
        }
        let entry = stager.close().await?;
        Ok(receipt_of(entry.size))
    }

    /// 全文件读：预算帽语义下等价 `open_range(0, total_size)`。
    async fn open(&self, file: &RemoteHandle) -> Result<ByteStream, StorageError> {
        self.open_range(file, 0, file.total_size).await
    }

    /// 半开窗口 `[off, min(off+len, total_size))`（预算帽；EOF 之下的
    /// 钳制由驱动 reader 承担）。`start >= size` / 空窗口 → 空流。
    async fn open_range(
        &self,
        file: &RemoteHandle,
        off: u64,
        len: u64,
    ) -> Result<ByteStream, StorageError> {
        let id = entry_id(&self.driver, file).await?;
        let want = len.min(file.total_size.saturating_sub(off));
        if want == 0 {
            return Ok(empty_stream());
        }
        let range = Range::new(off, Some(off.saturating_add(want)))?;
        let stream = StorageDriver::reader(self.driver.as_ref(), &id, Some(range)).await?;
        Ok(sync_stream(stream))
    }

    /// 删除句柄指向的卷内对象：薄壳委派 `StorageDriver::delete`（路径
    /// 缺失 → `NotFound` 幂等形态；**D2：进回收站语义**——远端状态真实
    /// 变更，误删恢复走 115 官方端）。
    async fn delete_remote(&self, handle: &RemoteHandle) -> Result<(), StorageError> {
        let id = entry_id(&self.driver, handle).await?;
        self.driver.delete(&id).await
    }

    /// 镜像 StorageDriver 位 + `remote_delete = true`（本面 delete_remote
    /// 真删远端——按 D2 为回收站语义）。
    fn capabilities(&self) -> Capabilities {
        let mut caps = StorageDriver::capabilities(self.driver.as_ref());
        caps.remote_delete = true;
        caps
    }
}

/// K6 receipt 形态：msg_id 恒 0 占位（不语义化，local/ck-sftp 同款）。
fn receipt_of(uploaded_bytes: u64) -> UploadReceipt {
    UploadReceipt {
        first_msg_id: 0,
        // K11 单 chunk 占位：`[0]`
        chunk_msg_ids: vec![0],
        uploaded_bytes,
    }
}
