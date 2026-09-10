//! Read state for one open file handle (WF2 / K41-K42).
//!
//! Two arms, one offset-addressed face:
//!
//! - [`WindowReader`] — the bounded `open_range` window model ported
//!   from the WebDAV adapter's `RangeFile` (SR1 / K34): one aggregated
//!   window per fetch, lazily re-anchored on the requested offset,
//!   short reads legal, never a byte past EOF. The cursor is gone:
//!   WinFsp hands `read(offset, length)` per call, so the requested
//!   offset IS the anchor and there is no position state that could
//!   desync from the buffer.
//! - [`LocalReader`] — the hydrate arm (K33 `StreamSource::Hydrate`,
//!   including WF0's cache-first hit): a plain local file read at an
//!   offset through `FileExt::seek_read`, which never touches the
//!   handle's own file pointer (the Unix `pread` shape) — the same
//!   file may be read concurrently from several WinFsp dispatcher
//!   threads without a seek/read race.
//!
//! Pure logic on purpose: nothing here calls into the WinFsp DLL, so
//! the whole read model is testable on a machine without WinFsp
//! installed.

use std::path::Path;
use std::sync::Arc;

use cloudkit_core::transport::{CloudTransport, RemoteHandle};
use cloudkit_core::vfs::VfsError;
use futures_util::StreamExt;
// The hydrate arm's positioned read. This module compiles only under
// `cfg(all(windows, feature = "winfsp"))` (lib.rs), so the Windows-only
// import is not itself gated.
use std::os::windows::fs::FileExt;

/// Window size for new fetches (K34 parity with `cloudkit-webdav`'s
/// `STREAM_WINDOW`): 4 MiB is also the baidu bounded-slice ceiling, and
/// one aggregated window per handle bounds the adapter's memory.
pub const DEFAULT_READ_WINDOW: u64 = 4 * 1024 * 1024;

/// One aggregated `open_range` window: the logical bytes
/// `[start, start + data.len())`.
struct Window {
    /// Logical offset of `data[0]`.
    start: u64,
    /// Window bytes (`data.len()` ≤ the configured window size).
    data: Vec<u8>,
}

/// Offset-addressed windowed reader over a remote object.
///
/// Ported from `cloudkit-webdav`'s `RangeFile` (SR1/K34) — same window
/// semantics, minus the stream cursor:
///
/// - a window is fetched only when the current one does not cover the
///   position, and a new window is anchored AT that position;
/// - a repeat read inside the window costs nothing (no refetch);
/// - the window is dropped, not refilled, when a read leaves it;
/// - a read never serves past `total_size`, even if a backend over
///   delivers its window.
pub struct WindowReader {
    /// Remote locator (chunks-first assembly, K33's `open_read`).
    handle: RemoteHandle,
    /// Authoritative total length (K35) — also the EOF read bound.
    total_size: u64,
    /// The VFS's transport for bounded-window `open_range` fetches.
    transport: Arc<dyn CloudTransport>,
    /// Window size for new fetches (K34; injectable for tests through
    /// [`crate::fs::CloudFs::with_stream_window`]).
    window: u64,
    /// The current window buffer, if any. There is no `pos` field: the
    /// caller's offset is the only position authority in this design.
    buf: Option<Window>,
}

impl std::fmt::Debug for WindowReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowReader")
            .field("total_size", &self.total_size)
            .field("window", &self.window)
            .field("buffered", &self.buf.is_some())
            .finish_non_exhaustive()
    }
}

impl WindowReader {
    /// Builds a reader over one `open_read` stream source.
    pub fn new(
        handle: RemoteHandle,
        total_size: u64,
        transport: Arc<dyn CloudTransport>,
        window: u64,
    ) -> Self {
        Self {
            handle,
            total_size,
            transport,
            // A 0-byte window could never make progress.
            window: window.max(1),
            buf: None,
        }
    }

    /// The authoritative total length (the row's size, K35).
    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    /// The configured fetch window.
    pub fn window(&self) -> u64 {
        self.window
    }

