//! RED-phase test for the connect deadline on the one-shot data-channel
//! stack (review H1, plan `docs/plans/2026-09-04-tier1-review-fixes.md`
//! F1).
//!
//! Regression context: `connect_stack` dials Telegram without any
//! deadline. Through a proxy that accepts the TCP connection but never
//! speaks SOCKS5 (the real-world "silent proxy" failure), the connect
//! blocks indefinitely and the `push` / `pull` subcommands sit without
//! output — the same silent-hang class the `run` connect guard already
//! fixed (see `tests/connect_guard.rs`).
//!
//! Contract under test: `connect_stack_with_deadline(&cfg, deadline)`
//! bounds the connect segment with the existing `connect_with_deadline`
//! guard, so a dead proxy surfaces `ConnectGuardError::Deadline`
//! ("connect did not finish within ...") within the budget instead of
//! hanging.
//!
//! No real Telegram traffic: the fake proxy swallows the connection, so
//! the transport never reaches Telegram and no session is established.

use std::time::Duration;

use cydrive_cli::connect_stack_with_deadline;
use cydrive_core::config::CyDriveConfig;

/// Spawns a silent SOCKS5 proxy: accepts every TCP connection and then
/// holds it open without ever writing a byte. The client completes the
/// TCP connect, sends its SOCKS5 greeting, and blocks forever waiting
/// for a server reply that never comes — the "connected but
/// unresponsive" failure mode. Returns the bound port.
async fn spawn_silent_socks5_proxy() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the fake proxy listener");
    let port = listener
        .local_addr()
        .expect("read the fake proxy port")
        .port();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            // Hold the accepted socket open forever: never reply, never
            // close. Closing would let the client error out immediately
            // (TCP FIN) instead of hanging, defeating the point; the
            // task dies with the test runtime.
            tokio::spawn(async move {
                let _held = socket;
                std::future::pending::<()>().await;
            });
        }
    });
    port
}

/// The deadline must beat a silent proxy: a config whose proxy accepts
/// TCP but never completes the SOCKS5 handshake must make
/// `connect_stack_with_deadline` return `Err` within the 2s budget,
/// carrying the deadline diagnosis — not hang for the transport default
/// or forever. The 10s outer timeout keeps CI from hanging outright if
/// the guard regresses.
#[tokio::test]
async fn connect_stack_deadline_beats_silent_proxy() {
    let port = spawn_silent_socks5_proxy().await;

    // Shape-valid credentials pointing at the silent proxy; db/cache
    // paths live under a temp dir so nothing leaks into the cwd even if
    // a future implementation reorders the boot steps.
    let dir = tempfile::tempdir().expect("create temp dir");
    let mut cfg = CyDriveConfig::default();
    cfg.bot_token = "1:AAAAfake_token_for_shape".to_string();
    cfg.chat_id = 1;
    cfg.proxy_url = Some(format!("socks5://127.0.0.1:{port}"));
    cfg.db_path = dir.path().join("meta.db").display().to_string();
    cfg.cache_path = dir.path().join("cache").display().to_string();

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        connect_stack_with_deadline(&cfg, Duration::from_secs(2)),
    )
    .await
    .expect("connect_stack_with_deadline must return within 10s against the silent proxy");

    let err = match result {
        Ok(_) => panic!("the connect must fail against a silent proxy"),
        Err(err) => err,
    };
    // Anyhow alternate formatting renders the whole error chain; the
    // deadline diagnosis must be in it ("connect did not finish within
    // 2s" per ConnectGuardError::Deadline).
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("did not finish within"),
        "expected the deadline diagnosis in the error chain, got: {rendered}"
    );
    // The deadline — not a fast connect failure — produced the error:
    // the call must have run at least the full 2s budget (tokio timers
    // never fire early).
    assert!(started.elapsed() >= Duration::from_secs(2));
}
