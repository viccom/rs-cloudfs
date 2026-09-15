//! SftpDriver——SSH/SFTP 存储驱动（Phase 4 / SF1 骨架）。
//!
//! 后端模型：服务器上一个根目录（`sftp_root`，缺省 `/`）下的远端
//! 文件系统——「网络上的 local」。[`RelPath`] 组件直接拼进
//! [`remote_path`]（词汇层已拒 `/`、`..`、`\`、`\0`，SFTP 路径在协议
//! 载荷里传输不经 shell，无需进一步转义）。
//!
//! **网络路径的行为测试在 SF2 桩批补**（SF1 以编译 + clippy 为门）；
//! 纯函数层（错误映射/路径拼接/能力位）单测在 lib.rs 钉死。
//!
//! ## 三硬仗纪律（计划 §4.4，aeroftp 实战教训）
//!
//! 1. **每次 open 必 awaited close**：reader 流在正常读完、提前 EOF、
//!    读错误三个分支都 `file.close().await`；stager 的 close/abort 同。
//!    消费者中途 Drop 流的兜底 = russh-sftp `File` 自身的 Drop（排队
//!    `close_nowait`，不等待确认——库行为，SF2 桩的服务端句柄计数
//!    测试钉住此边界）；
//! 2. **上传 close 后校验远端大小**：stager close 里 `metadata` 比对
//!    `written` 计数，短/零即判失败（`Io`）——0 字节上传 bug 的唯一
//!    持久修复；
//! 3. **覆盖/续传绝不用 `APPEND`**：写打开一律 `WRITE | CREATE |
//!    TRUNCATE`（[`crate::client::SftpClient::open_write_truncate`]）
//!    + 从 0 顺序写（aeroftp `:2282-2295`：「部分服务器设置 APPEND
//!      时忽略 seek 而在 EOF 写」）。驱动不声明 `resume` 能力位，续传
//!      语义由上层缓存承担。
//!
//! ## commit-on-close 的边界（与 local 的差异，诚实声明）
//!
//! 计划 §7 明确不做「远程临时文件 + rename 的原子上传」——SFTP 写
//! 直接落在最终路径上，staging 窗口内目标路径以部分内容可见。close
//! 前 `stat` 目标可能看到短文件（而非 `NotFound`）。abort 尽力删除
//! 目标；覆盖旧版本的场景 abort 后目标缺失（无 stash 能力）。这些
//! 行为由 SF2 桩测试钉死后再对 conformance 断言①（close 前不可见）
//! 做出豁免说明或补强。
//!
//! ## 并发与一致性
//!
//! 全方法可并发调用（`&self`）；连接的建立/拆除经客户端内互斥串行
//! （D3 单连接），文件句柄在锁外流式推进（会话层自支持并发请求）。
//! 远端文件系统的 POSIX 语义即一致性来源（同 local 的立场）。

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream;
use russh_sftp::protocol::FileAttributes;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use cloudkit_storage::{
    BackendHandle, ByteStream, Capabilities, Entry, EntryId, EntryKind, Listing, Page, PageCursor,
    Quota, Range, RelPath, StorageDriver, StorageError, UploadStager, VolumeId, WriteHint,
};

use crate::client::SftpClient;
use crate::config::SftpParams;

/// reader 每帧读取的字节数（64KiB，local 同款页粒度；会话层的并发
/// 流水线（3.0 `max_concurrent_reads=16`）在此粒度上工作）。
const READ_FRAME: u64 = 64 * 1024;

/// 卷内相对路径 → 后端绝对路径（纯函数；TDD 钉死）。
///
/// `root` 是 `sftp_root`（已校验 `/` 开头）；`rel` 组件已过词汇层
/// 校验。根目录原样返回 `root`；子路径用单个 `/` 连接，`root` 的
/// 尾部斜杠折叠（`/srv/data/` + `a` = `/srv/data/a`；`/` + `a` = `/a`）。
pub(crate) fn remote_path(root: &str, rel: &RelPath) -> String {
    if rel.is_root() {
        return root.to_string();
    }
    let trimmed = root.trim_end_matches('/');
    format!("{trimmed}/{}", rel.as_str())
}

