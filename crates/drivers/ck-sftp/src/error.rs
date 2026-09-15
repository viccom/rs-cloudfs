//! 错误映射（R2：驱动层错误禁止跨层裸传）——计划 §4.3 映射表。
//!
//! ## 分层设计（aeroftp 教训的正面落地）
//!
//! 映射函数按错误**来源层**分三个纯函数，而非一个大杂烩——每一层
//! 的「连接丢失」形态不同，混在一起必然有人把 `PermissionDenied` /
//! `ConnectionLost` 折叠成 `NotFound`（aeroftp `:102-120` 实测事故：
//! 不可读父目录下的根被误判为路径缺口）：
//!
//! 1. [`map_status`]——SFTP 协议状态码（SSH_FXP_STATUS）；
//! 2. [`map_sftp_error`]——russh-sftp 客户端层（含字符串化 IO 与
//!    超时；连接死信清单 [`CONNECTION_LOSS_MARKERS`] 是唯一来源，
//!    aeroftp 教训 A.2-5「四份拷贝互相不一致」的反面）；
//! 3. [`map_ssh_error`]——russh SSH 传输层（连接/会话/通道）。
//!
//! 认证失败与 host key 拒绝不走状态码：[`crate::client::SessionError`]
//! 的两个专属变体在认证/host-key 回调处产生，经 [`map_session_error`]
//! 归一（`Unauthorized { recoverable: false }` + 可行动文案）。
//!
//! ## 映射表（计划 §4.3，桩回放测试钉死于 SF2）
//!
//! | 来源 | StorageError |
//! |---|---|
//! | `SSH_FX_NO_SUCH_FILE` | `NotFound` |
//! | `SSH_FX_PERMISSION_DENIED` | `Unauthorized { recoverable: false }` |
//! | `SSH_FX_NO_CONNECTION` / `SSH_FX_CONNECTION_LOST` | `Unavailable` |
//! | `SSH_FX_FAILURE` / 其他状态码 | `Io`（保留原始码与消息；上下文相关的 `Exists`/`Invalid` 分派归驱动方法层的预检） |
//! | 认证失败（password/key 均拒） | `Unauthorized { recoverable: false }` |
//! | 连接/会话丢失、超时 | `Unavailable` |
//! | 其他未知 | `Io`（保留原始码与消息，R2 要求） |
//!
//! **红线**：`exists`/`stat` 的 NotFound 判定只认 `SSH_FX_NO_SUCH_FILE`
//! ——`PermissionDenied` 与连接类错误**绝不**折叠为 NotFound（本模块
//! 单测逐条钉死）。

use cloudkit_storage::StorageError;
use russh_sftp::client::error::Error as SftpClientError;
use russh_sftp::protocol::{Status, StatusCode};

/// russh-sftp 字符串化 IO 错误里「连接已死」的信标（**单一来源**，
/// aeroftp 教训 A.2-5：它曾有四份互相不一致的拷贝，一份把 broken
/// pipe 当磁盘错误而拒绝重试）。命中任一子串 → [`StorageError::Unavailable`]。
///
/// 栈内实测形态（SF2 桩断连测试钉死）：
/// - `sender dropped`——russh-sftp `Request::poll` 在回复通道 sender 被
///   drop 时返回 `UnexpectedBehavior("sender dropped")`（rawsession 内部
///   任务随连接死亡而终止 = 在途请求的传输层死信号）；
/// - `channel closed`——russh ChannelStream 对已死会话写入时的
///   `io::Error(BrokenPipe, "channel closed")`。
const CONNECTION_LOSS_MARKERS: &[&str] = &[
    "broken pipe",
    "connection reset",
    "connection aborted",
    "connection closed",
    "connection refused",
    "connection shutdown",
    "unexpected eof",
    "unexpected end of file",
    "os error 10054", // WSAECONNRESET
    "os error 10053", // WSAECONNABORTED
    "sender dropped", // russh-sftp 在途请求的会话死信号
    "channel closed", // russh 死会话上的通道写入
];

