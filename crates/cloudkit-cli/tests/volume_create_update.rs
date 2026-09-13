//! RED-phase tests for the web volume-management P3/P4 batches (plan
//! `docs/plans/2026-09-13-web-volume-management.md` §1.2/§3): the
//! control channel's `CREATE <name> <json>` command (controlled toml
//! generation validated BEFORE the disk write, then the same runtime ADD
//! the control channel already runs) and — the P4 batch —
//! `UPDATE <name> <json>` (the write-only credential overlay, the
//! file-first rewrite, and the REMOVE+ADD re-assembly with its
//! mixed-state replies).
//!
//! Credential discipline under test throughout: the payload MAY carry
//! credentials (a loopback channel, the same trust face as hand-editing
//! the volume file), but their VALUES never ride a reply or a log line
//! — refusals name keys, not values.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cloudkit_cli::control::read_control_addr;
use cloudkit_cli::{
    run_multi_with_transports_and_commands, MountedBackend, MountedVolume, MultiVolumeHandle,
    RunOptions, RuntimeMount, RuntimeVolumeCommands, VolumeMounts, VolumeTransportDispatch,
};
use cloudkit_core::config::{CyDriveConfig, VolumeConfig};
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::CloudTransport;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::timeout;

// ------------------------------------------------------------- helpers ---

/// Serialises every test that changes the process-wide working directory
/// (the `runtime_volumes.rs` convention).
static CWD_MUTEX: Mutex<()> = Mutex::new(());

/// Holds [`CWD_MUTEX`] and restores the previous working directory on
/// drop — including on panic.
struct CwdGuard {
    _lock: MutexGuard<'static, ()>,
    prev: PathBuf,
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.prev).expect("restore previous cwd");
    }
}

/// Locks [`CWD_MUTEX`] and moves the process cwd into `dir` for the
/// duration of the guard. Declare the guard *after* the owning `TempDir`.
fn chdir(dir: &Path) -> CwdGuard {
    let lock = CWD_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let prev = std::env::current_dir().expect("current dir");
    std::env::set_current_dir(dir).expect("chdir into temp dir");
    CwdGuard { _lock: lock, prev }
}

/// Writes `text` to `path`, creating missing parent directories.
fn write_file(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(path, text).expect("write file");
}

/// The process-level config for multi-volume mode: ephemeral ports, no
/// dashboard, no auto-mount (mount tests flip it on and stub the pass).
fn process_config() -> CyDriveConfig {
    CyDriveConfig {
        volumes_dir: Some("volumes".to_string()),
        webdav_port: 0,
        enable_web_ui: false,
        web_ui_port: 0,
        auto_mount_drive: false,
        ..CyDriveConfig::default()
    }
}

/// [`process_config`] with auto-mount ON (the CREATE mount arm; the
/// mount itself is stubbed, no real drive is touched).
fn mount_process_config() -> CyDriveConfig {
    CyDriveConfig {
        auto_mount_drive: true,
        ..process_config()
    }
}

/// A telegram-flavoured volume file body (the injected mock replaces the
/// real connect; the keys keep the file a legal volume shape).
fn volume_toml(token: &str, chat_id: i64) -> String {
    format!("backend = \"telegram\"\nbot_token = \"{token}\"\nchat_id = {chat_id}\n")
}

/// A pre-connected mock transport.
async fn mock_transport() -> Arc<MockTransport> {
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect mock transport");
    mock
}

/// The runtime-ADD dispatch seam under test: every dispatched volume
/// gets a fresh healthy mock.
fn mock_dispatch() -> VolumeTransportDispatch {
    Arc::new(move |_spec: &VolumeConfig| {
        Box::pin(async {
            let mock = Arc::new(MockTransport::new());
            mock.connect().await.expect("pre-connect mock transport");
            Ok(Some((
                RunOptions::default(),
                mock as Arc<dyn CloudTransport>,
            )))
        })
    })
}

/// A dispatch whose connects always fail — the "wrong credentials"
/// stand-in (CREATE's file-saved-but-assembly-failed path).
fn failing_dispatch(reason: &str) -> VolumeTransportDispatch {
    let reason = reason.to_string();
    Arc::new(move |spec: &VolumeConfig| {
        let _ = spec.name; // force the HRTB closure shape (see mock_dispatch)
        let reason = reason.clone();
        Box::pin(async move { Err(anyhow::anyhow!(reason)) })
    })
}

