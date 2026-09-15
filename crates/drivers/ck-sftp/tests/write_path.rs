//! SF2 行为测试 · 写路径与九方法行为面（计划 §5 SF2 + §4.4 三硬仗纪律）。
//!
//! 覆盖面：
//! - **mkdir**：新建 / 已存在 → Exists（目录与同名文件皆拒）/ 隐式父目录；
//! - **delete**：文件 / 递归目录 / 不存在 → NotFound / 他卷句柄 →
//!   NotFound / 卷根 → Invalid；
//! - **rename**：成功（含目录子树）/ 源缺 NotFound / 目标存在 Exists /
//!   后代 Invalid / 目标父目录隐式创建；
//! - **writer**：write+close 上传 → 远端 size == written（硬仗②）；
//!   size hint 不符 → Invalid；abort → 目标删除；空文件往返；
//! - **quota**：桩不声明 statvfs → total=None 降级（契约「未知/无上限」）；
//! - **三硬仗①句柄计数**：reader 正常读完 / 提前 Drop / stager close+abort
//!   后活句柄归零（桩的服务端计数器——aeroftp 教训的断言面）；
//! - **with_retry 重连腿**：服务端断连（abort 注入）后下次操作自动重建。

mod stub;

use ck_sftp::{SftpDriver, SftpParams};
use cloudkit_storage::{
    BackendHandle, EntryId, Page, RelPath, StorageDriver, StorageError, WriteHint,
};
use futures_util::StreamExt;
use stub::{Stub, StubAuth};

const USER: &str = "tester";
const PASSWORD: &str = "stub-only-password";

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

async fn setup() -> (Stub, SftpDriver) {
    let stub = Stub::start(StubAuth::password(USER, PASSWORD)).await;
    let mut pairs = stub.param_pairs();
    pairs.push((
        "sftp_host_fingerprint".to_string(),
        stub.fingerprint().to_string(),
    ));
    let params = SftpParams::from_pairs(&pairs).expect("params");
    let driver = SftpDriver::new(params).expect("driver");
    (stub, driver)
}

fn rel(path: &str) -> RelPath {
    RelPath::new(path).expect("valid rel path")
}

fn entry_id(driver: &SftpDriver, path: &str) -> EntryId {
    EntryId::new(driver.volume().clone(), BackendHandle::new(path))
}

async fn read_all(stream: cloudkit_storage::ByteStream) -> Result<Vec<u8>, StorageError> {
    let mut stream = stream;
    let mut out = Vec::new();
    while let Some(frame) = stream.next().await {
        out.extend_from_slice(&frame?);
    }
    Ok(out)
}

/// 轮询等待活句柄归零（close_nowait 是排队不确认的——最终一致性窗口）。
async fn await_zero_handles(stub: &Stub) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while stub.open_handle_count() != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "open handles did not drain: {}",
            stub.open_handle_count()
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

// -------------------------------------------------------------- mkdir ---

#[tokio::test]
async fn mkdir_creates_and_existing_yields_exists() {
    let (stub, driver) = setup().await;
    driver.mkdir(&rel("newdir")).await.expect("mkdir");
    assert!(stub.is_dir("/newdir"));

    // 已存在（目录）→ Exists
    assert_eq!(
        driver.mkdir(&rel("newdir")).await.err(),
        Some(StorageError::Exists)
    );
    // 同名文件同样拒（stat 预检命中即 Exists）
    stub.add_file("/afile", b"x");
    assert_eq!(
        driver.mkdir(&rel("afile")).await.err(),
        Some(StorageError::Exists)
    );
    // 卷根恒存在 → Exists
    assert_eq!(
        driver.mkdir(&RelPath::root()).await.err(),
        Some(StorageError::Exists)
    );
}