/// `true` 当消息文本呈现连接死形态（供 [`map_sftp_error`] 的 IO 腿）。
pub(crate) fn looks_like_connection_loss(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    CONNECTION_LOSS_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// SFTP 状态码 → StorageError（SSH_FXP_STATUS 面；模块文档映射表）。
///
/// **红线**（aeroftp 教训）：只有 `NoSuchFile` 映射为 `NotFound`——
/// `PermissionDenied` 与连接类状态绝不折叠成 NotFound。
pub(crate) fn map_status(status: &Status) -> StorageError {
    // 原始码数字保留进 Io/Unavailable 载荷（R2：可诊断不丢信息）
    let code = status.status_code as u32;
    match status.status_code {
        StatusCode::NoSuchFile => StorageError::NotFound,
        StatusCode::PermissionDenied => StorageError::Unauthorized {
            // SSH 服务器权限拒绝无自救面（改凭据/改服务器权限都是人工动作）
            recoverable: false,
        },
        StatusCode::NoConnection | StatusCode::ConnectionLost => {
            StorageError::Unavailable(format!(
                "sftp status {code} ({}): {}",
                status.status_code, status.error_message
            ))
        }
        StatusCode::Ok | StatusCode::Eof => {
            // Ok/Eof 作为错误浮现 = 协议错位（read_dir/read 内部消费它们）
            StorageError::Io(format!(
                "sftp status {code} ({}) surfaced as an error: {}",
                status.status_code, status.error_message
            ))
        }
        StatusCode::Failure | StatusCode::BadMessage | StatusCode::OpUnsupported => {
            // 上下文相关（Failure 可能是「已存在」等）——归 Io 保留原始
            // 码与消息，Exists/Invalid 的上下文分派归驱动方法层预检
            StorageError::Io(format!(
                "sftp status {code} ({}): {}",
                status.status_code, status.error_message
            ))
        }
        #[allow(unreachable_patterns)]
        _ => StorageError::Io(format!("sftp status {code}: {}", status.error_message)),
    }
}

/// russh-sftp 客户端错误 → StorageError（状态码 + 超时 + 字符串 IO）。
pub(crate) fn map_sftp_error(error: SftpClientError) -> StorageError {
    match error {
        SftpClientError::Status(status) => map_status(&status),
        SftpClientError::Timeout => StorageError::Unavailable(
            "sftp request timeout (the server stopped answering; connection presumed dead)"
                .to_string(),
        ),
        SftpClientError::IO(message) => {
            if looks_like_connection_loss(&message) {
                StorageError::Unavailable(format!("sftp io (connection lost): {message}"))
            } else {
                StorageError::Io(format!("sftp io: {message}"))
            }
        }
        SftpClientError::Limited(detail) => {
            // 分类学边界：RateLimited 不带消息载荷（FloodWait 先例同）；
            // detail 是 limits@openssh 的限制名（如句柄上限），不承载
            // 凭据/诊断必需信息，丢弃可接受
            let _ = &detail;
            StorageError::RateLimited {
                // limits@openssh.com 不携带等待时长——None = 调用方自行退避
                retry_after: None,
            }
        }
        SftpClientError::UnexpectedPacket => {
            StorageError::Io("sftp protocol error: unexpected packet from the server".to_string())
        }
        SftpClientError::UnexpectedBehavior(detail) => {
            // 会话死信号（"sender dropped"——在途请求的回复通道消失）
            // 归 Unavailable：with_retry 的重连触发形态；其余保持 Io
            //（保留原始消息，R2）。
            if looks_like_connection_loss(&detail) {
                StorageError::Unavailable(format!("sftp session lost: {detail}"))
            } else {
                StorageError::Io(format!("sftp unexpected server behavior: {detail}"))
            }
        }
    }
}

/// russh SSH 传输层错误 → StorageError（连接/会话/通道面）。
pub(crate) fn map_ssh_error(error: russh::Error) -> StorageError {
    use russh::Error;
    match error {
        // 连接/会话丢失与超时 → Unavailable（重连骨架的触发形态）
        Error::Disconnect
        | Error::HUP
        | Error::ConnectionTimeout
        | Error::KeepaliveTimeout
        | Error::InactivityTimeout
        | Error::SendError
        | Error::WrongChannel => StorageError::Unavailable(format!("ssh transport: {error}")),
        // 其他（KEX/算法/协议/包形态）→ Io 保留原始消息
        other => StorageError::Io(format!("ssh: {other}")),
    }
}

/// host key 指纹归一化：去首尾空白、大写化、补齐 `SHA256:` 前缀。
///
/// 容忍用户粘贴形态（`sha256:xxx` / 裸 base64 / 带空白），比较在归一
/// 化之后进行（D2 的「匹配静默通过」判定点）。这不是放松安全：指纹
/// 本体逐字符比较，归一化只统一**写法**（大小写与可选前缀）。
pub(crate) fn normalize_fingerprint(raw: &str) -> String {
    let trimmed = raw.trim().to_ascii_uppercase();
    if trimmed.starts_with("SHA256:") {
        trimmed
    } else {
        format!("SHA256:{trimmed}")
    }
}

/// 会话建立/认证/host key 的驱动侧错误（russh `Handler::Error` 关联
/// 类型——`check_server_key` 的 `Err` 原样穿透 `client::connect` 的
/// 返回，所以 host key 细节必须住在这里才能到达调用方文案）。
#[derive(Debug, thiserror::Error)]
pub(crate) enum SessionError {
    /// SSH 传输层失败（TCP/KEX/通道）。
    #[error("ssh transport error: {0}")]
    Ssh(#[from] russh::Error),
    /// D2-甲「未接受」：配置无指纹 → 拒连（绝不 TOFU、绝不无条件接受）。
    #[error(
        "server host key not accepted yet: the server presented {actual}; to trust this \
         server copy that fingerprint into the volume config's sftp_host_fingerprint key \
         (or run the SF3 accept flow)"
    )]
    HostKeyUnpinned { actual: String },
    /// D2「恒拒」：指纹不匹配（MITM 信号）——救济 = 显式移除旧指纹
    /// 重新接受。
    #[error(
        "server host key CHANGED (a man-in-the-middle is one possible cause): expected \
         {expected}, got {actual}; remove the old sftp_host_fingerprint value and accept \
         the new one only after verifying it out-of-band"
    )]
    HostKeyMismatch { expected: String, actual: String },
    /// D1 认证失败（password/key 均拒或材料不可读）。
    #[error("ssh authentication failed for user {user}: {detail}")]
    AuthFailed { user: String, detail: String },
    /// SFTP 子系统握手失败。
    #[error("sftp subsystem error: {0}")]
    Sftp(#[from] russh_sftp::client::error::Error),
}

