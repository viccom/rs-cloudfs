//! K47 chunk-window reader surface (Phase 3.5-a E1): the public
//! [`AeadV2Window`] exposes the container layout arithmetic and per-chunk
//! authenticated decryption that a random-access consumer (the core
//! decrypting transport) needs to translate plaintext windows into
//! ciphertext spans without whole-file buffering.
//!
//! The window reader must agree with the real encoder/decoder: chunk
//! counts and tail lengths are cross-validated against actual containers
//! (the encoder emits NO trailing empty chunk for exact multiples), and
//! window decryption of any half-open range must equal the full-decrypt
//! slice. All failure modes fail closed.

use cloudkit_crypto::v2::{AeadV2, AeadV2Window, HEADER_SIZE, MIN_CHUNK_SIZE, TAG_SIZE};
use cloudkit_crypto::CryptoError;

const PW: &str = "aead v2 window 测试密码 🔐";
/// Smallest legal chunk size keeps multi-chunk fixtures small.
const S: usize = MIN_CHUNK_SIZE;

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

fn scheme_small() -> AeadV2 {
    AeadV2::with_chunk_size(S).expect("min chunk size is legal")
}

/// Opens a window reader over a real container's header.
fn window_for(ct: &[u8]) -> AeadV2Window {
    scheme_small()
        .window_reader(PW, &ct[..HEADER_SIZE])
        .expect("window reader over a real header")
}

/// Deterministic LCG for window picks (no rand dev-dependency here).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 16
    }
}

/// Decrypts plaintext `[s, e)` through the window API: one ciphertext
/// span, per-chunk authenticated decryption, window slice.
fn window_via_chunks(
    w: &AeadV2Window,
    ct: &[u8],
    s: u64,
    e: u64,
    plain_len: u64,
) -> Result<Vec<u8>, CryptoError> {
    let cs = w.chunk_size() as u64;
    let first = s / cs;
    let last = (e - 1) / cs;
    let (body_off, body_len) = w.ciphertext_span(first, last, plain_len);
    let n = w.n_chunks_for_plain(plain_len);
    let mut covered = Vec::new();
    for i in first..=last {
        let (off, len) = w.ciphertext_span(i, i, plain_len);
        debug_assert!(off >= body_off && off + len <= body_off + body_len);
        covered.extend(w.decrypt_chunk(i, i + 1 == n, &ct[off as usize..(off + len) as usize])?);
    }
    let skip = (s - first * cs) as usize;
    let take = (e - s) as usize;
    Ok(covered[skip..skip + take].to_vec())
}

// --------------------------------------------------------------- opening ---

#[test]
fn window_reader_reports_container_chunk_size() {
    let ct = scheme_small().encrypt(PW, &pattern(2 * S + 5));
    let w = window_for(&ct);
    assert_eq!(w.chunk_size(), S, "chunk size comes from the header field");
    // A differently-configured scheme instance reads the same container:
    // decryption honors the header, not the reader's default.
    let w2 = AeadV2::new()
        .window_reader(PW, &ct[..HEADER_SIZE])
        .expect("window over the same header");
    assert_eq!(w2.chunk_size(), S);
}