    /// Whether the current window covers `p` with at least one unread
    /// byte (parity with `RangeFile::covers`).
    fn covers(&self, p: u64) -> bool {
        match &self.buf {
            Some(window) => p >= window.start && p - window.start < window.data.len() as u64,
            None => false,
        }
    }

    /// Fetches `[anchor, anchor + window)` — clamped to EOF — into the
    /// buffer, replacing whatever window was there (K34: opening a window
    /// IS the prefetch; there is no speculative read-ahead).
    async fn fill_window(&mut self, anchor: u64) -> Result<(), VfsError> {
        let len = self.window.min(self.total_size.saturating_sub(anchor));
        let mut stream = self
            .transport
            .open_range(&self.handle, anchor, len)
            .await
            .map_err(VfsError::Transport)?;
        let mut data = Vec::with_capacity(len as usize);
        // Aggregate the whole bounded window before serving any of it
        // (K34's deliberate backpressure shape: memory stays bounded by
        // the window size, never by the file size).
        while let Some(frame) = stream.next().await {
            data.extend_from_slice(&frame.map_err(VfsError::Transport)?);
        }
        self.buf = Some(Window {
            start: anchor,
            data,
        });
        Ok(())
    }

    /// Reads `[offset, offset + buf.len())`, filling as many windows as
    /// the request needs. Returns the bytes actually served: a short read
    /// at EOF (and at a stalled backend).
    ///
    /// WinFsp's cache manager may hand in a buffer far larger than the
    /// configured window (up to a whole file), so this loop — not the
    /// caller — owns the "fill until the buffer is full or EOF" rule.
    /// Every iteration either advances the position or fills the buffer,
    /// so the loop is bounded by `buf.len()` fetches.
    pub async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        let mut filled = 0usize;
        let mut pos = offset;
        while filled < buf.len() && pos < self.total_size {
            if !self.covers(pos) {
                self.fill_window(pos).await?;
                if self
                    .buf
                    .as_ref()
                    .is_none_or(|window| window.data.is_empty())
                {
                    // A zero-byte window below EOF is a backend contract
                    // violation. Ending the read (a short read) beats
                    // spinning: this runs on a WinFsp dispatcher thread
                    // and a stalled backend must not become a hung
                    // callback.
                    tracing::warn!(
                        offset = pos,
                        total_size = self.total_size,
                        "winfsp: backend served an empty window below EOF; answering a short read"
                    );
                    break;
                }
            }
            let window = self.buf.as_ref().expect("window filled above");
            let consumed = (pos - window.start) as usize;
            let available = window.data.len() - consumed;
            // Never serve past EOF, even if a backend over-delivers the
            // window it was asked for.
            let n = available
                .min(buf.len() - filled)
                .min((self.total_size - pos) as usize);
            buf[filled..filled + n].copy_from_slice(&window.data[consumed..consumed + n]);
            filled += n;
            pos += n as u64;
        }
        Ok(filled)
    }
}

/// The hydrate arm's reader: one local plaintext file, read at offsets.
pub struct LocalReader {
    file: std::fs::File,
    total_size: u64,
}

impl std::fmt::Debug for LocalReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalReader")
            .field("total_size", &self.total_size)
            .finish_non_exhaustive()
    }
}

impl LocalReader {
    /// Opens the local copy `hydrate` answered with.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let total_size = file.metadata()?.len();
        Ok(Self { file, total_size })
    }

    /// The local file's length (its own EOF bound).
    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    /// Reads `[offset, offset + buf.len())` from the local file.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        if offset >= self.total_size {
            return Ok(0);
        }
        let limit = buf.len().min((self.total_size - offset) as usize);
        // `seek_read` is Windows' positioned read (the Unix `pread`):
        // it neither moves nor depends on the handle's own file pointer,
        // so concurrent reads from several dispatcher threads cannot
        // interleave a seek with another thread's read.
        self.file.seek_read(&mut buf[..limit], offset)
    }
}

/// The read state of one open file handle: whichever arm K33's
/// `open_read` answered with.
pub enum ReadHandle {
    /// Range streaming (plaintext row, range-capable transport).
    Window(WindowReader),
    /// Local plaintext (hydrate: encrypted / range-incapable / 0-byte /
    /// cache-first hit).
    Local(LocalReader),
}

