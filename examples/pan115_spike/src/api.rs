//! 115 open-platform file-face + upload-face endpoint wrappers (proapi).
//! Shapes cross-checked against three reference implementations
//! (115-plus-desktop src/api/*, OpenList 115-sdk-go fs.go/upload.go).
//!
//! Business-error classification (task spec):
//! - `401*` (code 40100000..40199999) / `99`  -> token expired (probe exits,
//!   NO auto-refresh: refreshToken is rate-limited)
//! - `20130827`                                -> rate limited
//! - `911`                                     -> human verification required
//!   (STOP EVERYTHING immediately)
//!
//! Every wrapper carries `Authorization: Bearer <token>`; POST bodies are
//! `application/x-www-form-urlencoded` (reqwest `.form()`).

use anyhow::{bail, Result};
use serde_json::Value;

use crate::auth::envelope;

const BASE: &str = "https://proapi.115.com";

// ---------------------------------------------------------------------------
// error classification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    TokenExpired,
    RateLimited,
    HumanVerify,
    Rejected,
}

impl ErrKind {
    pub fn label(self) -> &'static str {
        match self {
            ErrKind::TokenExpired => "token-expired",
            ErrKind::RateLimited => "rate-limited",
            ErrKind::HumanVerify => "HUMAN-VERIFY-REQUIRED",
            ErrKind::Rejected => "rejected",
        }
    }
}

#[derive(Debug)]
pub struct ApiErr {
    pub stage: &'static str,
    pub http: u16,
    pub state: String,
    pub code: i64,
    pub errno: i64,
    pub message: String,
    pub kind: ErrKind,
}

impl std::fmt::Display for ApiErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: http={} state={} code={} errno={} message={} kind={}",
            self.stage,
            self.http,
            self.state,
            self.code,
            self.errno,
            self.message,
            self.kind.label()
        )
    }
}

impl std::error::Error for ApiErr {}

fn classify(code: i64, errno: i64) -> ErrKind {
    if code == 911 || errno == 911 {
        ErrKind::HumanVerify
    } else if code == 20130827 || errno == 20130827 {
        ErrKind::RateLimited
    } else if (40100000..=40199999).contains(&code)
        || matches!((code, errno), (99, _) | (_, 99))
        || (40100000..=40199999).contains(&errno)
    {
        ErrKind::TokenExpired
    } else {
        ErrKind::Rejected
    }
}

/// GET with bearer auth -> (data, count). Non-2xx-envelope becomes ApiErr
/// (classified). Transport / non-JSON failures stay anyhow errors.
async fn api_get(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    query: &[(&str, &str)],
    stage: &'static str,
) -> std::result::Result<(Value, i64), ApiErr> {
    let url = format!("{BASE}{path}");
    let resp = client
        .get(url)
        .bearer_auth(token)
        .query(query)
        .send()
        .await
        .map_err(|e| ApiErr {
            stage,
            http: 0,
            state: "?".into(),
            code: 0,
            errno: 0,
            message: format!("transport: {e}"),
            kind: ErrKind::Rejected,
        })?;
    let (http, env) = envelope(resp, stage).await.map_err(|e| ApiErr {
        stage,
        http: 0,
        state: "?".into(),
        code: 0,
        errno: 0,
        message: format!("{e}"),
        kind: ErrKind::Rejected,
    })?;
    if env.is_ok() {
        Ok((env.data, env.count))
    } else {
        Err(ApiErr {
            stage,
            http: http.as_u16(),
            state: env.state.to_string(),
            code: env.code,
            errno: env.errno,
            message: env.message,
            kind: classify(env.code, env.errno),
        })
    }
}