/// SFTP 元数据 → Entry（stat/list/stager-close 共用；mtime 取
/// `attrs.mtime` 秒，f64 epoch 形态）。
pub(crate) fn entry_from_attrs(volume: &VolumeId, rel: &RelPath, attrs: &FileAttributes) -> Entry {
    let is_dir = attrs.is_dir();
    Entry {
        id: EntryId::new(volume.clone(), BackendHandle::new(rel.as_str())),
        path: rel.clone(),
        kind: if is_dir {
            EntryKind::Dir
        } else {
            EntryKind::File
        },
        size: if is_dir { 0 } else { attrs.len() },
        mtime: attrs.mtime.map(|secs| secs as f64).unwrap_or(0.0),
    }
}

/// 句柄 → RelPath（K6 往返：句柄字符串 = rel_path 形态，local 先例）。
fn rel_from_handle(id: &EntryId) -> Option<RelPath> {
    RelPath::new(id.handle.as_str()).ok()
}

/// SFTP 存储驱动。
///
/// - 语义：远端文件系统即「云」；写入路径缺失父目录隐式创建（trait
///   契约，[`SftpDriver::ensure_parents`]）；目录删除递归（trait 契约，
///   [`SftpDriver::recursive_remove`]）；
/// - 错误：russh/russh-sftp 错误按 [`crate::error`] 映射表归一（R2）；
/// - 并发：全方法可并发调用；单 SSH 连接（D3），元数据操作连接内
///   串行、文件流锁外并发；
/// - 生命周期：连接惰性建立、断线重建（[`crate::client`]）；stager
///   生命周期见 [`SftpStager`]。
pub struct SftpDriver {
    volume: VolumeId,
    client: Arc<SftpClient>,
}

impl SftpDriver {
    /// 同步构造（不连接——惰性建立是 D3 形态）；卷身份
    /// `sftp:<user>@<host>:<port>`（D6：同服务器同账号同卷）。
    pub fn new(params: SftpParams) -> Result<Self, StorageError> {
        let volume = VolumeId::new(
            "sftp",
            &format!("{}@{}:{}", params.username, params.host, params.port),
        )?;
        let client = Arc::new(SftpClient::new(Arc::new(params)));
        Ok(SftpDriver { volume, client })
    }

    /// 客户端句柄（transport 面共用同一连接世界）。
    pub(crate) fn client(&self) -> &Arc<SftpClient> {
        &self.client
    }

    /// 后端绝对路径（remote_path 的驱动侧便捷面）。
    fn path(&self, rel: &RelPath) -> String {
        remote_path(&self.client.params().root, rel)
    }

    /// stat 的驱动面（含断线重连重放）。
    async fn stat_path(&self, rel: &RelPath) -> Result<FileAttributes, StorageError> {
        self.client.metadata(&self.path(rel)).await
    }

    /// mkdir 的父链预建（trait 契约：写入路径缺失父目录由驱动隐式
    /// 创建；百度 ensure_parents 先例——逐段 stat 已存在则跳过、缺失
    /// 则 create_dir，撞到同名文件则 Invalid）。
    async fn ensure_parents(&self, rel: &RelPath) -> Result<(), StorageError> {
        let root = self.client.params().root.clone();
        let mut current = root.trim_end_matches('/').to_string();
        for comp in rel.components() {
            current.push('/');
            current.push_str(comp);
            match self.client.metadata(&current).await {
                Ok(attrs) if attrs.is_dir() => continue,
                Ok(_) => {
                    return Err(StorageError::Invalid);
                }
                Err(StorageError::NotFound) => {
                    self.client.create_dir(&current).await?;
                }
                Err(other) => return Err(other),
            }
        }
        Ok(())
    }

