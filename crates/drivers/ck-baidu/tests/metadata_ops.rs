//! 元数据面表单**字节级断言**（Batch B1 红③；K16）。
//!
//! 断言收到的 query/form 恰为黄金参照（解析为参数集后做**精确集合断言**：
//! 无多余、无缺失、无重复；JSON 载荷做结构等值断言）。逐操作注源：
//! spike `examples/baidu_spike/src/api.rs` + PCFS `drivers/baidu`（分歧以
//! spike 为准）。

mod common;

use std::sync::Arc;

use ck_baidu::factory;
use cloudkit_storage::{
    BackendHandle, EntryId, EntryKind, Page, RelPath, StorageDriver, StorageError,
};
use serde_json::{json, Value};

use common::{
    assert_exact_pairs, assert_form_encoded, filter_recorded, parse_urlencoded, pattern_bytes,
    MockBaidu, INITIAL_ACCESS_TOKEN, MOCK_ROOT, MOCK_UID, NETDISK_UA, XPAN_FILE, XPAN_NAS,
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
    assert_eq!(reqs.len(), 1, "list 恰一次后端调用（单页目录不起续页）");
    // 黄金参照（H1 真机定谳 2026-09-25：无参默认页 1000 静默截断——
    // 成对 start+limit 窗口方言逐页拉齐，不发 num/order/web）。
    assert_exact_pairs(
        &query_pairs(reqs[0]),
        &[
            ("method", "list"),
            ("dir", MOCK_ROOT),
            ("start", "0"),
            ("limit", "1000"),
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

/// 真网 31300 实证驱动（2026-09-08 第四轮返工，原
/// `stat_sends_meta_with_path_param` 改造）：干净探针实证此第三方 appkey
/// 下 `method=meta` 全废——meta&path → error_code=31300 "stream type is not
/// authorized"（持续无权限非延迟，轮询 10s 不可见）；meta&fs_ids → 31023
/// param error。stat 改「list 父目录 + path 精确匹配」（list 全程即时可用
/// ——探针多次实证）。黄金参照从 `method=meta&path=…` 改为
/// `method=list&dir=<父目录>`。
#[tokio::test]
async fn stat_lists_parent_dir_with_exact_query_and_never_calls_meta() {
    let (mock, driver) = setup().await;
    mock.seed_file(&format!("{MOCK_ROOT}/b.txt"), 1, 1757311000);

    driver
        .stat(&RelPath::new("b.txt").expect("rel path"))
        .await
        .expect("stat");

    let recorded = mock.recorded();
    // 驱动已不调用 meta（31300 停用——decisions 2026-09-08；正式 appkey
    // 复测候选，mock meta 臂保留为无消费者路由）。
    let metas = filter_recorded(&recorded, "GET", XPAN_FILE, &["method=meta"]);
    assert!(
        metas.is_empty(),
        "stat 不得调用 meta（31300 全废）：{metas:?}"
    );
    let reqs = filter_recorded(&recorded, "GET", XPAN_FILE, &["method=list"]);
    assert_eq!(reqs.len(), 1, "stat 恰一次父目录 list 调用");
    // 黄金参照（真网探针 2026-09-08）：恰三参数，dir = 目标父目录。
    assert_exact_pairs(
        &query_pairs(reqs[0]),
        &[
            ("method", "list"),
            ("dir", MOCK_ROOT),
            ("start", "0"),
            ("limit", "1000"),
            ("access_token", INITIAL_ACCESS_TOKEN),
        ],
    );
}

/// 真网实证 #3 驱动（filemanager delete 的 fs_id 形态 → errno=12 不删，
/// 只支持 path 形态）+ #1/#2（meta 全废）：delete 句柄解析两级——句柄缓存
/// （list/stat/Entry 流量填充）→ 未命中递归 list 扫描。本测试钉**冷句柄**
/// 形态：driver 构造后零 list/stat 流量（缓存空），delete 仍经卷根递归
/// 扫描定位 fs_id → path 成功删除。
#[tokio::test]
async fn delete_cold_handle_resolves_via_recursive_scan() {
    let (mock, driver) = setup().await;
    mock.seed_dir(&format!("{MOCK_ROOT}/sub"));
    let fs_id = mock.seed_file(&format!("{MOCK_ROOT}/sub/cold.txt"), 10, 1757311000);

    // 冷句柄：直接构造 EntryId（不经 list/stat——缓存必然空）。
    let id = EntryId::new(
        driver.volume().clone(),
        BackendHandle::new(fs_id.to_string()),
    );
    driver
        .delete(&id)
        .await
        .expect("冷句柄 delete 经递归扫描解析");

    // 扫描流量形态：从卷根起的深度优先目录列举（根 + sub 两级，恰两次）。
    let recorded = mock.recorded();
    let lists = filter_recorded(&recorded, "GET", XPAN_FILE, &["method=list"]);
    assert_eq!(lists.len(), 2, "递归扫描 = 卷根 + sub 两级 list：{lists:?}");
    // 文件确实删除（深一层路径也删得掉——扫描穿透目录层级）。
    let err = driver
        .stat(&RelPath::new("sub/cold.txt").expect("rel path"))
        .await
        .expect_err("删除后必须 NotFound");
    assert_eq!(err, StorageError::NotFound);
}

/// 真网 31300 实证驱动的缓存面：list 流量批量填充句柄缓存（一次 list 一次
/// 锁），此后 delete 命中缓存——**零递归扫描**（复审 M13 后缓存命中
/// 增加恰一次父目录 list 做「路径 + fs_id」双核对——删错对象不可逆，
/// 这一次核对是数据安全级的固定代价；无 meta）。
#[tokio::test]
async fn delete_warm_handle_hits_cache_without_resolution_traffic() {
    let (mock, driver) = setup().await;
    mock.seed_file(&format!("{MOCK_ROOT}/f.txt"), 10, 1757311000);

    let listing = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list 取句柄（批量填充缓存）");
    let entry = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "f.txt")
        .expect("种子条目可见");
    let id = entry.id.clone();
    let lists_before = filter_recorded(&mock.recorded(), "GET", XPAN_FILE, &["method=list"]).len();

    driver.delete(&id).await.expect("delete（缓存命中）");

    let lists_after = filter_recorded(&mock.recorded(), "GET", XPAN_FILE, &["method=list"]).len();
    assert_eq!(
        lists_before + 1,
        lists_after,
        "缓存命中 delete 恰一次核对 list（M13 双核对），零递归扫描"
    );
    let recorded = mock.recorded();
    let metas = filter_recorded(&recorded, "GET", XPAN_FILE, &["method=meta"]);
    assert!(
        metas.is_empty(),
        "delete 不得调用 meta（31300 全废）：{metas:?}"
    );
    let err = driver
        .stat(&RelPath::new("f.txt").expect("rel path"))
        .await
        .expect_err("删除后必须 NotFound");
    assert_eq!(err, StorageError::NotFound);
}

/// 缓存陈旧纠偏（decisions 2026-09-08 连带项）：rename 后缓存条目仍指向
/// 旧路径（move 是后端单侧搬移，驱动缓存不知情）→ filemanager delete 撞
/// -9 → 缓存条目失效 + 递归扫描重解析 + 重试一次——防陈旧缓存删错/删空。
#[tokio::test]
async fn delete_stale_cache_path_corrects_via_rescan_and_retry() {
    let (mock, driver) = setup().await;
    mock.seed_file(&format!("{MOCK_ROOT}/a.txt"), 10, 1757311000);

    // list 填充缓存（fs_id → /apps/cloudfs/a.txt）。
    let listing = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list");
    let id = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "a.txt")
        .expect("条目可见")
        .id
        .clone();

    // rename（a.txt → b.txt）：缓存条目自此陈旧。
    driver
        .rename(
            &RelPath::new("a.txt").expect("rel path"),
            &RelPath::new("b.txt").expect("rel path"),
        )
        .await
        .expect("rename");

    // delete 旧句柄：缓存陈旧路径 → -9 → 纠偏（失效+重扫+重试）→ 成功。
    driver
        .delete(&id)
        .await
        .expect("陈旧缓存经纠偏后 delete 成功");

    // 删的是新路径上的同一对象（fs_id 稳定，K5）——不是删了个寂寞。
    let err = driver
        .stat(&RelPath::new("b.txt").expect("rel path"))
        .await
        .expect_err("新路径必须 NotFound（对象确已删除）");
    assert_eq!(err, StorageError::NotFound);
}

/// 复审 M13（2026-09-25）：陈旧缓存 × **路径被外部复用**——旧路径上
/// 站着另一个对象（filemanager delete 按路径删 → 删错对象且「成功」，
/// -9 纠偏腿覆盖不到：路径存在、删除不失败）。修复：缓存命中先列父
/// 目录按「路径 + fs_id」双核对；不吻合 → 失效 + 扫描真路径（扫描
/// 出自实时列举，天然吻合）。
#[tokio::test]
async fn delete_stale_cache_with_a_reused_path_deletes_the_right_object() {
    let (mock, driver) = setup().await;
    mock.seed_file(&format!("{MOCK_ROOT}/a.txt"), 10, 1757311000);

    // list 填充缓存（fs_id → /…/a.txt）。
    let listing = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list");
    let id = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "a.txt")
        .expect("条目可见")
        .id
        .clone();

    // 外部改动（不经驱动——驱动缓存自此陈旧）：a.txt 搬到 b.txt；
    // **另一个对象**落到 a.txt 原位。
    mock.rename_path(&format!("{MOCK_ROOT}/a.txt"), &format!("{MOCK_ROOT}/b.txt"));
    mock.seed_file(&format!("{MOCK_ROOT}/a.txt"), 20, 1757312000);

    // delete 旧句柄：必须删 b.txt（该 fs_id 的真身），a.txt（新对象）不动。
    driver
        .delete(&id)
        .await
        .expect("delete resolves the true object of the handle");

    let err = driver
        .stat(&RelPath::new("b.txt").expect("rel path"))
        .await
        .expect_err("真身对象确已删除");
    assert_eq!(err, StorageError::NotFound);
    driver
        .stat(&RelPath::new("a.txt").expect("rel path"))
        .await
        .expect("复用路径上的新对象必须幸存（删错它 = 数据丢失级）");
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

// ----------------------------- 审查 M2（2026-09-25）：rename × 句柄缓存 -----------------

async fn read_all(mut stream: cloudkit_storage::ByteStream) -> Vec<u8> {
    use futures_util::StreamExt;
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => out.extend_from_slice(&bytes),
            Err(e) => panic!("读流中途错误: {e}"),
        }
    }
    out
}