/// trait 契约：写入路径缺失父目录由驱动隐式创建（baidu ensure_parents
/// 先例——逐段 stat + create_dir）。
#[tokio::test]
async fn mkdir_creates_missing_parents_implicitly() {
    let (stub, driver) = setup().await;
    driver
        .mkdir(&rel("deep/nested/dir"))
        .await
        .expect("mkdir with missing parents");
    assert!(stub.is_dir("/deep"));
    assert!(stub.is_dir("/deep/nested"));
    assert!(stub.is_dir("/deep/nested/dir"));
}

// ------------------------------------------------------------- delete ---

#[tokio::test]
async fn delete_file_and_missing_not_found() {
    let (stub, driver) = setup().await;
    stub.add_file("/doomed.bin", b"bye");
    driver
        .delete(&entry_id(&driver, "doomed.bin"))
        .await
        .expect("delete file");
    assert!(!stub.file_exists("/doomed.bin"));

    assert_eq!(
        driver.delete(&entry_id(&driver, "doomed.bin")).await.err(),
        Some(StorageError::NotFound),
        "deleting a missing path reports NotFound"
    );
}

/// trait 契约：目录删除为递归（SFTP rmdir 只删空目录——深度优先清空
/// 再删自身；条目类型以 readdir attrs 为准，绝不跟随链接）。
#[tokio::test]
async fn delete_directory_recursively() {
    let (stub, driver) = setup().await;
    stub.add_dir("/tree");
    stub.add_dir("/tree/mid");
    stub.add_file("/tree/top.txt", b"t");
    stub.add_file("/tree/mid/leaf.bin", b"l");

    driver
        .delete(&entry_id(&driver, "tree"))
        .await
        .expect("recursive delete");
    assert!(!stub.is_dir("/tree"));
    assert!(!stub.is_dir("/tree/mid"));
    assert!(!stub.file_exists("/tree/top.txt"));
    assert!(!stub.file_exists("/tree/mid/leaf.bin"));
}

/// 他卷句柄 → NotFound；卷根 → Invalid（trait 契约，local 同款）。
#[tokio::test]
async fn delete_foreign_handle_and_volume_root() {
    let (_stub, driver) = setup().await;
    let foreign = EntryId::new(
        cloudkit_storage::VolumeId::new("sftp", "other@host:22").expect("volume"),
        BackendHandle::new("a.txt"),
    );
    assert_eq!(
        driver.delete(&foreign).await.err(),
        Some(StorageError::NotFound)
    );

    assert_eq!(
        driver.delete(&entry_id(&driver, "")).await.err(),
        Some(StorageError::Invalid),
        "volume root is not deletable"
    );
}

// ---------------------------------------------------------- symlinks ---

