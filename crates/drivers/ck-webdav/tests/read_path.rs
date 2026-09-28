//! WD2b 读路径行为测试（手搓桩消费——WD2a `stub/mod.rs`「消费手册」）。
//!
//! 覆盖对照（计划 §4.7 读路径 / §4.4 映射表 / §4.5 负面清单 / 矩阵）：
//!
//! | 用例 | 依据 |
//! |---|---|
//! | stat 文件/目录/404 | §4.7 |
//! | 目录 getcontentlength 内层 404 → size 0（毒值 999 不外漏） | 矩阵⑧ |
//! | list 双 ns 风格产出一致 | 矩阵⑧（local-name 解析） |
//! | list 剔 self（容忍尾斜杠差异） | §4.7 |
//! | K67 不可寻址名过滤 | §4.5-12 |
//! | `.ckwd-` 暂存件过滤 | §4.6/断言③ |
//! | Page 切片稳定（两次遍历一致） | §4.7（驱动内全量切片） |
//! | reader 全量/跨窗（8 MiB）/Range 钳制 | §4.7 |
//! | start ≥ size 空流不开 GET | §4.7 |
//! | 200 截断回退（range_ignore 旋钮） | 矩阵④/§4.5-6 |
//! | 416 → 复核 EOF 空收尾（并发收缩形态，WD3 经正式读面） | §4.4 |
//! | 416 复核增长半边 → Unavailable / 复核 stat 出错臂如实上抛 | M10 |
//! | 206 校验负路径三条（lie / short / 200 不足窗 → Io） | M12 |
//! | 403 → Unauthorized{false}（stat/list 双面） | M11 |
//! | 429 耗尽重试预算 → Unavailable（记录器对账 1+3） | M13 |
//! | Range 头逐窗核对（记录器 range 字段） | M9 |
//! | malformed multistatus → Io 带截断片段 | §4.4/§6 风险表 |
//! | unexpected 301 → Unavailable + Location | §4.4 |
//! | 慢滴流不卡死（窗口超时内完成） | §4.5-7 |
//! | kill_connections 白名单自愈 | §4.5-10 |
//! | quota → None 降级 | 矩阵⑪ |
//! | transport open/open_range | K2/E-5 |

mod stub;

use std::sync::Arc;

use cloudkit_storage::transport::{CloudTransport, RemoteHandle};
use cloudkit_storage::{
    ByteStream, EntryKind, Page, PageCursor, Range, RelPath, StorageDriver, StorageError,
};
use futures_util::StreamExt;
use stub::{spawn_stub, AuthMode, Knobs, StubHandle, StubStyle, Vfs, MTIME_SEED};

use ck_webdav::{parse_from_map, WebdavDriver, WebdavParams, WebdavTransport};

// ------------------------------------------------------------ 测试工具 ---

fn params(handle: &StubHandle) -> WebdavParams {
    let mut map = std::collections::HashMap::new();
    map.insert("webdav_url".to_string(), handle.url.clone());
    map.insert("webdav_username".to_string(), "spike".to_string());
    map.insert("webdav_password".to_string(), "pw".to_string());
    parse_from_map(&map).expect("test params parse")
}

fn driver(handle: &StubHandle) -> WebdavDriver {
    WebdavDriver::new(params(handle)).expect("driver constructs")
}

fn rel(path: &str) -> RelPath {
    RelPath::new(path).expect("rel path")
}

/// 逐字节收集一条 ByteStream（错误帧即时上抛——「错误可在流中途浮现」
/// 契约的消费面）。
async fn collect(stream: ByteStream) -> Result<Vec<u8>, StorageError> {
    let mut stream = stream;
    let mut out = Vec::new();
    while let Some(frame) = stream.next().await {
        out.extend_from_slice(&frame?);
    }
    Ok(out)
}

/// 确定性测试模式（i 字节 = (i % 251) as u8——跨窗比对零碰撞面）。
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

async fn plain(vfs: Vfs) -> StubHandle {
    spawn_stub(vfs, AuthMode::None, Knobs::default(), StubStyle::rclone()).await
}

fn gets(handle: &StubHandle) -> usize {
    handle
        .requests()
        .iter()
        .filter(|request| request.method == "GET")
        .count()
}

fn propfinds(handle: &StubHandle) -> usize {
    handle
        .requests()
        .iter()
        .filter(|request| request.method == "PROPFIND")
        .count()
}

/// GET 请求的 Range 头序列（M9 观测面）：每个窗口 GET 都必须携带
/// `bytes=a-b` 字面——驱动丢 Range 头则服务器回 200 全量、200 回退切
/// 出同字节，全绿假象的防线（stub 记录器的 `range` 字段消费面）。
fn get_ranges(handle: &StubHandle) -> Vec<String> {
    handle
        .requests()
        .iter()
        .filter(|request| request.method == "GET")
        .map(|request| {
            request
                .range
                .clone()
                .expect("every window GET carries a Range header")
        })
        .collect()
}

// -------------------------------------------------------------- stat ---

