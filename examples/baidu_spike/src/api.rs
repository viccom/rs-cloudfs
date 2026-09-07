//! Thin Baidu Pan API wrappers (shapes per PCFS `drivers/baidu`, appendix A
//! of docs/plans/2026-09-06-multicloud.md). Every wrapper scrubs bodies
//! before surfacing them and refreshes once on errno 110 (PCFS pattern).

use std::time::Instant;

use anyhow::{anyhow, bail, Context as _, Result};
use serde::Deserialize;

use crate::common::{ensure_not_token_error, scrub, xe, Ctx, Secrets, Token};

const XPAN_FILE: &str = "https://pan.baidu.com/rest/2.0/xpan/file";
const OAUTH_TOKEN: &str = "https://openapi.baidu.com/oauth/2.0/token";
const SUPERFILE2: &str = "https://d.pcs.baidu.com/rest/2.0/pcs/superfile2";

// ---------------------------------------------------------------------------
// oauth
// ---------------------------------------------------------------------------

/// Standalone refresh grant (works before any cached token exists:
/// pass `current_refresh = ""` to fall back to the instance-file value).
pub async fn refresh_grant(
    client: &reqwest::Client,
    secrets: &Secrets,
    current_refresh: &str,
) -> Result<Token> {
    let refresh_token = if current_refresh.is_empty() {
        secrets.refresh_token.clone()
    } else {
        current_refresh.to_string()
    };
    let t0 = Instant::now();
    let resp = client
        .get(OAUTH_TOKEN)
        .query(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", secrets.client_id.as_str()),
            ("client_secret", secrets.client_secret.as_str()),
        ])
        .send()
        .await
        .map_err(|e| xe("oauth transport", e))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| xe("oauth body", e))?;
    let v: serde_json::Value =
        serde_json::from_str(&body).with_context(|| format!("oauth non-json: {}", scrub(&body)))?;

    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        let desc = v
            .get("error_description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        bail!("oauth refresh rejected: error={err} desc={desc} (refresh_token expired?)");
    }
    let token = Token {
        access_token: v
            .get("access_token")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("oauth response missing access_token: {}", scrub(&body)))?
            .to_string(),
        refresh_token: v
            .get("refresh_token")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("oauth response missing refresh_token"))?
            .to_string(),
        obtained_at_unix: now_unix(),
        expires_in: v.get("expires_in").and_then(|x| x.as_i64()).unwrap_or(0),
    };
    crate::common::register_secrets(vec![
        token.access_token.clone(),
        token.refresh_token.clone(),
    ]);
    crate::common::summ(format!(
        "refresh|http={}|ok=1|expires_in={}s|took_ms={}",
        status.as_u16(),
        token.expires_in,
        t0.elapsed().as_millis()
    ));
    Ok(token)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Refresh using ctx-held state, update the lock + cache file.
pub async fn ctx_refresh(ctx: &Ctx) -> Result<Token> {
    let current = ctx.token.read().await.refresh_token.clone();
    let tok = refresh_grant(&ctx.api, &ctx.secrets, &current).await?;
    *ctx.token.write().await = tok.clone();
    crate::common::save_token(&ctx.cfg, &tok)?;
    Ok(tok)
}

// ---------------------------------------------------------------------------
// xpan/file method=list
// ---------------------------------------------------------------------------

/// Outcome structs keep raw/http fields for evidence even when a given
/// subcommand prints only some of them.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ListOutcome {
    pub http: reqwest::StatusCode,
    pub errno: i64,
    pub entries: Vec<RemoteEntry>,
    pub ms: u128,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteEntry {
    #[serde(default, rename = "server_filename")]
    pub filename: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub isdir: i64,
    #[serde(default)]
    pub md5: String,
    #[serde(default, rename = "fs_id")]
    pub fs_id: i64,
}

pub async fn list_dir(ctx: &Ctx, dir: &str) -> Result<ListOutcome> {
    let t0 = Instant::now();
    let token = ctx.token.read().await.access_token.clone();
    let resp = ctx
        .api
        .get(XPAN_FILE)
        .query(&[
            ("method", "list"),
            ("dir", dir),
            ("access_token", token.as_str()),
        ])
        .send()
        .await
        .map_err(|e| xe("list transport", e))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| xe("list body", e))?;
    parse_list(status, &body, t0.elapsed().as_millis())
}

fn parse_list(status: reqwest::StatusCode, body: &str, ms: u128) -> Result<ListOutcome> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        errno: i64,
        #[serde(default)]
        list: Vec<RemoteEntry>,
    }
    let raw: Raw = serde_json::from_str(body)
        .with_context(|| format!("list non-json (http {status}): {}", scrub(body)))?;
    Ok(ListOutcome {
        http: status,
        errno: raw.errno,
        entries: raw.list,
        ms,
    })
}

