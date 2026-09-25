//! WD3 写路径行为测试（手搓桩消费——`stub/mod.rs`「消费手册」）。
//!
//! 覆盖对照（计划 §4.4 映射表 / §4.6 stager / 矩阵写侧行）：
//!
//! | 用例 | 依据 |
//! |---|---|
//! | mkdir stat 预检（rclone201 幂等陷阱上 Exists） | 矩阵⑥ / §4.4 |
//! | mkdir 409 → 隐式建父重试恰一次 | §4.4 |
//! | delete 文件/目录递归/幂等形态（NotFound 恒定） | §4.4 / sftp 先例 |
//! | delete 集合腿尾斜杠 | 矩阵⑦ |
//! | rename 文件/目录（目录腿双侧尾斜杠） | 矩阵⑤ |
//! | rename 恒显式 Overwrite + 绝对 Destination | 矩阵⑤/⑩ |
//! | rename 412 → 重 stat 复核后 Exists（源原位） | §4.4 / K75-1 |
//! | rename 5xx 且父在 → 不映射 Exists（源原位） | K75-1 |
//! | rename 缺父 → 隐式建父重试 | 矩阵⑤ |
//! | stager 提交链（PUT .part → MOVE T → size 复核） | §4.6 |
//! | stager 0 字节上传全链（空体 PUT + MOVE 固化 + 回读空） | §4.6 / K85.6 |
//! | 覆盖写 stash（staging 窗口旧对象不可见/abort 恢复） | §4.6 / 断言① |
//! | 超承诺 write → Invalid | sftp hint 契约 |
//! | size 复核不符 → Unavailable 不静默 | §4.6 / stat_size_delta |
//! | lost-ACK（PUT/MOVE 半边）→ 对账后按已提交继续 | §4.6 / K67 H2 |
//! | X-OC-Mtime 搭车仅 nextcloud / generic 零 PROPPATCH | D2/D4 |
//! | `.ckwd-` 暂存件 list 不可见 | §4.6 / 断言③ |

mod stub;

use std::collections::HashMap;

use cloudkit_storage::{
    ByteStream, EntryKind, Page, PageCursor, RelPath, StorageDriver, StorageError, WriteHint,
};
use futures_util::StreamExt;
use stub::{spawn_stub, Knobs, RecordedRequest, StubHandle, StubStyle, Vfs, MTIME_SEED};

use ck_webdav::{parse_from_map, WebdavDriver, WebdavParams};

// ------------------------------------------------------------ 测试工具 ---

fn params(handle: &StubHandle, pairs: &[(&str, &str)]) -> WebdavParams {
    let mut map = HashMap::new();
    map.insert("webdav_url".to_string(), handle.url.clone());
    for (key, value) in pairs {
        map.insert(key.to_string(), value.to_string());
    }
    parse_from_map(&map).expect("test params parse")
}

fn driver(handle: &StubHandle) -> WebdavDriver {
    WebdavDriver::new(params(handle, &[])).expect("driver constructs")
}