/// 符号链接契约（SF4 真机矩阵揭出的 GAP-A02 缺陷的**离线回放**——防
/// 回归）：link-to-dir 的本体是链接，不是目录。
///
/// 修复前（真机红）：`stat`/`list` 用 SSH_FXP_STAT（跟随）→ link-to-dir
/// 报 `Dir`；`list(link)` 枚举链接目标（真机列出 /etc 的 207 个条目）；
/// 递归删除会下潜。修复 = 目录性判定与递归删除走 lstat（不跟随）。
#[tokio::test]
async fn symlink_to_dir_is_not_traversed() {
    let (stub, driver) = setup().await;
    stub.add_dir("/guard");
    stub.add_file("/guard/x.bin", &pattern(10));
    // 受保护的目标目录 + 指向它的链接
    stub.add_dir("/protector");
    stub.add_file("/protector/keep.bin", &pattern(3));
    stub.add_symlink("/guard/dirlink", "/protector");

    // ①list(link-to-dir) 必须 Invalid（不可下潜），绝不是链接目标的条目
    assert_eq!(
        driver.list(&rel("guard/dirlink"), Page::all()).await.err(),
        Some(StorageError::Invalid),
        "a symlink to a directory must not be traversable"
    );
    // ②stat(link-to-dir) 报本体形态（File——链接自身），不是 Dir
    let st = driver.stat(&rel("guard/dirlink")).await.expect("stat link");
    assert_eq!(
        st.kind,
        cloudkit_storage::EntryKind::File,
        "lstat semantics: the link reports itself, not the target's kind"
    );
    // ③递归删除 guard：链接被删、**目标内容完好**（绝不下潜）
    driver
        .delete(&entry_id(&driver, "guard"))
        .await
        .expect("recursive delete");
    assert_eq!(
        driver.stat(&rel("guard")).await.err(),
        Some(StorageError::NotFound),
        "guard is gone"
    );
    let kept = driver
        .list(&rel("protector"), Page::all())
        .await
        .expect("the link target survives");
    assert!(
        kept.entries
            .iter()
            .any(|e| e.path.as_str().ends_with("keep.bin")),
        "the link target's content must survive the recursive delete: {:?}",
        kept.entries
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>()
    );
    // ④list 中链接按本体形态呈现（File，不是 Dir——aeroftp 教训 7）
    stub.add_symlink("/guard2_link", "/protector");
    let listing = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list root");
    let link_entry = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "guard2_link")
        .expect("the link appears in its parent listing");
    assert_eq!(
        link_entry.kind,
        cloudkit_storage::EntryKind::File,
        "the listing reports the link itself, not a directory"
    );
    // ⑤链接**指向文件**时 reader 仍跟随（用户预期：读链即读目标）
    stub.add_dir("/fdir");
    stub.add_file("/fdir/real.bin", &pattern(64));
    stub.add_symlink("/fdir/link.bin", "real.bin");
    let got = read_all(
        driver
            .reader(&entry_id(&driver, "fdir/link.bin"), None)
            .await
            .expect("reader follows a file link"),
    )
    .await
    .expect("read");
    assert_eq!(
        got,
        pattern(64),
        "reading through a link yields the target's bytes"
    );
}

// ------------------------------------------------------------- rename ---

#[tokio::test]
async fn rename_file_moves_content() {
    let (stub, driver) = setup().await;
    let data = pattern(1_000);
    stub.add_file("/origin.bin", &data);

    driver
        .rename(&rel("origin.bin"), &rel("moved.bin"))
        .await
        .expect("rename");
    assert!(!stub.file_exists("/origin.bin"), "old path gone");
    assert_eq!(
        stub.file_bytes("/moved.bin"),
        Some(data),
        "content preserved"
    );
}

#[tokio::test]
async fn rename_error_surface() {
    let (stub, driver) = setup().await;
    stub.add_file("/src.bin", b"s");
    stub.add_file("/dst.bin", b"d");
    stub.add_dir("/adir");

    // 源缺失 → NotFound
    assert_eq!(
        driver.rename(&rel("ghost.bin"), &rel("x.bin")).await.err(),
        Some(StorageError::NotFound)
    );
    // 目标已存在 → Exists（预检归一——服务器对 overwrite rename 行为
    // 不一致，OpenSSH 拒绝/Failure 形态）
    assert_eq!(
        driver.rename(&rel("src.bin"), &rel("dst.bin")).await.err(),
        Some(StorageError::Exists)
    );
    // 目标是源的后代 → Invalid（把目录 rename 进自己内部）
    assert_eq!(
        driver.rename(&rel("adir"), &rel("adir/inside")).await.err(),
        Some(StorageError::Invalid)
    );
    // 卷根端点 → Invalid
    assert_eq!(
        driver.rename(&RelPath::root(), &rel("x")).await.err(),
        Some(StorageError::Invalid)
    );
    assert_eq!(
        driver.rename(&rel("src.bin"), &RelPath::root()).await.err(),
        Some(StorageError::Invalid)
    );
}

