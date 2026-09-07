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
//! 6. a legacy `config.json` alone is discovered and loads;
//! 7. with `auto_mount_drive` off, the handle reports no mounted letter.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use cloudkit_cli::control::{control_file_path, read_control_addr, send_stop};
use cloudkit_cli::{discover_config, run_with_transport, vfs_config, RunHandle, ShutdownWatch};
use cloudkit_core::config::CyDriveConfig;
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::CloudTransport;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

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
/// cache, plus a fully "configured" token/chat pair. Auto-mount stays
/// OFF so the offline gate never maps a real network drive, and the web
/// dashboard stays OFF too: the config *default* is `true` (Python
/// parity) but these boots would all contend on the fixed 8088 port
/// when the test binary runs them in parallel.
fn temp_config(dir: &Path, webdav_port: u16) -> CyDriveConfig {
    CyDriveConfig {
        bot_token: "123456:ABC-DEF".to_string(),
        chat_id: 123456789,
        db_path: dir.join("meta.db").to_string_lossy().into_owned(),
        cache_path: dir.join("cache").to_string_lossy().into_owned(),
        webdav_port,
        auto_mount_drive: false,
        enable_web_ui: false,
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

/// 7. With `auto_mount_drive` off (every offline test config), the run
///    handle reports no mounted letter — the offline gate never maps a
///    real network drive — and no mounted point (the Unix claim, status
///    plan C5), and shutdown skips the unmount path.
#[tokio::test]
async fn auto_mount_disabled_leaves_mounted_letter_none() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = temp_config(dir.path(), 0);
    let mock = mock_transport().await;
    let handle = boot(&cfg, mock).await;
    assert_eq!(handle.mounted_letter, None);
    assert_eq!(handle.mounted_point, None);
    handle.shutdown().await;
}

/// 9. With `enable_web_ui` on and an ephemeral `web_ui_port = 0` (same
///    `:0` semantics as the WebDAV port — validation of non-zero ports
///    is the config layer's concern), the boot also serves the
///    dashboard: GET / answers the real index.html and the listener
///    closes on shutdown.
#[tokio::test]
async fn web_ui_enabled_serves_dashboard() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut cfg = temp_config(dir.path(), 0);
    cfg.enable_web_ui = true;
    cfg.web_ui_port = 0;
    let mock = mock_transport().await;
    let handle = boot(&cfg, mock).await;

    let addr = handle.web_ui_local_addr().expect("web ui bound");
    assert_ne!(addr.port(), 0, ":0 must resolve to the real bound port");

    let resp = send(addr, &request("GET", "/", addr, &[], "")).await;
    assert_eq!(status_of(&resp), 200, "dashboard served: {resp}");
    assert!(
        resp.contains("<title>CyDrive"),
        "real index.html, not a stub: {resp}"
    );

    handle.shutdown().await;
    let refused = TcpStream::connect(addr).await;
    assert!(
        refused.is_err(),
        "web ui listener must refuse connections after shutdown"
    );
}

/// 8. `vfs_config` applies the Python AND semantics
///    (`enable_encryption && encryption_password`, telegram_client.py:167):
///    a configured password reaches the VFS only while the flag is on; the
///    flag off means plaintext uploads even with a password in the config.
#[test]
fn vfs_config_respects_enable_encryption_flag() {
    let mut cfg = temp_config(Path::new("."), 0);
    cfg.encryption_password = Some("x".to_string());

    cfg.enable_encryption = false;
    assert_eq!(
        vfs_config(&cfg).encryption_password,
        None,
        "flag off: the password must not reach the VFS"
    );

    cfg.enable_encryption = true;
    assert_eq!(
        vfs_config(&cfg).encryption_password,
        Some("x".to_string()),
        "flag on: the configured password reaches the VFS"
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

// ------------------------------------------ task 2 (plan C3): shutdown wiring ---

/// The unified shutdown latch (service-lifecycle plan, contract C3) is
/// multi-trigger idempotent: untriggered it blocks, and once triggered —
/// no matter how many times — every `wait()` resolves immediately. This
/// is the seam the three shutdown sources (Ctrl+C / SIGTERM / the
/// control-channel STOP) funnel through, so a second trigger arriving
/// while the graceful drain is already running must never wedge the exit.
#[tokio::test]
async fn shutdown_watch_multi_trigger_idempotent() {
    let watch = Arc::new(ShutdownWatch::new());

    // Untriggered the latch must hold `wait` back (a short window, not a
    // hang: 100ms is orders of magnitude above a spurious readiness).
    let untriggered = timeout(Duration::from_millis(100), watch.wait()).await;
    assert!(
        untriggered.is_err(),
        "wait() must block until the latch is triggered"
    );

    // A waiter racing the triggers must resolve under either ordering
    // (wait-then-trigger and trigger-then-wait): the triggered state
    // wakes every current and future waiter.
    let waiter = {
        let watch = Arc::clone(&watch);
        tokio::spawn(async move { watch.wait().await })
    };
    watch.trigger();
    watch.trigger(); // the second trigger must be a harmless no-op
    timeout(Duration::from_secs(1), waiter)
        .await
        .expect("the waiting task resolves after the triggers")
        .expect("the waiting task joins cleanly");

    // And a *later* wait still returns at once — the latch stays set.
    timeout(Duration::from_secs(1), watch.wait())
        .await
        .expect("wait() after a trigger resolves immediately (idempotent latch)");
}

/// A running instance stops via the control channel (contract C3):
/// `run_with_transport` binds the loopback control server and writes the
/// port file next to the db (observable the moment boot returns), a STOP
/// answers `OK: shutting down`, and the STOP alone — this test never
/// calls `handle.shutdown()` — drives the full graceful stop: within the
/// deadline the WebDAV listener refuses new connections and the stop
/// chain has removed the port file. `RunHandle` exposes no
/// completion future, so those two observable terminal states are the
/// verdict; the handle stays alive until both are seen (dropping it
/// early could take the listener down and fake the refusal).
#[tokio::test]
async fn run_instance_stops_via_control_channel() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = temp_config(dir.path(), 0);
    let mock = mock_transport().await;
    let handle = boot(&cfg, mock).await;
    let addr = handle.local_addr();
    assert_ne!(addr.port(), 0, ":0 must resolve to the real bound port");

    // The control channel is part of the booted stack: the port file
    // exists next to the db and parses back into an address.
    let control_file = control_file_path(&cfg);
    assert!(
        control_file.exists(),
        "run_with_transport must write the control file {} next to the db",
        control_file.display()
    );
    let control_addr = read_control_addr(&cfg).expect("the control file parses into an address");

    let reply = send_stop(control_addr)
        .await
        .expect("STOP over the control channel");
    assert!(
        reply.contains("OK: shutting down"),
        "control reply: {reply}"
    );

    // Poll the two terminal states; 15s is generous against the graceful
    // drain (this boot has no in-flight uploads to wait for).
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let webdav_refused = TcpStream::connect(addr).await.is_err();
        let control_file_removed = !control_file.exists();
        if webdav_refused && control_file_removed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "instance did not stop within 15s of STOP (webdav refused: {webdav_refused}, \
             control file removed: {control_file_removed})"
        );
        sleep(Duration::from_millis(100)).await;
    }

    // The stop chain removes the port file (C3: the deletion sits in the
    // RunHandle shutdown sequence, just before the drive unmount).
    assert!(
        !control_file.exists(),
        "the shutdown must remove the control file {}",
        control_file.display()
    );

    drop(handle); // STOP is the whole exit; never call shutdown() here
}

