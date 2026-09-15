//! 进程内 SFTP 测试桩（Phase 4 / SF2）——hermetic，无真实网络。
//!
//! 形态：`russh::server`（Ed25519 随机 host key + 可配置用户名/密码/
//! 公钥校验 + "sftp" subsystem）+ `russh_sftp::server::run` 之上的内存
//! VFS。每测试一个桩实例，bind `127.0.0.1:0` 随机端口；不依赖
//! `$HOME`/Docker/known_hosts（信任策略在客户端 handler 注入——计划
//! 附录 C.2 的 Windows 可行性结论）。
//!
//! ## 与 OpenSSH 真实语义的对齐声明（桩侧必守）
//!
//! - **readdir EOF 二轮语义**：`readdir` 每轮返回至多 2 条（多轮翻页），
//!   取尽后返回 `Err(StatusCode::Eof)`——客户端 `read_dir` 循环到 EOF
//!   才终止，桩若一直返回条目会静默挂死（计划附录 C.2 实测坑，20s
//!   超时形态）。空目录首轮即 EOF。
//! - **rename 目标已存在 → `Failure`**：OpenSSH 对 overwrite rename 返回
//!   `SSH_FX_FAILURE`（部分实现覆盖——驱动侧 stat 预检归一为 Exists，
//!   桩按 OpenSSH 形态回放）。rename 源缺失 → `NoSuchFile`；目标父目录
//!   缺失 → `NoSuchFile`。目录 rename 整棵子树随路径键迁移。
//! - **句柄计数**：文件句柄 open 计数 / close 递减（aeroftp 教训的
//!   服务端断言面——「每次 open 必 awaited close」由
//!   [`Stub::open_handle_count`] 观测）。写句柄 close 时才提交缓冲到
//!   VFS（真实服务器的 commit-on-close 形态）。
//! - **read 越界**：offset >= 句柄长度 → `Err(Eof)`（SFTP v3 规范；
//!   客户端 `check_read_result` 将其归一为优雅 EOF）；窗口内 → 精确
//!   窗口切片（offset 生效 + 只取窗口，Range 语义的桩侧依据）。
//! - **mkdir/rmdir/remove**：目标存在 → `Failure`；父目录缺失 →
//!   `NoSuchFile`；rmdir 非空/根 → `Failure`；remove 作用于目录 →
//!   `Failure`（OpenSSH 均为此形态）。
//! - **readdir 含 `.`/`..`**：真实服务器目录流包含两者（驱动 list 侧
//!   过滤——桩回放真实形态以覆盖该过滤路径）。
//! - **init 无扩展声明**：不声明 statvfs/fsync/limits@openssh.com →
//!   客户端 `fs_info` 直接 `Ok(None)`（quota total=None 降级腿）、
//!   `sync_all` 短路（桩的 unimplemented 腿，服务端零回放）。
//! - **host key**：服务端起好后经 [`Stub::fingerprint`] 暴露
//!   `SHA256:...`（`ssh_key::PublicKey::fingerprint(HashAlg::Sha256)` 的
//!   Display 形态，与 client.rs `check_server_key` 逐字同源）。
//! - **stat 故障注入**（SF3 增补，conformance 断言⑤）：
//!   [`Stub::fail_next_stat`] 让下一次 stat 以给定状态码失败——错误
//!   映射经真实协议面回放（服务器 Status 码 → 客户端映射函数）。
//!
//! SF3 的 conformance 复用本桩（`mod stub;` 从任意集成测试引入）。

// 共享测试支撑模块：不同集成测试（connect_auth / read_path / write_path，
// SF3 起还有 conformance）各自只消费本桩 API 的一个子集——dead_code 按
// 测试目标逐个编译判定，共享面必然在部分目标里「未用」。整体豁免。
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use russh::keys::ssh_key::PublicKey;
use russh::keys::{Algorithm, HashAlg, PrivateKey};
use russh::server::Server as _;
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};

/// readdir 每轮返回的条目数上限（多轮翻页 + EOF 二轮语义的复合回放）。
const READDIR_BATCH: usize = 2;

/// 确定性 mtime 基准（epoch 秒；每次文件提交 +1——stat 断言可预期）。
const MTIME_BASE: u32 = 1_700_000_000;

