//! 下载器 dlink 缓存与 fallback（Batch B2 红③；K8/K9）。
//!
//! 语义契约（断言即契约，实现者禁改；黄金参照：spike §5 dl-try 矩阵 +
//! 附录 B TTL 实测）：
//!
//! - **dlink 签发**：GET `xpan/file?method=download&access_token&path`
//!   （恰三参数）禁重定向看 302 Location；驱动按 (fs_id/path) 缓存，
//!   **TTL=60min 缺省**（实测下界 ≥96min 的保守值）；TTL 内重复读同一
//!   文件不重取 dlink；
//! - **CDN 下载三约束**（违反任一 → 403 error_code=31326）：netdisk 族
//!   UA、有界 Range ≤4MiB（无 Range/开放/超界全拒）、过期链重取（追加
//!   access_token 救不了 expires 过期）；
//! - **两段 fallback**（403/31326 时）：先同 URL 追加 access_token 重试
//!   一次 → 仍败再重取 dlink（spike §5：直连与追加 token 两态都出现过）；
//! - **Range >4MiB 对上层透明**：驱动内部分片拼接（请求 8MiB 得 8MiB，
//!   mock 收到的每个 Range 有界 ≤4MiB）。

mod common;

use std::sync::Arc;

use ck_baidu::{factory, BaiduDriver};
use cloudkit_storage::{Range, RelPath, StorageDriver};
use futures_util::StreamExt;

use common::{
    cdn_requests, filter_recorded, parse_urlencoded, pattern_bytes, CdnAuthMode, MockBaidu,
    CHUNK_4M, INITIAL_ACCESS_TOKEN, MOCK_ROOT, NETDISK_UA, XPAN_FILE,
};

/// 播种一个 8MiB+ 文件（内容入 mock blob——独立于上传路径，本套件只测
/// 下载面）+ 构造驱动。
async fn setup() -> (MockBaidu, Arc<BaiduDriver>, cloudkit_storage::Entry) {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let content = pattern_bytes(2 * CHUNK_4M + 12345);
    mock.seed_file_bytes(&format!("{MOCK_ROOT}/dl.bin"), &content, 1757311000);
    let driver = factory(&mock.params(None))
        .await
        .expect("baidu driver connect");
    let entry = driver
        .stat(&RelPath::new("dl.bin").expect("rel path"))
        .await
        .expect("stat 播种文件");
    (mock, driver, entry)
}

async fn read_all(stream: cloudkit_storage::ByteStream) -> Vec<u8> {
    let mut stream = stream;
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => out.extend_from_slice(&bytes),
            Err(e) => panic!("读流中途错误: {e}"),
        }
    }
    out
}

/// dlink 签发端点调用次数（method=download）。
fn dlink_calls(mock: &MockBaidu) -> usize {
    filter_recorded(&mock.recorded(), "GET", XPAN_FILE, &["method=download"]).len()
}

#[tokio::test]
async fn dlink_fetched_once_within_ttl_and_cdn_requires_netdisk_ua() {
    let (mock, driver, entry) = setup().await;
    let content = pattern_bytes(2 * CHUNK_4M + 12345);

    // 两次全量读：TTL（缺省 60min）内 dlink 恰签发一次。
    let first = read_all(driver.reader(&entry.id, None).await.expect("第一次读")).await;
    assert_eq!(first, content, "第一次读逐字节正确");
    let second = read_all(driver.reader(&entry.id, None).await.expect("第二次读")).await;
    assert_eq!(second, content, "第二次读逐字节正确");
    assert_eq!(
        dlink_calls(&mock),
        1,
        "TTL 内 dlink 恰一次签发（K8 缓存复用）"
    );

    // dlink 签发 query 恰三参数（spike §5 实抓形态）。
    let recorded = mock.recorded();
    let dlink_reqs = filter_recorded(&recorded, "GET", XPAN_FILE, &["method=download"]);
    common::assert_exact_pairs(
        &parse_urlencoded(&dlink_reqs[0].query),
        &[
            ("method", "download"),
            ("access_token", INITIAL_ACCESS_TOKEN),
            ("path", &format!("{MOCK_ROOT}/dl.bin")),
        ],
    );

    // CDN 侧：全部请求 UA 都是 netdisk 族（下载三约束之一）。
    let cdn = cdn_requests(&recorded);
    assert!(!cdn.is_empty(), "CDN 分片请求已发生");
    for req in &cdn {
        assert_eq!(
            req.user_agent.as_deref(),
            Some(NETDISK_UA),
            "CDN 请求 UA 必须是 netdisk 族（spike §5：非 netdisk UA → 403）"
        );
    }
}

#[tokio::test]
async fn expired_dlink_ttl_refetches_dlink() {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let content = pattern_bytes(CHUNK_4M / 3);
    mock.seed_file_bytes(&format!("{MOCK_ROOT}/ttl.bin"), &content, 1757311000);
    // 注入短 TTL=1s（BaiduParams.dlink_ttl_secs——测试注入契约）。
    let driver = factory(&mock.params_with(None, None, Some(1)))
        .await
        .expect("driver connect");
    let entry = driver
        .stat(&RelPath::new("ttl.bin").expect("rel path"))
        .await
        .expect("stat");

    let first = read_all(driver.reader(&entry.id, None).await.expect("第一次读")).await;
    assert_eq!(first, content);
    assert_eq!(dlink_calls(&mock), 1);

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let second = read_all(driver.reader(&entry.id, None).await.expect("TTL 过期后读")).await;
    assert_eq!(second, content, "TTL 过期后读仍逐字节正确");
    assert_eq!(dlink_calls(&mock), 2, "缓存过期 → 重取 dlink（恰两次签发）");
}

