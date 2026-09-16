//! OSS（阿里云）V1 手写签名 + 115 上传链的五个操作（Phase 5 / 115-1
//! 自 spike 整体 port，K69.5 裁决：不引入 ali-oss-rs）。
//!
//! 忠实移植 aliyun-oss-go-sdk 的 header 签名（oss/auth.go getSignedStr
//! 与 oss/conn.go getSubResource/getResource——OpenList 115_open 驱动
//! 骑乘的栈），已对 SDK 源码核对（master，2026-09-16）而非 ali-oss-rs
//! 的 V4 路径。**115-0 真机已验证**（`examples/pan115_spike/src/oss.rs`
//! ——PutObject 3MiB 1.8s / 三片 multipart 12MiB 2.3s / resume 差集
//! 补片 / callback JSON 回应，K69.8）。
//!
//! V1 string-to-sign（Content-MD5 恒空）：
//! ```text
//! VERB \n <md5> \n <Content-Type> \n <Date> \n
//! <CanonicalizedOSSHeaders><CanonicalizedResource>
//! ```
//!
//! - CanonicalizedOSSHeaders：全部 `x-oss-*` 头，名小写升序，每个
//!   `name:value\n`（**最后一个也带尾 \n**——Go SDK 行为，陷阱一）；
//! - CanonicalizedResource：`/bucket/object` + 仅**签名子资源白名单**
//!   的 `?k=v&k2`，排序，**RAW（未编码）值**（陷阱二：与 URL 侧的
//!   percent-encode 排序串不同——两串永不可混淆，构造收在
//!   [`oss_execute`] 一处）；
//! - URL query 携带全部参数（签名 + 未签名），排序 + Go-QueryEscape
//!   形态 percent-encode；`sequential` 子资源是 115 特有要求（initiate
//!   必带）；
//! - STS：每请求 `x-oss-security-token` 头（进 canonicalized headers）；
//!   endpoint 强制 https、virtual-host 形态。
//!
//! 错误形态（[`OssError`]）：`NoSuchUpload` = 会话真死（必须重 initiate
//! 而非重试）；429/5xx/SlowDown 族 = 可退避重试——K69.8 的 resume 双坑
//! （PartAlreadyExist 死循环 / UploadId 意外重置）在 115-3 写路径按此
//! 分类消费。

use base64::Engine as _;
use hmac::{Hmac, Mac};

use crate::api::UploadCallback;

/// 签名子资源白名单（Go SDK signKeyList 的子集——五操作可产生的全部
/// 签名参数；含 115 特有的 `sequential` 与 callback 对）。
const SIGNED_PARAMS: &[&str] = &[
    "callback",
    "callback-var",
    "partNumber",
    "sequential",
    "uploadId",
    "uploads",
];

/// 类型化 OSS 失败（调用方据此分类 NoSuchUpload vs retryable，不做
/// 字符串匹配）。
#[derive(Debug)]
pub struct OssError {
    /// HTTP 状态码。
    pub status: u16,
    /// 错误 XML 的 `<Code>`（body 非 XML 时为 `"?"`）。
    pub code: String,
    /// 出错操作的动词（PUT/POST/GET）。
    pub verb: &'static str,
    /// 错误体前若干字符（SignatureDoesNotMatch 响应可携带服务端计算的
    /// StringToSign——诊断真值；STS token 已掩码）。
    pub body_head: String,
}

impl OssError {
    /// 会话真死（必须重新 initiate，重试无意义）。
    pub fn is_no_such_upload(&self) -> bool {
        self.code == "NoSuchUpload"
    }

