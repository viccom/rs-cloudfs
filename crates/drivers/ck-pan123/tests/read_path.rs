//! 读路径桩回放测试（Phase 6 / 123-2）——list/stat/mkdir/delete/rename/
//! quota 的协议面行为矩阵（`stub_common` 假 123pan API）。
//!
//! 语义契约（断言即契约，实现者禁改；黄金参照 = 123-0 真机事实 +
//! 任务 A–D/F 规格）：
//!
//! - **list**：`Page` 1 基分页全页拉齐合并 + **驱动内稳定排序**（名字
//!   典序、同名字回退 file_id——服务端只按 file_id 排序）；目录 size
//!   报 0（聚合 Size 不透出）；mtime 双态解析（ISO8601 字符串）；
//! - **stat**：末级新鲜查询（注入的后端错误不被缓存吞掉——M-S1）；
//! - **mkdir**：已存在**预检** → `Exists` 且**不发** upload_request；
//!   隐式父目录逐级创建；
//! - **delete**：trash 四键载荷（`operation:true` **必填**——123-5 真机
//!   钉死：缺省恒 `400 请输入Operation`）+ 回读校验（静默陷阱 → `Io`
//!   不吞）；冷句柄走 info 回读（Trashed 标志）；
//! - **rename**：文件与目录同端点（任务 0 实证）；跨父走 mod_pid
//!   （wire 形态校验）；目标占用 → `Exists`（冷目录现列预检）；后代
//!   目标 → `Invalid`；
//! - **quota**：user/info 的 SpacePermanent/SpaceUsed 映射。

mod stub_common;

use ck_pan123::{Pan123Driver, Pan123Params};
use cloudkit_storage::{Page, PageCursor, RelPath, StorageDriver, StorageError};
use stub_common::ApiStub;

fn path(s: &str) -> RelPath {
    RelPath::new(s.trim_start_matches('/')).expect("valid path")
}

async fn stub() -> ApiStub {
    ApiStub::start().await
}

// ------------------------------------------------------------- list ---

/// 分页合并 + 稳定排序：250 条（> 单页 100，服务端只按 file_id desc
/// 交页）→ 驱动合并后按名字典序稳定输出（conformance ③ 的分页稳定
/// 有序靠这个）。
#[tokio::test]
async fn list_merges_pages_into_name_sorted_output() {
    let s = stub().await;
    s.put_many("0", "f", 250);

    let driver = s.driver();
    let listing = driver
        .list(
            &path("/"),
            Page {
                limit: 500,
                cursor: PageCursor::Start,
            },
        )
        .await
        .expect("list");
    assert_eq!(listing.entries.len(), 250, "all pages merged");
    assert!(listing.next.is_none(), "one sweep took everything");
    let names: Vec<&str> = listing
        .entries
        .iter()
        .map(|e| e.path.as_str().trim_start_matches('/'))
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "driver-side name ordering");

    // Page 切片（内部 offset 游标）。
    let page1 = driver
        .list(
            &path("/"),
            Page {
                limit: 100,
                cursor: PageCursor::Start,
            },
        )
        .await
        .expect("page 1");
    assert_eq!(page1.entries.len(), 100);
    let next = page1.next.expect("more pages");
    let page2 = driver
        .list(
            &path("/"),
            Page {
                limit: 100,
                cursor: next,
            },
        )
        .await
        .expect("page 2");
    assert_eq!(page2.entries.len(), 100);
    assert_ne!(
        page1.entries[99].path.as_str(),
        page2.entries[0].path.as_str(),
        "cursor advances"
    );
}

