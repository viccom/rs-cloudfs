//! ck-local 边界钉测（审查 M1/M2 修复批，2026-09-25）。
//!
//! 契约（findings = docs/tracking/baidu-local-review-findings.md ck-local 节；
//! sftp K67 是全队的 symlink 语义先例——**报本体、删只删链、reader 跟随**）：
//!
//! - **M1 lstat 面**：stat/mkdir 预检/delete/rename 预检走
//!   `symlink_metadata`（本体形态）——断链 symlink 是可寻址本体而非
//!   NotFound、symlink-to-dir 的 delete 只删链（跟随语义会把 link-to-dir
//!   送进 remove_dir_all，Unix ENOTDIR 删除失效）；reader 保持跟随
//!   （读的是链接指向的内容——与 sftp `reader` 同裁决）；
//! - **M2 Windows 大小写改名**：`rename` 的目标存在预检在大小写不敏感
//!   FS 上会命中源自身（`Mixed.TXT`→`mixed.txt` 恒 Exists）——
//!   canonicalize 同一文件且名字非逐字相同 → 放行让 `fs::rename` 翻
//!   拼写；Unix 大小写敏感、两个 case 变体可各自存在 → 照常 Exists
//!   （防放行面过宽的钉测在侧）；
//! - Windows 对 symlink 创建需要特权/开发者模式：Unix 腿 `#[cfg(unix)]`
//!   在 WSL 实跑；Windows 上本文件只承 M2 腿（lstat 代码路径跨平台共用
//!   ——`fs::symlink_metadata`）。

use std::sync::Arc;

use ck_local::{factory, LocalDriver, LocalParams};
#[cfg(unix)]
use cloudkit_storage::{EntryKind, Page, StorageError};
use cloudkit_storage::{RelPath, StorageDriver, WriteHint};
#[cfg(unix)]
use std::path::Path;

async fn setup() -> (tempfile::TempDir, Arc<LocalDriver>) {
    let dir = tempfile::tempdir().expect("temp root");
    let driver = factory(&LocalParams {
        root: dir.path().to_path_buf(),
    })
    .await
    .expect("local driver");
    (dir, driver)
}

/// 经驱动写面播种一个已提交文件（commit-on-close 全链）。
async fn seed(driver: &LocalDriver, rel: &str, bytes: &[u8]) {
    let rel = RelPath::new(rel).expect("valid rel");
    let hint = WriteHint {
        size: Some(bytes.len() as u64),
        ..Default::default()
    };
    let mut stager = driver.writer(&rel, &hint).await.expect("writer");
    stager.write(bytes).await.expect("write");
    stager.close().await.expect("close");
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> bool {
    match std::os::unix::fs::symlink(target, link) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("symlink 不可用（{e}）——本腿跳过");
            false
        }
    }
}

// ---------------------------------------------------------------- M1 ---
// （Unix 实跑——symlink 创建与 ENOTDIR 形态都是 Unix 语义；Windows 腿
// 需要特权，lstat 代码路径跨平台共用不重复造特权腿。）

/// 断链 symlink 是**可寻址本体**：stat 报 File 而非 NotFound（修复前
/// fs::metadata 跟随断链 → NotFound）。红→绿钉住本体面。
#[cfg(unix)]
#[tokio::test]
async fn stat_reports_a_broken_symlink_body_instead_of_not_found() {
    let (_dir, driver) = setup().await;
    let root = driver.root_path().to_path_buf();
    assert!(
        make_symlink(&root.join("no-such-target.bin"), &root.join("dangling")),
        "symlink 不可用——环境缺创建权限"
    );

    let entry = driver
        .stat(&RelPath::new("dangling").expect("rel"))
        .await
        .expect("断链 symlink 的本体可寻址（修复前 NotFound）");
    assert_eq!(entry.kind, EntryKind::File, "本体形态：{entry:?}");
}

/// 活链同样报本体（修复前跟随目标：size 是目标内容长度；本体面下
/// size 是链接自身 = 目标路径串长）。
#[cfg(unix)]
#[tokio::test]
async fn stat_reports_the_link_body_for_a_live_symlink() {
    let (_dir, driver) = setup().await;
    let root = driver.root_path().to_path_buf();
    seed(&driver, "a.txt", b"AAA").await;
    // 相对目标（"a.txt" 5 字符）——本体 size 断言即目标串长。
    match std::os::unix::fs::symlink("a.txt", root.join("lnk")) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("symlink 不可用（{e}）——本腿跳过");
            return;
        }
    }

    let entry = driver
        .stat(&RelPath::new("lnk").expect("rel"))
        .await
        .expect("stat 链接本体");
    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(
        entry.size, 5,
        "本体 size = 链接目标串长度（修复前 = 目标内容长度 3）"
    );
}