/// §4.7：stat 文件（size/mtime）/目录（kind=Dir）/不存在 → NotFound。
#[tokio::test]
async fn stat_file_dir_and_missing() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    vfs.seed_file("/docs/f.bin", b"hello");
    let handle = plain(vfs).await;
    let driver = driver(&handle);

    let file = driver.stat(&rel("docs/f.bin")).await.expect("file stat");
    assert_eq!(file.kind, EntryKind::File);
    assert_eq!(file.size, 5);
    assert_eq!(
        file.mtime, MTIME_SEED as f64,
        "IMF-fixdate mtime round-trips"
    );
    assert_eq!(file.path, rel("docs/f.bin"));
    assert_eq!(file.id.handle.as_str(), "docs/f.bin", "path-shaped handle");

    let dir = driver.stat(&rel("docs")).await.expect("dir stat");
    assert_eq!(dir.kind, EntryKind::Dir);

    let root = driver.stat(&RelPath::root()).await.expect("root stat");
    assert_eq!(root.kind, EntryKind::Dir);

    let error = driver.stat(&rel("nope.txt")).await.expect_err("missing");
    assert!(matches!(error, StorageError::NotFound), "{error:?}");
}

/// 矩阵⑧：目录的 getcontentlength 在内层 404 propstat（占位毒值 999）
/// ——投影按 propstat 状态过滤，size 落 0（999 不外漏）。apache
/// SlashStrict 下目录 stat 走 301→尾斜杠重试腿（附录 C ⑧ 集合 no-slash
/// 301 规避）。
#[tokio::test]
async fn stat_dir_size_defaults_to_zero_and_survives_slash_strict_redirect() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    vfs.seed_file("/docs/f.bin", b"xy");
    let handle = spawn_stub(vfs, AuthMode::None, Knobs::default(), StubStyle::apache()).await;
    let driver = driver(&handle);

    let dir = driver
        .stat(&rel("docs"))
        .await
        .expect("dir stat through the 301 retry");
    assert_eq!(dir.kind, EntryKind::Dir);
    assert_eq!(dir.size, 0, "the poisoned 404-block value must not leak");

    let file = driver.stat(&rel("docs/f.bin")).await.expect("file stat");
    assert_eq!(file.size, 2);
}

// -------------------------------------------------------------- list ---

/// 矩阵⑧：rclone（Classic `D:` 前缀）与 apache（ApacheStyle 多前缀并存
/// + lp2 私有 ns + 404 块在前）两形态产出**一致**的 Entry 集（路径/形态/
/// 尺寸/mtime）——local-name 解析纪律的行为面。
#[tokio::test]
async fn list_agrees_across_both_ns_styles() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    vfs.seed_dir("/docs/sub");
    vfs.seed_file("/docs/a&b.txt", b"x");
    vfs.seed_file("/docs/my file.txt", b"hello");
    vfs.seed_file("/docs/ü.txt", b"z");
    vfs.seed_file("/side.txt", b"!");

    let mut rclone_vfs = vfs.clone();
    rclone_vfs.seed_file("/docs/.ckwd-4242-7.part", b"staging");
    let rclone = plain(rclone_vfs).await;
    let apache = spawn_stub(vfs, AuthMode::None, Knobs::default(), StubStyle::apache()).await;

    let shape = |listing: cloudkit_storage::Listing| {
        listing
            .entries
            .into_iter()
            .map(|e| (e.path, e.kind, e.size, e.mtime))
            .collect::<Vec<_>>()
    };
    let from_rclone = shape(
        driver(&rclone)
            .list(&rel("docs"), Page::all())
            .await
            .expect("rclone list"),
    );
    let from_apache = shape(
        driver(&apache)
            .list(&rel("docs"), Page::all())
            .await
            .expect("apache list"),
    );
    assert_eq!(from_rclone, from_apache);

    // 期望集：名字字典序 + href 解码形态（%20/&amp;/%C3%BC 全还原）+
    // self 剔除（尾斜杠差异容忍）+ `.ckwd-` 暂存件过滤（rclone 侧独有）。
    let dir = EntryKind::Dir;
    let file = EntryKind::File;
    assert_eq!(
        from_rclone,
        vec![
            (rel("docs/a&b.txt"), file, 1, MTIME_SEED as f64),
            (rel("docs/my file.txt"), file, 5, MTIME_SEED as f64),
            (rel("docs/sub"), dir, 0, MTIME_SEED as f64),
            (rel("docs/ü.txt"), file, 1, MTIME_SEED as f64),
        ]
    );

    // 根列表（slash 形态 + 根 href 自条目剔除）。
    let root = shape(
        driver(&rclone)
            .list(&RelPath::root(), Page::all())
            .await
            .expect("root list"),
    );
    assert_eq!(root.len(), 2, "{root:?}");
    assert_eq!(root[0].0, rel("docs"));
    assert_eq!(root[1].0, rel("side.txt"));
}

