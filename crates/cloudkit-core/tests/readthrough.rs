//! Read-through 按需逐层索引原语（Phase 8 / RT2，
//! docs/plans/2026-09-22-readthrough-index.md Task 2）的行为测试。
//!
//! 契约（D1–D10 冻结语义，实现与测试的唯一依据）：
//!
//! - `Vfs::read_dir_fresh`（D5）：门不过（`authoritative_index` 关或
//!   `as_driver() = None`）→ 逐字退化为 `db.list_dir`（D2，telegram 零
//!   行为变化）；门开 → 每次调用现查后端（恰一次 list，A3）+ 物化落库
//!   + 单飞归并并发（D6）+ 删除双确认（D7）+ stale-if-error（R2）；
//! - `Vfs::stat_fresh`（D6）：父目录 TTL 窗（5s）内行命中零网络直出；
//!   过期/缺行重列父目录恰一次（风暴归并）；仍无行 `driver.stat`
//!   兜底；根恒存（合成元数据）；
//! - in-flight 行（`is_uploaded = 0` 且本地副本在盘，sync.rs 判据单点
//!   同源）两侧豁免（D7/D10，含 NotFound 臂——M1）；
//! - 加密实例「拒物化、不拒读」（D10 经 K83 裁决收窄）：加密 + 宽面卷
//!   的读面绝不回源物化（密文 size 会错标行），但就地退化到 db 索引
//!   读（D2 同形臂）——读行为与 read-through 之前逐字一致；rebuild 的
//!   拒收语义独立存在（ensure_plaintext_instance）；
//! - 写侧就近失效（pan115 先例）：commit_put / create_dir / remove_file
//!   后撤父目录 TTL 窗。
//!
//! Harness：MockStorageDriver + 真 sqlite + 真 Vfs（core/tests/rebuild.rs
//! 形态）；宽面经测试替身 transport 的 `as_driver` 探针暴露（六生产
//! transport_face 同形的最小测试替身）。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::{FileRecord, FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::{
    ByteStream, CloudTransport, RemoteHandle, StorageError, UploadJob, UploadReceipt,
};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig, VfsError};
use cloudkit_storage::{
    BackendHandle, Capabilities, Entry, EntryId, EntryKind, Listing, MockStorageDriver, Page,
    Range, RelPath as VolRel, StorageDriver, VolumeId, WriteHint,
};

// ------------------------------------------------------------- harness ---

/// list/stat 调用计数 + list 故障注入的共享观测面（mock 本体无 list
/// 计数/list 故障注入——薄包装委托 + 注入，AGENTS 关键背景）。
#[derive(Default)]
struct SharedCounters {
    list_calls: AtomicUsize,
    stat_calls: AtomicUsize,
    /// 注入队列：每次 `list` 消费一个错误（恰一次）。
    fail_list: Mutex<VecDeque<StorageError>>,
}

impl SharedCounters {
    fn list_calls(&self) -> usize {
        self.list_calls.load(Ordering::SeqCst)
    }

    fn stat_calls(&self) -> usize {
        self.stat_calls.load(Ordering::SeqCst)
    }

    fn fail_next_list(&self, err: StorageError) {
        self.fail_list
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(err);
    }
}

/// 计数/注入薄包装：委托内层 mock，list 前先消费注入并计数。
struct CountingDriver {
    inner: Arc<MockStorageDriver>,
    shared: Arc<SharedCounters>,
}

impl CountingDriver {
    fn new(inner: Arc<MockStorageDriver>, shared: Arc<SharedCounters>) -> Self {
        Self { inner, shared }
    }
}

#[async_trait]
impl StorageDriver for CountingDriver {
    fn volume(&self) -> &VolumeId {
        self.inner.volume()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn list(&self, dir: &VolRel, page: Page) -> Result<Listing, StorageError> {
        self.shared.list_calls.fetch_add(1, Ordering::SeqCst);
        // 真实后端的 list 必然经过网络挂起点——同步完成的 mock 不具备的
        // 形态在此补齐（单飞并发语义的可复现前提，用例 10）。
        tokio::task::yield_now().await;
        if let Some(err) = self
            .shared
            .fail_list
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
        {
            return Err(err);
        }
        self.inner.list(dir, page).await
    }

    async fn stat(&self, path: &VolRel) -> Result<Entry, StorageError> {
        self.shared.stat_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.stat(path).await
    }

    async fn mkdir(&self, path: &VolRel) -> Result<(), StorageError> {
        self.inner.mkdir(path).await
    }

    async fn delete(&self, id: &EntryId) -> Result<(), StorageError> {
        self.inner.delete(id).await
    }

    async fn rename(&self, from: &VolRel, to: &VolRel) -> Result<(), StorageError> {
        self.inner.rename(from, to).await
    }

    async fn reader(
        &self,
        id: &EntryId,
        range: Option<Range>,
    ) -> Result<cloudkit_storage::ByteStream, StorageError> {
        self.inner.reader(id, range).await
    }

    async fn writer(
        &self,
        path: &VolRel,
        hint: &WriteHint,
    ) -> Result<Box<dyn cloudkit_storage::UploadStager>, StorageError> {
        self.inner.writer(path, hint).await
    }

    async fn quota(&self) -> Result<cloudkit_storage::Quota, StorageError> {
        self.inner.quota().await
    }
}

/// 宽面测试 transport：持有驱动并经 `as_driver` 探针暴露（D1 探针的
/// 消费形态）；capabilities 照驱动申报（R4 诚实同款），`caps` 覆盖缝
/// 服务 D2 第二臂的注入（M8：宽面在场但能力位未申报）。
struct WideTransport<D: StorageDriver> {
    driver: D,
    /// 能力位覆盖：`None` = 照驱动申报；`Some` = 测试注入的申报。
    caps: Option<Capabilities>,
}
#[async_trait]
impl<D: StorageDriver> CloudTransport for WideTransport<D> {
    async fn connect(&self) -> Result<(), StorageError> {
        Ok(())
    }

