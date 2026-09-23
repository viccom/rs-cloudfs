//! materialize_entry = Entry → files 行的**唯一**物化映射（Phase 8 / D4）
//! ——rebuild 与 read-through 共用。本文件钉死的不变量：
//!
//! 1. 路径形句柄（local/sftp/webdav 形态）→ K6 `0` 占位落库，行经词汇
//!    转行键（"/docs/a.txt"）可读回；
//! 2. 数字形句柄（fs_id/msg_id 形态）直进 msg_id，且落 K11 单容器
//!    chunk 行（index 0、同 msg_id、整 size、无 sha）；
//! 3. 重物化刷新 updated_at（D8③ sweep 的「扫描期再触碰」判据根基），
//!    同 rel_path 冲突键归同一条行；
//! 4. （Phase 8-B EB1 / B1+B3+B4）cipher 真相三层优先：T1 物化行 ≡ 上传
//!    行的 cipher 语义列；T2 容器尺寸闭式精确（含边界与防御臂）；T3 既
//!    有行真相保留（防修正-回冲震荡）；T4 配置初值只填空（明文实例不坏
//!    legacy 加密行、Dir 行恒明文）。

use cloudkit_core::config::{SCHEME_AEAD_V2, SCHEME_GCM};
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::materialize::{
    list_all_pages, materialize_entry, plaintext_len_from_container, plaintext_len_with_chunk,
    CipherCtx, MaterializedRow,
};
use cloudkit_storage::{
    BackendHandle, ByteStream, Capabilities, Entry, EntryId, EntryKind, Listing, Page, PageCursor,
    Quota, Range, RelPath, StorageDriver, StorageError, UploadStager, VolumeId, WriteHint,
};

/// 构造一条后端条目（`handle` 形态即被测分叉：路径形 vs 数字形）。
fn entry(path: &str, kind: EntryKind, handle: &str, size: u64) -> Entry {
    Entry {
        id: EntryId::new(
            VolumeId::parse("baidu:123456789").expect("volume id"),
            BackendHandle::new(handle),
        ),
        path: RelPath::new(path).expect("合法卷内相对路径"),
        kind,
        size,
        mtime: 1_700_000_000.0,
    }
}

/// 真 sqlite 的临时库（rebuild 测试同款形态）。
fn db() -> (tempfile::TempDir, MetaDatabase) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open db");
    (dir, db)
}

#[test]
fn path_shaped_handle_degrades_to_the_k6_placeholder_and_reads_back() {
    let (_dir, db) = db();
    let e = entry("docs/a.txt", EntryKind::File, "/dav/docs/a.txt", 5);
    let row: MaterializedRow = materialize_entry(&db, &e, None).expect("materialize");
    assert_eq!(row.telegram_msg_id, Some(0), "路径形句柄 → K6 0 占位");
    assert!(row.is_uploaded, "权威索引行：字节已在后端");
    assert!(!row.is_encrypted, "明文语义行");
    let stored = db
        .get_file("/docs/a.txt")
        .expect("get_file")
        .expect("行已落库");
    assert_eq!(stored.size, 5, "词汇转行键后可按 \"/docs/a.txt\" 读回");
}

#[test]
fn numeric_handle_lands_as_the_msg_id_with_a_single_container_chunk() {
    let (_dir, db) = db();
    let e = entry("a.bin", EntryKind::File, "12345", 9);
    let row = materialize_entry(&db, &e, None).expect("materialize");
    assert_eq!(row.telegram_msg_id, Some(12345));
    let chunks = db.get_chunks_by_file_id(row.id).expect("chunks");
    assert_eq!(chunks.len(), 1, "K11 单容器 chunk 行");
    assert_eq!(chunks[0].chunk_index, 0);
    assert_eq!(chunks[0].telegram_msg_id, Some(12345));
    assert_eq!(chunks[0].size, 9);
    assert_eq!(chunks[0].sha256, None, "单容器行不带 sha");
}