/// §4.5-12（K67）：不可寻址名（`\`/`\0`/lossy U+FFFD）不出现在 list 产出
/// ——「list 产出即可寻址」。
#[tokio::test]
async fn list_filters_unaddressable_names() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    vfs.seed_file("/docs/good.txt", b"ok");
    vfs.seed_file("/docs/back\\slash.txt", b"x");
    vfs.seed_file("/docs/nul\0byte.txt", b"x");
    vfs.seed_file("/docs/mojibake\u{FFFD}.txt", b"x");
    let handle = plain(vfs).await;
    let listing = driver(&handle)
        .list(&rel("docs"), Page::all())
        .await
        .expect("list");
    let names: Vec<String> = listing
        .entries
        .iter()
        .map(|e| e.path.file_name().expect("name").to_string())
        .collect();
    assert_eq!(names, vec!["good.txt".to_string()], "{names:?}");
}

/// §4.6/断言③：`.ckwd-<pid>-<seq>.part|.old` 暂存件是驱动实现细节，
/// 不进 list 集合（用户同名合法文件不误伤——双条件）。
#[tokio::test]
async fn list_filters_ckwd_staging_artifacts_only() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    vfs.seed_file("/docs/report.ckwd-4242-7.part", b"staging");
    vfs.seed_file("/docs/report.ckwd-4242-7.old", b"stash");
    // 双条件防误伤：`.ckwd-` 但非 part/old 结尾 = 用户合法文件，必须可见。
    vfs.seed_file("/docs/notes.ckwd-diary.txt", b"legit");
    vfs.seed_file("/docs/keep.part", b"also legit");
    let handle = plain(vfs).await;
    let listing = driver(&handle)
        .list(&rel("docs"), Page::all())
        .await
        .expect("list");
    let names: Vec<String> = listing
        .entries
        .iter()
        .map(|e| e.path.file_name().expect("name").to_string())
        .collect();
    assert_eq!(
        names,
        vec!["keep.part".to_string(), "notes.ckwd-diary.txt".to_string()],
        "{names:?}"
    );
}

/// §4.7：Page 在驱动内全量切片（PROPFIND 无分页原语）——limit/游标语
/// 义 + 两次全遍历产出逐字一致（稳定排序）。
#[tokio::test]
async fn list_page_slicing_is_stable_across_traversals() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    for name in ["b.txt", "a.txt", "c.txt", "d.txt"] {
        vfs.seed_file(&format!("/docs/{name}"), name.as_bytes());
    }
    let handle = plain(vfs).await;
    let driver = driver(&handle);

    let names_of = |page: Page| {
        let driver = &driver;
        async move {
            let listing = driver.list(&rel("docs"), page).await.expect("page");
            let names: Vec<String> = listing
                .entries
                .iter()
                .map(|e| e.path.file_name().expect("name").to_string())
                .collect();
            (names, listing.next)
        }
    };

    let (first, next) = names_of(Page {
        limit: 2,
        cursor: PageCursor::Start,
    })
    .await;
    assert_eq!(first, vec!["a.txt", "b.txt"]);
    let token = next.expect("more pages remain");

    let (second, next2) = names_of(Page {
        limit: 2,
        cursor: token,
    })
    .await;
    assert_eq!(second, vec!["c.txt", "d.txt"]);
    // 4 条 / limit 2 = 两页恰好穷尽（next 只在还有余量时回吐）。
    assert_eq!(next2, None, "exhausted exactly at the boundary");

    // 两次全遍历（limit 3）逐字一致。
    let once = names_of(Page {
        limit: 3,
        cursor: PageCursor::Start,
    })
    .await;
    let twice = names_of(Page {
        limit: 3,
        cursor: PageCursor::Start,
    })
    .await;
    assert_eq!(once.0, twice.0);
    assert_eq!(once.1, twice.1);

    // 不存在的目录 → NotFound；指向文件 → Invalid（trait 契约）。
    let error = driver
        .list(&rel("nope"), Page::all())
        .await
        .expect_err("missing dir");
    assert!(matches!(error, StorageError::NotFound), "{error:?}");
    let error = driver
        .list(&rel("docs/a.txt"), Page::all())
        .await
        .expect_err("file target");
    assert!(matches!(error, StorageError::Invalid), "{error:?}");
}

// ------------------------------------------------------------ reader ---

/// §4.7：reader 全量读——字节逐字一致。
#[tokio::test]
async fn reader_streams_full_file_byte_exact() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", &pattern(1000));
    let handle = plain(vfs).await;
    let driver = driver(&handle);
    let entry = driver.stat(&rel("f.bin")).await.expect("stat");
    let bytes = collect(driver.reader(&entry.id, None).await.expect("reader"))
        .await
        .expect("bytes");
    assert_eq!(bytes, pattern(1000));
    assert_eq!(gets(&handle), 1, "single window for a small file");
    // M9：唯一的窗口 GET 携带逐字 Range 头。
    assert_eq!(get_ranges(&handle), vec!["bytes=0-999".to_string()]);
}

