//! SQLite metadata store for the CyDrive virtual file system.
//!
//! Contract source: Python `cydrive/database.py`. The DDL and SQL semantics
//! are frozen (see `docs/rust-rewrite-design.md`, «兼容契约»): a database
//! produced by the Python implementation must be adopted unchanged, and the
//! schema created here must stay byte-identical to the Python
//! `CREATE TABLE` / `CREATE INDEX` statements.
//!
//! Deviations from the Python version (both mandated by the design doc):
//!
//! - `open()` enables `PRAGMA foreign_keys = ON` (Python never enabled the
//!   `ON DELETE CASCADE` it declared) and `busy_timeout = 5000`;
//! - `upsert_file` returns the row id via `INSERT ... RETURNING id`, fixing
//!   the Python `lastrowid` bug where a conflict-update returned the id of
//!   the *previous* insert instead of the surviving row.
//!
//! Outside the frozen contract: the sync-lite mirror tables `sync_mirror`
//! and `sync_state` (2026-09-04 plan, «客户端») are Rust-added, purely
//! additive `IF NOT EXISTS` tables created in a separate batch — the
//! contract DDL above stays byte-identical and untouched — and the
//! `files.encryption_scheme` column (Batch E / E-4) is added by a
//! pragma-guarded `ALTER TABLE` in the same separate-batch spirit,
//! `NOT NULL DEFAULT 'gcm'` so pre-existing rows and Python-shaped
//! INSERTs keep their exact pre-E-4 behavior. The `rebuild_state` KV
//! table (Phase 8 / D8①, the resumable-rebuild checkpoint) follows the
//! same additive precedent.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::hooks::Action;
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::Notify;

/// Error wrapper around [`rusqlite::Error`].
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct DbError(#[from] rusqlite::Error);

/// Seconds since the Unix epoch — the timestamp source for
/// `created_at` / `updated_at` (Python used `time.time()`).
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// A row of the `files` table (file or directory metadata).
#[derive(Debug, Clone)]
pub struct FileRecord {
    /// SQLite rowid (`INTEGER PRIMARY KEY AUTOINCREMENT`).
    pub id: i64,
    /// Canonical virtual path, unique — the conflict key of `upsert_file`.
    pub rel_path: String,
    /// Final path segment.
    pub name: String,
    /// Virtual path of the parent directory (`/` for top level).
    pub parent_dir: String,
    /// Size in bytes; `0` for directories.
    pub size: i64,
    /// Modification time as fractional seconds since the Unix epoch.
    pub mtime: f64,
    /// SHA-256 hex digest when computed (Python only hashes files ≤ 100 MB).
    pub sha256: Option<String>,
    /// Directories are plain records: `true` for dirs, `false` for files.
    pub is_dir: bool,
    /// Telegram message id of chunk 0 once uploaded.
    pub telegram_msg_id: Option<i64>,
    /// Whether all chunks have been pushed to Telegram.
    pub is_uploaded: bool,
    /// Whether the file is currently hydrated in the local cache.
    pub is_cached: bool,
    /// Whether the payload is client-side encrypted.
    pub is_encrypted: bool,
    /// Number of `.partNNN` chunks (1 for small files).
    pub chunk_count: i64,
    /// MIME type, when known.
    pub mime_type: Option<String>,
    /// Container scheme of the encrypted payload (`files.encryption_scheme`
    /// column, Batch E / E-4): `"gcm"` (frozen v1, the column default for
    /// pre-existing rows) or `"aead_v2"`. Meaningful only while
    /// `is_encrypted` is set. Unknown values from a newer build must not
    /// break row reads — consumers dispatch with an actionable error.
    pub encryption_scheme: String,
    /// Row creation time (set by the DB on first insert).
    pub created_at: Option<f64>,
    /// Last update time (set by the DB on every upsert).
    pub updated_at: Option<f64>,
}

/// A row of the `chunks` table (one `.partNNN` of a large file).
#[derive(Debug, Clone)]
pub struct ChunkRecord {
    /// Zero-based chunk ordinal within the file.
    pub chunk_index: i64,
    /// Telegram message id holding this chunk.
    pub telegram_msg_id: Option<i64>,
    /// Chunk size in bytes.
    pub size: i64,
    /// SHA-256 hex digest when computed.
    pub sha256: Option<String>,
}

/// Aggregated drive statistics (mirrors Python `get_stats`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stats {
    /// Count of non-directory rows.
    pub total_files: i64,
    /// `SUM(size)` over non-directory rows (`0` when there are none).
    pub total_bytes: i64,
    /// Count of directory rows.
    pub total_dirs: i64,
    /// Non-directory rows with `is_uploaded = 1`.
    pub uploaded_files: i64,
    /// `total_files - uploaded_files`.
    pub pending_uploads: i64,
}