fn rel(path: &str) -> RelPath {
    RelPath::new(path).expect("rel path")
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

async fn plain(vfs: Vfs) -> StubHandle {
    spawn_stub(
        vfs,
        stub::AuthMode::None,
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await
}

fn requests_of(handle: &StubHandle, method: &str) -> Vec<RecordedRequest> {
    handle
        .requests()
        .into_iter()
        .filter(|request| request.method == method)
        .collect()
}

/// stager 快捷：writer + write + close。
async fn upload_bytes(
    driver: &WebdavDriver,
    path: &RelPath,
    data: &[u8],
) -> Result<cloudkit_storage::Entry, StorageError> {
    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..Default::default()
    };
    let mut stager = driver.writer(path, &hint).await?;
    stager.write(data).await?;
    stager.close().await
}

/// 逐字节收集一条 ByteStream（回读面断言用——read_path 同款消费面）。
async fn collect(stream: ByteStream) -> Result<Vec<u8>, StorageError> {
    let mut stream = stream;
    let mut out = Vec::new();
    while let Some(frame) = stream.next().await {
        out.extend_from_slice(&frame?);
    }
    Ok(out)
}

// ------------------------------------------------------------- mkdir ---

/// 矩阵⑥：rclone 的 MKCOL-201 幂等陷阱由 stat 预检吸收——已存在（无论
/// 目录还是文件占位）恒 `Exists`，不赌服务器的「已存在」回应形态。
#[tokio::test]
async fn mkdir_existing_yields_exists_on_both_server_forms() {
    for style in [StubStyle::rclone(), StubStyle::apache()] {
        let mut vfs = Vfs::new();
        vfs.seed_dir("/d");
        vfs.seed_file("/f.bin", b"x");
        let handle = spawn_stub(vfs, stub::AuthMode::None, Knobs::default(), style).await;
        let driver = driver(&handle);

        let error = driver.mkdir(&rel("d")).await.expect_err("existing dir");
        assert!(matches!(error, StorageError::Exists), "{error:?}");
        let error = driver
            .mkdir(&rel("f.bin"))
            .await
            .expect_err("file in the way");
        assert!(matches!(error, StorageError::Exists), "{error:?}");
        handle.shutdown().await;
    }
}

/// §4.4：MKCOL 409（父缺失）→ 隐式建父后重试恰一次；MKCOL 恒带尾斜杠
///（记录器：apache SlashStrict 下无斜杠不执行）。
#[tokio::test]
async fn mkdir_missing_parent_is_created_implicitly() {
    let handle = plain(Vfs::new()).await;
    let driver = driver(&handle);

    driver
        .mkdir(&rel("a/b/c"))
        .await
        .expect("implicit parents then MKCOL succeeds");
    let entry = driver.stat(&rel("a/b/c")).await.expect("created");
    assert_eq!(entry.kind, EntryKind::Dir);
    let mkcols = requests_of(&handle, "MKCOL");
    assert!(!mkcols.is_empty(), "MKCOL must have been issued");
    assert!(
        mkcols.iter().all(|request| request.path.ends_with('/')),
        "MKCOL must always carry the collection trailing slash: {:?}",
        mkcols.iter().map(|r| r.path.clone()).collect::<Vec<_>>()
    );
}

/// §4.4/矩阵⑥：405 形态的服务器上「已存在目录 MKCOL」也归 Exists（预检
/// 之后的竞态带同样映射）——rclone201 形态见上一用例，这里钉 RFC405。
#[tokio::test]
async fn mkdir_on_file_blocked_parent_fails_cleanly() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/blocker", b"x");
    let handle = plain(vfs).await;
    let driver = driver(&handle);

    // 隐式建父腿撞上「文件占住父路径」：ensure_parents 的 stat 见到文件
    // → Exists（不是把文件路径当目录继续建）。
    let error = driver
        .mkdir(&rel("blocker/child"))
        .await
        .expect_err("file occupies the parent path");
    assert!(matches!(error, StorageError::Exists), "{error:?}");
}

// ------------------------------------------------------------ delete ---

/// §4.4/矩阵⑦：文件删除 + 幂等形态声明（不存在恒 NotFound——sftp 先例，
/// driver 文档恒定）+ 目录递归 + 集合腿尾斜杠。
#[tokio::test]
async fn delete_file_dir_recursive_and_missing_idempotency() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", b"gone");
    vfs.seed_dir("/tree/inner");
    vfs.seed_file("/tree/inner/leaf", b"leaf");
    let handle = plain(vfs).await;
    let driver = driver(&handle);

    // 文件
    let entry = driver.stat(&rel("f.bin")).await.expect("stat");
    driver.delete(&entry.id).await.expect("file delete");
    assert!(matches!(
        driver.stat(&rel("f.bin")).await,
        Err(StorageError::NotFound)
    ));

    // 不存在：声明形态恒定（两次都 NotFound）
    let missing = cloudkit_storage::EntryId::new(
        driver.volume().clone(),
        cloudkit_storage::BackendHandle::new("no-such-handle"),
    );
    for _ in 0..2 {
        assert!(matches!(
            driver.delete(&missing).await,
            Err(StorageError::NotFound)
        ));
    }

    // 目录递归
    let dir_entry = driver.stat(&rel("tree")).await.expect("dir stat");
    assert_eq!(dir_entry.kind, EntryKind::Dir);
    driver.delete(&dir_entry.id).await.expect("dir delete");
    assert!(matches!(
        driver.stat(&rel("tree/inner/leaf")).await,
        Err(StorageError::NotFound)
    ));

    // 集合腿恒带尾斜杠（矩阵⑦：no-slash 301 不执行）
    let deletes = requests_of(&handle, "DELETE");
    assert!(
        deletes.iter().any(|request| request.path == "/tree/"),
        "collection DELETE must be slashed: {:?}",
        deletes.iter().map(|r| r.path.clone()).collect::<Vec<_>>()
    );

    // 卷根删除拒绝
    let root_id = driver.stat(&RelPath::root()).await.expect("root").id;
    assert!(matches!(
        driver.delete(&root_id).await,
        Err(StorageError::Invalid)
    ));
}

// ------------------------------------------------------------ rename ---

