//! Red tests for the control channel + `cydrive stop` (plan
//! `docs/plans/2026-09-04-service-lifecycle.md`, contracts C1 + C2,
//! Task 1).
//!
//! C1: every running instance writes a port file `cydrive.control` next
//! to `db_path` (one line `127.0.0.1:<ephemeral>`) and serves a
//! loopback-only line protocol — `STOP` answers `OK: shutting down` and
//! fires the shutdown callback, any other line answers
//! `ERR: unknown command`. C2: the `stop` subcommand resolves that file
//! against the config's `db_path` and fails actionably when the file is
//! missing (no instance) or stale (dead port), removing the stale file.
//!
//! Assertion layers: the C1 pieces are pinned straight against
//! `cloudkit_cli::control`; the C2 pieces are pinned against the library
//! body `control::stop_cmd(&cfg)` (the established thin-arm pattern —
//! `cache_stats` / `cache_clear_cmd` / `run_migrate` — so `main.rs`'s
//! `Stop` arm is discover_config + stop_cmd by construction, and the
//! exit codes stay the implementation's concern).
//!
//! Bind's degrade-on-failure semantics (C1: a failed bind/write only
//! warns and the run continues) are deliberately NOT covered here — a
//! tempdir bind cannot be made to fail; that path is exercised by the
//! run_with_transport wiring tests of Task 2.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cloudkit_cli::control::{
    control_file_path, read_control_addr, send_ping, send_stop, stop_cmd, ControlServer,
};
use cloudkit_core::config::CyDriveConfig;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

// ------------------------------------------------------------- helpers ---

/// A config anchored in `dir` (same shape as `run_e2e::temp_config`,
/// minus the server knobs — nothing here boots the stack): the control
/// file lands in the db's parent directory, i.e. `dir` itself.
fn temp_config(dir: &Path) -> CyDriveConfig {
    CyDriveConfig {
        db_path: dir.join("meta.db").to_string_lossy().into_owned(),
        cache_path: dir.join("cache").to_string_lossy().into_owned(),
        ..CyDriveConfig::default()
    }
}

/// Spawns the server's accept loop with `on_stop` as the shutdown
/// callback. `run` consumes the server and keeps looping after a STOP
/// (C1: later connections no longer trigger), so the task simply lives
/// until the test's runtime drops.
fn spawn_run(server: ControlServer, on_stop: impl Fn() + Send + 'static) {
    tokio::spawn(async move {
        let _ = server.run(on_stop).await;
    });
}

// ------------------------------------------------------------ scenarios ---

/// C1: `ControlServer::bind` writes the port file next to the metadata
/// db — `db_path.parent()/cydrive.control` — and its trimmed content
/// parses back to the address the server actually bound.
#[tokio::test]
async fn bind_writes_control_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = temp_config(dir.path());

    let server = ControlServer::bind(&cfg)
        .await
        .expect("bind control server");

    let control_file = control_file_path(&cfg);
    assert_eq!(
        control_file,
        dir.path().join("cydrive.control"),
        "the control file lives next to the metadata db"
    );
    assert!(
        control_file.is_file(),
        "bind must leave the port file behind"
    );
    let contents = std::fs::read_to_string(&control_file).expect("read the port file");
    let parsed: SocketAddr = contents
        .trim()
        .parse()
        .expect("port file carries a socket addr");
    assert_eq!(
        parsed,
        server.local_addr(),
        "port file content must match the bound address"
    );
}

/// C1 round trip: a live server answers `STOP` with the OK line, and the
/// registered shutdown callback runs within 1s of that reply.
#[tokio::test]
async fn stop_roundtrip_triggers_shutdown() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = temp_config(dir.path());

    let server = ControlServer::bind(&cfg)
        .await
        .expect("bind control server");
    let addr = server.local_addr();
    let triggered = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&triggered);
    spawn_run(server, move || flag.store(true, Ordering::SeqCst));

    let resp = send_stop(addr).await.expect("send STOP to the live server");
    assert!(
        resp.contains("OK: shutting down"),
        "STOP must be acknowledged with the OK line: {resp}"
    );

    // Bounded wait (plan Task 1: within 1s). The dev tokio feature set
    // carries no `time`, so the loop yields via yield_now instead of
    // sleeping — enough for the accept loop's task to run the callback
    // (it fires in the same poll that wrote the reply).
    let deadline = Instant::now() + Duration::from_secs(1);
    while !triggered.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "shutdown callback not triggered within 1s of the OK reply"
        );
        tokio::task::yield_now().await;
    }
}

/// C1: any input line other than `STOP` gets the ERR reply — the
/// protocol has exactly one command.
#[tokio::test]
async fn unknown_command_replies_err() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = temp_config(dir.path());

    let server = ControlServer::bind(&cfg)
        .await
        .expect("bind control server");
    let addr = server.local_addr();
    spawn_run(server, || {});

    let mut stream = TcpStream::connect(addr)
        .await
        .expect("connect to control server");
    stream.write_all(b"HELLO\n").await.expect("send HELLO");
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .expect("read reply to EOF");
    let resp = String::from_utf8_lossy(&raw).into_owned();
    assert!(
        resp.contains("ERR: unknown command"),
        "unknown input must be rejected with the ERR line: {resp}"
    );
}

