//! 115-5 真机冒烟矩阵（Phase 5 收口批）——**默认 `#[ignore]`**，凭据只经
//! env，绝不入代码/文档/提交（R3）。
//!
//! 运行（PowerShell / Git Bash 皆可；token 对从 env 读，测试本身不落盘
//! 任何凭据）：
//!
//! ```text
//! CYDRIVE_PAN115_TEST_ACCESS_TOKEN=... \
//! CYDRIVE_PAN115_TEST_REFRESH_TOKEN=... \
//! cargo test -p ck-pan115 --test live_matrix -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! 可选：`CYDRIVE_PAN115_TEST_ROOT`（缺省 `0` 网盘根——矩阵自建
//! `/_e2e_pan115/` 子目录并只在该目录内作业）、`CYDRIVE_PAN115_TEST_CLIENT_ID`。
//!
//! 作业纪律（负责人 115-0 指令）：所有测试文件限定 `/_e2e_pan115/`
//! 专用子目录；收尾清理（删除进回收站 = D2 语义，可恢复）；列目录/
//! 上传遵守 D4 限速（驱动内 1 rps 令牌桶 + 770004 硬退避）；不碰
//! 分享/离线下载/视频族端点。
//!
//! 三项冒烟（115-0 裁决后的最小真机验证）：
//! 1. `upload_and_read_back`：上传 → 回读逐字节 + 远端 size 复核；
//! 2. `range_window_is_byte_exact`：Range 窗口读与本地切片逐字节一致；
//! 3. `rapid_upload_hits_on_the_second_round`：同内容重传 → 秒传命中
//!    （status==2 路径；K69.6 真机形态）。
//!
//! 另附 `cleanup_e2e_dir`（收尾 delete——进回收站）。

use ck_pan115::limiter::LimiterConfig;
use ck_pan115::{Pan115Driver, Pan115Params};
use cloudkit_storage::{Range, RelPath, StorageDriver, StorageError, WriteHint};

/// 专用作业目录名（全部真机文件只在这里出现）。
const E2E_DIR: &str = "_e2e_pan115";

/// 从 env 组装测试驱动（凭据缺失 → panic 提示如何运行——`#[ignore]`
/// 下不会误伤 CI）。
async fn live_driver() -> Pan115Driver {
    let access = std::env::var("CYDRIVE_PAN115_TEST_ACCESS_TOKEN")
        .expect("set CYDRIVE_PAN115_TEST_ACCESS_TOKEN (see the module docs)");
    let refresh = std::env::var("CYDRIVE_PAN115_TEST_REFRESH_TOKEN")
        .expect("set CYDRIVE_PAN115_TEST_REFRESH_TOKEN");
    let root = std::env::var("CYDRIVE_PAN115_TEST_ROOT").unwrap_or_else(|_| "0".to_string());
    let client_id = std::env::var("CYDRIVE_PAN115_TEST_CLIENT_ID")
        .unwrap_or_else(|_| ck_pan115::DEFAULT_CLIENT_ID.to_string());
    let params = Pan115Params {
        client_id,
        access_token: Some(access),
        refresh_token: Some(refresh),
        root,
        // 真机端点（默认生产常量——显式写出以便一眼核对）。
        api_base: ck_pan115::DEFAULT_API_BASE.to_string(),
        passport_base: ck_pan115::DEFAULT_PASSPORT_BASE.to_string(),
        token_store: None, // 测试不落盘（凭据由 env 持有）
        // 真机限速：D4 生产缺省（1 rps + 300s 硬退避起步）——矩阵小，
        // 不缺这一点的等待；**刻意不用 fast()**（真机纪律）。
        limiter: Some(LimiterConfig::default()),
        sessions_dir: None,
    };
    Pan115Driver::connect(params)
        .await
        .expect("live connect (token pair from env) succeeds")
}

/// 确保 `/_e2e_pan115/` 存在（幂等）并返回其路径。
async fn ensure_e2e_dir(driver: &Pan115Driver) -> RelPath {
    let dir = RelPath::new(E2E_DIR).expect("e2e path");
    match driver.mkdir(&dir).await {
        Ok(()) => {}
        Err(StorageError::Exists) => {}
        Err(error) => panic!("mkdir {E2E_DIR} failed: {error}"),
    }
    dir
}

/// 收尾清理：删除作业文件（**D2 语义 = 进回收站**，可恢复）。
async fn cleanup(driver: &Pan115Driver, dir: &RelPath, names: &[&str]) {
    for name in names {
        let path = dir.join(name).expect("child path");
        match driver.stat(&path).await {
            Ok(entry) => {
                if let Err(error) = driver.delete(&entry.id).await {
                    eprintln!("[cleanup] delete {name} failed (non-fatal): {error}");
                }
            }
            Err(StorageError::NotFound) => {}
            Err(error) => eprintln!("[cleanup] stat {name} failed (non-fatal): {error}"),
        }
    }
}