#[test]
fn re_materializing_bumps_updated_at_and_keeps_the_coalesce_columns() {
    // sweep 保护（D8③）靠 updated_at 刷新——本测试钉住该不变量。
    let (_dir, db) = db();
    let e = entry("a.bin", EntryKind::File, "1", 1);
    let first = materialize_entry(&db, &e, None).expect("first materialize");
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = materialize_entry(&db, &e, None).expect("second materialize");
    assert!(
        second.updated_at.unwrap() > first.updated_at.unwrap(),
        "重物化必须**严格**刷新 updated_at（L1：>= 放过同一毫秒的假刷新）"
    );
    assert!(second.is_uploaded);
    assert_eq!(second.id, first.id, "rel_path 冲突键：同键归同一条行");
}

// ==================== Phase 8-B EB1（B1+B3+B4）：cipher 真相物化 ==========

// T1 同构：物化行的 cipher 语义列 ≡ 上传行（vfs.rs:500-506 同源写法）——
// 同一明文（1MiB）在 aead_v2 下的密文长 1_048_626（1MiB + 34B 头 + 16B
// tag，v2.rs 格式文档）：上传侧行按 R6 记明文 size，物化侧从密文长闭式反
// 推，两者 is_encrypted/encryption_scheme/size 必须逐字段全等。
#[test]
fn materialized_encrypted_row_is_field_identical_to_the_upload_row() {
    const CT: u64 = 1_048_626; // 密文长 = 1MiB 明文 + 34B 头 + 16B tag
    const PT: i64 = 1_048_576; // 明文长

    // ① 上传侧形状（照 vfs.rs:500-506）：is_encrypted = 密码在 +
    // upsert_file_scheme(cfg.encryption_scheme)，size 记明文（R6）。
    let (_dir1, db1) = db();
    db1.upsert_file_scheme(
        &FileUpsert {
            rel_path: "/v2.bin".to_string(),
            name: "v2.bin".to_string(),
            parent_dir: "/".to_string(),
            size: PT,
            mtime: 1_700_000_000.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: Some(7),
            is_uploaded: true,
            is_cached: false,
            is_encrypted: true,
            chunk_count: 1,
            mime_type: None,
        },
        SCHEME_AEAD_V2,
    )
    .expect("upload-side upsert");

    // ② 物化侧：同 entry（密文长）+ CipherCtx 初值 {enabled, aead_v2}。
    let (_dir2, db2) = db();
    let e = entry("v2.bin", EntryKind::File, "7", CT);
    let ctx = CipherCtx {
        enabled: true,
        scheme: SCHEME_AEAD_V2,
    };
    let mat = materialize_entry(&db2, &e, Some(&ctx)).expect("materialize");

    let up = db1
        .get_file("/v2.bin")
        .expect("read")
        .expect("upload row exists");
    assert_eq!(mat.is_encrypted, up.is_encrypted, "is_encrypted 全等");
    assert_eq!(
        mat.encryption_scheme, up.encryption_scheme,
        "encryption_scheme 全等"
    );
    assert_eq!(mat.size, up.size, "size 全等");
    assert!(mat.is_encrypted, "cipher 初值：行标加密");
    assert_eq!(mat.encryption_scheme, SCHEME_AEAD_V2, "cipher 初值 scheme");
    assert_eq!(
        mat.size, PT,
        "密文长 1_048_626 → 明文 1_048_576（aead_v2 1MiB 闭式）"
    );
}

