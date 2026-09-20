//! 假 123pan web API + 假 CDN/中继端点的共享桩（Phase 6 / 123-2）。
//!
//! 一个 loopback 进程内的双层服务（ck-pan115 `stub_common` 先例形态）。
//!
//! # API 面
//!
//! 真机代际形态——任务规格钉的路径；envelope `{"code":0,"data":...}`
//! （双成功码/双拼由驱动侧解析）：
//!
//! - `/api/file/list/new`——Page 1 基分页
//! - `/b/api/file/info`——小写 `infoList`
//! - `/a/api/file/upload_request`——type=1 目录创建，5060 冲突
//! - `/a/api/file/trash`——载荷恰为 `fileTrashInfoList` 大写 `FileId` +
//!   `event:"intoRecycle"`；陷阱旋钮 = code=0 但不删（静默陷阱建模）
//! - `/a/api/file/rename`——文件与目录同端点（任务 0 实证）
//! - `/b/api/file/mod_pid`、`/b/api/user/info`
//! - `/b/api/file/download/traffic/check`、`/a/api/file/download_info`
//!
//! # 传输面
//!
//! CDN/中继（裸 client 直达）：
//!
//! - `/download-v2/`——URL 的 `params=` 段携真链（中继 URL 形态，纯
//!   解码零 GET——无路由，GET 它即 404）
//! - `/redirect/{fid}`——HTTP 210 + JSON `redirect_url` 重定向体
//! - `/loc/{fid}`——302 Location 头形态
//! - `/html/{fid}`——200 HTML 带 href 的防御形态
//! - `/loop/{fid}`——自指重定向（跳数封顶用例）
//! - `/mirror/{fid}`——206 Range 切片（镜像域形态）
//!
//! # 观测面
//!
//! 各端点命中计数（harness 断言用：`download_info` 恰一次 = dlink
//! 一次性纪律）与注入旋钮（流量超额/错误码/静默陷阱/直链死亡/200
//! 忽略 Range/链形态）。
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use ck_pan123::api::RetryConfig;
use ck_pan123::limiter::LimiterConfig;
use ck_pan123::{Pan123Driver, Pan123Params};
use serde_json::{json, Value};

/// 桩内条目：名字 / file_id / 是否目录 / size / 内容。
#[derive(Clone)]
pub struct StubEntry {
    pub name: String,
    pub fid: i64,
    pub is_dir: bool,
    pub size: i64,
    pub data: Vec<u8>,
}

impl StubEntry {
    fn to_row(&self, parent: i64) -> Value {
        json!({
            "FileId": self.fid,
            "ParentFileId": parent,
            "FileName": self.name,
            "Type": if self.is_dir { 1 } else { 0 },
            "Size": self.size,
            "Etag": "",
            "S3KeyFlag": "4006416717-0",
            "Trashed": false,
            "UpdateAt": "2026-09-20T12:20:15+08:00",
            "CreateAt": "2026-09-20T12:20:15+08:00",
        })
    }
}

#[derive(Default)]
pub struct StubState {
    /// 父 file_id → 子条目（根 = "0"）。
    pub dirs: HashMap<String, Vec<StubEntry>>,
    pub next_id: i64,
    /// 端点命中计数（路径 → 次数）。
    pub hits: HashMap<String, u32>,
    /// traffic/check 注入：isTrafficExceeded。
    pub traffic_exceeded: bool,
    /// download_info 注入：错误码（0 = 正常）。
    pub download_info_code: i64,
    /// trash 静默陷阱：载荷形状不恰当时 code=0 但不删（§5.11 建模）。
    pub trash_silent_trap: bool,
    /// mirror 死亡模式：404。
    pub mirror_dead: bool,
    /// mirror 忽略 Range：200 全量（写偏防线用例）。
    pub mirror_ignore_range: bool,
    /// download_info 产出的链形态：
    /// `relay`（默认——params 自解码 → 210 JSON → mirror，真机三跳）/
    /// `direct-cdn`（直指 210 JSON 重定向体）/ `location`（302 链）/
    /// `html`（200 HTML 带 href 的防御形态）/ `loop`（自指重定向——
    /// 跳数封顶用例）。
    pub chain_mode: String,
    /// 一次性 list 错误注入（stat 末级新鲜查询用例）。
    pub list_error_inject: Option<i64>,
}

