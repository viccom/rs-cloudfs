//! `CloudFs` — the WinFsp `FileSystemContext` adapter.
//!
//! Scope of this batch (plan §3-WF1 + WF2 + WF3): `get_security_by_name`,
//! `open`, `close`, `get_file_info`, `read_directory`, `get_volume_info`
//! (WF1: all answered from the local SQLite rows plus the assembly-time
//! volume snapshot — K44, zero network); the read path (WF2: K33's
//! triple-gate dispatch into the bounded `open_range` window model, the
//! zero-side-effect `flush` and the K41 handle grace period); and the
//! write path plus the filesystem operations (WF3: the K43
//! disposition→intent matrix, `create`, staged writes committed exactly
//! once by `cleanup`, `overwrite`/`set_file_size`/`set_basic_info`,
//! `rename`, `set_delete`, `set_volume_label`).
//!
//! Deliberately NOT implemented here:
//! - **WF4** owns `winfsp_init`, the host mount/unmount and the K40
//!   WebDAV fallback; the DLL preload and mount-point rules live there,
//!   not here.
//!
//! Case sensitivity (known, deliberate): lookups are byte-exact against
//! the `/`-separated rows (`RelPath`'s contract). Windows sends the names
//! it saw in `read_directory`, so Explorer's own navigation matches, but
//! a program typed-`\DOCS\README.TXT` miss is a `STATUS_OBJECT_NAME_NOT_FOUND`
//! until a case-folding layer is decided (WF4 open question, not
//! invented here).

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
// The read state is locked ACROSS an await (a window fetch runs under it,
// so two reads of one reused handle serialize instead of interleaving
// fills) — hence the await-aware lock, the same choice ck-baidu's token
// refresh lock makes. Everything else here is a plain `std` mutex: the
// enumeration buffer, the grace table, the open-time stat snapshot and
// the staged write state are only ever held synchronously (the staged
// writer's commit is taken OUT of its slot before the bridge blocks).
use tokio::sync::Mutex as AsyncMutex;

use cloudkit_core::database::FileRecord;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::vfs::{StreamSource, Vfs, VfsError};
use windows::Win32::Foundation::{
    STATUS_DIRECTORY_NOT_EMPTY, STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_PARAMETER,
    STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_OBJECT_PATH_NOT_FOUND,
};
use winfsp::constants::FspCleanupFlags;
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::{FspError, Result, U16CStr};

use crate::bridge::AsyncBridge;
use crate::error::{fsp_error, invalid_name};
use crate::reader::{LocalReader, ReadHandle, WindowReader};
use crate::writer::StagedWriter;

/// Compat contract 6 (Python `get_available_bytes`): the virtual cloud
/// headroom reported on top of what the rows already hold. Shared with
/// the WebDAV adapter so both mounts show the same numbers.
pub const VOLUME_HEADROOM: u64 = 10 * 1024 * 1024 * 1024 * 1024;

/// Win32 file attribute bits (winfsp-rs takes them as raw `u32`; pulling
/// `Win32_Storage_FileSystem` in for two constants would grow the
/// feature surface the FSD needs).
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0010;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0020;

/// `NtCreateFile` create-option bits the FSD carries in
/// `create_options`'s low 24 bits. Values cross-checked against
/// `windows::Win32::Wdk::Storage::FileSystem` (the WDK module is not
/// enabled in this crate's `windows` feature set — same reasoning as the
/// attribute bits above) and against WinFsp's own headers.
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
/// Rejected in combination with `FILE_DIRECTORY_FILE` — the FSD's kernel
/// side does the same (`src/sys/create.c:393`); it also drives
/// `FspFileSystemOpCreate_CollisionCheck`, which rewrites a create
/// collision into `STATUS_FILE_IS_A_DIRECTORY` when the caller asked for
/// a file and found a directory (the only reason that answer differs
/// from the raw collision this adapter returns).
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;

/// `create_options`'s disposition byte lives in the HIGH 8 bits — WinFsp
/// declares it verbatim (`fsctl.h:351`: "Disposition: high 8 bits;
/// Options: low 24 bits") and dispatches on exactly that
/// (`src/dll/fsop.c:918`, the `FspFileSystemOpCreate` switch).
const DISPOSITION_SHIFT: u32 = 24;

/// The `CreateDisposition` the FSD asks for, in `NtCreateFile` terms
/// (the Win32 `CreateFile` names in the comments are what the API layer
/// above turns into these). The discriminants ARE the ABI bytes the FSD
/// shifts into `create_options`'s high byte — `Disposition::Create as u32 == 2`
/// is a pinned fact, not a coincidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// `FILE_SUPERSEDE` (0): replace the file — content and attributes —
    /// or create it. Win32 `CREATE_ALWAYS`' supersede sibling.
    Supersede = 0,
    /// `FILE_OPEN` (1): the file must exist (Win32 `OPEN_EXISTING`, and
    /// the first half of `TRUNCATE_EXISTING`).
    Open = 1,
    /// `FILE_CREATE` (2): the file must not exist (Win32 `CREATE_NEW`).
    Create = 2,
    /// `FILE_OPEN_IF` (3): open it, create it when absent (Win32
    /// `OPEN_ALWAYS`).
    OpenIf = 3,
    /// `FILE_OVERWRITE` (4): the file must exist and its content is
    /// replaced.
    Overwrite = 4,
    /// `FILE_OVERWRITE_IF` (5): overwrite it, create it when absent
    /// (Win32 `CREATE_ALWAYS`).
    OverwriteIf = 5,
}

impl Disposition {
    /// Reads the disposition out of the raw `create_options`; `None` for
    /// a byte outside the six values above — the FSD itself answers
    /// `STATUS_INVALID_PARAMETER` for those (`fsop.c:933`), so this is a
    /// defensive arm, not a reachable one.
    pub fn from_create_options(create_options: u32) -> Option<Self> {
        match (create_options >> DISPOSITION_SHIFT) & 0xff {
            0 => Some(Self::Supersede),
            1 => Some(Self::Open),
            2 => Some(Self::Create),
            3 => Some(Self::OpenIf),
            4 => Some(Self::Overwrite),
            5 => Some(Self::OverwriteIf),
            _ => None,
        }
    }
}

/// What the caller is asking for: a file or a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateKind {
    /// A regular file (the staged write path).
    File,
    /// A directory (a `files` row, `Vfs::create_dir`).
    Directory,
}

/// The K43 disposition→intent table: one row per disposition, with the
/// three decisions the adapter actually needs spelled out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateIntent {
    /// File or directory.
    pub kind: CreateKind,
    /// The disposition this intent was derived from.
    pub disposition: Disposition,
    /// The name must not exist yet (an existing entry is a collision) —
    /// `FILE_CREATE` only.
    pub create_new: bool,
    /// The name may be created when it does not exist (`FILE_OPEN_IF` /
    /// `FILE_OVERWRITE_IF` / `FILE_SUPERSEDE`).
    pub create_if_missing: bool,
    /// The name must exist (`FILE_OPEN` / `FILE_OVERWRITE`).
    pub must_exist: bool,
    /// The content is replaced rather than extended in place
    /// (`FILE_SUPERSEDE` / `FILE_OVERWRITE` / `FILE_OVERWRITE_IF`).
    /// Windows implements this by posting a separate **Overwrite**
    /// transaction after the create/open response said
    /// `FILE_OVERWRITTEN` (`src/sys/create.c:1172-1228`), i.e. it lands
    /// in [`FileSystemContext::overwrite`], never in `create`.
    pub truncate: bool,
}

