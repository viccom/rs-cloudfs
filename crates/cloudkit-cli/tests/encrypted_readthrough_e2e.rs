//! Phase 8-B EB4 —— 两阶段验收离线三腿（**非 ignored，进常规 CI**）：
//! local 驱动 + TempDir 后端 + 实例密码，真 crypto 全链，零网络。
//!
//! - **腿 1 负责人两阶段协议**：①加密上传三边界文件（空件 / 恰 1MiB
//!   整倍数 / 跨块，内容按轮随机 64 位 LCG——K72/K77.6 去重纪律）→
//!   drop Vfs/db（阶段切换）→ ②wipe 出**全新空 db** → `read_dir_fresh`
//!   逐层物化（行 `is_encrypted=true`、scheme=配置、size=闭式反推精确）
//!   → 下载逐字节等于原明文 + Range 三窗口逐字节（aead_v2 流式）。
//! - **腿 2 混合方案自愈**：远端预置真 v1 容器（`crypto::encrypt` 现造）、
//!   实例 cfg=aead_v2、全新 db 按配置猜错标签（size 按 v2 闭式反推出
//!   4994≠5000）→ 首读 admission 不拒（Hydrate 信号）→ hydrate 双试
//!   自愈回写 `scheme=gcm`、`size=ct−44` → 读通逐字节。
//! - **腿 3 rebuild 全量**：真实加密上传 → 捕获上传行 truth → wipe db →
//!   `rebuild_from_backend_with_ctx`（生产 `CipherCtx` 形态）全树 →
//!   重建行与上传行 cipher 字段逐项同构 → 至少一文件读通逐字节。
//!   （与 EB3 `cli/tests/rebuild.rs` 集成腿的分工：那条用手搓容器种子 +
//!   生产缝 `run_rebuild_with_driver`；本腿 = 真实上传路径产物 + 直调
//!   `_with_ctx` + **上传行↔重建行对账**——协议面差异保留。）

#![cfg(feature = "local")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cloudkit_cli::vfs_config;
use cloudkit_core::cache::CacheManager;
use cloudkit_core::config::{Backend, CyDriveConfig, EncryptionScheme};
use cloudkit_core::database::{FileRecord, MetaDatabase};
use cloudkit_core::materialize::CipherCtx;
use cloudkit_core::rebuild::{rebuild_from_backend_with_ctx, RebuildLimits};
use cloudkit_core::rel_path::RelPath as VfsRelPath;
use cloudkit_core::transport::{ByteStream, CloudTransport};
use cloudkit_core::vfs::{StreamSource, Vfs};
use cloudkit_storage::RelPath as VocabRelPath;
use futures_util::StreamExt as _;

const MIB: usize = 1024 * 1024;
/// 跨块腿的尾部余量（1MiB + 12345 落进第二个 1MiB 容器块）。
const CROSS_TAIL: usize = 12_345;

// ------------------------------------------------------------- helpers --

fn unix_stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis()
}

/// 确定性伪随机载荷（64 位 LCG；种子按轮取钟表 = K72/K77.6 去重纪律——
/// 跨轮内容必不同，可复现于轮内）。
fn lcg_payload(len: usize, seed: u64) -> Vec<u8> {
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

/// 加密实例配置（local 后端 + 实例密码 + aead_v2——生产 `vfs_config`
/// 消费的形态；db/cache 全落 `dir`）。
fn enc_instance_cfg(dir: &Path, backend_root: &Path) -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Local,
        local_root: Some(backend_root.to_string_lossy().into_owned()),
        db_path: dir.join("meta.db").to_string_lossy().into_owned(),
        cache_path: dir.join("cache").to_string_lossy().into_owned(),
        enable_encryption: true,
        encryption_password: Some(format!("eb4-{}", unix_stamp())),
        encryption_scheme: EncryptionScheme::AeadV2,
        ..CyDriveConfig::default()
    }
}

/// 阶段切换的 wipe：删 db 主文件 + WAL/SHM 伴随文件（不存在即忽略）——
/// 之后重开 = 按构造为空的新 db。
fn wipe_db(db_path: &str) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{db_path}{suffix}"));
    }
}

async fn read_all(mut stream: ByteStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("chunk"));
    }
    out
}

