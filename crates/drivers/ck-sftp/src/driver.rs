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
//! ## commit-on-close：暂存件 + rename + stash（与 ck-local 同构）
//!
//! 计划 §7 的「远程临时文件 + rename」由 conformance 断言①（close 前
//! 目标不可见，interfaces §6）钉死为**必须实现**——本次裁决：以
//! SFTP 原生 rename 实现与 ck-local 同构的提交协议：
//!
//! - 数据落 `<final>.cksftp-<pid>-<seq>.part`（同目录——SFTP rename
//!   的原子性仅在**同一服务器文件系统内**成立，跨设备 rename 会失败，
//!   同目录命名保证同设备）；
//! - 覆盖场景：旧对象先 `rename(final → <final>.cksftp-<pid>-<seq>.old)`
//!   stash——staging 窗口内目标路径不可见（stat → NotFound），断言①
//!   的覆盖腿亦满足；
//! - `close` = part→final rename（目标已空，POSIX rename 语义直接
//!   落位）→ 远端 size 校验（硬仗②）→ 删 stash；
//! - `abort` = 删 part + stash→final rename 恢复旧版（abort = 回到
//!   writer 打开前状态，local 同款）；
//! - `.cksftp-` 命名的暂存件对 `list` 恒过滤（驱动实现细节，不是卷
//!   内容——断言③集合完整性依赖；local 的 `.cklocal-staging/` 同源）。
//!
//! **真机（OpenSSH）的形态差**：rename 语义与 POSIX 一致，stash 腿在
//! 真机上等价成立；SF4 真机矩阵复验该协议（含 crash 残留的 `.cksftp-`
//! 件清理——断言③过滤同时覆盖残留不可见）。
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
    /// 深度优先清空再删自身）。
    ///
    /// **符号链接契约（aeroftp 教训 8 / 其 GAP-A02，真机矩阵揭出的
    /// 缺陷修复）**：条目类型以 **lstat**（不跟随）判定——link-to-dir
    /// 的本体是链接而非目录：递归**绝不下潜**进链接目标（跟随形态会
    /// 把 `/etc` 之类的链接目标当子目录枚举并删除）。链接条目按文件
    /// 语义删（`remove_file` 在 OpenSSH 上删链接本体、不动目标）。
    async fn recursive_remove(&self, rel: &RelPath) -> Result<(), StorageError> {
        let attrs = self.client.symlink_metadata(&self.path(rel)).await?;
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
    ///
    /// **目录性预检用 lstat（不跟随）**：link-to-dir 的本体是链接——
    /// 把它当目录会枚举链接目标（真机矩阵揭出：指向 `/etc` 的链会列出
    /// 207 个宿主条目，且为递归删除/rebuild 走查打开下潜通道）→
    /// 拒以 `Invalid`（walkable 判定拒绝下潜，aeroftp 教训 8）。
    /// 条目 kind 取 readdir attrs（OpenSSH 报链接本体形态：link-to-dir
    /// 在 list 中呈现为 File/link 长度——与 POSIX lstat 观感一致）。
    ///
    /// **根例外（审查修复）**：`dir` 为卷根时预检用**跟随** stat——根是
    /// 操作者经 `sftp_root` 声明的挂载点，不是走查发现的条目，symlink
    /// 根（如 `/var/www → /srv/www`）必须可列（修复前 symlink 根让整卷
    /// 列表面报 Invalid）。跟随的只是挂载点解析，卷内条目仍走 lstat
    /// ——GAP-A02 的防线下潜不重开。
    async fn list(&self, dir: &RelPath, page: Page) -> Result<Listing, StorageError> {
        let attrs = if dir.is_root() {
            self.stat_path(dir).await?
        } else {
            self.client.symlink_metadata(&self.path(dir)).await?
        };
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
                // 暂存件/孤儿残件（`.cksftp-<pid>-<seq>.part|.old`）是
                // 驱动实现细节，不是卷内容（断言③集合完整性；local 的
                // `.cklocal-staging/` 过滤同源）
                if is_staging_artifact(&name) {
                    return None;
                }
                // 词汇层不可表示的名字（join 拒绝的组件）不可见即不可
                // 寻址；**不可寻址名同样不可见（审查修复）**——句柄往返
                // `RelPath::new` 拒 `\`/`\0`，非 UTF-8 名被 russh-sftp
                // 协议层 lossy 成 U+FFFD 替换串（替换名在服务器上并非
                // 真实路径）——ck-local `join_validated` / 非 UTF-8 跳过
                // 的同源硬化：list 产出的每条 Entry 都必须能被本驱动
                // 的 delete/reader 寻址。
                let child = match dir.join(&name) {
                    Ok(child) if name_is_addressable(&name) => child,
                    _ => return None,
                };
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
    ///
    /// 链接语义（与 `list` 保持一致）：**用 lstat 报本体形态**——
    /// link-to-dir 报 `File`（链接自身），而不是跟随后的 `Dir`。这样
    /// `stat`/`list` 对同一条目给出一致的 kind，`entry.kind == Dir`
    /// 的消费方（递归走查/rebuild/列表渲染）不会把链接误当目录下潜。
    /// 需要跟随语义的读取（reader/下载）仍走 `metadata`（SSH_FXP_STAT
    /// 跟随，能读到链接指向的内容——这是用户预期）。
    ///
    /// **根例外（审查修复）**：卷根与 `list` 的根预检同裁决——跟随
    /// 解析（symlink 根报目标目录的 `Dir`，而非链接本体的 `File`）。
    async fn stat(&self, path: &RelPath) -> Result<Entry, StorageError> {
        let attrs = if path.is_root() {
            self.stat_path(path).await?
        } else {
            self.client.symlink_metadata(&self.path(path)).await?
        };
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
        // 不一致——OpenSSH 拒绝、部分实现覆盖——预检归一）。预检走
        // **lstat**（M4：悬空链也是既有目录项——跟随 stat 对悬空链误报
        // NotFound，把服务端必然的拒绝漏成 Io/假成功）。
        match self.client.symlink_metadata(&self.path(to)).await {
            Ok(_) => return Err(StorageError::Exists),
            Err(StorageError::NotFound) => {}
            Err(other) => return Err(other),
        }
        if let Some(parent) = to.parent() {
            if !parent.is_root() {
                self.ensure_parents(&parent).await?;
            }
        }
        match self.client.rename(&self.path(from), &self.path(to)).await {
            Ok(()) => Ok(()),
            // 竞态窗内目标被抢占创建（reject 形态服务器对已存在目标回
            // Failure→Io）：按 trait 契约归一 Exists——镜像 mkdir 的
            // 竞态臂（审查修复：修复前该交错 surfaced 为 Io）。判定同
            // 走 lstat（与预检同面，悬空链形态一致）。
            Err(_) if self.client.symlink_metadata(&self.path(to)).await.is_ok() => {
                Err(StorageError::Exists)
            }
            Err(other) => Err(other),
        }
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

    /// 打开上传暂存器（硬仗③：WRITE|CREATE（无 TRUNCATE 需求——part
    /// 是新文件）——绝不用 APPEND；commit-on-close 协议见模块文档：
    /// 数据落 `.part` 暂存件，覆盖场景旧对象先 stash 成 `.old`）。
    async fn writer(
        &self,
        path: &RelPath,
        hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        if path.is_root() {
            return Err(StorageError::Invalid); // 根是目录，不可作为上传目标
        }
        // 目标形态预检：已存在目录 → Invalid（trait 契约）；文件 → 待
        // stash（下）；缺失 → 直建 part
        let existing = match self.stat_path(path).await {
            Ok(attrs) if attrs.is_dir() => return Err(StorageError::Invalid),
            Ok(_) => true,
            Err(StorageError::NotFound) => false,
            Err(other) => return Err(other),
        };
        if let Some(parent) = path.parent() {
            if !parent.is_root() {
                self.ensure_parents(&parent).await?;
            }
        }
        // 暂存名：final 同目录 + `.cksftp-<pid>-<seq>.part`（同目录保证
        // rename 同设备——SFTP rename 跨设备失败；list 侧按前缀过滤）
        let final_remote = self.path(path);
        let (part_remote, old_remote) = staging_names(&final_remote);
        let file = self.client.open_write_truncate(&part_remote).await?;
        // 覆盖场景：旧对象 stash（staging 窗口内最终路径不可见——断言①
        // 覆盖腿；rename 到 .old 后 final 为空，close 的 rename 直接落位）
        let stash = if existing {
            match self.client.rename(&final_remote, &old_remote).await {
                Ok(()) => Some(old_remote),
                // 竞态窗内旧对象消失：按「原本不存在」继续
                Err(StorageError::NotFound) => None,
                Err(other) => {
                    // 收拾刚建的 part 再报错（不留孤儿）
                    let _ = self.client.remove_file(&part_remote).await;
                    return Err(other);
                }
            }
        } else {
            None
        };
        Ok(Box::new(SftpStager {
            volume: self.volume.clone(),
            final_rel: path.clone(),
            client: self.client.clone(),
            file: Some(file),
            part_remote,
            stash_remote: stash,
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

/// SFTP 上传暂存器（commit-on-close：`.part` 暂存 + rename 固化 +
/// `.old` stash；协议见 driver.rs 模块文档）。
///
/// - 语义：数据落同目录 `.cksftp-<pid>-<seq>.part`；覆盖场景旧对象在
///   writer 打开时已 stash 成 `.old`（staging 窗口目标不可见，断言①）；
/// - 错误：io/协议失败按 error.rs 映射；close 时 size 承诺不符 →
///   `Invalid`（WriteHint 契约）；close 后远端大小 != written → `Io`
///   （硬仗②）；
/// - 并发：单 stager 串行使用（trait 契约）；
/// - 生命周期：close（flush + awaited close + rename + 大小校验 + 删
///   stash）/ abort（awaited close + 删 part + stash→final 恢复）/
///   Drop（awaited close 无路——russh-sftp close_nowait 兜底 + 同步
///   part 清理不保证；残件靠 list 过滤不可见，模块文档边界）三选一。
pub struct SftpStager {
    volume: VolumeId,
    final_rel: RelPath,
    client: Arc<SftpClient>,
    file: Option<russh_sftp::client::fs::File>,
    /// 暂存件远端路径（`.cksftp-<pid>-<seq>.part`）。
    part_remote: String,
    /// 被 stash 的旧版本远端路径（`.old`）；None = 目标原本不存在。
    stash_remote: Option<String>,
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

    /// 提交：flush → awaited close（硬仗①）→ part→final rename（原子
    /// 固化）→ 远端大小校验（硬仗②）→ 删 stash → Entry。
    ///
    /// 失败路径尽力恢复现场（async 面可做真正的 stash→final 恢复——
    /// 与 LocalStager 的 Drop 同步恢复同语义）：提交失败时 target 不得
    /// 停在被 stash 丢空的状态。恢复失败只留不可见残件（list 过滤），
    /// 不吞原错误。
    async fn close(mut self: Box<Self>) -> Result<Entry, StorageError> {
        let Some(mut file) = self.file.take() else {
            return Err(StorageError::Invalid); // 未打开/已收尾
        };
        if let Some(hinted) = self.hinted_size {
            if hinted != self.written {
                // WriteHint 契约：承诺与实际不符——恢复现场（删 part /
                // 复位旧版）后报 Invalid
                let _ = file.close().await;
                self.finished = true;
                self.restore_scene().await;
                return Err(StorageError::Invalid);
            }
        }
        if let Err(e) = file.flush().await {
            let _ = file.close().await;
            self.finished = true;
            self.restore_scene().await;
            return Err(StorageError::Io(format!("sftp flush: {e}")));
        }
        let _ = file.sync_all().await; // fsync@openssh.com 可选扩展，尽力
                                       // 硬仗①：awaited close——等待在途写与 close 确认，之后远端
                                       // 状态才可信
        if let Err(e) = file.close().await {
            self.finished = true;
            self.restore_scene().await;
            return Err(StorageError::Io(format!("sftp close after upload: {e}")));
        }
        // 原子固化：final 此刻为空（旧版已 stash）——POSIX rename 语义
        // 直接落位；这是断言①「close 后立即可见」的兑现点。
        //
        // **重放窗（审查修复）**：rename 可能在服务端已执行而回复丢失
        //（连接死亡 → with_retry 重连 → 重放 rename → part 已不在 →
        // NotFound）——探测 final 是否已就位且尺寸恰为 written：是 =
        // 提交已落地，按已提交继续（下方校验/清 stash 照常）；否 = 真
        // 失败，恢复现场后报原错误。不探测直接 restore_scene 会把 stash
        // 复位回去——**把已提交的新版本覆盖回旧版（数据丢失）**。
        let final_remote = remote_path(&self.client.params().root, &self.final_rel);
        if let Err(e) = self.client.rename(&self.part_remote, &final_remote).await {
            let already_committed = matches!(e, StorageError::NotFound)
                && matches!(
                    self.client.metadata(&final_remote).await,
                    Ok(attrs) if attrs.len() == self.written
                );
            if !already_committed {
                self.finished = true;
                // rename 未确认落地（K67 重放窗的否臂）：恢复 writer 打开
                // 前状态。final 可能被占（我方 rename 实已落地但回复形态
                // 不在豁免内 / 竞态外物）——先清出 final 再走常规恢复；
                // 直接 restore_scene 的 stash→final rename 会被占据的
                // final 拒绝，其旧兜底还会误删 stash（旧版本唯一副本）。
                self.restore_scene_after_commit().await;
                return Err(e);
            }
        }
        self.finished = true;
        // 硬仗②：close 后校验远端大小（短/零判失败——0 字节上传 bug
        // 的唯一持久修复）
        let attrs = self.client.metadata(&final_remote).await?;
        let remote_size = attrs.len();
        if remote_size != self.written {
            // 提交已落但内容不可信（服务端短写/并发篡改）：同走提交窗
            // 恢复——嫌疑版本清除、旧版本从 stash 复位。修复前该分支
            // 直接返回：final 停在嫌疑版本，.old 永久遗留（旧版本从此
            // 不可见 = 数据丢失，H1）。
            self.restore_scene_after_commit().await;
            return Err(StorageError::Io(format!(
                "sftp upload size mismatch: wrote {} bytes but the remote reports {remote_size}",
                self.written
            )));
        }
        // 新版本已就位，stash 作废（best-effort——失败只留不可见残件）
        if let Some(old) = &self.stash_remote {
            let _ = self.client.remove_file(old).await;
        }
        Ok(entry_from_attrs(&self.volume, &self.final_rel, &attrs))
    }

    /// 放弃：awaited close → 删 part → stash→final 恢复（abort = 回到
    /// writer 打开前状态，local stager 同款恢复语义）。
    async fn abort(mut self: Box<Self>) -> Result<(), StorageError> {
        if let Some(file) = self.file.take() {
            let _ = file.close().await; // 硬仗①：失败分支也 awaited close
        }
        self.finished = true;
        self.restore_scene().await;
        Ok(())
    }
}

impl SftpStager {
    /// 现场恢复（close 失败 / abort 的共用收尾）：删 part 暂存件 +
    /// stash→final 复位。order 有讲究——先复位 stash 再删 part 会与
    /// 「part 是孤儿」的判定互不影响，这里按「删自己的、还别人的」
    /// 顺序：part 是我们创建的（删除安全），stash 是别人的（归还）。
    /// 每步 best-effort：远端残件不可见（list 过滤），调用方错误优先。
    async fn restore_scene(&self) {
        match self.client.remove_file(&self.part_remote).await {
            Ok(()) | Err(StorageError::NotFound) => {}
            Err(_) => {} // 残件不可见（list 过滤）
        }
        if let Some(old) = &self.stash_remote {
            let final_remote = remote_path(&self.client.params().root, &self.final_rel);
            // 复位失败只留不可见残件（模块文档「恢复失败只留不可见
            // 残件」契约）——绝不删 stash 本体（旧版本唯一副本；修复前
            // 的删除兜底在 final 被占形态下就是数据丢失，H1 家族）。
            let _ = self.client.rename(old, &final_remote).await;
        }
    }

    /// 提交窗失败的恢复（close 的 rename 之后各失败臂共用）：回到
    /// writer 打开前状态——先清出 final（NotFound = 本就空缺；占据者
    /// 是我方嫌疑字节或竞态外物，staging 契约下该路径归 writer 所有），
    /// 再走常规 [`Self::restore_scene`]（删 part 残件 + stash→final）。
    async fn restore_scene_after_commit(&self) {
        let final_remote = remote_path(&self.client.params().root, &self.final_rel);
        let _ = self.client.remove_file(&final_remote).await;
        self.restore_scene().await;
    }
}

/// 暂存件命名（`final` 同目录）：`.cksftp-<pid>-<seq>.part` / `.old`。
///
/// 同目录是硬要求——SFTP rename 的原子性只在同一服务器文件系统内成立
/// （跨设备 rename 直接失败）；这一形态让 close 的固化对真机 OpenSSH
/// 同样成立（SF4 复验）。进程级单调序号防同进程多 stager 撞名。
fn staging_names(final_remote: &str) -> (String, String) {
    static STAGING_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = STAGING_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let suffix = format!(".cksftp-{pid}-{seq}");
    (
        format!("{final_remote}{suffix}.part"),
        format!("{final_remote}{suffix}.old"),
    )
}

/// 暂存件前缀过滤（list 不可见——断言③集合完整性的驱动侧保障；同时
/// 覆盖 crash/close 失败残留的孤儿件）。判定 = 文件名含 `.cksftp-`
/// 且以 `.part`/`.old` 结尾（双重条件防误伤用户合法文件名——两者
/// 同时命中才算驱动残件）。
pub(crate) fn is_staging_artifact(name: &str) -> bool {
    name.contains(".cksftp-") && (name.ends_with(".part") || name.ends_with(".old"))
}

/// 组件名可寻址性（审查修复，ck-local `join_validated` 同源硬化）：
/// list 产出的每条 Entry 必须能被本驱动的 delete/reader 寻址——句柄
/// 字符串 → `RelPath::new` 的往返拒绝 `\` 与 `\0`；russh-sftp 协议层
/// 对非 UTF-8 名做 **lossy 替换**（U+FFFD 替换串在服务器上并非真实
/// 路径，对它的任何寻址都是 NotFound）。三类名字不可见（不可寻址即
/// 不可见；local 对非 UTF-8 名 `to_str()` 失败即跳过的同款立场）。
pub(crate) fn name_is_addressable(name: &str) -> bool {
    !name.contains('\\') && !name.contains('\0') && !name.contains('\u{FFFD}')
}

impl Drop for SftpStager {
    /// 未 close/abort 即丢弃（消费者异常路径）→ russh-sftp File 的 Drop
    /// 兜底（排入 close_nowait，不等待确认——库行为；三硬仗①的异常
    /// 路径边界，SF2 桩的服务端句柄计数测试钉住）。暂存件与 stash 的
    /// 远端残件不可见（list 过滤），不阻塞后续同名路径的上传（part
    /// 名带进程级序号，不撞旧件）。
    fn drop(&mut self) {
        let _ = self.file.take();
    }
}
