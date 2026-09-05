//! Real-binary logging behavior (ops feedback batch): a bare
//! `cydrive-sync-server` run must be diagnosable out of the box.
//!
//! The bug this pins: `EnvFilter`'s built-in default is ERROR, so a
//! manual run without `RUST_LOG` printed *nothing at all* — not even
//! the listening line — which made the public deployment undiagnosable
//! ("无输出日志，诊断不友好"). Two requirements:
//!
//! 1. unset `RUST_LOG` defaults to info (the listening line appears);
//!    an explicitly set `RUST_LOG` keeps its meaning.
//! 2. every push/pull request logs one completion line at info
//!    (endpoint, namespace-key *prefix* — first 8 chars, enough to
//!    correlate without pasting the full key — row count, max_version,
//!    elapsed ms), secret rejections log a WARN that never contains
//!    the secret itself.
//!
//! Both tests drive the real binary over real HTTP (hand-rolled
//! HTTP/1.1 over a std TcpStream: `Connection: close` makes the
//! response readable to EOF without a tokio client in this file).

use std::cell::RefCell;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Ceiling for waiting on log lines from a healthy serving child.
const TIMEOUT: Duration = Duration::from_secs(5);
/// The server-side secret; its value must never surface in logs.
const SECRET: &str = "topsecret-never-log-me";
/// 16-char namespace key; logs must show at most its first 8 chars.
const NS: &str = "abcdefghijklmnop";
const NS_PREFIX: &str = "abcdefgh";

/// Kills the child on drop so a failed assertion cannot leak a serving
/// process past the test (same pattern as `bin_cli.rs`).
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A serving child plus a live feed of its stdout. `rust_log` is the
/// exact `RUST_LOG` the child sees (`None` = the variable is removed,
/// pinning the unset-default behavior).
struct ServingChild {
    _guard: KillOnDrop,
    // Keeps the throwaway db directory alive for the child's lifetime
    // (dropping it early would race the open file handle).
    _scratch: tempfile::TempDir,
    addr: String,
    // Everything the child ever printed (fed from the reader thread).
    // A single rolling buffer, not per-wait locals: a log line emitted
    // before one wait started must still be visible to the next one.
    stdout: RefCell<String>,
    incoming: mpsc::Receiver<String>,
}

/// Spawns the real binary on an ephemeral port with a throwaway db,
/// waits for the `listening on http://…` line and returns the child
/// with the parsed bound address.
fn spawn(rust_log: Option<&str>, secret: Option<&str>) -> ServingChild {
    let scratch = tempfile::tempdir().expect("scratch dir for the child's db");
    let mut command = Command::new(env!("CARGO_BIN_EXE_cydrive-sync-server"));
    command
        .env("SYNC_LISTEN", "127.0.0.1:0")
        .env("SYNC_DB", scratch.path().join("sync.db"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // The cargo/harness environment must not leak a RUST_LOG into the
    // child: `None` means "variable unset", the exact deployed accident.
    match rust_log {
        Some(value) => {
            command.env("RUST_LOG", value);
        }
        None => {
            command.env_remove("RUST_LOG");
        }
    }
    if let Some(secret) = secret {
        command.env("SYNC_SECRET", secret);
    }
    let mut child = command.spawn().expect("spawn cydrive-sync-server");
    let mut pipe = child.stdout.take().expect("piped stdout");
    let guard = KillOnDrop(child);

    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(read) = pipe.read(&mut chunk) {
            if read == 0
                || tx
                    .send(String::from_utf8_lossy(&chunk[..read]).into_owned())
                    .is_err()
            {
                break;
            }
        }
    });

    let deadline = Instant::now() + TIMEOUT;
    let mut seen = String::new();
    let addr = loop {
        while let Ok(chunk) = rx.try_recv() {
            seen.push_str(&chunk);
        }
        // Only a newline-terminated address counts (chunk boundaries
        // could truncate the port digits mid-number).
        if let Some(rest) = seen.split("listening on http://").nth(1) {
            if let Some(line_end) = rest.find('\n') {
                break rest[..line_end].trim().to_string();
            }
        }
        assert!(
            Instant::now() < deadline,
            "no \"listening on\" line within {TIMEOUT:?} — stdout so far:\n{seen}"
        );
        std::thread::sleep(Duration::from_millis(25));
    };

    ServingChild {
        _guard: guard,
        _scratch: scratch,
        addr,
        stdout: RefCell::new(seen),
        incoming: rx,
    }
}