/// Flat input of [`MetaDatabase::upsert_file`]; every column the Python
/// version writes. `created_at` / `updated_at` are maintained by the DB.
#[derive(Debug, Clone)]
pub struct FileUpsert {
    pub rel_path: String,
    pub name: String,
    pub parent_dir: String,
    pub size: i64,
    pub mtime: f64,
    pub sha256: Option<String>,
    pub is_dir: bool,
    pub telegram_msg_id: Option<i64>,
    pub is_uploaded: bool,
    pub is_cached: bool,
    pub is_encrypted: bool,
    pub chunk_count: i64,
    pub mime_type: Option<String>,
}

/// SQLite metadata manager for the CyDrive virtual file system.
///
/// The connection sits behind a [`Mutex`] so that `&self` methods are
/// safe from multiple threads (a bare `rusqlite::Connection` is Send but
/// not Sync, which blocked sharing `Arc<MetaDatabase>` across tokio
/// tasks). Every public method takes the lock for its full body; no
/// public method calls another, so the lock is never re-entered.
///
/// The connection carries a rusqlite `update_hook` (wake chokepoint
/// batch, 2026-09-06): every `files` table row change rings the
/// doorbell this type owns and exposes via
/// [`MetaDatabase::sync_notifier`]. This is the single chokepoint that
/// replaced the hand-placed `Vfs::wake_sync()` call sites — a write
/// path can no longer forget to ring, because the ring happens in the
/// layer every write already goes through.
pub struct MetaDatabase {
    conn: Mutex<Connection>,
    /// The sync doorbell rung by the update hook. The database (not the
    /// VFS) owns it: `MetaDatabase` necessarily exists before any
    /// consumer ([`crate::vfs::Vfs::new`] takes an `Arc<MetaDatabase>`),
    /// so the doorbell's lifetime equals the hook's with no constructor
    /// ordering or injection to get wrong — the alternative (Vfs builds
    /// the `Notify`, then injects it into an already-open connection)
    /// would need a post-construction setter and leaves a window where
    /// the hook exists but rings a placeholder. `Vfs::sync_notifier`
    /// delegates here, so every existing consumer keeps its handle.
    files_wake: Arc<Notify>,
    /// While set, the update hook stays silent (see
    /// [`MetaDatabase::suppress_files_hook`]).
    hook_suppressed: Arc<AtomicBool>,
}

/// RAII silence over the files-table doorbell, returned by
/// [`MetaDatabase::suppress_files_hook`]. Dropping it re-enables the
/// hook — early returns included — so a suppressed span can never leak
/// past its scope.
///
/// The flag is connection-global and guards are not counted: nesting a
/// second guard inside a live one re-enables the doorbell at the inner
/// drop. The only possible failure of that shape is a benign extra wake
/// (one extra sync pass that finds nothing), never a lost one — so the
/// simple bool is enough for the two call sites (sync apply, hydrate
/// cache-flag flips), neither of which nests. A files write racing a
/// suppression span from another thread is also silenced; both spans
/// contain no await points, and the cost is bounded to at most one
/// fallback-tick delay with eventual consistency intact.
#[must_use = "the hook stays silent only while the guard is alive; dropping it re-enables immediately"]
pub struct FilesHookSuppression {
    flag: Arc<AtomicBool>,
}

