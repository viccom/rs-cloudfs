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

/// The Windows reserved device names (Microsoft's documented set): a
/// segment whose stem — the part before the FIRST dot, so `nul.txt` too —
/// equals one of these (case-insensitively) cannot be used as a literal
/// file name in a Win32 path.
const RESERVED_DEVICE_STEMS: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Whether a segment's stem is a reserved device name (`nul` and
/// `nul.txt` both are; `my.nul.txt` is not) — review M2.
fn is_reserved_stem(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment);
    RESERVED_DEVICE_STEMS
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

/// Encodes one path segment into a SAFE disk name (review M2). Windows
/// silently mangles three classes of segment: a reserved device stem
/// routes the whole access to the device (`/nul.txt` "writes" into NUL —
/// the file never exists, so every read re-downloads), and trailing
/// dots/spaces are stripped (`/foo.`, `/foo ` and `/foo. ` all collapse
/// onto `/foo`'s disk file — cross-row cache pollution). The mapping is a
/// pure function of the segment: deterministic, stable across restarts,
/// and injective (two vpaths never share a disk file), at the cost of
/// reversibility:
///
/// - every `%` becomes `%25` FIRST, so the escape tokens introduced below
///   can never be confused with literal input;
/// - a reserved stem gets a `~` marker prefix (`nul.txt` -> `~nul.txt`);
///   a segment that literally starts with `~` gets that leading char
///   encoded (`%7E`) so marker output stays unambiguous;
/// - every trailing `.` / ` ` becomes `%2E` / `%20`.
///
/// Only the DISK mapping is sanitized — the virtual path and the db keep
/// the original spelling. Old (pre-sanitization) cache copies simply miss
/// and re-hydrate. [`desanitize_segment`] is the exact inverse for
/// mapping disk names back to virtual paths.
fn sanitize_segment(segment: &str) -> String {
    debug_assert!(!segment.contains('/') && !segment.contains('\\'));
    let escaped = segment.replace('%', "%25");
    let mut out = if is_reserved_stem(segment) {
        format!("~{escaped}")
    } else if let Some(rest) = escaped.strip_prefix('~') {
        format!("%7E{rest}")
    } else {
        escaped
    };
    let stem_len = out.trim_end_matches(['.', ' ']).len();
    let tail: String = out[stem_len..]
        .bytes()
        .map(|byte| match byte {
            b'.' => "%2E",
            _ => "%20",
        })
        .collect();
    out.truncate(stem_len);
    out.push_str(&tail);
    out
}

/// The exact inverse of [`sanitize_segment`] (review M2): maps a disk name
/// under the cache root back to the segment of the virtual path it came
/// from. Legacy names (pre-sanitization copies) carry no escape tokens and
/// decode to themselves.
fn desanitize_segment(name: &str) -> String {
    // 1. The trailing run of whole 3-byte tokens back to their literals,
    //    parsed from the end (the encoded tail is a clean token sequence —
    //    a stem can never end in a bare "%2E"/"%20" because every literal
    //    "%" was escaped to "%25" before any token was appended).
    let mut out = name.to_string();
    let mut tail = Vec::new();
    while out.len() >= 3 {
        match &out[out.len() - 3..] {
            "%2E" => tail.push('.'),
            "%20" => tail.push(' '),
            _ => break,
        }
        out.truncate(out.len() - 3);
    }
    out.extend(tail.into_iter().rev());
    // 2. The leading markers: `%7E` before a literal-`~` segment, `~`
    //    before a reserved stem.
    if let Some(rest) = out.strip_prefix("%7E") {
        out = format!("~{rest}");
    } else if let Some(rest) = out.strip_prefix('~') {
        if is_reserved_stem(rest) {
            out = rest.to_string();
        }
    }
    // 3. The literal percent back.
    out.replace("%25", "%")
}