/// The K43 matrix, as a pure function of the two raw callback arguments.
///
/// Error contract: an unknown disposition byte is
/// `STATUS_INVALID_PARAMETER` (WinFsp's own answer for it), and a
/// directory request that also carries `FILE_NON_DIRECTORY_FILE` is
/// rejected as invalid, mirroring the FSD's kernel-side check
/// (`src/sys/create.c:393`: "if (FILE_NON_DIRECTORY_FILE && FILE_DIRECTORY_FILE) → invalid").
///
/// Kind determination (probed, not guessed): the FSD's kernel side
/// clears `FILE_ATTRIBUTE_NORMAL|DIRECTORY|REPARSE_POINT` from
/// `FileAttributes` and then sets `FILE_ATTRIBUTE_DIRECTORY` exactly when
/// `FILE_DIRECTORY_FILE` is in `CreateOptions`
/// (`src/sys/create.c:576-579`) — so **in `create` the two bits agree by
/// construction** and either one is authoritative. In `open` they do not:
/// the spike observed the same directory open arriving as `0x01004021`
/// (directory bit set) and `0x01204000` (no directory bit at all, both
/// disposition `FILE_OPEN`), which is why the open path resolves dir vs.
/// file from the row and never from these bits (`open_with_read`).
pub fn create_intent(
    create_options: u32,
    file_attributes: u32,
) -> std::result::Result<CreateIntent, FspError> {
    let disposition = Disposition::from_create_options(create_options)
        .ok_or_else(|| FspError::from(STATUS_INVALID_PARAMETER))?;
    let directory_bit = create_options & FILE_DIRECTORY_FILE != 0;
    let directory_attribute = file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if directory_bit && create_options & FILE_NON_DIRECTORY_FILE != 0 {
        return Err(STATUS_INVALID_PARAMETER.into());
    }
    let kind = if directory_bit || directory_attribute {
        CreateKind::Directory
    } else {
        CreateKind::File
    };
    Ok(CreateIntent {
        kind,
        disposition,
        create_new: matches!(disposition, Disposition::Create),
        create_if_missing: matches!(
            disposition,
            Disposition::OpenIf | Disposition::OverwriteIf | Disposition::Supersede
        ),
        must_exist: matches!(disposition, Disposition::Open | Disposition::Overwrite),
        truncate: matches!(
            disposition,
            Disposition::Supersede | Disposition::Overwrite | Disposition::OverwriteIf
        ),
    })
}

/// 100 ns ticks between the Windows epoch (1601-01-01) and the Unix
/// epoch — the FILETIME conversion offset every Win32 consumer expects.
const WINDOWS_EPOCH_OFFSET: u64 = 116_444_736_000_000_000;

/// Assembly-time volume numbers (K44): `get_volume_info` is one of the
/// callbacks Explorer and every copy engine hammer, so it answers from
/// this snapshot — no db query, no network, ever.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeSnapshot {
    /// Total visible capacity (used bytes at assembly + [`VOLUME_HEADROOM`]).
    pub total_size: u64,
    /// Free capacity (the headroom alone).
    pub free_size: u64,
    /// Volume label (WinFsp truncates it to 32 wide chars).
    pub label: String,
}

/// The stat block every metadata answer renders from.
#[derive(Clone, Debug, PartialEq)]
pub struct Meta {
    /// Directory row?
    pub is_dir: bool,
    /// Size in bytes (`0` for directories — the db's contract).
    pub size: u64,
    /// Legacy mtime in fractional Unix seconds (the db's contract).
    pub mtime: f64,
    /// Stable per-volume file index (the SQLite rowid; the root has none).
    pub index_number: u64,
}

impl Meta {
    /// The implicit volume root (no `files` row exists for `/`).
    pub fn root() -> Self {
        Self {
            is_dir: true,
            size: 0,
            mtime: 0.0,
            index_number: 0,
        }
    }

    /// The stat of a file that exists only in its staged bytes (no row
    /// yet): zero bytes at the create moment, index 0 — the db assigns
    /// the real rowid at commit, and until then the handle reports the
    /// staged length through `Handle::current_meta`.
    pub fn staged(mtime: f64) -> Self {
        Self {
            is_dir: false,
            size: 0,
            mtime,
            index_number: 0,
        }
    }

    /// Renders one `files` row.
    pub fn from_record(record: &FileRecord) -> Self {
        Self {
            is_dir: record.is_dir,
            size: record.size.max(0) as u64,
            mtime: record.mtime,
            index_number: record.id.max(0) as u64,
        }
    }

    /// Win32 attribute bits for this entry.
    pub fn attributes(&self) -> u32 {
        if self.is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_ARCHIVE
        }
    }

    /// `change_time` / `last_write_time` / ... all four time fields use it.
    pub fn filetime(&self) -> u64 {
        unix_to_filetime(self.mtime)
    }
}

/// One directory entry: the name plus the stat data the FSD renders in
/// the same call (K44 — `readdir` must not trigger a `getattr` storm).
#[derive(Clone, Debug, PartialEq)]
pub struct DirEntry {
    /// Final path segment, as stored (case preserved).
    pub name: String,
    /// Stat data.
    pub meta: Meta,
}

impl DirEntry {
    /// Renders one `files` row.
    pub fn from_record(record: &FileRecord) -> Self {
        Self {
            name: record.name.clone(),
            meta: Meta::from_record(record),
        }
    }
}

/// One open handle: what the FSD hangs off its file object between
/// `open`/`create` and `close`.
///
/// The metadata snapshot is taken at open/create time (K44 —
/// `get_file_info` never re-reads the db) and refreshed once a staged
/// write commits, the read state (WF2) is the K33 dispatch result for
/// files, the write state (WF3) is the staged commit target, and the
/// directory enumeration buffer arrives on the first fresh
/// `read_directory`. `DirBuffer` is interior-mutable by design
/// (winfsp-rs hands out `&FileContext` from several threads), so the
/// framework's shared reference is all this needs — the same reason the
/// stat snapshot, the read slot, the write slot and the delete mark are
/// interior-mutable too.
pub struct Handle {
    rel: RelPath,
    /// The stat snapshot every `FileInfo` renders from, refreshed after a
    /// commit. Behind a mutex because `cleanup` (and the lazy read
    /// acquisition) mutate it through `&self`, which is all WinFsp gives
    /// a callback.
    meta: Mutex<Meta>,
    /// The read state of a file handle (K41: shared with the grace table,
    /// so a reopen inside the grace window reuses the live window buffer
    /// instead of rebuilding it). `None` for directories and for a
    /// freshly created file (the committed bytes do not exist yet) —
    /// `read` acquires it lazily.
    read: Mutex<Option<Arc<AsyncMutex<ReadHandle>>>>,
    /// The staged write state of a file handle (WF3/K43): `Some` while
    /// uncommitted bytes are staged. Created by `create`/`overwrite` (and
    /// lazily by the first `write`/`set_file_size` of a handle Windows
    /// opened for writing), taken exactly once by the cleanup commit or
    /// the delete-on-close abort. Deliberately NOT an `Arc` shared with
    /// any table (unlike the read state): a commit is destructive and
    /// must happen exactly once, for this handle only.
    write: Mutex<Option<StagedWriter>>,
    /// The delete-on-close mark (`set_delete`). Bookkeeping only: the
    /// authority for the delete at cleanup is the FSD's own
    /// `FspCleanupDelete` flag, which it derives from the file object's
    /// delete disposition (`src/sys/cleanup.c:92`, `Delete = CleanupFlags & 1`) —
    /// this mark is what `set_delete(false)` clears and what tests observe.
    delete_pending: AtomicBool,
    /// Directory enumeration buffer, created on the first fresh
    /// `read_directory` — the only WinFsp DLL call this adapter makes
    /// (`DirBuffer`'s acquire/fill/read/delete are DLL exports, and its
    /// `Drop` is one too). Lazy on purpose: a handle that is only ever
    /// stat'ed must not touch the DLL, which is also what lets the
    /// metadata tests run on boxes without WinFsp installed.
    dir_buffer: Mutex<Option<DirBuffer>>,
}

impl Handle {
    /// The VFS path this handle was opened on.
    pub fn rel(&self) -> &RelPath {
        &self.rel
    }

    /// The stat snapshot taken at open time (the committed state; a
    /// pending staged write is visible through [`Handle::current_meta`]).
    pub fn meta(&self) -> Meta {
        self.meta
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The live stat: the open-time snapshot with the staged writer's
    /// length and requested mtime applied while uncommitted bytes exist.
    /// Every `FileInfo` a write-path callback returns comes from here, so
    /// Explorer's copy progress sees the bytes it just wrote.
    fn current_meta(&self) -> Meta {
        let mut meta = self.meta();
        if let Some(writer) = self
            .write
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            if let Ok(len) = writer.len() {
                meta.size = len;
            }
            if let Some(mtime) = writer.mtime() {
                meta.mtime = mtime;
            }
        }
        meta
    }

    /// Replaces the stat snapshot (post-commit refresh, K44's "one db
    /// read per open" rule extended to "plus one after a commit").
    fn set_meta(&self, meta: Meta) {
        *self
            .meta
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = meta;
    }

    /// Directory handle? (`read_directory` refuses files.)
    pub fn is_dir(&self) -> bool {
        self.meta().is_dir
    }

