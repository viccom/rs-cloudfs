//! Write-path probe commands — 123-0 leg ⑤ (upload full chain, seven-step
//! strict order) + ⑥ (empty etag) plus the four pin-down legs:
//!   a. duplicate=1 vs duplicate=2 overwrite semantics (D4 truth)
//!   b. rapid upload (`Reuse=true`) response shape
//!   c. part-state retention (resume capability-bit ⑦ evidence)
//!   d. empty / omitted / wrong etag acceptance (ciphertext-volume question)
//!
//! Chain shape follows 123panNextGen's current implementation verbatim,
//! endpoint casing included: `s3_list_upload_parts` and
//! `s3_complete_multipart_upload` take LOWERCASE `storageNode`,
//! `s3_repare_upload_parts_batch` ("repare" is the official misspelling —
//! do NOT "fix" it) takes UPPERCASE `StorageNode`, per-part PUT goes through
//! the bare transfer client (no 123pan auth headers, §5.12 dual session),
//! `upload_complete` takes the single-key `{"fileId": N}` form.
//!
//! Discipline: gentle pacing (600ms between chain steps), transport-error
//! retries <= 3 with >= 2s spacing, PUT timeout max(300s, MB*2s), unique
//! `e2e_pan123_w<rand>` stamp per run, cleanup by strict prefix sweep.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use serde_json::{json, Value};

use crate::api::{self, Spike};
use crate::probes::{field_i64, field_str, info_list_of, list_query, print_item, Args};
use crate::state;

/// 123panNextGen fixed client-side block size (5 MiB) — the fallback when
/// the server does not dictate a usable SliceSize.
const DEFAULT_PART_SIZE: u64 = 5 * 1024 * 1024;

async fn pace() {
    tokio::time::sleep(Duration::from_millis(600)).await;
}

fn spike_with_tokens() -> Result<(state::Paths, Spike)> {
    let paths = state::Paths::from_env();
    let tokens =
        state::load_tokens(&paths).context("need pan123-tokens.json (run `sign-in` first)")?;
    Ok((paths, Spike::build(Some(tokens))?))
}

// ---------------------------------------------------------- chain primitives

/// Parsed upload_request session (five-tuple + slice truth + reuse form).
pub(crate) struct Session {
    pub reuse: bool,
    pub info: Option<Value>,
    /// temp up_file_id (top-level data.FileId; 0 when Reuse short-circuits).
    pub up_file_id: i64,
    pub bucket: String,
    pub key: String,
    pub upload_id: String,
    pub storage_node: String,
    /// RAW server SliceSize value, exactly as returned (may be "", a number
    /// or absent) — this is the leg-⑤ truth being pinned.
    pub slice_raw: Value,
    /// SliceSize parsed to a usable u64 (Some only when numeric and > 0).
    pub slice_size: Option<u64>,
}

fn session_of(data: &Value) -> Result<Session> {
    let reuse = data.get("Reuse").and_then(|v| v.as_bool()).unwrap_or(false);
    let info = data.get("Info").filter(|v| !v.is_null()).cloned();
    let slice_raw = data
        .get("SliceSize")
        .or_else(|| data.get("sliceSize"))
        .cloned()
        .unwrap_or(Value::Null);
    let slice_size = field_i64(data, &["SliceSize", "sliceSize"])
        .and_then(|v| u64::try_from(v).ok())
        .filter(|v| *v > 0);
    let up_file_id = field_i64(data, &["FileId", "fileId"]).unwrap_or(0);
    if reuse {
        return Ok(Session {
            reuse,
            info,
            up_file_id,
            bucket: String::new(),
            key: String::new(),
            upload_id: String::new(),
            storage_node: String::new(),
            slice_raw,
            slice_size,
        });
    }
    let g = |keys: &[&str]| -> Result<String> {
        field_str(data, keys)
            .map(|s| s.to_string())
            .with_context(|| format!("{keys:?} missing from upload_request data"))
    };
    Ok(Session {
        reuse,
        info,
        up_file_id,
        bucket: g(&["Bucket", "bucket"])?,
        key: g(&["Key", "key"])?,
        upload_id: g(&["UploadId", "uploadId"])?,
        storage_node: g(&["StorageNode", "storageNode"])?,
        slice_raw,
        slice_size,
    })
}