/// 常规文件权限位（0o100644：type=REG + rw-r--r--）。
const PERM_FILE: u32 = 0o100_644;
/// 目录权限位（0o040755：type=DIR + rwxr-xr-x）。
const PERM_DIR: u32 = 0o040_755;

/// 符号链接的权限位（S_IFLNK——POSIX lstat 的 type 面）。
const PERM_LINK: u32 = 0o120_777;

/// VFS 内部锁的统一入口（毒锁恢复比卡死正确——桩不区分毒化场景）。
macro_rules! lock_vfs {
    ($self:ident) => {
        $self
            .vfs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
}

// ------------------------------------------------------------ VFS 模型 ---

struct FileNode {
    data: Vec<u8>,
    mtime: u32,
}

struct FileHandle {
    path: String,
    writable: bool,
    /// 打开时快照的缓冲（写句柄 close 时提交回 VFS——commit-on-close）。
    buf: Vec<u8>,
}

struct DirHandle {
    /// opendir 时刻的子条目快照（name + attrs；readdir 期间不受并发
    /// 变更影响——真实服务器同语义）。
    entries: Vec<(String, FileAttributes)>,
    pos: usize,
}

struct VfsState {
    dirs: BTreeSet<String>,
    files: BTreeMap<String, FileNode>,
    /// 符号链接（aeroftp 教训 8 的语句面；SF4 真机矩阵揭出的缺陷——
    /// 驱动对 link-to-dir 不得下潜——在桩侧可离线回放）：link path →
    /// 目标路径（绝对或相对，解析同 POSIX：相对目标相对链接的父目录）。
    symlinks: BTreeMap<String, String>,
    file_handles: HashMap<String, FileHandle>,
    dir_handles: HashMap<String, DirHandle>,
    next_handle: u64,
    mtime_tick: u32,
    /// 一次性 stat 故障注入（conformance 断言⑤的注入面）：下一次
    /// `stat` 请求直接以该状态码失败——驱动的错误映射经真实协议
    /// 回放（服务器回 Status 码 → 客户端 map_status）。消费即清空。
    fail_next_stat: Option<StatusCode>,
}

impl VfsState {
    fn new() -> Self {
        VfsState {
            dirs: BTreeSet::from(["/".to_string()]),
            files: BTreeMap::new(),
            symlinks: BTreeMap::new(),
            file_handles: HashMap::new(),
            dir_handles: HashMap::new(),
            next_handle: 0,
            mtime_tick: 0,
            fail_next_stat: None,
        }
    }

    /// 链接目标解析（POSIX 语义：绝对目标原样、相对目标相对链接父目录；
    /// 只解析一层——测试夹具不需要链接链）。
    fn resolve_link(&self, link: &str) -> Option<String> {
        let target = self.symlinks.get(link)?;
        if target.starts_with('/') {
            Some(target.clone())
        } else {
            let parent = Self::parent_of(link);
            let joined = if parent == "/" {
                format!("/{target}")
            } else {
                format!("{parent}/{target}")
            };
            Some(joined)
        }
    }

    /// 链接本体的 attrs（POSIX lstat：file type = link，size = 目标串长；
    /// OpenSSH 的 readdir/stat 对链接报的也是本体形态）。
    fn link_attrs(&self, link: &str) -> FileAttributes {
        FileAttributes {
            size: Some(self.symlinks.get(link).map(|t| t.len()).unwrap_or(0) as u64),
            permissions: Some(PERM_LINK),
            mtime: Some(MTIME_BASE),
            ..FileAttributes::default()
        }
    }

    /// lstat 语义查找（**不跟随**链接）。
    fn lstat_of(&self, path: &str) -> Option<FileAttributes> {
        if self.symlinks.contains_key(path) {
            return Some(self.link_attrs(path));
        }
        self.attr_of(path)
    }

    /// stat 语义查找（**跟随**链接；一层解析后走真实查找）。
    fn stat_following(&self, path: &str) -> Option<FileAttributes> {
        if let Some(resolved) = self.resolve_link(path) {
            return self.lstat_of(&resolved);
        }
        self.attr_of(path)
    }

    fn next_mtime(&mut self) -> u32 {
        self.mtime_tick += 1;
        MTIME_BASE + self.mtime_tick
    }

    fn file_attrs(&self, node: &FileNode) -> FileAttributes {
        FileAttributes {
            size: Some(node.data.len() as u64),
            permissions: Some(PERM_FILE),
            mtime: Some(node.mtime),
            ..FileAttributes::default()
        }
    }

    fn dir_attrs(&self) -> FileAttributes {
        FileAttributes {
            size: Some(0),
            permissions: Some(PERM_DIR),
            mtime: Some(MTIME_BASE),
            ..FileAttributes::default()
        }
    }

    fn attr_of(&self, path: &str) -> Option<FileAttributes> {
        if path == "/" {
            return Some(self.dir_attrs());
        }
        if let Some(node) = self.files.get(path) {
            return Some(self.file_attrs(node));
        }
        if self.dirs.contains(path) {
            return Some(self.dir_attrs());
        }
        None
    }

    /// 父目录路径（"/a/b" → "/a"；"/a" → "/"）。
    fn parent_of(path: &str) -> &str {
        match path.rfind('/') {
            Some(0) => "/",
            Some(i) => &path[..i],
            None => "/",
        }
    }

    /// 末段名字（"/a/b" → "b"）。
    fn name_of(path: &str) -> &str {
        path.rsplit('/').next().unwrap_or(path)
    }

    /// 直接子条目（名字字典序；**含 `.`/`..`**——真实 readdir 形态，
    /// 驱动 list 的过滤路径由此覆盖）。
    fn children_of(&self, dir: &str) -> Vec<(String, FileAttributes)> {
        let prefix = if dir == "/" {
            "/".to_string()
        } else {
            format!("{dir}/")
        };
        let mut out: Vec<(String, FileAttributes)> = vec![
            (".".to_string(), self.dir_attrs()),
            ("..".to_string(), self.dir_attrs()),
        ];
        for (path, node) in &self.files {
            if path.starts_with(&prefix) && !path[prefix.len()..].contains('/') {
                out.push((Self::name_of(path).to_string(), self.file_attrs(node)));
            }
        }
        for path in &self.dirs {
            if path != "/" && path.starts_with(&prefix) && !path[prefix.len()..].contains('/') {
                out.push((Self::name_of(path).to_string(), self.dir_attrs()));
            }
        }
        // 链接条目按**本体**形态列出（OpenSSH 的 readdir 报 lstat 观感：
        // link-to-dir 呈现为链接自身，不是目录——aeroftp 教训 7/8 的
        // 服务端侧对齐）
        for path in self.symlinks.keys() {
            if path.starts_with(&prefix) && !path[prefix.len()..].contains('/') {
                out.push((Self::name_of(path).to_string(), self.link_attrs(path)));
            }
        }
        out
    }
}

// ------------------------------------------------------- 服务端可观测面 ---

/// 服务端计数器（测试读取接口；aeroftp 教训的断言面）。
#[derive(Default)]
struct Counters {
    /// 活文件句柄数（open 计数 / close 递减；目录句柄不计数——
    /// read_dir 客户端侧同步 close，非泄漏面）。
    open_file_handles: AtomicUsize,
    /// 已接受的 SSH 连接数（D3 单连接观测 + 重连腿观测）。
    connections: AtomicUsize,
    /// 认证成功次数。
    auth_successes: AtomicUsize,
}

/// 桩认证配置（D1 回放面：用户名 + 密码 / 公钥至少其一）。
#[derive(Clone)]
pub struct StubAuth {
    pub username: String,
    pub password: Option<String>,
    /// 授权公钥的 SHA256 指纹（字符串比较——确定性且免 PublicKey
    /// 相等性依赖）。
    pub authorized_key_fingerprint: Option<String>,
}

impl StubAuth {
    /// 密码形态（最常用）。
    pub fn password(username: &str, password: &str) -> Self {
        StubAuth {
            username: username.to_string(),
            password: Some(password.to_string()),
            authorized_key_fingerprint: None,
        }
    }

    /// 公钥形态（追加授权指纹；密码可同置——驱动私钥优先、密码兜底）。
    pub fn with_public_key(mut self, key: &PublicKey) -> Self {
        self.authorized_key_fingerprint = Some(format!("{}", key.fingerprint(HashAlg::Sha256)));
        self
    }

    fn check_password(&self, user: &str, password: &str) -> bool {
        user == self.username && self.password.as_deref() == Some(password)
    }

    fn check_public_key(&self, user: &str, key: &PublicKey) -> bool {
        user == self.username
            && self.authorized_key_fingerprint.as_deref()
                == Some(&format!("{}", key.fingerprint(HashAlg::Sha256)))
    }
}

// ------------------------------------------------------- SSH 服务端层 ---

/// russh server::Server 工厂（每连接一个 SshSession handler）。
struct SshServer {
    auth: StubAuth,
    vfs: Arc<Mutex<VfsState>>,
    counters: Arc<Counters>,
}

struct SshSession {
    auth: StubAuth,
    vfs: Arc<Mutex<VfsState>>,
    counters: Arc<Counters>,
    channels: tokio::sync::Mutex<HashMap<russh::ChannelId, russh::Channel<russh::server::Msg>>>,
}

impl russh::server::Server for SshServer {
    type Handler = SshSession;

    fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> Self::Handler {
        SshSession {
            auth: self.auth.clone(),
            vfs: self.vfs.clone(),
            counters: self.counters.clone(),
            channels: tokio::sync::Mutex::new(HashMap::new()),
        }
    }
}

impl russh::server::Handler for SshSession {
    type Error = StubError;

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<russh::server::Auth, Self::Error> {
        if self.auth.check_password(user, password) {
            self.counters.auth_successes.fetch_add(1, Ordering::SeqCst);
            Ok(russh::server::Auth::Accept)
        } else {
            Ok(russh::server::Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<russh::server::Auth, Self::Error> {
        if self.auth.check_public_key(user, key) {
            self.counters.auth_successes.fetch_add(1, Ordering::SeqCst);
            Ok(russh::server::Auth::Accept)
        } else {
            Ok(russh::server::Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: russh::Channel<russh::server::Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        id: russh::ChannelId,
        name: &str,
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        if name == "sftp" {
            let channel = self
                .channels
                .lock()
                .await
                .remove(&id)
                .expect("subsystem on an open channel");
            session.channel_success(id)?;
            russh_sftp::server::run(
                channel.into_stream(),
                SftpHandler {
                    vfs: self.vfs.clone(),
                    counters: self.counters.clone(),
                },
            )
            .await;
        } else {
            session.channel_failure(id)?;
        }
        Ok(())
    }
}

/// 极薄错误类型：russh server Handler::Error 只需 Error + Send + Sync
///（避免为 dev-only 桩引入 anyhow 依赖）。
#[derive(Debug)]
struct StubError(String);

impl std::error::Error for StubError {}

impl std::fmt::Display for StubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<russh::Error> for StubError {
    fn from(error: russh::Error) -> Self {
        StubError(error.to_string())
    }
}

// ------------------------------------------------------- SFTP 协议层 ---

struct SftpHandler {
    vfs: Arc<Mutex<VfsState>>,
    counters: Arc<Counters>,
}

fn ok_status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_string(),
        language_tag: "en-US".to_string(),
    }
}

impl russh_sftp::server::Handler for SftpHandler {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        // 未覆盖面（lstat/setstat/symlink/extended 等）统一 OpUnsupported
        //——OpenSSH 对未识别扩展同样回 OpUnsupported。
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        // 无扩展声明：statvfs/fsync/limits 全关（客户端 fs_info → None、
        // sync_all 短路——quota 降级腿的桩侧来源）。
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let resolved = if path == "." || path.is_empty() {
            "/".to_string()
        } else {
            path
        };
        Ok(Name {
            id,
            files: vec![File::dummy(&resolved)],
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let mut vfs = lock_vfs!(self);
        // 断言⑤注入优先于真实查找（注入的码恰好是 NoSuchFile 时也不
        // 能被真实查找路径"顺便"满足——回放的是驱动映射，不是 VFS 态）
        if let Some(code) = vfs.fail_next_stat.take() {
            return Err(code);
        }
        // SSH_FXP_STAT：**跟随**链接（OpenSSH 语义）
        let attrs = vfs.stat_following(&path).ok_or(StatusCode::NoSuchFile)?;
        Ok(Attrs { id, attrs })
    }

    /// SSH_FXP_LSTAT：**不跟随**链接（驱动的递归删除/目录性预检面；
    /// SF4 真机矩阵揭出的 GAP-A02 缺陷在桩侧的可回放形态）。
    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let mut vfs = lock_vfs!(self);
        if let Some(code) = vfs.fail_next_stat.take() {
            return Err(code);
        }
        let attrs = vfs.lstat_of(&path).ok_or(StatusCode::NoSuchFile)?;
        Ok(Attrs { id, attrs })
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let mut vfs = lock_vfs!(self);
        // SSH_FXP_OPEN 跟随链接（OpenSSH 语义：读链即读目标）——
        // 写入也必须落到**目标**而非替换链接本体。
        let filename = vfs.resolve_link(&filename).unwrap_or(filename);
        if vfs.dirs.contains(&filename) {
            // 打开目录当文件 → Failure（OpenSSH 形态）
            return Err(StatusCode::Failure);
        }
        let existing = vfs.files.get(&filename).map(|n| n.data.clone());
        let buf = match (&existing, pflags.contains(OpenFlags::TRUNCATE)) {
            (Some(_), true) => Vec::new(),
            (Some(data), false) => data.clone(),
            (None, _) => {
                if !pflags.contains(OpenFlags::CREATE) {
                    return Err(StatusCode::NoSuchFile);
                }
                // CREATE 隐含父目录必须存在（OpenSSH：缺父 → NoSuchFile）
                let parent = VfsState::parent_of(&filename).to_string();
                if !vfs.dirs.contains(&parent) {
                    return Err(StatusCode::NoSuchFile);
                }
                Vec::new()
            }
        };
        vfs.next_handle += 1;
        let handle = format!("f{}", vfs.next_handle);
        vfs.file_handles.insert(
            handle.clone(),
            FileHandle {
                path: filename,
                writable: pflags.contains(OpenFlags::WRITE),
                buf,
            },
        );
        self.counters
            .open_file_handles
            .fetch_add(1, Ordering::SeqCst);
        Ok(Handle { id, handle })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let vfs = lock_vfs!(self);
        let fh = vfs.file_handles.get(&handle).ok_or(StatusCode::Failure)?;
        let start = offset as usize;
        if start >= fh.buf.len() {
            // SFTP v3：offset 越过 EOF → SSH_FX_EOF（客户端归一为优雅
            // EOF——check_read_result 的 Err(Status{Eof}) 腿）
            return Err(StatusCode::Eof);
        }
        let end = (start + len as usize).min(fh.buf.len());
        Ok(Data {
            id,
            data: fh.buf[start..end].to_vec(),
        })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let mut vfs = lock_vfs!(self);
        let fh = vfs
            .file_handles
            .get_mut(&handle)
            .ok_or(StatusCode::Failure)?;
        if !fh.writable {
            return Err(StatusCode::Failure);
        }
        let start = offset as usize;
        let end = start + data.len();
        if end > fh.buf.len() {
            fh.buf.resize(end, 0);
        }
        fh.buf[start..end].copy_from_slice(&data);
        Ok(ok_status(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        let mut vfs = lock_vfs!(self);
        if let Some(mut fh) = vfs.file_handles.remove(&handle) {
            if fh.writable {
                // commit-on-close：写缓冲此刻落 VFS
                let mtime = vfs.next_mtime();
                vfs.files.insert(
                    fh.path.clone(),
                    FileNode {
                        data: std::mem::take(&mut fh.buf),
                        mtime,
                    },
                );
            }
            self.counters
                .open_file_handles
                .fetch_sub(1, Ordering::SeqCst);
            return Ok(ok_status(id));
        }
        if vfs.dir_handles.remove(&handle).is_some() {
            return Ok(ok_status(id));
        }
        Err(StatusCode::Failure)
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let mut vfs = lock_vfs!(self);
        // 链接本体不是目录：link-to-dir 不可 opendir（OpenSSH 对链接
        // 目标为目录的 opendir 会跟随成功，但**驱动不应发出该请求**
        // ——桩按 lstat 形态拒绝，使「驱动是否下潜」在桩侧可观测）
        if vfs.symlinks.contains_key(&path) {
            return Err(StatusCode::Failure);
        }
        if !vfs.dirs.contains(&path) {
            return Err(StatusCode::NoSuchFile);
        }
        let entries = vfs.children_of(&path);
        vfs.next_handle += 1;
        let handle = format!("d{}", vfs.next_handle);
        vfs.dir_handles
            .insert(handle.clone(), DirHandle { entries, pos: 0 });
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let mut vfs = lock_vfs!(self);
        let dh = vfs
            .dir_handles
            .get_mut(&handle)
            .ok_or(StatusCode::Failure)?;
        if dh.pos >= dh.entries.len() {
            // EOF 二轮语义：取尽后必须回 Eof（附录 C.2 实测坑）
            return Err(StatusCode::Eof);
        }
        let take = READDIR_BATCH.min(dh.entries.len() - dh.pos);
        let batch: Vec<File> = dh.entries[dh.pos..dh.pos + take]
            .iter()
            .map(|(name, attrs)| File::new(name.clone(), attrs.clone()))
            .collect();
        dh.pos += take;
        Ok(Name { id, files: batch })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let mut vfs = lock_vfs!(self);
        if vfs.files.remove(&filename).is_some() {
            return Ok(ok_status(id));
        }
        // 链接本体：remove/unlink 删链**本身**，绝不动目标
        // （OpenSSH 语义；递归删除的「只删链不下潜」依赖这一条）
        if vfs.symlinks.remove(&filename).is_some() {
            return Ok(ok_status(id));
        }
        if vfs.dirs.contains(&filename) {
            // REMOVE 作用于目录 → Failure（OpenSSH 形态）
            return Err(StatusCode::Failure);
        }
        Err(StatusCode::NoSuchFile)
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let mut vfs = lock_vfs!(self);
        if vfs.files.contains_key(&path) || vfs.dirs.contains(&path) {
            // 已存在（文件或目录）→ Failure（OpenSSH 形态；驱动的 Exists
            // 来自 stat 预检，竞态臂同样命中 Failure）
            return Err(StatusCode::Failure);
        }
        let parent = VfsState::parent_of(&path).to_string();
        if !vfs.dirs.contains(&parent) {
            return Err(StatusCode::NoSuchFile);
        }
        vfs.dirs.insert(path);
        Ok(ok_status(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let mut vfs = lock_vfs!(self);
        if !vfs.dirs.contains(&path) {
            return Err(StatusCode::NoSuchFile);
        }
        if path != "/" {
            // children_of 含 "."/".."：非空判定须排除二者
            let real_children = vfs
                .children_of(&path)
                .into_iter()
                .filter(|(name, _)| name != "." && name != "..")
                .count();
            if real_children == 0 {
                vfs.dirs.remove(&path);
                return Ok(ok_status(id));
            }
        }
        // 根不可删 / 非空 → Failure（OpenSSH 形态）
        Err(StatusCode::Failure)
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let mut vfs = lock_vfs!(self);
        let source_is_dir = vfs.dirs.contains(&oldpath);
        if !source_is_dir && !vfs.files.contains_key(&oldpath) {
            return Err(StatusCode::NoSuchFile);
        }
        if vfs.files.contains_key(&newpath) || vfs.dirs.contains(&newpath) {
            // OpenSSH：overwrite rename 恒拒（Failure 形态）
            return Err(StatusCode::Failure);
        }
        let new_parent = VfsState::parent_of(&newpath).to_string();
        if !vfs.dirs.contains(&new_parent) {
            return Err(StatusCode::NoSuchFile);
        }
        if source_is_dir {
            // 子树整体迁移（路径键模型下重写前缀——目录与文件都要搬）
            let prefix = format!("{oldpath}/");
            let moved_dirs: Vec<String> = vfs
                .dirs
                .iter()
                .filter(|p| p.as_str() == oldpath || p.starts_with(&prefix))
                .cloned()
                .collect();
            for path in moved_dirs {
                let renamed = if path == oldpath {
                    newpath.clone()
                } else {
                    format!("{newpath}/{}", &path[prefix.len()..])
                };
                vfs.dirs.insert(renamed);
                vfs.dirs.remove(&path);
            }
            let moved_files: Vec<String> = vfs
                .files
                .keys()
                .filter(|p| p.starts_with(&prefix))
                .cloned()
                .collect();
            for path in moved_files {
                let node = vfs.files.remove(&path).expect("prefix-filtered");
                vfs.files
                    .insert(format!("{newpath}/{}", &path[prefix.len()..]), node);
            }
        } else {
            let node = vfs.files.remove(&oldpath).expect("checked above");
            vfs.files.insert(newpath, node);
        }
        Ok(ok_status(id))
    }
}

// ------------------------------------------------------------ 桩句柄 ---

/// 运行中的桩实例（测试面）。
pub struct Stub {
    port: u16,
    fingerprint: String,
    auth: StubAuth,
    vfs: Arc<Mutex<VfsState>>,
    counters: Arc<Counters>,
    /// 每连接的 kill 开关（watch sender → 连接任务发 SSH_MSG_DISCONNECT）。
    kill_switches: Arc<Mutex<Vec<tokio::sync::watch::Sender<bool>>>>,
    /// accept 循环任务（桩 Drop 时随测试结束自然回收）。
    _listener_task: tokio::task::JoinHandle<()>,
}

impl Stub {
    /// 起一个桩：随机 Ed25519 host key + 随机回环端口。
    pub async fn start(auth: StubAuth) -> Stub {
        // 服务端 host key：Ed25519 随机生成；指纹与 client.rs
        // check_server_key 的 Display 形态逐字同源。
        let key =
            PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("random ed25519 key");
        let fingerprint = format!("{}", key.public_key().fingerprint(HashAlg::Sha256));
        let ssh_config = Arc::new(russh::server::Config {
            keys: vec![key],
            ..russh::server::Config::default()
        });

        let vfs = Arc::new(Mutex::new(VfsState::new()));
        let counters = Arc::new(Counters::default());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind loopback ephemeral port");
        let port = listener.local_addr().expect("local addr").port();

        let kill_switches: Arc<Mutex<Vec<tokio::sync::watch::Sender<bool>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let mut server = SshServer {
            auth: auth.clone(),
            vfs: vfs.clone(),
            counters: counters.clone(),
        };
        let accept_counters = counters.clone();
        let switches = kill_switches.clone();
        let listener_task = tokio::spawn(async move {
            loop {
                let Ok((stream, peer)) = listener.accept().await else {
                    return;
                };
                accept_counters.connections.fetch_add(1, Ordering::SeqCst);
                let handler = server.new_client(Some(peer));
                let config = ssh_config.clone();
                let (kill_tx, mut kill_rx) = tokio::sync::watch::channel(false);
                // run_stream 内部自起 runtime 任务持有 TcpStream——外部
                // abort 包装任务杀不掉连接（实测）。协议级断开才是真杀：
                // 持有 RunningSession::handle，收到 kill 信号即发
                // SSH_MSG_DISCONNECT。
                tokio::spawn(async move {
                    let Ok(mut running) = russh::server::run_stream(config, stream, handler).await
                    else {
                        return;
                    };
                    let handle = running.handle();
                    tokio::select! {
                        changed = kill_rx.changed() => {
                            if changed.is_ok() && *kill_rx.borrow() {
                                let _ = handle
                                    .disconnect(
                                        russh::Disconnect::ByApplication,
                                        "stub connection killed".to_string(),
                                        "en".to_string(),
                                    )
                                    .await;
                            }
                        }
                        _ = &mut running => {}
                    }
                });
                switches
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(kill_tx);
            }
        });

        Stub {
            port,
            fingerprint,
            auth,
            vfs,
            counters,
            kill_switches,
            _listener_task: listener_task,
        }
    }

    /// 便捷构造参数对（SftpParams::from_pairs 的原料；含密码形态的
    /// 凭据与端口/用户名）。
    pub fn param_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("sftp_host".to_string(), "127.0.0.1".to_string()),
            ("sftp_port".to_string(), self.port.to_string()),
            ("sftp_username".to_string(), self.auth.username.clone()),
        ];
        if let Some(password) = &self.auth.password {
            pairs.push(("sftp_password".to_string(), password.clone()));
        }
        pairs
    }

    /// 服务器 host key 指纹（喂给 SftpParams.host_fingerprint——D2 面）。
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// 回环地址对（驱动连接目标）。
    pub fn addr(&self) -> (&'static str, u16) {
        ("127.0.0.1", self.port)
    }

    // ------------------------------------------------ VFS 观测/装配 ---

    /// 预置一个文件（父目录须已存在；返回确定性 mtime 供断言）。
    pub fn add_file(&self, path: &str, data: &[u8]) -> u32 {
        let mut vfs = self.lock_vfs();
        let parent = VfsState::parent_of(path).to_string();
        assert!(
            vfs.dirs.contains(&parent),
            "stub: add_file parent must exist ({path})"
        );
        let mtime = vfs.next_mtime();
        vfs.files.insert(
            path.to_string(),
            FileNode {
                data: data.to_vec(),
                mtime,
            },
        );
        mtime
    }

    /// 预置一个目录（含父链）。
    pub fn add_dir(&self, path: &str) {
        let mut vfs = self.lock_vfs();
        let mut current = String::new();
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            current.push('/');
            current.push_str(comp);
            vfs.dirs.insert(current.clone());
        }
    }

    /// 预置一个符号链接（link → target；target 绝对或相对链接父目录）——
    /// 符号链接契约的离线回放面（SF4 真机矩阵揭出的缺陷形态）。
    pub fn add_symlink(&self, link: &str, target: &str) {
        let mut vfs = self.lock_vfs();
        let parent = VfsState::parent_of(link).to_string();
        assert!(
            vfs.dirs.contains(&parent),
            "stub: add_symlink parent must exist ({link})"
        );
        let mut current = String::new();
        for comp in target.split('/').filter(|c| !c.is_empty()) {
            current.push('/');
            current.push_str(comp);
            vfs.dirs.insert(current.clone());
        }
        vfs.symlinks.insert(link.to_string(), target.to_string());
    }

    /// 读回文件字节（上传往返断言）。
    pub fn file_bytes(&self, path: &str) -> Option<Vec<u8>> {
        self.lock_vfs().files.get(path).map(|n| n.data.clone())
    }

    /// 目录探测（递归删除/迁移断言）。
    pub fn is_dir(&self, path: &str) -> bool {
        self.lock_vfs().dirs.contains(path)
    }

    /// 文件探测。
    pub fn file_exists(&self, path: &str) -> bool {
        self.lock_vfs().files.contains_key(path)
    }

    /// 活文件句柄数（三硬仗① 的服务端观测）。
    pub fn open_handle_count(&self) -> usize {
        self.counters.open_file_handles.load(Ordering::SeqCst)
    }

    /// 已接受的 SSH 连接数（D3 单连接 / 重连腿观测）。
    pub fn connection_count(&self) -> usize {
        self.counters.connections.load(Ordering::SeqCst)
    }

    /// 认证成功次数。
    pub fn auth_success_count(&self) -> usize {
        self.counters.auth_successes.load(Ordering::SeqCst)
    }

    /// 注入一次 stat 故障（conformance 断言⑤）：下一次到达服务端的
    /// `stat`/`lstat` 直接以 `code` 失败，消费后恢复。
    pub fn fail_next_stat(&self, code: StatusCode) {
        self.lock_vfs().fail_next_stat = Some(code);
    }

    /// 杀掉当前所有连接（协议级 SSH_MSG_DISCONNECT → 客户端传输层死）
    /// ——with_retry 重连腿的断连注入。监听器仍在，后续重连可接受。
    pub fn kill_connections(&self) {
        let switches = self
            .kill_switches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for switch in switches.iter() {
            let _ = switch.send(true);
        }
    }
}

/// 桩观测面的锁便捷方法（与 SftpHandler 的 lock_vfs! 同语义）。
impl Stub {
    fn lock_vfs(&self) -> std::sync::MutexGuard<'_, VfsState> {
        self.vfs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
