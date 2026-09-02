// Copyright 2020 - developers of the `grammers` project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Vendored modification: ported from libsql to rusqlite (bundled) so the
// workspace links a single SQLite C library; the schema, statements and
// write-through semantics are upstream's, unchanged. See VENDOR.md.

use std::collections::HashMap;
use std::fmt;
use std::net::AddrParseError;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use rusqlite::{named_params, params};

use crate::types::{
    ChannelKind, ChannelState, DcOption, PeerAuth, PeerId, PeerInfo, PeerKind, UpdateState,
    UpdatesState,
};
use crate::{BoxFuture, DEFAULT_DC, KNOWN_DC_OPTIONS, Session};

const VERSION: i64 = 1;

struct Database(rusqlite::Connection);

struct Cache {
    pub home_dc: i32,
    pub dc_options: HashMap<i32, DcOption>,
}

/// SQLite-based storage. This is the recommended option.
pub struct SqliteSession {
    database: Mutex<Database>,
    cache: Mutex<Cache>,
}

#[derive(Debug)]
pub enum SqliteSessionError {
    AddrParse(std::net::AddrParseError),
    Sql(rusqlite::Error),
    InvalidAuthKeyLength(usize),
}

impl std::error::Error for SqliteSessionError {}

impl fmt::Display for SqliteSessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqliteSessionError::AddrParse(_) => write!(f, "invalid socket address syntax"),
            SqliteSessionError::Sql(err) => write!(f, "{err}"),
            SqliteSessionError::InvalidAuthKeyLength(actual) => {
                write!(f, "invalid auth_key length: expected 256, got {actual}")
            }
        }
    }
}

impl From<AddrParseError> for SqliteSessionError {
    fn from(x: AddrParseError) -> Self {
        Self::AddrParse(x)
    }
}

impl From<rusqlite::Error> for SqliteSessionError {
    fn from(x: rusqlite::Error) -> Self {
        Self::Sql(x)
    }
}

#[repr(u8)]
enum PeerSubtype {
    UserSelf = 1,
    UserBot = 2,
    UserSelfBot = 3,
    Megagroup = 4,
    Broadcast = 8,
    Gigagroup = 12,
}

impl Database {
    fn init(&self) -> Result<(), SqliteSessionError> {
        let mut user_version: i64 = self
            .fetch_one("PRAGMA user_version", params![], |row| {
                row.get::<_, i64>(0)
            })?
            .unwrap_or(0);
        if user_version == VERSION {
            return Ok(());
        }

        if user_version == 0 {
            self.migrate_v0_to_v1()?;
            user_version += 1;
        }
        if user_version == VERSION {
            // Can't bind PRAGMA parameters, but `VERSION` is not user-controlled input.
            self.0
                .execute(&format!("PRAGMA user_version = {VERSION}"), params![])?;
        }
        Ok(())
    }

