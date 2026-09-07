//! Tests for the connect deadline guard (UX hardening unit).
//!
//! Regression context (2026-09-02 real-machine report): `cydrive run`
//! showed zero output while the Telegram connect blocked for tens of
//! seconds, and a Ctrl+C during that window hard-killed the process
//! silently. The guard bounds the connect phase so the CLI can surface a
//! human-readable diagnosis instead of hanging without feedback.

use std::time::Duration;

use cloudkit_cli::{connect_with_deadline, ConnectGuardError};

/// A future that never resolves: models a blocked TCP connect.
fn pending_result() -> impl std::future::Future<Output = Result<u8, String>> {
    std::future::pending()
}

/// A blocked connect must surface `Deadline` once the budget elapses —
/// never hang forever.
#[tokio::test]
async fn blocked_connect_hits_deadline() {
    let started = std::time::Instant::now();
    let result = connect_with_deadline(pending_result(), Duration::from_millis(50)).await;
    match result {
        Err(ConnectGuardError::Deadline(d)) if d == Duration::from_millis(50) => {}
        other => panic!("expected Deadline(50ms), got: {other:?}"),
    }
    assert!(started.elapsed() >= Duration::from_millis(45));
}

/// A connect that finishes in time returns its success untouched.
#[tokio::test]
async fn fast_connect_passes_through() {
    let result = connect_with_deadline(
        std::future::ready(Ok::<u8, String>(7u8)),
        Duration::from_secs(30),
    )
    .await;
    match result {
        Ok(value) => assert_eq!(value, 7),
        Err(error) => panic!("expected Ok(7), got: {error:?}"),
    }
}

/// A connect that finishes in time with an error returns `Inner` — the
/// deadline must not mask real errors.
#[tokio::test]
async fn fast_error_passes_through_inner() {
    let result = connect_with_deadline(
        std::future::ready(Err::<u8, _>("auth failed".to_string())),
        Duration::from_secs(30),
    )
    .await;
    match result {
        Err(ConnectGuardError::Inner(e)) => assert_eq!(e, "auth failed"),
        other => panic!("expected Inner, got: {other:?}"),
    }
}