/// 一套「db + Vfs」舞台（transport/driver 由调用方跨阶段共享——后端是
/// 同一个 TempDir 目录）。
fn build_stage(
    cfg: &CyDriveConfig,
    transport: Arc<dyn CloudTransport>,
    cache_dir: PathBuf,
) -> (Arc<MetaDatabase>, Arc<Vfs>) {
    let db = Arc::new(MetaDatabase::open(Path::new(&cfg.db_path)).expect("open db"));
    let vfs = Arc::new(Vfs::new(
        Arc::clone(&db),
        CacheManager::new(cache_dir, 1 << 30),
        transport,
        cloudkit_cli::vfs_config(cfg),
    ));
    (db, vfs)
}

fn row_of<'a>(rows: &'a [FileRecord], rel: &str) -> &'a FileRecord {
    rows.iter()
        .find(|r| r.rel_path == rel)
        .unwrap_or_else(|| panic!("row {rel} materialized: {} row(s) present", rows.len()))
}

// ------------------------------------------------ 腿 1：两阶段协议 —— --

#[tokio::test]
async fn two_stage_protocol_materializes_and_reads_back_byte_exact() {
    let backend = tempfile::tempdir().expect("backend tempdir");
    let inst = tempfile::tempdir().expect("instance tempdir");
    let cfg = enc_instance_cfg(inst.path(), backend.path());
    let seed = unix_stamp() as u64;

    let driver = ck_local::factory(&ck_local::LocalParams {
        root: backend.path().to_path_buf(),
    })
    .await
    .expect("local driver over the TempDir backend");
    let transport: Arc<dyn CloudTransport> = Arc::new(ck_local::LocalTransport::new(driver));

    // 三边界载荷：空件 / 恰 1MiB 整倍数 / 跨块（1MiB+12345）。
    let plain_empty: Vec<u8> = Vec::new();
    let plain_exact = lcg_payload(MIB, seed ^ 0xE0E1);
    let plain_cross = lcg_payload(MIB + CROSS_TAIL, seed ^ 0xC0C2);
    let v_empty = VfsRelPath::new("/empty.bin").expect("vpath");
    let v_exact = VfsRelPath::new("/exact-1mib.bin").expect("vpath");
    let v_cross = VfsRelPath::new("/docs/sub/cross.bin").expect("vpath");

    // ---- 阶段 ①：加密上传 ----
    {
        let (db, vfs) = build_stage(&cfg, Arc::clone(&transport), inst.path().join("cache"));
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

        // 上传行 truth（阶段 ① 断言）：uploaded + cipher 标志 + 配置
        // 方案 + **明文 size**（加密上传保留明文行尺寸，E-3 persist）。
        for (rel, plain) in [
            ("/empty.bin", plain_empty.as_slice()),
            ("/exact-1mib.bin", plain_exact.as_slice()),
            ("/docs/sub/cross.bin", plain_cross.as_slice()),
        ] {
            let row = db
                .get_file(rel)
                .expect("db read")
                .unwrap_or_else(|| panic!("uploaded row {rel} exists"));
            assert!(row.is_uploaded, "{rel}: uploaded after drain");
            assert!(row.is_encrypted, "{rel}: cipher flag from the instance");
            assert_eq!(row.encryption_scheme, "aead_v2", "{rel}: config scheme");
            assert_eq!(
                row.size,
                plain.len() as i64,
                "{rel}: upload row size = plaintext size"
            );
        }
        eprintln!("[leg1] stage 1: three encrypted uploads drained (empty / 1MiB / cross)");
    } // drop Vfs + db（阶段切换）

    wipe_db(&cfg.db_path);

    // ---- 阶段 ②：全新空 db → 逐层物化 → 读回 ----
    let cache2 = inst.path().join("cache-stage2");
    let (db2, vfs2) = build_stage(&cfg, Arc::clone(&transport), cache2);
    assert!(
        db2.list_dir("/").expect("fresh db read").is_empty(),
        "the stage-2 db starts empty — every row must come from the backend"
    );

    // 逐层 read_dir_fresh：根 → docs → docs/sub。
    let root_rows = vfs2
        .read_dir_fresh(&VfsRelPath::new("/").expect("vpath root"))
        .await
        .expect("read_dir_fresh root");
    let empty = row_of(&root_rows, "/empty.bin");
    assert!(empty.is_encrypted, "empty: materialized encrypted");
    assert_eq!(empty.encryption_scheme, "aead_v2", "empty: config scheme");
    assert_eq!(empty.size, 0, "empty: v2 container ct=50 backsolves to 0");
    let exact = row_of(&root_rows, "/exact-1mib.bin");
    assert!(exact.is_encrypted, "exact: materialized encrypted");
    assert_eq!(exact.encryption_scheme, "aead_v2", "exact: config scheme");
    assert_eq!(
        exact.size, MIB as i64,
        "exact: size = closed-form back-solve of the 1MiB container (34+1MiB+16)"
    );
    let docs = row_of(&root_rows, "/docs");
    assert!(docs.is_dir && !docs.is_encrypted, "dir rows stay plaintext");

    let docs_rows = vfs2
        .read_dir_fresh(&VfsRelPath::new("/docs").expect("vpath docs"))
        .await
        .expect("read_dir_fresh docs");
    row_of(&docs_rows, "/docs/sub");

    let sub_rows = vfs2
        .read_dir_fresh(&VfsRelPath::new("/docs/sub").expect("vpath sub"))
        .await
        .expect("read_dir_fresh sub");
    let cross = row_of(&sub_rows, "/docs/sub/cross.bin");
    assert!(cross.is_encrypted, "cross: materialized encrypted");
    assert_eq!(cross.encryption_scheme, "aead_v2", "cross: config scheme");
    assert_eq!(
        cross.size,
        (MIB + CROSS_TAIL) as i64,
        "cross: size = closed-form back-solve across two container blocks"
    );
    eprintln!(
        "[leg1] stage 2: materialized from an empty db (empty=0, exact={MIB}, cross={})",
        MIB + CROSS_TAIL
    );

    // 下载逐字节：空件 + 恰 1MiB 走 hydrate 全量（真驱动往返解密）。
    match vfs2.open_read(&v_empty).await.expect("open_read empty") {
        StreamSource::Hydrate => {}
        StreamSource::Stream { .. } => panic!("a size=0 row routes to Hydrate (K47 size>0 gate)"),
    }
    let got_empty = std::fs::read(vfs2.hydrate(&v_empty).await.expect("hydrate empty"))
        .expect("read hydrated empty");
    assert_eq!(got_empty, plain_empty, "empty hydrates byte-exact");

    let got_exact = std::fs::read(vfs2.hydrate(&v_exact).await.expect("hydrate exact"))
        .expect("read hydrated exact");
    assert_eq!(got_exact, plain_exact, "1MiB hydrates byte-exact");
    eprintln!("[leg1] hydrate: empty + exact byte-exact through decrypt");

    // Range：跨块文件走 aead_v2 流式——整窗（=下载逐字节）+ 三窗口。
    let total = (MIB + CROSS_TAIL) as u64;
    match vfs2.open_read(&v_cross).await.expect("open_read cross") {
        StreamSource::Stream {
            handle,
            total_size,
            transport: window,
        } => {
            assert_eq!(
                total_size, total,
                "K35: stream total_size = back-solved plaintext length"
            );
            // 整窗 = 下载逐字节（aead_v2 流式全量解密）。
            let whole = read_all(
                window
                    .open_range(&handle, 0, total)
                    .await
                    .expect("full window"),
            )
            .await;
            assert_eq!(whole, plain_cross, "full-window stream read byte-exact");

            // 三窗口：头 / 跨 1MiB 容器块边界 / 尾（中窗止于 EOF——
            // 文件仅 1MiB+12345，跨界后余量不足以再放 128KiB）。
            let windows: [(u64, u64); 3] = [
                (0, 64 * 1024),
                (MIB as u64 - 64 * 1024, (64 * 1024 + CROSS_TAIL) as u64),
                (total - 64 * 1024, 64 * 1024),
            ];
            for (off, len) in windows {
                let got =
                    read_all(window.open_range(&handle, off, len).await.expect("window")).await;
                let start = off as usize;
                assert_eq!(
                    got,
                    plain_cross[start..start + len as usize],
                    "window [{off},+{len}) decrypts byte-exact"
                );
            }
            eprintln!("[leg1] open_read: full window + three range windows byte-exact");
        }
        StreamSource::Hydrate => {
            panic!("an aead_v2 size>0 row over a range-capable transport must stream (K47)")
        }
    }
}

