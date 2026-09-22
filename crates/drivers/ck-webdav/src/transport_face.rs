//! CloudTransport 面（Phase 7 / WD2b 读面接线；照 ck-sftp
//! transport_face.rs 薄壳形态）。
//!
//! WebDAV 的 transport 面是 [`WebdavDriver`] 的薄壳：与 StorageDriver
//! 面共享同一驱动实现与连接/协商世界（同一 `Arc<WebdavDriver>`），
//! 不重复任何协议逻辑——两面差异只在句柄语义（K2/K6，与 local/sftp
//! 完全同构）：
//!
//! - **句柄**：路径寻址（`RemoteHandle::path`，vpath 形态）；
//!   `first_msg_id`/`chunk_msg_ids` 是不语义化的占位（receipt 恒 0/
//!   `[0]`——K11 簿记与 rebuild 契约一致）；`path = None` → `Invalid`；
//! - **chunk 计划**：`UploadJob` 的 chunk_count/chunk_size 对整文件
//!   PUT 的 WebDAV 无意义，不消费；`job.size` 作为 WriteHint 承诺由
//!   stager close 校验（WD3）；
//! - **connect**：OPTIONS 探活（含认证协商一轮——D1 状态机的首次驱动
//!   点）+ 外层 deadline（belt-and-braces：per-request 30s 预算之上
//!   再钉一道墙，防御重试退避叠加的病态形态）；
//! - **字节预算**：`open`/`open_range` 以 `total_size` 为窗口上界
//!   （E-5 预算帽语义）；EOF 钳制归驱动 reader。
//!
//! ByteStream 适配：storage 面（Send）与 transport 面（Send+Sync）是
//! 两个类型（R-3 迁移记录的并存）——经 [`SyncStream`] 的 Mutex 桥接
//!（ck-local/ck-sftp 同款最小适配）。

use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use std::pin::Pin;

use cloudkit_storage::transport::{
    ByteStream, CloudTransport, RemoteHandle, StorageError, UploadJob, UploadReceipt,
};
use cloudkit_storage::vpath::RelPath as VPath;
use cloudkit_storage::{Capabilities, EntryId, Range, RelPath, StorageDriver};

use crate::driver::WebdavDriver;

/// connect 探活的外层 deadline（per-request 30s 预算之上的第二道墙——
/// 探活必须有限时地结束）。
const CONNECT_PROBE_DEADLINE: Duration = Duration::from_secs(60);

/// vpath（CyDrive VFS 绝对式，transport 载荷）→ vocab（卷内相对，
/// StorageDriver 词汇）RelPath 转换（ck-local/ck-sftp 同款私换算点）。
fn vocab_rel(path: &VPath) -> Result<RelPath, StorageError> {
    RelPath::new(path.as_str().trim_start_matches('/'))
}

/// 句柄寻址（K2/K6）：path → 卷内 EntryId（K6 句柄字符串 = rel_path
/// 形态，与 StorageDriver 面往返一致）。
fn entry_id(driver: &WebdavDriver, handle: &RemoteHandle) -> Result<EntryId, StorageError> {
    let path = handle.path.as_ref().ok_or(StorageError::Invalid)?;
    let rel = vocab_rel(path)?;
    Ok(EntryId::new(
        driver.volume().clone(),
        cloudkit_storage::BackendHandle::new(rel.as_str()),
    ))
}

/// storage ByteStream（Send）→ transport ByteStream（Send + Sync）的
/// 最小适配（ck-local/ck-sftp 同款：std Mutex 引入 Sync，poll 内短
/// 临界区、永不跨 await；毒锁恢复继续比卡死正确）。
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
pub struct WebdavTransport {
    driver: Arc<WebdavDriver>,
}

impl WebdavTransport {
    /// 以既有驱动实例构造 transport 面（两面共享同一后端状态）。
    pub fn new(driver: Arc<WebdavDriver>) -> Self {
        WebdavTransport { driver }
    }

    /// 底层驱动（两面互访；`Arc` 克隆经 [`Self::into_driver`]）。
    pub fn driver(&self) -> &WebdavDriver {
        &self.driver
    }

    /// 归还驱动句柄（组合根双面装配共用一个 `Arc<WebdavDriver>`）。
    pub fn into_driver(self) -> Arc<WebdavDriver> {
        self.driver
    }
}

#[async_trait]
impl CloudTransport for WebdavTransport {
    /// 探活：OPTIONS 一轮（**真连接检查**——认证协商也在这里发生：401
    /// 协商/凭据被拒/NTLM 拒绝以 `Unauthorized` 浮现，connect 拒绝/超时
    /// 以 `Unavailable` 浮现；OPTIONS 在重试白名单内，传输毛刺自愈）。
    /// 外层 deadline 见模块文档。
    async fn connect(&self) -> Result<(), StorageError> {
        match tokio::time::timeout(CONNECT_PROBE_DEADLINE, self.driver.client().options()).await {
            Ok(result) => result,
            Err(_) => Err(StorageError::Unavailable(format!(
                "webdav connect probe exceeded its {}s deadline",
                CONNECT_PROBE_DEADLINE.as_secs()
            ))),
        }
    }

    /// 整文件复制上传（读盘 → stager 链；WD3 接线）。receipt 遵循 K6
    ///（first_msg_id = 0 占位、chunk_msg_ids = [0] 单 chunk 占位）。
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

    /// 全文件读：预算帽语义下等价 `open_range(0, total_size)`。
    async fn open(&self, file: &RemoteHandle) -> Result<ByteStream, StorageError> {
        self.open_range(file, 0, file.total_size).await
    }

    /// 半开窗口 `[off, min(off+len, total_size))`（预算帽；EOF 之下的
    /// 钳制由驱动 reader 承担）。`want == 0` / `start >= size` → 空流。
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

    /// 删除句柄指向的卷内对象：薄壳委派 `StorageDriver::delete`
    /// （WD3 写面已接线——缺失句柄 `NotFound` 幂等、目录递归）。
    async fn delete_remote(&self, handle: &RemoteHandle) -> Result<(), StorageError> {
        let id = entry_id(&self.driver, handle)?;
        self.driver.delete(&id).await
    }

    /// 镜像 StorageDriver 位 + `remote_delete = true`（K4：本面
    /// delete_remote 真删远端——WebDAV DELETE 即终删，协议无回收站；
    /// Nextcloud trashbin 是服务端行为不可依赖，计划 §4.3）。
    fn capabilities(&self) -> Capabilities {
        StorageDriver::capabilities(self.driver.as_ref())
    }
}

impl WebdavTransport {
    /// 上传收尾共通（sftp `store_bytes` 同款薄壳）：writer + 全量
    /// write + close → K6 receipt（first_msg_id = 0、chunk_msg_ids =
    /// [0] 占位——整件 PUT 无 chunk 簿记）。
    async fn store_bytes(
        &self,
        rel: &RelPath,
        promised: u64,
        data: &[u8],
    ) -> Result<UploadReceipt, StorageError> {
        let hint = cloudkit_storage::WriteHint {
            size: Some(promised),
            ..Default::default()
        };
        let mut stager = self.driver.writer(rel, &hint).await?;
        stager.write(data).await?;
        let entry = stager.close().await?;
        Ok(UploadReceipt {
            first_msg_id: 0,
            chunk_msg_ids: vec![0],
            uploaded_bytes: entry.size,
        })
    }
}