fn list_calls(mock: &MockBaidu) -> usize {
    filter_recorded(&mock.recorded(), "GET", XPAN_FILE, &["method=list"]).len()
}

/// 文件腿：rename 后持旧句柄读必须读通，且**零额外解析流量**——修复前
/// rename 不改写句柄缓存，reader 拿陈旧 path 签发 dlink 撞 -9 → NotFound
/// 无自愈（审查 M2 红①）；修复选「改写」而非「失效」：命中缓存直达
/// fetch_dlink(新路径)，无重扫 list（钉住设计选择）。
#[tokio::test]
async fn rename_rewrites_cached_paths_so_reads_follow_the_object() {
    let (mock, driver) = setup().await;
    let content = pattern_bytes(1000);
    mock.seed_file_bytes(&format!("{MOCK_ROOT}/old.bin"), &content, 1757311000);

    let entry = driver
        .stat(&RelPath::new("old.bin").expect("rel"))
        .await
        .expect("stat 播种（缓存批量填充）");
    driver
        .rename(
            &RelPath::new("old.bin").expect("rel"),
            &RelPath::new("moved.bin").expect("rel"),
        )
        .await
        .expect("rename");

    let before = list_calls(&mock);
    let stream = driver
        .reader(&entry.id, None)
        .await
        .expect("rename 后持旧句柄读通（缓存路径已随搬移改写）");
    assert_eq!(read_all(stream).await, content, "读到的是同一对象的内容");
    assert_eq!(
        list_calls(&mock),
        before,
        "零额外解析流量——缓存条目被改写而非失效"
    );
}