    fn migrate_v0_to_v1(&self) -> Result<(), SqliteSessionError> {
        let transaction = self.begin_transaction()?;
        transaction
            .execute(
                "CREATE TABLE dc_home (
                dc_id INTEGER NOT NULL,
                PRIMARY KEY(dc_id))",
                params![],
            )?;
        transaction
            .execute(
                "CREATE TABLE dc_option (
                dc_id INTEGER NOT NULL,
                ipv4 TEXT NOT NULL,
                ipv6 TEXT NOT NULL,
                auth_key BLOB,
                PRIMARY KEY (dc_id))",
                params![],
            )?;
        transaction
            .execute(
                "CREATE TABLE peer_info (
                peer_id INTEGER NOT NULL,
                hash INTEGER,
                subtype INTEGER,
                PRIMARY KEY (peer_id))",
                params![],
            )?;
        transaction
            .execute(
                "CREATE TABLE update_state (
                pts INTEGER NOT NULL,
                qts INTEGER NOT NULL,
                date INTEGER NOT NULL,
                seq INTEGER NOT NULL)",
                params![],
            )?;
        transaction
            .execute(
                "CREATE TABLE channel_state (
                peer_id INTEGER NOT NULL,
                pts INTEGER NOT NULL,
                PRIMARY KEY (peer_id))",
                params![],
            )?;

        transaction.commit()?;
        Ok(())
    }

    /// Opens a transaction on the shared connection. Uses rusqlite's
    /// `unchecked_transaction` (documented exactly for connections shared
    /// behind a `Mutex`, where the `&mut self` `transaction()` is not
    /// available); like upstream, no deferred/immediate distinction is
    /// needed.
    fn begin_transaction(&self) -> rusqlite::Result<rusqlite::Transaction<'_>> {
        self.0.unchecked_transaction()
    }

    fn fetch_one<T, P: rusqlite::Params, F>(&self, statement: &str, params: P, select: F) -> Result<Option<T>, SqliteSessionError>
    where
        F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    {
        match self.0.query_row(statement, params, select) {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn fetch_all<T, P: rusqlite::Params, F>(
        &self,
        statement: &str,
        params: P,
        mut select: F,
    ) -> Result<Vec<T>, SqliteSessionError>
    where
        F: FnMut(&rusqlite::Row<'_>) -> Result<T, SqliteSessionError>,
    {
        let mut statement = self.0.prepare(statement)?;
        let mut rows = statement.query(params)?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            result.push(select(row)?);
        }
        Ok(result)
    }
}

impl SqliteSession {
    /// Locks the database, recovering from mutex poisoning: rusqlite
    /// statements return `Result`s rather than panicking mid-statement, and
    /// even if some other panic happened while holding the lock the SQLite
    /// handle itself remains usable, while dropping the session over a
    /// poisoned lock would lose write-through data.
    fn lock_database(&self) -> MutexGuard<'_, Database> {
        self.database.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Like [`SqliteSession::lock_database`]: recovered rather than
    /// propagated (upstream mapped poisoning to an error variant instead).
    fn lock_cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Open a connection to the SQLite database at `path`,
    /// creating one if it doesn't exist.
    ///
    /// Note: this performs blocking file I/O inside an `async fn` without
    /// `spawn_blocking`. Kept that way deliberately: the vendored crate is
    /// free of a tokio dependency (upstream's async mutex around the libsql
    /// connection is replaced by `std::sync::Mutex`), the session file is
    /// tiny, and `open` runs once at startup, so blocking the executor for
    /// a sub-millisecond local open is a fine trade for not reintroducing
    /// tokio into this crate.
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self, SqliteSessionError> {
        let conn = rusqlite::Connection::open(path)?;
        let db = Database(conn);
        db.init()?;

        let home_dc = db
            .fetch_one("SELECT * FROM dc_home LIMIT 1", named_params![], |row| {
                row.get::<_, i32>(0)
            })?
            .unwrap_or(DEFAULT_DC);

        let dc_options = db
            .fetch_all("SELECT * FROM dc_option", named_params![], |row| {
                Ok(DcOption {
                    id: row.get::<_, i32>(0)?,
                    ipv4: row.get::<_, String>(1)?.parse()?,
                    ipv6: row.get::<_, String>(2)?.parse()?,
                    auth_key: match row.get::<_, Option<Vec<u8>>>(3)? {
                        None => None,
                        Some(auth_key) => Some(auth_key.try_into().map_err(|v: Vec<u8>| {
                            SqliteSessionError::InvalidAuthKeyLength(v.len())
                        })?),
                    },
                })
            })?
            .into_iter()
            .map(|dc_option| (dc_option.id, dc_option))
            .collect();

        Ok(SqliteSession {
            database: Mutex::new(db),
            cache: Mutex::new(Cache {
                home_dc,
                dc_options,
            }),
        })
    }
}

impl Session for SqliteSession {
    type Error = SqliteSessionError;

    fn home_dc_id(&self) -> Result<i32, SqliteSessionError> {
        Ok(self.lock_cache().home_dc)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), SqliteSessionError>> {
        self.lock_cache().home_dc = dc_id;
        Box::pin(async move {
            let db = self.lock_database();
            let transaction = db.begin_transaction()?;
            transaction.execute("DELETE FROM dc_home", params![])?;
            // Scoped so the statement (which has `Drop`) is released before
            // the commit consumes the transaction.
            {
                let mut stmt = transaction.prepare("INSERT INTO dc_home VALUES (:dc_id)")?;
                stmt.execute(named_params! {":dc_id": dc_id})?;
            }
            transaction.commit()?;
            Ok(())
        })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, SqliteSessionError> {
        Ok(self
            .lock_cache()
            .dc_options
            .get(&dc_id)
            .cloned()
            .or_else(|| {
                KNOWN_DC_OPTIONS
                    .iter()
                    .find(|dc_option| dc_option.id == dc_id)
                    .cloned()
            }))
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), SqliteSessionError>> {
        self.lock_cache()
            .dc_options
            .insert(dc_option.id, dc_option.clone());

        let dc_option = dc_option.clone();
        Box::pin(async move {
            let db = self.lock_database();
            db.0.execute(
                "INSERT OR REPLACE INTO dc_option VALUES (:dc_id, :ipv4, :ipv6, :auth_key)",
                named_params! {
                    ":dc_id": dc_option.id,
                    ":ipv4": dc_option.ipv4.to_string(),
                    ":ipv6": dc_option.ipv6.to_string(),
                    ":auth_key": dc_option.auth_key.map(|k| k.to_vec()),
                },
            )?;
            Ok(())
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, SqliteSessionError>> {
        Box::pin(async move {
            let db = self.lock_database();
            let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<PeerInfo> {
                let subtype = row.get::<_, Option<i64>>(2)?.map(|s| s as u8);
                Ok(match peer.kind() {
                    PeerKind::User => PeerInfo::User {
                        id: PeerId::user_unchecked(row.get::<_, i64>(0)?).bare_id_unchecked(),
                        auth: row.get::<_, Option<i64>>(1)?.map(PeerAuth::from_hash),
                        bot: subtype.map(|s| s & PeerSubtype::UserBot as u8 != 0),
                        is_self: subtype.map(|s| s & PeerSubtype::UserSelf as u8 != 0),
                    },
                    PeerKind::Chat => PeerInfo::Chat {
                        id: peer.bare_id_unchecked(),
                    },
                    PeerKind::Channel => PeerInfo::Channel {
                        id: peer.bare_id_unchecked(),
                        auth: row.get::<_, Option<i64>>(1)?.map(PeerAuth::from_hash),
                        kind: subtype.and_then(|s| {
                            if (s & PeerSubtype::Gigagroup as u8) == PeerSubtype::Gigagroup as u8 {
                                Some(ChannelKind::Gigagroup)
                            } else if s & PeerSubtype::Broadcast as u8 != 0 {
                                Some(ChannelKind::Broadcast)
                            } else if s & PeerSubtype::Megagroup as u8 != 0 {
                                Some(ChannelKind::Megagroup)
                            } else {
                                None
                            }
                        }),
                    },
                })
            };

            Ok(if let Some(peer_id) = peer.bot_api_dialog_id() {
                db.fetch_one(
                    "SELECT * FROM peer_info WHERE peer_id = :peer_id LIMIT 1",
                    named_params! {":peer_id": peer_id},
                    map_row,
                )?
            } else {
                db.fetch_one(
                    "SELECT * FROM peer_info WHERE subtype & :type LIMIT 1",
                    named_params! {":type": PeerSubtype::UserSelf as i64},
                    map_row,
                )?
            })
        })
    }

    fn cache_peer(&self, peer: &PeerInfo) -> BoxFuture<'_, Result<(), SqliteSessionError>> {
        let peer = peer.clone();
        Box::pin(async move {
            let peer = if let Some(mut existing_peer) = self.peer(peer.id()).await? {
                existing_peer.extend_info(&peer);
                existing_peer
            } else {
                peer
            };

            let db = self.lock_database();
            let mut stmt =
                db.0.prepare("INSERT OR REPLACE INTO peer_info VALUES (:peer_id, :hash, :subtype)")?;
            let subtype = match peer {
                PeerInfo::User { bot, is_self, .. } => {
                    match (bot.unwrap_or_default(), is_self.unwrap_or_default()) {
                        (true, true) => Some(PeerSubtype::UserSelfBot),
                        (true, false) => Some(PeerSubtype::UserBot),
                        (false, true) => Some(PeerSubtype::UserSelf),
                        (false, false) => None,
                    }
                }
                PeerInfo::Chat { .. } => None,
                PeerInfo::Channel { kind, .. } => kind.map(|kind| match kind {
                    ChannelKind::Megagroup => PeerSubtype::Megagroup,
                    ChannelKind::Broadcast => PeerSubtype::Broadcast,
                    ChannelKind::Gigagroup => PeerSubtype::Gigagroup,
                }),
            };
            // Upstream conditionally omitted the named bindings for absent
            // fields (leaving the columns at their implicit NULL default);
            // binding NULL explicitly is semantically identical and lets a
            // single fixed statement be used.
            stmt.execute(named_params! {
                ":peer_id": peer.id().bot_api_dialog_id_unchecked(),
                ":hash": peer.auth().map(|auth| auth.hash()),
                ":subtype": subtype.map(|s| s as i64),
            })?;
            Ok(())
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, SqliteSessionError>> {
        Box::pin(async move {
            let db = self.lock_database();
            let mut state = db
                .fetch_one(
                    "SELECT * FROM update_state LIMIT 1",
                    named_params![],
                    |row| {
                        Ok(UpdatesState {
                            pts: row.get(0)?,
                            qts: row.get(1)?,
                            date: row.get(2)?,
                            seq: row.get(3)?,
                            channels: Vec::new(),
                        })
                    },
                )?
                .unwrap_or_default();
            state.channels = db
                .fetch_all("SELECT * FROM channel_state", named_params![], |row| {
                    Ok(ChannelState {
                        id: row.get(0)?,
                        pts: row.get(1)?,
                    })
                })?;
            Ok(state)
        })
    }

    fn set_update_state(
        &self,
        update: UpdateState,
    ) -> BoxFuture<'_, Result<(), SqliteSessionError>> {
        Box::pin(async move {
            let db = self.lock_database();
            let transaction = db.begin_transaction()?;

            match update {
                UpdateState::All(updates_state) => {
                    transaction
                        .execute("DELETE FROM update_state", params![])
                        ?;
                    transaction
                        .execute(
                            "INSERT INTO update_state VALUES (:pts, :qts, :date, :seq)",
                            named_params! {
                                ":pts": updates_state.pts,
                                ":qts": updates_state.qts,
                                ":date": updates_state.date,
                                ":seq": updates_state.seq,
                            },
                        )
                        ?;

                    transaction
                        .execute("DELETE FROM channel_state", params![])
                        ?;
                    for channel in updates_state.channels {
                        transaction
                            .execute(
                                "INSERT INTO channel_state VALUES (:peer_id, :pts)",
                                named_params! {
                                    ":peer_id": channel.id,
                                    ":pts": channel.pts,
                                },
                            )
                            ?;
                    }
                }
                UpdateState::Primary { pts, date, seq } => {
                    let previous = db
                        .fetch_one(
                            "SELECT * FROM update_state LIMIT 1",
                            named_params![],
                            |_| Ok(()),
                        )
                        ?;

                    if previous.is_some() {
                        transaction
                            .execute(
                                "UPDATE update_state SET pts = :pts, date = :date, seq = :seq",
                                named_params! {
                                    ":pts": pts,
                                    ":date": date,
                                    ":seq": seq,
                                },
                            )
                            ?;
                    } else {
                        transaction
                            .execute(
                                "INSERT INTO update_state VALUES (:pts, 0, :date, :seq)",
                                named_params! {
                                    ":pts": pts,
                                    ":date": date,
                                    ":seq": seq,
                                },
                            )
                            ?;
                    }
                }
                UpdateState::Secondary { qts } => {
                    let previous = db
                        .fetch_one(
                            "SELECT * FROM update_state LIMIT 1",
                            named_params![],
                            |_| Ok(()),
                        )
                        ?;

                    if previous.is_some() {
                        transaction
                            .execute(
                                "UPDATE update_state SET qts = :qts",
                                named_params! {":qts": qts},
                            )
                            ?;
                    } else {
                        transaction
                            .execute(
                                "INSERT INTO update_state VALUES (0, :qts, 0, 0)",
                                named_params! {":qts": qts},
                            )
                            ?;
                    }
                }
                UpdateState::Channel { id, pts } => {
                    transaction
                        .execute(
                            "INSERT OR REPLACE INTO channel_state VALUES (:peer_id, :pts)",
                            named_params! {
                                ":peer_id": id,
                                ":pts": pts,
                            },
                        )
                        ?;
                }
            }

            transaction.commit()?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

    use {DcOption, KNOWN_DC_OPTIONS, PeerInfo, Session, UpdateState};

    use super::*;

    #[test]
    fn exercise_sqlite_session() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(do_exercise_sqlite_session());
    }

    async fn do_exercise_sqlite_session() {
        let session = SqliteSession::open(":memory:").await.unwrap();

        assert_eq!(session.home_dc_id().unwrap(), DEFAULT_DC);
        session.set_home_dc_id(DEFAULT_DC + 1).await.unwrap();
        assert_eq!(session.home_dc_id().unwrap(), DEFAULT_DC + 1);

        assert_eq!(
            session.dc_option(KNOWN_DC_OPTIONS[0].id).unwrap(),
            Some(KNOWN_DC_OPTIONS[0].clone())
        );
        let new_dc_option = DcOption {
            id: KNOWN_DC_OPTIONS
                .iter()
                .map(|dc_option| dc_option.id)
                .max()
                .unwrap()
                + 1,
            ipv4: SocketAddrV4::new(Ipv4Addr::from_bits(0), 1),
            ipv6: SocketAddrV6::new(Ipv6Addr::from_bits(0), 1, 0, 0),
            auth_key: Some([1; 256]),
        };
        assert_eq!(session.dc_option(new_dc_option.id).unwrap(), None);
        session.set_dc_option(&new_dc_option).await.unwrap();
        assert_eq!(
            session.dc_option(new_dc_option.id).unwrap(),
            Some(new_dc_option)
        );

        assert_eq!(session.peer(PeerId::self_user()).await.unwrap(), None);
        assert_eq!(session.peer(PeerId::user_unchecked(1)).await.unwrap(), None);
        let peer = PeerInfo::User {
            id: 1,
            auth: None,
            bot: Some(true),
            is_self: Some(true),
        };
        session.cache_peer(&peer).await.unwrap();
        assert_eq!(
            session.peer(PeerId::self_user()).await.unwrap(),
            Some(peer.clone())
        );
        assert_eq!(
            session.peer(PeerId::user_unchecked(1)).await.unwrap(),
            Some(peer)
        );

        assert_eq!(
            session.peer(PeerId::channel_unchecked(1)).await.unwrap(),
            None
        );
        let peer = PeerInfo::Channel {
            id: 1,
            auth: Some(PeerAuth::from_hash(-1)),
            kind: Some(ChannelKind::Broadcast),
        };
        session.cache_peer(&peer).await.unwrap();
        assert_eq!(
            session.peer(PeerId::channel_unchecked(1)).await.unwrap(),
            Some(peer)
        );

        assert_eq!(
            session.updates_state().await.unwrap(),
            UpdatesState::default()
        );
        session
            .set_update_state(UpdateState::All(UpdatesState {
                pts: 1,
                qts: 2,
                date: 3,
                seq: 4,
                channels: vec![
                    ChannelState { id: 5, pts: 6 },
                    ChannelState { id: 7, pts: 8 },
                ],
            }))
            .await
            .unwrap();
        session
            .set_update_state(UpdateState::Primary {
                pts: 2,
                date: 4,
                seq: 5,
            })
            .await
            .unwrap();
        session
            .set_update_state(UpdateState::Secondary { qts: 3 })
            .await
            .unwrap();
        session
            .set_update_state(UpdateState::Channel { id: 7, pts: 9 })
            .await
            .unwrap();
        assert_eq!(
            session.updates_state().await.unwrap(),
            UpdatesState {
                pts: 2,
                qts: 3,
                date: 4,
                seq: 5,
                channels: vec![
                    ChannelState { id: 5, pts: 6 },
                    ChannelState { id: 7, pts: 9 },
                ],
            }
        );
    }
}
