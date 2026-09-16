//! Hand-written OSS (Aliyun) V1 signing + the five operations the 115 upload
//! chain needs. Faithful port of aliyun-oss-go-sdk's header signing
//! (oss/auth.go getSignedStr + oss/conn.go getSubResource/getResource), the
//! stack OpenList's 115_open driver rides on — verified against the SDK
//! source (master, 2026-09-16), NOT against the V4 path ali-oss-rs uses.
//!
//! V1 string-to-sign (Content-MD5 always empty here):
//! ```text
//! VERB \n <md5> \n <Content-Type> \n <Date> \n
//! <CanonicalizedOSSHeaders><CanonicalizedResource>
//! ```
//! - CanonicalizedOSSHeaders: every `x-oss-*` header, names lowercased and
//!   sorted ascending, each as `name:value\n` (trailing \n INCLUDED on the
//!   last one — Go SDK behavior).
//! - CanonicalizedResource: `/bucket/object` + `?k=v&k2` over the SIGNED
//!   sub-resource whitelist only, sorted, RAW (unencoded) values; keys with
//!   empty values appear bare.
//! - The URL query carries ALL params (signed + unsigned) sorted and
//!   percent-encoded Go-QueryEscape style.
//! - STS: `x-oss-security-token` header on every request (enters the
//!   canonicalized headers). endpoint is forced https, virtual-host style.

use anyhow::{anyhow, Context as _, Result};
use base64::Engine as _;
use hmac::{Hmac, Mac};

use crate::api::UploadCallback;

/// Signed sub-resource whitelist (subset of the Go SDK signKeyList that the
/// five operations below can produce; includes `sequential`, `callback`,
/// `callback-var`).
const SIGNED_PARAMS: &[&str] = &[
    "callback",
    "callback-var",
    "partNumber",
    "sequential",
    "uploadId",
    "uploads",
];

/// Typed OSS failure so callers can classify (NoSuchUpload vs retryable)
/// instead of string-matching.
#[derive(Debug)]
pub struct OssError {
    pub status: u16,
    /// OSS `<Code>` from the error XML ("" when the body wasn't XML).
    pub code: String,
    pub verb: &'static str,
    /// First ~600 chars of the error body — SignatureDoesNotMatch responses
    /// can carry the server-computed StringToSign (debugging ground truth).
    pub body_head: String,
}

impl OssError {
    /// Session is truly gone — must re-initiate, not retry.
    pub fn is_no_such_upload(&self) -> bool {
        self.code == "NoSuchUpload"
    }

    /// 429 / 5xx / SlowDown-family: safe to back off and retry.
    pub fn retryable(&self) -> bool {
        self.status == 429
            || (500..600).contains(&self.status)
            || matches!(
                self.code.as_str(),
                "SlowDown" | "RequestTimeout" | "ServiceUnavailable" | "InternalError"
            )
    }
}

impl std::fmt::Display for OssError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "oss {} -> http={} code={} body_head={}",
            self.verb, self.status, self.code, self.body_head
        )
    }
}

impl std::error::Error for OssError {}

/// Extract the typed OssError from an anyhow chain (ops below raise it via
/// `anyhow::Error::new`, so downcast always finds it).
pub fn oss_error_of(e: &anyhow::Error) -> Option<&OssError> {
    e.downcast_ref::<OssError>()
}

pub struct OssCtx {
    /// e.g. `oss-c.xxx.aliyuncs.com` — scheme stripped, https forced.
    pub endpoint: String,
    pub bucket: String,
    pub object: String,
    pub access_key_id: String,
    pub access_key_secret: String,
    pub security_token: String,
}

impl OssCtx {
    pub fn host(&self) -> String {
        format!("{}.{}", self.bucket, self.endpoint)
    }
}

