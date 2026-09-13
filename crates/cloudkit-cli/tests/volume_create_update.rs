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
    RemoveTuning, RunOptions, RuntimeMount, RuntimeVolumeCommands, VolumeMounts,
    VolumeTransportDispatch,
};
use cloudkit_core::config::{CyDriveConfig, VolumeConfig};
use cloudkit_core::transport::mock::{MockTransport, UploadAction};
use cloudkit_core::transport::CloudTransport;
use cloudkit_storage::StorageError;
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

// --------------------------------------------------- P4: UPDATE (§1.2) ---

/// The happy UPDATE: a running volume's non-credential fields are
/// rewritten through the controlled overlay (the old credential SURVIVES
/// a payload that does not name it — the write-only rule's flip side),
/// the volume re-assembles through REMOVE+ADD, and the reply carries
/// the re-assembly state plus the comments-loss note (裁决①: accepted
/// and surfaced).
#[tokio::test]
async fn update_rewrites_fields_reassembles_and_keeps_unnamed_credentials() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:FAKE-KEEP-ME", 111111),
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

    let reply = send_cmd(addr, "UPDATE a {\"chat_id\":999999,\"drive_letter\":\"Q\"}").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("updated volume `a`"),
        "the UPDATE acknowledges and names the volume: {reply}"
    );
    assert!(
        reply.contains("re-assembled"),
        "the reply carries the re-assembly state: {reply}"
    );
    assert!(
        reply.to_lowercase().contains("comment"),
        "the reply surfaces the comments loss (裁决①): {reply}"
    );

    // The file: the overlaid fields changed, the unnamed credential
    // survived, the new key appended.
    let file = fs::read_to_string(dir.path().join("volumes").join("a.toml"))
        .expect("read the rewritten file");
    assert!(
        file.contains("chat_id = 999999"),
        "the overlay landed: {file}"
    );
    assert!(
        file.contains("drive_letter = \"Q\""),
        "the new key appended: {file}"
    );
    assert!(
        file.contains("bot_token = \"111:FAKE-KEEP-ME\""),
        "a credential the payload did not name survives verbatim: {file}"
    );

    // The runtime: re-assembled and running.
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        list_rows(&reply)
            .iter()
            .any(|row| row.starts_with("a running ")),
        "the volume runs on the updated settings: {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// The write-only credential rule (K49 分档), read back off the FILE
