//! 115 open-platform endpoint wrappers (passportapi / qrcodeapi / proapi).
//! Every shape below is a live-probed fact (2026-09-16):
//!
//! - all endpoints are reached DIRECT (client built with `.no_proxy()`)
//! - unified envelope `{state, code, message, data, errno}`,
//!   success = `state==1 && errno==0`
//! - `get.status` long-polls ~30s server-side; waiting responses carry an
//!   EMPTY data object (no `status` key) — the client timeout is 60s

use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use base64::Engine as _;
use rand::Rng;
use serde::Deserialize;
use sha2::Digest;

/// UA validated against the 115 WAF in earlier probes — do not change.
pub const UA: &str = "cydrive-pan115-spike/0.1";

const AUTH_DEVICE_CODE: &str = "https://passportapi.115.com/open/authDeviceCode";
const QR_STATUS: &str = "https://qrcodeapi.115.com/get/status/";
const DEVICE_CODE_TO_TOKEN: &str = "https://passportapi.115.com/open/deviceCodeToToken";
const USER_INFO: &str = "https://proapi.115.com/open/user/info";
const REFRESH_TOKEN: &str = "https://passportapi.115.com/open/refreshToken";

// ---------------------------------------------------------------------------
// unified envelope
// ---------------------------------------------------------------------------

/// Envelope with lenient defaults: `get.status` waiting responses omit both
/// `errno` and `message`; error responses (live-probed, e.g. bad Bearer on
/// user/info -> http 200 `{"state":false,"code":40140123,...}`) carry a
/// BOOLEAN `state` and put the error code in `code`, so `state` is kept as a
/// raw Value and `code` is surfaced alongside `errno`.
#[derive(Debug, Deserialize)]
pub struct Envelope {
    pub state: serde_json::Value,
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub errno: i64,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub data: serde_json::Value,
    /// Top-level sibling of `data` on `ufile/files` (total row count) —
    /// absent elsewhere, hence defaulted.
    #[serde(default)]
    pub count: i64,
}

impl Envelope {
    /// Success = `state` numeric 1 OR boolean true, with errno 0 (live-probed
    /// 2026-09-16: proapi user/info SUCCESS returns `{"state":true,...}`,
    /// errors `state:false`; passportapi uses numeric 1/0).
    pub fn is_ok(&self) -> bool {
        (self.state.as_i64() == Some(1) || self.state.as_bool() == Some(true)) && self.errno == 0
    }

    fn ok(self, stage: &str, http: reqwest::StatusCode) -> Result<serde_json::Value> {
        if self.is_ok() {
            Ok(self.data)
        } else {
            // Error text carries state/code/message only — never `data`
            // (token endpoints return secrets inside data on success).
            bail!(
                "{stage}: rejected http={} state={} code={} errno={} message={}",
                http.as_u16(),
                self.state,
                self.code,
                self.errno,
                self.message
            )
        }
    }
}

/// Read a response into the envelope; unparseable bodies fail with the HTTP
/// status only (body text is never echoed — it may embed secrets).
pub async fn envelope(
    resp: reqwest::Response,
    stage: &str,
) -> Result<(reqwest::StatusCode, Envelope)> {
    let http = resp.status();
    let body = resp
        .text()
        .await
        .with_context(|| format!("{stage}: read body"))?;
    let env: Envelope = serde_json::from_str(&body).map_err(|_| {
        anyhow!(
            "{stage}: response not in envelope form http={} len={}",
            http.as_u16(),
            body.len()
        )
    })?;
    Ok((http, env))
}

// ---------------------------------------------------------------------------
// http client + PKCE
// ---------------------------------------------------------------------------

/// Direct client (`.no_proxy()` — this box reaches 115 without a proxy) with
/// a 60s per-request timeout so the ~30s server-side long-poll can return.
pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .user_agent(UA)
        .timeout(Duration::from_secs(60))
        .build()
        .context("build http client")
}

/// RFC 7636 unreserved + marks, exactly the charset the prober validated.
const VERIFIER_CHARSET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// PKCE code_verifier: 64 chars (spec range is 43..=128).
pub fn gen_code_verifier() -> String {
    let mut rng = rand::thread_rng();
    (0..64)
        .map(|_| VERIFIER_CHARSET[rng.gen_range(0..VERIFIER_CHARSET.len())] as char)
        .collect()
}

/// code_challenge = STANDARD base64 (with padding, NOT urlsafe) of
/// sha256(verifier) — probed form; `code_challenge_method=sha256`.
pub fn pkce_challenge(verifier: &str) -> String {
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(digest)
}

// ---------------------------------------------------------------------------
// endpoints
// ---------------------------------------------------------------------------