// ---------------------------------------------------------------------------
// precreate / superfile2 / create  (upload three-step)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PrecreateOutcome {
    pub http: reqwest::StatusCode,
    pub errno: i64,
    pub return_type: i64,
    pub uploadid: String,
    /// block indexes returned by the server (semantics verified empirically
    /// by the `resume` subcommand: already-uploaded vs still-needed).
    pub block_list: Vec<i64>,
    pub fs_id: i64,
    pub raw: String,
    pub ms: u128,
}

pub async fn precreate(
    ctx: &Ctx,
    path: &str,
    size: i64,
    block_md5: &[String],
) -> Result<PrecreateOutcome> {
    let blocks_json = serde_json::to_string(block_md5)?;
    let t0 = Instant::now();

    let call = |token: String| {
        ctx.api
            .post(XPAN_FILE)
            .query(&[("method", "precreate"), ("access_token", token.as_str())])
            .form(&[
                ("path", path.to_string()),
                ("size", size.to_string()),
                ("isdir", "0".to_string()),
                ("autoinit", "1".to_string()),
                ("rtype", "1".to_string()),
                ("block_list", blocks_json.clone()),
            ])
            .send()
    };

    let token = ctx.token.read().await.access_token.clone();
    let resp = call(token)
        .await
        .map_err(|e| xe("precreate transport", e))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| xe("precreate body", e))?;
    let first = parse_precreate(status, &body, t0.elapsed().as_millis())?;
    if first.errno == 110 {
        // token expired mid-session: refresh once and replay once (PCFS pattern)
        let _ = ctx_refresh(ctx).await?;
        let token = ctx.token.read().await.access_token.clone();
        let resp = call(token)
            .await
            .map_err(|e| xe("precreate transport(retry)", e))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| xe("precreate body(retry)", e))?;
        return parse_precreate(status, &body, t0.elapsed().as_millis());
    }
    ensure_not_token_error(first.errno)?;
    Ok(first)
}

fn parse_precreate(status: reqwest::StatusCode, body: &str, ms: u128) -> Result<PrecreateOutcome> {
    let v: serde_json::Value = serde_json::from_str(body)
        .with_context(|| format!("precreate non-json (http {status}): {}", scrub(body)))?;
    let out = PrecreateOutcome {
        http: status,
        errno: v.get("errno").and_then(|x| x.as_i64()).unwrap_or(-999),
        return_type: v.get("return_type").and_then(|x| x.as_i64()).unwrap_or(-1),
        uploadid: v
            .get("uploadid")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        block_list: v
            .get("block_list")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|i| i.as_i64()).collect())
            .unwrap_or_default(),
        fs_id: v.get("fs_id").and_then(|x| x.as_i64()).unwrap_or(0),
        raw: scrub(body),
        ms,
    };
    ensure_not_token_error(out.errno)?;
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct SuperfileOutcome {
    pub http: reqwest::StatusCode,
    pub error_code: i64,
    pub md5: String,
    pub raw: String,
    pub ms: u128,
}

pub async fn superfile2(
    ctx: &Ctx,
    path: &str,
    uploadid: &str,
    partseq: i64,
    data: Vec<u8>,
) -> Result<SuperfileOutcome> {
    let t0 = Instant::now();
    let token = ctx.token.read().await.access_token.clone();
    let partseq = partseq.to_string();
    let part = reqwest::multipart::Part::bytes(data)
        .file_name("file")
        .mime_str("application/octet-stream")?;
    let form = reqwest::multipart::Form::new().part("file", part);
    let resp = ctx
        .stream
        .post(SUPERFILE2)
        .query(&[
            ("method", "upload"),
            ("access_token", token.as_str()),
            ("path", path),
            ("type", "tmpfile"),
            ("uploadid", uploadid),
            ("partseq", partseq.as_str()),
        ])
        .multipart(form)
        .send()
        .await
        .map_err(|e| xe("superfile2 transport", e))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| xe("superfile2 body", e))?;

    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        md5: String,
        #[serde(default, rename = "error_code")]
        error_code: i64,
    }
    let parsed: Raw = serde_json::from_str(&body).unwrap_or(Raw {
        md5: String::new(),
        error_code: -999,
    });
    Ok(SuperfileOutcome {
        http: status,
        error_code: parsed.error_code,
        md5: parsed.md5,
        raw: scrub(&body),
        ms: t0.elapsed().as_millis(),
    })
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct CreateOutcome {
    pub http: reqwest::StatusCode,
    pub errno: i64,
    pub fs_id: i64,
    pub raw: String,
    pub ms: u128,
}

