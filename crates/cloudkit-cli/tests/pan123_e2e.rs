//! pan123 真机 E2E（Phase 6 / 123-5）：**加密全栈** + **WebDAV 双模式**
//! + **rebuild 收敛** + **setup 流真机探测**——五腿都是 `#[ignore]`
//!   真网测试，凭据只经 env / 仓外测试文件（R3）。
//!
//! ```text
//! CYDRIVE_PAN123_TEST_TOKEN=... [CYDRIVE_PAN123_TEST_ROOT=<专用测试目录 fid>] \
//!   cargo test -p cloudkit-cli --test pan123_e2e -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! - [`pan123_vfs_aead_v2_full_stack_roundtrip`]：VFS(aead_v2) + 真驱动
//!   ——写明文 → 队列排空 → 行元数据（uploaded/encrypted/scheme/明文
//!   size）→ hydrate 逐字节 → open_read 跨块窗口解密 → **远端侧核验
//!   密文**（驱动直读远端对象：≠ 明文、明文特征段不出现）。镜像 K54
//!   `tg_vfs_e2e.rs` / pan115_e2e 形态。
//! - [`pan123_webdav_roundtrip_plain_and_encrypted`]：`run_with_transport`
//!   起 WebDAV（`:0` 临时端口，auto-mount/web-ui 关）——明文与加密各
//!   一轮：MKCOL → PUT → （上传排空后）GET 逐字节 → PROPFIND 207 →
//!   收尾 driver 删远端（D2 回收站语义）+ NotFound 核验。
//! - [`pan123_rebuild_converges_under_the_rate_limit`]：rebuild 走
//!   driver 卷根（**fresh `rb-<stamp>` 专用子目录为根**——K74 教训：
//!   绝不用 "0"/共享目录，计数只含本轮种子）→ outcome 对账 + 实例
//!   db 行核对 + 2rps 限速下的耗时记录。
//! - [`pan123_setup_flow_endpoints_reach_the_live_service`]：QR 腿
//!   （generate → 终端渲染 → 一次轮询 Waiting → 弃码；确认态需人扫，
//!   交互面由 `cydrive setup` 的人工/tmux 运行覆盖）。
//! - [`pan123_setup_sign_in_path_with_the_test_account`]：sign_in 腿用
//!   测试账密走通（凭据从仓外 json 运行期读取，绝不打印/落日志）→
//!   换发 token 立即 user_info 验证——向导核心链路的真机钉。
//!
//! 作业纪律：文件只出现在 `_e2e_pan123/`（卷根 = 专用测试目录）；K72
//! stamp 唯一名 + 按轮随机内容；驱动侧限速 = 生产缺省（2rps）。

#![cfg(feature = "pan123")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ck_pan123::oauth;
use ck_pan123::{Pan123Driver, Pan123Params, Pan123Transport};
use cloudkit_core::cache::CacheManager;
use cloudkit_core::config::{Backend, CyDriveConfig, EncryptionScheme};
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{StreamSource, Vfs, VfsConfig};
use cloudkit_storage::transport::CloudTransport;
use cloudkit_storage::vpath::RelPath as VPath;
use cloudkit_storage::{RelPath, StorageDriver, StorageError};

const MIB: u64 = 1024 * 1024;
const PHASE_TIMEOUT: Duration = Duration::from_secs(600);
/// 上传队列排空轮询：2rps 限速下七步链（request/list/presign/PUT×n/
/// list/complete/v2）按秒计，120s 覆盖含重试的慢形态。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(120);
/// 专用作业目录（全部真机文件只在这里出现；卷根即专用测试目录）。
const E2E_DIR: &str = "_e2e_pan123";

// ------------------------------------------------------------- harness --

fn env_token() -> String {
    std::env::var("CYDRIVE_PAN123_TEST_TOKEN")
        .expect("set CYDRIVE_PAN123_TEST_TOKEN (see the module docs)")
}

