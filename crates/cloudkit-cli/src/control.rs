//! The loopback control channel behind `cydrive stop` and `cydrive
//! status` (service-lifecycle plan contracts C1/C2; status plan C1).
//!
//! Every running instance binds a loopback-only listener on an ephemeral
//! port and drops a one-line port file (`127.0.0.1:<port>`) next to its
//! metadata db; the line protocol has exactly two commands — `STOP`
//! answers `OK: shutting down` and fires the shutdown callback, `PING`
//! answers `OK: cydrive <version>` and disturbs nothing (the payload
//! `cydrive status` shows on its instance row), anything else answers
//! `ERR: unknown command`.
//!
//! Security model (plan C1): the listener binds 127.0.0.1 only and
//! carries no authentication — an attacker who can already talk to the
//! machine's loopback can equally `taskkill` the process, so the channel
//! opens no new attack surface. The port file is a runtime artifact;
//! the run shutdown removes it (Task 2 wiring).

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use cloudkit_core::config::CyDriveConfig;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// The port-file name every running instance writes next to its metadata
/// db (one line `127.0.0.1:<ephemeral>`); `cydrive stop` resolves the
/// instance through it, so the subcommand must run in the same working
/// directory as `cydrive run`.
pub const CONTROL_FILE_NAME: &str = "cydrive.control";

/// [`send_stop`]'s connect budget: a dead port must fail fast instead of
/// hanging the `stop` subcommand.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Where the running instance's port file lives:
/// `db_path.parent()/cydrive.control` (a `None` or empty parent — a bare
/// file name — means the working directory).
pub fn control_file_path(cfg: &CyDriveConfig) -> PathBuf {
    let parent = Path::new(&cfg.db_path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    parent.join(CONTROL_FILE_NAME)
}

/// A running instance's loopback control listener (contract C1): the
/// one-command line protocol whose `STOP` fires the shutdown callback.
pub struct ControlServer {
    listener: TcpListener,
    addr: SocketAddr,
}

impl ControlServer {
    /// Binds the loopback listener (`127.0.0.1:0`) and (re)writes the
    /// port file: a stale file from a previous run is removed first (a
    /// missing one is fine — nothing ran here), then the freshly bound
    /// address lands in `db_path.parent()/cydrive.control`. Bind and
    /// write errors surface as-is; the optional-component degrade (warn
    /// and keep running without `stop` support) belongs to the `run`
    /// wiring, not the primitive.
    pub async fn bind(cfg: &CyDriveConfig) -> io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
        let addr = listener.local_addr()?;
        let path = control_file_path(cfg);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        std::fs::write(&path, format!("{addr}\n"))?;
        Ok(Self { listener, addr })
    }

    /// The address the listener actually bound (the `:0` port resolved
    /// to its ephemeral value).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The accept loop (one spawned task per connection): a line reading
    /// `STOP` (trimmed) is answered `OK: shutting down`, the connection
    /// is closed, and `shutdown` fires; a line reading `PING` is answered
    /// with [`ping_reply`]'s version line and fires nothing; any other
    /// line answers `ERR: unknown command`. The loop keeps serving after
    /// a STOP, so later `STOP`s fire the callback again — **the callback
    /// must be idempotent** (the run wiring passes the unified shutdown
    /// trigger, which is). Per-connection I/O errors only end that
    /// connection's task with a warning; only a failed `accept` ends the
    /// loop.
    ///
    /// The callback itself runs on this loop's task — the connection
    /// tasks only request it over an internal channel — so a plain
    /// `Send` closure with no `Sync` is enough.
    pub async fn run(self, shutdown: impl Fn() + Send + 'static) -> io::Result<()> {
        let listener = self.listener;
        // One buffered request per STOP: every STOP line fires the
        // callback exactly once, serialized on this task.
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut stream, peer) = accepted?;
                    let stop_tx = stop_tx.clone();
                    tokio::spawn(async move {
                        // read_until also returns the partial bytes at
                        // EOF, so a client that closes without a newline
                        // still gets its (ERR) answer instead of hanging
                        // the task. The BufReader borrow ends here — the
                        // one-line protocol never reads again, so bytes
                        // buffered past the newline are simply dropped
                        // with the connection.
                        let mut request = Vec::new();
                        let mut reader = BufReader::new(&mut stream);
                        if let Err(error) = reader.read_until(b'\n', &mut request).await {
                            tracing::warn!(%peer, %error, "control connection: read failed");
                            return;
                        }
                        let line = String::from_utf8_lossy(&request).trim().to_owned();
                        if line == "STOP" {
                            if let Err(error) = stream.write_all(b"OK: shutting down\n").await {
                                tracing::warn!(%peer, %error, "control connection: write failed");
                                return;
                            }
                            // TcpStream writes are unbuffered; flush pins
                            // the reply-before-close ordering explicitly.
                            // Closing before requesting the callback lets
                            // the stopper's read-to-EOF see the reply the
                            // moment the shutdown starts.
                            let _ = stream.flush().await;
                            drop(stream);
                            let _ = stop_tx.send(());
                        } else if line == "PING" {
                            // Status probe (status plan C1): answer and
                            // let the task end — dropping the stream
                            // closes the connection, ending the client's
                            // read-to-EOF without touching the shutdown
                            // callback.
                            if let Err(error) = stream.write_all(ping_reply().as_bytes()).await {
                                tracing::warn!(%peer, %error, "control connection: write failed");
                            }
                        } else if let Err(error) = stream.write_all(b"ERR: unknown command\n").await {
                            tracing::warn!(%peer, %error, "control connection: write failed");
                        }
                    });
                }
                stop = stop_rx.recv() => {
                    if stop.is_some() {
                        shutdown();
                    }
                }
            }
        }
    }
}

