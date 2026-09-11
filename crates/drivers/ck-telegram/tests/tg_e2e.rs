//! Real-network E2E tests for the [`GrammersTransport`] (telegram E2E batch
//! TG0–TG2). Everything here is `#[ignore]`d: these tests talk to the live
//! Telegram MTProto network through the dedicated test bot and must be run
//! explicitly, serially, with generous timeouts:
//!
//! ```text
//! CARGO_TARGET_DIR=<shared target> cargo test -p ck-telegram --test tg_e2e \
//!     -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Harness contract (TG0):
//! - Credentials come from the gitignored test config
//!   (`E:\GitHub\rs-CyDrive\test\config.toml`, keys `bot_token` / `chat_id`),
//!   overridable via `CYDRIVE_TG_BOT_TOKEN` / `CYDRIVE_TG_CHAT_ID`; the file
//!   location itself via `CYDRIVE_TG_CRED_FILE`. Missing credentials SKIP
//!   the test (declared loudly) instead of failing — a credential-less
//!   environment (CI) stays green.
//! - The proxy defaults to `socks5://127.0.0.1:7897` (the only way this
//!   machine reaches Telegram) and is overridable via `CYDRIVE_TG_PROXY_URL`.
//!   It is forwarded verbatim through `TransportConfig::proxy_url` into
//!   grammers' `ConnectionParams::proxy_url` — the same path production
//!   takes (`cloudkit_cli::transport_config_from`).
//! - Every run writes under a unique `/_e2e/tg-e2e-<unix-ts>-<tag>/` remote
//!   prefix (telegram has no directories; the prefix lives in the caption
//!   contract paths only) and DELETES everything it uploaded in a cleanup
//!   phase that runs before any assertion panic — success or failure both
//!   clean, and cleanup completeness is verified by re-fetching every
//!   deleted message through an independent checker connection.
//! - MTProto session files live under a STABLE directory in the OS temp
//!   area (`<temp>/tg-e2e-sessions/`, `*.session` is globally gitignored)
//!   and are REUSED across runs. Rationale (real-network lesson, first
//!   review run): signing in a fresh session per test re-imports the bot
//!   authorization every time, and Telegram answers rapid re-auth with
//!   hour-scale `FLOOD_WAIT`s. With a persisted session the authorization
//!   happens once per machine and every later connect skips the import.
//!   The trade-off is that the checker's bot-identity assertion
//!   (`is_bot`) only executes on the fresh sign-in path; on the cached
//!   path the login evidence is the successful RPC round-trip itself.
//! - The upload size is deliberately multi-part (chunk 1 MiB) so one E2E
//!   run exercises several MTProto messages end to end.
//!
//! Scenario notes (behavior recorded from this real run, not assumed):
//! - TG2 overwrite: the transport is id-keyed and Telegram-native sends are
//!   append-only — re-uploading the same rel_path produces a NEW message
//!   set (new ids) while the old messages remain visible to raw fetches.
//!   Overwrite semantics live at the VFS layer (new row + the old row's
//!   remote objects kept per the K4/B3b `remote_delete = false` decision).
//!   The test pins exactly that observed behavior.
//! - TG2 range: the driver declares `range_read: true`; mid-file windows
//!   spanning part boundaries are compared byte-for-byte against the
//!   reference buffer.

use std::path::PathBuf;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ck_telegram::config::{TransportConfig, DEFAULT_API_HASH, DEFAULT_API_ID};
use ck_telegram::transport::GrammersTransport;
use cloudkit_storage::transport::{
    ByteStream, CloudTransport, RemoteHandle, StorageError, UploadJob, UploadReceipt,
};
use cloudkit_storage::vpath::RelPath;
use grammers_client::media::Media;
use grammers_client::sender::{ConnectionParams, SenderPool};
use grammers_client::Client;
use grammers_session::storages::SqliteSession;
use grammers_session::types::PeerId;
use grammers_session::Session;