impl ServingChild {
    /// Moves everything the reader thread has produced so far into the
    /// rolling buffer.
    fn absorb(&self) {
        while let Ok(chunk) = self.incoming.try_recv() {
            self.stdout.borrow_mut().push_str(&chunk);
        }
    }

    /// Blocks until `predicate` accepts the accumulated stdout (or the
    /// deadline turns it into a failure carrying everything seen so far).
    fn wait_for_stdout(&self, what: &str, predicate: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.absorb();
            if predicate(&self.stdout.borrow()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no {what} line within {TIMEOUT:?} — stdout so far:\n{}",
                self.stdout.borrow()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Everything the child has printed so far.
    fn drain_stdout(&self) -> String {
        self.absorb();
        self.stdout.borrow().clone()
    }
}

/// Minimal blocking HTTP/1.1 POST: `Connection: close` means the full
/// response is readable to EOF. Returns the status code.
fn post(addr: &str, path: &str, body: &str) -> u16 {
    let mut stream = std::net::TcpStream::connect(addr).expect("connect to server");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no status line in response: {response}"))
}

/// Regression (ops feedback): with `RUST_LOG` unset the subscriber
/// must default to info, so a manual run prints the listening line —
/// the old ERROR default printed nothing at all.
#[test]
fn unset_rust_log_still_prints_the_listening_line() {
    let child = spawn(None, None);
    // Reachable proof: the logged address genuinely accepts connections.
    std::net::TcpStream::connect_timeout(
        &child.addr.parse().expect("logged addr parses"),
        Duration::from_secs(2),
    )
    .expect("logged address is this server's listener");
}

/// Every push/pull logs one info completion line — endpoint, namespace
/// key *prefix* (first 8 chars, never the full key), row count,
/// max_version, elapsed ms — and a secret rejection logs a WARN that
/// never contains the secret value. (`RUST_LOG` set explicitly, so
/// this test isolates the request logs from the default-filter test.)
#[test]
fn requests_log_ns_prefix_elapsed_and_never_the_secret() {
    let child = spawn(Some("info"), Some(SECRET));

    let push_body = format!(
        r#"{{"key":"{NS}","secret":"{SECRET}","rows":[{{"rel_path":"/a","deleted":false,"payload":"x"}}]}}"#
    );
    assert_eq!(post(&child.addr, "/v1/push", &push_body), 200);
    let pull_ok = format!(r#"{{"key":"{NS}","since":0,"secret":"{SECRET}"}}"#);
    assert_eq!(post(&child.addr, "/v1/pull", &pull_ok), 200);
    let pull_denied = format!(r#"{{"key":"{NS}","since":0,"secret":"nope"}}"#);
    assert_eq!(post(&child.addr, "/v1/pull", &pull_denied), 403);

    // info completion lines: endpoint + ns prefix + elapsed ms field,
    // on push and pull alike
    for endpoint in ["push", "pull"] {
        child.wait_for_stdout(
            format!("info {endpoint} completion line").as_str(),
            |seen| {
                seen.lines().any(|line| {
                    line.contains(endpoint)
                        && line.contains(NS_PREFIX)
                        && line.contains("elapsed_ms")
                })
            },
        );
    }
    // the 403 rejection is a WARN (still carrying the ns prefix)
    child.wait_for_stdout("WARN rejection line", |seen| {
        seen.lines()
            .any(|line| line.contains("WARN") && line.contains(NS_PREFIX))
    });

    // confidentiality: neither the secret value nor the full namespace
    // key may appear anywhere in the logs
    let seen = child.drain_stdout();
    assert!(
        !seen.contains(SECRET),
        "logs must never contain the secret — stdout:\n{seen}"
    );
    assert!(
        !seen.contains(NS),
        "logs must truncate the namespace key to its first 8 chars — stdout:\n{seen}"
    );
}
