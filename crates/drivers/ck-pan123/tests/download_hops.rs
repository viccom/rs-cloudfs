//! 下载链桩回放测试（Phase 6 / 123-2；任务 E——三跳怪癖）。
//!
//! 语义契约（断言即契约，实现者禁改；黄金参照 = 123-0 spike ④ 真机
//! 实证 + §5.7/§5.12/§5.14 + D5）：
//!
//! - **三跳解析**（每跳每形态）：① web-pro2 中继 URL `params=` 段
//!   urlsafe-base64 **自解码**（纯解析零 GET——dlink 一次性纪律）→
//!   ② CDN 域 HTTP 210 + JSON `redirect_url` 重定向体 → ③ 镜像域 206
//!   （Content-Range 校验）；**30x Location 头**与 **200 HTML href**
//!   防御形态同样容错；**≤3 跳封顶**（自指重定向 → `Unavailable`）；
//! - **dlink 缓存**：同 fid 二次 open 复用——`download_info` 恰一次
//!   （§5.14 禁止重复 GET 探测）；缓存 URL 死亡（404/410）→ 失效 +
//!   重取一次（M-S3 自愈路径可观测）；
//! - **traffic 预检**（§5.7）：`isTrafficExceeded:true` → `RateLimited`
//!   （先于 download_info）；`download_info` 回 5113/5114 → 同样
//!   `RateLimited`（D5：不绕过）；
//! - **Range**：半开区间 + 越界钳制；`bytes=N-` → 206 + Content-Range
//!   解析总量；>4MiB 文件跨多窗口逐字节；start≥size → 空流；
//! - **双会话分离**（§5.12）：CDN GET 不带 123pan 鉴权头（桩断言）；
//! - **写偏防线**：206 Content-Range 起始不吻合 → `Unavailable`；
//!   200 全量（Range 被忽略）→ `Unavailable`。

mod stub_common;

use ck_pan123::Pan123Driver;
use cloudkit_storage::{EntryId, Range, StorageDriver, StorageError};
use futures_util::StreamExt;
use stub_common::ApiStub;

async fn stub() -> ApiStub {
    ApiStub::start().await
}

fn handle(driver: &Pan123Driver, fid: i64) -> EntryId {
    EntryId::new(
        driver.volume().clone(),
        cloudkit_storage::BackendHandle::new(fid.to_string()),
    )
}

async fn read_all(
    driver: &Pan123Driver,
    fid: i64,
    range: Option<Range>,
) -> Result<Vec<u8>, StorageError> {
    use cloudkit_storage::ByteStream;
    let stream: ByteStream = driver.reader(&handle(driver, fid), range).await?;
    let mut out = Vec::new();
    let mut stream = stream;
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}

/// 真机三跳全形态：relay（params 自解码，零 GET）→ 210 JSON 重定向体
/// → 镜像 206——窗口字节与源逐字节吻合；传输腿不带鉴权头。
#[tokio::test]
async fn happy_path_walks_the_live_three_hop_chain() {
    let s = stub().await;
    let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    let fid = s.put_file("0", "chain.bin", data.clone());

    let driver = s.driver();
    let got = read_all(&driver, fid, None).await.expect("full read");
    assert_eq!(got, data, "byte-for-byte through the three hops");
    // params 自解码 = 中继 URL 零 GET（无 /download-v2 路由——404 会
    // 杀死链路；走到 206 即证明没 GET 过它）。
    assert_eq!(s.hits("/redirect"), 1, "exactly one 210-JSON hop");
    assert_eq!(
        s.hits("/mirror"),
        1,
        "exactly one mirror GET for one window"
    );
    assert_eq!(s.hits("/a/api/file/download_info"), 1);
}