    /// 429 / 5xx / SlowDown 族：可安全退避重试。
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

/// OSS 请求上下文（`upload/get_token` 的 STS 凭证 + init/resume 下发的
/// bucket/object）。
///
/// 不实现 `Debug`：内嵌 STS access key/secret/security token——派生展开
/// 有印进日志的风险（R3）。
pub struct OssCtx {
    /// 如 `oss-c.xxx.aliyuncs.com`（scheme 已剥，https 强制）。
    pub endpoint: String,
    pub bucket: String,
    pub object: String,
    pub access_key_id: String,
    pub access_key_secret: String,
    pub security_token: String,
}

impl OssCtx {
    /// virtual-host 形态主机名（`<bucket>.<endpoint>`）。
    pub fn host(&self) -> String {
        format!("{}.{}", self.bucket, self.endpoint)
    }
}

/// 错误体 dump 内 `x-oss-security-token:` 后的值打码（StringToSign 逐字
/// 内嵌 STS token——秘密保持掩码；spike mask_sts 同形）。
fn mask_sts(s: &str) -> String {
    const MARKER: &str = "x-oss-security-token:";
    match s.find(MARKER) {
        Some(i) => {
            let head = &s[..i + MARKER.len()];
            match s[i..].find('\n') {
                Some(nl) => format!("{head}***MASKED***{}", &s[i + nl..]),
                None => format!("{head}***MASKED***"),
            }
        }
        None => s.to_string(),
    }
}

/// percent-encode（Go `url.QueryEscape` 形态：unreserved = A-Za-z0-9-_.~，
/// 空格 = `%20` 而非 `+`）。
pub(crate) fn pct_encode(s: &str) -> String {
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

/// V1 签名的 string-to-sign（纯函数——canonical headers 排序 + 尾 \n
/// 陷阱 + resource 原样拼接；Content-MD5 恒空）。
fn string_to_sign(
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
    format!("{verb}\n\n{content_type}\n{date}\n{canonical}{resource}")
}

/// HMAC-SHA1 签名（base64）。
fn sign_v1(
    ctx: &OssCtx,
    verb: &str,
    content_type: &str,
    date: &str,
    oss_headers: Vec<(String, String)>,
    resource: &str,
) -> String {
    let sts = string_to_sign(verb, content_type, date, oss_headers, resource);
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(ctx.access_key_secret.as_bytes())
        .expect("hmac accepts any key length");
    mac.update(sts.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// 一个签名请求的描述（`params` 持 RAW 值——排序与签名/未签名拆分
/// 收在 [`oss_execute`]，URL 与签名永不相左）。
struct OssRequest<'a> {
    verb: &'static str,
    params: Vec<(&'static str, String)>,
    content_type: Option<&'static str>,
    body: Vec<u8>,
    callback: Option<&'a UploadCallback>,
}

/// 执行一个签名请求：构造 URL（全参数排序 + encode）与 Canonicalized
/// Resource（白名单排序 + RAW）→ 签名 → 发送 → 非 2xx 归一 [`OssError`]。
async fn oss_execute(
    client: &reqwest::Client,
    ctx: &OssCtx,
    req: OssRequest<'_>,
) -> Result<(u16, reqwest::header::HeaderMap, String), OssError> {
    let date = httpdate::fmt_http_date(std::time::SystemTime::now());

    // URL query：全部参数、排序、encode；空值裸键。
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

    // 签名子资源串：白名单、排序、RAW 值。
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

    // 实际发送的 x-oss-* 头（每头都进 canonicalized 集合）。
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

    // URL 构造。生产 = 强制 https + virtual-host（`<bucket>.<endpoint>`）；
    // 测试缝（115-4 conformance 桩）：endpoint 自带 scheme（含 `://`）时
    // 走 path-style `{endpoint}/{bucket}/{object}` 且保留该 scheme——
    // **签名不依赖二者**（CanonicalizedResource 恒为 `/bucket/object`，
    // V1 签名与 host 形态无关），因此该缝不改变任何被签名输入。
    let base = if ctx.endpoint.contains("://") {
        format!(
            "{}/{}/{}",
            ctx.endpoint.trim_end_matches('/'),
            ctx.bucket,
            pct_encode(&ctx.object)
        )
    } else {
        format!("https://{}/{}", ctx.host(), pct_encode(&ctx.object))
    };
    let url = if url_query.is_empty() {
        base
    } else {
        format!("{base}?{url_query}")
    };

    let mut request = client
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
        request = request.header(reqwest::header::CONTENT_TYPE, ct);
    }
    for (k, v) in &oss_headers {
        request = request.header(
            reqwest::header::HeaderName::from_bytes(k.as_bytes()).expect("ascii"),
            v,
        );
    }
    // 传输失败归一为 OssError 的 status=0 形态（重试分类由调用方决定）。
    let resp = request
        .body(reqwest::Body::from(req.body))
        .send()
        .await
        .map_err(|e| OssError {
            status: 0,
            code: "Transport".to_string(),
            verb: req.verb,
            body_head: format!("transport: {}", e.without_url()),
        })?;

    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body_bytes = resp.bytes().await.map_err(|e| OssError {
        status,
        code: "Transport".to_string(),
        verb: req.verb,
        body_head: format!("body read: {}", e.without_url()),
    })?;
    let body = String::from_utf8_lossy(&body_bytes).to_string();
    if !(200..300).contains(&status) {
        let code = xml_tag(&body, "Code").unwrap_or("?").to_string();
        Err(OssError {
            status,
            code,
            verb: req.verb,
            body_head: mask_sts(&body.chars().take(4000).collect::<String>()),
        })
    } else {
        Ok((status, headers, body))
    }
}

// ---------------------------------------------------------------------------
// 微型 XML 工具（仅固定已知形态——不引通用解析器）
// ---------------------------------------------------------------------------

/// 首个 `<tag>...</tag>` 的内文。
pub(crate) fn xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

/// 全部 `<tag>...</tag>` 块的内文切片。
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
// 五操作（115-3 写路径消费；形态 = spike 真机验证版）
// ---------------------------------------------------------------------------

/// PUT 整对象（小文件路径；callback 头随行——K69.8：callback 挂 Put）。
/// 响应体可能是 OSS XML 或 115 callback 的 JSON 回应，原样返回。
pub async fn put_object(
    client: &reqwest::Client,
    ctx: &OssCtx,
    body: Vec<u8>,
    callback: Option<&UploadCallback>,
) -> Result<String, OssError> {
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

/// POST `?sequential&uploads` → uploadId（115 要求 initiate 带
/// `sequential` 子资源）。
pub async fn initiate_multipart(
    client: &reqwest::Client,
    ctx: &OssCtx,
) -> Result<String, OssError> {
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
        .ok_or_else(|| OssError {
            status: 200,
            code: "ParseError".to_string(),
            verb: "POST",
            body_head: format!(
                "UploadId missing: {}",
                body.chars().take(300).collect::<String>()
            ),
        })
}

/// PUT 单个分片 → ETag（**带引号**，与响应头原样——Complete XML 需要引号
/// 形态）。
pub async fn upload_part(
    client: &reqwest::Client,
    ctx: &OssCtx,
    upload_id: &str,
    part_number: u32,
    body: Vec<u8>,
) -> Result<String, OssError> {
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
        .ok_or_else(|| OssError {
            status: 200,
            code: "ParseError".to_string(),
            verb: "PUT",
            body_head: "ETag response header missing".to_string(),
        })?;
    Ok(etag.to_string())
}

/// ListParts 的一片（part_number / 引号 etag / 字节数）。
#[derive(Debug, Clone)]
pub struct PartInfo {
    pub part_number: u32,
    /// ListParts XML 回报的引号 etag。
    pub etag: String,
    pub size: i64,
}

/// GET ListParts（每页 1000，跟随 NextPartNumberMarker 直至 IsTruncated
/// =false；`max-parts`/`part-number-marker` **不在** V1 签名白名单——
/// 按Go SDK 形态在 URL 侧未签名随行）。115-3 的 resume 差集面消费。
pub async fn list_parts(
    client: &reqwest::Client,
    ctx: &OssCtx,
    upload_id: &str,
) -> Result<Vec<PartInfo>, OssError> {
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
                .ok_or_else(|| OssError {
                    status: 200,
                    code: "ParseError".to_string(),
                    verb: "GET",
                    body_head: "bad PartNumber".to_string(),
                })?;
            let etag = xml_tag(block, "ETag")
                .map(str::to_string)
                .ok_or_else(|| OssError {
                    status: 200,
                    code: "ParseError".to_string(),
                    verb: "GET",
                    body_head: "ETag missing".to_string(),
                })?;
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
            .ok_or_else(|| OssError {
                status: 200,
                code: "ParseError".to_string(),
                verb: "GET",
                body_head: "truncated page without NextPartNumberMarker".to_string(),
            })?;
    }
    parts.sort_by_key(|p| p.part_number);
    Ok(parts)
}

/// POST CompleteMultipartUpload：parts 的 XML 体 + callback 头。响应体
/// 可能是 OSS XML **或** 115 callback 的 JSON 回应——原样返回由调用方
/// 记录（K69.8：complete 后 size 复核纪律在 115-3 兑现）。
pub async fn complete_multipart(
    client: &reqwest::Client,
    ctx: &OssCtx,
    upload_id: &str,
    parts: &[(u32, String)],
    callback: Option<&UploadCallback>,
) -> Result<(u16, String), OssError> {
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

/// 纯函数层的测试观测面（lib.rs 单测引用——签名输入构造/编码/XML
/// 工具是 TDD 钉死的防回归面）。
#[cfg(test)]
pub(crate) mod tests {
    /// [`pct_encode`] 直通（Go QueryEscape 形态向量钉死）。
    pub(crate) fn pct(s: &str) -> String {
        super::pct_encode(s)
    }

    /// [`xml_blocks`] 直通。
    pub(crate) fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
        super::xml_blocks(xml, tag)
    }

    /// [`string_to_sign`] 直通（两个 canonicalization 陷阱的向量钉死）。
    pub(crate) fn sts(
        verb: &str,
        content_type: &str,
        date: &str,
        oss_headers: Vec<(String, String)>,
        resource: &str,
    ) -> String {
        super::string_to_sign(verb, content_type, date, oss_headers, resource)
    }
}
