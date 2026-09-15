//! russh 会话建立 / 认证 / host key 校验 / 重连骨架（Phase 4 / SF1）。
//!
//! D1（认证 = 密码 + 私钥含 passphrase）、D2（host key = 显式接受 +
//! 指纹落盘，变更恒拒）、D3（起步单连接、惰性建立）三个 SF0 拍板
//! 在本模块落地。**网络路径的行为测试在 SF2 桩批补**（SF1 以编译 +
//! clippy 为门）——每个网络函数的文档注明对应桩验收点。
//!
//! ## 连接生命周期（D3 单连接）
//!
//! [`SftpClient`] 持有 `Mutex<Option<SshConnection>>`：首次用时惰性
//! 建立（工厂不连接）；操作以 `Unavailable` 形态失败 → 清空槽位、
//! 重试一次（[`with_retry`] 宏，重连骨架）。同一时刻至多一条 SSH
//! 连接；元数据操作在锁内 await（tokio Mutex 允许跨 await），文件
//! 句柄（russh-sftp `File` 自持 `Arc<RawSftpSession>`）从锁内取出后
//! 在锁外流式读写——会话层自身支持并发请求（russh-sftp 3.0 的
//! `max_concurrent_reads` 流水线），锁只守护连接的建立/拆除。
//!
//! 领域方法一律两段式：`*_once`（锁内建连 + 执行 + 错误映射）+
//! 公开方法（[`with_retry`] 包装）。不用泛型闭包是有意的——
//! russh-sftp 的方法 future 借用 `&self`，HRTB 闭包会强迫捕获数据
//! `'static`，具体方法让生命周期自然成立。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Handle};
use russh::keys::{decode_secret_key, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::fs::{File, ReadDir};
use russh_sftp::client::{Config as SftpConfig, SftpSession};
use russh_sftp::extensions::Statvfs;
use russh_sftp::protocol::{FileAttributes, OpenFlags};

use cloudkit_storage::StorageError;

use crate::config::SftpParams;
use crate::error::{map_session_error, map_sftp_error, normalize_fingerprint, SessionError};

/// connect 全程预算（含 TCP + KEX + 认证 + SFTP 握手）——网络阻塞的
/// 静默挂起是真机事故形态（driver-onboarding §10：连接路径必须有
/// deadline 界），驱动内自设上限，组合根另有 connect_with_deadline
/// 外层守卫。
const CONNECT_BUDGET: Duration = Duration::from_secs(45);

/// SFTP 会话请求超时（russh-sftp `set_timeout`；对齐其库缺省 10s
/// 语义，显式声明防漂移）。
const SFTP_REQUEST_TIMEOUT_SECS: u64 = 30;

/// 断线重连一次重放（重连骨架，行为测试在 SF2 桩批）：`Unavailable`
/// 形态浮现 → 清空连接槽 → 重试一次；重试仍 `Unavailable` 时合并两
/// 段诊断（不吞掉任何一段）。
macro_rules! with_retry {
    ($self:ident, $($call:tt)*) => {
        match $self.$($call)*.await {
            Err(StorageError::Unavailable(detail)) => {
                $self.invalidate().await;
                $self.$($call)*.await.map_err(|error| match error {
                    StorageError::Unavailable(retry_detail) => StorageError::Unavailable(
                        format!("{detail}; reconnect retry: {retry_detail}"),
                    ),
                    other => other,
                })
            }
            other => other,
        }
    };
}

/// host key 校验回调的载体（D2 三态在此判定）。
///
/// 绝不无条件接受（termcp 教训）：`expected = None` → `Err`（拒连 +
/// 文案带服务器实际指纹与接受途径）；匹配 → `Ok(true)`；不匹配 →
/// `Err`（恒拒，MITM 信号，文案指明移除旧指纹重新接受）。
struct SshHandler {
    expected: Option<String>,
}

impl client::Handler for SshHandler {
    type Error = SessionError;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // D2 判定只用公钥本体的 SHA256 指纹（证书形态取证书内公钥；
        // KeyData → PublicKey 有 Into 桥）。
        let public = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => cert.public_key().clone().into(),
        };
        let actual = format!("{}", public.fingerprint(russh::keys::HashAlg::Sha256));
        match &self.expected {
            None => Err(SessionError::HostKeyUnpinned { actual }),
            Some(expected) if normalize_fingerprint(expected) == normalize_fingerprint(&actual) => {
                Ok(true)
            }
            Some(expected) => Err(SessionError::HostKeyMismatch {
                expected: normalize_fingerprint(expected),
                actual,
            }),
        }
    }
}