fn env_root() -> String {
    std::env::var("CYDRIVE_PAN123_TEST_ROOT").unwrap_or_else(|_| "0".to_string())
}

fn live_params_with_root(root: &str) -> Pan123Params {
    Pan123Params {
        token: Some(env_token()),
        root: root.to_string(),
        ..Pan123Params::default()
    }
}

/// 真机驱动（生产端点 + 生产限速 2rps；connect 读 uid 定卷身份）。
async fn live_driver() -> Arc<Pan123Driver> {
    Arc::new(
        Pan123Driver::connect(live_params_with_root(&env_root()))
            .await
            .expect("live connect"),
    )
}

fn unix_stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis()
}

/// 确定性伪随机载荷（pan115_e2e 同款 LCG；64 位状态——绝不与历史
/// 上传撞 etag 触发服务端 Reuse，K72）。
fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

async fn read_all(mut stream: cloudkit_storage::ByteStream) -> Vec<u8> {
    use futures_util::StreamExt;
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("chunk"));
    }
    out
}

/// 远端清理：driver 删除（D2 = 进回收站）+ NotFound 核验。
async fn cleanup_remote(driver: &Pan123Driver, rel: &str) {
    let path = RelPath::new(rel).expect("rel path");
    match driver.stat(&path).await {
        Ok(entry) => {
            driver.delete(&entry.id).await.expect("remote delete");
        }
        Err(StorageError::NotFound) => {}
        Err(other) => panic!("cleanup stat {rel}: {other}"),
    }
    match driver.stat(&path).await {
        Err(StorageError::NotFound) => {}
        other => panic!("cleanup verify {rel}: still present, got {other:?}"),
    }
}

// ------------------------------------------------- 1. 加密全栈（K54 形态） --