// T2 闭式边界（格式文档语义，v2.rs 头部格式为权威）：空件/恰整倍数/多块
// 短尾 + 防御臂（结构违例与未知方案 → 保守回 ct，B2 首读会修）。
#[test]
fn container_size_backsolves_plaintext_exactly() {
    // ---- v1（冻结：16B 盐 + 12B nonce + 密文 + 16B tag，无 magic）----
    assert_eq!(
        plaintext_len_from_container(44, SCHEME_GCM),
        0,
        "v1 空件（ct = 44）"
    );
    assert_eq!(
        plaintext_len_from_container(44 + 9, SCHEME_GCM),
        9,
        "v1 普通件 ct − 44"
    );

    // ---- v2（34B CKCRYPT2 头 + 分块流，每块密文 + 16B tag）----
    assert_eq!(
        plaintext_len_from_container(50, SCHEME_AEAD_V2),
        0,
        "v2 空件（34 + 16，单个空 final 块）"
    );
    // 恰整倍数满尾块：34 + (1MiB + 16) = 1_048_626（与 T1 密文长同值）。
    assert_eq!(
        plaintext_len_from_container(34 + 1_048_576 + 16, SCHEME_AEAD_V2),
        1_048_576,
        "恰整倍数 → 满尾块（无尾随空块）"
    );
    // 多块 + 短尾：34 + (1MiB + 16) + (1_000_000 + 16) → 1MiB + 1_000_000。
    assert_eq!(
        plaintext_len_from_container(34 + 1_048_576 + 16 + 1_000_000 + 16, SCHEME_AEAD_V2),
        1_048_576 + 1_000_000,
        "多块 + 短尾（自 delimiting 闭式）"
    );

    // ---- 防御臂（Step 3 规格）：未知方案 / 结构违例 → 保守回 ct + warn ----
    assert_eq!(
        plaintext_len_from_container(1_234, "rot13"),
        1_234,
        "未知方案防御臂：回 ct 原值"
    );
    assert_eq!(
        plaintext_len_from_container(10, SCHEME_GCM),
        10,
        "v1 短于 44B 开销 → 防御臂回 ct"
    );
    assert_eq!(
        plaintext_len_from_container(40, SCHEME_AEAD_V2),
        40,
        "v2 短于 34B 头 + 16B tag → 防御臂回 ct"
    );
    assert_eq!(
        plaintext_len_from_container(1_048_627, SCHEME_AEAD_V2),
        1_048_627,
        "尾块长超 1MiB chunk 上限 → 防御臂回 ct"
    );
}

// T4 只填空（B1）：配置初值只在「无 cipher 真相」时落库——无行 → cfg
// 推导值；既有 is_encrypted = 1 行在**明文实例**回源后仍 is_encrypted = 1
//（该路径今日走 upsert_file 会把行写坏——B1 要堵的洞）；Dir 行恒明文。
#[test]
fn config_guess_fills_only_absent_truth() {
    // (a1) 无行 + 加密实例初值 → cfg 推导值落库（加密 + 配置 scheme + 闭式尺寸）。
    let (_d1, db1) = db();
    let enc = CipherCtx {
        enabled: true,
        scheme: SCHEME_AEAD_V2,
    };
    let e1 = entry("fresh.bin", EntryKind::File, "1", 60);
    let fresh = materialize_entry(&db1, &e1, Some(&enc)).expect("materialize");
    assert!(fresh.is_encrypted, "无行 → 加密实例初值：行标加密");
    assert_eq!(fresh.encryption_scheme, SCHEME_AEAD_V2, "无行 → cfg scheme");
    assert_eq!(fresh.size, 10, "无行 → 60B 容器按 aead_v2 闭式 → 10B 明文");

    // (a2) 无行 + 明文实例初值 → 明文行（entry.size 即明文，无闭式）。
    let plain = CipherCtx {
        enabled: false,
        scheme: SCHEME_AEAD_V2,
    };
    let e2 = entry("plain.bin", EntryKind::File, "2", 500);
    let fresh_plain = materialize_entry(&db1, &e2, Some(&plain)).expect("materialize");
    assert!(!fresh_plain.is_encrypted, "无行 → 明文实例初值：不标加密");
    assert_eq!(fresh_plain.size, 500, "明文行尺寸 = entry.size 原样");

    // (b) 既有 is_encrypted = 1 行（legacy 真相）+ **明文实例**回源 →
    // 永不降级（B1：列举猜测绝不冲掉 cipher 真相）。
    let (_d2, db2) = db();
    db2.upsert_file_scheme(
        &FileUpsert {
            rel_path: "/legacy.bin".to_string(),
            name: "legacy.bin".to_string(),
            parent_dir: "/".to_string(),
            size: 9,
            mtime: 1_700_000_000.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: Some(3),
            is_uploaded: true,
            is_cached: false,
            is_encrypted: true,
            chunk_count: 1,
            mime_type: None,
        },
        SCHEME_GCM,
    )
    .expect("seed legacy encrypted row");
    let e3 = entry("legacy.bin", EntryKind::File, "3", 53); // v1 密文长 53 = 9 + 44
    let relisted = materialize_entry(&db2, &e3, Some(&plain)).expect("relist");
    assert!(
        relisted.is_encrypted,
        "明文实例回源绝不降级既有加密行（B1 永不降级），got is_encrypted=false"
    );

    // (c) Dir 行恒明文（即使加密实例初值在场）。
    let ed = entry("d", EntryKind::Dir, "9", 0);
    let dir_row = materialize_entry(&db2, &ed, Some(&enc)).expect("materialize dir");
    assert!(
        !dir_row.is_encrypted,
        "目录行永不明文化（is_encrypted 恒 false）"
    );
}