/// 矩阵⑤/⑩：文件 rename 落位；MOVE 恒显式 Overwrite=F（trait 契约：目标
/// 存在 → Exists，不覆盖——sftp 先例）+ Destination 恒绝对 URI。
#[tokio::test]
async fn rename_file_moves_content_with_explicit_headers() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/src.bin", b"payload");
    let handle = plain(vfs).await;
    let driver = driver(&handle);

    driver
        .rename(&rel("src.bin"), &rel("dst.bin"))
        .await
        .expect("rename");
    assert!(matches!(
        driver.stat(&rel("src.bin")).await,
        Err(StorageError::NotFound)
    ));
    let entry = driver.stat(&rel("dst.bin")).await.expect("moved");
    assert_eq!(entry.size, 7);
    assert_eq!(
        handle.take("/dst.bin").as_deref(),
        Some(b"payload".as_slice())
    );

    let moves = requests_of(&handle, "MOVE");
    assert_eq!(moves.len(), 1, "exactly one MOVE");
    assert_eq!(
        moves[0].overwrite.as_deref(),
        Some("F"),
        "rename must send an explicit Overwrite: F"
    );
    let destination = moves[0]
        .destination
        .as_deref()
        .expect("Destination header present");
    assert!(
        destination.starts_with("http://"),
        "Destination must be an absolute URI: {destination}"
    );
    assert!(destination.contains("/dst.bin"), "points at the target");
}

/// 矩阵⑤：目录 rename 的源与 Destination 双侧尾斜杠（apache no-slash
/// 301 不执行）+ 递归搬移内容不变。
#[tokio::test]
async fn rename_directory_uses_slashed_legs_and_moves_subtree() {
    let handle = spawn_stub(
        Vfs::new(),
        stub::AuthMode::None,
        Knobs::default(),
        StubStyle::apache(),
    )
    .await;
    let driver = driver(&handle);
    upload_bytes(&driver, &rel("tree/deep/leaf"), b"leaf-bytes")
        .await
        .expect("seed via writer");

    driver
        .rename(&rel("tree"), &rel("tree2"))
        .await
        .expect("dir rename");
    assert!(matches!(
        driver.stat(&rel("tree")).await,
        Err(StorageError::NotFound)
    ));
    let entry = driver
        .stat(&rel("tree2/deep/leaf"))
        .await
        .expect("moved leaf");
    assert_eq!(entry.size, 10);

    let moves = requests_of(&handle, "MOVE");
    let dir_move = moves
        .iter()
        .find(|request| request.path == "/tree/")
        .expect("directory MOVE slashed on the source side");
    let destination = dir_move.destination.as_deref().expect("destination");
    assert!(
        destination.contains("/tree2/"),
        "directory Destination slashed: {destination}"
    );
}

/// §4.4：MOVE 412（Overwrite:F 撞既有目标）→ 重 stat 复核后 Exists；
/// 源原位（K75-1 纪律的行为面）。
#[tokio::test]
async fn rename_onto_existing_yields_exists_with_source_intact() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/src.bin", b"source");
    vfs.seed_file("/dst.bin", b"destination");
    let handle = plain(vfs).await;
    let driver = driver(&handle);

    let error = driver
        .rename(&rel("src.bin"), &rel("dst.bin"))
        .await
        .expect_err("target exists");
    assert!(matches!(error, StorageError::Exists), "{error:?}");
    // 源与目标都原位原内容（未发生任何移动效果）
    assert_eq!(
        handle.take("/src.bin").as_deref(),
        Some(b"source".as_slice()),
        "take() removes; assert then re-seed"
    );
    handle.seed_file("/src.bin", b"source");
    assert_eq!(
        handle.snapshot().get("/dst.bin"),
        Some(&stub::VfsEntry::File {
            bytes: b"destination".to_vec(),
            mtime: MTIME_SEED,
        })
    );
}

/// K75-1：服务端瞬态 503 落在 MOVE 上（目标父目录真实存在——缺父三态
/// 复核不成立）绝不映射 Exists——按 §4.4 通用表归一 Unavailable，源
/// 原位。注入：`transient_5xx_move`（与读侧 transient_5xx 独立计数——
/// stat 预检的 PROPFIND 重试链不会先吃掉）。
#[tokio::test]
async fn rename_transient_503_is_not_exists_with_source_intact() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/src.bin", b"source");
    vfs.seed_dir("/dest-dir");
    let handle = spawn_stub(
        vfs,
        stub::AuthMode::None,
        Knobs {
            transient_5xx_move: 1,
            ..Knobs::default()
        },
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle);

    let error = driver
        .rename(&rel("src.bin"), &rel("dest-dir/new.bin"))
        .await
        .expect_err("MOVE hits the injected 503");
    assert!(
        matches!(error, StorageError::Unavailable(_)),
        "server 5xx with the parent present must stay Unavailable, got {error:?}"
    );
    assert!(
        !matches!(error, StorageError::Exists),
        "transport/server errors must never fold into Exists (K75-1)"
    );
    // 源原位未被移动（503 在路由前注入——效果未落）
    assert_eq!(
        handle.take("/src.bin").as_deref(),
        Some(b"source".as_slice())
    );
    handle.seed_file("/src.bin", b"source");
    assert!(!handle.exists("/dest-dir/new.bin"));
}

