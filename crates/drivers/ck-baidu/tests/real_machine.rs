//! 真机套件（`#[ignore]`——Batch B2 骨架，真机窗口由主会话执行）。
//!
//! 凭据注入（R3：**任何凭据值绝不硬编码/不入日志**——token 打印一律
//! 掩码前 6 后 4）：
//!
//! - `CYDRIVE_BAIDU_TEST_CONFIG`：json 路径（兼容两种形态：顶层
//!   `config.{access_token,refresh_token}` 嵌套或顶层平铺；app_key/
//!   app_secret 可经同文件或 env 补齐）；或
//! - env 四件套 `CYDRIVE_BAIDU_APP_KEY` / `CYDRIVE_BAIDU_APP_SECRET` /
//!   `CYDRIVE_BAIDU_ACCESS_TOKEN` / `CYDRIVE_BAIDU_REFRESH_TOKEN`；
//! - `CYDRIVE_BAIDU_TEST_ROOT`：测试根（缺省 `/apps/cloudfs-b2`——与
//!   生产根 `/apps/cloudfs` 及 E2E 根 `/apps/cloudfs-e2e` 隔离）。
//!
//! 真机断言集（对应计划 §4 验收）：
//! - refresh → uinfo → 卷身份（connect 隐含刷新链路）；
//! - 3 分片上传往返 + **rtype=3 覆盖复核（K10）**：同路径重传 → stat
//!   size/内容更新且**无 `_2026…` 冲突重命名副本**（spike §3 备注 5 的
//!   rtype=1 反例即复核基准）；
//! - Range 语义抽查（半开窗口）；
//! - cleanup 删除测试根 + errno=-9 复查（spike cleanup 同款收尾）。

mod common;

use std::sync::Arc;

use ck_baidu::{factory, BaiduDriver, BaiduParams};
use cloudkit_storage::{EntryKind, Range, RelPath, StorageDriver, WriteHint};
use futures_util::StreamExt;

/// 真机凭据（从 env/config 装配；缺失 → panic 给可行动指引）。
struct RealCreds {
    app_key: String,
    app_secret: String,
    access_token: String,
    refresh_token: String,
}

fn env_or_die(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!(
            "真机套件需要 env {name}（或 CYDRIVE_BAIDU_TEST_CONFIG 指向凭据 json）——\
             参见 tests/real_machine.rs 模块文档"
        )
    })
}

fn load_creds() -> RealCreds {
    if let Ok(path) = std::env::var("CYDRIVE_BAIDU_TEST_CONFIG") {
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("读取 CYDRIVE_BAIDU_TEST_CONFIG={path}: {e}"));
        let v: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("凭据 json 解析失败（{path}）: {e}"));
        let cfg = v.get("config").unwrap_or(&v);
        let pick = |k: &str| {
            let from_cfg = cfg.get(k).and_then(|x| x.as_str()).map(str::to_string);
            from_cfg.or_else(|| std::env::var(format!("CYDRIVE_BAIDU_{}", k.to_uppercase())).ok())
        };
        return RealCreds {
            app_key: pick("app_key").expect("json/env 均缺 app_key"),
            app_secret: pick("app_secret").expect("json/env 均缺 app_secret"),
            access_token: pick("access_token").expect("json/env 均缺 access_token"),
            refresh_token: pick("refresh_token").expect("json/env 均缺 refresh_token"),
        };
    }
    RealCreds {
        app_key: env_or_die("CYDRIVE_BAIDU_APP_KEY"),
        app_secret: env_or_die("CYDRIVE_BAIDU_APP_SECRET"),
        access_token: env_or_die("CYDRIVE_BAIDU_ACCESS_TOKEN"),
        refresh_token: env_or_die("CYDRIVE_BAIDU_REFRESH_TOKEN"),
    }
}

