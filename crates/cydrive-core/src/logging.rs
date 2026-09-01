//! Structured logging setup (tracing + tracing-subscriber).
//!
//! Selected per `docs/rust-rewrite-design.md` (logging/observability row):
//! tracing + tracing-subscriber (JSON/pretty switch) + tracing-appender
//! rolling files, `RUST_LOG` for ad-hoc control.

use std::path::PathBuf;

use tracing_appender::rolling::{InitError, RollingFileAppender, Rotation};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::Registry;

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
        Self {
            format: LogFormat::Pretty,
            level: tracing::Level::INFO,
            file: None,
        }
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
pub fn build_subscriber<W>(
    cfg: &LogConfig,
    writer: W,
) -> impl tracing::Subscriber + Send + Sync + 'static
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    // `pretty()` and `json()` return distinct `fmt::Layer` types, so the
    // chosen layer is type-erased behind a boxed layer to give both arms of
    // the match one shared type.
    let fmt_layer: Box<dyn Layer<Registry> + Send + Sync + 'static> = match cfg.format {
        LogFormat::Pretty => tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .pretty()
            .boxed(),
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .json()
            .boxed(),
    };
    Registry::default()
        .with(fmt_layer)
        .with(filter_from_env(cfg))
}

/// Resolve the filter layer: a strictly parseable `RUST_LOG` wins, otherwise
/// `cfg.level` becomes the global directive.
///
/// `RUST_LOG` is read with [`EnvFilter::try_from_default_env`], the strict
/// parser; any error (variable unset or malformed) falls back to a global
/// directive built from `cfg.level`. Raw `RUST_LOG` content must never be fed
/// to [`EnvFilter::new`]: that constructor is lossy and silently drops
/// invalid directives.
fn filter_from_env(cfg: &LogConfig) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(cfg.level.to_string()))
}

/// Map a rolling-appender [`InitError`] to [`LogInitError::Io`], recovering
/// the underlying [`std::io::Error`] via `Error::source` (the fields of
/// `InitError` are private).
fn appender_io_error(err: InitError) -> LogInitError {
    use std::error::Error as _;
    match err
        .source()
        .and_then(|source| source.downcast_ref::<std::io::Error>())
    {
        // `io::Error` is not `Clone`; rebuild it from kind + display text.
        Some(io) => LogInitError::Io(std::io::Error::new(io.kind(), io.to_string())),
        None => LogInitError::Io(std::io::Error::other(err)),
    }
}

/// Install the subscriber built from `cfg` as the process-global default.
///
/// `cfg.file == None` writes to stdout; `Some(dir)` uses
/// `tracing_appender::rolling::daily(dir, LOG_FILE_PREFIX)`. Filter rule: a
/// parseable `RUST_LOG` environment variable wins, otherwise `cfg.level` is
/// the global directive. Returns [`LogInitError::AlreadyInstalled`] when a
/// global default subscriber already exists.
pub fn init(cfg: &LogConfig) -> Result<(), LogInitError> {
    match &cfg.file {
        Some(dir) => {
            // The `rolling::daily` convenience constructor panics on IO
            // failure; the equivalent builder reports the error instead, so
            // it can be routed into `LogInitError::Io`.
            let appender = RollingFileAppender::builder()
                .rotation(Rotation::DAILY)
                .filename_prefix(LOG_FILE_PREFIX)
                .build(dir)
                .map_err(appender_io_error)?;
            tracing::subscriber::set_global_default(build_subscriber(cfg, appender))
        }
        None => tracing::subscriber::set_global_default(build_subscriber(cfg, std::io::stdout)),
    }
    .map_err(|_| LogInitError::AlreadyInstalled)?;
    Ok(())
}
