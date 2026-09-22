//! 薄客户端（Phase 7 / WD2b）——动词面 + 认证状态机 + 重试白名单 +
//! 超时分层 + 错误映射表（计划 §4.4/§4.5）。
//!
//! 构造面（WD1a）：reqwest Client（rustls + connect 超时 +
//! `accept_invalid_certs` 开洞的一次性 warn——§8-D3）+ 基地址与
//! [`AuthState`]。WD2b 接线面：
//!
//! - **重定向不自动跟随**：3xx 是「意外重定向」错误分类面（§4.4），且
//!   PROPFIND/MKCOL 的 301 语义（apache 集合 no-slash 不执行——附录
//!   C ⑧）必须浮到驱动层处理（stat 的尾斜杠重试腿），reqwest 默认的
//!   follow 策略会把两类信号都吃掉；
//! - **认证状态机**（D1）：`auto` = Basic 预发 → 401 Digest challenge →
//!   协商重发**恰一次**；`stale=true` → 换 nonce 重发**恰一次**；NTLM/
//!   Negotiate → 拒绝（可行动文案在 warn 通道——`StorageError::
//!   Unauthorized` 无载荷，R3 双通道）；`basic` 只 Basic；`digest` 无
//!   认证首发吃 challenge（避免 Basic 明文泄漏）。nc 每 nonce 单调递增
//!   （§4.5-1）——**签名临界区持 `tokio::sync::Mutex` 且不跨 await**
//!   （并发读窗口共享 nonce/nc）；
//! - **重试白名单**（§4.5-10，rs-f4ss 正面资产）：仅 GET/HEAD/PROPFIND/
//!   OPTIONS 可重试；触发 = 传输错误（connect/timeout）或 5xx 或 429；
//!   上限 3 次重试、指数退避（基数 500ms 封顶 30s）；429 的
//!   `Retry-After` 优先（秒数 clamp 1–60——K79.3）；
//! - **错误映射**（§4.4 表，读写共用）：传输类 → `Unavailable`
//!   （reqwest 类型化 `is_connect`/`is_timeout` 判定，无字符串信标——
//!   §4.1 注记）；404→NotFound；401/403→Unauthorized{false}；429/5xx
//!   （重试耗尽后）→Unavailable 带码；507→Io；3xx 意外→Unavailable +
//!   Location 提示。
//!
//! ## 代理语义（与 baidu/pan123 的 `no_proxy` 直连相反）
//!
//! **不 no_proxy**：本仓 http 层有代理世界观（telegram 必须走代理），
//! WebDAV 是用户自备服务器——用 reqwest 默认（尊重系统代理 env），
//! 由用户环境决定直连或代理。自签内网 NAS 场景经
//! `webdav_accept_invalid_certs` 开洞（D3）。
//!
//! ## 超时分层（§4.5-7）
//!
//! connect 15s（client 级）/ 控制面（PROPFIND/OPTIONS；写动词同档——
//! WD3 的 spool PUT 会按体量另设预算）30s / 窗口 GET 120s（8 MiB 有界
//! 窗口 → 超时安全——窗口常量的消费点在 driver.rs 读路径）。

use std::time::Duration;

use url::Url;

use cloudkit_storage::StorageError;

use crate::auth::{
    authorization_header, parse_challenge, rand_cnonce, split_challenges, AuthState, DigestSession,
};
use crate::config::{AuthMode, WebdavParams};
use crate::urls::collection_url;
use crate::xml::{self, FailedMember, PropfindEntry};

/// TCP/TLS 连接建立预算（分层之一；rs-f4ss 骨架同值）。
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 控制面动词预算（PROPFIND/OPTIONS；MKCOL/MOVE/DELETE/PROPPATCH 同档）。
pub(crate) const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// 窗口 GET 预算（8 MiB 有界窗口上的读超时——慢滴流注入桩的回归面）。
pub(crate) const WINDOW_TIMEOUT: Duration = Duration::from_secs(120);

/// 自动重试上限（§4.5-10：3 次重试 = 4 次尝试）。
const MAX_RETRIES: u32 = 3;

/// 指数退避基数（500ms → 1s → 2s → …）。
const RETRY_BASE: Duration = Duration::from_millis(500);

/// 退避封顶（§4.5-7 同表：30s）。
const RETRY_CAP: Duration = Duration::from_secs(30);

/// spool PUT 的体量预算（§4.5-7 分层的写面补充——模块文档「WD3 的
/// spool PUT 会按体量另设预算」的兑现）：控制面 30s 基线 + 每 MiB 2s
///（pan123 分片超时同款斜率）。
fn put_timeout(len: u64) -> Duration {
    let per_mib = Duration::from_secs(2 * (len / (1024 * 1024)));
    CONTROL_TIMEOUT.max(per_mib)
}

/// `Retry-After` 秒数的消费钳制（K79.3 纪律：下限防 0s 空转、上限防
/// 服务器勒索性长眠）。
const RETRY_AFTER_CLAMP_SECS: (u64, u64) = (1, 60);

/// 丢弃响应体时的读上限（连接复用纪律——rs-f4ss `drain_response` 资产；
/// 超限即放弃该连接，绝不无界缓冲错误页）。
const DRAIN_LIMIT: usize = 64 * 1024;

/// 诊断片段截断长度（§4.4「保留原文片段，截断脱敏」——XML/HTTP 错误
/// 体的进载荷上限）。
const SNIPPET_LIMIT: usize = 200;

/// PROPFIND allprop 请求体（严格良构——apache expat 对畸形声明 400 是
/// WD0 副发现，桩同款形态）。
pub(crate) const ALLPROP_BODY: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8" ?>"#,
    r#"<D:propfind xmlns:D="DAV:"><D:allprop/></D:propfind>"#
);

/// RFC 4331 quota 点名体（quota 面：`quota-used-bytes` /
/// `quota-available-bytes`；矩阵⑪——双真机均内层 404 → None 降级）。
pub(crate) const QUOTA_BODY: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8" ?>"#,
    r#"<D:propfind xmlns:D="DAV:"><D:prop>"#,
    r#"<D:quota-used-bytes/><D:quota-available-bytes/>"#,
    r#"</D:prop></D:propfind>"#
);

