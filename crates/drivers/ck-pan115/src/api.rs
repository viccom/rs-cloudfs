//! 115 开放平台 API 面（Phase 5 / 115-1）——envelope 解析、错误分类
//! （K69.7 采样表）、dispatch 状态机与端点封装。
//!
//! ## envelope（K69.7：错误包恒 HTTP 200——状态码不可作判据）
//!
//! 统一信封 `{state, code, errno, message, data, count}`，但 `state`
//! **双形态**：proapi 携带布尔（成功 `true` / 错误 `false`，错误码在
//! `code`），passportapi 携带数字（`1`/`0`，错误码在 `errno`）——
//! [`Envelope::is_ok`] 两者都认（`state==1 || state==true` 且
//! `errno==0`）。`ufile/files` 的顶层兄弟 `count`（总行数）是分页终止
//! 规则的依据。
//!
//! ## 错误分类表（R2；K69.7 真机采样 + 桌面版形态入表）
//!
//! | 码（code 或 errno 任一命中） | 语义 | 分类 | 终态映射 |
//! |---|---|---|---|
//! | `401*` 段（40100000..=40199999）/ `99` | token 过期（40199002 QR 过期 / 40101017 未确认 / 40140123 格式错） | [`ErrKind::TokenExpired`] | dispatch 刷+重放一次后仍命中 → `Unauthorized{recoverable:true}` |
//! | `770004` | 账号级访问上限（跨端点族整账号封锁，K69.3） | [`ErrKind::AccountRateLimited`] | dispatch 上报 limiter 后 → `RateLimited{retry_after:Some(封锁窗)}` |
//! | `911` | 需人工验证（桌面版形态，未真机复现） | [`ErrKind::HumanVerify`] | `Unauthorized{recoverable:false}` + warn 可行动文案 |
//! | `430004` | 文件不存在（SDK 侧） | [`ErrKind::NotFound`] | `NotFound` |
//! | `20130827` | 限流（桌面版形态，K69.3 注记） | [`ErrKind::RateLimited`] | `RateLimited{retry_after:None}` |
//! | 其他 | 未知 | [`ErrKind::Rejected`] | `Unavailable` 载荷保留原码与消息（R2 可诊断） |
//!
//! ## dispatch 状态机（ck-baidu client.rs 同款形态）
//!
//! 1. 请求前过 [`RateLimiter::check_wait`]——770004 封锁窗内**零请求**
//!    （硬退避拦截，本地立即 `RateLimited{retry_after:Some(剩余)}`）；
//! 2. envelope 命中 `TokenExpired`（401*/99）且未刷新过 →
//!    [`Pan115Client::refresh_once`]（单飞 + on-arrival 持久化）→
//!    重放一次；
//! 3. 刷新失败 → `Unauthorized{recoverable:true}`（refresh 官方频控下
//!    的暂时性失败居多，再试通常可行；重放仍过期亦同——绝不循环）；
//! 4. 命中 `AccountRateLimited` → `report_limit` → `RateLimited`；
//! 5. 其余按映射表归一。
//!
//! ## R3（凭据不入载荷/日志）
//!
//! 错误文本只拼 stage/state/code/errno/message——**绝不携带 data**
//! （token 端点成功响应的 data 内含新凭据对）；reqwest 错误
//! `without_url()` 剥离内嵌 URL；非 JSON 体的截断片段经当前
//! access_token 掩码（baidu mask 同款三防线）。

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use cloudkit_storage::StorageError;

use crate::limiter::RateLimiter;
use crate::oauth::{self, TokenPair, TokenStore};

/// 全请求恒定 UA（K69.4 实测：UA 形态不约束——browser/spike/空 UA 三态
/// 全通，选固定常量即可，无需伪造浏览器；取 spike 已被 WAF 验证的值）。
pub const UA: &str = "cydrive-pan115/0.1";

/// 凭据掩码（baidu client.rs 同形）：前 6 + 后 4；短值整体隐去。
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

/// 统一响应信封（双形态 `state`；`get.status` 等待响应省略 errno 与
/// message、`ufile/files` 携带顶层 `count`——全部 `default` 防御）。
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    /// 原始 `state`（布尔 true / 数字 1 = 成功）——保留原始 Value，
    /// 两种形态都认（真机实证：proapi 布尔 / passportapi 数字）。
    pub state: Value,
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub errno: i64,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub data: Value,
    /// `ufile/files` 顶层兄弟（总行数）；别处缺省 0。
    #[serde(default)]
    pub count: i64,
}

