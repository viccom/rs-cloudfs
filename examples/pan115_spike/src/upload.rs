//! Upload-chain orchestration: hashing, init (with the second-round
//! sign_check loop), OSS transfer, post-upload verification and the
//! probe-resume session file. Mirrors 115-plus-desktop src-tauri/src/upload/
//! api.rs semantics (init loop re-sends ALL first-round fields plus
//! pick_code/sign_key/sign_val) and OpenList 115_open driver.go Put().

use std::path::Path;

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use sha1::Digest;

use crate::api::{self, InitResp, UploadCallback};

pub const PART_SIZE: usize = 5 * 1024 * 1024;
pub const E2E_DIR_NAME: &str = "_e2e_pan115";
const ROOT_CID: &str = "0";
const PRE_HASH_LEN: usize = 128 * 1024;
const INIT_MAX_ROUNDS: u32 = 6;

fn sha1_hex_upper(data: &[u8]) -> String {
    let mut h = sha1::Sha1::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02X}")).collect()
}

pub struct FileHashes {
    pub file_name: String,
    pub file_size: i64,
    pub sha1_full: String,
    /// First 128 KiB (whole file when smaller), uppercase hex.
    pub sha1_pre: String,
    pub data: Vec<u8>,
}

pub fn compute_file_hashes(path: &Path) -> Result<FileHashes> {
    let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("file name must be utf-8")?
        .to_string();
    let sha1_pre = sha1_hex_upper(&data[..data.len().min(PRE_HASH_LEN)]);
    let sha1_full = sha1_hex_upper(&data);
    Ok(FileHashes {
        file_name,
        file_size: data.len() as i64,
        sha1_full,
        sha1_pre,
        data,
    })
}

/// SHA1 over [start, end] INCLUSIVE (end-start+1 bytes) — the sign_check
/// contract ("start-end" closed interval).
pub fn sha1_range_hex_upper(data: &[u8], start: u64, end: u64) -> Result<String> {
    if end < start {
        bail!("sign_check range inverted: start={start} end={end}");
    }
    let s = start as usize;
    let e = (end as usize) + 1;
    if e > data.len() {
        bail!(
            "sign_check range {}-{} exceeds file size {}",
            start,
            end,
            data.len()
        );
    }
    Ok(sha1_hex_upper(&data[s..e]))
}

/// Idempotent /_e2e_pan115/ under the drive root -> (cid, created).
pub async fn ensure_dir(client: &reqwest::Client, token: &str) -> Result<(String, bool)> {
    let (rows, _count) = api::list_all(client, token, ROOT_CID, 1150).await?;
    if let Some(row) = rows.iter().find(|r| r.fname == E2E_DIR_NAME && r.fc == "0") {
        return Ok((row.fid.clone(), false));
    }
    let fid = api::mkdir(client, token, ROOT_CID, E2E_DIR_NAME).await?;
    Ok((fid, true))
}

/// Poll the target dir for `name` (post-complete the callback-side
/// registration can lag a second or two) -> row.
pub async fn find_in_dir(
    client: &reqwest::Client,
    token: &str,
    cid: &str,
    name: &str,
) -> Result<Option<api::ListRow>> {
    for attempt in 0..12u32 {
        let (rows, _) = api::list_all(client, token, cid, 1150).await?;
        if let Some(row) = rows.iter().find(|r| r.fname == name) {
            return Ok(Some(row.clone()));
        }
        if attempt < 11 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
    Ok(None)
}

pub struct InitOutcome {
    pub resp: InitResp,
    pub reauth_rounds: u32,
}

/// init + second-round loop. `hash_override` swaps in FAKE fileid/preid
/// (garbage probe). Loop ends at status 1 or 2; 6/7/8 without sign fields is
/// an unsupported state.
pub async fn init_with_reauth(
    client: &reqwest::Client,
    token: &str,
    hashes: &FileHashes,
    target_cid: &str,
    hash_override: Option<(&str, &str)>,
) -> Result<InitOutcome> {
    let (fileid, preid) =
        hash_override.unwrap_or((hashes.sha1_full.as_str(), hashes.sha1_pre.as_str()));
    let target = format!("U_1_{target_cid}");
    let mut pick_code: Option<String> = None;
    let mut sign_key: Option<String> = None;
    let mut sign_val: Option<String> = None;
    let mut rounds = 0u32;
    loop {
        let resp = api::upload_init(
            client,
            token,
            &hashes.file_name,
            hashes.file_size,
            &target,
            fileid,
            preid,
            pick_code.as_deref(),
            sign_key.as_deref(),
            sign_val.as_deref(),
        )
        .await?;
        // Second-round detection: BOTH by status 6/7/8 AND by presence of
        // sign_key+sign_check (spec: check all three signals).
        let wants_reauth =
            matches!(resp.status, 6..=8) || (resp.sign_key.is_some() && resp.sign_check.is_some());
        if wants_reauth {
            let (Some(sk), Some(sc)) = (resp.sign_key.clone(), resp.sign_check.clone()) else {
                bail!(
                    "init: status {} without sign_key/sign_check pair",
                    resp.status
                );
            };
            if rounds >= INIT_MAX_ROUNDS {
                bail!("init: second-round loop exceeded {INIT_MAX_ROUNDS} rounds");
            }
            let (start, end) = sc
                .split_once('-')
                .and_then(|(s, e)| Some((s.parse::<u64>().ok()?, e.parse::<u64>().ok()?)))
                .with_context(|| format!("parse sign_check {sc:?}"))?;
            if end < start {
                bail!("sign_check inverted: {sc}");
            }
            rounds += 1;
            println!(
                "[init] second-round auth #{}: sign_check={sc} ({} bytes)",
                rounds,
                end - start + 1
            );
            sign_val = Some(sha1_range_hex_upper(&hashes.data, start, end)?);
            pick_code = Some(resp.pick_code.clone());
            sign_key = Some(sk);
            continue;
        }
        match resp.status {
            1 | 2 => {
                return Ok(InitOutcome {
                    resp,
                    reauth_rounds: rounds,
                })
            }
            other => bail!("init: unsupported status {other} (expected 1/2 after auth rounds)"),
        }
    }
}

pub struct TransferReport {
    /// "put_object" | "multipart"
    pub mode: &'static str,
    pub parts: usize,
    /// "xml" | "json" | "empty" — shape of the complete/put response body
    /// (callback-effective completes answer with 115's JSON).
    pub body_kind: &'static str,
}

fn body_kind(body: &str) -> &'static str {
    let t = body.trim_start();
    if t.starts_with('{') {
        "json"
    } else if t.starts_with('<') {
        "xml"
    } else {
        "empty"
    }
}