/// 缺父三态复核的「父被文件占住」臂：祖先路径被文件占位 → `Exists`
///（占位冲突——与 mkdir 的 ensure_parents 同型判定）。
#[tokio::test]
async fn rename_destination_blocked_by_a_file_ancestor_yields_exists() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/src.bin", b"source");
    vfs.seed_file("/blocker", b"x");
    let handle = spawn_stub(
        vfs,
        stub::AuthMode::None,
        Knobs::default(),
        StubStyle {
            move_missing_parent: stub::MoveMissingParent::Apache500,
            ..StubStyle::rclone()
        },
    )
    .await;
    let driver = driver(&handle);

    let error = driver
        .rename(&rel("src.bin"), &rel("blocker/new.bin"))
        .await
        .expect_err("a file occupies the destination parent path");
    assert!(matches!(error, StorageError::Exists), "{error:?}");
    assert!(handle.exists("/src.bin"), "source untouched");
}

/// 矩阵⑤：403（rclone 缺父真形）→ 隐式建父后重试恰一次 → 成功。
#[tokio::test]
async fn rename_missing_parent_is_created_then_retried() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/src.bin", b"moved");
    let handle = plain(vfs).await;
    let driver = driver(&handle);

    driver
        .rename(&rel("src.bin"), &rel("newdir/sub/g.bin"))
        .await
        .expect("implicit parents then MOVE retry");
    let entry = driver.stat(&rel("newdir/sub/g.bin")).await.expect("moved");
    assert_eq!(entry.size, 5);
    assert_eq!(
        handle.take("/newdir/sub/g.bin").as_deref(),
        Some(b"moved".as_slice())
    );
    // 重试恰一次：MOVE 到达两次（首次 403 + 建父后的重试）
    assert_eq!(requests_of(&handle, "MOVE").len(), 2);
}

/// M13③：MOVE 缺父 409（RFC 4918 §9.9.4 形态——缺父三态的第三态，
/// rclone403/apache500 之外的补腿）→ 隐式建父后重试恰一次 → 成功。
#[tokio::test]
async fn rename_missing_parent_409_is_created_then_retried() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/src.bin", b"moved");
    let handle = spawn_stub(
        vfs,
        stub::AuthMode::None,
        Knobs::default(),
        StubStyle {
            move_missing_parent: stub::MoveMissingParent::Apache409,
            ..StubStyle::rclone()
        },
    )
    .await;
    let driver = driver(&handle);

    driver
        .rename(&rel("src.bin"), &rel("newdir/sub/g.bin"))
        .await
        .expect("409 → implicit parents → MOVE retry");
    assert_eq!(
        handle.take("/newdir/sub/g.bin").as_deref(),
        Some(b"moved".as_slice())
    );
    // 重试恰一次：MOVE 到达两次（首次 409 + 建父后的重试）。
    assert_eq!(requests_of(&handle, "MOVE").len(), 2);
}

/// 复审 M1（2026-09-25）：缺父 → 建父 → 重试撞**并发占位**（412）——
/// 重试臂必须与首发同判（stat 复核 → `Exists`），绝不压成 `NotFound`
/// （父）。构造面：`concurrent_target_on_move = 2`——第 2 个 MOVE（建父
/// 后的重试）处理前并发写手落地目标。
#[tokio::test]
async fn rename_retry_412_classifies_exists_not_missing_parent() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/src.bin", b"source");
    // 目标 newdir/sub/g.bin 的父链不存在：首发 MOVE 缺父 409 →
    // ParentSuspect → stat 父 NotFound → MKCOL 建父 → 重试。
    let knobs = Knobs {
        concurrent_target_on_move: 2,
        ..Knobs::default()
    };
    let handle = spawn_stub(
        vfs,
        stub::AuthMode::None,
        knobs,
        StubStyle {
            move_missing_parent: stub::MoveMissingParent::Apache409,
            ..StubStyle::rclone()
        },
    )
    .await;
    let driver = driver(&handle);

    let err = driver
        .rename(&rel("src.bin"), &rel("newdir/sub/g.bin"))
        .await
        .expect_err("retry-412 rename must fail");
    assert!(
        matches!(err, StorageError::Exists),
        "重试撞 412 = 目标被并发占位，必须 Exists（不压成 NotFound/父）: {err}"
    );
    assert!(handle.exists("/src.bin"), "412 先于移动——源原位");
    assert!(
        handle.exists("/newdir/sub/g.bin"),
        "并发占位的目标仍在（rename 未覆盖它）"
    );
    // MOVE 恰两次（首发 409 + 撞 412 的重试）。
    assert_eq!(requests_of(&handle, "MOVE").len(), 2);
    handle.shutdown().await;
}