impl Envelope {
    /// 成功 = `state` 数字 1 或布尔 true，且 `errno == 0`。
    pub fn is_ok(&self) -> bool {
        (self.state.as_i64() == Some(1) || self.state.as_bool() == Some(true)) && self.errno == 0
    }

    /// 成功 → `data`；错误 → 映射表终态（[`map_rejection`]）。
    pub(crate) fn ok(self, stage: &'static str, _http: u16) -> Result<Value, StorageError> {
        if self.is_ok() {
            Ok(self.data)
        } else {
            Err(self.to_storage_error(stage))
        }
    }

    /// 错误信封 → StorageError（911 的可行动文案经 warn 通道——
    /// `Unauthorized` 无载荷，ck-sftp error.rs 的双通道裁决）。
    pub(crate) fn to_storage_error(&self, stage: &'static str) -> StorageError {
        if classify(self.code, self.errno) == ErrKind::HumanVerify {
            tracing::warn!(
                target: "ck_pan115::api",
                stage,
                "115 requires human verification (code 911): complete the verification \
                 on 115's official client/web, then re-run setup to re-authorize"
            );
        }
        let mapped = map_rejection(self.code, self.errno, &self.message);
        if let StorageError::Unavailable(detail) = &mapped {
            // R2 可诊断约定带上 stage（载荷不携带 data/message 之外的
            // 任何响应内容——message 本身后端原文）。
            return StorageError::Unavailable(format!("{stage}: {detail}"));
        }
        mapped
    }
}

/// 响应体 → 信封（非 JSON → `Unavailable`，载荷不回显 body——半截
/// 响应可能携带一半凭据对；携带 HTTP 状态与长度供诊断）。
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
// 错误分类（K69.7 采样表）
// ---------------------------------------------------------------------------

/// 业务错误的五类归一（分派依据见模块文档映射表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    /// `401*` 段 / `99`：token 过期——dispatch 刷新+重放一次。
    TokenExpired,
    /// `770004`：账号级访问上限——本地硬退避。
    AccountRateLimited,
    /// `911`：需人工验证——不重试，可行动上抛。
    HumanVerify,
    /// `430004`：文件不存在。
    NotFound,
    /// `20130827`：限流（桌面版形态）。
    RateLimited,
    /// 未知码（终态 `Unavailable` 保留原码）。
    Rejected,
}

/// code/errno 任一命中即分类（真机采样：错误码出现在 `code`（proapi
/// 布尔形态）或 `errno`（passportapi 数字形态）均有可能）。
pub fn classify(code: i64, errno: i64) -> ErrKind {
    if code == 770004 || errno == 770004 {
        ErrKind::AccountRateLimited
    } else if code == 911 || errno == 911 {
        ErrKind::HumanVerify
    } else if code == 430004 || errno == 430004 {
        ErrKind::NotFound
    } else if (40100000..=40199999).contains(&code)
        || (40100000..=40199999).contains(&errno)
        || code == 99
        || errno == 99
    {
        ErrKind::TokenExpired
    } else if code == 20130827 || errno == 20130827 {
        ErrKind::RateLimited
    } else {
        ErrKind::Rejected
    }
}

/// 分类 → StorageError 终态（dispatch 自救之后的归一；映射表见模块
/// 文档）。未知码保留原码与后端消息（R2）。
pub fn map_rejection(code: i64, errno: i64, message: &str) -> StorageError {
    match classify(code, errno) {
        // dispatch 已刷+重放过一次后的兜底（重放仍 401*）→ true：
        // refresh 官方频控下的暂时性失败居多，再试通常可行。
        ErrKind::TokenExpired => StorageError::Unauthorized { recoverable: true },
        // 人工验证 = 到 115 侧完成验证后重新授权——不重试。
        ErrKind::HumanVerify => StorageError::Unauthorized { recoverable: false },
        // 纯终态（dispatch 的 770004 拦截路径会带 retry_after=封锁窗）。
        ErrKind::AccountRateLimited | ErrKind::RateLimited => {
            StorageError::RateLimited { retry_after: None }
        }
        ErrKind::NotFound => StorageError::NotFound,
        ErrKind::Rejected => {
            StorageError::Unavailable(format!("pan115 code={code} errno={errno}: {message}"))
        }
    }
}