/// (never off a log): a present non-empty credential overwrites; an
/// EMPTY string overlays nothing (the form's leave-alone affordance) —
/// and a payload that sets nothing at all answers the actionable
/// no-op refusal without touching the file.
#[tokio::test]
async fn update_credentials_are_write_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:OLD", 111111),
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
    let file = dir.path().join("volumes").join("a.toml");

    // Present and non-empty: overwrite.
    let reply = send_cmd(addr, "UPDATE a {\"bot_token\":\"222:NEW\"}").await;
    assert!(
        reply.starts_with("OK:"),
        "the overwrite update lands: {reply}"
    );
    let text = fs::read_to_string(&file).expect("read the file");
    assert!(
        text.contains("bot_token = \"222:NEW\"") && !text.contains("111:OLD"),
        "the present non-empty credential overwrote: {text}"
    );

    // Empty string: keep (no key written, the old value intact).
    let reply = send_cmd(addr, "UPDATE a {\"bot_token\":\"\",\"chat_id\":777777}").await;
    assert!(
        reply.starts_with("OK:"),
        "the leave-alone update lands: {reply}"
    );
    let text = fs::read_to_string(&file).expect("read the file");
    assert!(
        text.contains("bot_token = \"222:NEW\""),
        "the empty credential kept the stored value: {text}"
    );
    assert!(
        text.contains("chat_id = 777777"),
        "the sibling overlay still landed: {text}"
    );

    // A payload that sets NOTHING (every key empty/absent) answers the
    // no-op refusal and never rewrites the file.
    let reply = send_cmd(addr, "UPDATE a {\"bot_token\":\"\"}").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("no fields"),
        "the empty overlay is refused actionably: {reply}"
    );
    let text = fs::read_to_string(&file).expect("read the file");
    assert!(
        text.contains("chat_id = 777777"),
        "the no-op refusal did not rewrite the file: {text}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// The `enabled` key rides UPDATE (coexisting with ENABLE/DISABLE): a
/// running volume updated to `enabled = false` unmounts through the
/// same K50 sequence (the DISABLE semantics), and a volume that is not
/// running answers the file-only OK with its ENABLE pointer — UPDATE
/// never auto-assembles what ENABLE owns.
#[tokio::test]
async fn update_flips_enabled_and_answers_the_stopped_volume() {
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
            remove_tuning: RemoveTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;

    // enabled = false on the RUNNING volume: file flips, volume comes
    // down (no re-add — the DISABLE twin).
    let reply = send_cmd(addr, "UPDATE a {\"enabled\":false,\"chat_id\":888888}").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("updated volume `a`"),
        "the disabling update acknowledges: {reply}"
    );
    let text =
        fs::read_to_string(dir.path().join("volumes").join("a.toml")).expect("read the file");
    assert!(
        text.contains("enabled = false") && text.contains("chat_id = 888888"),
        "the overlay + disable landed: {text}"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        reply.starts_with("OK: 0 volume(s)"),
        "the disabled volume came down: {reply}"
    );

    // The stopped volume: a further UPDATE is file-only, with the
    // ENABLE pointer.
    let reply = send_cmd(addr, "UPDATE a {\"chat_id\":123456}").await;
    assert!(
        reply.starts_with("OK:") && reply.contains("not running"),
        "the stopped volume answers the file-only OK: {reply}"
    );
    assert!(
        reply.contains("ENABLE a"),
        "the OK names the way up: {reply}"
    );
    let text =
        fs::read_to_string(dir.path().join("volumes").join("a.toml")).expect("read the file");
    assert!(
        text.contains("chat_id = 123456") && text.contains("enabled = false"),
        "the stopped update landed without touching the runtime: {text}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// The mixed state a drain refusal leaves (REMOVE step 1 of the
/// re-assembly): the FILE already carries the new settings while the
/// RUNNING volume still serves the old ones — the reply must say
/// exactly that, with the drain guidance, and never pretend the update
/// failed (the file IS the persistent truth; the runtime is stale and
/// the named retry resolves it).
#[tokio::test]
async fn update_drain_refusal_reports_the_mixed_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("config.toml"),
        "volumes_dir = \"volumes\"\n",
    );
    write_file(
        &dir.path().join("volumes").join("a.toml"),
        &volume_toml("111:AAA", 111111),
    );
    write_file(
        &dir.path().join("volumes").join("stuck.toml"),
        &volume_toml("222:BBB", 222222),
    );
    let _guard = chdir(dir.path());

    // The boot's own `a` is healthy; `stuck` joins through the hold
    // dispatch (its uploads park on RateLimited{3600s}).
    let specs = {
        let mut v =
            cloudkit_core::config::load_volumes(Path::new("volumes")).expect("load volume specs");
        v.retain(|spec| spec.name == "a");
        v
    };
    let hold_dispatch: VolumeTransportDispatch = Arc::new(move |spec: &VolumeConfig| {
        let stuck = spec.name == "stuck";
        Box::pin(async move {
            let mock = Arc::new(
                MockTransport::builder()
                    .upload_action(if stuck {
                        UploadAction::Fail {
                            error: StorageError::RateLimited {
                                retry_after: Some(Duration::from_secs(3600)),
                            },
                        }
                    } else {
                        UploadAction::Ok
                    })
                    .build(),
            );
            mock.connect().await.expect("connect mock");
            Ok(Some((
                RunOptions::default(),
                mock as Arc<dyn CloudTransport>,
            )))
        })
    });
    let injections = vec![(
        specs[0].clone(),
        RunOptions::default(),
        mock_transport().await as Arc<dyn CloudTransport>,
    )];
    let handle = run_multi_with_transports_and_commands(
        &process_config(),
        injections,
        RuntimeVolumeCommands {
            dispatch: Some(hold_dispatch),
            remove_tuning: RemoveTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await
    .expect("boot");
    let addr = read_control_addr(&process_config()).expect("control address");

    let reply = send_cmd(addr, "ADD stuck").await;
    assert!(reply.starts_with("OK:"), "stuck adds cleanly: {reply}");
    let source = dir.path().join("hold.txt");
    fs::write(&source, b"hold me in flight").expect("write source");
    let rel = cloudkit_core::rel_path::RelPath::new("/hold.txt").expect("valid rel path");
    handle
        .volume("stuck")
        .expect("volume stuck")
        .vfs()
        .expect("vfs stuck")
        .ingest_file(&rel, &source, 1.0)
        .await
        .expect("ingest into stuck");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let reply = send_cmd(addr, "UPDATE stuck {\"chat_id\":333333}").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("OLD configuration"),
        "the drain refusal reports the mixed state: {reply}"
    );
    assert!(
        reply.contains("REMOVE stuck") && reply.contains("ENABLE stuck"),
        "the refusal names the two-step retry: {reply}"
    );
    let text =
        fs::read_to_string(dir.path().join("volumes").join("stuck.toml")).expect("read the file");
    assert!(
        text.contains("chat_id = 333333"),
        "the FILE carries the new settings: {text}"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        reply.contains("stuck running"),
        "the RUNTIME still serves the volume (old settings, kept alive): {reply}"
    );

    // Deliberate leak (the remove_with_undrained_queue precedent): the
    // held worker sleeps 3600s; a drain-join would hang the test.
    drop(handle);
}

/// The unmounted mixed state (REMOVE ok, ADD fails — the re-add hit the
/// same broken dispatch a wrong credential would): the file is updated,
/// the volume is DOWN, and the refusal says so with its ENABLE retry
/// and the comments note.
#[tokio::test]
async fn update_whose_readd_fails_reports_the_unmounted_state() {
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
            remove_tuning: RemoveTuning::fast(),
            ..RuntimeVolumeCommands::default()
        },
    )
    .await;

    let reply = send_cmd(addr, "UPDATE a {\"chat_id\":777777}").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("UNMOUNTED"),
        "the failed re-add reports the unmounted state: {reply}"
    );
    assert!(
        reply.contains("ENABLE a"),
        "the refusal names the retry: {reply}"
    );
    let text =
        fs::read_to_string(dir.path().join("volumes").join("a.toml")).expect("read the file");
    assert!(
        text.contains("chat_id = 777777"),
        "the file carries the update: {text}"
    );
    let reply = send_cmd(addr, "LIST").await;
    assert!(
        reply.starts_with("OK: 0 volume(s)"),
        "the volume is down (removed, not re-added): {reply}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}

/// UPDATE joins the mutating family on the shutdown gate (H1): after a
/// real STOP the command is refused — the file included.
#[tokio::test]
async fn update_is_refused_after_the_stop_gate_fires() {
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
    let reply = send_cmd(addr, "UPDATE a {\"chat_id\":1}").await;
    assert!(
        reply.starts_with("ERR:") && reply.contains("shutting down"),
        "UPDATE after the gate is refused with the shutdown reason: {reply}"
    );
    let text =
        fs::read_to_string(dir.path().join("volumes").join("a.toml")).expect("read the file");
    assert!(
        text.contains("chat_id = 111111"),
        "the refused UPDATE rewrote nothing: {text}"
    );

    timeout(Duration::from_secs(30), handle.shutdown())
        .await
        .expect("shutdown completes")
        .expect("shutdown joins cleanly");
}
