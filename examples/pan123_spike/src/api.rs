//! HTTP layer: web-identity clients (D5 — no android simulation), envelope
//! helpers, dynamic_params signature form, and response redaction.

use std::time::Duration;

use anyhow::Result;
use rand::Rng;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;

use crate::state::Tokens;

/// Primary API domain (123panNextGen 2026-08-29+ truth).
pub const PRIMARY_BASE: &str = "https://www.123pan.cn";
/// Legacy/sticky-fallback API domain (pan123-rs era primary).
pub const FALLBACK_BASE: &str = "https://api.123278.com";
/// QR login domain (web headers, no auth token).
pub const LOGIN_BASE: &str = "https://login.123pan.com";

/// Browser UA (pan123-rs web form; matches web identity, not android).
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36 Edg/146.0.0.0";

/// A captured probe response: status, final URL, body text and the few
/// headers the spike evidence cares about.
#[derive(Debug, Clone)]
pub struct Resp {
    pub status: u16,
    pub url: String,
    pub body: String,
    pub content_type: Option<String>,
    pub content_range: Option<String>,
    pub content_length: Option<String>,
    pub etag: Option<String>,
    pub location: Option<String>,
    pub set_cookie_masked: Option<String>,
}

impl Resp {
    pub async fn from_response(mut r: reqwest::Response) -> Result<Self> {
        let status = r.status().as_u16();
        let url = r.url().to_string();
        let hdr = |name: &str| -> Option<String> {
            r.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        };
        let content_type = hdr("content-type");
        let content_range = hdr("content-range");
        let content_length = hdr("content-length");
        let etag = hdr("etag");
        let location = hdr("location");
        let set_cookie_masked = hdr("set-cookie").map(|c| crate::state::mask(&c));
        let body = r.text().await?;
        Ok(Self {
            status,
            url,
            body,
            content_type,
            content_range,
            content_length,
            etag,
            location,
            set_cookie_masked,
        })
    }
}

/// The spike's two HTTP faces (123panNextGen dual-session form, §5.12):
/// - `api`: web identity headers + Bearer/sso-token dual head — everything
///   under 123pan API domains.
/// - `transfer`: bare browser UA, no 123pan auth headers — CDN GET /
///   presigned PUT (auth headers are useless or harmful there).
pub struct Spike {
    pub api: reqwest::Client,
    pub transfer: reqwest::Client,
    pub token: Option<String>,
    pub login_uuid: String,
}

fn base_builder(follow: usize) -> reqwest::ClientBuilder {
    let mut b = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::limited(follow));
    // 123pan is domestic: direct by default; explicit proxy env overrides.
    match std::env::var("PAN123_SPIKE_PROXY") {
        Ok(p) if !p.is_empty() => {
            if let Ok(proxy) = reqwest::Proxy::all(&p) {
                b = b.proxy(proxy);
            }
        }
        _ => {
            b = b.no_proxy();
        }
    }
    // IPv4 dial (K18 / plan §5.9 discipline).
    b.local_address(std::net::IpAddr::from(std::net::Ipv4Addr::new(0, 0, 0, 0)))
}

impl Spike {
    /// Build with optional persisted tokens (auth headers applied when the
    /// token is present).
    pub fn build(tokens: Option<Tokens>) -> Result<Self> {
        let mut api_headers = HeaderMap::new();
        fn ins(h: &mut HeaderMap, k: &'static str, v: &str) {
            if let Ok(val) = HeaderValue::from_str(v) {
                h.insert(k, val);
            }
        }
        ins(&mut api_headers, "user-agent", USER_AGENT);
        ins(
            &mut api_headers,
            "accept",
            "application/json, text/plain, */*",
        );
        ins(&mut api_headers, "accept-language", "zh-CN,zh;q=0.9,en;q=0.8");
        ins(&mut api_headers, "app-version", "3");
        ins(&mut api_headers, "origin", "https://yun.123pan.cn");
        ins(&mut api_headers, "referer", "https://yun.123pan.cn/");
        ins(&mut api_headers, "platform", "web");
        // gzip: handled by the reqwest `gzip` feature (auto accept-encoding
        // + transparent decompress — a manual header without the feature
        // yields undecodable bodies).
        let login_uuid = tokens
            .as_ref()
            .map(|t| t.login_uuid.clone())
            .unwrap_or_else(|| crate::state::rand_hex(16));
        ins(&mut api_headers, "loginuuid", &login_uuid);

        let api = base_builder(10).default_headers(api_headers).build()?;
        let mut transfer_headers = HeaderMap::new();
        ins(&mut transfer_headers, "user-agent", USER_AGENT);
        ins(&mut transfer_headers, "accept", "*/*");
        let transfer = base_builder(3)
            .default_headers(transfer_headers)
            .build()?;

        Ok(Self {
            api,
            transfer,
            token: tokens.map(|t| t.token),
            login_uuid,
        })
    }

    fn auth(&self, mut b: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(token) = &self.token {
            b = b
                .header("authorization", format!("Bearer {token}"))
                .header("cookie", format!("sso-token={token}"))
                .header("loginuuid", &self.login_uuid);
        }
        b
    }

    /// Dual-head necessity probe face: send a GET with only ONE of the two
    /// auth heads (`bearer` or `cookie`) — or neither (`none`).
    pub async fn api_get_single_head(
        &self,
        url: &str,
        query: &[(String, String)],
        head: &str,
    ) -> Result<Resp> {
        let b = self.api.get(url).query(query);
        let b = match (head, &self.token) {
            ("bearer", Some(t)) => b
                .header("authorization", format!("Bearer {t}"))
                .header("loginuuid", &self.login_uuid),
            ("cookie", Some(t)) => b
                .header("cookie", format!("sso-token={t}"))
                .header("loginuuid", &self.login_uuid),
            ("none", _) => b, // truly anonymous: no auth, no loginuuid
            _ => self.auth(b),
        };
        Ok(Resp::from_response(b.send().await?).await?)
    }