impl std::fmt::Debug for ReadHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadHandle::Window(reader) => reader.fmt(f),
            ReadHandle::Local(reader) => reader.fmt(f),
        }
    }
}

impl ReadHandle {
    /// The byte length this reader serves up to.
    pub fn total_size(&self) -> u64 {
        match self {
            ReadHandle::Window(reader) => reader.total_size(),
            ReadHandle::Local(reader) => reader.total_size(),
        }
    }

    /// Reads `[offset, offset + buf.len())`; [`Ok(0)`](Ok) means EOF.
    pub async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        match self {
            ReadHandle::Window(reader) => reader.read_at(offset, buf).await,
            ReadHandle::Local(reader) => reader.read_at(offset, buf).map_err(VfsError::Io),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloudkit_core::rel_path::RelPath;
    use cloudkit_core::transport::mock::MockTransport;
    use cloudkit_core::transport::UploadJob;

    /// Deterministic payload bytes (`i % 251`).
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Seeds `bytes` into a fresh mock remote as one message; returns the
    /// transport (the fetch log lives there) plus the handle addressing
    /// the payload.
    async fn seeded(bytes: &[u8]) -> (Arc<MockTransport>, RemoteHandle) {
        let dir = tempfile::tempdir().expect("temp dir");
        let rel = RelPath::new("/payload.bin").expect("valid rel path");
        let local = dir.path().join("payload.bin");
        std::fs::write(&local, bytes).expect("write scratch payload");
        let mock = Arc::new(MockTransport::builder().build());
        mock.connect().await.expect("connect mock");
        let receipt = mock
            .upload(&UploadJob {
                rel_path: rel.clone(),
                local_path: local,
                size: bytes.len() as u64,
                chunk_count: 1,
                chunk_size: bytes.len().max(1) as u64,
            })
            .await
            .expect("seed upload");
        let handle = RemoteHandle {
            first_msg_id: receipt.first_msg_id,
            chunk_msg_ids: receipt.chunk_msg_ids,
            total_size: bytes.len() as u64,
            path: Some(rel),
        };
        (mock, handle)
    }

    /// ①③④⑥ The three anchor rules in one sequence: the first read
    /// fetches a window at the requested offset, a read that leaves the
    /// buffer re-anchors at ITS offset, and a fetch is clamped to EOF.
    #[tokio::test]
    async fn window_fetches_anchor_at_the_requested_offset() {
        let content = pattern(64);
        let (mock, handle) = seeded(&content).await;
        let mut reader = WindowReader::new(handle, 64, mock.clone(), 16);

        let mut buf = [0u8; 4];
        assert_eq!(reader.read_at(0, &mut buf).await.expect("read"), 4);
        assert_eq!(&buf, &content[0..4]);
        assert_eq!(mock.open_range_calls(), vec![(0, 16)]);

        // ③ A far offset re-anchors there (nothing speculative in
        // between), and the window is clamped to EOF: min(16, 64 - 60).
        let mut tail = [0u8; 10];
        assert_eq!(reader.read_at(60, &mut tail).await.expect("read"), 4);
        assert_eq!(&tail[..4], &content[60..64]);
        assert_eq!(mock.open_range_calls(), vec![(0, 16), (60, 4)]);
    }

    /// ② Reads inside the buffered window cost nothing: no refetch, and
    /// the bytes are the exact slice of the remote payload.
    #[tokio::test]
    async fn reads_inside_a_window_do_not_refetch() {
        let content = pattern(64);
        let (mock, handle) = seeded(&content).await;
        let mut reader = WindowReader::new(handle, 64, mock.clone(), 16);

        // (offset, length) pairs that all stay inside [0, 16).
        for (offset, len) in [(0u64, 4usize), (2, 4), (12, 4), (15, 1)] {
            let mut buf = vec![0u8; len];
            assert_eq!(reader.read_at(offset, &mut buf).await.expect("read"), len);
            assert_eq!(
                buf,
                content[offset as usize..offset as usize + len],
                "bytes at offset {offset}"
            );
        }
        assert_eq!(
            mock.open_range_calls(),
            vec![(0, 16)],
            "one window serves every read inside it"
        );
    }

    /// ⑤ At and past EOF the read is empty and performs no fetch at all.
    #[tokio::test]
    async fn reads_at_or_past_eof_are_empty_and_fetch_nothing() {
        let content = pattern(32);
        let (mock, handle) = seeded(&content).await;
        let mut reader = WindowReader::new(handle, 32, mock.clone(), 16);

        let mut buf = [0u8; 8];
        assert_eq!(reader.read_at(32, &mut buf).await.expect("read at EOF"), 0);
        assert_eq!(
            reader.read_at(4096, &mut buf).await.expect("read past EOF"),
            0
        );
        assert!(mock.open_range_calls().is_empty());
    }

    /// ⑦ A read larger than the window fills window after window, in
    /// order, byte-exact across the seams — and the EOF clamp still holds
    /// when the tail request is not a multiple of the window.
    #[tokio::test]
    async fn a_large_read_spans_windows_in_order_and_stops_at_eof() {
        let content = pattern(4096);
        let (mock, handle) = seeded(&content).await;
        let mut reader = WindowReader::new(handle, 4096, mock.clone(), 1024);

        let mut big = vec![0u8; 3072];
        assert_eq!(reader.read_at(0, &mut big).await.expect("read"), 3072);
        assert_eq!(big, content[0..3072]);
        assert_eq!(
            mock.open_range_calls(),
            vec![(0, 1024), (1024, 1024), (2048, 1024)]
        );

        // Offset 3000 is inside the third window; the request then spans
        // into a fourth fetch that stops exactly at EOF.
        let mut tail = vec![0u8; 1096];
        assert_eq!(reader.read_at(3000, &mut tail).await.expect("read"), 1096);
        assert_eq!(tail, content[3000..4096]);
        assert_eq!(
            mock.open_range_calls(),
            vec![(0, 1024), (1024, 1024), (2048, 1024), (3072, 1024)]
        );
    }

    /// A backend that answers a non-EOF window with zero bytes must end
    /// the read (short read) — never spin inside the callback. An empty
    /// chunk list is the mock's way to serve that.
    #[tokio::test]
    async fn a_stalled_backend_window_ends_the_read_instead_of_spinning() {
        let mock = Arc::new(MockTransport::builder().build());
        mock.connect().await.expect("connect mock");
        let handle = RemoteHandle {
            first_msg_id: 0,
            chunk_msg_ids: Vec::new(),
            total_size: 100,
            path: None,
        };
        let mut reader = WindowReader::new(handle, 100, mock.clone(), 16);

        let mut buf = [0u8; 8];
        assert_eq!(reader.read_at(0, &mut buf).await.expect("read"), 0);
        assert_eq!(
            mock.open_range_calls(),
            vec![(0, 16)],
            "one attempt, then stop"
        );
    }

    /// The hydrate arm reads offsets straight off the local file — no
    /// file pointer involved — and stops at its own EOF.
    #[test]
    fn local_reader_serves_offsets_and_stops_at_eof() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("local.bin");
        let bytes = pattern(20);
        std::fs::write(&path, &bytes).expect("write local payload");
        let mut reader = LocalReader::open(&path).expect("open local");
        assert_eq!(reader.total_size(), 20);

        let mut buf = [0u8; 4];
        assert_eq!(reader.read_at(3, &mut buf).expect("read at 3"), 4);
        assert_eq!(&buf, &bytes[3..7]);

        // A read crossing EOF is a legal short read.
        let mut tail = [0u8; 8];
        assert_eq!(reader.read_at(16, &mut tail).expect("read tail"), 4);
        assert_eq!(&tail[..4], &bytes[16..20]);

        // At and past EOF: nothing.
        assert_eq!(reader.read_at(20, &mut tail).expect("read at EOF"), 0);
        assert_eq!(reader.read_at(9999, &mut tail).expect("read past EOF"), 0);

        // Reads are independent of one another (no cursor to rewind).
        assert_eq!(reader.read_at(0, &mut buf).expect("read at 0 again"), 4);
        assert_eq!(&buf, &bytes[0..4]);
    }
}
