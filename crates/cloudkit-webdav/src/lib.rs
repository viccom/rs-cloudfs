//! CyDrive WebDAV layer: a [`DavFileSystem`] adapter over the VFS.
//!
//! Behavior baseline: the Python `cydrive/webdav_server.py` provider
//! (WsgiDAV 4.3 dispatch semantics), pinned by the tests under
//! `tests/fs_adapter.rs`:
//!
//! - metadata / listings answer straight off the SQLite rows (PROPFIND
//!   never touches the network);
//! - reads hydrate through [`Vfs::hydrate`] (cache first, remote
//!   download + optional decrypt second) — the remote always sees a
//!   whole-file `open`, so Range requests slice the local cached copy
//!   and never depend on the transport's RANGE_READ bit (R-5);
//! - writes stage into a `.{name}.tmp` sibling of the cache path and
//!   commit on `flush` — fsync, atomic rename, pending row, enqueued
//!   upload — never reading the payload back into memory;
//! - delete removes the row plus any cached copy; the remote side is
//!   gated on the transport's `remote_delete` capability (K4, Phase 2):
//!   bit on (baidu/local) → the remote object dies first through the
//!   shared VFS seam and a refusal keeps the row; bit off
//!   (telegram/mock) → the remote messages are kept (Python
//!   `handle_delete` parity);
//! - [`DavFileSystem::copy`] stays `NotImplemented` (Explorer
//!   drag-copy rides the PUT path; see the design doc);
//! - quota reports the DB total bytes with 10 TB of headroom (compat
//!   contract 6).
//!
//! [`WebDavServer`] assembles the hyper listener (see `server.rs`).

pub mod server;