impl StubState {
    fn new() -> Self {
        let mut st = StubState {
            next_id: 6400_0000,
            chain_mode: "relay".to_string(),
            ..StubState::default()
        };
        st.dirs.insert("0".to_string(), Vec::new());
        st
    }
}

pub struct ApiStub {
    pub state: Arc<Mutex<StubState>>,
    pub base: String,
}

impl ApiStub {
    pub async fn start() -> ApiStub {
        let state = Arc::new(Mutex::new(StubState::new()));
        let app = Router::new()
            .route("/api/dydomain", get(dydomain))
            .route("/api/file/list/new", get(list_new))
            .route("/b/api/user/info", get(user_info))
            .route("/b/api/file/info", post(file_info))
            .route("/a/api/file/upload_request", post(upload_request))
            .route("/a/api/file/trash", post(trash))
            .route("/a/api/file/rename", post(rename))
            .route("/b/api/file/mod_pid", post(mod_pid))
            .route("/b/api/file/download/traffic/check", post(traffic_check))
            .route("/a/api/file/download_info", post(download_info))
            // 传输面
            .route("/redirect/{fid}", get(redirect_get))
            .route("/loc/{fid}", get(loc_get))
            .route("/html/{fid}", get(html_get))
            .route("/loop/{fid}", get(loop_get))
            .route("/mirror/{fid}", get(mirror_get))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub listener");
        let addr = listener.local_addr().expect("stub addr");
        let base = format!("http://{addr}");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("stub serve") });
        ApiStub { state, base }
    }

    /// 构造被测驱动（api + cdn 全指桩；限流/重试毫秒级注入）。
    pub fn driver(&self) -> Pan123Driver {
        self.driver_with_root("0")
    }

    pub fn driver_with_root(&self, root: &str) -> Pan123Driver {
        Pan123Driver::new(Pan123Params {
            token: Some("stub-token-0123456789".to_string()),
            root: root.to_string(),
            api_base: self.base.clone(),
            fallback_base: self.base.clone(),
            login_base: self.base.clone(),
            limiter: Some(LimiterConfig::fast()),
            retry: Some(RetryConfig::fast()),
        })
        .expect("stub driver")
    }

    pub fn hits(&self, path: &str) -> u32 {
        self.state
            .lock()
            .unwrap()
            .hits
            .get(path)
            .copied()
            .unwrap_or(0)
    }

    /// 在目录下建一个子目录（测试准备面；size 注入聚合值——驱动须报 0）。
    pub fn mkdir(&self, parent: &str, name: &str) -> i64 {
        let mut st = self.state.lock().unwrap();
        let fid = st.next_id;
        st.next_id += 1;
        st.dirs
            .entry(parent.to_string())
            .or_default()
            .push(StubEntry {
                name: name.to_string(),
                fid,
                is_dir: true,
                size: 987654, // 目录带累计聚合 Size——驱动须报 0（任务 A）
                data: Vec::new(),
            });
        st.dirs.insert(fid.to_string(), Vec::new());
        fid
    }

    /// 在目录下放一个文件（带内容；测试准备面）。
    pub fn put_file(&self, parent: &str, name: &str, data: Vec<u8>) -> i64 {
        let mut st = self.state.lock().unwrap();
        let fid = st.next_id;
        st.next_id += 1;
        let size = data.len() as i64;
        st.dirs
            .entry(parent.to_string())
            .or_default()
            .push(StubEntry {
                name: name.to_string(),
                fid,
                is_dir: false,
                size,
                data,
            });
        fid
    }

    /// 灌入 N 个文件（分页合并排序用例——超单页 100）。
    pub fn put_many(&self, parent: &str, prefix: &str, count: usize) {
        let mut st = self.state.lock().unwrap();
        for i in 0..count {
            let fid = st.next_id;
            st.next_id += 1;
            st.dirs
                .entry(parent.to_string())
                .or_default()
                .push(StubEntry {
                    name: format!("{prefix}{i:04}"),
                    fid,
                    is_dir: false,
                    size: 1,
                    data: vec![b'x'],
                });
        }
    }

    /// 名字重复但 file_id 不同的两行（排序 file_id 回退的用例面——真机
    /// 服务端不会产同名对，桩开放该形态钉驱动排序的确定性）。
    pub fn put_dup_name(&self, parent: &str, name: &str) -> i64 {
        let mut st = self.state.lock().unwrap();
        let fid = st.next_id;
        st.next_id += 1;
        st.dirs
            .entry(parent.to_string())
            .or_default()
            .push(StubEntry {
                name: name.to_string(),
                fid,
                is_dir: false,
                size: 1,
                data: vec![b'y'],
            });
        fid
    }
}

