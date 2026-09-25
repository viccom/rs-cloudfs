//! CloudTransport 面（Phase 4 / SF1；照 ck-local 薄壳形态逐面比对）。
//!
//! SFTP 的 transport 面是 [`SftpDriver`] 的薄壳：与 StorageDriver 面
//! 共享同一驱动实现与连接世界（同一 `Arc<SftpClient>`），不重复任何
//! 协议逻辑——两面差异只在句柄语义（K2/K6，与 local 完全同构）：
//!
//! - **句柄**：路径寻址（[`RemoteHandle::path`]，vpath 形态）；
//!   `first_msg_id`/`chunk_msg_ids` 是不语义化的占位（receipt 恒 0 /
//!   `[0]`——K11 簿记与 rebuild 契约一致）；`path = None` → `Invalid`；
//! - **chunk 计划**：`UploadJob` 的 chunk_count/chunk_size 对单流写入
//!   的 SFTP 无意义，不消费；`job.size` 作为 WriteHint 承诺由 stager
//!   close 校验（不符 → `Invalid`）；
//! - **connect**：SFTP 会话探活（根目录 stat）——惰性连接世界的首次
//!   建立也发生在这里；
//! - **删除幂等形态**：路径缺失 → `NotFound`（telegram deleted==0 →
//!   NotFound 先例；驱动面 stat 预检自然给出）；
//! - **字节预算**：`open`/`open_range` 以 `total_size` 为窗口上界
//!   （E-5 预算帽语义）；EOF 钳制归驱动 reader。
//!
//! ByteStream 适配：storage 面（Send）与 transport 面（Send+Sync）是
//! 两个类型（R-3 迁移记录的并存）——经 [`SyncStream`] 的 Mutex 桥接
//!（ck-local 同款最小适配）。
//!
//! 行为测试在 SF2 桩批（SF1 以编译 + clippy 为门）。

use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use std::pin::Pin;

use cloudkit_storage::transport::{
    ByteStream, CloudTransport, RemoteHandle, StorageError, UploadJob, UploadReceipt,
};
use cloudkit_storage::vpath::RelPath as VPath;
use cloudkit_storage::UploadStager;
use cloudkit_storage::{Capabilities, EntryId, Range, RelPath, StorageDriver, WriteHint};

use crate::driver::SftpDriver;

/// vpath（CyDrive VFS 绝对式，transport 载荷）→ vocab（卷内相对，
/// StorageDriver 词汇）RelPath 转换（ck-local 同款私换算点）。
fn vocab_rel(path: &VPath) -> Result<RelPath, StorageError> {
    RelPath::new(path.as_str().trim_start_matches('/'))
}

/// 句柄寻址（K2/K6）：path → 卷内 EntryId（K6 句柄字符串 = rel_path
/// 形态，与 StorageDriver 面往返一致）。
fn entry_id(driver: &SftpDriver, handle: &RemoteHandle) -> Result<EntryId, StorageError> {
    let path = handle.path.as_ref().ok_or(StorageError::Invalid)?;
    let rel = vocab_rel(path)?;
    Ok(EntryId::new(
        driver.volume().clone(),
        cloudkit_storage::BackendHandle::new(rel.as_str()),
    ))
}

/// storage ByteStream（Send）→ transport ByteStream（Send + Sync）的
/// 最小适配（ck-local 同款：std Mutex 引入 Sync，poll 内短临界区、
/// 永不跨 await；毒锁恢复继续比卡死正确）。
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
pub struct SftpTransport {
    driver: Arc<SftpDriver>,
}

impl SftpTransport {
    /// 以既有驱动实例构造 transport 面（两面共享同一后端状态）。
    pub fn new(driver: Arc<SftpDriver>) -> Self {
        SftpTransport { driver }
    }

    /// 底层驱动（两面互访；`Arc` 克隆经 [`Self::into_driver`]）。
    pub fn driver(&self) -> &SftpDriver {
        &self.driver
    }

    /// 归还驱动句柄（组合根双面装配共用一个 `Arc<SftpDriver>`）。
    pub fn into_driver(self) -> Arc<SftpDriver> {
        self.driver
    }
}

#[async_trait]
impl CloudTransport for SftpTransport {
    /// 探活 = **卷根校验**（复审修复 2026-09-25，负责人真机报障裁定
    /// 「根目录不存在就别带病挂载」）：stat 卷根——不存在/不是目录以
    /// **可行动**错误拒绝（指名 sftp_root 键与路径，装配期的 connect 门
    /// 据此拒绝挂载——报障形态即挂载成功后每个上传 not found 重试到
    /// degrade）。host key 未接受/认证失败仍以 `Unauthorized` 浮现；
    /// 传输类按 error.rs 映射（含重连骨架的一次重放）。
    async fn connect(&self) -> Result<(), StorageError> {
        let root = self.driver.client().params().root.clone();
        match self.driver.client().metadata(&root).await {
            Ok(attrs) if attrs.is_dir() => Ok(()),
            Ok(_) => Err(StorageError::Unavailable(format!(
                "the sftp volume root {root} is not a directory — point sftp_root at a \
                 directory on the server"
            ))),
            Err(StorageError::NotFound) => Err(StorageError::Unavailable(format!(
                "the sftp volume root {root} does not exist on the server — create it there \
                 or fix sftp_root in the volume file"
            ))),
            Err(other) => Err(other),
        }
    }

