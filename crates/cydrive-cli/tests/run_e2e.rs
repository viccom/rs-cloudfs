//! End-to-end offline tests for the `cydrive` run orchestration (unit C).
//!
//! Each scenario writes a real `config.toml` (via `save_toml`, so Windows
//! temp paths are escaped correctly) into a temp directory, boots the full
//! stack with a pre-connected `MockTransport` on an ephemeral WebDAV port
//! (`webdav_port = 0`; production's 8080 comes from the config), drives it
//! with hand-rolled HTTP/1.1 over a raw `TcpStream` and verifies the boot
//! and shutdown contracts:
//!
//! 1. boot → PROPFIND answers 207 → shutdown refuses new connections;
//! 2. PUT lands through HTTP → FS adapter → queue → Mock remote and the
//!    shutdown drain marks the row uploaded;
//! 3. crash-staged pending rows re-enter the queue at boot;
//! 4. config discovery prefers `config.toml` over a legacy `config.json`;
//! 5. discovery in an empty directory is an actionable error;
//! 6. a legacy `config.json` alone is discovered and loads.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use cydrive_cli::{discover_config, run_with_transport, RunHandle};
use cydrive_core::config::CyDriveConfig;
use cydrive_core::database::{FileUpsert, MetaDatabase};
use cydrive_core::transport::mock::MockTransport;
use cydrive_core::transport::CloudTransport;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

// ------------------------------------------------------------- helpers ---

/// Every `CYDRIVE_*` override key `with_env_overrides` recognises; the
/// discovery tests clear them so a developer shell cannot skew results.
const ENV_KEYS: &[&str] = &[
    "CYDRIVE_BOT_TOKEN",
    "CYDRIVE_CHAT_ID",
    "CYDRIVE_WEBDAV_PORT",
    "CYDRIVE_WEB_UI_PORT",
    "CYDRIVE_DRIVE_LETTER",
    "CYDRIVE_CHUNK_SIZE_MB",
    "CYDRIVE_ENABLE_ENCRYPTION",
];

/// Serialises every test that changes the process-wide working directory
/// (tests in one binary share one process; `set_current_dir` races).
static CWD_MUTEX: Mutex<()> = Mutex::new(());

/// Holds [`CWD_MUTEX`] and restores the previous working directory (and a
/// clean `CYDRIVE_*` environment) on drop — including on panic.
struct CwdGuard {
    _lock: MutexGuard<'static, ()>,
    prev: PathBuf,
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        for key in ENV_KEYS {
            std::env::remove_var(key);
        }
        std::env::set_current_dir(&self.prev).expect("restore previous cwd");
    }
}

/// Locks [`CWD_MUTEX`], clears `CYDRIVE_*` overrides and moves the process
/// cwd into `dir` for the duration of the guard. Declare the guard *after*
/// the owning `TempDir` so the cwd is restored before the directory is
/// deleted (Windows refuses to remove the cwd).
fn chdir(dir: &Path) -> CwdGuard {
    let lock = CWD_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for key in ENV_KEYS {
        std::env::remove_var(key);
    }
    let prev = std::env::current_dir().expect("current dir");
    std::env::set_current_dir(dir).expect("chdir into temp dir");
    CwdGuard { _lock: lock, prev }
}

/// A test config anchored in `dir`: ephemeral WebDAV port, temp DB and
/// cache, plus a fully "configured" token/chat pair.
fn temp_config(dir: &Path, webdav_port: u16) -> CyDriveConfig {
    CyDriveConfig {
        bot_token: "123456:ABC-DEF".to_string(),
        chat_id: 123456789,
        db_path: dir.join("meta.db").to_string_lossy().into_owned(),
        cache_path: dir.join("cache").to_string_lossy().into_owned(),
        webdav_port,
        ..CyDriveConfig::default()
    }
}

/// A pre-connected mock transport (the seam the production `run`
/// substitutes `GrammersTransport` for).
async fn mock_transport() -> Arc<MockTransport> {
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect mock transport");
    mock
}

/// Boots the stack from `cfg` with `mock` injected, panicking on failure.
async fn boot(cfg: &CyDriveConfig, mock: Arc<MockTransport>) -> RunHandle {
    let transport: Arc<dyn CloudTransport> = mock;
    run_with_transport(cfg, transport)
        .await
        .expect("boot the stack")
}

/// Sends one raw HTTP/1.1 request (`Connection: close`) and reads the
/// response bytes until the server closes the connection.
async fn send(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect to server");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("send request");
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .expect("read response to EOF");
    String::from_utf8_lossy(&raw).into_owned()
}

/// Builds an HTTP/1.1 request with Host, Connection: close and an exact
/// Content-Length for `body`.
fn request(
    method: &str,
    target: &str,
    addr: SocketAddr,
    extra: &[(&str, &str)],
    body: &str,
) -> String {
    let mut req = format!(
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
        addr.port()
    );
    for (name, value) in extra {
        req.push_str(name);
        req.push_str(": ");
        req.push_str(value);
        req.push_str("\r\n");
    }
    req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    req.push_str("\r\n");
    req.push_str(body);
    req
}

/// Parses the numeric status code off the status line.
fn status_of(resp: &str) -> u16 {
    let line = resp.lines().next().expect("status line");
    line.split_whitespace()
        .nth(1)
        .expect("status code token")
        .parse()
        .expect("numeric status code")
}