/// PROPFIND 的结构化产出：multistatus 投影（驱动 stat/list/quota 三面
/// 的公共载体）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PropfindOutcome {
    pub entries: Vec<PropfindEntry>,
    /// 仅非 2xx 块的 response 成员（M7）——不进 `entries`，但按成员映
    /// 射浮现（[`map_member_failure`]），绝不静默消失。
    pub failed_members: Vec<FailedMember>,
}

/// WebDAV 薄客户端：单 reqwest Client（池内并发，D6）+ 基地址 + 认证
/// 协商状态。
///
/// - 语义：动词面在（必要时协商的）会话上执行；Digest 会话跨请求复用
///   （nc 单调），stale nonce 原地换新；
/// - 错误：对外统一 [`StorageError`]（映射表 [`map_status`]/
///   [`map_transport`]，§4.4）；
/// - 并发：全方法可并发调用（`&self`）；auth 状态经 tokio Mutex 单一
///   来源（签名临界区不跨 await——「锁不跨 await」纪律）；
/// - 生命周期：HTTP client 无连接态；nonce 会话的过期/重协商在 401
///   路径自理（恰一次，无 OpenList 的 cron 重建形态）。
pub(crate) struct WebdavClient {
    /// 共享 HTTP 面（池内并发；connect 超时 + 不跟随重定向已烙进 builder）。
    http: reqwest::Client,
    /// 规范化基地址（尾斜杠形态；子路径即卷根）——OPTIONS 探活与驱动
    /// 侧 URL 构造的锚。
    base: Url,
    /// 认证形态（`webdav_auth` 键）。
    auth_mode: AuthMode,
    /// 凭据对（双缺 = 匿名；成对性由 config 层钉死）。
    username: Option<String>,
    password: Option<String>,
    /// 认证协商状态（D1 状态机；nc 单调自守的单一来源）。
    auth: tokio::sync::Mutex<AuthState>,
    /// 最近一次 401 终局的分类（[`RejectionReason`]——doctor 探活的投
    /// 影面；`StorageError::Unauthorized` 无载荷契约不动，R3 双通道：
    /// 文案在 warn 通道，分类在这里供 probe 读）。std Mutex：读写都
    /// 不跨 await 的短临界区。
    last_rejection: std::sync::Mutex<Option<RejectionReason>>,
}

/// 401 终局的分类（handle_401 决策表的记录版——单一来源：probe 读的
/// 就是协商路径实际走过的分支，不重推）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RejectionReason {
    /// 未配置凭据遇 401。
    MissingCredentials,
    /// auth=basic 显式模式被拒（`offered` = 服务器的 challenge 列表）。
    BasicModeRefused { offered: Vec<String> },
    /// 服务器只要 NTLM/Negotiate（不支持）。
    UnsupportedScheme(String),
    /// Digest 协商 + 恰一次重发后仍拒。
    RejectedAfterNegotiation,
}

