//! 本地共性冒烟腿（Phase 8 / RT5）——**非 ignored**（进常规测试门）：
//! 真 `ck-local` 驱动（宽面 + `authoritative_index` 能力位）+ 空 sqlite +
//! 真 `Vfs`，验证 read-through 机制的第二实现后端（跨驱动共性面）。
//!
//! local = 直连文件系统：外部真值 = 文件系统真相，因此一条测试即可覆盖
//! read-through 的全部共性形态——
//!
//! 1. 空索引 `read_dir_fresh` 零 rebuild 即见（A1 形态）；
//! 2. `stat_fresh` 深跳命中（父目录重列恰一次后行直出）；
//! 3. 外部新增 → 下一次 `read_dir_fresh` 立即可见（D5 强制刷新）；
//! 4. 外部删除 → 行经**真驱动 stat 双确认**（NotFound）prune（D7）。
//!
//! 机制本体只有一份（`cloudkit-core::readthrough`，RT2 已 16 测试钉
//! 死语义）；本腿证明的是「真 local 驱动经 `LocalTransport::as_driver`
//! 探针 + `authoritative_index` 门」把机制接进真后端的装配面——RT1 的
//! 宽面探针腿与本腿拼成完整的 local 形证据链。
//!
//! 类型纪律：`Vfs` 面收 core 的 vpath 形 `RelPath`（`/x` 绝对式），
//! 驱动面收 `cloudkit_storage::RelPath`（卷内相对词汇）——本腿两者都用，
//! 与装配点（组合根）同构。L1→L3 dev-dep 边的裁决记录见 ck-webdav
//! tests/live_readthrough.rs 的 Cargo.toml 注释（check_layers 机械面
//! 通过；RT5 任务单指定其为裁决面）。

use std::{path::Path, sync::Arc, time::Duration};

use ck_local::{LocalDriver, LocalTransport};
use cloudkit_core::cache::CacheManager;
use cloudkit_core::config::EncryptionScheme;
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::rel_path::RelPath as VfsPath;
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};

/// 真 local 驱动 + 空 sqlite + 真 Vfs（明文实例）。夹具状态（db/cache）
/// 落在驱动根**外**的独立临时目录——生产装配两者亦分离（卷根由卷配置
/// 指定，meta.db 在实例家目录），混放会把夹具文件卷进索引视图。
fn harness(driver_root: &Path, state: &Path) -> (Arc<MetaDatabase>, Arc<Vfs>) {
    let driver = Arc::new(LocalDriver::new(driver_root.to_path_buf()).expect("local driver"));
    let db = Arc::new(MetaDatabase::open(&state.join("meta.db")).expect("open db"));
    let cache = CacheManager::new(state.join("cache"), u64::MAX);
    let transport = Arc::new(LocalTransport::new(driver));
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
    (db, vfs)
}

/// 外部真值播种（直接 fs 写——local 后端的外部真值形态）。
fn seed(root: &Path, rel: &str, len: usize, byte: u8) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("seed mkdir");
    }
    std::fs::write(&path, vec![byte; len]).expect("seed write");
}

fn names(rows: &[cloudkit_core::database::FileRecord]) -> Vec<String> {
    let mut names: Vec<String> = rows.iter().map(|row| row.name.clone()).collect();
    names.sort();
    names
}

#[tokio::test]
async fn readthrough_local_smoke_fresh_index_deep_stat_add_and_delete() {
    let volume_dir = tempfile::tempdir().expect("volume tempdir");
    let state_dir = tempfile::tempdir().expect("state tempdir");
    let root = volume_dir.path();

    // 外部真值：a.txt / docs/inner.txt（读穿发生前就在盘上）。
    seed(root, "a.txt", 4096, b'X');
    seed(root, "docs/inner.txt", 512, b'Y');

    let (db, vfs) = harness(root, state_dir.path());

    // ① 空索引零 rebuild 即见 + 行落库。
    let rows = vfs
        .read_dir_fresh(&VfsPath::new("/").expect("vpath"))
        .await
        .expect("read_dir_fresh root");
    assert_eq!(
        names(&rows),
        vec!["a.txt", "docs"],
        "empty index must see the externally seeded tree"
    );
    let a = db
        .get_file("/a.txt")
        .expect("db")
        .expect("a.txt row must be materialized");
    assert_eq!(a.size, 4096);
    assert!(!a.is_dir);

    // ② stat_fresh 深跳：父目录重列恰一次后行直出。
    let inner = vfs
        .stat_fresh(&VfsPath::new("/docs/inner.txt").expect("vpath"))
        .await
        .expect("stat_fresh deep path");
    assert_eq!(inner.size, 512);
    assert!(!inner.is_dir);

    // ③ 外部新增 → 下一次 read_dir_fresh 立即可见（D5）。
    seed(root, "docs/added.bin", 333, b'A');
    let rows = vfs
        .read_dir_fresh(&VfsPath::new("/docs").expect("vpath"))
        .await
        .expect("read_dir_fresh docs");
    assert_eq!(
        names(&rows),
        vec!["added.bin", "inner.txt"],
        "the external addition must be visible on the next fresh read (D5)"
    );

    // ④ 外部删除 → 行经真驱动 stat 双确认 prune（D7），兄弟行完好。
    std::fs::remove_file(root.join("docs/inner.txt")).expect("external rm");
    let rows = vfs
        .read_dir_fresh(&VfsPath::new("/docs").expect("vpath"))
        .await
        .expect("read_dir_fresh docs after external rm");
    assert_eq!(
        names(&rows),
        vec!["added.bin"],
        "the externally deleted file must vanish from the fresh view"
    );
    assert!(
        db.get_file("/docs/inner.txt").expect("db").is_none(),
        "the deleted file's row must be pruned (driver stat double-confirm NotFound)"
    );
    assert!(db.get_file("/docs/added.bin").expect("db").is_some());

    // ⑤ 删除后的深跳 stat_fresh：行已 prune、远端确无 → NotFound（不
    //    落半错语义行）。
    let missing = vfs
        .stat_fresh(&VfsPath::new("/docs/inner.txt").expect("vpath"))
        .await;
    assert!(
        missing.is_err(),
        "stat_fresh of a confirmed-absent path must err"
    );
}