    async fn upload(&self, _job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        // read-through 测试不走 transport 上传：put 的队列任务在此失败
        // 降级，行保持 pending（恰是 in-flight 形态，断言确定性不受影响）。
        Err(StorageError::Unsupported)
    }

    async fn open(&self, _file: &RemoteHandle) -> Result<ByteStream, StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn open_range(
        &self,
        _file: &RemoteHandle,
        _off: u64,
        _len: u64,
    ) -> Result<ByteStream, StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn delete_remote(&self, _handle: &RemoteHandle) -> Result<(), StorageError> {
        Err(StorageError::Unsupported)
    }

    fn capabilities(&self) -> Capabilities {
        self.caps.unwrap_or_else(|| self.driver.capabilities())
    }

    fn as_driver(&self) -> Option<&dyn StorageDriver> {
        Some(&self.driver)
    }
}

/// 谎报驱动（用例 5b）：`list` 委托真实节点表（残缺列表形态），`stat`
/// 对 `lie` 路径恒报 Ok——D7「谎报/残缺列表不得引发删除」的注入点。
struct LyingStatDriver {
    inner: Arc<MockStorageDriver>,
    /// stat 恒谎报存在的卷相对路径。
    lie: String,
}

#[async_trait]
impl StorageDriver for LyingStatDriver {
    fn volume(&self) -> &VolumeId {
        self.inner.volume()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn list(&self, dir: &VolRel, page: Page) -> Result<Listing, StorageError> {
        self.inner.list(dir, page).await
    }

    async fn stat(&self, path: &VolRel) -> Result<Entry, StorageError> {
        if path.as_str() == self.lie {
            return Ok(Entry {
                id: EntryId::new(
                    self.inner.volume().clone(),
                    BackendHandle::new("lie".to_string()),
                ),
                path: path.clone(),
                kind: EntryKind::File,
                size: 1,
                mtime: 1.0,
            });
        }
        self.inner.stat(path).await
    }

    async fn mkdir(&self, path: &VolRel) -> Result<(), StorageError> {
        self.inner.mkdir(path).await
    }

    async fn delete(&self, id: &EntryId) -> Result<(), StorageError> {
        self.inner.delete(id).await
    }

    async fn rename(&self, from: &VolRel, to: &VolRel) -> Result<(), StorageError> {
        self.inner.rename(from, to).await
    }

    async fn reader(
        &self,
        id: &EntryId,
        range: Option<Range>,
    ) -> Result<cloudkit_storage::ByteStream, StorageError> {
        self.inner.reader(id, range).await
    }

    async fn writer(
        &self,
        path: &VolRel,
        hint: &WriteHint,
    ) -> Result<Box<dyn cloudkit_storage::UploadStager>, StorageError> {
        self.inner.writer(path, hint).await
    }

    async fn quota(&self) -> Result<cloudkit_storage::Quota, StorageError> {
        self.inner.quota().await
    }
}

/// 宽面 harness：计数驱动 + 真 sqlite + 真 Vfs。
struct Harness {
    _dir: tempfile::TempDir,
    db: Arc<MetaDatabase>,
    vfs: Arc<Vfs>,
    mock: Arc<MockStorageDriver>,
    shared: Arc<SharedCounters>,
    cache_root: PathBuf,
}

fn test_cfg(encryption_password: Option<&str>) -> VfsConfig {
    VfsConfig {
        chunk_size_bytes: 64,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: encryption_password.map(str::to_string),
        encryption_scheme: cloudkit_core::config::EncryptionScheme::AeadV2,
        hydrate_timeout: Duration::from_secs(180),
    }
}

async fn wide_harness() -> Harness {
    wide_harness_cfg(None).await
}

async fn wide_harness_cfg(encryption_password: Option<&str>) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open db"));
    let cache_root = dir.path().join("cache");
    let mock = Arc::new(MockStorageDriver::new(
        VolumeId::parse("baidu:123456789").expect("volume id"),
    ));
    let shared = Arc::new(SharedCounters::default());
    let counting = CountingDriver::new(Arc::clone(&mock), Arc::clone(&shared));
    let transport = Arc::new(WideTransport {
        driver: counting,
        caps: None,
    });
    let cache = CacheManager::new(cache_root.clone(), u64::MAX);
    let vfs = Arc::new(Vfs::new(
        Arc::clone(&db),
        cache,
        transport,
        test_cfg(encryption_password),
    ));
    Harness {
        _dir: dir,
        db,
        vfs,
        mock,
        shared,
        cache_root,
    }
}

// -------------------------------------------------------- seed helpers ---

/// 卷相对播种一个文件（writer + write + close；父目录由 mock 隐式建）。
async fn seed_file(driver: &MockStorageDriver, path: &str, data: &[u8]) {
    let rel = VolRel::new(path).expect("seed path");
    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..Default::default()
    };
    let mut stager = driver.writer(&rel, &hint).await.expect("seed writer");
    stager.write(data).await.expect("seed write");
    stager.close().await.expect("seed close");
}