/// Redact the value following `x-oss-security-token:` inside an error body
/// dump (StringToSign embeds the STS token verbatim — secrets stay masked).
fn mask_sts(s: &str) -> String {
    match s.find("x-oss-security-token:") {
        Some(i) => {
            let head = &s[..i + "x-oss-security-token:".len()];
            match s[i..].find('\n') {
                Some(nl) => format!("{head}***MASKED***{}", &s[i + nl..]),
                None => format!("{head}***MASKED***"),
            }
        }
        None => s.to_string(),
    }
}

/// Percent-encode matching Go's url.QueryEscape (unreserved: A-Za-z0-9-_.~).
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-'..=b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn sign_v1(
    ctx: &OssCtx,
    verb: &str,
    content_type: &str,
    date: &str,
    mut oss_headers: Vec<(String, String)>,
    resource: &str,
) -> String {
    oss_headers.sort_by(|a, b| a.0.cmp(&b.0));
    let mut canonical = String::new();
    for (k, v) in &oss_headers {
        canonical.push_str(k);
        canonical.push(':');
        canonical.push_str(v);
        canonical.push('\n');
    }
    let string_to_sign = format!("{verb}\n\n{content_type}\n{date}\n{canonical}{resource}",);
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(ctx.access_key_secret.as_bytes())
        .expect("hmac accepts any key length");
    mac.update(string_to_sign.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// One signed OSS request. `params` holds raw values; sorting and the
/// signed/unsigned split happen here so the URL and the signature can never
/// disagree.
struct OssRequest<'a> {
    verb: &'static str,
    params: Vec<(&'static str, String)>,
    content_type: Option<&'static str>,
    body: Vec<u8>,
    callback: Option<&'a UploadCallback>,
}

async fn oss_execute(
    client: &reqwest::Client,
    ctx: &OssCtx,
    req: OssRequest<'_>,
) -> Result<(u16, reqwest::header::HeaderMap, String)> {
    let date = httpdate::fmt_http_date(std::time::SystemTime::now());

    // URL query: ALL params, sorted, encoded; bare key when value is empty.
    let mut url_params = req.params.clone();
    url_params.sort_by(|a, b| a.0.cmp(b.0));
    let url_query = url_params
        .iter()
        .map(|(k, v)| {
            if v.is_empty() {
                (*k).to_string()
            } else {
                format!("{}={}", *k, pct_encode(v))
            }
        })
        .collect::<Vec<_>>()
        .join("&");

    // Signature sub-resource string: whitelist only, sorted, RAW values.
    let mut signed: Vec<(&str, &String)> = req
        .params
        .iter()
        .filter(|(k, _)| SIGNED_PARAMS.contains(k))
        .map(|(k, v)| (*k, v))
        .collect();
    signed.sort_by(|a, b| a.0.cmp(b.0));
    let sub = signed
        .iter()
        .map(|(k, v)| {
            if v.is_empty() {
                (*k).to_string()
            } else {
                format!("{}={}", *k, v)
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    let resource = if sub.is_empty() {
        format!("/{}/{}", ctx.bucket, ctx.object)
    } else {
        format!("/{}/{}?{}", ctx.bucket, ctx.object, sub)
    };

    // x-oss-* headers actually sent (each joins the canonicalized set).
    let mut oss_headers: Vec<(String, String)> = vec![(
        "x-oss-security-token".to_string(),
        ctx.security_token.clone(),
    )];
    if let Some(cb) = req.callback {
        oss_headers.push((
            "x-oss-callback".to_string(),
            base64::engine::general_purpose::STANDARD.encode(cb.callback.as_bytes()),
        ));
        oss_headers.push((
            "x-oss-callback-var".to_string(),
            base64::engine::general_purpose::STANDARD.encode(cb.callback_var.as_bytes()),
        ));
    }

    let content_type = req.content_type.unwrap_or("");
    let auth = sign_v1(
        ctx,
        req.verb,
        content_type,
        &date,
        oss_headers.clone(),
        &resource,
    );

    let url = if url_query.is_empty() {
        format!("https://{}/{}", ctx.host(), pct_encode(&ctx.object))
    } else {
        format!(
            "https://{}/{}?{}",
            ctx.host(),
            pct_encode(&ctx.object),
            url_query
        )
    };

    if std::env::var("PAN115_SPIKE_OSS_DEBUG").is_ok() {
        let mut dbg_headers = oss_headers.clone();
        dbg_headers.sort_by(|a, b| a.0.cmp(&b.0));
        let mut canonical = String::new();
        for (k, v) in &dbg_headers {
            let v = if k == "x-oss-security-token" {
                "***MASKED***"
            } else {
                v.as_str()
            };
            canonical.push_str(&format!("{k}:{v}\n"));
        }
        let sts = format!(
            "{}\n\n{}\n{}\n{}{}",
            req.verb, content_type, date, canonical, resource
        );
        eprintln!("[oss-debug] url={url}");
        eprintln!(
            "[oss-debug] object={:?} encoded={:?}",
            ctx.object,
            pct_encode(&ctx.object)
        );
        eprintln!("[oss-debug] my_string_to_sign:\n{sts}");
    }

    let mut r = client
        .request(
            reqwest::Method::from_bytes(req.verb.as_bytes()).expect("static verb"),
            &url,
        )
        .header(reqwest::header::DATE, &date)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("OSS {}:{}", ctx.access_key_id, auth),
        );
    if let Some(ct) = req.content_type {
        r = r.header(reqwest::header::CONTENT_TYPE, ct);
    }
    for (k, v) in &oss_headers {
        r = r.header(
            reqwest::header::HeaderName::from_bytes(k.as_bytes()).expect("ascii"),
            v,
        );
    }
    let resp = r
        .body(reqwest::Body::from(req.body))
        .send()
        .await
        .with_context(|| format!("oss {} transport", req.verb))?;

    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body_bytes = resp.bytes().await.context("oss read body")?;
    let body = String::from_utf8_lossy(&body_bytes).to_string();
    if !(200..300).contains(&status) {
        let code = xml_tag(&body, "Code").unwrap_or("?").to_string();
        Err(anyhow::Error::new(OssError {
            status,
            code,
            verb: req.verb,
            body_head: mask_sts(&body.chars().take(4000).collect::<String>()),
        }))
    } else {
        Ok((status, headers, body))
    }
}

// ---------------------------------------------------------------------------
// tiny XML helpers (fixed known shapes only — no general parser wanted)
// ---------------------------------------------------------------------------

/// Inner text of the first `<tag>...</tag>`.
pub fn xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

/// Inner slices of every `<tag>...</tag>` block.
fn xml_blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let start = i + open.len();
        let Some(rel) = rest[start..].find(&close) else {
            break;
        };
        out.push(&rest[start..start + rel]);
        rest = &rest[start + rel + close.len()..];
    }
    out
}

// ---------------------------------------------------------------------------
// operations
// ---------------------------------------------------------------------------

/// PUT the whole object (small-file path). Callback headers ride along.
pub async fn put_object(
    client: &reqwest::Client,
    ctx: &OssCtx,
    body: Vec<u8>,
    callback: Option<&UploadCallback>,
) -> Result<String> {
    let (_s, _h, resp_body) = oss_execute(
        client,
        ctx,
        OssRequest {
            verb: "PUT",
            params: vec![],
            content_type: Some("application/octet-stream"),
            body,
            callback,
        },
    )
    .await?;
    Ok(resp_body)
}

/// POST `?sequential&uploads` -> uploadId (115 requires the `sequential`
/// sub-resource on initiation).
pub async fn initiate_multipart(client: &reqwest::Client, ctx: &OssCtx) -> Result<String> {
    let (_s, _h, body) = oss_execute(
        client,
        ctx,
        OssRequest {
            verb: "POST",
            params: vec![("uploads", String::new()), ("sequential", String::new())],
            content_type: None,
            body: Vec::new(),
            callback: None,
        },
    )
    .await?;
    xml_tag(&body, "UploadId")
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow!(
                "initiate_multipart: UploadId missing in body {:?}",
                body.chars().take(300).collect::<String>()
            )
        })
}

