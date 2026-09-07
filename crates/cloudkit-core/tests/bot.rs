//! RED-phase tests for `cloudkit_core::bot` (M2 bot-command unit). All
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

use cloudkit_core::bot::handle_command;
use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::inbound::spawn_inbound_worker;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::{CloudTransport, IncomingEvent};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};

/// Help text: Python baseline (`telegram_client.py:99-107`) extended with
/// the tier-1 command rows (plan contract C8, commit 5725976): `/ls`,
/// `/mkdir`, `/rm`, `/quota`, `/queue` inserted after `/get`, before the
/// footnote. Not a Python compat contract — adjudicated 2026-09-03.
const HELP_TEXT: &str = "🚀 **CyDrive Cloud Storage Engine v2.0**\nDeveloped by Cynet Security Team (https://cynetx.ir)\n\n**Available Commands:**\n📊 `/stats` - View cloud storage analytics\n🔍 `/search <query>` - Search files in your drive\n📥 `/get <filename>` - Download a file directly\n📂 `/ls [path]` - List a directory\n📁 `/mkdir <path>` - Create a directory\n🗑️ `/rm <path>` - Delete a file\n💾 `/quota` - View storage usage\n📋 `/queue` - View the upload queue\nℹ️ Send any file to this chat to save it to your Windows Drive!";

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
        encryption_scheme: cloudkit_core::config::EncryptionScheme::Gcm,
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

// ---------------------------------------------------------------------------
// Tier-1 bot commands (/ls /mkdir /rm /quota /queue) — plan contract C8.
// All reply texts are new (no Python baseline); expected strings are pinned
// to the plan's contract table verbatim.
// ---------------------------------------------------------------------------

/// `/help` must list the five new commands alongside the baseline ones.
#[tokio::test]
async fn help_text_lists_new_commands() {
    let (_dir, db, vfs, mock) = bot_env().await;

    handle_command(&db, &vfs, &*mock, "Y:", "/help")
        .await
        .expect("handle /help");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    for command in ["/ls", "/mkdir", "/rm", "/quota", "/queue"] {
        assert!(
            texts[0].contains(command),
            "help text must list {command}: {}",
            texts[0]
        );
    }
    vfs.shutdown().await;
}

/// `/ls` at the root: 25 entries (24 files + 1 dir) collapse into the
/// header line, exactly 20 entry lines (dirs first, then name ASC) and
/// one `… and 5 more` line. Entry formats are pinned: `d {name}/` and
/// `f {name} ({size/1024} KB)` with floor-divided KB.
#[tokio::test]
async fn ls_root_lists_entries_with_cap() {
    let (_dir, db, vfs, mock) = bot_env().await;
    for index in 0..24 {
        seed_file(&db, &format!("/file_{index:02}.txt"), 2048, true);
    }
    seed_dir(&db, "/zdir");

    handle_command(&db, &vfs, &*mock, "Y:", "/ls")
        .await
        .expect("handle /ls");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    let lines: Vec<&str> = texts[0].lines().collect();
    assert_eq!(lines[0], "📁 /", "the first line echoes the path");
    assert_eq!(
        lines.len(),
        22,
        "1 header + 20 entry lines + 1 more-line, got: {texts:?}"
    );
    assert_eq!(
        lines[1], "d zdir/",
        "directory entries use the `d <name>/` format"
    );
    for (offset, line) in lines[2..=20].iter().enumerate() {
        assert_eq!(
            *line,
            format!("f file_{offset:02}.txt (2 KB)"),
            "file entries keep `f <name> (<kb> KB)`, name ASC after the dirs"
        );
    }
    assert_eq!(lines[21], "… and 5 more");
    assert!(
        !texts[0].contains("file_19"),
        "entries past the cap are folded into the more-line"
    );
    vfs.shutdown().await;
}

/// `/ls <file path>` answers with that file's single entry line.
#[tokio::test]
async fn ls_specific_file_replies_single_line() {
    let (_dir, db, vfs, mock) = bot_env().await;
    seed_file(&db, "/solo.txt", 2048, true);

    handle_command(&db, &vfs, &*mock, "Y:", "/ls /solo.txt")
        .await
        .expect("handle /ls on a file path");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(
        texts[0], "f solo.txt (2 KB)",
        "a file path answers with its single entry line, nothing else"
    );
    vfs.shutdown().await;
}

