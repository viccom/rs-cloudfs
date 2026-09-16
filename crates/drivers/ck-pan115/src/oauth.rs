//! 115 开放平台认证层（Phase 5 / 115-1）——device-code PKCE 三端点 +
//! refresh wire + 持久化回调。
//!
//! 语义契约（115-0 spike 真机验证，`examples/pan115_spike/src/auth.rs`；
//! K69.1）：
//!
//! - **device-code PKCE 流**（路径丙：公共 client_id 自铸，全程无
//!   secret）：`authDeviceCode`（载荷仅 `client_id` + `code_challenge`）
//!   → `get/status` 长轮询（~30s 服务端保持；等待响应 data 为**空
//!   对象**）→ `deviceCodeToToken`（仅 `uid` + `code_verifier`）；
//! - **PKCE 形态**：code_verifier 64 字符（RFC 7636 unreserved+marks）；
//!   code_challenge = **STANDARD base64（带填充，非 urlsafe）** 的
//!   SHA-256(verifier)，`code_challenge_method=sha256`；
//! - **refresh**（`/open/refreshToken`）载荷仅 `refresh_token` 一个
//!   字段；**一次一换**（响应返回全新 token 对，旧 refresh_token 即刻
//!   作废）；**官方频控严禁频繁**——驱动内只在 401*/99 触发时至多刷
//!   一次（client.rs dispatch 状态机）；
//! - **token 有效期 7200s**（`expires_in`）；
//! - **QR 窗口 ~5 分钟**：过期后 `get.status` 快拒 `40199002`
//!   （"key invalid"），文档形态 `status:-1` 未在真机出现——两者都
//!   归一为 [`PollStatus::Expired`]；`-2` = 用户在 App 内取消。
//!
//! 刷新的**编排**（单飞锁 + 陈旧检查 + on-arrival 持久化）在
//! [`crate::api::Pan115Client::refresh_once`]；本模块只做单次 wire
//! 请求与错误归一（ck-baidu oauth.rs 的同款分工）。
//!
//! 三端点函数是 115-4 setup（扫码引导）的接线面；端点 base 为参数
//! （生产常量见 [`crate::DEFAULT_PASSPORT_BASE`] / [`crate::DEFAULT_QRCODE_BASE`]，
//! 测试注入 mock）。

use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;
use sha2::Digest;

use cloudkit_storage::StorageError;

/// 刷新产物持久化回调（K13 形态，ck-baidu `TokenStore` 同款裁决）。
///
/// 由组合根（115-4 装配批的 ConfigTokenStore 桥接——baidu 先例）实现
/// 并经 [`crate::Pan115Params::token_store`] 注入：驱动自助刷新成功后
/// 回调本方法，把新 token 对回写卷配置键 `pan115_access_token` /
/// `pan115_refresh_token`。驱动内不依赖任何 core 类型（R1）——回调是
/// 驱动 crate 自有契约。
///
/// 生命周期契约：回调在驱动的请求路径上同步发生；实现方不得再回调驱动
/// （防重入），且应自行处理持久化失败（不得丢失新 refresh_token——
/// 一次一换下它是唯一的活值）。
pub trait TokenStore: Send + Sync {
    /// 持久化刷新产物（access_token 与 refresh_token 成对落盘）。
    fn save_tokens(&self, access_token: &str, refresh_token: &str);
}

/// 驱动内当前 token 对（RwLock 保护；刷新成功后整体替换并回调持久化；
/// ck-baidu client.rs `TokenPair` 同款形态）。
///
/// 不实现 `Debug`：派生展开有把凭据印进日志的风险（R3）。
#[derive(Clone)]
pub(crate) struct TokenPair {
    pub(crate) access: String,
    pub(crate) refresh: String,
}

/// `authDeviceCode` 成功响应的 `data`。
#[derive(Debug, Deserialize)]
pub struct DeviceCode {
    pub uid: String,
    pub time: i64,
    /// `https://115.com/scan/dg-<uid>`——编码进 QR PNG 的内容（115-4
    /// setup 的扫码引导面；spike 用 qrcode crate 渲染）。
    pub qrcode: String,
    pub sign: String,
}

/// `get.status` 一次长轮询的归一结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollStatus {
    /// 空 data 对象：尚无人扫码——继续轮询。
    Waiting,
    /// 1：已扫码，待确认。
    Scanned,
    /// 2：已确认——立即换取 token。
    Confirmed,
    /// QR 窗口失效（~5min）：快拒 `40199002` 或 `status:-1`（真机只
    /// 出现前者，两者同归）。
    Expired,
    /// -2：用户在 App 内取消了确认。
    Cancelled,
}

// ---------------------------------------------------------------------------
// setup 向导装配面（115-4b 挂账项收口，2026-09-16）
// ---------------------------------------------------------------------------