// T3 既有真相保留 / 防震荡（B1 核心）：首读修正后的行（is_encrypted = 1、
// scheme = gcm、size = 真值）在 cfg = aead_v2 的实例回源后 scheme/size
// 不被冲——scheme 保留原值、size 按**保留的 scheme** 从密文长重推（非按
// cfg 猜）；连续两拍回源恒稳（无修正-回冲震荡）。
#[test]
fn relisting_never_downgrades_a_corrected_encrypted_row() {
    let (_dir, db) = db();
    // 播种：模拟首读修正后的行——is_encrypted = 1、scheme = gcm、
    // size = 9（53B v1 密文容器的明文真值，53 − 44 = 9）。
    db.upsert_file_scheme(
        &FileUpsert {
            rel_path: "/legacy.bin".to_string(),
            name: "legacy.bin".to_string(),
            parent_dir: "/".to_string(),
            size: 9,
            mtime: 1_700_000_000.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: Some(5),
            is_uploaded: true,
            is_cached: false,
            is_encrypted: true,
            chunk_count: 1,
            mime_type: None,
        },
        SCHEME_GCM,
    )
    .expect("seed corrected gcm row");

    // 实例 cfg = aead_v2 + 密码在；回源同名 entry（entry.size = 53 密文长）。
    let ctx = CipherCtx {
        enabled: true,
        scheme: SCHEME_AEAD_V2,
    };
    let e = entry("legacy.bin", EntryKind::File, "5", 53);
    let row = materialize_entry(&db, &e, Some(&ctx)).expect("relist");
    assert_eq!(
        row.encryption_scheme, SCHEME_GCM,
        "既有真相保留——cfg=aead_v2 不得回冲 scheme"
    );
    assert_eq!(
        row.size, 9,
        "按保留的 gcm 闭式重推（53 − 44 = 9），非按 cfg 猜"
    );
    assert!(row.is_encrypted, "is_encrypted 恒保持 1");

    // 第二拍回源：同样不被冲（震荡防线——首读修正永不被列举回冲）。
    let again = materialize_entry(&db, &e, Some(&ctx)).expect("relist again");
    assert_eq!(again.encryption_scheme, SCHEME_GCM, "第二拍 scheme 仍 gcm");
    assert_eq!(again.size, 9, "第二拍 size 仍真值");
    assert!(again.is_encrypted, "第二拍仍标加密");
}