pub use server::{ServerError, WebDavServer};

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{Buf, Bytes};
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, DavProp, FsError, FsFuture, FsResult,
    FsStream, OpenOptions, ReadDirMeta,
};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::{DbError, FileRecord, FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::vfs::{Vfs, VfsError};

/// Virtual cloud headroom reported by the quota (compat contract 6:
/// Python `get_available_bytes` = 10 TiB).
const TEN_TB: u64 = 10 * 1024 * 1024 * 1024 * 1024;

/// The CyDrive virtual filesystem as a dav-server backend. Cheap to
/// clone (all interior state is shared).
pub struct CyDriveFs {
    vfs: Arc<Vfs>,
    db: Arc<MetaDatabase>,
    /// Cache handle over the same root as the VFS's cache, used for
    /// path math and cache-copy housekeeping.
    cache: Arc<CacheManager>,
}

impl Clone for CyDriveFs {
    fn clone(&self) -> Self {
        Self {
            vfs: Arc::clone(&self.vfs),
            db: Arc::clone(&self.db),
            cache: Arc::clone(&self.cache),
        }
    }
}

impl CyDriveFs {
    /// Assembles the adapter over a running VFS, its metadata DB and a
    /// cache handle rooted at the same cache tree as the VFS's.
    pub fn new(vfs: Arc<Vfs>, db: Arc<MetaDatabase>, cache: CacheManager) -> Self {
        Self {
            vfs,
            db,
            cache: Arc::new(cache),
        }
    }

    /// Row at `rel`, or `None`.
    fn row(&self, rel: &RelPath) -> FsResult<Option<FileRecord>> {
        self.db.get_file(rel.as_str()).map_err(db_err)
    }

    /// Ensures the parent of `rel` is an existing directory (the root
    /// collection always qualifies — it has no row); otherwise
    /// `NotFound` (dav-server maps that to 409 on PUT/MKCOL, matching
    /// wsgidav's "parent must be an existing collection").
    fn require_dir_parent(&self, rel: &RelPath) -> FsResult<()> {
        match rel.parent() {
            None => Ok(()),
            Some(parent) if parent.is_root() => Ok(()),
            Some(parent) => match self.row(&parent)? {
                Some(row) if row.is_dir => Ok(()),
                _ => Err(FsError::NotFound),
            },
        }
    }
}

impl DavFileSystem for CyDriveFs {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            let rel = dav_to_rel(path)?;
            if options.read && !options.write {
                // Read state: the row must be a file; hydrate then wrap.
                //
                // R-5 capability note: Range requests never reach the
                // transport — hydration always streams the WHOLE file
                // through `Vfs::hydrate` (`CloudTransport::open`, never
                // `open_range`) and dav-server then slices the local
                // cached copy via `HydratedFile` seeks. A transport that
                // declares no RANGE_READ therefore serves byte-identical
                // Range behavior (interfaces §1: degrade, never a panic);
                // pinned by the smoke test
                // `get_range_without_range_read_capability_still_slices`.
                // Any future remote-Range forwarding MUST first check
                // `capabilities().range_read` and fall back to this
                // whole-file path when the bit is off.
                let row = self.row(&rel)?.ok_or(FsError::NotFound)?;
                if row.is_dir {
                    return Err(FsError::Forbidden);
                }
                let local = self.vfs.hydrate(&rel).await.map_err(vfs_err)?;
                let file = tokio::fs::File::open(&local).await.map_err(io_err)?;
                return Ok(Box::new(HydratedFile {
                    file,
                    meta: RowMetaData::from_row(&row),
                }) as Box<dyn DavFile>);
            }
            if options.write {
                // Write state (PUT shape): create/truncate into a staged
                // sibling; flush commits. A collection target is
                // Forbidden (wsgidav: "Cannot PUT to a collection").
                if let Some(row) = self.row(&rel)? {
                    if row.is_dir {
                        return Err(FsError::Forbidden);
                    }
                    if options.create_new {
                        return Err(FsError::Exists);
                    }
                }
                self.require_dir_parent(&rel)?;
                let final_local = self.cache.local_path(&rel);
                let staged = staged_sibling(&final_local);
                if let Some(parent) = final_local.parent() {
                    std::fs::create_dir_all(parent).map_err(io_err)?;
                }
                let mut open = std::fs::OpenOptions::new();
                open.write(true).create(true);
                if options.append {
                    open.append(true);
                } else {
                    open.truncate(true);
                }
                let file = open.open(&staged).map_err(io_err)?;
                return Ok(Box::new(StagedFile {
                    file: Some(file),
                    rel,
                    staged,
                    vfs: Arc::clone(&self.vfs),
                    db: Arc::clone(&self.db),
                    flushed: false,
                }) as Box<dyn DavFile>);
            }
            Err(FsError::GeneralFailure)
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        Box::pin(async move {
            let rel = dav_to_rel(path)?;
            if !rel.is_root() {
                let row = self.row(&rel)?.ok_or(FsError::NotFound)?;
                if !row.is_dir {
                    return Err(FsError::Forbidden);
                }
            }
            // ReadDirMeta is an optimization hint only; entries always
            // carry full metadata (the DB row is already in hand).
            let items = self.db.list_dir(rel.as_str()).map_err(db_err)?;
            let entries: Vec<Box<dyn DavDirEntry>> = items
                .iter()
                .map(|row| {
                    Box::new(RowDirEntry {
                        name: row.name.clone(),
                        meta: RowMetaData::from_row(row),
                    }) as Box<dyn DavDirEntry>
                })
                .collect();
            Ok(Box::pin(futures_util::stream::iter(entries).map(Ok))
                as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let rel = dav_to_rel(path)?;
            if rel.is_root() {
                // The root collection always exists (Python provider
                // synthesizes VirtualTelegramFolder("/")).
                return Ok(Box::new(RowMetaData::root()) as Box<dyn DavMetaData>);
            }
            let row = self.row(&rel)?.ok_or(FsError::NotFound)?;
            Ok(Box::new(RowMetaData::from_row(&row)) as Box<dyn DavMetaData>)
        })
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let rel = dav_to_rel(path)?;
            if rel.is_root() || self.row(&rel)?.is_some() {
                return Err(FsError::Exists);
            }
            self.require_dir_parent(&rel)?;
            let parent_dir = match rel.parent() {
                Some(parent) => parent.as_str().to_string(),
                None => "/".to_string(),
            };
            // Python `create_collection` parity: rows are size-0,
            // is_uploaded and is_cached.
            self.db
                .upsert_file(&FileUpsert {
                    rel_path: rel.as_str().to_string(),
                    name: rel.name().to_string(),
                    parent_dir,
                    size: 0,
                    mtime: unix_now(),
                    sha256: None,
                    is_dir: true,
                    telegram_msg_id: None,
                    is_uploaded: true,
                    is_cached: true,
                    is_encrypted: false,
                    chunk_count: 0,
                    mime_type: None,
                })
                .map_err(db_err)?;
            Ok(())
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let rel = dav_to_rel(path)?;
            if rel.is_root() {
                return Err(FsError::Forbidden);
            }
            let row = self.row(&rel)?.ok_or(FsError::NotFound)?;
            if !row.is_dir {
                return Err(FsError::Forbidden);
            }
            // Python `handle_delete` deletes the row unconditionally,
            // silently orphaning children; dav-server's handler empties
            // children first for the default Depth-infinity DELETE, so
            // rejecting a non-empty dir here only guards the Depth-0
            // corner (which wsgidav 400s before the provider) against
            // orphaned rows.
            if !self.db.list_dir(rel.as_str()).map_err(db_err)?.is_empty() {
                return Err(FsError::Exists);
            }
            // K4 remote-delete gate (Phase 2): a collection row on a
            // remote_delete backend (baidu/local) has its remote object
            // deleted FIRST — the shared VFS seam owns the ordering
            // (idempotent retry; refusal keeps the row). Bit off
            // (telegram/mock) is a no-op and the legacy row delete
            // proceeds unchanged.
            self.vfs
                .delete_remote_for_row(&rel)
                .await
                .map_err(vfs_err)?;
            self.db.delete_file(rel.as_str()).map_err(db_err)?;
            // No manual doorbell: the row delete above rang the db-layer
            // files hook (the chokepoint) — deletion is the tombstone's
            // origin either way.
            Ok(())
        })
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let rel = dav_to_rel(path)?;
            if rel.is_root() {
                return Err(FsError::Forbidden);
            }
            let row = self.row(&rel)?.ok_or(FsError::NotFound)?;
            if row.is_dir {
                return Err(FsError::Forbidden);
            }
            // Pending-upload guard (review H2 / plan F2 — the same
            // adjudication as core `Vfs::remove_file`): a pending row
            // whose local cache copy still exists is refused — that copy
            // is the only copy of the bytes (nothing is on the remote
            // yet). A ghost pending row (copy already vanished) falls
            // through and stays deletable, otherwise it could never be
            // cleaned up.
            if !row.is_uploaded && self.cache.local_path(&rel).exists() {
                return Err(FsError::Forbidden);
            }
            // K4 remote-delete gate (Phase 2): the shared VFS seam —
            // remote object first, row + cache only after it is gone;
            // refusal aborts with the row kept. Bit off
            // (telegram/mock) is a no-op (Python parity: the remote
            // messages stay).
            self.vfs
                .delete_remote_for_row(&rel)
                .await
                .map_err(vfs_err)?;
            self.db.delete_file(rel.as_str()).map_err(db_err)?;
            // No manual doorbell: the row delete above rang the db-layer
            // files hook (the chokepoint) — deletion is the tombstone's
            // origin either way.
            // Cached copy goes too; removal errors are ignored (Python
            // `handle_delete` swallows OSError). With the gate on, the
            // remote message WAS deleted above; with it off the legacy
            // keep-the-remote behavior stands.
            let local = self.cache.local_path(&rel);
            if local.exists() {
                let _ = std::fs::remove_file(&local);
            }
            Ok(())
        })
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let from = dav_to_rel(from)?;
            let to = dav_to_rel(to)?;
            if from == to {
                return Err(FsError::Forbidden);
            }
            let row = self.row(&from)?.ok_or(FsError::NotFound)?;
            self.require_dir_parent(&to)?;
            // Trait contract: an existing file destination is replaced,
            // a directory destination errors (dav-server's handler
            // pre-deletes the destination when Overwrite: T, so this is
            // mostly a direct-call guard).
            if let Some(dest) = self.row(&to)? {
                if dest.is_dir || row.is_dir {
                    return Err(FsError::Exists);
                }
                self.db.delete_file(to.as_str()).map_err(db_err)?;
                let dest_local = self.cache.local_path(&to);
                if dest_local.exists() {
                    let _ = std::fs::remove_file(&dest_local);
                }
            }
            // Rows move in place (ids and chunk linkage preserved); the
            // remote keeps its messages — no re-upload, no delete.
            self.db
                .rename_path(from.as_str(), to.as_str())
                .map_err(db_err)?;
            // No manual doorbell: the overwrite deletion above (when it
            // happened) and the rename's row updates all rang the
            // db-layer files hook (the chokepoint).
            let from_local = self.cache.local_path(&from);
            let to_local = self.cache.local_path(&to);
            if row.is_dir {
                move_tree_best_effort(&from_local, &to_local);
            } else if from_local.exists() {
                if let Some(parent) = to_local.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::rename(&from_local, &to_local);
            }
            Ok(())
        })
    }

    fn get_quota(&'_ self) -> FsFuture<'_, (u64, Option<u64>)> {
        Box::pin(async move {
            let used = self.db.get_stats().map_err(db_err)?.total_bytes.max(0) as u64;
            Ok((used, Some(used + TEN_TB)))
        })
    }

    fn patch_props<'a>(
        &'a self,
        _path: &'a DavPath,
        patch: Vec<(bool, DavProp)>,
    ) -> FsFuture<'a, Vec<(http::StatusCode, DavProp)>> {
        Box::pin(async move {
            // Windows MiniRedir ends every Explorer copy with a PROPPATCH
            // meant to preserve the source file's mtime; answering 405 made
            // it roll the whole copy back with DELETE. dav-server's liveprop
            // policy intercepts DAV:getlastmodified (hardcoded 403 inside
            // the 207, read-only live property — same as Apache mod_dav)
            // and the MS `urn:schemas-microsoft-com:` Win32* props (fake
            // OK), so the dead props reaching here come from other
            // namespaces. Report success for every one of them: Explorer
            // only needs the 207 to keep the copy; the mtime is
            // deliberately not applied (known limitation — there is no
            // mtime-update method on MetaDatabase and the rows' mtime is
            // upload-owned).
            Ok(patch
                .into_iter()
                .map(|(_, prop)| (http::StatusCode::OK, prop))
                .collect())
        })
    }
}

