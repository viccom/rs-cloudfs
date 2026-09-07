//! On-disk LRU cache for hydrated CyDrive files.
//!
//! Contract source: Python `cydrive/cache_manager.py`, with one mandated
//! design change (see `docs/rust-rewrite-design.md`, «数据模型»): access
//! recency is tracked in memory by [`CacheManager::record_access`] instead
//! of the filesystem atime, which Windows commonly disables
//! (`NtfsDisableLastAccessUpdate`). The on-disk layout is unchanged — a
//! mirror of the virtual path tree under the cache root — so existing
//! Python cache directories keep working.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::rel_path::RelPath;

/// Smart on-demand LRU cache manager.
pub struct CacheManager {
    /// Cache root, mirroring the virtual path tree.
    root: PathBuf,
    /// Hard capacity in bytes.
    limit_bytes: u64,
    /// In-memory recency log (replaces the Python atime/`touch` mechanism).
    last_access: Mutex<HashMap<RelPath, SystemTime>>,
}

/// Recursively collects every regular file under `dir` (Python `os.walk`).
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Maps a file inside the cache tree back to its virtual path
/// (`<root>/a/b.txt` -> `/a/b.txt`). `None` if the path does not sit under
/// `root` or would not be a valid virtual path.
fn rel_from_disk(root: &Path, path: &Path) -> Option<RelPath> {
    let under_root = path.strip_prefix(root).ok()?;
    let rel = under_root.to_string_lossy().replace('\\', "/");
    RelPath::new(&format!("/{rel}")).ok()
}

impl CacheManager {
    /// Creates a manager over `root` (created if missing, Python parity)
    /// with a hard capacity of `limit_bytes`.
    pub fn new(root: PathBuf, limit_bytes: u64) -> Self {
        // Python parity: `os.makedirs(cache_dir, exist_ok=True)` (best
        // effort — the constructor has no error channel).
        let _ = fs::create_dir_all(&root);
        Self {
            root,
            limit_bytes,
            last_access: Mutex::new(HashMap::new()),
        }
    }

    /// Absolute path of `rel` inside the mirrored cache tree:
    /// `/a/b.txt` maps to `<root>/a/b.txt`, `/` maps to `<root>` itself.
    /// Pure mapping — never creates directories.
    pub fn local_path(&self, rel: &RelPath) -> PathBuf {
        // The virtual root maps onto the cache root itself; RelPath
        // guarantees no leading `/` inside the remaining segments.
        let stripped = rel.as_str().trim_start_matches('/');
        if stripped.is_empty() {
            self.root.clone()
        } else {
            self.root.join(stripped)
        }
    }

    /// Whether the cached copy exists and is non-empty (`size > 0`),
    /// matching Python `is_cached`.
    pub fn is_cached(&self, rel: &RelPath) -> bool {
        fs::metadata(self.local_path(rel))
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)
    }

    /// Records an access to `rel` in the in-memory recency log
    /// (replaces the Python `touch`/atime mechanism).
    pub fn record_access(&self, rel: &RelPath) {
        let mut log = self.last_access.lock().expect("access log mutex poisoned");
        log.insert(rel.clone(), SystemTime::now());
    }

    /// Total size in bytes of all files under the cache root
    /// (recursive, files only).
    pub fn total_size(&self) -> u64 {
        let mut files = Vec::new();
        collect_files(&self.root, &mut files);
        files
            .iter()
            .filter_map(|path| fs::metadata(path).ok())
            .map(|meta| meta.len())
            .sum()
    }

    /// Evicts least-recently-used cache files until `needed_bytes` fit.
    ///
    /// If `total_size() + needed_bytes <= limit`, deletes nothing and
    /// returns an empty vector. Otherwise deletes files ordered by last
    /// access, oldest first — files never passed to [`record_access`]
    /// fall back to their mtime as the access time — until the limit is
    /// satisfied, then returns the evicted paths in deletion order.
    /// Directories are never deleted. Deleting everything and still not
    /// fitting is not an error (Python returns silently in that case).
    pub fn evict_lru(&self, needed_bytes: u64) -> io::Result<Vec<RelPath>> {
        let mut paths = Vec::new();
        collect_files(&self.root, &mut paths);

        // Access times from the log, mtime as the fallback; files whose
        // metadata cannot be read are invisible (Python skips stat errors).
        let log = self.last_access.lock().expect("access log mutex poisoned");
        let mut current_size = 0;
        let mut candidates = Vec::new();
        for path in paths {
            let meta = match fs::metadata(&path) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            let rel = match rel_from_disk(&self.root, &path) {
                Some(rel) => rel,
                None => continue,
            };
            let accessed = log
                .get(&rel)
                .copied()
                .unwrap_or_else(|| meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
            current_size += meta.len();
            candidates.push((accessed, meta.len(), path, rel));
        }
        drop(log);

        if current_size.saturating_add(needed_bytes) <= self.limit_bytes {
            return Ok(Vec::new());
        }
        candidates.sort_by_key(|&(accessed, ..)| accessed);

        let mut evicted = Vec::new();
        for (_, size, path, rel) in candidates {
            if current_size.saturating_add(needed_bytes) <= self.limit_bytes {
                break;
            }
            fs::remove_file(&path)?;
            current_size -= size;
            evicted.push(rel);
        }
        Ok(evicted)
    }

    /// Removes every cached file and (bottom-up) every subdirectory,
    /// keeping the cache root itself — Python `clear_all` parity.
    pub fn clear_all(&self) -> io::Result<()> {
        clear_dir(&self.root)
    }

    /// [`CacheManager::clear_all`] minus the paths in `keep` (matched by
    /// virtual path): the `cache clear` command hands it the pending
    /// uploads, whose local copy is the only copy of the bytes (plan
    /// revision A1). Directories are still cleaned bottom-up — a
    /// directory whose kept files are gone goes too, and one that still
    /// holds kept files simply stays.
    pub fn clear_except(&self, keep: &[RelPath]) -> io::Result<()> {
        clear_dir_except(&self.root, &self.root, keep)
    }
}

/// Depth-first `clear_all` worker: deletes every file under `dir`, then
/// the (now empty) subdirectories — `os.walk(topdown=False)` semantics.
fn clear_dir(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            clear_dir(&path)?;
            fs::remove_dir(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Depth-first [`CacheManager::clear_except`] worker: deletes every file
/// under `dir` except the virtual paths in `keep` (`root` maps disk paths
/// back to virtual ones), then removes the (now empty) subdirectories. A
/// directory that still holds kept files cannot be removed — that
/// `DirectoryNotEmpty` outcome is the expected "preserved" result, not a
/// failure.
fn clear_dir_except(dir: &Path, root: &Path, keep: &[RelPath]) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            clear_dir_except(&path, root, keep)?;
            if let Err(error) = fs::remove_dir(&path) {
                if error.kind() != io::ErrorKind::DirectoryNotEmpty {
                    return Err(error);
                }
            }
        } else {
            // A file that cannot be expressed as a virtual path can never
            // be in `keep`, so it goes exactly as in clear_all.
            let kept = rel_from_disk(root, &path).is_some_and(|rel| keep.contains(&rel));
            if !kept {
                fs::remove_file(&path)?;
            }
        }
    }
    Ok(())
}
