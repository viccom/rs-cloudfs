//! CloudTransport 面（Phase 2 Batch B3b 段一；K2 path 寻址 / K6 0 占位）。
//!
//! local 的 transport 面是 [`LocalDriver`] 的薄壳：与 StorageDriver 面共
//! 享同一驱动实现（写入=writer+close、读取=reader、删除=delete），不重
//! 复任何协议逻辑——两面差异只在句柄语义：
//!
//! - **句柄**（K2/K6）：本地后端无消息模型，`first_msg_id`/`chunk_msg_ids`
//!   是不语义化的占位（receipt 恒 0 / 空）；寻址全部经
//!   [`RemoteHandle::path`](cloudkit_storage::transport::RemoteHandle::path)
//!   （vpath 形态）——`path = None` 的句柄不可寻址 → `Invalid`；
//! - **chunk 计划**：`UploadJob` 的 chunk_count/chunk_size 对无分块原语
//!   的本地后端无意义，不消费；`job.size` 作为 WriteHint 承诺由 stager
//!   close 校验（不符 → `Invalid`）；
//! - **connect**：根可写探活（暂存目录内写删探针文件）；构造已保证根
//!   存在，本调用是运行时健康探测；
//! - **删除幂等形态**：telegram 先例（deleted==0 → NotFound）——路径
//!   缺失 → `NotFound` 恒定（`StorageDriver::delete` 的 metadata 预检
//!   自然给出）；
//! - **字节预算**：`open`/`open_range` 以 `total_size` 为窗口上界
//!   （E-5 预算帽语义，telegram 先例）；EOF 钳制归驱动 reader。
//!
//! ByteStream 适配：storage 面 [`cloudkit_storage::ByteStream`]（Send）与
//! transport 面 `ByteStream`（Send+Sync）是两个类型（R-3 迁移记录的并存
//! ）——经 [`sync_stream`] 的 Mutex 适配桥接（最小适配，见其文档）。

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
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
use cloudkit_storage::{
    BackendHandle, Capabilities, EntryId, Range, RelPath, StorageDriver, WriteHint,
};

use crate::driver::{map_io, LocalDriver, STAGING_DIR_NAME};

/// connect 探针文件名序号（进程级单调——同进程多实例不撞名）。
static PROBE_SEQ: AtomicU64 = AtomicU64::new(0);

/// vpath（CyDrive VFS 绝对式，transport 载荷）→ vocab（卷内相对，
/// StorageDriver 词汇）RelPath 转换：`/` 前缀剥离，根 `/` → 卷根。
///
/// R-3 记录的两类型并存是有意的（caption 契约 vs 驱动词汇）；本桥是
/// local transport 面的私换算点。vpath 已校验的同源规则使失败只可能是
/// 防御形态（仍映射 `Invalid`，不 panic）。
fn vocab_rel(path: &VPath) -> Result<RelPath, StorageError> {
    RelPath::new(path.as_str().trim_start_matches('/'))
}

/// 句柄寻址（K2/K6）：path → 卷内 EntryId（K6 句柄字符串 = rel_path 形态
/// ，与 StorageDriver 面往返一致）；`path = None` → `Invalid`（本地后端
/// 无 id 寻址面，0 占位的 msg_id 不可用）。
fn entry_id(driver: &LocalDriver, handle: &RemoteHandle) -> Result<EntryId, StorageError> {
    let path = handle.path.as_ref().ok_or(StorageError::Invalid)?;
    let rel = vocab_rel(path)?;
    Ok(EntryId::new(
        driver.volume().clone(),
        BackendHandle::new(rel.as_str()),
    ))
}

/// storage ByteStream（Send）→ transport ByteStream（Send + Sync）的
/// 最小适配（R-3 两类型并存的桥）。
///
/// `std::sync::Mutex` 引入 Sync（内部流只需 Send）；poll 内短临界区、
/// 永不跨 await（poll 本身非 async），std 锁足够——mock transport 同款
/// 论证。毒锁恢复（`into_inner`）：流推进无不变量可破，继续比卡死正确。
struct SyncStream {
    inner: std::sync::Mutex<cloudkit_storage::ByteStream>,
}