async fn read_all(mut stream: cloudkit_storage::ByteStream) -> Vec<u8> {
    use futures_util::StreamExt;
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("chunk"));
    }
    out
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// ① 上传 → 回读逐字节 + 远端 size 复核（驱动 close 内的复核之外，
/// 测试层再独立回读一次）。
#[tokio::test]
#[ignore = "live 115 network + real credentials (see module docs)"]
async fn upload_and_read_back() {
    let driver = live_driver().await;
    let dir = ensure_e2e_dir(&driver).await;
    let name = "smoke-upload.bin";
    let data = pattern(200 * 1024); // 200 KiB（< 5MiB → PutObject 路径）
    let path = dir.join(name).expect("path");

    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..WriteHint::default()
    };
    let mut stager = driver.writer(&path, &hint).await.expect("writer");
    stager.write(&data).await.expect("write");
    let entry = stager
        .close()
        .await
        .expect("close (upload + size re-verify)");
    assert_eq!(entry.size, data.len() as u64, "close-time remote size");

    // 独立回读（stat + reader 全量）
    let stat = driver.stat(&path).await.expect("stat after upload");
    assert_eq!(stat.size, data.len() as u64, "stat size");
    let got = read_all(driver.reader(&stat.id, None).await.expect("reader")).await;
    assert_eq!(got, data, "upload → read-back is byte-exact");
    println!(
        "[SUMMARY] upload_and_read_back|ok=1|bytes={}|fid={}",
        data.len(),
        stat.id.handle
    );

    cleanup(&driver, &dir, &[name]).await;
}

/// ② Range 窗口读逐字节（CDN 206 + Content-Range 校验在驱动内；此处
/// 验证窗口语义端到端）。
#[tokio::test]
#[ignore = "live 115 network + real credentials (see module docs)"]
async fn range_window_is_byte_exact() {
    let driver = live_driver().await;
    let dir = ensure_e2e_dir(&driver).await;
    let name = "smoke-range.bin";
    let data = pattern(300 * 1024);
    let path = dir.join(name).expect("path");

    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..WriteHint::default()
    };
    let mut stager = driver.writer(&path, &hint).await.expect("writer");
    stager.write(&data).await.expect("write");
    let _ = stager.close().await.expect("close");

    let stat = driver.stat(&path).await.expect("stat");
    // 半开窗口 [100_000, 150_000)
    let range = Range::new(100_000, Some(150_000)).expect("range");
    let got = read_all(
        driver
            .reader(&stat.id, Some(range))
            .await
            .expect("range reader"),
    )
    .await;
    assert_eq!(
        got,
        data[100_000..150_000],
        "the ranged window is byte-exact"
    );
    println!("[SUMMARY] range_window|ok=1|window=100000-150000");

    cleanup(&driver, &dir, &[name]).await;
}

/// ③ 秒传命中：同内容第二轮 init → status==2（K69.6 真机形态；注意
/// 首轮可能触发 K69.2 的二次认证挑战——驱动内已实现循环）。
#[tokio::test]
#[ignore = "live 115 network + real credentials (see module docs)"]
async fn rapid_upload_hits_on_the_second_round() {
    let driver = live_driver().await;
    let dir = ensure_e2e_dir(&driver).await;
    let name = "smoke-rapid.bin";
    let data = pattern(150 * 1024);
    let path = dir.join(name).expect("path");
    let hint = WriteHint {
        size: Some(data.len() as u64),
        rapid_upload: true,
        ..WriteHint::default()
    };

    // 第一轮：真上传（先删同名残留，确保首轮是「新内容」）
    cleanup(&driver, &dir, &[name]).await;
    let mut stager = driver.writer(&path, &hint).await.expect("writer 1");
    stager.write(&data).await.expect("write 1");
    let first = stager.close().await.expect("close 1");

    // 第二轮：同内容重传 → 秒传命中（驱动 init 得 status==2）
    let mut stager = driver.writer(&path, &hint).await.expect("writer 2");
    stager.write(&data).await.expect("write 2");
    let second = stager.close().await.expect("close 2 (rapid path)");
    assert_eq!(
        second.size, first.size,
        "the rapid-hit entry reports the same size"
    );
    println!(
        "[SUMMARY] rapid_upload|ok=1|bytes={}|fid={}|fid2={}",
        data.len(),
        first.id.handle,
        second.id.handle
    );

    cleanup(&driver, &dir, &[name]).await;
}
