//! 读路径桩回放（Phase 5 / 115-2）：假开放平台 HTTP 服务端下的
//! list/stat/mkdir/delete/rename/quota 行为矩阵 + downurl/CDN 流读。
//!
//! 桩形态（115-4 conformance 桩的先导）：一个内存 VFS（cid 树 + 条目
//! 表 + pick_code 分配）+ axum loopback 路由，覆盖 `/open/*` 端点面与
//! CDN 的 `/cdn/<pick_code>`（HEAD 支持 accept-ranges、GET 支持 Range
//! 206 / 403 注入）。真机事实（K69.x）在断言注释里标注出处。
//!
//! 未覆盖（如实挂账）：CDN 403 退避全路径（需 3 次退避 + 重取，本文件
//! 只钉「403 分类为 RateLimited」）；`rename` 的目录形态（115-0 未实
//! 测，115-5 真机复核）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, head, post};
use axum::{Json, Router};
use ck_pan115::limiter::LimiterConfig;
use ck_pan115::{Pan115Driver, Pan115Params};
use cloudkit_storage::{Page, Range, RelPath, StorageDriver};
use serde_json::{json, Value};

// ---------------------------------------------------------------------
// 内存 VFS 桩
// ---------------------------------------------------------------------

#[derive(Clone)]
struct Node {
    fid: String,
    name: String,
    /// true = 目录
    dir: bool,
    size: i64,
    pick_code: String,
    /// 目录专属：父 cid（rename/delete 的 parent_id 面）
    parent: String,
    /// 文件专属：字节内容
    data: Arc<Vec<u8>>,
}

#[derive(Default)]
struct Vfs {
    /// cid → 子条目名清单（有序：插入序，服务端排序不稳定由驱动侧兜）
    children: HashMap<String, Vec<String>>,
    nodes: HashMap<String, Node>,
    /// 自增 fid/pid 源
    next_id: u64,
    /// proapi 请求计数（list 风暴类断言用）
    api_calls: usize,
    /// CDN 403 注入（**持续**——驱动的恢复路径会重取直链并重试，
    /// 一次性注入会被正常恢复掉，测不到「退避耗尽 → RateLimited」）
    cdn_403: bool,
    /// 桩监听基址（downurl 回绝对 URL——真机形态：115 下发的直链是
    /// 绝对 https；相对形态会让 reqwest 直接 builder error）。
    cdn_base: String,
    /// CDN 忽略 Range（返回 200 全量——写偏防线的负例注入）
    cdn_ignore_range: bool,
    /// 最近一次 ufile/delete 收到的 parent_id 原始值（M-S1 的观测面：
    /// 句柄 parent 段必须把真实父 cid 送到 API——空形态真机未验）
    last_delete_parent: String,
    /// CDN GET 返 410（直链过期形态）的预算——每发一次消耗一次（M-S3
    /// 的注入面：耗尽后 GET 恢复正常 = 「重取直链后可用」的模拟）
    cdn_gone_budget: u32,
    /// downurl 调用计数（M-S3 断言面：自愈必须重取直链）
    downurl_calls: u32,
    /// ufile/move 传输失败注入（K75-1：返回 502 + 非 JSON 体——网络/网关
    /// 错形态；rename 的错误映射不得把它误报成目标占用）
    move_transport_fail: bool,
}

impl Vfs {
    fn new() -> Self {
        let mut vfs = Vfs {
            next_id: 1000,
            ..Vfs::default()
        };
        // 根 cid "0"
        vfs.children.insert("0".to_string(), Vec::new());
        vfs
    }

    fn alloc(&mut self) -> String {
        self.next_id += 1;
        self.next_id.to_string()
    }

    fn mkdir(&mut self, parent: &str, name: &str) -> String {
        let fid = self.alloc();
        self.nodes.insert(
            fid.clone(),
            Node {
                fid: fid.clone(),
                name: name.to_string(),
                dir: true,
                size: 0,
                pick_code: String::new(),
                parent: parent.to_string(),
                data: Arc::new(Vec::new()),
            },
        );
        self.children
            .entry(parent.to_string())
            .or_default()
            .push(name.to_string());
        self.children.entry(fid.clone()).or_default();
        fid
    }

    fn put_file(&mut self, parent: &str, name: &str, data: Vec<u8>) -> String {
        let fid = self.alloc();
        let pc = format!("pc-{}", self.alloc());
        self.nodes.insert(
            fid.clone(),
            Node {
                fid: fid.clone(),
                name: name.to_string(),
                dir: false,
                size: data.len() as i64,
                pick_code: pc,
                parent: parent.to_string(),
                data: Arc::new(data),
            },
        );
        self.children
            .entry(parent.to_string())
            .or_default()
            .push(name.to_string());
        fid
    }

    fn child(&self, parent: &str, name: &str) -> Option<&Node> {
        let names = self.children.get(parent)?;
        names
            .iter()
            .find(|n| n.as_str() == name)
            .and_then(|n| self.nodes.values().find(|node| &node.name == n))
    }