// --------------------------------------------------- 腿 2：混合方案 —-- -

#[tokio::test]
async fn mixed_scheme_first_read_dual_try_self_heals() {
    let backend = tempfile::tempdir().expect("backend tempdir");
    let inst = tempfile::tempdir().expect("instance tempdir");
    let cfg = enc_instance_cfg(inst.path(), backend.path());
    let seed = unix_stamp() as u64;

    // 远端预置**真 v1 容器**（现造；v1 无 magic，列举期无从分辨）。
    let plain = lcg_payload(5_000, seed ^ 0x1115);
    let ct = cloudkit_core::crypto::encrypt(
        cfg.encryption_password.as_deref().expect("password"),
        &plain,
    );
    assert_eq!(ct.len(), 5_044, "v1 container = plaintext + 44 B overhead");
    std::fs::write(backend.path().join("legacy.bin"), &ct).expect("seed v1 container");

    let driver = ck_local::factory(&ck_local::LocalParams {
        root: backend.path().to_path_buf(),
    })
    .await
    .expect("local driver");
    let transport: Arc<dyn CloudTransport> = Arc::new(ck_local::LocalTransport::new(driver));
    let (db, vfs) = build_stage(&cfg, transport, inst.path().join("cache"));
    let v = VfsRelPath::new("/legacy.bin").expect("vpath");

    // 全新 db 按配置猜错标签：is_encrypted + scheme=aead_v2 +
    // size = v2 闭式反推 v1 密文长 = 4994（≠真值 5000）。
    let rows = vfs
        .read_dir_fresh(&VfsRelPath::new("/").expect("vpath root"))
        .await
        .expect("materialize the mixed backend");
    let row = row_of(&rows, "/legacy.bin");
    assert!(row.is_encrypted, "config guess flags the row encrypted");
    assert_eq!(
        row.encryption_scheme, "aead_v2",
        "config guess labels the scheme"
    );
    assert_eq!(
        row.size, 4_994,
        "the wrong guess lands: v2 back-solve of a v1 container (5044-34-16)"
    );
    assert_ne!(row.size, plain.len() as i64, "…and it is wrong");

    // 首读 admission 不做内容级判决也绝不拒（winfsp 直通面不卡死）——
    // 无 magic → Hydrate 信号（K84.2 双试方向的入口）。
    match vfs.open_read(&v).await.expect("admission must not error") {
        StreamSource::Hydrate => {}
        StreamSource::Stream { .. } => {
            panic!("v1 content under an aead_v2 label must reach Hydrate for the dual try")
        }
    }

    // hydrate 双试：无 magic → 试 v1 → 成功 → 行回写自愈 → 读通。
    let path = vfs.hydrate(&v).await.expect("hydrate dual-try");
    let got = std::fs::read(path).expect("read hydrated");
    assert_eq!(got, plain, "the mixed-scheme file decrypts byte-exact");

    let healed = db
        .get_file("/legacy.bin")
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
        "[leg2] dual-try self-heal: scheme aead_v2→gcm, size 4994→{}, byte-exact",
        plain.len()
    );
}