/// 凭据掩码（前 6 后 4；短值整体隐去——R3）。
fn mask(secret: &str) -> String {
    let n = secret.chars().count();
    if n >= 12 {
        let head: String = secret.chars().take(6).collect();
        let tail: String = secret.chars().skip(n - 4).collect();
        format!("{head}...{tail}")
    } else {
        "***".to_string()
    }
}

fn real_params(creds: &RealCreds, root: &str) -> BaiduParams {
    // K7 会话表根（真机亦启用差集续传；临时目录，进程隔离）。
    let sessions_dir = std::env::temp_dir().join(format!("cloudfs-b2-real-{}", std::process::id()));
    let mut params = BaiduParams {
        app_key: creds.app_key.clone(),
        app_secret: creds.app_secret.clone(),
        access_token: Some(creds.access_token.clone()),
        refresh_token: Some(creds.refresh_token.clone()),
        root: root.to_string(),
        ..Default::default()
    };
    params.sessions_dir = Some(sessions_dir);
    params
}

fn test_root() -> String {
    std::env::var("CYDRIVE_BAIDU_TEST_ROOT").unwrap_or_else(|_| "/apps/cloudfs-b2".to_string())
}

async fn real_driver() -> Arc<BaiduDriver> {
    let creds = load_creds();
    println!(
        "[real] app_key={} access_token={}（掩码，R3）",
        mask(&creds.app_key),
        mask(&creds.access_token)
    );
    factory(&real_params(&creds, &test_root()))
        .await
        .expect("真机 connect（refresh 链路 + uinfo）")
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

/// 真机独有路径 helper：测试根下建日期唯一子目录，跨跑互不污染。
fn unique_sub(name: &str) -> RelPath {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    RelPath::new(&format!("{name}-{stamp}")).expect("rel path")
}

#[tokio::test]
#[ignore = "真机套件：需 env 凭据 + 生产 endpoint（主会话真机窗口执行）"]
async fn real_connect_refreshes_and_resolves_baidu_volume() {
    let driver = real_driver().await;
    let volume = driver.volume().as_str().to_string();
    assert!(
        volume.starts_with("baidu:"),
        "卷形态 baidu:<uid>（K5），实得 {volume}"
    );
    println!("[real] connect ok | volume={volume}");
    // quota 探活（附加冒烟：凭据链路全通）。
    let quota = driver.quota().await.expect("真机 quota");
    println!("[real] quota | used={} total={:?}", quota.used, quota.total);
}

#[tokio::test]
#[ignore = "真机套件：需 env 凭据 + 生产 endpoint（主会话真机窗口执行）"]
async fn real_upload_roundtrip_rtype3_overwrites_without_rename_copy() {
    let driver = real_driver().await;
    let dir = unique_sub("rtype3");
    let path = dir.join("f.bin").expect("join");

    // 3 分片随机内容（首片随机防秒传——spike §3 同款防抖）。
    let data = common::pattern_bytes(3 * common::CHUNK_4M);
    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..Default::default()
    };
    let mut stager = driver.writer(&path, &hint).await.expect("真机 writer");
    stager.write(&data).await.expect("真机 write 3 分片");
    let entry = stager.close().await.expect("真机 close 三步曲");
    assert_eq!(entry.size, data.len() as u64);

    let st = driver.stat(&path).await.expect("上传后 stat");
    assert_eq!(st.size, data.len() as u64, "stat size 与上传一致");
    assert_eq!(st.kind, EntryKind::File);

    // Range 抽查：中段半开窗口。
    let mid = data.len() / 2;
    let window = &data[mid..mid + 65536];
    let got = read_all(
        driver
            .reader(
                &entry.id,
                Some(Range::new(mid as u64, Some((mid + 65536) as u64)).expect("range")),
            )
            .await
            .expect("真机 range reader"),
    )
    .await;
    assert_eq!(got, window, "Range 半开窗口抽查逐字节匹配");

    // rtype=3 覆盖复核（K10）：同路径重传不同长度内容 → 覆盖而非重命名。
    let data2 = common::pattern_bytes(2 * common::CHUNK_4M + 99);
    let hint2 = WriteHint {
        size: Some(data2.len() as u64),
        ..Default::default()
    };
    let mut stager2 = driver.writer(&path, &hint2).await.expect("覆盖 writer");
    stager2.write(&data2).await.expect("覆盖 write");
    let entry2 = stager2.close().await.expect("覆盖 close");
    assert_eq!(entry2.size, data2.len() as u64, "覆盖后 size 更新");

    let st2 = driver.stat(&path).await.expect("覆盖后 stat");
    assert_eq!(
        st2.size,
        data2.len() as u64,
        "覆盖后 stat size = 新内容长度（rtype=3 生效）"
    );

    // 无 `_2026…` 冲突重命名副本（rtype=1 反例形态——spike §3 备注 5）。
    let listing = driver
        .list(&dir, cloudkit_storage::Page::all())
        .await
        .expect("列目录查副本");
    assert_eq!(
        listing.entries.len(),
        1,
        "同路径重传后目录内恰一个条目（无重命名副本）"
    );
    assert_eq!(listing.entries[0].path, path);

    // 覆盖内容抽查（中段窗口换新内容对账）。
    let mid2 = data2.len() / 2;
    let got2 = read_all(
        driver
            .reader(
                &entry2.id,
                Some(Range::new(mid2 as u64, Some((mid2 + 65536) as u64)).expect("range")),
            )
            .await
            .expect("覆盖后 range reader"),
    )
    .await;
    assert_eq!(got2, &data2[mid2..mid2 + 65536], "覆盖内容生效");

    // cleanup：删本目录（递归）。
    let dir_entry = driver.stat(&dir).await.expect("stat 目录");
    driver.delete(&dir_entry.id).await.expect("cleanup 删除");
    match driver.stat(&dir).await {
        Err(cloudkit_storage::StorageError::NotFound) => {}
        res => panic!("cleanup 后目录必须 NotFound，实得 {res:?}"),
    }
}