#[tokio::test]
#[ignore = "real network: needs the live pan123 token from env; run with --ignored --test-threads=1"]
async fn pan123_vfs_aead_v2_full_stack_roundtrip() {
    let driver = live_driver().await;
    let transport: Arc<dyn CloudTransport> = Arc::new(Pan123Transport::new(driver.clone()));

    let data_dir = tempfile::tempdir().expect("tempdir for db/cache");
    let db = Arc::new(MetaDatabase::open(&data_dir.path().join("meta.db")).expect("db"));
    let cache_root = data_dir.path().join("cache");
    let password = format!("pan123-e2e-{}", unix_stamp());
    let cfg = VfsConfig {
        chunk_size_bytes: MIB,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(5),
            max_attempts: 3,
        },
        encryption_password: Some(password),
        encryption_scheme: EncryptionScheme::AeadV2,
        hydrate_timeout: Duration::from_secs(300),
    };
    let vfs = Arc::new(Vfs::new(
        Arc::clone(&db),
        CacheManager::new(cache_root.clone(), 1 << 30),
        Arc::clone(&transport),
        cfg,
    ));

    // 1.5 MiB：跨两个 1MiB VFS 块——v2 容器里是两段密文块 + tag。
    const SIZE: usize = (MIB + MIB / 2) as usize;
    let plaintext = pseudo_random(SIZE, 0x7E61);
    let rel_str = format!("{E2E_DIR}/e2e-{}-enc.bin", unix_stamp());
    let rel = VPath::new(&format!("/{rel_str}")).expect("vfs path (vpath)");
    let drv_rel = RelPath::new(&rel_str).expect("driver path (vocab)");

    let body = tokio::time::timeout(PHASE_TIMEOUT, async {
        vfs.put(&rel, &plaintext, 1.0).await.expect("vfs put");
        vfs.shutdown().await; // 排空上传队列到终态

        // 行元数据：uploaded / encrypted / scheme / 明文 size。
        let row = db
            .get_file(rel.as_str())
            .expect("db read")
            .expect("row exists");
        assert!(row.is_uploaded, "row uploaded after drain");
        assert!(row.is_encrypted, "row flagged encrypted");
        assert_eq!(row.encryption_scheme, "aead_v2", "row scheme");
        assert_eq!(row.size, SIZE as i64, "row size = plaintext size");
        let chunks = db.get_chunks_by_file_id(row.id).expect("chunks");
        eprintln!(
            "[enc] row: uploaded encrypted aead_v2 size={SIZE} chunks={}",
            chunks.len()
        );

        // hydrate：清本地缓存副本 → 字节必须经真驱动往返并解密。
        let cache = CacheManager::new(cache_root.clone(), u64::MAX);
        let _ = std::fs::remove_file(cache.local_path(&rel));
        let path = vfs.hydrate(&rel).await.expect("hydrate");
        let roundtrip = std::fs::read(&path).expect("read hydrated");
        assert_eq!(roundtrip, plaintext, "hydrate byte-exact through decrypt");
        eprintln!("[enc] hydrate: {SIZE} bytes byte-exact");

        // open_read：跨第一块边界的窗口解密（流式形态）。
        let _ = std::fs::remove_file(cache.local_path(&rel));
        match vfs.open_read(&rel).await.expect("open_read") {
            StreamSource::Stream {
                handle,
                total_size,
                transport,
            } => {
                assert_eq!(total_size, SIZE as u64, "stream total_size");
                let off = MIB - 4096;
                let len = 8192usize;
                let stream = transport
                    .open_range(&handle, off, len as u64)
                    .await
                    .expect("open_range");
                let got = read_all(stream).await;
                assert_eq!(
                    got,
                    plaintext[off as usize..off as usize + len],
                    "range window decrypt byte-exact"
                );
                eprintln!("[enc] open_read: window [{off},+{len}) decrypted byte-exact");
            }
            StreamSource::Hydrate => {
                panic!("open_read routed an aead_v2 row to Hydrate — streaming shape not admitted");
            }
        }

        // **远端密文核验**：绕过 VFS 的解密面，驱动直读远端对象——必须
        // 是密文（≠ 明文，且明文特征段不出现）+ v2 容器开销。
        let stat = driver.stat(&drv_rel).await.expect("remote stat");
        let remote = read_all(driver.reader(&stat.id, None).await.expect("remote read")).await;
        assert_ne!(remote, plaintext, "remote object must not be the plaintext");
        assert!(
            remote.len() > SIZE,
            "v2 container carries per-chunk overhead: remote {} vs plaintext {SIZE}",
            remote.len()
        );
        let marker = &plaintext[..64.min(SIZE)];
        assert!(
            !remote.windows(64).any(|w| w == marker),
            "the plaintext's first 64 bytes must not appear anywhere in the remote object"
        );
        eprintln!(
            "[enc] remote: ciphertext confirmed (remote {} B vs plaintext {SIZE} B)",
            remote.len()
        );

        Ok::<(), String>(())
    })
    .await
    .unwrap_or_else(|_| Err("vfs scenario timed out".to_owned()));

    // 清理在超时外执行（远端对象必在——上传已终态）。
    cleanup_remote(&driver, &rel_str).await;
    body.unwrap_or_else(|error| panic!("pan123 aead_v2 full-stack scenario failed: {error}"));
    eprintln!("PASS: pan123 vfs aead_v2 full-stack roundtrip (remote cleaned)");
}

// ------------------------------------------------ 2. WebDAV E2E（双模式） --

/// 进程 cwd 守卫（控制通道端口文件等落在临时目录；Windows 不许删
/// cwd，Drop 先还原再由 TempDir 回收）。
struct CwdGuard {
    prev: PathBuf,
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.prev).expect("restore cwd");
    }
}

fn chdir(dir: &Path) -> CwdGuard {
    let prev = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(dir).expect("chdir");
    CwdGuard { prev }
}