/// 同名字回退 file_id：桩注入同名不同 fid 两行（真机不会产——钉驱动
/// 排序的确定性）。
#[tokio::test]
async fn same_name_rows_fall_back_to_file_id_order() {
    let s = stub().await;
    s.put_many("0", "a", 2);
    let first = {
        let st = s.state.lock().unwrap();
        st.dirs["0"][0].fid
    };
    s.put_dup_name("0", "a0000"); // 与 a0000 同名、更大 fid

    let driver = s.driver();
    let listing = driver.list(&path("/"), Page::all()).await.expect("list");
    let a_rows: Vec<&cloudkit_storage::Entry> = listing
        .entries
        .iter()
        .filter(|e| e.path.as_str().trim_start_matches('/') == "a0000")
        .collect();
    assert_eq!(a_rows.len(), 2);
    assert_eq!(
        a_rows[0].id.handle.as_str(),
        first.to_string(),
        "the smaller file_id sorts first on a name tie"
    );
    let _ = a_rows[1];
}

/// 目录条目 size 报 0（聚合 Size 不透出——baidu 先例）；mtime 取
/// ISO8601 字符串解析值。
#[tokio::test]
async fn directories_report_zero_size_and_parsed_mtime() {
    let s = stub().await;
    let dir_fid = s.mkdir("0", "sub");
    s.put_file("0", "file.bin", vec![1, 2, 3]);

    let driver = s.driver();
    let listing = driver.list(&path("/"), Page::all()).await.expect("list");
    let dir = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "sub")
        .expect("dir present");
    assert_eq!(dir.kind, cloudkit_storage::EntryKind::Dir);
    assert_eq!(dir.size, 0, "aggregate dir size is NOT surfaced");
    assert_eq!(dir.id.handle.as_str(), dir_fid.to_string());
    assert_eq!(dir.mtime as i64, 1789878015, "ISO8601 +08:00 parsed");
    let file = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "file.bin")
        .expect("file present");
    assert_eq!(file.size, 3);
    assert_eq!(file.kind, cloudkit_storage::EntryKind::File);
}

/// 子目录 list：list-walk 下行（缓存喂给后）。
#[tokio::test]
async fn nested_directory_lists_through_the_walk() {
    let s = stub().await;
    let sub = s.mkdir("0", "sub").to_string();
    s.put_file(&sub, "inner.bin", vec![9; 5]);

    let driver = s.driver();
    let listing = driver.list(&path("/sub"), Page::all()).await.expect("list");
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].path.as_str(), "sub/inner.bin");
    assert_eq!(listing.entries[0].size, 5);
}

// ------------------------------------------------------------- stat ---

/// stat 末级新鲜查询：注入一次性 list 错误——stat 必须把后端错误透出
/// （M-S1：缓存命中会把错误吞掉）。
#[tokio::test]
async fn stat_surfaces_injected_backend_errors_freshly() {
    let s = stub().await;
    s.put_file("0", "a.bin", vec![1]);

    let driver = s.driver();
    // 预热缓存（list 喂缓存——中间层此后可命中）。
    driver.list(&path("/"), Page::all()).await.expect("warm");
    s.state.lock().unwrap().list_error_inject = Some(500);
    let err = driver.stat(&path("a.bin")).await.expect_err("must surface");
    assert!(
        matches!(err, StorageError::Unavailable(ref d) if d.contains("500")),
        "the injected backend error surfaces: {err:?}"
    );
    // 注入被消费后 stat 恢复。
    let entry = driver.stat(&path("a.bin")).await.expect("recovers");
    assert_eq!(entry.path.as_str(), "a.bin");
}

/// stat 根 = 卷根条目（handle = pan123_root 值）；不存在 → NotFound。
#[tokio::test]
async fn stat_root_and_missing_paths() {
    let s = stub().await;
    s.put_file("0", "a.bin", vec![1]);

    let driver = s.driver_with_root("0");
    let root = driver.stat(&path("/")).await.expect("root");
    assert_eq!(root.kind, cloudkit_storage::EntryKind::Dir);
    assert_eq!(root.id.handle.as_str(), "0");

    let err = driver.stat(&path("/nope.bin")).await.expect_err("missing");
    assert_eq!(err, StorageError::NotFound);

    let err = driver
        .stat(&path("/a.bin/deeper"))
        .await
        .expect_err("file mid-path");
    assert_eq!(err, StorageError::NotFound);
}