/// Read-state [`DavFile`]: a hydrated cache copy wrapped in a tokio
/// file. Writes are rejected.
struct HydratedFile {
    file: tokio::fs::File,
    meta: RowMetaData,
}

impl std::fmt::Debug for HydratedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HydratedFile")
            .field("meta", &self.meta)
            .finish_non_exhaustive()
    }
}

impl DavFile for HydratedFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(std::future::ready(Ok(
            Box::new(self.meta.clone()) as Box<dyn DavMetaData>
        )))
    }

    fn write_buf(&'_ mut self, _buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Err(FsError::Forbidden)))
    }

    fn write_bytes(&'_ mut self, _buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Err(FsError::Forbidden)))
    }

    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            // Short reads are fine: dav-server's GET keeps reading until
            // an empty payload signals EOF.
            let mut buf = vec![0u8; count];
            let n = self.file.read(&mut buf).await.map_err(io_err)?;
            buf.truncate(n);
            Ok(Bytes::from(buf))
        })
    }

    fn seek(&'_ mut self, pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move { self.file.seek(pos).await.map_err(io_err) })
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// Write-state [`DavFile`] (PUT): a `.{name}.tmp` sibling of the final
/// cache path. `flush` is the PUT completion point — fsync, hand the
/// staged file to [`Vfs::put_staged`] (atomic rename + pending row +
/// enqueued upload) — after which further writes are rejected.
struct StagedFile {
    file: Option<std::fs::File>,
    rel: RelPath,
    staged: PathBuf,
    vfs: Arc<Vfs>,
    db: Arc<MetaDatabase>,
    flushed: bool,
}

impl std::fmt::Debug for StagedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedFile")
            .field("rel", &self.rel)
            .field("staged", &self.staged)
            .field("flushed", &self.flushed)
            .finish_non_exhaustive()
    }
}