#[tokio::test]
#[ignore = "真机套件：需 env 凭据 + 生产 endpoint（主会话真机窗口执行）"]
async fn real_throughput_100mb_roundtrip_optional() {
    // 可选吞吐记录（计划 §4 验收「≥100MB 往返吞吐记录」）：100MB 上传+
    // 下载计时（4MiB 分片×4 并发的实际吞吐——spike §6 对照：上 27.4/
    // 下 21.7 MB/s）。
    let driver = real_driver().await;
    let dir = unique_sub("throughput");
    let path = dir.join("bench.bin").expect("join");
    let size = 100 * 1024 * 1024;
    let data = common::pattern_bytes(size);

    let t0 = std::time::Instant::now();
    let hint = WriteHint {
        size: Some(size as u64),
        ..Default::default()
    };
    let mut stager = driver.writer(&path, &hint).await.expect("writer");
    stager.write(&data).await.expect("write 100MB");
    stager.close().await.expect("close");
    let up = t0.elapsed();

    let t1 = std::time::Instant::now();
    let entry = driver.stat(&path).await.expect("stat");
    let got = read_all(driver.reader(&entry.id, None).await.expect("reader")).await;
    let down = t1.elapsed();
    assert_eq!(got.len(), size, "下载字节数一致");

    println!(
        "[real] throughput | up={:.1}s ({:.1} MB/s) | down={:.1}s ({:.1} MB/s)",
        up.as_secs_f32(),
        size as f32 / 1024.0 / 1024.0 / up.as_secs_f32(),
        down.as_secs_f32(),
        size as f32 / 1024.0 / 1024.0 / down.as_secs_f32(),
    );

    // cleanup。
    let dir_entry = driver.stat(&dir).await.expect("stat 目录");
    driver.delete(&dir_entry.id).await.expect("cleanup");
}