/// 一次 WebDAV 轮：起栈 → MKCOL → PUT → 等上传终态 → GET 逐字节 →
/// PROPFIND。
async fn webdav_round(encrypt: bool, driver: &Pan123Driver) -> (String, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = tempfile::tempdir().expect("tempdir for the boot");
    let _cwd = chdir(dir.path());
    let stamp = unix_stamp();
    let name = format!(
        "{}-{}.bin",
        if encrypt { "wd-enc" } else { "wd-plain" },
        stamp
    );
    let rel_str = format!("{E2E_DIR}/{name}");

    let cfg = CyDriveConfig {
        backend: Backend::Pan123,
        pan123_token: Some(env_token()),
        pan123_root: Some(env_root()),
        db_path: "meta.db".to_string(),
        cache_path: "cache".to_string(),
        webdav_port: 0,
        auto_mount_drive: false,
        enable_web_ui: false,
        enable_encryption: encrypt,
        encryption_password: encrypt.then(|| format!("pan123-wd-{stamp}")),
        encryption_scheme: EncryptionScheme::AeadV2,
        ..CyDriveConfig::default()
    };
    // 注：不调 cfg.validate()——它按生产口径拒绝 webdav_port=0，而
    // run 流程的 :0 = 临时端口（run_e2e.rs 同款 boot 形态）。

    let transport: Arc<dyn CloudTransport> = Arc::new(Pan123Transport::new(Arc::new(
        // 每轮独立驱动实例（独立队列世界；同 token/限速参数）。
        Pan123Driver::connect(live_params_with_root(&env_root()))
            .await
            .expect("round connect"),
    )));
    let handle = cloudkit_cli::run_with_transport(&cfg, transport)
        .await
        .expect("boot the stack");
    let addr = handle.local_addr();
    assert_ne!(addr.port(), 0, ":0 resolves to the bound port");

    // MKCOL 父集合（RFC 4918：PUT 到父集合不存在的路径 → 409；VFS 面
    // 会隐式建父，但 HTTP 面按标准语义要求先建集合）。
    let mkcol = format!(
        "MKCOL /{E2E_DIR}/ HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        addr.port()
    );
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect MKCOL");
    stream
        .write_all(mkcol.as_bytes())
        .await
        .expect("send MKCOL");
    let mut resp = Vec::new();
    stream
        .read_to_end(&mut resp)
        .await
        .expect("read MKCOL resp");
    let text = String::from_utf8_lossy(&resp);
    assert!(
        text.starts_with("HTTP/1.1 201") || text.starts_with("HTTP/1.1 405"),
        "MKCOL parent: 201 created or 405 already exists, got: {text}"
    );

    // PUT：256 KiB 确定性载荷（Content-Length 定界）。
    let payload = pseudo_random(
        256 * 1024,
        (if encrypt { 0xE11C } else { 0x91A7 }) ^ (stamp as u64),
    );
    let put = format!(
        "PUT /{rel_str} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        addr.port(),
        payload.len()
    );
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(put.as_bytes())
        .await
        .expect("send PUT head");
    stream.write_all(&payload).await.expect("send PUT body");
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).await.expect("read PUT resp");
    let resp_text = String::from_utf8_lossy(&resp);
    let status: u16 = resp_text
        .lines()
        .next()
        .expect("status line")
        .split_whitespace()
        .nth(1)
        .expect("code")
        .parse()
        .expect("u16");
    assert_eq!(status, 201, "PUT created: {resp_text}");

    // 等上传终态（2rps 链条数秒）——轮询远端可见性，随后 GET 才有
    // 稳定的远端语义。
    let deadline = tokio::time::Instant::now() + DRAIN_TIMEOUT;
    loop {
        let rel = RelPath::new(&rel_str).expect("rel");
        match driver.stat(&rel).await {
            Ok(_) => break,
            Err(StorageError::NotFound) => {}
            Err(other) => panic!("poll remote stat: {other}"),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "upload did not reach the remote within {:?}",
            DRAIN_TIMEOUT
        );
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }

    // GET：逐字节（加密轮 = HTTP 面拿到解密后的明文）。
    let get = format!(
        "GET /{rel_str} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        addr.port()
    );
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect GET");
    stream.write_all(get.as_bytes()).await.expect("send GET");
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).await.expect("read GET resp");
    let sep = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header/body separator");
    let head = String::from_utf8_lossy(&resp[..sep]).to_string();
    let status: u16 = head
        .lines()
        .next()
        .expect("status line")
        .split_whitespace()
        .nth(1)
        .expect("code")
        .parse()
        .expect("u16");
    assert_eq!(status, 200, "GET ok: {head}");
    let body = resp[sep + 4..].to_vec();
    assert_eq!(
        body,
        payload,
        "GET body byte-exact ({} mode)",
        if encrypt { "encrypted" } else { "plain" }
    );
    eprintln!(
        "[wd-{}] GET {} bytes byte-exact",
        if encrypt { "enc" } else { "plain" },
        body.len()
    );

    // PROPFIND 作业集合（Depth:1 列直接子项）。
    let propfind = format!(
        "PROPFIND /{E2E_DIR}/ HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nDepth: 1\r\nContent-Length: 0\r\n\r\n",
        addr.port()
    );
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect PF");
    stream
        .write_all(propfind.as_bytes())
        .await
        .expect("send PF");
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).await.expect("read PF resp");
    let text = String::from_utf8_lossy(&resp);
    assert!(text.starts_with("HTTP/1.1 207"), "PROPFIND 207: {text}");
    assert!(text.contains(&name), "listing carries the uploaded name");

    handle.shutdown().await;
    (rel_str, payload)
}

