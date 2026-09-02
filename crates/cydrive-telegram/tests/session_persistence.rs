//! RED-phase spec tests for the vendored grammers-session's `SqliteSession`
//! (rusqlite backend; see `crates/vendor/grammers-session/VENDOR.md`).
//! The tested methods are `todo!()` stubs: every behavioral test here must
//! fail with "not yet implemented" until the GREEN rusqlite port lands.
//!
//! These tests are offline (no network, no Telegram credentials): they only
//! exercise the write-through file persistence contract that
//! `GrammersTransport::connect` relies on — open/create, reopen, and
//! state surviving process restarts. The upstream trait has no
//! dialogs-specific methods in grammers 0.10 (those folded into
//! `updates_state`/`set_update_state`), so the "dialogs state" case is
//! speced as update-state persistence.

use std::path::PathBuf;

use grammers_session::storages::{MemorySession, SqliteSession};
use grammers_session::types::{
    ChannelState, PeerAuth, PeerId, PeerInfo, UpdateState, UpdatesState,
};
use grammers_session::Session;

/// Per-test session file inside a unique temp directory.
fn session_path(tag: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("{tag}.session"));
    (dir, path)
}

#[tokio::test]
async fn open_creates_file_and_reopens() {
    let (_dir, path) = session_path("create-reopen");
    {
        let _session = SqliteSession::open(&path).await.expect("first open");
    }
    assert!(path.exists(), "session file must exist after open");
    let _session = SqliteSession::open(&path).await.expect("reopen");
}

#[tokio::test]
async fn peer_persists_across_reopen() {
    let (_dir, path) = session_path("peer");
    let peer = PeerInfo::User {
        id: 1,
        auth: Some(PeerAuth::from_hash(42)),
        bot: Some(true),
        is_self: Some(false),
    };
    {
        let session = SqliteSession::open(&path).await.expect("open");
        session.cache_peer(&peer).await.expect("cache peer");
    }
    let session = SqliteSession::open(&path).await.expect("reopen");
    let loaded = session
        .peer(PeerId::user_unchecked(1))
        .await
        .expect("peer lookup");
    assert_eq!(loaded, Some(peer));
}

#[tokio::test]
async fn updates_state_persists_across_reopen() {
    let (_dir, path) = session_path("updates");
    {
        let session = SqliteSession::open(&path).await.expect("open");
        session
            .set_update_state(UpdateState::All(UpdatesState {
                pts: 1,
                qts: 2,
                date: 3,
                seq: 4,
                channels: vec![ChannelState { id: 5, pts: 6 }],
            }))
            .await
            .expect("set full state");
        // Partial write-through update on top of the persisted row.
        session
            .set_update_state(UpdateState::Secondary { qts: 9 })
            .await
            .expect("set secondary state");
    }
    let session = SqliteSession::open(&path).await.expect("reopen");
    let state = session.updates_state().await.expect("updates state");
    assert_eq!(
        state,
        UpdatesState {
            pts: 1,
            qts: 9,
            date: 3,
            seq: 4,
            channels: vec![ChannelState { id: 5, pts: 6 }],
        }
    );
}

/// Compile-time proof that both built-in storages implement the same
/// `Session` trait that `GrammersTransport` erases its storage behind.
fn assert_implements_session<S: Session>() {}

#[test]
fn memory_and_sqlite_share_trait() {
    assert_implements_session::<MemorySession>();
    assert_implements_session::<SqliteSession>();
}
