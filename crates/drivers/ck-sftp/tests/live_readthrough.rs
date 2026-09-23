//! sftp 共性冒烟腿（Phase 8 / RT5）——**`#[ignore]` 真机**（WSL2
//! OpenSSH fixture，照 `docs/tracking/phase4-sftp-fixture.md` 与本 crate
//! live_matrix 的 env 惯例）：真 `ck-sftp` 驱动 + 空 sqlite + 真 `Vfs`，
//! 各一腿 `read_dir_fresh` / `stat_fresh` + 驱动侧删除的 D7 prune。
//!
//! ## 运行形态
//!
//! ```text
//! # WSL2 fixture（sshd -p 2222；搭建步骤与指纹取法见 fixture 文档；
//! # 凭据只经 env，绝不入库）：
//! MSYS_NO_PATHCONV=1 \
//! CYDRIVE_SFTP_TEST_HOST=<WSL VM IP 或 127.0.0.1> \
//! CYDRIVE_SFTP_TEST_PORT=2222 \
//! CYDRIVE_SFTP_TEST_USER=cydrivetest \
//! CYDRIVE_SFTP_TEST_PASSWORD=<一次性测试密码> \
//! CYDRIVE_SFTP_TEST_FINGERPRINT='SHA256:...' \
//! CYDRIVE_SFTP_TEST_ROOT=/srv/sftp-test \
//! cargo test -p ck-sftp --test live_readthrough -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! D1 两形态照 live_matrix：密码（PASSWORD）或本机未加密私钥
//! （KEY_PATH）任一即可。缺必需 env → panic 带指引（K79.6，本腿不豁免）。
//!
//! **共性定位（如实声明）**：机制本体只有一份
//! （`cloudkit-core::readthrough`，RT2 已 16 测试钉死语义；local 形由
//! ck-local 的非 ignored 冒烟、webdav 形由 ck-webdav 的 live_readthrough
//! 六腿覆盖）。本腿证明 sftp 宽面经 `SftpTransport::as_driver` 探针 +
//! `authoritative_index` 门把同一机制接进第三实现的装配面——第三后端
//! 的 read-through 端到端各一腿，不做 live_matrix 已覆盖的驱动契约
//! 重复断言。L1→L3 dev-dep 边的裁决记录见 ck-webdav 的 Cargo.toml 注释
//! （check_layers 机械面通过；RT5 任务单指定其为裁决面）。
//!
//! **加密两阶段腿（Phase 8-B 验收延伸，RT5-enc）**：同一文件的
//! `live_encrypted_readthrough_smoke` 把 EB4 离线三腿的协议语义搬到真
//! sftp 上——加密实例（aead_v2 + 实例密码）上传三边界文件 → drop Vfs
//! （阶段切换）→ **全新空 db** 现查物化 → 逐字节读回。混合方案自愈腿
//! （`live_encrypted_mixed_scheme_self_heals`）在真机上预置 v1 容器，
//! 实例 cfg=aead_v2，首读双试自愈。

use std::{path::PathBuf, sync::Arc, time::Duration};

use ck_sftp::{SftpDriver, SftpParams, SftpTransport};
use cloudkit_core::cache::CacheManager;
use cloudkit_core::config::EncryptionScheme;
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::rel_path::RelPath as VfsPath;
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{StreamSource, Vfs, VfsConfig};
use cloudkit_storage::{RelPath as VolRel, StorageDriver, StorageError, WriteHint};
use futures_util::StreamExt as _;

/// 真机 fixture 的环境变量组（照 live_matrix 的 env 名；R3：凭据只经
/// env，空串视同未设置）。
struct LiveEnv {
    host: String,
    port: u16,
    username: String,
    password: Option<String>,
    key_path: Option<String>,
    fingerprint: String,
    root: String,
}

fn live_env() -> LiveEnv {
    let required = |name: &str| -> String {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                panic!(
                    "{name} is not set: the RT5 sftp read-through leg reads its fixture from \
                 the CYDRIVE_SFTP_TEST_* environment variables (see the module docs and \
                 docs/tracking/phase4-sftp-fixture.md), then rerun with --ignored"
                )
            })
    };
    let optional = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    LiveEnv {
        host: required("CYDRIVE_SFTP_TEST_HOST"),
        port: std::env::var("CYDRIVE_SFTP_TEST_PORT").map_or_else(
            |_| 22,
            |raw| {
                raw.trim()
                    .parse()
                    .unwrap_or_else(|_| panic!("CYDRIVE_SFTP_TEST_PORT is not a number: {raw:?}"))
            },
        ),
        username: required("CYDRIVE_SFTP_TEST_USER"),
        password: optional("CYDRIVE_SFTP_TEST_PASSWORD"),
        key_path: optional("CYDRIVE_SFTP_TEST_KEY_PATH"),
        // D2：本腿走真机会话，指纹必钉（缺失 = panic，不作 D2 拒绝腿——
        // 那是 live_matrix 腿⑩的职责）。
        fingerprint: required("CYDRIVE_SFTP_TEST_FINGERPRINT"),
        root: std::env::var("CYDRIVE_SFTP_TEST_ROOT").unwrap_or_else(|_| "/".to_string()),
    }
}

