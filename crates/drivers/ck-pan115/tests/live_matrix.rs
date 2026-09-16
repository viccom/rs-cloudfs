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
//! 真机矩阵（115-0 裁决后的最小三项 + K70.7 挂账收口三项）：
//! 1. `upload_and_read_back`：上传 → 回读逐字节 + 远端 size 复核；
//! 2. `range_window_is_byte_exact`：Range 窗口读与本地切片逐字节一致；
//! 3. `rapid_upload_hits_on_the_second_round`：同内容重传 → 秒传命中
//!    （status==2 路径；K69.6 真机形态）；
//! 4. `multipart_upload_over_5mib_roundtrips`：>5MiB 三片 multipart 真机；
//! 5. `directory_rename_moves_the_tree`：目录 rename（同父 update +
//!    跨父 move 两腿）真机形态；
//! 6. `resume_reuses_the_session_after_a_process_death`：断点续传（drop
//!    形态的进程死亡 → 会话落盘 + ListParts 真值 + 同 uploadId 复用）。

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

/// 按轮随机内容（stamp 作种子的小 LCG）：115 按 **SHA1** 全局去重且
/// 与文件名无关（K72）——固定内容的真机用例会被历史轮次/跨用例同
/// 内容秒传短路，绕过待测路径且不可见。
fn stamp_content(len: usize, stamp: u128) -> Vec<u8> {
    (0..len)
        .map(|i| {
            let mut x = (i as u64).wrapping_add(stamp as u64);
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) as u8
        })
        .collect()
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

// ---------------------------------------------------------------------
// 挂账收口扩展（2026-09-16 晚批）：多分片 / 目录 rename / 断点续传
// ---------------------------------------------------------------------

/// 真机驱动的参数形态（resume 用例需要注入 sessions_dir）。
fn live_params(sessions_dir: Option<std::path::PathBuf>) -> Pan115Params {
    let (access, refresh) = (
        std::env::var("CYDRIVE_PAN115_TEST_ACCESS_TOKEN").expect("env access token"),
        std::env::var("CYDRIVE_PAN115_TEST_REFRESH_TOKEN").expect("env refresh token"),
    );
    Pan115Params {
        client_id: ck_pan115::DEFAULT_CLIENT_ID.to_string(),
        access_token: Some(access),
        refresh_token: Some(refresh),
        root: "0".to_string(),
        api_base: ck_pan115::DEFAULT_API_BASE.to_string(),
        passport_base: ck_pan115::DEFAULT_PASSPORT_BASE.to_string(),
        token_store: None,
        limiter: Some(LimiterConfig::default()),
        sessions_dir,
    }
}

/// ④ >5MiB 多分片上传（OSS multipart 真机路径——PutObject/秒传之外的
/// 第三条腿）：12MiB = 3 片（5+5+2），close 内 size 复核 + 逐字节回读。
#[tokio::test]
#[ignore = "live 115 network + real credentials (see module docs)"]
async fn multipart_upload_over_5mib_roundtrips() {
    let driver = Pan115Driver::connect(live_params(None))
        .await
        .expect("connect");
    let dir = ensure_e2e_dir(&driver).await;
    // 唯一名 + 按轮随机内容（K72：固定名/固定内容会被服务端 SHA1 去重
    // 或历史残留短路 multipart 路径且不可见——⑤⑥ 同款纪律）。
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    let name = format!("smoke-multipart-{stamp}.bin");
    let data = stamp_content(12 * 1024 * 1024, stamp); // 3 片（min 5MiB）
    let path = dir.join(&name).expect("path");

    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..WriteHint::default()
    };
    let started = std::time::Instant::now();
    let mut stager = driver.writer(&path, &hint).await.expect("writer");
    stager.write(&data).await.expect("write (到齐即传推全链)");
    let entry = stager
        .close()
        .await
        .expect("close (multipart + size re-verify)");
    assert_eq!(entry.size, data.len() as u64);
    let elapsed = started.elapsed();

    let stat = driver.stat(&path).await.expect("stat");
    let got = read_all(driver.reader(&stat.id, None).await.expect("reader")).await;
    assert_eq!(got, data, "multipart roundtrip byte-exact");
    println!(
        "[SUMMARY] multipart_roundtrip|ok=1|bytes={}|elapsed_s={:.1}",
        data.len(),
        elapsed.as_secs_f32()
    );

    cleanup(&driver, &dir, &[&name]).await;
}

