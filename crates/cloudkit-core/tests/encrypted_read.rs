//! Phase 8-B EB2（B2+B6）：加密读的**首读内容校验与行回写**。
//!
//! 内容是唯一权威（B2）：任何字节/Content-Length 交给客户端之前，首读
//! 必须完成——
//!
//! 1. `stream_arm_repairs_a_wrong_scheme_row_before_serving`：错 scheme
//!    （gcm 标签）/错 size（三方账目自相矛盾的错账）行 + 远端真 v2 容器
//!    → `open_read` 流式窗口逐字节 = 明文 + 行回写 aead_v2/真值 +
//!    coalesce 列（sha256/msg_id）原值保留（定向 UPDATE 钉）；
//! 2. `hydrate_repairs_size_from_local_plaintext_length`：错 size 行 →
//!    Hydrate 全量读通 + 行 size = 本地明文长度（解密后真值回写）；
//! 3. `gcm_labelled_v2_content_redispatches`：行标 gcm、内容 `CKCRYPT2`
//!    → hydrate 分发处**先看头 8B 再 v1 decrypt**，改判 v2 成功 +
//!    回写 scheme/size（而非 v1 解密失败）；
//! 4. `v2_labelled_non_container_fails_actionably`（B6 文案钉）：行标
//!    aead_v2、内容非容器 → 可行动 Err（双假说 + `cydrive sync`），
//!    **绝不返回原文密文字节**。
//!
//! 面级传导（Content-Length 与实发字节数一致）钉在
//! `cloudkit-webdav/tests/fs_adapter.rs`。
//!
//! Harness 照 `vfs_open_read.rs` 既有形态：MockTransport（宽面替身的
//! CloudTransport 面）+ 真 sqlite + 真 Vfs；加密内容用
//! `cloudkit_crypto` 现造。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::config::{SCHEME_AEAD_V2, SCHEME_GCM};
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::{ByteStream, CloudTransport, UploadJob, UploadReceipt};
use cloudkit_core::vfs::{StreamSource, Vfs, VfsConfig};
use futures_util::StreamExt;

/// sha256 coalesce 列的哨兵值：首读回写绝不动它（B2 定向 UPDATE 钉）。
const SHA_SENTINEL: &str = "7f83b1657ff1fc53b92dc18148a1d65dfc2d4b1fa3d677284addd200126d9069";

/// VfsConfig：可选加密密码；其余缺省（读路径只消费密码门与行字段）。
fn test_cfg(encryption_password: Option<&str>) -> VfsConfig {
    VfsConfig {
        encryption_password: encryption_password.map(str::to_string),
        ..VfsConfig::default()
    }
}

/// Real temp environment: SQLite db + mirrored cache tree + the given
/// mock transport, pre-connected（`vfs_open_read.rs::test_env_with_mock`
/// 同款形态）。
async fn test_env_with_mock(
    mock: MockTransport,
) -> (
    tempfile::TempDir,
    Arc<MetaDatabase>,
    CacheManager,
    PathBuf,
    Arc<MockTransport>,
) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let cache_root = dir.path().join("cache");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let cache = CacheManager::new(cache_root.clone(), 1 << 20);
    let mock = Arc::new(mock);
    mock.connect().await.expect("pre-connect mock transport");
    (dir, db, cache, cache_root, mock)
}

/// `test_env_with_mock` with the default always-Ok mock.
async fn test_env() -> (
    tempfile::TempDir,
    Arc<MetaDatabase>,
    CacheManager,
    PathBuf,
    Arc<MockTransport>,
) {
    test_env_with_mock(MockTransport::new()).await
}

/// Pushes `bytes` to the mock remote as `rel`（`vfs_open_read.rs`
/// 同款：经 `CloudTransport::upload` 直推）。
async fn seed_remote(
    mock: &Arc<MockTransport>,
    rel: &str,
    bytes: &[u8],
    chunk_count: u32,
    chunk_size: u64,
) -> UploadReceipt {
    let dir = tempfile::tempdir().expect("seed scratch dir");
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let local_path = dir.path().join(rel_path.name());
    fs::write(&local_path, bytes).expect("write seed scratch file");
    mock.upload(&UploadJob {
        rel_path,
        local_path,
        size: bytes.len() as u64,
        chunk_count,
        chunk_size,
    })
    .await
    .expect("seed upload to the mock remote")
}

