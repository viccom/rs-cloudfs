//! Real-network E2E (telegram E2E batch, stretch item): the FULL stack —
//! `Vfs` + `EncryptionScheme::AeadV2` + the real `GrammersTransport` —
//! against the dedicated test bot. `#[ignore]`d like every real-network
//! test; run explicitly and serially:
//!
//! ```text
//! CARGO_TARGET_DIR=<shared target> cargo test -p cloudkit-cli \
//!     --test tg_vfs_e2e -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Location note: this lives in the composition root because R1 forbids
//! any other crate from depending on a driver (dev-dependencies included),
//! and the driver layer must not depend on `cloudkit-core` — the CLI test
//! target is the one crate allowed to wire both ends.
//!
//! Scenario: put a multi-chunk plaintext through the v2 chunked-AEAD
//! streaming upload (no ciphertext staging file) onto the real transport,
//! verify the metadata row, then hydrate and range-read back through the
//! decrypting path — every leg compared byte-for-byte against the
//! reference buffer. Cleanup deletes the remote message set (collected
//! from the row's chunk records) and verifies emptiness at the transport
//! level; it runs before any assertion panic. Credentials follow the same
//! contract as `crates/drivers/ck-telegram/tests/tg_e2e.rs` (gitignored
//! test config + env overrides; missing credentials SKIP).
//!
//! Observed-behavior note: telegram declares `remote_delete = false` (K4),
//! so `Vfs::delete_remote_for_row` is a no-op — the cleanup here goes
//! through the transport's `delete_remote` directly, which is exactly the
//! same primitive a future remote-delete activation would use.

#![cfg(feature = "telegram")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ck_telegram::config::TransportConfig;
use ck_telegram::transport::GrammersTransport;
use cloudkit_core::cache::CacheManager;
use cloudkit_core::config::EncryptionScheme;
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{StreamSource, Vfs, VfsConfig};
use cloudkit_storage::transport::{ByteStream, CloudTransport, RemoteHandle, StorageError};

const MIB: u64 = 1024 * 1024;
const PHASE_TIMEOUT: Duration = Duration::from_secs(600);
const DEFAULT_PROXY_URL: &str = "socks5://127.0.0.1:7897";
const DEFAULT_CRED_FILE: &str = r"E:\GitHub\rs-CyDrive\test\config.toml";

// ------------------------------------------------------------- harness --

struct Credentials {
    bot_token: String,
    chat_id: i64,
}

impl Credentials {
    fn load() -> Option<Self> {
        let path = PathBuf::from(
            std::env::var("CYDRIVE_TG_CRED_FILE").unwrap_or_else(|_| DEFAULT_CRED_FILE.to_owned()),
        );
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("SKIP: credential file {} unreadable ({e})", path.display());
                return None;
            }
        };
        let mut bot_token = None;
        let mut chat_id = None;
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
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
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs() as u128
}

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    (0..len).map(|_| (next() >> 33) as u8).collect()
}