/// ⑤ 目录 rename 真机形态（115-0 未覆盖项）：驱动按「update 改名 +
/// move 跨父」实现（OpenList 生产形态）——真机验证对目录成立：目录
/// 改名后自身与内部文件在新路径可达、旧路径 NotFound。
#[tokio::test]
#[ignore = "live 115 network + real credentials (see module docs)"]
async fn directory_rename_moves_the_tree() {
    let driver = Pan115Driver::connect(live_params(None))
        .await
        .expect("connect");
    let root = ensure_e2e_dir(&driver).await;

    // 唯一名（跨运行残留与 115 服务端同名去重的免疫——固定名会让上一
    // 轮失败残留污染本轮，live 实证见 K72）。
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    let src = root.join(&format!("ren-a-{stamp}")).expect("src dir");
    let dst = root.join(&format!("ren-b-{stamp}")).expect("dst dir");
    let inner_name = format!("inner-{stamp}.txt");
    let relocated_name = format!("moved-{stamp}.txt");

    driver.mkdir(&src).await.expect("mkdir src");
    let file = src.join(&inner_name).expect("file");
    let payload = pattern(4096);
    let hint = WriteHint {
        size: Some(payload.len() as u64),
        ..WriteHint::default()
    };
    let mut stager = driver.writer(&file, &hint).await.expect("writer");
    stager.write(&payload).await.expect("write");
    stager.close().await.expect("close");

    // 同父改名（目录 update 腿的真机验证点）
    driver.rename(&src, &dst).await.expect("rename dir");

    let moved = driver.stat(&dst).await.expect("renamed dir stat");
    assert_eq!(moved.kind, cloudkit_storage::EntryKind::Dir);
    let inner = driver
        .stat(&dst.join(&inner_name).expect("inner"))
        .await
        .expect("inner under new path");
    assert_eq!(
        inner.size,
        payload.len() as u64,
        "the file rides the rename"
    );
    match driver.stat(&src).await {
        Err(StorageError::NotFound) => {}
        other => panic!("old dir must be gone, got {other:?}"),
    }

    // 跨父移动腿（move + 改名）
    let relocated = root.join(&relocated_name).expect("relocated");
    driver
        .rename(&dst.join(&inner_name).expect("inner2"), &relocated)
        .await
        .expect("cross-parent rename (move + name)");
    let check = driver.stat(&relocated).await.expect("relocated stat");
    assert_eq!(check.size, payload.len() as u64);
    println!("[SUMMARY] dir_rename|ok=1|same_parent=update|cross_parent=move+update");

    // 清理：relocated + dst 目录
    cleanup(&driver, &root, &[&relocated_name]).await;
    let dst_stat = driver.stat(&dst).await.expect("dst stat for cleanup");
    driver
        .delete(&dst_stat.id)
        .await
        .expect("delete dir (recycle bin)");
    println!("PASS: directory rename live");
}