const MIB: u64 = 1024 * 1024;
/// Generous per-phase budget: login + multi-MB upload/download through a
/// SOCKS5 proxy is minutes-scale on a bad day.
const PHASE_TIMEOUT: Duration = Duration::from_secs(600);
const DEFAULT_PROXY_URL: &str = "socks5://127.0.0.1:7897";
const DEFAULT_CRED_FILE: &str = r"E:\GitHub\rs-CyDrive\test\config.toml";
/// Rate-limit (FLOOD_WAIT) retry budget per network operation. grammers
/// 0.10 has no built-in retry policy, so the harness honors the
/// `StorageError::RateLimited` mapping (`retry_after`, capped at 60 s).
const RATE_LIMIT_ATTEMPTS: usize = 4;
const RATE_LIMIT_CAP: Duration = Duration::from_secs(60);

// ------------------------------------------------------------------ TG0 --

/// Test credentials (never logged, never committed — values only flow into
/// the transport config).
struct Credentials {
    bot_token: String,
    chat_id: i64,
}

impl Credentials {
    /// File first (`bot_token = "..."` / `chat_id = <int>`, the two-key
    /// gitignored test config), env overrides on top. `None` = skip.
    fn load() -> Option<Self> {
        let path = PathBuf::from(
            std::env::var("CYDRIVE_TG_CRED_FILE").unwrap_or_else(|_| DEFAULT_CRED_FILE.to_owned()),
        );
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                eprintln!(
                    "SKIP: credential file {} unreadable ({e}) — set CYDRIVE_TG_CRED_FILE or the env overrides",
                    path.display()
                );
                return None;
            }
        };
        let mut bot_token = None;
        let mut chat_id = None;
        for line in text.lines() {
            let (key, value) = match line.split_once('=') {
                Some(pair) => pair,
                None => continue,
            };
            let value = value.trim().trim_matches('"').trim();
            match key.trim() {
                "bot_token" => bot_token = Some(value.to_owned()),
                "chat_id" => chat_id = value.parse().ok(),
                _ => {}
            }
        }
        let creds = Self {
            bot_token: bot_token?,
            chat_id: chat_id?,
        };
        // Env overrides (task contract: CYDRIVE_TG_BOT_TOKEN/CYDRIVE_TG_CHAT_ID).
        let creds = Self {
            bot_token: std::env::var("CYDRIVE_TG_BOT_TOKEN").unwrap_or(creds.bot_token),
            chat_id: std::env::var("CYDRIVE_TG_CHAT_ID")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(creds.chat_id),
        };
        if creds.bot_token.is_empty() || creds.chat_id == 0 {
            eprintln!("SKIP: credentials present but incomplete (empty token or chat_id 0)");
            return None;
        }
        Some(creds)
    }
}

fn proxy_url() -> String {
    std::env::var("CYDRIVE_TG_PROXY_URL").unwrap_or_else(|_| DEFAULT_PROXY_URL.to_owned())
}

fn unix_stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs() as u128
}

/// Stable session directory in the OS temp area — survives across runs so
/// the bot authorization is imported once, not per test/per run (FLOOD_WAIT
/// hygiene; see the module docs). Never inside the repository.
fn stable_session_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tg-e2e-sessions");
    std::fs::create_dir_all(&dir).expect("create stable session dir");
    dir.join(format!("{name}.session"))
}

/// Deterministic pseudo-random filler (xorshift64*): reproducible buffers
/// with no `rand` dependency. Distinct seeds give distinct streams.
fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545F4914F6CDD1D)
    };
    (0..len).map(|_| (next() >> 33) as u8).collect()
}

/// One remote part as seen through the independent checker connection.
struct RemotePart {
    name: String,
    size: u64,
    caption: String,
}

/// An independent MTProto connection over the same bot token (own session
/// file) used to verify what actually landed in the chat — a second
/// witness that does not share state with the transport under test. Its
/// sign-in also yields the bot identity (TG1's "login as me" leg).
struct Checker {
    client: Client,
    chat: grammers_session::types::PeerRef,
}