fn live_driver(env: &LiveEnv) -> Arc<SftpDriver> {
    assert!(
        env.password.is_some() || env.key_path.is_some(),
        "no credential in the environment: set CYDRIVE_SFTP_TEST_PASSWORD or \
         CYDRIVE_SFTP_TEST_KEY_PATH (D1 needs one of the two forms)"
    );
    let params = SftpParams {
        host: env.host.clone(),
        port: env.port,
        username: env.username.clone(),
        password: env.password.clone(),
        private_key_path: env.key_path.clone().map(PathBuf::from),
        private_key_passphrase: None,
        host_fingerprint: Some(env.fingerprint.clone()),
        root: env.root.clone(),
    };
    Arc::new(SftpDriver::new(params).expect("driver constructs"))
}

/// 每轮唯一工作目录（本套件前缀 `sf4rt/`，与 live_matrix 的 `sf4/`
/// 分开；K72 跨轮不撞）。
fn work_dir() -> VolRel {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    VolRel::new(&format!("sf4rt/rt5-{nanos:x}")).expect("work dir path")
}

fn rand_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64
}

/// 确定性伪随机载荷（64 位 LCG，K77.6）。
fn pattern(n: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..n)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

async fn upload(driver: &SftpDriver, path: &VolRel, data: &[u8], label: &str) {
    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..Default::default()
    };
    let mut stager = driver
        .writer(path, &hint)
        .await
        .unwrap_or_else(|e| panic!("{label} writer: {e}"));
    stager
        .write(data)
        .await
        .unwrap_or_else(|e| panic!("{label} write: {e}"));
    stager
        .close()
        .await
        .unwrap_or_else(|e| panic!("{label} close: {e}"));
}

fn names(rows: &[cloudkit_core::database::FileRecord]) -> Vec<String> {
    let mut names: Vec<String> = rows.iter().map(|row| row.name.clone()).collect();
    names.sort();
    names
}

/// sftp 真驱动 + 空 sqlite + 真 Vfs（明文实例；夹具状态在驱动根外的
/// 独立临时目录——ck-local 冒烟腿同理由）。TempDir 排首位：局部变量
/// 逆声明序 drop，让目录删除发生在 db 连接关闭之后。
async fn harness(driver: Arc<SftpDriver>) -> (tempfile::TempDir, Arc<MetaDatabase>, Arc<Vfs>) {
    let state = tempfile::tempdir().expect("state tempdir");
    let db = Arc::new(MetaDatabase::open(&state.path().join("meta.db")).expect("open db"));
    let cache = CacheManager::new(state.path().join("cache"), u64::MAX);
    let transport = Arc::new(SftpTransport::new(driver));
    let cfg = VfsConfig {
        chunk_size_bytes: 64 * 1024,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: None,
        encryption_scheme: EncryptionScheme::AeadV2,
        hydrate_timeout: Duration::from_secs(60),
    };
    let vfs = Arc::new(Vfs::new(Arc::clone(&db), cache, transport, cfg));
    (state, db, vfs)
}

/// 加密舞台：db/cache 路径由调用方给（阶段一用 `meta-1.db`、阶段二用
/// `meta-2.db`——**全新空 db 文件**，与 EB4 离线腿的 wipe 同语义，
/// 真机上不需要删文件）。密码在 = 加密实例。返回 `(db, Vfs)`——与
/// `encrypted_readthrough_e2e.rs` 的 `build_stage` 同形。
fn enc_stage(
    driver: Arc<SftpDriver>,
    state: &tempfile::TempDir,
    db_name: &str,
    cache_name: &str,
    password: Option<String>,
) -> (Arc<MetaDatabase>, Arc<Vfs>) {
    let db = Arc::new(MetaDatabase::open(&state.path().join(db_name)).expect("open db"));
    let cache = CacheManager::new(state.path().join(cache_name), u64::MAX);
    let transport = Arc::new(SftpTransport::new(driver));
    let cfg = VfsConfig {
        chunk_size_bytes: 64 * 1024,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: password,
        encryption_scheme: EncryptionScheme::AeadV2,
        // 真机下行慢 + 多文件腿：放宽到 300s（EB4 离线腿用默认 60s，
        // 本机环回快；真机夹具保守取值）。
        hydrate_timeout: Duration::from_secs(300),
    };
    let vfs = Arc::new(Vfs::new(Arc::clone(&db), cache, transport, cfg));
    (db, vfs)
}