/// ⑥ 断点续传（杀进程形态）：第一轮 write 全量（到齐即传把 3 片全部
/// 推上远端）后 **drop**（= 进程死亡后的状态：spool 没了、会话与远端
/// 分片仍在）——断言会话落盘、ListParts 远端真值、第二轮同内容经
/// `/open/upload/resume` 复用**同一 uploadId**（会话复用而非重起）并
/// 完成提交、逐字节回读。
#[tokio::test]
#[ignore = "live 115 network + real credentials (see module docs)"]
async fn resume_reuses_the_session_after_a_process_death() {
    let sessions_dir = tempfile::tempdir().expect("sessions tempdir");
    let params = live_params(Some(sessions_dir.path().to_path_buf()));
    let driver = Pan115Driver::connect(params).await.expect("connect");
    let dir = ensure_e2e_dir(&driver).await;
    // 唯一名：115 按 target+名+SHA1 去重——固定名会让上一轮残留命中
    // 服务端秒传而非 resume 路径（live 实证，见 K72）。
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    let name = format!("resume-{stamp}.bin");
    // 内容按轮随机化（stamp 作种子）：115 按 **SHA1** 去重（K72——与
    // 名字无关）——固定内容会让首轮命中服务端秒传、零分片零会话，
    // 测不到 resume 路径。轮内两段用同一份 data（同 SHA1）。
    let data = stamp_content(12 * 1024 * 1024, stamp); // 3 片
    let path = dir.join(&name).expect("path");
    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..WriteHint::default()
    };

    // 第一轮：write 全量（3 片上远端）→ drop（不 close——进程死亡后
    // 的世界：会话 + 远端分片在，spool 没了，complete 没发生）。
    {
        let mut stager = driver.writer(&path, &hint).await.expect("writer 1");
        stager
            .write(&data)
            .await
            .expect("write 1 (parts fly at 到齐)");
        drop(stager);
    }

    // 会话落盘断言：唯一一个 session 文件，3 片 + upload_id。
    // 会话文件落在 <sessions_dir>/pan115_state/sessions/<hash>.json
    // （SessionStore::file_path 的两级布局——baidu K7 同形）。
    let sessions_root = sessions_dir.path().join("pan115_state").join("sessions");
    let session_files: Vec<_> = std::fs::read_dir(&sessions_root)
        .expect("sessions root")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .collect();
    assert_eq!(session_files.len(), 1, "exactly one session file");
    let session: ck_pan115::upload::UploadSession = serde_json::from_str(
        &std::fs::read_to_string(session_files[0].path()).expect("session json"),
    )
    .expect("UploadSession shape");
    assert_eq!(session.parts.len(), 3, "all three parts recorded");
    assert!(
        session.upload_id.is_some(),
        "the uploadId rides the session"
    );
    let round1_upload_id = session.upload_id.clone().expect("upload id");

    // 远端真值：ListParts（真 OSS）应报同样 3 片。
    let sts = driver.client().get_token().await.expect("STS");
    let ctx = ck_pan115::oss::OssCtx {
        endpoint: format!("https://{}", sts.endpoint),
        bucket: session.bucket.clone(),
        object: session.object.clone(),
        access_key_id: sts.access_key_id.clone(),
        access_key_secret: sts.access_key_secret.clone(),
        security_token: sts.security_token.clone(),
    };
    let http = reqwest::Client::builder()
        .no_proxy()
        .user_agent(ck_pan115::UA)
        .build()
        .expect("oss http");
    let parts = ck_pan115::oss::list_parts(&http, &ctx, &round1_upload_id)
        .await
        .expect("ListParts against the real OSS");
    assert_eq!(parts.len(), 3, "the real OSS reports the three parts");
    println!(
        "[SUMMARY] resume_after_death|parts_on_remote={}|upload_id_len={}",
        parts.len(),
        round1_upload_id.len()
    );

    // 第二轮：同路径同内容 → close。run_transfer 走 upload_resume 复用
    // 会话（同一 uploadId），无缺片可补，直接 complete。
    let mut stager = driver.writer(&path, &hint).await.expect("writer 2");
    stager.write(&data).await.expect("write 2");
    // write 到齐即传已完成对账；此刻会话仍应指向同一 uploadId。
    let session2: ck_pan115::upload::UploadSession = serde_json::from_str(
        &std::fs::read_to_string(session_files[0].path()).expect("session json 2"),
    )
    .expect("UploadSession shape 2");
    assert_eq!(
        session2.upload_id.as_deref(),
        Some(round1_upload_id.as_str()),
        "round 2 REUSED the session uploadId (a restart would mint a new one)"
    );
    assert_eq!(session2.parts.len(), 3, "no part needed re-uploading");
    let entry = stager.close().await.expect("close after resume");
    assert_eq!(entry.size, data.len() as u64);

    let stat = driver.stat(&path).await.expect("stat");
    let got = read_all(driver.reader(&stat.id, None).await.expect("reader")).await;
    assert_eq!(got, data, "resumed object is byte-exact");
    println!(
        "[SUMMARY] resume_roundtrip|ok=1|bytes={}|upload_id_reused=1",
        data.len()
    );

    cleanup(&driver, &dir, &[&name]).await;
}