/// 插入一条**加密** files 行（显式 scheme + size + sha256 哨兵）并返回
/// 行 id。
fn seed_encrypted_row(db: &MetaDatabase, rel: &str, size: i64, msg_id: i64, scheme: &str) -> i64 {
    let rel_path = RelPath::new(rel).expect("valid rel path");
    db.upsert_file_scheme(
        &FileUpsert {
            rel_path: rel_path.as_str().to_string(),
            name: rel_path.name().to_string(),
            parent_dir: rel_path
                .parent()
                .expect("non-root path")
                .as_str()
                .to_string(),
            size,
            mtime: 1_700_000_000.0,
            sha256: Some(SHA_SENTINEL.to_string()),
            is_dir: false,
            telegram_msg_id: Some(msg_id),
            is_uploaded: true,
            is_cached: false,
            is_encrypted: true,
            chunk_count: 1,
            mime_type: None,
        },
        scheme,
    )
    .expect("seed encrypted files row")
}

/// 单容器 chunk 行（K11 形态；size = 密文容器长——upload/materialize/
/// sync 三方写入均按密文边界记录，首读校验的 ct 来源）。
fn seed_container_chunk(db: &MetaDatabase, file_id: i64, msg_id: i64, container_len: i64) {
    db.upsert_chunk(file_id, 0, msg_id, container_len, None)
        .expect("seed container chunk row");
}

/// Concatenates every chunk of a byte stream; propagates the first error.
async fn drain(stream: ByteStream) -> Result<Vec<u8>, cloudkit_core::transport::StorageError> {
    let mut stream = stream;
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}

/// Builds the Vfs over the environment's pieces.
fn build_vfs(
    db: &Arc<MetaDatabase>,
    cache: CacheManager,
    mock: &Arc<MockTransport>,
    cfg: VfsConfig,
) -> Vfs {
    let transport: Arc<dyn CloudTransport> = mock.clone();
    Vfs::new(db.clone(), cache, transport, cfg)
}

/// Deterministic test pattern.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

// ------------------------------------------------------------ EB2 红→绿 ---

/// 1.（B2 stream 臂）错 scheme/错 size 行 + 远端真 v2 容器 →
///    `open_read` **先于任何字节**完成首读校验（行账目矛盾 → 内容权威
///    分流进校验）：admission 恰一次 34B 头读（B4 零额外往返——就是
///    DecryptingTransport 本来要在首窗做的那一读）、流式窗口逐字节 =
///    明文、行回写 `aead_v2`/真值 plain_len、coalesce 列
///    （sha256/telegram_msg_id）原值保留（定向 UPDATE）。
#[tokio::test]
async fn stream_arm_repairs_a_wrong_scheme_row_before_serving() {
    let (_dir, db, cache, _cache_root, mock) = test_env().await;

    // 远端真 v2 容器（AeadV2::new() 默认 1MiB 分块，与生产同源）。
    let plain = pattern(3_000);
    let ct = cloudkit_crypto::AeadV2::new().encrypt("pw", &plain);
    let receipt = seed_remote(&mock, "/fixme.bin", &ct, 1, ct.len() as u64).await;

    // 播种错行：scheme=gcm（标签错——内容实为 v2）+ size=错账
    //（真值的一半：与 gcm 闭式反推自相矛盾 → 首读校验的分流判据成立，
    // 该行的三方账目【标签/尺寸/容器长】对不上）。
    let wrong_size = (plain.len() / 2) as i64;
    assert_ne!(
        wrong_size,
        plain.len() as i64,
        "seed sanity: 错 size 确实错"
    );
    assert_ne!(
        cloudkit_core::materialize::plaintext_len_from_container(ct.len() as i64, SCHEME_GCM),
        wrong_size,
        "seed sanity: 行账目与 gcm 闭式自相矛盾（否则分流不进校验）"
    );
    let file_id = seed_encrypted_row(
        &db,
        "/fixme.bin",
        wrong_size,
        receipt.first_msg_id,
        SCHEME_GCM,
    );
    seed_container_chunk(&db, file_id, receipt.first_msg_id, ct.len() as i64);

    let vfs = build_vfs(&db, cache, &mock, test_cfg(Some("pw")));
    let rel = RelPath::new("/fixme.bin").expect("valid rel path");

    let StreamSource::Stream {
        handle,
        total_size,
        transport,
    } = vfs
        .open_read(&rel)
        .await
        .expect("首读校验把 gcm 标签修正为 aead_v2 后必须进流式臂（内容是唯一权威）")
    else {
        panic!(
            "wrong-scheme row over a range-capable transport must be repaired into the stream arm"
        );
    };

    // 校验先于任何字节出门：admission 恰一次 34B 头读（零额外往返）。
    assert_eq!(
        mock.open_range_calls(),
        vec![(0, 34)],
        "admission = 恰一次容器头读（B4：DecryptingTransport 首窗本就要做的那一读）"
    );
    // 真值 plain_len 作为 Content-Length/窗口总数的权威（K35 承重面）。
    assert_eq!(
        total_size,
        plain.len() as u64,
        "回写后的真值 plain_len 是流式总数权威"
    );

    // 流式窗口字节逐字节 = 明文。
    let window = drain(
        transport
            .open_range(&handle, 0, total_size)
            .await
            .expect("open encrypted window"),
    )
    .await
    .expect("window bytes");
    assert_eq!(window, plain, "流式窗口逐字节 = 明文");

    // 行已回写（scheme/size 对）+ coalesce 列未动。
    let row = db
        .get_file("/fixme.bin")
        .expect("db read")
        .expect("row exists");
    assert_eq!(
        row.encryption_scheme, SCHEME_AEAD_V2,
        "回写 scheme = 容器真值 aead_v2"
    );
    assert_eq!(
        row.size,
        plain.len() as i64,
        "回写 size = 容器头/闭式真值 plain_len"
    );
    assert_eq!(
        row.sha256.as_deref(),
        Some(SHA_SENTINEL),
        "定向 UPDATE 不碰 sha256（coalesce 列原值保留）"
    );
    assert_eq!(
        row.telegram_msg_id,
        Some(receipt.first_msg_id),
        "定向 UPDATE 不碰 telegram_msg_id（coalesce 列原值保留）"
    );
    assert!(row.is_encrypted, "cipher 标志不在回写列集，保持 1");
}

