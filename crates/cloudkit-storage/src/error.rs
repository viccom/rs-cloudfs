//! 存储错误分类学（foundation D2 / interfaces §3）。
//!
//! **R2 红线：驱动层错误禁止跨层裸传**——所有后端错误必须在驱动内映射
//! 为本分类学；L3+ 永远只见 [`StorageError`]。未知后端错误 → `Io`/
//! `Unavailable`，且原始错误码与消息必须保留在载荷里（可诊断不丢信息）。

use std::time::Duration;

/// 统一存储错误。
///
/// 未知后端错误的归置约定：本地 IO 性质的失败 → [`StorageError::Io`]，
/// 后端/网络/服务侧的暂时性失败 → [`StorageError::Unavailable`]，
/// 两者都必须在字符串载荷中保留后端错误码与原始消息。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// 目标不存在（路径或句柄）。
    #[error("not found")]
    NotFound,

    /// 目标已存在（如 mkdir 已有目录）。
    #[error("already exists")]
    Exists,

    /// 鉴权失败。
    ///
    /// 语义契约：**驱动必须先自救**（如刷新 token 并重放原请求一次），
    /// 自救失败才抛出本错误——调用方收到的 `Unauthorized` 一定是「驱动
    /// 已尝试过自救」之后的状态。
    /// - `recoverable = true`：换新凭据重试有望恢复（如 access token
    ///   过期且自动刷新那次恰好失败——再刷一次通常可行）；
    /// - `recoverable = false`：需要人工重新授权（如 refresh token 失效），
    ///   调用方应给出「重新走授权流程」的可行动指引，**勿盲目重试循环**。
    #[error("unauthorized (recoverable: {recoverable})")]
    Unauthorized { recoverable: bool },

    /// 被限流；`retry_after` 为后端明示的等待时长（如 telegram FloodWait）。
    #[error("rate limited (retry after {retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },

    /// 配额/容量不足。
    #[error("quota exceeded")]
    QuotaExceeded,

    /// 非法参数或非法状态（如非法路径、size 提示与实际不符、对目录 reader）。
    #[error("invalid argument or state")]
    Invalid,

    /// 驱动不支持该操作（能力位未声明却仍被调用）。
    #[error("unsupported operation")]
    Unsupported,

    /// 本地 IO 失败；载荷保留原始错误码与消息。
    #[error("io error: {0}")]
    Io(String),

    /// 后端/网络暂时不可用；载荷保留原始错误码与消息，重试合理。
    #[error("backend unavailable: {0}")]
    Unavailable(String),
}

/// 本地 `io::Error` 归置为 [`StorageError::Io`]（`to_string()` 保留消息）。
///
/// 服务于 trait 家族的 `?` 传播形态（mock / 驱动上传路径读取本地文件）；
/// 分类学本身不携带 io::Error 载荷（Clone/Eq 派生的代价取舍）。
impl From<std::io::Error> for StorageError {
    fn from(error: std::io::Error) -> Self {
        StorageError::Io(error.to_string())
    }
}
