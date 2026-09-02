//! RED-phase tests for `cydrive_core::bot` (M2 bot-command unit). All
//! bodies panic through the `handle_command`/inspection-API stubs until
//! the GREEN phase lands.
//!
//! Contract under test (Python baseline `telegram_client.py:94-141`,
//! `_process_bot_command`): the trimmed text is matched with
//! `startswith` prefixes in the order help (/start, /help) -> /stats ->
//! /search -> /get; anything else is silently ignored (no reply). The
//! help and stats texts and the search reply format are pinned verbatim,
//! including the `startswith` quirk ("/helpme" answers help) and the
//! `size_gb >= 1` GB/MB branch. `/get` is the adjudicated completion of
//! the never-implemented baseline command (adjudication 2026-09-02):
//! exact virtual-path lookup, then unique-match name search, then
//! hydrate + send_document.

use std::sync::Arc;
use std::time::Duration;

use cydrive_core::bot::handle_command;
use cydrive_core::cache::CacheManager;
use cydrive_core::database::{FileUpsert, MetaDatabase};
use cydrive_core::inbound::spawn_inbound_worker;
use cydrive_core::rel_path::RelPath;
use cydrive_core::transport::mock::MockTransport;
use cydrive_core::transport::{CloudTransport, IncomingEvent};
use cydrive_core::upload_queue::RetryPolicy;
use cydrive_core::vfs::{Vfs, VfsConfig};

/// Help text, verbatim Python baseline (`telegram_client.py:99-107`).
const HELP_TEXT: &str = "🚀 **CyDrive Cloud Storage Engine v2.0**\nDeveloped by Cynet Security Team (https://cynetx.ir)\n\n**Available Commands:**\n📊 `/stats` - View cloud storage analytics\n🔍 `/search <query>` - Search files in your drive\n📥 `/get <filename>` - Download a file directly\nℹ️ Send any file to this chat to save it to your Windows Drive!";

/// VfsConfig for the bot tests (same shape as `tests/inbound.rs`): the
/// queue is only exercised by `/get`, it just has to spawn cleanly.
fn test_cfg() -> VfsConfig {
    VfsConfig {
        chunk_size_bytes: 1024 * 1024,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: None,
        hydrate_timeout: Duration::from_secs(180),
    }
}

/// Builds a real temp environment (SQLite db + cache tree + `Vfs` over a
/// plain mock transport whose inspection APIs back every assertion). The
/// first tuple item keeps the temp dir alive.
async fn bot_env() -> (
    tempfile::TempDir,
    Arc<MetaDatabase>,
    Arc<Vfs>,
    Arc<MockTransport>,
) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let mock = Arc::new(MockTransport::new());
    let vfs = Arc::new(Vfs::new(
        Arc::clone(&db),
        CacheManager::new(dir.path().join("cache"), 64 * 1024 * 1024),
        Arc::clone(&mock) as Arc<dyn CloudTransport>,
        test_cfg(),
    ));
    (dir, db, vfs, mock)
}

/// Splits a virtual path into (parent_dir, name) the way every upsert
/// caller does (`/a/b.txt` -> ("/a", "b.txt"), `/x` -> ("/", "x")).
fn split_parent(rel: &str) -> (String, String) {
    let (parent, name) = rel.rsplit_once('/').expect("rel paths start with /");
    let parent = if parent.is_empty() {
        "/".to_string()
    } else {
        parent.to_string()
    };
    (parent, name.to_string())
}

/// Seeds one plain file row (no upload, no cache copy, plaintext).
fn seed_file(db: &MetaDatabase, rel: &str, size: i64, is_uploaded: bool) {
    let (parent_dir, name) = split_parent(rel);
    db.upsert_file(&FileUpsert {
        rel_path: rel.to_string(),
        name,
        parent_dir,
        size,
        mtime: 0.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: None,
        is_uploaded,
        is_cached: false,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: None,
    })
    .expect("seed file row");
}

/// Seeds one directory row (size 0, as the Python baseline stores dirs).
fn seed_dir(db: &MetaDatabase, rel: &str) {
    let (parent_dir, name) = split_parent(rel);
    db.upsert_file(&FileUpsert {
        rel_path: rel.to_string(),
        name,
        parent_dir,
        size: 0,
        mtime: 0.0,
        sha256: None,
        is_dir: true,
        telegram_msg_id: None,
        is_uploaded: true,
        is_cached: false,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: None,
    })
    .expect("seed dir row");
}