impl WebdavClient {
    /// 构造（纯本地：建 reqwest Client，不碰网络——惰性连接归
    /// reqwest 池）。**不打 D3 warn**——warn 归
    /// [`crate::driver::WebdavDriver::new`]（装配面一次性）：doctor
    /// 探活的宽校验诊断客户端（`accept_invalid_certs=true`、与用户配
    /// 置无关）也走这里，warn 在装配面才不误导。
    pub(crate) fn new(params: &WebdavParams) -> Result<Self, StorageError> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            // 不自动跟随重定向：3xx 是错误分类面/驱动处理面（模块文档）。
            .redirect(reqwest::redirect::Policy::none())
            .danger_accept_invalid_certs(params.accept_invalid_certs)
            .build()
            .map_err(|error| {
                StorageError::Unavailable(format!("building the webdav http client: {error}"))
            })?;
        Ok(WebdavClient {
            http,
            base: params.url.clone(),
            auth_mode: params.auth,
            username: params.username.clone(),
            password: params.password.clone(),
            auth: tokio::sync::Mutex::new(AuthState::None),
            last_rejection: std::sync::Mutex::new(None),
        })
    }

    /// 最近一次 401 终局的分类（短临界区；探活专读）。
    pub(crate) fn last_rejection(&self) -> Option<RejectionReason> {
        self.last_rejection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 基地址（驱动侧 `join_path` 的锚）。
    pub(crate) fn base(&self) -> &Url {
        &self.base
    }

    // ------------------------------------------------------ 执行管线 ---

    /// 统一执行管线：认证应用 → 发送 → 401 协商（恰一次重发）→ 白名单
    /// 重试（传输错误/5xx/429，≤ [`MAX_RETRIES`] 次）。
    ///
    /// 返回**原始响应**（状态语义归调用方——verb 方法各自校验 207/206
    /// 形态后走 [`map_status`]）；传输失败在重试耗尽后归
    /// [`map_transport`]。
    async fn execute(
        &self,
        verb: &str,
        url: &Url,
        headers: &[(&'static str, String)],
        body: Option<bytes::Bytes>,
        timeout: Duration,
    ) -> Result<reqwest::Response, StorageError> {
        let wire_uri = wire_uri(url);
        let retryable = verb_is_retryable(verb);
        // 401 协商重发的预算：每请求恰一次（初始协商与 stale 再协商共用
        // 同一预算——「重算重发恰一次」的统一实现，D1）。
        let mut auth_resent = false;
        let mut attempt: u32 = 0;
        loop {
            let authz = self.authz_for(verb, &wire_uri).await;
            let method = http_method(verb)?;
            let mut builder = self.http.request(method, url.clone()).timeout(timeout);
            for (name, value) in headers {
                builder = builder.header(*name, value.clone());
            }
            // Basic 成功后的状态升级判据（先取——match 臂会移出字段）。
            let used_basic = matches!(authz, Authz::Basic { .. });
            builder = match authz {
                Authz::None => builder,
                // Basic 头经 reqwest 内建（base64 不进直接依赖，§1.1）。
                Authz::Basic { user, pass } => builder.basic_auth(user, Some(pass)),
                Authz::Digest(header) => builder.header("authorization", header),
            };
            if let Some(bytes) = &body {
                builder = builder.body(bytes.clone());
            }
            let outcome = builder.send().await;
            let response = match outcome {
                Ok(response) => response,
                Err(error) => {
                    // 传输错误（connect/timeout/中断）——白名单内退避重试。
                    if retryable && attempt < MAX_RETRIES {
                        attempt += 1;
                        let delay = backoff_delay(attempt, None);
                        tracing::warn!(
                            target: "ck_webdav::client",
                            verb,
                            attempt,
                            ?delay,
                            error = %error,
                            "webdav transport error on an idempotent verb; backing off before \
                             the retry"
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(map_transport(error));
                }
            };
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                if !auth_resent && self.handle_401(verb, &wire_uri, response).await {
                    auth_resent = true;
                    continue; // 协商/换 nonce 后重发（恰一次预算）。
                }
                if auth_resent {
                    // 重发后的再 401 = 凭据被拒的终局（首 401 的分类文案
                    // 已在 handle_401 留痕；这里补「重发后仍拒」的事实
                    // ——doctor 探活的分类投影同步记录）。
                    self.record_rejection(RejectionReason::RejectedAfterNegotiation);
                    tracing::warn!(
                        target: "ck_webdav::client",
                        verb,
                        "{}",
                        message_credentials_rejected()
                    );
                }
                // 终局 401：`Unauthorized{false}`（驱动自救已尽的契约面；
                // 可行动文案在 warn 通道——R3 双通道）。
                return Err(StorageError::Unauthorized { recoverable: false });
            }
            if retryable
                && attempt < MAX_RETRIES
                && (status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
            {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|text| text.trim().parse::<u64>().ok());
                drain_bounded(response).await;
                attempt += 1;
                let delay = backoff_delay(attempt, retry_after);
                tracing::warn!(
                    target: "ck_webdav::client",
                    verb,
                    status = status.as_u16(),
                    attempt,
                    ?delay,
                    "webdav transient status on an idempotent verb; backing off before the retry"
                );
                tokio::time::sleep(delay).await;
                continue;
            }
            // Basic 首发成功后把状态升到 BasicReady（状态机卫生：None →
            // 「Basic 已证实可用」——后续请求同一签名路径，行为无差）。
            if used_basic {
                self.mark_basic_ready().await;
            }
            return Ok(response);
        }
    }

    /// 401 处理：解析 challenge 并（可能）更新协商状态。
    ///
    /// 返回 `true` = 已建立/换新 Digest 会话，调用方重发**恰一次**；
    /// `false` = 无协商出路（缺凭据/basic 模式/NTLM/凭据被拒）——分类
    /// 文案已在此留痕，调用方以 `Unauthorized{false}` 终局。
    async fn handle_401(&self, verb: &str, wire_uri: &str, response: reqwest::Response) -> bool {
        let challenges: Vec<String> = response
            .headers()
            .get_all(reqwest::header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_string)
            .collect();
        drain_bounded(response).await;

        // 每个无出路的分支先把分类记进 last_rejection（doctor 探活的投
        // 影面）——文案与分类同源同步走。
        if self.username.is_none() || self.password.is_none() {
            self.record_rejection(RejectionReason::MissingCredentials);
            tracing::warn!(
                target: "ck_webdav::client",
                verb,
                uri = %wire_uri,
                "{}",
                message_missing_credentials()
            );
            return false;
        }
        if self.auth_mode == AuthMode::Basic {
            self.record_rejection(RejectionReason::BasicModeRefused {
                offered: challenges.clone(),
            });
            tracing::warn!(
                target: "ck_webdav::client",
                verb,
                "{}",
                message_basic_mode_refused(&challenges)
            );
            return false;
        }
        // M6：单头并置多 challenge（RFC 7235 允许的
        // `Basic realm=..., Digest ...` 逗号并置形态）先按引号感知的顶
        // 层逗号拆 scheme 段，再逐段解析——按头整段解析会把并置的
        // Digest offer 整个拒掉（明明可协商却误分类「凭据被拒」）。
        let segments: Vec<String> = challenges
            .iter()
            .flat_map(|value| split_challenges(value))
            .collect();
        // 多 challenge 服务器上优先 Digest（首个可解析者）。
        for challenge_value in &segments {
            if let Ok(challenge) = parse_challenge(challenge_value) {
                let stale = challenge.stale;
                let realm = challenge.realm.clone();
                let mut guard = self.auth.lock().await;
                // M1：同 nonce 的重复 challenge **保留现会话**——并发请求
                // 各自吃到同一 nonce 的 401（时间基 nonce 服务器的真形）
                // 时，无条件重置会把 nc 归零，严格 nc 服务器上第二个签名
                // 与第一个同 nc → 伪凭据拒绝。仅 nonce 变化（或现状态非
                // Digest）才整体重置。
                let renegotiated = match &*guard {
                    AuthState::Digest(session) if session.nonce == challenge.nonce => false,
                    _ => {
                        *guard = AuthState::Digest(DigestSession::from_challenge(&challenge));
                        true
                    }
                };
                drop(guard);
                // realm/nonce 是服务器签发物，日志安全（模块文档纪律）。
                tracing::info!(
                    target: "ck_webdav::client",
                    verb,
                    stale,
                    realm,
                    renegotiated,
                    "Digest challenge accepted; resending exactly once"
                );
                return true;
            }
        }
        if let Some(scheme) = unsupported_scheme_token(&segments) {
            self.record_rejection(RejectionReason::UnsupportedScheme(scheme.clone()));
            tracing::warn!(
                target: "ck_webdav::client",
                verb,
                "{}",
                message_unsupported_scheme(&scheme)
            );
            return false;
        }
        self.record_rejection(RejectionReason::RejectedAfterNegotiation);
        tracing::warn!(
            target: "ck_webdav::client",
            verb,
            "{}",
            message_credentials_rejected()
        );
        false
    }

    /// 记录 401 终局分类（短临界区，不跨 await）。
    fn record_rejection(&self, reason: RejectionReason) {
        *self
            .last_rejection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
    }

    /// 请求签名（短临界区，锁不跨 await）：按当前 [`AuthState`] 产出
    /// Authorization 材料。Digest 臂先递增 nc 再签名——nc 单调性的
    /// 单一来源（§4.5-1）。
    async fn authz_for(&self, verb: &str, wire_uri: &str) -> Authz {
        let Some(user) = self.username.clone() else {
            return Authz::None;
        };
        let Some(pass) = self.password.clone() else {
            return Authz::None;
        };
        let mut guard = self.auth.lock().await;
        match &mut *guard {
            AuthState::Digest(session) => {
                session.nc += 1;
                Authz::Digest(authorization_header(
                    &user,
                    &pass,
                    verb,
                    wire_uri,
                    session,
                    &rand_cnonce(),
                ))
            }
            // BasicReady 只会在 auto/basic 形态下标记；Digest 形态下此臂
            // 不可达，防御性按匿名处理。
            AuthState::None | AuthState::BasicReady => match self.auth_mode {
                // digest 形态：无认证首发吃 challenge（Basic 明文不泄漏）。
                AuthMode::Digest => Authz::None,
                AuthMode::Auto | AuthMode::Basic => Authz::Basic { user, pass },
            },
        }
    }

    /// Basic 首发成功后的状态升级（None → BasicReady；无行为差，状态机
    /// 卫生）。
    async fn mark_basic_ready(&self) {
        let mut guard = self.auth.lock().await;
        if matches!(&*guard, AuthState::None) {
            *guard = AuthState::BasicReady;
        }
    }

    // ------------------------------------------------------ 动词面 ---

    /// PROPFIND（Depth `0`/`1`，body 由调用方给——allprop/quota 点名两
    /// 形态）→ multistatus 投影。
    ///
    /// - 重试白名单：幂等可重试（§4.5-10）；
    /// - 集合腿 URL 恒带尾斜杠（附录 C ⑧——`collection_url`）；
    /// - 2xx → 解析（207 之外 2xx 宽收为 multistatus 解析）；解析失败 →
    ///   `Io` 带 ≤200 字节截断片段（§4.4）；
    /// - 非 2xx → [`map_status`]。
    pub(crate) async fn propfind(
        &self,
        url: &Url,
        depth: Depth,
        body: &str,
    ) -> Result<PropfindOutcome, StorageError> {
        let response = self
            .execute(
                "PROPFIND",
                url,
                &[
                    ("depth", depth.as_str().to_string()),
                    ("content-type", "text/xml; charset=utf-8".to_string()),
                ],
                Some(bytes::Bytes::from(body.to_string())),
                CONTROL_TIMEOUT,
            )
            .await?;
        let status = response.status();
        if !status.is_success() {
            let text = body_bounded(response).await;
            return Err(map_status(status, "PROPFIND", &diagnostic(&text, url)));
        }
        let text = response.text().await.map_err(map_transport)?;
        match xml::parse_multistatus(&text) {
            Ok(rows) => Ok(PropfindOutcome {
                entries: xml::entries(&rows),
                failed_members: xml::failed_members(&rows),
            }),
            Err(error) => Err(StorageError::Io(format!(
                "webdav multistatus parse failed: {error}; body: {}",
                snippet(&text)
            ))),
        }
    }

    /// stat 语义的 PROPFIND **Depth 0** → 单条目投影（404 → `NotFound`）。
    ///
    /// **Depth 头恒显式发送**（RFC 4918 §9.1 要求；缺省值留给服务器
    /// 自由发挥——dav-server 0.11 对无 Depth 的 PROPFIND 回**空
    /// multistatus**，真实现揭出的 WD2 缺陷，双桩制的参照腿首功）。
    ///
    /// **尾斜杠重试腿（附录 C ⑧ apache SlashStrict）**：文件形态 URL
    /// （无尾斜杠）遇 3xx → 以集合形态（尾斜杠）重试恰一次——集合
    /// no-slash 301 是「不执行」信号而非错误，跟随到 slashed 形态即
    /// 真值；其余 3xx（如 unexpected_301 旋钮的文件腿）两次都 3xx →
    /// `Unavailable` + Location 提示（§4.4）。
    pub(crate) async fn stat(&self, url: &Url) -> Result<PropfindEntry, StorageError> {
        let depth = [("depth", Depth::Zero.as_str().to_string())];
        let response = self
            .execute("PROPFIND", url, &depth, None, CONTROL_TIMEOUT)
            .await?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            drain_bounded(response).await;
            let slashed = collection_url(url);
            let second = self
                .execute("PROPFIND", &slashed, &depth, None, CONTROL_TIMEOUT)
                .await?;
            return self.stat_entry_of(second, &slashed, Some(&location)).await;
        }
        let url = url.clone();
        self.stat_entry_of(response, &url, None).await
    }

    /// stat 的收尾半边（对给定响应做状态归一 + 单条目提取）。
    async fn stat_entry_of(
        &self,
        response: reqwest::Response,
        url: &Url,
        prior_location: Option<&str>,
    ) -> Result<PropfindEntry, StorageError> {
        let status = response.status();
        if !status.is_success() {
            let text = body_bounded(response).await;
            let mut diagnostic = diagnostic(&text, url);
            if let Some(location) = prior_location {
                diagnostic = format!("Location: {location}; {diagnostic}");
            }
            return Err(map_status(status, "PROPFIND", &diagnostic));
        }
        let text = response.text().await.map_err(map_transport)?;
        let rows = xml::parse_multistatus(&text).map_err(|error| {
            StorageError::Io(format!(
                "webdav multistatus parse failed: {error}; body: {}",
                snippet(&text)
            ))
        })?;
        // M7：单成员 PROPFIND（Depth 0）的失败成员按成员映射——仅内层
        // 非块 2xx 的成员不进 entries（既有纪律），但按真类浮现而不是
        // 落进 NotFound 的兜底。
        if let Some(failed) = xml::failed_members(&rows).first() {
            return Err(map_member_failure(failed.status));
        }
        xml::entries(&rows)
            .into_iter()
            .next()
            .ok_or(StorageError::NotFound)
    }

    /// 窗口 GET → `Ok(Some(bytes))`；`Ok(None)` = 416（Range 不可满足
    /// ——复核归调用方：driver reader 的 stat 复核腿，§4.4）。
    ///
    /// - `window` 恒为半开区间 `(start, end_exclusive)`（M3：驱动 reader
    ///   是唯一调用方，`None` 全量形态已随 transport 面改走窗口路径
    ///   删除）；退化窗口（`end <= start`）→ 空答案；
    /// - `Range: bytes=start-{end-1}`；**206 校验**：`Content-Range` 的
    ///   rs/re 必须逐字段吻合请求（不吻合 → `Io` 带两侧值——§4.5-6）；
    ///   **200 回退**（服务器无视 Range——矩阵④）：body 覆盖请求区间 →
    ///   截断；不足 → `Io`；
    /// - **读取封顶（M3）**：成功体至多读到窗口终点（206 = 窗长、200 =
    ///   `end` 字节）即停——服务器多给的部分随连接放弃。apache 倒序
    ///   Range 回 200 全量的真形上，这一条把读取量从「整个文件」钉到
    ///   「窗口终点」，杜绝整文件进内存的 OOM 面；
    /// - GET 在重试白名单内（窗口间不跨窗重试——driver 文档注明）。
    pub(crate) async fn get_range(
        &self,
        url: &Url,
        window: (u64, u64),
    ) -> Result<Option<bytes::Bytes>, StorageError> {
        let (start, end) = window;
        let mut headers: Vec<(&'static str, String)> = Vec::new();
        if end > start {
            // 半开 → 闭区间头（RFC 9110 Range 形态）；退化窗口不发现。
            headers.push(("range", format!("bytes={}-{}", start, end - 1)));
        }
        let response = self
            .execute("GET", url, &headers, None, WINDOW_TIMEOUT)
            .await?;
        let status = response.status();
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            drain_bounded(response).await;
            return Ok(None);
        }
        if !status.is_success() {
            let text = body_bounded(response).await;
            return Err(map_status(status, "GET", &diagnostic(&text, url)));
        }
        if end <= start {
            drain_bounded(response).await;
            return Ok(Some(bytes::Bytes::new()));
        }
        let content_range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            // 206 体 = 恰好请求窗（封顶 = 窗长；多给即服务器撒谎——校验
            // 腿如实报错）。
            let bytes = read_capped(response, (end - start) as usize).await?;
            let actual = content_range.as_deref().and_then(parse_content_range);
            match actual {
                Some((rs, re, _total)) if rs == start && re == end - 1 => {
                    if bytes.len() as u64 != end - start {
                        return Err(StorageError::Io(format!(
                            "webdav 206 body length {} disagrees with the window \
                             [{start},{end})",
                            bytes.len()
                        )));
                    }
                    Ok(Some(bytes))
                }
                other => Err(StorageError::Io(format!(
                    "webdav 206 Content-Range mismatch: requested bytes {}-{} but got {:?} \
                     (header {:?})",
                    start,
                    end - 1,
                    other,
                    content_range
                ))),
            }
        } else {
            // 200 族回退：body 自文件头起——封顶 = 窗口终点 `end`（覆盖
            // 请求区间 → 截断，§4.4；不足 → Io）。
            let bytes = read_capped(response, end as usize).await?;
            if (bytes.len() as u64) < end {
                return Err(StorageError::Io(format!(
                    "webdav server ignored the Range and its {}-byte body does not cover the \
                     window [{start},{end})",
                    bytes.len()
                )));
            }
            Ok(Some(bytes.slice(start as usize..end as usize)))
        }
    }

    /// PUT（带 Content-Length 的整体上传——D5；`.part` 暂存件腿）。
    /// **永不自动重试**（§4.5-10：PUT 非幂等——重放可能重复写效果；
    /// stager 的 lost-ACK 重放窗协议承担恢复语义）。响应体做有界排空
    /// （连接杀在 PUT 上的可观察面——排空即传输错误浮现）。
    ///
    /// `x_oc_mtime`：nextcloud vendor 的 `X-OC-Mtime` 搭车（D4/D2 修订
    /// ——generic 恒 `None` 不发；客户端只搬运头，vendor 决策归 stager）。
    /// 超时按体量另设预算（§4.5-7 写面补充：30s 基线 + 每 MiB 2s——
    /// 整件 PUT 的宽收斜率，pan123 分片超时同款）。
    pub(crate) async fn put(
        &self,
        url: &Url,
        body: bytes::Bytes,
        x_oc_mtime: Option<u64>,
    ) -> Result<(), StorageError> {
        let headers: Vec<(&'static str, String)> = match x_oc_mtime {
            Some(secs) => vec![("x-oc-mtime", secs.to_string())],
            None => Vec::new(),
        };
        let response = self
            .execute(
                "PUT",
                url,
                &headers,
                Some(body.clone()),
                put_timeout(body.len() as u64),
            )
            .await?;
        let status = response.status();
        if !status.is_success() {
            let text = body_bounded(response).await;
            return Err(map_status(status, "PUT", &diagnostic(&text, url)));
        }
        drain_bounded(response).await;
        Ok(())
    }

    /// OPTIONS（doctor 探活 + Class 1 能力发现；幂等可重试）。2xx →
    /// `Ok(())`；非 2xx → [`map_status`]（认证失败以 Unauthorized 浮现
    /// ——transport 面 connect 的探活语义）。
    pub(crate) async fn options(&self) -> Result<(), StorageError> {
        self.options_probe().await.map(|(_dav_class, _allow)| ())
    }

    /// OPTIONS 的**带投影形态**（WD4 doctor 探活专用）：2xx →
    /// `(DAV 头, Allow 头)`（都可选——RFC 9110/4918 不强制服务器回
    /// 这两个头）；非 2xx → [`map_status`]。[`Self::options`] 是它的
    /// 丢投影薄壳（transport connect 语义零漂移）。
    pub(crate) async fn options_probe(
        &self,
    ) -> Result<(Option<String>, Option<String>), StorageError> {
        let response = self
            .execute("OPTIONS", &self.base.clone(), &[], None, CONTROL_TIMEOUT)
            .await?;
        let status = response.status();
        if status.is_success() {
            let projected = {
                let headers = response.headers();
                (
                    headers
                        .get("dav")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string),
                    headers
                        .get("allow")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string),
                )
            };
            drain_bounded(response).await;
            return Ok(projected);
        }
        let text = body_bounded(response).await;
        Err(map_status(
            status,
            "OPTIONS",
            &diagnostic(&text, &self.base),
        ))
    }

    // ------------------------------------------------ 写侧动词（WD3 接线）---

    /// MKCOL（恒带尾斜杠——附录 C ⑥/⑧；rclone 已存在 201 幂等陷阱由
    /// 驱动 stat 预检吸收）。**永不自动重试**（非幂等）。
    ///
    /// 结果分类（§4.4 verb 专用行的驱动介入面——[`get_range`] 的 416
    /// `Ok(None)` passthrough 同款纪律：可处置形态不预映射，语义决策归
    /// 驱动）：
    /// - 2xx → [`MkcolOutcome::Created`]；
    /// - 409（父集合缺失）→ [`MkcolOutcome::ParentMissing`]——驱动隐式
    ///   建父后重试恰一次；
    /// - 405（目标已被占用——RFC/apache 形态；rclone 的 201 陷阱由预检
    ///   吸收，此臂只接预检后的竞态带）→ `Err(Exists)`；
    /// - 其余 → [`map_status`]。
    pub(crate) async fn mkcol(&self, url: &Url) -> Result<MkcolOutcome, StorageError> {
        let response = self
            .execute("MKCOL", url, &[], None, CONTROL_TIMEOUT)
            .await?;
        let status = response.status();
        if !status.is_success() {
            let text = body_bounded(response).await;
            if status == reqwest::StatusCode::CONFLICT {
                return Ok(MkcolOutcome::ParentMissing);
            }
            if status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
                return Err(StorageError::Exists);
            }
            return Err(map_status(status, "MKCOL", &diagnostic(&text, url)));
        }
        drain_bounded(response).await;
        Ok(MkcolOutcome::Created)
    }

    /// DELETE（集合腿恒带尾斜杠——附录 C ⑦，由驱动 stat 预检判形态）。
    /// **永不自动重试**。404 → `NotFound`（幂等形态声明的动词半边）；
    /// 其余非 2xx → [`map_status`]。
    pub(crate) async fn delete(&self, url: &Url) -> Result<(), StorageError> {
        let response = self
            .execute("DELETE", url, &[], None, CONTROL_TIMEOUT)
            .await?;
        let status = response.status();
        if !status.is_success() {
            let text = body_bounded(response).await;
            return Err(map_status(status, "DELETE", &diagnostic(&text, url)));
        }
        drain_bounded(response).await;
        Ok(())
    }

    /// MOVE（**恒显式 `Overwrite` 头 + 绝对 `Destination`**——附录 C ⑤
    /// rclone 缺头偏离 / ⑩ apache 拒相对 URI 两行的驱动对策；目录腿源
    /// 与 Destination 的尾斜杠由驱动经 [`crate::urls::collection_url`]
    /// 组合后传入）。**永不自动重试**（非幂等——重放窗防线归 stager，
    /// K67 H2）。
    ///
    /// 结果分类（§4.4 verb 专用行，passthrough 纪律同 [`Self::mkcol`]）：
    /// - 2xx → [`MoveOutcome::Done`]；
    /// - 412 且 `overwrite == false`（Overwrite:F 撞既有目标）→
    ///   [`MoveOutcome::PreconditionFailed`]——驱动重 stat 复核后定
    ///   `Exists`（K75-1：只认显式 precondition）；
    /// - 403/409/500（缺父嫌疑：矩阵⑤ rclone403 / apache500 + RFC 409
    ///   形态）→ [`MoveOutcome::ParentSuspect`]——驱动 stat 目标父核实
    ///   后定夺（缺→隐式建父重试恰一次；在→按通用表归一）；
    /// - 其余 → `Err(map_status)`。
    pub(crate) async fn move_(
        &self,
        from: &Url,
        to: &Url,
        overwrite: bool,
    ) -> Result<MoveOutcome, StorageError> {
        let headers = [
            ("overwrite", if overwrite { "T" } else { "F" }.to_string()),
            // Destination 恒绝对 URI（url::Url 的序列化面——构造即保证）。
            ("destination", to.as_str().to_string()),
        ];
        let response = self
            .execute("MOVE", from, &headers, None, CONTROL_TIMEOUT)
            .await?;
        let status = response.status();
        if !status.is_success() {
            let text = body_bounded(response).await;
            let diagnostic = diagnostic(&text, from);
            if status == reqwest::StatusCode::PRECONDITION_FAILED && !overwrite {
                return Ok(MoveOutcome::PreconditionFailed { diagnostic });
            }
            if matches!(status.as_u16(), 403 | 409 | 500) {
                return Ok(MoveOutcome::ParentSuspect {
                    status: status.as_u16(),
                    diagnostic,
                });
            }
            return Err(map_status(status, "MOVE", &diagnostic));
        }
        drain_bounded(response).await;
        Ok(MoveOutcome::Done)
    }
}