// --------------------------------------- Batch R R-5: capability banner ---

/// A `MakeWriter` that appends formatted log lines into a shared buffer,
/// so a test can boot the stack under a capturing subscriber and assert
/// on the boot banner (tracing's `set_default` guard is thread-local;
/// `#[tokio::test]`'s default current-thread runtime keeps every spawned
/// task — inbound worker, accept loop — on this same thread, so the
/// whole boot logs through the capture).
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for &LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut sink = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sink.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = &'a LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self
    }
}

/// R-5: boot declares the transport's capability bits once, next to the
/// WebDAV banner (interfaces §1 / logging §2: one-shot lifecycle info;
/// consumer degrade warnings elsewhere — inbound worker not started, bot
/// commands dropped — point back at this line for diagnosis). The mock
/// declares exactly RANGE_READ / INBOUND / CHAT (transport_traits test
/// 2), so the banner must show those three on and the storage-side bits
/// off.
#[tokio::test]
async fn boot_declares_transport_capabilities() {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(LogBuffer(Arc::clone(&buffer)))
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _capture = tracing::subscriber::set_default(subscriber);

    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = temp_config(dir.path(), 0);
    let mock = mock_transport().await;
    let handle = boot(&cfg, mock).await;

    let logs = {
        let sink = buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        String::from_utf8_lossy(&sink).into_owned()
    };
    assert!(
        logs.contains("transport capabilities"),
        "the one-line capability declaration is present at boot: {logs}"
    );
    assert!(
        logs.contains("range_read=true")
            && logs.contains("inbound=true")
            && logs.contains("chat=true"),
        "the mock's three declared bits show ON: {logs}"
    );
    assert!(
        logs.contains("resume=false"),
        "undeclared bits show OFF (the full nine-bit line): {logs}"
    );

    handle.shutdown().await;
}

// ---------------------------------------------- task 3 (plan C3): sigterm ---

/// The unix SIGTERM helper resolves on a *real* signal (plan task 3's
/// verification strategy: injecting signals into the test process is
/// flaky as an always-on gate, so this lives behind `#[ignore]` and runs
/// explicitly on unix — the WSL step of the plan). A `kill -TERM` aimed
/// at this very process must make the `sigterm()` future resolve within
/// the deadline; on Windows the helper compiles to a permanently pending
/// stub and this test does not exist (`#[cfg(unix)]`).
#[cfg(unix)]
#[ignore = "sends a real SIGTERM to the test process; run explicitly on unix: cargo test -- --ignored"]
#[tokio::test]
async fn sigterm_future_resolves_on_real_signal() {
    let waiter = tokio::spawn(async { cloudkit_cli::sigterm().await });
    sleep(Duration::from_millis(200)).await;
    let status = std::process::Command::new("kill")
        .args(["-TERM", &std::process::id().to_string()])
        .status()
        .expect("kill -TERM self");
    assert!(status.success());
    timeout(Duration::from_secs(5), waiter)
        .await
        .expect("sigterm future resolved within 5s")
        .expect("join ok")
        .expect("sigterm ok");
}