/// POST form with bearer auth (+ optional per-request User-Agent override —
/// downurl needs the UA that will later fetch the CDN link). -> data.
async fn api_post(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    form: &[(&str, &str)],
    stage: &'static str,
    ua: Option<&str>,
) -> std::result::Result<Value, ApiErr> {
    let url = format!("{BASE}{path}");
    let mut req = client.post(url).bearer_auth(token).form(form);
    if let Some(ua) = ua {
        req = req.header(reqwest::header::USER_AGENT, ua);
    }
    let resp = req.send().await.map_err(|e| ApiErr {
        stage,
        http: 0,
        state: "?".into(),
        code: 0,
        errno: 0,
        message: format!("transport: {e}"),
        kind: ErrKind::Rejected,
    })?;
    let (http, env) = envelope(resp, stage).await.map_err(|e| ApiErr {
        stage,
        http: 0,
        state: "?".into(),
        code: 0,
        errno: 0,
        message: format!("{e}"),
        kind: ErrKind::Rejected,
    })?;
    if env.is_ok() {
        Ok(env.data)
    } else {
        Err(ApiErr {
            stage,
            http: http.as_u16(),
            state: env.state.to_string(),
            code: env.code,
            errno: env.errno,
            message: env.message,
            kind: classify(env.code, env.errno),
        })
    }
}

// ---------------------------------------------------------------------------
// tolerant Value extractors (115 mixes string/number freely)
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

// ---------------------------------------------------------------------------
// file face
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
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