// ------------------------------------------------------ 认证材料形态 ---

/// 一次请求的 Authorization 材料（签名临界区的产出）。
enum Authz {
    /// 不带认证头。
    None,
    /// Basic（reqwest `basic_auth` 内建编码）。
    Basic { user: String, pass: String },
    /// Digest（已签名头值）。
    Digest(String),
}

// ------------------------------------------------ 写侧结果分类（WD3）---

/// MKCOL 的结果分类（§4.4 verb 专用行——409 缺父形态 passthrough 给
/// 驱动的隐式建父重试腿；见 [`WebdavClient::mkcol`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MkcolOutcome {
    /// 建成（2xx）。
    Created,
    /// 409：父集合缺失——驱动隐式建父后重试恰一次。
    ParentMissing,
}

/// MOVE 的结果分类（§4.4 verb 专用行——412/缺父形态 passthrough 给驱动；
/// 见 [`WebdavClient::move_`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MoveOutcome {
    /// 移动完成（2xx）。
    Done,
    /// 412（`Overwrite: F` 撞既有目标）——驱动重 stat 复核后定 `Exists`
    ///（K75-1：只认显式 precondition，传输类绝不映射 Exists）。
    PreconditionFailed {
        /// 截断诊断片段（复核不成立时的原文保留）。
        diagnostic: String,
    },
    /// 缺父嫌疑（403/409/500——矩阵⑤ rclone403 / apache500 + RFC 409）：
    /// 驱动 stat 目标父核实——缺则隐式建父重试恰一次；在则按通用表归一。
    ParentSuspect {
        /// 原始状态码。
        status: u16,
        /// 截断诊断片段。
        diagnostic: String,
    },
}