// T5（Phase 8-B 审查批 / 钉测补齐）：`upsert_materialized` 的 ON CONFLICT
// CASE 保留集**直接**钉在 SQL 面上——不经 `materialize_entry` 的 Rust 侧
// 读-改写（那层会先把既有真相读出来再喂对值，把 SQL 兜底掩盖掉）。
// 变异杀手段：把 CASE 删掉改回 `excluded` 直写（对齐 `upsert_file` 的
// 无条件覆盖）本测试必红——并发写窗内（Rust 读-改写之外）SQL 侧是
// 「列举永不降级 cipher 真相」的唯一防线。
#[test]
fn upsert_materialized_conflict_preserves_the_cipher_truth_columns() {
    let (_dir, db) = db();
    // 播种：首读修正后的 legacy 加密行（is_encrypted=1、scheme=gcm、
    // size=9）。
    db.upsert_file_scheme(
        &FileUpsert {
            rel_path: "/case.bin".to_string(),
            name: "case.bin".to_string(),
            parent_dir: "/".to_string(),
            size: 9,
            mtime: 1_700_000_000.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: Some(3),
            is_uploaded: true,
            is_cached: false,
            is_encrypted: true,
            chunk_count: 1,
            mime_type: None,
        },
        SCHEME_GCM,
    )
    .expect("seed legacy encrypted row");

    // 冲突臂直写：绕过 materialize_entry 的 Rust 面，**故意**送一个
    // 明文形状 + 另一方案的载荷（模拟并发窗内的最坏写者——若 SQL 不
    // 兜底，这发写入就会把 cipher 真相冲掉）。
    let id = db
        .upsert_materialized(
            &FileUpsert {
                rel_path: "/case.bin".to_string(),
                name: "case.bin".to_string(),
                parent_dir: "/".to_string(),
                size: 500,
                mtime: 1_700_000_000.0,
                sha256: None,
                is_dir: false,
                telegram_msg_id: Some(4),
                is_uploaded: true,
                is_cached: false,
                is_encrypted: false,
                chunk_count: 1,
                mime_type: None,
            },
            SCHEME_AEAD_V2,
        )
        .expect("conflicting materialized upsert");

    let row = db
        .get_file("/case.bin")
        .expect("db read")
        .expect("row exists");
    assert_eq!(row.id, id, "冲突键归同一条行");
    assert!(
        row.is_encrypted,
        "is_encrypted=1 经 CASE 保留——excluded 的 false 不得落地"
    );
    assert_eq!(
        row.encryption_scheme, SCHEME_GCM,
        "scheme 经 CASE 保留——excluded 的 aead_v2 不得落地"
    );
    assert_eq!(
        row.size, 500,
        "size = excluded（调用方派生值，按设计可写——B1 的 ?derived 语义）"
    );
}

// T6（Phase 8-B 审查批 / 钉测补齐）：非默认分块的「头权威」差分——
// `AeadV2::with_chunk_size(64*1024)` 造出的真容器，其密文长只有代入
// **头内真值分块**的 `plaintext_len_with_chunk` 才反推得回明文长；
// 恒按默认 1MiB 分块反推的 `plaintext_len_from_container` 对非默认分
// 块必然算错。这是 `first_read_admit` 头权威分支（头参数 > 应用默认）
// 的存在意义：没有它，非默认分块容器在列举/首读修正里永远拿错尺寸。
#[test]
fn header_chunk_size_wins_over_the_default_backsolve_for_non_default_containers() {
    const CHUNK: usize = 64 * 1024; // = MIN_CHUNK_SIZE，合法非默认分块
    let scheme = cloudkit_crypto::AeadV2::with_chunk_size(CHUNK)
        .expect("64 KiB is the minimum legal chunk size");
    // 100_000 = 65536 + 34464：恰跨两块（非默认分块的多块形态）。
    let plain: Vec<u8> = (0..100_000usize).map(|i| (i % 251) as u8).collect();
    let ct = scheme.encrypt("pw", &plain);
    let ct_len = ct.len() as i64;

    // 头参数代入：34 + (65536+16) + (34464+16) → 恰回明文长。
    assert_eq!(
        plaintext_len_with_chunk(ct_len, CHUNK as i64),
        Some(plain.len() as i64),
        "头内真值分块代入闭式 → 精确明文长"
    );
    // 默认 1MiB 分块反推：同一 ct 按单块尾算 → 必然 ≠ 明文长
    //（100_000 明文 + 32B 尾块 tag 混进「尾块明文」——默认闭式无法
    // 区分，把 tag 计成明文）。差分成立 = 头权威分支不可删。
    let default_backsolve = plaintext_len_from_container(ct_len, SCHEME_AEAD_V2);
    assert_ne!(
        default_backsolve,
        plain.len() as i64,
        "默认 1MiB 反推对非默认分块必然算错（头权威分支的存在意义）"
    );
    assert_eq!(
        default_backsolve,
        ct_len - 34 - 16,
        "seed sanity: 默认闭式把两块当一块单尾（尾块长含 32B tag 误差）"
    );
}

