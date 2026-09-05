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
//! contract DDL above stays byte-identical and untouched.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

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
pub struct MetaDatabase {
    conn: Mutex<Connection>,
}

/// Column list shared by every `files` SELECT (index-mapped by
/// [`row_to_file`]).
const FILE_COLUMNS: &str = "id, rel_path, name, parent_dir, size, mtime, sha256, \
     is_dir, telegram_msg_id, is_uploaded, is_cached, is_encrypted, chunk_count, \
     mime_type, created_at, updated_at";

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
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
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
        Ok(Self {
            conn: Mutex::new(conn),
        })
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

    /// Clears the `is_cached` flag on every non-directory row that has it
    /// set **and is already uploaded**, returning the number of changed
    /// rows (the `cache clear` command's freed-flags count). Directory
    /// rows keep their flag — theirs is a row-shape constant (born
    /// uploaded + cached), not evidence of a local cache copy. Pending
    /// uploads keep their flag too: their cache copy is the only copy of
    /// the bytes (plan revision A1), so it must not read as freed.
    pub fn clear_cached_flags(&self) -> Result<u64, DbError> {
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
