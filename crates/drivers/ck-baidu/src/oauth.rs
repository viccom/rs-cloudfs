//! OAuth refresh 状态机（K13）与持久化回调。
//!
//! 语义契约（mock 钉死于 `tests/oauth_state_machine.rs`；分歧以 spike
//! 实抓为准——`examples/baidu_spike/src/api.rs:22-81` 与报告 §1）：
//!
//! - **errno 110**（access_token 过期）：驱动内刷新 + 原请求**重放一次**；
//!   重放成功 → 操作成功；重放仍 110 → `Unauthorized { recoverable: true }`；
//! - **errno 111**（refresh_token 过期）/ **-6**（鉴权失败）：
//!   `Unauthorized { recoverable: false }`，**零刷新调用**——上层给
//!   「重新走授权流程」指引，绝不死循环（§7a）；
//! - **刷新产物即刻持久化**：refresh_token 一次一换、旧值即刻作废
//!   （spike §1 实证）——刷新响应到达即回调 [`TokenStore::save_tokens`]，
//!   即便随后的重放失败也不回收（新 refresh_token 已是唯一活值）；
//! - oauth 端点错误形态：顶层 `error`/`error_description` 字符串（HTTP
//!   4xx），成功形态 `access_token`/`refresh_token`/`expires_in`。

/// 刷新产物持久化回调（K13）。
///
/// 由凭据存储（CredentialStore，B3b 组合根接线）实现并经
/// [`crate::BaiduParams::token_store`] 注入：驱动自助刷新成功后回调本
/// 方法持久化新 token 对。驱动内不依赖任何 core 类型（R1）——回调是
/// 驱动 crate 自有契约，B3b 负责与 CredentialStore 桥接。
///
/// 生命周期契约：回调在驱动的请求路径上同步发生；实现方不得再回调驱动
/// （防重入），且应自行处理持久化失败（阻塞或吞掉由实现方决定，但不得
/// 丢失新 refresh_token——它是唯一的活值）。
pub trait TokenStore: Send + Sync {
    /// 持久化刷新产物（access_token 与 refresh_token 一次一换，成对落盘）。
    fn save_tokens(&self, access_token: &str, refresh_token: &str);
}
