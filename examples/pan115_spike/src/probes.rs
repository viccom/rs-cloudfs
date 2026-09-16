//! Live probe subcommands (file face, upload chain, downurl/UA matrix,
//! resume, qps storm). Discipline: everything lands under /_e2e_pan115/;
//! tokens/STS material printed masked only; 911 aborts the process loudly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context as _, Result};

use crate::api::{self, ErrKind};
use crate::state::{mask, Tokens};
use crate::upload;

const CHROME_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

fn summarize(cmd: &str, kvs: &[(&str, String)]) {
    let body = kvs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("|");
    println!("[SUMMARY] {cmd}|{body}");
}

async fn load() -> Result<(reqwest::Client, String)> {
    let paths = crate::state::Paths::from_env();
    let tokens: Tokens = crate::state::read_json(&paths.tokens())
        .context("probes need pan115-tokens.json (run auth-qr + auth-poll first)")?;
    let client = crate::auth::http_client()?;
    Ok((client, tokens.access_token.clone()))
}

// ---------------------------------------------------------------------------
// arg parsing: positionals + `--flag value` pairs, strict on unknowns
// ---------------------------------------------------------------------------

pub struct Parsed {
    pub pos: Vec<String>,
    pub flags: HashMap<String, String>,
}

pub fn parse_args(items: &[String], known_flags: &[&str], n_pos: usize) -> Result<Parsed> {
    let mut pos = Vec::new();
    let mut flags = HashMap::new();
    let mut i = 0;
    while i < items.len() {
        let it = &items[i];
        if it.starts_with("--") {
            let name = it.trim_start_matches('-').to_string();
            if !known_flags.contains(&name.as_str()) {
                bail!("unknown flag {it}");
            }
            let val = items
                .get(i + 1)
                .with_context(|| format!("flag --{name} needs a value"))?;
            if val.starts_with("--") {
                bail!("flag --{name} needs a value (got {val})");
            }
            flags.insert(name, val.clone());
            i += 2;
        } else {
            pos.push(it.clone());
            i += 1;
        }
    }
    if pos.len() != n_pos {
        bail!("expected {n_pos} positional argument(s), got {}", pos.len());
    }
    Ok(Parsed { pos, flags })
}

fn path_arg(s: &str) -> Result<std::path::PathBuf> {
    let p = std::path::PathBuf::from(s);
    if !p.is_file() {
        bail!("not a file: {}", p.display());
    }
    Ok(p)
}

// ---------------------------------------------------------------------------
// probe-mkdir / probe-ls / probe-ensure-dir / probe-rm
// ---------------------------------------------------------------------------

pub async fn cmd_mkdir(items: &[String]) -> Result<i32> {
    let a = parse_args(items, &[], 2)?;
    let (client, token) = load().await?;
    let pid = a.pos[0].clone();
    let name = a.pos[1].clone();
    let fid = api::mkdir(&client, &token, &pid, &name).await?;
    println!("file_id={fid}");
    summarize(
        "probe-mkdir",
        &[("pid", pid), ("name", name), ("file_id", fid)],
    );
    Ok(0)
}

pub async fn cmd_ls(items: &[String]) -> Result<i32> {
    let a = parse_args(items, &["cid", "limit"], 0)?;
    let cid = a
        .flags
        .get("cid")
        .cloned()
        .unwrap_or_else(|| "0".to_string());
    let limit: i64 = a
        .flags
        .get("limit")
        .map(|s| s.parse())
        .transpose()
        .context("--limit must be an integer")?
        .unwrap_or(1150)
        .clamp(1, 1150);
    let (client, token) = load().await?;
    let (rows, count) = api::list_all(&client, &token, &cid, limit).await?;
    println!("fid                 fc  fs             fn                                      pc                 sha1(masked)     upt");
    for r in &rows {
        println!(
            "{:<20} {:<3} {:>12}  {:<40} {:<18} {:<16} {}",
            r.fid,
            r.fc,
            r.fs,
            r.fname,
            r.pc,
            mask(&r.sha1),
            r.upt
        );
    }
    summarize(
        "probe-ls",
        &[
            ("cid", cid),
            ("count", count.to_string()),
            ("listed", rows.len().to_string()),
        ],
    );
    Ok(0)
}