#[tokio::test]
#[ignore = "real network: needs the live pan123 token from env; run with --ignored --test-threads=1"]
async fn pan123_webdav_roundtrip_plain_and_encrypted() {
    let driver = live_driver().await;

    // 明文轮
    let (plain_rel, _) = webdav_round(false, &driver).await;
    eprintln!("PASS: webdav plain round");

    // 加密轮（aead_v2 + 随机口令；GET 面验证解密往返）
    let (enc_rel, _) = webdav_round(true, &driver).await;
    eprintln!("PASS: webdav encrypted round");

    // 收尾：两轮远端对象删除（D2 回收站）+ 核验
    cleanup_remote(&driver, &plain_rel).await;
    cleanup_remote(&driver, &enc_rel).await;
    eprintln!("PASS: pan123 webdav roundtrip (plain + encrypted, remote cleaned)");
}

// --------------------------------------- 3. rebuild 收敛（限速下） ---

/// rebuild 真机收敛：fresh `rb-<stamp>` 子目录为 scoped 卷根 → 种 2 文件
/// + 1 子目录 → `run_rebuild_with_driver` 走全树 → outcome 计数与种子
/// 一致（**fresh 根保证计数不混入他轮残留**——K74 scoping 教训的 123
/// 形态）→ 实例 db 行对账 → **2rps 限速下的耗时记录**。
#[tokio::test]
#[ignore = "real network: needs the live pan123 token from env; run with --ignored --test-threads=1"]
async fn pan123_rebuild_converges_under_the_rate_limit() {
    // bootstrap：unscoped driver 建 fresh 作业根，随后换 scoped driver
    // （rebuild 从 driver 的卷根走——run_rebuild_with_driver 契约）。
    let boot = live_driver().await;
    let stamp = unix_stamp();
    let scope_name = format!("{E2E_DIR}/rb-{stamp}");
    let scope = RelPath::new(&scope_name).expect("scope path");
    boot.mkdir(&scope).await.expect("mkdir scope");
    let scope_stat = boot.stat(&scope).await.expect("scope stat");
    let scope_fid = scope_stat.id.handle.as_str().to_string();
    drop(boot);

    let driver = Arc::new(
        Pan123Driver::connect(live_params_with_root(&scope_fid))
            .await
            .expect("scoped connect"),
    );

    // 种子：stamp 唯一名 + 按轮随机内容（K72）。scoped 根内裸名。
    let mut seeded: Vec<(String, Vec<u8>)> = Vec::new();
    for (i, (base, size)) in [("alpha", 1024usize), ("beta", 4096)]
        .into_iter()
        .enumerate()
    {
        let name = format!("{base}-{stamp}.bin");
        let data = pseudo_random(size, stamp as u64 ^ (i as u64 + 1));
        let rel = RelPath::new(&name).expect("file path");
        let hint = cloudkit_storage::WriteHint {
            size: Some(data.len() as u64),
            ..Default::default()
        };
        let mut stager = driver.writer(&rel, &hint).await.expect("writer");
        stager.write(&data).await.expect("write");
        stager.close().await.expect("close");
        seeded.push((name, data));
    }
    let sub = RelPath::new("sub").expect("sub path");
    driver.mkdir(&sub).await.expect("mkdir sub");

    // rebuild 配置：卷根 = scope fid；实例 db 在临时目录。
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = CyDriveConfig {
        backend: Backend::Pan123,
        pan123_token: Some(env_token()),
        pan123_root: Some(scope_fid.clone()),
        db_path: dir.path().join("meta.db").to_string_lossy().into_owned(),
        cache_path: dir.path().join("cache").to_string_lossy().into_owned(),
        ..CyDriveConfig::default()
    };

    let started = std::time::Instant::now();
    let outcome = cloudkit_cli::run_rebuild_with_driver(&cfg, driver.as_ref())
        .await
        .expect("rebuild over the live backend");
    let elapsed = started.elapsed();

    // 对账：fresh 根内恰为 2 files + 1 dir。
    assert_eq!(outcome.files, 2, "two seeded files rebuilt: {outcome:?}");
    assert_eq!(outcome.dirs, 1, "one seeded dir rebuilt: {outcome:?}");

    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("reopen db");
    for (name, data) in &seeded {
        let row = db
            .get_file(&format!("/{name}"))
            .expect("db read")
            .unwrap_or_else(|| panic!("row for {name} missing"));
        assert_eq!(row.size, data.len() as i64, "{name} size");
    }
    println!(
        "[SUMMARY] rebuild_converge|files={}|dirs={}|elapsed_s={:.1}|rate=2rps",
        outcome.files,
        outcome.dirs,
        elapsed.as_secs_f32()
    );

    // 清理（D2 回收站）——经 unscoped 面逐件 + 根目录本身。
    let boot = live_driver().await;
    for (name, _) in &seeded {
        let rel = RelPath::new(&format!("{scope_name}/{name}")).expect("rel");
        let entry = boot.stat(&rel).await.expect("seed stat");
        boot.delete(&entry.id).await.expect("seed delete");
    }
    let sub_rel = RelPath::new(&format!("{scope_name}/sub")).expect("sub rel");
    let sub_entry = boot.stat(&sub_rel).await.expect("sub stat");
    boot.delete(&sub_entry.id).await.expect("sub delete");
    let scope_stat = boot.stat(&scope).await.expect("scope stat");
    boot.delete(&scope_stat.id).await.expect("scope delete");
    assert!(
        boot.stat(&scope).await.is_err(),
        "the rebuild scope is cleaned"
    );
    println!("PASS: pan123 rebuild convergence (remote cleaned)");
}

