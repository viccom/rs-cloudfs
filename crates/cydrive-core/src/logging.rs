//! Structured logging setup (tracing + tracing-subscriber).
//!
//! RED-phase stub: every function body is `todo!()`. Signatures, derives and
//! constants below are frozen by the task spec; real logic lands in the GREEN
//! phase. Selected per `docs/rust-rewrite-design.md` (logging/observability
//! row): tracing + tracing-subscriber (JSON/pretty switch) + tracing-appender
//! rolling files, `RUST_LOG` for ad-hoc control.

use std::path::PathBuf;

/// Output format of the fmt layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable console formatting.
    Pretty,
    /// Machine-parseable JSON, one object per event.
    Json,
}

/// Logging configuration.
///
/// Defaults: `Pretty` format, [`tracing::Level::INFO`], stdout (`file: None`).
pub struct LogConfig {
    /// Output format (pretty vs JSON).
    pub format: LogFormat,
    /// Global level directive used when `RUST_LOG` is unset or unparseable.
    pub level: tracing::Level,
    /// Directory for the daily rolling appender; `None` writes to stdout.
    pub file: Option<PathBuf>,
}

impl Default for LogConfig {
    fn default() -> Self {
        todo!()
    }
}

/// Failure modes of [`init`].
#[derive(Debug, thiserror::Error)]
pub enum LogInitError {
    /// A global default subscriber is already installed in this process.
    #[error("global subscriber already installed")]
    AlreadyInstalled,
    /// Creating or writing the rolling file appender failed.
    #[error("log file appender failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Rolling appender file-name prefix; generated files look like
/// `cydrive.log.YYYY-MM-DD`.
pub const LOG_FILE_PREFIX: &str = "cydrive.log";

/// Build a complete subscriber (filter + formatter) writing events to `writer`.
// `todo!()` alone does not type-check here: the never type falls back to `()`
// for return-position `impl Trait`, and `()` does not implement `Subscriber`.
// The unreachable `Registry` tail exists purely to satisfy the opaque return
// type's bounds; runtime behavior is still "not yet implemented".
#[allow(unreachable_code)]
pub fn build_subscriber<W>(
    cfg: &LogConfig,
    writer: W,
) -> impl tracing::Subscriber + Send + Sync + 'static
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let _ = (cfg, &writer);
    todo!();
    tracing_subscriber::Registry::default()
}

/// Install the subscriber built from `cfg` as the process-global default.
///
/// `cfg.file == None` writes to stdout; `Some(dir)` uses
/// `tracing_appender::rolling::daily(dir, LOG_FILE_PREFIX)`. Filter rule: a
/// parseable `RUST_LOG` environment variable wins, otherwise `cfg.level` is
/// the global directive. Returns [`LogInitError::AlreadyInstalled`] when a
/// global default subscriber already exists.
pub fn init(cfg: &LogConfig) -> Result<(), LogInitError> {
    let _ = cfg;
    todo!()
}