impl MoveOutcome {
    /// 非 `Done` 形态的终局映射（stager 等不做处置腿的调用方共用）：
    /// 412 → `Unavailable` 保留原文；缺父三态 → [`map_status`] 通用表
    ///（403→Unauthorized / 409→Invalid / 500→Unavailable）。
    pub(crate) fn into_storage_error(self) -> StorageError {
        match self {
            MoveOutcome::PreconditionFailed { diagnostic } => StorageError::Unavailable(format!(
                "webdav MOVE failed with 412 Precondition Failed: {diagnostic}"
            )),
            MoveOutcome::ParentSuspect { status, diagnostic } => {
                match reqwest::StatusCode::from_u16(status) {
                    Ok(code) => map_status(code, "MOVE", &diagnostic),
                    // 不可达防御（构造面只产 403/409/500）。
                    Err(_) => StorageError::Unavailable(format!(
                        "webdav MOVE failed (HTTP {status}): {diagnostic}"
                    )),
                }
            }
            MoveOutcome::Done => StorageError::Unavailable(
                "internal: MoveOutcome::Done has no error form".to_string(),
            ),
        }
    }
}

// ---------------------------------------------------- 纯函数（单测面）---

/// 请求 URL → Digest 签名用的 **wire 形态** URI（path+query，与发出去
/// 的逐字节一致——§4.5-2；桩 enforce：apache 严格比对）。无 query 的
/// 常态即 `url.path()`。
fn wire_uri(url: &Url) -> String {
    let mut out = url.path().to_string();
    if let Some(query) = url.query() {
        out.push('?');
        out.push_str(query);
    }
    out
}

