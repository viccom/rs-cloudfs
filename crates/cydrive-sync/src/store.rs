//! SQLite-backed store for the sync-lite server.
//!
//! Schema (frozen by the sync-lite plan, byte-for-byte column lists):
//!
//! - `namespaces(key TEXT PRIMARY KEY, version INTEGER NOT NULL
//!   DEFAULT 0)` — one monotonic LWW counter per namespace.
//! - `rows(ns_key, rel_path, version, deleted, payload, PRIMARY
//!   KEY(ns_key, rel_path))` — one logical row per path; `version` is
//!   the counter value of its last write, `deleted` the tombstone
//!   flag, `payload` an opaque string.
//!
//! Concurrency follows the cydrive-core `MetaDatabase` precedent
//! (decision log 2026-09-02): the connection sits behind a
//! `std::sync::Mutex` so `&self` methods are callable from any thread
//! (axum handlers run on the tokio runtime); every public method
//! takes the lock for its whole body and no public method calls
//! another, so the lock is never re-entered.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension};

use crate::wire::{PulledRow, PushRow};

/// Errors from the store. Everything funnels through rusqlite: the
/// schema is `IF NOT EXISTS` (no migration failures) and inputs are
/// plain strings, so SQLite errors are the only failure mode.
#[derive(Debug, thiserror::Error)]
#[error("sqlite error: {0}")]
pub struct SyncStoreError(#[from] rusqlite::Error);

/// DDL verbatim from the sync-lite plan.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS namespaces(
    key TEXT PRIMARY KEY,
    version INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS rows(
    ns_key TEXT,
    rel_path TEXT,
    version INTEGER,
    deleted INTEGER NOT NULL DEFAULT 0,
    payload TEXT NOT NULL,
    PRIMARY KEY(ns_key, rel_path)
);
";

/// The sync server's entire state: two tables behind one connection.
pub struct SyncStore {
    conn: Mutex<Connection>,
}

impl SyncStore {
    /// Opens (creating parent directory and file if needed) the sync
    /// database at `path`. Existing databases are adopted as-is; the
    /// `IF NOT EXISTS` DDL is a no-op for them.
    pub fn open(path: &Path) -> Result<Self, SyncStoreError> {
        // Best-effort parent creation, mirroring MetaDatabase::open —
        // Connection::open reports the real error if this cannot work.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self::init(Connection::open(path)?)
    }

    /// An in-memory database (tests and examples).
    pub fn open_in_memory() -> Result<Self, SyncStoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, SyncStoreError> {
        // Same tuning as MetaDatabase::open; on an in-memory database
        // the WAL pragma is harmlessly a no-op.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // MetaDatabase precedent: a poisoned lock means some earlier
        // request panicked mid-statement; the connection itself is
        // still usable, so recover instead of cascading the panic.
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Upserts `rows` into namespace `ns_key` inside a single
    /// transaction: the namespace is auto-registered at version 0 if
    /// new, then every row (in batch order) consumes the next counter
    /// value and overwrites any previous row at the same `rel_path` —
    /// last pusher wins. Returns the counter after the batch (which is
    /// the response's `max_version`).
    ///
    /// Atomicity: all increments and row writes of the batch commit
    /// together or not at all.
    pub fn push(&self, ns_key: &str, rows: &[PushRow]) -> Result<i64, SyncStoreError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO namespaces(key) VALUES(?1) ON CONFLICT DO NOTHING",
            [ns_key],
        )?;
        let mut version: i64 = tx.query_row(
            "SELECT version FROM namespaces WHERE key = ?1",
            [ns_key],
            |row| row.get(0),
        )?;
        for row in rows {
            version += 1;
            tx.execute(
                "INSERT INTO rows(ns_key, rel_path, version, deleted, payload)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(ns_key, rel_path) DO UPDATE SET
                    version = excluded.version,
                    deleted = excluded.deleted,
                    payload = excluded.payload",
                rusqlite::params![
                    ns_key,
                    row.rel_path,
                    version,
                    row.deleted as i64,
                    row.payload
                ],
            )?;
        }
        tx.execute(
            "UPDATE namespaces SET version = ?2 WHERE key = ?1",
            rusqlite::params![ns_key, version],
        )?;
        tx.commit()?;
        Ok(version)
    }

    /// Returns every row of `ns_key` whose version exceeds `since`,
    /// ordered by version ascending (natural apply order), plus the
    /// namespace's current counter.
    ///
    /// An unknown namespace yields `([], 0)` and is NOT registered
    /// (pull is read-only).
    pub fn pull(&self, ns_key: &str, since: i64) -> Result<(Vec<PulledRow>, i64), SyncStoreError> {
        let conn = self.lock();
        let max_version = conn
            .query_row(
                "SELECT version FROM namespaces WHERE key = ?1",
                [ns_key],
                |row| row.get(0),
            )
            .optional()?;
        let Some(max_version) = max_version else {
            return Ok((Vec::new(), 0));
        };
        let mut stmt = conn.prepare(
            "SELECT rel_path, version, deleted, payload FROM rows
             WHERE ns_key = ?1 AND version > ?2
             ORDER BY version ASC",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![ns_key, since], |row| {
                Ok(PulledRow {
                    rel_path: row.get(0)?,
                    version: row.get(1)?,
                    deleted: row.get::<_, i64>(2)? != 0,
                    payload: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((rows, max_version))
    }

    /// The namespace's current counter, or `None` when it is not
    /// registered (pull of an unknown namespace must keep it that
    /// way — this accessor exists to pin that in tests).
    pub fn namespace_version(&self, ns_key: &str) -> Result<Option<i64>, SyncStoreError> {
        self.lock()
            .query_row(
                "SELECT version FROM namespaces WHERE key = ?1",
                [ns_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(SyncStoreError::from)
    }
}