/// 2.（B2 hydrate 臂·尺寸回写）错 size 行 → Hydrate 全量读通 +
///    解密**完成后**以本地明文文件长度为真值回写行 size
///    （`write_atomic`/rename 落盘后、`set_cached_flag` 前）。
#[tokio::test]
async fn hydrate_repairs_size_from_local_plaintext_length() {
    let (_dir, db, cache, _cache_root, mock) = test_env().await;

    let plain = pattern(5_000);
    let ct = cloudkit_crypto::AeadV2::new().encrypt("pw", &plain);
    let receipt = seed_remote(&mock, "/hydr.bin", &ct, 1, ct.len() as u64).await;

    // 错 size（真值的一半）；scheme 标签正确，只 size 错。
    let wrong_size = (plain.len() / 2) as i64;
    let file_id = seed_encrypted_row(
        &db,
        "/hydr.bin",
        wrong_size,
        receipt.first_msg_id,
        SCHEME_AEAD_V2,
    );
    seed_container_chunk(&db, file_id, receipt.first_msg_id, ct.len() as i64);

    let vfs = build_vfs(&db, cache, &mock, test_cfg(Some("pw")));
    let rel = RelPath::new("/hydr.bin").expect("valid rel path");

    let path = vfs.hydrate(&rel).await.expect("hydrate 读通");
    assert_eq!(
        fs::read(&path).expect("read hydrated"),
        plain,
        "Hydrate 全量读通（逐字节明文）"
    );

    let row = db
        .get_file("/hydr.bin")
        .expect("db read")
        .expect("row exists");
    assert_eq!(
        row.size,
        plain.len() as i64,
        "行 size 已按本地明文长度回写（解密后真值）"
    );
    assert_eq!(
        row.encryption_scheme, SCHEME_AEAD_V2,
        "scheme 标签正确时不动 scheme（只修 size）"
    );
    assert_eq!(
        row.sha256.as_deref(),
        Some(SHA_SENTINEL),
        "定向 UPDATE 不碰 coalesce 列"
    );
}