// ------------------------------------- 4. setup 流真机探测（QR 腿） ---

/// 向导 QR 路的三个 oauth 端点从 cli 侧可达（非交互探测：起一个码 →
/// 终端渲染 → 一次轮询应答 Waiting → 弃码）。确认态需人扫（交互面由
/// `cydrive setup` 的人工/tmux 运行覆盖——123-5 收尾批实测）。
#[tokio::test]
#[ignore = "real network: hits the live 123pan login endpoints (no scan needed); run with --ignored --test-threads=1"]
async fn pan123_setup_flow_endpoints_reach_the_live_service() {
    let http = ck_pan123::api::web_http_client(&ck_pan123::api::new_login_uuid())
        .expect("web-identity client");

    let session = oauth::qr_generate(&http, ck_pan123::DEFAULT_LOGIN_BASE)
        .await
        .expect("qr generate against the live service");
    assert!(!session.uni_id.is_empty(), "uniID came back");
    assert!(session.url.starts_with("https://"), "QR URL shape");

    let art = oauth::render_qr_terminal(&session.url).expect("terminal QR renders");
    assert!(art.lines().count() > 10, "the wizard's QR has substance");
    eprintln!(
        "[setup-qr] rendered {} lines of QR art",
        art.lines().count()
    );

    // 一次轮询：无人扫码 → Waiting（0–4 状态机的 0 态）。
    let verdict = oauth::qr_poll(&http, ck_pan123::DEFAULT_LOGIN_BASE, &session.uni_id)
        .await
        .expect("qr result against the live service");
    assert!(
        matches!(verdict, oauth::QrPoll::Waiting),
        "a fresh unscanned code polls Waiting, got {verdict:?}"
    );
    // 弃码（窗口自然失效，无需清理）。
    println!("[SUMMARY] setup_qr_probe|generate=ok|render=ok|poll=waiting");
}