pub async fn create_file(
    ctx: &Ctx,
    path: &str,
    uploadid: &str,
    size: i64,
    block_md5: &[String],
) -> Result<CreateOutcome> {
    let blocks_json = serde_json::to_string(block_md5)?;
    let t0 = Instant::now();
    let token = ctx.token.read().await.access_token.clone();
    let resp = ctx
        .api
        .post(XPAN_FILE)
        .query(&[("method", "create"), ("access_token", token.as_str())])
        .form(&[
            ("path", path.to_string()),
            ("size", size.to_string()),
            ("isdir", "0".to_string()),
            ("rtype", "1".to_string()),
            ("uploadid", uploadid.to_string()),
            ("block_list", blocks_json),
        ])
        .send()
        .await
        .map_err(|e| xe("create transport", e))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| xe("create body", e))?;
    let v: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("create non-json (http {status}): {}", scrub(&body)))?;
    let out = CreateOutcome {
        http: status,
        errno: v.get("errno").and_then(|x| x.as_i64()).unwrap_or(-999),
        fs_id: v.get("fs_id").and_then(|x| x.as_i64()).unwrap_or(0),
        raw: scrub(&body),
        ms: t0.elapsed().as_millis(),
    };
    ensure_not_token_error(out.errno)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// download dlink (xpan/file?method=download, no-redirect) + range GET
// ---------------------------------------------------------------------------

/// Returns (status, Location header if any). Never follows the redirect.
pub async fn fetch_dlink(ctx: &Ctx, path: &str) -> Result<(reqwest::StatusCode, Option<String>)> {
    let token = ctx.token.read().await.access_token.clone();
    let resp = ctx
        .stream
        .get(XPAN_FILE)
        .query(&[
            ("method", "download"),
            ("access_token", token.as_str()),
            ("path", path),
        ])
        .send()
        .await
        .map_err(|e| xe("download-dlink transport", e))?;
    let status = resp.status();
    if status == reqwest::StatusCode::FOUND || status == reqwest::StatusCode::MOVED_PERMANENTLY {
        let loc = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        return Ok((status, loc));
    }
    if status.is_success() {
        // PCFS tolerates direct 200; record it (no redirect to chase).
        return Ok((status, None));
    }
    let body = resp.text().await.unwrap_or_default();
    Err(anyhow!(
        "download-dlink unexpected status {}: {}",
        status,
        scrub(&body).chars().take(200).collect::<String>()
    ))
}

/// GET a dlink with a Range header; returns (status, bytes received).
pub async fn range_get(
    ctx: &Ctx,
    url: &str,
    range: &str,
) -> Result<(reqwest::StatusCode, Vec<u8>)> {
    let resp = ctx
        .stream
        .get(url)
        .header(reqwest::header::RANGE, range)
        .send()
        .await
        .map_err(|e| xe("range-get transport", e))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| xe("range-get body", e))?
        .to_vec();
    Ok((status, bytes))
}

/// Stream the whole URL, counting bytes (kept for reference; the CDN
/// currently rejects non-ranged GETs — see the spike report).
#[allow(dead_code)]
pub async fn stream_all(ctx: &Ctx, url: &str) -> Result<(reqwest::StatusCode, u64)> {
    use futures_util::StreamExt;
    let resp = ctx
        .stream
        .get(url)
        .send()
        .await
        .map_err(|e| xe("stream transport", e))?;
    let status = resp.status();
    let mut n: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| xe("stream chunk", e))?;
        n += chunk.len() as u64;
    }
    Ok((status, n))
}

// ---------------------------------------------------------------------------
// filemanager opera=delete
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DeleteOutcome {
    pub http: reqwest::StatusCode,
    pub errno: i64,
    /// (per-path errno, path) in info[]
    pub infos: Vec<(i64, String)>,
    pub raw: String,
}

pub async fn filemanager_delete(ctx: &Ctx, paths: &[String]) -> Result<DeleteOutcome> {
    let filelist = serde_json::to_string(
        &paths
            .iter()
            .map(|p| serde_json::json!({ "path": p }))
            .collect::<Vec<_>>(),
    )?;
    let token = ctx.token.read().await.access_token.clone();
    let resp = ctx
        .api
        .post(XPAN_FILE)
        .query(&[
            ("method", "filemanager"),
            ("opera", "delete"),
            ("access_token", token.as_str()),
        ])
        .form(&[("filelist", filelist)])
        .send()
        .await
        .map_err(|e| xe("delete transport", e))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| xe("delete body", e))?;
    let v: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("delete non-json (http {status}): {}", scrub(&body)))?;
    let infos = v
        .get("info")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .map(|i| {
                    (
                        i.get("errno").and_then(|e| e.as_i64()).unwrap_or(-999),
                        i.get("path")
                            .and_then(|p| p.as_str())
                            .unwrap_or("?")
                            .to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let out = DeleteOutcome {
        http: status,
        errno: v.get("errno").and_then(|x| x.as_i64()).unwrap_or(-999),
        infos,
        raw: scrub(&body),
    };
    ensure_not_token_error(out.errno)?;
    Ok(out)
}
