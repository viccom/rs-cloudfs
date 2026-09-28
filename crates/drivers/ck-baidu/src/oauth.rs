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
//! - **刷新产物即刻持久化**：refresh_token 实测可复用（2026-09-25 勘误，旧值并非即刻作废），仍按保守策略即刻落盘
//!   （spike §1 实证）——刷新响应到达即回调 [`TokenStore::save_tokens`]，
//!   即便随后的重放失败也不回收（新 refresh_token 已是唯一活值）；
//! - oauth 端点错误形态：顶层 `error`/`error_description` 字符串（HTTP
//!   4xx），成功形态 `access_token`/`refresh_token`/`expires_in`。
//!
//! 刷新的**编排**（单飞锁 + 陈旧检查 + 持久化回调时序）在 client.rs 的
//! `BaiduClient::refresh_once`；本模块只做单次 wire 请求与错误归一。

use cloudkit_storage::StorageError;

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
    /// 持久化刷新产物（access_token 与 refresh_token 成对落盘——RT 实测可复用，保守策略不变）。
    fn save_tokens(&self, access_token: &str, refresh_token: &str);
}

/// GET `/oauth/2.0/token?grant_type=refresh_token&…`——标准刷新授权
/// （wire 形态 = spike `api.rs:33-40` 实抓：恰四参数）。
///
/// 错误归一：
/// - 顶层 `error` 字符串（HTTP 4xx，如 `invalid_grant`）→
///   `Unauthorized { recoverable: false }`——refresh_token 已失效，唯一
///   出路是人工重新授权（K13 三档之二；error/error_description 不含
///   凭据值，可入载荷，但本驱动只取 error 码入日志路径，token 值
///   **绝不**出现在任何载荷/日志——R3）；
/// - 网络层失败 → `Unavailable`（暂时性；调用方（client.rs 的
///   `BaiduClient` 单次自救机会）就此放弃并上抛——不重试 oauth 本身）；
/// - 成功响应缺 `access_token`/`refresh_token` 字段 → `Unavailable`
///   （协议异常；载荷不含响应体——半截响应可能携带一半新凭据对）。
pub(crate) async fn refresh_grant(
    http: &reqwest::Client,
    oauth_base: &str,
    app_key: &str,
    app_secret: &str,
    refresh_token: &str,
) -> Result<super::client::TokenPair, StorageError> {
    let resp = http
        .get(format!("{oauth_base}/oauth/2.0/token"))
        .query(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", app_key),
            ("client_secret", app_secret),
        ])
        .send()
        .await
        .map_err(|e| {
            // without_url：reqwest 错误串默认内嵌完整 URL（query 含
            // refresh_token/client_secret——R3 凭据红线），剥离后再入载荷。
            StorageError::Unavailable(format!("oauth transport: {}", e.without_url()))
        })?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| StorageError::Unavailable(format!("oauth body read: {}", e.without_url())))?;
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|_| StorageError::Unavailable(format!("oauth non-json (http {status})")))?;
    if let Some(_err) = v.get("error").and_then(|e| e.as_str()) {
        // oauth 端点错误形态：顶层 error 字符串（spike api.rs:49-55 实抓）。
        // invalid_grant = refresh_token 陈旧/失效（保守模型下旧值视为不可依赖）
        // → Unauthorized{recoverable:false}（K13 三档之二）。Unauthorized
        // 无载荷位，error/error_description 明细不外带（二者不含凭据值，
        // 丢弃明细是分类学形态的既有取舍）。
        return Err(StorageError::Unauthorized { recoverable: false });
    }
    let access = v
        .get("access_token")
        .and_then(|x| x.as_str())
        .ok_or_else(|| StorageError::Unavailable("oauth response missing access_token".into()))?
        .to_string();
    let refresh = v
        .get("refresh_token")
        .and_then(|x| x.as_str())
        .ok_or_else(|| StorageError::Unavailable("oauth response missing refresh_token".into()))?
        .to_string();
    Ok(super::client::TokenPair { access, refresh })
}
