//! SF2 行为测试 · 读路径往返（计划 §5 SF2 验收原文：list / stat /
//! range-read / error 往返）。
//!
//! 全部 hermetic（`stub::Stub` 进程内服务端）。核心断言：
//! - **list**：depth-1 + 字典序 + `off:N` 分页令牌翻页 + `.`/`..` 过滤
//!   （桩 readdir 回放真实目录流形态，含二者）；
//! - **stat**：size / mtime / kind；missing → `NotFound`（NoSuchFile 映射）；
//! - **reader Range**：seek offset 生效 + 跨 64KiB 帧边界读 + 越界钳制 +
//!   `start >= size` 空流（窗口外数据不取——桩 read 精确窗口切片）；
//! - **错误往返**：对不存在路径的 stat / list / reader 分类断言
//!   （PermissionDenied/连接类绝不折叠为 NotFound——error.rs 红线）。

mod stub;

use ck_sftp::{SftpDriver, SftpParams};
use cloudkit_storage::{
    BackendHandle, EntryId, EntryKind, Page, PageCursor, Range, RelPath, StorageDriver,
    StorageError,
};
use futures_util::StreamExt;
use stub::{Stub, StubAuth};

const USER: &str = "tester";
const PASSWORD: &str = "stub-only-password";

/// 确定性载荷（非全零——防偷懒匹配；周期 251 与仓外探针同款）。
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

/// 起桩 + 落好指纹的驱动。
async fn setup() -> (Stub, SftpDriver) {
    let stub = Stub::start(StubAuth::password(USER, PASSWORD)).await;
    let mut pairs = stub.param_pairs();
    pairs.push((
        "sftp_host_fingerprint".to_string(),
        stub.fingerprint().to_string(),
    ));
    let params = SftpParams::from_pairs(&pairs).expect("params");
    let driver = SftpDriver::new(params).expect("driver");
    (stub, driver)
}

fn rel(path: &str) -> RelPath {
    RelPath::new(path).expect("valid rel path")
}

fn file_id(driver: &SftpDriver, path: &str) -> EntryId {
    EntryId::new(driver.volume().clone(), BackendHandle::new(path))
}

/// 消费 ByteStream 到字节向量（错误项即时 panic——读路径测试不含
/// 预期中的流中途错误）。
async fn read_all(stream: cloudkit_storage::ByteStream) -> Result<Vec<u8>, StorageError> {
    let mut stream = stream;
    let mut out = Vec::new();
    while let Some(frame) = stream.next().await {
        out.extend_from_slice(&frame?);
    }
    Ok(out)
}

// --------------------------------------------------------------- list ---

/// depth-1 + 字典序 + `.`/`..` 过滤：子目录只出目录条目本身，孙辈不
/// 展开；readdir 的多轮翻页（桩每轮 ≤2 条）与条目序不影响最终序。
#[tokio::test]
async fn list_is_depth1_sorted_and_filters_dot_entries() {
    let (stub, driver) = setup().await;
    stub.add_file("/b.txt", b"beta");
    stub.add_file("/a.txt", b"alpha");
    stub.add_dir("/dir");
    stub.add_file("/dir/inner.txt", b"inner");
    stub.add_file("/z.txt", b"zeta");

    let listing = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list root");
    let names: Vec<&str> = listing.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        names,
        vec!["a.txt", "b.txt", "dir", "z.txt"],
        "depth-1 lexicographic"
    );
    assert!(listing.next.is_none(), "Page::all exhausts");
    // kind 标注：dir 是 Dir，文件是 File
    let dir_entry = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "dir")
        .expect("dir");
    assert_eq!(dir_entry.kind, EntryKind::Dir);
    let file_entry = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "a.txt")
        .expect("a");
    assert_eq!(file_entry.kind, EntryKind::File);
    assert_eq!(file_entry.size, 5);
}