// ------------------------------------------------------------- mkdir ---

/// mkdir 已存在**预检** → `Exists` 且**零** upload_request（百度教训：
/// 不盲发不产副本垃圾）。
#[tokio::test]
async fn mkdir_preflights_existing_names_without_issuing_create() {
    let s = stub().await;
    s.mkdir("0", "taken");

    let driver = s.driver();
    let err = driver.mkdir(&path("/taken")).await.expect_err("exists");
    assert_eq!(err, StorageError::Exists);
    assert_eq!(
        s.hits("/a/api/file/upload_request"),
        0,
        "the pre-check must refuse before any create call"
    );
    // 文件占位同名同样 Exists。
    s.put_file("0", "file-name", vec![1]);
    let err = driver
        .mkdir(&path("/file-name"))
        .await
        .expect_err("file in the way");
    assert_eq!(err, StorageError::Exists);
}

/// mkdir 隐式父目录逐级创建（/a/b/c 三级一次到位）。
#[tokio::test]
async fn mkdir_creates_implicit_parents_level_by_level() {
    let s = stub().await;
    let driver = s.driver();
    driver
        .mkdir(&path("/a/b/c"))
        .await
        .expect("implicit parents");

    let b = driver.stat(&path("/a/b")).await.expect("a/b exists");
    assert_eq!(b.kind, cloudkit_storage::EntryKind::Dir);
    let c = driver.stat(&path("/a/b/c")).await.expect("a/b/c exists");
    assert_eq!(c.kind, cloudkit_storage::EntryKind::Dir);
    assert_eq!(
        s.hits("/a/api/file/upload_request"),
        3,
        "one create per level"
    );
}

/// mkdir 卷根 → Exists（卷根本就存在）。
#[tokio::test]
async fn mkdir_refuses_the_volume_root() {
    let s = stub().await;
    let driver = s.driver();
    let err = driver.mkdir(&path("/")).await.expect_err("root exists");
    assert_eq!(err, StorageError::Exists);
}

// ----------------------------------------------------------- delete ---

/// delete 正确载荷 + 回读：成功后父 list 不在 + 缓存失效（再 list 不见
/// ghost 行）。
#[tokio::test]
async fn delete_trashes_and_verifies_via_read_back() {
    let s = stub().await;
    let fid = s.put_file("0", "doomed.bin", vec![1, 2]);

    let driver = s.driver();
    let listing = driver.list(&path("/"), Page::all()).await.expect("warm");
    let entry = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "doomed.bin")
        .expect("present");
    driver.delete(&entry.id).await.expect("trash + verify");

    let after = driver.list(&path("/"), Page::all()).await.expect("re-list");
    assert!(
        !after
            .entries
            .iter()
            .any(|e| e.path.as_str() == "doomed.bin"),
        "gone from the listing"
    );
    assert_eq!(s.hits("/a/api/file/trash"), 1);
    let _ = fid;
}

/// trash 静默陷阱（§5.11 建模）：载荷形状不被服务端接受时 code=0 但
/// 不删——驱动的**回读校验**必须抓到（`Io` 不吞）。
#[tokio::test]
async fn delete_catches_the_silent_trash_failure_by_read_back() {
    let s = stub().await;
    s.put_file("0", "trap.bin", vec![1]);
    s.state.lock().unwrap().trash_silent_trap = true;

    let driver = s.driver();
    let listing = driver.list(&path("/"), Page::all()).await.expect("warm");
    let entry = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "trap.bin")
        .expect("present");
    let err = driver.delete(&entry.id).await.expect_err("trap caught");
    match err {
        StorageError::Io(detail) => {
            assert!(
                detail.contains("still listed") || detail.contains("trash"),
                "actionable diagnosis: {detail}"
            );
        }
        other => panic!("the silent failure must surface as Io, got {other:?}"),
    }
}

