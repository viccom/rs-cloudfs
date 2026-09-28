//! CloudTransport 面（Phase 2 Batch B3b 段一；K5 fs_id 句柄）。
//!
//! baidu 的 transport 面是 [`BaiduDriver`] 的薄壳：读取/删除/探活直接
//! 委派 StorageDriver 实现（reader 的 dlink+4MiB 窗口、delete 的缓存→
//! 扫描解析与纠偏——零协议重复），**上传走专用整文件路径**
//! [`upload::upload_whole_file`]（[`UPLOAD_WORKERS`] 并发 superfile2
//! ——PCFS api.go:440-479 的 4 并发吞吐形态；stager 流式路径的串行
//! 契约不变，两条路径共用 K7 会话表）。
//!
//! - **句柄**（K5）：`RemoteHandle.first_msg_id` = fs_id（十进制即
//!   StorageDriver 面 BackendHandle，跨 rename 稳定）；
//!   `chunk_msg_ids` 不参与寻址（fs_id 已完备；receipt 派生句柄携带
//!   单元素列表，见下）；`path` 顺带携带但不参与寻址；
//! - **receipt**（K5/K11）：`first_msg_id = create 返回的 fs_id`，
//!   `chunk_msg_ids = [fs_id]`（单容器单 chunk——K11 簿记：upload
//!   persist 以 receipt 计数 chunk/写 chunks 行，单元素形态使
//!   `chunk_count=1` 且 chunks 行 msg_id 与 files 行主字段同值，
//!   与 rebuild 契约一致）、`uploaded_bytes = 整文件字节数`；
//! - **connect**：quota 轻量探活——构造（factory→uinfo）即已连接，
//!   本调用断言 token 仍有效（失效按 errno 映射表上抛）；
//! - **upload_stream**：流式全缓冲到 staging 文件（OS 临时目录）后走
//!   整文件同路——**全缓冲是唯一正确路径**：precreate 需全量 block_list
//!   （真网 31363 实证：会话一次性锁定 `(path, size, block_list)`，流式
//!   无法增量声明）；逐帧落盘保内存有界；
//! - **字节预算**：`open`/`open_range` 以 `total_size` 为窗口上界
//!   （E-5 预算帽语义，telegram 先例）。
//!
//! ByteStream 适配与 vpath→vocab 转换同 ck-local 形态（R-3 两类型并存的
//! 私有桥；见 [`sync_stream`]/[`vocab_rel`]）。

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;

use cloudkit_storage::transport::{
    ByteStream, CloudTransport, RemoteHandle, StorageError, UploadJob, UploadReceipt,
};
use cloudkit_storage::vpath::RelPath as VPath;
use cloudkit_storage::{BackendHandle, Capabilities, EntryId, Range, RelPath, StorageDriver};

use crate::upload;
use crate::BaiduDriver;

/// upload_stream 的 staging 临时文件名序号（进程级单调）。
static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

/// vpath（CyDrive VFS 绝对式，transport 载荷）→ vocab（卷内相对，
/// StorageDriver 词汇）RelPath 转换：`/` 前缀剥离，根 `/` → 卷根。
///
/// R-3 记录的两类型并存是有意的；本桥是 baidu transport 面的私换算点
///（与 ck-local 的同名桥同形态，驱动 crate 间不共享 L2 之外的代码）。
fn vocab_rel(path: &VPath) -> Result<RelPath, StorageError> {
    RelPath::new(path.as_str().trim_start_matches('/'))
}

/// storage ByteStream（Send）→ transport ByteStream（Send + Sync）的
/// 最小适配（R-3 两类型并存的桥；ck-local 同款形态）：
/// `std::sync::Mutex` 引入 Sync，poll 内短临界区、永不跨 await；毒锁
/// 恢复（`into_inner`）——流推进无不变量可破。
struct SyncStream {
    inner: std::sync::Mutex<cloudkit_storage::ByteStream>,
}

impl Stream for SyncStream {
    type Item = Result<Bytes, StorageError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // SyncStream 无条件 Unpin（std::sync::Mutex<T> 恒 Unpin）。
        let mut guard = self
            .get_mut()
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.as_mut().poll_next(cx)
    }
}

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
pub struct BaiduTransport {
    driver: Arc<BaiduDriver>,
}

