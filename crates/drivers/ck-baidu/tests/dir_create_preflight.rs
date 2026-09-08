//! 目录 create 冲突 = errno=0 + 空副本重命名的预检回归（Batch B2 第五轮
//! 真网返工，2026-09-08）。
//!
//! 真网实证（干净探针，netdisk UA）：`POST xpan/file?method=create` form
//! `path=<已存在目录>&isdir=1` → **errno=0**（成功假象），远端保留原目录
//! 并生成 `<原名>_<时间戳>` 空副本目录——**非 -8**。驱动原「-8 → Exists」
//! 容错永不触发：mkdir 撞已存在返回 Ok（conformance ④ 真网必挂）、
//! ensure_parents 每次撞已存在层即产出空目录垃圾。
//!
//! mock 已对齐真实形态（errno=0 + 副本，`common/mod.rs`），驱动 mkdir /
//! ensure_parents 全面转 **list 预检**（先 list 父目录：已存在 → Exists/
//! 跳过，不存在才 create）。本套件钉死该预检的离线检出力：若实现回退到
//! 无预检形态，ghost 副本断言即红（mock 会真的生成副本入树）。

mod common;

use std::sync::Arc;

use ck_baidu::{factory, BaiduDriver};
use cloudkit_storage::{RelPath, StorageDriver, StorageError, WriteHint};

use common::{pattern_bytes, MockBaidu, MOCK_ROOT};

/// 种子根目录 + 构造驱动（metadata_ops 同款形态）。
async fn setup() -> (MockBaidu, Arc<BaiduDriver>) {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let driver = factory(&mock.params(None))
        .await
        .expect("baidu driver connect");
    (mock, driver)
}

/// ghost 副本观测：树中 `<父>/<名>_` 前缀条目数（后端冲突重命名形态
/// `<原名>_<时间戳>`——对 `<原名>_` 前缀计数即可捕获，不依赖具体时戳值）。
fn ghost_count(mock: &MockBaidu, dir_abs: &str) -> usize {
    mock.entry_count_with_prefix(&format!("{dir_abs}_"))
}

/// mkdir 撞已存在目录：必须 `Err(Exists)` 且**不触发后端冲突重命名**
/// （树中无 `<名>_<时间戳>` 空副本）。
///
/// 真网形态下这条断言是 conformance ④「mkdir 已存在 → Exists」的真网
/// 承重：errno=0 成功假象使 -8 容错失效，Exists 只能由 list 预检兜住。
#[tokio::test]
async fn mkdir_on_existing_path_yields_exists_without_ghost_copy() {
    let (mock, driver) = setup().await;
    let dir_abs = format!("{MOCK_ROOT}/dup");
    mock.seed_dir(&dir_abs);

    let err = driver
        .mkdir(&RelPath::new("dup").expect("rel path"))
        .await
        .expect_err("mkdir 已存在路径必须报错");
    assert_eq!(
        err,
        StorageError::Exists,
        "mkdir 已存在 → Exists（trait 契约；errno=0 假象下由 list 预检兜住）"
    );
    assert_eq!(
        ghost_count(&mock, &dir_abs),
        0,
        "不得触发后端冲突重命名：树中无 `dup_<时间戳>` 空副本（真网实证的垃圾形态）"
    );
}

/// 上传到已存在目录树（ensure_parents 逐级撞已存在层）：全程**零 ghost
/// 目录**——每次上传都产空目录垃圾是本轮真网实证发现的核心缺陷。
#[tokio::test]
async fn upload_into_existing_dir_tree_creates_no_ghost_dirs() {
    let (mock, driver) = setup().await;
    let a_abs = format!("{MOCK_ROOT}/a");
    let b_abs = format!("{MOCK_ROOT}/a/b");
    mock.seed_dir(&a_abs);
    mock.seed_dir(&b_abs);

    let data = pattern_bytes(1024);
    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..Default::default()
    };
    let path = RelPath::new("a/b/f.bin").expect("rel path");
    let mut stager = driver.writer(&path, &hint).await.expect("writer 打开");
    stager.write(&data).await.expect("write 到齐");
    let entry = stager.close().await.expect("close");

    // 上传本身成功且对象落在目标路径。
    assert_eq!(entry.size, data.len() as u64);
    assert_eq!(entry.path.as_str(), "a/b/f.bin");
    // 核心断言：每一层已存在目录都不被 create 撞出空副本。
    assert_eq!(
        ghost_count(&mock, &a_abs),
        0,
        "a 已存在：ensure_parents 不得 create 它（否则生成 `a_<时间戳>` 空副本）"
    );
    assert_eq!(
        ghost_count(&mock, &b_abs),
        0,
        "a/b 已存在：ensure_parents 不得 create 它（否则生成 `b_<时间戳>` 空副本）"
    );
    // 树中无任何 ghost 形态条目（泛化扫描——防预检只护了断言过的层）。
    let all_ghosts: Vec<String> = mock
        .entry_paths()
        .into_iter()
        .filter(|p| is_ghost_path(p))
        .collect();
    assert!(
        all_ghosts.is_empty(),
        "树中不得有任何 `<名>_<时间戳>` 后缀条目：{all_ghosts:?}"
    );
}

/// ghost 副本形态判定：路径名以 `_YYYYMMDD_HHMMSS`（15 字符时间戳）结尾。
fn is_ghost_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or_default();
    let Some((stem, ts)) = name.rsplit_once('_') else {
        return false;
    };
    !stem.is_empty()
        && ts.len() == 15
        && ts.as_bytes()[8] == b'_'
        && ts.replace('_', "").bytes().all(|b| b.is_ascii_digit())
}