/// trash 载荷四键形（123-5 真机钉死，2026-09-20）：`operation:true`
/// **必填**——缺省即 `400 请输入Operation`（live_matrix 三个用例的
/// cleanup 恒红揭出；123-0 spike 一直发四键形故真机从未踩到，123-2
/// 实现按跟踪单「两键」摘要抄漏——「桩照实现抄」第三例）。`event`/
/// `driveId` 实测可省（变体 B：code=0），驱动仍发全四键形（语义显式
/// + spike 真机四连绿形态）。
#[tokio::test]
async fn delete_sends_the_operation_key_the_live_server_requires() {
    let s = stub().await;
    let fid = s.put_file("0", "op.bin", vec![1, 2]);

    let driver = s.driver();
    let listing = driver.list(&path("/"), Page::all()).await.expect("warm");
    let entry = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "op.bin")
        .expect("present");
    driver.delete(&entry.id).await.expect("trash + verify");

    let body = s
        .state
        .lock()
        .unwrap()
        .last_trash_body
        .clone()
        .expect("the stub saw the trash body");
    assert_eq!(
        body.get("operation").and_then(|o| o.as_bool()),
        Some(true),
        "operation:true is required by the live server: {body}"
    );
    assert_eq!(
        body.get("event").and_then(|e| e.as_str()),
        Some("intoRecycle"),
        "event stays explicit (spike-verified four-key form): {body}"
    );
    assert_eq!(body.get("driveId").and_then(|d| d.as_i64()), Some(0));
    assert_eq!(
        body.get("fileTrashInfoList")
            .and_then(|l| l.as_array())
            .and_then(|a| a.first())
            .and_then(|it| it.get("FileId"))
            .and_then(|f| f.as_i64()),
        Some(fid),
        "the target fid rides the uppercase FileId key: {body}"
    );
    let _ = fid;
}

/// 冷句柄删除（缓存无父——重启形态）：info 回读（Trashed/查无）。
#[tokio::test]
async fn cold_handle_delete_verifies_via_info() {
    let s = stub().await;
    s.put_file("0", "cold.bin", vec![1]);

    // 全新驱动实例（零缓存）+ 手工铸句柄。
    let driver = s.driver();
    let fid = {
        let st = s.state.lock().unwrap();
        st.dirs["0"]
            .iter()
            .find(|e| e.name == "cold.bin")
            .unwrap()
            .fid
    };
    let id = cloudkit_storage::EntryId::new(
        driver.volume().clone(),
        cloudkit_storage::BackendHandle::new(fid.to_string()),
    );
    driver
        .delete(&id)
        .await
        .expect("cold delete verifies via info");
    assert_eq!(s.hits("/b/api/file/info"), 1);
}

/// 他卷句柄 → NotFound（trait 契约）；垃圾句柄 → **幂等 Ok**（本卷
/// 实体域全是数字 file_id——查无实体的极端形态归入 delete 的声明形态
/// 恒定；123-4 conformance ④ 红灯修复：原 Invalid 破坏「delete 不存在
/// → 幂等 Ok 恒定」的契约）。
#[tokio::test]
async fn delete_rejects_foreign_and_treats_malformed_as_missing() {
    let s = stub().await;
    let driver = s.driver();
    let foreign = cloudkit_storage::EntryId::new(
        cloudkit_storage::VolumeId::new("other", "x").unwrap(),
        cloudkit_storage::BackendHandle::new("1"),
    );
    let err = driver.delete(&foreign).await.expect_err("foreign volume");
    assert_eq!(err, StorageError::NotFound);
    let bad = cloudkit_storage::EntryId::new(
        driver.volume().clone(),
        cloudkit_storage::BackendHandle::new("not-a-number"),
    );
    driver
        .delete(&bad)
        .await
        .expect("malformed handle: the idempotent missing form (constant)");
}

// ----------------------------------------------------------- rename ---

