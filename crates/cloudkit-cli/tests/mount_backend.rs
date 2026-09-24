//! K40 mount-backend dispatch tests (Phase 3 / WF4).
//!
//! The decision half of `mount_backend`: which backend a boot attempts,
//! and — the load-bearing half — what happens when the config asks for
//! WinFsp and this build or this machine cannot provide it. The rule is
//! K40's: **degrade visibly, never silently, never refuse to start** —
//! and when the binary was built without `--features winfsp`, say exactly
//! that (K31's driver-features precedent, which the task pins for this
//! path too).
//!
//! These tests run in both legs. The default leg (no `winfsp` feature) is
//! the CI box's: the capability is [`WinFspCapability::NotCompiled`] and
//! the degradation message must carry the rebuild command. The
//! `--features winfsp` leg exercises the same matrix with the *runtime*
//! gate instead.

use cloudkit_cli::{
    choose_mount_backend, single_volume_winfsp_note, winfsp_capability, winfsp_unavailable_notice,
    winfsp_unmount_note, winfsp_unmount_step, MountBackendDecision, MountedBackend,
    WinFspCapability, WinfspUnmountStep, WINFSP_FEATURE_REQUIRED,
};
use cloudkit_core::config::{CyDriveConfig, MountBackend};

/// The default config asks for nothing new: the WebDAV mapping, exactly
/// 负责人 2026-09-24 裁决: the default backend is winfsp, and an
/// unavailable winfsp runtime does NOT fall back to webdav.
#[test]
fn default_config_chooses_winfsp_and_never_falls_back() {
    match choose_mount_backend(
        CyDriveConfig::default().mount_backend,
        &WinFspCapability::NotCompiled,
    ) {
        MountBackendDecision::WinfspUnavailable(reason) => {
            assert!(
                reason.contains("--features winfsp"),
                "the default (winfsp) without the feature answers unavailable with the                  rebuild hint, got: {reason}"
            );
        }
        other => panic!("the default must not fall back to webdav, got: {other:?}"),
    }
    assert_eq!(
        choose_mount_backend(MountBackend::Webdav, &WinFspCapability::Ready),
        MountBackendDecision::WebDav,
        "an explicit webdav request is honoured verbatim"
    );
}

/// The happy path: winfsp requested, winfsp usable.
#[test]
fn a_ready_runtime_mounts_through_winfsp() {
    assert_eq!(
        choose_mount_backend(MountBackend::Winfsp, &WinFspCapability::Ready),
        MountBackendDecision::WinFsp
    );
}

/// The CI-leg case the task calls out: the config asks for winfsp, the
/// binary has no feature — the message must name the rebuild command, the
/// config key, and the fallback that is actually happening.
#[test]
fn a_missing_feature_answers_unavailable_with_the_rebuild_hint() {
    match choose_mount_backend(MountBackend::Winfsp, &WinFspCapability::NotCompiled) {
        MountBackendDecision::WinfspUnavailable(reason) => {
            assert!(
                reason.contains("--features winfsp"),
                "the reason names the rebuild feature, got: {reason}"
            );
            assert!(
                reason.contains("cargo build"),
                "the reason names the rebuild command, got: {reason}"
            );
            assert!(
                reason.contains("mount_backend"),
                "the reason names the config key that selected it, got: {reason}"
            );
            assert!(
                reason.contains("webdav"),
                "the reason names the explicit webdav opt-out, got: {reason}"
            );
        }
        other => panic!("expected an unavailable verdict, got {other:?}"),
    }
}

/// An installed-but-unusable runtime answers unavailable too, carrying the
/// runtime's own reason (the WinFsp detection's string — actionable by
/// construction). No webdav fallback (负责人 2026-09-24 裁决).
#[test]
fn an_unusable_runtime_answers_unavailable_with_its_reason() {
    let capability =
        WinFspCapability::Unavailable("WinFsp is not installed (no InstallDir)".into());
    match choose_mount_backend(MountBackend::Winfsp, &capability) {
        MountBackendDecision::WinfspUnavailable(reason) => {
            assert_eq!(reason, "WinFsp is not installed (no InstallDir)");
        }
        other => panic!("expected an unavailable verdict, got {other:?}"),
    }
}