/// setup 向导的直连 http client（spike auth.rs `http_client` 同形态：
/// no_proxy + 恒定 UA + 60s 超时——oauth 三端点与 user/info 验证共用）。
pub fn setup_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .user_agent(crate::UA)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("setup http client builds")
}

/// 终端二维码渲染（Dense1x2 半块字符，两列一字符——横向补偿终端
/// 字符的高宽比）。扫码 URL 即 115 App 的登录确认页。
///
/// 错误只来自编码容量（URL 远低于 V1-L 容量上限，实践不可达）——
/// 归一 `Invalid` 而非 panic（向导循环里可上抛）。
pub fn render_qr_terminal(url: &str) -> Result<String, StorageError> {
    use qrcode::render::unicode::Dense1x2;
    use qrcode::QrCode;
    let code = QrCode::with_error_correction_level(url, qrcode::EcLevel::L)
        .map_err(|_e| StorageError::Invalid)?;
    Ok(code
        .render::<Dense1x2>()
        .quiet_zone(true)
        .module_dimensions(2, 1)
        .build())
}

/// RFC 7636 unreserved + marks——spike 验证过被接受的字符集。
pub(crate) const VERIFIER_CHARSET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// PKCE code_verifier：64 字符（spec 允许 43..=128；spike 取 64）。
pub fn gen_code_verifier() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    (0..64)
        .map(|_| VERIFIER_CHARSET[rng.random_range(0..VERIFIER_CHARSET.len())] as char)
        .collect()
}

/// code_challenge = STANDARD base64（带填充，**非 urlsafe**）的
/// SHA-256(verifier)——spike 实测被接受的形态；方法名 `sha256`。
pub fn pkce_challenge(verifier: &str) -> String {
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(digest)
}

// ---------------------------------------------------------------------------
// wire 请求（单次；错误归一 StorageError——R3：错误文本绝不携带 token 值）
// ---------------------------------------------------------------------------

/// 请求超时：60s——`get.status` 的 ~30s 服务端长轮询必须能返回。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// `POST {passport_base}/open/authDeviceCode`（x-www-form-urlencoded）。
pub async fn auth_device_code(
    http: &reqwest::Client,
    passport_base: &str,
    client_id: &str,
    verifier: &str,
) -> Result<DeviceCode, StorageError> {
    let challenge = pkce_challenge(verifier);
    let resp = http
        .post(format!("{passport_base}/open/authDeviceCode"))
        .timeout(REQUEST_TIMEOUT)
        .form(&[
            ("client_id", client_id),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "sha256"),
        ])
        .send()
        .await
        .map_err(|e| {
            StorageError::Unavailable(format!("authDeviceCode transport: {}", e.without_url()))
        })?;
    let stage = "authDeviceCode";
    let (http_status, env) = super::api::read_envelope(resp, stage).await?;
    let data = env.ok(stage, http_status)?;
    serde_json::from_value(data)
        .map_err(|e| StorageError::Unavailable(format!("authDeviceCode data parse: {e}")))
}

/// 一次 `GET {qrcode_base}/get/status/` 长轮询（~30s 服务端保持）。
///
/// 传输失败上抛 `Unavailable`（调用方的轮询策略决定重试）；QR 窗口
/// 失效（40199002 / status:-1）归一为 [`PollStatus::Expired`]——不是
/// 错误。
pub async fn poll_status(
    http: &reqwest::Client,
    qrcode_base: &str,
    uid: &str,
    time: i64,
    sign: &str,
) -> Result<PollStatus, StorageError> {
    let resp = http
        .get(format!("{qrcode_base}/get/status/"))
        .timeout(REQUEST_TIMEOUT)
        .query(&[
            ("uid", uid.to_string()),
            ("time", time.to_string()),
            ("sign", sign.to_string()),
            ("_", now_unix().to_string()),
        ])
        .send()
        .await
        .map_err(|e| {
            StorageError::Unavailable(format!("get.status transport: {}", e.without_url()))
        })?;
    let stage = "get.status";
    let (_http_status, env) = super::api::read_envelope(resp, stage).await?;
    if !env.is_ok() {
        // 真机实测（K69.9）：QR 窗口（~5min）失效后该端点对每次轮询快拒
        // {"state":0,"code":40199002,"message":"key invalid"}——status:-1
        // 从未在此路径出现；两者同归 Expired。
        if env.code == 40199002 || env.errno == 40199002 {
            return Ok(PollStatus::Expired);
        }
        return Err(env.to_storage_error(stage));
    }
    match env.data.get("status").and_then(|v| v.as_i64()) {
        None => Ok(PollStatus::Waiting), // 空 {} data = 仍在等待
        Some(1) => Ok(PollStatus::Scanned),
        Some(2) => Ok(PollStatus::Confirmed),
        Some(-1) => Ok(PollStatus::Expired),
        Some(-2) => Ok(PollStatus::Cancelled),
        Some(other) => Err(StorageError::Unavailable(format!(
            "get.status: unknown status {other}"
        ))),
    }
}

