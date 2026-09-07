//! Smoke tests for the `StreamReader` bridge (Batch E / E-3): the
//! frames-stream → `AsyncRead` adapter the streaming v2 upload feeds
//! into grammers. The adapter is pure byte plumbing, but a single
//! off-by-one here corrupts every v2 upload — so the frame-to-byte
//! continuity, the short-read contract and the error surfacing are
//! pinned independently of any network.

use ck_telegram::stream::StreamReader;
use cloudkit_storage::transport::ByteStream;
use tokio::io::AsyncReadExt;

fn frames_stream(frames: Vec<Vec<u8>>) -> ByteStream {
    Box::pin(futures_util::stream::iter(frames.into_iter().map(|f| {
        Ok::<bytes::Bytes, cloudkit_storage::StorageError>(bytes::Bytes::from(f))
    })))
}

#[tokio::test]
async fn concatenates_frames_across_arbitrary_read_boundaries() {
    let payload: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
    // Frames deliberately misaligned with every read buffer size below.
    let frames = vec![
        payload[..1].to_vec(),
        payload[1..600].to_vec(),
        payload[600..601].to_vec(),
        payload[601..].to_vec(),
    ];
    let mut reader = StreamReader::new(frames_stream(frames));
    let mut out = Vec::new();
    // Read in odd-sized chunks to cross frame boundaries mid-read.
    let mut buf = [0u8; 7];
    loop {
        let n = reader.read(&mut buf).await.expect("read");
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    assert_eq!(out, payload, "bytes are continuous and in order");
    // EOF is sticky.
    let n = reader.read(&mut buf).await.expect("read after EOF");
    assert_eq!(n, 0);
}

#[tokio::test]
async fn empty_stream_reads_eof_immediately() {
    let mut reader = StreamReader::new(frames_stream(vec![]));
    let mut buf = [0u8; 16];
    assert_eq!(reader.read(&mut buf).await.expect("read"), 0);
}