    /// The read state slot, cloned out (files only; `read` serves through
    /// it). Cloning the `Arc` — not holding the mutex — is what lets the
    /// read path take the async read lock without ever nesting the two
    /// locks.
    fn read_state(&self) -> Option<Arc<AsyncMutex<ReadHandle>>> {
        self.read
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Installs a lazily acquired read state (the `read` path's slow arm).
    fn set_read_state(&self, read: Option<Arc<AsyncMutex<ReadHandle>>>) {
        *self
            .read
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = read;
    }

    /// The staged write slot (WF3: `write`/`overwrite`/`set_file_size`
    /// stage through it; `cleanup` commits from it).
    fn write_state(&self) -> MutexGuard<'_, Option<StagedWriter>> {
        self.write
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Installs a staged writer (the create/overwrite constructor shape).
    fn set_write(&self, writer: StagedWriter) {
        *self.write_state() = Some(writer);
    }

    /// Takes the staged writer out of the handle: the commit (and the
    /// delete abort) happen exactly once because this can only succeed
    /// once.
    fn take_write(&self) -> Option<StagedWriter> {
        self.write_state().take()
    }

    /// Whether uncommitted staged bytes are still owned by this handle
    /// (`rename` refuses to move a path that has them, `delete` discards
    /// them).
    pub fn has_pending_write(&self) -> bool {
        self.write_state().is_some()
    }

    /// The delete-on-close mark (`set_delete`).
    pub fn delete_mark(&self) -> bool {
        self.delete_pending.load(Ordering::Relaxed)
    }

    /// Sets/clears the delete-on-close mark (`set_delete`).
    fn mark_delete(&self, pending: bool) {
        self.delete_pending.store(pending, Ordering::Relaxed);
    }

    /// Destructures a closing handle: its path plus the read state the
    /// grace table takes back (WF3's cleanup reads the same two).
    fn into_grace_parts(self) -> (RelPath, Option<Arc<AsyncMutex<ReadHandle>>>) {
        let Handle { rel, read, .. } = self;
        let read = read
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (rel, read)
    }

    /// The enumeration buffer slot (poison recovery per code-style §2).
    fn dir_buffer(&self) -> MutexGuard<'_, Option<DirBuffer>> {
        self.dir_buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn new(rel: RelPath, meta: Meta, read: Option<Arc<AsyncMutex<ReadHandle>>>) -> Self {
        Self {
            rel,
            meta: Mutex::new(meta),
            read: Mutex::new(read),
            write: Mutex::new(None),
            delete_pending: AtomicBool::new(false),
            dir_buffer: Mutex::new(None),
        }
    }
}

/// One entry of the K41 grace table: the read state of a closed handle
/// plus the moment it stops being reusable.
struct GraceEntry {
    /// The closed handle's read state (shared, never rebuilt on reuse).
    read: Arc<AsyncMutex<ReadHandle>>,
    /// `Instant` after which a reopen must build a fresh state.
    expires_at: Instant,
    /// The row size the state was parked under (the handle's stat
    /// snapshot). Review H1: a delete + recreate behind the same path
    /// leaves the parked state describing a file that no longer exists —
    /// the reopen's fresh row size is the witness that discards it.
    size: u64,
}

/// The handle grace table (K41), keyed by path.
///
/// Semantics straight from rclone's `--vfs-handle-caching 5s`
/// (`vfscache/item.go:703-752`): closing a handle does not tear its read
/// state down; the state is parked here, and a reopen within the grace
/// window reuses it — the cure for a player or an antivirus scanner that
/// closes and immediately reopens the same file.
///
/// Expiry is LAZY (checked whenever the table is touched), not
/// timer-driven: a reopen is the only event that can observe expiry, so a
/// background sweeper per close would add spawned tasks and test
/// nondeterminism for no observable difference. The bound on retained
/// state is the capacity, applied on insert (oldest close evicted
/// first), so the table cannot grow without limit even when nothing ever
/// expires it.
#[derive(Default)]
struct GraceTable {
    entries: HashMap<RelPath, GraceEntry>,
}

impl GraceTable {
    /// Drops every entry whose grace window has passed.
    fn prune_expired(&mut self, now: Instant) {
        self.entries.retain(|_, entry| now < entry.expires_at);
    }

    /// Parks `read` under `rel`, sweeping expired entries and keeping
    /// the table within `capacity` (the entry closest to expiry — the
    /// oldest close — goes first). `size` is the row size the state was
    /// parked under (the handle's stat snapshot).
    fn park(
        &mut self,
        rel: RelPath,
        read: Arc<AsyncMutex<ReadHandle>>,
        expires_at: Instant,
        now: Instant,
        capacity: usize,
        size: u64,
    ) {
        self.prune_expired(now);
        self.entries.insert(
            rel,
            GraceEntry {
                read,
                expires_at,
                size,
            },
        );
        while self.entries.len() > capacity.max(1) {
            let victim = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(rel, _)| rel.clone());
            match victim {
                Some(rel) => {
                    self.entries.remove(&rel);
                }
                // Unreachable while the table is over capacity; the loop
                // guard keeps a hand-edited capacity from spinning.
                None => break,
            }
        }
    }

    /// Takes the live entry for `rel`, if any. Expired entries are
    /// dropped, never handed out; so is a parked state whose size
    /// disagrees with the reopened row's size — it describes a different
    /// file behind the same path (review H1: delete + recreate inside the
    /// grace window reused the parked EOF and truncated the new file).
    fn take_live(
        &mut self,
        rel: &RelPath,
        now: Instant,
        row_size: u64,
    ) -> Option<Arc<AsyncMutex<ReadHandle>>> {
        self.prune_expired(now);
        match self.entries.remove(rel) {
            Some(entry) if entry.size == row_size => Some(entry.read),
            Some(_) => None,
            None => None,
        }
    }

    /// Drops the entry for `rel`, whatever its expiry (review H1):
    /// delete, rename and commit are events a parked read state must not
    /// survive.
    fn invalidate(&mut self, rel: &RelPath) {
        self.entries.remove(rel);
    }
}

/// The adapter: one instance per mounted volume.
pub struct CloudFs {
    vfs: Arc<Vfs>,
    bridge: AsyncBridge,
    /// The volume numbers behind `get_volume_info` (K44) — behind a mutex
    /// because `set_volume_label` renames the volume in place
    /// (process-level, K44: the label is not persisted anywhere).
    volume: Mutex<VolumeSnapshot>,
    /// Streaming-read window for new fetches (K34; [`crate::reader::DEFAULT_READ_WINDOW`]
    /// in production, shrinkable through [`CloudFs::with_stream_window`]).
    stream_window: u64,
    /// The K41 grace table: read states of closed handles, keyed by path.
    grace: Mutex<GraceTable>,
    /// How long a closed handle's read state stays reusable (K41).
    grace_period: Duration,
    /// Upper bound on grace table entries.
    grace_capacity: usize,
}

/// K41 default grace period: rclone's `--vfs-handle-caching 5s`, chosen
/// for exactly the same failure mode (a player or a scanner that closes
/// and immediately reopens the same file).
pub const DEFAULT_HANDLE_GRACE: Duration = Duration::from_secs(5);

/// Upper bound on parked read states. Each one may hold up to one
/// [`crate::reader::DEFAULT_READ_WINDOW`] of buffered bytes (only once it
/// has actually been read through), so the table's worst case is
/// capacity × window = 256 MiB; 64 is rclone's own order of magnitude for
/// "files recently touched" and is far more than a desktop workload
/// keeps closing at once. Expired entries are swept on every touch
/// (open/close), so the table drains as soon as any handle moves.
pub const DEFAULT_GRACE_CAPACITY: usize = 64;

impl CloudFs {
    /// Assembles the adapter over an already-built VFS.
    ///
    /// `rt` is the process runtime's handle (the mount owns the runtime;
    /// see [`AsyncBridge`]). The volume snapshot is taken here — K44's
    /// "assembly-time", the only moment this type reads the usage stats.
    pub fn new(vfs: Arc<Vfs>, rt: tokio::runtime::Handle, label: impl Into<String>) -> Self {
        let used = match vfs.db().get_stats() {
            Ok(stats) => stats.total_bytes.max(0) as u64,
            Err(error) => {
                // Best-effort: an unreadable usage stat must not refuse a
                // mount; the volume simply reports an empty footprint.
                tracing::warn!(
                    error = %error,
                    "winfsp: volume usage snapshot failed; reporting 0 bytes used"
                );
                0
            }
        };
        Self {
            vfs,
            bridge: AsyncBridge::new(rt),
            volume: Mutex::new(VolumeSnapshot {
                total_size: used + VOLUME_HEADROOM,
                free_size: VOLUME_HEADROOM,
                label: label.into(),
            }),
            stream_window: crate::reader::DEFAULT_READ_WINDOW,
            grace: Mutex::new(GraceTable::default()),
            grace_period: DEFAULT_HANDLE_GRACE,
            grace_capacity: DEFAULT_GRACE_CAPACITY,
        }
    }