impl BaiduTransport {
    /// 以既有驱动实例构造 transport 面（两面共享同一后端状态：句柄
    /// 缓存/dlink 缓存/K7 会话表）。
    pub fn new(driver: Arc<BaiduDriver>) -> Self {
        BaiduTransport { driver }
    }

    /// 底层驱动（两面互访）。
    pub fn driver(&self) -> &BaiduDriver {
        &self.driver
    }

    /// 归还驱动句柄（组合根双面装配共用一个 `Arc<BaiduDriver>`）。
    pub fn into_driver(self) -> Arc<BaiduDriver> {
        self.driver
    }

    /// 句柄 → EntryId（K5：`first_msg_id` = fs_id，与 StorageDriver 面
    /// 句柄十进制字符串同形态）。
    fn entry_id(&self, handle: &RemoteHandle) -> EntryId {
        EntryId::new(
            self.driver.volume().clone(),
            BackendHandle::new(handle.first_msg_id.to_string()),
        )
    }

    /// 帧流 → staging 文件（OS 临时目录；逐帧落盘保内存有界）。超计划/
    /// 短流在落盘途中拒绝（计划与流必须逐字节一致——precreate 的
    /// size/block_list 声明容不得半点偏差）。**不再全量读回**（审查
    /// M4，2026-09-25）：返回 staging 路径 + 清理卫士，后续 md5 预计算
    /// 与分片上传按 CHUNK 窗口按需读盘。
    async fn stage_stream(
        &self,
        data: ByteStream,
        planned: u64,
    ) -> Result<(PathBuf, StreamStagingGuard), StorageError> {
        stage_stream_to_disk(data, planned).await
    }
}

/// staging 临时文件的 Drop 兜底清理（任意退出路径——含错误/panic——
/// 不留孤儿临时文件）。Drop 内同步 remove 是单次快速元数据 syscall
/// （tempfile crate 的 TempDir 同款形态），不属 code-style §4 针对的
/// 重 IO 阻塞面。上传方持有到上传结束（`Ok`/`Err` 皆然）。
pub(crate) struct StreamStagingGuard(PathBuf);
impl Drop for StreamStagingGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// [`BaiduTransport::stage_stream`] 的自由函数体（抽出于复审 H1 修复批，
/// 供可见性钉测直接打点）：帧流 → OS 临时目录 staging 文件，逐帧落盘
/// 保内存有界；超计划/短流在落盘途中拒绝。
///
/// 契约（复审 H1 钉测）：本函数返回的瞬间，`std::fs::metadata(path)
/// .len()` 必须已等于 `planned`——后续 `PartFile::block_md5s`/`part` 的
/// 零间隙读回依赖这一可见性（Linux 上 tokio::fs File 句柄写在
/// `write().await` 返回时 syscall 可能在途——L2 `cloudkit_storage::
/// spool` 的定谳注记）。
async fn stage_stream_to_disk(
    data: ByteStream,
    planned: u64,
) -> Result<(PathBuf, StreamStagingGuard), StorageError> {
    // 唯一临时名：pid + 进程级单调序号；撞名递增重试（create_new）。
    // std::fs 建名（单次快速元数据 syscall——同 StreamStagingGuard 的
    // Drop 形态，不属 code-style §4 的重 IO 阻塞面）。
    let pid = std::process::id();
    let mut claimed: Option<PathBuf> = None;
    for _ in 0..8 {
        let seq = STAGING_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp: PathBuf = std::env::temp_dir().join(format!("ck-baidu-tstream-{pid}-{seq}.part"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(_claim) => {
                claimed = Some(tmp);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(StorageError::Io(format!("stream staging open: {e}"))),
        }
    }
    let Some(path) = claimed else {
        return Err(StorageError::Io(format!(
            "stream staging 命名冲突重试耗尽（pid={pid}）"
        )));
    };
    let cleanup = StreamStagingGuard(path.clone());

    // std::fs on the blocking pool — NOT tokio::fs（复审 H1）：Linux 上
    // tokio::fs File 句柄的 write().await 返回时 write syscall 可能在途
    // （WSL 实测：6 MiB 流返回瞬间盘上缺 192 KiB——stage_visibility 钉测
    // 红腿 6094848/6291456）。紧随其后的 `PartFile::block_md5s` 零间隙读
    // 回会把缺尾/错位字节算进 md5，经 precreate 的全量 block_list 声明
    // 永久落服务端。std write_all 在阻塞线程上有诚实的完成语义（L2
    // `cloudkit_storage::spool_append_write` 同款定谳）。
    let mut written: u64 = 0;
    let mut frames = data;
    while let Some(frame) = futures_util::StreamExt::next(&mut frames).await {
        let frame = frame?;
        written += frame.len() as u64;
        if written > planned {
            return Err(StorageError::Unavailable(format!(
                "stream exceeded the planned {planned} bytes"
            )));
        }
        let target = path.clone();
        let bytes = frame.to_vec();
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&target)?
                .write_all(&bytes)
        })
        .await
        .map_err(|e| StorageError::Io(format!("stream staging join: {e}")))?
        .map_err(|e| StorageError::Io(format!("stream staging write: {e}")))?;
    }
    if written != planned {
        return Err(StorageError::Unavailable(format!(
            "chunk plan mismatch: stream ended at {written} bytes, but the job planned {planned}"
        )));
    }
    Ok((path, cleanup))
}

