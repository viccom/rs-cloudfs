//! `CloudFs` — the WinFsp `FileSystemContext` adapter.
//!
//! Scope of this batch (plan §3-WF1 + WF2): `get_security_by_name`,
//! `open`, `close`, `get_file_info`, `read_directory`, `get_volume_info`
//! (WF1: all answered from the local SQLite rows plus the assembly-time
//! volume snapshot — K44, zero network) and the read path (WF2: K33's
//! triple-gate dispatch into the bounded `open_range` window model, the
//! zero-side-effect `flush` and the K41 handle grace period).
//!
//! Deliberately NOT implemented here (the trait defaults answer
//! `STATUS_INVALID_DEVICE_REQUEST`):
//! - **WF3** fills `write` / `create` / `cleanup` / `rename` / `set_*`
//!   (staged commits) and `set_volume_label`;
//! - **WF4** owns `winfsp_init`, the host mount/unmount and the K40
//!   WebDAV fallback; the DLL preload and mount-point rules live there,
//!   not here.
//!
//! Case sensitivity (known, deliberate): lookups are byte-exact against
//! the `/`-separated rows (`RelPath`'s contract). Windows sends the names
//! it saw in `read_directory`, so Explorer's own navigation matches, but
//! a program typed-`\DOCS\README.TXT` miss is a `STATUS_OBJECT_NAME_NOT_FOUND`
//! until a case-folding layer is decided (WF2/WF4 open question, not
//! invented here).

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
// The read state is locked ACROSS an await (a window fetch runs under it,
// so two reads of one reused handle serialize instead of interleaving
// fills) — hence the await-aware lock, the same choice ck-baidu's token
// refresh lock makes. Everything else here is a plain `std` mutex: the
// enumeration buffer and the grace table are only ever held
// synchronously.
use tokio::sync::Mutex as AsyncMutex;

use cloudkit_core::database::FileRecord;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::vfs::{StreamSource, Vfs, VfsError};
use windows::Win32::Foundation::{STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_NOT_FOUND};
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::{FspError, Result, U16CStr};

use crate::bridge::AsyncBridge;
use crate::error::{fsp_error, invalid_name};
use crate::reader::{LocalReader, ReadHandle, WindowReader};

/// Compat contract 6 (Python `get_available_bytes`): the virtual cloud
/// headroom reported on top of what the rows already hold. Shared with
/// the WebDAV adapter so both mounts show the same numbers.
pub const VOLUME_HEADROOM: u64 = 10 * 1024 * 1024 * 1024 * 1024;

/// Win32 file attribute bits (winfsp-rs takes them as raw `u32`; pulling
/// `Win32_Storage_FileSystem` in for two constants would grow the
/// feature surface the FSD needs).
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0010;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0020;

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
/// `open` and `close`.
///
/// The metadata snapshot is taken at open time (K44 — `get_file_info`
/// never re-reads the db), the read state (WF2) is the K33 dispatch
/// result for files, and the directory enumeration buffer arrives on the
/// first fresh `read_directory`. `DirBuffer` is interior-mutable by
/// design (winfsp-rs hands out `&FileContext` from several threads), so
/// the framework's shared reference is all this needs.
pub struct Handle {
    rel: RelPath,
    meta: Meta,
    /// The read state of a file handle (K41: shared with the grace table,
    /// so a reopen inside the grace window reuses the live window buffer
    /// instead of rebuilding it). `None` for directories.
    read: Option<Arc<AsyncMutex<ReadHandle>>>,
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

    /// The stat snapshot taken at open time.
    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// Directory handle? (`read_directory` refuses files.)
    pub fn is_dir(&self) -> bool {
        self.meta.is_dir
    }

    /// The read state slot (files only; `read` serves through it).
    fn read_state(&self) -> Option<&Arc<AsyncMutex<ReadHandle>>> {
        self.read.as_ref()
    }

