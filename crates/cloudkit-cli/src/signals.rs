//! Signal helpers for the run flow's unified shutdown wait (contract C3).
//!
//! [`sigterm`] joins `tokio::signal::ctrl_c` and the control channel's
//! STOP as the run flow's shutdown sources; `run` races all three in one
//! `tokio::select!`, so this module keeps the helper's shape identical
//! across platforms.

/// Waits for the process's SIGTERM — the signal `kill` sends by default
/// and `systemctl stop` sends to a service — one-shot like
/// `tokio::signal::ctrl_c`: resolving means the signal arrived, and what
/// happens next belongs to the caller (the run flow funnels it into the
/// graceful shutdown).
#[cfg(unix)]
pub async fn sigterm() -> std::io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut stream = signal(SignalKind::terminate())?;
    stream.recv().await;
    Ok(())
}

/// Non-unix stand-in: never resolves. On Windows Ctrl+C is the interrupt
/// signal and `tokio::signal::ctrl_c` already covers it, so the run
/// flow's `tokio::select!` stays uniform across platforms with this
/// branch permanently pending.
#[cfg(not(unix))]
pub async fn sigterm() -> std::io::Result<()> {
    std::future::pending().await
}