/// M13③ 补臂：409 + 目标父路径被文件占住 → `Exists`（占位冲突——隐式
/// 建父不可行，与既有 Apache500 腿同型判定；传输/服务端类绝不映射
/// Exists 的 K75-1 纪律不适用于此：这是 stat 核实的真实占位）。
#[tokio::test]
async fn rename_409_with_a_file_occupied_parent_yields_exists() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/src.bin", b"source");
    vfs.seed_file("/blocker", b"x");
    let handle = spawn_stub(
        vfs,
        stub::AuthMode::None,
        Knobs::default(),
        StubStyle {
            move_missing_parent: stub::MoveMissingParent::Apache409,
            ..StubStyle::rclone()
        },
    )
    .await;
    let driver = driver(&handle);

    let error = driver
        .rename(&rel("src.bin"), &rel("blocker/new.bin"))
        .await
        .expect_err("a file occupies the destination parent path");
    assert!(matches!(error, StorageError::Exists), "{error:?}");
    assert!(handle.exists("/src.bin"), "source untouched");
}

/// M13①：PUT 507（配额满真形）→ `Io` 带码（map_status 尾行），零效果
/// 且不进重试白名单（PUT 非幂等）。注入：`storage_full_once` 旋钮打在
/// close 链的 `.part` PUT 上；lost-ACK 对账核实未落 → 如实上抛原错误。
#[tokio::test]
async fn put_insufficient_storage_maps_to_io_with_the_code() {
    let handle = spawn_stub(
        Vfs::new(),
        stub::AuthMode::None,
        Knobs {
            storage_full_once: true,
            ..Knobs::default()
        },
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle);

    let error = upload_bytes(&driver, &rel("f.bin"), b"payload")
        .await
        .expect_err("the .part PUT hits the injected 507");
    match error {
        StorageError::Io(message) => {
            assert!(message.contains("507"), "{message}");
            assert!(
                message.contains("insufficient storage"),
                "the code is spelled out: {message}"
            );
        }
        other => panic!("expected Io, got {other:?}"),
    }
    // 零效果：final 与 `.part` 都未落地（快照只剩根目录）。
    assert_eq!(
        handle.snapshot().len(),
        1,
        "a rejected PUT must land nothing: {:?}",
        handle.snapshot()
    );
    // 旋钮已耗：同一目标重传成功（507 不在重试白名单——重传是调用方
    // 的显式决定）。
    let entry = upload_bytes(&driver, &rel("f.bin"), b"payload")
        .await
        .expect("retry lands after the knob is spent");
    assert_eq!(entry.size, 7);
}

// ------------------------------------------------------------ stager ---

/// §4.6 提交链：PUT `<final>.ckwd-<pid>-<seq>.part`（Content-Length =
/// spool 长度）→ MOVE .part → final（Overwrite:T）→ stat 复核 → Entry。
/// generic：零 PROPPATCH（D2）+ 零 X-OC-Mtime。
#[tokio::test]
async fn stager_commits_via_part_then_move() {
    let handle = plain(Vfs::new()).await;
    let driver = driver(&handle);
    let data = pattern(4096);

    let entry = upload_bytes(&driver, &rel("docs/f.bin"), &data)
        .await
        .expect("commit chain");
    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.size, data.len() as u64);
    assert_eq!(entry.path, rel("docs/f.bin"));
    assert_eq!(handle.take("/docs/f.bin").as_deref(), Some(data.as_slice()));

    let puts = requests_of(&handle, "PUT");
    assert_eq!(puts.len(), 1, "exactly one PUT");
    let put = &puts[0];
    assert!(
        put.path.contains(".ckwd-") && put.path.ends_with(".part"),
        "PUT must target the staging part file: {}",
        put.path
    );
    assert_eq!(put.body_len, data.len(), "Content-Length = spool length");
    assert!(
        put.x_oc_mtime.is_none(),
        "generic vendor must not send X-OC-Mtime"
    );

    let moves = requests_of(&handle, "MOVE");
    assert_eq!(moves.len(), 1, "exactly one MOVE");
    assert_eq!(moves[0].overwrite.as_deref(), Some("T"));
    let destination = moves[0].destination.as_deref().expect("destination");
    assert!(
        destination.ends_with("/docs/f.bin"),
        "lands on final: {destination}"
    );
    assert!(
        destination.starts_with("http://"),
        "absolute Destination: {destination}"
    );

    assert!(
        requests_of(&handle, "PROPPATCH").is_empty(),
        "generic vendor never issues PROPPATCH (D2)"
    );
}