    fn rows(&self, cid: &str) -> Vec<Value> {
        self.children
            .get(cid)
            .map(|names| {
                names
                    .iter()
                    .filter_map(|n| self.child(cid, n))
                    .map(|node| {
                        json!({
                            "fid": node.fid,
                            "fc": if node.dir { "0" } else { "1" },
                            "fs": node.size,
                            "fn": node.name,
                            "pc": node.pick_code,
                            "sha1": "",
                            "upt": 1_700_000_000i64,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

struct Mock {
    vfs: Arc<Mutex<Vfs>>,
    base: String,
}

impl Mock {
    async fn start(vfs: Vfs) -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let mut vfs = vfs;
        vfs.cdn_base = format!("http://{addr}");
        let vfs = Arc::new(Mutex::new(vfs));
        let app = Router::new()
            .route("/open/user/info", get(user_info))
            .route("/open/ufile/files", get(ufile_files))
            .route("/open/folder/get_info", get(folder_get_info))
            .route("/open/folder/add", post(folder_add))
            .route("/open/ufile/delete", post(ufile_delete))
            .route("/open/ufile/update", post(ufile_update))
            .route("/open/ufile/move", post(ufile_move))
            .route("/open/ufile/downurl", post(ufile_downurl))
            .route("/cdn/{pc}", get(cdn_get))
            .route("/cdn/{pc}", head(cdn_head))
            .with_state(vfs.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        Mock {
            vfs,
            base: format!("http://{addr}"),
        }
    }

    fn driver(&self) -> Pan115Driver {
        self.driver_with_root("0")
    }

    fn driver_with_root(&self, root: &str) -> Pan115Driver {
        Pan115Driver::new(Pan115Params {
            client_id: "100197303".to_string(),
            access_token: Some("mock-access".to_string()),
            refresh_token: Some("mock-refresh".to_string()),
            root: root.to_string(),
            api_base: self.base.clone(),
            passport_base: self.base.clone(),
            token_store: None,
            limiter: Some(LimiterConfig::fast()),
            sessions_dir: None,
        })
        .expect("driver constructs")
    }

    fn set(&self, f: impl FnOnce(&mut Vfs)) {
        f(&mut self.vfs.lock().unwrap());
    }
}

const CDN_UA: &str = ck_pan115::UA;

fn ok_json(data: Value) -> Response {
    Json(json!({"state": true, "errno": 0, "data": data})).into_response()
}

fn err_json(code: i64, message: &str) -> Response {
    Json(json!({"state": false, "code": code, "errno": 0, "message": message})).into_response()
}

async fn user_info(State(vfs): State<Arc<Mutex<Vfs>>>) -> Response {
    let _ = vfs;
    ok_json(json!({
        "user_id": 1205495,
        "user_name": "mock",
        "rt_space_info": {
            "all_total": {"size": 201834401841285i64},
            "all_use": {"size": 76593764015308i64},
            "all_remain": {"size": 125240637825977i64}
        }
    }))
}

async fn ufile_files(
    State(vfs): State<Arc<Mutex<Vfs>>>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let mut vfs = vfs.lock().unwrap();
    vfs.api_calls += 1;
    let cid = q.get("cid").cloned().unwrap_or_else(|| "0".to_string());
    if !vfs.children.contains_key(&cid) {
        // 目录不存在：430004（K69.7 采样形态之一）
        return err_json(430004, "目录不存在");
    }
    let rows = vfs.rows(&cid);
    let count = rows.len() as i64;
    let offset: usize = q.get("offset").and_then(|s| s.parse().ok()).unwrap_or(0);
    let limit: usize = q.get("limit").and_then(|s| s.parse().ok()).unwrap_or(20);
    let page: Vec<Value> = rows.into_iter().skip(offset).take(limit).collect();
    Json(json!({"state": true, "errno": 0, "data": page, "count": count})).into_response()
}

async fn folder_get_info(
    State(vfs): State<Arc<Mutex<Vfs>>>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let vfs = vfs.lock().unwrap();
    let fid = q.get("file_id").cloned().unwrap_or_default();
    match vfs.nodes.get(&fid) {
        Some(node) => ok_json(json!({
            "file_id": node.fid,
            "file_name": node.name,
            "file_category": if node.dir { "0" } else { "1" },
            "pick_code": node.pick_code,
            "sha1": "",
            "size_byte": node.size,
            "size": node.size.to_string(),
        })),
        None => err_json(430004, "文件不存在"),
    }
}

async fn folder_add(
    State(vfs): State<Arc<Mutex<Vfs>>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let _ = headers;
    let form = parse_form(&body);
    let mut vfs = vfs.lock().unwrap();
    vfs.api_calls += 1;
    let pid = form.get("pid").cloned().unwrap_or_else(|| "0".to_string());
    let name = form.get("file_name").cloned().unwrap_or_default();
    if !vfs.children.contains_key(&pid) {
        return err_json(430004, "父目录不存在");
    }
    if vfs.child(&pid, &name).is_some() {
        // 已存在：115 真机形态未采（K69 未覆盖）——桩用 430004 家族
        // 之外的显式拒绝，驱动侧靠 list 预检避免触达。
        return err_json(430001, "同名已存在");
    }
    let fid = vfs.mkdir(&pid, &name);
    ok_json(json!({"file_name": name, "file_id": fid}))
}

async fn ufile_delete(State(vfs): State<Arc<Mutex<Vfs>>>, body: String) -> Response {
    let form = parse_form(&body);
    let mut vfs = vfs.lock().unwrap();
    vfs.api_calls += 1;
    let fid = form.get("file_ids").cloned().unwrap_or_default();
    let parent = form
        .get("parent_id")
        .cloned()
        .unwrap_or_else(|| "0".to_string());
    vfs.last_delete_parent = parent.clone();
    let Some(node) = vfs.nodes.get(&fid).cloned() else {
        return err_json(430004, "文件不存在");
    };
    // 从父目录移除（递归：子目录整棵移出可见面）
    if let Some(names) = vfs.children.get_mut(&parent) {
        names.retain(|n| n != &node.name);
    }
    vfs.children.remove(&fid);
    vfs.nodes.remove(&fid);
    ok_json(json!([]))
}

async fn ufile_update(State(vfs): State<Arc<Mutex<Vfs>>>, body: String) -> Response {
    let form = parse_form(&body);
    let mut vfs = vfs.lock().unwrap();
    vfs.api_calls += 1;
    let fid = form.get("file_id").cloned().unwrap_or_default();
    let new_name = form.get("file_name").cloned().unwrap_or_default();
    let Some(node) = vfs.nodes.get(&fid).cloned() else {
        return err_json(430004, "文件不存在");
    };
    if node.dir {
        // 目录 update 的远端语义未实测——桩模拟「端点只对文件生效」。
        return err_json(430001, "目录不支持该操作");
    }
    if let Some(names) = vfs.children.get_mut(&node.parent) {
        for n in names.iter_mut() {
            if n == &node.name {
                *n = new_name.clone();
            }
        }
    }
    if let Some(n) = vfs.nodes.get_mut(&fid) {
        n.name = new_name;
    }
    ok_json(json!({"file_name": form.get("file_name")}))
}

async fn ufile_move(State(vfs): State<Arc<Mutex<Vfs>>>, body: String) -> Response {
    let form = parse_form(&body);
    let mut vfs = vfs.lock().unwrap();
    vfs.api_calls += 1;
    // K75-1 注入：传输/网关错形态（502 + HTML 错误页——非 JSON 信封，
    // 驱动侧归 Unavailable）。
    if vfs.move_transport_fail {
        return Response::builder()
            .status(502)
            .body(Body::from("<html>bad gateway</html>"))
            .unwrap();
    }
    let fid = form.get("file_ids").cloned().unwrap_or_default();
    // SDK 文档形态（115-sdk-go MoveReq：file_ids + to_cid）——真机实证
    // （2026-09-17）：to_pid 形态被后端静默接受但移动不生效（索引孤儿：
    // get_info 活着、源/目标两个 list 都不可见）。桩按文档严格建模。
    let Some(to) = form.get("to_cid").cloned() else {
        return err_json(701000, "参数错误：缺少 to_cid");
    };
    let Some(node) = vfs.nodes.get(&fid).cloned() else {
        return err_json(430004, "文件不存在");
    };
    if !vfs.children.contains_key(&to) {
        return err_json(430004, "目标目录不存在");
    }
    if vfs.child(&to, &node.name).is_some() {
        return err_json(430001, "同名已存在");
    }
    if let Some(names) = vfs.children.get_mut(&node.parent) {
        names.retain(|n| n != &node.name);
    }
    vfs.children
        .entry(to.clone())
        .or_default()
        .push(node.name.clone());
    if let Some(n) = vfs.nodes.get_mut(&fid) {
        n.parent = to;
    }
    ok_json(json!([]))
}

async fn ufile_downurl(State(vfs): State<Arc<Mutex<Vfs>>>, body: String) -> Response {
    let form = parse_form(&body);
    let mut vfs = vfs.lock().unwrap();
    vfs.downurl_calls += 1;
    let pc = form.get("pick_code").cloned().unwrap_or_default();
    if !vfs.nodes.values().any(|n| n.pick_code == pc) {
        return err_json(430004, "pick_code 无效");
    }
    // UA 绑定在真机是 CDN 侧校验（K69.4）；桩的取链恒成功，绑定由
    // 测试的 UA 一致性断言覆盖（见 downurl_ua_and_range）。
    ok_json(json!({
        pc.clone(): {
            "file_name": "x",
            "file_size": 0,
            "pick_code": pc.clone(),
            "sha1": "",
            "url": {"url": format!("{}/cdn/{}", vfs.cdn_base, pc)}
        }
    }))
}

async fn cdn_head(State(vfs): State<Arc<Mutex<Vfs>>>, AxPath(pc): AxPath<String>) -> Response {
    let node = vfs
        .lock()
        .unwrap()
        .nodes
        .values()
        .find(|n| n.pick_code == pc)
        .cloned();
    match node {
        Some(n) => Response::builder()
            .status(StatusCode::OK)
            .header("accept-ranges", "bytes")
            .header("etag", format!("\"etag-{}\"", n.fid))
            .header("content-length", n.size.to_string())
            .body(Body::empty())
            .unwrap(),
        None => (StatusCode::NOT_FOUND, "no such pick_code").into_response(),
    }
}

async fn cdn_get(
    State(vfs): State<Arc<Mutex<Vfs>>>,
    AxPath(pc): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    let (node, ignore_range, force_403, gone) = {
        let mut vfs = vfs.lock().unwrap();
        let gone = vfs.cdn_gone_budget > 0;
        if gone {
            vfs.cdn_gone_budget -= 1;
        }
        let force = vfs.cdn_403;
        let ignore = vfs.cdn_ignore_range;
        let node = vfs.nodes.values().find(|n| n.pick_code == pc).cloned();
        (node, ignore, force, gone)
    };
    if force_403 {
        return (StatusCode::FORBIDDEN, "rate limited").into_response();
    }
    if gone {
        // 直链过期形态（真机 K69.4 注记的 401/410 族——这里用 410 Gone）
        return (StatusCode::GONE, "link expired").into_response();
    }
    let Some(node) = node else {
        return (StatusCode::NOT_FOUND, "no such pick_code").into_response();
    };
    // UA 校验（真机 K69.4：取链与下载 UA 逐字节一致；桩在 CDN 侧要求
    // 恒定模块 UA——测试的错配负例走 403）。
    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if ua != CDN_UA {
        return (StatusCode::FORBIDDEN, "ua mismatch").into_response();
    }
    let data = node.data.clone();
    let size = data.len() as u64;
    let range = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match (range, ignore_range) {
        (Some(r), false) => {
            let (start, end) = parse_range(&r, size);
            if start >= size {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .body(Body::empty())
                    .unwrap();
            }
            let end = end.min(size); // 半开
            let slice = data[start as usize..end as usize].to_vec();
            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(
                    "content-range",
                    format!("bytes {}-{}/{}", start, end - 1, size),
                )
                .header("content-length", slice.len().to_string())
                .body(Body::from(slice))
                .unwrap()
        }
        _ => Response::builder()
            .status(StatusCode::OK)
            .header("content-length", size.to_string())
            .body(Body::from(data.as_ref().clone()))
            .unwrap(),
    }
}

/// `bytes=a-b` → (a, b+1) 半开；开放结尾 `a-` → (a, size)。
fn parse_range(r: &str, size: u64) -> (u64, u64) {
    let spec = r.strip_prefix("bytes=").unwrap_or(r);
    let mut parts = spec.splitn(2, '-');
    let start: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let end: Option<u64> = parts.next().and_then(|s| s.parse().ok());
    (start, end.map(|e| e + 1).unwrap_or(size))
}

fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((urldecode(k), urldecode(v)))
        })
        .collect()
}

fn urldecode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b as char);
                    i += 3;
                    continue;
                }
                out.push('%');
                i += 1;
            }
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out
}

/// 测试可读性：允许写 `/a/b`；vocab::RelPath 是**相对**路径（无前导
/// 斜杠），helper 负责剥离。
fn path(s: &str) -> RelPath {
    RelPath::new(s.trim_start_matches('/')).expect("valid path")
}

// ---------------------------------------------------------------------
// 断言矩阵
// ---------------------------------------------------------------------

#[tokio::test]
async fn list_is_depth_one_sorted_and_paged() {
    let mut vfs = Vfs::new();
    vfs.mkdir("0", "zeta");
    let d = vfs.mkdir("0", "docs");
    vfs.put_file("0", "alpha.txt", b"aaaa".to_vec());
    vfs.put_file(&d, "inner.txt", b"inner".to_vec()); // 深层条目不出现
    let mock = Mock::start(vfs).await;
    let drv = mock.driver();

    let listing = drv.list(&RelPath::root(), Page::all()).await.expect("list");
    let names: Vec<String> = listing
        .entries
        .iter()
        .map(|e| e.path.as_str().to_string())
        .collect();
    // 字典序稳定（trait 契约）+ depth-1（inner.txt 不出现）
    assert_eq!(names, vec!["alpha.txt", "docs", "zeta"]);
    assert_eq!(listing.entries[1].kind, cloudkit_storage::EntryKind::Dir);
    assert_eq!(listing.entries[0].kind, cloudkit_storage::EntryKind::File);
    assert_eq!(listing.entries[0].size, 4);
    assert!(listing.next.is_none(), "full page in one shot");

    // 分页：limit=2 → next 游标 → 第二页恰余一条
    let first = drv
        .list(
            &RelPath::root(),
            Page {
                limit: 2,
                cursor: cloudkit_storage::PageCursor::Start,
            },
        )
        .await
        .expect("page 1");
    assert_eq!(first.entries.len(), 2);
    let cursor = first.next.expect("cursor");
    let second = drv
        .list(&RelPath::root(), Page { limit: 2, cursor })
        .await
        .expect("page 2");
    assert_eq!(second.entries.len(), 1);
    assert_eq!(second.entries[0].path.as_str(), "zeta");
    assert!(second.next.is_none());
}

#[tokio::test]
async fn list_missing_dir_is_not_found() {
    let mock = Mock::start(Vfs::new()).await;
    let drv = mock.driver();
    let err = drv
        .list(&path("/nope"), Page::all())
        .await
        .expect_err("missing");
    assert!(
        matches!(err, cloudkit_storage::StorageError::NotFound),
        "{err:?}"
    );
}

#[tokio::test]
async fn stat_resolves_paths_through_nested_dirs_and_root() {
    let mut vfs = Vfs::new();
    let a = vfs.mkdir("0", "a");
    let b = vfs.mkdir(&a, "b");
    vfs.put_file(&b, "deep.txt", b"deep-data".to_vec());
    vfs.put_file("0", "top.bin", b"top".to_vec());
    let mock = Mock::start(vfs).await;
    let drv = mock.driver();

    // 根
    let root = drv.stat(&RelPath::root()).await.expect("root stat");
    assert_eq!(root.kind, cloudkit_storage::EntryKind::Dir);

    // 两级下行到文件
    let e = drv.stat(&path("/a/b/deep.txt")).await.expect("stat file");
    assert_eq!(e.size, 9);
    assert_eq!(e.kind, cloudkit_storage::EntryKind::File);

    // 目录本身
    let d = drv.stat(&path("/a/b")).await.expect("stat dir");
    assert_eq!(d.kind, cloudkit_storage::EntryKind::Dir);

    // **stat 是新鲜度查询**：末级组件绕过缓存现列父目录（conformance
    // ⑤ 要求后端错误能被 stat 观察到；缓存命中会把注入的后端错误吞
    // 掉）。中间层仍走缓存——重复 stat 只花「末级一次 list」的网络。
    let before = mock.vfs.lock().unwrap().api_calls;
    let _ = drv.stat(&path("/a/b/deep.txt")).await.expect("fresh stat");
    let after = mock.vfs.lock().unwrap().api_calls;
    assert_eq!(
        after - before,
        1,
        "a repeated stat costs exactly one backend call (the fresh last level)"
    );

    // list 的**路径解析**走缓存（目录链稳定）；它的 1 次网络是目录内容
    // 本身（必付）。所以 list 净增恰 1，而非路径长度的次数。
    let before_list = mock.vfs.lock().unwrap().api_calls;
    let _ = drv
        .list(&path("/a/b"), cloudkit_storage::Page::all())
        .await
        .expect("cached list");
    let after_list = mock.vfs.lock().unwrap().api_calls;
    assert_eq!(
        after_list - before_list,
        1,
        "list costs exactly its own content fetch (the path resolution is cached)"
    );

    // 不存在
    let err = drv.stat(&path("/a/ghost")).await.expect_err("missing");
    assert!(
        matches!(err, cloudkit_storage::StorageError::NotFound),
        "{err:?}"
    );
}

#[tokio::test]
async fn mkdir_creates_implicit_parents_and_reports_exists() {
    let mock = Mock::start(Vfs::new()).await;
    let drv = mock.driver();

    drv.mkdir(&path("/x/y/z")).await.expect("implicit parents");
    let e = drv.stat(&path("/x/y/z")).await.expect("stat created");
    assert_eq!(e.kind, cloudkit_storage::EntryKind::Dir);

    // 再次 mkdir 同路径 → Exists（预检形态，不产重名副本）
    let err = drv.mkdir(&path("/x/y/z")).await.expect_err("exists");
    assert!(
        matches!(err, cloudkit_storage::StorageError::Exists),
        "{err:?}"
    );

    // 根 → Exists（卷根本就存在）
    let err = drv.mkdir(&RelPath::root()).await.expect_err("root");
    assert!(
        matches!(err, cloudkit_storage::StorageError::Exists),
        "{err:?}"
    );

    // 中间层是文件 → NotFound
    drv.mkdir(&path("/file_at_mid")).await.expect("dir");
    mock.set(|vfs| {
        vfs.put_file("0", "blocker", b"x".to_vec());
    });
    // 用已存在文件当中间层
    let err = drv
        .mkdir(&path("/blocker/below"))
        .await
        .expect_err("file in the middle");
    assert!(
        matches!(err, cloudkit_storage::StorageError::NotFound),
        "{err:?}"
    );
}

#[tokio::test]
async fn delete_uses_composite_handle_and_second_delete_is_not_found() {
    let mut vfs = Vfs::new();
    let a = vfs.mkdir("0", "a");
    vfs.put_file(&a, "doomed.txt", b"bye".to_vec());
    let mock = Mock::start(vfs).await;
    let drv = mock.driver();

    // 先 stat 拿句柄（句柄编码 fid:pc:parent）
    let e = drv.stat(&path("/a/doomed.txt")).await.expect("stat");
    let handle = e.id.handle.as_str().to_string();
    drv.delete(&e.id).await.expect("delete");

    // 幂等声明 = NotFound（查询式，trait 二选一）
    let e2 = cloudkit_storage::EntryId::new(
        e.id.volume.clone(),
        cloudkit_storage::BackendHandle::new(handle),
    );
    let err = drv.delete(&e2).await.expect_err("second delete");
    assert!(
        matches!(err, cloudkit_storage::StorageError::NotFound),
        "{err:?}"
    );

    // 他卷句柄 → NotFound
    let other_volume = cloudkit_storage::VolumeId::new("pan115", "someone-else").expect("vid");
    let foreign = cloudkit_storage::EntryId::new(
        other_volume,
        cloudkit_storage::BackendHandle::new("123:pc:0"),
    );
    let err = drv.delete(&foreign).await.expect_err("foreign handle");
    assert!(
        matches!(err, cloudkit_storage::StorageError::NotFound),
        "{err:?}"
    );
}

#[tokio::test]
async fn mkdir_after_delete_reuses_the_freed_name() {
    let mut vfs = Vfs::new();
    let a = vfs.mkdir("0", "a");
    vfs.put_file(&a, "doomed.txt", b"bye".to_vec());
    let mock = Mock::start(vfs).await;
    let drv = mock.driver();

    // stat 喂路径缓存（该目录已列过）→ 句柄 delete → 同名 mkdir：缓存
    // 里的 ghost 行不得让 mkdir 误报 Exists（审查 M-S1——句柄 parent 段
    // 恒空使 delete 的 invalidate("") 成为 no-op）。
    let e = drv.stat(&path("/a/doomed.txt")).await.expect("stat");
    drv.delete(&e.id).await.expect("delete");
    drv.mkdir(&path("/a/doomed.txt"))
        .await
        .expect("the freed name is mkdir-able again");

    // delete API 的 parent_id 必须是真实父 cid（spike 真机形态；空串
    // 形态真机从未验证过——句柄 parent 段修复的 API 面）。
    let seen_parent = mock.vfs.lock().unwrap().last_delete_parent.clone();
    assert_eq!(
        seen_parent, a,
        "delete parent_id carries the real parent cid"
    );
}

/// M-S4：rename 的目标存在预检不得只看缓存——目标父目录本进程未列过
/// 时必须现列预检（mkdir 同款纪律）。跨目录 rename 到冷目录曾直接
/// 穿透到 move/update，trait 契约的 Exists 语义丢失（同父场景由源
/// resolve 顺带喂缓存而侥幸成立）。
/// K75-1：跨父 rename 的 move 腿遇**传输类失败**（502/非 JSON——映射为
/// Unavailable）时不得误报 `Exists`——「目标已占用」是数据面判断，只有
/// 后端明确拒绝（如 430001 同名）才成立；网络错说成目标占用会把用户
/// 引向覆盖操作。
#[tokio::test]
async fn rename_reports_transport_failure_as_is_not_exists() {
    let mut vfs = Vfs::new();
    let d1 = vfs.mkdir("0", "srcdir");
    vfs.mkdir("0", "dst");
    vfs.put_file(&d1, "a.txt", b"aaa".to_vec());
    vfs.move_transport_fail = true; // move 请求 502（网络/网关形态）
    let mock = Mock::start(vfs).await;
    let drv = mock.driver();

    let err = drv
        .rename(&path("/srcdir/a.txt"), &path("/dst/b.txt"))
        .await
        .expect_err("the move must fail");
    assert!(
        !matches!(err, cloudkit_storage::StorageError::Exists),
        "a transport failure must not masquerade as an occupied target: {err:?}"
    );
    // 源文件原位未动（move 从未生效）
    let still = drv
        .stat(&path("/srcdir/a.txt"))
        .await
        .expect("source untouched");
    assert_eq!(still.size, 3);
}

#[tokio::test]
async fn rename_into_a_cold_directory_prechecks_the_target() {
    let mut vfs = Vfs::new();
    let d1 = vfs.mkdir("0", "srcdir");
    let d2 = vfs.mkdir("0", "dst");
    vfs.put_file(&d1, "a.txt", b"aaa".to_vec());
    vfs.put_file(&d2, "b.txt", b"bbb".to_vec());
    let mock = Mock::start(vfs).await;
    let drv = mock.driver();

    // dst 目录从未被本驱动列过（resolve 源路径只喂了 root 与 srcdir）
    let err = drv
        .rename(&path("/srcdir/a.txt"), &path("/dst/b.txt"))
        .await
        .expect_err("an occupied target must be refused with Exists");
    assert!(
        matches!(err, cloudkit_storage::StorageError::Exists),
        "{err:?}"
    );

    // 穿透会造成数据面破坏——源文件必须原位未动
    let still = drv
        .stat(&path("/srcdir/a.txt"))
        .await
        .expect("source untouched");
    assert_eq!(still.size, 3);
}

#[tokio::test]
async fn rename_file_same_parent_updates_in_place() {
    let mut vfs = Vfs::new();
    vfs.put_file("0", "old.txt", b"content".to_vec());
    let mock = Mock::start(vfs).await;
    let drv = mock.driver();

    drv.rename(&path("/old.txt"), &path("/new.txt"))
        .await
        .expect("rename");
    let e = drv.stat(&path("/new.txt")).await.expect("new name visible");
    assert_eq!(e.size, 7);
    let err = drv.stat(&path("/old.txt")).await.expect_err("old gone");
    assert!(
        matches!(err, cloudkit_storage::StorageError::NotFound),
        "{err:?}"
    );
}

#[tokio::test]
async fn rename_cross_directory_moves_and_renames() {
    let mut vfs = Vfs::new();
    let src = vfs.mkdir("0", "src");
    vfs.mkdir("0", "dst");
    vfs.put_file(&src, "f.bin", b"payload".to_vec());
    let mock = Mock::start(vfs).await;
    let drv = mock.driver();

    drv.rename(&path("/src/f.bin"), &path("/dst/g.bin"))
        .await
        .expect("cross-dir rename");
    let e = drv.stat(&path("/dst/g.bin")).await.expect("moved+renamed");
    assert_eq!(e.size, 7);
    let err = drv
        .stat(&path("/src/f.bin"))
        .await
        .expect_err("source gone");
    assert!(
        matches!(err, cloudkit_storage::StorageError::NotFound),
        "{err:?}"
    );
}

#[tokio::test]
async fn rename_rejects_existing_target_and_descendant_target() {
    let mut vfs = Vfs::new();
    vfs.put_file("0", "a.txt", b"a".to_vec());
    vfs.put_file("0", "b.txt", b"b".to_vec());
    let d = vfs.mkdir("0", "dir");
    vfs.mkdir(&d, "sub");
    let mock = Mock::start(vfs).await;
    let drv = mock.driver();

    // 目标已存在 → Exists
    let err = drv
        .rename(&path("/a.txt"), &path("/b.txt"))
        .await
        .expect_err("target exists");
    assert!(
        matches!(err, cloudkit_storage::StorageError::Exists),
        "{err:?}"
    );

    // 目标是源的后代 → Invalid
    let err = drv
        .rename(&path("/dir"), &path("/dir/sub/deeper"))
        .await
        .expect_err("descendant");
    assert!(
        matches!(err, cloudkit_storage::StorageError::Invalid),
        "{err:?}"
    );
}

#[tokio::test]
async fn reader_requires_file_handle_and_rejects_dirs() {
    let mut vfs = Vfs::new();
    vfs.mkdir("0", "ad");
    vfs.put_file("0", "data.bin", vec![7u8; 100]).to_string();
    let mock = Mock::start(vfs).await;
    let drv = mock.driver();

    let dir_entry = drv.stat(&path("/ad")).await.expect("dir stat");
    match drv.reader(&dir_entry.id, None).await {
        Err(cloudkit_storage::StorageError::Invalid) => {}
        Err(other) => panic!("expected Invalid for a dir handle, got {other:?}"),
        Ok(_) => panic!("a dir handle must not open a reader"),
    }

    let file_entry = drv.stat(&path("/data.bin")).await.expect("file stat");
    let foreign = cloudkit_storage::EntryId::new(
        cloudkit_storage::VolumeId::new("pan115", "other").expect("vid"),
        cloudkit_storage::BackendHandle::new("1:pc:0"),
    );
    match drv.reader(&foreign, None).await {
        Err(cloudkit_storage::StorageError::NotFound) => {}
        Err(other) => panic!("expected NotFound for a foreign handle, got {other:?}"),
        Ok(_) => panic!("a foreign-volume handle must not open a reader"),
    }

    let _ = file_entry;
}

#[tokio::test]
async fn reader_streams_full_and_ranged_windows() {
    use futures_util::StreamExt;

    let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    let mut vfs = Vfs::new();
    vfs.put_file("0", "blob.bin", payload.clone());
    let mock = Mock::start(vfs).await;
    let drv = mock.driver();
    let entry = drv.stat(&path("/blob.bin")).await.expect("stat");

    // 整读（range=None）
    let mut stream = drv.reader(&entry.id, None).await.expect("full read");
    let mut got: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        got.extend_from_slice(&chunk.expect("chunk"));
    }
    assert_eq!(got, payload, "full stream is byte-exact");