/// 目录 rename = 服务端单侧移动整棵子树（server_side_move 能力位依据）。
#[tokio::test]
async fn rename_moves_directory_subtree() {
    let (stub, driver) = setup().await;
    stub.add_dir("/old");
    stub.add_dir("/old/inner");
    let data = pattern(64);
    stub.add_file("/old/inner/file.bin", &data);

    driver
        .rename(&rel("old"), &rel("new"))
        .await
        .expect("rename dir");
    assert!(!stub.is_dir("/old"));
    assert!(stub.is_dir("/new/inner"));
    assert_eq!(stub.file_bytes("/new/inner/file.bin"), Some(data));
}

/// 目标父目录缺失 → 隐式创建后移动。
#[tokio::test]
async fn rename_creates_missing_target_parents() {
    let (stub, driver) = setup().await;
    stub.add_file("/plain.bin", b"p");
    driver
        .rename(&rel("plain.bin"), &rel("fresh/nest/plain.bin"))
        .await
        .expect("rename with missing target parents");
    assert!(stub.is_dir("/fresh/nest"));
    assert!(stub.file_exists("/fresh/nest/plain.bin"));
}

// ------------------------------------------------------------- writer ---

/// 硬仗②：上传 close 后远端 size == written（桩 commit-on-close 真实
/// 落盘下自然成立——0 字节上传 bug 的持久修复面）。
#[tokio::test]
async fn writer_upload_roundtrip_matches_remote_bytes() {
    let (stub, driver) = setup().await;
    let data = pattern(200_000); // > 32KiB 写包 × 多段流水线
    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..Default::default()
    };
    let mut stager = driver
        .writer(&rel("up/loads/payload.bin"), &hint)
        .await
        .expect("writer");

    // 三帧写入（模拟加密分块帧边界——对 stager 是不透明字节序列）
    for chunk in data.chunks(70_000) {
        stager.write(chunk).await.expect("stager write");
    }
    let entry = stager.close().await.expect("close");
    assert_eq!(entry.size, data.len() as u64, "entry size == written");
    assert_eq!(entry.kind, cloudkit_storage::EntryKind::File);
    assert_eq!(
        stub.file_bytes("/up/loads/payload.bin"),
        Some(data.clone()),
        "remote bytes"
    );

    // 隐式父目录已建
    assert!(stub.is_dir("/up/loads"));

    // 硬仗①：close 后无活句柄
    assert_eq!(stub.open_handle_count(), 0);

    // 读回字节等（读写往返闭环）
    let got = read_all(
        driver
            .reader(&entry_id(&driver, "up/loads/payload.bin"), None)
            .await
            .expect("reader"),
    )
    .await
    .expect("read back");
    assert_eq!(got, data);
}

#[tokio::test]
async fn writer_empty_upload_roundtrip() {
    let (stub, driver) = setup().await;
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
    assert_eq!(entry.size, 0);
    assert_eq!(stub.file_bytes("/empty.bin"), Some(Vec::new()));
    assert_eq!(stub.open_handle_count(), 0);
}

/// WriteHint 契约：承诺与实际不符 → Invalid（file 由 russh-sftp Drop
/// 收尾，远端残留是 truncate 语义的既定边界——模块文档声明）。
#[tokio::test]
async fn writer_size_hint_mismatch_is_invalid() {
    let (_stub, driver) = setup().await;
    let hint = WriteHint {
        size: Some(100),
        ..Default::default()
    };
    let mut stager = driver
        .writer(&rel("short.bin"), &hint)
        .await
        .expect("writer");
    stager.write(&pattern(50)).await.expect("write");
    assert_eq!(
        stager.close().await.err(),
        Some(StorageError::Invalid),
        "hinted 100 but wrote 50"
    );
}

/// abort：awaited close + 尽力删除目标。
#[tokio::test]
async fn writer_abort_deletes_target() {
    let (stub, driver) = setup().await;
    let hint = WriteHint::default();
    let mut stager = driver
        .writer(&rel("aborted.bin"), &hint)
        .await
        .expect("writer");
    stager.write(&pattern(128)).await.expect("write");
    stager.abort().await.expect("abort");
    assert!(
        !stub.file_exists("/aborted.bin"),
        "abort removes the target"
    );
    assert_eq!(stub.open_handle_count(), 0);
}