/// 一条活着的 SSH+SFTP 连接（D3：每卷至多一个实例存在）。
struct SshConnection {
    /// SSH 会话句柄（断开/保活；保活任务 SF4 真机批议）。
    #[allow(dead_code)] // SF1 骨架：disconnect() 在 SF2 桩批的收尾路径接线
    handle: Handle<SshHandler>,
    /// SFTP 子系统会话（元数据操作的执行面）。
    session: SftpSession,
}

/// SFTP 客户端：单连接 + 惰性建立 + 断线重建（模块文档「连接生命
/// 周期」节）。
///
/// - 语义：领域方法（`metadata`/`read_dir`/`rename`/...）在（必要时
///   建立的）会话上执行；`Unavailable` 失败自动清槽重试一次；
/// - 错误：连接建立失败按 [`map_session_error`] 归一（host key/认证
///   → `Unauthorized{false}` + 可行动文案）；
/// - 并发：内部 `tokio::sync::Mutex` 串行化建立/拆除；已建立的
///   SftpSession 可被并发使用（会话层自同步）；
/// - 生命周期：连接归本客户端自理（trait 契约），Drop 交由 russh
///   句柄自然拆除。
pub(crate) struct SftpClient {
    params: Arc<SftpParams>,
    connection: tokio::sync::Mutex<Option<SshConnection>>,
}

impl SftpClient {
    /// 构造（不连接——惰性建立是 D3 的既定形态）。
    pub(crate) fn new(params: Arc<SftpParams>) -> Self {
        SftpClient {
            params,
            connection: tokio::sync::Mutex::new(None),
        }
    }

    /// 参数快照（driver 层路径计算用）。
    pub(crate) fn params(&self) -> &SftpParams {
        &self.params
    }

    /// 清空连接槽（`with_retry` 内部使用；测试面）。
    async fn invalidate(&self) {
        self.connection.lock().await.take();
    }

    /// 锁内确保连接存在（惰性建立点）。返回的 guard 持有到当前
    /// 操作完成——连接的拆除（invalidate）必须等在途操作出锁。
    async fn connected(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<SshConnection>>, StorageError> {
        let mut guard = self.connection.lock().await;
        if guard.is_none() {
            let connection = tokio::time::timeout(CONNECT_BUDGET, connect_once(&self.params))
                .await
                .map_err(|_| {
                    StorageError::Unavailable(format!(
                        "sftp connect to {}:{} did not finish within {CONNECT_BUDGET:?}",
                        self.params.host, self.params.port
                    ))
                })?
                .map_err(map_session_error)?;
            *guard = Some(connection);
        }
        Ok(guard)
    }

    // ------------------------------------------------ 领域方法面 ---

    /// stat 一个远端路径（missing → `SSH_FX_NO_SUCH_FILE` → NotFound；
    /// PermissionDenied / 连接错误绝不折叠为 NotFound——error.rs 钉死）。
    /// **跟随符号链接**（SSH_FXP_STAT 语义）——面向用户可见的 path 面。
    pub(crate) async fn metadata(&self, path: &str) -> Result<FileAttributes, StorageError> {
        with_retry!(self, metadata_once(path))
    }

    async fn metadata_once(&self, path: &str) -> Result<FileAttributes, StorageError> {
        let guard = self.connected().await?;
        let conn = guard.as_ref().expect("connected() just ensured");
        conn.session.metadata(path).await.map_err(map_sftp_error)
    }

    /// **不跟随**符号链接的 stat（SSH_FXP_LSTAT 语义）——递归删除与
    /// 「本体是什么」判定的正确面（aeroftp 教训 8 / 其 GAP-A02：跟随
    /// 形态会把 symlink-to-dir 当目录下潜，递归删除将走进链接目标）。
    pub(crate) async fn symlink_metadata(
        &self,
        path: &str,
    ) -> Result<FileAttributes, StorageError> {
        with_retry!(self, symlink_metadata_once(path))
    }

    async fn symlink_metadata_once(&self, path: &str) -> Result<FileAttributes, StorageError> {
        let guard = self.connected().await?;
        let conn = guard.as_ref().expect("connected() just ensured");
        conn.session
            .symlink_metadata(path)
            .await
            .map_err(map_sftp_error)
    }

    /// depth-1 列目录（readdir 到 EOF——SF2 桩批钉死协议语义）。
    pub(crate) async fn read_dir(&self, path: &str) -> Result<ReadDir, StorageError> {
        with_retry!(self, read_dir_once(path))
    }

    async fn read_dir_once(&self, path: &str) -> Result<ReadDir, StorageError> {
        let guard = self.connected().await?;
        let conn = guard.as_ref().expect("connected() just ensured");
        conn.session.read_dir(path).await.map_err(map_sftp_error)
    }

    /// 创建一个目录（单层；父链由 driver 的 ensure_parents 负责）。
    pub(crate) async fn create_dir(&self, path: &str) -> Result<(), StorageError> {
        with_retry!(self, create_dir_once(path))
    }

    async fn create_dir_once(&self, path: &str) -> Result<(), StorageError> {
        let guard = self.connected().await?;
        let conn = guard.as_ref().expect("connected() just ensured");
        conn.session.create_dir(path).await.map_err(map_sftp_error)
    }

    /// 删除文件。
    pub(crate) async fn remove_file(&self, path: &str) -> Result<(), StorageError> {
        with_retry!(self, remove_file_once(path))
    }

    async fn remove_file_once(&self, path: &str) -> Result<(), StorageError> {
        let guard = self.connected().await?;
        let conn = guard.as_ref().expect("connected() just ensured");
        conn.session.remove_file(path).await.map_err(map_sftp_error)
    }

    /// 删除空目录。
    pub(crate) async fn remove_dir(&self, path: &str) -> Result<(), StorageError> {
        with_retry!(self, remove_dir_once(path))
    }

    async fn remove_dir_once(&self, path: &str) -> Result<(), StorageError> {
        let guard = self.connected().await?;
        let conn = guard.as_ref().expect("connected() just ensured");
        conn.session.remove_dir(path).await.map_err(map_sftp_error)
    }

    /// 重命名/移动（服务端单侧，server_side_move 能力位的依据）。
    pub(crate) async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
        with_retry!(self, rename_once(from, to))
    }