/// `POST {passport_base}/open/deviceCodeToToken`——与 authDeviceCode
/// **同一个** verifier 换取 token 对。
pub async fn device_code_to_token(
    http: &reqwest::Client,
    passport_base: &str,
    uid: &str,
    verifier: &str,
) -> Result<(String, String), StorageError> {
    let resp = http
        .post(format!("{passport_base}/open/deviceCodeToToken"))
        .timeout(REQUEST_TIMEOUT)
        .form(&[("uid", uid), ("code_verifier", verifier)])
        .send()
        .await
        .map_err(|e| {
            StorageError::Unavailable(format!("deviceCodeToToken transport: {}", e.without_url()))
        })?;
    let stage = "deviceCodeToToken";
    let (http_status, env) = super::api::read_envelope(resp, stage).await?;
    let data = env.ok(stage, http_status)?;
    parse_token_pair(data, stage)
}

/// `POST {passport_base}/open/refreshToken`——载荷恰一个字段。
///
/// **官方频控严禁频繁**（K69.1）：调用方（client 的 dispatch 状态机）
/// 保证一个逻辑调用至多刷一次。响应 = 全新 token 对（一次一换，旧
/// refresh_token 即刻作废）。
pub(crate) async fn refresh_grant(
    http: &reqwest::Client,
    passport_base: &str,
    refresh_token: &str,
) -> Result<TokenPair, StorageError> {
    let resp = http
        .post(format!("{passport_base}/open/refreshToken"))
        .timeout(REQUEST_TIMEOUT)
        .form(&[("refresh_token", refresh_token)])
        .send()
        .await
        .map_err(|e| {
            // without_url：reqwest 错误串内嵌完整 URL（form 体不进 URL，
            // 但保持与 baidu 一致的剥离纪律）。
            StorageError::Unavailable(format!("refreshToken transport: {}", e.without_url()))
        })?;
    let stage = "refreshToken";
    let (http_status, env) = super::api::read_envelope(resp, stage).await?;
    let data = env.ok(stage, http_status)?;
    let pair = parse_token_pair(data, stage)?;
    Ok(TokenPair {
        access: pair.0,
        refresh: pair.1,
    })
}

/// 成功 data → token 对（access/refresh 必须齐备——半截响应可能携带
/// 一半新凭据对，缺失字段按协议异常 `Unavailable` 上抛且**不回显**
/// data 内容，baidu oauth.rs 同款裁决）。
fn parse_token_pair(
    data: serde_json::Value,
    stage: &'static str,
) -> Result<(String, String), StorageError> {
    let access = data
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| StorageError::Unavailable(format!("{stage}: missing access_token")))?;
    let refresh = data
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| StorageError::Unavailable(format!("{stage}: missing refresh_token")))?;
    Ok((access.to_string(), refresh.to_string()))
}

/// Unix 秒（get.status 的缓存穿透参数 `_`）。
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QR 渲染面（纯函数）：非空、多行、含半块字符（▀▄█ 族——终端
    /// 可显示形态），且静区存在（quiet_zone 边缘行是全空格行）。
    #[test]
    fn render_qr_terminal_produces_a_scannable_block_matrix() {
        let url = "https://115.com/scan/dg-0123456789abcdef0123456789abcdef";
        let art = render_qr_terminal(url).expect("render");
        assert!(art.lines().count() > 10, "a QR matrix has many rows");
        assert!(
            art.contains('█') || art.contains('▀') || art.contains('▄'),
            "dense half-block glyphs present: {art}"
        );
        let first = art.lines().next().expect("row");
        assert!(
            first.trim().is_empty(),
            "the quiet zone renders as blank margins: {first:?}"
        );
    }

    /// PKCE 形态钉死（spike 实测被接受的形态）：64 字符合法字符集 +
    /// challenge = STANDARD base64 的 SHA-256。
    #[test]
    fn pkce_shapes_hold() {
        let verifier = gen_code_verifier();
        assert_eq!(verifier.len(), 64);
        assert!(
            verifier.bytes().all(|b| VERIFIER_CHARSET.contains(&b)),
            "charset"
        );
        let challenge = pkce_challenge(&verifier);
        assert_eq!(challenge.len(), 44, "SHA-256 → base64 with padding");
        assert!(
            !challenge.contains('-') && !challenge.contains('_'),
            "STANDARD base64, not urlsafe"
        );
    }
}