/// 目录腿：rename 目录后，子条目旧句柄读通（缓存子树前缀整体改写）。
#[tokio::test]
async fn dir_rename_rewrites_the_cached_subtree_for_reads() {
    let (mock, driver) = setup().await;
    mock.seed_dir(&format!("{MOCK_ROOT}/d"));
    let content = pattern_bytes(777);
    mock.seed_file_bytes(&format!("{MOCK_ROOT}/d/kid.bin"), &content, 1757311000);

    let entry = driver
        .stat(&RelPath::new("d/kid.bin").expect("rel"))
        .await
        .expect("stat 播种");
    driver
        .rename(
            &RelPath::new("d").expect("rel"),
            &RelPath::new("d2").expect("rel"),
        )
        .await
        .expect("rename 目录");

    let stream = driver
        .reader(&entry.id, None)
        .await
        .expect("目录搬移后子条目旧句柄读通");
    assert_eq!(read_all(stream).await, content);
    driver
        .stat(&RelPath::new("d2/kid.bin").expect("rel"))
        .await
        .expect("新路径可见");
}

/// 纠偏腿（外部搬移场景）：绕过驱动的后端搬移让缓存陈旧——reader 必须像
/// delete 一样自愈（失效 + 重扫 + 重试恰一次），而不是 NotFound 到进程
/// 重启（审查 M2 红②；mock.rename_path = 外部竞态窗的桩面）。
#[tokio::test]
async fn reader_stale_cache_path_corrects_via_rescan_and_retry() {
    let (mock, driver) = setup().await;
    let content = pattern_bytes(500);
    mock.seed_file_bytes(&format!("{MOCK_ROOT}/a.txt"), &content, 1757311000);

    let entry = driver
        .stat(&RelPath::new("a.txt").expect("rel"))
        .await
        .expect("stat 播种（缓存填充）");
    // 后端侧搬移（驱动不知情）——缓存条目自此陈旧。
    mock.rename_path(&format!("{MOCK_ROOT}/a.txt"), &format!("{MOCK_ROOT}/b.txt"));

    let stream = driver
        .reader(&entry.id, None)
        .await
        .expect("陈旧缓存经纠偏（失效+重扫+重试）后读通");
    assert_eq!(read_all(stream).await, content);
}