impl StagedFile {
    /// The open staging handle, or Forbidden once flushed.
    fn handle(&mut self) -> FsResult<&mut std::fs::File> {
        self.file.as_mut().ok_or(FsError::Forbidden)
    }
}

impl DavFile for StagedFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(async move {
            // After flush the committed row is the truth (PUT reads it
            // for the response ETag/Last-Modified headers).
            if let Ok(Some(row)) = self.db.get_file(self.rel.as_str()) {
                return Ok(Box::new(RowMetaData::from_row(&row)) as Box<dyn DavMetaData>);
            }
            // Pre-flush: report the staged bytes as they stand.
            let (len, mtime) = std::fs::metadata(&self.staged)
                .map(|meta| {
                    (
                        meta.len(),
                        meta.modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map_or(0.0, |d| d.as_secs_f64()),
                    )
                })
                .unwrap_or((0, unix_now()));
            Ok(Box::new(RowMetaData {
                len,
                mtime,
                created: None,
                is_dir: false,
                sha256: None,
            }) as Box<dyn DavMetaData>)
        })
    }

    fn write_buf(&'_ mut self, buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async move {
            let file = self.handle()?;
            let mut buf = buf;
            while buf.has_remaining() {
                let chunk = buf.chunk();
                file.write_all(chunk).map_err(io_err)?;
                buf.advance(chunk.len());
            }
            Ok(())
        })
    }

    fn write_bytes(&'_ mut self, buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(async move {
            self.handle()?.write_all(&buf).map_err(io_err)?;
            Ok(())
        })
    }

    fn read_bytes(&'_ mut self, _count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(std::future::ready(Err(FsError::Forbidden)))
    }

    fn seek(&'_ mut self, pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            let file = self.handle()?;
            std::io::Seek::seek(file, pos).map_err(io_err)
        })
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        Box::pin(async move {
            if self.flushed {
                return Ok(());
            }
            self.flushed = true;
            // Sync and close before the rename (Windows refuses some
            // renames of open handles); the std handle closes
            // synchronously on drop.
            if let Some(file) = self.file.take() {
                file.sync_all().map_err(io_err)?;
            }
            let mtime = unix_now();
            self.vfs
                .put_staged(&self.rel, &self.staged, mtime)
                .await
                .map_err(vfs_err)
        })
    }
}