/// 会话错误 → StorageError（host key/认证 → `Unauthorized{false}`；
/// 传输/协议层走对应 map 函数）。
///
/// **分类学边界与文案可达性**：`Unauthorized` 变体无消息载荷（L2
/// 冻结契约，interfaces §3——「可行动指引由调用方给出」），而 D2
/// 要求拒绝文案包含服务器实际指纹与接受途径。落法：完整可行动
/// 文案保留在 [`SessionError`] 的 Display 里，并在映射点经
/// `tracing::warn!` 落日志（boot 错误/日志双通道可见）；错误本身
/// 归一为 `Unauthorized { recoverable: false }`（信任决策未完成 =
/// 人工动作，语义与计划 §4.5「可行动的 Unauthorized」一致）。
pub(crate) fn map_session_error(error: SessionError) -> StorageError {
    match error {
        SessionError::HostKeyUnpinned { .. }
        | SessionError::HostKeyMismatch { .. }
        | SessionError::AuthFailed { .. } => {
            // 完整文案（指纹 + 接受/移除途径 / 认证诊断）先进日志——
            // Unauthorized 无载荷，这是 D2「可行动文案」的可达通道
            tracing::warn!(target: "ck_sftp::session", "{error}");
            StorageError::Unauthorized { recoverable: false }
        }
        SessionError::Ssh(transport) => map_ssh_error(transport),
        SessionError::Sftp(sftp) => map_sftp_error(sftp),
    }
}
