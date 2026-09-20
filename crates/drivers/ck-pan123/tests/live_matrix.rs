//! 123-5 真机矩阵（Phase 6；123-4 预置——**默认 `#[ignore]`**，凭据
//! 只经 env，本批零真机）。
//!
//! 运行形态（照 ck-pan115 live_matrix 先例）：
//!
//! ```text
//! CYDRIVE_PAN123_TEST_TOKEN=<90天token> \
//! CYDRIVE_PAN123_TEST_ROOT=<专用测试目录 folder id> \
//! cargo test -p ck-pan123 --test live_matrix -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! 前置注明：
//! - **token**：`cydrive setup`（QR/sign_in）产出，或网页控制台手取；
//!   90 天有效（K76.4 无 refresh——过期重扫）；
//! - **root**：**强烈建议专用测试目录的 folder id**（纯数字）——矩阵
//!   会在卷内建删大量 `cydrive-conformance-*` 条目；缺省 `"0"` =
//!   网盘根（脏数据自担）；
//! - 断言族与离线 conformance 同源（interfaces §6 八条；⑤ 错误映射
//!   与分片级故障注入无真机注入面——离线桩钉死，本套不重复）；
//! - 命名纪律（K72）：stamp 唯一名 + 按轮随机内容；全部真机文件限定
//!   `/_e2e_pan123/` 专用子目录（pan115 live_matrix 作业纪律同源），
//!   收尾清理（trash 语义 = D2，可恢复）。

use ck_pan123::{Pan123Driver, Pan123Params};
use cloudkit_storage::{EntryKind, Page, PageCursor, Range, RelPath, StorageDriver, WriteHint};

/// 专用作业目录名（全部真机文件只在这里出现）。
const E2E_DIR: &str = "_e2e_pan123";

fn stamp() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("cydrive-conf-{nanos:x}")
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    let mut x = seed;
    (0..n)
        .map(|i| {
            x = x.wrapping_mul(31).wrapping_add(i as u8);
            x
        })
        .collect()
}

/// 从 env 组装真机驱动（凭据缺失 → panic 提示如何运行）。
fn live_params() -> Pan123Params {
    let token = std::env::var("CYDRIVE_PAN123_TEST_TOKEN").unwrap_or_else(|_| {
        panic!(
            "CYDRIVE_PAN123_TEST_TOKEN is required for the live matrix: obtain a token via \
             `cydrive setup` (QR scan or sign_in) and export it; CYDRIVE_PAN123_TEST_ROOT may \
             pin a dedicated test folder id (default 0 = the netdisk root)"
        )
    });
    let root = std::env::var("CYDRIVE_PAN123_TEST_ROOT").unwrap_or_else(|_| "0".to_string());
    Pan123Params {
        token: Some(token),
        root,
        ..Pan123Params::default()
    }
}

async fn read_all(mut s: cloudkit_storage::ByteStream) -> Vec<u8> {
    use futures_util::StreamExt;
    let mut out = Vec::new();
    while let Some(chunk) = s.next().await {
        out.extend_from_slice(&chunk.expect("chunk"));
    }
    out
}

/// 分页遍历（limit=2 强制翻页；稳定有序断言的取数面）。
async fn walk_paged(driver: &Pan123Driver, base: &RelPath) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = PageCursor::Start;
    loop {
        let listing = driver
            .list(base, Page { limit: 2, cursor })
            .await
            .expect("list");
        out.extend(listing.entries.iter().map(|e| e.path.as_str().to_string()));
        cursor = match listing.next {
            Some(next) => next,
            None => break,
        };
    }
    out
}