/// 动词名 → `reqwest::Method`（PROPFIND 等非标准动词经 from_bytes；
/// 固定 ASCII 集合上的不可达错误仍走类型化映射——生产路径零 unwrap）。
fn http_method(verb: &str) -> Result<reqwest::Method, StorageError> {
    reqwest::Method::from_bytes(verb.as_bytes()).map_err(|error| {
        StorageError::Unavailable(format!("internal: invalid webdav verb {verb:?}: {error}"))
    })
}

/// 重试白名单（§4.5-10，rs-f4ss `should_retry_request` 正面资产移植）：
/// 仅幂等读侧动词可自动重试。**PUT/MOVE/MKCOL/DELETE/PROPPATCH 永不
/// 自动重试——它们非幂等：连接中断后重放可能把已生效的写效果再执行
/// 一次（重复建目录/覆盖更新版本/删两次），恢复语义归调用方协议
/// （WD3 stager 的 lost-ACK 重放窗）。**
pub(crate) fn verb_is_retryable(verb: &str) -> bool {
    matches!(verb, "GET" | "HEAD" | "PROPFIND" | "OPTIONS")
}

/// 退避档选择（§4.5-10 + K79.3）：`Retry-After` 秒数（clamp 1–60）优先；
/// 否则指数退避（基数 500ms、封顶 30s；attempt 从 1 起）。
pub(crate) fn backoff_delay(attempt: u32, retry_after: Option<u64>) -> Duration {
    if let Some(seconds) = retry_after {
        let (floor, ceiling) = RETRY_AFTER_CLAMP_SECS;
        return Duration::from_secs(seconds.clamp(floor, ceiling));
    }
    let shift = attempt.saturating_sub(1).min(16);
    RETRY_BASE.saturating_mul(1u32 << shift).min(RETRY_CAP)
}