/// §4.7：9 MiB 跨 8 MiB 窗口边界——两窗串行拼接逐字一致（记录器恰 2 个
/// GET）；Range 半开 + 越界钳制（end=10^6 钳到 EOF）。
#[tokio::test]
async fn reader_crosses_the_window_boundary_byte_exact() {
    let window: usize = 8 * 1024 * 1024;
    let total = window + 1024 * 1024; // 9 MiB
    let mut vfs = Vfs::new();
    vfs.seed_file("/big.bin", &pattern(total));
    let handle = plain(vfs).await;
    let driver = driver(&handle);
    let entry = driver.stat(&rel("big.bin")).await.expect("stat");
    assert_eq!(entry.size, total as u64);

    let bytes = collect(driver.reader(&entry.id, None).await.expect("reader"))
        .await
        .expect("bytes");
    assert_eq!(bytes.len(), total);
    assert_eq!(bytes, pattern(total));
    assert_eq!(gets(&handle), 2, "two windows: [0,8MiB) + [8MiB,9MiB)");
    // M9：逐窗核对 Range 头（半开区间 → 闭区间字面）——两个窗口请求
    // 各自携带自己的 `bytes=a-b`。
    assert_eq!(
        get_ranges(&handle),
        vec![
            "bytes=0-8388607".to_string(),
            "bytes=8388608-9437183".to_string(),
        ]
    );

    // 中段窗口 + 开放区间（钳制到 EOF）。
    let mid = Range {
        start: window as u64 / 2,
        end: Some((total + 5_000_000) as u64),
    };
    let bytes = collect(
        driver
            .reader(&entry.id, Some(mid))
            .await
            .expect("mid reader"),
    )
    .await
    .expect("mid bytes");
    let expect_start = window / 2;
    assert_eq!(bytes, pattern(total)[expect_start..], "clamped to EOF");
    // M9：第三个窗口 GET 的 Range 头同样逐字（end 钳到 EOF-1）。
    assert_eq!(get_ranges(&handle)[2], "bytes=4194304-9437183".to_string());
}

/// §4.7：start ≥ size → 空流不开 GET（记录器零 GET）；空文件同理。
#[tokio::test]
async fn reader_start_beyond_size_is_an_empty_stream_without_a_get() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", b"0123456789");
    vfs.seed_file("/empty.bin", b"");
    let handle = plain(vfs).await;
    let driver = driver(&handle);

    let entry = driver.stat(&rel("f.bin")).await.expect("stat");
    let bytes = collect(
        driver
            .reader(&entry.id, Some(Range::from_start(10)))
            .await
            .expect("beyond-EOF reader"),
    )
    .await
    .expect("empty");
    assert!(bytes.is_empty());

    let empty = driver.stat(&rel("empty.bin")).await.expect("stat");
    let bytes = collect(driver.reader(&empty.id, None).await.expect("empty reader"))
        .await
        .expect("empty");
    assert!(bytes.is_empty());

    assert_eq!(gets(&handle), 0, "no GET ever fired");
    // 4 个 PROPFIND = 测试侧 2 次 stat + reader 内部 2 次 stat 先行
    //（stat-first 的可见足迹）。
    assert_eq!(propfinds(&handle), 4, "stat-first only");
}

/// 矩阵④/§4.5-6：服务器无视 Range 回 200 全量——body 覆盖请求区间 →
/// 截断继续（range_ignore 旋钮；apache 倒序真形的强化回放）。
#[tokio::test]
async fn reader_falls_back_to_200_truncation_when_range_is_ignored() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", &pattern(300));
    let style = StubStyle {
        range_ignore: true,
        ..StubStyle::rclone()
    };
    let handle = spawn_stub(vfs, AuthMode::None, Knobs::default(), style).await;
    let driver = driver(&handle);
    let entry = driver.stat(&rel("f.bin")).await.expect("stat");

    let bytes = collect(
        driver
            .reader(&entry.id, Some(Range::new(10, Some(60)).expect("range")))
            .await
            .expect("reader"),
    )
    .await
    .expect("truncated bytes");
    assert_eq!(
        bytes,
        pattern(300)[10..60],
        "the 200 body is sliced to the window"
    );
    // M9：即便回退腿，请求侧的 Range 头也必须逐字发出（丢头即无回退
    // 可言——本测试的前提）。
    assert_eq!(get_ranges(&handle), vec!["bytes=10-59".to_string()]);
}

/// M3（审查修复批）：200-回退读面**封顶**——服务器无视 Range 回 200
/// 全量时，客户端至多读到窗口终点（`end` 字节），绝不把整个 body 读进
/// 内存（apache 倒序 200 真形 × 大文件 = OOM 面）。桩的 `served_bytes`
/// 计数流是客户端读取量的可观测代理；慢滴限速让服务端自限（loopback
/// 缓冲不放大），封顶后 ≈ 窗口终点、整读缺陷 = 全量 12 MiB。
#[tokio::test]
async fn range_ignored_200_body_is_read_only_up_to_the_window_end() {
    let total = 12 * 1024 * 1024;
    let window_end = 2 * 1024 * 1024;
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", &pattern(total));
    let style = StubStyle {
        range_ignore: true,
        ..StubStyle::rclone()
    };
    let knobs = Knobs {
        // 限速（1ms/4KiB 块）把服务端交付率钉在客户端读取率附近——
        // in-flight 缓冲不再掩盖读取量差（全量整读 ≈ 3s，封顶 ≈ 0.5s）。
        slow_drip: Some(std::time::Duration::from_millis(1)),
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, style).await;
    let driver = driver(&handle);
    let entry = driver.stat(&rel("f.bin")).await.expect("stat");

    let bytes = collect(
        driver
            .reader(
                &entry.id,
                Some(Range::new(0, Some(window_end as u64)).expect("range")),
            )
            .await
            .expect("reader"),
    )
    .await
    .expect("window bytes");
    assert_eq!(
        bytes,
        pattern(total)[..window_end],
        "the 200 body is sliced to the window"
    );
    let served = handle
        .knobs
        .served_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        served <= window_end + 2 * 1024 * 1024,
        "the client must stop reading at the window end: served {served} bytes of a \
         {total}-byte body for a {window_end}-byte window"
    );
}