/// C2 (library layer): `stop` without a port file is an actionable error
/// saying no instance runs and naming the missing `cydrive.control`; the
/// reader primitive surfaces the same absence as an `Err`.
#[tokio::test]
async fn stop_without_control_file_is_actionable_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = temp_config(dir.path());

    assert!(
        read_control_addr(&cfg).is_err(),
        "no control file: read_control_addr must fail"
    );

    let err = stop_cmd(&cfg)
        .await
        .expect_err("stop must fail without a control file");
    let full = format!("{err:#}");
    assert!(
        full.contains("no running CyDrive instance"),
        "error must say no instance is running: {full}"
    );
    assert!(
        full.contains("cydrive.control"),
        "error must name the missing control file: {full}"
    );
}

/// C2 (library layer): a stale port file pointing at a dead address
/// makes `stop` report the staleness and remove the file, so a later
/// stop starts from a clean slate instead of hammering the dead port.
#[tokio::test]
async fn stop_against_stale_file_reports_and_cleans() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = temp_config(dir.path());

    let control_file = control_file_path(&cfg);
    std::fs::write(&control_file, "127.0.0.1:1\n").expect("seed a stale control file");
    assert!(control_file.is_file(), "stale control file seeded");

    let err = stop_cmd(&cfg)
        .await
        .expect_err("stop against a dead address must fail");
    let full = format!("{err:#}");
    assert!(
        full.contains("stale"),
        "error must report the stale control file: {full}"
    );
    assert!(
        !control_file.exists(),
        "the stale control file must be removed after the failed stop"
    );
}

/// C1 (status plan, `2026-09-04-status-and-automount.md`): `PING` is the
/// protocol's second command — it answers the version line
/// (`OK: cydrive <version>`, exactly what `cydrive status` shows on its
/// instance row) and, unlike STOP, leaves the shutdown callback
/// untriggered; a STOP sent afterwards still fires it.
#[tokio::test]
async fn ping_replies_version_without_stopping() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = temp_config(dir.path());

    let server = ControlServer::bind(&cfg)
        .await
        .expect("bind control server");
    let addr = server.local_addr();
    let triggered = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&triggered);
    spawn_run(server, move || flag.store(true, Ordering::SeqCst));

    let resp = send_ping(addr).await.expect("send PING to the live server");
    assert!(
        resp.contains("OK: cydrive"),
        "PING must be acknowledged with the OK: cydrive line: {resp}"
    );
    assert!(
        resp.contains(env!("CARGO_PKG_VERSION")),
        "PING reply must carry this binary's version: {resp}"
    );

    // Give a (wrongly fired) stop request the chance to land before
    // asserting it never did — same yield-now pattern as the STOP test.
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        !triggered.load(Ordering::SeqCst),
        "PING must not fire the shutdown callback"
    );

    let resp = send_stop(addr)
        .await
        .expect("STOP must still work after a PING");
    assert!(
        resp.contains("OK: shutting down"),
        "the follow-up STOP must be acknowledged: {resp}"
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    while !triggered.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "STOP after a PING must fire the shutdown callback within 1s"
        );
        tokio::task::yield_now().await;
    }
}

// -------------------------------------------------- double-start guard ---

/// The double-start guard: a boot over a LIVE instance in the same
/// working directory must refuse (a second boot overwrites the control
/// file and orphans the first instance's stop handle — the live trap
/// this guard exists for); a stale file pointing at a dead address is
/// removed and the boot proceeds; no file at all just proceeds.
#[tokio::test]
async fn ensure_not_running_refuses_live_removes_stale_and_passes_clean() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = temp_config(dir.path());

    // No file at all: clean to boot.
    cloudkit_cli::control::ensure_not_running(&cfg)
        .await
        .expect("no control file is a clean start");

    // A live instance: the guard must refuse, naming the situation.
    let server = ControlServer::bind(&cfg).await.expect("bind live server");
    spawn_run(server, || {});
    cloudkit_cli::control::ensure_not_running(&cfg)
        .await
        .expect_err("a live instance must refuse a second boot");

    // Stale file (nothing listens at the written address): the guard
    // removes it and lets the boot proceed.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").expect("grab a port");
    let dead_addr = dead.local_addr().expect("addr");
    drop(dead); // the address is now closed — nothing answers there
    std::fs::write(control_file_path(&cfg), format!("{dead_addr}\n"))
        .expect("write the stale control file");
    cloudkit_cli::control::ensure_not_running(&cfg)
        .await
        .expect("a stale control file must not block the boot");
    assert!(
        !control_file_path(&cfg).exists(),
        "the guard removes the stale control file"
    );
}