/// 卷相对播种一个目录。
async fn seed_dir(driver: &MockStorageDriver, path: &str) {
    driver
        .mkdir(&VolRel::new(path).expect("seed dir path"))
        .await
        .expect("seed mkdir");
}

/// 远端真删（stat 拿句柄 → delete）。
async fn remote_delete(driver: &MockStorageDriver, path: &str) {
    let rel = VolRel::new(path).expect("delete path");
    let id = driver.stat(&rel).await.expect("stat before delete").id;
    driver.delete(&id).await.expect("remote delete");
}

/// 直接落库一行已上传行（rebuild.rs harness 同款 FileUpsert 形态）。
fn seed_uploaded_row(db: &MetaDatabase, vpath: &str, msg_id: i64) {
    db.upsert_file(&FileUpsert {
        rel_path: vpath.to_string(),
        name: vpath.rsplit('/').next().unwrap_or(vpath).to_string(),
        parent_dir: parent_dir_of(vpath),
        size: 1,
        mtime: 1.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: Some(msg_id),
        is_uploaded: true,
        is_cached: false,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: None,
    })
    .expect("seed uploaded row");
}

/// 直接落库一行 pending 行（is_uploaded = 0）。
fn seed_pending_row(db: &MetaDatabase, vpath: &str, msg_id: i64) {
    db.upsert_file(&FileUpsert {
        rel_path: vpath.to_string(),
        name: vpath.rsplit('/').next().unwrap_or(vpath).to_string(),
        parent_dir: parent_dir_of(vpath),
        size: 3,
        mtime: 1.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: Some(msg_id),
        is_uploaded: false,
        is_cached: true,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: None,
    })
    .expect("seed pending row");
}

fn parent_dir_of(vpath: &str) -> String {
    match vpath.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => vpath[..i].to_string(),
        None => "/".to_string(),
    }
}

/// 在缓存树落一份本地副本（in-flight 判据的磁盘半边；路径数学经
/// CacheManager 同源规则）。
fn seed_local_copy(h: &Harness, vpath: &str, bytes: &[u8]) {
    let rel = RelPath::new(vpath).expect("local copy path");
    let mirror = CacheManager::new(h.cache_root.clone(), u64::MAX);
    let local = mirror.local_path(&rel);
    std::fs::create_dir_all(local.parent().expect("cache parent")).expect("mkdir cache parent");
    std::fs::write(local, bytes).expect("write local copy");
}

// -------------------------------------------------------------- tests ---

// 用例 9（A6 / D2 门退化）：窄面 transport（as_driver = None，telegram
// 形态）→ 行为与今日 `db.list_dir` 逐字一致 + 零网络。
#[tokio::test]
async fn gate_failure_degrades_verbatim_to_db_list_with_zero_network() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open db"));
    seed_uploaded_row(&db, "/a.txt", 1);
    seed_uploaded_row(&db, "/sub", 2);
    // 窄面 mock（as_driver 默认 None；capabilities 亦无 authoritative_index）
    let mock = Arc::new(MockTransport::new());
    let cache = CacheManager::new(dir.path().join("cache"), u64::MAX);
    let vfs = Vfs::new(
        Arc::clone(&db),
        cache,
        Arc::clone(&mock) as _,
        test_cfg(None),
    );

    let fresh = vfs
        .read_dir_fresh(&RelPath::root())
        .await
        .expect("degraded read_dir_fresh");
    let direct = db.list_dir("/").expect("direct db.list_dir");
    assert_eq!(fresh.len(), 2, "退化臂 = db.list_dir 逐字一致");

    // 行级全等（同库同行：关键列逐一比对，含排序契约）
    let sig = |row: &FileRecord| {
        (
            row.rel_path.clone(),
            row.name.clone(),
            row.parent_dir.clone(),
            row.size,
            row.mtime,
            row.is_dir,
            row.telegram_msg_id,
            row.is_uploaded,
            row.is_cached,
            row.chunk_count,
        )
    };
    let fresh_sigs: Vec<_> = fresh.iter().map(sig).collect();
    let direct_sigs: Vec<_> = direct.iter().map(sig).collect();
    assert_eq!(fresh_sigs, direct_sigs, "D2：退化必须逐字（同序同字段）");

    // 零网络：transport 的每个远端调用面都零调用
    assert!(
        mock.upload_calls().is_empty() && mock.open_calls().is_empty(),
        "门不过绝不允许任何网络流量（A6）"
    );
}

// 用例 1（A3 miss 回源物化）：db 空 + mock 远端 8 目录 → 根列表见 8 项
// + 行落库 + 恰 1 次 list。
#[tokio::test]
async fn miss_falls_through_to_the_driver_and_materializes_rows() {
    let h = wide_harness().await;
    for i in 0..8 {
        seed_dir(&h.mock, &format!("dir{i}")).await;
    }

    let rows = h
        .vfs
        .read_dir_fresh(&RelPath::root())
        .await
        .expect("read_dir_fresh on an empty index");
    assert_eq!(rows.len(), 8, "远端 8 目录全部可见");
    assert!(rows.iter().all(|row| row.is_dir));
    assert_eq!(
        h.shared.list_calls(),
        1,
        "A3：任一层访问的 list 调用数 = O(1)/层（恰 1 次）"
    );

    // 行落库（物化契约：权威索引行形态）
    let row =
        h.db.get_file("/dir3")
            .expect("read back")
            .expect("row materialized");
    assert!(row.is_dir);
    assert!(row.is_uploaded);
    assert_eq!(row.parent_dir, "/");
    assert_eq!(row.name, "dir3");
}

