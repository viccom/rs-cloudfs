//! HTTP 面：双 reqwest client + endpoint base URL 实例化 + 110 刷新重放
//! 与 31034 重试钩子。
//!
//! ## 网络形态（K18；照抄 spike `common.rs:240-264` 实证形态）
//!
//! - 直连：`no_proxy()`（实例配置 proxy_url 对本驱动无效，K18）；
//! - 强制 IPv4 dial：`local_address(Ipv4Addr::UNSPECIFIED)`（PCFS
//!   client.go:18-24 同款——DNS 优先 IPv6 连百度问题）；
//! - UA 恒为 netdisk 族（PCS 端点对非 netdisk UA 返回 403，spike §5 矩阵
//!   实证）：`netdisk;P2SP;2.2.91.136;android-android`；
//! - api client：redirect none（download 302 必须被观测而非跟随）+
//!   60s 每请求超时；stream client：redirect none + 20s 连接超时（B2
//!   下载/分片用，B1 一并构造）。
//!
//! ## 请求策略（dispatch 状态机；细节见 [`BaiduClient::dispatch`] 文档）
//!
//! - **110 刷新重放一次**：业务响应 errno=110 → oauth 刷新（新 token 对
//!   即刻回调 TokenStore 持久化）→ 原请求重放一次；重放仍 110 →
//!   `Unauthorized { recoverable: true }`；
//! - **31034 单点重试一次**（K15）：固定短退避（80ms——「单点重试一次」
//!   语义下指数退避退化为固定值；测试时间敏感，计划 §3 要求 base ≤1s
//!   量级，取 50–100ms 档）后重放一次，仍 31034 →
//!   `RateLimited { retry_after: None }`；不做全链路限速器；
//! - endpoint base URL 为**实例字段**（默认生产常量，测试注入 mock）。
//!
//! ## R3（凭据不入载荷/日志）
//!
//! 本 crate 无日志设施（B1）；错误载荷是唯一外泄面——三条防线：
//! reqwest 错误 `without_url()` 剥离内嵌 URL（query 带 token）、非 JSON
//! 响应体片段经当前 access_token 掩码替换（[`mask`]）、错误消息一律不
//! 拼接请求参数。

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use cloudkit_storage::StorageError;
use serde_json::Value;

use crate::oauth::{self, TokenStore};
use crate::BaiduParams;

/// 全请求恒定 UA（spike common.rs:16；PCS 端点行为依赖 netdisk 族 UA）。
pub(crate) const UA: &str = "netdisk;P2SP;2.2.91.136;android-android";

/// K15 重试退避：固定 80ms（模块文档「请求策略」节注源；B2 真机复测
/// spike §2 后如需加长再议——当前 appkey 桶零拒绝形态）。
const RATE_LIMIT_BACKOFF: Duration = Duration::from_millis(80);

/// 凭据掩码（spike common.rs:36-44 同形）：前 6 + 后 4；短值整体隐去。
pub(crate) fn mask(secret: &str) -> String {
    let n = secret.chars().count();
    if n >= 12 {
        let head: String = secret.chars().take(6).collect();
        let tail: String = secret.chars().skip(n - 4).collect();
        format!("{head}...{tail}")
    } else {
        "***".to_string()
    }
}

/// 驱动内当前 token 对（RwLock 保护；刷新成功后整体替换并回调持久化）。
#[derive(Debug, Clone)]
pub(crate) struct TokenPair {
    pub(crate) access: String,
    pub(crate) refresh: String,
}