/// §4.4：416 → `Ok(None)` passthrough 的驱动面决策腿（reader 的 stat
/// 复核）——并发收缩形态：reader() 的 stat 先行拿到旧尺寸，首窗 GET
/// 前文件被替换为更短版本 → 416 → 复核 stat 见 offset ≥ 新尺寸 →
/// EOF 空收尾（不报错）。原 WD2 seam 直测（`test_get_range`）已随
/// WD3 写面落地移除。复核三分支的覆盖分工（M10 修正：本注释此前声称
/// 「正式读面覆盖同一契约」过强——实际只盖 EOF 半边）：EOF 半边 =
/// 本测试；增长半边（offset < 新 size → `Unavailable`）=
/// `window_416_with_the_file_grown_is_unavailable`；复核 stat 出错臂
/// （Err 如实上抛不吞）= `window_416_recheck_stat_error_surfaces_unchanged`。
#[tokio::test]
async fn beyond_eof_window_after_shrink_ends_the_stream_at_eof() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", &pattern(300));
    let handle = plain(vfs).await;
    let driver = driver(&handle);
    let entry = driver.stat(&rel("f.bin")).await.expect("stat");

    let stream = driver
        .reader(&entry.id, Some(Range::new(250, None).expect("range")))
        .await
        .expect("reader built on the pre-shrink size");
    // reader() 的 stat 已完成——首窗 GET 前替换为 20 字节版本（确定性
    // 注入：窗口 GET 只在首次 poll 时发出）。
    handle.seed_file("/f.bin", &pattern(20));
    let bytes = collect(stream).await.expect("416 recheck ends at EOF");
    assert!(bytes.is_empty());
    assert_eq!(gets(&handle), 1, "exactly one window GET (the 416)");
    // M9：即便 416 腿，Range 头也逐字（[250,300) → bytes=250-299）。
    assert_eq!(get_ranges(&handle), vec!["bytes=250-299".to_string()]);
}