/// One directory listing entry backed by a `files` row.
#[derive(Debug, Clone)]
struct RowDirEntry {
    name: String,
    meta: RowMetaData,
}

impl DavDirEntry for RowDirEntry {
    fn name(&self) -> Vec<u8> {
        self.name.clone().into_bytes()
    }

    fn metadata(&'_ self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(std::future::ready(Ok(
            Box::new(self.meta.clone()) as Box<dyn DavMetaData>
        )))
    }
}

/// [`DavMetaData`] backed by a `files` row (or synthesized for the
/// root).
#[derive(Debug, Clone)]
struct RowMetaData {
    len: u64,
    mtime: f64,
    created: Option<f64>,
    is_dir: bool,
    sha256: Option<String>,
}

impl RowMetaData {
    fn from_row(row: &FileRecord) -> Self {
        Self {
            len: row.size.max(0) as u64,
            mtime: row.mtime,
            created: row.created_at,
            is_dir: row.is_dir,
            sha256: row.sha256.clone(),
        }
    }

    /// The always-existing root collection.
    fn root() -> Self {
        Self {
            len: 0,
            mtime: 0.0,
            created: None,
            is_dir: true,
            sha256: None,
        }
    }
}

impl DavMetaData for RowMetaData {
    fn len(&self) -> u64 {
        self.len
    }