    /// 递归删除（trait 契约：目录删除为递归；SFTP rmdir 只删空目录，
    /// 深度优先清空再删自身。条目类型以 readdir attrs 为准——链接
    /// 本体按其自身类型处理，绝不跟随）。
    async fn recursive_remove(&self, rel: &RelPath) -> Result<(), StorageError> {
        let attrs = self.stat_path(rel).await?;
        if attrs.is_dir() {
            let dir_path = self.path(rel);
            let names = self
                .client
                .read_dir(&dir_path)
                .await?
                .map(|entry| entry.file_name())
                .collect::<Vec<_>>();
            for name in names {
                let child = rel.join(&name).map_err(|_| StorageError::Invalid)?;
                Box::pin(self.recursive_remove(&child)).await?;
            }
            self.client.remove_dir(&self.path(rel)).await?;
        } else {
            self.client.remove_file(&self.path(rel)).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl StorageDriver for SftpDriver {
    fn volume(&self) -> &VolumeId {
        &self.volume
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // SFTP 原生 offset 读（File 实现 AsyncSeek+AsyncRead；3.0
            // 流水线并发读）——支撑 K47 流式解密播放。
            range_read: true,
            // 无远端分片会话（计划 §4.2）：断点续传由上层 .enc.tmp/
            // 缓存层承担；驱动三硬仗③禁 APPEND 也排除了驱动层续传。
            resume: false,
            // SFTP 无分块上传原语（单流写入，计划 §4.2）。
            multipart: false,
            // SFTP 原生 rename = 服务端单侧 O(1) 移动（计划 §4.2）。
            server_side_move: true,
            // 无内容寻址去重后端（无秒传原语）。
            rapid_upload: false,
            // 远端文件系统即真相（同 local/baidu，D4）——rebuild 可用
            //（rebuild.rs 零 backend 分支，SFTP 同构继承）。
            authoritative_index: true,
            // SFTP 无变更推送通道。
            change_feed: false,
            // 非 bot 后端：无入站通道。
            inbound: false,
            // 无对话通道。
            chat: false,
            // K4：delete_remote 经 transport 面真删远端（transport_
            // face.rs 薄壳委派本面 delete）。
            remote_delete: true,
        }
    }

    /// depth-1 列目录（字典序稳定排序 + `off:N` 不透明分页令牌，
    /// local 同款形态）。行为测试在 SF2 桩批（readdir EOF 语义）。
    async fn list(&self, dir: &RelPath, page: Page) -> Result<Listing, StorageError> {
        let attrs = self.stat_path(dir).await?;
        if !attrs.is_dir() {
            return Err(StorageError::Invalid);
        }
        let dir_path = self.path(dir);
        let mut entries: Vec<Entry> = self
            .client
            .read_dir(&dir_path)
            .await?
            .filter_map(|entry| {
                let name = entry.file_name();
                if name == "." || name == ".." {
                    return None;
                }
                // 词汇层不可表示的名字（驱动侧再硬化：join 拒绝的
                // 组件）不可见即不可寻址
                let child = dir.join(&name).ok()?;
                Some(entry_from_attrs(&self.volume, &child, &entry.metadata()))
            })
            .collect();
        // RelPath 字典序稳定排序（trait 契约；readdir 返回序是实现
        // 细节，必须显式排序）
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let total = entries.len();
        let offset = match page.cursor {
            PageCursor::Start => 0usize,
            // 伪令牌回退 0 重放（local 同款容错）
            PageCursor::Next(tok) => tok
                .strip_prefix("off:")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
        };
        let offset = offset.min(total);
        let end = offset.saturating_add(page.limit).min(total);
        let next = (end < total).then(|| PageCursor::Next(format!("off:{end}")));
        Ok(Listing {
            entries: entries.drain(offset..end).collect(),
            next,
        })
    }

    /// stat（missing → NoSuchFile → NotFound；PermissionDenied /
    /// 连接错误绝不折叠为 NotFound——error.rs 分层映射钉死）。
    async fn stat(&self, path: &RelPath) -> Result<Entry, StorageError> {
        let attrs = self.stat_path(path).await?;
        Ok(entry_from_attrs(&self.volume, path, &attrs))
    }

    /// mkdir：已存在 → Exists（预检）；缺失父目录隐式创建。
    async fn mkdir(&self, path: &RelPath) -> Result<(), StorageError> {
        if path.is_root() {
            return Err(StorageError::Exists); // 卷根恒存在
        }
        match self.stat_path(path).await {
            Ok(_) => return Err(StorageError::Exists), // 目录/同名文件皆拒
            Err(StorageError::NotFound) => {}
            Err(other) => return Err(other),
        }
        if let Some(parent) = path.parent() {
            if !parent.is_root() {
                self.ensure_parents(&parent).await?;
            }
        }
        let target = self.path(path);
        match self.client.create_dir(&target).await {
            Ok(()) => Ok(()),
            // 竞态窗内被他人抢先创建：按 trait 契约报 Exists
            Err(_) if self.stat_path(path).await.is_ok() => Err(StorageError::Exists),
            Err(other) => Err(other),
        }
    }

    /// 按句柄删除；目录递归。不存在 → NotFound（声明形态，local 同款
    /// ——stat 预检天然给出）；他卷句柄 → NotFound；卷根 → Invalid。
    async fn delete(&self, id: &EntryId) -> Result<(), StorageError> {
        if id.volume != self.volume {
            return Err(StorageError::NotFound);
        }
        let Some(rel) = rel_from_handle(id) else {
            return Err(StorageError::Invalid);
        };
        if rel.is_root() {
            return Err(StorageError::Invalid);
        }
        Box::pin(self.recursive_remove(&rel)).await
    }

    /// rename（SFTP rename = 服务端单侧移动，能力位 server_side_move
    /// 的依据）。源缺失 → NotFound、目标已存在 → Exists、目标是源的
    /// 后代 → Invalid、to 的父目录隐式创建。
    async fn rename(&self, from: &RelPath, to: &RelPath) -> Result<(), StorageError> {
        if from.is_root() || to.is_root() {
            return Err(StorageError::Invalid); // 卷根不可作为 rename 端点
        }
        let from_prefix = format!("{}/", from.as_str());
        if to.as_str().starts_with(&from_prefix) {
            return Err(StorageError::Invalid);
        }
        // 源必须存在（显式预检，语义早失败）
        self.stat_path(from).await?;
        // 目标已存在 → Exists（SFTP 服务器对 overwrite rename 的行为
        // 不一致——OpenSSH 拒绝、部分实现覆盖——预检归一）
        match self.stat_path(to).await {
            Ok(_) => return Err(StorageError::Exists),
            Err(StorageError::NotFound) => {}
            Err(other) => return Err(other),
        }
        if let Some(parent) = to.parent() {
            if !parent.is_root() {
                self.ensure_parents(&parent).await?;
            }
        }
        self.client.rename(&self.path(from), &self.path(to)).await
    }

    /// 流式读（Range 半开 + 越界钳制；offset 读 = SFTP 原生 seek）。
    /// 行为测试在 SF2 桩批。
    async fn reader(&self, id: &EntryId, range: Option<Range>) -> Result<ByteStream, StorageError> {
        if id.volume != self.volume {
            return Err(StorageError::NotFound); // 他卷句柄（trait 契约）
        }
        let rel = rel_from_handle(id).ok_or(StorageError::Invalid)?;
        let attrs = self.stat_path(&rel).await?;
        if attrs.is_dir() {
            return Err(StorageError::Invalid); // 句柄指向目录（trait 契约）
        }
        let size = attrs.len();
        // 窗口：None 全量；Some 按半开+钳制（local 同款）
        let (start, len) = match range {
            None => (0, size),
            Some(r) => (r.start, r.clamped_len(size)),
        };
        if len == 0 {
            // 空窗口/空文件/start>=size：声明形态=空流（local 同款；
            // 不开远程句柄——三硬仗①的最好履行是不打开）
            let empty: Vec<Result<Bytes, StorageError>> = Vec::new();
            return Ok(Box::pin(stream::iter(empty)));
        }
        let path = self.path(&rel);
        let mut file = self.client.open_read(&path).await?;
        file.seek(tokio::io::SeekFrom::Start(start))
            .await
            .map_err(|e| StorageError::Io(format!("sftp seek {path}: {e}")))?;
        // 64KiB 帧流；三个终止分支都 awaited close（三硬仗①）
        let frames = stream::unfold(Some((file, len)), |state| async move {
            let (mut f, remaining) = state?;
            if remaining == 0 {
                let _ = f.close().await;
                return None;
            }
            let cap = READ_FRAME.min(remaining) as usize;
            let mut buf = vec![0u8; cap];
            match f.read(&mut buf).await {
                Ok(0) => {
                    // 声明长度内提前 EOF（并发截断）：close 后报错终止
                    let close = f.close().await;
                    let detail = match close {
                        Ok(()) => format!(
                            "sftp reader hit early EOF with {remaining} bytes outstanding \
                             (file truncated concurrently?)"
                        ),
                        Err(e) => format!("sftp reader early EOF and close failed: {e}"),
                    };
                    Some((Err(StorageError::Io(detail)), None))
                }
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok(Bytes::from(buf)), Some((f, remaining - n as u64))))
                }
                Err(e) => {
                    let _ = f.close().await;
                    Some((Err(StorageError::Io(format!("sftp read: {e}"))), None))
                }
            }
        });
        Ok(Box::pin(frames))
    }

    /// 打开上传暂存器（硬仗③：WRITE|CREATE|TRUNCATE——绝不用 APPEND；
    /// commit-on-close 边界见模块文档）。
    async fn writer(
        &self,
        path: &RelPath,
        hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        if path.is_root() {
            return Err(StorageError::Invalid); // 根是目录，不可作为上传目标
        }
        if let Ok(attrs) = self.stat_path(path).await {
            if attrs.is_dir() {
                return Err(StorageError::Invalid); // 目标是已存在目录（trait 契约）
            }
        }
        if let Some(parent) = path.parent() {
            if !parent.is_root() {
                self.ensure_parents(&parent).await?;
            }
        }
        let remote = self.path(path);
        let file = self.client.open_write_truncate(&remote).await?;
        Ok(Box::new(SftpStager {
            volume: self.volume.clone(),
            final_rel: path.clone(),
            client: self.client.clone(),
            file: Some(file),
            written: 0,
            hinted_size: hint.size,
            finished: false,
        }))
    }

    /// 卷配额：statvfs@openssh.com v2 可用则真实数字（total =
    /// blocks*fragment_size），否则 `total = None`（契约「未知/无上
    /// 限」形态）。行为测试在 SF2 桩批。
    async fn quota(&self) -> Result<Quota, StorageError> {
        let root = self.client.params().root.clone();
        match self.client.fs_info(&root).await? {
            Some(statvfs) => {
                let fragment = statvfs.fragment_size.max(1);
                let total = statvfs.blocks.saturating_mul(fragment);
                let free = statvfs.blocks_avail.saturating_mul(fragment);
                Ok(Quota {
                    total: Some(total),
                    used: total.saturating_sub(free),
                })
            }
            None => Ok(Quota {
                total: None,
                used: 0,
            }),
        }
    }
}