    /// Overrides the streaming-read window (a testing seam, same shape as
    /// the WebDAV adapter's `with_stream_window`: small windows turn
    /// multi-window read sequences observable without multi-megabyte
    /// fixtures; production keeps [`crate::reader::DEFAULT_READ_WINDOW`]).
    pub fn with_stream_window(mut self, window: u64) -> Self {
        self.stream_window = window.max(1);
        self
    }

    /// Overrides the K41 handle grace period (production: [`DEFAULT_HANDLE_GRACE`];
    /// tests inject milliseconds so "inside the window" and "after it"
    /// are cheap to pin).
    pub fn with_handle_grace(mut self, grace: Duration) -> Self {
        self.grace_period = grace;
        self
    }

    /// Overrides the grace table capacity (production: [`DEFAULT_GRACE_CAPACITY`];
    /// tests shrink it to pin the eviction order).
    pub fn with_grace_capacity(mut self, capacity: usize) -> Self {
        self.grace_capacity = capacity.max(1);
        self
    }

    /// The injected async bridge (the read path awaits on it).
    pub fn bridge(&self) -> &AsyncBridge {
        &self.bridge
    }

    /// The assembly-time volume snapshot behind `get_volume_info` (a
    /// clone — `set_volume_label` may replace it at any time).
    pub fn volume(&self) -> VolumeSnapshot {
        self.volume
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Lists one directory off the rows (dirs first, then name-ascending
    /// — the db's Python-parity order).
    ///
    /// K44: the whole listing carries its stat data; the FSD never has to
    /// ask again per entry.
    pub fn dir_entries(&self, rel: &RelPath) -> std::result::Result<Vec<DirEntry>, FspError> {
        let rows = self
            .vfs
            .db()
            .list_dir(rel.as_str())
            .map_err(|error| fsp_error(&VfsError::Db(error)))?;
        Ok(rows.iter().map(DirEntry::from_record).collect())
    }

    /// The DLL-free half of `read_directory` (split out so it is
    /// testable on boxes without WinFsp installed — the `DirBuffer`
    /// half past it calls into the WinFsp DLL).
    ///
    /// `Some(entries)` means "this call starts a fresh enumeration, write
    /// these"; `None` means "marker continuation, the buffer already
    /// holds the listing". A file handle is refused before anything else.
    pub fn prepare_enumeration(
        &self,
        context: &Handle,
        marker_is_none: bool,
    ) -> std::result::Result<Option<Vec<DirEntry>>, FspError> {
        if !context.is_dir() {
            return Err(STATUS_NOT_A_DIRECTORY.into());
        }
        if marker_is_none {
            Ok(Some(self.dir_entries(context.rel())?))
        } else {
            Ok(None)
        }
    }

    /// Resolves the stat block for one path (the root is implicit — no
    /// `files` row exists for `/`).
    fn meta_for(&self, rel: &RelPath) -> std::result::Result<Meta, FspError> {
        if rel.is_root() {
            return Ok(Meta::root());
        }
        self.resolve_row(rel)?
            .as_ref()
            .map(Meta::from_record)
            .ok_or_else(|| STATUS_OBJECT_NAME_NOT_FOUND.into())
    }

    /// The row for `rel` under Windows case semantics: the exact
    /// spelling first, then a case-insensitive scan of the parent's
    /// listing. The FSD resolves names case-insensitively and may hand
    /// the adapter another case form — rename sources notably arrive
    /// upcased, and Win32 apps pass arbitrary spellings — so every
    /// lookup goes through here. An ambiguous parent (two rows
    /// differing only by case) keeps the exact-miss result rather than
    /// guessing.
    fn resolve_row(&self, rel: &RelPath) -> std::result::Result<Option<FileRecord>, FspError> {
        let db = self.vfs.db();
        if let Some(record) = db
            .get_file(rel.as_str())
            .map_err(|error| fsp_error(&VfsError::Db(error)))?
        {
            return Ok(Some(record));
        }
        if rel.is_root() {
            return Ok(None);
        }
        let parent_dir = rel
            .parent()
            .map(|parent| parent.as_str().to_string())
            .unwrap_or_else(|| "/".to_string());
        let entries = db
            .list_dir(&parent_dir)
            .map_err(|error| fsp_error(&VfsError::Db(error)))?;
        let wanted = rel.name().to_lowercase();
        let mut hit = None;
        for record in entries {
            if record.name.to_lowercase() == wanted {
                if hit.is_some() {
                    return Ok(None);
                }
                hit = Some(record);
            }
        }
        Ok(hit)
    }

    /// Opens one path's metadata: `get_file_info` and `read_directory`
    /// then work off the returned handle alone. No read state and no
    /// remote work (WF1's DLL-free seam, still used by enumerations).
    pub fn open_handle(&self, rel: &RelPath) -> std::result::Result<Handle, FspError> {
        let meta = self.meta_for(rel)?;
        Ok(Handle::new(rel.clone(), meta, None))
    }

    /// The FSD's `open`: the metadata snapshot plus, for files, the K33
    /// read state (WF2).
    ///
    /// The dispatch is `Vfs::open_read`'s triple gate, resolved here into
    /// a live reader:
    ///
    /// - `Stream` → [`WindowReader`] (bounded `open_range` windows);
    /// - `Hydrate` → [`LocalReader`] over `Vfs::hydrate`'s local path —
    ///   the arm WF0's cache-first probe routes cache hits into, so a
    ///   warm file opens with zero remote traffic;
    /// - `Err` → the K45 table, so `MissingPassword` is
    ///   `STATUS_ACCESS_DENIED` at open.
    ///
    /// The error arm is not softened into a hydrate fallback (the WebDAV
    /// adapter's shape): `open_read` already probed the cache and the
    /// row, so the hydrate path would re-derive the same failure — an
    /// unreadable file should fail its open, not its first read.
    fn open_with_read(&self, rel: &RelPath) -> std::result::Result<Handle, FspError> {
        if rel.is_root() {
            return Ok(Handle::new(rel.clone(), Meta::root(), None));
        }
        // Resolve to the canonical spelling up front (case-insensitive
        // Windows semantics): the read state, grace key and cleanup
        // commit all key off the handle's rel, so they must never carry
        // a caller's case variant.
        let record = self
            .resolve_row(rel)?
            .ok_or_else(|| FspError::from(STATUS_OBJECT_NAME_NOT_FOUND))?;
        let canonical = RelPath::new(&record.rel_path).expect("db rows carry canonical rel paths");
        let meta = Meta::from_record(&record);
        let read = if meta.is_dir {
            // Directories have no read state; their handle only ever
            // serves metadata and enumeration.
            None
        } else {
            Some(self.acquire_read(&canonical, meta.size)?)
        };
        Ok(Handle::new(canonical, meta, read))
    }

    /// Drops any parked read state for `rel` (review H1): delete, rename
    /// and commit are events a parked read state must not survive — a
    /// reopen after them must build a fresh state.
    fn grace_invalidate(&self, rel: &RelPath) {
        self.grace
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .invalidate(rel);
    }

    /// The read state for one file open: the parked one when the K41
    /// grace table still holds it, otherwise a fresh K33 dispatch.
    ///
    /// `row_size` is the fresh row's size (review H1): a parked state
    /// whose size disagrees describes a different file behind the same
    /// path and is discarded instead of reused.
    fn acquire_read(
        &self,
        rel: &RelPath,
        row_size: u64,
    ) -> std::result::Result<Arc<AsyncMutex<ReadHandle>>, FspError> {
        let now = Instant::now();
        if let Some(parked) = self
            .grace
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take_live(rel, now, row_size)
        {
            return Ok(parked);
        }
        let source = self
            .bridge
            .block_on(self.vfs.open_read(rel))
            .map_err(|error| fsp_error(&error))?;
        let read = match source {
            StreamSource::Stream {
                handle,
                total_size,
                transport,
            } => ReadHandle::Window(WindowReader::new(
                handle,
                total_size,
                transport,
                self.stream_window,
            )),
            StreamSource::Hydrate => {
                let local = self
                    .bridge
                    .block_on(self.vfs.hydrate(rel))
                    .map_err(|error| fsp_error(&error))?;
                let reader =
                    LocalReader::open(&local).map_err(|error| fsp_error(&VfsError::Io(error)))?;
                ReadHandle::Local(reader)
            }
        };
        Ok(Arc::new(AsyncMutex::new(read)))
    }

    // ------------------------------------------------------------ rows ---

    /// One `files` row, or `None` (`get_security_by_name` renders the
    /// 0-byte descriptor for absent names itself).
    fn row(&self, rel: &RelPath) -> std::result::Result<Option<FileRecord>, FspError> {
        self.resolve_row(rel)
    }

    /// The parent gate for a create or a rename destination.
    ///
    /// `Vfs::create_dir` carries this check for directories, but nothing
    /// does for files: staging bytes for a path whose parent row does not
    /// exist would scatter cache directories for a path that can never
    /// commit. The root needs no row and always passes.
    fn require_dir_parent(&self, rel: &RelPath) -> std::result::Result<(), FspError> {
        match rel.parent() {
            Some(parent) if !parent.is_root() => match self.row(&parent)? {
                Some(row) if row.is_dir => Ok(()),
                _ => Err(STATUS_OBJECT_PATH_NOT_FOUND.into()),
            },
            _ => Ok(()),
        }
    }

    // -------------------------------------------------- write staging ---

    /// The K43 create half, DLL-free (the callback past it only fills the
    /// reply's `FileInfo`): the disposition matrix, then a directory row
    /// or a staged write handle.
    ///
    /// A file create stages an EMPTY sibling and never seeds from
    /// existing content, because `create` is only reached for a name that
    /// is being created: FILE_CREATE directly, and the
    /// OPEN_IF/OVERWRITE_IF arms only after `Open` answered
    /// `STATUS_OBJECT_NAME_NOT_FOUND` (`src/dll/fsop.c:918-941`). An
    /// existing name in `create` (the FILE_CREATE collision) is reported
    /// as a collision — which the FSD may rewrite into
    /// `STATUS_FILE_IS_A_DIRECTORY` when the caller asked for a file and
    /// found a directory (`FspFileSystemOpCreate_CollisionCheck`).
    /// Existing content is replaced through the separate Overwrite
    /// transaction, never here (`src/sys/create.c:1172-1228`).
    pub fn prepare_create(
        &self,
        rel: &RelPath,
        create_options: u32,
        file_attributes: u32,
    ) -> std::result::Result<Handle, FspError> {
        let intent = create_intent(create_options, file_attributes)?;
        if self.row(rel)?.is_some() {
            return Err(STATUS_OBJECT_NAME_COLLISION.into());
        }
        match intent.kind {
            CreateKind::Directory => {
                self.vfs
                    .create_dir(rel)
                    .map_err(|error| fsp_error(&error))?;
                // `create_dir` published the row: read the real stat back
                // (ids, mtime) instead of synthesizing one.
                let meta = self.meta_for(rel)?;
                Ok(Handle::new(rel.clone(), meta, None))
            }
            CreateKind::File => {
                self.require_dir_parent(rel)?;
                let writer = StagedWriter::create_empty(rel.clone(), &self.vfs.local_path(rel))
                    .map_err(io_error)?;
                let handle = Handle::new(rel.clone(), Meta::staged(unix_now()), None);
                handle.set_write(writer);
                Ok(handle)
            }
        }
    }

    /// The staged writer of `handle`, materialised on first use, with the
    /// write slot's lock held for the caller.
    ///
    /// A handle Windows opened for writing without an overwrite
    /// (`FILE_OPEN_IF` on an existing file, `TRUNCATE_EXISTING`, or plain
    /// `OPEN_EXISTING` + `WriteFile`) has no writer yet: the bytes that
    /// follow must not lose the existing content, so the staging sibling
    /// starts as a copy of the plaintext the hydrate arm serves
    /// (`Vfs::hydrate`; WF0's cache-first probe makes a warm file free,
    /// and `put_staged` uploads the whole file anyway, so a partial
    /// update needs the whole content staged regardless).
    ///
    /// The slot's lock is held across the hydrate call on purpose: it is
    /// a plain `std` guard (never taken by the awaited future) and two
    /// dispatcher threads writing one handle must not each materialise a
    /// staging copy.
    fn writer_slot<'a>(
        &self,
        handle: &'a Handle,
    ) -> std::result::Result<MutexGuard<'a, Option<StagedWriter>>, FspError> {
        let mut slot = handle.write_state();
        if slot.is_none() {
            let rel = handle.rel();
            let source = self
                .bridge
                .block_on(self.vfs.hydrate(rel))
                .map_err(|error| fsp_error(&error))?;
            let writer =
                StagedWriter::create_seeded(rel.clone(), &self.vfs.local_path(rel), &source)
                    .map_err(io_error)?;
            *slot = Some(writer);
        }
        Ok(slot)
    }