impl Checker {
    async fn connect(creds: &Credentials, session_name: &str) -> Self {
        let session_path = stable_session_path(session_name);
        let session = Arc::new(
            SqliteSession::open(&session_path)
                .await
                .expect("checker session open"),
        );
        let pool = SenderPool::with_configuration(
            Arc::clone(&session),
            DEFAULT_API_ID,
            ConnectionParams {
                device_model: "cydrive-e2e-checker".to_string(),
                system_version: std::env::consts::OS.to_string(),
                app_version: env!("CARGO_PKG_VERSION").to_string(),
                system_lang_code: "en".to_string(),
                lang_code: "en".to_string(),
                proxy_url: Some(proxy_url()),
                use_ipv6: false,
                __non_exhaustive: (),
            },
        );
        let client = Client::new(pool.handle);
        tokio::spawn(pool.runner.run());
        // Fresh session → sign in and capture the bot identity this token
        // resolves to (is_bot is the assertion). Cached session → the
        // authorization is reused and no identity object is available
        // (grammers 0.10 exposes no `me()`); the login evidence on that
        // path is the successful RPC round-trips below.
        if !client.is_authorized().await.expect("checker auth check") {
            let me = client
                .bot_sign_in(&creds.bot_token, DEFAULT_API_HASH)
                .await
                .expect("checker bot_sign_in (login as the test bot)");
            assert!(
                me.is_bot(),
                "checker identity signed in via bot token must be a bot"
            );
            eprintln!(
                "login: signed in as @{} (full name {:?}, is_bot=true)",
                me.username().unwrap_or("<no-username>"),
                me.full_name()
            );
        } else {
            eprintln!(
                "login: cached session already authorized (auth import skipped, {})",
                session_path.display()
            );
        }
        let peer_id = PeerId::from_bot_api_dialog_id(creds.chat_id).expect("valid dialog id");
        let chat = session
            .peer_ref(peer_id)
            .await
            .expect("peer lookup")
            .unwrap_or_else(|| peer_id.to_ambient_ref());
        Self { client, chat }
    }

    /// Fetches one message's document part by id: `None` = the message is
    /// gone (or carries no downloadable document) — the both-states-are-
    /// "empty" shape the cleanup verification needs.
    async fn part(&self, msg_id: i64) -> Option<RemotePart> {
        let id = i32::try_from(msg_id).ok()?;
        let messages = self
            .client
            .get_messages_by_id(self.chat, &[id])
            .await
            .expect("checker get_messages_by_id");
        let message = messages.into_iter().next().flatten()?;
        match message.media() {
            Some(Media::Document(document)) => Some(RemotePart {
                name: document.name().unwrap_or_default().to_owned(),
                size: document.size().unwrap_or_default() as u64,
                caption: message.text().to_owned(),
            }),
            _ => None,
        }
    }
}

/// Per-test run state: connected transport, independent checker, unique
/// remote prefix, and the set of remote handles to clean up.
struct E2eRun {
    transport: GrammersTransport,
    checker: Checker,
    dir: String,
    handles: Vec<RemoteHandle>,
}

impl E2eRun {
    /// `None` = credentials missing → the caller SKIPs (declared).
    /// Any network failure after credentials exist is a hard error (never
    /// a silent skip). Both connections share the stable session pair
    /// (`tg-e2e-shared` / `tg-e2e-shared-checker`): within one serial run
    /// the first test imports the authorization, later tests reuse it.
    async fn setup(tag: &str) -> Option<Self> {
        let creds = Credentials::load()?;
        let transport = GrammersTransport::connect(TransportConfig {
            bot_token: creds.bot_token.clone(),
            chat_id: creds.chat_id,
            session_path: stable_session_path("tg-e2e-shared"),
            proxy_url: Some(proxy_url()),
            ..TransportConfig::default()
        })
        .await
        .expect("transport connect (MTProto login via proxy)");
        assert!(
            transport.capabilities().range_read,
            "driver must declare range_read (the TG2 range leg tests the declared capability)"
        );
        let checker = Checker::connect(&creds, "tg-e2e-shared-checker").await;
        let dir = format!("/_e2e/tg-e2e-{}-{tag}", unix_stamp());
        eprintln!("run prefix: {dir}");
        Some(Self {
            transport,
            checker,
            dir,
            handles: Vec::new(),
        })
    }