/// The reply the control channel's `PING` command answers with: the
/// binary name and version (`OK: cydrive 0.3.0`) — exactly the payload
/// `cydrive status` reports on its instance row. Never fires the
/// shutdown callback.
fn ping_reply() -> String {
    format!("OK: cydrive {}\n", env!("CARGO_PKG_VERSION"))
}

/// The connect-exchange skeleton shared by the control clients: connect
/// bounded by [`CONNECT_TIMEOUT`], send `request`, read the reply to EOF
/// and return it trimmed. The connect error propagates as-is so callers
/// can tell a refused/timeout (stale port file) from a protocol failure.
async fn exchange_line(addr: SocketAddr, request: &[u8]) -> io::Result<String> {
    let mut stream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(connected) => connected?,
        Err(_elapsed) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("connecting to the control port {addr} timed out"),
            ));
        }
    };
    stream.write_all(request).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    Ok(String::from_utf8_lossy(&response).trim().to_owned())
}

/// The `cydrive stop` client half (contract C1): send `STOP`, read the
/// reply to EOF and return it trimmed (see [`exchange_line`] for the
/// connect budget and error semantics).
pub async fn send_stop(addr: SocketAddr) -> io::Result<String> {
    exchange_line(addr, b"STOP\n").await
}

/// The `cydrive status` client half (status plan C1): send `PING` and
/// return the version line a live instance answers with. Same skeleton
/// and error semantics as [`send_stop`] — a refused/timeout address is
/// the caller's "stale control file" signal.
pub async fn send_ping(addr: SocketAddr) -> io::Result<String> {
    exchange_line(addr, b"PING\n").await
}

/// The double-start guard (Phase 2.5, run-flow front door): a boot over
/// a LIVE instance in the same working directory is refused — a second
/// boot would overwrite the first instance's control file and orphan
/// its stop handle (the live trap: the first instance keeps running but
/// `cydrive stop` can no longer reach it). A stale file pointing at a
/// dead address is removed so [`ControlServer::bind`] writes a fresh
/// one; no file at all is a clean start.
pub async fn ensure_not_running(cfg: &CyDriveConfig) -> anyhow::Result<()> {
    let control_file = control_file_path(cfg);
    let addr = match read_control_addr(cfg) {
        Ok(addr) => addr,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading the control file {}", control_file.display()));
        }
    };
    if send_ping(addr).await.is_ok() {
        anyhow::bail!(
            "another CyDrive instance is already running in this directory (control {addr}) — \
             stop it first (`cydrive stop`) before starting another"
        );
    }
    match std::fs::remove_file(&control_file) {
        Ok(()) => tracing::warn!(%addr, "removed the stale control file of a dead instance"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("removing the stale control file {}", control_file.display())
            });
        }
    }
    Ok(())
}

/// Reads the running instance's control address off the port file (the
/// `stop` discovery step): trim, parse as a [`SocketAddr`]. A missing
/// file is the plain io `NotFound` the caller maps to the actionable
/// "nothing is running" message.
pub fn read_control_addr(cfg: &CyDriveConfig) -> io::Result<SocketAddr> {
    let path = control_file_path(cfg);
    let contents = std::fs::read_to_string(&path)?;
    contents.trim().parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("parsing {}: {error}", path.display()),
        )
    })
}

/// The `stop` subcommand body (contract C2), run against the same config
/// discovery as `run` — the port file resolves relative to the
/// discovered `db_path`, so `stop` must run in the instance's working
/// directory. Three outcomes:
///
/// - no port file → actionable error naming [`CONTROL_FILE_NAME`];
/// - a live instance → send STOP, print the reply and the draining note
///   (the graceful drain itself belongs to the running instance);
/// - a dead address (refused / timeout) → the port file is stale: remove
///   it and say so, so the next `stop` starts clean.
pub async fn stop_cmd(cfg: &CyDriveConfig) -> Result<()> {
    let control_file = control_file_path(cfg);
    let addr = match read_control_addr(cfg) {
        Ok(addr) => addr,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!(
                "no running CyDrive instance found (no {} in the working directory)",
                CONTROL_FILE_NAME
            );
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading the control file {}", control_file.display()));
        }
    };

    match send_stop(addr).await {
        Ok(response) => {
            println!("{response}");
            println!(
                "shutdown requested; draining uploads (may take a while for large in-flight \
                 files) ..."
            );
            Ok(())
        }
        Err(_connect_error) => match std::fs::remove_file(&control_file) {
            Ok(()) => bail!("no instance responded at {addr}; removed the stale control file"),
            Err(error) => bail!(
                "no instance responded at {addr}; the stale control file {} could not be \
                 removed: {error}",
                control_file.display()
            ),
        },
    }
}