/// ①+②+⑥+⑧ 同源断言族的真机最小面（123-5 展开为全矩阵；本骨架
/// 先钉：上传往返跨块 / Range 半开窗口 / 并发读 / rename 文件与目录
/// / resume 会话保留真服务端对账）。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_PAN123_TEST_TOKEN (see the module docs)"]
async fn live_matrix_skeleton() {
    let driver = ck_pan123::factory(&live_params())
        .await
        .expect("connect the live backend");
    let chunk = 5 * 1024 * 1024u64;
    let tag = stamp();
    let base = RelPath::new(&format!("{E2E_DIR}/{tag}")).expect("base path");

    // ① 上传往返（跨块）：2.4 块。
    let total = 2 * chunk + 4096;
    let data = pattern(total as usize, 0x2F);
    let f = base.join("roundtrip.bin").expect("path");
    let mut st = driver
        .writer(
            &f,
            &WriteHint {
                size: Some(total),
                ..Default::default()
            },
        )
        .await
        .expect("writer");
    st.write(&data).await.expect("write");
    let entry = st.close().await.expect("close");
    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.size, total);
    let got = read_all(driver.reader(&entry.id, None).await.expect("reader")).await;
    assert_eq!(got, data, "byte-exact roundtrip");

    // ② Range 半开窗口 + EOF 钳制。
    let s = total / 4;
    let got = read_all(
        driver
            .reader(
                &entry.id,
                Some(Range::new(s, Some(s + 1024)).expect("range")),
            )
            .await
            .expect("range reader"),
    )
    .await;
    assert_eq!(got, data[s as usize..s as usize + 1024]);
    let got = read_all(
        driver
            .reader(
                &entry.id,
                Some(Range::new(s, Some(total + chunk)).expect("range")),
            )
            .await
            .expect("clamped reader"),
    )
    .await;
    assert_eq!(got, data[s as usize..], "end clamps to EOF");

    // ⑦ resume 真服务端对账：同参重发 → 同一 UploadId + 分片在册
    // （K74「真 OSS 对账」形态——字节级观测在离线桩，服务端会话保留
    // 在真机钉）。第二轮到齐即传后 close 收尾 + 逐字节回读。
    let rpath = base.join("resume.bin").expect("path");
    let mut st1 = driver
        .writer(
            &rpath,
            &WriteHint {
                size: Some(total),
                ..Default::default()
            },
        )
        .await
        .expect("resume writer 1");
    st1.write(&data)
        .await
        .expect("resume write 1 (eager transfer)");
    drop(st1); // 不 close：会话保留在服务端
    let mut st2 = driver
        .writer(
            &rpath,
            &WriteHint {
                size: Some(total),
                ..Default::default()
            },
        )
        .await
        .expect("resume writer 2");
    st2.write(&data).await.expect("resume write 2");
    let rentry = st2.close().await.expect("resume close");
    let got = read_all(driver.reader(&rentry.id, None).await.expect("reader")).await;
    assert_eq!(got, data, "resumed object is byte-exact");

    // ⑥ rename 文件 + 目录（同父改名）。
    let f2 = base.join("roundtrip-moved.bin").expect("path");
    driver.rename(&f, &f2).await.expect("rename file");
    assert!(driver.stat(&f).await.is_err(), "old path is gone");
    let moved = driver.stat(&f2).await.expect("new path");
    assert_eq!(moved.size, total);

    let d = base.join("dtree").expect("dir");
    driver.mkdir(&d).await.expect("mkdir");
    let leaf = d.join("leaf.bin").expect("leaf");
    let mut st = driver
        .writer(
            &leaf,
            &WriteHint {
                size: Some(1024),
                ..Default::default()
            },
        )
        .await
        .expect("leaf writer");
    st.write(&pattern(1024, 0x51)).await.expect("leaf write");
    st.close().await.expect("leaf close");
    let d2 = base.join("dtree2").expect("dir2");
    driver.rename(&d, &d2).await.expect("rename dir");
    assert!(driver.stat(&leaf).await.is_err(), "old subtree is gone");
    assert!(
        driver
            .stat(&d2.join("leaf.bin").expect("leaf2"))
            .await
            .is_ok(),
        "subtree moved"
    );

    // ③ list 分页稳定有序（两轮一致）。
    let w1 = walk_paged(&driver, &base).await;
    let w2 = walk_paged(&driver, &base).await;
    assert_eq!(w1, w2, "stable pagination");
    assert!(w1.windows(2).all(|w| w[0] <= w[1]), "sorted");

    // ⑧ 并发读互不干扰。
    let (a, b) = futures_util::future::join(
        read_all(
            driver
                .reader(&rentry.id, Some(Range::new(0, Some(chunk + 3)).expect("r")))
                .await
                .expect("c1"),
        ),
        read_all(
            driver
                .reader(&rentry.id, Some(Range::new(chunk, None).expect("r")))
                .await
                .expect("c2"),
        ),
    )
    .await;
    assert_eq!(a, data[..(chunk + 3) as usize]);
    assert_eq!(b, data[chunk as usize..]);

    // ④ 清理：目录递归删除（trash 语义）+ 回读核空。
    let dir_entry = driver.stat(&base).await.expect("base entry");
    driver.delete(&dir_entry.id).await.expect("cleanup");
    assert!(driver.stat(&base).await.is_err(), "matrix folder cleaned");
}