pub async fn cmd_ensure_dir(items: &[String]) -> Result<i32> {
    parse_args(items, &[], 0)?;
    let (client, token) = load().await?;
    let (cid, created) = upload::ensure_dir(&client, &token).await?;
    println!("cid={cid} created={created}");
    summarize(
        "probe-ensure-dir",
        &[
            ("cid", cid.clone()),
            ("created", u8::from(created).to_string()),
            ("name", upload::E2E_DIR_NAME.to_string()),
        ],
    );
    Ok(0)
}

pub async fn cmd_rm(items: &[String]) -> Result<i32> {
    let a = parse_args(items, &[], 2)?;
    let (client, token) = load().await?;
    let fid = a.pos[0].clone();
    let parent = a.pos[1].clone();
    api::delete(&client, &token, &fid, &parent).await?;
    println!("deleted (recycle-bin semantics): fid={fid} parent={parent}");
    summarize(
        "probe-rm",
        &[
            ("fid", fid),
            ("parent_cid", parent),
            ("ok", "1".to_string()),
        ],
    );
    Ok(0)
}

// ---------------------------------------------------------------------------
// probe-upload / probe-rapid / probe-garbage-hash
// ---------------------------------------------------------------------------

/// init -> (optional OSS transfer) -> size verification against get_info.
/// Returns (file_id, remote_size, mode, reauth_rounds, cb_body_kind).
struct UploadOutcome {
    file_id: String,
    remote_size: i64,
    mode: &'static str,
    parts: usize,
    reauth: u32,
    cb_body: &'static str,
}

async fn run_upload(
    client: &reqwest::Client,
    token: &str,
    hashes: &upload::FileHashes,
    target_cid: &str,
    hash_override: Option<(&str, &str)>,
    transfer: bool,
) -> Result<UploadOutcome> {
    let init = upload::init_with_reauth(client, token, hashes, target_cid, hash_override).await?;
    if init.resp.status == 2 {
        let fid = init
            .resp
            .file_id
            .clone()
            .context("rapid hit without file_id in init response")?;
        let info = api::get_info(client, token, &fid).await?;
        if info.size_byte != hashes.file_size {
            bail!(
                "rapid-hit size mismatch: local {} vs remote {}",
                hashes.file_size,
                info.size_byte
            );
        }
        return Ok(UploadOutcome {
            file_id: fid,
            remote_size: info.size_byte,
            mode: "rapid",
            parts: 0,
            reauth: init.reauth_rounds,
            cb_body: "n/a",
        });
    }
    if !transfer {
        bail!("probe asked init-only but server wants an OSS upload (status=1)");
    }
    // status == 1: OSS path
    let bucket = init
        .resp
        .bucket
        .clone()
        .context("init status=1 without bucket")?;
    let object = init
        .resp
        .object
        .clone()
        .context("init status=1 without object")?;
    let cb = init
        .resp
        .callback
        .clone()
        .context("init status=1 without callback")?;
    let report =
        upload::oss_transfer(client, token, &bucket, &object, Some(&cb), &hashes.data).await?;
    let row = upload::find_in_dir(client, token, target_cid, &hashes.file_name)
        .await?
        .with_context(|| {
            format!(
                "uploaded file {} not visible in dir {target_cid}",
                hashes.file_name
            )
        })?;
    let info = api::get_info(client, token, &row.fid).await?;
    if info.size_byte != hashes.file_size {
        bail!(
            "post-upload size mismatch: local {} vs remote {} (fid {})",
            hashes.file_size,
            info.size_byte,
            row.fid
        );
    }
    Ok(UploadOutcome {
        file_id: row.fid,
        remote_size: info.size_byte,
        mode: report.mode,
        parts: report.parts,
        reauth: init.reauth_rounds,
        cb_body: report.body_kind,
    })
}

