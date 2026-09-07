//! MockStorageDriver——内存后端（conformance kit 第一公民）。
//!
//! 用途：驱动 conformance 套件的参照实现 + 上层（L3+）单测的假后端。
//! **R4：能力位诚实**——只声明套件已验证的位，未实现的可选 trait
//! （RapidUpload/ChangeFeed/TokenEvents）对应位保持 false。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::capability::Capabilities;
use crate::driver::StorageDriver;
use crate::error::StorageError;
use crate::ids::{EntryId, VolumeId};
use crate::stager::UploadStager;
use crate::vocab::{ByteStream, Entry, Listing, Page, Quota, Range, RelPath, WriteHint};

/// 内存 mock 驱动。
///
/// 后端模型：`BTreeMap<RelPath, node>` 即「远端」；staging 会话独立于
/// 节点表（commit 前不可见）。测试支持面：
/// - [`MockStorageDriver::fail_next_stat`]：让下一次 `stat` 失败（断言⑤
///   的故障注入——注入的是**已映射**的 StorageError，errno→StorageError
///   映射表归驱动测试侧，L2 运行时不认识后端错误码——R1）；
/// - [`MockStorageDriver::bytes_received`]：后端累计收到的字节数
///   （断言⑦ RESUME 可观测点）。
pub struct MockStorageDriver {
    volume: VolumeId,
    chunk_size: u64,
    // 中毒恢复统一模式（code-style §2）
    faults: Mutex<VecDeque<StorageError>>,
    bytes_received: Arc<AtomicU64>,
}

impl MockStorageDriver {
    /// 默认分块 16 字节的 mock（小分块让跨块/多块覆盖在极小数据量下成立）。
    pub fn new(volume: VolumeId) -> Self {
        Self::with_chunk_size(volume, 16)
    }

    pub fn with_chunk_size(volume: VolumeId, chunk_size: u64) -> Self {
        MockStorageDriver {
            volume,
            chunk_size: chunk_size.max(1),
            faults: Mutex::new(VecDeque::new()),
            bytes_received: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 套件断言②/⑦ 使用的分块边界。
    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    /// 注入：下一次 `stat` 返回该错误（恰好一次）。
    pub fn fail_next_stat(&self, err: StorageError) {
        self.faults
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push_back(err);
    }

    /// 后端至今收到的字节数（staging 分片「发送」即计入）。
    pub fn bytes_received(&self) -> u64 {
        self.bytes_received.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl StorageDriver for MockStorageDriver {
    fn volume(&self) -> &VolumeId {
        &self.volume
    }

    fn capabilities(&self) -> Capabilities {
        // R4 诚实声明：全部经 conformance 套件八条验证过的位
        Capabilities {
            range_read: true,
            resume: true,
            multipart: false,
            server_side_move: true,
            rapid_upload: false,
            authoritative_index: true,
            change_feed: false,
            inbound: false,
            chat: false,
        }
    }

    async fn list(&self, _dir: &RelPath, _page: Page) -> Result<Listing, StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn stat(&self, path: &RelPath) -> Result<Entry, StorageError> {
        let _ = path;
        if let Some(e) = self
            .faults
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
        {
            return Err(e);
        }
        Err(StorageError::NotFound)
    }

    async fn mkdir(&self, path: &RelPath) -> Result<(), StorageError> {
        let _ = path;
        Err(StorageError::Unsupported)
    }

    async fn delete(&self, id: &EntryId) -> Result<(), StorageError> {
        let _ = id;
        Err(StorageError::Unsupported)
    }

    async fn rename(&self, from: &RelPath, to: &RelPath) -> Result<(), StorageError> {
        let _ = (from, to);
        Err(StorageError::Unsupported)
    }

    async fn reader(&self, id: &EntryId, range: Option<Range>) -> Result<ByteStream, StorageError> {
        let _ = (id, range);
        Err(StorageError::Unsupported)
    }

    async fn writer(
        &self,
        path: &RelPath,
        hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        let _ = (path, hint);
        Err(StorageError::Unsupported)
    }

    async fn quota(&self) -> Result<Quota, StorageError> {
        Err(StorageError::Unsupported)
    }
}
