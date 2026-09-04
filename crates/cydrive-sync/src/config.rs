//! Server configuration from environment variables (sync-lite plan):
//!
//! - `SYNC_LISTEN` — bind address (default `127.0.0.1:8290`)
//! - `SYNC_DB` — SQLite file path (default `cydrive_sync.db` in the
//!   working directory)
//! - `SYNC_SECRET` — optional shared secret; unset/empty = open server
//!
//! Empty strings count as unset (the repo-wide convention). TLS is
//! deliberately not configured here: the sync server is expected to
//! sit behind a reverse proxy (caddy/nginx) whenever it is exposed
//! beyond a trusted network.

use std::net::SocketAddr;
use std::path::PathBuf;

/// Default listen address (sync-lite plan).
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8290";
/// Default database file name (working-directory relative, mirroring
/// the `cydrive_meta.db` convention of the main program).
pub const DEFAULT_DB_FILENAME: &str = "cydrive_sync.db";

/// Resolved server configuration.
#[derive(Debug, Clone)]
pub struct SyncServerConfig {
    /// Address the axum listener binds.
    pub listen: SocketAddr,
    /// SQLite file path (created on first use).
    pub db_path: PathBuf,
    /// Shared secret gating pushes; `None` accepts everyone.
    pub secret: Option<String>,
}

/// Configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// `SYNC_LISTEN` did not parse as a socket address.
    #[error("invalid listen address {addr:?}: {reason}")]
    InvalidListen {
        /// The offending value.
        addr: String,
        /// Why `SocketAddr::from_str` rejected it.
        reason: String,
    },
}

/// Pure core of [`SyncServerConfig::from_env`]: empty inputs fall
/// back to the documented defaults, an empty secret becomes `None`,
/// and an unparseable listen address is a hard error carrying the
/// offending value (the bin turns it into a human-readable exit).
pub fn parse_config(
    listen: &str,
    db: &str,
    secret: Option<String>,
) -> Result<SyncServerConfig, ConfigError> {
    let listen_str = if listen.is_empty() {
        DEFAULT_LISTEN
    } else {
        listen
    };
    let listen: SocketAddr =
        listen_str
            .parse::<SocketAddr>()
            .map_err(|err| ConfigError::InvalidListen {
                addr: listen_str.to_string(),
                reason: err.to_string(),
            })?;
    Ok(SyncServerConfig {
        listen,
        db_path: if db.is_empty() {
            PathBuf::from(DEFAULT_DB_FILENAME)
        } else {
            PathBuf::from(db)
        },
        secret: secret.filter(|secret| !secret.is_empty()),
    })
}

impl SyncServerConfig {
    /// Reads `SYNC_LISTEN` / `SYNC_DB` / `SYNC_SECRET` from the
    /// process environment (missing variables = their defaults).
    pub fn from_env() -> Result<Self, ConfigError> {
        parse_config(
            &std::env::var("SYNC_LISTEN").unwrap_or_default(),
            &std::env::var("SYNC_DB").unwrap_or_default(),
            std::env::var("SYNC_SECRET").ok(),
        )
    }
}
