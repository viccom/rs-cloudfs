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
//! ## 请求策略（绿阶段实现；红骨架返回 Unsupported）
//!
//! - **110 刷新重放一次**：业务响应 errno=110 → oauth 刷新（新 token 对
//!   即刻回调 TokenStore 持久化）→ 原请求重放一次；重放仍 110 →
//!   `Unauthorized { recoverable: true }`；
//! - **31034 单点重试一次**（K15）：指数退避（base 保持测试友好 ≤1s）
//!   后重放一次，仍 31034 → `RateLimited { retry_after: None }`；不做
//!   全链路限速器；
//! - endpoint base URL 为**实例字段**（默认生产常量，测试注入 mock）。

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use cloudkit_storage::StorageError;

use crate::oauth::TokenStore;
use crate::BaiduParams;

/// 全请求恒定 UA（spike common.rs:16；PCS 端点行为依赖 netdisk 族 UA）。
pub(crate) const UA: &str = "netdisk;P2SP;2.2.91.136;android-android";

/// 驱动内当前 token 对（RwLock 保护；刷新成功后整体替换并回调持久化）。
#[allow(dead_code)] // 红骨架：绿阶段请求路径读取后移除
#[derive(Debug, Clone)]
pub(crate) struct TokenPair {
    pub(crate) access: String,
    pub(crate) refresh: String,
}

/// 百度 HTTP 客户端（api/stream 双 client + base URL + token 状态）。
#[allow(dead_code)] // 红骨架：绿阶段 api.rs 请求路径读取字段后移除
pub(crate) struct BaiduClient {
    /// pan/openapi API 面（redirect none + 60s 超时）。
    pub(crate) api: reqwest::Client,
    /// CDN 流面（B2 superfile2/下载；redirect none + 20s 连接超时）。
    pub(crate) stream: reqwest::Client,
    pub(crate) api_base: String,
    pub(crate) oauth_base: String,
    pub(crate) app_key: String,
    pub(crate) app_secret: String,
    pub(crate) tokens: tokio::sync::RwLock<TokenPair>,
    pub(crate) token_store: Option<Arc<dyn TokenStore>>,
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
        })
    }

    /// GET `{api_base}<path>` 追加 query 对（自动附当前 access_token），
    /// 返回解析后的 JSON 体；errno!=0 经 `api::map_errno` 归一，110/31034
    /// 按模块文档策略自救。
    #[allow(dead_code)] // 红骨架：绿阶段（oauth.rs 刷新链 + api.rs 包装）接线
    pub(crate) async fn api_get(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<serde_json::Value, StorageError> {
        let _ = (path, query);
        Err(StorageError::Unsupported)
    }

    /// POST `{api_base}<path>`：query 对（method/access_token 等）+ form
    /// 表单体（application/x-www-form-urlencoded）；策略同 [`Self::api_get`]。
    #[allow(dead_code)] // 红骨架：绿阶段（mkdir/filemanager 等写路径）接线
    pub(crate) async fn api_post_form(
        &self,
        path: &str,
        query: &[(&str, &str)],
        form: &[(&str, String)],
    ) -> Result<serde_json::Value, StorageError> {
        let _ = (path, query, form);
        Err(StorageError::Unsupported)
    }
}