    /// Registers an upload receipt for cleanup and returns the handle.
    fn track(&mut self, receipt: &UploadReceipt) -> RemoteHandle {
        let handle = RemoteHandle {
            first_msg_id: receipt.first_msg_id,
            chunk_msg_ids: receipt.chunk_msg_ids.clone(),
            total_size: receipt.uploaded_bytes,
            path: None,
        };
        self.handles.push(handle.clone());
        handle
    }

    fn rel(&self, name: &str) -> RelPath {
        RelPath::new(&format!("{}/{name}", self.dir)).expect("valid rel path")
    }

    /// Deletes every tracked remote message set and verifies emptiness
    /// through the independent checker. Returns the failure list (empty =
    /// clean); every failure is also printed loudly by the caller.
    async fn cleanup(&mut self) -> Vec<String> {
        let handles = std::mem::take(&mut self.handles);
        let mut failures = Vec::new();
        let mut deleted = 0usize;
        for handle in &handles {
            match self.transport.delete_remote(handle).await {
                Ok(()) => deleted += handle.chunk_msg_ids.len(),
                // Already gone counts as clean for OUR hygiene only if the
                // checker confirms it — verified below for every id.
                Err(StorageError::NotFound) => {}
                Err(e) => failures.push(format!("delete msg set {:?}: {e}", handle.first_msg_id)),
            }
            for id in &handle.chunk_msg_ids {
                if self.checker.part(*id).await.is_some() {
                    failures.push(format!(
                        "message {id} still fetchable after delete (prefix {} NOT clean)",
                        self.dir
                    ));
                }
            }
        }
        eprintln!(
            "CLEANUP: deleted {deleted} remote message(s) under {} ({} failure(s))",
            self.dir,
            failures.len()
        );
        for failure in &failures {
            eprintln!("CLEANUP FAILURE: {failure}");
        }
        failures
    }
}

