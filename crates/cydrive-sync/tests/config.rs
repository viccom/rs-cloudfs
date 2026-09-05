//! Configuration parsing specs (sync-lite Batch A): env-shaped string
//! inputs -> resolved config, with the documented defaults and the
//! repo-wide "empty string = unset" convention.

use std::path::PathBuf;
use std::time::Duration;

use cydrive_sync::config::{parse_config, ConfigError, DEFAULT_DB_FILENAME, DEFAULT_LISTEN};

#[test]
fn defaults_when_inputs_are_the_documented_defaults() {
    let config = parse_config(DEFAULT_LISTEN, DEFAULT_DB_FILENAME, None, "").unwrap();
    assert_eq!(config.listen.to_string(), "127.0.0.1:8290");
    assert_eq!(config.db_path, PathBuf::from("cydrive_sync.db"));
    assert_eq!(config.secret, None);
}

#[test]
fn empty_inputs_fall_back_to_defaults() {
    let config = parse_config("", "", None, "").unwrap();
    assert_eq!(config.listen.to_string(), "127.0.0.1:8290");
    assert_eq!(config.db_path, PathBuf::from("cydrive_sync.db"));
    assert_eq!(config.secret, None);
}

#[test]
fn explicit_values_override_defaults() {
    let config = parse_config(
        "0.0.0.0:9000",
        "/var/lib/cydrive-sync/sync.db",
        Some("topsecret".to_string()),
        "",
    )
    .unwrap();
    assert_eq!(config.listen.to_string(), "0.0.0.0:9000");
    assert_eq!(
        config.db_path,
        PathBuf::from("/var/lib/cydrive-sync/sync.db")
    );
    assert_eq!(config.secret.as_deref(), Some("topsecret"));
}

#[test]
fn empty_secret_string_means_unset() {
    let config =
        parse_config(DEFAULT_LISTEN, DEFAULT_DB_FILENAME, Some(String::new()), "").unwrap();
    assert_eq!(config.secret, None);
}

#[test]
fn invalid_listen_address_is_rejected_with_the_offending_value() {
    let error = parse_config("not-an-addr", DEFAULT_DB_FILENAME, None, "").unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("not-an-addr"),
        "error should name the bad value: {message}"
    );
    // the error type is address-specific (drives the bin's exit path)
    assert!(matches!(error, ConfigError::InvalidListen { .. }));
}

// ---- SSE doorbell batch: SYNC_HEARTBEAT_SECS ----

/// Unset/empty heartbeat falls back to the 20 s default (the SSE
/// keepalive cadence, sized to outlive nginx's default 60 s
/// proxy_read_timeout several times over).
#[test]
fn default_heartbeat_is_20s() {
    let config = parse_config(DEFAULT_LISTEN, DEFAULT_DB_FILENAME, None, "").unwrap();
    assert_eq!(config.heartbeat, Duration::from_secs(20));
}

/// An explicit heartbeat wins — this is the knob the short-heartbeat
/// binary smoke and unusual proxy timeouts tune.
#[test]
fn heartbeat_secs_override() {
    let config = parse_config(DEFAULT_LISTEN, DEFAULT_DB_FILENAME, None, "1").unwrap();
    assert_eq!(config.heartbeat, Duration::from_secs(1));
}

/// Zero (a spinning keepalive) and garbage are hard errors naming the
/// offending value.
#[test]
fn heartbeat_zero_and_garbage_are_rejected() {
    for bad in ["0", "abc"] {
        let error = parse_config(DEFAULT_LISTEN, DEFAULT_DB_FILENAME, None, bad).unwrap_err();
        assert!(
            matches!(error, ConfigError::InvalidHeartbeat { .. }),
            "{bad}: {error}"
        );
        assert!(
            error.to_string().contains(bad),
            "error should name the bad value: {error}"
        );
    }
}