/// 3.（B2 hydrate 臂·gcm 标签遇 `CKCRYPT2` 改判）行标 gcm、内容真
///    v2 容器 → hydrate 分发处**先看暂存文件头 8B、再 v1 decrypt**：
///    有 magic → 改走 v2 流式解密成功（而非 v1 解密失败）+ 回写
///    `aead_v2` 与真值 size。
#[tokio::test]
async fn gcm_labelled_v2_content_redispatches() {
    let (_dir, db, cache, _cache_root, mock) = test_env().await;

    let plain = pattern(4_000);
    let ct = cloudkit_crypto::AeadV2::new().encrypt("pw", &plain);
    let receipt = seed_remote(&mock, "/mixed.bin", &ct, 1, ct.len() as u64).await;

    // 行标 gcm + size 按 gcm 闭式猜（ct−44）——混合方案行的典型错形状。
    let wrong_size = ct.len() as i64 - 44;
    let file_id = seed_encrypted_row(
        &db,
        "/mixed.bin",
        wrong_size,
        receipt.first_msg_id,
        SCHEME_GCM,
    );
    seed_container_chunk(&db, file_id, receipt.first_msg_id, ct.len() as i64);

    let vfs = build_vfs(&db, cache, &mock, test_cfg(Some("pw")));
    let rel = RelPath::new("/mixed.bin").expect("valid rel path");

    let path = vfs
        .hydrate(&rel)
        .await
        .expect("gcm 标签行遇 CKCRYPT2 内容必须改判 v2 成功，而非 v1 解密失败");
    assert_eq!(
        fs::read(&path).expect("read hydrated"),
        plain,
        "改判 v2 后逐字节明文"
    );

    let row = db
        .get_file("/mixed.bin")
        .expect("db read")
        .expect("row exists");
    assert_eq!(
        row.encryption_scheme, SCHEME_AEAD_V2,
        "回写 scheme = aead_v2（改判成功）"
    );
    assert_eq!(row.size, plain.len() as i64, "回写 size = 本地明文真值");
    assert_eq!(
        row.sha256.as_deref(),
        Some(SHA_SENTINEL),
        "定向 UPDATE 不碰 coalesce 列"
    );
}

/// 4.（B6 文案钉，**通路级**——审查回派改义）行标 aead_v2、内容无
///    magic **且 v1 形失败**（K84.2 同份密文双试的 v2→v1 方向：34B 头
///    试不出 v1——v1 tag 在文件尾，admission 先转 hydrate 拿全量）→
///    `open_read` 回 `Hydrate` 零字节（admission 只付一次 34B 头读、
///    尚无全量 open、无 Stream）→ hydrate 双试失败 → **最终 Err 含
///    `cydrive sync`**（双假说：密钥/方案配置错，或实为明文/异期方案）
///    + 缓存树零残留——**绝不返回原文密文字节**。
#[tokio::test]
async fn v2_labelled_non_container_fails_actionably() {
    let (_dir, db, cache, cache_root, mock) = test_env().await;

    // 内容非容器且 v1 解不开：一段明文样字节（≥44B 使其过 v1 的
    // 长度闸、撞 GCM AuthFailed——「v1 形失败」的前提）。
    let not_a_container = pattern(96);
    assert_ne!(
        &not_a_container[..8],
        cloudkit_crypto::v2::MAGIC.as_slice(),
        "seed sanity: 内容无 CKCRYPT2 magic"
    );
    let receipt = seed_remote(
        &mock,
        "/liar.bin",
        &not_a_container,
        1,
        not_a_container.len() as u64,
    )
    .await;

    let file_id = seed_encrypted_row(
        &db,
        "/liar.bin",
        not_a_container.len() as i64,
        receipt.first_msg_id,
        SCHEME_AEAD_V2,
    );
    seed_container_chunk(
        &db,
        file_id,
        receipt.first_msg_id,
        not_a_container.len() as i64,
    );

    let vfs = build_vfs(&db, cache, &mock, test_cfg(Some("pw")));
    let rel = RelPath::new("/liar.bin").expect("valid rel path");

    // admission：34B 头试不出 v1 → 零字节的 Hydrate 信号（K84.2），
    // 而非直出 Err 卡死（winfsp 直通面由此进 hydrate 双试）。
    let StreamSource::Hydrate = vfs
        .open_read(&rel)
        .await
        .expect("非容器内容的 admission 不做内容级判决")
    else {
        panic!("aead_v2 标签 + 无 magic 必须转 Hydrate（K84.2 双试方向），绝不直接流式/卡 Err");
    };
    // admission 面：恰一次 34B 头读、零全量 open、零 Stream 字节。
    assert_eq!(
        mock.open_range_calls(),
        vec![(0, 34)],
        "admission 只付一次有界 34B 头读"
    );
    assert!(
        mock.open_calls().is_empty(),
        "admission 零全量 open（v1 判决留给 hydrate 的同一份内容）"
    );

    // 通路级 B6：hydrate 双试失败 → 最终 Err 含 cydrive sync。
    let err = match vfs.hydrate(&rel).await {
        Err(error) => error,
        Ok(_) => panic!("无 magic 且 v1 形失败的内容必须响亮失败，绝不吐字节"),
    };
    let message = err.to_string();
    assert!(
        message.contains("cydrive sync"),
        "B6 文案必须指路 cydrive sync，got: {message}"
    );
    assert!(
        message.contains("plaintext") && message.contains("scheme configuration"),
        "B6 双假说（实为明文/异期方案 vs 密钥/方案配置错）必须在文案里，got: {message}"
    );
    // 绝无字节返回：hydrate 失败不落盘，远端只付 admission 头读 +
    // hydrate 本职的全量 open（内部解密试验，字节从未出门）。
    let mut all_files = Vec::new();
    collect_files(&cache_root, &mut all_files);
    assert!(
        all_files.is_empty(),
        "失败的双试不留任何缓存残留（密文/明文都不落盘）: {all_files:?}"
    );
    // 行不被回写（真值不可知——B6：不猜）。
    let row = db
        .get_file("/liar.bin")
        .expect("db read")
        .expect("row exists");
    assert_eq!(
        row.encryption_scheme, SCHEME_AEAD_V2,
        "非容器内容真值不可知——scheme 不回写"
    );
    assert_eq!(
        row.size,
        not_a_container.len() as i64,
        "非容器内容真值不可知——size 不回写"
    );
    assert!(
        !row.is_cached,
        "失败的 hydrate 绝不置位 is_cached（否则下次命中脏路）"
    );
}