    // 精确窗口 [100, 200)（半开——vocab.rs Range 语义）
    let range = Range::new(100, Some(200)).expect("range");
    let mut stream = drv
        .reader(&entry.id, Some(range))
        .await
        .expect("range read");
    let mut got: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        got.extend_from_slice(&chunk.expect("chunk"));
    }
    assert_eq!(got, payload[100..200], "window bytes are exact");

    // 越界起点 → 空流（conformance ② 形态）
    let range = Range::new(50_000, None).expect("range past eof");
    let mut stream = drv
        .reader(&entry.id, Some(range))
        .await
        .expect("empty read");
    let mut got: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        got.extend_from_slice(&chunk.expect("chunk"));
    }
    assert!(got.is_empty(), "start >= size yields the empty stream");

    // end 越界钳制到 EOF
    let range = Range::new(9_990, Some(999_999)).expect("range clamp");
    let mut stream = drv
        .reader(&entry.id, Some(range))
        .await
        .expect("clamped read");
    let mut got: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        got.extend_from_slice(&chunk.expect("chunk"));
    }
    assert_eq!(got, payload[9_990..], "end clamps to EOF");
}

#[tokio::test]
async fn reader_maps_cdn_403_to_rate_limited() {
    let mut vfs = Vfs::new();
    vfs.put_file("0", "hot.bin", vec![1u8; 4096]);
    vfs.cdn_403 = true; // 首个 CDN GET 即 403（CDN 限流形态，K69.4）
    let mock = Mock::start(vfs).await;
    let drv = mock.driver();
    let entry = drv.stat(&path("/hot.bin")).await.expect("stat");

    let mut stream = drv.reader(&entry.id, None).await.expect("stream opens");
    use futures_util::StreamExt;
    // 第一帧（或终止错）应携带 RateLimited——桩的 403 是恒定的，
    // 退避梯度后重试同样 403 → 最终上报 RateLimited。
    let mut saw_rate_limit = false;
    let mut frames = 0;
    while let Some(chunk) = stream.next().await {
        frames += 1;
        match chunk {
            Ok(_) => {}
            Err(cloudkit_storage::StorageError::RateLimited { .. }) => {
                saw_rate_limit = true;
                break;
            }
            Err(other) => panic!("expected RateLimited, got {other:?}"),
        }
        if frames > 4 {
            panic!("stream should terminate with the rate-limit verdict");
        }
    }
    assert!(saw_rate_limit, "CDN 403 classifies as RateLimited");
}

