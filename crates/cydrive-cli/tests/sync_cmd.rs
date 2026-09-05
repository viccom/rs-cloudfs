//! Unit-level pins for the `cydrive sync` wiring (sync-lite Batch B4):
//! the pure pieces of the CLI layer — the sync_url gate, the secret env
//! parsing, the wire<->core type mapping, the error-body truncation and
//! the human summary. The real-HTTP behavior lives in `sync_e2e.rs`.

use std::sync::{Mutex, MutexGuard};

use cydrive_cli::sync_client::{
    endpoint_url, pull_core_result, push_wire_rows, truncate_for_log, SYNC_SECRET_ENV,
};
use cydrive_cli::{
    parse_sync_secret, render_sync_summary, resolve_sync_secret, run_sync_command,
    sync_secret_from_env,
};
use cydrive_core::config::CyDriveConfig;
use cydrive_core::sync::{SyncOutcome, SyncPulledRow, SyncRowUpdate};
use cydrive_sync::wire::{PullResponse, PulledRow, PushRow};

// ------------------------------------------------------------- helpers ---

/// A config with injected db/cache paths and the sync fields, mirroring
/// the other CLI test files' `temp_config` shape (web UI off, no mount).
fn sync_test_config(sync_url: Option<String>) -> CyDriveConfig {
    CyDriveConfig {
        bot_token: "123456:ABC-DEF".to_string(),
        chat_id: 42,
        db_path: "meta.db".to_string(),
        cache_path: "cache".to_string(),
        enable_web_ui: false,
        auto_mount_drive: false,
        sync_url,
        ..CyDriveConfig::default()
    }
}

/// Serialises every test that touches the process-wide `CYDRIVE_SYNC_SECRET`
/// variable (tests in one binary share one process; env vars race).
static SECRET_ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Holds [`SECRET_ENV_MUTEX`] and removes the secret env var on drop —
/// including on panic, so a failing assertion cannot poison later runs.
/// The guard field is held purely for its Drop (hence the underscore).
struct SecretEnvGuard<'a> {
    _guard: MutexGuard<'a, ()>,
}

impl Drop for SecretEnvGuard<'_> {
    fn drop(&mut self) {
        std::env::remove_var(SYNC_SECRET_ENV);
    }
}

/// Locks [`SECRET_ENV_MUTEX`] for a secret-env test.
fn lock_secret_env() -> SecretEnvGuard<'static> {
    let guard = SECRET_ENV_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    SecretEnvGuard { _guard: guard }
}

// --------------------------------------------------------------- tests ---

/// The endpoint joiner glues base + endpoint with exactly one slash: a
/// trailing slash on the configured sync_url (and a path prefix behind
/// a reverse proxy) must not produce `//v1/push`.
#[test]
fn endpoint_url_joins_and_trims_trailing_slash() {
    assert_eq!(
        endpoint_url("http://127.0.0.1:8290", "/v1/push"),
        "http://127.0.0.1:8290/v1/push"
    );
    assert_eq!(
        endpoint_url("http://127.0.0.1:8290/", "/v1/pull"),
        "http://127.0.0.1:8290/v1/pull"
    );
    assert_eq!(
        endpoint_url("https://example.com/sync/", "/v1/push"),
        "https://example.com/sync/v1/push"
    );
}

/// The push mapping is field-for-field: core updates become wire rows
/// with the same key, tombstone flag and payload.
#[test]
fn push_wire_rows_maps_core_updates_to_wire() {
    let updates = vec![
        SyncRowUpdate {
            rel_path: "/a".to_string(),
            deleted: false,
            payload: "{\"size\":1}".to_string(),
        },
        SyncRowUpdate {
            rel_path: "/b".to_string(),
            deleted: true,
            payload: String::new(),
        },
    ];
    assert_eq!(
        push_wire_rows(&updates),
        vec![
            PushRow {
                rel_path: "/a".to_string(),
                deleted: false,
                payload: "{\"size\":1}".to_string(),
            },
            PushRow {
                rel_path: "/b".to_string(),
                deleted: true,
                payload: String::new(),
            },
        ]
    );
}