// 用例 2（D6 深跳）：深路径 miss → 恰 1 次父目录 list（按路径直址，
// 非根游走）+ 行命中。
#[tokio::test]
async fn deep_stat_miss_costs_exactly_one_parent_list() {
    let h = wide_harness().await;
    seed_file(&h.mock, "a/b/c.txt", b"hello").await;

    let row = h
        .vfs
        .stat_fresh(&RelPath::new("/a/b/c.txt").expect("deep path"))
        .await
        .expect("stat_fresh materializes the deep row");
    assert_eq!(row.rel_path, "/a/b/c.txt");
    assert_eq!(row.size, 5, "行命中 = 物化后的权威行");
    assert_eq!(
        h.shared.list_calls(),
        1,
        "D6：深跳路径 = 1 次 list(\"/a/b\") 即命中"
    );
}

// 用例 3（D6 stat 风暴归并）：连续 100 次同父 stat_fresh → 1 次父 list
//（TTL 窗内快路径直出，零网络）。
#[tokio::test]
async fn stat_storm_collapses_into_one_parent_list_within_the_ttl_window() {
    let h = wide_harness().await;
    for i in 0..100 {
        seed_file(&h.mock, &format!("many/f{i}.txt"), b"x").await;
    }

    for i in 0..100 {
        let rel = RelPath::new(&format!("/many/f{i}.txt")).expect("storm path");
        let row = h.vfs.stat_fresh(&rel).await.expect("storm stat");
        assert_eq!(row.name, format!("f{i}.txt"));
    }
    assert_eq!(
        h.shared.list_calls(),
        1,
        "D6：百次 stat 风暴归并为 1 次父 list"
    );
    assert_eq!(
        h.shared.stat_calls(),
        0,
        "D6：窗内快路径绝不触碰 driver.stat"
    );
}

// DirCache TTL 缝（with_ttl 挂账预登记的可调面）：窗内 fresh、过窗失效。
#[tokio::test]
async fn dir_cache_ttl_window_expires() {
    use cloudkit_core::readthrough::DirCache;
    let cache = DirCache::with_ttl(Duration::from_millis(50));
    cache.mark("/d");
    assert!(cache.fresh("/d"), "窗内必须 fresh");
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(!cache.fresh("/d"), "过窗必须失效（快路径退回回源）");
    assert!(!cache.fresh("/missing"), "未标记目录恒不 fresh");
}

// 用例 4（D5 强制刷新）：远端加文件 → read_dir_fresh 立即可见（无 TTL
// 等待——顺序调用每调必列，绝不吃窗）。
#[tokio::test]
async fn read_dir_fresh_force_refreshes_without_waiting_for_any_ttl() {
    let h = wide_harness().await;
    seed_dir(&h.mock, "docs").await;
    seed_file(&h.mock, "docs/a.txt", b"a").await;

    let rows = h
        .vfs
        .read_dir_fresh(&RelPath::new("/docs").expect("docs"))
        .await
        .expect("first view");
    assert_eq!(rows.len(), 1);
    assert_eq!(h.shared.list_calls(), 1);

    // 紧随其后（TTL 窗内）的远端新增 → 下一次调用立即可见。
    seed_file(&h.mock, "docs/b.txt", b"b").await;
    let rows = h
        .vfs
        .read_dir_fresh(&RelPath::new("/docs").expect("docs"))
        .await
        .expect("forced view");
    assert_eq!(rows.len(), 2, "D5：每次调用强制 revalidate，绝不供陈旧窗");
    assert_eq!(h.shared.list_calls(), 2, "D5：顺序调用必须各列一次");
}

// 用例 11（写侧就近失效，pan115 先例）：commit_put 撤父窗 → 下一次
// stat_fresh 强制重列（若快路径直出则计数停在 1）。
#[tokio::test]
async fn commit_put_invalidates_the_parent_window_so_stat_fresh_relists() {
    let h = wide_harness().await;
    seed_dir(&h.mock, "docs").await;
    seed_file(&h.mock, "docs/a.txt", b"a").await;
    h.vfs
        .read_dir_fresh(&RelPath::new("/docs").expect("docs"))
        .await
        .expect("warm the parent window");
    h.vfs
        .stat_fresh(&RelPath::new("/docs/a.txt").expect("a"))
        .await
        .expect("fast-path stat");
    assert_eq!(h.shared.list_calls(), 1, "前置：窗已喂饱（零网络直出）");

    // 本侧写入：pending 行落库 + 失效父窗（双保险）。
    h.vfs
        .put(&RelPath::new("/docs/new.txt").expect("new"), b"new", 1.0)
        .await
        .expect("put");
    let row = h
        .vfs
        .stat_fresh(&RelPath::new("/docs/new.txt").expect("new"))
        .await
        .expect("stat after put");
    assert_eq!(row.rel_path, "/docs/new.txt");
    assert!(!row.is_uploaded, "put 后行保持 pending（in-flight）");
    assert_eq!(
        h.shared.list_calls(),
        2,
        "写侧失效必须撤窗——stat_fresh 重列父目录而非快路径直出"
    );
}

// 写侧失效的 DirCache 缝：invalidate 提前撤窗。
#[tokio::test]
async fn dir_cache_invalidate_revokes_the_window_early() {
    use cloudkit_core::readthrough::DirCache;
    let cache = DirCache::with_ttl(Duration::from_secs(60));
    cache.mark("/d");
    assert!(cache.fresh("/d"));
    cache.invalidate("/d");
    assert!(!cache.fresh("/d"), "失效必须提前撤窗（不等 TTL）");
}