/// M-S3：直链中途过期（CDN 410）必须自愈——失效缓存、重取直链、续传
/// 剩余字节。模块文档声明的「401/410 → 重取直链一次」曾只对 403 生效：
/// 长流在 30min TTL 假设之外会硬死（Unavailable 直接杀死流）。
#[tokio::test]
async fn reader_self_heals_a_mid_stream_link_expiry() {
    let mut vfs = Vfs::new();
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    vfs.put_file("0", "stream.bin", payload.clone());
    vfs.cdn_gone_budget = 1; // 首个 CDN GET 返 410（直链过期形态）
    let mock = Mock::start(vfs).await;
    let drv = mock.driver();
    let entry = drv.stat(&path("/stream.bin")).await.expect("stat");

    let mut stream = drv.reader(&entry.id, None).await.expect("stream opens");
    use futures_util::StreamExt;
    let mut got: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        got.extend_from_slice(&chunk.expect("the healed stream keeps serving bytes"));
    }
    assert_eq!(got, payload, "the stream survives a mid-flight link expiry");

    let calls = mock.vfs.lock().unwrap().downurl_calls;
    assert!(calls >= 2, "the driver refetched the dlink (got {calls})");
}

#[tokio::test]
async fn reader_rejects_a_cdn_that_ignores_range() {
    let mut vfs = Vfs::new();
    vfs.put_file("0", "r.bin", vec![9u8; 8192]);
    vfs.cdn_ignore_range = true; // 200 + 全量（写偏防线的负例，baidu 同款）
    let mock = Mock::start(vfs).await;
    let drv = mock.driver();
    let entry = drv.stat(&path("/r.bin")).await.expect("stat");

    let range = Range::new(10, Some(19)).expect("range");
    let mut stream = drv
        .reader(&entry.id, Some(range))
        .await
        .expect("stream opens");
    use futures_util::StreamExt;
    let first = stream.next().await.expect("a verdict frame");
    match first {
        Err(cloudkit_storage::StorageError::Unavailable(msg)) => {
            assert!(msg.contains("206"), "names the 206 expectation: {msg}");
        }
        other => panic!("expected Unavailable(206), got {other:?}"),
    }
}

#[tokio::test]
async fn quota_reads_rt_space_info() {
    let mock = Mock::start(Vfs::new()).await;
    let drv = mock.driver();
    let q = drv.quota().await.expect("quota");
    assert_eq!(q.total, Some(201834401841285));
    assert_eq!(q.used, 76593764015308);
}

#[tokio::test]
async fn root_parameter_scopes_the_whole_volume() {
    let mut vfs = Vfs::new();
    let sub = vfs.mkdir("0", "scoped");
    vfs.put_file(&sub, "only.txt", b"inside".to_vec());
    vfs.put_file("0", "outside.txt", b"outside".to_vec());
    let mock = Mock::start(vfs).await;
    // D3：pan115_root 可设置——根解析从 scoped cid 起
    let drv = mock.driver_with_root(&sub);

    let listing = drv.list(&RelPath::root(), Page::all()).await.expect("list");
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].path.as_str(), "only.txt");
    let err = drv
        .stat(&path("/outside.txt"))
        .await
        .expect_err("outside root");
    assert!(
        matches!(err, cloudkit_storage::StorageError::NotFound),
        "{err:?}"
    );
}
