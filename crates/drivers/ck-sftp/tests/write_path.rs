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

// --------------------------------------------- 审查修复批（提交/竞态面）---

/// close 的 rename 重放窗（审查修复）：rename 已在服务端执行但回复
/// 丢失（连接死亡 → with_retry 重连 → 重放 rename → part 已不在 →
/// NotFound）时，close 必须探测 final 是否已就位——已就位且尺寸恰为
/// written = 提交已落地，清 stash 返回成功；**不得**走 restore_scene
/// 把 stash 复位回去（那会把已提交的新版本覆盖回旧版——数据丢失）。
#[tokio::test]
async fn close_treats_replayed_rename_as_committed() {
    let (stub, driver) = setup().await;
    stub.add_file("/f.bin", b"old-old!");

    let hint = WriteHint {
        size: Some(7),
        ..Default::default()
    };
    let mut stager = driver.writer(&rel("f.bin"), &hint).await.expect("writer");
    stager.write(b"newdata").await.expect("write");

    // 模拟「rename 已在服务端执行、ACK 丢失」：服务端面把 part 直接
    // 搬到 final（此时 final 已是新内容、stash 仍在、part 已不在）
    assert!(
        stub.simulate_lost_ack_rename("/f.bin").await,
        "the staging part must exist before the simulated lost-ack rename"
    );
    assert_eq!(
        stub.file_bytes("/f.bin").as_deref(),
        Some(b"newdata".as_slice()),
        "sanity: the server-side rename landed the new content"
    );

    let entry = stager
        .close()
        .await
        .expect("close must treat the replayed rename as already committed");
    assert_eq!(entry.size, 7);
    assert_eq!(
        stub.file_bytes("/f.bin").as_deref(),
        Some(b"newdata".as_slice()),
        "the committed new version must survive close"
    );
    assert!(
        stub.staging_artifacts().is_empty(),
        "the stash must be cleaned after the detected commit: {:?}",
        stub.staging_artifacts()
    );
}

/// rename 执行点撞已存在 → `Exists`（审查修复，镜像 mkdir 竞态臂）：
/// 预检后、执行前目标被抢占创建——reject 形态服务器（OpenSSH 经典
/// rename 对已存在目标回 Failure）在驱动面必须归一为契约的 Exists
/// 而不是 Io。桩注入：目标对 stat 隐身、在下一个 rename 请求到达时
/// 现形并撞车。
#[tokio::test]
async fn rename_race_into_existing_target_reports_exists() {
    let (stub, driver) = setup().await;
    stub.add_file("/from.txt", b"moving");
    stub.add_file("/to.txt", b"occupied");
    stub.hide_until_next_rename("/to.txt");

    assert_eq!(
        driver.rename(&rel("from.txt"), &rel("to.txt")).await.err(),
        Some(StorageError::Exists),
        "a rename racing into a freshly created target must surface Exists, not Io"
    );
}

// --------------------------------- 审查修复批二（sftp-review，2026-09-25）---

/// H1：close 的硬仗②大小校验失败必须恢复 writer 打开前状态——服务端
/// 短写（close 确认正常、落盘字节 < written）时，嫌疑版本清除、旧版
/// 本从 .old stash 复位回 final。修复前该分支直接返回 Err：final 停在
/// 短写版本、旧版本遗留 .old 永久不可见（数据丢失级）。
#[tokio::test]
async fn close_size_mismatch_restores_the_previous_version() {
    let (stub, driver) = setup().await;
    stub.add_file("/f.bin", b"old-old!");

    let hint = WriteHint {
        size: Some(7),
        ..Default::default()
    };
    let mut stager = driver.writer(&rel("f.bin"), &hint).await.expect("writer");
    stager.write(b"newdata").await.expect("write");
    // 服务端短写注入：close 落盘只保留 3 字节（part 路径前缀匹配）
    stub.shrink_next_close("/f.bin.cksftp-", 3);

    let err = stager
        .close()
        .await
        .expect_err("size mismatch must fail the close");
    assert!(
        matches!(err, StorageError::Io(ref detail) if detail.contains("size mismatch")),
        "{err:?}"
    );
    // 修复点：旧版本复位回 final，嫌疑版本与暂存残件清空
    assert_eq!(
        stub.file_bytes("/f.bin").as_deref(),
        Some(b"old-old!".as_slice()),
        "the previous version must be restored after the size-mismatch failure"
    );
    assert!(
        stub.staging_artifacts().is_empty(),
        "no staging residue may survive: {:?}",
        stub.staging_artifacts()
    );
}

