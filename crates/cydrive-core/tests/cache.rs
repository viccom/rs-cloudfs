//! RED-phase tests for `cydrive_core::cache`.
//!
//! Contract under test: Python `cydrive/cache_manager.py` semantics with the
//! one mandated design change — LRU recency comes from the in-memory
//! [`cydrive_core::cache::CacheManager::record_access`] log instead of the
//! filesystem atime (unreliable on Windows). The on-disk layout stays a
//! mirror of the virtual path tree, so Python cache directories keep working.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use cydrive_core::cache::CacheManager;
use cydrive_core::rel_path::RelPath;

// ------------------------------------------------------------- helpers ---

fn rp(s: &str) -> RelPath {
    RelPath::new(s).expect("valid virtual path")
}

/// Tempdir guard + cache root + manager bound to `limit_bytes`.
fn new_cm(limit_bytes: u64) -> (tempfile::TempDir, PathBuf, CacheManager) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("Telegram_Cache");
    let cm = CacheManager::new(root.clone(), limit_bytes);
    (dir, root, cm)
}

/// Writes a cache file through the mirrored tree (tests create the dirs
/// themselves: `local_path` is a pure mapping).
fn write_cached(cm: &CacheManager, rel: &str, contents: &[u8]) -> PathBuf {
    let path = cm.local_path(&rp(rel));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create cache dirs");
    }
    fs::write(&path, contents).expect("write cache file");
    path
}

/// std-only mtime override for the atime-fallback tests.
fn set_mtime(path: &Path, t: SystemTime) {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for set_times");
    file.set_times(fs::FileTimes::new().set_modified(t))
        .expect("set mtime");
}

// ---------------------------------------------------------- local_path ---

#[test]
fn local_path_mirrors_virtual_tree() {
    let (_dir, root, cm) = new_cm(1024);

    // Python parity: the constructor creates the cache root.
    assert!(root.is_dir());

    assert_eq!(cm.local_path(&rp("/notes.txt")), root.join("notes.txt"));
    assert_eq!(
        cm.local_path(&rp("/a/b/c.bin")),
        root.join("a").join("b").join("c.bin")
    );
    // The virtual root maps onto the cache root itself.
    assert_eq!(cm.local_path(&rp("/")), root);
}

// ----------------------------------------------------------- is_cached ---

#[test]
fn is_cached_distinguishes_missing_empty_and_present() {
    let (_dir, _root, cm) = new_cm(1024);

    assert!(!cm.is_cached(&rp("/missing.txt")), "absent -> false");

    let empty = write_cached(&cm, "/empty.bin", b"");
    assert!(empty.exists());
    assert!(!cm.is_cached(&rp("/empty.bin")), "0-byte file -> false");

    write_cached(&cm, "/real.bin", b"payload");
    assert!(cm.is_cached(&rp("/real.bin")), "non-empty file -> true");
}

// --------------------------------------------------------- total_size ---

#[test]
fn total_size_sums_files_recursively() {
    let (dir, _root, cm) = new_cm(1024);

    write_cached(&cm, "/top.dat", &[0u8; 10]);
    write_cached(&cm, "/sub/mid.dat", &[0u8; 20]);
    write_cached(&cm, "/sub/deep/low.dat", &[0u8; 30]);
    assert_eq!(cm.total_size(), 60);

    // A fresh empty cache reports zero, not an error.
    let fresh = CacheManager::new(dir.path().join("fresh-cache"), 100);
    assert_eq!(fresh.total_size(), 0);
}

// ------------------------------------------------------------ evict_lru ---

#[test]
fn evict_lru_removes_oldest_accessed_first() {
    let (_dir, _root, cm) = new_cm(100);
    let a = write_cached(&cm, "/a.bin", &[0u8; 60]);
    let b = write_cached(&cm, "/b.bin", &[0u8; 50]);

    cm.record_access(&rp("/a.bin"));
    thread::sleep(Duration::from_millis(10));
    cm.record_access(&rp("/b.bin"));

    // 110 + 20 > 100: the older access (a) is evicted, b survives.
    let evicted = cm.evict_lru(20).expect("evict");
    assert_eq!(evicted, vec![rp("/a.bin")]);
    assert!(!a.exists(), "evicted file is gone from disk");
    assert!(b.exists());
    assert_eq!(cm.total_size(), 50);
}

