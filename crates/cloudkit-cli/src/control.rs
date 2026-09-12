//! The loopback control channel behind `cydrive stop` and `cydrive
//! status` (service-lifecycle plan contracts C1/C2; status plan C1;
//! runtime-volumes plan K48).
//!
//! Every running instance binds a loopback-only listener on an ephemeral
//! port and drops a one-line port file (`127.0.0.1:<port>`) next to its
//! metadata db. The line protocol, one command per connection, the reply
//! read to EOF:
//!
//! - `STOP` answers `OK: shutting down`, closes, and fires the shutdown
//!   callback (the reply lands before the close, so the stopper's
//!   read-to-EOF sees it the moment the shutdown starts);
//! - `PING` answers `OK: cydrive <version>` and disturbs nothing (the
//!   payload `cydrive status` shows on its instance row);
//! - `ADD <name>` / `REMOVE <name>` / `LIST` (runtime-volumes K48) are
//!   forwarded to the installed [`VolumeCommandHandler`]; its reply is
//!   passed through verbatim — `OK: ...` or a multi-line `OK:`/`ERR:`
//!   block whose text is the handler's actionable message. The replies
//!   are documented where the handler lives ([`crate`]'s multi-volume
//!   boot); the transport layer adds nothing to them;
//! - anything else answers `ERR: unknown command`. An instance without a
//!   handler answers the volume commands with the actionable
//!   "not available" ERR instead of routing them.
//!
//! Security model (plan C1): the listener binds 127.0.0.1 only and
//! carries no authentication — an attacker who can already talk to the
//! machine's loopback can equally `taskkill` the process, so the channel
//! opens no new attack surface. The port file is a runtime artifact;
//! the run shutdown removes it (Task 2 wiring).
//!
//! Concurrency (runtime-volumes §4): volume commands are serialized —
//! the accept loop awaits the handler for at most one command at a time,
//! so ADD and REMOVE can never interleave (a later command, STOP
//! included, queues behind the one in flight).

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use cloudkit_core::config::CyDriveConfig;
use futures_util::FutureExt as _;
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

/// The volume-command handler the multi-volume boot installs (K48): the
/// raw command line (trimmed, e.g. `ADD b` / `REMOVE b` / `LIST`) in,
/// the verbatim reply text out — `OK: ...` or `ERR: ...`, multi-line
/// allowed, `\n`-terminated lines. At most one invocation runs at a time
/// (the accept loop serializes), so the handler needs no internal
/// locking of its own for command-vs-command races.
pub type VolumeCommandHandler = Arc<
    dyn for<'a> Fn(&'a str) -> Pin<Box<dyn Future<Output = String> + Send + 'a>>
        + Send
        + Sync
        + 'static,
>;

/// The reply an instance without a handler gives its volume commands:
/// actionable about who does serve them (a multi-volume `run`).
const VOLUME_COMMANDS_UNAVAILABLE: &str =
    "ERR: volume commands are not available on this instance\n";

/// The reply a command whose handler panicked gets (review M1-1): the
/// panic is contained, the channel keeps serving — the instance log
/// carries the panic payload for diagnosis.
const HANDLER_PANIC_REPLY: &str =
    "ERR: internal error while executing the command — the control channel stays up; see the instance log\n";

/// The first token of a control line (uppercased for the keyword match).
fn first_token(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or("")
}

/// `true` for the lines the volume-command surface owns (K48): the
/// three keywords, with or without their argument — the handler answers
/// malformed shapes with its own usage ERR.
fn is_volume_command(line: &str) -> bool {
    matches!(first_token(line), "ADD" | "REMOVE" | "LIST")
}