/// 逐段删除远端目录树（驱动侧递归：list 出子项 → 目录递归 → 文件删）。
/// 明文腿只删一层，加密腿有子目录故需要它。
async fn remove_tree(driver: &SftpDriver, dir: &VolRel) -> Result<(), StorageError> {
    let mut cursor = cloudkit_storage::PageCursor::Start;
    loop {
        let listing = driver
            .list(
                dir,
                cloudkit_storage::Page {
                    cursor,
                    limit: 1000,
                },
            )
            .await?;
        for entry in &listing.entries {
            if entry.kind == cloudkit_storage::EntryKind::Dir {
                Box::pin(remove_tree(driver, &entry.path)).await?;
            } else {
                driver.delete(&entry.id).await?;
            }
        }
        match listing.next {
            Some(next) => cursor = next,
            None => break,
        }
    }
    driver.delete(&driver.stat(dir).await?.id).await
}

async fn read_all(mut stream: cloudkit_storage::ByteStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("chunk"));
    }
    out
}

/// 失败路径的尽力清理（明文腿的显式 cleanup 纪律的加密侧加固）：
/// 腿在收尾前 panic/早期返回时，Drop 仍会把远端工作树删掉——真机
/// 夹具不该因一次断言失败积残件（本轮 `sf4rt/rt5-<stamp>` 前缀跨轮
/// 唯一，残件不污染后续轮，但不留垃圾是纪律）。
///
/// **实现形态（实测钉住）**：`#[tokio::test]` 的 panic unwind 发生在
/// runtime 线程上——Drop 里 `block_on` 新 runtime 会 panic（「Cannot
/// start a runtime from within a runtime」），`Handle::try_current()`
/// 也恒为有值（故不能靠它挑分支）。唯一可移植的做法 = 把清理搬到
/// **独立 OS 线程**（`std::thread::spawn`）里建一次性 runtime 跑完，
/// 主线程 `join` 等它收尾。正常路径的显式 cleanup 先行 → `disarm`
/// 后 Drop 变 no-op，零额外成本。
struct RemoteTreeGuard {
    driver: Arc<SftpDriver>,
    dir: VolRel,
    armed: bool,
}

impl RemoteTreeGuard {
    fn new(driver: Arc<SftpDriver>, dir: VolRel) -> Self {
        RemoteTreeGuard {
            driver,
            dir,
            armed: true,
        }
    }

    /// 正常收尾已删完 → 撤防（Drop 变 no-op）。
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for RemoteTreeGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let driver = Arc::clone(&self.driver);
        let dir = self.dir.clone();
        // 独立线程 + 一次性 runtime：绕开「runtime 内不能再 block_on
        // 建 runtime」的限制（panic unwind 时就处于 runtime 线程）。
        let worker = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            let _ = rt.block_on(async move { remove_tree(&driver, &dir).await });
        });
        // join 确保清理跑完（Drop 在 unwind 中调用，等待是安全的——
        // worker 不碰 panic 状态）。
        let _ = worker.join();
    }
}