// ---------------------------------------------------------------------
// API 面路由
// ---------------------------------------------------------------------

fn ok_json(data: Value) -> Response {
    Json(json!({"code": 0, "message": "ok", "data": data})).into_response()
}

fn err_code(code: i64, message: &str) -> Response {
    Json(json!({"code": code, "message": message, "data": {}})).into_response()
}

fn count_hit(st: &mut StubState, path: &str) {
    *st.hits.entry(path.to_string()).or_insert(0) += 1;
}

fn host_of(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1")
        .to_string()
}

async fn dydomain() -> Response {
    // 解析失败形态（空 domains）：驱动维持构造主域（= 桩 base），业务
    // 请求直达。解析成功换域路径由 123-1 的 api_client.rs 覆盖。
    Json(json!({"code": 0, "data": {"domains": []}})).into_response()
}

async fn list_new(
    State(state): State<Arc<Mutex<StubState>>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mut st = state.lock().unwrap();
    count_hit(&mut st, "/api/file/list/new");
    if let Some(code) = st.list_error_inject.take() {
        return err_code(code, "injected");
    }
    let parent = q.get("parentFileId").cloned().unwrap_or_else(|| "0".into());
    let page: u32 = q.get("Page").and_then(|v| v.parse().ok()).unwrap_or(1);
    let limit: usize = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(100);
    let Some(entries) = st.dirs.get(&parent).cloned() else {
        return err_code(1024, "目录不存在");
    };
    let parent_fid = parent.parse::<i64>().unwrap_or(0);
    let total = entries.len() as i64;
    let start = (page as usize - 1) * limit;
    // 服务端排序约束面：只按 file_id desc 交页（真机约束——名字乱序
    // 注入靠 fid 与名字的错位构造）。
    let mut sorted = entries;
    sorted.sort_by_key(|e| std::cmp::Reverse(e.fid));
    let slice: Vec<Value> = sorted
        .iter()
        .skip(start)
        .take(limit)
        .map(|e| e.to_row(parent_fid))
        .collect();
    Json(json!({
        "code": 0,
        "data": {
            "InfoList": slice,
            "Total": total,
            "Len": slice.len(),
            "IsFirst": page == 1,
            "Next": "-1",
        }
    }))
    .into_response()
}

async fn user_info(State(state): State<Arc<Mutex<StubState>>>) -> Response {
    let mut st = state.lock().unwrap();
    count_hit(&mut st, "/b/api/user/info");
    ok_json(json!({
        "UID": 4006416717i64,
        "SpacePermanent": 2199023255552i64,
        "SpaceUsed": 1073741824i64,
        "DirectTraffic": 0,
        "ShareTraffic": 0,
        "Vip": false,
    }))
}

