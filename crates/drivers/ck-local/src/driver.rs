//! LocalDriver——本地文件系统 StorageDriver（Phase 2 Batch L）。
//!
//! 后端模型：卷根目录即「远端」，[`RelPath`] 组件直接映射为根下路径。
//! 无网络、无后端错误码——驱动唯一的错误源是 OS 文件系统（io::Error）。
//!
//! ## io::Error → StorageError 映射表（interfaces §3 驱动义务）
//!
//! | io::ErrorKind | StorageError |
//! |---|---|
//! | `NotFound` | `NotFound` |
//! | `AlreadyExists` | `Exists` |
//! | `PermissionDenied` | `Unauthorized { recoverable: false }`（本地盘无 token 可自救，重试无益） |
//! | `InvalidInput` | `Invalid` |
//! | 其他（含 `Uncategorized`/`InvalidData` 等） | `Io(原始消息)`——可诊断不丢信息 |
//!
//! conformance 断言⑤对 local 类为空表回放（无后端错误码可注入，
//! interfaces §6 豁免，harness 已按此声明）。
//!
//! ## 保留名规则（驱动私有，词汇层不可见）
//!
//! - `.cklocal-staging/`（卷根下）：上传暂存目录（[`crate::stager`]），
//!   `list(根)` 恒过滤它——它是驱动实现细节，不是卷内容（conformance
//!   断言③的集合完整性依赖此过滤）；
//! - 文件名含 Windows 保留/ADS 语义字符（`:` `?` `*` `<` `>` `|` `"`）
//!   的条目不可见且不可寻址（[`LocalDriver::fs_path`] 的硬化校验）；
//! - 非 UTF-8 文件名：无法经词汇层（`RelPath`）往返，list 中不可见。
//!
//! ## 并发与一致性
//!
//! 全方法可并发调用（trait 契约，`&self` 无跨方法锁）；OS 文件系统语义
//! 即一致性来源，检查-执行窗内的竞态与 POSIX 语义一致（如 rename 竞态
//! 覆盖），由调用方约定。reader/进行中条目被并发删除或截断：reader 流
//! 中途浮现错误项（trait 契约「错误可在流中途浮现」）；list 跳过窗口内
//! 消失的条目（不承诺快照一致性）。

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

use cloudkit_storage::{
    BackendHandle, ByteStream, Capabilities, Entry, EntryId, EntryKind, Listing, Page, PageCursor,
    Quota, Range, RelPath, StorageDriver, StorageError, UploadStager, VolumeId, WriteHint,
};

use crate::stager::LocalStager;

/// 卷根下的上传暂存目录名（驱动保留名；`list(根)` 过滤，见模块文档）。
pub(crate) const STAGING_DIR_NAME: &str = ".cklocal-staging";

/// reader 每帧读取的字节数（64KiB——顺序读的常规页粒度）。
const READ_FRAME: u64 = 64 * 1024;

/// staging 临时名序号（进程级单调——同进程多驱动实例间也不撞名）。
static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

/// 组件名是否含 Windows 保留/ADS 语义字符。
///
/// `\` 与 `\0` 在词汇层（`RelPath::new`）已被拒；这里是驱动侧对 `:`
/// 等字符的追加硬化——词汇层必须允许 `:`（VolumeId key 形态需要），
/// 但作为文件名组件在 Windows 上是 NTFS 备用数据流/非法形态。驱动可比
/// L2 词汇层更严（跨平台统一拒绝，避免「Linux 可建、Windows 不可寻址」
/// 的跨平台条目）。`\0` 一并列出是防御性冗余（词汇层已拦）。
fn has_reserved_char(comp: &str) -> bool {
    comp.contains([':', '?', '*', '<', '>', '|', '"', '\0'])
}

/// io::Error → StorageError 的 kind 敏感映射（模块文档映射表）。
///
/// 取代 `?` 的 blanket `From`（恒 Io）：NotFound/Exists 等语义形态必须
/// 按 kind 还原，conformance 断言④（delete 不存在 → NotFound）依赖它。
pub(crate) fn map_io(e: io::Error) -> StorageError {
    match e.kind() {
        io::ErrorKind::NotFound => StorageError::NotFound,
        io::ErrorKind::AlreadyExists => StorageError::Exists,
        io::ErrorKind::PermissionDenied => StorageError::Unauthorized {
            recoverable: false, // 本地盘无 token 可自救；重授权 = 人工修文件系统权限
        },
        io::ErrorKind::InvalidInput => StorageError::Invalid,
        _ => StorageError::Io(e.to_string()),
    }
}