/// 腿本体：①真机预置树（驱动自落盘）→ ②空索引 `read_dir_fresh` 全见
/// + 行落库 → ③`stat_fresh` 深跳命中 → ④驱动面删 1 个（外部真值：不经
/// Vfs 簿记）→ ⑤再枚举行消失（D7 真驱动 stat 双确认）→ 收尾核空。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_SFTP_TEST_* env and the WSL2 OpenSSH fixture (see the module docs)"]
async fn live_readthrough_smoke() {
    let env = live_env();
    let driver = live_driver(&env);
    let work = work_dir();
    let stamp = work.as_str().rsplit('/').next().expect("stamp").to_string();

    // ① 预置：2 文件 + 子目录 + 深层 1 文件（内容按轮随机）。
    driver
        .mkdir(&work)
        .await
        .unwrap_or_else(|e| panic!("setup mkdir work: {e}"));
    upload(
        &driver,
        &work.join("f0.bin").expect("path"),
        &pattern(2048, rand_seed()),
        "setup/f0",
    )
    .await;
    upload(
        &driver,
        &work.join("f1.bin").expect("path"),
        &pattern(512, rand_seed()),
        "setup/f1",
    )
    .await;
    let sub = work.join("sub").expect("path");
    driver
        .mkdir(&sub)
        .await
        .unwrap_or_else(|e| panic!("setup mkdir sub: {e}"));
    let deep_data = pattern(777, rand_seed());
    upload(
        &driver,
        &sub.join("deep.bin").expect("path"),
        &deep_data,
        "setup/deep",
    )
    .await;

    // ② 空索引 → read_dir_fresh 全见 + 行落库。
    let (_state, db, vfs) = harness(Arc::clone(&driver)).await;
    let vwork = format!("/{work}");
    let rows = vfs
        .read_dir_fresh(&VfsPath::new(&vwork).expect("vpath"))
        .await
        .unwrap_or_else(|e| panic!("read_dir_fresh(work): {e}"));
    assert_eq!(
        names(&rows),
        vec!["f0.bin", "f1.bin", "sub"],
        "empty index must see the live sftp tree"
    );
    assert_eq!(
        db.get_file(&format!("/{work}/f0.bin"))
            .expect("db")
            .map(|row| row.size),
        Some(2048),
        "f0.bin row must be materialized with its size"
    );

    // ③ stat_fresh 深跳命中（父目录重列恰一次后行直出）。
    let deep = vfs
        .stat_fresh(&VfsPath::new(&format!("/{work}/sub/deep.bin")).expect("vpath"))
        .await
        .unwrap_or_else(|e| panic!("stat_fresh deep: {e}"));
    assert_eq!(deep.size, deep_data.len() as i64);

    // ④ 驱动面删 f1（= 外部真值）→ ⑤ 再枚举行消失。
    let victim = driver
        .stat(&work.join("f1.bin").expect("path"))
        .await
        .unwrap_or_else(|e| panic!("stat victim: {e}"));
    driver
        .delete(&victim.id)
        .await
        .unwrap_or_else(|e| panic!("delete victim: {e}"));
    let rows = vfs
        .read_dir_fresh(&VfsPath::new(&vwork).expect("vpath"))
        .await
        .unwrap_or_else(|e| panic!("read_dir_fresh after delete: {e}"));
    assert_eq!(
        names(&rows),
        vec!["f0.bin", "sub"],
        "the deleted file must vanish from the fresh view"
    );
    assert!(
        db.get_file(&format!("/{work}/f1.bin"))
            .expect("db")
            .is_none(),
        "the victim row must be pruned (driver stat double-confirm NotFound)"
    );

    // 收尾：删工作目录 + stat 核空。
    match driver.stat(&work).await {
        Ok(entry) => {
            driver
                .delete(&entry.id)
                .await
                .unwrap_or_else(|e| panic!("cleanup delete: {e}"));
        }
        Err(StorageError::NotFound) => return,
        Err(e) => panic!("cleanup stat: {e}"),
    }
    match driver.stat(&work).await {
        Err(StorageError::NotFound) => {}
        Ok(_) => panic!("cleanup verify: {stamp} still stat-able"),
        Err(e) => panic!("cleanup verify: expected NotFound, got {e}"),
    }
    eprintln!(
        "[SUMMARY] rt5 sftp|3+1 rows cold-index visible|stat_fresh deep hit ({}B)|delete → pruned|workdir removed and verified gone",
        deep_data.len()
    );
}

// ------------------------------------------- 加密两阶段腿（Phase 8-B） ---

/// 跨块腿的尾部余量：1MiB + 12345 落进第二个 1MiB 容器块（照 EB4 离线
/// 腿 `CROSS_TAIL`，同一数值便于对账）。
const ENC_MIB: usize = 1024 * 1024;
const ENC_CROSS_TAIL: usize = 12_345;