/// A running instance's loopback control listener (contract C1): the
/// line protocol whose `STOP` fires the shutdown callback and whose
/// volume commands reach the installed [`VolumeCommandHandler`].
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
    /// with [`ping_reply`]'s version line and fires nothing; a volume
    /// command (`ADD`/`REMOVE`/`LIST`, K48) is forwarded to the
    /// installed handler and its reply written verbatim; any other line
    /// answers `ERR: unknown command`. The loop keeps serving after
    /// a STOP, so later `STOP`s fire the callback again — **the callback
    /// must be idempotent** (the run wiring passes the unified shutdown
    /// trigger, which is). Per-connection I/O errors only end that
    /// connection's task with a warning; only a failed `accept` ends the
    /// loop.
    ///
    /// The callback itself runs on this loop's task — the connection
    /// tasks only request it over an internal channel — so a plain
    /// `Send` closure with no `Sync` is enough. The volume commands run
    /// on the same task, one at a time: the serialization that keeps
    /// ADD and REMOVE from interleaving (runtime-volumes §4).
    pub async fn run(self, shutdown: impl Fn() + Send + 'static) -> io::Result<()> {
        self.run_with_commands(shutdown, None).await
    }

    /// [`ControlServer::run`] with the volume-command surface installed
    /// (K48): `ADD`/`REMOVE`/`LIST` lines route to `handler`, whose
    /// reply is written to the connection verbatim; without a handler
    /// those lines get the actionable "not available" ERR. The STOP and
    /// PING branches are untouched by the handler's presence. A
    /// panicking handler is contained (review M1-1): its command answers
    /// [`HANDLER_PANIC_REPLY`] and the loop keeps serving — the panic
    /// never reaches the accept loop's task.
    pub async fn run_with_commands(
        self,
        shutdown: impl Fn() + Send + 'static,
        commands: Option<VolumeCommandHandler>,
    ) -> io::Result<()> {
        let listener = self.listener;
        // One buffered request per STOP: every STOP line fires the
        // callback exactly once, serialized on this task.
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        // Volume commands queue here and run one at a time on the loop
        // task; the reply travels back to the connection task on a
        // one-shot.
        let (command_tx, mut command_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, tokio::sync::oneshot::Sender<String>)>(
            );
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut stream, peer) = accepted?;
                    let stop_tx = stop_tx.clone();
                    let command_tx = command_tx.clone();
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
                        } else if is_volume_command(&line) {
                            // K48: the handler lives on the accept loop
                            // (one command at a time); the connection
                            // task waits for the reply one-shot and
                            // writes it verbatim. A dropped one-shot
                            // means the accept loop died mid-command —
                            // there is nobody left to answer.
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            if command_tx.send((line, reply_tx)).is_err() {
                                tracing::warn!(%peer, "control connection: the command loop is gone");
                                return;
                            }
                            match reply_rx.await {
                                Ok(reply) => {
                                    if let Err(error) = stream.write_all(reply.as_bytes()).await {
                                        tracing::warn!(%peer, %error, "control connection: write failed");
                                    }
                                }
                                Err(_) => {
                                    tracing::warn!(
                                        %peer,
                                        "control connection: the command handler dropped the reply"
                                    );
                                }
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
                command = command_rx.recv() => {
                    if let Some((line, reply_tx)) = command {
                        // The serialized command execution: at most one
                        // handler invocation is awaited here at a time
                        // (runtime-volumes §4's concurrency ruling). The
                        // unwind guard (review M1-1) keeps a panicking
                        // handler from taking this loop task down — the
                        // listener lives on it, so an unguarded panic
                        // would also kill STOP/PING and leave the
                        // panicking command's client on a bare EOF;
                        // instead the client gets the actionable
                        // internal-error ERR and the next command runs
                        // normally. The payload is our own command text
                        // and panic message — nothing credential-shaped
                        // reaches the log.
                        let reply = match &commands {
                            Some(handler) => {
                                match AssertUnwindSafe(handler(&line))
                                    .catch_unwind()
                                    .await
                                {
                                    Ok(reply) => reply,
                                    Err(panic) => {
                                        let reason = panic
                                            .downcast_ref::<&str>()
                                            .map(|str| (*str).to_string())
                                            .or_else(|| {
                                                panic.downcast_ref::<String>().cloned()
                                            })
                                            .unwrap_or_else(|| {
                                                "<non-string panic payload>".to_string()
                                            });
                                        tracing::error!(
                                            command = %line,
                                            %reason,
                                            "a volume command handler panicked; the \
                                             control channel stays up"
                                        );
                                        HANDLER_PANIC_REPLY.to_string()
                                    }
                                }
                            }
                            None => VOLUME_COMMANDS_UNAVAILABLE.to_string(),
                        };
                        let _ = reply_tx.send(reply);
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
    exchange_line_bounded(addr, request, EXCHANGE_BUDGET).await
}

/// The whole client-side exchange budget (review M1-2): volume commands
/// serialize on the instance's accept loop, and a REMOVE legitimately
/// waits out its 60s drain + 10s unmount windows — 120s covers that
/// plus margin, so a busy instance's eventual reply still lands; past
/// it the client gives up with the actionable busy-instance error
/// instead of parking forever (`cydrive status`'s LIST forward is the
/// caller that used to hang blind).
const EXCHANGE_BUDGET: Duration = Duration::from_secs(120);

/// [`exchange_line`] with the budget injectable — published as the test
/// seam (review M1-2) so the give-up is deterministic against a server
/// that never replies (the tests pass milliseconds). Both the write and
/// the read-to-EOF run inside the budget: an instance wedged mid-reply
/// (or a socket that stalls) surfaces as an actionable TimedOut instead
/// of parking the caller forever.
pub async fn exchange_line_bounded(
    addr: SocketAddr,
    request: &[u8],
    budget: Duration,
) -> io::Result<String> {
    let mut stream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(connected) => connected?,
        Err(_elapsed) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("connecting to the control port {addr} timed out"),
            ));
        }
    };
    let written = match tokio::time::timeout(budget, stream.write_all(request)).await {
        Ok(written) => written,
        Err(_elapsed) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "writing the control command to {addr} timed out after {budget:?} — the \
                     instance is likely busy with a long volume command (e.g. a REMOVE \
                     draining a large upload); retry once it settles"
                ),
            ));
        }
    };
    written?;
    let mut response = Vec::new();
    let read = match tokio::time::timeout(budget, stream.read_to_end(&mut response)).await {
        Ok(read) => read,
        Err(_elapsed) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "reading the control reply from {addr} timed out after {budget:?} — the \
                     instance is likely busy with a long volume command (e.g. a REMOVE \
                     draining a large upload); retry once it settles"
                ),
            ));
        }
    };
    read?;
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

/// The volume-command client half (runtime-volumes K48): send one
/// command line (`LIST`, `ADD <name>`, `REMOVE <name>`) and return the
/// running instance's verbatim reply — `OK: ...` or an actionable
/// `ERR: ...`, multi-line. Same skeleton and error semantics as
/// [`send_stop`] — a refused/timeout address is the caller's "stale
/// control file" signal. `cydrive status`'s runtime volume section
/// forwards `LIST` through this.
pub async fn send_command(addr: SocketAddr, command: &str) -> io::Result<String> {
    let line = format!("{command}\n");
    exchange_line(addr, line.as_bytes()).await
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