// ------------------------------------- 腿 3：rebuild 全量（B5 验收） ---

#[tokio::test]
async fn rebuild_after_wipe_converges_to_the_upload_truth() {
    let backend = tempfile::tempdir().expect("backend tempdir");
    let inst = tempfile::tempdir().expect("instance tempdir");
    let cfg = enc_instance_cfg(inst.path(), backend.path());
    let seed = unix_stamp() as u64;

    let driver = ck_local::factory(&ck_local::LocalParams {
        root: backend.path().to_path_buf(),
    })
    .await
    .expect("local driver");
    let transport: Arc<dyn CloudTransport> =
        Arc::new(ck_local::LocalTransport::new(driver.clone()));

    let plain_alpha = lcg_payload(3_000, seed ^ 0xA1A1);
    let plain_beta = lcg_payload(400_000, seed ^ 0xB2B2);
    let v_alpha = VfsRelPath::new("/alpha.txt").expect("vpath");
    let v_beta = VfsRelPath::new("/docs/beta.bin").expect("vpath");

    // 真实加密上传（两文件 + 一层子目录）。
    let (db, vfs) = build_stage(&cfg, Arc::clone(&transport), inst.path().join("cache"));
    vfs.put(&v_alpha, &plain_alpha, 1_700_000_000.0)
        .await
        .expect("put alpha");
    vfs.put(&v_beta, &plain_beta, 1_700_000_000.0)
        .await
        .expect("put beta");
    vfs.shutdown().await;

    // 捕获上传行 truth（wipe 前拷出字段值——之后的对账基准）。
    let mut upload_truth: Vec<(String, bool, String, i64)> = Vec::new();
    for rel in ["/alpha.txt", "/docs/beta.bin"] {
        let row = db
            .get_file(rel)
            .expect("db read")
            .unwrap_or_else(|| panic!("uploaded row {rel}"));
        assert!(
            row.is_uploaded && row.is_encrypted,
            "{rel}: uploaded encrypted"
        );
        upload_truth.push((
            rel.to_string(),
            row.is_encrypted,
            row.encryption_scheme.clone(),
            row.size,
        ));
    }
    drop(vfs);
    drop(db);
    drop(transport);
    wipe_db(&cfg.db_path);

    // wipe 后的全新 db：生产 cipher 形态直调 `_with_ctx`（EB3 生产缝的
    // 核心函数；`CipherCtx::from_cfg(vfs_config(cfg))` = 生产构造）。
    let db_fresh = Arc::new(MetaDatabase::open(Path::new(&cfg.db_path)).expect("fresh db"));
    let ctx = CipherCtx::from_cfg(&vfs_config(&cfg));
    let outcome = rebuild_from_backend_with_ctx(
        driver.as_ref(),
        &db_fresh,
        &VocabRelPath::root(),
        RebuildLimits::default(),
        Some(ctx),
    )
    .await
    .expect("full-tree rebuild of the encrypted backend");
    assert_eq!(
        (outcome.files, outcome.dirs),
        (2, 1),
        "two files + one dir rebuilt: {outcome:?}"
    );

    // 重建行 ≡ 上传行（cipher 字段逐项同构——rebuild 与现查同轨的
    // e2e 级对账）。
    for (rel, enc, scheme, size) in &upload_truth {
        let row = db_fresh
            .get_file(rel)
            .expect("db read")
            .unwrap_or_else(|| panic!("rebuilt row {rel}"));
        assert_eq!(
            (row.is_encrypted, row.encryption_scheme.as_str(), row.size),
            (*enc, scheme.as_str(), *size),
            "{rel}: rebuilt cipher fields equal the upload-path truth"
        );
    }
    let docs = db_fresh
        .get_file("/docs")
        .expect("db read")
        .expect("dir row");
    assert!(
        docs.is_dir && !docs.is_encrypted,
        "directory rows stay plaintext through rebuild"
    );

    // 至少一文件经重建后的行读通（逐字节）。
    let transport2: Arc<dyn CloudTransport> = Arc::new(ck_local::LocalTransport::new(driver));
    let (_db2, vfs2) = build_stage(&cfg, transport2, inst.path().join("cache-stage3"));
    match vfs2.open_read(&v_alpha).await.expect("open_read alpha") {
        StreamSource::Stream {
            handle,
            total_size,
            transport: window,
        } => {
            assert_eq!(
                total_size,
                plain_alpha.len() as u64,
                "stream total = plaintext length"
            );
            let got = read_all(
                window
                    .open_range(&handle, 0, total_size)
                    .await
                    .expect("full window"),
            )
            .await;
            assert_eq!(got, plain_alpha, "stream arm reads byte-exact");
        }
        StreamSource::Hydrate => {
            let path = vfs2.hydrate(&v_alpha).await.expect("hydrate alpha");
            assert_eq!(
                std::fs::read(path).expect("read hydrated"),
                plain_alpha,
                "hydrate arm reads byte-exact"
            );
        }
    }
    eprintln!(
        "[leg3] rebuild converged: files={} dirs={} rows ≡ upload truth, one file read byte-exact",
        outcome.files, outcome.dirs
    );
}
