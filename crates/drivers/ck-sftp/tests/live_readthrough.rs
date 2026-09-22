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

use std::{path::PathBuf, sync::Arc, time::Duration};

use ck_sftp::{SftpDriver, SftpParams, SftpTransport};
use cloudkit_core::cache::CacheManager;
use cloudkit_core::config::EncryptionScheme;
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::rel_path::RelPath as VfsPath;
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cloudkit_storage::{RelPath as VolRel, StorageDriver, StorageError, WriteHint};

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