#[test]
fn evict_lru_is_noop_when_everything_fits() {
    let (_dir, _root, cm) = new_cm(100);
    let a = write_cached(&cm, "/a.bin", &[0u8; 60]);
    cm.record_access(&rp("/a.bin"));

    // 60 + 20 <= 100: nothing to do.
    let evicted = cm.evict_lru(20).expect("evict");
    assert!(evicted.is_empty());
    assert!(a.exists());
    assert_eq!(cm.total_size(), 60);
}

#[test]
fn evict_lru_falls_back_to_mtime_when_never_accessed() {
    let (_dir, _root, cm) = new_cm(100);
    let older = write_cached(&cm, "/older.bin", &[0u8; 60]);
    let newer = write_cached(&cm, "/newer.bin", &[0u8; 50]);

    let now = SystemTime::now();
    set_mtime(&older, now - Duration::from_secs(3600));
    set_mtime(&newer, now);

    // Neither file was ever recorded: mtime is the access time.
    let evicted = cm.evict_lru(20).expect("evict");
    assert_eq!(evicted, vec![rp("/older.bin")]);
    assert!(!older.exists());
    assert!(newer.exists());
}

#[test]
fn evict_lru_mixes_access_log_and_mtime_fallback() {
    let (_dir, _root, cm) = new_cm(100);
    let recorded = write_cached(&cm, "/recorded.bin", &[0u8; 60]);
    let stale = write_cached(&cm, "/stale.bin", &[0u8; 50]);

    // stale's mtime is a day old; recorded was just accessed.
    set_mtime(&stale, SystemTime::now() - Duration::from_secs(24 * 3600));
    set_mtime(
        &recorded,
        SystemTime::now() - Duration::from_secs(24 * 3600),
    );
    cm.record_access(&rp("/recorded.bin"));

    let evicted = cm.evict_lru(20).expect("evict");
    assert_eq!(evicted, vec![rp("/stale.bin")]);
    assert!(!stale.exists());
    assert!(recorded.exists());
}

#[test]
fn evict_lru_deletes_in_order_until_it_fits() {
    let (_dir, _root, cm) = new_cm(100);
    let a = write_cached(&cm, "/a.bin", &[0u8; 60]);
    let b = write_cached(&cm, "/b.bin", &[0u8; 50]);

    cm.record_access(&rp("/a.bin"));
    thread::sleep(Duration::from_millis(10));
    cm.record_access(&rp("/b.bin"));

    // 110 + 51 > 100 and 50 + 51 > 100: both must go, oldest first.
    let evicted = cm.evict_lru(51).expect("evict");
    assert_eq!(evicted, vec![rp("/a.bin"), rp("/b.bin")]);
    assert!(!a.exists());
    assert!(!b.exists());
    assert_eq!(cm.total_size(), 0);
}

#[test]
fn evict_lru_never_removes_directories() {
    let (_dir, root, cm) = new_cm(100);
    write_cached(&cm, "/keep-dir/file.bin", &[0u8; 60]);

    // 60 + 100 > 100 -> the file goes, the directory structure stays.
    let evicted = cm.evict_lru(100).expect("evict");
    assert_eq!(evicted, vec![rp("/keep-dir/file.bin")]);
    assert!(
        root.join("keep-dir").is_dir(),
        "directories survive eviction"
    );
}

// ------------------------------------------------------------ clear_all ---

#[test]
fn clear_all_empties_tree_but_keeps_root() {
    let (_dir, root, cm) = new_cm(1024);
    let f1 = write_cached(&cm, "/top.dat", &[1u8; 10]);
    let f2 = write_cached(&cm, "/sub/deep/low.dat", &[2u8; 20]);

    cm.clear_all().expect("clear");

    assert_eq!(cm.total_size(), 0);
    assert!(!f1.exists());
    assert!(!f2.exists());
    assert!(
        !root.join("sub").exists(),
        "subdirectories removed bottom-up"
    );
    assert!(root.is_dir(), "the cache root itself survives");
}