async fn file_info(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let mut st = state.lock().unwrap();
    count_hit(&mut st, "/b/api/file/info");
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return err_code(400, "bad json");
    };
    let Some(fid) = v
        .get("fileIdList")
        .and_then(|l| l.as_array())
        .and_then(|a| a.first())
        .and_then(|it| it.get("fileId"))
        .and_then(|f| f.as_i64())
    else {
        return err_code(400, "The fileIdList field is required");
    };
    // 全树查（info 无父信息面）。
    let found = st.dirs.values().flatten().find(|e| e.fid == fid).cloned();
    match found {
        Some(e) => ok_json(json!({ "infoList": [e.to_row(0)] })),
        None => ok_json(json!({ "infoList": [] })),
    }
}

async fn upload_request(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let mut st = state.lock().unwrap();
    count_hit(&mut st, "/a/api/file/upload_request");
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return err_code(400, "bad json");
    };
    let name = v.get("fileName").and_then(|n| n.as_str()).unwrap_or("");
    let parent = v
        .get("parentFileId")
        .and_then(|p| p.as_i64())
        .unwrap_or(0)
        .to_string();
    let is_dir = v.get("type").and_then(|t| t.as_i64()) == Some(1);
    if !st.dirs.contains_key(&parent) {
        return err_code(1024, "父目录不存在");
    }
    if st
        .dirs
        .get(&parent)
        .is_some_and(|rows| rows.iter().any(|e| e.name == name))
    {
        // 5060 真机形态：code=5060 + data{etag,size,updated_at}。
        return Json(json!({
            "code": 5060,
            "message": "检测到1个同名文件",
            "data": {"etag": "x", "size": 1, "updated_at": 0}
        }))
        .into_response();
    }
    let fid = st.next_id;
    st.next_id += 1;
    let size = if is_dir {
        987654
    } else {
        v.get("size").and_then(|s| s.as_i64()).unwrap_or(0)
    };
    st.dirs.entry(parent).or_default().push(StubEntry {
        name: name.to_string(),
        fid,
        is_dir,
        size,
        data: Vec::new(),
    });
    if is_dir {
        st.dirs.insert(fid.to_string(), Vec::new());
    }
    let now = "2026-09-20T15:33:39.481603578+08:00";
    ok_json(json!({
        "FileId": 0,
        "Info": {
            "FileId": fid,
            "FileName": name,
            "Type": if is_dir { 1 } else { 0 },
            "Size": size,
            "CreateAt": now,
            "UpdateAt": now,
            "S3KeyFlag": "4006416717-0",
            "StorageNode": "m0",
            "Trashed": false,
        },
        "Reuse": false,
        "SliceSize": "16777216",
        "UploadId": "",
    }))
}

async fn trash(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let mut st = state.lock().unwrap();
    count_hit(&mut st, "/a/api/file/trash");
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return err_code(400, "bad json");
    };
    // 载荷形状记录（§5.11）：**恰为**两键——fileTrashInfoList[{大写
    // FileId}] + event:"intoRecycle"。`trash_silent_trap` 旋钮 = 无条件
    // code=0 但不删（服务端漂移使正确载荷也静默失效的建模——回读校验
    // 的用例面；形状校验保留为可观测约束，驱动载荷错形由回读间接揭出）。
    let shape_ok = v.get("event").and_then(|e| e.as_str()) == Some("intoRecycle")
        && v.as_object().is_some_and(|m| m.len() == 2)
        && v.get("fileTrashInfoList")
            .and_then(|l| l.as_array())
            .is_some_and(|a| {
                a.first()
                    .and_then(|it| it.get("FileId"))
                    .and_then(|f| f.as_i64())
                    .is_some()
            });
    let _ = shape_ok;
    let fid = v
        .get("fileTrashInfoList")
        .and_then(|l| l.as_array())
        .and_then(|a| a.first())
        .and_then(|it| it.get("FileId"))
        .and_then(|f| f.as_i64());
    if st.trash_silent_trap {
        return ok_json(json!({})); // code=0，实际不删
    }
    if let Some(fid) = fid {
        for rows in st.dirs.values_mut() {
            rows.retain(|e| e.fid != fid);
        }
        st.dirs.remove(&fid.to_string());
    }
    ok_json(json!({}))
}