/// M10①：416 复核的增长半边——offset < 新 size（并发写入把文件增长）
/// → `Unavailable`（形态异常如实命名，绝不伪装成 EOF 空流继续）。注入：
/// `range_416_once`（一次性）打 416 + 测试侧种入更大的新版本（确定性
/// 注入，收缩腿同款）——复核 stat 经真实 PROPFIND 拿到增长后真值。
#[tokio::test]
async fn window_416_with_the_file_grown_is_unavailable() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", &pattern(300));
    let knobs = Knobs {
        range_416_once: true,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let driver = driver(&handle);
    let entry = driver.stat(&rel("f.bin")).await.expect("stat");

    let stream = driver
        .reader(&entry.id, Some(Range::new(250, None).expect("range")))
        .await
        .expect("reader built on the pre-grow size");
    // 首窗 GET 前并发写入把文件增长到 400 字节。
    handle.seed_file("/f.bin", &pattern(400));
    let error = collect(stream)
        .await
        .expect_err("416 recheck must see the growth");
    match error {
        StorageError::Unavailable(message) => {
            assert!(message.contains("416"), "{message}");
            assert!(
                message.contains("250"),
                "the offending offset rides along: {message}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

/// M10②：416 复核的 stat 出错臂——Err 如实上抛不吞（吞成空流 = 把
/// 「复核失败」伪装成「并发收缩到 EOF」的数据谎言）。注入：复核 stat
/// 的 PROPFIND 连杀 4 次（初始 + 3 次白名单重试全灭）→ 传输错误原样
/// 浮现为流内 `Err` 帧。
#[tokio::test]
async fn window_416_recheck_stat_error_surfaces_unchanged() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", &pattern(300));
    let knobs = Knobs {
        range_416_once: true,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let driver = driver(&handle);
    let entry = driver.stat(&rel("f.bin")).await.expect("stat");
    let stream = driver
        .reader(&entry.id, Some(Range::new(250, None).expect("range")))
        .await
        .expect("reader built while the file is intact");
    // reader() 的 stat 已完成——现在把复核腿的 PROPFIND 全灭（4 次 =
    // 初始尝试 + MAX_RETRIES 3；重试白名单在 execute 面逐发退避）。
    handle
        .knobs
        .kill_propfinds
        .store(4, std::sync::atomic::Ordering::SeqCst);
    let error = collect(stream)
        .await
        .expect_err("the recheck failure must surface, not fake an EOF");
    assert!(
        matches!(error, StorageError::Unavailable(_)),
        "the transport error passes through unchanged: {error:?}"
    );
    // 确定性对账：2 次先行 stat（测试侧 + reader 内部）+ 复核 4 连杀。
    assert_eq!(
        propfinds(&handle),
        6,
        "the recheck burned the full retry budget"
    );
}

/// 复审 M5（2026-09-25）：200-回退在**高偏移窗口**下的输出等价护栏
/// ——流式丢弃前缀实现（`read_200_window`）与旧的整读切片必须逐字同
/// 出：窗口 `[8MiB,10MiB)` 落在 12MiB body 中段，前缀丢弃/跨块切片的
/// 数学若有偏差即在此暴露（资源缺陷本身——内存峰位——不在单测可观测
/// 面，由 seam 单测 + 实现结构保证，见 client.rs `read_200_window`）。
#[tokio::test]
async fn range_ignored_200_high_offset_window_is_sliced_byte_exact() {
    let total = 12 * 1024 * 1024;
    let start = 8 * 1024 * 1024;
    let window = 2 * 1024 * 1024;
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", &pattern(total));
    let style = StubStyle {
        range_ignore: true,
        ..StubStyle::rclone()
    };
    let handle = spawn_stub(vfs, AuthMode::None, Knobs::default(), style).await;
    let driver = driver(&handle);
    let entry = driver.stat(&rel("f.bin")).await.expect("stat");

    let bytes = collect(
        driver
            .reader(
                &entry.id,
                Some(Range::new(start, Some(start + window)).expect("range")),
            )
            .await
            .expect("reader"),
    )
    .await
    .expect("high-offset window bytes");
    assert_eq!(
        bytes,
        pattern(total)[start as usize..(start + window) as usize],
        "the 200 body is sliced to the high-offset window byte-exactly"
    );
    handle.shutdown().await;
}

/// §4.4：malformed multistatus（截断无闭合）→ `Io` 带截断片段（≤200 字
/// 节、脱敏）——不崩不静默。
#[tokio::test]
async fn malformed_multistatus_is_io_with_a_snippet() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    let knobs = Knobs {
        malformed_multistatus: true,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let driver = driver(&handle);
    let error = driver.stat(&rel("docs")).await.expect_err("malformed");
    match error {
        StorageError::Io(message) => {
            assert!(message.contains("truncated"), "{message}");
            assert!(
                message.contains("multistatus"),
                "the body snippet rides along: {message}"
            );
        }
        other => panic!("expected Io, got {other:?}"),
    }
}

/// §4.4：意外 3xx → `Unavailable` + Location 提示（不自动跟随——文件收到
/// 301 本身就是意外重定向）。
#[tokio::test]
async fn unexpected_redirect_is_unavailable_with_a_location_hint() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.txt", b"x");
    let knobs = Knobs {
        unexpected_301: true,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let driver = driver(&handle);
    let error = driver
        .stat(&rel("f.txt"))
        .await
        .expect_err("unexpected 301");
    match error {
        StorageError::Unavailable(message) => {
            assert!(
                message.contains("/f.txt/"),
                "Location rides along: {message}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

/// §4.5-7：慢滴流（分块延迟）在窗口预算（120s）内完成、字节不丢。
#[tokio::test]
async fn slow_drip_completes_within_the_window_budget() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/drip.bin", &pattern(20_000)); // 5 块 × 4 KiB
    let knobs = Knobs {
        slow_drip: Some(std::time::Duration::from_millis(10)),
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let driver = driver(&handle);
    let entry = driver.stat(&rel("drip.bin")).await.expect("stat");
    let bytes = collect(driver.reader(&entry.id, None).await.expect("reader"))
        .await
        .expect("bytes");
    assert_eq!(bytes, pattern(20_000), "drip delays but never drops bytes");
}

/// §4.5-10：连接杀（响应头后 body 即断）——白名单内（PROPFIND/GET）单
/// 请求自愈，读路径整体成功。
#[tokio::test]
async fn killed_connections_self_heal_within_the_read_path() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", &pattern(500));
    let knobs = Knobs {
        kill_connections: std::sync::atomic::AtomicUsize::new(2),
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let driver = driver(&handle);
    let entry = driver.stat(&rel("f.bin")).await.expect("stat");
    let bytes = collect(driver.reader(&entry.id, None).await.expect("reader"))
        .await
        .expect("bytes");
    assert_eq!(bytes, pattern(500));
    let total = handle.requests().len();
    assert!(
        total >= 3,
        "the two kills were retried away: {total} requests"
    );
}

/// M7（审查修复批）：207 内层**成员失败**不再静默消失——list 对失败
/// 子成员（仅内层 500 块）回 `Unavailable` 带码并点名成员，而非把它悄
/// 悄剔出列表（列表可以不全但必须响亮；内层 404 子成员仍按缺席处理
/// ——成员确已不在，是唯一允许消失的形态）。注入：`member_500_once`
/// 旋钮打在首个子成员（名字序确定）。
#[tokio::test]
async fn list_reports_a_failed_member_as_unavailable() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    vfs.seed_file("/docs/a.txt", b"x");
    vfs.seed_file("/docs/b.txt", b"y");
    let knobs = Knobs {
        member_500_once: true,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let driver = driver(&handle);

    let error = driver
        .list(&rel("docs"), Page::all())
        .await
        .expect_err("a failed member must surface, not vanish");
    match error {
        StorageError::Unavailable(message) => {
            assert!(
                message.contains("500"),
                "the inner status rides along: {message}"
            );
            assert!(
                message.contains("a.txt"),
                "the failed member is named: {message}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

/// M7：stat 的单成员 PROPFIND（Depth 0）遇「仅内层 500」的自成员 →
/// `Unavailable` 带码——真类（映射成 NotFound 会误导「路径不存在」的
/// 排查方向）；内层 404 成员仍归 NotFound（既有语义，xml 面钉死）。
#[tokio::test]
async fn stat_maps_a_failed_member_to_unavailable_not_notfound() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/docs/a.txt", b"x");
    let knobs = Knobs {
        member_500_once: true,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let driver = driver(&handle);

    let error = driver
        .stat(&rel("docs/a.txt"))
        .await
        .expect_err("a failed member must surface as its true class");
    match error {
        StorageError::Unavailable(message) => {
            assert!(message.contains("500"), "{message}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

/// M11：403（NAS 权限真形——apache GET 集合 403 的同族）→
/// `Unauthorized{recoverable:false}`，stat 与 list 双面。403 不在重试
/// 白名单——两面各恰消耗旋钮计数 1。
#[tokio::test]
async fn forbidden_propfind_maps_to_unauthorized_on_stat_and_list() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    vfs.seed_file("/docs/a.txt", b"x");
    let knobs = Knobs {
        forbidden_propfinds: 2,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let driver = driver(&handle);

    let error = driver.stat(&rel("docs/a.txt")).await.expect_err("403");
    assert!(
        matches!(error, StorageError::Unauthorized { recoverable: false }),
        "403 is a credential-class rejection: {error:?}"
    );
    let error = driver
        .list(&rel("docs"), Page::all())
        .await
        .expect_err("403");
    assert!(
        matches!(error, StorageError::Unauthorized { recoverable: false }),
        "{error:?}"
    );
    // 恰消耗：PROPFIND 各 1 次（403 不触发任何重试）。
    assert_eq!(propfinds(&handle), 2);
}

// ---------------------------------------------- 206 校验负路径（M12）---

/// M12①：206 但 Content-Range 与请求不符（服务器撒谎）→ `Io` 带两侧
/// 值（§4.5-6「无 206 校验」缺陷的正面修法本体）。注入：
/// `range_206_lie`（头平移 +1，body 仍是请求区间真字节）。
#[tokio::test]
async fn a_lying_206_content_range_is_an_io_error_with_both_sides() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", &pattern(300));
    let knobs = Knobs {
        range_206_lie: true,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let driver = driver(&handle);
    let entry = driver.stat(&rel("f.bin")).await.expect("stat");

    let error = collect(
        driver
            .reader(&entry.id, Some(Range::new(10, Some(60)).expect("range")))
            .await
            .expect("reader"),
    )
    .await
    .expect_err("the lying header must fail the 206 validation");
    match error {
        StorageError::Io(message) => {
            assert!(message.contains("mismatch"), "{message}");
            assert!(
                message.contains("10-59"),
                "the requested window rides along: {message}"
            );
            assert!(
                message.contains("11-60"),
                "the lying header rides along: {message}"
            );
        }
        other => panic!("expected Io, got {other:?}"),
    }
}

/// M12②：206 头正确但 body 短于窗口 → `Io` 带两侧值（实测长度 vs 窗
/// 口）。注入：`range_206_short`（body 短一字节）。
#[tokio::test]
async fn a_short_206_body_is_an_io_error_with_both_sides() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", &pattern(300));
    let knobs = Knobs {
        range_206_short: true,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let driver = driver(&handle);
    let entry = driver.stat(&rel("f.bin")).await.expect("stat");

    let error = collect(
        driver
            .reader(&entry.id, Some(Range::new(10, Some(60)).expect("range")))
            .await
            .expect("reader"),
    )
    .await
    .expect_err("the short body must fail the 206 validation");
    match error {
        StorageError::Io(message) => {
            assert!(message.contains("disagrees"), "{message}");
            assert!(
                message.contains("49"),
                "the actual body length rides along: {message}"
            );
            assert!(
                message.contains("[10,60)"),
                "the requested window rides along: {message}"
            );
        }
        other => panic!("expected Io, got {other:?}"),
    }
}

/// M12③：服务器无视 Range 回 200 但 body 不足窗 → `Io`（200 回退的
/// 「不足」半边；「覆盖 → 截断」半边由
/// `reader_falls_back_to_200_truncation_when_range_is_ignored` 钉死）。
/// 形态 = range_ignore + 首窗 GET 前文件被并发替换为更短版本。
#[tokio::test]
async fn a_200_fallback_body_shorter_than_the_window_is_an_io_error() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", &pattern(300));
    let style = StubStyle {
        range_ignore: true,
        ..StubStyle::rclone()
    };
    let handle = spawn_stub(vfs, AuthMode::None, Knobs::default(), style).await;
    let driver = driver(&handle);
    let entry = driver.stat(&rel("f.bin")).await.expect("stat");

    let stream = driver
        .reader(&entry.id, None)
        .await
        .expect("reader built on the pre-shrink size");
    handle.seed_file("/f.bin", &pattern(100));
    let error = collect(stream)
        .await
        .expect_err("a short 200 body must not pass as the window");
    match error {
        StorageError::Io(message) => {
            assert!(message.contains("ignored the Range"), "{message}");
            assert!(
                message.contains("100"),
                "the actual body length rides along: {message}"
            );
            assert!(
                message.contains("[0,300)"),
                "the requested window rides along: {message}"
            );
        }
        other => panic!("expected Io, got {other:?}"),
    }
}

/// M13②：429 耗尽重试预算 → `Unavailable` 带码（映射表尾部行）。注入：
/// `rate_limit_429` 设 4（> MAX_RETRIES 3）——初始 + 3 次重试全吃 429
/// 后终局。
#[tokio::test]
async fn rate_limit_exhaustion_lands_on_unavailable_after_the_full_budget() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.txt", b"x");
    let knobs = Knobs {
        rate_limit_429: Some((4, None)),
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let driver = driver(&handle);

    let error = driver.stat(&rel("f.txt")).await.expect_err("429 exhausted");
    match error {
        StorageError::Unavailable(message) => {
            assert!(message.contains("429"), "{message}");
            assert!(
                message.contains("retry"),
                "the exhaustion is named: {message}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    // 记录器对账：初始 1 + 重试 3 = 4 个 PROPFIND（重试预算恰好烧尽）。
    assert_eq!(propfinds(&handle), 4);
}

// ------------------------------------------------------------- quota ---

/// 矩阵⑪：RFC 4331 quota 点名——双服务器真形 = 内层 404 → `total: None`
/// 降级（实证常态而非异常路径）。
#[tokio::test]
async fn quota_degrades_to_none_on_inner_404() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    let handle = plain(vfs).await;
    let quota = driver(&handle)
        .quota()
        .await
        .expect("quota degrades, not fails");
    assert_eq!(quota.total, None);
    assert_eq!(quota.used, 0);
    // 真探测过（RFC 4331 点名体是有请求体的 PROPFIND）——不是占位返回。
    assert!(
        handle
            .requests()
            .iter()
            .any(|request| request.method == "PROPFIND" && request.body_len > 0),
        "quota must actually ask (body-bearing PROPFIND): {:?}",
        handle.requests()
    );
}

// ------------------------------------------------------ transport 面 ---

/// K2/E-5：transport open/open_range——vpath 句柄 → 驱动 reader 的窗口
/// 流（预算帽语义）。
#[tokio::test]
async fn transport_open_and_open_range_stream_bytes() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", &pattern(1000));
    let handle = plain(vfs).await;
    let transport = WebdavTransport::new(Arc::new(driver(&handle)));
    transport.connect().await.expect("connect");

    let vpath = cloudkit_storage::vpath::RelPath::new("/f.bin").expect("vpath");
    let file = RemoteHandle {
        first_msg_id: 0,
        chunk_msg_ids: vec![0],
        total_size: 1000,
        path: Some(vpath.clone()),
    };
    let mut stream = transport.open(&file).await.expect("open");
    let mut all = Vec::new();
    while let Some(frame) = stream.next().await {
        all.extend_from_slice(&frame.expect("frame"));
    }
    assert_eq!(all, pattern(1000));

    let mut stream = transport
        .open_range(&file, 100, 150)
        .await
        .expect("open_range [100,250)");
    let mut slice = Vec::new();
    while let Some(frame) = stream.next().await {
        slice.extend_from_slice(&frame.expect("frame"));
    }
    assert_eq!(slice, pattern(1000)[100..250]);
}

/// WD3：transport upload——读盘 → stager 链 → K6 占位 receipt
///（first_msg_id = 0 / chunk_msg_ids = [0]，单 chunk 簿记形态）。
#[tokio::test]
async fn transport_upload_roundtrips_via_the_stager_chain() {
    let handle = plain(Vfs::new()).await;
    let transport = WebdavTransport::new(Arc::new(driver(&handle)));
    transport.connect().await.expect("connect");

    let local = tempfile::NamedTempFile::new().expect("local spool");
    let payload = pattern(2048);
    std::fs::write(local.path(), &payload).expect("local write");
    let vpath = cloudkit_storage::vpath::RelPath::new("/up/f.bin").expect("vpath");
    let job = cloudkit_storage::transport::UploadJob {
        rel_path: vpath,
        local_path: local.path().to_path_buf(),
        size: payload.len() as u64,
        chunk_count: 1,
        chunk_size: payload.len() as u64,
    };
    let receipt = transport.upload(&job).await.expect("upload");
    assert_eq!(receipt.first_msg_id, 0, "K6 placeholder receipt");
    assert_eq!(receipt.chunk_msg_ids, vec![0], "single-chunk placeholder");
    assert_eq!(receipt.uploaded_bytes, payload.len() as u64);

    let entry = driver(&handle)
        .stat(&rel("up/f.bin"))
        .await
        .expect("landed (parents created implicitly)");
    assert_eq!(entry.size, payload.len() as u64);
    assert_eq!(
        handle.take("/up/f.bin").as_deref(),
        Some(payload.as_slice())
    );
}