/// delete 纠偏腿的外部搬移形态（既有测试走 driver.rename——M2 修复后缓存
/// 即时改写、不再撞 -9；本腿用外部搬移保住纠偏语义的钉测）。
#[tokio::test]
async fn delete_stale_cache_path_corrects_after_external_move() {
    let (mock, driver) = setup().await;
    mock.seed_file(&format!("{MOCK_ROOT}/x.txt"), 10, 1757311000);
    let listing = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list");
    let id = listing
        .entries
        .iter()
        .find(|e| e.path.as_str() == "x.txt")
        .expect("条目可见")
        .id
        .clone();
    mock.rename_path(&format!("{MOCK_ROOT}/x.txt"), &format!("{MOCK_ROOT}/y.txt"));

    driver.delete(&id).await.expect("陈旧缓存经纠偏后删除成功");
    let err = driver
        .stat(&RelPath::new("y.txt").expect("rel"))
        .await
        .expect_err("新路径必须 NotFound");
    assert_eq!(err, StorageError::NotFound);
}

// ----------------------- H1（2026-09-25）：list 分页截断与拉齐 -----------------

/// H1 真机定谳后的桩真形钉测：无参 list 默认页 1000 静默截断（探针实测
/// 1010 播种裸查恰 1000）。驱动必须按「start+limit 成对窗口」方言逐页
/// 拉齐。红：修复前 api::list 单次调用 → 驱动只见 1000。
#[tokio::test]
async fn list_pulls_all_pages_beyond_the_backend_default_page() {
    let (mock, driver) = setup().await;
    for i in 0..1005u32 {
        mock.seed_file(&format!("{MOCK_ROOT}/p{i:04}.bin"), 1, 1757311000);
    }

    let listing = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list");

    assert_eq!(
        listing.entries.len(),
        1005,
        "全量可见——分页拉齐修复前静默截断在 1000"
    );
}

/// 分页方言钉测：驱动发出的 list 请求必带成对 start/limit（首页 0/1000，
/// 续页按页长推进）——防止回退到「裸 list」形态（后端默认页截断的根源）。
#[tokio::test]
async fn list_requests_carry_paired_start_limit_windows() {
    let (mock, driver) = setup().await;
    for i in 0..1005u32 {
        mock.seed_file(&format!("{MOCK_ROOT}/w{i:04}.bin"), 1, 1757311000);
    }
    let _ = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list");

    let recorded = mock.recorded();
    let reqs = filter_recorded(&recorded, "GET", XPAN_FILE, &["method=list"]);
    assert!(reqs.len() >= 2, "两页拉齐：{reqs:?}");
    let first = query_pairs(reqs[0]);
    assert!(
        first.contains(&("start".to_string(), "0".to_string()))
            && first.contains(&("limit".to_string(), "1000".to_string())),
        "首页窗口 start=0&limit=1000：{first:?}"
    );
    let second = query_pairs(reqs[1]);
    assert!(
        second.contains(&("start".to_string(), "1000".to_string())),
        "续页 start 按页长推进：{second:?}"
    );
}