#[async_trait]
impl CloudTransport for BaiduTransport {
    /// quota 轻量探活：构造（factory→uinfo）即已连接，本调用断言 token
    /// 仍有效（110/111/-6 按映射表归一上抛——消费方拿到的错误可行动）。
    async fn connect(&self) -> Result<(), StorageError> {
        self.driver.quota().await.map(|_| ())
    }

    /// 整文件上传（[`UPLOAD_WORKERS`] 并发）：读盘 → 计划校验 →
    /// [`BaiduDriver::upload_whole_file`]。receipt 遵循 K5
    /// （`first_msg_id = fs_id`）。
    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        let rel = vocab_rel(&job.rel_path)?;
        // 审查 M4：不再 `fs::read` 整文件入内存——元数据校验计划后按
        // CHUNK 窗口按需读盘（`upload::PartFile`）。
        let actual = tokio::fs::metadata(&job.local_path)
            .await
            .map_err(|e| StorageError::Io(format!("upload source stat: {e}")))?
            .len();
        if actual != job.size {
            return Err(StorageError::Unavailable(format!(
                "upload job planned {} bytes but the local file holds {}",
                job.size, actual
            )));
        }
        let src = upload::PartFile::new(job.local_path.clone(), job.size);
        self.finish_upload(&rel, src).await
    }

    /// 流式上传：缓冲到 staging 文件（31363 裁决——precreate 需全量
    /// block_list，流式无法增量声明，落盘缓冲是唯一正确路径）后走整
    /// 文件同路（按需读盘，不驻内存——审查 M4）。`job.local_path` 是
    /// provenance only（线上字节来自流）。清理卫士持有到上传结束。
    async fn upload_stream(
        &self,
        job: &UploadJob,
        data: ByteStream,
    ) -> Result<UploadReceipt, StorageError> {
        let rel = vocab_rel(&job.rel_path)?;
        let (path, _guard) = self.stage_stream(data, job.size).await?;
        let src = upload::PartFile::new(path, job.size);
        self.finish_upload(&rel, src).await
    }

    /// 全文件读：预算帽语义下等价 `open_range(0, total_size)`。
    async fn open(&self, file: &RemoteHandle) -> Result<ByteStream, StorageError> {
        self.open_range(file, 0, file.total_size).await
    }

    /// 半开窗口 `[off, min(off+len, total_size))`（预算帽，telegram 先例）：
    /// fs_id → driver reader（dlink 缓存 + 4MiB 有界窗口 + 两段 fallback，
    /// 全部复用 StorageDriver 面实现）。
    async fn open_range(
        &self,
        file: &RemoteHandle,
        off: u64,
        len: u64,
    ) -> Result<ByteStream, StorageError> {
        let id = self.entry_id(file);
        let want = len.min(file.total_size.saturating_sub(off));
        if want == 0 {
            return Ok(empty_stream());
        }
        let range = Range::new(off, Some(off.saturating_add(want)))?;
        let stream = StorageDriver::reader(self.driver.as_ref(), &id, Some(range)).await?;
        Ok(sync_stream(stream))
    }

    /// 删除句柄指向的网盘对象：薄壳委派 `StorageDriver::delete`（fs_id
    /// 解析两级：句柄缓存→递归扫描；filemanager delete + 陈旧纠偏 +
    /// 成功失效——`tests/transport_face.rs` 钉死 warm 缓存零扫描路径）。
    async fn delete_remote(&self, handle: &RemoteHandle) -> Result<(), StorageError> {
        let id = self.entry_id(handle);
        self.driver.delete(&id).await
    }

    /// 镜像 StorageDriver 位 + `remote_delete = true`（K4：本面
    /// delete_remote 真删网盘对象）。
    fn capabilities(&self) -> Capabilities {
        let mut caps = StorageDriver::capabilities(self.driver.as_ref());
        caps.remote_delete = true;
        caps
    }

    /// 探针（Phase 8 / D1）：宽面在此——transport 薄壳与 StorageDriver
    /// 装配共享同一 `Arc<BaiduDriver>`，read-through 回源经本探针取回
    /// list/stat 宽面。
    fn as_driver(&self) -> Option<&dyn StorageDriver> {
        Some(self.driver.as_ref())
    }
}

