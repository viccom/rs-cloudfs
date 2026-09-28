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
//! - **stat/lstat 故障注入**（SF3 增补；M3 拆分为双旋钮）：错误映射
//!   经真实协议面回放（服务器 Status 码 → 客户端映射函数）。
//!   [`Stub::fail_next_lstat`] = conformance 断言⑤的注入面（driver.stat
//!   非根走 SSH_FXP_LSTAT，K67 语义）；[`Stub::fail_next_stat`] = stat
//!   跟随面（卷根腿/connect）的对称旋钮，两注入面互不越界。
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
    /// 一次性 stat 故障注入：下一次 `stat`（SSH_FXP_STAT，跟随面——
    /// driver.stat 卷根腿 / transport connect）直接以该状态码失败。
    /// 消费即清空。**只归 `stat`**——与 lstat 旋钮互不越界（M3）。
    fail_next_stat: Option<StatusCode>,
    /// 一次性 lstat 故障注入（SSH_FXP_LSTAT 面）：**conformance ⑤ 的
    /// 注入面**——driver.stat 的非根路径自 K67 起实现为
    /// symlink_metadata，注入必须打在真实动词上；M3 独立性测试同用。
    fail_next_lstat: Option<StatusCode>,
    /// 一次性 opendir 故障注入（复审 D）：**connect 可读性校验腿的
    /// 构造面**——场景 3（根存在、stat 通过、列目录被拒）。
    fail_next_opendir: Option<StatusCode>,
    /// 竞态注入（rename 撞车测试的注入面）：集合内的路径对 stat/
    /// lstat 请求隐身（NoSuchFile），在下一个 rename 请求到达时
    /// 现形——回放「驱动预检放行、执行撞已存在目标」的交错。
    hidden: BTreeSet<String>,
    /// 复审 M6 注入：armed 路径的下一次 rename **应用效果后回
    /// NoSuchFile**（lost-ACK 重放形态——首个 rename 已成功、ACK
    /// 丢失、重放请求如实撞「源不在」）。一次性。
    apply_rename_but_no_such_file: Option<String>,
    /// H1 注入面：下一次可写 close 落盘时，路径以前缀开头的句柄数据
    /// 截到 N 字节——回放「服务端短写」（close 确认正常但落盘尺寸与
    /// 客户端 written 不符，硬仗②校验的触发器）。
    shrink_next_close: Option<(String, u64)>,
    /// ① 注入面：下一次 write 动词直接失败（服务端写错误回放——
    /// transport 上传错误路径的裸 Drop 触发器）。
    fail_next_write: bool,
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
            fail_next_lstat: None,
            fail_next_opendir: None,
            hidden: BTreeSet::new(),
            apply_rename_but_no_such_file: None,
            shrink_next_close: None,
            fail_next_write: false,
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
        // 竞态注入：hidden 路径对 stat 隐身（NoSuchFile）
        if vfs.hidden.contains(&path) {
            return Err(StatusCode::NoSuchFile);
        }
        // SSH_FXP_STAT：**跟随**链接（OpenSSH 语义）
        let attrs = vfs.stat_following(&path).ok_or(StatusCode::NoSuchFile)?;
        Ok(Attrs { id, attrs })
    }

    /// SSH_FXP_LSTAT：**不跟随**链接（驱动的递归删除/目录性预检面；
    /// SF4 真机矩阵揭出的 GAP-A02 缺陷在桩侧的可回放形态）。
    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let mut vfs = lock_vfs!(self);
        // M3：lstat 消费自己的注入旋钮——与 stat 的 fail_next_stat 互
        // 不越界（注入面 = 文档声明）。
        if let Some(code) = vfs.fail_next_lstat.take() {
            return Err(code);
        }
        if vfs.hidden.contains(&path) {
            return Err(StatusCode::NoSuchFile);
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
        // ① 注入面：一次性写失败（消费即清）——先于句柄查找，回放
        // 「服务端在写路径上报错」形态
        if std::mem::take(&mut vfs.fail_next_write) {
            return Err(StatusCode::Failure);
        }
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
                let mut data = std::mem::take(&mut fh.buf);
                // H1 注入面：服务端短写回放（截到 N 字节再落盘——close
                // 确认正常，远端尺寸与客户端 written 不符）
                if let Some((prefix, size)) = vfs.shrink_next_close.take() {
                    if fh.path.starts_with(&prefix) {
                        data.truncate(size as usize);
                    }
                }
                let mtime = vfs.next_mtime();
                vfs.files.insert(fh.path.clone(), FileNode { data, mtime });
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
        // 复审 D 注入面：一次性 opendir 故障——connect 可读性校验腿
        //（场景 3）的构造面，消费即清。
        if let Some(code) = vfs.fail_next_opendir.take() {
            return Err(code);
        }
        // OpenSSH 语义：opendir 的路径解析**跟随**链接（symlink 根、
        // 或任何直指目录的链接都能打开）——「不下潜卷内链接」由驱动
        // 侧 lstat 预检保证（list(link) 在预检处 Invalid，桩对齐真机
        // 后该契约不弱化）。桩早期在这里拒链是比真机更严的第二道
        // 墙，但把 symlink 根也挡了——审查修复批放通。
        let path = vfs.resolve_link(&path).unwrap_or(path);
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
        // 竞态注入解除：hidden 路径在 rename 请求到达时现形（此后
        // 正常处理——已存在目标按 OpenSSH 形态回 Failure）
        vfs.hidden.clear();
        // 复审 M6 注入（lost-ACK 重放形态）：rename 效果已落地（首个
        // 请求成功、ACK 丢失），重放请求到达时源已不在——真实服务端
        // 此刻如实回 NoSuchFile。桩 = 应用效果（文件/链接单件搬移，
        // 目录腿不在本旋钮射程）+ 回 NoSuchFile——确定性模拟，无需
        // 连接手术。
        if let Some(armed) = vfs.apply_rename_but_no_such_file.clone() {
            if oldpath == armed {
                vfs.apply_rename_but_no_such_file = None;
                if let Some(node) = vfs.files.remove(&oldpath) {
                    vfs.files.insert(newpath.clone(), node);
                } else if let Some(target) = vfs.symlinks.remove(&oldpath) {
                    vfs.symlinks.insert(newpath.clone(), target);
                }
                return Err(StatusCode::NoSuchFile);
            }
        }
        let source_is_dir = vfs.dirs.contains(&oldpath);
        if !source_is_dir
            && !vfs.files.contains_key(&oldpath)
            && !vfs.symlinks.contains_key(&oldpath)
        {
            return Err(StatusCode::NoSuchFile);
        }
        if vfs.files.contains_key(&newpath)
            || vfs.dirs.contains(&newpath)
            || vfs.symlinks.contains_key(&newpath)
        {
            // OpenSSH：overwrite rename 恒拒（Failure 形态）。M4：链接
            // 也是既有目录项——此前漏查 symlinks 表会把文件写到与链
            // 同键的位置（files/symlinks 两表各存一份，状态错乱）。
            return Err(StatusCode::Failure);
        }
        let new_parent = VfsState::parent_of(&newpath).to_string();
        if !vfs.dirs.contains(&new_parent) {
            return Err(StatusCode::NoSuchFile);
        }
        if source_is_dir {
            // 子树整体迁移（路径键模型下重写前缀——目录、文件与链接
            // 都要搬；H2：真机 rename 是服务端原子迁移，链接此前漏搬
            // 让它留旧前缀——新路径不可见、删除删不到）
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
            let moved_links: Vec<String> = vfs
                .symlinks
                .range(prefix.clone()..)
                .take_while(|(p, _)| p.starts_with(&prefix))
                .map(|(p, _)| p.clone())
                .collect();
            for path in moved_links {
                let target = vfs.symlinks.remove(&path).expect("prefix-filtered");
                vfs.symlinks
                    .insert(format!("{newpath}/{}", &path[prefix.len()..]), target);
            }
        } else if let Some(target) = vfs.symlinks.remove(&oldpath) {
            // 链接本体的 rename（OpenSSH 语义：rename 搬链接自身，
            // 不解析目标）
            vfs.symlinks.insert(newpath, target);
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
            // ④：已被文件/链接占据的组件不再向 dirs 表登记（两表同键
            // = 桩状态污染）；目录目标不受影响（dirs 幂等）。
            if vfs.files.contains_key(&current) || vfs.symlinks.contains_key(&current) {
                continue;
            }
            vfs.dirs.insert(current.clone());
        }
        vfs.symlinks.insert(link.to_string(), target.to_string());
    }

    /// 预置一个**悬空**符号链接（目标不创建——解析必 NotFound；M4
    /// 测试面：「链接也是既有目录项」的拒绝形态）。
    pub fn add_dangling_symlink(&self, link: &str, target: &str) {
        let mut vfs = self.lock_vfs();
        let parent = VfsState::parent_of(link).to_string();
        assert!(
            vfs.dirs.contains(&parent),
            "stub: add_dangling_symlink parent must exist ({link})"
        );
        vfs.symlinks.insert(link.to_string(), target.to_string());
    }

    /// 读回链接目标（桩状态一致性断言面）。
    pub fn symlink_target(&self, link: &str) -> Option<String> {
        self.lock_vfs().symlinks.get(link).cloned()
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

    /// 注入一次 stat 故障：下一次到达服务端的 `stat`（SSH_FXP_STAT，
    /// 跟随面——driver.stat 卷根腿 / transport connect）直接以 `code`
    /// 失败，消费后恢复。**只归 stat**——lstat 走
    /// [`Self::fail_next_lstat`]（M3 拆分）。
    pub fn fail_next_stat(&self, code: StatusCode) {
        self.lock_vfs().fail_next_stat = Some(code);
    }

    /// lstat 故障注入（SSH_FXP_LSTAT 面）：**conformance 断言⑤的注入
    /// 面**——driver.stat 的非根路径自 K67 起实现为 `symlink_metadata`，
    /// 注入必须打在驱动实际发出的动词上。
    pub fn fail_next_lstat(&self, code: StatusCode) {
        self.lock_vfs().fail_next_lstat = Some(code);
    }

    /// opendir 故障注入（复审 D）：下一个 opendir 直接以 `code` 失败
    ///（消费即清）——connect 可读性校验腿的构造面（场景 3：stat 通过、
    /// 列目录被拒）。
    pub fn fail_next_opendir(&self, code: StatusCode) {
        self.lock_vfs().fail_next_opendir = Some(code);
    }

    /// H1 注入面：下一次可写 close 落盘时，句柄路径以 `path_prefix`
    /// 开头的把数据截到 `size` 字节——回放「服务端短写」（close 确认
    /// 正常但远端尺寸与客户端 written 不符，硬仗②校验的触发器）。
    pub fn shrink_next_close(&self, path_prefix: &str, size: u64) {
        self.lock_vfs().shrink_next_close = Some((path_prefix.to_string(), size));
    }

    /// ① 注入面：下一次 `write` 动词直接以 Failure 失败（消费即清）。
    pub fn fail_next_write(&self) {
        self.lock_vfs().fail_next_write = true;
    }

    /// 竞态注入（rename 撞车测试）：路径对 stat/lstat 隐身
    /// （NoSuchFile），在下一个 rename 请求到达时现形——回放「驱动
    /// 预检放行、执行撞已存在目标」的交错。
    pub fn hide_until_next_rename(&self, path: &str) {
        self.lock_vfs().hidden.insert(path.to_string());
    }

    /// 复审 M6 注入（lost-ACK 重放形态）：armed 路径的下一次 rename
    /// 应用效果后回 NoSuchFile（见 [`VfsState::apply_rename_but_no_such_file`]）。
    pub fn apply_rename_but_reply_no_such_file(&self, path: &str) {
        self.lock_vfs().apply_rename_but_no_such_file = Some(path.to_string());
    }

    /// 模拟「rename 已在服务端执行但 ACK 丢失」（close 重放窗测试）：
    /// 把 final 同目录的 `.cksftp-*….part` 暂存件直接搬到 final
    /// （服务端面动作，不经客户端协议）。桩是 commit-on-close 模型，
    /// 在写的 part 句柄尚不在 `files` 里——模拟 = 把句柄缓冲以新名
    /// 落盘并把句柄改指 final（POSIX：rename 打开中的文件句柄保持
    /// 有效；客户端随后的 awaited close 落在同一处，无害）。russh-sftp
    /// 的写是 nowait 入队（服务端异步消费），故先等在途写落服（≤1s
    /// 预算）。返回是否找到了已收到数据的暂存句柄。
    pub async fn simulate_lost_ack_rename(&self, final_path: &str) -> bool {
        let prefix = format!("{final_path}.cksftp-");
        for _ in 0..500 {
            let found = {
                let vfs = self.lock_vfs();
                vfs.file_handles
                    .iter()
                    .find(|(_h, fh)| {
                        fh.path.starts_with(&prefix)
                            && fh.path.ends_with(".part")
                            && !fh.buf.is_empty()
                    })
                    .map(|(h, _)| h.clone())
            };
            if let Some(handle_key) = found {
                let mut vfs = self.lock_vfs();
                let (data,) = {
                    let fh = vfs.file_handles.get_mut(&handle_key).expect("just found");
                    let data = fh.buf.clone();
                    fh.path = final_path.to_string();
                    (data,)
                };
                let mtime = vfs.next_mtime();
                vfs.files
                    .insert(final_path.to_string(), FileNode { data, mtime });
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        false
    }

    /// 当前 VFS 里的驱动暂存残件清单（`.cksftp-` 前缀 + `.part`/`.old`
    /// 结尾——与驱动 `is_staging_artifact` 同判定）。
    pub fn staging_artifacts(&self) -> Vec<String> {
        self.lock_vfs()
            .files
            .keys()
            .filter(|p| p.contains(".cksftp-") && (p.ends_with(".part") || p.ends_with(".old")))
            .cloned()
            .collect()
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