/// Polls (bounded: 200 x 10ms) until the row at `rel` reports
/// `is_uploaded` — the fire-and-forget queue finished the upload.
async fn wait_uploaded(db: &MetaDatabase, rel: &str) {
    for _ in 0..200 {
        let uploaded = db
            .get_file(rel)
            .expect("db read")
            .is_some_and(|row| row.is_uploaded);
        if uploaded {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("upload of {rel} never completed");
}

#[tokio::test]
async fn help_and_start_prefix_reply_exact() {
    let (_dir, db, vfs, mock) = bot_env().await;

    handle_command(&db, &vfs, &*mock, "Y:", "/start")
        .await
        .expect("handle /start");
    handle_command(&db, &vfs, &*mock, "Y:", "/help")
        .await
        .expect("handle /help");
    // startswith quirk mirror: any "/help*" prefix triggers the help
    // text (Python `text.startswith("/help")`).
    handle_command(&db, &vfs, &*mock, "Y:", "/helpme")
        .await
        .expect("handle /helpme");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 3);
    for text in &texts {
        assert_eq!(text, HELP_TEXT);
    }
    vfs.shutdown().await;
}

#[tokio::test]
async fn unknown_text_no_reply() {
    let (_dir, db, vfs, mock) = bot_env().await;

    handle_command(&db, &vfs, &*mock, "Y:", "hello")
        .await
        .expect("handle non-command text");

    assert!(
        mock.sent_texts().is_empty(),
        "non-command text is silently ignored"
    );
    vfs.shutdown().await;
}

#[tokio::test]
async fn stats_reply_exact_mb_branch() {
    let (_dir, db, vfs, mock) = bot_env().await;
    // Two files of 1.5 MiB each (3 MiB total) + one directory; both
    // files uploaded -> 3.00 MB (size_gb = 0.0029 < 1).
    seed_file(&db, "/first.bin", 1_572_864, true);
    seed_file(&db, "/second.bin", 1_572_864, true);
    seed_dir(&db, "/docs");

    handle_command(&db, &vfs, &*mock, "Y:", "/stats")
        .await
        .expect("handle /stats");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(
        texts[0],
        "📊 **CyDrive Storage Statistics**\n\n\
         📁 **Total Files:** `2`\n\
         🗂️ **Total Folders:** `1`\n\
         ☁️ **Total Cloud Storage:** `3.00 MB`\n\
         ✅ **Synced Files:** `2`\n\
         🖥️ **Mapped Windows Drive:** `Y:`"
    );
    vfs.shutdown().await;
}

#[tokio::test]
async fn stats_reply_gb_branch() {
    let (_dir, db, vfs, mock) = bot_env().await;
    // One row of 1_610_612_736 bytes (1536 MiB -> size_gb = 1.50).
    seed_file(&db, "/big.bin", 1_610_612_736, true);

    handle_command(&db, &vfs, &*mock, "Y:", "/stats")
        .await
        .expect("handle /stats");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(
        texts[0],
        "📊 **CyDrive Storage Statistics**\n\n\
         📁 **Total Files:** `1`\n\
         🗂️ **Total Folders:** `0`\n\
         ☁️ **Total Cloud Storage:** `1.50 GB`\n\
         ✅ **Synced Files:** `1`\n\
         🖥️ **Mapped Windows Drive:** `Y:`"
    );
    vfs.shutdown().await;
}

#[tokio::test]
async fn search_missing_arg_warning_exact() {
    let (_dir, db, vfs, mock) = bot_env().await;

    handle_command(&db, &vfs, &*mock, "Y:", "/search")
        .await
        .expect("handle /search without a keyword");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(
        texts[0],
        "⚠️ Please specify a search keyword. e.g. `/search document.pdf`"
    );
    vfs.shutdown().await;
}

#[tokio::test]
async fn search_no_results_exact() {
    let (_dir, db, vfs, mock) = bot_env().await;
    seed_file(&db, "/unrelated.bin", 42, true);

    handle_command(&db, &vfs, &*mock, "Y:", "/search zzz")
        .await
        .expect("handle /search with no matches");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0], "🔍 No files found matching: `zzz`");
    vfs.shutdown().await;
}

