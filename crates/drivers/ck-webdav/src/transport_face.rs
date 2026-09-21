//! CloudTransport 面（Phase 7 / WD1a 骨架；照 ck-sftp transport_face.rs
//! 薄壳形态——动词面占位，WD2 接线）。
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
//!   stager close 校验；
//! - **connect**：OPTIONS/PROPFIND Depth 0 探活（含认证协商——D1
//!   状态机的首次驱动点，WD2 接线）；
//! - **字节预算**：`open`/`open_range` 以 `total_size` 为窗口上界
//!   （E-5 预算帽语义）；EOF 钳制归驱动 reader。

use std::sync::Arc;

use async_trait::async_trait;

use cloudkit_storage::transport::{
    ByteStream, CloudTransport, RemoteHandle, StorageError, UploadJob, UploadReceipt,
};

use crate::driver::WebdavDriver;

/// CloudTransport 薄壳：持有驱动经 `Arc` 与 StorageDriver 装配共享。
pub struct WebdavTransport {
    #[allow(dead_code)] // WD1a 骨架：动词面 WD2 接线后进入使用
    driver: Arc<WebdavDriver>,
}

impl WebdavTransport {
    /// 以既有驱动实例构造 transport 面（两面共享同一后端状态）。
    #[allow(dead_code)] // WD1a 骨架：装配批（WD4）接入组合根
    pub fn new(driver: Arc<WebdavDriver>) -> Self {
        WebdavTransport { driver }
    }

    /// 底层驱动（两面互访；`Arc` 克隆经 [`Self::into_driver`]）。
    #[allow(dead_code)] // WD1a 骨架：装配批（WD4）接入组合根
    pub fn driver(&self) -> &WebdavDriver {
        &self.driver
    }

    /// 归还驱动句柄（组合根双面装配共用一个 `Arc<WebdavDriver>`）。
    #[allow(dead_code)] // WD1a 骨架：装配批（WD4）接入组合根
    pub fn into_driver(self) -> Arc<WebdavDriver> {
        self.driver
    }
}

#[async_trait]
impl CloudTransport for WebdavTransport {
    /// 探活：OPTIONS/根 PROPFIND（认证协商首驱动点——host key 类
    /// 不可用，401 协商失败以 `Unauthorized` 浮现；WD2 接线）。
    async fn connect(&self) -> Result<(), StorageError> {
        // TODO(wd2): OPTIONS 探活 + auth 协商接线。
        Err(StorageError::Unsupported)
    }

    /// 整文件复制上传（读盘 → stager 链；WD3 接线）。receipt 遵循 K6
    ///（`first_msg_id = 0` 占位、`chunk_msg_ids = [0]` 单 chunk 占位）。
    async fn upload(&self, _job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        // TODO(wd3): stager 链接线。
        Err(StorageError::Unsupported)
    }

    /// 全文件读：预算帽语义下等价 `open_range(0, total_size)`（WD2）。
    async fn open(&self, file: &RemoteHandle) -> Result<ByteStream, StorageError> {
        self.open_range(file, 0, file.total_size).await
    }

    /// 半开窗口 `[off, min(off+len, total_size))`（预算帽；`want == 0`
    /// → 空流——WD2 接线）。
    async fn open_range(
        &self,
        _file: &RemoteHandle,
        _off: u64,
        _len: u64,
    ) -> Result<ByteStream, StorageError> {
        // TODO(wd2): 窗口流适配（storage 面 → transport 面的 SyncStream
        // 桥，ck-sftp 同款）。
        Err(StorageError::Unsupported)
    }

    /// 删除句柄指向的卷内对象：薄壳委派 `StorageDriver::delete`（WD2）。
    async fn delete_remote(&self, _handle: &RemoteHandle) -> Result<(), StorageError> {
        // TODO(wd2): 薄壳委派接线。
        Err(StorageError::Unsupported)
    }

    /// 镜像 StorageDriver 位 + `remote_delete = true`（K4：本面
    /// delete_remote 真删远端——WebDAV DELETE 即终删，协议无回收站；
    /// Nextcloud trashbin 是服务端行为不可依赖，计划 §4.3）。
    fn capabilities(&self) -> cloudkit_storage::Capabilities {
        // TODO(wd2): StorageDriver::capabilities 镜像接线（占位 = 全
        // false——动词面接线前的诚实形态）。
        cloudkit_storage::Capabilities::none()
    }
}