/// One page of `GET /open/ufile/files` -> (rows, total count).
pub async fn list_files_page(
    client: &reqwest::Client,
    token: &str,
    cid: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<ListRow>, i64), ApiErr> {
    let (data, count) = api_get(
        client,
        token,
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

/// Full pagination until accumulated >= count (spec: page-termination rule).
pub async fn list_all(
    client: &reqwest::Client,
    token: &str,
    cid: &str,
    page_limit: i64,
) -> Result<(Vec<ListRow>, i64), ApiErr> {
    let mut all = Vec::new();
    let mut count = 0i64;
    let mut offset = 0i64;
    loop {
        let (rows, c) = list_files_page(client, token, cid, page_limit, offset).await?;
        if count == 0 {
            count = c;
        }
        let got = rows.len() as i64;
        all.extend(rows);
        if got == 0 || (all.len() as i64) >= count || got < page_limit {
            break;
        }
        offset += got;
    }
    Ok((all, count))
}

#[derive(Debug, Clone)]
pub struct InfoEntry {
    pub file_id: String,
    pub file_name: String,
    /// "0" dir / "1" file
    pub file_category: String,
    pub pick_code: String,
    pub sha1: String,
    pub size_byte: i64,
    pub size: String,
}

/// `GET /open/folder/get_info` — data may be an object OR a single-element
/// array (both tolerated). An empty array means "not found" (spec: code
/// 430004) and surfaces as ApiErr with the server code.
pub async fn get_info(client: &reqwest::Client, token: &str, file_id: &str) -> Result<InfoEntry> {
    let (data, _) = api_get(
        client,
        token,
        "/open/folder/get_info",
        &[("file_id", file_id)],
        "folder/get_info",
    )
    .await?;
    let obj = match data {
        Value::Object(_) => data,
        Value::Array(a) if !a.is_empty() => a[0].clone(),
        Value::Array(_) => {
            bail!("folder/get_info: empty data array for {file_id} (not-found shape, code 430004)")
        }
        other => bail!("folder/get_info: unexpected data shape: {other}"),
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

/// `POST /open/folder/add` -> new folder file_id.
pub async fn mkdir(
    client: &reqwest::Client,
    token: &str,
    pid: &str,
    file_name: &str,
) -> Result<String, ApiErr> {
    let data = api_post(
        client,
        token,
        "/open/folder/add",
        &[("pid", pid), ("file_name", file_name)],
        "folder/add",
        None,
    )
    .await?;
    Ok(v_str(&data, "file_id").unwrap_or_default())
}

/// `POST /open/ufile/delete` -> recycle-bin semantics.
pub async fn delete(
    client: &reqwest::Client,
    token: &str,
    file_ids: &str,
    parent_id: &str,
) -> Result<(), ApiErr> {
    api_post(
        client,
        token,
        "/open/ufile/delete",
        &[("file_ids", file_ids), ("parent_id", parent_id)],
        "ufile/delete",
        None,
    )
    .await?;
    Ok(())
}

/// `POST /open/ufile/downurl` — MUST carry the exact UA that will later GET
/// the CDN link (byte-for-byte binding). data is a map keyed by pick_code or
/// fid; take Object.values(data)[0].url.url (most robust).
pub async fn downurl(
    client: &reqwest::Client,
    token: &str,
    pick_code: &str,
    ua: &str,
) -> Result<String> {
    let data = api_post(
        client,
        token,
        "/open/ufile/downurl",
        &[("pick_code", pick_code)],
        "ufile/downurl",
        Some(ua),
    )
    .await?;
    let Value::Object(map) = data else {
        bail!("ufile/downurl: data is not a map");
    };
    let first = map
        .values()
        .next()
        .ok_or_else(|| anyhow::anyhow!("ufile/downurl: empty data map"))?;
    let url = first
        .get("url")
        .and_then(|u| u.get("url"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow::anyhow!("ufile/downurl: data[key].url.url missing"))?;
    Ok(url.to_string())
}

// ---------------------------------------------------------------------------
// upload face
// ---------------------------------------------------------------------------

/// OSS callback config from init/resume: two opaque JSON STRINGS carried
/// verbatim (base64) on the complete/put request. Never printed raw —
/// callback_var embeds upload-scoped auth material.
#[derive(Debug, Clone, Default)]
pub struct UploadCallback {
    pub callback: String,
    pub callback_var: String,
}

impl UploadCallback {
    pub fn len_hint(&self) -> String {
        format!(
            "cb={}B/var={}B",
            self.callback.len(),
            self.callback_var.len()
        )
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

/// `POST /open/upload/init`. Optional second-round fields (pick_code /
/// sign_key / sign_val) are omitted when empty. The parameter list mirrors
/// the form fields one-to-one — kept flat on purpose (spike ergonomics).
#[allow(clippy::too_many_arguments)]
pub async fn upload_init(
    client: &reqwest::Client,
    token: &str,
    file_name: &str,
    file_size: i64,
    target: &str,
    fileid: &str,
    preid: &str,
    pick_code: Option<&str>,
    sign_key: Option<&str>,
    sign_val: Option<&str>,
) -> Result<InitResp, ApiErr> {
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
    let data = api_post(
        client,
        token,
        "/open/upload/init",
        &form,
        "upload/init",
        None,
    )
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

/// STS credentials from `GET /open/upload/get_token`. Secrets — print masked.
#[derive(Debug, Clone)]
pub struct StsToken {
    pub endpoint: String,
    pub access_key_id: String,
    pub access_key_secret: String,
    pub security_token: String,
    pub expiration: String,
}

pub async fn get_token(client: &reqwest::Client, token: &str) -> Result<StsToken, ApiErr> {
    let (data, _) = api_get(
        client,
        token,
        "/open/upload/get_token",
        &[],
        "upload/get_token",
    )
    .await?;
    Ok(StsToken {
        endpoint: v_str(&data, "endpoint").unwrap_or_default(),
        access_key_id: v_str(&data, "AccessKeyId").unwrap_or_default(),
        access_key_secret: v_str(&data, "AccessKeySecret").unwrap_or_default(),
        security_token: v_str(&data, "SecurityToken").unwrap_or_default(),
        expiration: v_str(&data, "Expiration").unwrap_or_default(),
    })
}

#[derive(Debug, Clone)]
pub struct ResumeResp {
    pub pick_code: String,
    pub bucket: String,
    pub object: String,
    pub callback: Option<UploadCallback>,
}

/// `POST /open/upload/resume` (no status field in the response).
pub async fn upload_resume(
    client: &reqwest::Client,
    token: &str,
    file_size: i64,
    target: &str,
    fileid: &str,
    pick_code: &str,
) -> Result<ResumeResp, ApiErr> {
    let data = api_post(
        client,
        token,
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
