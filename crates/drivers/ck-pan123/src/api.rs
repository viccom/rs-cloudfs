//! 123 云盘 web API 面（Phase 6 / 123-1 骨架 + 123-2 读路径扩展 + 123-3
//! 写路径端点）——域名管理（dydomain 动态发现 + 会话级粘性 fallback）、
//! web 身份头集合（D5）、envelope 解析/错误分类、令牌桶限流 + HTTP
//! 重试/退避、读写路径端点封装。
//!
//! ## envelope（§5.10 双成功码——123-0 真机实证）
//!
//! 统一信封 `{code, message, data}`，顶层键双拼（`code/Code`、
//! `message/Message`、`data/Data`）。**双成功码**：`code==0` 通用成功；
//! `code==200` 仅认证类成功（sign_in 与 QR 确认态——唯一非 0 成功码）。
//! [`Envelope::is_ok`] 只认 0；认证面（[`Envelope::is_auth_ok`]）加认
//! 200。非 JSON 响应（HTML 错误页/空体）→ `Unavailable`，serde 不崩穿。
//!
//! ## 错误分类表（R2；123-1 认证面 + 123-2 读写面扩充）
//!
//! | code | 语义 | 分类 | 终态映射 |
//! |---|---|---|---|
//! | `20101` | 未登录（list 面） | [`ErrKind::NotLoggedIn`] | `Unauthorized{recoverable:false}`（web API 无 refresh，K76.4——重扫码可行动指引经 warn 通道） |
//! | `401` | 未登录（user 面，"cookie token is empty"） | [`ErrKind::NotLoggedIn`] | 同上 |
//! | `5060` | 同名文件/目录冲突（mkdir/upload 面，data{etag,size,updated_at}） | [`ErrKind::NameConflict`] | `Exists` |
//! | `5113` / `5114` | 每日下载流量限额（D5：不绕过） | [`ErrKind::TrafficExceeded`] | `RateLimited{retry_after:None}` + warn 人话指引（会员消解/次日恢复） |
//! | `-1` | rpc 形态失败（MalformedXML / ListParts NoSuchKey 采样） | [`ErrKind::RpcFailure`] | `Io` 保留原码与消息 |
//! | `400` | 参数类校验失败（"The Fids field is required" / "请输入Etag" 采样） | [`ErrKind::BadParams`] | `Invalid`（warn 通道保留后端消息） |
//! | 其他 | 未知 | [`ErrKind::Rejected`] | `Unavailable` 载荷保留原码与消息（R2 可诊断） |
//!
//! ## 限流与重试（§5.15；任务 G）
//!
//! - **令牌桶**（[`crate::limiter`]）：缺省保守 ~2 rps（真值未测挂账），
//!   dispatch 前统一过门；
//! - **HTTP 重试**：`Retry-After` 头优先（clamp 1–60s）→ 指数退避封顶
//!   30s + 抖动；限流类（HTTP 429）重试 ≤6、普通错误（传输/5xx）≤3；
//!   envelope 终态错误（`Unauthorized`/`RateLimited`/映射表错误）与
//!   非 JSON 体**恒不重试**；
//! - **域名粘性 fallback**（§5.17）：主域连接错误 → 备域重放恰一次
//!   （会话级粘性，不回切）。
//!
//! ## 域名管理（§5.17）
//!
//! 启动期 `GET /api/dydomain` 解析现行主域（响应 `data.domains[]`，
//! 真机回 `["www.123pan.cn"]`）；任何失败回退缺省主域
//! [`DEFAULT_PRIMARY_BASE`]。主域**连接错误**后会话级切备域
//! [`DEFAULT_FALLBACK_BASE`]（`api.123278.com`）且**不回切**（粘性——
//! 防振荡）。
//!
//! ## 传输会话分离（§5.12；123-2）
//!
//! CDN GET 走 [`Pan123Client::transfer_http`] 的**裸 client**（仅 UA，
//! 无 123pan 鉴权头/Cookie；重定向手动跟——三跳封顶的执行面）。
//!
//! ## R3（凭据不入载荷/日志）
//!
//! 错误文本只拼 stage/code/message——绝不携带 data；reqwest 错误
//! `without_url()` 剥离内嵌 URL；非 JSON 体的截断片段经当前 token
//! 掩码（ck-pan115 dispatch 同款三防线）。

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use cloudkit_storage::StorageError;

use crate::limiter::{LimiterConfig, RateLimiter};
use crate::models::{FileEntry, TrafficStatus, UserInfo};

/// 浏览器 UA（D5 web 身份；spike `api.rs USER_AGENT` 同值——真机验证
/// 形态。**无安卓头**：platform:android/设备指纹族经 D5 裁决不采纳）。
pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36 Edg/146.0.0.0";

/// 现行主域（123panNextGen 2026-08-29+ 真相 + dydomain 真机回值）。
pub const DEFAULT_PRIMARY_BASE: &str = "https://www.123pan.cn";
/// 备域（pan123-rs 时代主域；连接错误后的粘性 fallback，§5.17）。
pub const DEFAULT_FALLBACK_BASE: &str = "https://api.123278.com";

/// 凭据掩码（ck-pan115 api.rs 同形）：前 6 + 后 4；短值整体隐去。
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

// ---------------------------------------------------------------------------
// envelope
// ---------------------------------------------------------------------------

/// 统一响应信封（顶层键双拼：`code/Code`、`message/Message`、
/// `data/Data`——123-0 真机实证的代际混拼形态）。
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    #[serde(default, alias = "Code")]
    pub code: i64,
    #[serde(default, alias = "Message")]
    pub message: String,
    #[serde(default, alias = "Data")]
    pub data: Value,
}

impl Envelope {
    /// 通用成功 = `code == 0`（业务面判据）。
    pub fn is_ok(&self) -> bool {
        self.code == 0
    }

    /// 认证面成功 = `code == 0 || code == 200`（200 仅 sign_in 与 QR
    /// 确认态——§5.10 唯一非 0 成功码）。
    pub fn is_auth_ok(&self) -> bool {
        self.code == 0 || self.code == 200
    }