/// EB4 离线腿 1 的真机镜像：加密实例经真 sftp 驱动 + 上传队列写三边界
/// 文件（空件 / 恰 1MiB / 跨块），drop Vfs 切阶段，**全新空 db** 现查
/// 物化 → 尺寸闭式精确 + 逐字节读回 → 驱动侧递归清理并核空。
///
/// 断言口径与 `cloudkit-cli/tests/encrypted_readthrough_e2e.rs` 腿 1
/// 逐条对齐（那边 local/TempDir，这边真 sftp）；差异仅三点：①后端是
/// 真 SSH 文件系统（range_read=true，aead_v2 可流式）②阶段二用**另一
/// 个 db 文件名**而非删文件（真机无 wipe 需求）③清理走驱动递归。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_SFTP_TEST_* env and a writable real sftp server (see the module docs)"]
async fn live_encrypted_readthrough_smoke() {
    let env = live_env();
    let driver = live_driver(&env);
    let work = work_dir();
    let stamp = work.as_str().rsplit('/').next().expect("stamp").to_string();
    // 实例密码按轮唯一（K72 去重纪律的加密侧等价物：跨轮密文必不同，
    // 秒传/缓存短路无从命中）；值只在进程内流转，绝不落日志/回传。
    let password = format!("rt5enc-{}", rand_seed());

    // 三边界载荷（按轮随机 64 位 LCG）。
    let plain_empty: Vec<u8> = Vec::new();
    let plain_exact = pattern(ENC_MIB, rand_seed());
    let plain_cross = pattern(ENC_MIB + ENC_CROSS_TAIL, rand_seed());

    let state = tempfile::tempdir().expect("state tempdir");
    // 失败路径兜底（断言 panic 也要把远端工作树删掉）。
    let guard = RemoteTreeGuard::new(Arc::clone(&driver), work.clone());

    // ---- 阶段一：加密上传（Vfs::put 全链：容器构造 + 队列 + 真 sftp） ----
    {
        let (db, vfs) = enc_stage(
            Arc::clone(&driver),
            &state,
            "meta-1.db",
            "cache-1",
            Some(password.clone()),
        );
        let root = VfsPath::new(&format!("/{work}")).expect("vpath root");
        let v_empty = root.join("empty.bin").expect("vpath");
        let v_exact = root.join("exact-1mib.bin").expect("vpath");
        let v_docs = root.join("docs").expect("vpath");
        let v_sub = v_docs.join("sub").expect("vpath");
        let v_cross = v_sub.join("cross.bin").expect("vpath");
        // 远端目录树先经驱动落盘（Vfs::create_dir 只写本地行、不建远端，
        // 且要求父行已在——真机工作目录是全新的，故驱动侧 mkdir 先行）。
        driver
            .mkdir(&work)
            .await
            .unwrap_or_else(|e| panic!("mkdir work: {e}"));
        driver
            .mkdir(&work.join("docs").expect("path"))
            .await
            .unwrap_or_else(|e| panic!("mkdir docs: {e}"));
        driver
            .mkdir(&work.join("docs").expect("path").join("sub").expect("path"))
            .await
            .unwrap_or_else(|e| panic!("mkdir docs/sub: {e}"));
        // 本地目录行：从根逐层 read_dir_fresh 物化（驱动侧树已就位，
        // 现查即建行——比 create_dir 手写父链更贴近真实使用，也顺带
        // 覆盖新目录在空索引实例下的可见性）。
        for dir in [root.clone(), v_docs.clone(), v_sub.clone()] {
            vfs.read_dir_fresh(&dir)
                .await
                .unwrap_or_else(|e| panic!("materialize {}: {e}", dir.as_str()));
        }

        vfs.put(&v_empty, &plain_empty, 1_700_000_000.0)
            .await
            .expect("put empty");
        vfs.put(&v_exact, &plain_exact, 1_700_000_000.0)
            .await
            .expect("put exact");
        vfs.put(&v_cross, &plain_cross, 1_700_000_000.0)
            .await
            .expect("put cross");
        vfs.shutdown().await; // 排空上传队列到终态

        // 上传行真相（阶段一断言）：uploaded + cipher 标志 + 明文 size。
        for (rel, plain) in [
            (&v_empty, plain_empty.as_slice()),
            (&v_exact, plain_exact.as_slice()),
            (&v_cross, plain_cross.as_slice()),
        ] {
            let row = db
                .get_file(rel.as_str())
                .expect("db read")
                .unwrap_or_else(|| panic!("uploaded row {rel:?} exists"));
            assert!(row.is_uploaded, "{rel:?}: uploaded after drain");
            assert!(row.is_encrypted, "{rel:?}: cipher flag from the instance");
            assert_eq!(row.encryption_scheme, "aead_v2", "{rel:?}: config scheme");
            assert_eq!(
                row.size,
                plain.len() as i64,
                "{rel:?}: upload row size = plaintext size"
            );
        }
        eprintln!(
            "[encl1] stage 1: three encrypted uploads drained to the real sftp \
             backend (empty / 1MiB / cross) — container sizes verified below"
        );
    } // drop Vfs + db：阶段切换（模拟进程退出）

    // 远端容器尺寸核验（阶段一与阶段二的闭式反推互为交叉验证）：
    // v2 空件 = 34B 头 + 16B tag = 50B；恰 1MiB = 34+1MiB+16；跨块 =
    // 34 + (1MiB+16) + (tail+16)。
    let remote_empty = driver
        .stat(&work.join("empty.bin").expect("path"))
        .await
        .expect("stat remote empty container");
    assert_eq!(
        remote_empty.size, 50,
        "the empty file's 50-byte v2 container must really be on the remote \
         (Contract-6 encrypted exception, K85.6)"
    );
    let remote_exact = driver
        .stat(&work.join("exact-1mib.bin").expect("path"))
        .await
        .expect("stat remote exact container");
    assert_eq!(
        remote_exact.size,
        (34 + ENC_MIB + 16) as u64,
        "1MiB container = header + one full chunk + tag"
    );
    let remote_cross = driver
        .stat(
            &work
                .join("docs")
                .expect("path")
                .join("sub")
                .expect("path")
                .join("cross.bin")
                .expect("path"),
        )
        .await
        .expect("stat remote cross container");
    assert_eq!(
        remote_cross.size,
        (34 + ENC_MIB + 16 + ENC_CROSS_TAIL + 16) as u64,
        "cross container = header + full chunk + short tail chunk"
    );
    eprintln!(
        "[encl1] remote containers: empty={} exact={} cross={} (bytes)",
        remote_empty.size, remote_exact.size, remote_cross.size
    );

    // ---- 阶段二：全新空 db → 逐层现查物化 → 尺寸 + 读通 ----
    let cache2 = "cache-2";
    let (db2, vfs2) = enc_stage(
        Arc::clone(&driver),
        &state,
        "meta-2.db",
        cache2,
        Some(password.clone()),
    );
    assert!(
        db2.list_dir("/").expect("fresh db read").is_empty(),
        "the stage-2 db starts empty — every row must come from the backend"
    );
    let root = VfsPath::new(&format!("/{work}")).expect("vpath root");

    // a) 三文件都在列表里（根层：empty + exact-1mib + docs）。
    let rows = vfs2
        .read_dir_fresh(&root)
        .await
        .unwrap_or_else(|e| panic!("read_dir_fresh(stamp root): {e}"));
    assert_eq!(
        names(&rows),
        vec!["docs", "empty.bin", "exact-1mib.bin"],
        "the encrypted tree materializes from an empty index"
    );

    // b) 行字段：is_encrypted / scheme / size 闭式反推精确值。
    let empty = rows
        .iter()
        .find(|r| r.name == "empty.bin")
        .expect("empty row");
    assert!(empty.is_encrypted, "empty: materialized encrypted");
    assert_eq!(empty.encryption_scheme, "aead_v2", "empty: config scheme");
    assert_eq!(
        empty.size, 0,
        "empty: the 50B v2 container backsolves to a 0-byte plaintext"
    );
    let exact = rows
        .iter()
        .find(|r| r.name == "exact-1mib.bin")
        .expect("exact row");
    assert!(exact.is_encrypted, "exact: materialized encrypted");
    assert_eq!(exact.encryption_scheme, "aead_v2", "exact: config scheme");
    assert_eq!(
        exact.size, ENC_MIB as i64,
        "exact: size = closed-form back-solve of the 1MiB container"
    );
    let docs = rows.iter().find(|r| r.name == "docs").expect("docs row");
    assert!(
        docs.is_dir && !docs.is_encrypted,
        "directory rows stay plaintext"
    );

    let sub_rows = vfs2
        .read_dir_fresh(
            &root
                .join("docs")
                .expect("vpath")
                .join("sub")
                .expect("vpath"),
        )
        .await
        .unwrap_or_else(|e| panic!("read_dir_fresh(docs/sub): {e}"));
    let cross = sub_rows
        .iter()
        .find(|r| r.name == "cross.bin")
        .expect("cross row");
    assert!(cross.is_encrypted, "cross: materialized encrypted");
    assert_eq!(cross.encryption_scheme, "aead_v2", "cross: config scheme");
    assert_eq!(
        cross.size,
        (ENC_MIB + ENC_CROSS_TAIL) as i64,
        "cross: size = closed-form back-solve across two container blocks"
    );
    eprintln!(
        "[encl1] stage 2: materialized from an empty db — empty=0 exact={ENC_MIB} cross={}",
        ENC_MIB + ENC_CROSS_TAIL
    );

    // c) stat_fresh 深跳命中（父目录重列恰一次后行直出）。
    let deep = vfs2
        .stat_fresh(
            &root
                .join("docs")
                .expect("vpath")
                .join("sub")
                .expect("vpath")
                .join("cross.bin")
                .expect("vpath"),
        )
        .await
        .unwrap_or_else(|e| panic!("stat_fresh deep: {e}"));
    assert_eq!(deep.size, (ENC_MIB + ENC_CROSS_TAIL) as i64);

    // d) 读通逐字节：空件 + 恰 1MiB 走 hydrate 全量；跨块走 aead_v2
    //    流式（真 sftp 申报 range_read）——整窗 + 两个窗口。
    let v_empty = root.join("empty.bin").expect("vpath");
    let v_exact = root.join("exact-1mib.bin").expect("vpath");
    let v_cross = root
        .join("docs")
        .expect("vpath")
        .join("sub")
        .expect("vpath")
        .join("cross.bin")
        .expect("vpath");

    match vfs2.open_read(&v_empty).await.expect("open_read empty") {
        StreamSource::Hydrate => {}
        StreamSource::Stream { .. } => panic!("a size=0 row routes to Hydrate"),
    }
    assert_eq!(
        std::fs::read(vfs2.hydrate(&v_empty).await.expect("hydrate empty")).expect("read"),
        plain_empty,
        "empty hydrates byte-exact"
    );
    assert_eq!(
        std::fs::read(vfs2.hydrate(&v_exact).await.expect("hydrate exact")).expect("read"),
        plain_exact,
        "1MiB hydrates byte-exact through the real sftp decrypt round-trip"
    );
    eprintln!("[encl1] hydrate: empty + 1MiB byte-exact through real-sftp decrypt");

    let total = (ENC_MIB + ENC_CROSS_TAIL) as u64;
    match vfs2
        .open_read(&v_cross)
        .await
        .expect("open_read cross must not error")
    {
        StreamSource::Stream {
            handle,
            total_size,
            transport: window,
        } => {
            assert_eq!(
                total_size, total,
                "K35: stream total_size = back-solved plaintext length"
            );
            let whole = read_all(
                window
                    .open_range(&handle, 0, total)
                    .await
                    .expect("full window"),
            )
            .await;
            assert_eq!(whole, plain_cross, "full-window stream decrypts byte-exact");
            // 两个窗口：头 / 跨 1MiB 容器块边界（止于 EOF——文件仅
            // 1MiB+12345，跨界后余量放不下 128KiB）。
            for (off, len) in [
                (0u64, 64 * 1024u64),
                (
                    ENC_MIB as u64 - 64 * 1024,
                    (64 * 1024 + ENC_CROSS_TAIL) as u64,
                ),
            ] {
                let got =
                    read_all(window.open_range(&handle, off, len).await.expect("window")).await;
                let start = off as usize;
                assert_eq!(
                    got,
                    plain_cross[start..start + len as usize],
                    "window [{off},+{len}) decrypts byte-exact over real sftp"
                );
            }
            eprintln!(
                "[encl1] open_read: full window + two range windows byte-exact (aead_v2 streaming)"
            );
        }
        StreamSource::Hydrate => {
            panic!("an aead_v2 size>0 row over a range-capable transport must stream (K47)")
        }
    }

    // ---- 阶段三：驱动侧递归清理 + 核空（失败路径也尽力清理） ----
    match remove_tree(&driver, &work).await {
        Ok(()) => {}
        Err(e) => panic!("cleanup remove_tree: {e}"),
    }
    match driver.stat(&work).await {
        Err(StorageError::NotFound) => {}
        Ok(_) => panic!("cleanup verify: {stamp} still stat-able"),
        Err(e) => panic!("cleanup verify: expected NotFound, got {e}"),
    }
    guard.disarm();
    eprintln!(
        "[SUMMARY] rt5-enc sftp|3 encrypted uploads on real sftp (remote ct 50/{} /{})|\
         empty db materialized 0/{ENC_MIB}/{} byte-exact|hydrate + range stream decrypt byte-exact|\
         workdir recursively removed and verified gone",
        34 + ENC_MIB + 16,
        34 + ENC_MIB + 16 + ENC_CROSS_TAIL + 16,
        ENC_MIB + ENC_CROSS_TAIL
    );
}