/// SystemTime → f64 epoch 秒（interfaces §5 时间戳形态；epoch 前钳制 0.0）。
pub(crate) fn mtime_secs(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// metadata → Entry（stat/list/close 三处共用）。
///
/// - id.handle = rel_path 字符串形态（K6：可往返——`RelPath::new(handle)`
///   复原同一 rel；根 = 空串）；
/// - 目录 size 恒 0（conformance 断言③；Windows 目录 metadata 的 len()
///   是实现细节值，不透出）；
/// - mtime 取 modified()（跟随符号链接——fs::metadata 语义）。
pub(crate) fn entry_from_meta(volume: &VolumeId, rel: &RelPath, meta: &std::fs::Metadata) -> Entry {
    let is_dir = meta.is_dir();
    Entry {
        id: EntryId::new(volume.clone(), BackendHandle::new(rel.as_str())),
        path: rel.clone(),
        kind: if is_dir {
            EntryKind::Dir
        } else {
            EntryKind::File
        },
        size: if is_dir { 0 } else { meta.len() },
        mtime: meta.modified().map(mtime_secs).unwrap_or(0.0),
    }
}

/// 句柄 → RelPath（K6 往返解析；不可解析形态 → None）。
fn rel_from_handle(id: &EntryId) -> Option<RelPath> {
    RelPath::new(id.handle.as_str()).ok()
}

/// 本地文件系统驱动：卷根目录即后端。
///
/// - 语义：根下普通 FS；写入路径缺失父目录隐式创建（trait 契约）；
/// - 错误：io::Error 按 [`map_io`] 映射表归一（R2）；
/// - 并发：全方法可并发调用，无驱动级锁（OS 语义即一致性来源）；
/// - 生命周期：无连接/会话资源；stager 生命周期见 [`crate::stager`]。
pub struct LocalDriver {
    volume: VolumeId,
    /// 规范化绝对根（构造时 canonicalize；Windows 为 `\\?\` verbatim 形态）。
    root: PathBuf,
}

impl LocalDriver {
    /// 同步构造（测试与同步装配入口；async 装配走 [`crate::factory`]）：
    /// 创建根目录（幂等）→ 规范化为绝对路径 → 卷身份 `local:<root>`。
    ///
    /// 错误：根不可创建/不可规范化（权限、路径非法）→ 按 [`map_io`]
    /// 映射（典型 PermissionDenied → `Unauthorized`）。
    ///
    /// Windows 上规范化产生 `\\?\` 扩展路径前缀——VolumeId 的 key 对
    /// L2 是 opaque 字符串（ids.rs D6），该形态合法。
    pub fn new(root: PathBuf) -> Result<Self, StorageError> {
        std::fs::create_dir_all(&root).map_err(map_io)?;
        let canonical = std::fs::canonicalize(&root).map_err(map_io)?;
        let volume = VolumeId::new("local", &canonical.to_string_lossy())?;
        Ok(LocalDriver {
            volume,
            root: canonical,
        })
    }

    /// RelPath → 卷内绝对路径（组件逐一拼接）。
    ///
    /// 路径安全：组件已过词汇层校验（无 `/`、无 `..`、无 `\`、非空），
    /// 拼接只可能产出根下路径——构造即钉死在根内，无需事后 canonicalize
    /// 前缀比对。驱动侧追加硬化：组件含保留字符 → `Invalid`（模块文档
    /// 保留名规则）。
    fn fs_path(&self, rel: &RelPath) -> Result<PathBuf, StorageError> {
        let mut path = self.root.clone();
        for comp in rel.components() {
            if has_reserved_char(comp) {
                return Err(StorageError::Invalid);
            }
            path.push(comp);
        }
        Ok(path)
    }
}

#[async_trait]
impl StorageDriver for LocalDriver {
    fn volume(&self) -> &VolumeId {
        &self.volume
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // reader(range) 半开区间全套（断言②）：精确窗口/越界钳制/空窗口/
            // start>=size=空流（harness empty_range_yields_empty_stream=true）。
            range_read: true,
            // 暂存是根下临时文件：stager Drop/abort 即删、不跨 stager 生命
            // 周期复活——无「已传分片」可复用，差集续传无从谈起（断言⑦
            // 因此跳过）。
            resume: false,
            // 本地 FS 无远端分片概念（无 multipart 会话后端形态）。
            multipart: false,
            // fs::rename = 同卷原子移动（Windows 实现为 MoveFileEx
            // (REPLACE_EXISTING)，POSIX rename(2) 同义），无拷贝重传
            //（断言⑥验证文件+目录搬移）。
            server_side_move: true,
            // 无内容寻址去重后端（秒传需要后端按指纹直接落盘）。
            rapid_upload: false,
            // list 即本地 FS 真相，无影子索引（D4；断言③）；local 类真机
            // 豁免（driver-onboarding §3），离线套件即全量验证。
            authoritative_index: true,
            // 非云后端：无变更推送通道。
            change_feed: false,
            // 无入站通道（bot 收文件类能力不适用）。
            inbound: false,
            // 无对话通道。
            chat: false,
            // K4（B3b transport 面）：delete_remote 经本驱动 delete 真删
            // 卷内文件（transport_face.rs 薄壳委派）；StorageDriver.delete
            // 的真删语义不变。
            remote_delete: true,
        }
    }

    async fn list(&self, dir: &RelPath, page: Page) -> Result<Listing, StorageError> {
        let dir_path = self.fs_path(dir)?;
        // 预检目录性：read_dir 对「文件/缺失」的错误 kind 平台不一致
        //（Windows 对文件报权限类），预检归一为 NotFound/Invalid。
        let meta = tokio::fs::metadata(&dir_path).await.map_err(map_io)?;
        if !meta.is_dir() {
            return Err(StorageError::Invalid);
        }
        let mut rd = tokio::fs::read_dir(&dir_path).await.map_err(map_io)?;
        let mut entries = Vec::new();
        while let Some(de) = rd.next_entry().await.map_err(map_io)? {
            // 非 UTF-8 文件名无法经 RelPath 往返 → 不可见（模块文档保留名规则）
            let file_name = de.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            // 暂存目录是驱动实现细节，list(根) 恒过滤（断言③集合完整性）
            if dir.is_root() && name == STAGING_DIR_NAME {
                continue;
            }
            let Some(child) = join_validated(dir, name) else {
                continue; // 保留字符名：不可寻址即不可见
            };
            // 竞态窗内消失的条目跳过（不承诺快照一致性）
            if let Ok(m) = de.metadata().await {
                entries.push(entry_from_meta(&self.volume, &child, &m));
            }
        }
        // RelPath 字典序稳定排序（trait 契约「稳定有序」；read_dir 返回序
        // 是 FS 实现细节，必须显式排序）
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let total = entries.len();
        let offset = match page.cursor {
            PageCursor::Start => 0usize,
            // 不透明令牌（mock 的 "off:{end}" 形态）；伪令牌回退 0 重放
            PageCursor::Next(tok) => tok
                .strip_prefix("off:")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
        };
        let offset = offset.min(total); // 伪超大 offset 钳制
        let end = offset.saturating_add(page.limit).min(total);
        let next = (end < total).then(|| PageCursor::Next(format!("off:{end}")));
        Ok(Listing {
            entries: entries.drain(offset..end).collect(),
            next,
        })
    }

    async fn stat(&self, path: &RelPath) -> Result<Entry, StorageError> {
        let p = self.fs_path(path)?;
        // fs::metadata 跟随符号链接；断链 symlink → NotFound（kind 归一）
        let meta = tokio::fs::metadata(&p).await.map_err(map_io)?;
        Ok(entry_from_meta(&self.volume, path, &meta))
    }

    async fn mkdir(&self, path: &RelPath) -> Result<(), StorageError> {
        let p = self.fs_path(path)?;
        // 已存在（含卷根本身）→ Exists（断言④；mock 同款——根恒存在）
        if tokio::fs::metadata(&p).await.is_ok() {
            return Err(StorageError::Exists);
        }
        // create_dir_all = 隐式父目录（断言④）
        tokio::fs::create_dir_all(&p).await.map_err(map_io)?;
        Ok(())
    }

    async fn delete(&self, id: &EntryId) -> Result<(), StorageError> {
        // 他卷句柄 → NotFound（trait 契约与 mock 先例：本卷视角下他卷
        // 对象即不存在，与 reader 同形态）；空句柄（= 卷根，删除卷根
        // 无意义且危险）→ Invalid
        if id.volume != self.volume {
            return Err(StorageError::NotFound);
        }
        let Some(rel) = rel_from_handle(id) else {
            return Err(StorageError::Invalid);
        };
        if rel.is_root() {
            return Err(StorageError::Invalid);
        }
        let p = self.fs_path(&rel)?;
        let meta = tokio::fs::metadata(&p).await.map_err(map_io)?;
        if meta.is_dir() {
            // 目录递归删除（断言④）
            tokio::fs::remove_dir_all(&p).await.map_err(map_io)?;
        } else {
            tokio::fs::remove_file(&p).await.map_err(map_io)?;
        }
        Ok(())
    }

    async fn rename(&self, from: &RelPath, to: &RelPath) -> Result<(), StorageError> {
        if from.is_root() || to.is_root() {
            return Err(StorageError::Invalid); // 卷根不可作为 rename 端点
        }
        // 目标是源的后代 → Invalid（trait 契约；mock 同款）
        let from_prefix = format!("{}/", from.as_str());
        if to.as_str().starts_with(&from_prefix) {
            return Err(StorageError::Invalid);
        }
        let from_path = self.fs_path(from)?;
        let to_path = self.fs_path(to)?;
        // 源必须存在（fs::rename 对缺失源在两平台都报 NotFound kind，
        // 预检让语义显式且错误更早浮现）
        tokio::fs::metadata(&from_path).await.map_err(map_io)?;
        // 目标已存在 → Exists（trait 契约）——必须预检：Windows/POSIX 的
        // rename 对已存在目标是「原子覆盖」而非报错
        if tokio::fs::metadata(&to_path).await.is_ok() {
            return Err(StorageError::Exists);
        }
        if let Some(parent) = to_path.parent() {
            // 隐式父目录（写入路径契约）
            tokio::fs::create_dir_all(parent).await.map_err(map_io)?;
        }
        // 同卷原子移动——能力位 server_side_move 的依据
        tokio::fs::rename(&from_path, &to_path)
            .await
            .map_err(map_io)?;
        Ok(())
    }

    async fn reader(&self, id: &EntryId, range: Option<Range>) -> Result<ByteStream, StorageError> {
        if id.volume != self.volume {
            return Err(StorageError::NotFound); // 他卷句柄（trait 契约/mock 先例）
        }
        let rel = rel_from_handle(id).ok_or(StorageError::Invalid)?;
        let p = self.fs_path(&rel)?;
        let meta = tokio::fs::metadata(&p).await.map_err(map_io)?;
        if meta.is_dir() {
            return Err(StorageError::Invalid); // 句柄指向目录（trait 契约）
        }
        let size = meta.len();
        // 窗口：None 全量；Some 按半开+钳制——clamped_len 统一处理
        // end=None→EOF、end 越界钳到 EOF、start>=size→0
        let (start, len) = match range {
            None => (0, size),
            Some(r) => (r.start, r.clamped_len(size)),
        };
        if len == 0 {
            // 空窗口/空文件/start>=size：声明形态=空流（断言②）
            let empty: Vec<Result<Bytes, StorageError>> = Vec::new();
            return Ok(Box::pin(stream::iter(empty)));
        }
        let mut file = tokio::fs::File::open(&p).await.map_err(map_io)?;
        file.seek(SeekFrom::Start(start)).await.map_err(map_io)?;
        // 64KiB 帧流：io 错误在流中途浮现为 Err 项（trait 契约）
        let frames = stream::unfold((file, len), |(mut f, remaining)| async move {
            if remaining == 0 {
                return None;
            }
            let cap = READ_FRAME.min(remaining) as usize;
            let mut buf = vec![0u8; cap];
            match f.read(&mut buf).await {
                Ok(0) => {
                    // 声明长度内提前 EOF（并发截断）：报错并终止
                    let err = StorageError::Io(format!(
                        "reader 提前 EOF：还差 {remaining} 字节（文件被并发截断？）"
                    ));
                    Some((Err(err), (f, 0)))
                }
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok(Bytes::from(buf)), (f, remaining - n as u64)))
                }
                Err(e) => Some((Err(map_io(e)), (f, 0))),
            }
        });
        Ok(Box::pin(frames))
    }

    async fn writer(
        &self,
        path: &RelPath,
        hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        if path.is_root() {
            return Err(StorageError::Invalid); // 根是目录，不可作为上传目标
        }
        // 组件硬化（含最终名与全部父目录段）
        let final_path = self.fs_path(path)?;
        if let Ok(meta) = tokio::fs::metadata(&final_path).await {
            if meta.is_dir() {
                return Err(StorageError::Invalid); // 目标是已存在目录（trait 契约）
            }
        }
        // 暂存目录就绪（幂等；保留名，list(根) 不可见）
        let staging_dir = self.root.join(STAGING_DIR_NAME);
        tokio::fs::create_dir_all(&staging_dir)
            .await
            .map_err(map_io)?;
        // 唯一临时名：pid + 进程级单调计数；撞名（复用 pid 的陈旧文件/
        // 同进程多实例）则递增重试
        let pid = std::process::id();
        let mut opened = None;
        for _ in 0..8 {
            let seq = STAGING_SEQ.fetch_add(1, Ordering::Relaxed);
            let tmp_path = staging_dir.join(format!("{pid}-{seq}.part"));
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)
                .await
            {
                Ok(file) => {
                    opened = Some((seq, file, tmp_path));
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(map_io(e)),
            }
        }
        let Some((seq, file, tmp_path)) = opened else {
            return Err(StorageError::Io(format!(
                "staging 临时文件命名冲突重试耗尽（pid={pid}）"
            )));
        };
        // stash 旧版本（overwrite 场景）：commit-on-close 要求 close 前对象
        // 不可见——旧对象也一并搬进暂存区；close 删 stash、abort/Drop 恢复
        //（放弃上传 = 回到 writer 打开前状态）。与 tmp 同 seq 配对命名。
        let old_path = match tokio::fs::metadata(&final_path).await {
            Ok(m) if m.is_file() => {
                let old = staging_dir.join(format!("{pid}-{seq}.old"));
                match tokio::fs::rename(&final_path, &old).await {
                    Ok(()) => Some(old),
                    // 竞态窗内旧对象消失：无 stash（同「原本不存在」）
                    Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                    Err(e) => {
                        // 收拾刚建的 tmp 再报错（不留孤儿）
                        let _ = tokio::fs::remove_file(&tmp_path).await;
                        return Err(map_io(e));
                    }
                }
            }
            _ => None, // 不存在（目录形态已在前面拒绝）
        };
        Ok(Box::new(LocalStager::new(
            self.volume.clone(),
            path.clone(),
            final_path,
            tmp_path,
            old_path,
            file,
            hint.size,
        )))
    }

    async fn quota(&self) -> Result<Quota, StorageError> {
        // 本地卷无配额概念：total=None（契约「未知/无上限」形态）；
        // used 恒 0——不递归统计卷占用（本地盘容量管理归 OS，驱动不
        // 镜像 du 语义；调用方先看 total 再用 available()）。
        Ok(Quota {
            total: None,
            used: 0,
        })
    }
}

/// 目录 + 组件名 → 子 RelPath；名字经与 [`LocalDriver::fs_path`] 相同的
/// 硬化——保证 list 产出的每条 Entry 都能被本驱动的 delete/reader 寻址
///（不可寻址的名字直接不可见）。
fn join_validated(dir: &RelPath, name: &str) -> Option<RelPath> {
    if has_reserved_char(name) {
        return None;
    }
    dir.join(name).ok() // join 自拒 "."/".."/空/含 '/'
}
