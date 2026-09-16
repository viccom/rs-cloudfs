//! pan115 真机 E2E（Phase 5 补批，2026-09-16）：**加密全栈** + **WebDAV
//! 双模式**——两腿都是 `#[ignore]` 真网测试，凭据只经 env（R3）。
//!
//! ```text
//! CYDRIVE_PAN115_TEST_ACCESS_TOKEN=... CYDRIVE_PAN115_TEST_REFRESH_TOKEN=... \
//!   cargo test -p cloudkit-cli --test pan115_e2e -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! - [`pan115_vfs_aead_v2_full_stack_roundtrip`]：VFS(aead_v2) + 真驱动
//!   ——写明文 → 队列排空 → 行元数据（uploaded/encrypted/scheme/明文
//!   size）→ hydrate 逐字节 → open_read 跨块窗口解密 → **远端侧核验
//!   密文**（驱动直读远端对象：≠ 明文、明文特征段不出现）。镜像
//!   K54 `tg_vfs_e2e.rs` 的形态（telegram 加密全栈先例）。
//! - [`pan115_webdav_roundtrip_plain_and_encrypted`]：`run_with_transport`
//!   起 WebDAV（`:0` 临时端口，auto-mount/web-ui 关）——明文与加密各
//!   一轮：PUT → （上传排空后）GET 逐字节 → PROPFIND 207 → 收尾
//!   driver 删远端（D2 回收站语义）+ NotFound 核验。
//!
//! 作业纪律：文件只出现在 `/_e2e_pan115/`（VFS 路径即远端路径）；
//! 驱动侧限速 = 生产缺省（1 rps）；不碰分享/离线/视频族端点。

#![cfg(feature = "pan115")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ck_pan115::limiter::LimiterConfig;
use ck_pan115::{Pan115Driver, Pan115Params, Pan115Transport};
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
/// 上传队列排空轮询：1 rps 限速下 hash→init→get_token→OSS 的链条按
/// 秒计，120s 覆盖含重试的慢形态。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(120);

// ------------------------------------------------------------- harness --

fn env_pair() -> (String, String) {
    (
        std::env::var("CYDRIVE_PAN115_TEST_ACCESS_TOKEN")
            .expect("set CYDRIVE_PAN115_TEST_ACCESS_TOKEN"),
        std::env::var("CYDRIVE_PAN115_TEST_REFRESH_TOKEN")
            .expect("set CYDRIVE_PAN115_TEST_REFRESH_TOKEN"),
    )
}

/// 真机驱动（生产端点 + 生产限速；connect 读 uid 定卷身份）。
async fn live_driver() -> Arc<Pan115Driver> {
    let (access, refresh) = env_pair();
    let params = Pan115Params {
        client_id: ck_pan115::DEFAULT_CLIENT_ID.to_string(),
        access_token: Some(access),
        refresh_token: Some(refresh),
        root: "0".to_string(),
        api_base: ck_pan115::DEFAULT_API_BASE.to_string(),
        passport_base: ck_pan115::DEFAULT_PASSPORT_BASE.to_string(),
        token_store: None,
        limiter: Some(LimiterConfig::default()),
        sessions_dir: None,
    };
    Arc::new(Pan115Driver::connect(params).await.expect("live connect"))
}

fn unix_stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis()
}

