//! SyncStore semantics over an in-memory SQLite database (sync-lite
//! Batch A, store-level contract):
//!
//! - push auto-registers the namespace and bumps the counter per row
//! - versions are monotonic across batches; upsert = last pusher wins
//! - tombstones are stored and returned verbatim
//! - pull(since) returns only rows with version > since
//! - pull of an unknown namespace is empty and does NOT register it
//! - a batch of N rows raises the counter by exactly N (one
//!   transaction: all-or-nothing writes and increments)

use cydrive_sync::store::SyncStore;
use cydrive_sync::wire::PushRow;

fn row(rel_path: &str, deleted: bool, payload: &str) -> PushRow {
    PushRow {
        rel_path: rel_path.to_string(),
        deleted,
        payload: payload.to_string(),
    }
}

fn mem() -> SyncStore {
    SyncStore::open_in_memory().expect("in-memory store")
}

#[test]
fn push_registers_namespace_and_versions_from_one() {
    let store = mem();
    let max = store
        .push("ns", &[row("/a", false, "pa"), row("/b", false, "pb")])
        .unwrap();
    assert_eq!(max, 2);

    let (rows, max_version) = store.pull("ns", 0).unwrap();
    assert_eq!(max_version, 2);
    let mut by_path: Vec<(String, i64)> = rows
        .iter()
        .map(|r| (r.rel_path.clone(), r.version))
        .collect();
    by_path.sort();
    assert_eq!(by_path, vec![("/a".to_string(), 1), ("/b".to_string(), 2)]);

    assert_eq!(store.namespace_version("ns").unwrap(), Some(2));
}

#[test]
fn versions_increase_monotonically_across_batches() {
    let store = mem();
    assert_eq!(store.push("ns", &[row("/a", false, "one")]).unwrap(), 1);
    assert_eq!(store.push("ns", &[row("/a", false, "two")]).unwrap(), 2);
    assert_eq!(store.push("ns", &[row("/b", false, "bee")]).unwrap(), 3);

    let (rows, max_version) = store.pull("ns", 0).unwrap();
    assert_eq!(max_version, 3);
    let get = |r: &cydrive_sync::wire::PulledRow| r.version;
    assert_eq!(
        get(rows.iter().find(|r| r.rel_path == "/a").unwrap()),
        2,
        "upsert at v2 supersedes the v1 write"
    );
    assert_eq!(get(rows.iter().find(|r| r.rel_path == "/b").unwrap()), 3);
}

#[test]
fn last_pusher_wins_upsert() {
    let store = mem();
    store.push("ns", &[row("/a", false, "{\"v\":1}")]).unwrap();
    store.push("ns", &[row("/a", false, "{\"v\":2}")]).unwrap();

    let (rows, _) = store.pull("ns", 0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].payload, "{\"v\":2}");
    assert!(!rows[0].deleted);
    assert_eq!(rows[0].version, 2);
}

#[test]
fn tombstone_row_roundtrip() {
    let store = mem();
    store.push("ns", &[row("/gone", true, "")]).unwrap();

    let (rows, _) = store.pull("ns", 0).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].deleted);
    assert_eq!(rows[0].payload, "");
}

#[test]
fn pull_since_returns_only_newer_versions() {
    let store = mem();
    store
        .push("ns", &[row("/a", false, "a"), row("/b", false, "b")])
        .unwrap(); // v1, v2
    store.push("ns", &[row("/c", false, "c")]).unwrap(); // v3

    let (rows, max_version) = store.pull("ns", 2).unwrap();
    assert_eq!(max_version, 3);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].rel_path, "/c");
    assert_eq!(rows[0].version, 3);

    // an upsert re-delivers the row with its new version
    store.push("ns", &[row("/a", false, "a2")]).unwrap(); // v4
    let (rows, _) = store.pull("ns", 3).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].rel_path, "/a");
    assert_eq!(rows[0].version, 4);
    assert_eq!(rows[0].payload, "a2");

    let (all, _) = store.pull("ns", 0).unwrap();
    assert_eq!(all.len(), 3, "upserts do not duplicate rows");
}

#[test]
fn pull_unknown_namespace_is_empty_and_registers_nothing() {
    let store = mem();
    let (rows, max_version) = store.pull("ghost", 0).unwrap();
    assert!(rows.is_empty());
    assert_eq!(max_version, 0);
    assert_eq!(
        store.namespace_version("ghost").unwrap(),
        None,
        "pull must not register the namespace"
    );

    // a later first push starts from version 1 (pull stayed unknown)
    assert_eq!(store.push("ghost", &[row("/x", false, "x")]).unwrap(), 1);
}

#[test]
fn batch_raises_counter_by_exactly_row_count() {
    let store = mem();
    assert_eq!(
        store
            .push(
                "ns",
                &[
                    row("/a", false, "1"),
                    row("/b", false, "2"),
                    row("/c", false, "3")
                ]
            )
            .unwrap(),
        3
    );
    // re-pushing an existing row still consumes a version (LWW needs it)
    assert_eq!(store.push("ns", &[row("/a", false, "1b")]).unwrap(), 4);
    // an empty batch changes nothing
    assert_eq!(store.push("ns", &[]).unwrap(), 4);

    // an empty batch on a fresh namespace still auto-registers it at 0
    assert_eq!(store.push("fresh", &[]).unwrap(), 0);
    assert_eq!(store.namespace_version("fresh").unwrap(), Some(0));
}

#[test]
fn file_backed_store_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("sync.db");
    {
        let store = SyncStore::open(&db_path).unwrap();
        store.push("ns", &[row("/a", false, "payload")]).unwrap();
    }
    let store = SyncStore::open(&db_path).unwrap();
    let (rows, max_version) = store.pull("ns", 0).unwrap();
    assert_eq!(max_version, 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].payload, "payload");
    // the counter continues from where the previous store left it
    assert_eq!(store.push("ns", &[row("/b", false, "b")]).unwrap(), 2);
}