/// rename 文件（同父）与目录（同父——任务 0 实证同端点）。
#[tokio::test]
async fn rename_files_and_directories_in_place() {
    let s = stub().await;
    s.put_file("0", "old.bin", vec![1, 2, 3]);
    s.mkdir("0", "olddir");

    let driver = s.driver();
    driver
        .rename(&path("/old.bin"), &path("/new.bin"))
        .await
        .expect("file rename");
    let entry = driver.stat(&path("/new.bin")).await.expect("new name");
    assert_eq!(entry.size, 3);
    assert!(
        driver.stat(&path("/old.bin")).await.is_err(),
        "old name gone"
    );

    driver
        .rename(&path("/olddir"), &path("/newdir"))
        .await
        .expect("directory rename (task 0: same endpoint)");
    let dir = driver.stat(&path("/newdir")).await.expect("dir renamed");
    assert_eq!(dir.kind, cloudkit_storage::EntryKind::Dir);
}

/// 跨父 rename：mod_pid（wire 形态——fileIdList 大写 FileId + event
/// fileMove）+ 改名腿。
#[tokio::test]
async fn rename_across_parents_moves_then_renames() {
    let s = stub().await;
    let src = s.mkdir("0", "src").to_string();
    let dst = s.mkdir("0", "dst").to_string();
    s.put_file(&src, "movable.bin", vec![7; 9]);

    let driver = s.driver();
    driver
        .rename(&path("/src/movable.bin"), &path("/dst/renamed.bin"))
        .await
        .expect("cross-parent rename");
    assert_eq!(s.hits("/b/api/file/mod_pid"), 1);
    assert_eq!(s.hits("/a/api/file/rename"), 1);
    let entry = driver.stat(&path("/dst/renamed.bin")).await.expect("moved");
    assert_eq!(entry.size, 9);
    assert!(driver.stat(&path("/src/movable.bin")).await.is_err());
    let _ = dst;
}

/// M11：跨父 rename 两步非原子——mod_pid 成功后改名腿失败 → 部分
/// 变更要**可观测**：错误文案明示「文件已移至目标目录、保留旧名」+
/// 保留原始错误文本（不回滚——回滚自身也可能失败）；桩状态实锤
/// 文件已在目标父目录且保持旧名；旋钮解除后在新位置用旧名重试成功。
#[tokio::test]
async fn rename_across_parents_partial_move_failure_is_observable() {
    let s = stub().await;
    let src = s.mkdir("0", "src").to_string();
    let dst = s.mkdir("0", "dst").to_string();
    s.put_file(&src, "movable.bin", vec![7; 9]);
    s.state.lock().unwrap().rename_fail_times = 1; // 改名腿恰一次失败

    let driver = s.driver();
    let err = driver
        .rename(&path("/src/movable.bin"), &path("/dst/renamed.bin"))
        .await
        .expect_err("rename leg fails after mod_pid");
    // 两步各恰一次（mod_pid 成功 + 改名腿终态失败不重试）。
    assert_eq!(s.hits("/b/api/file/mod_pid"), 1);
    assert_eq!(s.hits("/a/api/file/rename"), 1);
    // 错误大类不变（5000 → Rejected → Unavailable）+ 文案：原始错误
    // 文本保留、部分变更事实、旧名与目标目录、重试出路。
    let StorageError::Unavailable(msg) = &err else {
        panic!("expected Unavailable (code=5000), got: {err:?}")
    };
    assert!(msg.contains("pan123 code=5000"), "原始错误文本保留: {msg}");
    assert!(msg.contains("partial move"), "部分变更提示: {msg}");
    assert!(msg.contains("movable.bin"), "旧名入文案: {msg}");
    assert!(msg.contains("dst"), "目标目录入文案: {msg}");

    // 部分变更实锤（桩状态直查）：文件已在目标父目录下、保留旧名，
    // 源父已无此行。
    {
        let st = s.state.lock().unwrap();
        let in_dst = st.dirs.get(&dst).unwrap();
        assert_eq!(in_dst.len(), 1, "exactly the moved file in target");
        assert_eq!(in_dst[0].name, "movable.bin", "old name kept");
        assert!(
            st.dirs.get(&src).unwrap().is_empty(),
            "no longer in source parent"
        );
    }

    // 旋钮已解除：在新位置（目标目录下旧名）resolve 源重试 rename
    // → 成功走到位（驱动不得拿过期缓存把新位置藏掉）。
    driver
        .rename(&path("/dst/movable.bin"), &path("/dst/renamed.bin"))
        .await
        .expect("retry from the new location with the old name");
    let entry = driver
        .stat(&path("/dst/renamed.bin"))
        .await
        .expect("fully moved and renamed now");
    assert_eq!(entry.size, 9);
}