/// POST with transport-error retry (<= 3 attempts, 2s apart — risk-control
/// discipline; API-level error codes are NOT retried).
async fn post_retry(spike: &Spike, label: &str, url: &str, body: &Value) -> Result<api::Resp> {
    let mut last: Option<anyhow::Error> = None;
    for attempt in 1..=3u32 {
        match spike.api_post_json(url, body, &[]).await {
            Ok(r) => return Ok(r),
            Err(e) => {
                eprintln!("[{label}] attempt {attempt} transport error: {e:#}");
                last = Some(e);
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
    Err(last.unwrap())
}

/// Step 1 — upload_request. `etag: None` OMITS the key entirely (leg ⑥),
/// `Some("")` sends an empty string (leg ⑥), `Some(hex)` is the normal form.
/// Returns (code, parsed session | None, raw response).
async fn upload_request(
    spike: &Spike,
    label: &str,
    parent: i64,
    name: &str,
    size: u64,
    etag: Option<&str>,
    duplicate: Option<i64>,
    file_type: i64,
) -> Result<(i64, Option<Session>, api::Resp)> {
    let mut body = json!({
        "driveId": 0,
        "fileName": name,
        "parentFileId": parent,
        "size": size,
        "type": file_type,
    });
    if let Some(e) = etag {
        body["etag"] = json!(e);
    }
    if let Some(d) = duplicate {
        body["duplicate"] = json!(d);
    }
    let url = format!("{}/b/api/file/upload_request", api::PRIMARY_BASE);
    let r = post_retry(spike, label, &url, &body).await?;
    api::show(label, &r);
    let (code, _msg) = api::code_of(&r.body);
    let data = serde_json::from_str::<Value>(&r.body)
        .ok()
        .and_then(|v| v.get("data").cloned())
        .filter(|d| d.is_object());
    let sess = match (&data, code) {
        (Some(d), Some(0)) => session_of(d).ok(),
        _ => None,
    };
    Ok((code.unwrap_or(-1), sess, r))
}

/// Step 2/5 — s3_list_upload_parts (LOWERCASE storageNode). Returns
/// (code, sorted part numbers, raw resp).
async fn list_parts(spike: &Spike, label: &str, s: &Session) -> Result<(i64, Vec<i64>, api::Resp)> {
    let body = json!({
        "bucket": s.bucket,
        "key": s.key,
        "uploadId": s.upload_id,
        "storageNode": s.storage_node,
    });
    let url = format!("{}/b/api/file/s3_list_upload_parts", api::PRIMARY_BASE);
    let r = post_retry(spike, label, &url, &body).await?;
    api::show(label, &r);
    let (code, _msg) = api::code_of(&r.body);
    let mut parts: Vec<i64> = serde_json::from_str::<Value>(&r.body)
        .ok()
        .and_then(|v| {
            v.get("data")?
                .get("parts")
                .or_else(|| v.get("data")?.get("Parts"))?
                .as_array()
                .cloned()
        })
        .unwrap_or_default()
        .iter()
        .filter_map(|p| field_i64(p, &["PartNumber", "partNumber"]))
        .collect();
    parts.sort_unstable();
    Ok((code.unwrap_or(-1), parts, r))
}

/// Step 3 — s3_repare_upload_parts_batch ("repare" official misspelling,
/// UPPERCASE StorageNode) over the half-open range [start, end).
async fn presign_batch(
    spike: &Spike,
    label: &str,
    s: &Session,
    start: i64,
    end: i64,
) -> Result<(i64, BTreeMap<i64, String>)> {
    let body = json!({
        "bucket": s.bucket,
        "key": s.key,
        "partNumberStart": start,
        "partNumberEnd": end,
        "uploadId": s.upload_id,
        "StorageNode": s.storage_node,
    });
    let url = format!(
        "{}/b/api/file/s3_repare_upload_parts_batch",
        api::PRIMARY_BASE
    );
    let r = post_retry(spike, label, &url, &body).await?;
    api::show(label, &r);
    let (code, msg) = api::code_of(&r.body);
    if code != Some(0) {
        bail!("presign batch refused code={code:?} msg={msg}");
    }
    let mut urls = BTreeMap::new();
    if let Some(map) = serde_json::from_str::<Value>(&r.body).ok().and_then(|v| {
        v.get("data")?
            .get("presignedUrls")
            .or_else(|| v.get("data")?.get("PresignedUrls"))?
            .as_object()
            .cloned()
    }) {
        for (k, v) in map {
            if let (Ok(n), Some(u)) = (k.parse::<i64>(), v.as_str()) {
                urls.insert(n, u.to_string());
            }
        }
    }
    Ok((code.unwrap_or(-1), urls))
}

/// Step 4 — pure PUT on the transfer face (no 123pan auth headers).
/// Timeout = max(300s, MB * 2s) (pan123-rs discipline).
async fn put_part(
    spike: &Spike,
    part_no: i64,
    url: &str,
    bytes: &[u8],
) -> Result<(u16, Option<String>)> {
    let mb = (bytes.len() / (1024 * 1024)) as u64;
    let timeout = Duration::from_secs((mb * 2).max(300));
    let t0 = std::time::Instant::now();
    let r = spike
        .transfer
        .put(url)
        .header("content-length", bytes.len().to_string())
        .body(bytes.to_vec())
        .timeout(timeout)
        .send()
        .await
        .with_context(|| format!("PUT part {part_no} transport error"))?;
    let status = r.status().as_u16();
    let etag = r
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    println!(
        "[put-part#{part_no}] HTTP {status} len={} etag={etag:?} elapsed={:.1}s url_head={:?}",
        bytes.len(),
        t0.elapsed().as_secs_f32(),
        api::truncate(url, 130)
    );
    if !(200..300).contains(&status) {
        bail!("PUT part {part_no} failed HTTP {status}");
    }
    Ok((status, etag))
}

/// Step 6 — s3_complete_multipart_upload (LOWERCASE storageNode). For
/// single-part sessions the -1 rpc MalformedXML form is a known harmless
/// precedent (read-path leg); record, do not abort.
async fn s3_complete(spike: &Spike, label: &str, s: &Session) -> Result<i64> {
    let body = json!({
        "bucket": s.bucket,
        "key": s.key,
        "uploadId": s.upload_id,
        "storageNode": s.storage_node,
    });
    let url = format!(
        "{}/b/api/file/s3_complete_multipart_upload",
        api::PRIMARY_BASE
    );
    let r = post_retry(spike, label, &url, &body).await?;
    api::show(label, &r);
    let (code, _msg) = api::code_of(&r.body);
    Ok(code.unwrap_or(-1))
}

/// Step 7 — upload_complete, OLD full-body /v2 form. 2026-09-20 spike
/// finding: for MULTIPART uploads the new single-key `{"fileId": N}` form
/// returns code=0 but silently never lands the file (root Total stayed 0 for
/// minutes), while this /v2 body lands it and returns `data.file_info`
/// (snake_case; real FileId + true-MD5 Etag). UPPERCASE `StorageNode` here.
/// Returns the file_info object when present.
async fn upload_complete_v2(
    spike: &Spike,
    label: &str,
    s: &Session,
    size: u64,
    is_multipart: bool,
) -> Result<Option<Value>> {
    let body = json!({
        "fileId": s.up_file_id,
        "bucket": s.bucket,
        "fileSize": size,
        "key": s.key,
        "isMultipart": is_multipart,
        "uploadId": s.upload_id,
        "StorageNode": s.storage_node,
    });
    let url = format!("{}/b/api/file/upload_complete/v2", api::PRIMARY_BASE);
    let r = post_retry(spike, label, &url, &body).await?;
    api::show(label, &r);
    let (code, msg) = api::code_of(&r.body);
    if code != Some(0) {
        bail!("upload_complete/v2 refused code={code:?} msg={msg}");
    }
    Ok(serde_json::from_str::<Value>(&r.body)?
        .get("data")
        .and_then(|d| {
            d.get("file_info")
                .or_else(|| d.get("FileInfo"))
                .or_else(|| d.get("Info"))
        })
        .filter(|v| v.is_object())
        .cloned())
}

// -------------------------------------------------------- read-back helpers

/// `/b/api/file/info` by file id (info is authoritative when the list cache
/// lags — read-path leg finding).
async fn fetch_info(spike: &Spike, fid: i64) -> Result<Option<Value>> {
    let url = format!("{}/b/api/file/info", api::PRIMARY_BASE);
    let r = spike
        .api_post_json(&url, &json!({"fileIdList": [{"fileId": fid}]}), &[])
        .await?;
    let (code, msg) = api::code_of(&r.body);
    if code != Some(0) {
        bail!("info failed code={code:?} msg={msg}");
    }
    Ok(serde_json::from_str::<Value>(&r.body)?
        .get("data")
        .and_then(|d| d.get("InfoList").or_else(|| d.get("infoList")))
        .and_then(|l| l.as_array())
        .and_then(|a| a.first())
        .cloned())
}

/// Poll the parent list until an exact-name entry shows up (cache lag on
/// freshly created entries — read-path leg finding).
async fn poll_find(
    spike: &Spike,
    parent: i64,
    name: &str,
    rounds: u32,
    interval_secs: u64,
) -> Result<Option<Value>> {
    for i in 0..=rounds {
        let url = format!("{}/api/file/list/new", api::PRIMARY_BASE);
        let r = spike
            .api_get(&url, &list_query(parent, 1, 100, false))
            .await?;
        let (code, msg) = api::code_of(&r.body);
        if code != Some(0) {
            bail!("list failed code={code:?} msg={msg}");
        }
        if let Some(it) = info_list_of(&r.body)
            .into_iter()
            .find(|it| field_str(it, &["FileName", "fileName"]) == Some(name))
        {
            if i > 0 {
                println!("[poll] {name:?} visible after {i} extra round(s)");
            }
            return Ok(Some(it));
        }
        if i < rounds {
            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
        }
    }
    Ok(None)
}

/// All parent entries whose name starts with `prefix` (catches auto-renamed
/// copies like "name (1)" produced by duplicate=1).
async fn list_by_prefix(spike: &Spike, parent: i64, prefix: &str) -> Result<Vec<Value>> {
    let url = format!("{}/api/file/list/new", api::PRIMARY_BASE);
    let mut out = Vec::new();
    for page in 1..=10u32 {
        let r = spike
            .api_get(&url, &list_query(parent, page, 100, false))
            .await?;
        let (code, msg) = api::code_of(&r.body);
        if code != Some(0) {
            bail!("list page {page} failed code={code:?} msg={msg}");
        }
        let items = info_list_of(&r.body);
        let n = items.len();
        out.extend(items.into_iter().filter(|it| {
            field_str(it, &["FileName", "fileName"])
                .map(|s| s.starts_with(prefix))
                .unwrap_or(false)
        }));
        if n < 100 {
            break;
        }
    }
    Ok(out)
}

/// Is `fid` present in the parent's recycle (trashed=true) view?
async fn in_recycle_view(spike: &Spike, parent: i64, fid: i64) -> Result<bool> {
    let url = format!("{}/api/file/list/new", api::PRIMARY_BASE);
    let r = spike
        .api_get(&url, &list_query(parent, 1, 100, true))
        .await?;
    Ok(info_list_of(&r.body)
        .iter()
        .any(|it| field_i64(it, &["FileId", "fileId"]) == Some(fid)))
}

// ------------------------------------------------------- download resolution

/// Resolve a download URL for `fid` (info -> download_info new/old -> hop
/// resolution: Location -> JSON redirect body -> HTML href -> params=
/// base64 self-decode; D5: plain resolution only). The resolved mirror URL
/// then serves ranged/full GETs (dlink reuse proven safe in-session).
async fn resolve_download(spike: &Spike, fid: i64) -> Result<String> {
    let item = fetch_info(spike, fid)
        .await?
        .context("info returned no entry for fid")?;
    let dl_body = json!({
        "driveId": 0,
        "etag": field_str(&item, &["Etag", "etag"]).unwrap_or(""),
        "fileId": fid,
        "s3keyFlag": field_str(&item, &["S3KeyFlag", "s3keyFlag"]).unwrap_or(""),
        "type": field_i64(&item, &["Type", "type"]).unwrap_or(0),
        "fileName": field_str(&item, &["FileName", "fileName"]).unwrap_or(""),
        "size": field_i64(&item, &["Size", "size"]).unwrap_or(0),
    });
    let mut chosen: Option<String> = None;
    for (gen, path) in [
        ("new(/a/)", "/a/api/file/download_info"),
        ("old(/b/v2/)", "/b/api/v2/file/download_info"),
    ] {
        let url = format!("{}{path}", api::PRIMARY_BASE);
        let r = spike.api_post_json(&url, &dl_body, &[]).await?;
        let (code, msg) = api::code_of(&r.body);
        if code != Some(0) {
            eprintln!("[resolve] {gen} refused code={code:?} msg={msg}");
            continue;
        }
        let d = serde_json::from_str::<Value>(&r.body)?
            .get("data")
            .cloned()
            .unwrap_or(Value::Null);
        let direct = field_str(&d, &["RedirectUrl", "redirect_url"]).map(|s| s.to_string());
        let dl = field_str(&d, &["DownloadUrl", "downloadUrl"]).map(|s| s.to_string());
        let dispatch = d
            .get("DispatchList")
            .or_else(|| d.get("dispatchList"))
            .and_then(|l| l.as_array())
            .and_then(|a| a.first())
            .and_then(|it| field_str(it, &["Prefix", "prefix"]))
            .map(|s| s.to_string());
        if let Some(u) = direct {
            chosen = Some(u);
            break;
        }
        if let Some(u) = dl {
            chosen = Some(match dispatch {
                Some(p) => format!("{p}{u}"),
                None => u,
            });
            break;
        }
    }
    let mut current = chosen.context("no download URL from either generation")?;

    // hop resolution — tiny ranged probe so no body bytes are wasted.
    // Final = 206 (mirror honoring Range) or a non-HTML 200; the web-pro2
    // interstitial answers 200 text/html and must be resolved THROUGH
    // (Location -> JSON redirect body -> HTML href -> params= base64
    // self-decode, read-path leg forms).
    for hop in 1..=5u32 {
        let r = spike
            .transfer
            .get(&current)
            .header("range", "bytes=0-0")
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .with_context(|| format!("resolve hop#{hop} transport error"))?;
        let status = r.status().as_u16();
        let ct = r
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if status == 206 || (status == 200 && !ct.contains("html") && !ct.contains("json")) {
            return Ok(current);
        }
        if (300..400).contains(&status) {
            if let Some(loc) = r
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
            {
                println!("[resolve] hop#{hop} HTTP {status} ct={ct:?} -> Location");
                current = loc;
                continue;
            }
        }
        let body = r.text().await.unwrap_or_default();
        if ct.contains("json") {
            if let Some(next) = serde_json::from_str::<Value>(&body).ok().and_then(|v| {
                v.get("data")
                    .and_then(|d| field_str(d, &["redirect_url", "RedirectUrl"]))
                    .or_else(|| field_str(&v, &["redirect_url", "RedirectUrl"]))
                    .map(|s| s.to_string())
            }) {
                println!("[resolve] hop#{hop} HTTP {status} json redirect");
                current = next;
                continue;
            }
        }
        if ct.contains("html") || body.starts_with("<!DOCTYPE") || body.starts_with("<html") {
            if let Some(href) = extract_href(&body) {
                println!("[resolve] hop#{hop} HTTP {status} html href");
                current = href;
                continue;
            }
        }
        if let Some(inner) = crate::probes::decode_download_v2_params(&current) {
            println!("[resolve] hop#{hop} HTTP {status} params base64 self-decode");
            current = inner;
            continue;
        }
        bail!(
            "resolve hop#{hop} dead end: HTTP {status} ct={ct:?} body_head={}",
            api::truncate(&body, 200)
        );
    }
    bail!("resolve exceeded 5 hops");
}

/// extract_href / params= decode (read-path leg forms, reused verbatim).
fn extract_href(html: &str) -> Option<String> {
    for marker in ["href='", "href=\""] {
        if let Some(pos) = html.find(marker) {
            let start = pos + marker.len();
            let rest = &html[start..];
            let end = marker
                .ends_with('\'')
                .then(|| rest.find('\''))
                .flatten()
                .or_else(|| rest.find('"'));
            if let Some(end) = end {
                let url = &rest[..end];
                if url.starts_with("http") {
                    return Some(url.to_string());
                }
            }
        }
    }
    None
}

/// GET bytes from the resolved mirror URL with an optional Range header
/// (follows one JSON-redirect hop if it fires again on the real GET).
async fn fetch_bytes(
    spike: &Spike,
    url: &str,
    range: Option<&str>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut current = url.to_string();
    for _hop in 0..2u32 {
        let mut req = spike.transfer.get(&current).timeout(timeout);
        if let Some(r) = range {
            req = req.header("range", r);
        }
        let r = req
            .send()
            .await
            .with_context(|| "mirror GET transport error")?;
        let status = r.status().as_u16();
        let ct = r
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if status == 200 || status == 206 {
            let cr = r
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            println!(
                "[fetch] HTTP {status} content-type={ct:?} content-range={cr:?} url_head={:?}",
                api::truncate(&r.url().to_string(), 120)
            );
            return Ok(r.bytes().await?.to_vec());
        }
        let body = r.text().await.unwrap_or_default();
        if ct.contains("json") {
            if let Some(next) = serde_json::from_str::<Value>(&body).ok().and_then(|v| {
                v.get("data")
                    .and_then(|d| field_str(d, &["redirect_url", "RedirectUrl"]))
                    .map(|s| s.to_string())
            }) {
                println!("[fetch] HTTP {status} json redirect -> following once");
                current = next;
                continue;
            }
        }
        bail!(
            "fetch failed HTTP {status} ct={ct:?} body_head={}",
            api::truncate(&body, 200)
        );
    }
    bail!("fetch exceeded redirect budget");
}

// ------------------------------------------------------------ shared runner

/// Run the full seven-step chain for `data` under `name`. Returns the final
/// session (with up_file_id). `put_filter` selects which parts to actually
/// PUT (resume leg passes Some(1) = only part 1 then aborts).
struct ChainOpts<'a> {
    label: &'a str,
    parent: i64,
    name: &'a str,
    /// Override part size (bytes); None = server SliceSize when usable,
    /// else the 5 MiB 123panNextGen convention.
    part_size: Option<u64>,
    duplicate: Option<i64>,
    /// When Some(n), PUT only part numbers in this set (resume abort leg).
    put_only: Option<Vec<i64>>,
    /// Skip the complete/upload_complete tail (resume abort leg).
    skip_tail: bool,
}

#[allow(clippy::too_many_arguments)]
async fn run_chain(
    spike: &Spike,
    data: &[u8],
    etag: &str,
    opts: &ChainOpts<'_>,
) -> Result<Session> {
    let size = data.len() as u64;
    println!(
        "[{}] === seven-step chain name={:?} size={} etag={etag} dup={:?} ===",
        opts.label, opts.name, size, opts.duplicate
    );
    // step 1
    let (code, sess, raw) = upload_request(
        spike,
        &format!("{}:1-upload_request", opts.label),
        opts.parent,
        opts.name,
        size,
        Some(etag),
        opts.duplicate,
        0,
    )
    .await?;
    if code == 5060 {
        bail!(
            "5060 conflict on a fresh name — unexpected; body={}",
            api::redact(&raw.body)
        );
    }
    if code != 0 {
        bail!("upload_request refused code={code}");
    }
    let s = sess.context("code=0 but session unparseable")?;
    if s.reuse {
        println!(
            "[{}] Reuse=true short-circuit (up_file_id={} info={:?})",
            opts.label,
            s.up_file_id,
            s.info.is_some()
        );
        return Ok(s);
    }
    println!(
        "[{}] five-tuple: bucket_len={} key_len={} upload_id_len={} node_len={} up_file_id={} SliceSize_raw={}",
        opts.label,
        s.bucket.len(),
        s.key.len(),
        s.upload_id.len(),
        s.storage_node.len(),
        s.up_file_id,
        s.slice_raw
    );
    pace().await;

    // part-size decision (leg ⑤ truth: server SliceSize vs client 5MiB)
    let part_size = match opts.part_size {
        Some(p) => {
            println!("[{}] part_size={p} (caller override)", opts.label);
            p
        }
        None => {
            let server = s.slice_size.unwrap_or(0);
            let chosen = if server >= 5 * 1024 * 1024 && server < size {
                println!(
                    "[{}] part_size={server} (server SliceSize, usable: >=5MiB and < size)",
                    opts.label
                );
                server
            } else {
                println!(
                    "[{}] part_size={DEFAULT_PART_SIZE} (fallback: server SliceSize raw={:?} unusable for this size — 123panNextGen fixed 5MiB convention)",
                    opts.label, s.slice_raw
                );
                DEFAULT_PART_SIZE
            };
            chosen
        }
    };
    let part_count = size.div_ceil(part_size).max(1);
    println!(
        "[{}] part layout: count={part_count} part_size={part_size} multipart={}",
        opts.label,
        part_count > 1
    );

    // step 2 — initial list (expect empty)
    let (code, parts, _) = list_parts(spike, &format!("{}:2-list-initial", opts.label), &s).await?;
    if code != 0 {
        bail!("initial list_parts refused code={code}");
    }
    println!("[{}] initial parts = {parts:?}", opts.label);
    pace().await;

    // step 3 — batch presign over all parts
    let (_c, urls) = presign_batch(
        spike,
        &format!("{}:3-presign-batch", opts.label),
        &s,
        1,
        part_count as i64 + 1,
    )
    .await?;
    println!(
        "[{}] presigned {} of {part_count} parts: {:?}",
        opts.label,
        urls.len(),
        urls.keys().collect::<Vec<_>>()
    );
    pace().await;

    // step 4 — PUT parts (filtered for the resume-abort leg)
    for pn in 1..=part_count as i64 {
        if let Some(allow) = &opts.put_only {
            if !allow.contains(&pn) {
                println!("[{}] put-part#{pn} SKIPPED (plan: {:?})", opts.label, allow);
                continue;
            }
        }
        let start = ((pn as u64 - 1) * part_size) as usize;
        let end = ((pn as u64) * part_size).min(size) as usize;
        let Some(url) = urls.get(&pn) else {
            bail!("presigned URL for part {pn} missing");
        };
        put_part(spike, pn, url, &data[start..end]).await?;
        pace().await;
    }
    if opts.skip_tail {
        println!(
            "[{}] ABORT after PUT stage (skip_tail) — no list-confirm/complete/upload_complete",
            opts.label
        );
        return Ok(s);
    }

    // step 5 — list confirm (all parts present)
    let (code, parts, _) = list_parts(spike, &format!("{}:5-list-confirm", opts.label), &s).await?;
    println!("[{}] confirm parts = {parts:?}", opts.label);
    let expected: Vec<i64> = (1..=part_count as i64).collect();
    let wanted: Vec<i64> = match &opts.put_only {
        Some(allow) => allow.clone(),
        None => expected,
    };
    let missing = wanted
        .iter()
        .filter(|p| !parts.contains(p))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!("parts missing after PUT stage: {missing:?} (listed {parts:?})");
    }
    if code != 0 {
        // single-part -1 MalformedXML precedent is harmless; record it
        println!(
            "[{}] NOTE list_parts code={code} (parts listed anyway)",
            opts.label
        );
    }
    pace().await;

    // step 6 — s3 complete
    let code = s3_complete(spike, &format!("{}:6-s3_complete", opts.label), &s).await?;
    println!(
        "[{}] s3_complete code={code} ({})",
        opts.label,
        if part_count > 1 {
            "multipart — expect 0"
        } else {
            "single-part — -1 rpc MalformedXML known-harmless form"
        }
    );
    pace().await;

    // step 7 — upload_complete /v2 full body. 2026-09-20 spike matrix
    // (probe-single E0..E4): the presign endpoint determines the session
    // type — s3_repare batch ALWAYS creates a multipart session (even with
    // one part), so isMultipart is unconditionally true on this path; the
    // isMultipart:false and new single-key forms are silent no-ops here
    // (code=0, data:{}, file never lands). /v2 returns `data.file_info`
    // (snake_case; real FileId + true-MD5 Etag).
    let finfo = upload_complete_v2(
        spike,
        &format!("{}:7-upload_complete_v2", opts.label),
        &s,
        size,
        true,
    )
    .await?;
    if let Some(fi) = &finfo {
        println!(
            "[{}] file_info: FileId={} Size={} Etag={:?}",
            opts.label,
            field_i64(fi, &["FileId", "fileId"]).unwrap_or(0),
            field_i64(fi, &["Size", "size"]).unwrap_or(-1),
            field_str(fi, &["Etag", "etag"]).unwrap_or("")
        );
    }
    // >64MB needs a 3s settle per 123panNextGen; our payloads are smaller,
    // 1.5s is enough for list visibility.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    Ok(s)
}

/// Size + full-content verification (size from list entry, bytes from a
/// full mirror GET hashed against the local MD5).
async fn verify_full(
    spike: &Spike,
    parent: i64,
    name: &str,
    data: &[u8],
    etag: &str,
) -> Result<i64> {
    let found = poll_find(spike, parent, name, 10, 3)
        .await?
        .context("uploaded file not visible in parent list after 30s")?;
    print_item("verify:list-entry", &found);
    let remote_size = field_i64(&found, &["Size", "size"]).unwrap_or(-1);
    let fid = field_i64(&found, &["FileId", "fileId"]).unwrap_or(0);
    let size_ok = remote_size == data.len() as i64;
    println!(
        "[verify] size remote={remote_size} local={} -> {}",
        data.len(),
        if size_ok { "OK" } else { "MISMATCH" }
    );
    let url = resolve_download(spike, fid).await?;
    let bytes = fetch_bytes(spike, &url, None, Duration::from_secs(180)).await?;
    let digest = format!("{:x}", md5::compute(&bytes));
    let hash_ok = digest == etag && bytes.len() == data.len();
    println!(
        "[verify] full download {} bytes md5={digest} vs local={etag} -> {}",
        bytes.len(),
        if hash_ok { "MATCH" } else { "MISMATCH" }
    );
    if !size_ok || !hash_ok {
        bail!("verification failed size_ok={size_ok} hash_ok={hash_ok}");
    }
    Ok(fid)
}

// --------------------------------------------------------- complete-v2 (dbg)

/// Debug leg — the pan123-rs OLD full-body complete form
/// (/b/api/file/upload_complete/v2) against an existing session: pins
/// whether multipart library-entry needs the full body when the new
/// single-key form returns code=0 without landing the file.
pub async fn cmd_complete_v2(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let pos = args.positional();
    if pos.len() < 6 {
        eprintln!("usage: complete-v2 <fileId> <bucket> <key> <uploadId> <storageNode> <size> [--parent X] [--name X]");
        return Ok(2);
    }
    let up_file_id: i64 = pos[0].parse()?;
    let bucket = pos[1].to_string();
    let key = pos[2].to_string();
    let upload_id = pos[3].to_string();
    let storage_node = pos[4].to_string();
    let size: u64 = pos[5].parse()?;
    let parent: i64 = args
        .flag("--parent")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let name = args.flag("--name");
    let form = args.flag("--form").unwrap_or_else(|| "v2".into());
    let (_paths, spike) = spike_with_tokens()?;

    if form == "new" {
        // new single-key form — read-leg proven for single-part
        let url = format!("{}/b/api/file/upload_complete", api::PRIMARY_BASE);
        let r = post_retry(&spike, "complete-new", &url, &json!({"fileId": up_file_id})).await?;
        api::show("complete-new:upload_complete", &r);
        let (code, msg) = api::code_of(&r.body);
        println!("[complete-new] code={code:?} msg={msg:?}");
        if let Some(n) = name {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            match poll_find(&spike, parent, &n, 10, 3).await? {
                Some(it) => {
                    print_item("complete-new:readback", &it);
                    println!("[SUMMARY] complete-new|landed=1|name={n:?}");
                }
                None => println!("[SUMMARY] complete-new|landed=0|name={n:?}"),
            }
        }
        return Ok(0);
    }

    // parts still listed under the session?
    let s = Session {
        reuse: false,
        info: None,
        up_file_id,
        bucket: bucket.clone(),
        key: key.clone(),
        upload_id: upload_id.clone(),
        storage_node: storage_node.clone(),
        slice_raw: Value::Null,
        slice_size: None,
    };
    let (_c, parts, _) = list_parts(&spike, "complete-v2:list", &s).await?;
    println!("[complete-v2] session still lists parts: {parts:?}");
    pace().await;

    let body = json!({
        "fileId": up_file_id,
        "bucket": bucket,
        "fileSize": size,
        "key": key,
        "isMultipart": true,
        "uploadId": upload_id,
        "StorageNode": storage_node,
    });
    let url = format!("{}/b/api/file/upload_complete/v2", api::PRIMARY_BASE);
    let r = post_retry(&spike, "complete-v2", &url, &body).await?;
    api::show("complete-v2:upload_complete_v2", &r);
    let (code, msg) = api::code_of(&r.body);
    println!("[complete-v2] code={code:?} msg={msg:?}");
    if let Some(n) = name {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        match poll_find(&spike, parent, &n, 10, 3).await? {
            Some(it) => {
                print_item("complete-v2:readback", &it);
                println!("[SUMMARY] complete-v2|landed=1|name={n:?}");
            }
            None => println!("[SUMMARY] complete-v2|landed=0|name={n:?}"),
        }
    }
    Ok(0)
}

// ------------------------------------------------------------ upload-full

/// Leg ⑤ — full seven-step chain over a multi-part file: slice_size truth,
/// five-tuple, batch presign, per-part PUT, complete, upload_complete,
/// size + full-hash read-back.
pub async fn cmd_upload_full(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let pos = args.positional();
    let Some(file) = pos.first().map(|s| s.to_string()) else {
        eprintln!("usage: upload-full <file> [--parent X] [--name X] [--part-mb N]");
        return Ok(2);
    };
    let parent: i64 = args
        .flag("--parent")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let part_mb: Option<u64> = args.flag("--part-mb").and_then(|v| v.parse().ok());
    let stamp = format!("e2e_pan123_w{}", &state::rand_hex(3));
    let name = args
        .flag("--name")
        .unwrap_or_else(|| format!("{stamp}_full.bin"));
    let (_paths, spike) = spike_with_tokens()?;

    let data = std::fs::read(&file).with_context(|| format!("read {file}"))?;
    let etag = format!("{:x}", md5::compute(&data));
    println!(
        "[upload-full] file={file} name={name:?} size={} md5={etag}",
        data.len()
    );
    if data.len() as u64 <= DEFAULT_PART_SIZE {
        println!(
            "[upload-full] WARNING size <= {DEFAULT_PART_SIZE} — will be single-part; use >5MiB for the multi-part evidence"
        );
    }

    let opts = ChainOpts {
        label: "upload-full",
        parent,
        name: &name,
        part_size: part_mb.map(|mb| mb * 1024 * 1024),
        duplicate: None,
        put_only: None,
        skip_tail: false,
    };
    let s = run_chain(&spike, &data, &etag, &opts).await?;
    if s.reuse {
        println!("[SUMMARY] upload-full|rapid=1|note=fresh random content hit Reuse — investigate");
        return Ok(0);
    }
    let fid = verify_full(&spike, parent, &name, &data, &etag).await?;
    println!(
        "[SUMMARY] upload-full|ok=1|name={name:?}|FileId={fid}|slice_raw={}|parts_ok=1|hash=MATCH",
        s.slice_raw
    );
    Ok(0)
}

// --------------------------------------------------------- probe-duplicate

/// Pin-down a — duplicate=1 vs duplicate=2. Two independent scenarios, each:
/// upload v1 (content A) -> upload v2 (content B, same name, bare request)
/// -> capture the 5060 full body -> resend with the scenario's duplicate
/// value -> complete the chain -> verdict from list state + a 64-byte Range
/// window of the entry living under the ORIGINAL name.
pub async fn cmd_probe_duplicate(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let pos = args.positional();
    if pos.len() < 2 {
        eprintln!("usage: probe-duplicate <v1-file> <v2-file> [--parent X]");
        return Ok(2);
    }
    let v1f = pos[0].to_string();
    let v2f = pos[1].to_string();
    let parent: i64 = args
        .flag("--parent")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let stamp = format!("e2e_pan123_w{}", &state::rand_hex(3));
    let (_paths, spike) = spike_with_tokens()?;

    let v1 = std::fs::read(&v1f).with_context(|| format!("read {v1f}"))?;
    let v2 = std::fs::read(&v2f).with_context(|| format!("read {v2f}"))?;
    let e1 = format!("{:x}", md5::compute(&v1));
    let e2 = format!("{:x}", md5::compute(&v2));
    println!(
        "[dup] v1 size={} md5={e1} / v2 size={} md5={e2} (contents must differ)",
        v1.len(),
        v2.len()
    );
    if e1 == e2 {
        bail!("v1/v2 identical — pick different content");
    }

    let scenarios: Vec<i64> = match args.flag("--dup").and_then(|v| v.parse::<i64>().ok()) {
        Some(n) => vec![n],
        None => vec![1, 2],
    };
    for dup_value in scenarios {
        let name = format!("{stamp}_dup{dup_value}.bin");
        println!("\n[dup] ===== scenario duplicate={dup_value} name={name:?} =====");
        // a) v1 baseline
        let opts = ChainOpts {
            label: "dup:v1",
            parent,
            name: &name,
            part_size: None,
            duplicate: None,
            put_only: None,
            skip_tail: false,
        };
        run_chain(&spike, &v1, &e1, &opts).await?;
        let base = poll_find(&spike, parent, &name, 10, 3)
            .await?
            .context("v1 baseline not visible")?;
        let fid_a = field_i64(&base, &["FileId", "fileId"]).unwrap_or(0);
        let size_a = field_i64(&base, &["Size", "size"]).unwrap_or(0);
        let upd_a = base
            .get("UpdateAt")
            .or_else(|| base.get("updateAt"))
            .cloned()
            .unwrap_or(Value::Null);
        println!("[dup] v1 live: FileId={fid_a} size={size_a} UpdateAt={upd_a}");
        pace().await;

        // b) v2 bare request -> 5060 full body
        let (code, _sess, raw) = upload_request(
            &spike,
            "dup:v2-bare",
            parent,
            &name,
            v2.len() as u64,
            Some(&e2),
            None,
            0,
        )
        .await?;
        if code != 5060 {
            println!(
                "[dup] WARNING bare v2 request code={code} (5060 expected) body={}",
                api::redact(&raw.body)
            );
        }
        pace().await;

        // c) resend with duplicate=dup_value -> full chain
        let opts = ChainOpts {
            label: "dup:v2",
            parent,
            name: &name,
            part_size: None,
            duplicate: Some(dup_value),
            put_only: None,
            skip_tail: false,
        };
        let s2 = run_chain(&spike, &v2, &e2, &opts).await?;
        if s2.reuse {
            println!(
                "[dup] NOTE v2 hit Reuse — md5 collision with server content, verdict unreliable"
            );
            continue;
        }
        pace().await;

        // d) list state: everything named <name> or its auto-renamed variants
        let base_prefix = name.trim_end_matches(".bin").to_string();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let entries = list_by_prefix(&spike, parent, &base_prefix).await?;
        println!(
            "[dup] entries with prefix {base_prefix:?}: {}",
            entries.len()
        );
        for it in &entries {
            print_item(&format!("dup{dup_value}:after"), it);
        }
        let original = entries
            .iter()
            .find(|it| field_str(it, &["FileName", "fileName"]) == Some(name.as_str()))
            .cloned();
        let old_still = entries
            .iter()
            .any(|it| field_i64(it, &["FileId", "fileId"]) == Some(fid_a));
        let old_size_now = original
            .as_ref()
            .and_then(|it| field_i64(it, &["Size", "size"]))
            .unwrap_or(-1);
        let original_fid = original
            .as_ref()
            .and_then(|it| field_i64(it, &["FileId", "fileId"]))
            .unwrap_or(0);
        let in_recycle = in_recycle_view(&spike, parent, fid_a).await?;

        // e) 64-byte window of the ORIGINAL-name entry: which content is live?
        let mut live_content = "?".to_string();
        if original_fid != 0 {
            let url = resolve_download(&spike, original_fid).await?;
            let win =
                fetch_bytes(&spike, &url, Some("bytes=0-63"), Duration::from_secs(60)).await?;
            live_content = if win.as_slice() == &v1[..64.min(v1.len())] {
                "v1".to_string()
            } else if win.as_slice() == &v2[..64.min(v2.len())] {
                "v2".to_string()
            } else {
                format!("neither ({} bytes)", win.len())
            };
        }
        let verdict = if old_still
            && original_fid == fid_a
            && old_size_now == size_a
            && live_content == "v1"
        {
            format!("KEEP-BOTH (old intact, {} entries)", entries.len())
        } else if !old_still && entries.len() == 1 && live_content == "v2" {
            "OVERWRITE (old gone from list, single v2 entry)".to_string()
        } else if old_still
            && original_fid == fid_a
            && old_size_now == v2.len() as i64
            && live_content == "v2"
        {
            "OVERWRITE-IN-PLACE (same FileId, content/size replaced)".to_string()
        } else {
            format!("OTHER (old_present={old_still} entries={} live_at_original={live_content} old_fid={fid_a} original_fid={original_fid} old_size_now={old_size_now})", entries.len())
        };
        println!(
            "[SUMMARY] dup{dup_value}|entries={}|old_fid_still_listed={old_still}|original_name_fid={original_fid}|size_at_original={old_size_now}|live_content={live_content}|old_in_recycle={in_recycle}|verdict={verdict}",
            entries.len()
        );
        pace().await;
    }
    Ok(0)
}

// ------------------------------------------------------------ probe-reuse

/// Pin-down b — rapid upload: same content (same MD5 + size), different
/// name/path. Where does Reuse=true surface, what fields come back, is the
/// new path entry visible with the right size (list cache lag recorded)?
pub async fn cmd_probe_reuse(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let pos = args.positional();
    let Some(file) = pos.first().map(|s| s.to_string()) else {
        eprintln!("usage: probe-reuse <file> [--parent X]");
        return Ok(2);
    };
    let parent: i64 = args
        .flag("--parent")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let stamp = format!("e2e_pan123_w{}", &state::rand_hex(3));
    let (_paths, spike) = spike_with_tokens()?;

    let data = std::fs::read(&file).with_context(|| format!("read {file}"))?;
    let etag = format!("{:x}", md5::compute(&data));
    let name_a = format!("{stamp}_seed.bin");
    let name_b = format!("{stamp}_reuse.bin");
    println!(
        "[reuse] size={} md5={etag} seed={name_a:?} reuse={name_b:?}",
        data.len()
    );

    // seed: full chain (unless content already lives on the server — e.g.
    // the upload-full leg just stored the same file)
    let (code, _sess, first_raw) = upload_request(
        &spike,
        "reuse:probe",
        parent,
        &name_b,
        data.len() as u64,
        Some(&etag),
        None,
        0,
    )
    .await?;
    let final_raw = if code == 0
        && serde_json::from_str::<Value>(&first_raw.body)
            .ok()
            .and_then(|v| {
                v.get("data")
                    .and_then(|d| d.get("Reuse"))
                    .and_then(|r| r.as_bool())
            })
            .unwrap_or(false)
    {
        println!("[reuse] Reuse=true immediately (content already on server) — seed leg skipped");
        first_raw
    } else {
        println!("[reuse] content not on server yet — seeding via full chain under {name_a:?}");
        let opts = ChainOpts {
            label: "reuse:seed",
            parent,
            name: &name_a,
            part_size: None,
            duplicate: None,
            put_only: None,
            skip_tail: false,
        };
        run_chain(&spike, &data, &etag, &opts).await?;
        let found = poll_find(&spike, parent, &name_a, 10, 3)
            .await?
            .context("seed not visible")?;
        print_item("reuse:seed-entry", &found);
        pace().await;
        // now the actual reuse request under a DIFFERENT name
        let (code, _sess2, raw2) = upload_request(
            &spike,
            "reuse:probe",
            parent,
            &name_b,
            data.len() as u64,
            Some(&etag),
            None,
            0,
        )
        .await?;
        println!("[reuse] second request code={code}");
        let reuse_hit = serde_json::from_str::<Value>(&raw2.body)
            .ok()
            .and_then(|v| {
                v.get("data")
                    .and_then(|d| d.get("Reuse"))
                    .and_then(|r| r.as_bool())
            })
            .unwrap_or(false);
        if !reuse_hit {
            println!(
                "[SUMMARY] reuse|hit=0|note=server did not Reuse identical etag+size — record form"
            );
            return Ok(0);
        }
        raw2
    };
    let body_data = serde_json::from_str::<Value>(&final_raw.body)
        .ok()
        .and_then(|v| v.get("data").cloned())
        .unwrap_or(Value::Null);
    println!(
        "[reuse] Reuse response data keys={}",
        body_data
            .as_object()
            .map(|m| m.keys().cloned().collect::<Vec<_>>().join(","))
            .unwrap_or_default()
    );
    let fid = field_i64(&body_data, &["FileId", "fileId"]).unwrap_or(0);
    let info_fid = body_data
        .get("Info")
        .and_then(|i| field_i64(i, &["FileId", "fileId"]))
        .unwrap_or(0);

    // visibility: info first (authoritative), then list poll (cache lag)
    let target = if info_fid > 0 { info_fid } else { fid };
    match fetch_info(&spike, target).await {
        Ok(Some(it)) => {
            print_item("reuse:info-entry", &it);
            println!(
                "[reuse] info Size={} vs local {} -> {}",
                field_i64(&it, &["Size", "size"]).unwrap_or(-1),
                data.len(),
                if field_i64(&it, &["Size", "size"]) == Some(data.len() as i64) {
                    "OK"
                } else {
                    "MISMATCH"
                }
            );
        }
        other => println!("[reuse] info lookup returned {other:?}"),
    }
    let mut lag_rounds: Option<u32> = None;
    for i in 0..=30u32 {
        let url = format!("{}/api/file/list/new", api::PRIMARY_BASE);
        let r = spike
            .api_get(&url, &list_query(parent, 1, 100, false))
            .await?;
        let seen = info_list_of(&r.body)
            .iter()
            .any(|it| field_str(it, &["FileName", "fileName"]) == Some(name_b.as_str()));
        if seen {
            lag_rounds = Some(i);
            break;
        }
        if i < 30 {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
    println!(
        "[SUMMARY] reuse|hit=1|data.FileId={fid}|data.Info.FileId={info_fid}|visible_in_list_after={:?}",
        lag_rounds.map(|r| format!("{r} polls (3s)")).unwrap_or_else(|| ">90s (lag)".into())
    );
    Ok(0)
}

// ------------------------------------------------------------ probe-resume

/// Pin-down c — part-state retention (resume capability-bit ⑦ evidence):
/// upload parts 1 only, abort; re-issue upload_request with identical
/// params and compare UploadId; if the re-request does NOT hand back the
/// session, try the ORIGINAL five-tuple directly at s3_list_upload_parts
/// (the 123panNextGen local-persist model). Whichever session shows part 1,
/// complete only the missing parts, finish the chain, full-hash verify.
pub async fn cmd_probe_resume(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let pos = args.positional();
    let Some(file) = pos.first().map(|s| s.to_string()) else {
        eprintln!("usage: probe-resume <file> [--parent X]");
        return Ok(2);
    };
    let parent: i64 = args
        .flag("--parent")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let stamp = format!("e2e_pan123_w{}", &state::rand_hex(3));
    let name = format!("{stamp}_resume.bin");
    let (_paths, spike) = spike_with_tokens()?;

    let data = std::fs::read(&file).with_context(|| format!("read {file}"))?;
    let etag = format!("{:x}", md5::compute(&data));
    println!(
        "[resume] size={} md5={etag} name={name:?} (needs >= 2 parts under the chosen part size)",
        data.len()
    );

    // leg 1: request + PUT part 1 only + abort
    let opts = ChainOpts {
        label: "resume:leg1",
        parent,
        name: &name,
        part_size: None,
        duplicate: None,
        put_only: Some(vec![1]),
        skip_tail: true,
    };
    let s1 = run_chain(&spike, &data, &etag, &opts).await?;
    if s1.reuse {
        bail!("fresh content hit Reuse — cannot test resume");
    }
    println!(
        "[resume] leg1 upload_id_len={} up_file_id={}",
        s1.upload_id.len(),
        s1.up_file_id
    );
    pace().await;

    // leg 2: re-issue IDENTICAL upload_request — same session back?
    let (code, sess2, raw2) = upload_request(
        &spike,
        "resume:leg2-reupload-request",
        parent,
        &name,
        data.len() as u64,
        Some(&etag),
        None,
        0,
    )
    .await?;
    if code != 0 {
        println!(
            "[resume] re-request refused code={code} body={}",
            api::redact(&raw2.body)
        );
    }
    let same_id = sess2
        .as_ref()
        .map(|s| s.upload_id == s1.upload_id)
        .unwrap_or(false);
    println!(
        "[resume] re-request UploadId same as leg1: {same_id} (leg2 upload_id_len={} up_file_id={})",
        sess2.as_ref().map(|s| s.upload_id.len()).unwrap_or(0),
        sess2.as_ref().map(|s| s.up_file_id).unwrap_or(0)
    );
    pace().await;

    // choose a working session: prefer the re-issued one when it preserved
    // the id; else probe the ORIGINAL tuple directly (local-persist model).
    let mut working: Option<Session> = None;
    let mut model = String::new();
    if same_id {
        let s = sess2.unwrap();
        let (_c, parts, _) = list_parts(&spike, "resume:leg2-list", &s).await?;
        if parts.contains(&1) {
            working = Some(s);
            model = "re-request-same-session".into();
            println!("[resume] leg2 session lists part 1 -> {parts:?}");
        }
    }
    if working.is_none() {
        println!("[resume] trying ORIGINAL five-tuple directly (local-persist model)");
        let (_c, parts, r) = list_parts(&spike, "resume:leg1-list-direct", &s1).await?;
        println!("[resume] original tuple lists: {parts:?}");
        if parts.contains(&1) {
            working = Some(s1);
            model = "local-persist-tuple".into();
        } else {
            let _ = r;
        }
    }
    let Some(s) = working else {
        println!(
            "[SUMMARY] resume|preserved=0|verdict=parts NOT retained across re-request or direct tuple — capability bit stays OFF; orphan session leaks server-side (harmless)"
        );
        return Ok(0);
    };
    println!("[resume] session retained via {model} — completing the diff-set");

    // diff-set: presign + PUT only the missing parts (fresh listing decides)
    let part_size = s.slice_size.unwrap_or(0);
    let part_size = if part_size >= 5 * 1024 * 1024 && part_size < data.len() as u64 {
        part_size
    } else {
        DEFAULT_PART_SIZE
    };
    let part_count = (data.len() as u64).div_ceil(part_size).max(1);
    let (_code, present, _r) = list_parts(&spike, "resume:pre-diff-list", &s).await?;
    let missing: Vec<i64> = (1..=part_count as i64)
        .filter(|p| !present.contains(p))
        .collect();
    println!(
        "[resume] present={present:?} missing={missing:?} (skipping re-PUT of {} part(s))",
        present.len()
    );
    for pn in &missing {
        let start = ((*pn as u64 - 1) * part_size) as usize;
        let end = ((*pn as u64) * part_size).min(data.len() as u64) as usize;
        let (_c, mut urls) = presign_batch(&spike, "resume:presign", &s, *pn, *pn + 1).await?;
        let Some(u) = urls.remove(pn) else {
            bail!("presign for part {pn} missing");
        };
        put_part(&spike, *pn, &u, &data[start..end]).await?;
        pace().await;
    }

    // confirm + complete tail
    let (_c, parts, _) = list_parts(&spike, "resume:post-diff-list", &s).await?;
    let want: Vec<i64> = (1..=part_count as i64).collect();
    let still_missing = want
        .iter()
        .filter(|p| !parts.contains(p))
        .collect::<Vec<_>>();
    if !still_missing.is_empty() {
        bail!("after diff-set still missing {still_missing:?}");
    }
    let code = s3_complete(&spike, "resume:s3_complete", &s).await?;
    println!("[resume] s3_complete code={code}");
    pace().await;
    let finfo = upload_complete_v2(
        &spike,
        "resume:upload_complete_v2",
        &s,
        data.len() as u64,
        true,
    )
    .await?;
    if let Some(fi) = &finfo {
        println!(
            "[resume] file_info FileId={} Size={}",
            field_i64(fi, &["FileId", "fileId"]).unwrap_or(0),
            field_i64(fi, &["Size", "size"]).unwrap_or(-1)
        );
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let fid = verify_full(&spike, parent, &name, &data, &etag).await?;
    println!(
        "[SUMMARY] resume|preserved=1|model={model}|re_request_same_upload_id={same_id}|diff_skipped={}parts|FileId={fid}|hash=MATCH",
        present.len()
    );
    Ok(0)
}

// ------------------------------------------------------------- list-prefix

/// Debug leg — print every root entry whose name starts with a prefix
/// (name/size/etag state inspection + final sweep verification helper).
pub async fn cmd_list_prefix(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let pos = args.positional();
    let Some(prefix) = pos.first().map(|s| s.to_string()) else {
        eprintln!("usage: list-prefix <prefix> [--parent X]");
        return Ok(2);
    };
    let parent: i64 = args
        .flag("--parent")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let (_paths, spike) = spike_with_tokens()?;
    let entries = list_by_prefix(&spike, parent, &prefix).await?;
    println!("[list-prefix] prefix={prefix:?} count={}", entries.len());
    for it in &entries {
        println!(
            "[list-prefix] FileId={:?} name={:?} size={:?} etag={:?} update={:?}",
            field_i64(it, &["FileId", "fileId"]),
            field_str(it, &["FileName", "fileName"]),
            field_i64(it, &["Size", "size"]),
            field_str(it, &["Etag", "etag"]),
            it.get("UpdateAt")
                .or_else(|| it.get("updateAt"))
                .map(|v| v.to_string())
                .unwrap_or_default(),
        );
    }
    Ok(0)
}

// -------------------------------------------------------- probe-single (dbg)

/// Debug leg — single-part completion matrix. Today's evidence: single-part
/// via s3_repare presign + s3_complete(code=0) + /v2(isMultipart:false) does
/// NOT land (code=0, data:{}); the read leg's exact form (s3_upload_object
/// presign + s3_complete -1 MalformedXML + new single-key complete) DID
/// land. This matrix isolates the variable: presign endpoint x s3_complete
/// x complete form/isMultipart. Five variants, each a fresh 2MiB random
/// payload (uploads are free; landing checked via list only).
pub async fn cmd_probe_single(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let parent: i64 = args
        .flag("--parent")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let stamp = format!("e2e_pan123_w{}", &state::rand_hex(3));
    let (_paths, spike) = spike_with_tokens()?;

    struct Var {
        tag: &'static str,
        old_presign: bool,
        do_s3_complete: bool,
        complete: &'static str, // "v2-mp0" | "v2-mp1" | "new"
    }
    let vars = [
        Var {
            tag: "E0-readleg-exact",
            old_presign: true,
            do_s3_complete: true,
            complete: "new",
        },
        Var {
            tag: "E1-repare+sc+v2mp1",
            old_presign: false,
            do_s3_complete: true,
            complete: "v2-mp1",
        },
        Var {
            tag: "E2-repare+v2mp0",
            old_presign: false,
            do_s3_complete: false,
            complete: "v2-mp0",
        },
        Var {
            tag: "E3-repare+v2mp1",
            old_presign: false,
            do_s3_complete: false,
            complete: "v2-mp1",
        },
        Var {
            tag: "E4-repare+new",
            old_presign: false,
            do_s3_complete: false,
            complete: "new",
        },
    ];

    for v in vars {
        println!("\n[single] ===== {} =====", v.tag);
        // fresh random payload per variant (avoid Reuse)
        let mut data = vec![0u8; 2 * 1024 * 1024];
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut data);
        let etag = format!("{:x}", md5::compute(&data));
        let name = format!("{stamp}_{}.bin", v.tag.split('-').next().unwrap_or("x"));
        println!("[single] name={name:?} md5={etag}");

        let (code, sess, _raw) = upload_request(
            &spike,
            "single:req",
            parent,
            &name,
            data.len() as u64,
            Some(&etag),
            None,
            0,
        )
        .await?;
        if code != 0 {
            println!("[single] upload_request code={code} — skip variant");
            continue;
        }
        let s = sess.context("session unparseable")?;
        if s.reuse {
            println!("[single] Reuse hit — skip variant (collision)");
            continue;
        }
        pace().await;

        // presign: old single-part auth endpoint vs repare batch
        let put_url = if v.old_presign {
            let body = json!({
                "bucket": s.bucket,
                "key": s.key,
                "partNumberStart": 1,
                "partNumberEnd": 2,
                "uploadId": s.upload_id,
                "StorageNode": s.storage_node,
            });
            let url = format!("{}/b/api/file/s3_upload_object/auth", api::PRIMARY_BASE);
            let r = post_retry(&spike, "single:presign-old", &url, &body).await?;
            api::show("single:presign-old", &r);
            let (c, msg) = api::code_of(&r.body);
            if c != Some(0) {
                println!("[single] old presign refused code={c:?} msg={msg} — skip variant");
                continue;
            }
            serde_json::from_str::<Value>(&r.body)?
                .get("data")
                .and_then(|d| d.get("presignedUrls").or_else(|| d.get("PresignedUrls")))
                .and_then(|p| p.get("1").or_else(|| p.get(1usize)).cloned())
                .and_then(|u| u.as_str().map(|s| s.to_string()))
                .context("presignedUrls[1] missing")?
        } else {
            let (_c, mut urls) = presign_batch(&spike, "single:presign-repare", &s, 1, 2).await?;
            urls.remove(&1).context("repare url for part 1 missing")?
        };
        pace().await;

        put_part(&spike, 1, &put_url, &data).await?;
        pace().await;

        if v.do_s3_complete {
            let code = s3_complete(&spike, "single:s3_complete", &s).await?;
            println!("[single] s3_complete code={code}");
            pace().await;
        }

        let mut landed = false;
        match v.complete {
            "v2-mp0" | "v2-mp1" => {
                let mp = v.complete.ends_with('1');
                let fi =
                    upload_complete_v2(&spike, "single:complete-v2", &s, data.len() as u64, mp)
                        .await?;
                println!("[single] /v2 isMultipart={mp} file_info={}", fi.is_some());
                if fi.is_some() {
                    landed = true;
                }
            }
            _ => {
                let url = format!("{}/b/api/file/upload_complete", api::PRIMARY_BASE);
                let r = post_retry(
                    &spike,
                    "single:complete-new",
                    &url,
                    &json!({"fileId": s.up_file_id}),
                )
                .await?;
                api::show("single:complete-new", &r);
                let (c, msg) = api::code_of(&r.body);
                println!("[single] new single-key code={c:?} msg={msg:?}");
            }
        }
        if !landed {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            if poll_find(&spike, parent, &name, 6, 3).await?.is_some() {
                landed = true;
            }
        }
        println!("[SUMMARY] single|variant={}|landed={landed}", v.tag);
        pace().await;
    }
    Ok(0)
}

// ------------------------------------------------------------ probe-etag

/// Pin-down d — etag strength: (1) key OMITTED entirely, (2) empty string
/// with a full chain (ciphertext-volume viability), (3) wrong-but-valid
/// md5 with a full chain (server-side integrity check strength).
pub async fn cmd_probe_etag(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let pos = args.positional();
    let Some(file) = pos.first().map(|s| s.to_string()) else {
        eprintln!("usage: probe-etag <file> [--parent X]");
        return Ok(2);
    };
    let parent: i64 = args
        .flag("--parent")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let stamp = format!("e2e_pan123_w{}", &state::rand_hex(3));
    let (_paths, spike) = spike_with_tokens()?;

    let data = std::fs::read(&file).with_context(|| format!("read {file}"))?;
    let true_etag = format!("{:x}", md5::compute(&data));
    println!("[etag] size={} true_md5={true_etag}", data.len());

    // (1) omitted key — observation only (orphan session if accepted)
    let name1 = format!("{stamp}_omit.bin");
    let (code, sess, raw) = upload_request(
        &spike,
        "etag:omit",
        parent,
        &name1,
        data.len() as u64,
        None,
        None,
        0,
    )
    .await?;
    let omit_accepted = code == 0;
    let omit_reuse = sess.as_ref().map(|s| s.reuse).unwrap_or(false);
    println!(
        "[SUMMARY] etag-omit|accepted={omit_accepted}|code={code}|reuse={omit_reuse}|msg={:?}",
        api::code_of(&raw.body).1
    );
    pace().await;

    // (2) empty-string etag — full chain when accepted
    let name2 = format!("{stamp}_empty.bin");
    let (code, sess, raw) = upload_request(
        &spike,
        "etag:empty",
        parent,
        &name2,
        data.len() as u64,
        Some(""),
        None,
        0,
    )
    .await?;
    let empty_accepted = code == 0;
    let empty_reuse = sess.as_ref().map(|s| s.reuse).unwrap_or(false);
    println!(
        "[etag:empty] accepted={empty_accepted} code={code} reuse={empty_reuse} body={}",
        api::redact(&raw.body)
    );
    if empty_accepted && !empty_reuse {
        let opts = ChainOpts {
            label: "etag:empty-chain",
            parent,
            name: &name2,
            part_size: None,
            duplicate: None,
            put_only: None,
            skip_tail: false,
        };
        run_chain(&spike, &data, "", &opts).await?;
        // size + window check (window is enough; full hash unnecessary here)
        let found = poll_find(&spike, parent, &name2, 10, 3)
            .await?
            .context("empty-etag upload not visible")?;
        print_item("etag:empty-entry", &found);
        let fid = field_i64(&found, &["FileId", "fileId"]).unwrap_or(0);
        let size_ok = field_i64(&found, &["Size", "size"]) == Some(data.len() as i64);
        if let Ok(Some(info_entry)) = fetch_info(&spike, fid).await {
            println!(
                "[etag:empty] stored Etag field={:?}",
                field_str(&info_entry, &["Etag", "etag"]).unwrap_or("")
            );
        }
        let url = resolve_download(&spike, fid).await?;
        let win = fetch_bytes(&spike, &url, Some("bytes=0-63"), Duration::from_secs(60)).await?;
        let win_ok = win.as_slice() == &data[..64.min(data.len())];
        println!(
            "[SUMMARY] etag-empty|accepted=1|chain=complete|size_ok={size_ok}|window_match={win_ok}"
        );
    } else {
        println!(
            "[SUMMARY] etag-empty|accepted={empty_accepted}|reuse={empty_reuse}|note={}",
            if empty_accepted {
                "short-circuited"
            } else {
                "refused — ciphertext volume must precompute MD5"
            }
        );
    }
    pace().await;

    // (3) wrong-but-valid etag (md5 of content with byte 0 flipped) — full
    // chain with the REAL bytes; does the server verify at complete time?
    let mut flipped = data.clone();
    flipped[0] ^= 0xff;
    let wrong_etag = format!("{:x}", md5::compute(&flipped));
    let name3 = format!("{stamp}_wrong.bin");
    println!("[etag:wrong] claiming etag={wrong_etag} while sending true_md5={true_etag}");
    let (code, sess, raw) = upload_request(
        &spike,
        "etag:wrong",
        parent,
        &name3,
        data.len() as u64,
        Some(&wrong_etag),
        None,
        0,
    )
    .await?;
    if code != 0 {
        println!(
            "[SUMMARY] etag-wrong|refused=1|code={code}|body={}",
            api::redact(&raw.body)
        );
        return Ok(0);
    }
    if sess.as_ref().map(|s| s.reuse).unwrap_or(false) {
        println!("[SUMMARY] etag-wrong|reuse=1|note=wrong etag hit Reuse — verdict INVALID, rerun");
        return Ok(0);
    }
    let opts = ChainOpts {
        label: "etag:wrong-chain",
        parent,
        name: &name3,
        part_size: None,
        duplicate: None,
        put_only: None,
        skip_tail: false,
    };
    run_chain(&spike, &data, &wrong_etag, &opts).await?;
    let found = poll_find(&spike, parent, &name3, 10, 3)
        .await?
        .context("wrong-etag upload not visible")?;
    print_item("etag:wrong-entry", &found);
    let fid = field_i64(&found, &["FileId", "fileId"]).unwrap_or(0);
    if let Ok(Some(info_entry)) = fetch_info(&spike, fid).await {
        println!(
            "[etag:wrong] stored Etag field={:?} (claimed={wrong_etag} true={true_etag})",
            field_str(&info_entry, &["Etag", "etag"]).unwrap_or("")
        );
    }
    let url = resolve_download(&spike, fid).await?;
    let win = fetch_bytes(&spike, &url, Some("bytes=0-63"), Duration::from_secs(60)).await?;
    let win_ok = win.as_slice() == &data[..64.min(data.len())];
    println!(
        "[SUMMARY] etag-wrong|accepted=1|chain=complete|window_vs_actual={}|verdict={}",
        if win_ok { "MATCH" } else { "MISMATCH" },
        if win_ok {
            "NO server-side content verification at this size (stored bytes = actual content)"
        } else {
            "content altered/rejected — server verifies etag vs content"
        }
    );
    Ok(0)
}