    /// 业务面：成功 → `data`；错误 → 映射表终态。
    pub(crate) fn ok(self, stage: &'static str) -> Result<Value, StorageError> {
        if self.is_ok() {
            Ok(self.data)
        } else {
            Err(self.to_storage_error(stage))
        }
    }

    /// 认证面：成功（含 200）→ `data`。
    pub(crate) fn auth_ok(self, stage: &'static str) -> Result<Value, StorageError> {
        if self.is_auth_ok() {
            Ok(self.data)
        } else {
            Err(self.to_storage_error(stage))
        }
    }

    /// 错误信封 → StorageError（未登录/流量限额的可行动文案经 warn 通道
    /// ——`Unauthorized`/`RateLimited{None}` 无载荷，ck-pan115 api.rs 的
    /// 双通道裁决）。
    pub(crate) fn to_storage_error(&self, stage: &'static str) -> StorageError {
        match classify(self.code) {
            ErrKind::NotLoggedIn => tracing::warn!(
                target: "ck_pan123::api",
                stage,
                code = self.code,
                "123pan session invalid (web API has no refresh): re-run the setup QR scan \
                 (or paste a fresh token) to re-authorize"
            ),
            ErrKind::TrafficExceeded => tracing::warn!(
                target: "ck_pan123::api",
                stage,
                code = self.code,
                "123pan daily download traffic quota exceeded (D5: not bypassed): traffic \
                 resets tomorrow, or a 123pan VIP subscription lifts the cap"
            ),
            ErrKind::BadParams => tracing::warn!(
                target: "ck_pan123::api",
                stage,
                code = self.code,
                message = %self.message,
                "123pan rejected the request parameters (code 400)"
            ),
            _ => {}
        }
        let mapped = map_rejection(self.code, &self.message);
        if let StorageError::Unavailable(detail) = &mapped {
            return StorageError::Unavailable(format!("{stage}: {detail}"));
        }
        mapped
    }
}

/// 响应体 → 信封（非 JSON → `Unavailable`，载荷不回显 body——错误页
/// 可能回显请求头片段；携带 HTTP 状态与长度供诊断）。
pub(crate) async fn read_envelope(
    resp: reqwest::Response,
    stage: &'static str,
) -> Result<(u16, Envelope), StorageError> {
    let http = resp.status().as_u16();
    let body = resp.text().await.map_err(|e| {
        StorageError::Unavailable(format!("{stage} body read: {}", e.without_url()))
    })?;
    let env: Envelope = serde_json::from_str(&body).map_err(|_| {
        StorageError::Unavailable(format!(
            "{stage} non-envelope (http {http}, len {})",
            body.len()
        ))
    })?;
    Ok((http, env))
}

// ---------------------------------------------------------------------------
// 错误分类（认证面 123-1 + 读写面 123-2 扩充）
// ---------------------------------------------------------------------------

/// 业务错误的归一枚举（分派依据见模块文档映射表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    /// `20101`（list 面）/ `401`（user 面）：未登录——web API 无
    /// refresh（K76.4），重扫码是唯一出路。
    NotLoggedIn,
    /// `5060`：同名冲突（mkdir/upload 面；data 带 etag/size/updated_at）。
    NameConflict,
    /// `5113` / `5114`：每日下载流量限额（D5：不绕过——人话指引经 warn）。
    TrafficExceeded,
    /// `-1`：rpc 形态失败（写路径采样 MalformedXML / ListParts
    /// NoSuchKey；Io 保留原文）。
    RpcFailure,
    /// `400`：参数类校验失败（采样："The Fids field is required" /
    /// "请输入Etag"）。
    BadParams,
    /// 未知码（终态 `Unavailable` 保留原码与消息）。
    Rejected,
}

/// code 命中即分类（真机采样：未登录码按端点族分叉——list 族 20101、
/// user 族 401；5060/5113/5114/-1/400 为 123-0 两腿采样入表）。
pub fn classify(code: i64) -> ErrKind {
    match code {
        20101 | 401 => ErrKind::NotLoggedIn,
        5060 => ErrKind::NameConflict,
        5113 | 5114 => ErrKind::TrafficExceeded,
        -1 => ErrKind::RpcFailure,
        400 => ErrKind::BadParams,
        _ => ErrKind::Rejected,
    }
}

/// 分类 → StorageError 终态。未知码保留原码与后端消息（R2）。
pub fn map_rejection(code: i64, message: &str) -> StorageError {
    match classify(code) {
        // web API 无 refresh（K76.4）——不可恢复；重扫码是唯一出路。
        ErrKind::NotLoggedIn => StorageError::Unauthorized { recoverable: false },
        // 同名冲突（调用方预检竞争窗口的兜底；mkdir 面常态形态）。
        ErrKind::NameConflict => StorageError::Exists,
        // D5：流量限额不绕过——人话指引走 warn 通道（见 to_storage_error）。
        ErrKind::TrafficExceeded => StorageError::RateLimited { retry_after: None },
        // rpc 形态失败：Io 保留原码与消息（可诊断；写路径采样形态）。
        ErrKind::RpcFailure => StorageError::Io(format!("pan123 code={code}: {message}")),
        // 参数类：Invalid 无载荷——后端消息经 warn 保留（R2 双通道）。
        ErrKind::BadParams => StorageError::Invalid,
        ErrKind::Rejected => StorageError::Unavailable(format!("pan123 code={code}: {message}")),
    }
}

// ---------------------------------------------------------------------------
// 域名管理（§5.17）
// ---------------------------------------------------------------------------

/// 会话级域名计划：dydomain 解析出的主域 + 粘性备域。
///
/// `Primary → Fallback` 单向（连接错误触发；不回切——防主域闪断时
/// 两域间振荡）。读锁短临界区、无 await 跨持——std RwLock 即足。
pub(crate) struct DomainState {
    fallback: String,
    plan: std::sync::RwLock<DomainPlan>,
}

struct DomainPlan {
    /// dydomain 是否已跑过（一次性；失败也标记——维持缺省）。
    resolved: bool,
    on_fallback: bool,
    primary: String,
}