/// 6.（K84.2 同份密文双试——v2→v1 方向钉）行标 aead_v2、远端**真 v1
///    容器**（混合期典型：配置已切 aead_v2、老文件是 v1、索引已丢按
///    配置猜错标签）→ `open_read` 回 `Ok(Hydrate)`（钉 winfsp 直通面
///    不再卡 Err）→ hydrate 同份密文双试 v1 **成功自愈**：读通逐字节
///    + 行回写 scheme=`gcm`、size=`ct−44` 真值（零额外下载——同一份
///    内容 hydrate 一次拿全）。
#[tokio::test]
async fn v2_labelled_v1_container_dual_trial_self_heals() {
    let (_dir, db, cache, _cache_root, mock) = test_env().await;

    // 远端真 v1 容器（冻结格式：16B 盐 + 12B nonce + 密文 + 16B tag，
    // 无 magic）。
    let plain = pattern(700);
    let ct = cloudkit_crypto::v1::encrypt("pw", &plain);
    assert_eq!(ct.len(), plain.len() + 44, "seed sanity: v1 容器开销 44B");
    let receipt = seed_remote(&mock, "/old-v1.bin", &ct, 1, ct.len() as u64).await;

    // 行按配置猜成 aead_v2、size 也猜错（索引已丢的混合期形状）。
    let guessed_size = (plain.len() / 2) as i64;
    let file_id = seed_encrypted_row(
        &db,
        "/old-v1.bin",
        guessed_size,
        receipt.first_msg_id,
        SCHEME_AEAD_V2,
    );
    seed_container_chunk(&db, file_id, receipt.first_msg_id, ct.len() as i64);

    let vfs = build_vfs(&db, cache, &mock, test_cfg(Some("pw")));
    let rel = RelPath::new("/old-v1.bin").expect("valid rel path");

    // open_read：aead_v2 标签 + 无 magic → Hydrate（K84.2——34B 头试
    // 不出 v1，先拿全量再判；绝不 Err 卡死、绝不直接流式）。
    let StreamSource::Hydrate = vfs
        .open_read(&rel)
        .await
        .expect("v1 内容在 aead_v2 标签下必须拿到 Hydrate 信号")
    else {
        panic!("aead_v2 + 无 magic 必须转 Hydrate（K84.2 双试），不得流式");
    };

    // hydrate 双试 v1 成功 → 读通 + 行回写 gcm/真值。
    let path = vfs
        .hydrate(&rel)
        .await
        .expect("同份密文双试 v1 成功自愈（K84.2：方案猜错由读时回退兜底）");
    assert_eq!(
        fs::read(&path).expect("read hydrated"),
        plain,
        "双试 v1 读出的明文逐字节"
    );
    let row = db
        .get_file("/old-v1.bin")
        .expect("db read")
        .expect("row exists");
    assert_eq!(
        row.encryption_scheme, SCHEME_GCM,
        "回写 scheme = v1 真值 gcm（K84.2 双试自愈）"
    );
    assert_eq!(
        row.size,
        ct.len() as i64 - 44,
        "回写 size = v1 闭式真值（ct − 44 = 明文长）"
    );
    assert_eq!(
        row.sha256.as_deref(),
        Some(SHA_SENTINEL),
        "定向 UPDATE 不碰 coalesce 列"
    );
}

/// Recursively collects files under `root`（缓存残留断言用）。
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}