// ---------------------------------------------------------------------------
// 载荷类型（tolerant 提取：115 混用字符串/数字）
// ---------------------------------------------------------------------------

fn v_str(v: &Value, key: &str) -> Option<String> {
    match v.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn v_i64(v: &Value, key: &str) -> Option<i64> {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.parse().ok(),
        Some(Value::Array(a)) if a.len() == 1 => v_i64(&a[0], key),
        _ => None,
    }
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|x| x.trim().to_string()).filter(|x| !x.is_empty())
}

/// `ufile/files` 行（字段名 = 后端 JSON 原名缩写：fid 文件 id / fc 分类
/// （0 目录 1 文件）/ fs 字节 / fn 文件名 / pc 挑码 / sha1 / upt 修改时刻）。
#[derive(Debug, Clone, Default)]
pub struct ListRow {
    pub fid: String,
    pub fc: String,
    pub fs: i64,
    pub fname: String,
    pub pc: String,
    pub sha1: String,
    pub upt: i64,
}

fn parse_row(v: &Value) -> ListRow {
    ListRow {
        fid: v_str(v, "fid").unwrap_or_default(),
        fc: v_str(v, "fc").unwrap_or_default(),
        fs: v_i64(v, "fs").unwrap_or(0),
        fname: v_str(v, "fn").unwrap_or_default(),
        pc: v_str(v, "pc").unwrap_or_default(),
        sha1: v_str(v, "sha1").unwrap_or_default(),
        upt: v_i64(v, "upt").unwrap_or(0),
    }
}

/// `folder/get_info` 条目（`file_category`："0" 目录 / "1" 文件）。
#[derive(Debug, Clone, Default)]
pub struct InfoEntry {
    pub file_id: String,
    pub file_name: String,
    pub file_category: String,
    pub pick_code: String,
    pub sha1: String,
    pub size_byte: i64,
    pub size: String,
}

/// OSS callback 配置（init/resume 下发；两个不透明 JSON **字符串**，在
/// complete/put 请求上 base64 原样回带——K69.8）。
///
/// 不实现 `Debug`：callback_var 内嵌上传域授权材料，派生展开有印进
/// 日志的风险（R3；诊断走 [`UploadCallback::len_hint`] 的长度摘要）。
#[derive(Clone, Default)]
pub struct UploadCallback {
    pub callback: String,
    pub callback_var: String,
}

impl UploadCallback {
    /// 长度摘要（日志安全的观测面）。
    pub fn len_hint(&self) -> String {
        format!(
            "cb={}B/var={}B",
            self.callback.len(),
            self.callback_var.len()
        )
    }
}

/// 手写 Debug：只输出长度摘要（R3——派生展开会把 callback_var 的授权
/// 材料印进日志）。
impl std::fmt::Debug for UploadCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadCallback")
            .field("hint", &self.len_hint())
            .finish()
    }
}

fn parse_callback(v: &Value) -> Option<UploadCallback> {
    match v {
        Value::Null => None,
        Value::Array(a) if a.is_empty() => None,
        Value::Object(o) if o.is_empty() => None,
        Value::Array(a) => parse_callback(&a[0]),
        Value::Object(_) => Some(UploadCallback {
            callback: v_str(v, "callback").unwrap_or_default(),
            callback_var: v_str(v, "callback_var").unwrap_or_default(),
        }),
        _ => None,
    }
}

/// `upload/init` 响应（第二轮字段按需出现：`status==2` 秒传命中；
/// `sign_key`/`sign_check` = K69.2 的用户级二次认证挑战——常态路径，
/// 115-3 写路径必须实现回带循环）。
#[derive(Debug, Clone, Default)]
pub struct InitResp {
    pub pick_code: String,
    pub status: i64,
    pub sign_key: Option<String>,
    pub sign_check: Option<String>,
    pub file_id: Option<String>,
    pub bucket: Option<String>,
    pub object: Option<String>,
    pub callback: Option<UploadCallback>,
}

/// `upload/get_token` 的 STS 凭证（OSS 面）。
///
/// 不实现 `Debug`：派生展开有把 access key 印进日志的风险（R3）。
#[derive(Clone, Default)]
pub struct StsToken {
    pub endpoint: String,
    pub access_key_id: String,
    pub access_key_secret: String,
    pub security_token: String,
    pub expiration: String,
}