pub async fn cmd_upload(items: &[String]) -> Result<i32> {
    let a = parse_args(items, &["target-cid"], 1)?;
    let path = path_arg(&a.pos[0])?;
    let (client, token) = load().await?;
    let target_cid = match a.flags.get("target-cid") {
        Some(c) => c.clone(),
        None => upload::ensure_dir(&client, &token).await?.0,
    };
    let hashes = upload::compute_file_hashes(&path)?;
    println!(
        "[hash] {} size={} sha1={} preid={}",
        hashes.file_name,
        hashes.file_size,
        mask(&hashes.sha1_full),
        mask(&hashes.sha1_pre)
    );
    let t0 = Instant::now();
    let out = run_upload(&client, &token, &hashes, &target_cid, None, true).await?;
    let elapsed = t0.elapsed().as_secs_f64();
    if out.mode == "rapid" {
        println!("秒传命中 (status=2)");
    }
    println!("file_id={}", out.file_id);
    summarize(
        "probe-upload",
        &[
            ("file", hashes.file_name.clone()),
            ("size", hashes.file_size.to_string()),
            ("mode", out.mode.to_string()),
            ("parts", out.parts.to_string()),
            ("reauth", out.reauth.to_string()),
            ("file_id", out.file_id.clone()),
            ("remote_size", out.remote_size.to_string()),
            ("cb_body", out.cb_body.to_string()),
            ("elapsed_s", format!("{elapsed:.2}")),
            ("target_cid", target_cid),
        ],
    );
    Ok(0)
}

pub async fn cmd_rapid(items: &[String]) -> Result<i32> {
    let a = parse_args(items, &[], 1)?;
    let path = path_arg(&a.pos[0])?;
    let (client, token) = load().await?;
    let target_cid = upload::ensure_dir(&client, &token).await?.0;
    let hashes = upload::compute_file_hashes(&path)?;
    let init = upload::init_with_reauth(&client, &token, &hashes, &target_cid, None).await?;
    println!(
        "init status={} file_id={:?} reauth_rounds={}",
        init.resp.status,
        init.resp.file_id.as_deref().unwrap_or("-"),
        init.reauth_rounds
    );
    if init.resp.status == 2 {
        let fid = init.resp.file_id.context("rapid hit without file_id")?;
        summarize(
            "probe-rapid",
            &[
                ("file", hashes.file_name.clone()),
                ("sha1", mask(&hashes.sha1_full)),
                ("status", "2".to_string()),
                ("hit", "1".to_string()),
                ("file_id", fid),
            ],
        );
        Ok(0)
    } else {
        summarize(
            "probe-rapid",
            &[
                ("file", hashes.file_name.clone()),
                ("sha1", mask(&hashes.sha1_full)),
                ("status", init.resp.status.to_string()),
                ("hit", "0".to_string()),
            ],
        );
        bail!(
            "probe-rapid: expected status=2 (file was uploaded earlier), got {}",
            init.resp.status
        )
    }
}

pub async fn cmd_garbage_hash(items: &[String]) -> Result<i32> {
    let a = parse_args(items, &[], 1)?;
    let path = path_arg(&a.pos[0])?;
    let (client, token) = load().await?;
    let target_cid = upload::ensure_dir(&client, &token).await?.0;
    let hashes = upload::compute_file_hashes(&path)?;
    // Fake but shape-valid hashes: 40x 'A' (SHA1 hex length).
    let fake = "A".repeat(40);
    println!(
        "[garbage] fileid={} preid={} (both fake; real sha1={})",
        fake,
        fake,
        mask(&hashes.sha1_full)
    );
    match upload::init_with_reauth(
        &client,
        &token,
        &hashes,
        &target_cid,
        Some((fake.as_str(), fake.as_str())),
    )
    .await
    {
        Ok(init) => {
            let status = init.resp.status;
            println!(
                "[garbage] init ACCEPTED fake hashes: status={} bucket={:?} object={:?} callback={} reauth_rounds={}",
                status,
                init.resp.bucket.as_deref().unwrap_or("-"),
                init.resp.object.as_deref().unwrap_or("-"),
                init.resp.callback.as_ref().map(|c| c.len_hint()).unwrap_or_else(|| "none".into()),
                init.reauth_rounds
            );
            if status == 1 {
                let sts = api::get_token(&client, &token).await?;
                println!(
                    "[garbage] get_token reachable: endpoint={} ak_id={} ak_secret={} token={} expires={} — NOT transferring any data",
                    sts.endpoint,
                    mask(&sts.access_key_id),
                    mask(&sts.access_key_secret),
                    mask(&sts.security_token),
                    sts.expiration
                );
                summarize(
                    "probe-garbage-hash",
                    &[
                        ("init", "accepted".to_string()),
                        ("status", status.to_string()),
                        ("server_checks_hash", "no-at-init".to_string()),
                        ("get_token", "ok".to_string()),
                        ("transferred", "0B".to_string()),
                    ],
                );
            } else {
                summarize(
                    "probe-garbage-hash",
                    &[
                        ("init", "accepted".to_string()),
                        ("status", status.to_string()),
                        ("server_checks_hash", format!("no(status={status})")),
                        ("get_token", "skipped".to_string()),
                    ],
                );
            }
            Ok(0)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            println!(
                "[garbage] init REJECTED fake hashes: {}",
                msg.chars().take(300).collect::<String>()
            );
            summarize(
                "probe-garbage-hash",
                &[
                    ("init", "rejected".to_string()),
                    ("server_checks_hash", "yes".to_string()),
                    ("err_head", msg.chars().take(120).collect::<String>()),
                ],
            );
            Ok(0)
        }
    }
}