/// `/ls <missing path>` answers `no such directory: {path}`.
#[tokio::test]
async fn ls_missing_dir_errors() {
    let (_dir, db, vfs, mock) = bot_env().await;

    handle_command(&db, &vfs, &*mock, "Y:", "/ls /nope")
        .await
        .expect("handle /ls on a missing path");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0], "no such directory: /nope");
    vfs.shutdown().await;
}

/// `/ls` of a directory with no children answers `(empty)`.
#[tokio::test]
async fn ls_empty_dir_replies_empty_marker() {
    let (_dir, db, vfs, mock) = bot_env().await;
    vfs.create_dir(&RelPath::new("/empty").expect("valid path"))
        .expect("create the empty directory");

    handle_command(&db, &vfs, &*mock, "Y:", "/ls /empty")
        .await
        .expect("handle /ls on an empty directory");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    assert!(
        texts[0].contains("(empty)"),
        "an empty directory answers `(empty)`: {}",
        texts[0]
    );
    vfs.shutdown().await;
}

/// `/mkdir` creates the directory row; a second `/mkdir` of the same path
/// reports `already exists` instead of failing.
#[tokio::test]
async fn mkdir_creates_and_reports_exists() {
    let (_dir, db, vfs, mock) = bot_env().await;

    handle_command(&db, &vfs, &*mock, "Y:", "/mkdir /newdir")
        .await
        .expect("handle first /mkdir");
    handle_command(&db, &vfs, &*mock, "Y:", "/mkdir /newdir")
        .await
        .expect("handle second /mkdir");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 2);
    assert_eq!(texts[0], "created: /newdir");
    assert_eq!(texts[1], "already exists: /newdir");
    let row = db
        .get_file("/newdir")
        .expect("db read")
        .expect("directory row exists");
    assert!(row.is_dir, "/mkdir produced a directory row");
    vfs.shutdown().await;
}

/// `/rm` of a real file: the row is gone, the reply confirms the deletion
/// and states that the remote Telegram message is kept (Python parity).
#[tokio::test]
async fn rm_deletes_file_and_mentions_remote_kept() {
    let (_dir, db, vfs, mock) = bot_env().await;
    mock.connect().await.expect("open the mock gate");
    let rel = RelPath::new("/rm_me.bin").expect("valid path");
    vfs.put(&rel, b"rm payload", 0.0)
        .await
        .expect("put the file");
    wait_uploaded(&db, "/rm_me.bin").await;

    handle_command(&db, &vfs, &*mock, "Y:", "/rm /rm_me.bin")
        .await
        .expect("handle /rm");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    assert!(
        texts[0].contains("deleted: /rm_me.bin"),
        "the reply confirms the deletion: {}",
        texts[0]
    );
    assert!(
        texts[0].to_lowercase().contains("remote"),
        "the reply mentions the remote message being kept: {}",
        texts[0]
    );
    assert!(
        db.get_file("/rm_me.bin").expect("db read").is_none(),
        "the row is gone after /rm"
    );
    vfs.shutdown().await;
}

/// `/rm` of a missing path answers `no such file: {path}`.
#[tokio::test]
async fn rm_missing_errors() {
    let (_dir, db, vfs, mock) = bot_env().await;

    handle_command(&db, &vfs, &*mock, "Y:", "/rm /ghost.bin")
        .await
        .expect("handle /rm on a missing path");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0], "no such file: /ghost.bin");
    vfs.shutdown().await;
}

/// `/rm` of a directory answers `is a directory: {path}` (no recursive
/// delete in tier 1).
#[tokio::test]
async fn rm_directory_errors() {
    let (_dir, db, vfs, mock) = bot_env().await;
    seed_dir(&db, "/docs");

    handle_command(&db, &vfs, &*mock, "Y:", "/rm /docs")
        .await
        .expect("handle /rm on a directory");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0], "is a directory: /docs");
    vfs.shutdown().await;
}