/// `upload/resume` 响应（无 status 字段）。
#[derive(Clone, Default)]
pub struct ResumeResp {
    pub pick_code: String,
    pub bucket: String,
    pub object: String,
    pub callback: Option<UploadCallback>,
}

// ---------------------------------------------------------------------------
// client + dispatch
// ---------------------------------------------------------------------------

/// 115 API 客户端（token 状态 + dispatch 状态机 + 全局限流器）。
///
/// 网络形态（计划 §5 硬纪律 6 + spike auth.rs `http_client`）：直连
/// （`no_proxy`——spike 实证本形态可达 115）+ 强制 IPv4 dial（PCFS
/// 两驱动同款坑，baidu K18 先例）+ 恒定 UA + 60s 每请求超时。
///
/// 可见性：115-1 骨架期的公开直连面（dispatch 状态机的测试与 115-4
/// 装配载体）；115-2 起 [`crate::Pan115Driver`] 内部持有。
pub struct Pan115Client {
    http: reqwest::Client,
    /// proapi（文件/上传面）。
    pub(crate) api_base: String,
    /// passportapi（refresh 面；oauth 三端点中 device-code 两端点由
    /// setup 直接调 `oauth::*`，本 client 只持 refresh 用 base）。
    passport_base: String,
    tokens: tokio::sync::RwLock<TokenPair>,
    token_store: Option<Arc<dyn TokenStore>>,
    /// 刷新单飞锁：并发 401* 只放一个刷新过（一次一换语义下，第二个
    /// 并发刷新必拿陈旧 refresh_token 撞失效）。
    refresh_lock: tokio::sync::Mutex<()>,
    /// D4 全局限流器（跨端点族；770004 硬退避状态共享于此）。
    pub(crate) limiter: Arc<RateLimiter>,
}