// ---------------------------------------------------------------------------
// probe-downurl: UA binding matrix
// ---------------------------------------------------------------------------

fn ua_for(kind: &str) -> String {
    match kind {
        "browser" => CHROME_UA.to_string(),
        "spike" => crate::auth::UA.to_string(),
        "empty" => String::new(),
        other => panic!("unknown --ua {other}"),
    }
}

fn other_ua(kind: &str) -> &'static str {
    if kind == "browser" {
        crate::auth::UA
    } else {
        CHROME_UA
    }
}

fn header_str(headers: &reqwest::header::HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string()
}

pub async fn cmd_downurl(items: &[String]) -> Result<i32> {
    let a = parse_args(items, &["ua"], 1)?;
    let fid = a.pos[0].clone();
    let ua_kind = a
        .flags
        .get("ua")
        .cloned()
        .unwrap_or_else(|| "browser".to_string());
    if !matches!(ua_kind.as_str(), "browser" | "spike" | "empty") {
        bail!("--ua must be browser|spike|empty");
    }
    let ua = ua_for(&ua_kind);
    let (client, token) = load().await?;

    let info = api::get_info(&client, &token, &fid).await?;
    println!(
        "[info] fid={} file={} category={} size={} size_byte={} sha1={} pc={}",
        info.file_id,
        info.file_name,
        info.file_category,
        info.size,
        info.size_byte,
        mask(&info.sha1),
        info.pick_code
    );

    let url = api::downurl(&client, &token, &info.pick_code, &ua).await?;
    let url_display: String = url.chars().take(96).collect();
    println!(
        "[downurl] ua_kind={ua_kind} url(len={})={url_display}...",
        url.chars().count()
    );

    // HEAD with the SAME UA
    let head = client
        .head(&url)
        .header(reqwest::header::USER_AGENT, ua.as_str())
        .send()
        .await
        .context("HEAD direct link")?;
    let head_status = head.status().as_u16();
    let accept_ranges = header_str(head.headers(), "accept-ranges");
    let etag = header_str(head.headers(), "etag");
    let content_length = header_str(head.headers(), "content-length");
    println!(
        "[head] status={head_status} accept-ranges={accept_ranges} etag={etag} content-length={content_length}"
    );

    // Range GET first 1KiB with the SAME UA
    let ranged = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, ua.as_str())
        .header(reqwest::header::RANGE, "bytes=0-1023")
        .send()
        .await
        .context("Range GET direct link")?;
    let range_status = ranged.status().as_u16();
    let content_range = header_str(ranged.headers(), "content-range");
    let body = ranged.bytes().await.context("Range GET body")?;
    let n = body.len();
    println!("[range] status={range_status} content-range={content_range} bytes={n}");

    // UA MISMATCH: plain GET with a different UA -> expect 403 (binding proof)
    let mismatch_ua = other_ua(&ua_kind);
    let mismatch = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, mismatch_ua)
        .header(reqwest::header::RANGE, "bytes=0-1023")
        .send()
        .await
        .context("mismatch-UA GET")?;
    let mismatch_status = mismatch.status().as_u16();

    let range_ok = range_status == 206 && content_range.starts_with("bytes 0-1023/") && n == 1024;
    summarize(
        "probe-downurl",
        &[
            ("fid", fid),
            ("ua", ua_kind),
            ("head", head_status.to_string()),
            ("accept_ranges", accept_ranges.clone()),
            ("etag", etag.clone()),
            ("range_status", range_status.to_string()),
            ("range_bytes", n.to_string()),
            ("range_ok", u8::from(range_ok).to_string()),
            ("mismatch_ua", mismatch_ua.to_string()),
            ("mismatch_status", mismatch_status.to_string()),
        ],
    );
    if !range_ok {
        bail!("Range leg failed (status={range_status}, content-range={content_range}, bytes={n})");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// probe-resume: two-phase breakpoint verification
// ---------------------------------------------------------------------------

pub async fn cmd_resume(items: &[String]) -> Result<i32> {
    let a = parse_args(items, &[], 1)?;
    let path = path_arg(&a.pos[0])?;
    let (client, token) = load().await?;

    if let Some(session) = upload::load_session() {
        if session.file_path != path.display().to_string() {
            bail!(
                "session file belongs to {} (got {}). Delete {} or rerun with the session's file",
                session.file_path,
                path.display(),
                upload::session_path().display()
            );
        }
        let hashes = upload::compute_file_hashes(&path)?;
        if hashes.file_size != session.file_size || hashes.sha1_full != session.fileid {
            bail!("file changed since the session was captured (size/sha1 mismatch)");
        }
        println!(
            "[resume] session: upload_id={} part1_etag={}",
            session.upload_id, session.part1_etag
        );
        let resume = api::upload_resume(
            &client,
            &token,
            hashes.file_size,
            &format!("U_1_{}", session.target_cid),
            &hashes.sha1_full,
            &session.pick_code,
        )
        .await?;
        println!(
            "[resume] resume pick_code={} bucket={:?} object={:?} callback={}",
            mask(&resume.pick_code),
            resume.bucket,
            resume.object,
            resume
                .callback
                .as_ref()
                .map(|c| c.len_hint())
                .unwrap_or_else(|| "none".into())
        );
        let sts = api::get_token(&client, &token).await?;
        // ctx built against the CURRENT resume bucket/object
        let endpoint = sts
            .endpoint
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .to_string();
        let ctx = crate::oss::OssCtx {
            endpoint,
            bucket: resume.bucket.clone(),
            object: resume.object.clone(),
            access_key_id: sts.access_key_id.clone(),
            access_key_secret: sts.access_key_secret.clone(),
            security_token: sts.security_token.clone(),
        };

        // Reuse the old uploadId only when bucket/object still match.
        let (upload_id, existing) =
            if resume.bucket == session.bucket && resume.object == session.object {
                match crate::oss::list_parts(&client, &ctx, &session.upload_id).await {
                    Ok(parts) => {
                        for p in &parts {
                            println!(
                                "[resume] existing part #{} size={} etag={}",
                                p.part_number, p.size, p.etag
                            );
                        }
                        (session.upload_id.clone(), parts)
                    }
                    Err(e)
                        if crate::oss::oss_error_of(&e)
                            .map(|oe| oe.is_no_such_upload())
                            .unwrap_or(false) =>
                    {
                        println!("[resume] old uploadId gone (NoSuchUpload) — fresh initiate");
                        (
                            crate::oss::initiate_multipart(&client, &ctx).await?,
                            Vec::new(),
                        )
                    }
                    Err(e) => return Err(e),
                }
            } else {
                println!("[resume] bucket/object changed — old uploadId invalid, fresh initiate");
                (
                    crate::oss::initiate_multipart(&client, &ctx).await?,
                    Vec::new(),
                )
            };

        let mut parts: Vec<(u32, String)> = existing
            .iter()
            .map(|p| (p.part_number, p.etag.clone()))
            .collect();
        let reused = parts.len();
        let total_parts = hashes.data.len().div_ceil(upload::PART_SIZE);
        for n in 1..=total_parts as u32 {
            if parts.iter().any(|(pn, _)| *pn == n) {
                continue;
            }
            let start = (n as usize - 1) * upload::PART_SIZE;
            let end = ((n as usize) * upload::PART_SIZE).min(hashes.data.len());
            let etag = crate::oss::upload_part(
                &client,
                &ctx,
                &upload_id,
                n,
                hashes.data[start..end].to_vec(),
            )
            .await?;
            println!("[resume] uploaded missing part {n}: etag={etag}");
            parts.push((n, etag));
        }
        parts.sort_by_key(|(n, _)| *n);
        let cb = resume
            .callback
            .clone()
            .context("resume response without callback")?;
        let (_status, body) =
            crate::oss::complete_multipart(&client, &ctx, &upload_id, &parts, Some(&cb)).await?;
        let body_kind = if body.trim_start().starts_with('{') {
            "json"
        } else {
            "xml"
        };
        println!("[resume] complete body kind={body_kind}");

        let row = upload::find_in_dir(&client, &token, &session.target_cid, &hashes.file_name)
            .await?
            .with_context(|| format!("resumed file {} not visible", hashes.file_name))?;
        let info = api::get_info(&client, &token, &row.fid).await?;
        if info.size_byte != hashes.file_size {
            bail!(
                "resumed size mismatch: local {} remote {}",
                hashes.file_size,
                info.size_byte
            );
        }
        upload::clear_session();
        println!("[resume] session cleared, file_id={}", row.fid);
        summarize(
            "probe-resume",
            &[
                ("phase", "complete".to_string()),
                ("reused_parts", reused.to_string()),
                ("uploaded_parts", (parts.len() - reused).to_string()),
                ("file_id", row.fid.clone()),
                ("remote_size", info.size_byte.to_string()),
                ("cb_body", body_kind.to_string()),
            ],
        );
        Ok(0)
    } else {
        // Phase A: fresh init -> initiate -> upload ONLY part 1 -> persist session.
        let target_cid = upload::ensure_dir(&client, &token).await?.0;
        let hashes = upload::compute_file_hashes(&path)?;
        if hashes.data.len() <= upload::PART_SIZE {
            bail!(
                "probe-resume needs a multi-part file (> {} bytes)",
                upload::PART_SIZE
            );
        }
        let init = upload::init_with_reauth(&client, &token, &hashes, &target_cid, None).await?;
        if init.resp.status == 2 {
            bail!("file already rapid-hits — use a fresh random file for probe-resume");
        }
        let bucket = init.resp.bucket.clone().context("init without bucket")?;
        let object = init.resp.object.clone().context("init without object")?;
        let sts = api::get_token(&client, &token).await?;
        let endpoint = sts
            .endpoint
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .to_string();
        let ctx = crate::oss::OssCtx {
            endpoint,
            bucket: bucket.clone(),
            object: object.clone(),
            access_key_id: sts.access_key_id.clone(),
            access_key_secret: sts.access_key_secret.clone(),
            security_token: sts.security_token.clone(),
        };
        let upload_id = crate::oss::initiate_multipart(&client, &ctx).await?;
        let etag1 = crate::oss::upload_part(
            &client,
            &ctx,
            &upload_id,
            1,
            hashes.data[..upload::PART_SIZE].to_vec(),
        )
        .await?;
        println!("[resume] phase A: upload_id={upload_id} part1_etag={etag1} (parts 2.. NOT uploaded, no complete)");
        let session = upload::ResumeSession {
            file_path: path.display().to_string(),
            file_name: hashes.file_name.clone(),
            file_size: hashes.file_size,
            target_cid,
            fileid: hashes.sha1_full.clone(),
            pick_code: init.resp.pick_code.clone(),
            bucket,
            object,
            upload_id,
            part1_etag: etag1.clone(),
            created_unix: crate::now_unix(),
        };
        upload::save_session(&session)?;
        println!(
            "[resume] session saved: {}",
            upload::session_path().display()
        );
        summarize(
            "probe-resume",
            &[
                ("phase", "partial".to_string()),
                ("upload_id", session.upload_id.clone()),
                ("part1_etag", etag1),
                ("session", upload::session_path().display().to_string()),
                ("pick_code", mask(&session.pick_code)),
            ],
        );
        Ok(0)
    }
}

// ---------------------------------------------------------------------------
// probe-qps
// ---------------------------------------------------------------------------

enum QpsOutcome {
    Ok(f64),
    Api(api::ApiErr),
    Skipped,
}

fn pct(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((q * sorted.len() as f64).floor() as usize).min(sorted.len() - 1);
    sorted[idx]
}

pub async fn cmd_qps(items: &[String]) -> Result<i32> {
    let a = parse_args(items, &["rps", "secs", "cid"], 0)?;
    let rps: u64 = a
        .flags
        .get("rps")
        .map(|s| s.parse())
        .transpose()
        .context("--rps must be an integer")?
        .unwrap_or(5);
    let secs: u64 = a
        .flags
        .get("secs")
        .map(|s| s.parse())
        .transpose()
        .context("--secs must be an integer")?
        .unwrap_or(10);
    if rps == 0 || rps > 100 || secs == 0 || secs > 300 {
        bail!("--rps (1..=100) / --secs (1..=300) out of range");
    }
    let (client, token) = load().await?;
    let cid = match a.flags.get("cid") {
        Some(c) => c.clone(),
        None => upload::ensure_dir(&client, &token).await?.0,
    };
    let total = rps * secs;
    println!("[qps] rps={rps} secs={secs} cid={cid} total={total} endpoint=ufile/files?limit=1");

    let stop = Arc::new(AtomicBool::new(false));
    let interval = std::time::Duration::from_nanos(1_000_000_000u64 / rps);
    let mut set = tokio::task::JoinSet::new();
    for seq in 0..total {
        let client = client.clone();
        let token = token.clone();
        let cid = cid.clone();
        let stop = stop.clone();
        set.spawn(async move {
            tokio::time::sleep(interval * seq as u32).await;
            if stop.load(Ordering::SeqCst) {
                return (seq, QpsOutcome::Skipped);
            }
            let t0 = Instant::now();
            let out = match api::list_files_page(&client, &token, &cid, 1, 0).await {
                Ok(_) => QpsOutcome::Ok(t0.elapsed().as_secs_f64() * 1000.0),
                Err(e) => QpsOutcome::Api(e),
            };
            if let QpsOutcome::Api(e) = &out {
                if matches!(e.kind, ErrKind::RateLimited | ErrKind::HumanVerify) {
                    stop.store(true, Ordering::SeqCst);
                }
            }
            (seq, out)
        });
    }

    let mut latencies = Vec::new();
    let mut api_errs: Vec<(u64, api::ApiErr)> = Vec::new();
    let mut skipped = 0u64;
    while let Some(joined) = set.join_next().await {
        let (seq, out) = joined.context("qps task panicked")?;
        match out {
            QpsOutcome::Ok(ms) => latencies.push(ms),
            QpsOutcome::Api(e) => {
                println!("[qps] #{seq}: {e}");
                api_errs.push((seq, e));
            }
            QpsOutcome::Skipped => skipped += 1,
        }
    }

    let mut human_verify: Vec<&(u64, api::ApiErr)> = api_errs
        .iter()
        .filter(|(_, e)| e.kind == ErrKind::HumanVerify)
        .collect();
    let first_limit = api_errs
        .iter()
        .find(|(_, e)| e.kind == ErrKind::RateLimited || e.kind == ErrKind::HumanVerify)
        .map(|(seq, e)| (seq, e.kind.label().to_string(), e.code, e.http));

    latencies.sort_by(|x, y| x.partial_cmp(y).expect("no NaN"));
    let avg = latencies.iter().sum::<f64>() / latencies.len().max(1) as f64;
    println!(
        "[qps] ok={} err={} skipped={} lat(ms): min={:.0} p50={:.0} p95={:.0} max={:.0} avg={:.0}",
        latencies.len(),
        api_errs.len(),
        skipped,
        latencies.first().copied().unwrap_or(0.0),
        pct(&latencies, 0.5),
        pct(&latencies, 0.95),
        latencies.last().copied().unwrap_or(0.0),
        avg
    );
    summarize(
        "probe-qps",
        &[
            ("rps", rps.to_string()),
            ("secs", secs.to_string()),
            ("cid", cid),
            ("sent", (total - skipped).to_string()),
            ("ok", latencies.len().to_string()),
            ("err", api_errs.len().to_string()),
            ("p50_ms", format!("{:.0}", pct(&latencies, 0.5))),
            ("p95_ms", format!("{:.0}", pct(&latencies, 0.95))),
            (
                "first_limit_seq",
                first_limit
                    .as_ref()
                    .map(|(s, _, _, _)| s.to_string())
                    .unwrap_or_else(|| "none".into()),
            ),
            (
                "first_limit_code",
                first_limit
                    .as_ref()
                    .map(|(_, kind, code, _)| format!("{kind}/{code}"))
                    .unwrap_or_else(|| "none".into()),
            ),
        ],
    );
    if let Some((seq, e)) = human_verify.pop() {
        bail!("911 HUMAN VERIFICATION REQUIRED at request #{seq} ({e}) — STOP ALL 115 ACTIVITY");
    }
    Ok(0)
}
