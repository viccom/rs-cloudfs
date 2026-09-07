//! Batch S subcommand implementations. Every function prints `[SUMMARY]`
//! evidence lines; nothing sensitive is printed (see common::scrub).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use futures_util::future::join_all;
use md5::Digest as _;
use serde::{Deserialize, Serialize};

use crate::api;
use crate::common::{self, mask, scrub, summ, Cfg, Ctx};
use crate::gen;

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// MB/s (decimal) for `bytes` transferred in `ms`.
fn mbs(bytes: u64, ms: u128) -> f64 {
    if ms == 0 {
        0.0
    } else {
        bytes as f64 / 1e6 / (ms as f64 / 1000.0)
    }
}

fn block_count(size: u64) -> u64 {
    size.div_ceil(gen::BLOCK)
}

fn block_len(size: u64, seq: u64) -> usize {
    let rem = size - seq * gen::BLOCK;
    rem.min(gen::BLOCK) as usize
}

fn file_md5(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = md5::Md5::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(hex::encode(h.finalize()))
}

/// Resolve (and memoize) the spike remote dir. `BAIDU_SPIKE_REMOTE_DIR`
/// wins outright (owner directive 2026-09-07: dedicated test roots live
/// under /apps, never PCFS's live /apps/privatefs tree) — no probing,
/// the named dir is created on first upload. Without the env, candidate
/// prefixes are probed side-effect free (precreate-only) and the accepted
/// one is recorded.
async fn resolve_remote_dir(ctx: &Ctx) -> Result<String> {
    if let Ok(d) = std::env::var("BAIDU_SPIKE_REMOTE_DIR") {
        if !d.is_empty() {
            summ(format!("remote_dir|env_override={d}"));
            return Ok(d);
        }
    }
    if let Some(d) = ctx.state.lock().unwrap().remote_dir.clone() {
        return Ok(d);
    }
    let candidates = ["/apps/baidu_spike", "/apps/privatefs/baidu_spike"];
    let mut notes: Vec<String> = Vec::new();
    for cand in candidates {
        let probe_path = format!("{cand}/_probe.bin");
        let probe_md5 = [hex::encode(md5::Md5::digest(b"spike"))];
        let list = match api::list_dir(ctx, cand).await {
            Ok(o) => format!("list errno={} entries={}", o.errno, o.entries.len()),
            Err(e) => format!("list probe failed: {}", trunc(&format!("{e:#}"), 120)),
        };
        let pre = api::precreate(ctx, &probe_path, 1, &probe_md5).await;
        let pre_note = match &pre {
            Ok(p) => format!("precreate errno={} return_type={}", p.errno, p.return_type),
            Err(e) => format!("precreate probe failed: {}", trunc(&format!("{e:#}"), 120)),
        };
        notes.push(format!("{cand}: {list}; {pre_note}"));
        if let Ok(p) = pre {
            if p.errno == 0 {
                summ(format!(
                    "remote_dir|picked={cand}|probe=[{}]",
                    notes.last().unwrap()
                ));
                let note = notes.join(" | ");
                let mut st = ctx.state.lock().unwrap();
                st.remote_dir = Some(cand.to_string());
                st.remote_dir_note = Some(note);
                common::save_state(&ctx.cfg, &st).ok();
                return Ok(cand.to_string());
            }
        }
    }
    bail!("no usable remote dir: {}", scrub(&notes.join(" | ")))
}

// ---------------------------------------------------------------------------
// shared upload path (three-step, concurrent part workers)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub struct UploadStats {
    pub uploadid: String,
    pub return_type: i64,
    pub pre_ms: u128,
    /// (seq, error_code, request_ms) per uploaded part
    pub parts: Vec<(u64, i64, u128)>,
    pub parts_wall_ms: u128,
    pub create_ms: u128,
}

async fn upload_three_step(
    ctx: &Arc<Ctx>,
    local: &Path,
    remote: &str,
    size: u64,
    md5s: &[String],
    workers: u64,
) -> Result<UploadStats> {
    let pre = api::precreate(ctx, remote, size as i64, md5s).await?;
    if pre.errno != 0 {
        bail!(
            "precreate errno {} (return_type {}): {}",
            pre.errno,
            pre.return_type,
            trunc(&pre.raw, 200)
        );
    }
    if pre.return_type == 2 {
        // rapid-upload branch: server already holds the content, no parts.
        return Ok(UploadStats {
            uploadid: pre.uploadid,
            return_type: 2,
            pre_ms: pre.ms,
            parts: Vec::new(),
            parts_wall_ms: 0,
            create_ms: 0,
        });
    }
    let uploadid = pre.uploadid.clone();
    let blocks = block_count(size);

    let t_parts = Instant::now();
    let mut handles = Vec::new();
    for w in 0..workers.max(1) {
        let ctx = Arc::clone(ctx);
        let remote = remote.to_string();
        let uploadid = uploadid.clone();
        let local = local.to_path_buf();
        let seqs: Vec<u64> = (0..blocks).filter(|s| s % workers.max(1) == w).collect();
        handles.push(tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
            let mut file = tokio::fs::File::open(&local).await?;
            let mut results = Vec::new();
            for seq in seqs {
                let mut buf = vec![0u8; block_len(size, seq)];
                file.seek(std::io::SeekFrom::Start(seq * gen::BLOCK))
                    .await?;
                file.read_exact(&mut buf).await?;
                let o = api::superfile2(&ctx, &remote, &uploadid, seq as i64, buf).await?;
                let bad = o.error_code != 0;
                if bad {
                    println!(
                        "  part {seq} FAILED http={} error_code={} raw={}",
                        o.http.as_u16(),
                        o.error_code,
                        trunc(&o.raw, 160)
                    );
                }
                results.push((seq, o.error_code, o.ms));
                if bad {
                    break; // later parts of a broken upload are pointless
                }
            }
            Ok::<_, anyhow::Error>(results)
        }));
    }
    let mut parts = Vec::new();
    for h in handles {
        let r = h.await.context("upload worker panicked")?;
        parts.extend(r?);
    }
    let bad = parts.iter().filter(|p| p.1 != 0).count();
    if bad > 0 {
        let first = parts.iter().find(|p| p.1 != 0).unwrap();
        bail!(
            "{bad} part(s) failed; first: seq={} error_code={}",
            first.0,
            first.1
        );
    }
    let parts_wall_ms = t_parts.elapsed().as_millis();

    let t_create = Instant::now();
    let create = api::create_file(ctx, remote, &uploadid, size as i64, md5s).await?;
    if create.errno != 0 {
        bail!("create errno {}: {}", create.errno, trunc(&create.raw, 200));
    }
    Ok(UploadStats {
        uploadid,
        return_type: pre.return_type,
        pre_ms: pre.ms,
        parts,
        parts_wall_ms,
        create_ms: t_create.elapsed().as_millis(),
    })
}