// -------------------------------------------- commit-on-close staging ---

/// 断言①的驱动面（conformance 断言的离线红→绿所对应）：staging 窗口内
/// 目标路径必须不可见——写入数据落在暂存件上，close 才固化。
#[tokio::test]
async fn mid_staging_target_is_invisible_and_close_commits() {
    let (_stub, driver) = setup().await;
    let data = pattern(4096);
    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..Default::default()
    };
    let mut stager = driver
        .writer(&rel("staged.bin"), &hint)
        .await
        .expect("writer");
    stager.write(&data).await.expect("write");
    assert_eq!(
        driver.stat(&rel("staged.bin")).await.err(),
        Some(StorageError::NotFound),
        "mid-staging stat must be NotFound (commit-on-close invisibility)"
    );
    let entry = stager.close().await.expect("close");
    assert_eq!(entry.size, data.len() as u64);
    assert_eq!(
        driver
            .stat(&rel("staged.bin"))
            .await
            .expect("visible after close")
            .size,
        data.len() as u64
    );
}

/// 覆盖写 + abort = 回到 writer 打开前状态（旧版本恢复，不是丢失）——
/// stash 方案的恢复语义（ck-local stager 同款）。
#[tokio::test]
async fn abort_restores_the_stashed_old_version() {
    let (stub, driver) = setup().await;
    let old = pattern(3000);
    stub.add_file("/keep.bin", &old);
    let mut stager = driver
        .writer(&rel("keep.bin"), &WriteHint::default())
        .await
        .expect("writer");
    stager.write(&pattern(100)).await.expect("write");
    // staging 窗口：旧版本已 stash → 目标不可见
    assert_eq!(
        driver.stat(&rel("keep.bin")).await.err(),
        Some(StorageError::NotFound),
        "overwrite staging must hide the old version too"
    );
    stager.abort().await.expect("abort");
    let got = read_all(
        driver
            .reader(&entry_id(&driver, "keep.bin"), None)
            .await
            .expect("reader after abort restores old"),
    )
    .await
    .expect("read");
    assert_eq!(got, old, "abort must restore the pre-writer version");
}

/// 暂存件对 list 不可见（驱动实现细节，不是卷内容——conformance 断言③
/// 的集合完整性依赖它；local 的 `.cklocal-staging/` 过滤同源）。
#[tokio::test]
async fn staging_artifacts_are_invisible_in_list() {
    let (_stub, driver) = setup().await;
    driver.mkdir(&rel("dir")).await.expect("mkdir");
    // 打开一个 stager 不关闭：暂存件此刻在远端存在（同一目录下）
    let mut stager = driver
        .writer(&rel("dir/live.bin"), &WriteHint::default())
        .await
        .expect("writer");
    stager.write(&pattern(64)).await.expect("write");
    let listing = driver
        .list(&rel("dir"), cloudkit_storage::Page::all())
        .await
        .expect("list");
    assert!(
        listing.entries.is_empty(),
        "staging artifacts must not surface: {:?}",
        listing
            .entries
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>()
    );
    stager.abort().await.expect("abort");
    let listing = driver
        .list(&rel("dir"), cloudkit_storage::Page::all())
        .await
        .expect("list after abort");
    assert!(listing.entries.is_empty(), "abort leaves no artifacts");
}

/// 目标是已存在目录 → Invalid；卷根 → Invalid（trait 契约）。
#[tokio::test]
async fn writer_rejects_dir_target_and_root() {
    let (stub, driver) = setup().await;
    stub.add_dir("/adir");
    assert_eq!(
        driver
            .writer(&rel("adir"), &WriteHint::default())
            .await
            .err(),
        Some(StorageError::Invalid)
    );
    assert_eq!(
        driver
            .writer(&RelPath::root(), &WriteHint::default())
            .await
            .err(),
        Some(StorageError::Invalid)
    );
}