#[test]
fn window_reader_rejects_structurally_bad_headers() {
    let good = scheme_small().encrypt(PW, &pattern(S + 3));
    let header = good[..HEADER_SIZE].to_vec();

    // Sub-header input.
    assert!(matches!(
        scheme_small().window_reader(PW, &header[..HEADER_SIZE - 1]),
        Err(CryptoError::TooShort)
    ));
    // Bad magic.
    let mut bad = header.clone();
    bad[0] = b'X';
    assert!(matches!(
        scheme_small().window_reader(PW, &bad),
        Err(CryptoError::BadMagic)
    ));
    // Unsupported version.
    let mut bad = header.clone();
    bad[8] = 0x7F;
    assert!(matches!(
        scheme_small().window_reader(PW, &bad),
        Err(CryptoError::UnsupportedVersion(0x7F))
    ));
    // Non-zero reserved byte.
    let mut bad = header.clone();
    bad[9] = 1;
    assert!(matches!(
        scheme_small().window_reader(PW, &bad),
        Err(CryptoError::Malformed)
    ));
    // Zero iterations (KDF-DoS guardrail) and over-cap iterations.
    for iters in [0u32, 2_000_000u32] {
        let mut bad = header.clone();
        bad[26..30].copy_from_slice(&iters.to_be_bytes());
        assert!(matches!(
            scheme_small().window_reader(PW, &bad),
            Err(CryptoError::Malformed)
        ));
    }
    // Chunk size outside the format guardrails.
    let mut bad = header;
    bad[30..34].copy_from_slice(&1_000u32.to_be_bytes());
    assert!(matches!(
        scheme_small().window_reader(PW, &bad),
        Err(CryptoError::Malformed)
    ));
}

// ------------------------------------------------- layout arithmetic -------

#[test]
fn layout_arithmetic_matches_real_containers() {
    let sizes = [0usize, 1, S - 1, S, S + 1, 2 * S, 2 * S + S / 2, 3 * S + 17];
    for &n in &sizes {
        let pt = pattern(n);
        let ct = scheme_small().encrypt(PW, &pt);
        let w = window_for(&ct);

        let n_chunks = w.n_chunks_for_plain(n as u64);
        // Cross-validated against the real encoder: total container
        // length is header + plaintext + one tag per chunk, so a wrong
        // chunk count (e.g. an extra empty tail on exact multiples)
        // breaks this equality.
        assert_eq!(
            ct.len() as u64,
            HEADER_SIZE as u64 + n as u64 + n_chunks * TAG_SIZE as u64,
            "chunk count for size {n}"
        );

        // Tail chunk plaintext length (0 only for the empty file).
        let expected_tail = if n == 0 {
            0
        } else {
            n - (n_chunks as usize - 1) * S
        };
        assert_eq!(w.plain_tail(n as u64), expected_tail, "tail for size {n}");

        // The whole-body span is exactly the body, header included.
        assert_eq!(
            w.ciphertext_span(0, n_chunks - 1, n as u64),
            (HEADER_SIZE as u64, ct.len() as u64 - HEADER_SIZE as u64),
            "body span for size {n}"
        );

        // The tail chunk decrypts (is_last=true) to the plaintext tail.
        let (off, len) = w.ciphertext_span(n_chunks - 1, n_chunks - 1, n as u64);
        let tail_pt = w
            .decrypt_chunk(n_chunks - 1, true, &ct[off as usize..(off + len) as usize])
            .unwrap_or_else(|e| panic!("tail chunk decrypt failed for size {n}: {e}"));
        assert_eq!(tail_pt, pt[n - expected_tail..], "tail plaintext for {n}");
    }
}

#[test]
fn exact_multiple_has_no_trailing_empty_chunk() {
    let pt = pattern(2 * S);
    let ct = scheme_small().encrypt(PW, &pt);
    let w = window_for(&ct);
    assert_eq!(w.n_chunks_for_plain(2 * S as u64), 2, "no empty tail chunk");
    assert_eq!(w.plain_tail(2 * S as u64), S, "the final chunk is FULL");
    assert_eq!(
        w.ciphertext_span(1, 1, 2 * S as u64),
        (
            HEADER_SIZE as u64 + (S + TAG_SIZE) as u64,
            (S + TAG_SIZE) as u64
        ),
        "chunk 1 span reaches the container end"
    );
}

#[test]
fn empty_file_window_is_a_single_tag_only_chunk() {
    let ct = scheme_small().encrypt(PW, &[]);
    let w = window_for(&ct);
    assert_eq!(w.n_chunks_for_plain(0), 1);
    assert_eq!(w.plain_tail(0), 0);
    assert_eq!(
        w.ciphertext_span(0, 0, 0),
        (HEADER_SIZE as u64, TAG_SIZE as u64),
        "tag-only final chunk"
    );
    assert_eq!(
        w.decrypt_chunk(0, true, &ct[HEADER_SIZE..HEADER_SIZE + TAG_SIZE])
            .expect("empty final chunk decrypts"),
        Vec::<u8>::new()
    );
}