/// 混合方案自愈腿（真机反证）：远端预置一个**真 v1(gcm) 容器**（用
/// `cloudkit_crypto` 现造后经驱动直传——不经 Vfs，模拟「配置已切
/// aead_v2、老文件是 v1、索引已丢」），实例 cfg=aead_v2 + 全新空 db →
/// ①物化按配置猜成 aead_v2（size 按 v2 闭式反推出错值）→ ②首读
/// admission 对无 magic 内容回 `Hydrate`（K84.2，不卡死）→ ③hydrate
/// 双试 v1 成功 → 行回写 `scheme=gcm`、`size=ct−44` → ④读通逐字节。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_SFTP_TEST_* env and a writable real sftp server (see the module docs)"]
async fn live_encrypted_mixed_scheme_self_heals() {
    let env = live_env();
    let driver = live_driver(&env);
    let work = work_dir();
    let stamp = work.as_str().rsplit('/').next().expect("stamp").to_string();
    let password = format!("rt5enc-{}", rand_seed());

    // 远端预置真 v1 容器（44B 开销；v1 无 magic，列举期无从分辨）。
    let plain = pattern(5_000, rand_seed());
    let ct = cloudkit_core::crypto::encrypt(&password, &plain);
    assert_eq!(ct.len(), 5_044, "v1 container = plaintext + 44 B overhead");
    driver
        .mkdir(&work)
        .await
        .unwrap_or_else(|e| panic!("mkdir work: {e}"));
    upload(
        &driver,
        &work.join("legacy.bin").expect("path"),
        &ct,
        "v1 seed",
    )
    .await;

    let state = tempfile::tempdir().expect("state tempdir");
    let guard = RemoteTreeGuard::new(Arc::clone(&driver), work.clone());
    let (db, vfs) = enc_stage(
        Arc::clone(&driver),
        &state,
        "meta.db",
        "cache",
        Some(password.clone()),
    );
    let v_legacy = VfsPath::new(&format!("/{work}/legacy.bin")).expect("vpath");

    // ① 配置猜错标签：is_encrypted + scheme=aead_v2 + size = v2 闭式
    //    反推 v1 密文长 = 4994（≠真值 5000）。
    let rows = vfs
        .read_dir_fresh(&VfsPath::new(&format!("/{work}")).expect("vpath"))
        .await
        .unwrap_or_else(|e| panic!("read_dir_fresh: {e}"));
    let row = rows
        .iter()
        .find(|r| r.name == "legacy.bin")
        .expect("legacy row materialized");
    assert!(row.is_encrypted, "config guess flags the row encrypted");
    assert_eq!(
        row.encryption_scheme, "aead_v2",
        "config guess labels the scheme"
    );
    assert_eq!(
        row.size, 4_994,
        "the wrong guess lands: v2 back-solve of a v1 container (5044-34-16)"
    );

    // ② 首读 admission 不做内容级判决也绝不拒（winfsp 直通面不卡死）。
    match vfs
        .open_read(&v_legacy)
        .await
        .expect("admission must not error")
    {
        StreamSource::Hydrate => {}
        StreamSource::Stream { .. } => {
            panic!("v1 content under an aead_v2 label must reach Hydrate for the dual try")
        }
    }

    // ③+④ hydrate 双试 → 自愈回写 → 读通逐字节。
    let got = std::fs::read(
        vfs.hydrate(&v_legacy)
            .await
            .expect("hydrate dual-try must succeed"),
    )
    .expect("read hydrated");
    assert_eq!(got, plain, "the mixed-scheme file decrypts byte-exact");
    let healed = db
        .get_file(&format!("/{work}/legacy.bin"))
        .expect("db read")
        .expect("row still there");
    assert_eq!(
        healed.encryption_scheme, "gcm",
        "first read repairs the scheme to the truth (v1 = gcm)"
    );
    assert_eq!(
        healed.size,
        ct.len() as i64 - 44,
        "first read repairs the size (v1 closed form = ct - 44 = plaintext)"
    );
    assert!(
        healed.is_encrypted,
        "the repair never downgrades the cipher flag (B1)"
    );
    eprintln!(
        "[encl2] dual-try self-heal on real sftp: scheme aead_v2→gcm, size 4994→{}, byte-exact",
        plain.len()
    );

    // 清理 + 核空。
    match remove_tree(&driver, &work).await {
        Ok(()) => {}
        Err(e) => panic!("cleanup remove_tree: {e}"),
    }
    match driver.stat(&work).await {
        Err(StorageError::NotFound) => {}
        Ok(_) => panic!("cleanup verify: {stamp} still stat-able"),
        Err(e) => panic!("cleanup verify: expected NotFound, got {e}"),
    }
    guard.disarm();
    eprintln!(
        "[SUMMARY] rt5-enc-scheme sftp|v1 container seeded remotely (5044B)|empty db guessed aead_v2/4994|\
         admission Hydrate|dual-try healed gcm/5000, byte-exact|workdir removed and verified gone"
    );
}
