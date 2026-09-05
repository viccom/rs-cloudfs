//! RED-phase tests for the per-instance stable sync identity
//! (quasi-realtime batch, doorbell model): `MetaDatabase` grows one more
//! additive table,
//!
//! ```sql
//! sync_client_id(id INTEGER PRIMARY KEY CHECK(id=0),
//!                client_id TEXT NOT NULL)
//! ```
//!
//! plus the get-or-create accessor `MetaDatabase::sync_client_id() -> String`:
//! the first call generates a random 32-hex-char identity, writes it, and
//! returns it; every later call (including from a re-opened database)
//! returns the same value — the identity must be stable per database so
//! the server's origin-skip ("never ring the pusher's own bell") keeps
//! working across restarts.

use cydrive_core::database::MetaDatabase;

fn fresh_db(tag: &str) -> (tempfile::TempDir, MetaDatabase) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join(format!("{tag}.db"))).expect("open");
    (dir, db)
}

/// The first call generates a 32-char lowercase hex identity.
#[test]
fn first_call_generates_32_hex_chars() {
    let (_dir, db) = fresh_db("format");
    let id = db.sync_client_id().expect("sync_client_id");
    assert_eq!(id.len(), 32, "32 hex characters (16 random bytes): {id}");
    assert!(
        id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "lowercase hex only: {id}"
    );
}

/// The second call returns the stored value (get-or-create, not
/// regenerate).
#[test]
fn second_call_returns_the_same_identity() {
    let (_dir, db) = fresh_db("stable");
    let first = db.sync_client_id().expect("first");
    let second = db.sync_client_id().expect("second");
    assert_eq!(first, second, "the identity is created exactly once");
}

/// The identity survives close + reopen — it lives in the database, not
/// in memory.
#[test]
fn identity_survives_reopen() {
    let (dir, db) = fresh_db("persist");
    let first = db.sync_client_id().expect("first");
    drop(db);
    let db_again = MetaDatabase::open(&dir.path().join("persist.db")).expect("reopen");
    let reopened = db_again.sync_client_id().expect("reopened");
    assert_eq!(first, reopened, "the identity must be stable per database");
}

/// Two databases hold two different identities.
#[test]
fn distinct_databases_get_distinct_identities() {
    let (_dir_a, db_a) = fresh_db("a");
    let (_dir_b, db_b) = fresh_db("b");
    let a = db_a.sync_client_id().expect("a");
    let b = db_b.sync_client_id().expect("b");
    assert_ne!(a, b, "identities are per-database random");
}
