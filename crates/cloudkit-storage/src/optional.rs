//! 可选能力 trait（interfaces §1：能力探测代替 trait 分叉）。
//!
//! v1 只定义契约不提供实现（消费方按 `Capabilities` 位 + trait 探测降级，
//! 绝不因后端缺能力而 panic）。演进规则：优先 provided 方法/新可选
//! trait，改签名 = breaking 须先列波及面清单。

use async_trait::async_trait;

use crate::driver::StorageDriver;
use crate::error::StorageError;
use crate::vocab::{Entry, RelPath, WriteHint};

/// 鉴权生命周期回调（D1 可选 trait #1）。
///
/// 由凭据存储（CredentialStore）实现并交给会自助刷新 token 的驱动
/// （baidu 型 OAuth）：驱动自救刷新**成功后**回调持久化新 token；
/// 鉴权不可恢复失效时回调上层走重新授权指引（`#[error]` 文案与
/// `Unauthorized { recoverable: false }` 的行动语义一致）。
///
/// 生命周期：回调在驱动的请求路径上同步发生；实现方不得再回调驱动
/// （防重入）。
#[async_trait]
pub trait TokenEvents: Send + Sync {
    /// 驱动自助刷新 token 成功——持久化新凭据。
    async fn on_token_refreshed(&self) -> Result<(), StorageError>;

    /// 鉴权不可恢复失效（如 refresh token 过期）——提示重新授权。
    async fn on_authorization_lost(&self, reason: &str) -> Result<(), StorageError>;
}

/// 秒传（D1 可选 trait #2）：由内容指纹直接落盘，消费 [`WriteHint`]。
///
/// 仅在能力位 `RAPID_UPLOAD` 声明后由消费方探测使用。
#[async_trait]
pub trait RapidUpload: StorageDriver {
    /// 尝试按 `hint`（size + 明文哈希）秒传。
    /// `Ok(None)` = 后端无该内容——调用方回落 `writer()` 常规暂存。
    async fn try_rapid_upload(
        &self,
        path: &RelPath,
        hint: &WriteHint,
    ) -> Result<Option<Entry>, StorageError>;
}

/// 后端变更推送（D1 可选 trait #3）：local 有（inotify 类）、baidu 无。
///
/// v1 骨架签名：游标轮询形态；事件载荷刻意最小（path + 删除位），
/// 字段演进走加字段 + serde default（interfaces §4）。
#[async_trait]
pub trait ChangeFeed: Send + Sync {
    /// 从 `cursor`（`None` = 从现在开始）拉取增量变更；
    /// `next_cursor = None` 表示暂无更多（调用方按退避轮询）。
    async fn poll_changes(&self, cursor: Option<String>) -> Result<ChangePage, StorageError>;
}

/// 单条后端变更事件。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChangeEvent {
    pub path: RelPath,
    pub removed: bool,
}

/// 一次轮询的变更批次。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChangePage {
    pub events: Vec<ChangeEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}