/// H2：目录 rename 的子树迁移必须携带 symlink——真机 OpenSSH 的
/// rename 是服务端原子子树迁移，链接一并随迁。桩此前只搬 dirs/files
/// 两张表（symlink 键留旧前缀：新路径下不可见、删除删不到）。
#[tokio::test]
async fn rename_directory_subtree_carries_symlinks() {
    let (stub, driver) = setup().await;
    stub.add_dir("/sub");
    stub.add_file("/sub/f.bin", b"payload");
    stub.add_dir("/outside");
    stub.add_file("/outside/keep.txt", b"guard");
    stub.add_symlink("/sub/link", "/outside");

    driver
        .rename(&rel("sub"), &rel("moved"))
        .await
        .expect("rename");

    // 链随迁：新路径下可见，本体是链接（lstat 面 stat → File 形态）
    let entry = driver
        .stat(&rel("moved/link"))
        .await
        .expect("the symlink must be visible under the new prefix");
    assert_eq!(entry.kind, cloudkit_storage::EntryKind::File);

    // 删除新子树：链接按文件语义删（绝不下潜 /outside）
    driver
        .delete(&entry_id(&driver, "moved"))
        .await
        .expect("recursive delete");
    assert!(driver.stat(&rel("moved")).await.is_err(), "subtree gone");
    assert_eq!(
        stub.file_bytes("/outside/keep.txt").as_deref(),
        Some(b"guard".as_slice()),
        "the link target must survive the recursive delete"
    );
    assert_eq!(
        stub.symlink_target("/moved/link"),
        None,
        "the moved link must be removed with its subtree"
    );
}

/// M4-甲：rename 撞悬空 symlink 目标 → `Exists`。悬空链也是既有目录
/// 项（OpenSSH 的 rename 恒拒覆盖）；驱动预检此前走跟随 stat——悬空
/// 链误报 NotFound，把服务端必然的拒绝漏成 Io（桩修复前甚至是假成
/// 功 + 桩状态错乱）。
#[tokio::test]
async fn rename_onto_dangling_symlink_reports_exists() {
    let (stub, driver) = setup().await;
    stub.add_file("/src.txt", b"src");
    stub.add_dangling_symlink("/dangling", "/nowhere");

    let err = driver
        .rename(&rel("src.txt"), &rel("dangling"))
        .await
        .expect_err("an existing entry (even a dangling link) must be refused");
    assert!(matches!(err, StorageError::Exists), "{err:?}");
    // 源原位、目标仍是原链（桩状态未被写花）
    assert_eq!(
        stub.file_bytes("/src.txt").as_deref(),
        Some(b"src".as_slice())
    );
    assert_eq!(
        stub.symlink_target("/dangling").as_deref(),
        Some("/nowhere"),
        "the target link must be untouched"
    );
    assert!(
        stub.file_bytes("/dangling").is_none(),
        "no file may be created over the link"
    );
}

/// M4-乙：活链目标同款（跟随预检即可命中——契约钉，修复前后恒绿）。
#[tokio::test]
async fn rename_onto_live_symlink_reports_exists() {
    let (stub, driver) = setup().await;
    stub.add_file("/src.txt", b"src");
    stub.add_dir("/guard");
    stub.add_symlink("/link", "/guard");

    let err = driver
        .rename(&rel("src.txt"), &rel("link"))
        .await
        .expect_err("a live link target must be refused");
    assert!(matches!(err, StorageError::Exists), "{err:?}");
    assert!(stub.is_dir("/guard"), "the link target must be untouched");
}

/// M3：stat/lstat 两个注入旋钮互不越界——stat 旋钮不被任何 lstat 面
/// 消费（delete、driver.stat 非根），lstat 旋钮在 driver.stat 的真实
/// 动词上生效（修复前单槽位共享 = 注入面比文档宽，conformance ⑤ 靠
/// lstat 偷吃 stat 槽位假绿）。
#[tokio::test]
async fn stat_and_lstat_injection_knobs_are_independent() {
    use russh_sftp::protocol::StatusCode;

    let (stub, driver) = setup().await;
    stub.add_file("/f.txt", b"x");
    stub.add_file("/g.txt", b"y");

    // stat 旋钮不被 lstat 面消费：delete（symlink_metadata = lstat）与
    // driver.stat（非根 = SSH_FXP_LSTAT）都不受 stat 注入影响
    stub.fail_next_stat(StatusCode::PermissionDenied);
    driver
        .delete(&entry_id(&driver, "f.txt"))
        .await
        .expect("lstat must not consume the stat injection");
    driver
        .stat(&rel("g.txt"))
        .await
        .expect("driver.stat issues lstat for non-root paths; the stat knob must not fire");

    // lstat 旋钮恰好在 driver.stat 的真实动词上生效（conformance ⑤
    // 的注入语义自此对齐 K67 后的 stat 实现）
    stub.fail_next_lstat(StatusCode::PermissionDenied);
    let err = driver
        .stat(&rel("g.txt"))
        .await
        .expect_err("the lstat injection must fire on driver.stat");
    assert!(
        matches!(err, StorageError::Unauthorized { recoverable: false }),
        "{err:?}"
    );
}