    async fn rename_once(&self, from: &str, to: &str) -> Result<(), StorageError> {
        let guard = self.connected().await?;
        let conn = guard.as_ref().expect("connected() just ensured");
        conn.session.rename(from, to).await.map_err(map_sftp_error)
    }

    /// statvfs（不支持扩展的服务器返回 `None`，配额形态交 driver）。
    pub(crate) async fn fs_info(&self, path: &str) -> Result<Option<Statvfs>, StorageError> {
        with_retry!(self, fs_info_once(path))
    }

    async fn fs_info_once(&self, path: &str) -> Result<Option<Statvfs>, StorageError> {
        let guard = self.connected().await?;
        let conn = guard.as_ref().expect("connected() just ensured");
        conn.session.fs_info(path).await.map_err(map_sftp_error)
    }

    /// 打开远端读文件（READ flag；句柄移出锁，流式读在锁外推进）。
    pub(crate) async fn open_read(&self, path: &str) -> Result<File, StorageError> {
        with_retry!(self, open_read_once(path))
    }

    async fn open_read_once(&self, path: &str) -> Result<File, StorageError> {
        let guard = self.connected().await?;
        let conn = guard.as_ref().expect("connected() just ensured");
        conn.session.open(path).await.map_err(map_sftp_error)
    }

    /// 打开远端写文件（硬仗③：`WRITE | CREATE | TRUNCATE`——绝不用
    /// `APPEND`；覆盖 = 截断重写，offset 语义完全由顺序写掌握）。
    pub(crate) async fn open_write_truncate(&self, path: &str) -> Result<File, StorageError> {
        with_retry!(self, open_write_truncate_once(path))
    }

    async fn open_write_truncate_once(&self, path: &str) -> Result<File, StorageError> {
        let guard = self.connected().await?;
        let conn = guard.as_ref().expect("connected() just ensured");
        conn.session
            .open_with_flags(
                path,
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
            )
            .await
            .map_err(map_sftp_error)
    }
}

/// 探连接（doctor 腿）：完整建连一次（TCP/KEX/指纹/认证/SFTP 握手）
/// 后丢弃句柄（Drop 拆除），把 [`SessionError`] 原样交给调用方做结构
/// 化归类（[`crate::probe`]）。指纹这三态的细节只有这里能拿到——
/// 驱动面的 `Unauthorized` 无载荷（L2 冻结契约），doctor 需要实际
/// 指纹来做「复制进配置」的指引。
pub(crate) async fn probe_connect(params: &SftpParams) -> Result<(), SessionError> {
    let connection = tokio::time::timeout(CONNECT_BUDGET, connect_once(params))
        .await
        .map_err(|_| SessionError::Ssh(russh::Error::ConnectionTimeout))??;
    // 探针不保留会话：显式断开（await 确认），SF2 桩的句柄计数据此归零
    let handle = connection.handle;
    let _ = handle
        .disconnect(russh::Disconnect::ByApplication, "", "")
        .await;
    Ok(())
}