impl Drop for FilesHookSuppression {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

/// Column list shared by every `files` SELECT (index-mapped by
/// [`row_to_file`]). `encryption_scheme` is selected by name, so the
/// logical order here never depends on the physical column order (the
/// migration appends the column at the end of adopted databases).
const FILE_COLUMNS: &str = "id, rel_path, name, parent_dir, size, mtime, sha256, \
     is_dir, telegram_msg_id, is_uploaded, is_cached, is_encrypted, chunk_count, \
     mime_type, encryption_scheme, created_at, updated_at";

/// Maps a `files` row (in [`FILE_COLUMNS`] order) to a [`FileRecord`].
fn row_to_file(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord {
        id: row.get(0)?,
        rel_path: row.get(1)?,
        name: row.get(2)?,
        parent_dir: row.get(3)?,
        size: row.get(4)?,
        mtime: row.get(5)?,
        sha256: row.get(6)?,
        is_dir: row.get::<_, i64>(7)? != 0,
        telegram_msg_id: row.get(8)?,
        is_uploaded: row.get::<_, i64>(9)? != 0,
        is_cached: row.get::<_, i64>(10)? != 0,
        is_encrypted: row.get::<_, i64>(11)? != 0,
        chunk_count: row.get(12)?,
        mime_type: row.get(13)?,
        encryption_scheme: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

/// Runs `sql` (a `files` SELECT in [`FILE_COLUMNS`] order) and collects
/// every row into a [`FileRecord`].
fn query_files(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<FileRecord>, DbError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, row_to_file)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

impl MetaDatabase {
    /// Opens (creating if needed) the database at `path`.
    ///
    /// Per connection: `journal_mode = WAL`, `synchronous = NORMAL`,
    /// `foreign_keys = ON`, `busy_timeout = 5000`. Creates `files`, `chunks`
    /// and `stats` plus the three indexes with `IF NOT EXISTS`, using DDL
    /// byte-identical to the Python `_init_db`; then, in a separate batch,
    /// the Rust-added sync-lite tables `sync_mirror` / `sync_state` (also
    /// `IF NOT EXISTS`, so pre-sync-lite databases gain them at open).
    /// Existing databases (e.g. produced by the Python version) are adopted
    /// without data loss.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        // Python parity: `os.makedirs(os.path.dirname(db_path), exist_ok=True)`
        // (best effort — the constructor has no error channel).
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        // DDL verbatim from the Python `_init_db` (files before chunks: the
        // foreign key must have its target when FK enforcement is on).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                rel_path TEXT UNIQUE NOT NULL,
                name TEXT NOT NULL,
                parent_dir TEXT NOT NULL,
                size INTEGER DEFAULT 0,
                mtime REAL DEFAULT 0,
                sha256 TEXT,
                is_dir INTEGER DEFAULT 0,
                telegram_msg_id INTEGER,
                is_uploaded INTEGER DEFAULT 0,
                is_cached INTEGER DEFAULT 1,
                is_encrypted INTEGER DEFAULT 0,
                chunk_count INTEGER DEFAULT 1,
                mime_type TEXT,
                created_at REAL,
                updated_at REAL
            );

            CREATE TABLE IF NOT EXISTS chunks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_id INTEGER NOT NULL,
                chunk_index INTEGER NOT NULL,
                telegram_msg_id INTEGER,
                size INTEGER NOT NULL,
                sha256 TEXT,
                FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE,
                UNIQUE(file_id, chunk_index)
            );

            CREATE TABLE IF NOT EXISTS stats (
                key TEXT PRIMARY KEY,
                value TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_files_parent ON files(parent_dir);
            CREATE INDEX IF NOT EXISTS idx_files_msg_id ON files(telegram_msg_id);
            CREATE INDEX IF NOT EXISTS idx_files_uploaded ON files(is_uploaded);",
        )?;
        // Rust-added sync-lite tables (docs/plans/2026-09-04-sync-lite.md,
        // client side) in a separate batch so the Python-contract DDL above
        // stays byte-identical. Purely additive: `IF NOT EXISTS` means a
        // database from before sync-lite gains these at open (adoption
        // without data loss), and neither table is referenced by the
        // contract tables.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sync_mirror (
                rel_path TEXT PRIMARY KEY,
                row_hash TEXT,
                server_version INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS sync_state (
                id INTEGER PRIMARY KEY CHECK(id=0),
                max_pulled INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS sync_client_id (
                id INTEGER PRIMARY KEY CHECK(id=0),
                client_id TEXT NOT NULL
            );",
        )?;
        // Rust-added `rebuild_state` KV table (Phase 8 / D8①, the
        // resumable-rebuild checkpoint): purely additive `IF NOT EXISTS`
        // in its own batch (`sync_mirror` precedent), so the contract
        // DDL above stays byte-identical. Three keys, all owned by
        // `crate::rebuild`: `pending` (the remaining directory queue, a
        // JSON array), `scan_started_at` (the FIRST pass's start time —
        // reused, never reset, on a resumed pass; the completion sweep's
        // protection anchor) and `entries_done` (the cumulative
        // materialized count).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS rebuild_state (
                key TEXT PRIMARY KEY,
                value TEXT
            );",
        )?;
        // Rust-added `files.encryption_scheme` column (Batch E / E-4, red
        // line R6 additive schema): the Python-contract DDL batch above
        // stays byte-identical, so the column lands here as a separate
        // `ALTER TABLE ... ADD COLUMN` guarded by a pragma probe (SQLite
        // has no `ADD COLUMN IF NOT EXISTS`; the probe keeps re-openings
        // and concurrently adopted databases idempotent). The `DEFAULT
        // 'gcm'` backfills every pre-existing row to the frozen v1 scheme
        // — exactly the behavior those rows already had — and keeps every
        // Python-shaped INSERT (column omitted) working unchanged. A
        // Python baseline instance reading this database later simply
        // ignores the unknown column (SQLite never projects unstated
        // columns); nothing is renamed or dropped.
        {
            let has_column = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM pragma_table_info('files') \
                     WHERE name = 'encryption_scheme')",
                [],
                |row| row.get::<_, i64>(0),
            )? != 0;
            if !has_column {
                conn.execute_batch(
                    "ALTER TABLE files \
                     ADD COLUMN encryption_scheme TEXT NOT NULL DEFAULT 'gcm';",
                )?;
            }
        }
        // Files-table change doorbell (wake chokepoint batch): one
        // update_hook per connection, installed once here so any later
        // row change — from this crate, the WebDAV adapter, the web
        // dashboard, the upload queue or a future caller — rings the
        // same [`Notify`] the CLI's sync task waits on. rusqlite's hook
        // is `FnMut + Send + 'static` and fires synchronously on the
        // thread executing the write (while that writer holds the
        // connection mutex), so the callback owns its state through
        // captured `Arc`s instead of borrowing the connection; a
        // `Notify` handle is deliberately cheap to clone for exactly
        // this cross-thread shape, and `notify_one` is sync-call-safe.
        //
        // Scope and timing notes, all benign by design:
        // - the hook fires per changed row, pre-commit inside
        //   transactions — a rolled-back `delete_file` transaction can
        //   ring for a change that never landed, costing one redundant
        //   sync pass; many rows (a directory rename) coalesce into one
        //   permit (`notify_one` stores at most one);
        // - one-off tools that open the db without a sync task
        //   (migrate, setup, doctor) install the hook with no waiter:
        //   at most one permit is parked and dropped with the database
        //   — a no-op;
        // - DDL and the sync-lite tables never fire it (row INSERT /
        //   UPDATE / DELETE on `files` only).
        let files_wake = Arc::new(Notify::new());
        let hook_suppressed = Arc::new(AtomicBool::new(false));
        {
            let wake = Arc::clone(&files_wake);
            let suppressed = Arc::clone(&hook_suppressed);
            conn.update_hook(Some(
                move |action: Action, _db_name: &str, table: &str, _row_id: i64| {
                    if table != "files" {
                        return;
                    }
                    match action {
                        Action::SQLITE_INSERT | Action::SQLITE_UPDATE | Action::SQLITE_DELETE => {}
                        // SQLITE_UNKNOWN and any future action: not a
                        // row change we can interpret — stay silent.
                        _ => return,
                    }
                    if suppressed.load(Ordering::Acquire) {
                        return;
                    }
                    wake.notify_one();
                },
            ))?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
            files_wake,
            hook_suppressed,
        })
    }

    /// The shared sync doorbell (doorbell model, quasi-realtime batch):
    /// the CLI's periodic sync task holds this handle and waits on
    /// `notified()` alongside its interval tick, turning files-table
    /// changes into immediate passes. The update hook rings it on every
    /// `files` row change; `notify_one` (not `notify_waiters`) keeps the
    /// batch's permit semantics — a wake arriving while a pass is in
    /// flight parks a permit and fires a follow-up pass, and several
    /// wakes coalesce into one pass ("many events, one pass").
    pub fn sync_notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.files_wake)
    }

    /// Silences the files-table doorbell for the lifetime of the
    /// returned guard (drop restores it). The consumer is code whose
    /// own writes must not trigger a local sync pass:
    ///
    /// - the sync engine's pull-apply (`sync::apply_pulled_rows`) —
    ///   remote rows applied locally; ringing would start a redundant
    ///   pass for data that already is at its server version;
    /// - hydrate's `is_cached` flips (and cache clears) — a local-only
    ///   flag the sync payload deliberately excludes, so a pass for it
    ///   is pure waste.
    pub fn suppress_files_hook(&self) -> FilesHookSuppression {
        self.hook_suppressed.store(true, Ordering::Release);
        FilesHookSuppression {
            flag: Arc::clone(&self.hook_suppressed),
        }
    }

    /// Inserts or updates the row keyed by `rel_path`.
    ///
    /// Python semantics: `ON CONFLICT(rel_path) DO UPDATE` refreshes every
    /// column except `rel_path`, but `sha256`, `telegram_msg_id` and
    /// `mime_type` are `coalesce(new, old)` — a `None` keeps the stored
    /// value. `created_at` is set on first insert, `updated_at` on every
    /// write. Returns the surviving row's real id via `RETURNING id`
    /// (repeat upserts of the same `rel_path` return the same id).
    pub fn upsert_file(&self, entry: &FileUpsert) -> Result<i64, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = now();
        let id = conn.query_row(
            "INSERT INTO files (
                rel_path, name, parent_dir, size, mtime, sha256, is_dir,
                telegram_msg_id, is_uploaded, is_cached, is_encrypted, chunk_count, mime_type,
                created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
            ON CONFLICT(rel_path) DO UPDATE SET
                name=excluded.name,
                parent_dir=excluded.parent_dir,
                size=excluded.size,
                mtime=excluded.mtime,
                sha256=coalesce(excluded.sha256, files.sha256),
                telegram_msg_id=coalesce(excluded.telegram_msg_id, files.telegram_msg_id),
                is_uploaded=excluded.is_uploaded,
                is_cached=excluded.is_cached,
                is_encrypted=excluded.is_encrypted,
                chunk_count=excluded.chunk_count,
                mime_type=coalesce(excluded.mime_type, files.mime_type),
                updated_at=excluded.updated_at
            RETURNING id",
            params![
                entry.rel_path,
                entry.name,
                entry.parent_dir,
                entry.size,
                entry.mtime,
                entry.sha256,
                entry.is_dir as i64,
                entry.telegram_msg_id,
                entry.is_uploaded as i64,
                entry.is_cached as i64,
                entry.is_encrypted as i64,
                entry.chunk_count,
                entry.mime_type,
                now,
                now,
            ],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Inserts or updates the row keyed by `rel_path`, additionally
    /// writing the `encryption_scheme` column (Batch E / E-4).
    ///
    /// Semantics identical to [`MetaDatabase::upsert_file`] except the
    /// scheme column is set to `encryption_scheme` on both the insert and
    /// the conflict-update paths. Callers: the VFS `put` path (recording
    /// the configured scheme on newly-flagged encrypted rows) and the
    /// sync apply (`replace_row`, restoring a pulled row's scheme after
    /// its delete+insert). Everything else keeps using
    /// [`MetaDatabase::upsert_file`], under which the column is
    /// `DEFAULT 'gcm'` on insert and silently preserved on update — so
    /// every pre-E-4 writer (this build, the Python baseline) leaves the
    /// column exactly as it was.
    pub fn upsert_file_scheme(
        &self,
        entry: &FileUpsert,
        encryption_scheme: &str,
    ) -> Result<i64, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = now();
        let id = conn.query_row(
            "INSERT INTO files (
                rel_path, name, parent_dir, size, mtime, sha256, is_dir,
                telegram_msg_id, is_uploaded, is_cached, is_encrypted, chunk_count, mime_type,
                encryption_scheme, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
            ON CONFLICT(rel_path) DO UPDATE SET
                name=excluded.name,
                parent_dir=excluded.parent_dir,
                size=excluded.size,
                mtime=excluded.mtime,
                sha256=coalesce(excluded.sha256, files.sha256),
                telegram_msg_id=coalesce(excluded.telegram_msg_id, files.telegram_msg_id),
                is_uploaded=excluded.is_uploaded,
                is_cached=excluded.is_cached,
                is_encrypted=excluded.is_encrypted,
                chunk_count=excluded.chunk_count,
                mime_type=coalesce(excluded.mime_type, files.mime_type),
                encryption_scheme=excluded.encryption_scheme,
                updated_at=excluded.updated_at
            RETURNING id",
            params![
                entry.rel_path,
                entry.name,
                entry.parent_dir,
                entry.size,
                entry.mtime,
                entry.sha256,
                entry.is_dir as i64,
                entry.telegram_msg_id,
                entry.is_uploaded as i64,
                entry.is_cached as i64,
                entry.is_encrypted as i64,
                entry.chunk_count,
                entry.mime_type,
                encryption_scheme,
                now,
                now,
            ],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Flips the `is_cached` flag of the row with primary key `id` and
    /// touches NOTHING else — hydrate/eviction's cache-flag bookkeeping
    /// (P3 snapshot write-back race fix: those paths used to rebuild the
    /// whole row from a pre-download snapshot and upsert it back, which
    /// resurrected any column concurrently updated inside the download
    /// window — a PUT overwrite's new size/msg id, a sync-applied remote
    /// version — and the stale values then spread via push; same shape
    /// as E-4's frozen-contract-upsert + targeted-column-write split).
    /// An affected-rows count of 0 means the row vanished mid-operation
    /// (e.g. a concurrent delete) — a benign no-op, surfaced as `Ok`.
    ///
    /// Like every `is_cached`-only write the caller wraps the call in
    /// [`MetaDatabase::suppress_files_hook`]: the flag is local state the
    /// sync payload excludes, so the files-table doorbell stays silent.
    pub fn set_cached_flag(&self, id: i64, is_cached: bool) -> Result<(), DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        conn.execute(
            "UPDATE files SET is_cached = ?1 WHERE id = ?2",
            params![is_cached as i64, id],
        )?;
        Ok(())
    }

    /// Looks up a single row by its unique virtual path.
    pub fn get_file(&self, rel_path: &str) -> Result<Option<FileRecord>, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sql = format!("SELECT {FILE_COLUMNS} FROM files WHERE rel_path = ?1");
        Ok(conn.query_row(&sql, [rel_path], row_to_file).optional()?)
    }

    /// Looks up a single row by the Telegram message id of its chunk 0.
    pub fn get_file_by_msg_id(&self, msg_id: i64) -> Result<Option<FileRecord>, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sql = format!("SELECT {FILE_COLUMNS} FROM files WHERE telegram_msg_id = ?1");
        Ok(conn.query_row(&sql, [msg_id], row_to_file).optional()?)
    }

    /// Lists direct children of `parent_dir`.
    ///
    /// Python ordering: `ORDER BY is_dir DESC, name ASC` — directories
    /// first, then name-ascending.
    pub fn list_dir(&self, parent_dir: &str) -> Result<Vec<FileRecord>, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sql = format!(
            "SELECT {FILE_COLUMNS} FROM files \
             WHERE parent_dir = ?1 ORDER BY is_dir DESC, name ASC"
        );
        query_files(&conn, &sql, &[&parent_dir])
    }

    /// Lists every row (files and directories) by `updated_at DESC`.
    pub fn list_all_files(&self) -> Result<Vec<FileRecord>, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sql = format!("SELECT {FILE_COLUMNS} FROM files ORDER BY updated_at DESC");
        query_files(&conn, &sql, &[])
    }

    /// Substring search over `name` and `rel_path`.
    ///
    /// Python semantics: `LIKE '%query%'` with no escaping of `%` / `_`,
    /// ordered by `is_dir DESC, name ASC`.
    pub fn search_files(&self, query: &str) -> Result<Vec<FileRecord>, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Python parity: no escaping of LIKE wildcards in the query.
        let pattern = format!("%{query}%");
        let sql = format!(
            "SELECT {FILE_COLUMNS} FROM files \
             WHERE name LIKE ?1 OR rel_path LIKE ?1 ORDER BY is_dir DESC, name ASC"
        );
        query_files(&conn, &sql, &[&pattern])
    }

    /// Inserts or updates one chunk of `file_id`.
    ///
    /// `ON CONFLICT(file_id, chunk_index) DO UPDATE` refreshes
    /// `telegram_msg_id` and `size`; `sha256` is `coalesce(new, old)`.
    pub fn upsert_chunk(
        &self,
        file_id: i64,
        chunk_index: i64,
        telegram_msg_id: i64,
        size: i64,
        sha256: Option<&str>,
    ) -> Result<(), DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        conn.execute(
            "INSERT INTO chunks (file_id, chunk_index, telegram_msg_id, size, sha256)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(file_id, chunk_index) DO UPDATE SET
                telegram_msg_id=excluded.telegram_msg_id,
                size=excluded.size,
                sha256=coalesce(excluded.sha256, chunks.sha256)",
            params![file_id, chunk_index, telegram_msg_id, size, sha256],
        )?;
        Ok(())
    }

    /// Lists the chunks of `file_id` ordered by `chunk_index ASC`.
    pub fn get_chunks_by_file_id(&self, file_id: i64) -> Result<Vec<ChunkRecord>, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut stmt = conn.prepare(
            "SELECT chunk_index, telegram_msg_id, size, sha256 FROM chunks \
                          WHERE file_id = ?1 ORDER BY chunk_index ASC",
        )?;
        let rows = stmt.query_map([file_id], |row| {
            Ok(ChunkRecord {
                chunk_index: row.get(0)?,
                telegram_msg_id: row.get(1)?,
                size: row.get(2)?,
                sha256: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Deletes the row at `rel_path` and, in one transaction, its chunks.
    ///
    /// The explicit `DELETE FROM chunks` (before `DELETE FROM files`) keeps
    /// older databases intact even though `foreign_keys = ON` would cascade.
    /// Deleting a missing path is a no-op, not an error.
    pub fn delete_file(&self, rel_path: &str) -> Result<(), DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // `unchecked_transaction`: `&self` API (Connection::transaction
        // needs &mut) — no reentrancy, and the guard keeps the whole
        // transaction exclusive anyway.
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM chunks WHERE file_id = (SELECT id FROM files WHERE rel_path = ?1)",
            [rel_path],
        )?;
        tx.execute("DELETE FROM files WHERE rel_path = ?1", [rel_path])?;
        tx.commit()?;
        Ok(())
    }

    /// The Phase 8 / D8③ completion sweep: deletes every UPLOADED row
    /// the finished scan did not re-touch (`updated_at` older than the
    /// scan's persisted `scan_started_at`) and, in the same transaction,
    /// those rows' `chunks` rows — the explicit child delete of
    /// [`MetaDatabase::delete_file`] (not FK-cascade reliance, so older
    /// databases and pragma variations behave identically). Returns the
    /// number of `files` rows deleted.
    ///
    /// The three D8 protections this predicate buys:
    /// 1. rows upserted during ANY pass of the scan — this one or an
    ///    earlier resumed one — carry `updated_at ≥ scan_started_at`
    ///    (the anchor is persisted by the first pass and reused, never
    ///    reset) and survive;
    /// 2. in-flight rows (`is_uploaded = 0`) are excluded outright;
    /// 3. `rebuild` calls this only after its work queue has drained —
    ///    an interrupted pass never reaches it.
    ///
    /// Rows with a `NULL` `updated_at` compare as `NULL < x` = unknown
    /// and are conservatively kept.
    pub fn sweep_unseen(&self, scan_started_at: f64) -> Result<usize, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM chunks WHERE file_id IN \
             (SELECT id FROM files WHERE is_uploaded = 1 AND updated_at < ?1)",
            params![scan_started_at],
        )?;
        let deleted = tx.execute(
            "DELETE FROM files WHERE is_uploaded = 1 AND updated_at < ?1",
            params![scan_started_at],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    /// The completion sweep's floor census (M3 / Phase 8 review): a
    /// `(uploaded_total, sweep_candidates)` pair at this anchor — the
    /// population the sweep's volume floor judges, read WITHOUT deleting
    /// anything. `uploaded_total` counts every `is_uploaded = 1` row (the
    /// sweep's whole population); `sweep_candidates` counts the rows the
    /// sweep WOULD delete at this anchor (`updated_at` predating it).
    pub fn sweep_census(&self, scan_started_at: f64) -> Result<(usize, usize), DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (total, candidates): (i64, i64) = conn.query_row(
            "SELECT (SELECT COUNT(*) FROM files WHERE is_uploaded = 1), \
                    (SELECT COUNT(*) FROM files WHERE is_uploaded = 1 AND updated_at < ?1)",
            params![scan_started_at],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((total as usize, candidates as usize))
    }

    /// Clears the `is_cached` flag on every non-directory row that has it
    /// set **and is already uploaded**, returning the number of changed
    /// rows (the `cache clear` command's freed-flags count). Directory
    /// rows keep their flag — theirs is a row-shape constant (born
    /// uploaded + cached), not evidence of a local cache copy. Pending
    /// uploads keep their flag too: their cache copy is the only copy of
    /// the bytes (plan revision A1), so it must not read as freed.
    pub fn clear_cached_flags(&self) -> Result<u64, DbError> {
        // is_cached-only UPDATE — a local flag the sync payload excludes,
        // so the doorbell stays silent for it (same rationale as
        // hydrate's suppressed cache-flag flips).
        let _quiet = self.suppress_files_hook();
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = conn.execute(
            "UPDATE files SET is_cached = 0 \
             WHERE is_dir = 0 AND is_cached = 1 AND is_uploaded = 1",
            [],
        )?;
        Ok(changed as u64)
    }

    /// Virtual paths of every pending upload (`is_uploaded = 0`,
    /// non-directory). The `cache clear` command preserves these rows'
    /// local cache copies — for a pending upload that copy is the only
    /// copy of the bytes (plan revision A1).
    pub fn pending_file_paths(&self) -> Result<Vec<String>, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut stmt =
            conn.prepare("SELECT rel_path FROM files WHERE is_uploaded = 0 AND is_dir = 0")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Moves the row at `from` to `to`, rewriting `rel_path` / `name` /
    /// `parent_dir`; when the row is a directory, every descendant row's
    /// path fields move under the new prefix as well. All of it in one
    /// transaction.
    ///
    /// Rows keep their ids, so `chunks` linkage and every other column
    /// survive verbatim — an upsert-at-dest + delete-at-source pair would
    /// orphan the chunk rows (they key on the old `files.id`). Renaming a
    /// missing path is a no-op, mirroring [`MetaDatabase::delete_file`].
    pub fn rename_path(&self, from: &str, to: &str) -> Result<(), DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tx = conn.unchecked_transaction()?;
        // Prefix matching uses substr comparisons, not LIKE: virtual paths
        // may legitimately contain `%` or `_`, which LIKE treats as
        // wildcards. Direct children carry `parent_dir = from` exactly and
        // must land on `to` itself, not on a rewritten prefix.
        tx.execute(
            "UPDATE files SET
                rel_path = ?2 || substr(rel_path, length(?1) + 1),
                parent_dir = CASE
                    WHEN parent_dir = ?1 THEN ?2
                    ELSE ?2 || substr(parent_dir, length(?1) + 1)
                END
            WHERE rel_path <> ?1
              AND substr(rel_path, 1, length(?1) + 1) = ?1 || '/'",
            params![from, to],
        )?;
        // Path fields of the moved node itself, derived the same way the
        // upsert helpers do (final segment, parent with `/` root).
        let name = to.rsplit_once('/').map_or(to, |(_, name)| name);
        let parent = match to.rfind('/') {
            Some(0) | None => "/",
            Some(i) => &to[..i],
        };
        tx.execute(
            "UPDATE files SET rel_path = ?2, name = ?3, parent_dir = ?4, updated_at = ?5
             WHERE rel_path = ?1",
            params![from, to, name, parent, now()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Aggregated statistics, exactly the Python `get_stats` SQL:
    /// totals over `is_dir = 0` rows, dirs over `is_dir = 1`, uploaded
    /// over `is_uploaded = 1 AND is_dir = 0`, pending as the difference.
    pub fn get_stats(&self) -> Result<Stats, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // `or 0` in Python: SUM over zero rows is NULL, COUNT is never NULL.
        let (total_files, total_bytes): (i64, Option<i64>) = conn.query_row(
            "SELECT COUNT(*), SUM(size) FROM files WHERE is_dir = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let (total_dirs, uploaded_files): (i64, i64) = conn.query_row(
            "SELECT (SELECT COUNT(*) FROM files WHERE is_dir = 1),
                    (SELECT COUNT(*) FROM files WHERE is_uploaded = 1 AND is_dir = 0)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let total_bytes = total_bytes.unwrap_or(0);
        Ok(Stats {
            total_files,
            total_bytes,
            total_dirs,
            uploaded_files,
            pending_uploads: total_files - uploaded_files,
        })
    }

    // ----------------------------------------------- sync-lite tables ---
    //
    // Client-side state of the sync-lite metadata mirror (2026-09-04 plan,
    // «客户端»): `sync_mirror` holds, per virtual path, the row hash and
    // server version of the last synced logical row; `sync_state` holds the
    // single `id = 0` row with the highest server version pulled. Purely
    // additive schema — the sync engine itself lives in later units.

    /// Reads the mirror row for `rel_path`: its `(row_hash, server_version)`
    /// as of the last sync, or `None` when the path was never synced.
    pub fn sync_mirror_get(&self, rel_path: &str) -> Result<Option<(String, i64)>, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(conn
            .query_row(
                "SELECT row_hash, server_version FROM sync_mirror WHERE rel_path = ?1",
                [rel_path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// Upserts the mirror row for `rel_path` (both columns overwrite).
    pub fn sync_mirror_set(
        &self,
        rel_path: &str,
        row_hash: &str,
        server_version: i64,
    ) -> Result<(), DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        conn.execute(
            "INSERT INTO sync_mirror (rel_path, row_hash, server_version)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(rel_path) DO UPDATE SET
                row_hash=excluded.row_hash,
                server_version=excluded.server_version",
            params![rel_path, row_hash, server_version],
        )?;
        Ok(())
    }

    /// Deletes the mirror row for `rel_path`. Deleting a path that was
    /// never mirrored is a no-op, mirroring [`MetaDatabase::delete_file`].
    pub fn sync_mirror_delete(&self, rel_path: &str) -> Result<(), DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        conn.execute("DELETE FROM sync_mirror WHERE rel_path = ?1", [rel_path])?;
        Ok(())
    }

    /// Every mirror row as `(rel_path, row_hash, server_version)`, ordered
    /// by `rel_path ASC` — the deterministic input the diff phase walks.
    pub fn sync_mirror_all(&self) -> Result<Vec<(String, String, i64)>, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut stmt = conn.prepare(
            "SELECT rel_path, row_hash, server_version FROM sync_mirror \
             ORDER BY rel_path ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The highest server version pulled so far; a database that never
    /// synced has no row and reads as `0`.
    pub fn sync_state_get(&self) -> Result<i64, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(conn
            .query_row(
                "SELECT max_pulled FROM sync_state WHERE id = 0",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// Upserts `max_pulled` into the single `id = 0` row.
    pub fn sync_state_set(&self, max_pulled: i64) -> Result<(), DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        conn.execute(
            "INSERT INTO sync_state (id, max_pulled) VALUES (0, ?1)
            ON CONFLICT(id) DO UPDATE SET max_pulled=excluded.max_pulled",
            params![max_pulled],
        )?;
        Ok(())
    }

    /// This instance's stable sync identity (quasi-realtime doorbell
    /// batch): the single `sync_client_id` row's value. Get-or-create —
    /// the first call generates 16 random bytes as 32 lowercase hex
    /// characters (`rand`, already a dependency; no uuid crate) and
    /// writes them; every later call returns the stored value. The whole
    /// get-or-create runs under the connection mutex, so concurrent first
    /// callers cannot race two identities into the table.
    pub fn sync_client_id(&self) -> Result<String, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = conn
            .query_row(
                "SELECT client_id FROM sync_client_id WHERE id = 0",
                [],
                |row| row.get(0),
            )
            .optional()?
        {
            return Ok(existing);
        }
        let id = hex_lower(&rand::random::<[u8; 16]>());
        conn.execute(
            "INSERT INTO sync_client_id (id, client_id) VALUES (0, ?1)",
            [&id],
        )?;
        Ok(id)
    }

    // -------------------------------------------------- rebuild_state ---
    //
    // Phase 8 / D8①: the resumable-rebuild checkpoint (see the table's
    // DDL comment in [`MetaDatabase::open`]). The KV surface mirrors the
    // `sync_mirror` trio's shape; every writer and reader lives in
    // `crate::rebuild` — nothing else owns these keys.

    /// Reads one `rebuild_state` value (`None` when the key is absent).
    pub fn rebuild_state_get(&self, key: &str) -> Result<Option<String>, DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(conn
            .query_row(
                "SELECT value FROM rebuild_state WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Upserts one `rebuild_state` value.
    pub fn rebuild_state_set(&self, key: &str, value: &str) -> Result<(), DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        conn.execute(
            "INSERT INTO rebuild_state (key, value) VALUES (?1, ?2)
            ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Clears every `rebuild_state` key — the completing pass's
    /// checkout. The table only ever holds the rebuild checkpoint, so a
    /// blanket delete is the precise inverse of the three writes.
    pub fn rebuild_state_clear(&self) -> Result<(), DbError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        conn.execute("DELETE FROM rebuild_state", [])?;
        Ok(())
    }
}

/// Lowercase hex of `bytes` (the same shape the `sync` and `chunker`
/// helpers use; local copy keeps `database` self-contained).
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