// ----------------------------------- 5. setup sign_in 路真机（测试账） ---

/// 向导 sign_in 路的核心链路（向导本体 = dialoguer 交互面 + 本链）：
/// 测试账密换发 token → 立即 user_info 验证。凭据从仓外 json 运行期
/// 读取，绝不打印/落日志（R3）。
///
/// **token 淘汰效应（123-5 真机实证）**：sign_in 换发的 token 并存
/// 有上限——账号内积到第 3 枚时**最旧的被判 Unauthorized**（FIFO-2
/// 形态：旧 token T1 在 T2 换发后仍活、在 T3 换发后死；T5 换发后
/// T4 仍活）。推论：重跑 `cydrive setup` 后以最新 token 为准；测试
/// 套件里本用例之后的断言用的是 env 里的既有 token——按实证其存活
/// 至少再容纳一次换发，连续多轮 mint 才会踢掉。
#[tokio::test]
#[ignore = "real network: hits the live sign_in endpoint with the test account (credentials from the gitignored test dir); run with --ignored --test-threads=1"]
async fn pan123_setup_sign_in_path_with_the_test_account() {
    let account_path = std::env::var("PAN123_E2E_ACCOUNT")
        .unwrap_or_else(|_| r"E:\GitHub\rs-CyDrive\test\pan123-test-account.json".to_string());
    let raw = std::fs::read_to_string(&account_path)
        .unwrap_or_else(|e| panic!("read the test account file ({e}) — see the module docs"));
    // serde derive 不在 cli 测试面——Value 直取（键缺失即 panic，与
    // derive 形态等价的守门）。
    let account: serde_json::Value = serde_json::from_str(&raw).expect("account json shape");
    let passport = account
        .get("passport")
        .and_then(|s| s.as_str())
        .expect("passport key")
        .to_string();
    let password = account
        .get("password")
        .and_then(|s| s.as_str())
        .expect("password key")
        .to_string();
    assert!(!passport.is_empty() && !password.is_empty());

    let http = ck_pan123::api::web_http_client(&ck_pan123::api::new_login_uuid())
        .expect("web-identity client");
    let token = oauth::sign_in(
        &http,
        ck_pan123::DEFAULT_API_BASE,
        &passport,
        &password,
        None,
    )
    .await
    .expect("sign_in with the test account");
    assert!(!token.is_empty(), "a token came back");

    // 向导同款验证链：换发 token 立即 user_info 一次。
    let client = ck_pan123::Pan123Client::new(
        token,
        ck_pan123::DEFAULT_API_BASE.to_string(),
        ck_pan123::DEFAULT_FALLBACK_BASE.to_string(),
        None,
    )
    .expect("client");
    let info = client.user_info().await.expect("verify the fresh token");
    eprintln!(
        "[setup-signin] token minted and verified (uid {}, vip={})",
        info.uid, info.vip
    );
    println!("[SUMMARY] setup_sign_in|sign_in=ok|user_info=ok");
}