impl Stream for SyncStream {
    type Item = Result<Bytes, StorageError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // SyncStream 无条件 Unpin（std::sync::Mutex<T> 恒 Unpin）→ get_mut
        // 免 unsafe 投影。
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
pub struct LocalTransport {
    driver: Arc<LocalDriver>,
}

impl LocalTransport {
    /// 以既有驱动实例构造 transport 面（两面共享同一后端状态）。
    pub fn new(driver: Arc<LocalDriver>) -> Self {
        LocalTransport { driver }
    }

    /// 底层驱动（两面互访；`Arc` 克隆经 [`Self::into_driver`]）。
    pub fn driver(&self) -> &LocalDriver {
        &self.driver
    }

    /// 归还驱动句柄（组合根双面装配共用一个 `Arc<LocalDriver>`）。
    pub fn into_driver(self) -> Arc<LocalDriver> {
        self.driver
    }
}

#[async_trait]
impl CloudTransport for LocalTransport {
    /// 根可写探活：暂存目录内 create_new 探针文件 + 删除（两个动作都
    /// 成功 = 根可写）。探针名带 pid + 进程级序号，8 次撞名重试。
    async fn connect(&self) -> Result<(), StorageError> {
        let staging_dir = self.driver.root_path().join(STAGING_DIR_NAME);
        tokio::fs::create_dir_all(&staging_dir)
            .await
            .map_err(map_io)?;
        let pid = std::process::id();
        for _ in 0..8 {
            let seq = PROBE_SEQ.fetch_add(1, Ordering::Relaxed);
            let probe = staging_dir.join(format!("{pid}-{seq}.probe"));
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe)
                .await
            {
                Ok(_) => {
                    tokio::fs::remove_file(&probe).await.map_err(map_io)?;
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(map_io(e)),
            }
        }
        Err(StorageError::Io(format!(
            "connect 探针命名冲突重试耗尽（pid={pid}）"
        )))
    }

    /// 整文件复制上传：读盘 → writer + write 全量 + close。receipt 遵循
    /// K6（`first_msg_id = 0` 占位、`chunk_msg_ids` 空）；`job.size` 作为
    /// WriteHint 承诺由 close 校验。
    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        let rel = vocab_rel(&job.rel_path)?;
        if rel.is_root() {
            return Err(StorageError::Invalid); // 卷根不可作为上传目标
        }
        let data = tokio::fs::read(&job.local_path).await.map_err(map_io)?;
        self.store_bytes(&rel, job.size, &data).await
    }

    /// 流式上传：字节逐帧进 stager（无内存全缓冲——落盘即暂存），计划
    /// 校验同 upload 面（超计划拒绝；短流由 close 的承诺校验给 `Invalid`）。
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

    /// 半开窗口 `[off, min(off+len, total_size))`（预算帽，telegram 先例；
    /// EOF 之下的钳制由驱动 reader 承担）。`start >= size` / 空窗口 → 空流。
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
    /// 缺失 → `NotFound`——telegram deleted==0 → NotFound 同款幂等形态；
    /// 卷根 → `Invalid`；目录 → 递归删）。
    async fn delete_remote(&self, handle: &RemoteHandle) -> Result<(), StorageError> {
        let id = entry_id(&self.driver, handle)?;
        self.driver.delete(&id).await
    }

    /// 镜像 StorageDriver 位 + `remote_delete = true`（K4：本面
    /// delete_remote 真删卷内文件）。
    fn capabilities(&self) -> Capabilities {
        let mut caps = StorageDriver::capabilities(self.driver.as_ref());
        caps.remote_delete = true;
        caps
    }
}

impl LocalTransport {
    /// 两上传面的共通收尾：writer + 全量 write + close → K6 receipt。
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
        stager.write(data).await?;
        let entry = stager.close().await?;
        Ok(receipt_of(entry.size))
    }
}

/// K6 receipt 形态：msg_id 恒 0 占位（不语义化）、无消息 id 列表。
fn receipt_of(uploaded_bytes: u64) -> UploadReceipt {
    UploadReceipt {
        first_msg_id: 0,
        chunk_msg_ids: Vec::new(),
        uploaded_bytes,
    }
}
