//! Real-binary behavior of the `cydrive-sync-server` command line. The
//! deployment-time paths (`--version`, `--help`, rejection of unknown
//! arguments) must print and exit *before* any setup — the bug this
//! pins: unknown flags used to be silently ignored and the service
//! started anyway, leaving a stray process holding the listen port.
//!
//! Review-followup additions: the startup log must carry the *actual*
//! bound address (a `SYNC_LISTEN=127.0.0.1:0` run must not log `:0`),
//! and a non-Unicode argument must be refused like any unknown
//! argument instead of panicking inside `std::env::args()`.
//!
//! Each invocation is pinned to an ephemeral port and a throwaway temp
//! db, so even a run against the buggy binary can never touch :8290 or
//! a real database; a child that has not exited within the timeout is
//! killed and reported as "did not exit on its own".

use std::ffi::OsString;
use std::io::Read as _;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Generous ceiling for a flag path that should be near-instant; on the
/// current (buggy) binary the server starts and never exits, so the
/// timeout exists to turn that hang into a test failure.
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

struct Outcome {
    /// False when the child had to be killed — i.e. it ignored its
    /// arguments and was still serving at the deadline.
    exited_on_own: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs the real binary with `args`, waiting at most `EXIT_TIMEOUT` for
/// it to exit on its own.
fn run_exe(args: &[&str]) -> Outcome {
    run_exe_os(&args.iter().map(OsString::from).collect::<Vec<_>>())
}

/// [`run_exe`] for raw (possibly non-UTF-8) arguments: the argument
/// vector is passed through the OS encoding untouched, so a test can
/// hand the binary an argument that is not valid Unicode.
fn run_exe_os(args: &[OsString]) -> Outcome {
    let scratch = tempfile::tempdir().expect("scratch dir for the child's db");
    let mut child = Command::new(env!("CARGO_BIN_EXE_cydrive-sync-server"))
        .args(args)
        // Ephemeral port + throwaway db: a buggy run must be harmless.
        .env("SYNC_LISTEN", "127.0.0.1:0")
        .env("SYNC_DB", scratch.path().join("sync.db"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cydrive-sync-server");

    let deadline = Instant::now() + EXIT_TIMEOUT;
    let exited_on_own = loop {
        if child.try_wait().expect("poll child").is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break false;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let status = child.wait().expect("reap child");
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    Outcome {
        exited_on_own,
        code: status.code(),
        stdout,
        stderr,
    }
}

#[test]
fn version_flag_prints_the_version_line_and_exits_zero() {
    let out = run_exe(&["--version"]);
    assert!(
        out.exited_on_own,
        "--version must exit before any setup; instead the process was still \
         alive after {:?} (it ignored the flag and started the server)",
        EXIT_TIMEOUT
    );
    assert_eq!(out.code, Some(0), "exit code — stdout:\n{}", out.stdout);
    // clap-style `name version` line, matching `cydrive --version`.
    assert_eq!(
        out.stdout.trim(),
        format!("cydrive-sync-server {}", env!("CARGO_PKG_VERSION")),
        "stdout"
    );
}

#[test]
fn help_flag_prints_usage_and_exits_zero() {
    let out = run_exe(&["--help"]);
    assert!(
        out.exited_on_own,
        "--help must exit before any setup; instead the process was still \
         alive after {:?} (it ignored the flag and started the server)",
        EXIT_TIMEOUT
    );
    assert_eq!(out.code, Some(0), "exit code — stdout:\n{}", out.stdout);
    for needle in [
        "SYNC_LISTEN",
        "SYNC_DB",
        "SYNC_SECRET",
        "--version",
        "--help",
    ] {
        assert!(
            out.stdout.contains(needle),
            "--help output should mention {needle} — stdout:\n{}",
            out.stdout
        );
    }
}

#[test]
fn unknown_argument_is_rejected_on_stderr_without_starting_the_server() {
    let out = run_exe(&["--bogus"]);
    assert!(
        out.exited_on_own,
        "an unknown argument must not start the server; the process was still \
         alive after {:?}, serving instead of failing",
        EXIT_TIMEOUT
    );
    assert_ne!(
        out.code,
        Some(0),
        "unknown argument must exit non-zero — stderr:\n{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("--bogus"),
        "stderr should name the offending argument — stderr:\n{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("--help"),
        "stderr should point at --help for usage — stderr:\n{}",
        out.stderr
    );
}

/// Kills the child on drop, so an assertion failure in a test that
/// spawned a serving child (which never exits on its own) cannot leak
/// the process past the test.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Regression (review Low, ops blind spot): the startup log line must
/// carry the *actual* bound address. With `SYNC_LISTEN=127.0.0.1:0` (an
/// ephemeral port) the old line printed the configured `:0`, which in
/// the journal points operators at a port nothing listens on. The
/// subscriber writes to stdout (the `tracing_subscriber::fmt` default)
/// at info level, so the child runs with `RUST_LOG=info`.
#[test]
fn startup_log_names_the_actual_bound_port_not_the_configured_zero() {
    let scratch = tempfile::tempdir().expect("scratch dir for the child's db");
    let mut child = Command::new(env!("CARGO_BIN_EXE_cydrive-sync-server"))
        // Ephemeral port + throwaway db: even a buggy run is harmless.
        .env("SYNC_LISTEN", "127.0.0.1:0")
        .env("SYNC_DB", scratch.path().join("sync.db"))
        // The listening line is info-level; without a filter the
        // EnvFilter defaults to ERROR and stdout stays empty.
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cydrive-sync-server");
    let mut pipe = child.stdout.take().expect("piped stdout");
    let child = KillOnDrop(child);

    // Stream the child's stdout so the line is readable while the
    // server (which never exits on its own) is still serving; the
    // reader thread exits when the guard's kill closes the pipe.
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

    let deadline = Instant::now() + EXIT_TIMEOUT;
    let mut seen = String::new();
    let listening_line = loop {
        while let Ok(chunk) = rx.try_recv() {
            seen.push_str(&chunk);
        }
        // Only a newline-terminated occurrence counts: a chunk boundary
        // could otherwise truncate the port digits mid-number.
        if let Some(rest) = seen.split("listening on http://").nth(1) {
            if let Some(line_end) = rest.find('\n') {
                break format!("listening on http://{}", &rest[..line_end]);
            }
        }
        assert!(
            Instant::now() < deadline,
            "no \"listening on\" line within {EXIT_TIMEOUT:?} — stdout so far:\n{seen}"
        );
        std::thread::sleep(Duration::from_millis(25));
    };

    let logged = listening_line
        .split("listening on http://")
        .nth(1)
        .and_then(|rest| rest.trim().parse::<SocketAddr>().ok())
        .unwrap_or_else(|| panic!("line carries no address: {listening_line}"));

    assert_ne!(
        logged.port(),
        0,
        "the log must carry the actual bound port, not the configured :0 — line: {listening_line}"
    );

    // The logged address must genuinely be this server's listener —
    // connecting proves the line names the bound socket.
    std::net::TcpStream::connect_timeout(&logged, Duration::from_secs(2))
        .unwrap_or_else(|error| panic!("logged address {logged} is not this server: {error}"));

    drop(child);
}

/// An argument that cannot be decoded as Unicode: a lone UTF-16
/// surrogate on Windows, an invalid UTF-8 byte on Unix. Lossy-decoded
/// both read back as `b<U+FFFD>b`.
#[cfg(windows)]
fn non_unicode_argument() -> OsString {
    use std::os::windows::ffi::OsStringExt;
    OsString::from_wide(&[0x0062, 0xD800, 0x0062])
}

#[cfg(unix)]
fn non_unicode_argument() -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(vec![0x62, 0xFF, 0x62])
}

/// Regression (review Low): a non-Unicode argument must be *refused*
/// like any unknown argument (the lossy-decoded string travels the
/// normal `decide_startup` path) — historically `std::env::args()`
/// panicked outright on such argv, turning a stray filename into a
/// crash instead of the usage error.
#[test]
fn non_unicode_argument_is_refused_not_panic() {
    let out = run_exe_os(&[non_unicode_argument()]);
    assert!(
        out.exited_on_own,
        "a non-Unicode argument must not hang or start the server; the process was \
         still alive after {EXIT_TIMEOUT:?} — stderr:\n{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("unexpected argument"),
        "the lossy-decoded argument must be refused on stderr like any unknown \
         argument — stderr:\n{}",
        out.stderr
    );
    assert!(
        !out.stderr.contains("panicked"),
        "argv decoding must not panic on non-Unicode input — stderr:\n{}",
        out.stderr
    );
}