/// 百度 HTTP 客户端（api/stream 双 client + base URL + token 状态）。
pub(crate) struct BaiduClient {
    /// pan/openapi API 面（redirect none + 60s 超时）。
    pub(crate) api: reqwest::Client,
    /// CDN 流面（redirect none + 20s 连接超时）。
    #[allow(dead_code)] // B1 一并构造（计划 §3）；B2 superfile2/下载路径消费后移除
    pub(crate) stream: reqwest::Client,
    pub(crate) api_base: String,
    pub(crate) oauth_base: String,
    pub(crate) app_key: String,
    pub(crate) app_secret: String,
    pub(crate) tokens: tokio::sync::RwLock<TokenPair>,
    pub(crate) token_store: Option<Arc<dyn TokenStore>>,
    /// 刷新单飞锁：并发 110 只放一个刷新过（一次一换语义下，第二个并发
    /// 刷新必拿陈旧 refresh_token 撞 `invalid_grant`）。
    refresh_lock: tokio::sync::Mutex<()>,
}

impl BaiduClient {
    /// 构造双 client 并装载 token 状态。
    ///
    /// `access_token`/`refresh_token` 缺失 → `Invalid`（B1 无授权流，
    /// 初始 token 由装配方提供——见 [`crate::factory`] 文档）。
    pub(crate) fn new(params: &BaiduParams) -> Result<Self, StorageError> {
        let (Some(access), Some(refresh)) = (&params.access_token, &params.refresh_token) else {
            return Err(StorageError::Invalid);
        };
        let base = || {
            reqwest::Client::builder()
                .no_proxy() // K18：直连，绕过本机代理
                .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED)) // K18：强制 IPv4 dial
                .user_agent(UA)
        };
        let api = base()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| StorageError::Io(format!("baidu api client build: {e}")))?;
        let stream = base()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| StorageError::Io(format!("baidu stream client build: {e}")))?;
        Ok(BaiduClient {
            api,
            stream,
            api_base: params.api_base.clone(),
            oauth_base: params.oauth_base.clone(),
            app_key: params.app_key.clone(),
            app_secret: params.app_secret.clone(),
            tokens: tokio::sync::RwLock::new(TokenPair {
                access: access.clone(),
                refresh: refresh.clone(),
            }),
            token_store: params.token_store.clone(),
            refresh_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// GET `{api_base}<path>` 追加 query 对（自动附当前 access_token），
    /// 返回解析后的 JSON 体；errno!=0 经 `api::map_errno` 归一，110/31034
    /// 按 [`Self::dispatch`] 策略自救。
    pub(crate) async fn api_get(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Value, StorageError> {
        self.dispatch(&format!("{}{path}", self.api_base), false, query, None)
            .await
    }

    /// POST `{api_base}<path>`：query 对（method/access_token 等）+ form
    /// 表单体（application/x-www-form-urlencoded）；策略同 [`Self::api_get`]。
    pub(crate) async fn api_post_form(
        &self,
        path: &str,
        query: &[(&str, &str)],
        form: &[(&str, String)],
    ) -> Result<Value, StorageError> {
        self.dispatch(&format!("{}{path}", self.api_base), true, query, Some(form))
            .await
    }

    /// 统一请求策略引擎（K13/K15；每逻辑调用至多 1 次刷新 + 1 次退避重试）。
    ///
    /// 状态机（`errno` 取自响应顶层；缺失视 0）：
    ///
    /// 1. `110` 且未刷新过 → [`Self::refresh_once`]（单飞 + on-arrival
    ///    持久化）→ `continue`（loop 头重读 token，重放即自动带新值）；
    /// 2. `110` 且已刷新过 → `Unauthorized { recoverable: true }`——
    ///    再刷一次通常可行，但绝不循环（K13）；
    /// 3. `31034` 且未重试过 → 固定短退避 → `continue` 重试一次；
    /// 4. `31034` 且已重试过 → `RateLimited { retry_after: None }`（后端
    ///    未明示等待时长）；
    /// 5. 其余非 0 → `api::map_errno` 归一（111/-6 零刷新直达
    ///    `Unauthorized { recoverable: false }`——因刷新只由 110 触发）；
    /// 6. `0` → 成功返回 JSON 体。
    ///
    /// 两钩子独立计次、可叠加（如重试后撞 110：仍允许自救一次，HTTP 层
    /// 至多 3 次请求，有界）。
    async fn dispatch(
        &self,
        url: &str,
        post: bool,
        query: &[(&str, &str)],
        form: Option<&[(&str, String)]>,
    ) -> Result<Value, StorageError> {
        let mut refreshed = false;
        let mut retried = false;
        loop {
            let token = self.tokens.read().await.access.clone();
            let mut pairs: Vec<(&str, &str)> = query.to_vec();
            pairs.push(("access_token", token.as_str()));
            let request = if post {
                let mut r = self.api.post(url);
                if let Some(form) = form {
                    r = r.form(form);
                }
                r
            } else {
                self.api.get(url)
            };
            let resp = request.query(&pairs).send().await.map_err(|e| {
                // without_url：reqwest 错误串内嵌完整 URL（query 带
                // access_token——R3），剥离后再入载荷。
                StorageError::Unavailable(format!("baidu transport: {}", e.without_url()))
            })?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| {
                StorageError::Unavailable(format!("baidu body read: {}", e.without_url()))
            })?;
            let v: Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(_) => {
                    // 非 JSON 体：截断 + 掩码当前 access_token 后入载荷
                    //（防御后端错误页回显 query 的形态）。
                    let snippet: String = body.chars().take(200).collect();
                    let masked = snippet.replace(token.as_str(), &mask(&token));
                    return Err(StorageError::Unavailable(format!(
                        "baidu non-json (http {status}): {masked}"
                    )));
                }
            };
            let errno = v.get("errno").and_then(Value::as_i64).unwrap_or(0);
            if errno == 110 && !refreshed {
                refreshed = true;
                self.refresh_once(&token).await?;
                continue; // 重放一次（loop 头重读 token，自动带新值）
            }
            if errno == 31034 && !retried {
                retried = true;
                tokio::time::sleep(RATE_LIMIT_BACKOFF).await;
                continue; // K15：单点重试一次
            }
            if errno != 0 {
                let errmsg = v.get("errmsg").and_then(Value::as_str).unwrap_or("");
                return Err(crate::api::map_errno(errno, errmsg));
            }
            return Ok(v);
        }
    }

    /// 刷新编排（单飞）：持锁双检 → wire 刷新（[`oauth::refresh_grant`]）→
    /// 整体替换 token 对 → on-arrival 持久化回调。
    ///
    /// - **双检**：拿到锁后若 access_token 已不是触发 110 时的陈旧值，
    ///   说明并发窗口内他人已刷新——直接复用新值（不消耗一次一换的
    ///   refresh_token）；
    /// - **on-arrival 持久化**（K13）：刷新响应到达即回调
    ///   [`TokenStore::save_tokens`]，**先于**重放结果——重放失败也不回收
    ///   （新 refresh_token 已是唯一活值，丢弃即凭据损失；mock 钉死）；
    /// - 刷新失败（`Unauthorized{false}`/`Unavailable`）：直接上抛——本
    ///   次逻辑调用的自救机会已用掉。
    async fn refresh_once(&self, stale_access: &str) -> Result<(), StorageError> {
        let _guard = self.refresh_lock.lock().await;
        {
            let current = self.tokens.read().await;
            if current.access != stale_access {
                return Ok(()); // 并发他人已刷新：复用新值
            }
        }
        let refresh_token = self.tokens.read().await.refresh.clone();
        let pair = oauth::refresh_grant(
            &self.api,
            &self.oauth_base,
            &self.app_key,
            &self.app_secret,
            &refresh_token,
        )
        .await?;
        *self.tokens.write().await = TokenPair {
            access: pair.access.clone(),
            refresh: pair.refresh.clone(),
        };
        if let Some(store) = &self.token_store {
            store.save_tokens(&pair.access, &pair.refresh);
        }
        Ok(())
    }
}
