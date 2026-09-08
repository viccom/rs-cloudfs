//! 元数据面表单**字节级断言**（Batch B1 红③；K16）。
//!
//! 断言收到的 query/form 恰为黄金参照（解析为参数集后做**精确集合断言**：
//! 无多余、无缺失、无重复；JSON 载荷做结构等值断言）。逐操作注源：
//! spike `examples/baidu_spike/src/api.rs` + PCFS `drivers/baidu`（分歧以
//! spike 为准）。

mod common;

use std::sync::Arc;

use ck_baidu::factory;
use cloudkit_storage::{EntryKind, Page, RelPath, StorageDriver};
use serde_json::{json, Value};

use common::{
    assert_exact_pairs, assert_form_encoded, filter_recorded, parse_urlencoded, MockBaidu,
    INITIAL_ACCESS_TOKEN, MOCK_ROOT, MOCK_UID, NETDISK_UA, XPAN_FILE, XPAN_NAS,
};

/// 种子根目录 + 构造驱动（测试各自按需追加播种）。
async fn setup() -> (MockBaidu, Arc<ck_baidu::BaiduDriver>) {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let driver = factory(&mock.params(None))
        .await
        .expect("baidu driver connect（uinfo 取 uid）");
    (mock, driver)
}

/// 解析某条请求记录的 query 参数集。
fn query_pairs(req: &common::RecordedRequest) -> Vec<(String, String)> {
    parse_urlencoded(&req.query)
}

#[tokio::test]
async fn list_sends_exact_golden_query() {
    let (mock, driver) = setup().await;
    mock.seed_file(&format!("{MOCK_ROOT}/b.txt"), 1, 1757311000);

    driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list 根目录");

    let recorded = mock.recorded();
    let reqs = filter_recorded(&recorded, "GET", XPAN_FILE, &["method=list"]);
    assert_eq!(reqs.len(), 1, "list 恰一次后端调用");
    // 黄金参照（spike api.rs:131-148 + PCFS api.go:49-52）：恰三参数——
    // **无分页参数**（web/num 等一律不发，driver 内 offset 游标切 Page）。
    assert_exact_pairs(
        &query_pairs(reqs[0]),
        &[
            ("method", "list"),
            ("dir", MOCK_ROOT),
            ("access_token", INITIAL_ACCESS_TOKEN),
        ],
    );
    // UA 恒为 netdisk 族（spike common.rs:16；client 构造照抄的防漂移钉）。
    assert_eq!(
        reqs[0].user_agent.as_deref(),
        Some(NETDISK_UA),
        "netdisk 族 UA 是后端行为契约（spike §5）"
    );
}

#[tokio::test]
async fn mkdir_posts_exact_create_form() {
    let (mock, driver) = setup().await;

    driver
        .mkdir(&RelPath::new("x").expect("rel path"))
        .await
        .expect("mkdir");

    let recorded = mock.recorded();
    let reqs = filter_recorded(&recorded, "POST", XPAN_FILE, &["method=create"]);
    assert_eq!(reqs.len(), 1, "create 恰一次后端调用");
    assert_exact_pairs(
        &query_pairs(reqs[0]),
        &[("method", "create"), ("access_token", INITIAL_ACCESS_TOKEN)],
    );
    assert_form_encoded(reqs[0]);
    // 黄金参照（PCFS api.go:708-735）：body 恰 path + isdir=1 两字段。
    assert_exact_pairs(
        &parse_urlencoded(&reqs[0].body),
        &[("path", &format!("{MOCK_ROOT}/x")), ("isdir", "1")],
    );
}

