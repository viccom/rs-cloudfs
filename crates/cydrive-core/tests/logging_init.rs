//! RED-phase test for `cydrive_core::logging::init` (process-global setup).
//!
//! Each integration-test file runs in its own process, so the global default
//! subscriber may be installed here and only here. This file contains exactly
//! one test function because `init` succeeds at most once per process.

use cydrive_core::logging::{init, LogConfig, LogInitError, LOG_FILE_PREFIX};

#[test]
fn init_writes_rolling_file_then_rejects_reinit() {
    // The filter reads RUST_LOG; make sure no ambient value leaks in.
    std::env::remove_var("RUST_LOG");

    let dir = tempfile::tempdir().expect("create temp log dir");

    // First init: daily rolling appender in the temp dir must succeed.
    let cfg = LogConfig {
        file: Some(dir.path().to_path_buf()),
        ..LogConfig::default()
    };
    init(&cfg).expect("first init with file appender should succeed");

    // Events emitted through the global default must reach the rolling file.
    // The appender is synchronous, so the content is on disk right away.
    tracing::info!("init probe message");

    let log_path = std::fs::read_dir(dir.path())
        .expect("read log dir")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(LOG_FILE_PREFIX))
        })
        .unwrap_or_else(|| panic!("no {LOG_FILE_PREFIX}* file in {}", dir.path().display()));
    let content = std::fs::read_to_string(&log_path).expect("read rolling log file");
    assert!(
        content.contains("init probe message"),
        "log file {} must contain the probe event, got: {content:?}",
        log_path.display()
    );

    // Second init must be rejected: the global default is already installed.
    let second = init(&LogConfig::default());
    assert!(
        matches!(second, Err(LogInitError::AlreadyInstalled)),
        "second init must return AlreadyInstalled, got: {second:?}"
    );
}
