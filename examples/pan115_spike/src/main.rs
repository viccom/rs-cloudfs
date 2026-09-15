//! pan115-spike — Phase 5 (pan115 driver) 115-0 route spike: device-code
//! PKCE auth leg against the 115 open platform. Endpoint shapes below are
//! live-probed facts (2026-09-16), not guesses; style follows
//! examples/baidu_spike (anyhow + small modules + masked output).
//!
//! ```text
//! pan115-spike auth-qr                        # device code + state file + QR PNG
//! pan115-spike auth-poll [--max-wait-secs N]  # long-poll scan status -> tokens
//! pan115-spike probe-user                     # user/info with cached token
//! pan115-spike probe-refresh                  # ONE refreshToken + verify
//! ```
//!
//! Env: PAN115_SPIKE_TEST_DIR (default E:\GitHub\rs-CyDrive\test — gitignored,
//! outside the repo), PAN115_CLIENT_ID (default 100197303 — non-secret
//! OpenList-hosted app identity). Tokens never appear in code, comments or
//! error text; every printed token is masked first-6/last-4.
//!
//! Exit codes (auth-poll): 0 = tokens obtained, 3 = QR expired/cancelled,
//! 4 = max-wait reached with no scan, 1 = other error, 2 = usage.

mod auth;
mod state;

use std::time::Duration;

use anyhow::{Context as _, Result};
use state::{mask, AuthState, Paths, Tokens};

const DEFAULT_CLIENT_ID: &str = "100197303";