/// K85.6（Phase 8-B）characterization：0 字节上传走完整提交链——hint
/// size=0 → write 空 → close：PUT `.part` 携**空体**（body_len=0）→
/// MOVE 固化恰一次 → size 复核 0==0 吻合（`content_length.unwrap_or(0)`
/// 面）→ Entry size=0；回读空流；不留 staging 残留。sftp 的
/// `writer_empty_upload_roundtrip` 同构腿（各 crate 独立同名）。
#[tokio::test]
async fn writer_empty_upload_roundtrip() {
    let handle = plain(Vfs::new()).await;
    let driver = driver(&handle);

    let hint = WriteHint {
        size: Some(0),
        ..Default::default()
    };
    let mut stager = driver
        .writer(&rel("empty.bin"), &hint)
        .await
        .expect("writer");
    stager.write(b"").await.expect("empty write");
    let entry = stager.close().await.expect("close");
    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.size, 0);
    assert_eq!(entry.path, rel("empty.bin"));

    // 提交链形态与既有腿一致：恰一次空体 PUT 落 `.part` + 恰一次 MOVE
    // 固化（Overwrite:T → final）。
    let puts = requests_of(&handle, "PUT");
    assert_eq!(puts.len(), 1, "exactly one PUT");
    assert_eq!(puts[0].body_len, 0, "the 0-byte upload PUTs an empty body");
    assert!(
        puts[0].path.contains(".ckwd-") && puts[0].path.ends_with(".part"),
        "PUT must target the staging part file: {}",
        puts[0].path
    );
    let moves = requests_of(&handle, "MOVE");
    assert_eq!(moves.len(), 1, "exactly one MOVE");
    assert_eq!(moves[0].overwrite.as_deref(), Some("T"));
    assert!(
        moves[0]
            .destination
            .as_deref()
            .is_some_and(|destination| destination.ends_with("/empty.bin")),
        "lands on final: {:?}",
        moves[0].destination
    );

    // 回读面：reader 读回空（stat-first 见 0 长度 → 空流）。
    let bytes = collect(driver.reader(&entry.id, None).await.expect("reader"))
        .await
        .expect("empty read-back");
    assert!(bytes.is_empty());

    // VFS 终态：final 是真实 0 字节对象，`.ckwd-` 暂存件零残留。
    assert_eq!(handle.take("/empty.bin"), Some(Vec::new()));
    assert!(
        !handle.snapshot().keys().any(|path| path.contains(".ckwd-")),
        "no part/old residue after close"
    );
}

/// D4：nextcloud vendor 的 PUT 搭车 X-OC-Mtime（记录器头观测）。
#[tokio::test]
async fn stager_nextcloud_piggybacks_x_oc_mtime() {
    let handle = plain(Vfs::new()).await;
    let driver = WebdavDriver::new(params(&handle, &[("webdav_vendor", "nextcloud")]))
        .expect("driver constructs");

    upload_bytes(&driver, &rel("f.bin"), b"nc-payload")
        .await
        .expect("commit");

    let puts = requests_of(&handle, "PUT");
    assert_eq!(puts.len(), 1);
    let mtime = puts[0]
        .x_oc_mtime
        .as_deref()
        .expect("nextcloud PUT carries X-OC-Mtime");
    assert!(
        mtime.parse::<u64>().is_ok(),
        "X-OC-Mtime must be epoch seconds, got {mtime:?}"
    );
}

/// §4.6/sftp hint 契约：写入超承诺 → Invalid。
#[tokio::test]
async fn stager_rejects_writes_beyond_the_hinted_size() {
    let handle = plain(Vfs::new()).await;
    let driver = driver(&handle);

    let hint = WriteHint {
        size: Some(5),
        ..Default::default()
    };
    let mut stager = driver.writer(&rel("f.bin"), &hint).await.expect("writer");
    let error = stager.write(b"123456").await.expect_err("overrun");
    assert!(matches!(error, StorageError::Invalid), "{error:?}");
    // close 时承诺不符同样 Invalid（sftp 判例）
    let error = stager.close().await.expect_err("close underrun");
    assert!(matches!(error, StorageError::Invalid), "{error:?}");
    assert!(!handle.exists("/f.bin"), "nothing committed");
}