/// 覆盖写：既有文件被 TRUNCATE 重写（硬仗③：WRITE|CREATE|TRUNCATE——
/// 绝不用 APPEND；旧尾巴不得残留）。
#[tokio::test]
async fn writer_overwrite_truncates_old_content() {
    let (stub, driver) = setup().await;
    stub.add_file("/stale.bin", &pattern(50_000));
    let replacement = pattern(100);
    let hint = WriteHint {
        size: Some(replacement.len() as u64),
        ..Default::default()
    };
    let mut stager = driver
        .writer(&rel("stale.bin"), &hint)
        .await
        .expect("writer");
    stager.write(&replacement).await.expect("write");
    stager.close().await.expect("close");
    assert_eq!(
        stub.file_bytes("/stale.bin"),
        Some(replacement),
        "old tail must not survive TRUNCATE"
    );
}

// -------------------------------------------------------------- quota ---

/// 桩不声明 statvfs@openssh.com → fs_info → None → Quota { total: None }
///（契约「未知/无上限」形态——桩的 unimplemented 腿）。
#[tokio::test]
async fn quota_degrades_to_none_without_statvfs() {
    let (_stub, driver) = setup().await;
    let quota = driver.quota().await.expect("quota");
    assert_eq!(quota.total, None, "no statvfs extension advertised");
    assert_eq!(quota.used, 0);
}

// ------------------------------------------------------ 三硬仗① 观测 ---

/// reader 正常读完 → 句柄归零（三个 awaited close 分支之一）。
#[tokio::test]
async fn reader_full_consumption_releases_handle() {
    let (stub, driver) = setup().await;
    let data = pattern(150_000);
    stub.add_file("/drain.bin", &data);
    let got = read_all(
        driver
            .reader(&entry_id(&driver, "drain.bin"), None)
            .await
            .expect("reader"),
    )
    .await
    .expect("read");
    assert_eq!(got, data);
    assert_eq!(
        stub.open_handle_count(),
        0,
        "completed reader closes its handle"
    );
}

/// reader 提前 Drop → close_nowait 排队 → 句柄**最终**归零（库行为：
/// 不等待确认但确实发出——若不归零则如实报告库边界）。
#[tokio::test]
async fn reader_early_drop_eventually_releases_handle() {
    let (stub, driver) = setup().await;
    let data = pattern(300_000);
    stub.add_file("/part.bin", &data);
    let mut stream = driver
        .reader(&entry_id(&driver, "part.bin"), None)
        .await
        .expect("reader");
    let first = stream
        .next()
        .await
        .expect("first frame")
        .expect("frame bytes");
    assert_eq!(first.len(), 64 * 1024);
    drop(stream); // 中途放弃——不读完
    await_zero_handles(&stub).await;
}

// ----------------------------------------------------- with_retry 腿 ---

/// 断连自动重建：服务端 abort 连接任务（TCP 关闭）→ 下一次操作以
/// `Unavailable` 形态浮现 → 清槽重连一次 → 操作成功；连接数 2。
#[tokio::test]
async fn next_operation_reconnects_after_connection_kill() {
    let (stub, driver) = setup().await;
    driver
        .stat(&RelPath::root())
        .await
        .expect("initial op connects");
    assert_eq!(stub.connection_count(), 1);

    stub.kill_connections();
    // 断连后的首个操作：重连骨架（with_retry）应吸收故障并重建
    let second = driver.stat(&RelPath::root()).await;
    assert!(
        second.is_ok(),
        "next op after kill must transparently reconnect, got {:?}",
        second.err()
    );
    assert_eq!(stub.connection_count(), 2, "exactly one rebuild");
}