fn usage() -> ! {
    eprintln!(
        "usage: pan115-spike <auth-qr | auth-poll [--max-wait-secs N] | probe-user | probe-refresh>"
    );
    std::process::exit(2);
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn client_id() -> String {
    std::env::var("PAN115_CLIENT_ID").unwrap_or_else(|_| DEFAULT_CLIENT_ID.to_string())
}

fn user_name(info: &serde_json::Value) -> String {
    info.get("user_name")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(cmd) = args.get(1).map(String::as_str) else {
        usage()
    };

    // `auth-poll [--max-wait-secs N]` — hand-rolled, no clap (baidu_spike style).
    let mut max_wait_secs: u64 = 480;
    if cmd == "auth-poll" {
        let mut i = 2;
        while i < args.len() {
            if args[i] == "--max-wait-secs" {
                i += 1;
                max_wait_secs = args
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage());
            } else {
                usage();
            }
            i += 1;
        }
    } else if args.len() > 2 {
        usage();
    }

    let result = match cmd {
        "auth-qr" => cmd_auth_qr().await,
        "auth-poll" => cmd_auth_poll(max_wait_secs).await,
        "probe-user" => cmd_probe_user().await,
        "probe-refresh" => cmd_probe_refresh().await,
        _ => usage(),
    };
    let code = match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("pan115-spike: {cmd} FAILED: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

/// auth-qr: new verifier -> authDeviceCode -> state file + QR PNG.
async fn cmd_auth_qr() -> Result<i32> {
    let paths = Paths::from_env();
    let client = auth::http_client()?;
    let client_id = client_id();
    let verifier = auth::gen_code_verifier();

    println!("[1/3] POST authDeviceCode (client_id={client_id})...");
    let dc = auth::auth_device_code(&client, &client_id, &verifier).await?;
    println!(
        "[2/3] device code issued: uid={} qrcode={}",
        dc.uid, dc.qrcode
    );

    let st = AuthState {
        client_id,
        code_verifier: verifier,
        uid: dc.uid.clone(),
        time: dc.time,
        sign: dc.sign.clone(),
        qrcode_url: dc.qrcode.clone(),
        created_unix: now_unix(),
    };
    state::write_json_atomic(&paths.auth_state(), &st)?;
    // 1024px floor: comfortably above the >=512px contract, scannable from
    // screen, and keeps the PNG well past the 5KB sanity size.
    let px = state::render_qr_png(&dc.qrcode, 1024, &paths.qr_png())?;
    println!(
        "[3/3] state + QR PNG ({px}x{px}) written under {}",
        paths.dir.display()
    );

    println!(
        "[SUMMARY] auth-qr|ok=1|uid={}|expires_hint=state-file",
        dc.uid
    );
    // Contract: stdout carries the absolute PNG path and the QR URL.
    let png_abs = paths.qr_png().canonicalize()?;
    let png_abs = png_abs.display().to_string();
    println!("{}", png_abs.trim_start_matches(r"\\?\"));
    println!("{}", dc.qrcode);
    Ok(0)
}

/// auth-poll: long-poll the QR status until confirmed/expired/max-wait.
async fn cmd_auth_poll(max_wait_secs: u64) -> Result<i32> {
    let paths = Paths::from_env();
    let client = auth::http_client()?;
    let st: AuthState = state::read_json(&paths.auth_state())
        .context("auth-poll needs the state file from a prior `auth-qr` run")?;
    println!(
        "polling QR uid={} (max-wait {}s; server holds each poll ~30s)",
        st.uid, max_wait_secs
    );

    let start = std::time::Instant::now();
    let mut consecutive_errors = 0u32;
    loop {
        if start.elapsed().as_secs() >= max_wait_secs {
            println!(
                "[SUMMARY] auth-poll|result=max-wait|waited_s={}|uid={}",
                start.elapsed().as_secs(),
                st.uid
            );
            return Ok(4);
        }
        match auth::poll_status(&client, &st.uid, st.time, &st.sign).await {
            Ok(auth::PollStatus::Waiting) => {
                consecutive_errors = 0;
                println!(
                    "[poll] waiting (empty data) at +{}s",
                    start.elapsed().as_secs()
                );
            }
            Ok(auth::PollStatus::Scanned) => {
                consecutive_errors = 0;
                println!(
                    "[poll] status=1 scanned, waiting for confirmation at +{}s",
                    start.elapsed().as_secs()
                );
            }
            Ok(auth::PollStatus::Confirmed) => {
                println!(
                    "[poll] status=2 confirmed at +{}s, exchanging code for tokens",
                    start.elapsed().as_secs()
                );
                return finish_auth(&client, &paths, &st).await;
            }
            Ok(auth::PollStatus::Expired) => {
                println!("[SUMMARY] auth-poll|result=expired|uid={}", st.uid);
                return Ok(3);
            }
            Ok(auth::PollStatus::Cancelled) => {
                println!("[SUMMARY] auth-poll|result=cancelled|uid={}", st.uid);
                return Ok(3);
            }
            Err(e) => {
                consecutive_errors += 1;
                eprintln!("[poll] transport error #{consecutive_errors}: {e:#}");
                if consecutive_errors >= 10 {
                    anyhow::bail!("10 consecutive poll failures, aborting");
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

/// Confirmed: exchange (retried) -> persist IMMEDIATELY (the code_verifier is
/// single-use; losing the pair means the user must rescan) -> user/info as a
/// non-fatal bonus -> summary.
async fn finish_auth(client: &reqwest::Client, paths: &Paths, st: &AuthState) -> Result<i32> {
    let mut pair = None;
    for attempt in 1..=3u32 {
        match auth::device_code_to_token(client, &st.uid, &st.code_verifier).await {
            Ok(p) => {
                pair = Some(p);
                break;
            }
            Err(e) if attempt < 3 => {
                eprintln!("[exchange] attempt {attempt} failed: {e:#}; retrying in 2s");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => return Err(e),
        }
    }
    let pair = pair.expect("loop returns Some or bails");

    let mut tokens = Tokens {
        client_id: st.client_id.clone(),
        access_token: pair.access_token.clone(),
        refresh_token: pair.refresh_token.clone(),
        expires_in: pair.expires_in,
        obtained_unix: now_unix(),
        user_info: serde_json::Value::Null,
    };
    state::write_json_atomic(&paths.tokens(), &tokens)?;
    println!("[capture] token pair persisted (user/info pending)");

    match auth::user_info(client, &tokens.access_token).await {
        Ok(info) => {
            tokens.user_info = info;
            state::write_json_atomic(&paths.tokens(), &tokens)?;
        }
        Err(e) => {
            eprintln!("[warn] user/info failed after capture (tokens are safe on disk): {e:#}")
        }
    }
    println!(
        "[SUMMARY] auth-poll|result=ok|access_token={}|refresh_token={}|expires_in={}s",
        mask(&tokens.access_token),
        mask(&tokens.refresh_token),
        tokens.expires_in
    );
    Ok(0)
}

/// probe-user: cached token -> user/info -> full pretty JSON (no credentials
/// inside user info; the token itself is never printed).
async fn cmd_probe_user() -> Result<i32> {
    let paths = Paths::from_env();
    let tokens: Tokens = state::read_json(&paths.tokens())
        .context("probe-user needs pan115-tokens.json (run auth-poll first)")?;
    let client = auth::http_client()?;
    let info = auth::user_info(&client, &tokens.access_token).await?;
    println!("{}", serde_json::to_string_pretty(&info)?);
    Ok(0)
}

/// probe-refresh: ONE refreshToken call -> persist the rotated pair FIRST
/// (old refresh_token is dead) -> verify with user/info -> masked summary.
async fn cmd_probe_refresh() -> Result<i32> {
    let paths = Paths::from_env();
    let tokens: Tokens = state::read_json(&paths.tokens())
        .context("probe-refresh needs pan115-tokens.json (run auth-poll first)")?;
    let client = auth::http_client()?;
    let old_expires_in = tokens.expires_in;

    println!("[1/3] POST refreshToken (once — 115 rate-limits this endpoint)...");
    let pair = auth::refresh_token_pair(&client, &tokens.refresh_token).await?;

    // Rotation semantics: new pair hits disk before any other work.
    let mut rotated = Tokens {
        client_id: tokens.client_id.clone(),
        access_token: pair.access_token.clone(),
        refresh_token: pair.refresh_token.clone(),
        expires_in: pair.expires_in,
        obtained_unix: now_unix(),
        user_info: serde_json::Value::Null,
    };
    state::write_json_atomic(&paths.tokens(), &rotated)?;
    println!(
        "[2/3] rotated token pair persisted to {}",
        paths.tokens().display()
    );

    let info = auth::user_info(&client, &pair.access_token)
        .await
        .context("user/info with the refreshed access token")?;
    rotated.user_info = info.clone();
    state::write_json_atomic(&paths.tokens(), &rotated)?;
    println!("[3/3] user/info OK with refreshed token");
    println!(
        "[SUMMARY] probe-refresh|ok=1|access_token={}|refresh_token={}|expires_in_old={}s|expires_in_new={}s|user={}",
        mask(&rotated.access_token),
        mask(&rotated.refresh_token),
        old_expires_in,
        rotated.expires_in,
        user_name(&info)
    );
    Ok(0)
}