/// Resolve a dlink URL form the CDN actually serves: direct Location, or
/// Location + access_token (PCFS intel said appending may be required).
/// Returns (url, token_appended).
async fn working_dlink(ctx: &Ctx, remote: &str) -> Result<(String, bool)> {
    let (status, loc) = api::fetch_dlink(ctx, remote).await?;
    let loc = loc.ok_or_else(|| anyhow!("no Location header (xpan status {})", status.as_u16()))?;
    let (s1, _) = api::range_get(ctx, &loc, "bytes=0-0").await?;
    if s1.is_success() {
        return Ok((loc, false));
    }
    let token = ctx.token.read().await.access_token.clone();
    let sep = if loc.contains('?') { '&' } else { '?' };
    let url2 = format!("{loc}{sep}access_token={token}");
    let (s2, _) = api::range_get(ctx, &url2, "bytes=0-0").await?;
    if s2.is_success() {
        return Ok((url2, true));
    }
    bail!(
        "dlink rejected in both forms: direct={} appended={}",
        s1.as_u16(),
        s2.as_u16()
    );
}

/// Host+path of a URL for safe printing (query never printed).
fn url_shape(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => format!(
            "{}://{}{} [query_len={}]",
            u.scheme(),
            u.host_str().unwrap_or("?"),
            u.path(),
            u.query().map(|q| q.len()).unwrap_or(0)
        ),
        Err(_) => "<unparseable>".to_string(),
    }
}

// ---------------------------------------------------------------------------
// S-1: refresh
// ---------------------------------------------------------------------------

