//! Doctor's WinFsp check (Phase 3 / WF4, K40): the verdict must be a
//! **Warn** when WinFsp is absent — never a Fail — because the default
//! `mount_backend = "webdav"` needs nothing installed and a winfsp
//! request degrades visibly instead of refusing to start. The pure
//! verdict function is exercised over synthesized probes (offline); the
//! real probe's wiring is `winfsp_checks()`, which on Windows always
//! yields exactly one check.

use cloudkit_cli::doctor::{evaluate_winfsp_install, winfsp_checks, CheckStatus};

#[cfg(windows)]
use cloudkit_platform::{WinFspInstall, WINFSP_X64_DLL};

/// Not installed (or unreadable registry) → Warn with the install pointer
/// *and* the reassurance that the default backend is unaffected.
#[test]
fn a_missing_install_is_a_warning_with_the_way_out() {
    let check = evaluate_winfsp_install(None);
    assert_eq!(check.name, "winfsp");
    assert_eq!(
        check.status,
        CheckStatus::Warn,
        "a missing WinFsp must never fail the doctor (webdav needs nothing)"
    );
    assert!(
        check.detail.contains("winfsp.dev"),
        "the detail points at the installer, got: {}",
        check.detail
    );
    assert!(
        check.detail.contains("webdav"),
        "the detail says the default backend is unaffected, got: {}",
        check.detail
    );
}

/// An installed runtime → Ok, naming the directory and the build flag the
/// winfsp backend additionally needs — or, on a build that carries the
/// winfsp feature, that the mount backend is ready (the note is
/// feature-aware; both branches are pinned).
#[cfg(windows)]
#[test]
fn an_installed_runtime_is_ok_and_names_the_build_flag() {
    let install = WinFspInstall {
        install_dir: std::path::PathBuf::from(r"C:\Program Files (x86)\WinFsp"),
        dll: Some(
            std::path::PathBuf::from(r"C:\Program Files (x86)\WinFsp")
                .join("bin")
                .join(WINFSP_X64_DLL),
        ),
    };
    let check = evaluate_winfsp_install(Some(&install));
    assert_eq!(check.status, CheckStatus::Ok, "detail: {}", check.detail);
    assert!(
        check.detail.contains("WinFsp"),
        "the detail names the install, got: {}",
        check.detail
    );
    if cfg!(feature = "winfsp") {
        assert!(
            check.detail.contains("carries the winfsp mount backend"),
            "a winfsp build states the mount backend is ready, got: {}",
            check.detail
        );
    } else {
        assert!(
            check.detail.contains("--features winfsp"),
            "the detail names what a winfsp mount additionally needs, got: {}",
            check.detail
        );
    }
}

/// A half install (directory present, runtime DLL missing) → Warn: a
/// winfsp mount would fail at the preload, the operator should reinstall.
#[cfg(windows)]
#[test]
fn a_half_install_is_a_warning() {
    let install = WinFspInstall {
        install_dir: std::path::PathBuf::from(r"C:\Program Files (x86)\WinFsp"),
        dll: None,
    };
    let check = evaluate_winfsp_install(Some(&install));
    assert_eq!(check.status, CheckStatus::Warn, "detail: {}", check.detail);
    assert!(
        check.detail.contains(WINFSP_X64_DLL) && check.detail.contains("reinstall"),
        "the detail names the missing piece and the action, got: {}",
        check.detail
    );
}

/// The real probe on this machine: exactly one check on Windows, whose
/// status is never Fail (the invariant this batch exists to pin).
#[cfg(windows)]
#[test]
fn the_real_check_never_fails_the_doctor() {
    let checks = winfsp_checks();
    assert_eq!(checks.len(), 1, "one winfsp check on Windows");
    assert_ne!(
        checks[0].status,
        CheckStatus::Fail,
        "WinFsp absence is a Warn, not a Fail: {}",
        checks[0].detail
    );
    assert!(!checks[0].detail.is_empty(), "the detail always explains");
}

/// Off Windows the walk contributes nothing (the probe cannot mean
/// anything there), which is what keeps the doctor output identical.
#[cfg(not(windows))]
#[test]
fn the_walk_is_empty_off_windows() {
    assert!(winfsp_checks().is_empty());
}