/// Retries an operation on `StorageError::RateLimited` (the transport's
/// FLOOD_WAIT mapping), sleeping `retry_after` (capped) between attempts.
async fn retry_rate_limited<T, F, Fut>(what: &str, mut op: F) -> Result<T, StorageError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, StorageError>>,
{
    let mut attempt = 1usize;
    loop {
        match op().await {
            Err(StorageError::RateLimited { retry_after }) if attempt < RATE_LIMIT_ATTEMPTS => {
                let wait = retry_after
                    .unwrap_or(Duration::from_secs(30))
                    .min(RATE_LIMIT_CAP);
                eprintln!(
                    "{what}: rate limited (attempt {attempt}/{RATE_LIMIT_ATTEMPTS}), sleeping {wait:?}"
                );
                tokio::time::sleep(wait).await;
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// Drains a transport `ByteStream` to bytes. `serve_range` wraps
/// pre-buffered frames and never pends in practice; the `Pending` arm
/// spins on yield to stay correct regardless.
async fn drain(stream: ByteStream) -> Vec<u8> {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut stream = stream;
    let mut out = Vec::new();
    loop {
        match stream.as_mut().poll_next(&mut cx) {
            Poll::Ready(Some(Ok(chunk))) => out.extend_from_slice(&chunk),
            Poll::Ready(Some(Err(e))) => panic!("unexpected transport error while reading: {e}"),
            Poll::Ready(None) => return out,
            Poll::Pending => tokio::task::yield_now().await,
        }
    }
}

/// Opens a byte window from the transport with rate-limit tolerance.
async fn open_range_window(
    transport: &GrammersTransport,
    handle: &RemoteHandle,
    off: u64,
    len: u64,
) -> Vec<u8> {
    let stream = retry_rate_limited("open_range", || transport.open_range(handle, off, len))
        .await
        .expect("open_range succeeds");
    drain(stream).await
}

/// Shared test epilogue: run the body, then ALWAYS clean up, then convert
/// (body, cleanup) into the final pass/fail. Keeps the cleanup guarantee
/// out of every scenario body.
async fn finish(run: &mut E2eRun, outcome: Result<(), String>) {
    let cleanup_failures = run.cleanup().await;
    match (outcome, cleanup_failures.is_empty()) {
        (Ok(()), true) => eprintln!("PASS: scenario clean"),
        (Ok(()), false) => {
            panic!("scenario passed but CLEANUP INCOMPLETE (see CLEANUP FAILURE lines above)")
        }
        (Err(body_error), clean) => panic!(
            "scenario failed: {body_error}{}",
            if clean {
                ""
            } else {
                " (AND cleanup incomplete)"
            }
        ),
    }
}

/// Uploads `data` under `name` in the run prefix with a 1 MiB chunk plan
/// (multi-message on purpose) and returns the tracked receipt.
async fn upload_buffer(run: &mut E2eRun, name: &str, data: &[u8]) -> UploadReceipt {
    let file_dir = tempfile::tempdir().expect("tempdir for upload staging");
    let local = file_dir.path().join(name);
    std::fs::write(&local, data).expect("stage upload file");
    let chunk_size = MIB;
    let chunk_count = data.len().div_ceil(chunk_size as usize) as u32;
    let job = UploadJob {
        rel_path: run.rel(name),
        local_path: local,
        size: data.len() as u64,
        chunk_count: chunk_count.max(1),
        chunk_size,
    };
    let receipt = tokio::time::timeout(
        PHASE_TIMEOUT,
        retry_rate_limited("upload", || run.transport.upload(&job)),
    )
    .await
    .expect("upload phase timeout")
    .expect("upload succeeds");
    eprintln!(
        "uploaded {name}: {} part message(s), {} bytes",
        receipt.chunk_msg_ids.len(),
        receipt.uploaded_bytes
    );
    receipt
}

// ------------------------------------------------------------------ TG1 --

/// TG1: MTProto login → multi-part upload → independent stat of every
/// remote part (name/size/caption path) → whole-file download, byte-exact.
#[tokio::test]
#[ignore = "real network: needs the dedicated test bot credentials and the SOCKS5 proxy; run with --ignored --test-threads=1"]
async fn tg1_upload_stat_download_roundtrip() {
    let Some(mut run) = E2eRun::setup("tg1").await else {
        eprintln!("SKIP tg1_upload_stat_download_roundtrip: credentials not found");
        return;
    };
    let outcome = tokio::time::timeout(PHASE_TIMEOUT, tg1_body(&mut run))
        .await
        .unwrap_or_else(|_| Err("tg1 body timed out".to_owned()));
    finish(&mut run, outcome).await;
}

async fn tg1_body(run: &mut E2eRun) -> Result<(), String> {
    // 4.5 MiB over a 1 MiB chunk plan = 4 full parts + a 0.5 MiB remainder
    // (exercises the last-part remainder arithmetic).
    const SIZE: usize = (4 * MIB + MIB / 2) as usize;
    const CHUNK: u64 = MIB;
    let data = pseudo_random(SIZE, 0x7E51);

    let receipt = upload_buffer(run, "tg1_multi.bin", &data).await;
    run.track(&receipt);

    if receipt.uploaded_bytes as usize != SIZE {
        return Err(format!(
            "receipt.uploaded_bytes = {} want {SIZE}",
            receipt.uploaded_bytes
        ));
    }
    if receipt.chunk_msg_ids.len() != 5 {
        return Err(format!(
            "chunk_msg_ids.len() = {} want 5 (multi-part plan)",
            receipt.chunk_msg_ids.len()
        ));
    }
    if receipt.first_msg_id != receipt.chunk_msg_ids[0] {
        return Err("first_msg_id must be chunk 0's id".to_owned());
    }

    // Independent stat/list witness: every part document must exist with
    // the contract name, exact part size, and the caption carrying the
    // virtual path (captions are telegram's only path link — R6).
    for (index, id) in receipt.chunk_msg_ids.iter().enumerate() {
        let part = run
            .checker
            .part(*id)
            .await
            .ok_or_else(|| format!("part {index} (msg {id}) not fetchable as a document"))?;
        let expected_name = format!("tg1_multi.bin.part{index:03}");
        if part.name != expected_name {
            return Err(format!(
                "part {index} name = {:?} want {expected_name:?}",
                part.name
            ));
        }
        let expected_len = if index < 4 { CHUNK } else { MIB / 2 };
        if part.size != expected_len {
            return Err(format!(
                "part {index} size = {} want {expected_len}",
                part.size
            ));
        }
        let expected_path = format!("{}/tg1_multi.bin", run.dir);
        if !part.caption.contains(&expected_path) {
            return Err(format!(
                "part {index} caption does not carry the virtual path {expected_path}: {:?}",
                part.caption
            ));
        }
        eprintln!(
            "stat part {index}: name={} size={} caption-path ok",
            part.name, part.size
        );
    }

    // Whole-file download, byte-exact against the reference buffer.
    let handle = RemoteHandle {
        first_msg_id: receipt.first_msg_id,
        chunk_msg_ids: receipt.chunk_msg_ids.clone(),
        total_size: SIZE as u64,
        path: None,
    };
    let stream = tokio::time::timeout(
        PHASE_TIMEOUT,
        retry_rate_limited("open", || run.transport.open(&handle)),
    )
    .await
    .expect("open phase timeout")
    .map_err(|e| format!("open: {e}"))?;
    let got = drain(stream).await;
    if got != data {
        let first_diff = got
            .iter()
            .zip(data.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(got.len().min(data.len()));
        return Err(format!(
            "roundtrip mismatch: got {} bytes want {SIZE}, first difference at byte {first_diff}",
            got.len()
        ));
    }
    eprintln!("roundtrip: {SIZE} bytes downloaded, byte-exact match");
    Ok(())
}

// ------------------------------------------------------------------ TG2 --

/// TG2: range reads (declared capability) across part boundaries →
/// same-path overwrite (observed append-only telegram behavior, recorded)
/// → delete with emptiness verification.
#[tokio::test]
#[ignore = "real network: needs the dedicated test bot credentials and the SOCKS5 proxy; run with --ignored --test-threads=1"]
async fn tg2_range_overwrite_delete() {
    let Some(mut run) = E2eRun::setup("tg2").await else {
        eprintln!("SKIP tg2_range_overwrite_delete: credentials not found");
        return;
    };
    let outcome = tokio::time::timeout(PHASE_TIMEOUT, tg2_body(&mut run))
        .await
        .unwrap_or_else(|_| Err("tg2 body timed out".to_owned()));
    finish(&mut run, outcome).await;
}

async fn tg2_body(run: &mut E2eRun) -> Result<(), String> {
    // 3.5 MiB over a 1 MiB chunk plan = 3 full parts + a 0.5 MiB tail:
    // mid-file windows can span 2-3 part boundaries.
    const SIZE: usize = (3 * MIB + MIB / 2) as usize;
    let data = pseudo_random(SIZE, 0x7E52);
    let receipt = upload_buffer(run, "tg2_ranged.bin", &data).await;
    run.track(&receipt);
    if receipt.chunk_msg_ids.len() != 4 {
        return Err(format!(
            "chunk_msg_ids.len() = {} want 4",
            receipt.chunk_msg_ids.len()
        ));
    }
    let handle = RemoteHandle {
        first_msg_id: receipt.first_msg_id,
        chunk_msg_ids: receipt.chunk_msg_ids.clone(),
        total_size: SIZE as u64,
        path: None,
    };

    // Range leg — the driver declares range_read: true; each window is
    // compared byte-for-byte against the reference buffer.
    let windows: [(u64, u64); 4] = [
        (0, 256 * 1024),                    // head of part 0
        (MIB + MIB / 2, MIB),               // mid-file: spans parts 1..=3
        (2 * MIB - 128 * 1024, 256 * 1024), // straddles the part 1→2 boundary
        (3 * MIB, MIB),                     // tail: ask 1 MiB, EOF clamps to 0.5 MiB
    ];
    for (off, len) in windows {
        let got = open_range_window(&run.transport, &handle, off, len).await;
        let end = (off as usize + len as usize).min(SIZE);
        let want = &data[off as usize..end];
        if got != want {
            return Err(format!(
                "range [{off}, {off}+{len}) mismatch: got {} bytes want {}",
                got.len(),
                want.len()
            ));
        }
        eprintln!(
            "range [{off}, {off}+{len}) -> {} bytes byte-exact",
            got.len()
        );
    }

    // Overwrite leg — same rel_path, different bytes. OBSERVED telegram
    // behavior (pinned here, recorded for the batch report): the upload
    // succeeds as a NEW message set; the old messages remain fetchable by
    // id (append-only remote, no server-side overwrite); readers of the
    // new handle see only the new bytes. VFS-level overwrite semantics
    // (new row, old remote kept per remote_delete=false) live above this
    // transport and are out of scope here.
    let new_data = pseudo_random((2 * MIB) as usize, 0x7E53);
    let new_receipt = upload_buffer(run, "tg2_ranged.bin", &new_data).await;
    run.track(&new_receipt);
    let new_handle = RemoteHandle {
        first_msg_id: new_receipt.first_msg_id,
        chunk_msg_ids: new_receipt.chunk_msg_ids.clone(),
        total_size: new_data.len() as u64,
        path: None,
    };
    let overlap: Vec<i64> = new_receipt
        .chunk_msg_ids
        .iter()
        .filter(|id| receipt.chunk_msg_ids.contains(id))
        .copied()
        .collect();
    if !overlap.is_empty() {
        return Err(format!(
            "overwrite produced overlapping message ids {overlap:?} — expected a fresh set"
        ));
    }
    let got = open_range_window(&run.transport, &new_handle, 0, new_data.len() as u64).await;
    if got != new_data {
        return Err(format!(
            "overwrite readback mismatch: got {} bytes want {}",
            got.len(),
            new_data.len()
        ));
    }
    eprintln!(
        "overwrite: same rel_path re-uploaded as fresh message set {:?}; old set {:?} still fetchable by id (append-only remote — recorded)",
        new_receipt.chunk_msg_ids, receipt.chunk_msg_ids
    );
    // Prove the "old still fetchable" half of the recorded behavior.
    if run.checker.part(receipt.chunk_msg_ids[0]).await.is_none() {
        return Err("old part unexpectedly gone during overwrite leg".to_owned());
    }

    // Delete leg — delete the overwrite's message set, verify emptiness
    // through the independent checker. The ORIGINAL set stays tracked in
    // run.handles for the shared cleanup (every created message is ours to
    // remove).
    tokio::time::timeout(
        PHASE_TIMEOUT,
        retry_rate_limited("delete", || run.transport.delete_remote(&new_handle)),
    )
    .await
    .expect("delete phase timeout")
    .map_err(|e| format!("delete: {e}"))?;
    for id in &new_receipt.chunk_msg_ids {
        if run.checker.part(*id).await.is_some() {
            return Err(format!("message {id} still fetchable after delete"));
        }
    }
    eprintln!(
        "delete: message set {:?} removed and verified empty via the checker connection",
        new_receipt.chunk_msg_ids
    );
    // The new set was already deleted here; drop it from cleanup duty.
    run.handles
        .retain(|h| h.first_msg_id != new_handle.first_msg_id);
    Ok(())
}