/// 传输腿双会话分离（§5.12）：CDN GET 只带 UA——不带 Bearer/Cookie。
#[tokio::test]
async fn transfer_leg_carries_no_pan123_auth_headers() {
    // 专用桩：mirror 断言请求头。
    use std::sync::{Arc, Mutex};
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen2 = Arc::clone(&seen);
    let app = axum::Router::new().route(
        "/mirror/{fid}",
        axum::routing::get(move |headers: axum::http::HeaderMap| {
            let seen = Arc::clone(&seen2);
            async move {
                for name in ["authorization", "cookie", "user-agent"] {
                    if let Some(v) = headers.get(name) {
                        seen.lock()
                            .unwrap()
                            .push(format!("{name}={}", v.to_str().unwrap_or("")));
                    }
                }
                axum::http::StatusCode::NOT_FOUND
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    let client = ck_pan123::api::Pan123Client::new(
        "secret-token-0123456789".into(),
        "http://127.0.0.1:1".into(),
        "http://127.0.0.1:1".into(),
        None,
    )
    .expect("client");
    let _ = client
        .transfer_http()
        .get(format!("http://{addr}/mirror/1"))
        .header("range", "bytes=0-0")
        .send()
        .await
        .expect("transfer GET goes out");
    let snapshot = seen.lock().unwrap().clone();
    assert!(
        snapshot.iter().any(|h| h.starts_with("user-agent=")),
        "UA rides: {snapshot:?}"
    );
    assert!(
        !snapshot.iter().any(|h| h.starts_with("authorization=")),
        "no bearer on the transfer leg: {snapshot:?}"
    );
    assert!(
        !snapshot.iter().any(|h| h.starts_with("cookie=")),
        "no sso cookie on the transfer leg: {snapshot:?}"
    );
}

/// dlink 缓存（§5.14）：同 fid 二次 open——download_info 恰一次、
/// traffic 预检恰一次；跳链零重放。
#[tokio::test]
async fn dlink_cache_reuses_the_resolved_url_across_opens() {
    let s = stub().await;
    let data: Vec<u8> = (0..500u32).map(|i| (i % 7) as u8).collect();
    let fid = s.put_file("0", "cached.bin", data.clone());

    let driver = s.driver();
    let first = read_all(&driver, fid, None).await.expect("first open");
    assert_eq!(first, data);
    let second = read_all(&driver, fid, None).await.expect("second open");
    assert_eq!(second, data);
    assert_eq!(
        s.hits("/a/api/file/download_info"),
        1,
        "dlink fetched exactly once (one-shot discipline)"
    );
    assert_eq!(s.hits("/b/api/file/download/traffic/check"), 1);
    assert_eq!(s.hits("/redirect"), 1, "the hop chain never replays");
}

/// 30x Location 头形态（§5.14 容忍面）：链模式 location——302 → 镜像。
#[tokio::test]
async fn location_header_form_resolves_too() {
    let s = stub().await;
    s.state.lock().unwrap().chain_mode = "location".into();
    let data = vec![9u8; 128];
    let fid = s.put_file("0", "loc.bin", data.clone());

    let driver = s.driver();
    let got = read_all(&driver, fid, None).await.expect("via Location");
    assert_eq!(got, data);
    assert_eq!(s.hits("/loc"), 1);
}

/// 200 HTML 带 href 的防御形态（真机中继页无 href——href 扫描腿）。
#[tokio::test]
async fn html_href_form_is_the_defensive_fallback() {
    let s = stub().await;
    s.state.lock().unwrap().chain_mode = "html".into();
    let data = vec![3u8; 64];
    let fid = s.put_file("0", "html.bin", data.clone());

    let driver = s.driver();
    let got = read_all(&driver, fid, None).await.expect("via href scan");
    assert_eq!(got, data);
    assert_eq!(s.hits("/html"), 1);
}

/// 直指 CDN（无中继跳）形态：210 JSON → 镜像（两跳链）。
#[tokio::test]
async fn direct_cdn_form_skips_the_relay() {
    let s = stub().await;
    s.state.lock().unwrap().chain_mode = "direct-cdn".into();
    let data = vec![5u8; 32];
    let fid = s.put_file("0", "direct.bin", data.clone());

    let driver = s.driver();
    let got = read_all(&driver, fid, None).await.expect("two-hop chain");
    assert_eq!(got, data);
}

/// 跳数封顶：自指重定向环 → `Unavailable`（载荷声明跳数）。
#[tokio::test]
async fn hop_cap_terminates_redirect_loops() {
    let s = stub().await;
    s.state.lock().unwrap().chain_mode = "loop".into();
    let fid = s.put_file("0", "loop.bin", vec![1; 8]);

    let driver = s.driver();
    let err = read_all(&driver, fid, None).await.expect_err("loop breaks");
    match err {
        StorageError::Unavailable(detail) => {
            assert!(detail.contains("hops"), "{detail}");
        }
        other => panic!("loop must die as Unavailable, got {other:?}"),
    }
}

/// traffic 预检（§5.7）：isTrafficExceeded → RateLimited，且先于
/// download_info（零取链调用）。
#[tokio::test]
async fn traffic_exceeded_blocks_before_any_link_fetch() {
    let s = stub().await;
    s.state.lock().unwrap().traffic_exceeded = true;
    let fid = s.put_file("0", "quota.bin", vec![1; 16]);

    let driver = s.driver();
    let err = read_all(&driver, fid, None)
        .await
        .expect_err("D5: not bypassed");
    assert!(
        matches!(err, StorageError::RateLimited { .. }),
        "quota exhaustion is RateLimited: {err:?}"
    );
    assert_eq!(
        s.hits("/a/api/file/download_info"),
        0,
        "no link fetch past the pre-check"
    );
}

/// download_info 回 5113（流量限额信封形态）→ RateLimited（D5）。
#[tokio::test]
async fn download_info_5113_maps_to_rate_limited() {
    let s = stub().await;
    s.state.lock().unwrap().download_info_code = 5113;
    let fid = s.put_file("0", "capped.bin", vec![1; 16]);

    let driver = s.driver();
    let err = read_all(&driver, fid, None)
        .await
        .expect_err("5113 surfaces");
    assert!(
        matches!(err, StorageError::RateLimited { .. }),
        "envelope 5113 -> RateLimited: {err:?}"
    );
    assert_eq!(s.hits("/mirror"), 0);
}

/// 缓存直链死亡（404）→ 失效 + 重取一次（M-S3 自愈路径可观测——
/// download_info 二次调用；镜像仍死则 NotFound 上抛）。
#[tokio::test]
async fn dead_cached_link_re_resolves_once() {
    let s = stub().await;
    let data = vec![7u8; 24];
    let fid = s.put_file("0", "mortal.bin", data.clone());

    let driver = s.driver();
    let first = read_all(&driver, fid, None).await.expect("first open");
    assert_eq!(first, data);
    assert_eq!(s.hits("/a/api/file/download_info"), 1);

    s.state.lock().unwrap().mirror_dead = true;
    let err = read_all(&driver, fid, None).await.expect_err("dead link");
    assert_eq!(err, StorageError::NotFound, "404 propagates as NotFound");
    assert_eq!(
        s.hits("/a/api/file/download_info"),
        2,
        "the cached link death triggered exactly one re-resolve"
    );
}

/// Range 契约：半开窗口精确切片；`bytes=N-` 尾读；start>=size → 空流；
/// 越界 end 钳制到 size。
#[tokio::test]
async fn range_semantics_slice_clamp_and_empty_tail() {
    let s = stub().await;
    let data: Vec<u8> = (0..300u32).map(|i| (i % 256) as u8).collect();
    let fid = s.put_file("0", "ranges.bin", data.clone());

    let driver = s.driver();
    // 精确窗口 [10, 50)。
    let got = read_all(
        &driver,
        fid,
        Some(Range {
            start: 10,
            end: Some(50),
        }),
    )
    .await
    .expect("window");
    assert_eq!(got, data[10..50]);

    // 尾读 N-。
    let got = read_all(
        &driver,
        fid,
        Some(Range {
            start: 250,
            end: None,
        }),
    )
    .await
    .expect("tail");
    assert_eq!(got, data[250..]);

    // 越界 end 钳制。
    let got = read_all(
        &driver,
        fid,
        Some(Range {
            start: 290,
            end: Some(10000),
        }),
    )
    .await
    .expect("clamped");
    assert_eq!(got, data[290..]);

    // start >= size：空流（不报错——声明一致形态）。
    let got = read_all(
        &driver,
        fid,
        Some(Range {
            start: 300,
            end: Some(400),
        }),
    )
    .await
    .expect("empty beyond EOF");
    assert!(got.is_empty());
}

/// 多窗口流：>4MiB 文件跨两窗逐字节（有界窗口流组织）。
#[tokio::test]
async fn multi_window_stream_reassembles_the_whole_file() {
    let s = stub().await;
    let mut data = vec![0u8; 4 * 1024 * 1024 + 65536];
    // 可校验的伪随机内容（无 rand 依赖）。
    let mut x: u32 = 0x12345678;
    for b in data.iter_mut() {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        *b = (x >> 24) as u8;
    }
    let fid = s.put_file("0", "big.bin", data.clone());

    let driver = s.driver();
    let got = read_all(&driver, fid, None).await.expect("big read");
    assert_eq!(got.len(), data.len());
    assert_eq!(got, data);
    assert!(
        s.hits("/mirror") >= 2,
        "the bounded-window stream issued multiple windows"
    );
    assert_eq!(s.hits("/a/api/file/download_info"), 1, "dlink stays cached");
}

/// 写偏防线：mirror 回 200 全量（Range 被忽略）→ `Unavailable` 明示。
#[tokio::test]
async fn range_ignored_200_is_rejected() {
    let s = stub().await;
    s.state.lock().unwrap().mirror_ignore_range = true;
    let fid = s.put_file("0", "flat.bin", vec![1; 100]);

    let driver = s.driver();
    let err = read_all(
        &driver,
        fid,
        Some(Range {
            start: 10,
            end: None,
        }),
    )
    .await
    .expect_err("ignored Range is fatal");
    match err {
        StorageError::Unavailable(detail) => {
            assert!(detail.contains("Range"), "{detail}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

/// reader 形态守卫：目录句柄 → Invalid；未知句柄 → NotFound；垃圾
/// 句柄 → Invalid；他卷句柄 → NotFound。
#[tokio::test]
async fn reader_guards_shapes_and_handles() {
    let s = stub().await;
    let dir_fid = s.mkdir("0", "adir");
    s.put_file("0", "f.bin", vec![1; 4]);

    let driver = s.driver();
    let err = match driver.reader(&handle(&driver, dir_fid), None).await {
        Err(e) => e,
        Ok(_) => panic!("directories are not readable"),
    };
    assert_eq!(err, StorageError::Invalid);

    let err = match driver.reader(&handle(&driver, 999999), None).await {
        Err(e) => e,
        Ok(_) => panic!("unknown fid must not open"),
    };
    assert_eq!(err, StorageError::NotFound);

    let bad = EntryId::new(
        driver.volume().clone(),
        cloudkit_storage::BackendHandle::new("junk"),
    );
    let err = match driver.reader(&bad, None).await {
        Err(e) => e,
        Ok(_) => panic!("junk handle must not open"),
    };
    assert_eq!(err, StorageError::Invalid);

    let foreign = EntryId::new(
        cloudkit_storage::VolumeId::new("other", "x").unwrap(),
        cloudkit_storage::BackendHandle::new("1"),
    );
    let err = match driver.reader(&foreign, None).await {
        Err(e) => e,
        Ok(_) => panic!("foreign volume must not open"),
    };
    assert_eq!(err, StorageError::NotFound);
}