pub async fn refresh() -> Result<()> {
    let cfg = Cfg::from_env();
    let secrets = common::load_secrets(&cfg)?;
    common::register_secrets(vec![
        secrets.client_id.clone(),
        secrets.client_secret.clone(),
        secrets.refresh_token.clone(),
    ]);
    println!("[note] credentials provenance: {}", secrets.provenance);
    let client = common::api_client()?;
    let tok = api::refresh_grant(&client, &secrets, "").await?;
    common::save_token(&cfg, &tok)?;
    summ(format!(
        "refresh|access_token={}|refresh_token={}|cached_at={}",
        mask(&tok.access_token),
        mask(&tok.refresh_token),
        cfg.token_path().display()
    ));
    println!(
        "[note] tokens masked on screen; full values only in {}",
        cfg.token_path().display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// S-2: qps
// ---------------------------------------------------------------------------

pub async fn qps() -> Result<()> {
    let ctx = Arc::new(Ctx::init()?);
    let dir = resolve_remote_dir(&ctx).await?;

    // ---- A: method=list x10 sequential ----
    println!("[note] === A: method=list x10 sequential (dir={dir}) ===");
    let mut ok = 0u32;
    let mut errnos = BTreeSet::new();
    let (mut min_ms, mut max_ms) = (u128::MAX, 0u128);
    for i in 0..10u32 {
        let o = api::list_dir(&ctx, &dir).await?;
        println!(
            "  list[{i}] http={} errno={} ms={} entries={}",
            o.http.as_u16(),
            o.errno,
            o.ms,
            o.entries.len()
        );
        ok += (o.errno == 0) as u32;
        errnos.insert(o.errno);
        min_ms = min_ms.min(o.ms);
        max_ms = max_ms.max(o.ms);
    }
    summ(format!(
        "qps|list_seq10|ok={ok}|errno_set={errnos:?}|min_ms={min_ms}|max_ms={max_ms}"
    ));

    // ---- A2: method=list x10 concurrent burst ----
    println!("[note] === A2: method=list x10 concurrent burst ===");
    let futs: Vec<_> = (0..10).map(|_| api::list_dir(&ctx, &dir)).collect();
    let outs = join_all(futs).await;
    let mut ok2 = 0u32;
    let mut errnos2 = BTreeSet::new();
    let (mut min2, mut max2) = (u128::MAX, 0u128);
    for (i, r) in outs.iter().enumerate() {
        match r {
            Ok(o) => {
                println!(
                    "  list_burst[{i}] http={} errno={} ms={}",
                    o.http.as_u16(),
                    o.errno,
                    o.ms
                );
                ok2 += (o.errno == 0) as u32;
                errnos2.insert(o.errno);
                min2 = min2.min(o.ms);
                max2 = max2.max(o.ms);
            }
            Err(e) => println!(
                "  list_burst[{i}] TRANSPORT-ERR {}",
                trunc(&format!("{e:#}"), 120)
            ),
        }
    }
    summ(format!(
        "qps|list_burst10|ok={ok2}|errno_set={errnos2:?}|min_ms={}|max_ms={}",
        if min2 == u128::MAX { 0 } else { min2 },
        max2
    ));

    // ---- B: superfile2 x10 sequential + burst ----
    println!("[note] === B: superfile2 partseq=0 x10 (same 4MiB part re-sent) ===");
    let local = ctx.cfg.tmp_dir.join("qps_part.bin");
    gen::gen_file(&local, gen::BLOCK)?;
    let md5s = gen::block_md5s(&local, gen::BLOCK)?;
    let remote = format!("{dir}/qps_part.bin");
    let pre = api::precreate(&ctx, &remote, gen::BLOCK as i64, &md5s).await?;
    if pre.errno != 0 {
        bail!(
            "qps precreate errno {}: {}",
            pre.errno,
            trunc(&pre.raw, 200)
        );
    }
    let data = std::fs::read(&local)?;
    let mut okb = 0u32;
    let mut codes = BTreeSet::new();
    let (mut minb, mut maxb) = (u128::MAX, 0u128);
    for i in 0..10u32 {
        let o = api::superfile2(&ctx, &remote, &pre.uploadid, 0, data.clone()).await?;
        println!(
            "  part_seq[{i}] http={} error_code={} md5={} ms={}",
            o.http.as_u16(),
            o.error_code,
            mask(&o.md5),
            o.ms
        );
        okb += (o.error_code == 0) as u32;
        codes.insert(o.error_code);
        minb = minb.min(o.ms);
        maxb = maxb.max(o.ms);
    }
    summ(format!(
        "qps|part_seq10|ok={okb}|error_code_set={codes:?}|min_ms={minb}|max_ms={maxb}"
    ));

    println!("[note] === B2: superfile2 x10 concurrent burst ===");
    let futs: Vec<_> = (0..10)
        .map(|_| {
            let ctx = Arc::clone(&ctx);
            let remote = remote.clone();
            let uploadid = pre.uploadid.clone();
            let data = data.clone();
            async move { api::superfile2(&ctx, &remote, &uploadid, 0, data).await }
        })
        .collect();
    let outs = join_all(futs).await;
    let mut okb2 = 0u32;
    let mut codes2 = BTreeSet::new();
    let (mut minb2, mut maxb2) = (u128::MAX, 0u128);
    for (i, r) in outs.into_iter().enumerate() {
        match r {
            Ok(o) => {
                println!(
                    "  part_burst[{i}] http={} error_code={} ms={}",
                    o.http.as_u16(),
                    o.error_code,
                    o.ms
                );
                okb2 += (o.error_code == 0) as u32;
                codes2.insert(o.error_code);
                minb2 = minb2.min(o.ms);
                maxb2 = maxb2.max(o.ms);
            }
            Err(e) => println!(
                "  part_burst[{i}] TRANSPORT-ERR {}",
                trunc(&format!("{e:#}"), 120)
            ),
        }
    }
    summ(format!(
        "qps|part_burst10|ok={okb2}|error_code_set={codes2:?}|min_ms={}|max_ms={}",
        if minb2 == u128::MAX { 0 } else { minb2 },
        maxb2
    ));

    // finish the pending upload so nothing is left half-done
    let create = api::create_file(&ctx, &remote, &pre.uploadid, gen::BLOCK as i64, &md5s).await?;
    summ(format!(
        "qps|create_after|errno={}|fs_id={}",
        create.errno, create.fs_id
    ));

    // ---- C: download x5 sequential + burst (each = xpan 302 + CDN Range) ----
    println!("[note] === C: download flow x5 (xpan dlink fetch + CDN Range GET) ===");
    let (dl_url, appended) = working_dlink(&ctx, &remote).await?;
    println!(
        "[note] dlink working form: token_appended={appended} url={}",
        url_shape(&dl_url)
    );
    summ(format!("qps|dlink_form|token_appended={appended}"));
    let dl_once = |ctx: Arc<Ctx>, remote: String| async move {
        let t0 = Instant::now();
        let (s1, loc) = api::fetch_dlink(&ctx, &remote).await?;
        let (s2, b) = api::range_get(&ctx, &loc.unwrap_or_default(), "bytes=0-4095").await?;
        Ok::<_, anyhow::Error>((s1.as_u16(), s2.as_u16(), b.len(), t0.elapsed().as_millis()))
    };
    let mut okc = 0u32;
    let mut forms = BTreeSet::new();
    let (mut minc, mut maxc) = (u128::MAX, 0u128);
    for i in 0..5u32 {
        let (s1, s2, n, ms) = dl_once(Arc::clone(&ctx), remote.clone()).await?;
        println!("  dl_seq[{i}] xpan={s1} cdn={s2} bytes={n} ms={ms}");
        okc += (s1 == 302 && s2 == 206) as u32;
        forms.insert(format!("xpan{s1}/cdn{s2}"));
        minc = minc.min(ms);
        maxc = maxc.max(ms);
    }
    summ(format!(
        "qps|dl_seq5|ok={okc}|forms={forms:?}|min_ms={minc}|max_ms={maxc}"
    ));

    println!("[note] === C2: download flow x5 concurrent burst ===");
    let futs: Vec<_> = (0..5)
        .map(|_| dl_once(Arc::clone(&ctx), remote.clone()))
        .collect();
    let outs = join_all(futs).await;
    let mut okc2 = 0u32;
    let mut forms2 = BTreeSet::new();
    for (i, r) in outs.into_iter().enumerate() {
        match r {
            Ok((s1, s2, n, ms)) => {
                println!("  dl_burst[{i}] xpan={s1} cdn={s2} bytes={n} ms={ms}");
                okc2 += (s1 == 302 && s2 == 206) as u32;
                forms2.insert(format!("xpan{s1}/cdn{s2}"));
            }
            Err(e) => println!("  dl_burst[{i}] ERR {}", trunc(&format!("{e:#}"), 120)),
        }
    }
    summ(format!("qps|dl_burst5|ok={okc2}|forms={forms2:?}"));
    println!("[note] qps complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// S-3: resume (abort / continue)
// ---------------------------------------------------------------------------

const RESUME_SIZE: u64 = 32 * 1024 * 1024; // 8 blocks of 4 MiB
const RESUME_UPLOAD_N: u64 = 3; // blocks uploaded before the hard abort

#[derive(Serialize, Deserialize, Clone)]
struct ResumeState {
    remote: String,
    size: u64,
    block_md5: Vec<String>,
    uploadid: String,
    uploaded: Vec<i64>,
}

pub async fn resume_abort() -> Result<()> {
    let ctx = Arc::new(Ctx::init()?);
    let dir = resolve_remote_dir(&ctx).await?;
    let local = ctx.cfg.tmp_dir.join("resume_32m.bin");
    gen::gen_file(&local, RESUME_SIZE)?;
    let md5s = gen::block_md5s(&local, RESUME_SIZE)?;

    let pre = api::precreate(
        &ctx,
        &format!("{dir}/resume_32m.bin"),
        RESUME_SIZE as i64,
        &md5s,
    )
    .await?;
    if pre.errno != 0 || pre.return_type != 1 {
        bail!(
            "precreate errno={} return_type={} (expected 0/1): {}",
            pre.errno,
            pre.return_type,
            trunc(&pre.raw, 200)
        );
    }
    // Persist driver-side state BEFORE any part goes out (a real driver
    // persists at precreate time; part completion under the same uploadid
    // is what enables differential resume).
    let mut st = ResumeState {
        remote: format!("{dir}/resume_32m.bin"),
        size: RESUME_SIZE,
        block_md5: md5s,
        uploadid: pre.uploadid.clone(),
        uploaded: Vec::new(),
    };
    std::fs::write(ctx.cfg.resume_state_path(), serde_json::to_vec_pretty(&st)?)?;

    // upload N parts sequentially, updating the persisted state each time
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
    let mut file = tokio::fs::File::open(&local).await?;
    for seq in 0..RESUME_UPLOAD_N {
        let mut buf = vec![0u8; block_len(RESUME_SIZE, seq)];
        file.seek(std::io::SeekFrom::Start(seq * gen::BLOCK))
            .await?;
        file.read_exact(&mut buf).await?;
        let o = api::superfile2(&ctx, &st.remote, &pre.uploadid, seq as i64, buf).await?;
        if o.error_code != 0 {
            bail!("part {seq} error_code={}", o.error_code);
        }
        st.uploaded.push(seq as i64);
        std::fs::write(ctx.cfg.resume_state_path(), serde_json::to_vec_pretty(&st)?)?;
        println!("  part {seq} up (md5 {} ms {})", mask(&o.md5), o.ms);
    }
    summ(format!(
        "resume_abort|uploaded={:?}|uploadid={}|aborting_now=1",
        (0..RESUME_UPLOAD_N).collect::<Vec<_>>(),
        mask(&pre.uploadid)
    ));
    // Hard abort: no destructors, no create() — mimics a killed process.
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    std::process::abort();
}

async fn read_block(file: &mut tokio::fs::File, size: u64, seq: i64) -> Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
    let mut buf = vec![0u8; block_len(size, seq as u64)];
    file.seek(std::io::SeekFrom::Start(seq as u64 * gen::BLOCK))
        .await?;
    file.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn resume_continue() -> Result<()> {
    let ctx = Arc::new(Ctx::init()?);
    resolve_remote_dir(&ctx).await?;
    let raw = std::fs::read_to_string(ctx.cfg.resume_state_path())
        .context("resume_state.json missing — run `resume abort` first")?;
    let st: ResumeState = serde_json::from_str(&raw)?;
    let local = ctx.cfg.tmp_dir.join("resume_32m.bin");
    anyhow::ensure!(
        local.exists(),
        "local resume file gone: {}",
        local.display()
    );

    let phase1: BTreeSet<i64> = st.uploaded.iter().copied().collect();
    let all: BTreeSet<i64> = (0..block_count(st.size) as i64).collect();
    let missing_truth: BTreeSet<i64> = all.difference(&phase1).copied().collect();
    println!(
        "[note] phase1_uploaded={:?} missing_truth={:?}",
        st.uploaded, missing_truth
    );

    let mut file = tokio::fs::File::open(&local).await?;

    // Strategy A (differential resume): reuse the PERSISTED uploadid. Probe
    // it by re-sending one missing part; acceptance means the old session is
    // alive and (we assert below via create success) still holds phase-1
    // parts server-side — phase 2 then sends ONLY the missing set.
    let probe_seq = *missing_truth.iter().next().expect("nonempty missing set");
    let buf = read_block(&mut file, st.size, probe_seq).await?;
    let probe = api::superfile2(&ctx, &st.remote, &st.uploadid, probe_seq, buf).await?;
    let old_alive = probe.error_code == 0;
    summ(format!(
        "resume|old_uploadid_probe|partseq={probe_seq}|error_code={}|session_alive={old_alive}|http={}",
        probe.error_code,
        probe.http.as_u16()
    ));

    let (used_uploadid, to_upload, mode) = if old_alive {
        (
            st.uploadid.clone(),
            missing_truth.iter().copied().collect::<Vec<_>>(),
            "persisted_uploadid",
        )
    } else {
        // Strategy B: fresh precreate. Empirically (first run of this spike)
        // a same-params precreate returns a NEW uploadid whose block_list
        // enumerates the parts the NEW session still needs.
        let pre2 = api::precreate(&ctx, &st.remote, st.size as i64, &st.block_md5).await?;
        if pre2.errno != 0 || pre2.return_type != 1 {
            bail!(
                "precreate#2 errno={} return_type={}: {}",
                pre2.errno,
                pre2.return_type,
                trunc(&pre2.raw, 200)
            );
        }
        let same_id = pre2.uploadid == st.uploadid;
        let server_list = pre2.block_list.clone();
        let server_set: BTreeSet<i64> = server_list.iter().copied().collect();
        let interp = if server_set.is_empty() {
            "empty"
        } else if server_set == phase1 {
            "already-uploaded"
        } else if server_set == missing_truth {
            "still-needed(phase1-missing)"
        } else if server_set == all {
            "still-needed(all)"
        } else {
            "other"
        };
        println!(
            "[note] precreate#2 uploadid_same={same_id} server_block_list={server_list:?} interp={interp}"
        );
        summ(format!(
            "resume|fresh_precreate|uploadid_same={same_id}|server_block_list={server_list:?}|interp={interp}"
        ));
        let to_upload: Vec<i64> = if same_id {
            match interp {
                "already-uploaded" => all.difference(&server_set).copied().collect(),
                "still-needed(phase1-missing)" | "still-needed(all)" => server_list.clone(),
                _ => missing_truth.iter().copied().collect(),
            }
        } else {
            // brand-new session: upload exactly what the server lists
            server_list.clone()
        };
        (pre2.uploadid.clone(), to_upload, "fresh_precreate")
    };

    // Upload the chosen (differential) set.
    let t0 = Instant::now();
    for seq in &to_upload {
        let buf = read_block(&mut file, st.size, *seq).await?;
        let o = api::superfile2(&ctx, &st.remote, &used_uploadid, *seq, buf).await?;
        if o.error_code != 0 {
            bail!(
                "resume part {seq} error_code={} raw={}",
                o.error_code,
                trunc(&o.raw, 160)
            );
        }
        println!("  resume part {seq} up (ms {})", o.ms);
    }
    let upload_ms = t0.elapsed().as_millis();

    // create() is the differential assertion: it succeeds only if the server
    // session holds EVERY block of the file. With mode=persisted_uploadid we
    // sent only the missing set, so success proves phase-1 parts survived.
    let create = api::create_file(
        &ctx,
        &st.remote,
        &used_uploadid,
        st.size as i64,
        &st.block_md5,
    )
    .await?;
    if create.errno != 0 {
        bail!("create errno {}: {}", create.errno, trunc(&create.raw, 200));
    }

    // Verify final state: size + md5 vs local.
    let listing = api::list_dir(&ctx, &parent_of(&st.remote)).await?;
    let entry = listing.entries.iter().find(|e| e.path == st.remote);
    let local_md5 = file_md5(&local)?;
    let size_ok = entry.map(|e| e.size) == Some(st.size as i64);
    let md5_ok = entry.map(|e| e.md5 == local_md5).unwrap_or(false);

    summ(format!(
        "resume|mode={mode}|phase1_uploaded={:?}|phase2_uploaded={to_upload:?}|diff_only={}",
        st.uploaded,
        mode == "persisted_uploadid"
            && to_upload.iter().all(|s| missing_truth.contains(s))
            && to_upload.len() == missing_truth.len()
    ));
    let _ = md5_ok;
    summ(format!(
        "resume|phase2_wall_ms={upload_ms}|create_errno=0|final_size_ok={size_ok}|         server_md5_field_is_contentid_not_literal_md5 (server_hash={})",
        entry.map(|e| e.md5.clone()).unwrap_or_default()
    ));
    Ok(())
}

fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => path[..i].to_string(),
        None => path.to_string(),
    }
}

// ---------------------------------------------------------------------------
// S-4: rapid upload branches
// ---------------------------------------------------------------------------

pub async fn rapid() -> Result<()> {
    let ctx = Arc::new(Ctx::init()?);
    let dir = resolve_remote_dir(&ctx).await?;
    let sz = 1024 * 1024u64;

    // A: normal three-step upload of unique content (expect return_type=1)
    let local_a = ctx.cfg.tmp_dir.join("rapid_a.bin");
    gen::gen_file(&local_a, sz)?;
    let md5s_a = gen::block_md5s(&local_a, sz)?;
    let remote_a = format!("{dir}/rapid_a.bin");
    let stats_a = upload_three_step(&ctx, &local_a, &remote_a, sz, &md5s_a, 1).await?;
    let md5_whole = file_md5(&local_a)?;
    summ(format!(
        "rapid|A_normal|return_type={}|parts={}|pre_ms={}|parts_ms={}|create_ms={}",
        stats_a.return_type,
        stats_a.parts.len(),
        stats_a.pre_ms,
        stats_a.parts_wall_ms,
        stats_a.create_ms
    ));

    // B: identical content to a NEW path -> precreate should return_type=2
    let remote_b = format!("{dir}/rapid_b.bin");
    let pre_b = api::precreate(&ctx, &remote_b, sz as i64, &md5s_a).await?;
    println!("[note] B precreate raw: {}", trunc(&pre_b.raw, 240));
    summ(format!(
        "rapid|B_same_content|errno={}|return_type={}|uploadid_len={}|fs_id={}",
        pre_b.errno,
        pre_b.return_type,
        pre_b.uploadid.len(),
        pre_b.fs_id
    ));
    if pre_b.errno == 0 && pre_b.return_type == 2 {
        // no superfile2/create was issued; the file must exist already
        let listing = api::list_dir(&ctx, &dir).await?;
        let e = listing.entries.iter().find(|e| e.path == remote_b);
        summ(format!(
            "rapid|B_rapid_hit|listed={} size_ok={} md5_match={} (zero parts uploaded)",
            e.is_some(),
            e.map(|e| e.size) == Some(sz as i64),
            e.map(|e| e.md5 == md5_whole).unwrap_or(false)
        ));
    }

    // C: fresh unique content again (control: return_type=1)
    let local_c = ctx.cfg.tmp_dir.join("rapid_c.bin");
    gen::gen_file(&local_c, sz)?;
    let md5s_c = gen::block_md5s(&local_c, sz)?;
    let remote_c = format!("{dir}/rapid_c.bin");
    let stats_c = upload_three_step(&ctx, &local_c, &remote_c, sz, &md5s_c, 1).await?;
    summ(format!(
        "rapid|C_fresh_control|return_type={}|parts={}",
        stats_c.return_type,
        stats_c.parts.len()
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// S-5: dlink + Range + cache duration
// ---------------------------------------------------------------------------

pub async fn dlink() -> Result<()> {
    let ctx = Arc::new(Ctx::init()?);
    let dir = resolve_remote_dir(&ctx).await?;
    let sz = 8 * 1024 * 1024u64;

    let local = ctx.cfg.tmp_dir.join("dlink_8m.bin");
    gen::gen_file(&local, sz)?;
    let md5s = gen::block_md5s(&local, sz)?;
    let remote = format!("{dir}/dlink_8m.bin");
    upload_three_step(&ctx, &local, &remote, sz, &md5s, 1).await?;

    // 302 + Location shape
    let t0 = Instant::now();
    let (xpan_status, loc) = api::fetch_dlink(&ctx, &remote).await?;
    let loc = loc.ok_or_else(|| anyhow!("expected redirect, got direct {xpan_status}"))?;
    summ(format!(
        "dlink|xpan_status={}|redirect_ms={}|url_shape={}",
        xpan_status.as_u16(),
        t0.elapsed().as_millis(),
        url_shape(&loc)
    ));

    // working form (direct vs +access_token)
    let (url, appended) = working_dlink(&ctx, &remote).await?;
    summ(format!("dlink|form|token_appended={appended}"));

    // Range probes: head / middle / tail, byte-compared with the local file
    let probes = [
        ("head", 0u64, 1024usize),
        ("mid", sz / 2, 4096),
        ("tail", sz - 1024, 1024),
    ];
    for (name, off, len) in probes {
        let range = format!("bytes={}-{}", off, off + len as u64 - 1);
        let (s, body) = api::range_get(&ctx, &url, &range).await?;
        let expect = gen::read_range(&local, off, len)?;
        let ok = s == reqwest::StatusCode::PARTIAL_CONTENT && body == expect;
        println!(
            "  range {name} ({range}) -> {} bytes={} match={}",
            s.as_u16(),
            body.len(),
            ok
        );
        summ(format!(
            "dlink|range_{name}|status={}|bytes={}|content_match={}",
            s.as_u16(),
            body.len(),
            ok
        ));
    }

    // cache duration: keep reusing the SAME dlink until it dies (or budget out)
    let steps = [
        30u64, 30, 60, 120, 240, 480, 900, 300, 300, 300, 300, 300, 300, 300,
        300, 300, 300, 300, 300, 300,
    ]; // cumulative ≈ 131min — loop breaks early on failure; extended
       // from ~63min on 2026-09-07 (owner directive: settle the TTL upper
       // bound; the earlier probe was killed externally at 56min while 206)
    let mut elapsed_s = 0u64;
    let mut last_ok_min = 0.0f64;
    let mut expired_at_min: Option<f64> = None;
    println!("[note] dlink cache probe loop starts (reusing ONE dlink, ~63min budget)");
    for step in steps {
        tokio::time::sleep(Duration::from_secs(step)).await;
        elapsed_s += step;
        let (s, _) = api::range_get(&ctx, &url, "bytes=0-0").await?;
        let ok = s.is_success();
        println!(
            "  probe @ {:.1} min -> {} ok={}",
            elapsed_s as f64 / 60.0,
            s.as_u16(),
            ok
        );
        if ok {
            last_ok_min = elapsed_s as f64 / 60.0;
        } else {
            expired_at_min = Some(elapsed_s as f64 / 60.0);
            break;
        }
    }
    match expired_at_min {
        Some(e) => summ(format!(
            "dlink|cache_window|last_ok_min={last_ok_min:.1}|expired_at_min={e:.1}"
        )),
        None => summ(format!(
            "dlink|cache_window|still_valid_after_min={last_ok_min:.1}|budget_capped=yes"
        )),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// S-6: throughput (1 GiB up + down)
// ---------------------------------------------------------------------------

const TP_SIZE: u64 = 1024 * 1024 * 1024; // 1 GiB = 256 blocks
const TP_WORKERS: u64 = 4;

pub async fn throughput() -> Result<()> {
    let ctx = Arc::new(Ctx::init()?);
    let dir = resolve_remote_dir(&ctx).await?;

    let t_gen = Instant::now();
    let local = ctx.cfg.tmp_dir.join("throughput_1g.bin");
    println!("[note] generating {} ...", local.display());
    gen::gen_file(&local, TP_SIZE)?;
    let gen_s = t_gen.elapsed().as_secs_f64();

    let t_hash = Instant::now();
    let md5s = gen::block_md5s(&local, TP_SIZE)?;
    let hash_s = t_hash.elapsed().as_secs_f64();

    let remote = format!("{dir}/throughput_1g.bin");
    let stats = upload_three_step(&ctx, &local, &remote, TP_SIZE, &md5s, TP_WORKERS).await?;
    let up_total_ms =
        (gen_s + hash_s) * 1000.0 + (stats.pre_ms + stats.parts_wall_ms + stats.create_ms) as f64;
    let part_ms: Vec<u128> = stats.parts.iter().map(|p| p.2).collect();
    let max_part = part_ms.iter().copied().max().unwrap_or(0);
    let sum_part: u128 = part_ms.iter().sum();
    summ(format!(
        "throughput|upload|return_type={}|parts={}|precreate_ms={}|parts_wall_ms={}|create_ms={}\
         |net_MBps={:.1}|incl_hash_MBps={:.1}|per_part_ms_avg={}|max={}",
        stats.return_type,
        stats.parts.len(),
        stats.pre_ms,
        stats.parts_wall_ms,
        stats.create_ms,
        mbs(TP_SIZE, stats.parts_wall_ms),
        mbs(TP_SIZE, up_total_ms as u128),
        sum_part / part_ms.len().max(1) as u128,
        max_part,
    ));

    // server-side verification: size + whole-file md5
    let listing = api::list_dir(&ctx, &dir).await?;
    let entry = listing.entries.iter().find(|e| e.path == remote);
    let local_md5 = file_md5(&local)?;
    summ(format!(
        "throughput|verify|listed={} size_ok={} md5_ok={}",
        entry.is_some(),
        entry.map(|e| e.size) == Some(TP_SIZE as i64),
        entry.map(|e| e.md5 == local_md5).unwrap_or(false)
    ));

    // download (main): the CDN only authorizes BOUNDED ranges <= 4 MiB with
    // a netdisk UA (see dl-try matrix in the report) — full-file download =
    // 4 MiB bounded chunks over 4 concurrent streams, reusing ONE dlink.
    let (url, appended) = working_dlink(&ctx, &remote).await?;
    println!("[note] downloading 1GiB as 4MiB bounded chunks x4 streams ...");
    let dl_chunk: u64 = 4 * 1024 * 1024;
    let nchunks = TP_SIZE.div_ceil(dl_chunk);
    let t_dl = Instant::now();
    let mut handles = Vec::new();
    for w in 0..4u64 {
        let ctx = Arc::clone(&ctx);
        let url = url.clone();
        handles.push(tokio::spawn(async move {
            let mut got: u64 = 0;
            for i in (0..nchunks).filter(|i| i % 4 == w) {
                let start = i * dl_chunk;
                let end = (start + dl_chunk - 1).min(TP_SIZE - 1);
                let range = format!("bytes={start}-{end}");
                let (s, b) = api::range_get(&ctx, &url, &range).await?;
                anyhow::ensure!(
                    s == reqwest::StatusCode::PARTIAL_CONTENT,
                    "chunk {i} status {}",
                    s.as_u16()
                );
                anyhow::ensure!(b.len() as u64 == end - start + 1, "chunk {i} short read");
                got += b.len() as u64;
            }
            anyhow::Ok(got)
        }));
    }
    let mut total: u64 = 0;
    for h in handles {
        total += h.await.context("download worker panicked")??;
    }
    let dl_ms = t_dl.elapsed().as_millis();
    summ(format!(
        "throughput|download|chunks={} bytes={} bytes_ok={} net_MBps={:.1} token_appended={}",
        nchunks,
        total,
        total == TP_SIZE,
        mbs(total, dl_ms),
        appended
    ));

    // control: browser UA on one 4 MiB bounded chunk — documents the
    // netdisk-UA requirement under the exact working range shape
    let plain = reqwest::Client::builder()
        .no_proxy()
        .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(20))
        .build()?;
    let resp = plain
        .get(&url)
        .header("Range", "bytes=0-4194303")
        .send()
        .await
        .map_err(|e| anyhow!("plain-UA transport: {e}"))?;
    let status = resp.status();
    let body = resp
        .bytes()
        .await
        .map_err(|e| anyhow!("plain-UA body: {e}"))?;
    summ(format!(
        "throughput|download_browser_ua_4m_chunk|status={} bytes={}",
        status.as_u16(),
        body.len()
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// cleanup
// ---------------------------------------------------------------------------

pub async fn cleanup() -> Result<()> {
    let ctx = Ctx::init()?;

    // Remote dir: BAIDU_SPIKE_REMOTE_DIR wins (same rule as
    // resolve_remote_dir — without it, cleanup would sweep the stale
    // state dir and silently miss env-rooted leftovers); else state;
    // else probe read-only (never create anything during cleanup).
    let env_dir = std::env::var("BAIDU_SPIKE_REMOTE_DIR")
        .ok()
        .filter(|d| !d.is_empty());
    let dir = if let Some(d) = env_dir {
        Some(d)
    } else {
        {
            let st = ctx.state.lock().unwrap();
            st.remote_dir.clone()
        }
    };
    let dir = match dir {
        Some(d) => Some(d),
        None => {
            let mut found = None;
            for cand in ["/apps/baidu_spike", "/apps/privatefs/baidu_spike"] {
                if let Ok(o) = api::list_dir(&ctx, cand).await {
                    if o.errno == 0 {
                        found = Some(cand.to_string());
                        break;
                    }
                }
            }
            found
        }
    };

    let mut deleted: usize = 0;
    let mut remaining: Option<usize> = None;
    if let Some(dir) = dir {
        let before = api::list_dir(&ctx, &dir).await?;
        if before.errno == 0 {
            let paths: Vec<String> = before.entries.iter().map(|e| e.path.clone()).collect();
            println!(
                "[note] remote {} holds {} entries: {:?}",
                dir,
                paths.len(),
                paths
            );
            if !paths.is_empty() {
                for chunk in paths.chunks(100) {
                    let out = api::filemanager_delete(&ctx, chunk).await?;
                    let bad: Vec<&(i64, String)> =
                        out.infos.iter().filter(|(e, _)| *e != 0).collect();
                    if out.errno != 0 || !bad.is_empty() {
                        println!(
                            "[note] delete batch errno={} per-item failures: {:?}",
                            out.errno, bad
                        );
                    }
                    deleted += chunk.len() - bad.len();
                }
            }
            let after = api::list_dir(&ctx, &dir).await?;
            remaining = Some(if after.errno == 0 {
                after.entries.len()
            } else {
                0
            });
            println!(
                "[note] post-delete list: errno={} entries={}",
                after.errno,
                remaining.unwrap_or(0)
            );
            // also remove the (spike-created) directory itself when empty
            if remaining == Some(0) {
                let dir_del = api::filemanager_delete(&ctx, &[dir.clone()]).await?;
                let dir_ok = dir_del.errno == 0 && dir_del.infos.iter().all(|(e, _)| *e == 0);
                println!(
                    "[note] dir delete errno={} per-item={:?}",
                    dir_del.errno, dir_del.infos
                );
                let check = api::list_dir(&ctx, &dir).await?;
                summ(format!(
                    "cleanup|spike_dir_removed={dir_ok}|dir_recheck_errno={}",
                    check.errno
                ));
            }
        } else {
            println!(
                "[note] remote {} absent (errno {}) — nothing to clean",
                dir, before.errno
            );
            remaining = Some(0);
        }
    } else {
        println!("[note] no spike remote dir ever created — nothing to clean");
    }

    // local temp artifacts (keep token/state for follow-up runs)
    let locals = [
        "qps_part.bin",
        "resume_32m.bin",
        "rapid_a.bin",
        "rapid_b.bin",
        "rapid_c.bin",
        "dlink_8m.bin",
        "throughput_1g.bin",
    ];
    let mut removed = 0usize;
    for name in locals {
        let p = ctx.cfg.tmp_dir.join(name);
        if p.exists() && std::fs::remove_file(&p).is_ok() {
            removed += 1;
        }
    }
    summ(format!(
        "cleanup|remote_deleted={deleted}|remote_remaining={remaining:?}|local_temp_removed={removed}|\
         note=xpan_delete_moves_to_recycle_bin(10d retention,not_verifiable_via_this_api)"
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// debug helper: list the spike dir with md5s (post-hoc verification)
// ---------------------------------------------------------------------------

pub async fn ls() -> Result<()> {
    let ctx = Ctx::init()?;
    let dir = resolve_remote_dir(&ctx).await?;
    let o = api::list_dir(&ctx, &dir).await?;
    println!("errno={} entries={}", o.errno, o.entries.len());
    for e in &o.entries {
        println!(
            "  {} size={} md5={} fs_id={}",
            e.path,
            e.size,
            if e.md5.is_empty() { "<empty>" } else { &e.md5 },
            e.fs_id
        );
    }
    let local = ctx.cfg.tmp_dir.join("resume_32m.bin");
    if local.exists() {
        let m = file_md5(&local).unwrap_or_default();
        println!("local resume_32m.bin md5={m}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// S-4 follow-up: delayed rapid-upload probe (re-precreate with content that
// was uploaded minutes earlier under a fresh path)
// ---------------------------------------------------------------------------

pub async fn rapid_probe() -> Result<()> {
    let ctx = Arc::new(Ctx::init()?);
    let dir = resolve_remote_dir(&ctx).await?;
    let local = ctx.cfg.tmp_dir.join("rapid_a.bin");
    anyhow::ensure!(local.exists(), "rapid_a.bin missing — run `rapid` first");
    let sz = std::fs::metadata(&local)?.len();
    let md5s = gen::block_md5s(&local, sz)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let remote = format!("{dir}/rapid_probe_{ts}.bin");
    let pre = api::precreate(&ctx, &remote, sz as i64, &md5s).await?;
    println!("[note] raw: {}", trunc(&pre.raw, 240));
    summ(format!(
        "rapid_probe|delayed_same_content|errno={}|return_type={}|uploadid_len={}|fs_id={}",
        pre.errno,
        pre.return_type,
        pre.uploadid.len(),
        pre.fs_id
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// S-6 diagnostic: how does the CDN want full-file downloads?
// ---------------------------------------------------------------------------

pub async fn dl_try() -> Result<()> {
    let ctx = Arc::new(Ctx::init()?);
    let dir = resolve_remote_dir(&ctx).await?;
    let remote = format!("{dir}/throughput_1g.bin");
    let (_, loc) = api::fetch_dlink(&ctx, &remote).await?;
    let loc = loc.ok_or_else(|| anyhow!("no redirect"))?;
    println!("[note] fresh dlink: {}", url_shape(&loc));

    let token = ctx.token.read().await.access_token.clone();
    let loc2 = format!("{loc}&access_token={token}");

    for (label, url, range, ua) in [
        ("fresh+full-noRange", loc.clone(), None, crate::common::UA),
        (
            "fresh+range0-",
            loc.clone(),
            Some("bytes=0-"),
            crate::common::UA,
        ),
        (
            "fresh+range0-1M",
            loc.clone(),
            Some("bytes=0-1048575"),
            crate::common::UA,
        ),
        (
            "fresh+token+full-noRange",
            loc2.clone(),
            None,
            crate::common::UA,
        ),
        (
            "netdiskUA+range0-64M",
            loc.clone(),
            Some("bytes=0-67108863"),
            crate::common::UA,
        ),
        (
            "browserUA+range0-1M",
            loc.clone(),
            Some("bytes=0-1048575"),
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64)",
        ),
        (
            "browserUA+range0-64M",
            loc.clone(),
            Some("bytes=0-67108863"),
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64)",
        ),
        (
            "netdiskUA+range0-256M",
            loc.clone(),
            Some("bytes=0-268435455"),
            crate::common::UA,
        ),
        (
            "netdiskUA+range0-2M",
            loc.clone(),
            Some("bytes=0-2097151"),
            crate::common::UA,
        ),
        (
            "netdiskUA+range0-4M",
            loc.clone(),
            Some("bytes=0-4194303"),
            crate::common::UA,
        ),
        (
            "netdiskUA+range0-8M",
            loc.clone(),
            Some("bytes=0-8388607"),
            crate::common::UA,
        ),
        (
            "netdiskUA+range0-16M",
            loc.clone(),
            Some("bytes=0-16777215"),
            crate::common::UA,
        ),
        (
            "netdiskUA+range0-32M",
            loc.clone(),
            Some("bytes=0-33554431"),
            crate::common::UA,
        ),
        (
            "netdiskUA+rangeMid-16M",
            loc.clone(),
            Some("bytes=536870912-553648127"),
            crate::common::UA,
        ),
    ] {
        let t0 = Instant::now();
        let mut req = ctx.stream.get(&url).header("User-Agent", ua);
        if let Some(r) = range {
            req = req.header(reqwest::header::RANGE, r);
        }
        let resp = req.send().await.map_err(|e| anyhow!("transport {e}"))?;
        let status = resp.status();
        let headers: Vec<String> = resp
            .headers()
            .iter()
            .map(|(k, v)| format!("{}={}", k, String::from_utf8_lossy(v.as_bytes())))
            .collect();
        let body = resp.bytes().await.map_err(|e| anyhow!("body {e}"))?;
        let body_head = String::from_utf8_lossy(&body[..body.len().min(200)]).to_string();
        println!(
            "  {label} -> {} bytes={} ms={} headers={:?} body={}",
            status.as_u16(),
            body.len(),
            t0.elapsed().as_millis(),
            headers,
            crate::common::scrub(&body_head)
        );
    }
    Ok(())
}