#[tokio::test]
async fn token_required_mode_falls_back_by_appending_access_token() {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let content = pattern_bytes(CHUNK_4M / 2);
    mock.seed_file_bytes(&format!("{MOCK_ROOT}/tok.bin"), &content, 1757311000);
    let driver = factory(&mock.params(None)).await.expect("driver connect");
    let entry = driver
        .stat(&RelPath::new("tok.bin").expect("rel path"))
        .await
        .expect("stat");

    // 模式 b（spike §5：直连 403、追加 access_token 后 206）。
    mock.set_cdn_mode(CdnAuthMode::TokenRequired);

    let got = read_all(driver.reader(&entry.id, None).await.expect("fallback 读")).await;
    assert_eq!(got, content, "两段 fallback 第一段（追 token）救回数据");

    // dlink 未重取（第一段已恢复）。
    assert_eq!(dlink_calls(&mock), 1, "追 token 即恢复 → 不重取 dlink");

    // CDN 请求序列：先无 token（403）→ 后带 token（206）。
    let recorded = mock.recorded();
    let cdn = cdn_requests(&recorded);
    assert!(cdn.len() >= 2, "CDN 至少两次请求（403 → 追 token 206）");
    let without_token = cdn.iter().any(|r| {
        !parse_urlencoded(&r.query)
            .iter()
            .any(|(k, _)| k == "access_token")
    });
    let with_token = cdn.iter().any(|r| {
        parse_urlencoded(&r.query)
            .iter()
            .any(|(k, v)| k == "access_token" && v == INITIAL_ACCESS_TOKEN)
    });
    assert!(without_token, "存在无 token 的直连尝试（撞 403）");
    assert!(with_token, "存在追加 access_token 的重试（恢复为 206）");
}

#[tokio::test]
async fn expired_cdn_link_refetches_new_dlink_after_token_retry_also_fails() {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let content = pattern_bytes(CHUNK_4M / 2);
    mock.seed_file_bytes(&format!("{MOCK_ROOT}/old.bin"), &content, 1757311000);
    let driver = factory(&mock.params(None)).await.expect("driver connect");
    let entry = driver
        .stat(&RelPath::new("old.bin").expect("rel path"))
        .await
        .expect("stat");

    // 首读建立 dlink 缓存（旧链，expires=mock 时钟+600）。
    let first = read_all(driver.reader(&entry.id, None).await.expect("首读")).await;
    assert_eq!(first, content);
    assert_eq!(dlink_calls(&mock), 1);

    // 模式 c：mock 时钟推进越过 expires——旧链 403 且追 token 不可救
    // （token 救不了过期链），必须重取 dlink。
    mock.advance_clock(3600);

    let second = read_all(driver.reader(&entry.id, None).await.expect("旧链过期后读")).await;
    assert_eq!(second, content, "两段 fallback 全走完（重取 dlink）后恢复");

    // dlink 恰两次签发：首读一次 + 旧链两段失败后重取一次。
    assert_eq!(dlink_calls(&mock), 2, "旧链 403+追 token 403 → 重取 dlink");

    // CDN 序列按 expires 分桶：旧链（早 expires）至少两次请求（直连+追
    // token 均 403），新链（晚 expires）206 成功。
    let recorded = mock.recorded();
    let cdn = cdn_requests(&recorded);
    let mut by_expires: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for req in &cdn {
        if let Some((_, v)) = parse_urlencoded(&req.query)
            .into_iter()
            .find(|(k, _)| k == "expires")
        {
            by_expires.insert(v);
        }
    }
    assert_eq!(
        by_expires.len(),
        2,
        "CDN 收到两个 expires 代的链接（旧链+新链）"
    );
}

#[tokio::test]
async fn large_range_is_sliced_into_bounded_cdn_ranges_transparently() {
    let (mock, driver, entry) = setup().await;
    let content = pattern_bytes(2 * CHUNK_4M + 12345);

    // 请求 8MiB 有界窗口（= 2 个 4MiB 分片）：对上层是一个流，mock 收到
    // 多个 ≤4MiB 的有界 Range（驱动内部分片拼接，K9）。
    let end = 2 * CHUNK_4M;
    let got = read_all(
        driver
            .reader(
                &entry.id,
                Some(Range::new(0, Some(end as u64)).expect("range")),
            )
            .await
            .expect("8MiB 窗口读"),
    )
    .await;
    assert_eq!(got.len(), end, "上层拿到完整 8MiB");
    assert_eq!(got, content[..end], "8MiB 窗口逐字节正确");

    let recorded = mock.recorded();
    let cdn = cdn_requests(&recorded);
    assert!(
        cdn.len() >= 2,
        "8MiB 窗口经多个分片请求承载（≥2 次 CDN 调用）"
    );
    for req in &cdn {
        let Some(range) = req.range_header.as_deref() else {
            panic!("CDN 请求必须带 Range 头（三约束）：{req:?}");
        };
        let spec = range
            .strip_prefix("bytes=")
            .unwrap_or_else(|| panic!("Range 头形态 bytes=s-e：{range}"));
        let (s, e) = spec
            .split_once('-')
            .unwrap_or_else(|| panic!("Range 必须有界：{range}"));
        assert!(
            !e.is_empty(),
            "Range 必须有界（开放区间 403，spike §5）：{range}"
        );
        let start: u64 = s.parse().expect("start");
        let end: u64 = e.parse().expect("end");
        let span = end - start + 1;
        assert!(
            span <= CHUNK_4M as u64,
            "单请求分片 ≤4MiB（超界 403，spike §5）：span={span}"
        );
    }
}