/// Every decision maps to a banner label: the per-volume backend the run
/// banner prints (the user-visible proof that the degradation happened).
#[test]
fn decisions_render_as_banner_labels() {
    assert_eq!(MountBackendDecision::WebDav.label(), "webdav");
    assert_eq!(MountBackendDecision::WinFsp.label(), "winfsp");
    let unavailable = MountBackendDecision::WinfspUnavailable("no runtime".to_string());
    let label = unavailable.label();
    assert!(
        label.starts_with("winfsp ("),
        "the unavailable label leads with the requested backend, got: {label}"
    );
    assert!(
        label.contains("no runtime"),
        "the unavailable label carries the reason, got: {label}"
    );
}

/// The outcome labels (what the multi-volume banner annotates per volume).
#[test]
fn mounted_backends_render_per_volume() {
    assert_eq!(MountedBackend::WinFsp.as_str(), "winfsp");
    assert_eq!(MountedBackend::WebDav.as_str(), "webdav");
    assert_eq!(MountedBackend::WinFsp.label(), "winfsp");
}

/// The notice printed (and logged at error level) when the winfsp arm is
/// unavailable: backend, reason, the fact that letters are simply not
/// mounted, and the two ways up (install WinFsp / opt into webdav).
#[test]
fn the_unavailable_notice_says_what_happened_and_the_ways_up() {
    let notice = winfsp_unavailable_notice("WinFsp is not installed");
    assert!(
        notice.contains("winfsp") && notice.contains("WinFsp is not installed"),
        "the notice names the backend and the reason, got: {notice}"
    );
    assert!(
        notice.contains("WebDAV") || notice.contains("webdav"),
        "the notice says drive letters are not mounted, got: {notice}"
    );
    assert!(
        notice.contains("mount_backend") && notice.contains("stay reachable"),
        "the notice names the explicit webdav opt-in and reachability, got: {notice}"
    );
    // The feature-less build's notice carries the same rebuild hint.
    let notice = winfsp_unavailable_notice(WINFSP_FEATURE_REQUIRED);
    assert!(notice.contains("--features winfsp"), "got: {notice}");
}

/// This build's capability is coherent: without the feature the answer is
/// exactly `NotCompiled`; with it, either `Ready` or an actionable
/// `Unavailable` (never a panic — the CI box has no WinFsp).
#[test]
fn the_capability_of_this_build_is_coherent() {
    let capability = winfsp_capability();
    #[cfg(not(all(windows, feature = "winfsp")))]
    assert_eq!(
        capability,
        WinFspCapability::NotCompiled,
        "a build without the winfsp feature cannot mount through WinFsp"
    );
    #[cfg(all(windows, feature = "winfsp"))]
    match &capability {
        WinFspCapability::Ready => {}
        WinFspCapability::Unavailable(reason) => assert!(
            !reason.is_empty(),
            "an unusable runtime must explain itself (K40's decision input)"
        ),
        WinFspCapability::NotCompiled => {
            panic!("the feature is on: the capability must reflect the runtime, not the build")
        }
    }
    // Whatever it is, the decision it drives is total.
    let _ = choose_mount_backend(MountBackend::Winfsp, &capability);
}