    /// The DLL-free half of `write`: stage `buffer` at `offset` (or at the
    /// staged EOF when the FSD set `write_to_eof`, its `Offset == -1`
    /// convention — `src/dll/fsop.c:1053`).
    ///
    /// `constrained_io` means "must not extend the file" (winfsp.h:460):
    /// the write is clamped to the staged EOF and a shorter count is
    /// reported, which is the short-write semantics the FSD expects.
    pub fn write_into(
        &self,
        handle: &Handle,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
    ) -> std::result::Result<usize, FspError> {
        if handle.is_dir() {
            // The I/O manager never grants write access to a directory
            // handle; this is the defensive answer, not a reachable path.
            return Err(STATUS_INVALID_DEVICE_REQUEST.into());
        }
        let mut slot = self.writer_slot(handle)?;
        let Some(writer) = slot.as_mut() else {
            // `writer_slot` just guaranteed a writer; never reachable.
            return Err(STATUS_INVALID_DEVICE_REQUEST.into());
        };
        let staged_len = writer.len().map_err(io_error)?;
        let position = if write_to_eof { staged_len } else { offset };
        let mut data = buffer;
        if constrained_io {
            let room = staged_len.saturating_sub(position);
            if room < data.len() as u64 {
                data = &data[..room as usize];
            }
        }
        writer.write_at(position, data).map_err(io_error)
    }

    /// The DLL-free half of `overwrite` (the FSD's Overwrite transaction,
    /// posted after every create/open whose response said
    /// FILE_OVERWRITTEN/FILE_SUPERSEDED — `src/sys/create.c:1172-1228`):
    /// the content is REPLACED, so nothing is seeded and any writer
    /// already staged is discarded first. `allocation_size` is the
    /// caller's requested length for the new content (0 in the common
    /// CREATE_ALWAYS case) — the FSD sends the separate SetFileSize
    /// requests for anything more precise.
    pub fn overwrite_staged(
        &self,
        handle: &Handle,
        allocation_size: u64,
    ) -> std::result::Result<(), FspError> {
        if handle.is_dir() {
            return Err(STATUS_INVALID_DEVICE_REQUEST.into());
        }
        if let Some(staged) = handle.take_write() {
            staged.abort();
        }
        let mut writer =
            StagedWriter::create_empty(handle.rel().clone(), &self.vfs.local_path(handle.rel()))
                .map_err(io_error)?;
        if allocation_size > 0 {
            writer.set_len(allocation_size).map_err(io_error)?;
        }
        handle.set_write(writer);
        Ok(())
    }

    /// The DLL-free half of `set_file_size`.
    ///
    /// File-size sets (`set_allocation_size = false`) resize the staged
    /// bytes exactly: a truncation to 0 rebuilds the staging sibling
    /// empty (nothing to copy — that is Win32 `TRUNCATE_EXISTING`'s
    /// shape), every other resize materialises the staged copy first when
    /// the handle has none. Allocation-size sets are the hint WinFsp
    /// documents (winfsp.h's SetFileSize rules): they never move the EOF,
    /// so they are a no-op unless the new allocation is smaller than the
    /// current file, in which case the file is truncated to it.
    ///
    /// Redundant sizes are no-ops too, so a `SetEndOfFile(same size)`
    /// costs neither a hydrate nor a commit.
    pub fn resize_staged(
        &self,
        handle: &Handle,
        new_size: u64,
        set_allocation_size: bool,
    ) -> std::result::Result<(), FspError> {
        if handle.is_dir() {
            return Err(STATUS_INVALID_DEVICE_REQUEST.into());
        }
        let current = handle.current_meta().size;
        if set_allocation_size && new_size >= current {
            return Ok(());
        }
        if !set_allocation_size && new_size == current {
            return Ok(());
        }
        if !set_allocation_size && new_size == 0 {
            return self.overwrite_staged(handle, 0);
        }
        let mut slot = self.writer_slot(handle)?;
        let Some(writer) = slot.as_mut() else {
            return Err(STATUS_INVALID_DEVICE_REQUEST.into());
        };
        writer.set_len(new_size).map_err(io_error)
    }