// ============================== M2（Phase 8 审查批）：分页归集双上限 ===

/// 自指游标的故障驱动（M2 注入点）：`list` 恒回 `per_page` 条目 + 恒有
/// 续读游标——读路径绝不允许被它挂死。
struct CursorLoopDriver {
    volume: VolumeId,
    per_page: usize,
}

impl CursorLoopDriver {
    fn new(per_page: usize) -> Self {
        Self {
            volume: VolumeId::parse("baidu:123456789").expect("volume id"),
            per_page,
        }
    }

    fn make_entry(&self, index: usize) -> Entry {
        Entry {
            id: EntryId::new(self.volume.clone(), BackendHandle::new(index.to_string())),
            path: RelPath::new(&format!("loop/f{index}.txt")).expect("path"),
            kind: EntryKind::File,
            size: 1,
            mtime: 1.0,
        }
    }
}

#[async_trait::async_trait]
impl StorageDriver for CursorLoopDriver {
    fn volume(&self) -> &VolumeId {
        &self.volume
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::none()
    }

    async fn list(&self, _dir: &RelPath, _page: Page) -> Result<Listing, StorageError> {
        Ok(Listing {
            entries: (0..self.per_page).map(|i| self.make_entry(i)).collect(),
            next: Some(PageCursor::Next("loop".to_string())),
        })
    }

    async fn stat(&self, _path: &RelPath) -> Result<Entry, StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn mkdir(&self, _path: &RelPath) -> Result<(), StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn delete(&self, _id: &EntryId) -> Result<(), StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn rename(&self, _from: &RelPath, _to: &RelPath) -> Result<(), StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn reader(
        &self,
        _id: &EntryId,
        _range: Option<Range>,
    ) -> Result<ByteStream, StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn writer(
        &self,
        _path: &RelPath,
        _hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn quota(&self) -> Result<Quota, StorageError> {
        Err(StorageError::Unsupported)
    }
}

// M2 红：恒 Some(next) 的自指游标必须在页数上限处报错返回（绝不挂死
// 读路径 / winfsp FSD 线程），错误消息含目录与计数。
#[tokio::test]
async fn a_self_feeding_cursor_is_capped_by_the_page_limit_instead_of_hanging() {
    let driver = CursorLoopDriver::new(0);
    let dir = RelPath::new("docs").expect("dir");
    let err = list_all_pages(&driver, &dir)
        .await
        .expect_err("the page cap must fire on a self-feeding cursor");
    let message = err.to_string();
    assert!(
        message.contains("docs"),
        "错误消息必须含目录，got: {message}"
    );
    assert!(
        message.contains("1024") || message.contains("page"),
        "错误消息必须点名页数上限，got: {message}"
    );
}

// M2 红：小页大流的自指游标在条目上限处报错（页数上限不误触）。
#[tokio::test]
async fn an_endless_entry_stream_is_capped_by_the_entry_limit() {
    let driver = CursorLoopDriver::new(128);
    let dir = RelPath::new("docs").expect("dir");
    let err = list_all_pages(&driver, &dir)
        .await
        .expect_err("the entry cap must fire on an endless entry stream");
    let message = err.to_string();
    assert!(
        message.contains("docs"),
        "错误消息必须含目录，got: {message}"
    );
    assert!(
        message.contains("65536") || message.contains("entries"),
        "错误消息必须点名条目上限，got: {message}"
    );
}