// -------------------------------------------------------------- scenarios ---

/// 1. Boot serves the root collection (207 on PROPFIND) and shutdown stops
///    the listener: new connections are refused afterwards.
#[tokio::test]
async fn boots_serves_propfind_and_stops_gracefully() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = temp_config(dir.path(), 0);
    let mock = mock_transport().await;
    let handle = boot(&cfg, mock).await;
    let addr = handle.local_addr();
    assert_ne!(addr.port(), 0, ":0 must resolve to the real bound port");

    let resp = send(addr, &request("PROPFIND", "/", addr, &[("Depth", "1")], "")).await;
    assert_eq!(status_of(&resp), 207, "root multistatus: {resp}");

    handle.shutdown().await;
    let refused = TcpStream::connect(addr).await;
    assert!(
        refused.is_err(),
        "new connections must be refused after shutdown"
    );
}

/// 2. PUT round-trips the whole pipeline — HTTP → FS adapter → upload
///    queue → Mock remote — and the shutdown drain marks the row uploaded.
#[tokio::test]
async fn put_roundtrips_and_drains_on_shutdown() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = temp_config(dir.path(), 0);
    let mock = mock_transport().await;
    let handle = boot(&cfg, mock.clone()).await;
    let addr = handle.local_addr();

    let resp = send(
        addr,
        &request("PUT", "/file.txt", addr, &[], "roundtrip body"),
    )
    .await;
    assert_eq!(status_of(&resp), 201, "PUT created: {resp}");

    handle.shutdown().await; // drains the upload queue

    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("reopen db");
    let row = db
        .get_file("/file.txt")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "drain marked the row uploaded");
    assert_eq!(row.size, "roundtrip body".len() as i64);
    assert!(
        !mock.upload_calls().is_empty(),
        "the mock remote saw the upload job"
    );
}

/// 3. A crash-staged pending row (row + cache copy, no upload) re-enters
///    the queue at boot and drains to uploaded on shutdown.
#[tokio::test]
async fn pending_rows_requeued_at_boot() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache_root = dir.path().join("cache");
    std::fs::create_dir_all(&cache_root).expect("create cache root");
    let db_path = dir.path().join("meta.db");

    {
        let db = MetaDatabase::open(&db_path).expect("open db to seed");
        db.upsert_file(&FileUpsert {
            rel_path: "/resume.txt".to_string(),
            name: "resume.txt".to_string(),
            parent_dir: "/".to_string(),
            size: 5,
            mtime: 1_700_000_000.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: None,
            is_uploaded: false,
            is_cached: true,
            is_encrypted: false,
            chunk_count: 1,
            mime_type: None,
        })
        .expect("seed pending row");
    }
    std::fs::write(cache_root.join("resume.txt"), b"hello").expect("seed cache copy");

    let cfg = temp_config(dir.path(), 0);
    let mock = mock_transport().await;
    let handle = boot(&cfg, mock.clone()).await;
    handle.shutdown().await; // requeue happened at boot; drain finishes it

    let db = MetaDatabase::open(&db_path).expect("reopen db");
    let row = db
        .get_file("/resume.txt")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "requeued job drained to uploaded");
    assert!(
        !mock.upload_calls().is_empty(),
        "the requeued job reached the mock remote"
    );
}

/// 4. With both files present, discovery prefers `config.toml` over a
///    legacy `config.json`.
#[test]
fn discover_config_prefers_toml_over_legacy_json() {
    let dir = tempfile::tempdir().expect("temp dir");
    let _cwd = chdir(dir.path());
    temp_config(dir.path(), 1111)
        .save_toml(&dir.path().join("config.toml"))
        .expect("save config.toml");
    std::fs::write(
        dir.path().join("config.json"),
        r#"{ "bot_token": "999:zzz", "chat_id": 42, "webdav_port": 2222 }"#,
    )
    .expect("save legacy config.json");

    let cfg = discover_config().expect("discovery succeeds");
    assert_eq!(cfg.webdav_port, 1111, "config.toml wins over legacy JSON");
    assert_eq!(cfg.bot_token, "123456:ABC-DEF", "values come from the toml");
}

/// 5. Discovery in an empty directory fails with an actionable error.
#[test]
fn discover_config_missing_is_actionable_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let _cwd = chdir(dir.path());

    let err = discover_config().expect_err("empty cwd must fail");
    assert!(
        err.to_string().to_lowercase().contains("config"),
        "error message mentions config: {err}"
    );
}

/// 6. A legacy Python `config.json` alone is discovered and loads with
///    its token/chat pair intact (discovery/load layer only — no server).
#[test]
fn legacy_json_config_boots() {
    let dir = tempfile::tempdir().expect("temp dir");
    let _cwd = chdir(dir.path());
    std::fs::write(
        dir.path().join("config.json"),
        r#"{ "bot_token": "123456:ABC-DEF", "chat_id": 123456789 }"#,
    )
    .expect("save legacy config.json");

    let cfg = discover_config().expect("legacy config.json discovered");
    assert_eq!(cfg.bot_token, "123456:ABC-DEF");
    assert_eq!(cfg.chat_id, 123456789);
    assert!(cfg.is_configured(), "token + chat pair is configured");
}