/// `off:N` 分页令牌翻页：limit=2 三页收齐四个条目，页序衔接无重无漏。
#[tokio::test]
async fn list_pagination_walks_off_tokens() {
    let (stub, driver) = setup().await;
    for name in ["a", "b", "c", "d"] {
        stub.add_file(&format!("/{name}.txt"), name.as_bytes());
    }

    let mut collected: Vec<String> = Vec::new();
    let mut cursor = PageCursor::Start;
    let pages = 3; // 4 条 / limit 2 → 2+2+空尾页（或 2 页后 next=None）
    for _ in 0..pages {
        let listing = driver
            .list(&RelPath::root(), Page { limit: 2, cursor })
            .await
            .expect("list page");
        collected.extend(listing.entries.iter().map(|e| e.path.as_str().to_string()));
        match listing.next {
            Some(next) => cursor = next,
            None => break,
        }
    }
    assert_eq!(
        collected,
        vec!["a.txt", "b.txt", "c.txt", "d.txt"],
        "paged walk collects everything exactly once"
    );
    // 伪令牌回退 0 重放（local 同款容错）——破坏性令牌不炸
    let listing = driver
        .list(
            &RelPath::root(),
            Page {
                limit: 2,
                cursor: PageCursor::Next("garbage".to_string()),
            },
        )
        .await
        .expect("garbage token falls back to start");
    assert_eq!(listing.entries.len(), 2);
}

/// 空目录：List 空 + next=None（桩 readdir 首轮即 EOF 的客户端消费面）。
#[tokio::test]
async fn list_empty_dir_is_empty_listing() {
    let (stub, driver) = setup().await;
    stub.add_dir("/empty");
    let listing = driver.list(&rel("empty"), Page::all()).await.expect("list");
    assert!(listing.entries.is_empty());
    assert!(listing.next.is_none());
}

/// 子目录视角：list(dir) 只见直接子项。
#[tokio::test]
async fn list_subdirectory_direct_children_only() {
    let (stub, driver) = setup().await;
    stub.add_dir("/x");
    stub.add_dir("/x/y");
    stub.add_file("/x/y/deep.txt", b"d");
    stub.add_file("/x/file.txt", b"f");

    let listing = driver.list(&rel("x"), Page::all()).await.expect("list x");
    let names: Vec<&str> = listing.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(names, vec!["x/file.txt", "x/y"]);
}

// --------------------------------------------------------------- stat ---

/// stat 字段：size / mtime（确定性非零）/ kind（文件与目录）。
#[tokio::test]
async fn stat_reports_size_mtime_and_kind() {
    let (stub, driver) = setup().await;
    let data = pattern(4096);
    let mtime = stub.add_file("/probe.bin", &data);
    stub.add_dir("/adir");

    let entry = driver.stat(&rel("probe.bin")).await.expect("stat file");
    assert_eq!(entry.size, 4096);
    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.mtime, mtime as f64, "mtime 来自服务端 attrs（秒）");

    let dir_entry = driver.stat(&rel("adir")).await.expect("stat dir");
    assert_eq!(dir_entry.kind, EntryKind::Dir);

    let root = driver.stat(&RelPath::root()).await.expect("stat root");
    assert_eq!(root.kind, EntryKind::Dir);
}

/// missing → NotFound（SSH_FX_NO_SUCH_FILE 映射——绝不 Io/Invalid）。
#[tokio::test]
async fn stat_missing_yields_not_found() {
    let (_stub, driver) = setup().await;
    assert_eq!(
        driver.stat(&rel("nope.bin")).await.err(),
        Some(StorageError::NotFound)
    );
}

// ------------------------------------------------------------- reader ---

/// Range 跨 64KiB 帧边界：offset 生效 + 窗口精确（driver READ_FRAME =
/// 64KiB；[65000, 130008) 跨两帧边界，桩 read 精确窗口切片）。
#[tokio::test]
async fn reader_range_crosses_frame_boundary_byte_exact() {
    let (stub, driver) = setup().await;
    let data = pattern(200_000); // > 3 帧
    stub.add_file("/big.bin", &data);

    let start = 65_000u64;
    let end = 130_008u64; // 跨 64KiB 与 128KiB 两个帧边界
    let range = Range::new(start, Some(end)).expect("range");
    let got = read_all(
        driver
            .reader(&file_id(&driver, "big.bin"), Some(range))
            .await
            .expect("reader"),
    )
    .await
    .expect("read");
    assert_eq!(got.len() as u64, end - start, "window only");
    assert_eq!(
        got,
        data[start as usize..end as usize],
        "byte exact across frames"
    );
}

