//! Offline contract tests for the `cydrive-platform` crate (unit D).
//!
//! Everything here exercises pure cross-platform logic — no drive is
//! mapped and no registry key is touched by a default `cargo test` run.
//!
//! # Real-machine checklist (deliberately `#[ignore]`d)
//!
//! The two `ignored_` tests below need a real Windows box with an
//! administrator terminal; they are **not** part of CI/gates and never run
//! under `cargo test` without an explicit `--ignored`:
//!
//! | test | needs | what it does |
//! |------|-------|--------------|
//! | `ignored_mount_unmount_roundtrip` | admin shell **and** a reachable WebDAV server at `http://127.0.0.1:8080` (start `cydrive run` first, or point `CYDRIVE_TEST_MOUNT_URL` at another server) | `mount_drive` on `"Y:"` (fallback-picked if occupied), assert a `X:`-shaped letter came back, then `unmount_drive` it |
//! | `ignored_optimize_webdav_registry` | admin shell | writes `FileSizeLimitInBytes` / `BasicAuthLevel` under the WebClient `Parameters` key and restarts the service |
//!
//! Run them by hand with:
//!
//! ```text
//! cargo test -p cydrive-platform --test platform -- --ignored
//! ```

use cydrive_platform::{
    mount_command, normalize_drive_letter, pick_drive_letter, unmount_command,
    used_letters_from_bitmask, BASIC_AUTH_LEVEL, FALLBACK_DRIVE_LETTERS, FILE_SIZE_LIMIT_BYTES,
    WEBCLIENT_REG_PATH,
};

/// 1. `normalize_drive_letter` — the three accepted spellings canonicalise
///    to `"Y:"`; multi-letter, empty and non-alphabetic inputs are `None`.
#[test]
fn normalize_letter_variants() {
    for valid in ["y", "Y:", " Y: "] {
        assert_eq!(
            normalize_drive_letter(valid).as_deref(),
            Some("Y:"),
            "input {valid:?} must canonicalise"
        );
    }
    for invalid in ["AB", "", "1:"] {
        assert_eq!(normalize_drive_letter(invalid), None, "input {invalid:?}");
    }
}

/// 2. A free preferred letter wins outright — no fallback consultation.
#[test]
fn pick_prefers_free_preferred() {
    let used = ["C:".to_string(), "D:".to_string(), "Z:".to_string()];
    assert_eq!(pick_drive_letter("Y:", &used), "Y:");
}

/// 3. An occupied preferred letter walks the fallback chain in order:
///    `Y:`+`Z:` taken → `X:`; only `Y:` taken → `Z:` (chain head).
#[test]
fn pick_falls_back_in_order() {
    let yz = ["Y:".to_string(), "Z:".to_string()];
    assert_eq!(pick_drive_letter("Y:", &yz), "X:");
    let y = ["Y:".to_string()];
    assert_eq!(pick_drive_letter("Y:", &y), "Z:");
}

/// 4. Every fallback letter occupied → the baseline tail behaviour:
///    return the preferred letter itself (the subsequent mount will
///    likely fail, but the choice mirrors the Python baseline exactly).
#[test]
fn pick_all_taken_returns_preferred() {
    let all: Vec<String> = FALLBACK_DRIVE_LETTERS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(pick_drive_letter("Y:", &all), "Y:");
}

/// 5. `GetLogicalDrives` bitmask decode: bit *i* ↔ drive `'A' + i`.
#[test]
fn bitmask_decode() {
    assert_eq!(
        used_letters_from_bitmask(0b101),
        ["A:".to_string(), "C:".to_string()]
    );
    assert!(used_letters_from_bitmask(0).is_empty());
    let all = used_letters_from_bitmask(u32::MAX);
    assert_eq!(all.len(), 26);
    assert_eq!(all.first().unwrap(), "A:");
    assert_eq!(all.last().unwrap(), "Z:");
}

/// 6. Exact argv for the `net use` mapping/unmapping commands.
#[test]
fn mount_and_unmount_commands_exact() {
    assert_eq!(
        mount_command("Y:", "http://127.0.0.1:8080")
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "net",
            "use",
            "Y:",
            "http://127.0.0.1:8080",
            "/persistent:no"
        ]
    );
    assert_eq!(
        unmount_command("Y:")
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["net", "use", "Y:", "/delete", "/y"]
    );
}

/// 7. Frozen contract constants: fallback chain order, the WebClient
///    registry path, the 4 GiB−1 file-size limit and the basic-auth level.
#[test]
fn contract_constants() {
    assert_eq!(
        FALLBACK_DRIVE_LETTERS,
        ["Z:", "Y:", "X:", "W:", "V:", "U:", "T:", "S:"]
    );
    assert_eq!(
        WEBCLIENT_REG_PATH,
        r"SYSTEM\CurrentControlSet\Services\WebClient\Parameters"
    );
    assert_eq!(FILE_SIZE_LIMIT_BYTES, 4294967295);
    assert_eq!(BASIC_AUTH_LEVEL, 2);
}

/// 8. Non-Windows stub shape: every mutating entry point answers
///    `Unsupported("windows-only mount/registry")` and drive probing is
///    the empty list. This test only compiles off Windows; on Windows the
///    cfg discipline keeps the real implementation compiled in its place
///    (not mechanically verifiable on this Windows-only host — the cfg
///    attributes are the compile gate).
#[cfg(not(windows))]
#[test]
fn unsupported_stub_shape() {
    use cydrive_platform::windows;
    use cydrive_platform::PlatformError;

    fn expect_unsupported(result: Result<(), PlatformError>) {
        match result {
            Err(PlatformError::Unsupported(msg)) => {
                assert_eq!(msg, "windows-only mount/registry")
            }
            Err(other) => panic!("expected Unsupported, got {other:?}"),
            Ok(()) => panic!("stub must not succeed"),
        }
    }

    assert!(windows::used_drive_letters().is_empty());
    expect_unsupported(windows::ensure_webclient_service());
    expect_unsupported(windows::optimize_webdav_registry());
    let mount: Result<String, PlatformError> = windows::mount_drive("Y:", "http://127.0.0.1:8080");
    match mount {
        Err(PlatformError::Unsupported(msg)) => {
            assert_eq!(msg, "windows-only mount/registry")
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
    expect_unsupported(windows::unmount_drive("Y:"));
}

// --------------------------------------------------- real-machine (manual) ---

/// Real-machine mount round-trip (see the checklist in the module docs).
#[cfg(windows)]
#[test]
#[ignore = "real-machine: maps a network drive; needs an admin shell and a reachable WebDAV server (CYDRIVE_TEST_MOUNT_URL, default http://127.0.0.1:8080)"]
fn ignored_mount_unmount_roundtrip() {
    let url = std::env::var("CYDRIVE_TEST_MOUNT_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let letter = cydrive_platform::windows::mount_drive("Y:", &url).expect("mount succeeds");
    assert!(
        letter.len() == 2 && letter.ends_with(':'),
        "mount returns a canonical letter, got {letter:?}"
    );
    cydrive_platform::windows::unmount_drive(&letter).expect("unmount succeeds");
}

/// Real-machine registry tuning (see the checklist in the module docs).
#[cfg(windows)]
#[test]
#[ignore = "real-machine: writes HKLM WebClient registry values and restarts the service; needs an admin shell"]
fn ignored_optimize_webdav_registry() {
    cydrive_platform::windows::optimize_webdav_registry().expect("registry tuning succeeds");
}