#[tokio::test]
async fn search_results_format_top15() {
    let (_dir, db, vfs, mock) = bot_env().await;
    // 20 matching files + 1 matching directory: the reply is capped at
    // 15 rows, dirs sort first (is_dir DESC, name ASC) and carry the
    // folder icon; size_kb is floor division (2048 -> 2, 0 -> 0).
    for index in 0..20 {
        seed_file(&db, &format!("/file_{index:02}.txt"), 2048, true);
    }
    seed_dir(&db, "/files");

    handle_command(&db, &vfs, &*mock, "Y:", "/search file")
        .await
        .expect("handle /search with matches");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    // Python: `lines = ["🔍 **Search Results:**\n"]`, rows appended,
    // reply = `"\n".join(lines)` — the header keeps its own trailing
    // newline AND a join separator follows it (a blank line between the
    // header and the first row), and the text ends without a newline.
    let mut lines = vec!["🔍 **Search Results:**\n".to_string()];
    lines.push("📁 `files` (0 KB)".to_string());
    for index in 0..14 {
        lines.push(format!("📄 `file_{index:02}.txt` (2 KB)"));
    }
    assert_eq!(texts[0], lines.join("\n"));

    // Exactly 15 rows: 1 dir + files 00..13 (16 joined elements ->
    // 16 newlines: the header's trailing one plus 15 separators).
    assert_eq!(texts[0].matches('\n').count(), 16);
    assert!(
        !texts[0].contains("file_14"),
        "rows past the 15th are dropped"
    );
    vfs.shutdown().await;
}

#[tokio::test]
async fn get_by_exact_path_sends_document() {
    let (_dir, db, vfs, mock) = bot_env().await;
    mock.connect().await.expect("open the mock gate");

    // Seed through the real upload path: put -> queue -> mock remote,
    // then the queue drops the local copy, so /get must hydrate back.
    let rel = RelPath::new("/hello.txt").expect("valid path");
    vfs.put(&rel, b"hello payload", 0.0)
        .await
        .expect("put the file");
    wait_uploaded(&db, "/hello.txt").await;

    // A token carrying its own leading '/' normalizes to the same
    // virtual path (adjudicated /get semantics).
    handle_command(&db, &vfs, &*mock, "Y:", "/get /hello.txt")
        .await
        .expect("handle /get");

    let docs = mock.sent_documents();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].0, "hello.txt");
    assert_eq!(docs[0].1, b"hello payload".to_vec());
    // Hydration put the cache copy back (the queue deleted it after the
    // upload), and no text reply accompanies the document.
    assert!(
        db.get_file("/hello.txt")
            .expect("db read")
            .expect("row exists")
            .is_cached
    );
    assert!(mock.sent_texts().is_empty());
    vfs.shutdown().await;
}

#[tokio::test]
async fn get_missing_and_ambiguous_replies() {
    let (_dir, db, vfs, mock) = bot_env().await;
    seed_file(&db, "/alpha.doc", 100, true);
    seed_file(&db, "/alpha2.doc", 200, true);

    // No exact path and no name match.
    handle_command(&db, &vfs, &*mock, "Y:", "/get nope.bin")
        .await
        .expect("handle /get without a match");
    // No exact path, two name matches ("alpha" is a substring of both).
    handle_command(&db, &vfs, &*mock, "Y:", "/get alpha")
        .await
        .expect("handle /get with an ambiguous match");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 2);
    assert_eq!(texts[0], "❌ File not found: `nope.bin`");
    assert_eq!(
        texts[1],
        "⚠️ Ambiguous match for `alpha`: 2 files found, be more specific"
    );
    assert!(mock.sent_documents().is_empty());
    vfs.shutdown().await;
}

#[tokio::test]
async fn worker_dispatches_stats() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let mock = Arc::new(
        MockTransport::builder()
            .incoming(vec![IncomingEvent::Command {
                text: "/stats".to_string(),
            }])
            .build(),
    );
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Arc::new(Vfs::new(
        Arc::clone(&db),
        CacheManager::new(dir.path().join("cache"), 64 * 1024 * 1024),
        Arc::clone(&transport),
        test_cfg(),
    ));

    let handle = spawn_inbound_worker(Arc::clone(&vfs), Arc::clone(&transport), "Y:".to_string());
    // Poll (bounded: 200 x 10ms) until the dispatched command produced a
    // reply on the transport.
    for _ in 0..200 {
        if !mock.sent_texts().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let texts = mock.sent_texts();
    assert!(
        !texts.is_empty(),
        "the worker dispatched the /stats command to the bot handler"
    );
    assert!(texts[0].contains("Total Files"));
    handle.shutdown().await;
    vfs.shutdown().await;
}