/// 越界钳制：end 越过 EOF → 只读到最后一个字节（clamped_len 语义）。
#[tokio::test]
async fn reader_clamps_end_beyond_eof() {
    let (stub, driver) = setup().await;
    let data = pattern(10_000);
    stub.add_file("/small.bin", &data);

    let range = Range::new(9_000, Some(100_000)).expect("range");
    let got = read_all(
        driver
            .reader(&file_id(&driver, "small.bin"), Some(range))
            .await
            .expect("reader"),
    )
    .await
    .expect("read");
    assert_eq!(got, data[9_000..], "clamped to EOF");
}

/// start >= size → 空流（非错误；且不开远程句柄——count 恒 0）。
#[tokio::test]
async fn reader_start_beyond_size_yields_empty_stream_without_open() {
    let (stub, driver) = setup().await;
    stub.add_file("/tiny.bin", b"abc");
    let range = Range::new(3, Some(10)).expect("range");
    let got = read_all(
        driver
            .reader(&file_id(&driver, "tiny.bin"), Some(range))
            .await
            .expect("empty window is a stream, not an error"),
    )
    .await
    .expect("read");
    assert!(got.is_empty());
    // 三硬仗① 的最好履行是不打开：空窗口不产生任何服务端句柄
    assert_eq!(stub.open_handle_count(), 0);
}

/// 全量读（range=None）：整文件字节等（多帧 64KiB 流 + 尾帧非整帧）。
#[tokio::test]
async fn reader_full_file_roundtrip() {
    let (stub, driver) = setup().await;
    let data = pattern(150_000); // 2×64KiB + 21_928 尾帧
    stub.add_file("/whole.bin", &data);
    let got = read_all(
        driver
            .reader(&file_id(&driver, "whole.bin"), None)
            .await
            .expect("reader"),
    )
    .await
    .expect("read");
    assert_eq!(got, data);
}

// ------------------------------------------------------- 错误往返面 ---

/// 对不存在路径的三个读面：stat / list / reader → NotFound / NotFound+Invalid
/// 组合的既定分类（list 对 missing 目录 stat 预检先给 NotFound）。
#[tokio::test]
async fn missing_paths_classify_as_not_found() {
    let (_stub, driver) = setup().await;
    // stat missing → NotFound
    assert_eq!(
        driver.stat(&rel("gone.bin")).await.err(),
        Some(StorageError::NotFound)
    );
    // list missing 目录 → NotFound（stat 预检）
    assert_eq!(
        driver.list(&rel("gone-dir"), Page::all()).await.err(),
        Some(StorageError::NotFound)
    );
    // reader missing → NotFound（stat 预检在 open 之前）
    let range = Range::new(0, Some(10)).expect("range");
    assert_eq!(
        driver
            .reader(&file_id(&driver, "gone.bin"), Some(range))
            .await
            .err(),
        Some(StorageError::NotFound)
    );
}

/// list 作用于文件路径 → Invalid（is_dir 预检）；reader 作用于目录 → Invalid。
#[tokio::test]
async fn wrong_kind_paths_are_invalid() {
    let (stub, driver) = setup().await;
    stub.add_file("/plain.txt", b"x");
    stub.add_dir("/folder");
    assert_eq!(
        driver.list(&rel("plain.txt"), Page::all()).await.err(),
        Some(StorageError::Invalid),
        "list on a file path"
    );
    assert_eq!(
        driver.reader(&file_id(&driver, "folder"), None).await.err(),
        Some(StorageError::Invalid),
        "reader on a dir handle"
    );
}

/// 他卷句柄 → NotFound（trait 契约；reader 面）。
#[tokio::test]
async fn reader_foreign_volume_handle_yields_not_found() {
    let (_stub, driver) = setup().await;
    let foreign = EntryId::new(
        cloudkit_storage::VolumeId::new("sftp", "other@host:22").expect("volume"),
        BackendHandle::new("a.txt"),
    );
    assert_eq!(
        driver.reader(&foreign, None).await.err(),
        Some(StorageError::NotFound)
    );
}