/// 对照（M11）：同父 rename 失败腿——无部分变更面，错误原样上抛
/// （无 partial-move 提示），文件原地未动。
#[tokio::test]
async fn rename_in_place_failure_has_no_partial_move_hint() {
    let s = stub().await;
    s.put_file("0", "old.bin", vec![1, 2, 3]);
    s.state.lock().unwrap().rename_fail_times = 1;

    let driver = s.driver();
    let err = driver
        .rename(&path("/old.bin"), &path("/new.bin"))
        .await
        .expect_err("in-place rename fails");
    assert_eq!(s.hits("/b/api/file/mod_pid"), 0, "no move involved");
    let StorageError::Unavailable(msg) = &err else {
        panic!("expected Unavailable, got: {err:?}")
    };
    assert!(msg.contains("pan123 code=5000"), "原始错误原样: {msg}");
    assert!(!msg.contains("partial move"), "无部分变更提示: {msg}");
    // 桩状态：文件仍在原父目录、旧名未动、新名未落。
    let st = s.state.lock().unwrap();
    let root = st.dirs.get("0").unwrap();
    assert!(root.iter().any(|e| e.name == "old.bin"), "untouched");
    assert!(root.iter().all(|e| e.name != "new.bin"), "no rename landed");
}

/// 目标占用 → Exists（冷目标目录现列预检——M-S4 同源）；后代目标 →
/// Invalid；根参与 → Invalid。
#[tokio::test]
async fn rename_guards_occupied_descendant_and_root_targets() {
    let s = stub().await;
    s.put_file("0", "a.bin", vec![1]);
    s.put_file("0", "b.bin", vec![2]);

    let driver = s.driver();
    let err = driver
        .rename(&path("a.bin"), &path("/b.bin"))
        .await
        .expect_err("occupied");
    assert_eq!(err, StorageError::Exists);

    let err = driver
        .rename(&path("a.bin"), &path("/a.bin/inner"))
        .await
        .expect_err("descendant");
    assert_eq!(err, StorageError::Invalid);

    let err = driver
        .rename(&path("/"), &path("/x"))
        .await
        .expect_err("root source");
    assert_eq!(err, StorageError::Invalid);
    let err = driver
        .rename(&path("a.bin"), &path("/"))
        .await
        .expect_err("root target");
    assert_eq!(err, StorageError::Invalid);

    let err = driver
        .rename(&path("/missing.bin"), &path("/c.bin"))
        .await
        .expect_err("missing source");
    assert_eq!(err, StorageError::NotFound);
}

// ------------------------------------------------------------ quota ---

/// quota：user/info 的 SpacePermanent/SpaceUsed 映射。
#[tokio::test]
async fn quota_maps_the_user_info_space_fields() {
    let s = stub().await;
    let driver = s.driver();
    let quota = driver.quota().await.expect("quota");
    assert_eq!(quota.total, Some(2199023255552));
    assert_eq!(quota.used, 1073741824);
}

/// 自定义卷根（D3）：root 指向子目录——list/stat 以它为根。
#[tokio::test]
async fn custom_root_scopes_the_volume() {
    let s = stub().await;
    let sub = s.mkdir("0", "scoped").to_string();
    s.put_file(&sub, "inside.bin", vec![4, 4]);

    let driver = s.driver_with_root(&sub);
    let listing = driver.list(&path("/"), Page::all()).await.expect("list");
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].path.as_str(), "inside.bin");
    let root = driver.stat(&path("/")).await.expect("root");
    assert_eq!(root.id.handle.as_str(), sub);
}

