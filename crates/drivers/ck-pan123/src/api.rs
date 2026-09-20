//! 123 云盘 web API 面（Phase 6 / 123-1）——域名管理（dydomain 动态
//! 发现 + 会话级粘性 fallback）、web 身份头集合（D5）与 envelope
//! 解析/认证面错误分类。
//!
//! ## envelope（§5.10 双成功码——123-0 真机实证）
//!
//! 统一信封 `{code, message, data}`，顶层键双拼（`code/Code`、
//! `message/Message`、`data/Data`）。**双成功码**：`code==0` 通用成功；
//! `code==200` 仅认证类成功（sign_in 与 QR 确认态——唯一非 0 成功码）。
//! [`Envelope::is_ok`] 只认 0；认证面（[`Envelope::is_auth_ok`]）加认
//! 200。非 JSON 响应（HTML 错误页/空体）→ `Unavailable`，serde 不崩穿。
//!
//! ## 错误分类表（R2；本批 = 认证面，后续批扩）
//!
//! | code | 语义 | 分类 | 终态映射 |
//! |---|---|---|---|
//! | `20101` | 未登录（list 面） | [`ErrKind::NotLoggedIn`] | `Unauthorized{recoverable:false}`（web API 无 refresh，K76.4——重扫码可行动指引经 warn 通道） |
//! | `401` | 未登录（user 面，"cookie token is empty"） | [`ErrKind::NotLoggedIn`] | 同上 |
//! | 其他 | 未知 | [`ErrKind::Rejected`] | `Unavailable` 载荷保留原码与消息（R2 可诊断；5060/5113 等读写面码 123-2/3 入表） |
//!
//! ## 域名管理（§5.17）
//!
//! 启动期 `GET /api/dydomain` 解析现行主域（响应 `data.domains[]`，
//! 真机回 `["www.123pan.cn"]`）；任何失败回退缺省主域
//! [`DEFAULT_PRIMARY_BASE`]。主域**连接错误**后会话级切备域
//! [`DEFAULT_FALLBACK_BASE`]（`api.123278.com`）且**不回切**（粘性——
//! 防振荡）。
//!
//! ## R3（凭据不入载荷/日志）
//!
//! 错误文本只拼 stage/code/message——绝不携带 data；reqwest 错误
//! `without_url()` 剥离内嵌 URL；非 JSON 体的截断片段经当前 token
//! 掩码（ck-pan115 dispatch 同款三防线）。

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use cloudkit_storage::StorageError;

use crate::models::UserInfo;

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

    /// 错误信封 → StorageError（未登录的可行动文案经 warn 通道——
    /// `Unauthorized` 无载荷，ck-pan115 api.rs 的双通道裁决）。
    pub(crate) fn to_storage_error(&self, stage: &'static str) -> StorageError {
        if classify(self.code) == ErrKind::NotLoggedIn {
            tracing::warn!(
                target: "ck_pan123::api",
                stage,
                code = self.code,
                "123pan session invalid (web API has no refresh): re-run the setup QR scan \
                 (or paste a fresh token) to re-authorize"
            );
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
// 错误分类（认证面；读写面码 123-2/3 入表扩充）
// ---------------------------------------------------------------------------

/// 业务错误的归一枚举（分派依据见模块文档映射表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    /// `20101`（list 面）/ `401`（user 面）：未登录——web API 无
    /// refresh（K76.4），重扫码是唯一出路。
    NotLoggedIn,
    /// 未知码（终态 `Unavailable` 保留原码与消息）。
    Rejected,
}

/// code 命中即分类（真机采样：未登录码按端点族分叉——list 族 20101、
/// user 族 401）。
pub fn classify(code: i64) -> ErrKind {
    match code {
        20101 | 401 => ErrKind::NotLoggedIn,
        _ => ErrKind::Rejected,
    }
}

/// 分类 → StorageError 终态。未知码保留原码与后端消息（R2）。
pub fn map_rejection(code: i64, message: &str) -> StorageError {
    match classify(code) {
        // web API 无 refresh（K76.4）——不可恢复；重扫码是唯一出路。
        ErrKind::NotLoggedIn => StorageError::Unauthorized { recoverable: false },
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

/// 123pan web API 客户端（token 状态 + 域名管理 + envelope dispatch）。
///
/// 认证头 = **Bearer 单头即足**（123-0 真机实证：cookie-only 才触发
/// 20101/401 未登录；Cookie sso-token 不发）。无 refresh 状态机——
/// web API 无 refresh 机制（K76.4），token 失效即 `Unauthorized`
/// 终态。
pub struct Pan123Client {
    http: reqwest::Client,
    domains: DomainState,
    token: tokio::sync::RwLock<String>,
    bootstrap: tokio::sync::Mutex<()>,
}

impl Pan123Client {
    /// 构造（不连网——dydomain 在首个请求前惰性解析一次；
    /// ck-pan115 `Pan115Client::new` 同款骨架形态）。
    pub fn new(
        token: String,
        primary_base: String,
        fallback_base: String,
        login_uuid: Option<String>,
    ) -> Result<Self, StorageError> {
        let login_uuid = login_uuid.unwrap_or_else(new_login_uuid);
        Ok(Pan123Client {
            http: web_http_client(&login_uuid)?,
            domains: DomainState::new(primary_base, fallback_base),
            token: tokio::sync::RwLock::new(token),
            bootstrap: tokio::sync::Mutex::new(()),
        })
    }

    /// GET dispatch：Bearer + query 对 → `data`（123-2 的端点封装地基；
    /// 123-1 的认证面错误映射桩测试经它回放）。
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
    /// 与 123-2 quota 面的地基）。
    pub async fn user_info(&self) -> Result<UserInfo, StorageError> {
        let data = self
            .dispatch_get("/b/api/user/info", &[], "user/info")
            .await?;
        serde_json::from_value(data)
            .map_err(|e| StorageError::Unavailable(format!("user/info data parse: {e}")))
    }

    /// 统一请求引擎：域名 failover（连接错误 → 粘性切备域重试一次）+
    /// envelope 判据（业务面 `code==0`）+ 非 JSON 防崩穿。
    ///
    /// 循环有界：`fail_over` 只成功一次（粘性位），至多两圈——备域上
    /// 的连接错误直接上抛 `Unavailable`。
    async fn dispatch(
        &self,
        path: &str,
        post: bool,
        query: &[(&str, &str)],
        body: Option<&Value>,
        stage: &'static str,
    ) -> Result<Value, StorageError> {
        self.ensure_bootstrapped().await;
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
                    return Err(StorageError::Unavailable(format!(
                        "{stage} transport: {}",
                        e.without_url()
                    )));
                }
            };
            let http = resp.status().as_u16();
            let body_text = resp.text().await.map_err(|e| {
                StorageError::Unavailable(format!("{stage} body read: {}", e.without_url()))
            })?;
            let env: Envelope = match serde_json::from_str(&body_text) {
                Ok(env) => env,
                Err(_) => {
                    // 非 JSON（HTML 错误页/空体）：截断 + 掩码当前 token
                    // （错误页可能回显请求头；R3）。
                    let snippet: String = body_text.chars().take(200).collect();
                    let masked = snippet.replace(token.as_str(), &mask(&token));
                    return Err(StorageError::Unavailable(format!(
                        "{stage} non-json (http {http}): {masked}"
                    )));
                }
            };
            return if env.is_ok() {
                Ok(env.data)
            } else {
                Err(env.to_storage_error(stage))
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
}