/// The boot over the volumes on disk plus the command surface.
async fn boot(
    cfg: CyDriveConfig,
    commands: RuntimeVolumeCommands,
) -> (MultiVolumeHandle, SocketAddr) {
    let specs =
        cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
    let mut mocks = Vec::new();
    for _ in &specs {
        mocks.push(mock_transport().await);
    }
    let injections = specs
        .into_iter()
        .zip(mocks)
        .map(|(spec, mock)| (spec, RunOptions::default(), mock as Arc<dyn CloudTransport>))
        .collect();
    let handle = run_multi_with_transports_and_commands(&cfg, injections, commands)
        .await
        .expect("multi-volume boot with runtime commands");
    let addr = read_control_addr(&process_config()).expect("control address");
    (handle, addr)
}

/// Sends one control-channel line and returns the reply (read to EOF).
async fn send_cmd(addr: SocketAddr, line: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect to control");
    stream
        .write_all(format!("{line}\n").as_bytes())
        .await
        .expect("send control line");
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .expect("read control reply");
    String::from_utf8_lossy(&raw).into_owned()
}

/// The instant mount stub (the `runtime_volumes.rs` shape): every claim
/// mounts immediately as a WebDAV-mapped letter.
fn instant_mount_stub() -> RuntimeMount {
    Arc::new(move |name: &str, letter: &str| {
        let volume = name.to_string();
        let letter = format!("{}:", letter.trim_end_matches(':').to_ascii_uppercase());
        Box::pin(async move {
            VolumeMounts {
                mounted: vec![MountedVolume {
                    volume,
                    letter,
                    backend: MountedBackend::WebDav,
                }],
                winfsp: Default::default(),
            }
        })
    })
}

/// The LIST reply's data rows (everything after the header line).
fn list_rows(reply: &str) -> Vec<String> {
    reply
        .lines()
        .skip(1)
        .map(|line| line.trim().to_owned())
        .collect()
}

// --------------------------------------------------- P3: CREATE (§1.2) ---

