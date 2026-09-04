//! Thin shell for `cydrive-sync-server` (sync-lite Batch A): env
//! config -> open db -> axum. Deliberately minimal:
//!
//! - No TLS — the reverse proxy's job (see deploy/cydrive-sync.service).
//! - No signal choreography — every request is one SQLite
//!   transaction, so systemd's default SIGTERM terminate cannot tear
//!   state mid-write.
//! - Bind failures exit non-zero with a human-readable message.

use std::sync::Arc;

use anyhow::{Context, Result};
use cydrive_sync::config::SyncServerConfig;
use cydrive_sync::router::router;
use cydrive_sync::store::SyncStore;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = SyncServerConfig::from_env().context(
        "invalid cydrive-sync-server configuration (check SYNC_LISTEN / SYNC_DB / SYNC_SECRET)",
    )?;
    let SyncServerConfig {
        listen,
        db_path,
        secret,
    } = config;

    let store = Arc::new(
        SyncStore::open(&db_path)
            .with_context(|| format!("cannot open sync database at {}", db_path.display()))?,
    );
    let app = router(store, secret);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| {
            format!(
                "cannot listen on {listen} (is another cydrive-sync-server already bound there?)"
            )
        })?;
    tracing::info!("cydrive-sync-server listening on http://{listen}");
    axum::serve(listener, app)
        .await
        .context("sync server accept loop failed")?;
    Ok(())
}
