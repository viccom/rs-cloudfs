//! Thin shell for `cydrive-sync-server` (sync-lite Batch A): env
//! config -> open db -> axum. Deliberately minimal:
//!
//! - Argument surface is exactly nothing, `--version` or `--help` —
//!   anything else exits 2 before any setup (see `startup`), so a
//!   mistyped flag can never silently start a service.
//! - No TLS — the reverse proxy's job (see deploy/cydrive-sync.service).
//! - No signal choreography — every request is one SQLite
//!   transaction, so systemd's default SIGTERM terminate cannot tear
//!   state mid-write.
//! - Bind failures exit non-zero with a human-readable message.

use std::sync::Arc;

use anyhow::{Context, Result};
use cydrive_sync::config::SyncServerConfig;
use cydrive_sync::router::router;
use cydrive_sync::startup::{decide_startup, StartupDecision};
use cydrive_sync::store::SyncStore;

/// The `--help` text; also appended after `Invalid` errors, so the
/// refusal and the usage travel together on stderr.
const HELP_TEXT: &str = "\
cydrive-sync-server - CyDrive lite metadata sync server: one SQLite file,
two JSON endpoints (POST /v1/push, POST /v1/pull) behind your reverse proxy.

Usage:
  cydrive-sync-server            start the server (takes no arguments)

Configuration (environment variables):
  SYNC_LISTEN    listen address (default: 127.0.0.1:8290)
  SYNC_DB        SQLite database path (default: ./cydrive_sync.db)
  SYNC_SECRET    optional shared secret; when set, pushes/pulls must match it

Options:
  -V, --version  print the version and exit
  -h, --help     print this help and exit
";

#[tokio::main]
async fn main() -> Result<()> {
    // args_os + lossy decode: std::env::args() panics outright on a
    // non-Unicode argument (a stray filename should be a usage error,
    // not a crash); decide_startup sees the replacement characters and
    // refuses the argument like any other unknown one.
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    match decide_startup(&args) {
        StartupDecision::Run => run().await,
        StartupDecision::PrintVersion => {
            println!("cydrive-sync-server {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        StartupDecision::PrintHelp => {
            print!("{HELP_TEXT}");
            Ok(())
        }
        StartupDecision::Invalid(message) => {
            eprintln!("error: {message}");
            eprintln!();
            eprint!("{HELP_TEXT}");
            std::process::exit(2);
        }
    }
}

async fn run() -> Result<()> {
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
    // Log the *actual* bound address: with SYNC_LISTEN=…:0 the kernel
    // picked an ephemeral port, and logging the configured :0 points
    // the journal at a port nothing listens on. local_addr() failing
    // on a bound socket is practically unreachable; fall back to the
    // configured value with a note rather than guessing.
    match listener.local_addr() {
        Ok(bound) => tracing::info!("cydrive-sync-server listening on http://{bound}"),
        Err(error) => tracing::info!(
            "cydrive-sync-server listening on http://{listen} \
             (actual bound address unavailable: {error})"
        ),
    }
    axum::serve(listener, app)
        .await
        .context("sync server accept loop failed")?;
    Ok(())
}