/// PUT one part -> ETag (WITH surrounding quotes, exactly as the header
/// returned it — Complete XML wants them).
pub async fn upload_part(
    client: &reqwest::Client,
    ctx: &OssCtx,
    upload_id: &str,
    part_number: u32,
    body: Vec<u8>,
) -> Result<String> {
    let (_s, headers, _b) = oss_execute(
        client,
        ctx,
        OssRequest {
            verb: "PUT",
            params: vec![
                ("partNumber", part_number.to_string()),
                ("uploadId", upload_id.to_string()),
            ],
            content_type: Some("application/octet-stream"),
            body,
            callback: None,
        },
    )
    .await?;
    let etag = headers
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow!("upload_part: ETag response header missing"))?;
    Ok(etag.to_string())
}

#[derive(Debug, Clone)]
pub struct PartInfo {
    pub part_number: u32,
    /// Quoted etag as returned by ListParts XML.
    pub etag: String,
    pub size: i64,
}

/// GET ListParts (single page 1000; follows NextPartNumberMarker). `max-parts`
/// / `part-number-marker` are NOT in the V1 signed whitelist — they ride the
/// URL unsigned, like the Go SDK.
pub async fn list_parts(
    client: &reqwest::Client,
    ctx: &OssCtx,
    upload_id: &str,
) -> Result<Vec<PartInfo>> {
    let mut parts = Vec::new();
    let mut marker = String::new();
    loop {
        let mut params = vec![
            ("uploadId", upload_id.to_string()),
            ("max-parts", "1000".to_string()),
        ];
        if !marker.is_empty() {
            params.push(("part-number-marker", marker.clone()));
        }
        let (_s, _h, body) = oss_execute(
            client,
            ctx,
            OssRequest {
                verb: "GET",
                params,
                content_type: None,
                body: Vec::new(),
                callback: None,
            },
        )
        .await?;
        for block in xml_blocks(&body, "Part") {
            let pn = xml_tag(block, "PartNumber")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .ok_or_else(|| anyhow!("list_parts: bad PartNumber"))?;
            let etag = xml_tag(block, "ETag")
                .map(str::to_string)
                .ok_or_else(|| anyhow!("list_parts: ETag missing"))?;
            let size = xml_tag(block, "Size")
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            parts.push(PartInfo {
                part_number: pn,
                etag,
                size,
            });
        }
        let truncated = xml_tag(&body, "IsTruncated")
            .map(|s| s.trim() == "true")
            .unwrap_or(false);
        if !truncated {
            break;
        }
        marker = xml_tag(&body, "NextPartNumberMarker")
            .map(str::to_string)
            .ok_or_else(|| anyhow!("list_parts: truncated page without NextPartNumberMarker"))?;
    }
    parts.sort_by_key(|p| p.part_number);
    Ok(parts)
}

/// POST CompleteMultipartUpload: XML body of parts + callback headers.
/// Response body may be OSS XML OR the 115 callback's JSON reply — returned
/// verbatim for the caller to record.
pub async fn complete_multipart(
    client: &reqwest::Client,
    ctx: &OssCtx,
    upload_id: &str,
    parts: &[(u32, String)],
    callback: Option<&UploadCallback>,
) -> Result<(u16, String)> {
    let mut xml = String::from("<CompleteMultipartUpload>");
    for (n, etag) in parts {
        let etag = if etag.starts_with('"') {
            etag.clone()
        } else {
            format!("\"{etag}\"")
        };
        xml.push_str(&format!(
            "<Part><PartNumber>{n}</PartNumber><ETag>{etag}</ETag></Part>"
        ));
    }
    xml.push_str("</CompleteMultipartUpload>");
    let (status, _h, body) = oss_execute(
        client,
        ctx,
        OssRequest {
            verb: "POST",
            params: vec![("uploadId", upload_id.to_string())],
            content_type: Some("application/xml"),
            body: xml.into_bytes(),
            callback,
        },
    )
    .await?;
    Ok((status, body))
}