/// Maps a file inside the cache tree back to its virtual path
/// (`<root>/a/b.txt` -> `/a/b.txt`). `None` if the path does not sit under
/// `root` or would not be a valid virtual path. The per-segment decode is
/// the exact inverse of [`sanitize_segment`] — eviction and
/// pending-preserving cache clears identify a disk file's row through
/// this mapping, so it must round-trip.
fn rel_from_disk(root: &Path, path: &Path) -> Option<RelPath> {
    let under_root = path.strip_prefix(root).ok()?;
    let rel = under_root
        .components()
        .map(|component| desanitize_segment(&component.as_os_str().to_string_lossy()))
        .collect::<Vec<_>>()
        .join("/");
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
    ///
    /// Every segment goes through [`sanitize_segment`] (review M2): the
    /// reserved device stems and trailing dot/space shapes Windows would
    /// silently mangle map onto encoded, distinct, stable disk names. The
    /// virtual path and the db keep the original spelling; legacy cache
    /// copies whose names predate the encoding simply miss and re-hydrate.
    pub fn local_path(&self, rel: &RelPath) -> PathBuf {
        // The virtual root maps onto the cache root itself; RelPath
        // guarantees no leading `/` inside the remaining segments.
        let stripped = rel.as_str().trim_start_matches('/');
        if stripped.is_empty() {
            self.root.clone()
        } else {
            let mut local = self.root.clone();
            for segment in stripped.split('/') {
                local.push(sanitize_segment(segment));
            }
            local
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned mapping table (review M2): the full reserved-stem set in
    /// the shapes Windows mangles — bare, `.ext`, trailing dot, trailing
    /// space — plus the degenerate trailing forms of ordinary names.
    #[test]
    fn reserved_and_degenerate_segments_map_to_encoded_disk_names() {
        // Every reserved stem maps under the `~` marker, bare and with an
        // extension; case is preserved.
        for reserved in [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ] {
            assert_eq!(
                sanitize_segment(reserved),
                format!("~{reserved}"),
                "bare reserved stem"
            );
            assert_eq!(
                sanitize_segment(&format!("{reserved}.txt")),
                format!("~{reserved}.txt"),
                "reserved stem with an extension"
            );
            assert_eq!(
                sanitize_segment(&reserved.to_lowercase()),
                format!("~{}", reserved.to_lowercase()),
                "case-insensitive stem, case-preserving output"
            );
        }

        // Trailing dots/spaces are percent-encoded per character, alone
        // and stacked with the reserved marker.
        assert_eq!(sanitize_segment("foo."), "foo%2E");
        assert_eq!(sanitize_segment("foo "), "foo%20");
        assert_eq!(sanitize_segment("foo. "), "foo%2E%20");
        assert_eq!(sanitize_segment("foo .  "), "foo%20%2E%20%20");
        assert_eq!(sanitize_segment("nul. "), "~nul%2E%20");
        assert_eq!(sanitize_segment("nul.txt "), "~nul.txt%20");
        // An all-dots segment is pure trailing run.
        assert_eq!(sanitize_segment("..."), "%2E%2E%2E");

        // Ordinary names pass through untouched.
        assert_eq!(sanitize_segment("report.bin"), "report.bin");
        assert_eq!(
            sanitize_segment("my.nul.txt"),
            "my.nul.txt",
            "the stem is before the FIRST dot"
        );
        assert_eq!(
            sanitize_segment("con1"),
            "con1",
            "CON1 is not a reserved stem"
        );
        assert_eq!(sanitize_segment("nulx"), "nulx");

        // The escape tokens cannot collide with literal input: `%` and a
        // literal leading `~` are encoded first.
        assert_eq!(sanitize_segment("a%b"), "a%25b");
        assert_eq!(
            sanitize_segment("~nul"),
            "%7Enul",
            "a literal leading tilde"
        );
        assert_eq!(sanitize_segment("~plain"), "%7Eplain");
        assert_eq!(sanitize_segment("%7Enul"), "%257Enul");
    }

    /// Injectivity: no two distinct segments ever map onto the same disk
    /// name (the bug being fixed — `/foo.`, `/foo ` and `/foo. ` used to
    /// collapse onto `/foo`).
    #[test]
    fn sanitized_segments_are_injective() {
        let segments = [
            "foo",
            "foo.",
            "foo ",
            "foo. ",
            "foo..",
            "foo.  ",
            "nul",
            "nul.txt",
            "nul. ",
            "~nul",
            "~plain",
            "%2E",
            "%7Enul",
            "a%b",
            "report.bin",
            "con",
            "CON",
            "con1",
        ];
        for (i, a) in segments.iter().enumerate() {
            for b in &segments[i + 1..] {
                assert_ne!(
                    sanitize_segment(a),
                    sanitize_segment(b),
                    "distinct segments {a:?} and {b:?} must not share a disk name"
                );
            }
        }
    }

    /// The disk mapping round-trips: `desanitize_segment` inverts
    /// `sanitize_segment` exactly, so eviction and pending-preserving
    /// cache clears find the right row for every disk file.
    #[test]
    fn segment_sanitization_round_trips() {
        let segments = [
            "foo",
            "foo.",
            "foo ",
            "foo. ",
            "foo .  ",
            "...",
            "nul",
            "NUL",
            "nul.txt",
            "nul. ",
            "nül.txt",
            "my.nul.txt",
            "con1",
            "~nul",
            "~plain",
            "%2E",
            "%7Enul",
            "a%b",
            "a.%b",
            "report.bin",
            "名字.txt",
        ];
        for segment in segments {
            assert_eq!(
                desanitize_segment(&sanitize_segment(segment)),
                segment,
                "round trip of {segment:?}"
            );
        }
    }

    /// End to end: `local_path` encodes each segment and `rel_from_disk`
    /// decodes it back to the original virtual path.
    #[test]
    fn local_path_and_rel_from_disk_round_trip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = CacheManager::new(dir.path().to_path_buf(), 1 << 30);
        for rel in ["/docs/report.bin", "/nul.txt", "/foo. /bar ", "/~nul"] {
            let rel = RelPath::new(rel).expect("valid rel path");
            let local = cache.local_path(&rel);
            // The disk path must be a plain relative walk under the root.
            let stripped = local.strip_prefix(dir.path()).expect("under the root");
            for component in stripped.components() {
                let name = component.as_os_str().to_string_lossy();
                assert!(
                    !is_reserved_stem(&name)
                        || name.starts_with('~')
                        || name.ends_with("%2E")
                        || name.ends_with("%20"),
                    "a reserved stem on disk must carry the ~ marker: {name:?}"
                );
                assert!(
                    !name.ends_with('.') && !name.ends_with(' '),
                    "no trailing dot/space may reach the disk: {name:?}"
                );
            }
            assert_eq!(
                rel_from_disk(dir.path(), &local).as_ref(),
                Some(&rel),
                "the disk path decodes back to {rel}"
            );
        }
    }
}