/// symlink-to-dir 的 delete **只删链**、真实子树完好（特性化钉子——
/// 2026-09-25 WSL 实证：现行 std 的 remove_dir_all 对 symlink 已是
/// O_NOFOLLOW 安全、只删链本身；审查断言②「ENOTDIR 删除失效」在现行
/// 工具链被推翻。保留本钉防回归；lstat 化后语义继续成立）。
#[cfg(unix)]
#[tokio::test]
async fn delete_removes_only_the_link_of_a_dir_symlink() {
    let (_dir, driver) = setup().await;
    let root = driver.root_path().to_path_buf();
    seed(&driver, "real/kid.txt", b"precious").await;
    assert!(make_symlink(&root.join("real"), &root.join("lnk-dir")));

    let listing = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list 根");
    let id = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "lnk-dir")
        .expect("链本体在列举中可见")
        .id
        .clone();

    driver
        .delete(&id)
        .await
        .expect("删除 symlink 本体（修复前 remove_dir_all 撞 ENOTDIR）");

    assert!(
        tokio::fs::symlink_metadata(&root.join("lnk-dir"))
            .await
            .is_err(),
        "链已消失"
    );
    assert_eq!(
        tokio::fs::read(root.join("real/kid.txt"))
            .await
            .expect("目标子树完好"),
        b"precious",
        "真实目录与其内容不受删除波及"
    );
}

/// 断链 symlink 可删（本体面）：修复前 delete 的 metadata 预检跟随断链
/// → NotFound，链接本体永远删不掉；修复后 lstat → 本体 File → 删。
#[cfg(unix)]
#[tokio::test]
async fn delete_removes_a_dangling_symlink_body() {
    let (_dir, driver) = setup().await;
    let root = driver.root_path().to_path_buf();
    assert!(
        make_symlink(&root.join("gone-target"), &root.join("dangling")),
        "symlink 不可用"
    );

    let listing = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list 根");
    let id = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "dangling")
        .expect("断链本体在列举中可见")
        .id
        .clone();

    driver
        .delete(&id)
        .await
        .expect("断链本体可删（修复前 NotFound）");
    assert!(
        tokio::fs::symlink_metadata(&root.join("dangling"))
            .await
            .is_err(),
        "链已消失"
    );
}

/// list 报本体（DirEntry::metadata 的 std lstat 语义天然成立——特性化
/// 钉子，防未来有人改成跟随）。
#[cfg(unix)]
#[tokio::test]
async fn list_reports_symlink_bodies_not_targets() {
    let (_dir, driver) = setup().await;
    let root = driver.root_path().to_path_buf();
    driver
        .mkdir(&RelPath::new("real-dir").expect("rel"))
        .await
        .expect("mkdir");
    assert!(make_symlink(&root.join("real-dir"), &root.join("lnk")));

    let listing = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list");
    let lnk = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "lnk")
        .expect("链可见");
    assert_eq!(
        lnk.kind,
        EntryKind::File,
        "本体形态（非下潜目标的 Dir）：{lnk:?}"
    );
}

/// reader **保持跟随**（sftp 同裁决：读的是链接指向的内容——修复显式
/// 保留 metadata 跟随语义，本钉防未来误改）。
#[cfg(unix)]
#[tokio::test]
async fn reader_follows_symlinks_to_content() {
    use futures_util::StreamExt;

    let (_dir, driver) = setup().await;
    let root = driver.root_path().to_path_buf();
    seed(&driver, "data.txt", b"follow-me").await;
    assert!(make_symlink(&root.join("data.txt"), &root.join("link.bin")));

    let listing = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list");
    let id = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "link.bin")
        .expect("链可见")
        .id
        .clone();

    let mut stream = driver.reader(&id, None).await.expect("reader 跟随");
    let mut out = Vec::new();
    while let Some(frame) = stream.next().await {
        out.extend_from_slice(&frame.expect("frame"));
    }
    assert_eq!(out, b"follow-me");
}

// ---------------------------------------------------------------- M2 ---

/// Windows 大小写改名（审查 M2）：目标预检在大小写不敏感 FS 上命中源
/// 自身——修复前恒 Exists；修复后 canonicalize 同一文件 → 放行，
/// fs::rename 翻拼写。
#[cfg(windows)]
#[tokio::test]
async fn case_only_rename_flips_the_spelling() {
    let (_dir, driver) = setup().await;
    seed(&driver, "Mixed.TXT", b"payload").await;

    driver
        .rename(
            &RelPath::new("Mixed.TXT").expect("from"),
            &RelPath::new("mixed.txt").expect("to"),
        )
        .await
        .expect("大小写改名放行（修复前命中源自身 → Exists）");

    let root = _dir.path();
    let actual = std::fs::read_dir(root)
        .expect("readdir")
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect::<Vec<_>>();
    assert!(
        actual.contains(&"mixed.txt".to_string()),
        "盘上实际拼写已翻转：{actual:?}"
    );
    assert!(
        !actual.iter().any(|n| n == "Mixed.TXT"),
        "旧拼写不复存在：{actual:?}"
    );
}

/// Unix 防过宽钉测：两个 case 变体是**不同文件**——rename(A→a) 必须
/// Exists 且不动 a.txt（canonicalize 同文件放行不得吞掉真冲突）。
#[cfg(unix)]
#[tokio::test]
async fn rename_to_a_distinct_case_variant_stays_exists() {
    let (_dir, driver) = setup().await;
    seed(&driver, "A.txt", b"upper").await;
    seed(&driver, "a.txt", b"lower").await;

    let err = driver
        .rename(
            &RelPath::new("A.txt").expect("from"),
            &RelPath::new("a.txt").expect("to"),
        )
        .await
        .expect_err("两个不同文件 → Exists");
    assert_eq!(err, StorageError::Exists);
    assert_eq!(
        tokio::fs::read(_dir.path().join("a.txt"))
            .await
            .expect("read"),
        b"lower",
        "被拒的 rename 不改动目标"
    );
}
