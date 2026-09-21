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

use crate::auth::{authorization_header, parse_challenge, rand_cnonce, AuthState, DigestSession};
use crate::config::{AuthMode, WebdavParams};
use crate::urls::collection_url;
use crate::xml::{self, PropfindEntry};

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
/// 的公共载体；WD3 写面若需要成员级状态可在此扩展）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PropfindOutcome {
    pub entries: Vec<PropfindEntry>,
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
}

impl WebdavClient {
    /// 构造（纯本地：建 reqwest Client，不碰网络——惰性连接归
    /// reqwest 池）。
    ///
    /// `accept_invalid_certs = true` 时一次性 warn（D3 开洞必须可
    /// 观察——doctor 与启动日志共用该通道）。
    pub(crate) fn new(params: &WebdavParams) -> Result<Self, StorageError> {
        if params.accept_invalid_certs {
            tracing::warn!(
                target: "ck_webdav::client",
                url = %params.url,
                "webdav_accept_invalid_certs is enabled: TLS certificates are NOT verified for \
                 this volume (self-signed NAS escape hatch) — do not enable this on untrusted \
                 networks"
            );
        }
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
        })
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
                    // 已在 handle_401 留痕；这里补「重发后仍拒」的事实）。
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

        if self.username.is_none() || self.password.is_none() {
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
            tracing::warn!(
                target: "ck_webdav::client",
                verb,
                "{}",
                message_basic_mode_refused(&challenges)
            );
            return false;
        }
        // 多 challenge 服务器上优先 Digest（首个可解析者）。
        for challenge_value in &challenges {
            if let Ok(challenge) = parse_challenge(challenge_value) {
                let stale = challenge.stale;
                let realm = challenge.realm.clone();
                let mut guard = self.auth.lock().await;
                *guard = AuthState::Digest(DigestSession::from_challenge(&challenge));
                drop(guard);
                // realm/nonce 是服务器签发物，日志安全（模块文档纪律）。
                tracing::info!(
                    target: "ck_webdav::client",
                    verb,
                    stale,
                    realm,
                    "negotiated a Digest session; resending exactly once"
                );
                return true;
            }
        }
        if let Some(scheme) = unsupported_scheme_token(&challenges) {
            tracing::warn!(
                target: "ck_webdav::client",
                verb,
                "{}",
                message_unsupported_scheme(&scheme)
            );
            return false;
        }
        tracing::warn!(
            target: "ck_webdav::client",
            verb,
            "{}",
            message_credentials_rejected()
        );
        false
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
            let text = response.text().await.unwrap_or_default();
            return Err(map_status(status, "PROPFIND", &diagnostic(&text, url)));
        }
        let text = response.text().await.map_err(map_transport)?;
        match xml::parse_multistatus(&text) {
            Ok(rows) => Ok(PropfindOutcome {
                entries: xml::entries(&rows),
            }),
            Err(error) => Err(StorageError::Io(format!(
                "webdav multistatus parse failed: {error}; body: {}",
                snippet(&text)
            ))),
        }
    }

    /// stat 语义的 PROPFIND Depth 0 → 单条目投影（404 → `NotFound`）。
    ///
    /// **尾斜杠重试腿（附录 C ⑧ apache SlashStrict）**：文件形态 URL
    /// （无尾斜杠）遇 3xx → 以集合形态（尾斜杠）重试恰一次——集合
    /// no-slash 301 是「不执行」信号而非错误，跟随到 slashed 形态即
    /// 真值；其余 3xx（如 unexpected_301 旋钮的文件腿）两次都 3xx →
    /// `Unavailable` + Location 提示（§4.4）。
    pub(crate) async fn stat(&self, url: &Url) -> Result<PropfindEntry, StorageError> {
        let response = self
            .execute("PROPFIND", url, &[], None, CONTROL_TIMEOUT)
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
                .execute("PROPFIND", &slashed, &[], None, CONTROL_TIMEOUT)
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
            let text = response.text().await.unwrap_or_default();
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
        xml::entries(&rows)
            .into_iter()
            .next()
            .ok_or(StorageError::NotFound)
    }

    /// 窗口 GET → `Ok(Some(bytes))`；`Ok(None)` = 416（Range 不可满足
    /// ——复核归调用方：driver reader 的 stat 复核腿，§4.4）。
    ///
    /// - `window = None` → 全量 GET（200 体即答案）；
    /// - `Some((start, end_exclusive))` → `Range: bytes=start-{end-1}`；
    ///   **206 校验**：`Content-Range` 的 rs/re 必须逐字段吻合请求
    ///   （不吻合 → `Io` 带两侧值——§4.5-6）；**200 回退**（服务器无视
    ///   Range——矩阵④）：body 覆盖请求区间 → 截断；不足 → `Io`；
    /// - GET 在重试白名单内（窗口间不跨窗重试——driver 文档注明）。
    pub(crate) async fn get_range(
        &self,
        url: &Url,
        window: Option<(u64, u64)>,
    ) -> Result<Option<bytes::Bytes>, StorageError> {
        let mut headers: Vec<(&'static str, String)> = Vec::new();
        if let Some((start, end)) = window {
            // 半开 → 闭区间头（RFC 9110 Range 形态）；退化窗口不发现。
            if end > start {
                headers.push(("range", format!("bytes={}-{}", start, end - 1)));
            }
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
            let text = response.text().await.unwrap_or_default();
            return Err(map_status(status, "GET", &diagnostic(&text, url)));
        }
        let content_range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = response.bytes().await.map_err(map_transport)?;
        let Some((start, end)) = window else {
            return Ok(Some(bytes));
        };
        if end <= start {
            return Ok(Some(bytes::Bytes::new()));
        }
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
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
            // 200 族回退：body 覆盖请求区间 → 截断（§4.4；不足 → Io）。
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
    /// stager 的 lost-ACK 重放窗协议在 WD3 承担恢复语义）。响应体做有
    /// 界排空（连接杀在 PUT 上的可观察面——排空即传输错误浮现）。
    pub(crate) async fn put(&self, url: &Url, body: bytes::Bytes) -> Result<(), StorageError> {
        let response = self
            .execute("PUT", url, &[], Some(body), CONTROL_TIMEOUT)
            .await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(map_status(status, "PUT", &diagnostic(&text, url)));
        }
        drain_bounded(response).await;
        Ok(())
    }

    /// OPTIONS（doctor 探活 + Class 1 能力发现；幂等可重试）。2xx →
    /// `Ok(())`；非 2xx → [`map_status`]（认证失败以 Unauthorized 浮现
    /// ——transport 面 connect 的探活语义）。
    pub(crate) async fn options(&self) -> Result<(), StorageError> {
        let response = self
            .execute("OPTIONS", &self.base.clone(), &[], None, CONTROL_TIMEOUT)
            .await?;
        let status = response.status();
        if status.is_success() {
            drain_bounded(response).await;
            return Ok(());
        }
        let text = response.text().await.unwrap_or_default();
        Err(map_status(
            status,
            "OPTIONS",
            &diagnostic(&text, &self.base),
        ))
    }

    // -------------------------------------------- 写侧动词占位（WD3 接线）---

    /// MKCOL（恒带尾斜杠——附录 C ⑥/⑧；rclone 已存在 201 幂等陷阱由
    /// 驱动 stat 预检吸收）。**永不自动重试**（非幂等）。
    #[allow(dead_code)] // WD3 写路径批接线（stager/驱动写面落地后进入使用）
    pub(crate) async fn mkcol(&self, _url: &Url) -> Result<(), StorageError> {
        // TODO(wd3): 写路径批接线。
        Err(StorageError::Unsupported)
    }

    /// DELETE（集合腿恒带尾斜杠——附录 C ⑦）。**永不自动重试**。
    #[allow(dead_code)] // WD3 写路径批接线
    pub(crate) async fn delete(&self, _url: &Url) -> Result<(), StorageError> {
        // TODO(wd3): 写路径批接线。
        Err(StorageError::Unsupported)
    }

    /// MOVE（恒显式 `Overwrite` 头 + 绝对 `Destination`——附录 C ⑤/⑩；
    /// 目录腿源与 Destination 均带尾斜杠）。**永不自动重试**。
    #[allow(dead_code)] // WD3 写路径批接线
    pub(crate) async fn move_(
        &self,
        _from: &Url,
        _to: &Url,
        _overwrite: bool,
    ) -> Result<(), StorageError> {
        // TODO(wd3): 写路径批接线。
        Err(StorageError::Unsupported)
    }

    /// PROPPATCH（D2 降级后 generic 不写 mtime——仅作动词面占位保留，
    /// 驱动面无调用方；WD3 conformance 批复核后若无消费面则随批移除）。
    /// **永不自动重试**。
    #[allow(dead_code)] // WD3 复核去留
    pub(crate) async fn proppatch(&self, _url: &Url) -> Result<(), StorageError> {
        // TODO(wd3): conformance 批复核去留。
        Err(StorageError::Unsupported)
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

/// HTTP 状态映射（§4.4 表——读写共用，WD3 写面扩展 MKCOL/MOVE 专用行）。
///
/// `snippet` 是截断诊断片段（body 片段 + url；3xx 场景由调用方组装进
/// Location 提示）。
pub(crate) fn map_status(status: reqwest::StatusCode, verb: &str, snippet: &str) -> StorageError {
    match status.as_u16() {
        404 => StorageError::NotFound,
        // 认证协商已尽（驱动先自救的契约在 execute 的 401 路径履行）。
        401 | 403 => StorageError::Unauthorized { recoverable: false },
        // 读面：405（对读动词不允许）/409（状态冲突）按非法调用归类；
        // 写面（MKCOL 405→Exists、409→隐式建父）WD3 扩展。
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