// 用例 5a（D7 删除双确认）：远端真删 → prune 候选经 driver.stat 复报
// NotFound 才删；其余行无伤。
#[tokio::test]
async fn missing_entries_are_deleted_only_after_a_not_found_double_confirm() {
    let h = wide_harness().await;
    seed_file(&h.mock, "docs/a.txt", b"a").await;
    seed_file(&h.mock, "docs/b.txt", b"b").await;
    h.vfs
        .read_dir_fresh(&RelPath::new("/docs").expect("docs"))
        .await
        .expect("materialize both");
    assert_eq!(h.shared.list_calls(), 1);

    // 远端真删 a.txt。
    remote_delete(&h.mock, "docs/a.txt").await;
    h.vfs
        .read_dir_fresh(&RelPath::new("/docs").expect("docs"))
        .await
        .expect("reconcile after remote delete");
    assert!(
        h.db.get_file("/docs/a.txt").expect("read").is_none(),
        "双确认 NotFound 后行必须删"
    );
    assert!(
        h.db.get_file("/docs/b.txt").expect("read").is_some(),
        "在列表内的行无伤"
    );
    assert_eq!(h.shared.list_calls(), 2);
    // D7 红线：删除经由 driver.stat 复核（1 候选 = 恰 1 次确认 stat）。
    assert_eq!(
        h.shared.stat_calls(),
        1,
        "删除必须经 driver.stat 双确认，绝不基于单次观测"
    );
}

// 用例 5b（D7 谎报防线）：list 不含某行、stat 却谎报 Ok → 行保留。
#[tokio::test]
async fn a_lying_stat_ok_keeps_the_row() {
    let mock = Arc::new(MockStorageDriver::new(
        VolumeId::parse("baidu:123456789").expect("volume id"),
    ));
    seed_file(&mock, "docs/a.txt", b"a").await;
    seed_file(&mock, "docs/b.txt", b"b").await;
    // 删前先取 a.txt 的远端句柄（test 侧的 mock Arc 与驱动 inner 同源）。
    let a_id = mock
        .stat(&VolRel::new("docs/a.txt").expect("path"))
        .await
        .expect("stat before delete")
        .id;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open db"));
    let cache = CacheManager::new(dir.path().join("cache"), u64::MAX);
    let transport = Arc::new(WideTransport {
        driver: LyingStatDriver {
            inner: Arc::clone(&mock),
            lie: "docs/a.txt".to_string(),
        },
        caps: None,
    });
    let vfs = Vfs::new(Arc::clone(&db), cache, transport, test_cfg(None));

    vfs.read_dir_fresh(&RelPath::new("/docs").expect("docs"))
        .await
        .expect("materialize both");
    // 远端真删 a.txt：此后 list 不再产出它，stat 却恒谎报 Ok。
    mock.delete(&a_id).await.expect("remote delete");

    vfs.read_dir_fresh(&RelPath::new("/docs").expect("docs"))
        .await
        .expect("reconcile with a lying stat");
    assert!(
        db.get_file("/docs/a.txt").expect("read").is_some(),
        "D7：stat 谎报 Ok → 行必须保留（删除永不基于单次观测）"
    );
    assert!(
        db.get_file("/docs/b.txt").expect("read").is_some(),
        "在列表内的行无伤"
    );
}

// 用例 6（D7 上限）：远端列表空洞（>32 缺失）→ 整批跳过删除 + 全部行
// 保留（疑似残缺列表；权威清账归 rebuild sweep）。
#[tokio::test]
async fn oversized_prune_batches_are_skipped_wholesale() {
    let h = wide_harness().await;
    seed_file(&h.mock, "docs/x.txt", b"x").await;
    // 库里 40 行陈旧行，远端全都不存在（40 > 32 上限）。
    for i in 0..40 {
        seed_uploaded_row(&h.db, &format!("/docs/stale{i}.txt"), 1000 + i as i64);
    }

    h.vfs
        .read_dir_fresh(&RelPath::new("/docs").expect("docs"))
        .await
        .expect("reconcile with an oversized prune batch");
    for i in 0..40 {
        assert!(
            h.db.get_file(&format!("/docs/stale{i}.txt"))
                .expect("read")
                .is_some(),
            "D7 上限：整批跳过删除，陈旧行全部保留（stale{i}）"
        );
    }
    assert!(
        h.db.get_file("/docs/x.txt").expect("read").is_some(),
        "在列表内的行照常物化"
    );
    assert_eq!(
        h.shared.stat_calls(),
        0,
        "超限批整批跳过——一个确认 stat 都不做"
    );
}