impl Pan115Client {
    /// 构造（token 对必须齐备——初始对来自 setup 扫码或手填配置；
    /// 缺失由装配方先行拒绝，baidu `BaiduClient::new` 同款契约）。
    ///
    /// 骨架期公开直连面（115-1）：驱动九方法占位中，本类型是装配批
    /// （115-4）与测试的状态机载体；115-2 起 [`crate::Pan115Driver`]
    /// 持有并经它驱动全部端点。
    pub fn new(
        access_token: String,
        refresh_token: String,
        passport_base: String,
        api_base: String,
        token_store: Option<Arc<dyn TokenStore>>,
        limiter: Arc<RateLimiter>,
    ) -> Result<Self, StorageError> {
        let http = reqwest::Client::builder()
            .no_proxy() // 直连（spike 实证形态）
            .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED)) // 强制 IPv4 dial
            .user_agent(UA)
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| StorageError::Io(format!("pan115 http client build: {e}")))?;
        Ok(Pan115Client {
            http,
            api_base,
            passport_base,
            tokens: tokio::sync::RwLock::new(TokenPair {
                access: access_token,
                refresh: refresh_token,
            }),
            token_store,
            refresh_lock: tokio::sync::Mutex::new(()),
            limiter,
        })
    }

    /// dispatch 核心：GET（Bearer + query 对）→ `(data, count)`。
    pub(crate) async fn dispatch_get(
        &self,
        path: &str,
        query: &[(&str, &str)],
        stage: &'static str,
    ) -> Result<(Value, i64), StorageError> {
        self.dispatch(
            &format!("{}{path}", self.api_base),
            false,
            query,
            &[],
            None,
            stage,
        )
        .await
    }

    /// dispatch 核心：POST form（Bearer + 可选 per-request UA 覆盖——
    /// downurl 必须携带将来 GET CDN 链接的同一 UA，K69.4 逐字节绑定）
    /// → `data`。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn dispatch_post(
        &self,
        path: &str,
        form: &[(&str, &str)],
        stage: &'static str,
        ua: Option<&str>,
    ) -> Result<Value, StorageError> {
        self.dispatch(
            &format!("{}{path}", self.api_base),
            true,
            &[],
            form,
            ua,
            stage,
        )
        .await
        .map(|(data, _)| data)
    }

    /// 统一请求策略引擎（模块文档「dispatch 状态机」节；每逻辑调用
    /// 至多 1 次刷新 + 1 次重放，有界）。
    async fn dispatch(
        &self,
        url: &str,
        post: bool,
        query: &[(&str, &str)],
        form: &[(&str, &str)],
        ua: Option<&str>,
        stage: &'static str,
    ) -> Result<(Value, i64), StorageError> {
        let mut refreshed = false;
        loop {
            // D4 门：770004 封锁窗内零请求（本地立即 RateLimited）
            if let Err(remaining) = self.limiter.check_wait().await {
                return Err(StorageError::RateLimited {
                    retry_after: Some(remaining),
                });
            }
            let token = self.tokens.read().await.access.clone();
            let mut request = if post {
                self.http.post(url).form(form)
            } else {
                self.http.get(url).query(query)
            };
            request = request.bearer_auth(&token);
            if let Some(ua) = ua {
                request = request.header(reqwest::header::USER_AGENT, ua);
            }
            let resp = request.send().await.map_err(|e| {
                // without_url：query/header 内嵌 token 的剥离（R3）。
                StorageError::Unavailable(format!("{stage} transport: {}", e.without_url()))
            })?;
            let http = resp.status().as_u16();
            let body = resp.text().await.map_err(|e| {
                StorageError::Unavailable(format!("{stage} body read: {}", e.without_url()))
            })?;
            let env: Envelope = match serde_json::from_str(&body) {
                Ok(env) => env,
                Err(_) => {
                    // 非 JSON：截断 + 掩码当前 access_token（后端错误页
                    // 回显请求头的防御；baidu dispatch 同款）。
                    let snippet: String = body.chars().take(200).collect();
                    let masked = snippet.replace(token.as_str(), &mask(&token));
                    return Err(StorageError::Unavailable(format!(
                        "{stage} non-json (http {http}): {masked}"
                    )));
                }
            };
            if env.is_ok() {
                self.limiter.report_ok().await;
                let count = env.count;
                return Ok((env.data, count));
            }
            match classify(env.code, env.errno) {
                ErrKind::TokenExpired if !refreshed => {
                    refreshed = true;
                    self.refresh_once(&token).await?;
                    continue; // 重放一次（loop 头重读 token + 重过限流门）
                }
                ErrKind::TokenExpired => {
                    // 刷新成功但重放仍 401*——绝不再刷（K13 同款语义）。
                    return Err(StorageError::Unauthorized { recoverable: true });
                }
                ErrKind::AccountRateLimited => {
                    let window = self.limiter.report_limit().await;
                    tracing::warn!(
                        target: "ck_pan115::api",
                        stage,
                        window_secs = window.as_secs(),
                        "115 account-level rate cap (770004): hard backoff engaged"
                    );
                    return Err(StorageError::RateLimited {
                        retry_after: Some(window),
                    });
                }
                _ => return Err(env.to_storage_error(stage)),
            }
        }
    }

    /// 刷新编排（单飞）：持锁双检 → wire 刷新（[`oauth::refresh_grant`]
    ///——绕过限流门：refresh 有自身的「至多一次」纪律，频控在 115 侧）
    /// → 整体替换 token 对 → on-arrival 持久化回调。
    ///
    /// - **双检**：拿到锁后若 access_token 已不是触发 401* 的陈旧值，
    ///   说明并发窗口内他人已刷新——直接复用（不消耗一次一换的
    ///   refresh_token）；
    /// - **on-arrival 持久化**（K13）：刷新响应到达即回调
    ///   [`TokenStore::save_tokens`]，先于重放结果——重放失败也不回收
    ///   （新 refresh_token 已是唯一活值，丢弃即凭据损失）；
    /// - 刷新失败 → `Unauthorized{recoverable:true}` / `Unavailable`
    ///   直接上抛（本次逻辑调用的自救机会已用掉）。
    async fn refresh_once(&self, stale_access: &str) -> Result<(), StorageError> {
        let _guard = self.refresh_lock.lock().await;
        {
            let current = self.tokens.read().await;
            if current.access != stale_access {
                return Ok(()); // 并发他人已刷新：复用新值
            }
        }
        let refresh_token = self.tokens.read().await.refresh.clone();
        let pair = oauth::refresh_grant(&self.http, &self.passport_base, &refresh_token).await?;
        *self.tokens.write().await = TokenPair {
            access: pair.access.clone(),
            refresh: pair.refresh.clone(),
        };
        if let Some(store) = &self.token_store {
            store.save_tokens(&pair.access, &pair.refresh);
        }
        Ok(())
    }

    // ------------------------------------------------------- 端点面 ---

    /// `GET /open/user/info` → `data`（uid/配额字段；VolumeId 的 uid 与
    /// quota 的空间字段在 115-2 装配批解析——115-1 骨架不消费）。
    pub async fn user_info(&self) -> Result<Value, StorageError> {
        self.dispatch_get("/open/user/info", &[], "user/info")
            .await
            .map(|(data, _)| data)
    }

    /// `GET /open/ufile/files` 一页 → (rows, count)。115-2 的 list 面
    /// 在此之上做全分页（终止规则：累计 ≥ count 或页短于 limit）。
    pub async fn list_files_page(
        &self,
        cid: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ListRow>, i64), StorageError> {
        let (data, count) = self
            .dispatch_get(
                "/open/ufile/files",
                &[
                    ("cid", cid),
                    ("limit", &limit.to_string()),
                    ("offset", &offset.to_string()),
                    ("show_dir", "1"),
                ],
                "ufile/files",
            )
            .await?;
        let rows = data
            .as_array()
            .map(|a| a.iter().map(parse_row).collect())
            .unwrap_or_default();
        Ok((rows, count))
    }

    /// `GET /open/folder/get_info`——data 为对象或单元素数组（两者都
    /// 容）；空数组 = 不存在形态（code 430004 由 dispatch 归一）。
    pub async fn get_info(&self, file_id: &str) -> Result<InfoEntry, StorageError> {
        let (data, _) = self
            .dispatch_get(
                "/open/folder/get_info",
                &[("file_id", file_id)],
                "folder/get_info",
            )
            .await?;
        let obj = match data {
            Value::Object(_) => data,
            Value::Array(a) if !a.is_empty() => a[0].clone(),
            Value::Array(_) => {
                // 空数组 = not-found 的数据面形态（真机未见，SDK 侧
                // 430004 是常态——防御归一）。
                return Err(StorageError::NotFound);
            }
            other => {
                return Err(StorageError::Unavailable(format!(
                    "folder/get_info: unexpected data shape {}",
                    diagnostic_shape(&other)
                )))
            }
        };
        Ok(InfoEntry {
            file_id: v_str(&obj, "file_id").unwrap_or_default(),
            file_name: v_str(&obj, "file_name").unwrap_or_default(),
            file_category: v_str(&obj, "file_category").unwrap_or_default(),
            pick_code: v_str(&obj, "pick_code").unwrap_or_default(),
            sha1: v_str(&obj, "sha1").unwrap_or_default(),
            size_byte: v_i64(&obj, "size_byte").unwrap_or(0),
            size: v_str(&obj, "size").unwrap_or_default(),
        })
    }

    /// `POST /open/folder/add` → 新目录 file_id。
    pub async fn mkdir(&self, pid: &str, file_name: &str) -> Result<String, StorageError> {
        let data = self
            .dispatch_post(
                "/open/folder/add",
                &[("pid", pid), ("file_name", file_name)],
                "folder/add",
                None,
            )
            .await?;
        Ok(v_str(&data, "file_id").unwrap_or_default())
    }

    /// `POST /open/ufile/delete`（D2：进回收站语义；回收站端点族不引入）。
    pub async fn delete(&self, file_ids: &str, parent_id: &str) -> Result<(), StorageError> {
        self.dispatch_post(
            "/open/ufile/delete",
            &[("file_ids", file_ids), ("parent_id", parent_id)],
            "ufile/delete",
            None,
        )
        .await
        .map(|_| ())
    }

    /// `POST /open/ufile/update`——文件重命名（`file_id` + 新名）。
    /// 115-2 的 rename 面：文件走本端点、目录走 move 自身。
    pub async fn update(&self, file_id: &str, file_name: &str) -> Result<(), StorageError> {
        self.dispatch_post(
            "/open/ufile/update",
            &[("file_id", file_id), ("file_name", file_name)],
            "ufile/update",
            None,
        )
        .await
        .map(|_| ())
    }

    /// `POST /open/ufile/move`——移动（rename 的目录腿 = move 自身；
    /// `file_ids` 逗号分隔）。
    pub async fn move_entries(&self, file_ids: &str, to_pid: &str) -> Result<(), StorageError> {
        self.dispatch_post(
            "/open/ufile/move",
            &[("file_ids", file_ids), ("to_pid", to_pid)],
            "ufile/move",
            None,
        )
        .await
        .map(|_| ())
    }

    /// `POST /open/ufile/downurl`——**必须**携带将来 GET CDN 链接的同一
    /// UA（逐字节绑定，K69.4）。data 是按 pick_code/fid 键的 map，取
    /// `Object.values(data)[0].url.url`（最稳形态）。
    pub async fn downurl(&self, pick_code: &str, ua: &str) -> Result<String, StorageError> {
        let data = self
            .dispatch_post(
                "/open/ufile/downurl",
                &[("pick_code", pick_code)],
                "ufile/downurl",
                Some(ua),
            )
            .await?;
        let Value::Object(map) = data else {
            return Err(StorageError::Unavailable(
                "ufile/downurl: data is not a map".into(),
            ));
        };
        let first = map
            .values()
            .next()
            .ok_or_else(|| StorageError::Unavailable("ufile/downurl: empty data map".into()))?;
        let url = first
            .get("url")
            .and_then(|u| u.get("url"))
            .and_then(|u| u.as_str())
            .ok_or_else(|| StorageError::Unavailable("ufile/downurl: url.url missing".into()))?;
        Ok(url.to_string())
    }

    /// `POST /open/upload/init`（115-3 写路径消费；K69.2：`sign_key`/
    /// `sign_val` 二次认证是用户级挑战——常态路径，第二轮 init 回带）。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_init(
        &self,
        file_name: &str,
        file_size: i64,
        target: &str,
        fileid: &str,
        preid: &str,
        pick_code: Option<&str>,
        sign_key: Option<&str>,
        sign_val: Option<&str>,
    ) -> Result<InitResp, StorageError> {
        let file_size_str = file_size.to_string();
        let mut form: Vec<(&str, &str)> = vec![
            ("file_name", file_name),
            ("file_size", file_size_str.as_str()),
            ("target", target),
            ("fileid", fileid),
            ("preid", preid),
        ];
        if let Some(pc) = pick_code.filter(|s| !s.is_empty()) {
            form.push(("pick_code", pc));
        }
        if let Some(sk) = sign_key.filter(|s| !s.is_empty()) {
            form.push(("sign_key", sk));
        }
        if let Some(sv) = sign_val.filter(|s| !s.is_empty()) {
            form.push(("sign_val", sv));
        }
        let data = self
            .dispatch_post("/open/upload/init", &form, "upload/init", None)
            .await?;
        Ok(InitResp {
            pick_code: v_str(&data, "pick_code").unwrap_or_default(),
            status: v_i64(&data, "status").unwrap_or(0),
            sign_key: non_empty(v_str(&data, "sign_key")),
            sign_check: non_empty(v_str(&data, "sign_check")),
            file_id: non_empty(v_str(&data, "file_id")),
            bucket: non_empty(v_str(&data, "bucket")),
            object: non_empty(v_str(&data, "object")),
            callback: data.get("callback").and_then(parse_callback),
        })
    }

    /// `GET /open/upload/get_token`——STS 凭证（OSS 面；115-3 消费）。
    pub async fn get_token(&self) -> Result<StsToken, StorageError> {
        let (data, _) = self
            .dispatch_get("/open/upload/get_token", &[], "upload/get_token")
            .await?;
        Ok(StsToken {
            endpoint: v_str(&data, "endpoint").unwrap_or_default(),
            access_key_id: v_str(&data, "AccessKeyId").unwrap_or_default(),
            access_key_secret: v_str(&data, "AccessKeySecret").unwrap_or_default(),
            security_token: v_str(&data, "SecurityToken").unwrap_or_default(),
            expiration: v_str(&data, "Expiration").unwrap_or_default(),
        })
    }

    /// `POST /open/upload/resume`（115-3 续传面消费）。
    pub async fn upload_resume(
        &self,
        file_size: i64,
        target: &str,
        fileid: &str,
        pick_code: &str,
    ) -> Result<ResumeResp, StorageError> {
        let data = self
            .dispatch_post(
                "/open/upload/resume",
                &[
                    ("file_size", &file_size.to_string()),
                    ("target", target),
                    ("fileid", fileid),
                    ("pick_code", pick_code),
                ],
                "upload/resume",
                None,
            )
            .await?;
        Ok(ResumeResp {
            pick_code: v_str(&data, "pick_code").unwrap_or_default(),
            bucket: v_str(&data, "bucket").unwrap_or_default(),
            object: v_str(&data, "object").unwrap_or_default(),
            callback: data.get("callback").and_then(parse_callback),
        })
    }
}

/// 诊断用的 data 形态摘要（不含内容——R3）。
fn diagnostic_shape(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