    /// The `files` row's committed mtime becomes this writer's, when the
    /// FSD asked for one (`set_basic_info`'s last-write time). A
    /// committed row's mtime cannot be updated through this adapter:
    /// `MetaDatabase` exposes no mtime setter and the column is
    /// upload-owned (the same limitation the WebDAV PROPPATCH handler
    /// documents) — accepted and dropped rather than refused, so Explorer
    /// keeps its copy.
    pub fn stage_mtime(&self, handle: &Handle, last_write_time: u64) {
        let Some(mtime) = filetime_to_unix(last_write_time) else {
            return;
        };
        let mut slot = handle.write_state();
        if let Some(writer) = slot.as_mut() {
            writer.set_mtime(mtime);
        }
    }

    // ---------------------------------------------------------- delete ---

    /// The delete gate (`CanDelete`'s job): the FSD's `SetDelete`
    /// callback is the only reportable one — winfsp-rs 0.13 does not
    /// expose `CanDelete`, and winfsp.h says "If both CanDelete and
    /// SetDelete are defined, SetDelete takes precedence" — so both
    /// `set_delete` and the cleanup delete run this.
    ///
    /// Refusals:
    /// - a directory that still has children: `STATUS_DIRECTORY_NOT_EMPTY`
    ///   (what `RemoveDirectory` reports; the WebDAV adapter's `Exists`
    ///   for the same case);
    /// - a **pending upload whose local copy still exists**: the shared
    ///   adjudication (`Vfs::remove_file`, the WebDAV adapter's
    ///   `remove_file`) — that copy is the only copy of the bytes, so
    ///   deleting it would orphan the queued job. `VfsError::UploadPending`
    ///   rides the K45 table to `STATUS_SHARING_VIOLATION` ("file in use").
    ///
    /// An absent row is deletable: a file that only exists in its staged
    /// bytes has nothing published to delete (and its staging sibling is
    /// discarded by the cleanup's abort).
    pub fn can_delete(&self, rel: &RelPath) -> std::result::Result<(), FspError> {
        let Some(row) = self.row(rel)? else {
            return Ok(());
        };
        if row.is_dir {
            let children = self
                .vfs
                .db()
                .list_dir(rel.as_str())
                .map_err(|error| fsp_error(&VfsError::Db(error)))?;
            if !children.is_empty() {
                return Err(STATUS_DIRECTORY_NOT_EMPTY.into());
            }
            return Ok(());
        }
        if !row.is_uploaded && self.vfs.local_copy_exists(rel) {
            return Err(fsp_error(&VfsError::UploadPending(
                rel.as_str().to_string(),
            )));
        }
        Ok(())
    }

    /// Deletes one entry through the shared VFS seams: files go through
    /// [`Vfs::remove_file`] (its own pending guard, the K4 remote gate,
    /// the row and the cache copy), directories through the K4 gate
    /// ([`Vfs::delete_remote_for_row`]) and then the row — the WebDAV
    /// adapter's `remove_dir` shape.
    async fn delete_entry(&self, rel: &RelPath) -> std::result::Result<(), VfsError> {
        match self.vfs.db().get_file(rel.as_str())? {
            None => Ok(()),
            Some(row) if row.is_dir => {
                self.vfs.delete_remote_for_row(rel).await?;
                self.vfs.db().delete_file(rel.as_str())?;
                Ok(())
            }
            Some(_) => self.vfs.remove_file(rel).await,
        }
    }

    /// The delete arm of `cleanup`: best-effort, because cleanup cannot
    /// report failure (winfsp.h). The guard runs again here — the
    /// `set_delete` call that normally precedes it is skipped entirely by
    /// the FILE_DELETE_ON_CLOSE shape — and a refusal keeps the file and
    /// lands in the log.
    ///
    /// `file_name` is the FSD's name for the entry ("Sent only when a
    /// Delete is requested", winfsp.h); the handle's own path is the
    /// fallback.
    fn delete_after_cleanup(&self, context: &Handle, file_name: Option<&U16CStr>) {
        let rel = match file_name {
            Some(name) => match rel_from_winfsp(name) {
                Ok(rel) => rel,
                Err(error) => {
                    tracing::error!(
                        %error,
                        "winfsp: cleanup delete with an unrepresentable name; kept"
                    );
                    return;
                }
            },
            None => context.rel().clone(),
        };
        // The FSD's delete name may be upcased (case-insensitive
        // resolution, same as rename sources): map it onto the canonical
        // row before the guard and the delete — `delete_entry`'s
        // not-found arm is a deliberate silent success, so an unresolved
        // spelling would delete nothing while reporting none of it.
        let rel = match self.resolve_row(&rel) {
            Ok(Some(record)) => {
                RelPath::new(&record.rel_path).expect("db rows carry valid rel paths")
            }
            Ok(None) => rel,
            Err(error) => {
                tracing::error!(
                    %error,
                    "winfsp: cleanup delete lookup failed; the entry was kept"
                );
                return;
            }
        };
        if let Err(error) = self.can_delete(&rel) {
            tracing::error!(
                rel_path = %rel,
                %error,
                "winfsp: cleanup delete refused; the entry was kept"
            );
            return;
        }
        match self.bridge.block_on(self.delete_entry(&rel)) {
            Ok(()) => {
                // Review H1: the row and the cache copy are gone — a
                // parked state for this path is stale by construction,
                // and the dying handle's own read state must not be
                // parked by the close that follows this cleanup.
                self.grace_invalidate(&rel);
                context.set_read_state(None);
            }
            Err(error) => tracing::error!(
                rel_path = %rel,
                %error,
                "winfsp: cleanup delete failed; the entry was kept"
            ),
        }
    }