    fn is_dir(&self) -> bool {
        self.is_dir
    }

    fn modified(&self) -> FsResult<SystemTime> {
        Ok(unix_to_systime(self.mtime))
    }

    fn created(&self) -> FsResult<SystemTime> {
        // Python `get_creation_date` falls back to the current time when
        // the row carries no creation date; rows here always do, with
        // mtime as the last-resort fallback.
        Ok(unix_to_systime(self.created.unwrap_or(self.mtime)))
    }

    fn etag(&self) -> Option<String> {
        // Compat contract 6: the sha256 hex digest unquoted when known,
        // else "{int(mtime)}-{size}"; directories carry no ETag
        // (Python folders report support_etag() == False).
        if self.is_dir {
            return None;
        }
        if let Some(sha) = &self.sha256 {
            return Some(sha.replace('"', ""));
        }
        Some(format!("{}-{}", self.mtime as i64, self.len))
    }
}

/// Converts a `DavPath` (prefix-stripped, percent-decoded bytes, with or
/// without a collection trailing slash) into a validated [`RelPath`].
fn dav_to_rel(path: &DavPath) -> FsResult<RelPath> {
    let raw = std::str::from_utf8(path.as_bytes()).map_err(|_| FsError::GeneralFailure)?;
    let trimmed = raw.trim_end_matches('/');
    let normalized = if trimmed.is_empty() { "/" } else { trimmed };
    RelPath::new(normalized).map_err(|_| FsError::GeneralFailure)
}

/// Staging sibling of the final cache path: `.{name}.tmp` in the same
/// directory (same-dir renames are atomic; the leading dot keeps the
/// half-written file out of the visible namespace).
fn staged_sibling(final_local: &Path) -> PathBuf {
    let name = final_local.file_name().map_or_else(
        || "cydrive".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    final_local.with_file_name(format!(".{name}.tmp"))
}

/// Best-effort recursive move of a cached directory subtree (cache
/// misses simply re-hydrate later).
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

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn unix_to_systime(t: f64) -> SystemTime {
    if t >= 0.0 {
        UNIX_EPOCH + Duration::from_secs_f64(t)
    } else {
        UNIX_EPOCH - Duration::from_secs_f64(-t)
    }
}

fn vfs_err(error: VfsError) -> FsError {
    match error {
        VfsError::NotFound(_) => FsError::NotFound,
        // No dedicated is-a-directory code in FsError; dav-server maps
        // EISDIR to Forbidden, so mirror that. MissingPassword is a
        // policy refusal -> Forbidden as well.
        VfsError::IsDirectory(_) | VfsError::MissingPassword => FsError::Forbidden,
        // Same conventions this adapter already uses: duplicate target is
        // Exists (405); missing parent maps to NotFound, which dav-server
        // turns into 409 on PUT/MKCOL (see `require_dir_parent`); a
        // refused pending-upload delete is a policy refusal -> Forbidden,
        // mirroring the adapter's own `remove_file` guard.
        VfsError::Exists(_) => FsError::Exists,
        VfsError::ParentMissing(_) => FsError::NotFound,
        VfsError::UploadPending(_) => FsError::Forbidden,
        VfsError::QueueClosed
        | VfsError::Db(_)
        | VfsError::Transport(_)
        | VfsError::Crypto(_)
        | VfsError::Timeout(_)
        | VfsError::UnsupportedEncryptionScheme { .. } => FsError::GeneralFailure,
        VfsError::Io(error) => io_err(error),
    }
}

fn io_err(error: std::io::Error) -> FsError {
    match error.kind() {
        std::io::ErrorKind::NotFound => FsError::NotFound,
        std::io::ErrorKind::PermissionDenied => FsError::Forbidden,
        _ => FsError::GeneralFailure,
    }
}

fn db_err(_error: DbError) -> FsError {
    FsError::GeneralFailure
}