/// §4.6 断言①（覆盖写腿）：旧对象在 staging 窗口内不可见（stash 成
/// `.ckwd-*.old`）；close 后新内容落位、stash 退役。
#[tokio::test]
async fn stager_overwrite_hides_old_object_during_staging() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", b"old-content");
    let handle = plain(vfs).await;
    let driver = driver(&handle);

    let hint = WriteHint {
        size: Some(11),
        ..Default::default()
    };
    let mut stager = driver.writer(&rel("f.bin"), &hint).await.expect("writer");
    assert!(
        matches!(
            driver.stat(&rel("f.bin")).await,
            Err(StorageError::NotFound)
        ),
        "the old object must be invisible while staging (assertion 1, overwrite leg)"
    );
    // 旧对象躺在 stash（list 不可见，但快照可证未丢）
    assert!(
        handle
            .snapshot()
            .keys()
            .any(|path| path.contains(".ckwd-") && path.ends_with(".old")),
        "the old object is parked at the .old stash"
    );
    stager.write(b"new-content").await.expect("write");
    let entry = stager.close().await.expect("commit");
    assert_eq!(entry.size, 11);
    assert_eq!(
        handle.take("/f.bin").as_deref(),
        Some(b"new-content".as_slice())
    );
    assert!(
        !handle
            .snapshot()
            .keys()
            .any(|path| path.ends_with(".old") || path.ends_with(".part")),
        "stash retired and no part residue after close"
    );
}

/// abort 语义（§4.6）：新建腿零残留；覆盖腿旧对象回位。
#[tokio::test]
async fn stager_abort_restores_the_pre_writer_state() {
    let handle = plain(Vfs::new()).await;
    let driver = driver(&handle);

    // 新建腿
    let mut fresh = driver
        .writer(&rel("new.bin"), &WriteHint::default())
        .await
        .expect("writer");
    fresh.write(b"discarded").await.expect("write");
    fresh.abort().await.expect("abort");
    assert!(matches!(
        driver.stat(&rel("new.bin")).await,
        Err(StorageError::NotFound)
    ));

    // 覆盖腿：旧对象回位（逐字节）
    upload_bytes(&driver, &rel("over.bin"), b"original")
        .await
        .expect("seed");
    let mut over = driver
        .writer(&rel("over.bin"), &WriteHint::default())
        .await
        .expect("writer");
    over.write(b"replacement-that-never-lands")
        .await
        .expect("write");
    over.abort().await.expect("abort");
    assert_eq!(
        handle.take("/over.bin").as_deref(),
        Some(b"original".as_slice()),
        "abort restores the pre-writer object byte-for-byte"
    );
}

/// §4.6/硬仗②：stat 复核 size 不符 → Unavailable 不静默（stat_size_delta
/// 谎报注入面；谎报只影响 stat 报告，真实字节照落）。
#[tokio::test]
async fn stager_size_recheck_mismatch_is_unavailable_not_silent() {
    let handle = spawn_stub(
        Vfs::new(),
        stub::AuthMode::None,
        Knobs {
            stat_size_delta: Some(3),
            ..Knobs::default()
        },
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle);

    let error = upload_bytes(&driver, &rel("f.bin"), b"exact-16-bytes!!")
        .await
        .expect_err("server lies about the size");
    assert!(
        matches!(error, StorageError::Unavailable(_)),
        "size disagreement must surface as Unavailable, got {error:?}"
    );
    // 真实字节照落（谎报仅在 stat 报告面）——第二次 stat 恢复真值
    let entry = driver.stat(&rel("f.bin")).await.expect("fresh stat");
    assert_eq!(entry.size, 16);
}

/// §4.6/K67 H2（MOVE 半边）：close 的 MOVE ACK 丢失（效果已落）→ 重放
/// 探测 .part 不在 + final 就位 + size 吻合 → 按已提交继续。
#[tokio::test]
async fn stager_lost_ack_on_move_resumes_as_committed() {
    let handle = spawn_stub(
        Vfs::new(),
        stub::AuthMode::None,
        Knobs {
            lost_ack_after_effect: true,
            lost_ack_after_effect_skip: 1, // PUT 消耗首个槽 → ACK 断在 MOVE
            ..Knobs::default()
        },
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle);
    let data = b"committed-while-ack-lost";

    let entry = upload_bytes(&driver, &rel("f.bin"), data)
        .await
        .expect("lost MOVE ack resumes as committed");
    assert_eq!(entry.size, data.len() as u64);
    assert_eq!(handle.take("/f.bin").as_deref(), Some(data.as_slice()));
    // 恰一次 PUT + 恰一次 MOVE（防线 = stat 对账，不是重发 MOVE）
    assert_eq!(requests_of(&handle, "PUT").len(), 1);
    assert_eq!(requests_of(&handle, "MOVE").len(), 1);
}

/// K67 H2（PUT 半边）：PUT 的 ACK 丢失（效果已落）→ stat .part 对账
/// 吻合 → 续链；不吻合（kill 在路由前、效果未落）→ 如实上抛 + 零残留。
#[tokio::test]
async fn stager_lost_ack_on_put_recovers_via_part_recheck() {
    let handle = spawn_stub(
        Vfs::new(),
        stub::AuthMode::None,
        Knobs {
            lost_ack_after_effect: true,
            lost_ack_after_effect_skip: 0, // ACK 断在首个效果型请求（PUT）
            ..Knobs::default()
        },
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle);

    let data = b"put-landed-ack-lost";
    let entry = upload_bytes(&driver, &rel("f.bin"), data)
        .await
        .expect("lost PUT ack recovers via the part recheck");
    assert_eq!(entry.size, data.len() as u64);
    assert_eq!(
        handle.take("/f.bin").as_deref(),
        Some(b"put-landed-ack-lost".as_slice())
    );
}