// 用例 13（NotFound 臂）：驱动报目录不存在 → 删本层行（目录自身行 +
// 直接子行，逐条双确认），更深层行留库由 rebuild sweep 清账，返回
// NotFound。
#[tokio::test]
async fn a_driver_not_found_prunes_the_local_layer_after_double_confirm() {
    let h = wide_harness().await;
    seed_file(&h.mock, "gone/child.txt", b"c").await;
    seed_dir(&h.mock, "gone/sub").await;
    // 根列物化 /gone 行，子列物化 child.txt / sub 行。
    h.vfs
        .read_dir_fresh(&RelPath::root())
        .await
        .expect("root layer");
    h.vfs
        .read_dir_fresh(&RelPath::new("/gone").expect("gone"))
        .await
        .expect("materialize the layer");
    // 更深层行直接落库（模拟早前更深层的索引残迹）。
    seed_uploaded_row(&h.db, "/gone/sub/deep.txt", 42);
    assert!(h.db.get_file("/gone").expect("read").is_some());
    assert!(h.db.get_file("/gone/child.txt").expect("read").is_some());

    // 远端整目录消失。
    remote_delete(&h.mock, "gone").await;
    let err = h
        .vfs
        .read_dir_fresh(&RelPath::new("/gone").expect("gone"))
        .await
        .expect_err("driver NotFound must surface");
    assert!(
        matches!(err, VfsError::NotFound(_)),
        "驱动 NotFound 必须回 NotFound，got {err:?}"
    );
    assert!(
        h.db.get_file("/gone").expect("read").is_none(),
        "目录自身行必须清（双确认后）"
    );
    assert!(
        h.db.get_file("/gone/child.txt").expect("read").is_none(),
        "直接子行必须清（双确认后）"
    );
    assert!(
        h.db.get_file("/gone/sub").expect("read").is_none(),
        "直接子目录行同层清"
    );
    assert!(
        h.db.get_file("/gone/sub/deep.txt").expect("read").is_some(),
        "更深层行留库——权威清账归 rebuild sweep（RT4）"
    );
}

// 用例 14（M1）：NotFound 臂的 in-flight 豁免——驱动报目录不存在时，
// pending 且本地副本在盘的行（在途上传）不得被双确认删除（D7「两侧永
// 不触碰」的 NotFound 第三面，与 upsert/prune 同一判据单点）；普通行
// 照常双确认删除。
#[tokio::test]
async fn a_driver_not_found_spares_in_flight_rows() {
    let h = wide_harness().await;
    seed_file(&h.mock, "gone/child.txt", b"c").await;
    h.vfs
        .read_dir_fresh(&RelPath::new("/gone").expect("gone"))
        .await
        .expect("materialize the layer");
    // 在途形态：pending 行 + 本地副本在盘（用例 7 同款播种）。
    seed_pending_row(&h.db, "/gone/wip.txt", 999);
    seed_local_copy(&h, "/gone/wip.txt", b"local");

    remote_delete(&h.mock, "gone").await;
    let err = h
        .vfs
        .read_dir_fresh(&RelPath::new("/gone").expect("gone"))
        .await
        .expect_err("driver NotFound must surface");
    assert!(
        matches!(err, VfsError::NotFound(_)),
        "驱动 NotFound 必须回 NotFound，got {err:?}"
    );
    assert!(
        h.db.get_file("/gone/child.txt").expect("read").is_none(),
        "普通行照常双确认删除"
    );
    assert!(
        h.db.get_file("/gone/wip.txt").expect("read").is_some(),
        "M1：在途行必须幸存——NotFound 臂与 upsert/prune 同判据豁免"
    );
}

// 用例 15（M4）：NotFound 臂的候选上限（与 reconcile 的 PRUNE_CANDIDATE_CAP
// 同款）——千级行目录的首次探测不得放大成 N 次串行确认 stat（winfsp 面
// = FSD 挂起防线）：超限整批跳过删除，直接回 NotFound。
#[tokio::test]
async fn a_driver_not_found_with_an_oversized_row_batch_skips_the_prune_wholesale() {
    let h = wide_harness().await;
    // 40 行陈旧行直接落库，远端没有任何对应物（list 一上来即 NotFound）。
    for i in 0..40 {
        seed_uploaded_row(&h.db, &format!("/gone/stale{i}.txt"), 2000 + i as i64);
    }

    let err = h
        .vfs
        .read_dir_fresh(&RelPath::new("/gone").expect("gone"))
        .await
        .expect_err("driver NotFound must surface");
    assert!(
        matches!(err, VfsError::NotFound(_)),
        "驱动 NotFound 必须回 NotFound，got {err:?}"
    );
    for i in 0..40 {
        assert!(
            h.db.get_file(&format!("/gone/stale{i}.txt"))
                .expect("read")
                .is_some(),
            "M4：超限批整批跳过删除，行全部幸存（stale{i}）"
        );
    }
    assert_eq!(
        h.shared.stat_calls(),
        0,
        "M4：整批跳过——一个确认 stat 都不做（防 FSD 挂起放大）"
    );
}

// 用例 7（D7/D10 in-flight 豁免）：pending 且本地副本在盘的行——
// upsert 侧不被回源物化覆盖（上传 worker 的成功写回不得被冲掉），
// prune 侧永不成为删除候选。
#[tokio::test]
async fn in_flight_rows_are_exempt_on_both_the_upsert_and_prune_sides() {
    let h = wide_harness().await;
    seed_file(&h.mock, "docs/keep.txt", b"k").await;
    // 竞态形态：远端已有同路径条目（并发上传刚落到远端），本地行仍
    // pending 且本地副本在盘。
    seed_file(&h.mock, "docs/race.txt", b"remote").await;
    seed_pending_row(&h.db, "/docs/race.txt", 777);
    seed_local_copy(&h, "/docs/race.txt", b"local");
    // prune 腿形态：远端列表里根本没有的 pending 行（未上传）。
    seed_pending_row(&h.db, "/docs/never.txt", 888);
    seed_local_copy(&h, "/docs/never.txt", b"local");

    h.vfs
        .read_dir_fresh(&RelPath::new("/docs").expect("docs"))
        .await
        .expect("reconcile with in-flight rows");

    // upsert 腿：race.txt 行不被物化覆盖（保持 pending + 原 msg_id）。
    let race = h.db.get_file("/docs/race.txt").expect("read").expect("row");
    assert!(!race.is_uploaded, "D7/D10：in-flight 行不得被回源物化覆盖");
    assert_eq!(
        race.telegram_msg_id,
        Some(777),
        "上传 worker 的写回归属行——msg_id 不得被远端句柄顶掉"
    );
    // prune 腿：远端不存在的 pending 行不被删。
    assert!(
        h.db.get_file("/docs/never.txt").expect("read").is_some(),
        "pending 行永不成为 prune 候选（is_uploaded=0 无远端副本，缺失是常态）"
    );
    // 对照：普通行照常物化。
    let keep = h.db.get_file("/docs/keep.txt").expect("read").expect("row");
    assert!(keep.is_uploaded, "非 in-flight 行照常物化");
}