/// `/quota`: first line names the mapped drive letter, then the file
/// count with the floor-divided MB total, the dir count and the pending
/// upload count (2 files of 1 MiB + 2 MiB -> `files: 2 (3 MB)`).
#[tokio::test]
async fn quota_reports_counts_and_bytes() {
    let (_dir, db, vfs, mock) = bot_env().await;
    seed_file(&db, "/q_a.bin", 1_048_576, true);
    seed_file(&db, "/q_b.bin", 2_097_152, true);
    seed_dir(&db, "/qdir");

    handle_command(&db, &vfs, &*mock, "Y:", "/quota")
        .await
        .expect("handle /quota");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    let lines: Vec<&str> = texts[0].lines().collect();
    assert_eq!(
        lines.len(),
        4,
        "header + files + dirs + pending uploads, got: {texts:?}"
    );
    assert!(
        lines[0].contains("Y:"),
        "the first line names the mapped drive letter: {lines:?}"
    );
    assert_eq!(
        lines[1], "files: 2 (3 MB)",
        "3 MiB over non-directory rows, MB floor-divided"
    );
    assert_eq!(lines[2], "dirs: 1");
    assert_eq!(lines[3], "pending uploads: 0");
    vfs.shutdown().await;
}

/// `/queue` reports the five counters on one line. Harness: the first job
/// eats the two scripted failures (two retries) then succeeds against the
/// exhausted script; the second job succeeds first try. All jobs drained
/// before the command runs, so every value is deterministic.
#[tokio::test]
async fn queue_reports_counters() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let mock = Arc::new(
        MockTransport::builder()
            .upload_action(cloudkit_core::transport::mock::UploadAction::Fail {
                error: cloudkit_core::transport::StorageError::Unavailable("flaky 1".into()),
            })
            .upload_action(cloudkit_core::transport::mock::UploadAction::Fail {
                error: cloudkit_core::transport::StorageError::Unavailable("flaky 2".into()),
            })
            .build(),
    );
    mock.connect().await.expect("open the mock gate");
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Arc::new(Vfs::new(
        Arc::clone(&db),
        CacheManager::new(dir.path().join("cache"), 64 * 1024 * 1024),
        Arc::clone(&transport),
        test_cfg(),
    ));

    for name in ["/q_one.bin", "/q_two.bin"] {
        let rel = RelPath::new(name).expect("valid path");
        vfs.put(&rel, b"queue payload", 0.0)
            .await
            .expect("put the file");
    }
    wait_uploaded(&db, "/q_one.bin").await;
    wait_uploaded(&db, "/q_two.bin").await;
    // Drain to the terminal state so succeeded/retries are final.
    for _ in 0..200 {
        if vfs.queue_stats().succeeded == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    handle_command(&db, &vfs, &*mock, "Y:", "/queue")
        .await
        .expect("handle /queue");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(
        texts[0], "queue: enqueued=2 succeeded=2 retries=2 degraded=0 pending=0",
        "one line, counter order enqueued/succeeded/retries/degraded/pending"
    );
    vfs.shutdown().await;
}

/// `/rm` of a pending upload (plan F2 / review H2): the delete is
/// refused with a "still uploading" reply and the row survives — for a
/// pending row the local cache copy is the only copy of the bytes.
///
/// Determinism: the mock stays **disconnected** on purpose — its
/// `connected` gate makes every upload attempt fail with `NotConnected`
/// (retry, then degrade), so the row can never flip to uploaded no
/// matter when the worker is polled relative to `handle_command`'s
/// internal awaits; `send_text` is not gated, so the reply still
/// records. The cache copy can therefore never be dropped by a
/// successful upload either.
#[tokio::test]
async fn rm_pending_upload_replies_still_uploading() {
    let (_dir, db, vfs, mock) = bot_env().await;
    let rel = RelPath::new("/uploading.bin").expect("valid path");
    vfs.put(&rel, b"uploading payload", 0.0)
        .await
        .expect("put the file (stays pending: the mock is not connected)");

    handle_command(&db, &vfs, &*mock, "Y:", "/rm /uploading.bin")
        .await
        .expect("handle /rm on a pending upload");

    let texts = mock.sent_texts();
    assert_eq!(texts.len(), 1);
    assert!(
        texts[0].contains("still uploading"),
        "the refusal tells the user the upload is still in flight: {}",
        texts[0]
    );
    let row = db
        .get_file("/uploading.bin")
        .expect("db read")
        .expect("the row survives the refused /rm");
    assert!(!row.is_uploaded, "the row is still a pending upload");
    let copy = CacheManager::new(_dir.path().join("cache"), u64::MAX).local_path(&rel);
    assert!(
        copy.exists(),
        "the only copy of the bytes survives the refused /rm"
    );
    vfs.shutdown().await;
}