impl DomainState {
    pub(crate) fn new(primary: String, fallback: String) -> Self {
        DomainState {
            fallback,
            plan: std::sync::RwLock::new(DomainPlan {
                resolved: false,
                on_fallback: false,
                primary,
            }),
        }
    }

    /// 当前生效 base（粘性：切备域后恒备域）。
    pub(crate) fn active(&self) -> String {
        let plan = self.plan.read().unwrap();
        if plan.on_fallback {
            self.fallback.clone()
        } else {
            plan.primary.clone()
        }
    }

    /// 缺省主域（dydomain 探测的目标——探测恒打主域）。
    pub(crate) fn default_primary(&self) -> String {
        self.plan.read().unwrap().primary.clone()
    }

    /// dydomain 解析产物落位（仅未 failover 时有意义——bootstrap 先于
    /// 任何请求，failover 不可能先行）。
    pub(crate) fn set_resolved_primary(&self, primary: String) {
        let mut plan = self.plan.write().unwrap();
        plan.primary = primary;
        plan.resolved = true;
    }

    /// dydomain 失败：标记已解析、维持缺省主域。
    pub(crate) fn mark_resolved_keep_default(&self) {
        self.plan.write().unwrap().resolved = true;
    }

    pub(crate) fn is_resolved(&self) -> bool {
        self.plan.read().unwrap().resolved
    }

    /// 主域 → 备域（粘性单向）。已备域时返回 `false`（调用方不再重试）。
    pub(crate) fn fail_over(&self) -> bool {
        let mut plan = self.plan.write().unwrap();
        if plan.on_fallback {
            return false;
        }
        plan.on_fallback = true;
        true
    }
}