    /// Moves `from` to `to`, porting the WebDAV adapter's `rename`
    /// (`cloudkit-webdav` src/lib.rs:388-431) with the WinFsp API's
    /// extras:
    ///
    /// - a **case-only rename** — the destination equals the canonical
    ///   source up to case, the shape every FSD-delivered upcased source
    ///   produces — is a legal rename: the row and the cache copy move to
    ///   the new spelling, nothing is refused or replaced (review C1;
    ///   the WebDAV face needs no equivalent because its row lookups are
    ///   byte-exact, so a case-variant destination never resolves onto
    ///   the source row there);
    /// - the destination is replaced only when the FSD says
    ///   `replace_if_exists` (Win32 `ReplaceIfExists`) — without it an
    ///   existing destination is `STATUS_OBJECT_NAME_COLLISION`;
    /// - a directory on EITHER side is a collision (the WebDAV
    ///   `FsError::Exists`), the destination's parent must exist;
    /// - the row moves in place (`id`/chunk linkage preserved, the remote
    ///   messages stay — no re-upload, no delete) and the cache copy
    ///   follows (`move_tree_best_effort` for subtrees);
    /// - **guards run before anything is mutated**: a handle with
    ///   uncommitted staged bytes is refused (the staging sibling has no
    ///   new name to follow), and both endpoints refuse while their bytes
    ///   are only local — the queue resolves a job by `rel_path`
    ///   (`upload_queue.rs:561`), so moving a pending row would orphan
    ///   its upload, and deleting a pending destination would drop the
    ///   only copy of the bytes. Both ride `UploadPending` →
    ///   `STATUS_SHARING_VIOLATION`.
    pub fn rename_entry(
        &self,
        handle: &Handle,
        from: &RelPath,
        to: &RelPath,
        replace_if_exists: bool,
    ) -> std::result::Result<(), FspError> {
        if handle.has_pending_write() {
            return Err(fsp_error(&VfsError::UploadPending(
                from.as_str().to_string(),
            )));
        }
        let row = self
            .row(from)?
            .ok_or_else(|| FspError::from(STATUS_OBJECT_NAME_NOT_FOUND))?;
        // The FSD may deliver the source upcased (case-insensitive
        // resolution): everything downstream keys off the canonical
        // spelling the row carries.
        let from = RelPath::new(&row.rel_path).expect("db rows carry valid rel paths");
        // Case-only rename (review C1): the canonical source and the
        // destination agree up to case — including the byte-equal shape
        // an upcased source produces — so there is no distinct
        // destination. The old raw `from == to` guard refused the most
        // common case rename outright (ACCESS_DENIED), and a case-variant
        // destination resolved onto THIS row in the overwrite branch
        // below and deleted it — the row plus, through NTFS's
        // case-insensitive remove, the cache copy — while the same-row
        // `rename_path` updated zero rows and still answered Ok. A case
        // rename is just a rename: move the row, let the case-insensitive
        // local rename flip the cache copy's spelling, and invalidate the
        // grace table (review H1).
        if from.as_str().to_lowercase() == to.as_str().to_lowercase() {
            if !row.is_uploaded && self.vfs.local_copy_exists(&from) {
                return Err(fsp_error(&VfsError::UploadPending(
                    from.as_str().to_string(),
                )));
            }
            self.vfs
                .db()
                .rename_path(from.as_str(), to.as_str())
                .map_err(|error| fsp_error(&VfsError::Db(error)))?;
            let from_local = self.vfs.local_path(&from);
            let to_local = self.vfs.local_path(to);
            if row.is_dir {
                move_tree_best_effort(&from_local, &to_local);
            } else if from_local.exists() {
                let _ = std::fs::rename(&from_local, &to_local);
            }
            self.grace_invalidate(&from);
            self.grace_invalidate(to);
            return Ok(());
        }
        self.require_dir_parent(to)?;
        if !row.is_uploaded && self.vfs.local_copy_exists(&from) {
            return Err(fsp_error(&VfsError::UploadPending(
                from.as_str().to_string(),
            )));
        }
        if let Some(dest) = self.row(to)? {
            // Review C1 guard: a destination that resolves to the row
            // being renamed is a case-only rename (answered above), never
            // an overwrite target — the one thing this branch must not do
            // is delete the row being renamed.
            if dest.id != row.id {
                if dest.is_dir || row.is_dir {
                    return Err(STATUS_OBJECT_NAME_COLLISION.into());
                }
                if !replace_if_exists || (!dest.is_uploaded && self.vfs.local_copy_exists(to)) {
                    return Err(STATUS_OBJECT_NAME_COLLISION.into());
                }
                self.vfs
                    .db()
                    .delete_file(to.as_str())
                    .map_err(|error| fsp_error(&VfsError::Db(error)))?;
                let dest_local = self.vfs.local_path(to);
                if dest_local.exists() {
                    let _ = std::fs::remove_file(&dest_local);
                }
            }
        }
        self.vfs
            .db()
            .rename_path(from.as_str(), to.as_str())
            .map_err(|error| fsp_error(&VfsError::Db(error)))?;
        let from_local = self.vfs.local_path(&from);
        let to_local = self.vfs.local_path(to);
        if row.is_dir {
            move_tree_best_effort(&from_local, &to_local);
        } else if from_local.exists() {
            if let Some(parent) = to_local.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::rename(&from_local, &to_local);
        }
        // Review H1: the source path now names a different (or no) file,
        // and a replaced destination's parked state describes deleted
        // bytes — neither may be reused inside the grace window.
        self.grace_invalidate(&from);
        self.grace_invalidate(to);
        Ok(())
    }
}

/// Converts an FSD path (`\dir\file`, root `\`, no trailing slash on the
/// FSD's side but trimmed anyway) into the VFS' `/`-separated namespace.
///
/// Rejects what the namespace cannot hold — bad UTF-16, `.` / `..`
/// segments, empty segments — with `STATUS_OBJECT_NAME_INVALID` rather
/// than guessing a neighbor.
pub fn rel_from_winfsp(file_name: &U16CStr) -> std::result::Result<RelPath, FspError> {
    // `to_string` (not the lossy variant): a name with unpaired
    // surrogates cannot exist in the `/`-separated UTF-8 namespace, and
    // mapping it to replacement characters would invent a different name
    // instead of reporting the failure.
    let raw = file_name.to_string().map_err(|_| invalid_name())?;
    let trimmed = raw.trim_matches('\\');
    if trimmed.is_empty() {
        return Ok(RelPath::root());
    }
    RelPath::new(&format!("/{}", trimmed.replace('\\', "/"))).map_err(|_| invalid_name())
}

/// Unix seconds -> FILETIME (100 ns ticks since 1601), clamped at the
/// Unix epoch for the db's occasional `0.0` / negative leftovers.
pub fn unix_to_filetime(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return WINDOWS_EPOCH_OFFSET;
    }
    WINDOWS_EPOCH_OFFSET + (seconds * 10_000_000.0).floor() as u64
}

/// FILETIME -> Unix seconds, the inverse of [`unix_to_filetime`].
///
/// `None` for the FSD's "do not change" encoding (a zero time,
/// winfsp.h's SetBasicInfo contract) and for anything before the Unix
/// epoch — the rows have no room for a 1601 timestamp.
pub fn filetime_to_unix(filetime: u64) -> Option<f64> {
    let ticks = filetime.checked_sub(WINDOWS_EPOCH_OFFSET)?;
    Some(ticks as f64 / 10_000_000.0)
}

/// Current wall-clock time as fractional Unix seconds (the timestamp
/// source for rows this adapter commits outside the db layer).
fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// A local I/O failure rides the K45 table as the EIO fallback class.
fn io_error(error: std::io::Error) -> FspError {
    fsp_error(&VfsError::Io(error))
}

/// Best-effort recursive move of a cached directory subtree (cache misses
/// simply re-hydrate later) — the WebDAV adapter's helper, same name and
/// semantics (`cloudkit-webdav` src/lib.rs:924).
fn move_tree_best_effort(from: &Path, to: &Path) {
    if !from.is_dir() {
        return;
    }
    let _ = std::fs::create_dir_all(to);
    if let Ok(entries) = std::fs::read_dir(from) {
        for entry in entries.flatten() {
            let source = entry.path();
            let dest = to.join(entry.file_name());
            if source.is_dir() {
                move_tree_best_effort(&source, &dest);
            } else {
                let _ = std::fs::rename(&source, &dest);
            }
        }
    }
    let _ = std::fs::remove_dir(from);
}

/// Renders one stat block into the FSD's `FileInfo`.
///
/// One mtime feeds all four timestamps: the rows carry a single mtime
/// (the Python schema's contract), and WinFsp/Explorer are happier with
/// a consistent set than with fabricated differences. `allocation_size`
/// mirrors the logical size: this volume has no cluster geometry, and
/// Explorer's copy progress reads `allocation_size` first.
pub fn fill_file_info(info: &mut FileInfo, meta: &Meta) {
    let now = meta.filetime();
    info.file_attributes = meta.attributes();
    info.reparse_tag = 0;
    info.allocation_size = meta.size;
    info.file_size = meta.size;
    info.creation_time = now;
    info.last_access_time = now;
    info.last_write_time = now;
    info.change_time = now;
    info.index_number = meta.index_number;
    info.hard_links = 0;
    info.ea_size = 0;
}

/// Writes one directory entry into a `DirInfo`.
///
/// The name goes through `set_name_raw(&[u16])` **without** a trailing
/// NUL: WinFsp derives the entry's name length from `DirInfo.Size`
/// (`(Size - sizeof(FSP_FSCTL_DIR_INFO)) / sizeof(WCHAR)` in
/// `dll/dirbuf.c`), so counting a NUL into `Size` makes the FSD's marker
/// search miss, restart enumeration and spin (spike's most expensive
/// lesson: 5.2M `read_directory` calls, Explorer hung). `set_name`
/// (OsStr) appends a NUL and counts it — never use it here.
pub fn fill_dir_info(entry: &mut DirInfo<255>, child: &DirEntry) -> Result<()> {
    {
        let info = entry.file_info_mut();
        *info = FileInfo::default();
        fill_file_info(info, &child.meta);
    }
    let wide: Vec<u16> = child.name.encode_utf16().collect();
    entry.set_name_raw(wide.as_slice())
}