/// 建立一条完整连接：TCP + KEX（含 D2 host key 校验）→ 认证（D1）→
/// sftp 子系统 → SftpSession。行为测试在 SF2 桩批。
async fn connect_once(params: &SftpParams) -> Result<SshConnection, SessionError> {
    let ssh_config = Arc::new(client::Config::default());
    let handler = SshHandler {
        expected: params.host_fingerprint.clone(),
    };
    let addr = (params.host.as_str(), params.port);
    // connect 的错误类型即 H::Error（SessionError）——host key 拒绝
    // （check_server_key 的 Err）原样穿透到这里。
    let mut handle = client::connect(ssh_config, addr, handler).await?;

    authenticate(&mut handle, params).await?;

    let channel = handle
        .channel_open_session()
        .await
        .map_err(SessionError::Ssh)?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(SessionError::Ssh)?;
    // 3.0 的并发读/写旋钮走库缺省（max_concurrent_reads=16 流水线，
    // 计划 §1.3——不复制 aeroftp 的 2.x 绕行），这里只钉请求超时。
    let sftp_config = SftpConfig {
        request_timeout_secs: SFTP_REQUEST_TIMEOUT_SECS,
        ..SftpConfig::default()
    };
    let session = SftpSession::new_with_config(channel.into_stream(), sftp_config)
        .await
        .map_err(SessionError::Sftp)?;
    Ok(SshConnection { handle, session })
}

/// D1 认证：私钥优先（自动化的强形态），密码兜底；两者皆失败 →
/// `AuthFailed`（→ `Unauthorized { recoverable: false }`）。
/// keyboard-interactive 与 ssh-agent 不做（SF0 挂账）。行为测试在
/// SF2 桩批。
async fn authenticate(
    handle: &mut Handle<SshHandler>,
    params: &SftpParams,
) -> Result<(), SessionError> {
    if let Some(key_path) = &params.private_key_path {
        if try_publickey(handle, params, key_path).await? {
            return Ok(());
        }
    }
    if let Some(password) = &params.password {
        let result = handle
            .authenticate_password(&params.username, password)
            .await
            .map_err(SessionError::Ssh)?;
        if result.success() {
            return Ok(());
        }
    }
    Err(SessionError::AuthFailed {
        user: params.username.clone(),
        detail: auth_failure_detail(params),
    })
}

/// 私钥面：读文件（阻塞 IO 隔离）→ `decode_secret_key`（含
/// passphrase 解锁）→ RSA 协商最佳散列 → `authenticate_publickey`。
async fn try_publickey(
    handle: &mut Handle<SshHandler>,
    params: &SftpParams,
    key_path: &Path,
) -> Result<bool, SessionError> {
    let path = key_path.to_path_buf();
    let passphrase = params.private_key_passphrase.clone();
    let text = tokio::task::spawn_blocking(move || std::fs::read_to_string(&path))
        .await
        .map_err(|e| SessionError::AuthFailed {
            user: params.username.clone(),
            detail: format!("reading the private key file failed: {e}"),
        })?
        .map_err(|e| SessionError::AuthFailed {
            user: params.username.clone(),
            detail: format!(
                "reading the private key file {} failed: {e}",
                key_path.display()
            ),
        })?;
    let key =
        decode_secret_key(&text, passphrase.as_deref()).map_err(|e| SessionError::AuthFailed {
            user: params.username.clone(),
            detail: format!(
                "parsing the private key {} failed: {e} (wrong format, or an encrypted \
                 key without its sftp_private_key_passphrase?)",
                key_path.display()
            ),
        })?;
    let hash_alg = if key.algorithm().is_rsa() {
        handle
            .best_supported_rsa_hash()
            .await
            .map_err(SessionError::Ssh)?
            .flatten()
    } else {
        None
    };
    let with_hash = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
    let result = handle
        .authenticate_publickey(&params.username, with_hash)
        .await
        .map_err(SessionError::Ssh)?;
    Ok(result.success())
}

/// 认证失败的可行动诊断（不回显任何凭据值——R3）。
fn auth_failure_detail(params: &SftpParams) -> String {
    let mut detail = String::from("the server rejected every configured auth method");
    if params.private_key_path.is_some() {
        detail.push_str(" (private key");
        if params.password.is_some() {
            detail.push_str(" and password");
        }
        detail.push(')');
    } else {
        detail.push_str(" (password)");
    }
    detail.push_str(": check sftp_username, the password/key and the passphrase");
    detail
}