/// `CREATE <name> <json>`: the controlled toml generation lands on disk
/// with EXACTLY the payload's explicit keys, validates before the write
/// (a refusal never creates a file), then assembles through the same
/// runtime ADD path — mount claim included — and the reply carries the
/// mount state.
#[tokio::test]
async fn create_writes_the_file_assembles_and_mounts() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:AAA", 111111),
    );
    let _guard = chdir(dir.path());

    let (handle, addr) = boot(
        mount_process_config(),
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            mount: Some(instant_mount_stub()),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;

    let reply = send_cmd(
        addr,
        "CREATE fresh {\"backend\":\"telegram\",\"bot_token\":\"222:BBB\",\"chat_id\":222222,\"drive_letter\":\"Q\"}",
    )
    .await;
    assert!(
        reply.starts_with("OK:") && reply.contains("created volume `fresh`"),
        "the CREATE acknowledges and names the volume: {reply}"
    );
    assert!(
        reply.contains("mounted Q:"),
        "the reply carries the mount state (the ADD leg's own text): {reply}"
    );

    // The file on disk: exactly the payload's keys, no paved defaults.
    let file = dir.path().join("volumes").join("fresh.toml");
    let text = fs::read_to_string(&file).expect("the volume file was written");
    assert!(text.contains("backend = \"telegram\""), "backend: {text}");
    assert!(text.contains("bot_token = \"222:BBB\""), "token: {text}");
    assert!(text.contains("chat_id = 222222"), "chat_id: {text}");
    assert!(text.contains("drive_letter = \"Q\""), "letter: {text}");
    assert!(
        !text.contains("cache_path") && !text.contains("chunk_size_mb"),
        "no default keys paved in: {text}"
    );

    // The runtime: registered and running everywhere at once.
    assert_eq!(
        handle
            .volume("fresh")
            .expect("the volume registered")
            .status(),
        cloudkit_cli::VolumeStatus::Running,
        "the created volume is running"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        list_rows(&reply)
            .iter()
            .any(|row| row.starts_with("fresh running Q: ")),
        "LIST sees the created volume with its mount (the letter column): {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// Every CREATE refusal that must leave the disk untouched — the
/// name rules, the already-existing file (with its UPDATE pointer),
/// the payload key space (unknown keys, process-level keys), the
/// payload types, the malformed JSON, the usage shapes — and the
/// cross-field validate() gate (the baidu credential group), which all
/// run BEFORE `fs::write`.
#[tokio::test]
async fn create_refusals_write_no_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:AAA", 111111),
    );
    let _guard = chdir(dir.path());

    let (handle, addr) = boot(
        process_config(),
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;

    // The name rules refuse before anything else.
    let reply = send_cmd(
        addr,
        "CREATE Bad-Name {\"backend\":\"local\",\"local_root\":\"C:/x\"}",
    )
    .await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("invalid volume name"),
        "the name rules refuse: {reply}"
    );
    assert!(
        !dir.path().join("volumes").join("Bad-Name.toml").exists(),
        "no file for the refused name"
    );

    // An existing volume file points at UPDATE instead.
    let reply = send_cmd(
        addr,
        "CREATE a {\"backend\":\"telegram\",\"bot_token\":\"9:9\",\"chat_id\":9}",
    )
    .await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("already exists"),
        "the existing file refuses: {reply}"
    );
    assert!(
        reply.contains("UPDATE"),
        "the refusal points at UPDATE: {reply}"
    );
    let text = fs::read_to_string(dir.path().join("volumes").join("a.toml"))
        .expect("read the existing file");
    assert!(
        text.contains("bot_token = \"111:AAA\""),
        "the existing file is untouched: {text}"
    );

    // The payload key space: an unknown key and a process-level key.
    for (payload, marker) in [
        (
            "{\"backend\":\"local\",\"local_root\":\"C:/x\",\"no_such_key\":1}",
            "no_such_key",
        ),
        (
            "{\"backend\":\"local\",\"local_root\":\"C:/x\",\"web_ui_port\":9}",
            "config.toml",
        ),
    ] {
        let reply = send_cmd(addr, &format!("CREATE ghost {payload}")).await;
        assert!(
            reply.starts_with("ERR:") && reply.contains(marker),
            "the key-space refusal ({marker}): {reply}"
        );
        assert!(
            !dir.path().join("volumes").join("ghost.toml").exists(),
            "the refused payload wrote no file ({marker})"
        );
    }

    // A wrong-typed payload value: the refusal names the key, never the
    // value.
    let reply = send_cmd(addr, "CREATE ghost {\"chat_id\":{\"nested\":1}}").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("chat_id"),
        "the type refusal names the key: {reply}"
    );
    assert!(
        !dir.path().join("volumes").join("ghost.toml").exists(),
        "the type refusal wrote no file"
    );

    // Malformed JSON and the usage shapes.
    let reply = send_cmd(addr, "CREATE ghost {not json").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("JSON"),
        "malformed JSON is refused actionably: {reply}"
    );
    for line in ["CREATE", "CREATE ghost"] {
        let reply = send_cmd(addr, line).await;
        assert!(
            reply.starts_with("ERR:") && reply.contains("usage"),
            "{line} gets the usage line: {reply}"
        );
        assert!(
            reply.contains("<json>"),
            "the usage line teaches the payload shape: {reply}"
        );
    }

    // The cross-field validate gate: baidu without its credential group
    // is refused BEFORE the write (assert the file never appeared).
    let reply = send_cmd(
        addr,
        "CREATE ghost {\"backend\":\"baidu\",\"baidu_app_key\":\"k\"}",
    )
    .await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("baidu_app_secret"),
        "the validate refusal names the missing credential: {reply}"
    );
    assert!(
        reply.contains("nothing was written"),
        "the refusal says the disk is untouched: {reply}"
    );
    assert!(
        !dir.path().join("volumes").join("ghost.toml").exists(),
        "the validate refusal wrote no file"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// A CREATE whose ASSEMBLY fails (the wrong-credentials shape): the file
/// was already written and STAYS (the persistent file is the source of
/// truth — K49's philosophy extended: a written file with a failed
/// runtime is a retry away), and the refusal says exactly that, naming
/// the ENABLE retry.
#[tokio::test]
async fn create_with_failed_assembly_keeps_the_file_and_guides_the_retry() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:AAA", 111111),
    );
    let _guard = chdir(dir.path());

    let (handle, addr) = boot(
        process_config(),
        RuntimeVolumeCommands {
            dispatch: Some(failing_dispatch("mock connect refused")),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;

    let reply = send_cmd(
        addr,
        "CREATE ghost {\"backend\":\"telegram\",\"bot_token\":\"222:BBB\",\"chat_id\":222222}",
    )
    .await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("was saved"),
        "the refusal acknowledges the written file: {reply}"
    );
    assert!(
        reply.contains("assembling the volume failed"),
        "the refusal carries the assembly reason: {reply}"
    );
    assert!(
        reply.contains("ENABLE ghost"),
        "the refusal names the retry command: {reply}"
    );
    let file = dir.path().join("volumes").join("ghost.toml");
    let text = fs::read_to_string(&file).expect("the file survived the failed assembly");
    assert!(
        text.contains("bot_token = \"222:BBB\""),
        "the payload's fields are what the file keeps: {text}"
    );
    assert!(
        handle.volume("ghost").is_none(),
        "nothing registered for the failed assembly"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// CREATE joins the mutating family on the shutdown gate (H1): after a
/// real STOP the command is refused — a shutting-down instance accepts
/// no new volumes (files included).
#[tokio::test]
async fn create_is_refused_after_the_stop_gate_fires() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:AAA", 111111),
    );
    let _guard = chdir(dir.path());

    let (handle, addr) = boot(
        process_config(),
        RuntimeVolumeCommands {
            dispatch: Some(mock_dispatch()),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;

    let reply = send_cmd(addr, "STOP").await;
    assert!(reply.starts_with("OK:"), "STOP acknowledges: {reply}");
    let reply = send_cmd(
        addr,
        "CREATE late {\"backend\":\"local\",\"local_root\":\"C:/x\"}",
    )
    .await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("shutting down"),
        "CREATE after the gate is refused with the shutdown reason: {reply}"
    );
    assert!(
        !dir.path().join("volumes").join("late.toml").exists(),
        "the refused CREATE wrote no file"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

// --------------------------------------------- credential non-leak (M3) ---

/// Process-wide log sink (the `run_e2e.rs` pattern): the capture
/// installs the test binary's ONLY subscriber via `set_global_default`
/// — a thread-local `set_default` loses the parallel-test Interest
/// race, the one global default covers every thread uniformly.
static LOGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// A `MakeWriter` draining into [`LOGS`].
struct LogBuffer;

impl std::io::Write for &LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut sink = LOGS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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

/// The CREATE payload carries credentials; neither the reply nor the
/// log may carry their VALUES: the refusal texts name keys (the M3
/// discipline) and the tracing line logs the field NAME list only. Both
/// a failed and a successful CREATE run against the capture (the
/// dispatch refuses `leak1` by name and serves `leak2`).
#[tokio::test]
async fn create_never_leaks_credential_values_into_replies_or_logs() {
    let subscriber = tracing_subscriber::fmt()
        .with_writer(LogBuffer)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _installed = tracing::subscriber::set_global_default(subscriber);

    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:AAA", 111111),
    );
    let _guard = chdir(dir.path());

    // A dispatch that refuses `leak1` by name and serves everyone else:
    // both CREATE legs run in one boot against one capture.
    let dispatch: VolumeTransportDispatch = {
        Arc::new(move |spec: &VolumeConfig| {
            let fail = spec.name == "leak1";
            Box::pin(async move {
                if fail {
                    return Err(anyhow::anyhow!("mock connect refused"));
                }
                let mock = Arc::new(MockTransport::new());
                mock.connect().await.expect("pre-connect mock transport");
                Ok(Some((
                    RunOptions::default(),
                    mock as Arc<dyn CloudTransport>,
                )))
            })
        })
    };
    let (handle, addr) = boot(
        process_config(),
        RuntimeVolumeCommands {
            dispatch: Some(dispatch),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;

    // The failed-assembly leg (the refusal-heavy path).
    let failed = send_cmd(
        addr,
        "CREATE leak1 {\"backend\":\"telegram\",\"bot_token\":\"111:FAKE-CREATE-MARKER\",\"chat_id\":4242}",
    )
    .await;
    assert!(failed.starts_with("ERR:"), "the failing leg ran: {failed}");
    assert!(
        !failed.contains("FAKE-CREATE-MARKER"),
        "the refusal never echoes the credential value: {failed}"
    );

    // The successful leg (the file write + the ADD path's own logging
    // run against the same capture).
    let succeeded = send_cmd(
        addr,
        "CREATE leak2 {\"backend\":\"telegram\",\"bot_token\":\"222:FAKE-CREATE-MARKER\",\"chat_id\":4242}",
    )
    .await;
    assert!(
        succeeded.starts_with("OK:"),
        "the succeeding leg ran: {succeeded}"
    );
    assert!(
        !succeeded.contains("FAKE-CREATE-MARKER"),
        "the OK reply carries no credential value either: {succeeded}"
    );

    let logs = {
        let sink = LOGS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        String::from_utf8_lossy(&sink).into_owned()
    };
    assert!(
        !logs.contains("FAKE-CREATE-MARKER"),
        "no log line carries the credential value: {logs}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}