/// challenge 列表里是否只有不支持的诗化方案（NTLM/Negotiate）——返回
/// 首个命中方案名（小写；文案用）。
fn unsupported_scheme_token(challenges: &[String]) -> Option<String> {
    challenges.iter().find_map(|challenge| {
        let scheme = challenge.split_whitespace().next()?.to_ascii_lowercase();
        matches!(scheme.as_str(), "ntlm" | "negotiate").then_some(scheme)
    })
}

/// 诊断片段组装：状态体片段 + URL 提示（3xx 场景由调用方附加 Location）。
fn diagnostic(text: &str, url: &Url) -> String {
    format!("url = {url}; body = {}", snippet(text))
}

/// 截断片段（≤ [`SNIPPET_LIMIT`] 字节，char boundary 对齐；非 ASCII
/// lossy 安全——载荷永不超预算）。
pub(crate) fn snippet(text: &str) -> String {
    if text.len() <= SNIPPET_LIMIT {
        return text.to_string();
    }
    let mut end = SNIPPET_LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// `Content-Range: bytes a-b/total` 解析（`*/total` 的 416 形态不在此
/// 面——那不进 206 校验路径）。
pub(crate) fn parse_content_range(header: &str) -> Option<(u64, u64, Option<u64>)> {
    let spec = header.trim().strip_prefix("bytes ")?.trim();
    let (range, total) = spec.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.trim().parse().ok()?;
    let end = end.trim().parse().ok()?;
    let total = match total.trim() {
        "*" => None,
        value => Some(value.parse().ok()?),
    };
    Some((start, end, total))
}

// ----------------------------------------------- 可行动文案（R3 双通道）---
// `Unauthorized{false}` 无载荷变体的诊断面：文案经 warn 通道留痕（R3
// 双通道——lib.rs 单测钉字面，集成测试断言变体）。

/// 无凭据配置遇 401（键名可行动）。
pub(crate) fn message_missing_credentials() -> String {
    "the webdav server requires authentication but no credentials are configured: set \
     webdav_username and webdav_password in config.toml (or the volume file), then retry"
        .to_string()
}

/// 服务器要求 NTLM/Negotiate（明确不支持 + 出路）。
pub(crate) fn message_unsupported_scheme(scheme: &str) -> String {
    format!(
        "the webdav server demands {scheme} authentication, which this driver does not support \
         (only Basic and Digest are implemented); switch the server to Basic or Digest (or set \
         webdav_auth to match what the server offers)"
    )
}

/// 凭据被拒（协商已尽）。
pub(crate) fn message_credentials_rejected() -> String {
    "webdav credentials were rejected (after the one renegotiation the driver performs): \
     verify webdav_username/webdav_password against the server"
        .to_string()
}

/// auth=basic 不协商（§4.2 键语义 + 出路）。
pub(crate) fn message_basic_mode_refused(challenges: &[String]) -> String {
    let offered = if challenges.is_empty() {
        "an authentication challenge".to_string()
    } else {
        challenges.join(", ")
    };
    format!(
        "webdav_auth=basic does not negotiate and the server refused Basic (it offers {offered}); \
         verify the credentials, or set webdav_auth=auto/digest when the server requires Digest"
    )
}

// ------------------------------------------------------- 错误映射（§4.4）---

/// 传输类错误映射（§4.4 末行）：reqwest **类型化**判定（`is_connect`/
/// `is_timeout`——§4.1 注记：不搞字符串信标），统一归 `Unavailable`、
/// 消息按类型成形。
pub(crate) fn map_transport(error: reqwest::Error) -> StorageError {
    if error.is_connect() {
        StorageError::Unavailable(format!("webdav connection failed: {error}"))
    } else if error.is_timeout() {
        StorageError::Unavailable(format!("webdav request timed out: {error}"))
    } else {
        StorageError::Unavailable(format!("webdav transport error: {error}"))
    }
}

/// M7 成员失败映射（§4.4「成员失败按成员映射」）：内层 404 →
/// `NotFound`（既有语义——成员确不存在），其余非 2xx → `Unavailable`
/// 带码（真类；折叠成 NotFound 会误导「路径不存在」的排查方向）。
pub(crate) fn map_member_failure(status: u16) -> StorageError {
    match status {
        404 => StorageError::NotFound,
        code => StorageError::Unavailable(format!("webdav PROPFIND member failed (HTTP {code})")),
    }
}

/// HTTP 状态映射（§4.4 表——读写共用的通用行；MKCOL/MOVE/PUT 的 verb
/// 专用行——MKCOL 405→Exists / MKCOL·MOVE 409·403·500 缺父处置 /
/// MOVE 412 复核——在各动词方法内先行拦截，不经过本函数）。
///
/// `snippet` 是截断诊断片段（body 片段 + url；3xx 场景由调用方组装进
/// Location 提示）。
pub(crate) fn map_status(status: reqwest::StatusCode, verb: &str, snippet: &str) -> StorageError {
    match status.as_u16() {
        404 => StorageError::NotFound,
        // 认证协商已尽（驱动先自救的契约在 execute 的 401 路径履行）。
        401 | 403 => StorageError::Unauthorized { recoverable: false },
        // 通用面：405（动词不被允许）/409（状态冲突）按非法调用归类
        //（MKCOL/MOVE 的专用形态已在动词方法内拦截）。
        405 | 409 => StorageError::Invalid,
        410 => StorageError::NotFound,
        416 => StorageError::Unavailable(format!(
            "webdav 416 range not satisfiable leaked to {verb}: {snippet}"
        )),
        429 => StorageError::Unavailable(format!(
            "webdav rate limited (HTTP 429) after exhausting the retry budget: {snippet}"
        )),
        507 => StorageError::Io(format!("webdav insufficient storage (HTTP 507): {snippet}")),
        code if (500..600).contains(&code) => StorageError::Unavailable(format!(
            "webdav server error (HTTP {code}) after exhausting the retry budget: {snippet}"
        )),
        code if (300..400).contains(&code) => StorageError::Unavailable(format!(
            "unexpected webdav redirect (HTTP {code}) — the client does not follow redirects: \
             {snippet}"
        )),
        code => StorageError::Unavailable(format!("webdav {verb} failed (HTTP {code}): {snippet}")),
    }
}

// --------------------------------------------------------- 排空（复用纪律）---

/// 有界排空响应体（连接复用纪律——rs-f4ss `drain_response` 资产）：读
/// 至多 [`DRAIN_LIMIT`]；超限或读错即放弃（连接关闭，不无界缓冲）。
async fn drain_bounded(mut response: reqwest::Response) {
    let mut read = 0usize;
    while read < DRAIN_LIMIT {
        match response.chunk().await {
            Ok(Some(chunk)) => read += chunk.len(),
            _ => break,
        }
    }
}

/// 错误面的有界读取（M3）：读至多 [`DRAIN_LIMIT`] 字节即封顶，超出部
/// 分随连接放弃——错误页（含超大 HTML 错误体）从不无界缓冲。返回
/// lossy UTF-8（[`diagnostic`] 的原料——`snippet` 在其后再截 200 字节
/// 进载荷）；读错时以已收部分如实成文（对齐旧 `unwrap_or_default` 的
/// 「不因诊断读失败吞原错误」行为）。
async fn body_bounded(mut response: reqwest::Response) -> String {
    let mut out: Vec<u8> = Vec::new();
    while out.len() < DRAIN_LIMIT {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let take = chunk.len().min(DRAIN_LIMIT - out.len());
                out.extend_from_slice(&chunk[..take]);
            }
            _ => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 成功体的封顶读取（M3）：读至多 `cap` 字节即止——服务器多给的部分
/// 随连接放弃，读取量恒 ≤ `cap`，不随 body 实长膨胀（200-回退整读的
/// OOM 面收口）。预分配取 `cap` 与 1 MiB 的较小者——`cap` 来自 stat
/// 报告的窗口终点，服务器谎报大尺寸时不可据此预支内存。
async fn read_capped(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<bytes::Bytes, StorageError> {
    let mut out: Vec<u8> = Vec::with_capacity(cap.min(1024 * 1024));
    while out.len() < cap {
        match response.chunk().await.map_err(map_transport)? {
            Some(chunk) => {
                let take = chunk.len().min(cap - out.len());
                out.extend_from_slice(&chunk[..take]);
            }
            None => break,
        }
    }
    Ok(bytes::Bytes::from(out))
}

/// PROPFIND 深度（`0` = stat 语义，`1` = 列目录语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Depth {
    Zero,
    One,
}

impl Depth {
    /// 头值形态。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Depth::Zero => "0",
            Depth::One => "1",
        }
    }
}