// 用例 8（stale-if-error，R2）：list 瞬态 Err——有旧行照常返回旧列表
//（绝不因瞬态错清库/回 404）；无旧行 → Err（非 NotFound）。
#[tokio::test]
async fn transient_list_errors_serve_stale_rows_and_never_404() {
    // 有旧缓存：瞬态 Err → 照常服务旧行。
    let h = wide_harness().await;
    seed_file(&h.mock, "docs/a.txt", b"a").await;
    h.vfs
        .read_dir_fresh(&RelPath::new("/docs").expect("docs"))
        .await
        .expect("warm cache");
    h.shared
        .fail_next_list(StorageError::Unavailable("transient".to_string()));
    let rows = h
        .vfs
        .read_dir_fresh(&RelPath::new("/docs").expect("docs"))
        .await
        .expect("stale rows must still be served");
    assert_eq!(rows.len(), 1, "stale-if-error：有旧行照常服务");
    assert_eq!(rows[0].rel_path, "/docs/a.txt");

    // 无旧行：Err 且绝不落 NotFound（404 只留给确认不存在）。
    let h2 = wide_harness().await;
    seed_dir(&h2.mock, "cold").await;
    h2.shared
        .fail_next_list(StorageError::Unavailable("transient".to_string()));
    let err = h2
        .vfs
        .read_dir_fresh(&RelPath::new("/cold").expect("cold"))
        .await
        .expect_err("no cache + backend error must surface");
    assert!(
        !matches!(err, VfsError::NotFound(_)),
        "瞬态错绝不回 404，got {err:?}"
    );
    assert!(
        matches!(err, VfsError::Transport(_)),
        "瞬态错保持 transport 形态，got {err:?}"
    );
}

// 用例 10（D5/D6 单飞）：并发 8 次 read_dir_fresh(同目录) → 恰 1 次
// list（排队者被完成趟归并为直读 db）。
#[tokio::test]
async fn concurrent_read_dir_fresh_collapses_into_exactly_one_list() {
    let h = wide_harness().await;
    for i in 0..8 {
        seed_file(&h.mock, &format!("hot/f{i}.txt"), b"x").await;
    }
    let dir = RelPath::new("/hot").expect("hot");

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let vfs = Arc::clone(&h.vfs);
        let dir = dir.clone();
        tasks.spawn(async move { vfs.read_dir_fresh(&dir).await });
    }
    let mut callers = 0usize;
    while let Some(joined) = tasks.join_next().await {
        let rows = joined.expect("join").expect("read_dir_fresh");
        assert_eq!(rows.len(), 8, "每个并发调用者都拿到完整列表");
        callers += 1;
    }
    assert_eq!(callers, 8);
    assert_eq!(
        h.shared.list_calls(),
        1,
        "D5/D6 单飞：并发 8 次 → 恰 1 次 list（A3 归并裁判）"
    );
}

// 用例 12（D10 经 K83 裁决收窄：「拒物化、不拒读」）：加密 + 宽面卷的
// 读面绝不回源物化（密文容器的尺寸是密文尺寸，物化成行会破坏
// 「size = 明文」契约与 AEAD 预算数学），但也不拒收——就地退化到 db
// 索引读（D2 同形臂），读行为与 read-through 之前逐字一致（计划 §7
// 「加密卷零变化」的兑现）。零网络：不回源、不物化、不 prune、不 mark。
#[tokio::test]
async fn encrypted_instances_degrade_to_the_index_read_without_any_backend_calls() {
    let h = wide_harness_cfg(Some("pw")).await;
    // 远端有货也不许碰、更不许物化。
    seed_dir(&h.mock, "docs").await;
    seed_file(&h.mock, "docs/a.txt", b"a").await;
    // 索引里有行：读面照常服务索引视图。
    seed_uploaded_row(&h.db, "/kept.txt", 11);

    let rows = h
        .vfs
        .read_dir_fresh(&RelPath::root())
        .await
        .expect("encrypted instance degrades to the index read, it does not refuse");
    assert_eq!(
        rows.len(),
        1,
        "退化臂只回索引行——远端 docs 不得出现（零回源物化）"
    );
    assert_eq!(rows[0].rel_path, "/kept.txt");

    // 无行目录 → 空列表（db.list_dir 逐字退化，绝不 404）。
    let empty = h
        .vfs
        .read_dir_fresh(&RelPath::new("/nothing").expect("nothing"))
        .await
        .expect("a rowless directory degrades to an empty listing");
    assert!(empty.is_empty(), "无行目录回空列表，got {empty:?}");

    // stat_fresh 同臂：有行 → 行；无行 → NotFound（根也照 degrade——
    // 加密臂先于根合成臂，消费面的根语义由各自前置检查承担）。
    let row = h
        .vfs
        .stat_fresh(&RelPath::new("/kept.txt").expect("kept"))
        .await
        .expect("degraded stat serves the indexed row");
    assert_eq!(row.telegram_msg_id, Some(11));
    let missing = h
        .vfs
        .stat_fresh(&RelPath::new("/missing.bin").expect("missing"))
        .await
        .expect_err("a rowless stat degrades to NotFound");
    assert!(
        matches!(missing, VfsError::NotFound(_)),
        "无行 stat 退化 = NotFound，got {missing:?}"
    );
    let root = h.vfs.stat_fresh(&RelPath::root()).await;
    assert!(
        matches!(root, Err(VfsError::NotFound(_))),
        "加密卷根照 degrade（无行 NotFound，无合成），got {root:?}"
    );

    assert_eq!(
        h.shared.list_calls(),
        0,
        "拒物化：加密卷读面零 list（不回源、不物化、不 prune、不 mark）"
    );
    assert_eq!(h.shared.stat_calls(), 0, "加密卷读面零 driver.stat");
}