/// 断言③：staging 进行中（含 stash 腿）list 只见业务条目——`.ckwd-`
/// 前缀暂存件（.part/.old）对 list 恒不可见。
#[tokio::test]
async fn staging_artifacts_stay_invisible_to_list() {
    let handle = plain(Vfs::new()).await;
    let driver = driver(&handle);
    upload_bytes(&driver, &rel("dir/committed.bin"), b"done")
        .await
        .expect("seed");

    // 覆盖写 staging 中：目录里有旧 stash + （手搓桩 VFS 里手动放一个
    // .part 孤儿，验证过滤双形态）
    let stager = driver
        .writer(&rel("dir/committed.bin"), &WriteHint::default())
        .await
        .expect("writer");
    handle.seed_file("/dir/orphan.ckwd-999-9.part", b"orphan");
    let names: Vec<String> = driver
        .list(
            &rel("dir"),
            Page {
                limit: 100,
                cursor: PageCursor::Start,
            },
        )
        .await
        .expect("list")
        .entries
        .into_iter()
        .map(|entry| entry.path.file_name().unwrap().to_string())
        .collect();
    assert!(
        names.is_empty(),
        "the in-flight target is stashed invisible and the .ckwd- part/.old \
         artifacts never surface in list: {names:?}"
    );
    stager.abort().await.expect("abort");
}

/// 全 verbs 的 Overwrite 恒显式（T/F 皆显式发送——矩阵⑤ rclone 缺头
/// 偏离行的驱动对策）：rename=F / stager=T 已在各用例断言，这里补
/// stash 腿的 Overwrite=T（writer 打开时的 final→.old 移动）。
#[tokio::test]
async fn stash_move_also_carries_explicit_overwrite() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", b"old");
    let handle = plain(vfs).await;
    let driver = driver(&handle);

    let stager = driver
        .writer(&rel("f.bin"), &WriteHint::default())
        .await
        .expect("writer");
    let moves = requests_of(&handle, "MOVE");
    assert_eq!(moves.len(), 1, "stash MOVE at writer open");
    assert_eq!(
        moves[0].overwrite.as_deref(),
        Some("T"),
        "stash sweep uses explicit Overwrite: T"
    );
    stager.abort().await.expect("abort");
}

/// H1（审查修复批）：close ④ 的 stat 复核**出错**臂（区别于尺寸不符臂
/// ——提交链已走完、MOVE 已固化）绝不 restore：stash 复位会把旧对象以
/// Overwrite:T 盖回已提交的新版（数据破坏，与尺寸不符臂的既定裁决自相
/// 矛盾）。注入：`kill_propfinds` 旋钮（PROPFIND 专属——close 链上的
/// PUT/MOVE 不消耗），覆盖写腿（stash 在场）恰断在 stat 复核。断言：
/// 错误如实上抛 + final 仍为**新写入字节**（stash 留为 `.ckwd-` 过滤残
/// 件——与尺寸不符臂同一善后形态）。
#[tokio::test]
async fn stager_stat_recheck_error_does_not_restore_the_stash() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", b"old-committed-content");
    let handle = plain(vfs).await;
    let driver = driver(&handle);

    let hint = WriteHint {
        size: Some(21),
        ..Default::default()
    };
    let mut stager = driver.writer(&rel("f.bin"), &hint).await.expect("writer");
    stager.write(b"new-committed-content").await.expect("write");
    // writer 打开已完成 stat 预检与 stash MOVE；close 链上 ensure_parents
    // 在卷根短路（零 PROPFIND）——这批 PROPFIND 杀必然全部落在 ④ 的
    // stat 复核上。4 枚 = 首发 + 3 次重试（PROPFIND 在重试白名单内、
    // send 层断连会自愈——必须耗尽预算才算「复核出错」）。
    handle
        .knobs
        .kill_propfinds
        .store(4, std::sync::atomic::Ordering::SeqCst);
    let error = stager
        .close()
        .await
        .expect_err("the failed recheck must surface, not be swallowed");
    assert!(
        matches!(error, StorageError::Unavailable(_) | StorageError::Io(_)),
        "{error:?}"
    );
    // 提交事实不动摇：final 仍是新写入的字节（错误臂 restore 会把旧对
    // 象盖回来——数据破坏的本缺陷面）。
    assert_eq!(
        handle.take("/f.bin").as_deref(),
        Some(b"new-committed-content".as_slice()),
        "the committed new version must survive the failed recheck"
    );
}