    pub async fn api_get(&self, url: &str, query: &[(String, String)]) -> Result<Resp> {
        let b = self.auth(self.api.get(url).query(query));
        Ok(Resp::from_response(b.send().await?).await?)
    }

    pub async fn api_post_json(
        &self,
        url: &str,
        body: &Value,
        query: &[(String, String)],
    ) -> Result<Resp> {
        let b = self.auth(self.api.post(url).query(query).json(body));
        Ok(Resp::from_response(b.send().await?).await?)
    }

    /// QR-login endpoints live on login.123pan.com and use their own web
    /// header set (123panNextGen `_qr_headers`); no Bearer token.
    pub async fn qr_get(&self, url: &str, query: &[(String, String)]) -> Result<Resp> {
        let b = self
            .api
            .get(url)
            .query(query)
            .header("loginuuid", &self.login_uuid)
            .header("app-version", "3")
            .header("platform", "web");
        Ok(Resp::from_response(b.send().await?).await?)
    }

    pub async fn qr_post_json(&self, url: &str, body: &Value) -> Result<Resp> {
        let b = self
            .api
            .post(url)
            .json(body)
            .header("loginuuid", &self.login_uuid)
            .header("app-version", "3")
            .header("platform", "web");
        Ok(Resp::from_response(b.send().await?).await?)
    }
}

/// pan123-rs `dynamic_params` signature form: one random numeric key with a
/// `{unix_ts}-{rand}-{rand}` value (plan §3.1: whether the server still
/// requires it is a spike question — 123panNextGen sends none).
pub fn dynamic_params() -> Vec<(String, String)> {
    let mut rng = rand::thread_rng();
    let key: i32 = rng.gen_range(0..i32::MAX);
    let value = format!(
        "{}-{}-{}",
        crate::state::now_unix(),
        rng.gen_range(0..10_000_000u64),
        rng.gen_range(0..10_000_000_000u64)
    );
    vec![(key.to_string(), value)]
}

/// Extract `(code, message)` from a JSON envelope (dual-cased).
pub fn code_of(body: &str) -> (Option<i64>, String) {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return (None, String::new());
    };
    let code = v
        .get("code")
        .or_else(|| v.get("Code"))
        .and_then(|c| c.as_i64());
    let msg = v
        .get("message")
        .or_else(|| v.get("Message"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    (code, msg)
}

/// One-line summary: `HTTP 200 code=0 msg="ok" data_keys=[a,b]`.
pub fn summarize(r: &Resp) -> String {
    let (code, msg) = code_of(&r.body);
    let data_keys = serde_json::from_str::<Value>(&r.body)
        .ok()
        .and_then(|v| {
            v.get("data").and_then(|d| match d {
                Value::Object(m) => Some(
                    m.keys()
                        .map(|k| k.as_str().to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
                _ => None,
            })
        })
        .unwrap_or_default();
    format!(
        "HTTP {} code={:?} msg={:?} data_keys=[{}] len={}",
        r.status,
        code,
        msg,
        data_keys,
        r.body.len()
    )
}

const MASK_KEYS: &[&str] = &[
    "token",
    "Token",
    "access_token",
    "accessToken",
    "sso-token",
    "ssoToken",
    "password",
    "passWord",
    "cookie",
    "Cookie",
];
const PII_KEYS: &[&str] = &[
    "phone",
    "Phone",
    "mobile",
    "Mobile",
    "email",
    "Email",
    "user_name",
    "UserName",
    "realName",
    "passport",
];

fn redact_value(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                if MASK_KEYS.iter().any(|mk| k.contains(mk)) {
                    if let Some(s) = val.as_str() {
                        *val = Value::String(crate::state::mask(s));
                    }
                } else if PII_KEYS.iter().any(|pk| k.contains(pk)) {
                    if let Some(s) = val.as_str() {
                        *val = Value::String(crate::state::mask(s));
                    }
                }
                redact_value(val);
            }
        }
        Value::Array(a) => {
            for item in a.iter_mut() {
                redact_value(item);
            }
        }
        Value::String(s) => {
            // long signed URLs / JWTs: keep the head as shape evidence
            if s.chars().count() > 200 {
                let head: String = s.chars().take(120).collect();
                *s = format!("{head}...[len={}]", s.chars().count());
            }
        }
        _ => {}
    }
}

/// Redact a response body for display: mask token/cookie/PII values and
/// truncate very long strings (signed download URLs). Falls back to plain
/// truncation for non-JSON bodies.
pub fn redact(body: &str) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(mut v) => {
            redact_value(&mut v);
            let s = serde_json::to_string_pretty(&v).unwrap_or_default();
            truncate(&s, 1600)
        }
        Err(_) => truncate(body, 1600),
    }
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let mut end = n;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...[truncated len={}]", &s[..end], s.len())
    }
}

/// Print a probe response line + redacted body.
pub fn show(label: &str, r: &Resp) {
    println!("[{label}] {}", summarize(r));
    if let Some(ct) = &r.content_type {
        println!("         content-type={ct:?}");
    }
    if let Some(cr) = &r.content_range {
        println!("         content-range={cr:?}");
    }
    if let Some(l) = &r.location {
        println!("         location={}", truncate(l, 160));
    }
    if let Some(c) = &r.set_cookie_masked {
        println!("         set-cookie(masked)={c}");
    }
    println!("         body={}", redact(&r.body));
}