/// 启动期动态域名发现：`GET {primary}/api/dydomain` →
/// `data.domains[0]`（真机回 `["www.123pan.cn"]` + `ucenterDomain`）。
///
/// 任何失败（传输/非 JSON/`code!=0`/空列表）→ `None`——调用方维持
/// 缺省主域（123-0 实证三域同答，探测主域即可）。
pub async fn resolve_domain(http: &reqwest::Client, primary_base: &str) -> Option<String> {
    let resp = http
        .get(format!("{primary_base}/api/dydomain"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    let (_http, env) = read_envelope(resp, "dydomain").await.ok()?;
    if !env.is_ok() {
        return None;
    }
    let domain = env.data.get("domains")?.as_array()?.first()?.as_str()?;
    if domain.is_empty() {
        return None;
    }
    // 真机回裸主机名（`www.123pan.cn`）→ 补 https；已带 scheme 的
    // 容忍原样（测试注入 loopback mock 的形态）。
    Some(match domain {
        d if d.starts_with("http://") || d.starts_with("https://") => d.to_string(),
        d => format!("https://{d}"),
    })
}

// ---------------------------------------------------------------------------
// client + dispatch
// ---------------------------------------------------------------------------

/// loginuuid = md5(uuid v4) 的 hex（pan123-rs 形态；会话内稳定——
/// 真机 spike 同值复用验证通过）。
pub fn new_login_uuid() -> String {
    use md5::{Digest, Md5};
    let digest = Md5::digest(uuid::Uuid::new_v4().to_string().as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// web 身份 HTTP client（D5）：浏览器 UA + `platform: web` +
/// `app-version: 3` + `Origin/Referer: yun.123pan.cn` + `loginuuid`
/// 头；**无安卓头、无签名参数**（123-0 真机实证：无签名即通）。
/// IPv4 dial（§5.9）+ 直连（no_proxy——国内直连实证可达）+ gzip
/// 透明解压 + 10s 连接/60s 请求超时。
pub fn web_http_client(login_uuid: &str) -> Result<reqwest::Client, StorageError> {
    use reqwest::header::{
        HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, ORIGIN, REFERER, USER_AGENT,
    };
    let mut headers = HeaderMap::new();
    let ins = |headers: &mut HeaderMap, name: &'static str, value: &str| {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    };
    headers.insert(USER_AGENT, HeaderValue::from_str(UA).expect("static UA"));
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/plain, */*"),
    );
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8"),
    );
    headers.insert(ORIGIN, HeaderValue::from_static("https://yun.123pan.cn"));
    headers.insert(REFERER, HeaderValue::from_static("https://yun.123pan.cn/"));
    ins(&mut headers, "app-version", "3");
    ins(&mut headers, "platform", "web");
    ins(&mut headers, "loginuuid", login_uuid);
    reqwest::Client::builder()
        .no_proxy()
        .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| StorageError::Io(format!("pan123 http client build: {e}")))
}

/// HTTP 重试/退避参数（§5.15 实测常量；[`RetryConfig::fast`] 毫秒级
/// 注入供桩回放）。
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    /// 限流类（HTTP 429）重试上限（§5.15 实测：6）。
    pub limited_max: u32,
    /// 普通错误（传输/5xx）重试上限（§5.15 实测：3）。
    pub ordinary_max: u32,
    /// 指数退避基数（2^n × base，封顶 cap）。
    pub backoff_base: Duration,
    /// 指数退避封顶（§5.15：30s）。
    pub backoff_cap: Duration,
    /// `Retry-After` 头的 clamp 下界（§5.15：1s——过小的服务端值不追）。
    pub retry_after_min: Duration,
    /// `Retry-After` 头的 clamp 上界（§5.15：60s）。
    pub retry_after_max: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        RetryConfig {
            limited_max: 6,
            ordinary_max: 3,
            backoff_base: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(30),
            retry_after_min: Duration::from_secs(1),
            retry_after_max: Duration::from_secs(60),
        }
    }
}

impl RetryConfig {
    /// 毫秒级窗口（测试专用，绝不用于生产）。
    pub fn fast() -> Self {
        RetryConfig {
            limited_max: 6,
            ordinary_max: 3,
            backoff_base: Duration::from_millis(5),
            backoff_cap: Duration::from_millis(20),
            retry_after_min: Duration::from_millis(1),
            retry_after_max: Duration::from_millis(50),
        }
    }

    /// 第 n 次重试的退避时长：指数（base × 2^n）封顶 cap + 抖动
    /// （0..=base/2，时钟纳秒源——不引 rand 依赖）。
    pub(crate) fn backoff(&self, attempt: u32) -> Duration {
        let exp = self.backoff_base.saturating_mul(1u32 << attempt.min(16));
        let capped = exp.min(self.backoff_cap);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let jitter = Duration::from_nanos((nanos % 500) as u64);
        capped + jitter
    }
}

/// 123pan web API 客户端（token 状态 + 域名管理 + 令牌桶限流 + HTTP
/// 重试/退避 + envelope dispatch）。
///
/// 认证头 = **Bearer 单头即足**（123-0 真机实证：cookie-only 才触发
/// 20101/401 未登录；Cookie sso-token 不发）。无 refresh 状态机——
/// web API 无 refresh 机制（K76.4），token 失效即 `Unauthorized`
/// 终态（**恒不重试**）。
pub struct Pan123Client {
    http: reqwest::Client,
    /// CDN/镜像域 GET 的裸会话（§5.12 双会话分离：仅 UA，无 123pan
    /// 鉴权头；重定向手动跟——三跳封顶的执行面）。
    transfer: reqwest::Client,
    domains: DomainState,
    token: tokio::sync::RwLock<String>,
    bootstrap: tokio::sync::Mutex<()>,
    /// 全局令牌桶（每卷一个；dispatch 前统一过门）。
    limiter: Arc<RateLimiter>,
    retry: RetryConfig,
}

impl Pan123Client {
    /// 构造（生产缺省限流/重试参数；不连网——dydomain 在首个请求前
    /// 惰性解析一次；ck-pan115 `Pan115Client::new` 同款骨架形态）。
    pub fn new(
        token: String,
        primary_base: String,
        fallback_base: String,
        login_uuid: Option<String>,
    ) -> Result<Self, StorageError> {
        Self::with_tuning(
            token,
            primary_base,
            fallback_base,
            login_uuid,
            LimiterConfig::default(),
            RetryConfig::default(),
        )
    }

    /// 可调参构造（测试注入毫秒级限流/退避；RebuildTuning 先例——结构
    /// 注入而非时钟替身）。
    pub fn with_tuning(
        token: String,
        primary_base: String,
        fallback_base: String,
        login_uuid: Option<String>,
        limiter: LimiterConfig,
        retry: RetryConfig,
    ) -> Result<Self, StorageError> {
        let login_uuid = login_uuid.unwrap_or_else(new_login_uuid);
        Ok(Pan123Client {
            http: web_http_client(&login_uuid)?,
            transfer: transfer_http_client()?,
            domains: DomainState::new(primary_base, fallback_base),
            token: tokio::sync::RwLock::new(token),
            bootstrap: tokio::sync::Mutex::new(()),
            limiter: Arc::new(RateLimiter::new(limiter)),
            retry,
        })
    }

    /// 传输会话（download.rs 的 CDN 窗口 GET 面；§5.12——裸 client
    /// 仅 UA，无 123pan 鉴权头/Cookie，重定向手动跟）。公开只读访问
    /// 面（桩测试的双会话分离断言）。
    pub fn transfer_http(&self) -> &reqwest::Client {
        &self.transfer
    }

    /// GET dispatch：Bearer + query 对 → `data`。
    pub async fn dispatch_get(
        &self,
        path: &str,
        query: &[(&str, &str)],
        stage: &'static str,
    ) -> Result<Value, StorageError> {
        self.dispatch(path, false, query, None, stage).await
    }

    /// POST dispatch：Bearer + JSON body → `data`。
    pub async fn dispatch_post_json(
        &self,
        path: &str,
        body: &Value,
        stage: &'static str,
    ) -> Result<Value, StorageError> {
        self.dispatch(path, true, &[], Some(body), stage).await
    }

    /// `GET /b/api/user/info` → uid/空间/流量字段（VolumeId 的 uid 真源
    /// 与 quota 面的地基）。
    pub async fn user_info(&self) -> Result<UserInfo, StorageError> {
        let data = self
            .dispatch_get("/b/api/user/info", &[], "user/info")
            .await?;
        serde_json::from_value(data)
            .map_err(|e| StorageError::Unavailable(format!("user/info data parse: {e}")))
    }

    /// 统一请求策略引擎（模块文档「限流与重试」节）：
    ///
    /// 1. 每次尝试前过令牌桶（节拍等待在锁外）；
    /// 2. 单次尝试内部含 §5.17 域名 failover（主域连接错误 → 粘性切
    ///    备域，同一请求重放恰一次；`fail_over` 只成功一次，有界）；
    /// 3. 传输错误/HTTP 429/5xx 按退避重试（限流类 ≤6、普通 ≤3）；
    /// 4. envelope 终态错误与非 JSON 体恒不重试（Unauthorized 更是
    ///    契约级禁重试——K76.4）。
    async fn dispatch(
        &self,
        path: &str,
        post: bool,
        query: &[(&str, &str)],
        body: Option<&Value>,
        stage: &'static str,
    ) -> Result<Value, StorageError> {
        self.ensure_bootstrapped().await;
        let mut limited_retries = 0u32;
        let mut ordinary_retries = 0u32;
        loop {
            self.limiter.check_wait().await;
            match self.attempt_once(path, post, query, body, stage).await {
                AttemptOutcome::Done(result) => return result,
                AttemptOutcome::Transport(err) => {
                    if ordinary_retries >= self.retry.ordinary_max {
                        return Err(err);
                    }
                    let delay = self.retry.backoff(ordinary_retries);
                    ordinary_retries += 1;
                    tracing::debug!(
                        target: "ck_pan123::api",
                        stage,
                        attempt = ordinary_retries,
                        ?delay,
                        "transport failure: backing off before the retry"
                    );
                    tokio::time::sleep(delay).await;
                }
                AttemptOutcome::HttpRetry {
                    status,
                    retry_after,
                } => {
                    let is_limited = status == 429;
                    let budget = if is_limited {
                        &mut limited_retries
                    } else {
                        &mut ordinary_retries
                    };
                    if *budget
                        >= (if is_limited {
                            self.retry.limited_max
                        } else {
                            self.retry.ordinary_max
                        })
                    {
                        return Err(if is_limited {
                            StorageError::RateLimited { retry_after }
                        } else {
                            StorageError::Unavailable(format!("{stage}: HTTP {status} persisted"))
                        });
                    }
                    let delay = match retry_after {
                        // Retry-After 头优先（clamp 1–60s；§5.15）。
                        Some(ra) => {
                            ra.clamp(self.retry.retry_after_min, self.retry.retry_after_max)
                        }
                        None => self.retry.backoff(*budget),
                    };
                    *budget += 1;
                    tracing::debug!(
                        target: "ck_pan123::api",
                        stage,
                        status,
                        ?delay,
                        "retryable HTTP status: backing off"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// 单次尝试（含域名 failover 重放恰一次）；产出可重试分类。
    async fn attempt_once(
        &self,
        path: &str,
        post: bool,
        query: &[(&str, &str)],
        body: Option<&Value>,
        stage: &'static str,
    ) -> AttemptOutcome {
        loop {
            let url = format!("{}{path}", self.domains.active());
            let token = self.token.read().await.clone();
            let mut request = if post {
                self.http.post(&url).json(&body)
            } else {
                self.http.get(&url).query(&query)
            };
            request = request.bearer_auth(&token);
            let resp = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    // §5.17：主域连接错误 → 会话级粘性切备域，同一请求
                    // 重放恰一次。其余传输错误（超时等）不触发切换。
                    if e.is_connect() && self.domains.fail_over() {
                        tracing::warn!(
                            target: "ck_pan123::api",
                            stage,
                            "primary domain unreachable: sticky fail-over to the backup domain \
                             for this session"
                        );
                        continue;
                    }
                    return AttemptOutcome::Transport(StorageError::Unavailable(format!(
                        "{stage} transport: {}",
                        e.without_url()
                    )));
                }
            };
            let http = resp.status().as_u16();
            // 可重试 HTTP 形态：429（限流类）与 5xx（普通类）——envelope
            // 文化下错误也可能在 body 的 code 里，那些走终态映射不重试。
            if http == 429 || (500..=599).contains(&http) {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .map(Duration::from_secs);
                return AttemptOutcome::HttpRetry {
                    status: http,
                    retry_after,
                };
            }
            let body_text = match resp.text().await {
                Ok(text) => text,
                Err(e) => {
                    return AttemptOutcome::Transport(StorageError::Unavailable(format!(
                        "{stage} body read: {}",
                        e.without_url()
                    )))
                }
            };
            let env: Envelope = match serde_json::from_str(&body_text) {
                Ok(env) => env,
                Err(_) => {
                    // 非 JSON（HTML 错误页/空体）：截断 + 掩码当前 token
                    // （错误页可能回显请求头；R3）——终态，不重试。
                    let snippet: String = body_text.chars().take(200).collect();
                    let masked = snippet.replace(token.as_str(), &mask(&token));
                    return AttemptOutcome::Done(Err(StorageError::Unavailable(format!(
                        "{stage} non-json (http {http}): {masked}"
                    ))));
                }
            };
            return if env.is_ok() {
                AttemptOutcome::Done(Ok(env.data))
            } else {
                AttemptOutcome::Done(Err(env.to_storage_error(stage)))
            };
        }
    }

    /// dydomain 一次性解析（单飞；失败维持缺省主域——构造不连网、
    /// 发现失败不阻塞服务）。
    async fn ensure_bootstrapped(&self) {
        let _guard = self.bootstrap.lock().await;
        if self.domains.is_resolved() {
            return;
        }
        let probe_target = self.domains.default_primary();
        match resolve_domain(&self.http, &probe_target).await {
            Some(primary) => self.domains.set_resolved_primary(primary),
            None => {
                tracing::debug!(
                    target: "ck_pan123::api",
                    "dydomain resolution unavailable: keeping the default primary"
                );
                self.domains.mark_resolved_keep_default();
            }
        }
    }

    // ------------------------------------------------- 读路径端点（123-2） ---

    /// `GET /api/file/list/new` 一页（新代际无前缀形态——123-0 真机实证
    /// 活）→ `(rows, total)`。
    ///
    /// 分页 = `Page`（1 基页码）+ `limit`（服务端排序仅 file_id
    /// asc/desc——**跨页合并后的稳定排序在驱动层自理**，任务 A）。
    pub async fn list_page(
        &self,
        parent_file_id: i64,
        page: u32,
        limit: u32,
    ) -> Result<(Vec<FileEntry>, i64), StorageError> {
        let data = self
            .dispatch_get(
                "/api/file/list/new",
                &[
                    ("driveId", "0"),
                    ("limit", &limit.to_string()),
                    ("next", "0"),
                    ("orderBy", "file_id"),
                    ("orderDirection", "desc"),
                    ("parentFileId", &parent_file_id.to_string()),
                    ("trashed", "false"),
                    ("SearchData", ""),
                    ("Page", &page.to_string()),
                    ("OnlyLookAbnormalFile", "0"),
                ],
                "file/list/new",
            )
            .await?;
        // 双拼：list 真机形态大写 InfoList（info 端点是小写 infoList——
        // §5.10；tolerant 手取两形态）。
        let rows_value = data
            .get("InfoList")
            .or_else(|| data.get("infoList"))
            .cloned()
            .unwrap_or(Value::Null);
        let rows: Vec<FileEntry> = serde_json::from_value(rows_value)
            .map_err(|e| StorageError::Unavailable(format!("file/list/new rows parse: {e}")))?;
        let total = data
            .get("Total")
            .or_else(|| data.get("total"))
            .and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(-1);
        Ok((rows, total))
    }

    /// `POST /b/api/file/info`（fileIdList 查询；响应键小写 `infoList`——
    /// spike 实证）→ 条目（不存在 → `None`；形态异常 → 错误）。
    pub async fn file_info(&self, file_id: i64) -> Result<Option<FileEntry>, StorageError> {
        let data = self
            .dispatch_post_json(
                "/b/api/file/info",
                &serde_json::json!({"fileIdList": [{"fileId": file_id}]}),
                "file/info",
            )
            .await?;
        let list = data
            .get("infoList")
            .or_else(|| data.get("InfoList"))
            .and_then(|v| v.as_array().cloned())
            .ok_or_else(|| StorageError::Unavailable("file/info: infoList missing".into()))?;
        match list.into_iter().next() {
            None => Ok(None),
            Some(v) => serde_json::from_value(v)
                .map(Some)
                .map_err(|e| StorageError::Unavailable(format!("file/info row parse: {e}"))),
        }
    }

    /// `POST /a/api/file/upload_request`（type=1 目录创建——**注意与文件
    /// 上传的 `/b/` 前缀分叉**，spike 实证）→ 新目录 FileId（在
    /// `data.Info`）。
    ///
    /// 5060 同名冲突 → `Exists`（errno 表）；`NotReuse:true` 与 etag:""
    /// 是 spike 真机验证过的请求形态（§5.11 逐端点保真）。
    pub async fn mkdir(&self, parent_file_id: i64, name: &str) -> Result<i64, StorageError> {
        let data = self
            .dispatch_post_json(
                "/a/api/file/upload_request",
                &serde_json::json!({
                    "driveId": 0,
                    "etag": "",
                    "fileName": name,
                    "parentFileId": parent_file_id,
                    "size": 0,
                    "type": 1,
                    "NotReuse": true,
                }),
                "mkdir",
            )
            .await?;
        let fid = data
            .get("Info")
            .and_then(|i| i.get("FileId").or_else(|| i.get("fileId")))
            .or_else(|| data.get("FileId").or_else(|| data.get("fileId")))
            .and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .filter(|v| *v > 0);
        fid.ok_or_else(|| StorageError::Unavailable("mkdir: FileId missing from data.Info".into()))
    }

    /// `POST /a/api/file/trash`——载荷**恰为**最小保真形态（§5.11：
    /// `fileTrashInfoList` 大写 `FileId` + `event:"intoRecycle"`；多余键
    /// 曾是静默失败陷阱的形态面——最小载荷 + 调用方回读校验维持纵深）。
    pub async fn trash(&self, file_id: i64) -> Result<(), StorageError> {
        self.dispatch_post_json(
            "/a/api/file/trash",
            &serde_json::json!({
                "fileTrashInfoList": [{"FileId": file_id}],
                "event": "intoRecycle",
            }),
            "file/trash",
        )
        .await
        .map(|_| ())
    }

    /// `POST /a/api/file/rename`（`fileId` + 新名）。**目录同样可用**
    /// （123-2 任务 0 真机实证：同 FileId、Type 保持、list 回读新名）。
    pub async fn rename(&self, file_id: i64, new_name: &str) -> Result<(), StorageError> {
        self.dispatch_post_json(
            "/a/api/file/rename",
            &serde_json::json!({
                "driveId": 0,
                "fileId": file_id,
                "fileName": new_name,
            }),
            "file/rename",
        )
        .await
        .map(|_| ())
    }

    /// `POST /b/api/file/mod_pid`——跨父移动（pan123-rs wire 形态：
    /// `fileIdList` 大写 `FileId` + `parentFileId` + `event:"fileMove"`；
    /// §5.11 键名保真）。
    pub async fn mod_pid(&self, file_id: i64, target_parent: i64) -> Result<(), StorageError> {
        self.dispatch_post_json(
            "/b/api/file/mod_pid",
            &serde_json::json!({
                "fileIdList": [{"FileId": file_id}],
                "parentFileId": target_parent,
                "event": "fileMove",
                "operatePlace": "bottom",
                "RequestSource": serde_json::Value::Null,
            }),
            "file/mod_pid",
        )
        .await
        .map(|_| ())
    }

    /// `POST /b/api/file/download/traffic/check`（载荷 `fids`）→ 流量
    /// 状态（§5.7 预检：`isTrafficExceeded` 才是阻断依据——`isBlocked`
    /// 含义未明不作为阻断，跟踪单挂账）。
    pub async fn traffic_check(&self, fids: &[i64]) -> Result<TrafficStatus, StorageError> {
        let data = self
            .dispatch_post_json(
                "/b/api/file/download/traffic/check",
                &serde_json::json!({"fids": fids}),
                "traffic/check",
            )
            .await?;
        serde_json::from_value(data)
            .map_err(|e| StorageError::Unavailable(format!("traffic/check parse: {e}")))
    }

    /// `POST /a/api/file/download_info`（新形态——spike 实证响应
    /// `data.DownloadUrl` = web-pro2 中继 URL）。
    ///
    /// 载荷携带条目元数据（etag/s3keyFlag/type/size——spike 验证形态）。
    /// 5113/5114（流量限额）→ `RateLimited`（errno 表，D5）。
    pub async fn download_info(&self, entry: &FileEntry) -> Result<String, StorageError> {
        let data = self
            .dispatch_post_json(
                "/a/api/file/download_info",
                &serde_json::json!({
                    "driveId": 0,
                    "etag": entry.etag,
                    "fileId": entry.file_id,
                    "s3keyFlag": entry.s3_key_flag,
                    "type": entry.entry_type,
                    "fileName": entry.file_name,
                    "size": entry.size,
                }),
                "download_info",
            )
            .await?;
        let url = data
            .get("DownloadUrl")
            .or_else(|| data.get("downloadUrl"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                StorageError::Unavailable("download_info: DownloadUrl missing".into())
            })?;
        Ok(url.to_string())
    }

    // ------------------------------------------------- 写路径端点（123-3） ---

    /// `POST /b/api/file/upload_request`——文件面（type=0；**与 mkdir 的
    /// `/a/` 前缀分叉**，§3.1）。首请求不带 `duplicate`；5060 由调用方
    /// （stager）以 `duplicate:2` 重发消化——**writer 面的覆盖语义恒用
    /// 2，绝不发 1**（D4 真机钉死：2=同 FileId 原地覆盖 / 1=`name(1).ext`
    /// 副本）。
    ///
    /// `Reuse:true`（内容已在服务端——**Reuse 优先于 5060**）时真 FileId
    /// 在 `data.Info.FileId`（顶层 FileId 是临时形态大数、`UploadId:""`）。
    pub async fn upload_request_file(
        &self,
        parent: i64,
        name: &str,
        size: u64,
        etag: &str,
        duplicate: Option<i64>,
    ) -> Result<UploadRequestOutcome, StorageError> {
        let mut body = serde_json::json!({
            "driveId": 0,
            "etag": etag,
            "fileName": name,
            "parentFileId": parent,
            "size": size,
            "type": 0,
        });
        if let Some(d) = duplicate {
            body["duplicate"] = serde_json::json!(d);
        }
        let data = match self
            .dispatch_post_json("/b/api/file/upload_request", &body, "upload_request")
            .await
        {
            Ok(data) => data,
            // 5060 → errno 表归一 Exists；writer 面内部消化（stager 重发
            // duplicate=2），绝不外泄到挂载面。
            Err(StorageError::Exists) => return Ok(UploadRequestOutcome::Conflict),
            Err(e) => return Err(e),
        };
        let field = |keys: &[&str]| -> Option<String> {
            keys.iter().find_map(|k| data.get(*k)).and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            })
        };
        let reuse = data
            .get("Reuse")
            .or_else(|| data.get("reuse"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if reuse {
            // 真 FileId 只在 data.Info（spike 钉死 b：顶层是临时形态）。
            let fid = data
                .get("Info")
                .and_then(|i| i.get("FileId").or_else(|| i.get("fileId")))
                .and_then(|v| {
                    v.as_i64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .filter(|v| *v > 0);
            let file_id = fid.ok_or_else(|| {
                StorageError::Unavailable(
                    "upload_request: Reuse hit without a real FileId in data.Info".into(),
                )
            })?;
            return Ok(UploadRequestOutcome::Rapid { file_id });
        }
        let upload_id = field(&["UploadId", "uploadId"]).unwrap_or_default();
        if upload_id.is_empty() {
            return Err(StorageError::Unavailable(
                "upload_request: neither Reuse nor an UploadId came back".into(),
            ));
        }
        let missing = |k: &str| StorageError::Unavailable(format!("upload_request: {k} missing"));
        Ok(UploadRequestOutcome::Ticket(Box::new(UploadTicket {
            up_file_id: data
                .get("FileId")
                .or_else(|| data.get("fileId"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            bucket: field(&["Bucket", "bucket"]).ok_or_else(|| missing("Bucket"))?,
            key: field(&["Key", "key"]).ok_or_else(|| missing("Key"))?,
            upload_id,
            storage_node: field(&["StorageNode", "storageNode"])
                .ok_or_else(|| missing("StorageNode"))?,
            // 服务端 SliceSize（"16777216" 字符串形态）——上界参考，驱动
            // 按客户端 5MiB 定值切片（§5.13）。
            slice_size: data
                .get("SliceSize")
                .or_else(|| data.get("sliceSize"))
                .and_then(|v| {
                    v.as_str()
                        .and_then(|s| s.parse().ok())
                        .or_else(|| v.as_u64())
                }),
        })))
    }

    /// `POST /b/api/file/s3_list_upload_parts`——七步序第 2/5 步（续传
    /// 差集依据）。body 小写 `storageNode`（与 repare/v2 的大写分叉——
    /// 逐字照抄）；响应 `data.Parts[]`（`PartNumber`/`Size` 均字符串）。
    ///
    /// 会话被 complete 消费 → `code:-1` ListParts NoSuchKey/404 形态 →
    /// [`PartsOutcome::SessionGone`]（调用方重走 upload_request）。
    pub async fn s3_list_parts(&self, ticket: &UploadTicket) -> Result<PartsOutcome, StorageError> {
        let data = match self
            .dispatch_post_json(
                "/b/api/file/s3_list_upload_parts",
                &serde_json::json!({
                    "bucket": ticket.bucket,
                    "key": ticket.key,
                    "uploadId": ticket.upload_id,
                    "storageNode": ticket.storage_node,
                }),
                "s3_list_upload_parts",
            )
            .await
        {
            Ok(data) => data,
            Err(StorageError::Io(msg)) if session_gone(&msg) => {
                return Ok(PartsOutcome::SessionGone)
            }
            Err(e) => return Err(e),
        };
        let rows = data
            .get("Parts")
            .or_else(|| data.get("parts"))
            .and_then(|v| v.as_array().cloned())
            .ok_or_else(|| {
                StorageError::Unavailable("s3_list_upload_parts: Parts missing".into())
            })?;
        let mut parts: Vec<(u32, i64)> = rows
            .iter()
            .filter_map(|p| {
                let n = p
                    .get("PartNumber")
                    .or_else(|| p.get("partNumber"))
                    .and_then(|v| {
                        v.as_u64()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    })?;
                let size = p
                    .get("Size")
                    .or_else(|| p.get("size"))
                    .and_then(|v| {
                        v.as_i64()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    })
                    .unwrap_or(0);
                Some((n as u32, size))
            })
            .collect();
        parts.sort_unstable();
        Ok(PartsOutcome::Parts(parts))
    }

    /// `POST /b/api/file/s3_repare_upload_parts_batch`——七步序第 3 步。
    /// **官方拼写 "repare" 勿「纠正」；参数 `StorageNode` 大写；区间
    /// `[start, end)` 半开**（§5.13 逐字照抄纪律）。响应
    /// `data.presignedUrls{"1":url,...}`。
    pub async fn s3_repare_presign(
        &self,
        ticket: &UploadTicket,
        start: u32,
        end: u32,
    ) -> Result<std::collections::BTreeMap<u32, String>, StorageError> {
        let data = self
            .dispatch_post_json(
                "/b/api/file/s3_repare_upload_parts_batch",
                &serde_json::json!({
                    "bucket": ticket.bucket,
                    "key": ticket.key,
                    "partNumberStart": start,
                    "partNumberEnd": end,
                    "uploadId": ticket.upload_id,
                    "StorageNode": ticket.storage_node,
                }),
                "s3_repare_upload_parts_batch",
            )
            .await?;
        let map = data
            .get("presignedUrls")
            .or_else(|| data.get("PresignedUrls"))
            .and_then(|v| v.as_object().cloned())
            .ok_or_else(|| StorageError::Unavailable("s3_repare: presignedUrls missing".into()))?;
        let mut urls = std::collections::BTreeMap::new();
        for (k, v) in map {
            if let (Ok(n), Some(u)) = (k.parse::<u32>(), v.as_str()) {
                urls.insert(n, u.to_string());
            }
        }
        Ok(urls)
    }

    /// `POST /b/api/file/s3_complete_multipart_upload`——七步序第 6 步
    /// （小写 `storageNode` 的同 4 键形）→ `code=0 + data.Location:""`。
    ///
    /// `-1 rpc MalformedXML` 是读路径腿实证的无害先例形态（单 PUT 会话
    /// 误调时）——容忍并 warn（repare 创建的恒 multipart 会话常态回 0）。
    pub async fn s3_complete_multipart(&self, ticket: &UploadTicket) -> Result<(), StorageError> {
        match self
            .dispatch_post_json(
                "/b/api/file/s3_complete_multipart_upload",
                &serde_json::json!({
                    "bucket": ticket.bucket,
                    "key": ticket.key,
                    "uploadId": ticket.upload_id,
                    "storageNode": ticket.storage_node,
                }),
                "s3_complete_multipart_upload",
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(StorageError::Io(msg)) if msg.contains("MalformedXML") => {
                tracing::warn!(
                    target: "ck_pan123::api",
                    "s3_complete returned the known-harmless -1 MalformedXML form; continuing"
                );
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// `POST /b/api/file/upload_complete/v2`——七步序第 7 步（提交点）。
    /// **全量 body + `isMultipart:true` 恒真 + 大写 `StorageNode`**
    /// （E0–E4 完成态矩阵：repare 批量恒 multipart 会话，新单键
    /// `{fileId}` / `isMultipart:false` 对该会话 code=0 **静默不入库**）。
    /// 响应 `data.file_info`（snake_case 键，内层双拼条目）= 真实
    /// FileId + 声称 etag + 完整条目。
    pub async fn upload_complete_v2(
        &self,
        ticket: &UploadTicket,
        size: u64,
    ) -> Result<crate::models::FileEntry, StorageError> {
        let data = self
            .dispatch_post_json(
                "/b/api/file/upload_complete/v2",
                &serde_json::json!({
                    "fileId": ticket.up_file_id,
                    "bucket": ticket.bucket,
                    "fileSize": size,
                    "key": ticket.key,
                    "isMultipart": true,
                    "uploadId": ticket.upload_id,
                    "StorageNode": ticket.storage_node,
                }),
                "upload_complete/v2",
            )
            .await?;
        let info = data
            .get("file_info")
            .or_else(|| data.get("FileInfo"))
            .or_else(|| data.get("Info"))
            .filter(|v| v.is_object())
            .cloned()
            .ok_or_else(|| {
                StorageError::Unavailable(
                    "upload_complete/v2: file_info missing — the silent no-op form \
                     (the full body with isMultipart:true is mandatory on a repare \
                     multipart session)"
                        .into(),
                )
            })?;
        serde_json::from_value(info).map_err(|e| {
            StorageError::Unavailable(format!("upload_complete/v2: file_info parse: {e}"))
        })
    }
}

/// 会话失效判据（123-0 采样形态）：`-1 rpc ListParts NoSuchKey/404`——
/// 会话已被 complete 消费（或查无）。
fn session_gone(msg: &str) -> bool {
    msg.contains("NoSuchKey") || msg.contains("404")
}

/// `upload_request` 文件面的三态产出。
pub enum UploadRequestOutcome {
    /// `Reuse:true` 秒传命中（真 FileId 在 `data.Info.FileId`）——零分片
    /// 零流量，直接跳 size 校验（任务 B Reuse 分支）。
    Rapid { file_id: i64 },
    /// 全量会话五元组（bucket/key/uploadId/storageNode + 临时 up_file_id）。
    Ticket(Box<UploadTicket>),
    /// 5060 同名冲突——stager 以 `duplicate:2` 重发消化（D4 覆盖语义）。
    Conflict,
}

/// 上传会话五元组（resume 持久化的载荷；`up_file_id` 是顶层临时
/// FileId——/v2 完成体的 `fileId` 键值）。
#[derive(Debug, Clone)]
pub struct UploadTicket {
    pub up_file_id: i64,
    pub bucket: String,
    pub key: String,
    pub upload_id: String,
    pub storage_node: String,
    /// 服务端下发的 `SliceSize`（字符串形态解析；上界参考——驱动按
    /// 客户端 5MiB 定值）。
    pub slice_size: Option<u64>,
}

/// `s3_list_upload_parts` 的产出：在册分片（part_number 升序 + size）或
/// 会话失效（调用方重走 upload_request 全量重传）。
pub enum PartsOutcome {
    Parts(Vec<(u32, i64)>),
    SessionGone,
}

/// 单次尝试的可重试分类（dispatch 重试引擎的决策面）。
enum AttemptOutcome {
    /// 终态（成功或不可重试错误——envelope 映射/非 JSON 体）。
    Done(Result<Value, StorageError>),
    /// 传输层失败（连接/超时/正文读败——退避后重试）。
    Transport(StorageError),
    /// 可重试 HTTP 状态（429 限流类 / 5xx 普通类；带可选 Retry-After）。
    HttpRetry {
        status: u16,
        retry_after: Option<Duration>,
    },
}

/// 传输会话（§5.12 双会话分离）：裸 client——仅 UA，**无 123pan 鉴权
/// 头/Cookie**（伪装头对 CDN/镜像域多余且可能干扰；pan115 oss 裸面 +
/// 123panNextGen 双 Session 同源纪律）；**重定向手动跟**（`Policy::none`
/// ——三跳封顶由 download.rs 的解析器执行）。
fn transfer_http_client() -> Result<reqwest::Client, StorageError> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_str(UA).expect("static UA"),
    );
    reqwest::Client::builder()
        .no_proxy()
        .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| StorageError::Io(format!("pan123 transfer client build: {e}")))
}