#[tokio::test]
async fn delete_posts_exact_filemanager_delete_form() {
    let (mock, driver) = setup().await;
    mock.seed_file(&format!("{MOCK_ROOT}/f.txt"), 10, 1757311000);

    // 句柄来自驱动自身的 list 产出（fs_id 十进制字符串，K5）。
    let listing = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list 取句柄");
    let entry = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "f.txt")
        .expect("种子条目可见");
    driver.delete(&entry.id).await.expect("delete");

    let recorded = mock.recorded();
    let reqs = filter_recorded(&recorded, "POST", XPAN_FILE, &["opera=delete"]);
    assert_eq!(reqs.len(), 1, "filemanager delete 恰一次后端调用");
    assert_exact_pairs(
        &query_pairs(reqs[0]),
        &[
            ("method", "filemanager"),
            ("opera", "delete"),
            ("access_token", INITIAL_ACCESS_TOKEN),
        ],
    );
    assert_form_encoded(reqs[0]);
    // 黄金参照（spike api.rs:469-474 + PCFS api.go:743-754）：
    // form 恰一个 filelist 字段，JSON 恰 [{"path": <abs>}]。
    let form = parse_urlencoded(&reqs[0].body);
    assert_eq!(form.len(), 1, "delete form 恰一个字段：{form:?}");
    let filelist = form
        .iter()
        .find(|(k, _)| k == "filelist")
        .map(|(_, v)| v)
        .expect("filelist 字段存在");
    let parsed: Value = serde_json::from_str(filelist).expect("filelist 是合法 JSON");
    assert_eq!(
        parsed,
        json!([{ "path": format!("{MOCK_ROOT}/f.txt") }]),
        "filelist 恰 [{{path}}] 形态（数组单元素、单键对象）"
    );
}

#[tokio::test]
async fn rename_posts_exact_filemanager_move_form() {
    let (mock, driver) = setup().await;
    mock.seed_file(&format!("{MOCK_ROOT}/a.txt"), 10, 1757311000);

    driver
        .rename(
            &RelPath::new("a.txt").expect("rel path"),
            &RelPath::new("b.txt").expect("rel path"),
        )
        .await
        .expect("rename");

    let recorded = mock.recorded();
    let reqs = filter_recorded(&recorded, "POST", XPAN_FILE, &["opera=move"]);
    assert_eq!(reqs.len(), 1, "filemanager move 恰一次后端调用");
    assert_exact_pairs(
        &query_pairs(reqs[0]),
        &[
            ("method", "filemanager"),
            ("opera", "move"),
            ("access_token", INITIAL_ACCESS_TOKEN),
        ],
    );
    assert_form_encoded(reqs[0]);
    // 黄金参照（PCFS api.go:829-845 实测形态）：form 恰 async=1 + filelist；
    // filelist JSON 恰 [{path, dest, newname, ondup}] 四键——**不是 from/to
    // 形态**（任务情报中的猜测已按 PCFS 源码否定）。
    let form = parse_urlencoded(&reqs[0].body);
    assert_eq!(
        form.len(),
        2,
        "move form 恰 async + filelist 两字段：{form:?}"
    );
    assert!(
        form.iter().any(|(k, v)| k == "async" && v == "1"),
        "async=1（PCFS api.go:844）：{form:?}"
    );
    let filelist = form
        .iter()
        .find(|(k, _)| k == "filelist")
        .map(|(_, v)| v)
        .expect("filelist 字段存在");
    let parsed: Value = serde_json::from_str(filelist).expect("filelist 是合法 JSON");
    assert_eq!(
        parsed,
        json!([{
            "path": format!("{MOCK_ROOT}/a.txt"),
            "dest": MOCK_ROOT,
            "newname": "b.txt",
            "ondup": "overwrite",
        }]),
        "filelist 恰四键 path/dest/newname/ondup（PCFS api.go:829-836）"
    );
}

#[tokio::test]
async fn stat_sends_meta_with_path_param() {
    let (mock, driver) = setup().await;
    mock.seed_file(&format!("{MOCK_ROOT}/b.txt"), 1, 1757311000);

    driver
        .stat(&RelPath::new("b.txt").expect("rel path"))
        .await
        .expect("stat");

    let recorded = mock.recorded();
    let reqs = filter_recorded(&recorded, "GET", XPAN_FILE, &["method=meta"]);
    assert_eq!(reqs.len(), 1, "meta 恰一次后端调用");
    // 黄金参照（PCFS api.go:110-113 权威核实）：**path 参数**（非 filelist、
    // 非 fs_ids——那是按句柄解析的姊妹形态 api.go:176-179）。
    assert_exact_pairs(
        &query_pairs(reqs[0]),
        &[
            ("method", "meta"),
            ("path", &format!("{MOCK_ROOT}/b.txt")),
            ("access_token", INITIAL_ACCESS_TOKEN),
        ],
    );
}