/// 确定性伪随机载荷（不落真随机——可复现）。
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
async fn cleanup_remote(driver: &Pan115Driver, rel: &str) {
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
#[ignore = "real network: needs the live 115 token pair from env; run with --ignored --test-threads=1"]
async fn pan115_vfs_aead_v2_full_stack_roundtrip() {
    let driver = live_driver().await;
    let transport: Arc<dyn CloudTransport> = Arc::new(Pan115Transport::new(driver.clone()));

    let data_dir = tempfile::tempdir().expect("tempdir for db/cache");
    let db = Arc::new(MetaDatabase::open(&data_dir.path().join("meta.db")).expect("db"));
    let cache_root = data_dir.path().join("cache");
    let password = format!("pan115-e2e-{}", unix_stamp());
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
    let rel_str = format!("_e2e_pan115/e2e-{}-enc.bin", unix_stamp());
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

        // open_read：跨第一块边界的窗口解密（K47 流式形态）。
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

        // **远端密文核验**（本腿的 pan115 特化断言）：绕过 VFS 的解密面，
        // 驱动直读远端对象——必须是密文（≠ 明文，且明文特征段不出现）。
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
    body.unwrap_or_else(|error| panic!("pan115 aead_v2 full-stack scenario failed: {error}"));
    eprintln!("PASS: pan115 vfs aead_v2 full-stack roundtrip (remote cleaned)");
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

/// 一次 WebDAV 轮：起栈 → PUT → 等上传终态 → GET 逐字节 → PROPFIND。
async fn webdav_round(encrypt: bool, driver: &Pan115Driver) -> (String, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (access, refresh) = env_pair();
    let dir = tempfile::tempdir().expect("tempdir for the boot");
    let _cwd = chdir(dir.path());
    let stamp = unix_stamp();
    let name = format!(
        "{}-{}.bin",
        if encrypt { "wd-enc" } else { "wd-plain" },
        stamp
    );
    let rel_str = format!("_e2e_pan115/{name}");

    let cfg = CyDriveConfig {
        backend: Backend::Pan115,
        pan115_access_token: Some(access),
        pan115_refresh_token: Some(refresh),
        db_path: "meta.db".to_string(),
        cache_path: "cache".to_string(),
        webdav_port: 0,
        auto_mount_drive: false,
        enable_web_ui: false,
        enable_encryption: encrypt,
        encryption_password: encrypt.then(|| format!("pan115-wd-{stamp}")),
        encryption_scheme: EncryptionScheme::AeadV2,
        ..CyDriveConfig::default()
    };
    // 注：不调 cfg.validate()——它按生产口径拒绝 webdav_port=0，而
    // run 流程的 :0 = 临时端口（run_e2e.rs 同款 boot 形态）。

    let transport: Arc<dyn CloudTransport> = Arc::new(Pan115Transport::new(Arc::new(
        // 每轮独立驱动实例（独立队列世界；连接共享同一 token/限速参数）。
        // 复用外层 driver 的参数形态但重连一次（uid 相同，卷身份一致）。
        Pan115Driver::connect(driver_params())
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
        "MKCOL /_e2e_pan115/ HTTP/1.1
Host: 127.0.0.1:{}
Connection: close
Content-Length: 0

",
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

    // PUT：256 KiB 确定性载荷（跨多个 4KiB TCP 帧也无妨——Content-Length
    // 定界）。
    let payload = pseudo_random(256 * 1024, if encrypt { 0xE11C } else { 0x91A7 });
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
    // 不主动 shutdown（Windows 上关写半边会把连接整断）：Connection:
    // close 由服务端收尾，read_to_end 到服务端关闭为止（run_e2e 同款）。
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

    // 等上传终态（1 rps 链条数秒）——轮询远端可见性，随后 GET 才有
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

    // PROPFIND 作业集合（Depth:1 列直接子项——文件在 /_e2e_pan115/ 内，
    // 根的 Depth:1 只见目录本身）。
    let propfind = format!(
        "PROPFIND /_e2e_pan115/ HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nDepth: 1\r\nContent-Length: 0\r\n\r\n",
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

fn driver_params() -> Pan115Params {
    let (access, refresh) = env_pair();
    Pan115Params {
        client_id: ck_pan115::DEFAULT_CLIENT_ID.to_string(),
        access_token: Some(access),
        refresh_token: Some(refresh),
        root: "0".to_string(),
        api_base: ck_pan115::DEFAULT_API_BASE.to_string(),
        passport_base: ck_pan115::DEFAULT_PASSPORT_BASE.to_string(),
        token_store: None,
        limiter: Some(LimiterConfig::default()),
        sessions_dir: None,
    }
}

#[tokio::test]
#[ignore = "real network: needs the live 115 token pair from env; run with --ignored --test-threads=1"]
async fn pan115_webdav_roundtrip_plain_and_encrypted() {
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
    eprintln!("PASS: pan115 webdav roundtrip (plain + encrypted, remote cleaned)");
}