/// Pan123Params 的限流/重试注入面（RebuildTuning 形态——参数结构体
/// 透传到 client）。
#[tokio::test]
async fn params_carry_limiter_and_retry_tuning() {
    let s = stub().await;
    let params = Pan123Params {
        token: Some("stub-token-0123456789".to_string()),
        api_base: s.base.clone(),
        fallback_base: s.base.clone(),
        limiter: Some(ck_pan123::limiter::LimiterConfig::fast()),
        retry: Some(ck_pan123::api::RetryConfig::fast()),
        ..Pan123Params::default()
    };
    let driver = Pan123Driver::new(params).expect("driver");
    let quota = driver.quota().await.expect("quota");
    assert_eq!(quota.used, 1073741824);
}

// --------------------------------------- P3（K79）cid 不变式防御 ---
//
// 生产不可达性声明：cid 全部由 `params.root`（`from_pairs` 配置校验
// 纯数字）或 `file_id.to_string()`（i64）铸出——三条链在类型系统 +
// 配置门下解析失败不可达。但 [`Pan123Params`] 是直接可构造的 pub
// 结构体（绕过 `from_pairs` 的第二道门即得非数字 root），旧形态
// `unwrap_or(0)` 会把破损**静默归到网盘根（0）**——错向账号根做
// mkdir/move 是数据破坏面。P3 = 防御性显式报错（Invalid + error 通道
// 保留原值）；下列用例以「绕过校验的 root」驱动该防御臂。

/// P3：mkdir 链（lib.rs mkdir 的 `cid.parse()`）——非数字卷根下建目录
/// 不得静默落进网盘根，显式 Invalid。
#[tokio::test]
async fn mkdir_with_a_non_numeric_root_fails_instead_of_targeting_the_netdisk_root() {
    let s = stub().await;
    let driver = s.driver_with_root("junk"); // 绕过 from_pairs 校验的形态

    let err = driver
        .mkdir(&path("d"))
        .await
        .expect_err("a non-numeric root must not silently become 0");
    assert!(matches!(err, StorageError::Invalid), "{err:?}");
    // 桩状态实锤：网盘根（"0"）零写入。
    let st = s.state.lock().unwrap();
    assert!(
        st.dirs.get("0").unwrap().iter().all(|e| e.name != "d"),
        "nothing landed in the netdisk root"
    );
}

/// P3：rename 的 mod_pid 臂（跨父移动的目标父 cid）——目标父是卷根
/// 且卷根非数字时，显式 Invalid 而非移动进网盘根。
#[tokio::test]
async fn rename_mod_pid_with_a_non_numeric_root_fails_instead_of_moving_to_root() {
    let s = stub().await;
    let sub = s.mkdir("0", "src").to_string();
    s.put_file(&sub, "movable.bin", vec![7; 4]);
    let driver = s.driver_with_root("junk");

    // 源在数字 cid 子目录、目标父 = 卷根（"junk"）→ 跨父 → mod_pid 臂。
    let err = driver
        .rename(&path("src/movable.bin"), &path("renamed.bin"))
        .await
        .expect_err("a non-numeric target parent must not silently become 0");
    assert!(matches!(err, StorageError::Invalid), "{err:?}");
    // 桩状态实锤：文件仍在源父、网盘根无新行。
    let st = s.state.lock().unwrap();
    assert!(
        st.dirs
            .get(&sub)
            .unwrap()
            .iter()
            .any(|e| e.name == "movable.bin"),
        "the file stays in its source parent"
    );
    assert!(
        st.dirs
            .get("0")
            .unwrap()
            .iter()
            .all(|e| e.name != "renamed.bin" && e.name != "movable.bin"),
        "nothing moved into the netdisk root"
    );
}