// ------------------------------------------------- window correctness ------

#[test]
fn window_decrypt_matches_full_decrypt_for_deterministic_windows() {
    let sizes = [
        1usize,
        100,
        S / 2,
        S,
        S + 1,
        2 * S,
        2 * S + S / 2,
        5 * S + 123,
    ];
    for &n in &sizes {
        let pt = pattern(n);
        let ct = scheme_small().encrypt(PW, &pt);
        let w = window_for(&ct);
        let full = scheme_small()
            .decrypt(PW, &ct)
            .expect("full decrypt sanity");
        assert_eq!(full, pt);

        // 24 deterministic windows per size: mixed alignments around
        // chunk boundaries, 1-byte windows, whole-file, EOF edges.
        let mut rng = Lcg(0x2545_F491_4F6C_DD1D);
        for _ in 0..24 {
            let len = n as u64;
            let s = rng.next() % len;
            let e = s + 1 + rng.next() % (len - s);
            let got = window_via_chunks(&w, &ct, s, e, len)
                .unwrap_or_else(|err| panic!("window decrypt failed at [{s}, {e}) of {n}: {err}"));
            assert_eq!(got, pt[s as usize..e as usize], "window [{s}, {e}) of {n}");
        }
        // Whole-file window pins the no-clamp identity.
        assert_eq!(
            window_via_chunks(&w, &ct, 0, n as u64, n as u64).expect("whole-file window"),
            pt
        );
    }
}

// ---------------------------------------------------- fail-closed ---------

#[test]
fn window_decrypt_fails_closed() {
    let pt = pattern(2 * S + 7);
    let ct = scheme_small().encrypt(PW, &pt);
    let w = window_for(&ct);
    let (off0, len0) = w.ciphertext_span(0, 0, pt.len() as u64);
    let chunk0 = ct[off0 as usize..(off0 + len0) as usize].to_vec();

    // Wrong password at open: the key derivation succeeds but every
    // chunk fails authentication.
    let wrong_pw = scheme_small()
        .window_reader("wrong password", &ct[..HEADER_SIZE])
        .expect("header parses under any password");
    assert!(matches!(
        wrong_pw.decrypt_chunk(0, false, &chunk0),
        Err(CryptoError::AuthFailed)
    ));

    // Corrupted ciphertext byte.
    let mut flipped = chunk0.clone();
    flipped[3] ^= 0x40;
    assert!(matches!(
        w.decrypt_chunk(0, false, &flipped),
        Err(CryptoError::AuthFailed)
    ));

    // Wrong chunk index (nonce domain separation).
    assert!(matches!(
        w.decrypt_chunk(1, false, &chunk0),
        Err(CryptoError::AuthFailed)
    ));

    // Wrong finality flag (final/non-final nonce spaces are disjoint).
    assert!(matches!(
        w.decrypt_chunk(0, true, &chunk0),
        Err(CryptoError::AuthFailed)
    ));

    // Truncated chunk (below the tag size).
    assert!(matches!(
        w.decrypt_chunk(0, false, &chunk0[..10]),
        Err(CryptoError::AuthFailed)
    ));

    // A truncated container's tail chunk span exceeds the ciphertext:
    // slicing honestly and decrypting the short input fails closed.
    let truncated = &ct[..ct.len() - 5];
    let (off, len) = w.ciphertext_span(2, 2, pt.len() as u64);
    let clamped = &truncated[off as usize..truncated.len().min((off + len) as usize)];
    assert!(matches!(
        w.decrypt_chunk(2, true, clamped),
        Err(CryptoError::AuthFailed)
    ));
}