// 用例 16（M6）：stat_fresh 兜底 Ok 臂——父目录 list 瞬断（stale 臂吞
// 掉，无旧行可服务）→ driver.stat 兜底物化命中返回行（深路径仍可寻
// 址；R2「瞬态错绝不回 404」的兜底面收口）。
#[tokio::test]
async fn stat_fresh_falls_through_to_the_driver_stat_when_the_parent_list_fails() {
    let h = wide_harness().await;
    seed_file(&h.mock, "docs/deep/x.txt", b"x").await;

    h.shared
        .fail_next_list(StorageError::Unavailable("transient".to_string()));
    let row = h
        .vfs
        .stat_fresh(&RelPath::new("/docs/deep/x.txt").expect("deep"))
        .await
        .expect("the stat fallback must still address the deep path");
    assert_eq!(row.rel_path, "/docs/deep/x.txt");
    assert_eq!(row.size, 1);
    assert!(
        h.db.get_file("/docs/deep/x.txt").expect("read").is_some(),
        "兜底命中照常物化落库"
    );
    assert_eq!(
        h.shared.list_calls(),
        1,
        "父目录 list 恰一次（瞬断被 stale 臂吞掉）"
    );
    assert_eq!(h.shared.stat_calls(), 1, "兜底 driver.stat 恰一次");
}

// 用例 17（M8 / D2 第二臂）：宽面在场但 `authoritative_index = false`
// → 两入口零网络退化（能力位未申报权威索引，逐字 db 读——R4 能力诚
// 实的可执行面）。caps 覆盖缝注入申报，驱动本申报权威位。
#[tokio::test]
async fn a_wide_face_without_the_authoritative_bit_degrades_with_zero_network() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open db"));
    seed_uploaded_row(&db, "/kept.txt", 21);
    // 远端有货也不许碰（能力位关 → 零回源）。
    let mock = Arc::new(MockStorageDriver::new(
        VolumeId::parse("baidu:123456789").expect("volume id"),
    ));
    seed_dir(&mock, "docs").await;
    let shared = Arc::new(SharedCounters::default());
    let counting = CountingDriver::new(Arc::clone(&mock), Arc::clone(&shared));
    let transport = Arc::new(WideTransport {
        driver: counting,
        caps: Some(Capabilities {
            authoritative_index: false,
            ..Capabilities::default()
        }),
    });
    let cache = CacheManager::new(dir.path().join("cache"), u64::MAX);
    let vfs = Vfs::new(Arc::clone(&db), cache, transport, test_cfg(None));

    let rows = vfs
        .read_dir_fresh(&RelPath::root())
        .await
        .expect("degraded read_dir_fresh");
    assert_eq!(rows.len(), 1, "退化臂只回索引行，远端 docs 不得出现");
    assert_eq!(rows[0].rel_path, "/kept.txt");

    let row = vfs
        .stat_fresh(&RelPath::new("/kept.txt").expect("kept"))
        .await
        .expect("degraded stat");
    assert_eq!(row.telegram_msg_id, Some(21));
    let missing = vfs
        .stat_fresh(&RelPath::new("/missing.bin").expect("missing"))
        .await
        .expect_err("a rowless degraded stat is NotFound");
    assert!(matches!(missing, VfsError::NotFound(_)), "got {missing:?}");

    assert_eq!(shared.list_calls(), 0, "D2 第二臂：零 list（零回源）");
    assert_eq!(shared.stat_calls(), 0, "D2 第二臂：零 driver.stat");
}

// L2（审查批）：stat_fresh("/") 在宽面回合成根——根恒存（消费面
// RowMetaData::root 同形：is_dir、0 尺寸、born uploaded+cached），零
// 网络（不做父列表、不做 driver.stat）。
#[tokio::test]
async fn stat_fresh_root_serves_the_synthesized_record_on_a_wide_face() {
    let h = wide_harness().await;
    let root = h
        .vfs
        .stat_fresh(&RelPath::root())
        .await
        .expect("the root is constant");
    assert!(root.is_dir, "根恒为目录");
    assert_eq!(root.size, 0, "合成根 0 尺寸");
    assert!(
        root.is_uploaded && root.is_cached,
        "合成根 born uploaded + cached"
    );
    assert_eq!(h.shared.list_calls(), 0, "根恒存：零网络");
    assert_eq!(h.shared.stat_calls(), 0, "根恒存：零 driver.stat");
}