impl FileSystemContext for CloudFs {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> Result<FileSecurity> {
        let rel = rel_from_winfsp(file_name)?;
        let meta = self.meta_for(&rel)?;
        Ok(FileSecurity {
            // No reparse points and no ACL: the descriptor size stays 0
            // (WinFsp treats a zero-length descriptor as "no ACL change",
            // which is the honest answer for a mount that has none).
            reparse: false,
            sz_security_descriptor: 0,
            attributes: meta.attributes(),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext> {
        let rel = rel_from_winfsp(file_name)?;
        let handle = self.open_with_read(&rel)?;
        // The FSD uses this FileInfo for the create/open response — it
        // must be filled here, not left for a later get_file_info.
        fill_file_info(file_info.as_mut(), &handle.meta());
        Ok(handle)
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext> {
        let rel = rel_from_winfsp(file_name)?;
        let handle = self.prepare_create(&rel, create_options, file_attributes)?;
        fill_file_info(file_info.as_mut(), &handle.current_meta());
        Ok(handle)
    }

    fn close(&self, context: Self::FileContext) {
        // K41 grace: a file handle's read state is PARKED, not torn down
        // — a reopen inside `grace_period` reuses the live window buffer
        // (rclone's `--vfs-handle-caching`, which exists for the player /
        // antivirus "closed it, open it again" pattern). Expiry is lazy
        // (checked on every table touch) and the table is capacity
        // bounded, so parking cannot grow without limit. Directory
        // handles have no read state, and their DirBuffer drops with the
        // handle as always (that drop is the DLL's own delete hook).
        //
        // The parked entry records the stat size it was parked under, so
        // a reopen whose fresh row disagrees (delete + recreate behind
        // the same path — review H1) discards it instead of reusing it.
        // Directory handles have no read state; `meta()` on one reports
        // 0 and is never parked.
        //
        // A staged writer that never reached `cleanup` (WinFsp always
        // posts one, so this is the never-reached safety net) drops with
        // the handle and removes its staging sibling.
        let size = context.meta().size;
        let (rel, read) = context.into_grace_parts();
        let Some(read) = read else { return };
        let now = Instant::now();
        self.grace
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .park(
                rel,
                read,
                now + self.grace_period,
                now,
                self.grace_capacity,
                size,
            );
    }

    fn cleanup(&self, context: &Self::FileContext, file_name: Option<&U16CStr>, flags: u32) {
        // The commit point (K41/K43). `take_write` can only succeed once,
        // so a repeated cleanup — and any cleanup after the first — is a
        // no-op, which IS the "commit exactly once" contract.
        let writer = context.take_write();
        let delete = FspCleanupFlags::FspCleanupDelete.is_flagged(flags);
        match writer {
            // Delete-on-close discards the staged bytes: the name is going
            // away, there is nothing to publish.
            Some(staged) if delete => staged.abort(),
            Some(staged) => {
                let mtime = staged.mtime().unwrap_or_else(unix_now);
                match self.bridge.block_on(staged.commit(&self.vfs, mtime)) {
                    Ok(()) => {
                        // Review H1: the commit replaced the file's
                        // content — no parked state for this path may be
                        // reused, and the handle's own open-time read
                        // state goes with it (otherwise the close that
                        // follows would park a pre-commit state under
                        // the refreshed size). The next read lazily
                        // re-acquires against the committed row.
                        self.grace_invalidate(context.rel());
                        context.set_read_state(None);
                        // Refresh the snapshot: the FSD may still query
                        // this handle before Close ("The file system must
                        // be ready to receive additional operations until
                        // close time" — winfsp.h's Cleanup docs).
                        if let Ok(meta) = self.meta_for(context.rel()) {
                            context.set_meta(meta);
                        }
                    }
                    // Cleanup cannot report failure (winfsp.h: "There is
                    // no way to report failure of this operation"), and
                    // the writer already removed the staging sibling on a
                    // failed commit — the log is the only trace the write
                    // happened at all.
                    Err(error) => tracing::error!(
                        rel_path = %context.rel(),
                        %error,
                        "winfsp: staged commit failed at cleanup; the write was discarded"
                    ),
                }
            }
            None => {}
        }
        if delete {
            self.delete_after_cleanup(context, file_name);
        }
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        self.overwrite_staged(context, allocation_size)?;
        fill_file_info(file_info, &context.current_meta());
        Ok(())
    }

    fn rename(
        &self,
        context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> Result<()> {
        let from = rel_from_winfsp(file_name)?;
        let to = rel_from_winfsp(new_file_name)?;
        self.rename_entry(context, &from, &to, replace_if_exists)
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        // Attributes and the other three times have no column in the rows
        // (the Python schema carries a single mtime): accepted and
        // dropped, the same white lie the WebDAV PROPPATCH handler tells
        // — Explorer rolls a whole copy back when a metadata write on it
        // fails. The last-write time is the one that lands: on the staged
        // writer now, in the row at the commit.
        self.stage_mtime(context, last_write_time);
        fill_file_info(file_info, &context.current_meta());
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        file_name: &U16CStr,
        delete_file: bool,
    ) -> Result<()> {
        // Never delete here (winfsp.h's SetDelete contract): mark the
        // handle and let cleanup do it. The guard is the only status
        // channel this path has — see `CloudFs::can_delete`.
        let rel = rel_from_winfsp(file_name)?;
        if !delete_file {
            context.mark_delete(false);
            return Ok(());
        }
        self.can_delete(&rel)?;
        context.mark_delete(true);
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        self.resize_staged(context, new_size, set_allocation_size)?;
        fill_file_info(file_info, &context.current_meta());
        Ok(())
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> Result<u32> {
        let written = self.write_into(context, buffer, offset, write_to_eof, constrained_io)?;
        // The FSD feeds this back into the file node, and Explorer's copy
        // progress reads it: it carries the live staged length.
        fill_file_info(file_info, &context.current_meta());
        Ok(written.min(u32::MAX as usize) as u32)
    }

    fn set_volume_label(&self, volume_label: &U16CStr, volume_info: &mut VolumeInfo) -> Result<()> {
        // K44: the label is process-level — this mount's own name, not a
        // persisted volume property (no other surface of this project has
        // one either). The rename lives in the in-memory snapshot, and
        // winfsp-rs truncates it to 32 wide chars when rendering.
        let label = volume_label.to_string().map_err(|_| invalid_name())?;
        self.volume
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .label = label;
        self.get_volume_info(volume_info)
    }

    fn flush(&self, _context: Option<&Self::FileContext>, _file_info: &mut FileInfo) -> Result<()> {
        // K41, rclone's rule (`read_write.go:189-198`: "Flush can be
        // called multiple times"): Windows, players and scanners flush
        // constantly, and neither the read path nor the staged write path
        // has anything to persist — the commit belongs to cleanup/Release
        // alone. Success with zero side effects: no window dropped, no
        // fetch, no commit, no enqueue, no state touched.
        Ok(())
    }

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> Result<()> {
        // The open-time snapshot: one db read per open, none per stat —
        // Explorer's attribute polling is the hottest metadata path
        // (K44) — with the staged writer's live size/mtime applied while
        // uncommitted bytes exist. The snapshot itself is refreshed once,
        // after a commit.
        fill_file_info(file_info, &context.current_meta());
        Ok(())
    }

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> Result<u32> {
        if context.is_dir() {
            // A directory handle has no read state: `read` on one is a
            // caller error, not a device failure.
            return Err(STATUS_NOT_A_DIRECTORY.into());
        }
        // Read-your-own-writes: while bytes are staged, the staging file
        // IS this handle's view of the file (nothing is published until
        // the commit).
        if let Some(writer) = context.write_state().as_ref() {
            let filled = writer.read_at(offset, buffer).map_err(io_error)?;
            return Ok(filled.min(u32::MAX as usize) as u32);
        }
        let state = match context.read_state() {
            Some(state) => state,
            None => {
                // A create handle starts without a read state (there was
                // no row to dispatch on). Acquiring it lazily is what
                // makes a post-cleanup read on the same handle work —
                // WF0's cache-first probe makes the just-committed copy
                // the local arm. The fresh row's size rides along as the
                // grace-reuse witness (review H1).
                let row_size = self.meta_for(context.rel())?.size;
                let state = self.acquire_read(context.rel(), row_size)?;
                context.set_read_state(Some(Arc::clone(&state)));
                state
            }
        };
        // The dispatcher thread blocks on the read: the window fetch (or
        // the local file read) runs on the injected runtime, exactly like
        // every other VFS call from this adapter (WF1's bridge). The read
        // state's lock is held across that fetch on purpose — a second
        // handle sharing this state (the K41 grace reuse) must serialize
        // behind the window rather than interleave fills into one buffer.
        let filled = self
            .bridge
            .block_on(async {
                let mut read = state.lock().await;
                read.read_at(offset, buffer).await
            })
            .map_err(|error| fsp_error(&error))?;
        // WinFsp's transfer sizes are u32-addressed (the buffer came from
        // the FSD), so the narrowing cannot lose; clamp defensively so a
        // programming error can never report more than was written.
        Ok(filled.min(u32::MAX as usize) as u32)
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker<'_>,
        buffer: &mut [u8],
    ) -> Result<u32> {
        let fresh = self.prepare_enumeration(context, marker.is_none())?;
        let mut slot = context.dir_buffer();
        if let Some(children) = fresh {
            let dir_buffer = slot.get_or_insert_with(DirBuffer::new);
            let lock = dir_buffer.acquire(true, None)?;
            for child in &children {
                let mut entry = DirInfo::<255>::new();
                fill_dir_info(&mut entry, child)?;
                lock.write(&mut entry)?;
            }
            // Drop the lock before reading: the buffer must be published
            // before the FSD walks its own copy (spike pitfall 7 —
            // holding it makes `read` write into a locked buffer).
            drop(lock);
        }
        let dir_buffer = slot.get_or_insert_with(DirBuffer::new);
        Ok(dir_buffer.read(marker, buffer))
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> Result<()> {
        let volume = self.volume();
        out_volume_info.total_size = volume.total_size;
        out_volume_info.free_size = volume.free_size;
        // winfsp-rs truncates to 32 wide chars itself.
        out_volume_info.set_volume_label(volume.label.as_str());
        Ok(())
    }
}