/// The pull mapping is field-for-field: wire rows keep their key,
/// version, tombstone flag and verbatim payload, and the counter rides
/// along as `max_version`.
#[test]
fn pull_core_result_maps_wire_rows_to_core() {
    let response = PullResponse {
        rows: vec![
            PulledRow {
                rel_path: "/a".to_string(),
                version: 7,
                deleted: false,
                payload: "payload-a".to_string(),
            },
            PulledRow {
                rel_path: "/b".to_string(),
                version: 9,
                deleted: true,
                payload: String::new(),
            },
        ],
        max_version: 9,
    };
    let core = pull_core_result(response);
    assert_eq!(core.max_version, 9);
    assert_eq!(
        core.rows,
        vec![
            SyncPulledRow {
                rel_path: "/a".to_string(),
                version: 7,
                deleted: false,
                payload: "payload-a".to_string(),
            },
            SyncPulledRow {
                rel_path: "/b".to_string(),
                version: 9,
                deleted: true,
                payload: String::new(),
            },
        ]
    );
}

/// Without a sync_url the command is an actionable error naming the
/// config.toml key to set (exit-non-zero comes from the anyhow error
/// leaving `main`, the CLI-wide convention).
#[tokio::test]
async fn sync_command_without_sync_url_is_actionable_error() {
    let cfg = sync_test_config(None);
    let err = run_sync_command(&cfg, None)
        .await
        .expect_err("no sync_url configured: the command must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("sync_url"), "names the key: {msg}");
    assert!(msg.contains("config.toml"), "names the file: {msg}");
}

/// The namespace derivation needs both halves of the bot+chat identity;
/// either one missing is an actionable error, not a silently wrong key.
#[tokio::test]
async fn sync_command_without_credentials_is_actionable_error() {
    let cfg = CyDriveConfig {
        bot_token: String::new(),
        chat_id: 0,
        ..sync_test_config(Some("http://127.0.0.1:8290".to_string()))
    };
    let err = run_sync_command(&cfg, None)
        .await
        .expect_err("no token/chat: the command must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("bot_token"), "names the token: {msg}");
    assert!(msg.contains("chat_id"), "names the chat: {msg}");
}

/// Unset and empty secrets both mean "send none"; only a non-empty
/// value travels.
#[test]
fn parse_sync_secret_treats_unset_and_empty_as_none() {
    assert_eq!(parse_sync_secret(None), None);
    assert_eq!(parse_sync_secret(Some(String::new())), None);
    assert_eq!(
        parse_sync_secret(Some("s3cret".to_string())),
        Some("s3cret".to_string())
    );
}

/// `sync_secret_from_env` reads the pinned variable name (and applies
/// the same empty-means-none rule).
#[test]
fn sync_secret_from_env_reads_the_pinned_variable() {
    assert_eq!(SYNC_SECRET_ENV, "CYDRIVE_SYNC_SECRET");
    let _guard = lock_secret_env();

    std::env::set_var(SYNC_SECRET_ENV, "env-secret");
    assert_eq!(
        sync_secret_from_env(),
        Some("env-secret".to_string()),
        "the env var supplies the secret"
    );

    std::env::set_var(SYNC_SECRET_ENV, "");
    assert_eq!(
        sync_secret_from_env(),
        None,
        "an empty value reads as no secret"
    );

    std::env::remove_var(SYNC_SECRET_ENV);
    assert_eq!(sync_secret_from_env(), None, "unset reads as no secret");
}

/// The single resolution chain both sync entry points use (`cydrive sync`
/// and the `run` periodic task): env `CYDRIVE_SYNC_SECRET` > config.toml
/// `sync_secret` > `None`. A set-but-empty env value explicitly CLEARS the
/// config value (the `CYDRIVE_SYNC_URL` precedent), and whitespace-only
/// values read as unset on either leg; non-empty values pass verbatim.
#[test]
fn resolve_sync_secret_env_beats_config_beats_none() {
    let _guard = lock_secret_env();
    std::env::remove_var(SYNC_SECRET_ENV);

    let cfg_file = CyDriveConfig {
        sync_secret: Some("cfg-secret".to_string()),
        ..sync_test_config(Some("http://127.0.0.1:8290".to_string()))
    };

    // Third state: neither source set → None.
    assert_eq!(
        resolve_sync_secret(&sync_test_config(None)),
        None,
        "no env, no config key → send none"
    );

    // Second state: env unset → the config.toml value governs.
    assert_eq!(
        resolve_sync_secret(&cfg_file),
        Some("cfg-secret".to_string()),
        "with the env var unset, the config.toml sync_secret must win"
    );

    // First state: a set env var outranks the file value.
    std::env::set_var(SYNC_SECRET_ENV, "env-secret");
    assert_eq!(
        resolve_sync_secret(&cfg_file),
        Some("env-secret".to_string()),
        "CYDRIVE_SYNC_SECRET must outrank the config.toml value"
    );

    // A set-but-empty env value explicitly clears (CYDRIVE_SYNC_URL
    // precedent), and whitespace-only reads as unset on either leg.
    std::env::set_var(SYNC_SECRET_ENV, "");
    assert_eq!(
        resolve_sync_secret(&cfg_file),
        None,
        "empty CYDRIVE_SYNC_SECRET clears the config value"
    );
    std::env::set_var(SYNC_SECRET_ENV, "   ");
    assert_eq!(
        resolve_sync_secret(&cfg_file),
        None,
        "whitespace-only env value reads as unset"
    );

    std::env::remove_var(SYNC_SECRET_ENV);
    let cfg_blank = CyDriveConfig {
        sync_secret: Some("   ".to_string()),
        ..sync_test_config(None)
    };
    assert_eq!(
        resolve_sync_secret(&cfg_blank),
        None,
        "whitespace-only config value reads as unset"
    );

    // Non-empty values pass through verbatim — a secret is byte-exact,
    // never trimmed.
    let cfg_exact = CyDriveConfig {
        sync_secret: Some(" padded secret ".to_string()),
        ..sync_test_config(None)
    };
    assert_eq!(
        resolve_sync_secret(&cfg_exact),
        Some(" padded secret ".to_string()),
        "the secret value itself is never trimmed"
    );
}

/// The human summary prints every counter with the labels the task
/// phrasing uses (applied/pushed/tombstoned/skipped_ghost included);
/// `skipped_invalid` covers rows skipped for undecodable payloads or
/// invalid row keys (decisions.md 2026-09-05 poison-row fix).
#[test]
fn render_sync_summary_lists_all_counters() {
    let outcome = SyncOutcome {
        pulled: 3,
        applied: 2,
        skipped_ghost: 1,
        skipped_idempotent: 4,
        skipped_invalid: 8,
        tombstoned: 5,
        pushed: 6,
        pushed_tombstones: 7,
    };
    let summary = render_sync_summary(&outcome);
    assert!(summary.contains("pulled 3"), "{summary}");
    assert!(summary.contains("applied 2"), "{summary}");
    assert!(summary.contains("pushed 6"), "{summary}");
    assert!(summary.contains("tombstoned 5"), "{summary}");
    assert!(summary.contains("skipped_ghost 1"), "{summary}");
    assert!(summary.contains("skipped_idempotent 4"), "{summary}");
    assert!(summary.contains("skipped_invalid 8"), "{summary}");
    assert!(summary.contains("pushed_tombstones 7"), "{summary}");
}

/// Error-message bodies are capped: long server answers truncate to the
/// cap with an ellipsis marker; short ones pass through untouched.
#[test]
fn truncate_for_log_caps_long_bodies() {
    assert_eq!(truncate_for_log(b"short body", 512), "short body");
    let long = "x".repeat(1000);
    let truncated = truncate_for_log(long.as_bytes(), 512);
    assert_eq!(
        truncated.chars().count(),
        512 + 3,
        "512 kept characters plus the \"...\" marker"
    );
    assert!(truncated.ends_with("..."), "{truncated}");
}
