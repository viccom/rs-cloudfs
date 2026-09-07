//! RED-phase tests for `cloudkit_core::logging` (subscriber construction only).
//!
//! These tests exercise [`build_subscriber`] through a thread-local default
//! dispatcher; the process-global default subscriber is never touched here.
//! Filter contract under test (per spec): a parseable `RUST_LOG` overrides
//! `cfg.level`; an unparseable `RUST_LOG` falls back to `cfg.level`. Because
//! `RUST_LOG` and the callsite interest cache are process-global, every test
//! that reaches `build_subscriber` serialises on [`ENV_LOCK`] via
//! [`env_guard`] (RUST_LOG removed on entry, restored on drop) and the helper
//! rebuilds the interest cache after installing its dispatcher.

use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard};

use cloudkit_core::logging::{build_subscriber, LogConfig, LogFormat};
use tracing_subscriber::fmt::MakeWriter;

// ------------------------------------------------------------- helpers ---

/// Serialises tests that touch the process-global `RUST_LOG` variable
/// (tests in one binary share one process; `set_var`/`remove_var` race).
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Holds [`ENV_LOCK`] with `RUST_LOG` removed; restores the original value on
/// drop (including on panic) so no env state leaks between tests.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    original: Option<String>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => std::env::set_var("RUST_LOG", value),
            None => std::env::remove_var("RUST_LOG"),
        }
    }
}

/// Locks [`ENV_LOCK`] and starts with `RUST_LOG` unset; the test may set it
/// afterwards, cleanup stays guaranteed by the guard.
fn env_guard() -> EnvGuard {
    let _lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let original = std::env::var("RUST_LOG").ok();
    std::env::remove_var("RUST_LOG");
    EnvGuard { _lock, original }
}

/// A [`Write`] handle appending into a shared capture buffer.
struct CaptureWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// [`MakeWriter`] handing out handles that all append to one shared buffer.
struct CaptureMakeWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl<'a> MakeWriter<'a> for CaptureMakeWriter {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter {
            buf: Arc::clone(&self.buf),
        }
    }
}

/// Builds a subscriber from `cfg`, runs `events` under it as this thread's
/// default dispatcher, and returns everything the subscriber wrote.
fn collect_events(cfg: &LogConfig, events: impl FnOnce()) -> String {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = build_subscriber(
        cfg,
        CaptureMakeWriter {
            buf: Arc::clone(&buf),
        },
    );
    let dispatch = tracing::Dispatch::new(subscriber);
    let _guard = tracing::dispatcher::set_default(&dispatch);
    // Interest for our callsites may have been cached as "never" by an
    // earlier, stricter subscriber in this process; re-evaluate so the
    // freshly built subscriber's filter is actually consulted.
    tracing::callsite::rebuild_interest_cache();
    events();
    drop(_guard);
    let bytes = buf
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Default config with `level` replaced.
fn with_level(level: tracing::Level) -> LogConfig {
    LogConfig {
        level,
        ..LogConfig::default()
    }
}

// ------------------------------------------------------------- defaults ---

#[test]
fn default_config_is_pretty_info_stdout() {
    let cfg = LogConfig::default();
    assert_eq!(cfg.format, LogFormat::Pretty);
    assert_eq!(cfg.level, tracing::Level::INFO);
    assert!(cfg.file.is_none());
}

// --------------------------------------------------------- formatting ---

#[test]
fn pretty_format_writes_message_to_writer() {
    let _env = env_guard();
    let out = collect_events(&LogConfig::default(), || {
        tracing::info!("hello cydrive");
    });
    assert!(
        out.contains("hello cydrive"),
        "pretty output must contain the message, captured: {out:?}"
    );
}

#[test]
fn json_format_emits_parseable_json() {
    let _env = env_guard();
    let cfg = LogConfig {
        format: LogFormat::Json,
        ..LogConfig::default()
    };
    let out = collect_events(&cfg, || {
        tracing::info!("hello cydrive");
    });
    // Must parse as JSON; the concrete field layout is deliberately not
    // asserted (no implementation coupling).
    serde_json::from_str::<serde_json::Value>(out.trim())
        .unwrap_or_else(|err| panic!("JSON output not parseable ({err}): {out:?}"));
    assert!(
        out.contains("hello cydrive"),
        "raw JSON line must contain the message, captured: {out:?}"
    );
    assert!(
        out.contains("INFO"),
        "raw JSON line must contain the level, captured: {out:?}"
    );
}

// ---------------------------------------------------- level filtering ---

#[test]
fn level_filter_respects_config_level() {
    let _env = env_guard(); // RUST_LOG unset
    let out = collect_events(&with_level(tracing::Level::ERROR), || {
        tracing::error!("hello cydrive");
        tracing::warn!("hello cydrive");
        tracing::info!("hello cydrive");
    });
    assert_eq!(
        out.matches("hello cydrive").count(),
        1,
        "level=ERROR must capture only the error event, captured: {out:?}"
    );
}

#[test]
fn rust_log_env_overrides_config_level() {
    let _env = env_guard();
    std::env::set_var("RUST_LOG", "debug");
    let out = collect_events(&with_level(tracing::Level::ERROR), || {
        tracing::info!("hello cydrive");
    });
    assert!(
        out.contains("hello cydrive"),
        "RUST_LOG=debug must let info through despite cfg level=ERROR, captured: {out:?}"
    );
}

#[test]
fn invalid_rust_log_falls_back_to_config_level() {
    let _env = env_guard();
    std::env::set_var("RUST_LOG", "not!a=level");
    let out = collect_events(&with_level(tracing::Level::ERROR), || {
        tracing::error!("hello cydrive");
        tracing::info!("hello cydrive");
    });
    assert_eq!(
        out.matches("hello cydrive").count(),
        1,
        "invalid RUST_LOG must fall back to cfg level=ERROR (only error captured), got: {out:?}"
    );
}
