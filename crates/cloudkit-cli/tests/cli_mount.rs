//! Offline tests for the CLI mount/unmount flag resolution (unit D).
//!
//! The `mount` / `unmount` / `fix-reg` subcommands resolve their `--url`
//! and `--letter` flags against the config before touching
//! `cloudkit-platform`; only that pure resolution layer is testable offline
//! (the actual `net use` / registry side belongs to the real-machine
//! checklist in `crates/cloudkit-platform/tests/platform.rs`).

use cloudkit_cli::{default_mount_url, resolve_mount_params, resolve_unmount_letter};
use cloudkit_core::config::CyDriveConfig;

/// The default URL is glued straight from the config's WebDAV host/port:
/// `http://127.0.0.1:8080` on defaults, reflecting overrides verbatim.
#[test]
fn default_mount_url_from_config() {
    assert_eq!(
        default_mount_url(&CyDriveConfig::default()),
        "http://127.0.0.1:8080"
    );

    let cfg = CyDriveConfig {
        webdav_host: "192.168.1.9".to_string(),
        webdav_port: 9000,
        ..CyDriveConfig::default()
    };
    assert_eq!(default_mount_url(&cfg), "http://192.168.1.9:9000");
}

/// Explicit flags win; omitted flags fall back to the config's
/// `drive_letter` and glued WebDAV URL.
#[test]
fn resolve_mount_params_flags_over_config() {
    let cfg = CyDriveConfig::default(); // drive_letter "Y:", port 8080

    let (letter, url) = resolve_mount_params(
        &cfg,
        Some("http://10.0.0.5:8081".to_string()),
        Some("Q".to_string()),
    );
    assert_eq!(letter, "Q");
    assert_eq!(url, "http://10.0.0.5:8081");

    let (letter, url) = resolve_mount_params(&cfg, None, None);
    assert_eq!(letter, "Y:");
    assert_eq!(url, "http://127.0.0.1:8080");
}

/// `unmount` resolves its letter the same way: flag first, config second.
#[test]
fn resolve_unmount_letter_defaults_to_config() {
    let cfg = CyDriveConfig::default();
    assert_eq!(resolve_unmount_letter(&cfg, Some("z".to_string())), "z");
    assert_eq!(resolve_unmount_letter(&cfg, None), "Y:");
}