/// SFTP 上传暂存器（commit-on-close；三硬仗纪律见 driver.rs 模块文档）。
///
/// - 语义：写入直接落最终路径（计划 §7 裁决不做临时文件+rename——
///   staging 窗口内目标以部分内容可见，边界声明见模块文档）；
/// - 错误：io/协议失败按 error.rs 映射；close 时 size 承诺不符 →
///   `Invalid`（WriteHint 契约）；close 后远端大小 != written → `Io`
///   （硬仗②）；
/// - 并发：单 stager 串行使用（trait 契约）；
/// - 生命周期：close（flush + awaited close + 大小校验）/ abort
///   （awaited close + 尽力删除目标）/ Drop（russh-sftp close_nowait
///   兜底——库行为，不等待确认）三选一收尾。
pub struct SftpStager {
    volume: VolumeId,
    final_rel: RelPath,
    client: Arc<SftpClient>,
    file: Option<russh_sftp::client::fs::File>,
    written: u64,
    hinted_size: Option<u64>,
    finished: bool,
}

#[async_trait]
impl UploadStager for SftpStager {
    async fn write(&mut self, data: &[u8]) -> Result<(), StorageError> {
        let file = self.file.as_mut().ok_or(StorageError::Invalid)?; // close/abort 后再用 = 非法状态
        file.write_all(data)
            .await
            .map_err(|e| StorageError::Io(format!("sftp write: {e}")))?;
        self.written += data.len() as u64;
        Ok(())
    }