    /// Destructures a closing handle: its path plus the read state the
    /// grace table takes back (WF3's cleanup reads the same two).
    fn into_grace_parts(self) -> (RelPath, Option<Arc<AsyncMutex<ReadHandle>>>) {
        let Handle { rel, read, .. } = self;
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
            meta,
            read,
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
    /// oldest close — goes first).
    fn park(
        &mut self,
        rel: RelPath,
        read: Arc<AsyncMutex<ReadHandle>>,
        expires_at: Instant,
        now: Instant,
        capacity: usize,
    ) {
        self.prune_expired(now);
        self.entries.insert(rel, GraceEntry { read, expires_at });
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

    /// Takes the live entry for `rel`, if any (expired entries are
    /// dropped, never handed out).
    fn take_live(&mut self, rel: &RelPath, now: Instant) -> Option<Arc<AsyncMutex<ReadHandle>>> {
        self.prune_expired(now);
        self.entries.remove(rel).map(|entry| entry.read)
    }
}

/// The adapter: one instance per mounted volume.
pub struct CloudFs {
    vfs: Arc<Vfs>,
    bridge: AsyncBridge,
    volume: VolumeSnapshot,
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
            volume: VolumeSnapshot {
                total_size: used + VOLUME_HEADROOM,
                free_size: VOLUME_HEADROOM,
                label: label.into(),
            },
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

    /// The assembly-time volume snapshot behind `get_volume_info`.
    pub fn volume(&self) -> &VolumeSnapshot {
        &self.volume
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
        let row = self
            .vfs
            .db()
            .get_file(rel.as_str())
            .map_err(|error| fsp_error(&VfsError::Db(error)))?;
        row.as_ref()
            .map(Meta::from_record)
            .ok_or_else(|| STATUS_OBJECT_NAME_NOT_FOUND.into())
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
        let meta = self.meta_for(rel)?;
        let read = if meta.is_dir {
            // Directories have no read state; their handle only ever
            // serves metadata and enumeration.
            None
        } else {
            Some(self.acquire_read(rel)?)
        };
        Ok(Handle::new(rel.clone(), meta, read))
    }

    /// The read state for one file open: the parked one when the K41
    /// grace table still holds it, otherwise a fresh K33 dispatch.
    fn acquire_read(
        &self,
        rel: &RelPath,
    ) -> std::result::Result<Arc<AsyncMutex<ReadHandle>>, FspError> {
        let now = Instant::now();
        if let Some(parked) = self
            .grace
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take_live(rel, now)
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
        fill_file_info(file_info.as_mut(), handle.meta());
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
        let (rel, read) = context.into_grace_parts();
        let Some(read) = read else { return };
        let now = Instant::now();
        self.grace
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .park(rel, read, now + self.grace_period, now, self.grace_capacity);
    }

    fn flush(&self, _context: Option<&Self::FileContext>, _file_info: &mut FileInfo) -> Result<()> {
        // K41, rclone's rule (`read_write.go:189-198`: "Flush can be
        // called multiple times"): Windows, players and scanners flush
        // constantly, and the read path has nothing to persist. Success
        // with zero side effects — no window dropped, no fetch, no state
        // touched. WF3 hangs the write commit off cleanup/Release, never
        // off flush.
        Ok(())
    }

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> Result<()> {
        // The open-time snapshot: one db read per open, none per stat —
        // Explorer's attribute polling is the hottest metadata path
        // (K44). WF3 refreshes the snapshot after a commit.
        fill_file_info(file_info, context.meta());
        Ok(())
    }

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> Result<u32> {
        let Some(state) = context.read_state() else {
            // Only directory handles have no read state: `read` on a
            // directory is a caller error, not a device failure.
            return Err(STATUS_NOT_A_DIRECTORY.into());
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
        out_volume_info.total_size = self.volume.total_size;
        out_volume_info.free_size = self.volume.free_size;
        // winfsp-rs truncates to 32 wide chars itself.
        out_volume_info.set_volume_label(self.volume.label.as_str());
        Ok(())
    }
}