async fn with_retry<F, Fut, T>(label: &str, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut delay_ms = 500u64;
    let mut attempt = 1u32;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let retryable = crate::oss::oss_error_of(&e)
                    .map(|oe| oe.retryable())
                    .unwrap_or(false);
                if !retryable || attempt >= 4 {
                    return Err(e.context(format!("{label} failed after {attempt} attempt(s)")));
                }
                eprintln!(
                    "[oss] {label} attempt {attempt} retryable failure ({e}); backoff {delay_ms}ms"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                delay_ms *= 2;
                attempt += 1;
            }
        }
    }
}

fn oss_ctx(sts: &api::StsToken, bucket: &str, object: &str) -> crate::oss::OssCtx {
    // endpoint may arrive with a scheme — strip it, https is forced here.
    let endpoint = sts
        .endpoint
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string();
    crate::oss::OssCtx {
        endpoint,
        bucket: bucket.to_string(),
        object: object.to_string(),
        access_key_id: sts.access_key_id.clone(),
        access_key_secret: sts.access_key_secret.clone(),
        security_token: sts.security_token.clone(),
    }
}

/// Fresh STS -> PutObject (<= 1 part) or sequential multipart + complete,
/// callback headers riding the terminal request.
pub async fn oss_transfer(
    client: &reqwest::Client,
    token: &str,
    bucket: &str,
    object: &str,
    callback: Option<&UploadCallback>,
    data: &[u8],
) -> Result<TransferReport> {
    let sts = api::get_token(client, token).await?;
    let ctx = oss_ctx(&sts, bucket, object);
    if data.len() <= PART_SIZE {
        let body = with_retry("put_object", || {
            crate::oss::put_object(client, &ctx, data.to_vec(), callback)
        })
        .await?;
        return Ok(TransferReport {
            mode: "put_object",
            parts: 1,
            body_kind: body_kind(&body),
        });
    }
    let upload_id = crate::oss::initiate_multipart(client, &ctx).await?;
    let mut parts: Vec<(u32, String)> = Vec::new();
    for (idx, chunk) in data.chunks(PART_SIZE).enumerate() {
        let n = (idx + 1) as u32;
        let etag = with_retry(&format!("upload_part#{n}"), || {
            crate::oss::upload_part(client, &ctx, &upload_id, n, chunk.to_vec())
        })
        .await?;
        println!("[oss] part {n}: etag={etag}");
        parts.push((n, etag));
    }
    let (_status, body) =
        crate::oss::complete_multipart(client, &ctx, &upload_id, &parts, callback).await?;
    Ok(TransferReport {
        mode: "multipart",
        parts: parts.len(),
        body_kind: body_kind(&body),
    })
}

// ---------------------------------------------------------------------------
// probe-resume session file
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct ResumeSession {
    pub file_path: String,
    pub file_name: String,
    pub file_size: i64,
    pub target_cid: String,
    pub fileid: String,
    pub pick_code: String,
    pub bucket: String,
    pub object: String,
    pub upload_id: String,
    pub part1_etag: String,
    pub created_unix: i64,
}

pub fn session_path() -> std::path::PathBuf {
    crate::state::Paths::from_env()
        .dir
        .join("pan115-upload-session.json")
}

pub fn save_session(s: &ResumeSession) -> Result<()> {
    crate::state::write_json_atomic(&session_path(), s)
}

pub fn load_session() -> Option<ResumeSession> {
    crate::state::read_json(&session_path()).ok()
}

pub fn clear_session() {
    let _ = std::fs::remove_file(session_path());
}