async fn rename(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let mut st = state.lock().unwrap();
    count_hit(&mut st, "/a/api/file/rename");
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return err_code(400, "bad json");
    };
    let fid = v.get("fileId").and_then(|f| f.as_i64());
    let name = v.get("fileName").and_then(|n| n.as_str()).unwrap_or("");
    let Some(fid) = fid else {
        return err_code(400, "fileId required");
    };
    let mut found = None;
    for rows in st.dirs.values_mut() {
        for e in rows.iter_mut() {
            if e.fid == fid {
                e.name = name.to_string();
                found = Some(e.clone());
            }
        }
    }
    match found {
        Some(e) => ok_json(json!({
            "Info": e.to_row(0),
            "FormatUpdateAt": "2026-09-20 15:33:39",
        })),
        None => err_code(1024, "文件不存在"),
    }
}

async fn mod_pid(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let mut st = state.lock().unwrap();
    count_hit(&mut st, "/b/api/file/mod_pid");
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return err_code(400, "bad json");
    };
    // wire 形态校验：fileIdList[{大写 FileId}] + parentFileId + event。
    let fid = v
        .get("fileIdList")
        .and_then(|l| l.as_array())
        .and_then(|a| a.first())
        .and_then(|it| it.get("FileId"))
        .and_then(|f| f.as_i64());
    let target = v.get("parentFileId").and_then(|p| p.as_i64());
    let event = v.get("event").and_then(|e| e.as_str());
    let (Some(fid), Some(target)) = (fid, target) else {
        return err_code(400, "fileIdList/parentFileId required");
    };
    if event != Some("fileMove") {
        return err_code(400, "event must be fileMove");
    }
    let target_key = target.to_string();
    if !st.dirs.contains_key(&target_key) {
        return err_code(1024, "目标目录不存在");
    }
    let mut moved: Option<StubEntry> = None;
    for rows in st.dirs.values_mut() {
        if let Some(idx) = rows.iter().position(|e| e.fid == fid) {
            moved = Some(rows.remove(idx));
            break;
        }
    }
    match moved {
        Some(e) => {
            st.dirs.entry(target_key).or_default().push(e);
            ok_json(json!({}))
        }
        None => err_code(1024, "文件不存在"),
    }
}

async fn traffic_check(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let mut st = state.lock().unwrap();
    count_hit(&mut st, "/b/api/file/download/traffic/check");
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return err_code(400, "bad json");
    };
    if v.get("fids").and_then(|f| f.as_array()).is_none() {
        return err_code(400, "The Fids field is required");
    }
    ok_json(json!({
        "isTrafficExceeded": st.traffic_exceeded,
        "originalRemainTraffic": 10735321088i64,
        "isBlocked": true,
        "clientFileSize": 1048576,
    }))
}

async fn download_info(
    State(state): State<Arc<Mutex<StubState>>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let mut st = state.lock().unwrap();
    count_hit(&mut st, "/a/api/file/download_info");
    if st.download_info_code != 0 {
        let code = st.download_info_code;
        return err_code(code, "流量限额");
    }
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return err_code(400, "bad json");
    };
    let fid = v.get("fileId").and_then(|f| f.as_i64()).unwrap_or(0);
    let host = host_of(&headers);
    let url = match st.chain_mode.as_str() {
        // 真机三跳形态：web-pro2 中继 URL（params= 携真链——纯解码零
        // GET）→ CDN 210 JSON → 镜像 206。
        "relay" => {
            let inner = format!("http://{host}/redirect/{fid}");
            let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(inner.as_bytes());
            format!("http://{host}/download-v2/?params={b64}&is_s3=0")
        }
        "direct-cdn" => format!("http://{host}/redirect/{fid}"),
        "location" => format!("http://{host}/loc/{fid}"),
        "html" => format!("http://{host}/html/{fid}"),
        "loop" => format!("http://{host}/loop/{fid}"),
        other => {
            return err_code(400, &format!("unknown chain mode {other}"));
        }
    };
    ok_json(json!({ "DownloadUrl": url }))
}