impl BaiduTransport {
    /// 两上传面的共通收尾：整文件路径 → K5 receipt（fs_id 句柄）。
    async fn finish_upload(
        &self,
        rel: &RelPath,
        src: upload::PartFile,
    ) -> Result<UploadReceipt, StorageError> {
        let entry = self.driver.upload_whole_file(rel, src).await?;
        // K5：Entry.handle 即 fs_id 十进制字符串（同一构造来源，parse 必
        // 成功；防御分支仍归一错误而非 panic）。
        let fs_id: i64 = entry.id.handle.as_str().parse().map_err(|_| {
            StorageError::Unavailable(format!("fs_id 句柄不可解析：{}", entry.id.handle.as_str()))
        })?;
        Ok(UploadReceipt {
            first_msg_id: fs_id,
            // K11 单容器单 chunk：chunk_msg_ids = [fs_id]——upload
            // persist 以此计数 chunk_count=1 并写 chunks 行（msg_id 与
            // files 行 telegram_msg_id 主字段同值），与 rebuild 契约
            // 对齐（E2E 观察项②）。
            chunk_msg_ids: vec![fs_id],
            uploaded_bytes: entry.size,
        })
    }
}

#[cfg(test)]
mod stage_visibility_tests {
    use super::*;

    /// 复审 H1 钉测：staging 返回的瞬间全部帧必须已在盘上（std 视角
    /// 可见）——`PartFile::block_md5s` 的零间隙读回依赖它。Linux 上
    /// tokio::fs 句柄写形态会红（write().await 返回时 syscall 在途，
    /// WSL 复现；Windows 免疫——两平台形态见 L2 spool 定谳）。
    #[tokio::test]
    async fn staged_frames_are_fully_on_disk_the_moment_staging_returns() {
        const ROUNDS: usize = 3;
        const FRAMES: usize = 24;
        const FRAME: usize = 256 * 1024;
        for round in 0..ROUNDS {
            let frames: Vec<Result<Bytes, StorageError>> = (0..FRAMES)
                .map(|i| Ok(Bytes::from(vec![(i % 251) as u8 + round as u8; FRAME])))
                .collect();
            let stream: ByteStream = Box::pin(futures_util::stream::iter(frames));
            let planned = (FRAMES * FRAME) as u64;
            let (path, _guard) = stage_stream_to_disk(stream, planned)
                .await
                .expect("staging completes");
            let len = std::fs::metadata(&path)
                .expect("immediate std stat after staging returns")
                .len();
            assert_eq!(
                len, planned,
                "every staged frame must be visible the moment staging returns \
                 (the md5 read-back runs zero-gap right after)"
            );
            let _ = std::fs::remove_file(&path); // guard Drop 亦删；幂等
        }
    }
}
