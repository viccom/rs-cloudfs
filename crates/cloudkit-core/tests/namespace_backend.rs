//! RED-phase tests for the backend-aware sync namespace (Phase 2 / K12,
//! docs/plans/2026-09-08-phase2-execution.md §6): `cloudkit_core::sync`.
//!
//! Contract under test:
//!
//! - **Telegram guard rail**: the legacy derivation
//!   (`hex(SHA-256("{token}:{chat_id}"))`) stays **byte-for-byte
//!   unchanged** — existing drives must keep converging against the
//!   same server-side namespace (compatibility pinned to the death).
//!   The golden vectors are the ones already pinned by
//!   `tests/sync.rs` (precomputed with an independent python hashlib
//!   run); the backend-aware entry point must reproduce them exactly.
//! - **Baidu**: `baidu:<uid>` — the raw uinfo uid, identical in shape
//!   to the driver's VolumeId (same account = same namespace).
//! - **Local**: `local:<hash(root)>` — a 16-hex-digit digest of the
//!   normalized root path. Stability within a build and distinctness
//!   across roots are the load-bearing properties (the exact digest is
//!   NOT pinned — see the implementation note on DefaultHasher).
//! - **Sync support matrix**: telegram and baidu run the sync task;
//!   local never does (a local drive is the source of truth itself —
//!   `is_sync_supported` is the doctor/启动判定函数, wiring lands in
//!   the B3b dispatch unit).

use cloudkit_core::config::Backend;
use cloudkit_core::sync::{is_sync_supported, namespace_key, namespace_key_for, NamespaceIdentity};

// -------------------------------------------------------------- tests ---

#[test]
fn telegram_derivation_is_byte_identical_guard_rail() {
    // The two golden vectors already pinned by tests/sync.rs
    // (python hashlib precomputation). The backend-aware entry point
    // must return these EXACT bytes — this is the K12 compatibility
    // nail: any drift splits every deployed telegram drive from its
    // server-side namespace.
    let goldens = [
        (
            "bot",
            "42",
            "0c0aa9463ef5ee35cc31f32f8d7f59fbd715a934c5afb946ae354f7e4092ead9",
        ),
        (
            "123456789:AAHfiqkKZ8W2fRzBn8Gh5jX7yLmNpQrStUvWxYz",
            "-1001234567890",
            "1de41cf8423e647d699188a5d2133fc5a235cc6cd24153e6c7b075a804e78052",
        ),
    ];
    for (token, chat, expected) in goldens {
        assert_eq!(namespace_key(token, chat), expected, "legacy fn is frozen");
        assert_eq!(
            namespace_key_for(&NamespaceIdentity::Telegram {
                bot_token: token,
                chat_id: chat,
            }),
            expected,
            "backend-aware fn must be byte-identical for telegram ({token})"
        );
    }
}

#[test]
fn baidu_namespace_is_the_volume_identity() {
    // baidu:<uid> — raw uid, same shape as the driver's VolumeId
    // (K5: uinfo's uid). No hashing: the uid is already a stable,
    // non-secret account identifier.
    assert_eq!(
        namespace_key_for(&NamespaceIdentity::Baidu { uid: "123456789" }),
        "baidu:123456789",
        "baidu namespace = the volume identity string"
    );
    assert_ne!(
        namespace_key_for(&NamespaceIdentity::Baidu { uid: "1" }),
        namespace_key_for(&NamespaceIdentity::Baidu { uid: "2" }),
        "different accounts must not share a namespace"
    );
}

#[test]
fn local_namespace_is_a_stable_hash_of_the_root() {
    let a = namespace_key_for(&NamespaceIdentity::Local {
        root: "E:\\data\\cloudfs",
    });
    let a_again = namespace_key_for(&NamespaceIdentity::Local {
        root: "E:\\data\\cloudfs",
    });
    let b = namespace_key_for(&NamespaceIdentity::Local {
        root: "E:\\data\\other",
    });

    // Shape: "local:" + 16 lowercase hex digits (a u64 digest).
    assert!(a.starts_with("local:"), "local namespace prefix, got {a}");
    let digest = a.strip_prefix("local:").expect("prefix checked above");
    assert_eq!(digest.len(), 16, "u64 hex digest, got {digest:?} in {a:?}");
    assert!(
        digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "lowercase hex only, got {digest:?}"
    );
    // Stability and distinctness are the load-bearing properties (the
    // digest value itself is deliberately not pinned — see the
    // DefaultHasher note in sync.rs).
    assert_eq!(a, a_again, "same root must derive the same namespace");
    assert_ne!(a, b, "different roots must derive different namespaces");
}

#[test]
fn sync_support_matrix() {
    // K12: local drives never start the sync task (even with sync_url
    // set — doctor warns); telegram and baidu both sync.
    assert!(is_sync_supported(&Backend::Telegram));
    assert!(is_sync_supported(&Backend::Baidu));
    assert!(
        !is_sync_supported(&Backend::Local),
        "local must not run the sync task"
    );
}