    /// 整文件复制上传：读盘 → writer + write 全量 + close。receipt 遵循
    /// K6（`first_msg_id = 0` 占位、`chunk_msg_ids = [0]` 单 chunk 占位
    /// ——K11 簿记与 rebuild 契约一致）。
    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        let rel = vocab_rel(&job.rel_path)?;
        if rel.is_root() {
            return Err(StorageError::Invalid); // 卷根不可作为上传目标
        }
        let data = tokio::fs::read(&job.local_path)
            .await
            .map_err(|e| StorageError::Io(format!("reading {}: {e}", job.local_path.display())))?;
        self.store_bytes(&rel, job.size, &data).await
    }

    /// 流式上传：字节逐帧进 stager（无内存全缓冲）；超计划拒绝、短流
    /// 由 close 的承诺校验给 `Invalid`（ck-local 同款形态）。
    ///
    /// **错误路径显式 abort（sftp-review ①）**：`frame?`/`write?` 的
    /// 早退若裸 Drop stager，覆盖写场景的旧版本会被困在 `.old` 残件
    /// （final 被 stash 丢空，重试耗尽后文件对卷消失）——错误必须经
    /// `abort()` 复位现场后再上抛。
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
        let outcome = write_frames(&mut stager, data, job.size).await;
        match outcome {
            Ok(()) => {
                let entry = stager.close().await?;
                Ok(receipt_of(entry.size))
            }
            Err(e) => {
                let _ = stager.abort().await;
                Err(e)
            }
        }
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
        let id = entry_id(&self.driver, file)?;
        let want = len.min(file.total_size.saturating_sub(off));
        if want == 0 {
            return Ok(empty_stream());
        }
        let range = Range::new(off, Some(off.saturating_add(want)))?;
        let stream = StorageDriver::reader(self.driver.as_ref(), &id, Some(range)).await?;
        Ok(sync_stream(stream))
    }

    /// 删除句柄指向的卷内对象：薄壳委派 `StorageDriver::delete`（路径
    /// 缺失 → `NotFound` 幂等形态；目录 → 递归删）。
    async fn delete_remote(&self, handle: &RemoteHandle) -> Result<(), StorageError> {
        let id = entry_id(&self.driver, handle)?;
        self.driver.delete(&id).await
    }

    /// 镜像 StorageDriver 位 + `remote_delete = true`（K4：本面
    /// delete_remote 真删远端文件）。
    fn capabilities(&self) -> Capabilities {
        let mut caps = StorageDriver::capabilities(self.driver.as_ref());
        caps.remote_delete = true;
        caps
    }

    /// 探针（Phase 8 / D1）：宽面在此——transport 薄壳与 StorageDriver
    /// 装配共享同一 `Arc<SftpDriver>`，read-through 回源经本探针取回
    /// list/stat 宽面。
    fn as_driver(&self) -> Option<&dyn StorageDriver> {
        Some(self.driver.as_ref())
    }
}

impl SftpTransport {
    /// 两上传面的共通收尾：writer + 全量 write + close → K6 receipt。
    ///
    /// **错误路径显式 abort（sftp-review ①）**：`write?` 早退裸 Drop
    /// 会把旧版本困在 `.old`（final 被 stash 丢空）——同 upload_stream
    /// 的恢复纪律。
    async fn store_bytes(
        &self,
        rel: &RelPath,
        promised: u64,
        data: &[u8],
    ) -> Result<UploadReceipt, StorageError> {
        let hint = WriteHint {
            size: Some(promised),
            ..Default::default()
        };
        let mut stager = self.driver.writer(rel, &hint).await?;
        match stager.write(data).await {
            Ok(()) => {
                let entry = stager.close().await?;
                Ok(receipt_of(entry.size))
            }
            Err(e) => {
                let _ = stager.abort().await;
                Err(e)
            }
        }
    }
}

/// upload_stream 的帧泵（拆出使错误路径的 `stager.abort()` 借用成立）：
/// 逐帧写 stager，超计划拒绝。
async fn write_frames(
    stager: &mut Box<dyn UploadStager>,
    data: ByteStream,
    planned: u64,
) -> Result<(), StorageError> {
    let mut frames = data;
    let mut written: u64 = 0;
    while let Some(frame) = futures_util::StreamExt::next(&mut frames).await {
        let frame = frame?;
        written += frame.len() as u64;
        if written > planned {
            return Err(StorageError::Unavailable(format!(
                "stream exceeded the planned {planned} bytes"
            )));
        }
        stager.write(&frame).await?;
    }
    Ok(())
}

/// K6 receipt 形态：msg_id 恒 0 占位（不语义化，local 同款）。
fn receipt_of(uploaded_bytes: u64) -> UploadReceipt {
    UploadReceipt {
        first_msg_id: 0,
        // K11 单 chunk 占位：`[0]`
        chunk_msg_ids: vec![0],
        uploaded_bytes,
    }
}
