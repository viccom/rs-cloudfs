//! K85.6（负责人 2026-09-23 裁决「事2 修」）—— **明文 0 字节在权威后端
//! 落真实对象**的用户可见形态端到端腿（非 ignored，进常规 CI）。
//!
//! 缺陷现场：明文 0 字节文件 + 权威索引后端（`authoritative_index=true`，
//! 即 local/sftp/webdav/baidu/pan115/pan123 六宽面驱动）走 Contract 6 的
//! 「0 字节不触远端」跳过 → 远端不落任何对象 → 本地索引丢失后该文件从
//! 网盘视角消失（read-through / rebuild 都建不出它的行，文件名消失）。
//! 加密空件已在 EB4 修（fd4f3c2，闸 = 加密 + 密码 + authoritative）；
//! K85.6 把闸放宽为「**权威索引后端即放行**（加密与否都可）」。
//!
//! 本腿钉目标可见性：明文 0 字节上传 → 排空 → **远端（local 驱动后台
//! 目录）确有该对象且长 0** → wipe db → `read_dir_fresh` 回源物化 →
//! **该文件出现在列表里**（修复前会消失）。
//!
//! 单测面（`cloudkit-core/tests/upload_queue.rs`）另有两条：权威后端
//! 明文 0 字节触远端（真对象 + 行 / 尺寸）、影子索引（telegram 形态）
//! 跳过保持——本腿补的是「用户可见的列表可见性」这一跳。

#![cfg(feature = "local")]

use std::path::Path;
use std::sync::Arc;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::config::{Backend, CyDriveConfig};
use cloudkit_core::database::{FileRecord, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::CloudTransport;
use cloudkit_core::vfs::Vfs;

fn unix_stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis()
}

/// 明文实例（local 后端，**无密码**——正是本缺陷的现场）。
fn plaintext_instance_cfg(dir: &Path, backend_root: &Path) -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Local,
        local_root: Some(backend_root.to_string_lossy().into_owned()),
        db_path: dir.join("meta.db").to_string_lossy().into_owned(),
        cache_path: dir.join("cache").to_string_lossy().into_owned(),
        enable_encryption: false,
        encryption_password: None,
        ..CyDriveConfig::default()
    }
}

/// 阶段切换的 wipe：删 db 主文件 + WAL/SHM 伴随文件（不存在即忽略）。
fn wipe_db(db_path: &str) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{db_path}{suffix}"));
    }
}

fn build_stage(
    cfg: &CyDriveConfig,
    transport: Arc<dyn CloudTransport>,
    cache_dir: std::path::PathBuf,
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

fn row_of<'a>(rows: &'a [FileRecord], rel: &str) -> Option<&'a FileRecord> {
    rows.iter().find(|r| r.rel_path == rel)
}

#[tokio::test]
async fn plaintext_zero_byte_survives_a_db_wipe_on_an_authoritative_backend() {
    let backend = tempfile::tempdir().expect("backend tempdir");
    let inst = tempfile::tempdir().expect("instance tempdir");
    let cfg = plaintext_instance_cfg(inst.path(), backend.path());

    let driver = ck_local::factory(&ck_local::LocalParams {
        root: backend.path().to_path_buf(),
    })
    .await
    .expect("local driver over the TempDir backend");
    let transport: Arc<dyn CloudTransport> = Arc::new(ck_local::LocalTransport::new(driver));
    assert!(
        transport.capabilities().authoritative_index,
        "the local driver is an authoritative-index backend (the gate must open)"
    );

    // 命名字段带轮次戳（K72/K77.6 去重纪律）——本次空件是唯一载荷。
    let stamp = unix_stamp();
    let empty_rel = format!("/empty-{stamp}.bin");
    let sibling_rel = format!("/sibling-{stamp}.bin");
    let v_empty = RelPath::new(&empty_rel).expect("vpath");
    let v_sibling = RelPath::new(&sibling_rel).expect("vpath");

    // ---- 阶段 ①：明文上传（0 字节空件 + 一个非空对照）----
    {
        let (db, vfs) = build_stage(&cfg, Arc::clone(&transport), inst.path().join("cache"));
        vfs.put(&v_empty, b"", 1_700_000_000.0)
            .await
            .expect("put empty");
        vfs.put(&v_sibling, b"non-empty", 1_700_000_000.0)
            .await
            .expect("put sibling");
        vfs.shutdown().await; // 排空上传队列到终态

        let row = db
            .get_file(&empty_rel)
            .expect("db read")
            .expect("uploaded empty row exists");
        assert!(row.is_uploaded, "0-byte row lands uploaded");
        assert_eq!(row.size, 0, "plaintext row keeps size 0");
    }

    // ---- 远端核验：后端目录里确实有该对象且恰为 0 字节 ----
    let backend_name = empty_rel.trim_start_matches('/');
    let remote_path = backend.path().join(backend_name);
    assert!(
        remote_path.exists(),
        "the 0-byte object must exist on an authoritative backend (Contract 6 \
         skip would leave it remote-absent)"
    );
    assert_eq!(
        std::fs::metadata(&remote_path)
            .expect("stat the remote object")
            .len(),
        0,
        "the plaintext remote object is exactly 0 bytes"
    );

    // ---- 阶段 ②：wipe db → 全新空 db → read_dir_fresh 回源物化 ----
    wipe_db(&cfg.db_path);
    let (db2, vfs2) = build_stage(
        &cfg,
        Arc::clone(&transport),
        inst.path().join("cache-stage2"),
    );
    assert!(
        db2.list_dir("/").expect("fresh db read").is_empty(),
        "the stage-2 db starts empty — every row must come from the backend"
    );

    let rows = vfs2
        .read_dir_fresh(&RelPath::root())
        .await
        .expect("read_dir_fresh root");

    // 这正是修复的目标：修复前空件不在远端 → 物化列表里根本没有它。
    let empty = row_of(&rows, &empty_rel).unwrap_or_else(|| {
        panic!(
            "{empty_rel} must be visible after a db wipe on an authoritative \
             backend: {} row(s) present ({:?})",
            rows.len(),
            rows.iter().map(|r| r.rel_path.as_str()).collect::<Vec<_>>()
        )
    });
    assert_eq!(
        empty.size, 0,
        "the rematerialized 0-byte row keeps size 0 (it was a real 0-byte object)"
    );
    assert!(!empty.is_dir, "it rematerializes as a file");
    row_of(&rows, &sibling_rel).expect("the non-empty sibling also rematerializes");
}