/// `data` of a successful authDeviceCode call.
#[derive(Debug, Deserialize)]
pub struct DeviceCode {
    pub uid: String,
    pub time: i64,
    /// https://115.com/scan/dg-<uid> — encode this into the QR PNG.
    pub qrcode: String,
    pub sign: String,
}

/// POST /open/authDeviceCode (x-www-form-urlencoded).
pub async fn auth_device_code(
    client: &reqwest::Client,
    client_id: &str,
    verifier: &str,
) -> Result<DeviceCode> {
    let challenge = pkce_challenge(verifier);
    let resp = client
        .post(AUTH_DEVICE_CODE)
        .form(&[
            ("client_id", client_id),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "sha256"),
        ])
        .send()
        .await
        .context("authDeviceCode transport")?;
    let (http, env) = envelope(resp, "authDeviceCode").await?;
    let data = env.ok("authDeviceCode", http)?;
    serde_json::from_value(data).context("authDeviceCode: parse data")
}

/// One get.status long-poll (~30s server hold). Transport errors are
/// surfaced as Err for the caller's retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollStatus {
    /// Empty data object: nobody scanned yet — keep polling.
    Waiting,
    /// 1: scanned, awaiting confirmation.
    Scanned,
    /// 2: confirmed — exchange the code for tokens now.
    Confirmed,
    /// QR window lapsed (~5 min): fast-reject 40199002 "key invalid" (the
    /// measured form; the documented -1 never showed up live), or status -1.
    Expired,
    /// -2: user cancelled in the app.
    Cancelled,
}

pub async fn poll_status(
    client: &reqwest::Client,
    uid: &str,
    time: i64,
    sign: &str,
) -> Result<PollStatus> {
    let resp = client
        .get(QR_STATUS)
        .query(&[
            ("uid", uid.to_string()),
            ("time", time.to_string()),
            ("sign", sign.to_string()),
            ("_", crate::now_unix().to_string()),
        ])
        .send()
        .await
        .context("get.status transport")?;
    let (http, env) = envelope(resp, "get.status").await?;
    if !env.is_ok() {
        // Measured live (2026-09-16): once the ~5-minute QR window lapses the
        // endpoint fast-rejects every poll with `{"state":0,"code":40199002,
        // "message":"key invalid"}` — status:-1 never appears on this path.
        // Map that to Expired; anything else is a genuine error.
        if env.code == 40199002 || env.errno == 40199002 {
            return Ok(PollStatus::Expired);
        }
        bail!(
            "get.status: rejected http={} state={} code={} errno={} message={}",
            http.as_u16(),
            env.state,
            env.code,
            env.errno,
            env.message
        );
    }
    match env.data.get("status").and_then(|v| v.as_i64()) {
        None => Ok(PollStatus::Waiting), // empty {} data = still waiting
        Some(1) => Ok(PollStatus::Scanned),
        Some(2) => Ok(PollStatus::Confirmed),
        Some(-1) => Ok(PollStatus::Expired),
        Some(-2) => Ok(PollStatus::Cancelled),
        Some(other) => bail!("get.status: unknown status {other}"),
    }
}

/// `data` of deviceCodeToToken / refreshToken (same shape). Deliberately NO
/// `Debug` derive: an accidental `{:?}` would print raw tokens.
#[derive(Deserialize)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

/// POST /open/deviceCodeToToken with the SAME verifier used in authDeviceCode.
pub async fn device_code_to_token(
    client: &reqwest::Client,
    uid: &str,
    verifier: &str,
) -> Result<TokenPair> {
    let resp = client
        .post(DEVICE_CODE_TO_TOKEN)
        .form(&[("uid", uid), ("code_verifier", verifier)])
        .send()
        .await
        .context("deviceCodeToToken transport")?;
    let (http, env) = envelope(resp, "deviceCodeToToken").await?;
    let data = env.ok("deviceCodeToToken", http)?;
    serde_json::from_value(data).context("deviceCodeToToken: parse data")
}

/// POST /open/refreshToken — form carries exactly one field. Rate-limited by
/// 115: callers must invoke this at most once per run.
pub async fn refresh_token_pair(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<TokenPair> {
    let resp = client
        .post(REFRESH_TOKEN)
        .form(&[("refresh_token", refresh_token)])
        .send()
        .await
        .context("refreshToken transport")?;
    let (http, env) = envelope(resp, "refreshToken").await?;
    let data = env.ok("refreshToken", http)?;
    serde_json::from_value(data).context("refreshToken: parse data")
}

/// GET /open/user/info with `Authorization: Bearer <access_token>`; returns
/// the `data` object (no credentials inside).
pub async fn user_info(client: &reqwest::Client, access_token: &str) -> Result<serde_json::Value> {
    let resp = client
        .get(USER_INFO)
        .bearer_auth(access_token)
        .send()
        .await
        .context("user/info transport")?;
    let (http, env) = envelope(resp, "user/info").await?;
    env.ok("user/info", http)
}
