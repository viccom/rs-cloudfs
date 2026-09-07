//! `CryptoScheme` trait semantics: scheme identity, v1-through-trait format
//! identity with the legacy free functions, clamped `decrypt_range`
//! semantics, and polymorphic dispatch over both schemes via
//! `&dyn CryptoScheme` (the shape the Entry-metadata read path will use,
//! E-4).

use std::io::Cursor;

use cloudkit_crypto::v1::{self, GcmV1};
use cloudkit_crypto::v2::AeadV2;
use cloudkit_crypto::{CryptoError, CryptoScheme, CryptoSchemeId};

const PW: &str = "scheme dispatch 测试";

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

fn schemes() -> Vec<Box<dyn CryptoScheme>> {
    vec![
        Box::new(GcmV1::new()),
        Box::new(AeadV2::with_chunk_size(64 * 1024).expect("min chunk legal")),
    ]
}

// --------------------------------------------------------------- identity ---

#[test]
fn scheme_ids_and_wire_strings() {
    let gcm = GcmV1::new();
    let aead = AeadV2::new();
    assert_eq!(gcm.id(), CryptoSchemeId::GcmV1);
    assert_eq!(aead.id(), CryptoSchemeId::AeadV2);
    assert_eq!(CryptoSchemeId::GcmV1.as_str(), "gcm-v1");
    assert_eq!(CryptoSchemeId::AeadV2.as_str(), "aead-v2");
    assert_eq!(CryptoSchemeId::GcmV1.to_string(), "gcm-v1");
    assert_ne!(CryptoSchemeId::GcmV1, CryptoSchemeId::AeadV2);
}

// ------------------------------------------------ v1 through the trait ---

#[test]
fn gcm_v1_trait_stream_is_the_frozen_format() {
    let pt = pattern(70_003);
    let scheme = GcmV1::new();

    let mut ct = Vec::new();
    let written = scheme
        .encrypt_stream(PW, &mut Cursor::new(&pt), &mut ct)
        .expect("encrypt_stream");
    // Frozen v1 overhead: salt + nonce + tag = 44 bytes.
    assert_eq!(ct.len(), pt.len() + v1::SALT_SIZE + v1::NONCE_SIZE + 16);
    assert_eq!(written as usize, ct.len());
    // The trait output must be consumable by the legacy free function —
    // same bytes, same format.
    assert_eq!(v1::decrypt(PW, &ct).expect("legacy decrypt"), pt);

    let mut out = Vec::new();
    scheme
        .decrypt_stream(PW, &mut Cursor::new(&ct), &mut out)
        .expect("decrypt_stream");
    assert_eq!(out, pt);
}

#[test]
fn gcm_v1_decrypt_range_clamps_and_slices() {
    let pt = pattern(1000);
    let mut ct = Vec::new();
    GcmV1::new()
        .encrypt_stream(PW, &mut Cursor::new(&pt), &mut ct)
        .expect("encrypt");
    let scheme = GcmV1::new();

    assert_eq!(scheme.decrypt_range(PW, &ct, 10..20).unwrap(), &pt[10..20]);
    // end beyond the plaintext length clamps to it.
    assert_eq!(
        scheme.decrypt_range(PW, &ct, 990..5000).unwrap(),
        &pt[990..]
    );
    // start at/past the end yields empty, not an error.
    assert!(scheme
        .decrypt_range(PW, &ct, 1000..1010)
        .unwrap()
        .is_empty());
    assert!(scheme
        .decrypt_range(PW, &ct, 5000..6000)
        .unwrap()
        .is_empty());
    assert!(scheme.decrypt_range(PW, &ct, 5..5).unwrap().is_empty());
    // Tampering stays fail-closed through the trait surface.
    let mut bad = ct.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xFF;
    assert!(matches!(
        scheme.decrypt_range(PW, &bad, 0..10),
        Err(CryptoError::AuthFailed)
    ));
}

// ----------------------------------------------------- polymorphic dispatch ---

#[test]
fn dyn_dispatch_roundtrip_and_range_for_both_schemes() {
    let pt = pattern(150_000); // spans chunks for the v2 leg (64 KiB)
    for scheme in schemes() {
        let label = scheme.id().to_string();
        let mut ct = Vec::new();
        scheme
            .encrypt_stream(PW, &mut Cursor::new(&pt), &mut ct)
            .unwrap_or_else(|e| panic!("{label}: encrypt_stream: {e}"));
        assert!(!ct.is_empty(), "{label}: ciphertext must not be empty");

        let mut out = Vec::new();
        scheme
            .decrypt_stream(PW, &mut Cursor::new(&ct), &mut out)
            .unwrap_or_else(|e| panic!("{label}: decrypt_stream: {e}"));
        assert_eq!(out, pt, "{label}: stream roundtrip");

        let mid = scheme
            .decrypt_range(PW, &ct, 60_000..90_000)
            .unwrap_or_else(|e| panic!("{label}: decrypt_range: {e}"));
        assert_eq!(mid, &pt[60_000..90_000], "{label}: range slice");

        assert!(
            matches!(
                scheme.decrypt_range("wrong", &ct, 0..10),
                Err(CryptoError::AuthFailed)
            ),
            "{label}: wrong password must fail closed"
        );
    }
}

// ------------------------------------------------------------ trait shape ---

fn assert_send_sync<T: Send + Sync + ?Sized>(_: &T) {}

#[test]
fn scheme_objects_are_send_sync_and_dyn_safe() {
    // Compile-time pins: usable as `&dyn CryptoScheme` across threads and
    // from async contexts (L4 consumers wrap in spawn_blocking).
    for scheme in schemes() {
        assert_send_sync(&*scheme);
    }
    let boxed: Box<dyn CryptoScheme> = Box::new(GcmV1::new());
    assert_send_sync(&boxed);
}