/// `cydrive unmount` under the winfsp backend: the honest answer, with
/// the way out — and nothing that pretends the mapping was released.
#[test]
fn winfsp_unmount_is_explained_not_pretended() {
    let mut cfg = CyDriveConfig::default();
    assert!(
        winfsp_unmount_note(&cfg).is_some(),
        "the default (winfsp) instance gets the unmount-lifetime explanation"
    );
    cfg.mount_backend = MountBackend::Webdav;
    assert!(
        winfsp_unmount_note(&cfg).is_none(),
        "the WebDAV mapping IS cross-process: nothing to explain"
    );
    cfg.mount_backend = MountBackend::Winfsp;
    let note = winfsp_unmount_note(&cfg).expect("a winfsp instance must be answered");
    assert!(
        note.contains("in-process") && note.contains("exit"),
        "the note explains the lifetime, got: {note}"
    );
    assert!(
        note.contains("cross-process unmount") || note.contains("no cross-process"),
        "the note names WinFsp's missing cross-process unmount, got: {note}"
    );
    assert!(
        note.contains("cydrive stop") && note.contains("Ctrl+C"),
        "the note names the way out, got: {note}"
    );
}

/// Single-volume mode's scope note: the in-process backend is wired into
/// the multi-volume flow, so a single-volume config asking for it must be
/// told (in visible text) what it is getting instead.
#[test]
fn single_volume_winfsp_request_is_an_explicit_note() {
    let mut cfg = CyDriveConfig::default();
    // The default IS winfsp now (负责人 2026-09-24 裁决), so a default
    // single-volume config gets the scope note; an EXPLICIT webdav
    // request is the quiet one.
    assert!(
        single_volume_winfsp_note(&cfg).is_some(),
        "the default (winfsp) single-volume config carries the scope note"
    );
    cfg.mount_backend = MountBackend::Webdav;
    assert!(
        single_volume_winfsp_note(&cfg).is_none(),
        "an explicit webdav request needs no note"
    );
    cfg.mount_backend = MountBackend::Winfsp;
    let note = single_volume_winfsp_note(&cfg).expect("a winfsp request must be answered");
    assert!(
        note.contains("multi-volume"),
        "the note says where the backend IS wired, got: {note}"
    );
    assert!(
        note.contains("webdav") || note.contains("WebDAV"),
        "the note says what is happening instead, got: {note}"
    );
}

/// RB3 / cli-H1: the unmount decision against probed reality. A
/// *degraded* winfsp boot created a real `net use` mapping (K40's
/// visible fallback) — `cydrive unmount` must release it, at the letter
/// the probe actually found, instead of refusing; only a winfsp instance
/// with no mapping for its drive URL gets the in-process note, and the
/// WebDAV backend keeps its cross-process flow (`None`).
#[test]
fn unmount_releases_the_probed_mapping_of_a_degraded_winfsp_boot() {
    let step = winfsp_unmount_step(MountBackend::Winfsp, Some("Q:".to_string()));
    match step {
        Some(WinfspUnmountStep::ReleaseDegradedMapping { letter }) => {
            assert_eq!(
                letter, "Q:",
                "the release targets the probe-verified letter, not the configured one"
            );
        }
        other => panic!("a probed mapping must be released, got {other:?}"),
    }
}

/// No mapping for the drive URL: the honest in-process note stands and
/// nothing is touched (an in-process winfsp mount is released when its
/// process exits — never a fake success here).
#[test]
fn unmount_without_a_mapping_explains_the_in_process_mount() {
    assert_eq!(
        winfsp_unmount_step(MountBackend::Winfsp, None),
        Some(WinfspUnmountStep::ExplainInProcess),
        "no mapping for the drive URL: the note stands, nothing is touched"
    );
}

/// The WebDAV backend's mapping IS cross-process: the decision hands the
/// request back to the original resolve-and-release flow, whatever the
/// probe saw.
#[test]
fn the_webdav_backend_keeps_its_cross_process_unmount() {
    assert_eq!(
        winfsp_unmount_step(MountBackend::Webdav, Some("Y:".to_string())),
        None,
        "webdav keeps the resolve-and-release flow"
    );
    assert_eq!(winfsp_unmount_step(MountBackend::Webdav, None), None);
}