// ---------------------------------------------------------------------
// 传输面路由（CDN/中继）
// ---------------------------------------------------------------------

async fn redirect_get(
    State(state): State<Arc<Mutex<StubState>>>,
    Path(fid): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let mut st = state.lock().unwrap();
    count_hit(&mut st, "/redirect");
    // 210 + JSON 重定向体（真机实证形态）。
    let host = host_of(&headers);
    let next = format!("http://{host}/mirror/{fid}");
    (
        StatusCode::from_u16(210).expect("210 is a valid status"),
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        format!("{{\"code\":0,\"data\":{{\"redirect_url\":\"{next}\"}}}}"),
    )
        .into_response()
}

async fn loc_get(
    State(state): State<Arc<Mutex<StubState>>>,
    Path(fid): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let mut st = state.lock().unwrap();
    count_hit(&mut st, "/loc");
    let host = host_of(&headers);
    let next = format!("http://{host}/mirror/{fid}");
    Response::builder()
        .status(StatusCode::FOUND)
        .header(axum::http::header::LOCATION, next)
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn html_get(
    State(state): State<Arc<Mutex<StubState>>>,
    Path(fid): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let mut st = state.lock().unwrap();
    count_hit(&mut st, "/html");
    // 200 HTML 带 href 的防御形态（真机中继页无 href——此为 href 扫描
    // 腿的桩面）。
    let host = host_of(&headers);
    let next = format!("http://{host}/mirror/{fid}");
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        format!("<html><body><a href=\"{next}\">go</a></body></html>"),
    )
        .into_response()
}

async fn loop_get(
    State(state): State<Arc<Mutex<StubState>>>,
    Path(fid): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let mut st = state.lock().unwrap();
    count_hit(&mut st, "/loop");
    let host = host_of(&headers);
    let next = format!("http://{host}/loop/{fid}");
    (
        StatusCode::from_u16(210).expect("210 is a valid status"),
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        format!("{{\"code\":0,\"data\":{{\"redirect_url\":\"{next}\"}}}}"),
    )
        .into_response()
}

async fn mirror_get(
    State(state): State<Arc<Mutex<StubState>>>,
    Path(fid): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let mut st = state.lock().unwrap();
    count_hit(&mut st, "/mirror");
    if st.mirror_dead {
        return (StatusCode::NOT_FOUND, "gone").into_response();
    }
    let data = st
        .dirs
        .values()
        .flatten()
        .find(|e| e.fid == fid)
        .map(|e| e.data.clone());
    let Some(data) = data else {
        return (StatusCode::NOT_FOUND, "no such file").into_response();
    };
    if st.mirror_ignore_range {
        return (StatusCode::OK, data).into_response();
    }
    let size = data.len() as u64;
    let range = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match range {
        Some(r) => {
            let spec = r.strip_prefix("bytes=").unwrap_or(&r);
            let mut parts = spec.splitn(2, '-');
            let start: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let end_incl: Option<u64> = parts.next().and_then(|s| s.parse().ok());
            if start >= size {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .body(axum::body::Body::empty())
                    .unwrap();
            }
            let end = end_incl.map(|e| e + 1).unwrap_or(size).min(size);
            let slice = data[start as usize..end as usize].to_vec();
            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(
                    axum::http::header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, end - 1, size),
                )
                .body(axum::body::Body::from(slice))
                .unwrap()
        }
        None => (StatusCode::OK, data).into_response(),
    }
}
