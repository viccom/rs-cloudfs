//! Real-binary behavior of the `cydrive-sync-server` command line. The
//! deployment-time paths (`--version`, `--help`, rejection of unknown
//! arguments) must print and exit *before* any setup — the bug this
//! pins: unknown flags used to be silently ignored and the service
//! started anyway, leaving a stray process holding the listen port.
//!
//! Each invocation is pinned to an ephemeral port and a throwaway temp
//! db, so even a run against the buggy binary can never touch :8290 or
//! a real database; a child that has not exited within the timeout is
//! killed and reported as "did not exit on its own".

use std::io::Read as _;
use std::process::{Command, Stdio};
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