#[tokio::test]
async fn quota_passes_backend_values_with_exact_query() {
    let (mock, driver) = setup().await;
    mock.set_quota(123456789, 1099511627776);

    let quota = driver.quota().await.expect("quota");

    assert_eq!(quota.total, Some(1099511627776), "后端 total 原样传递");
    assert_eq!(quota.used, 123456789, "后端 used 原样传递");
    let recorded = mock.recorded();
    let reqs = filter_recorded(&recorded, "GET", XPAN_FILE, &["method=quota"]);
    assert_eq!(reqs.len(), 1, "quota 恰一次后端调用");
    // 黄金参照（PCFS api.go:914-933）：quota 拼在 xpan/file 上，恰两参数。
    assert_exact_pairs(
        &query_pairs(reqs[0]),
        &[("method", "quota"), ("access_token", INITIAL_ACCESS_TOKEN)],
    );
}

#[tokio::test]
async fn volume_id_is_baidu_uid_from_uinfo() {
    let (mock, driver) = setup().await;

    assert_eq!(
        driver.volume().as_str(),
        format!("baidu:{MOCK_UID}"),
        "VolumeId = baidu:<uid>（K5；uinfo 取 uid）"
    );
    // uinfo 在 /xpan/nas 不在 /xpan/file（PCFS baiduauth/config.go:370-373）。
    let recorded = mock.recorded();
    let reqs = filter_recorded(&recorded, "GET", XPAN_NAS, &[]);
    assert_eq!(reqs.len(), 1, "构造期恰一次 uinfo");
    assert_eq!(reqs[0].path, XPAN_NAS);
    assert_exact_pairs(
        &query_pairs(reqs[0]),
        &[("method", "uinfo"), ("access_token", INITIAL_ACCESS_TOKEN)],
    );
}

#[tokio::test]
async fn list_parses_entries_sorted_with_root_stripped_and_server_mtime() {
    let (mock, driver) = setup().await;
    // 播种顺序刻意逆字典序（sub 先于 b.txt）：后端返回序是种子序，
    // 驱动必须输出 RelPath 字典序（trait「稳定有序」契约）。
    mock.seed_dir(&format!("{MOCK_ROOT}/sub"));
    let fs_id = mock.seed_file(&format!("{MOCK_ROOT}/b.txt"), 12345, 1757311000);

    let listing = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list");
    assert_eq!(listing.next, None, "Page::all 一次取完 → 无续读游标");
    let paths: Vec<&str> = listing.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, vec!["b.txt", "sub"], "字典序稳定排序");

    let b = &listing.entries[0];
    assert_eq!(b.kind, EntryKind::File);
    assert_eq!(b.size, 12345);
    assert_eq!(
        b.mtime, 1757311000.0,
        "mtime 读 server_mtime（PCFS api.go:85-87；两源均无 local_mtime）"
    );
    assert_eq!(
        b.id.handle.as_str(),
        fs_id.to_string(),
        "句柄 = fs_id 十进制字符串（K5）"
    );
    assert_eq!(
        b.id.volume.as_str(),
        format!("baidu:{MOCK_UID}"),
        "条目携带卷身份"
    );

    let sub = &listing.entries[1];
    assert_eq!(sub.kind, EntryKind::Dir);
    assert_eq!(sub.path.as_str(), "sub");
    assert_eq!(sub.size, 0, "目录 size 不透出后端实现值");

    // stat 同一解析面：字段一致。
    let s = driver
        .stat(&RelPath::new("b.txt").expect("rel path"))
        .await
        .expect("stat");
    assert_eq!(s.path.as_str(), "b.txt", "root 前缀剥离为 RelPath 相对形态");
    assert_eq!(s.kind, EntryKind::File);
    assert_eq!(s.size, 12345);
    assert_eq!(s.mtime, 1757311000.0);
    assert_eq!(s.id.handle.as_str(), fs_id.to_string());
}
