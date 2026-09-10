//! `CloudFs` — the WinFsp `FileSystemContext` adapter (WF1: the readonly
//! metadata face).
//!
//! Scope of this batch (plan §3-WF1): `get_security_by_name`, `open`,
//! `close`, `get_file_info`, `read_directory`, `get_volume_info`. All of
//! them answer from the local SQLite rows plus the assembly-time volume
//! snapshot (K44) — a metadata walk must never open a socket, which
//! `tests/metadata.rs` pins against the mock transport's call log.
//!
//! Deliberately NOT implemented here (the trait defaults answer
//! `STATUS_INVALID_DEVICE_REQUEST`):
//! - **WF2** fills `read` (the K33 triple gate -> windowed `RangeFile`
//!   reads) and the handle grace period;
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

use std::ffi::c_void;
use std::sync::{Arc, Mutex, MutexGuard};

use cloudkit_core::database::FileRecord;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::vfs::{Vfs, VfsError};
use windows::Win32::Foundation::{STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_NOT_FOUND};
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::{FspError, Result, U16CStr};

use crate::bridge::AsyncBridge;
use crate::error::{fsp_error, invalid_name};

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
/// WF1 keeps the metadata snapshot taken at open time plus the directory
/// enumeration buffer; the read/write payload state (window position,
/// staged file) arrives with WF2/WF3. `DirBuffer` is interior-mutable by
/// design (winfsp-rs hands out `&FileContext` from several threads), so
/// the framework's shared reference is all this needs.
pub struct Handle {
    rel: RelPath,
    meta: Meta,
    /// Directory enumeration buffer, created on the first fresh
    /// `read_directory` — the only WinFsp DLL call this batch makes
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

    /// The enumeration buffer slot (poison recovery per code-style §2).
    fn dir_buffer(&self) -> MutexGuard<'_, Option<DirBuffer>> {
        self.dir_buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn new(rel: RelPath, meta: Meta) -> Self {
        Self {
            rel,
            meta,
            dir_buffer: Mutex::new(None),
        }
    }
}

/// The adapter: one instance per mounted volume.
pub struct CloudFs {
    vfs: Arc<Vfs>,
    bridge: AsyncBridge,
    volume: VolumeSnapshot,
}

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
        }
    }

    /// The injected async bridge (WF2's read path awaits on it).
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

    /// Opens one path: `get_file_info` and `read_directory` then work off
    /// the returned handle alone.
    pub fn open_handle(&self, rel: &RelPath) -> std::result::Result<Handle, FspError> {
        let meta = self.meta_for(rel)?;
        Ok(Handle::new(rel.clone(), meta))
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
        let handle = self.open_handle(&rel)?;
        // The FSD uses this FileInfo for the create/open response — it
        // must be filled here, not left for a later get_file_info.
        fill_file_info(file_info.as_mut(), handle.meta());
        Ok(handle)
    }

    fn close(&self, _context: Self::FileContext) {
        // Dropping the handle drops its DirBuffer (the DLL's own delete
        // hook) — the whole teardown this batch needs. WF2 adds the grace
        // period bookkeeping here (K41).
    }

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> Result<()> {
        // The open-time snapshot: one db read per open, none per stat —
        // Explorer's attribute polling is the hottest metadata path
        // (K44). WF3 refreshes the snapshot after a commit.
        fill_file_info(file_info, context.meta());
        Ok(())
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