    /// 提交：flush → awaited close（硬仗①）→ 远端大小校验（硬仗②）→
    /// Entry。
    async fn close(mut self: Box<Self>) -> Result<Entry, StorageError> {
        let Some(mut file) = self.file.take() else {
            return Err(StorageError::Invalid); // 未打开/已收尾
        };
        if let Some(hinted) = self.hinted_size {
            if hinted != self.written {
                // WriteHint 契约：承诺与实际不符；file 由 russh-sftp Drop
                // 收尾（close_nowait），远端残留是 truncate 语义的既定
                // 边界（模块文档）
                self.finished = true;
                return Err(StorageError::Invalid);
            }
        }
        file.flush()
            .await
            .map_err(|e| StorageError::Io(format!("sftp flush: {e}")))?;
        let _ = file.sync_all().await; // fsync@openssh.com 可选扩展，尽力
                                       // 硬仗①：awaited close——等待在途写与 close 确认，之后远端
                                       // 状态才可信
        file.close()
            .await
            .map_err(|e| StorageError::Io(format!("sftp close after upload: {e}")))?;
        self.finished = true;
        // 硬仗②：close 后校验远端大小（短/零判失败——0 字节上传 bug
        // 的唯一持久修复）
        let remote = remote_path(&self.client.params().root, &self.final_rel);
        let attrs = self.client.metadata(&remote).await?;
        let remote_size = attrs.len();
        if remote_size != self.written {
            return Err(StorageError::Io(format!(
                "sftp upload size mismatch: wrote {} bytes but the remote reports {remote_size}",
                self.written
            )));
        }
        Ok(entry_from_attrs(&self.volume, &self.final_rel, &attrs))
    }

    /// 放弃：awaited close + 尽力删除目标（我们创建/截断的对象；覆盖
    /// 场景旧版本不可恢复——模块文档边界声明）。
    async fn abort(mut self: Box<Self>) -> Result<(), StorageError> {
        if let Some(file) = self.file.take() {
            let _ = file.close().await; // 硬仗①：失败分支也 awaited close
        }
        self.finished = true;
        let remote = remote_path(&self.client.params().root, &self.final_rel);
        match self.client.remove_file(&remote).await {
            Ok(()) => Ok(()),
            Err(StorageError::NotFound) => Ok(()), // 已消失视为已清理
            Err(other) => Err(other),
        }
    }
}

impl Drop for SftpStager {
    /// 未 close/abort 即丢弃 → russh-sftp File 的 Drop 兜底（排入
    /// close_nowait，不等待确认——库行为；三硬仗①的异常路径边界，
    /// SF2 桩的服务端句柄计数测试钉住）。
    fn drop(&mut self) {
        let _ = self.file.take();
    }
}