/// Random hex password for this run only — never persisted anywhere.
fn random_password(seed: u64) -> String {
    pseudo_random(16, seed)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn drain(stream: ByteStream) -> Vec<u8> {
    use std::task::{Context, Poll, Waker};
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

// ------------------------------------------------------------ scenario --

#[tokio::test]
#[ignore = "real network: needs the dedicated test bot credentials and the SOCKS5 proxy; run with --ignored --test-threads=1"]
async fn tg_vfs_aead_v2_full_stack_roundtrip() {
    let Some(creds) = Credentials::load() else {
        eprintln!("SKIP tg_vfs_aead_v2_full_stack_roundtrip: credentials not found");
        return;
    };
    // Stable session path in the OS temp area: the authorization is
    // imported once per machine, later runs reuse it (FLOOD_WAIT hygiene —
    // rapid per-run re-auth earns hour-scale server-side waits). Shares the
    // SAME session file the ck-telegram e2e suite uses; the three suites
    // are run serially so the file never has two live owners.
    let session_path = std::env::temp_dir()
        .join("tg-e2e-sessions")
        .join("tg-e2e-shared.session");
    std::fs::create_dir_all(session_path.parent().expect("session dir parent"))
        .expect("session dir");
    let transport = GrammersTransport::connect(TransportConfig {
        bot_token: creds.bot_token,
        chat_id: creds.chat_id,
        session_path,
        proxy_url: Some(proxy_url()),
        ..TransportConfig::default()
    })
    .await
    .expect("transport connect (MTProto login via proxy)");
    let transport: Arc<dyn CloudTransport> = Arc::new(transport);

    // Vfs over the real transport with the v2 scheme and a random password.
    let data_dir = tempfile::tempdir().expect("tempdir for db/cache");
    let db = Arc::new(MetaDatabase::open(&data_dir.path().join("meta.db")).expect("db"));
    let cache_root = data_dir.path().join("cache");
    let password = random_password(0x7E60);
    let cfg = VfsConfig {
        chunk_size_bytes: MIB,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(5),
            max_attempts: 3,
        },
        encryption_password: Some(password),
        encryption_scheme: EncryptionScheme::AeadV2,
        hydrate_timeout: Duration::from_secs(300),
    };
    let vfs = Arc::new(Vfs::new(
        Arc::clone(&db),
        CacheManager::new(cache_root.clone(), 1 << 30),
        Arc::clone(&transport),
        cfg,
    ));

    // 1.5 MiB over the 1 MiB VFS chunk plan: multi-message on the wire,
    // two crypto chunks + tags in the v2 container.
    const SIZE: usize = (MIB + MIB / 2) as usize;
    let plaintext = pseudo_random(SIZE, 0x7E61);
    let dir_prefix = format!("/_e2e/tg-e2e-{}-vfs", unix_stamp());
    let rel = RelPath::new(&format!("{dir_prefix}/vfs_aead.bin")).expect("valid rel path");
    let body = tokio::time::timeout(PHASE_TIMEOUT, async {
        vfs.put(&rel, &plaintext, 1.0).await.expect("vfs put");
        vfs.shutdown().await; // drain the upload queue to a terminal state

        // Row metadata: uploaded, encrypted, scheme recorded, plaintext size.
        let row = db.get_file(rel.as_str()).expect("db read").expect("row exists");
        if !row.is_uploaded {
            return Err("row not uploaded after queue drain".to_owned());
        }
        if !row.is_encrypted {
            return Err("row not flagged encrypted".to_owned());
        }
        if row.encryption_scheme != "aead_v2" {
            return Err(format!("row scheme = {:?} want \"aead_v2\"", row.encryption_scheme));
        }
        if row.size != SIZE as i64 {
            return Err(format!("row size = {} want {SIZE} (plaintext size)", row.size));
        }
        eprintln!(
            "vfs row: uploaded=true encrypted=true scheme=aead_v2 size={} chunks={}",
            row.size,
            db.get_chunks_by_file_id(row.id).expect("chunks").len()
        );

        // Hydrate with the local cache copy removed: the bytes must come
        // back over the real transport and decrypt to the plaintext.
        let cache = CacheManager::new(cache_root.clone(), u64::MAX);
        let _ = std::fs::remove_file(cache.local_path(&rel));
        let path = vfs.hydrate(&rel).await.expect("hydrate");
        let roundtrip = std::fs::read(&path).expect("read hydrated file");
        if roundtrip != plaintext {
            return Err(format!(
                "hydrate roundtrip mismatch: got {} bytes want {SIZE}",
                roundtrip.len()
            ));
        }
        eprintln!("hydrate: {SIZE} bytes round-tripped byte-exact through the v2 decrypting path");

        // Range-read leg: open_read must admit the row to the streaming
        // arm (K47 v2 shape), and a mid-file window straddling the first
        // crypto-chunk boundary must decrypt byte-exactly.
        let _ = std::fs::remove_file(cache.local_path(&rel));
        match vfs.open_read(&rel).await.expect("open_read") {
            StreamSource::Stream { handle, total_size, transport } => {
                if total_size != SIZE as u64 {
                    return Err(format!("Stream total_size = {total_size} want {SIZE}"));
                }
                let off = MIB - 4096;
                let len = 8192;
                let stream = transport
                    .open_range(&handle, off, len)
                    .await
                    .expect("open_range on the decrypting transport");
                let got = drain(stream).await;
                let want = &plaintext[off as usize..(off + len) as usize];
                if got != want {
                    return Err(format!(
                        "range window [{off},+{len}) mismatch: got {} bytes want {}",
                        got.len(),
                        want.len()
                    ));
                }
                eprintln!("open_read: Stream arm, window [{off},+{len}) decrypted byte-exact");
            }
            StreamSource::Hydrate => {
                return Err("open_read routed an aead_v2 row to Hydrate — K47 streaming shape not admitted".to_owned());
            }
        }

        // Cleanup: collect every remote message id the row references,
        // delete them at the transport level, and verify emptiness.
        let row = db.get_file(rel.as_str()).expect("db read").expect("row exists");
        let chunk_rows = db.get_chunks_by_file_id(row.id).expect("chunks");
        let mut msg_ids: Vec<i64> = chunk_rows
            .iter()
            .filter_map(|c| c.telegram_msg_id)
            .collect();
        msg_ids.sort_unstable();
        msg_ids.dedup();
        if msg_ids.is_empty() {
            return Err("no remote message ids on the uploaded row — nothing to clean up".to_owned());
        }
        let handle = RemoteHandle {
            first_msg_id: msg_ids[0],
            chunk_msg_ids: msg_ids.clone(),
            total_size: 0,
            path: None,
        };
        let delete_result = transport.delete_remote(&handle).await;
        if let Err(StorageError::NotFound) = delete_result {
            // Already gone counts as clean; verified below.
        } else {
            delete_result.expect("delete_remote");
        }
        let mut failures = Vec::new();
        if let Ok(resurrected) = transport.open(&handle).await {
            let mut bytes = drain(resurrected).await;
            if !bytes.is_empty() {
                bytes.clear();
                failures.push(format!(
                    "messages {msg_ids:?} still downloadable after delete (prefix {dir_prefix} NOT clean)"
                ));
            }
        }
        let deleted = msg_ids.len();
        eprintln!("CLEANUP: deleted {deleted} remote message(s) under {dir_prefix} ({} failure(s))", failures.len());
        for failure in &failures {
            eprintln!("CLEANUP FAILURE: {failure}");
        }
        if !failures.is_empty() {
            return Err("cleanup incomplete".to_owned());
        }
        Ok(())
    })
    .await
    .unwrap_or_else(|_| Err("vfs scenario timed out".to_owned()));

    if let Err(error) = body {
        panic!("vfs aead_v2 full-stack scenario failed: {error}");
    }
    eprintln!("PASS: vfs aead_v2 full-stack clean");
}
