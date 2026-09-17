//! 假 115 开放平台 + 假 OSS 端点的共享桩（Phase 5 / 115-4）。
//!
//! 一个 loopback 进程内的双层服务：
//!
//! - **开放平台面**（`/open/*`）：`user/info`、`ufile/files`、
//!   `folder/get_info|add`、`ufile/delete|update|move|downurl`、
//!   `upload/init|get_token|resume`——返回 115 真机形态的 envelope
//!   （`state`/`errno`/`data`，HTTP 200 错误包，K69.7）；
//! - **OSS 对象面**（`/{bucket}/{object}`，path-style）：PutObject /
//!   Initiate?uploads / UploadPart?partNumber / Complete?uploadId，
//!   `get_token` 返回带 scheme 的 loopback 端点 → oss.rs 的 scheme 缝
//!   让对象面也走本桩（K69.5「端点可控」的离线形态）。
//!
//! 观测面（harness 断言用）：`object_bytes_received()`（对象面累计
//! 收到的 body 字节——断言⑦的差集观测点）、`inject_stat_error()`（断言
//! ⑤的一次性后端错误注入）、`fail_part_after()`（分片级 resume 用例的
//! 中断注入）。
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use ck_pan115::limiter::LimiterConfig;
use ck_pan115::{Pan115Driver, Pan115Params};
use serde_json::{json, Value};

/// 对象（分片会话 + 固化内容）。
#[derive(Clone, Default)]
pub struct StubObj {
    pub parts: HashMap<u32, Vec<u8>>,
    pub etags: HashMap<u32, String>,
    pub upload_id: String,
    pub data: Vec<u8>,
    pub completed: bool,
}

/// 桩内的一条目录条目：(名字, fid, 是否目录, 字节数, pick_code)。
pub type StubEntry = (String, String, bool, i64, String);

#[derive(Default)]
pub struct StubState {
    /// cid → 子条目清单（文件树；条目形态见 [`StubEntry`]）
    pub dirs: HashMap<String, Vec<StubEntry>>,
    /// fid → 对象 key（文件内容反查）
    pub fid_obj: HashMap<String, String>,
    pub objs: HashMap<String, StubObj>,
    pub next_id: u64,
    /// OSS 对象面累计收到字节（断言⑦观测点）
    pub object_bytes: u64,
    /// 一次性 stat 错误注入（断言⑤）
    pub stat_inject: Option<String>,
    /// **持续** user/info 错误注入（probe 分类面——H-T2：NeedsReauth/
    /// RateLimited/Unreachable 三变体的触发器；不 take，恒失败）
    pub user_info_inject: Option<String>,
    /// 第 N 片之后的 PUT 失败注入（分片级 resume 用例）
    pub fail_part_after: Option<u32>,
    pub part_puts: u32,
    /// 二次认证轮数（init 首轮返回挑战的次数）
    pub auth_rounds: u32,
    /// 已收到挑战回带的 init 调用（sign_val 断言）
    pub sign_vals: Vec<String>,
    /// downurl 响应里的 CDN base（下载流经此）
    pub cdn_base: String,
    /// 强制 complete 后 size 报 0（复核负例）
    pub force_zero_size: bool,
    /// 对象面是否要求 UA 一致（K69.4 绑定的桩面）
    pub ua_binding: bool,
    /// object key → (目标文件名, 目标 cid)（init 记录；complete/put
    /// 固化成可见条目）
    pub pending_names: HashMap<String, (String, String)>,
    /// pick_code → object key（resume 会话复用面：同一 pick_code 必须
    /// 回到同一对象——115 真机的续传语义）。
    pub pc_objects: HashMap<String, String>,
}

pub struct OsStub {
    pub state: Arc<Mutex<StubState>>,
    pub base: String,
}

impl OsStub {
    pub async fn start() -> OsStub {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub listener");
        let addr = listener.local_addr().expect("stub addr");
        let base = format!("http://{addr}");
        let state = Arc::new(Mutex::new(StubState {
            next_id: 5000,
            cdn_base: base.clone(),
            ua_binding: true,
            ..StubState::default()
        }));
        {
            // 根目录（cid "0"）
            let mut st = state.lock().unwrap();
            st.dirs.insert("0".to_string(), Vec::new());
        }
        let app = Router::new()
            .route("/open/user/info", get(user_info))
            .route("/open/refreshToken", post(refresh_token))
            .route("/open/ufile/files", get(ufile_files))
            .route("/open/folder/get_info", get(folder_get_info))
            .route("/open/folder/add", post(folder_add))
            .route("/open/ufile/delete", post(ufile_delete))
            .route("/open/ufile/update", post(ufile_update))
            .route("/open/ufile/move", post(ufile_move))
            .route("/open/ufile/downurl", post(ufile_downurl))
            .route("/open/upload/init", post(upload_init))
            .route("/open/upload/get_token", get(upload_get_token))
            .route("/open/upload/resume", post(upload_resume))
            .route("/cdn/{pc}", get(cdn_get))
            .route("/cdn/{pc}", axum::routing::head(cdn_head))
            .route("/{bucket}/{*object}", put(oss_put))
            .route("/{bucket}/{*object}", post(oss_post))
            .route("/{bucket}/{*object}", get(oss_list_parts))
            .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024))
            .with_state(state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.expect("stub serve") });
        OsStub { state, base }
    }

    /// 构造驱动参数（probe 面单测用；与 [`OsStub::driver`] 同源）。
    pub fn params(&self) -> Pan115Params {
        Pan115Params {
            client_id: "100197303".to_string(),
            access_token: Some("stub-access".to_string()),
            refresh_token: Some("stub-refresh".to_string()),
            root: "0".to_string(),
            api_base: self.base.clone(),
            passport_base: self.base.clone(),
            token_store: None,
            limiter: Some(LimiterConfig::fast()),
            sessions_dir: None,
        }
    }

    /// 构造被测驱动（api/passport/cdn 全指桩；OSS 端点由 get_token 给）。
    pub fn driver(&self) -> Pan115Driver {
        Pan115Driver::new(Pan115Params {
            client_id: "100197303".to_string(),
            access_token: Some("stub-access".to_string()),
            refresh_token: Some("stub-refresh".to_string()),
            root: "0".to_string(),
            api_base: self.base.clone(),
            passport_base: self.base.clone(),
            token_store: None,
            limiter: Some(LimiterConfig::fast()),
            sessions_dir: None,
        })
        .expect("stub driver")
    }

    pub fn object_bytes_received(&self) -> u64 {
        self.state.lock().unwrap().object_bytes
    }

    pub fn part_put_count(&self) -> u32 {
        self.state.lock().unwrap().part_puts
    }

    /// probe 分类注入：user/info 恒返回该错误码（H-T2——持续，非一次性）。
    pub async fn inject_user_info_error(&self, code: &str) {
        self.state.lock().unwrap().user_info_inject = Some(code.to_string());
    }

    /// 断言⑤注入：下一次 stat（= ufile/files 的目录解析面）返回该码。
    pub async fn inject_stat_error(&self, code: &str) {
        self.state.lock().unwrap().stat_inject = Some(code.to_string());
    }

    /// 分片级 resume 用例：第 N 片**之后**的 PUT 失败。
    pub async fn fail_part_after(&self, n: u32) {
        self.state.lock().unwrap().fail_part_after = Some(n);
    }

    pub async fn clear_part_failure(&self) {
        self.state.lock().unwrap().fail_part_after = None;
    }

    /// 在根下建一个目录（测试准备面）。
    pub fn mkdir(&self, parent: &str, name: &str) -> String {
        let mut st = self.state.lock().unwrap();
        let fid = st.next_id.to_string();
        st.next_id += 1;
        st.dirs.entry(parent.to_string()).or_default().push((
            name.to_string(),
            fid.clone(),
            true,
            0,
            String::new(),
        ));
        st.dirs.entry(fid.clone()).or_default();
        fid
    }

    /// 在目录下放一个文件（带内容；测试准备面）。
    pub fn put_file(&self, parent: &str, name: &str, data: Vec<u8>) -> String {
        let mut st = self.state.lock().unwrap();
        let fid = st.next_id.to_string();
        st.next_id += 1;
        let pc = format!("pc-{fid}");
        let key = format!("obj-{fid}");
        let size = data.len() as i64;
        st.objs.insert(
            key.clone(),
            StubObj {
                data,
                completed: true,
                ..StubObj::default()
            },
        );
        st.dirs.entry(parent.to_string()).or_default().push((
            name.to_string(),
            fid.clone(),
            false,
            size,
            pc,
        ));
        st.fid_obj.insert(fid.clone(), key);
        fid
    }
}

// ---------------------------------------------------------------------
// 路由实现
// ---------------------------------------------------------------------

fn ok_json(data: Value) -> Response {
    Json(json!({"state": true, "errno": 0, "data": data})).into_response()
}

fn err_json_code(code: i64, message: &str) -> Response {
    // 真机形态（K69.7）：HTTP 200 + **布尔 state:false** + code；
    // passportapi 面用数字 state:0——两形态驱动都接受，此处钉 proapi
    // 形态（错误包的判据是 state 非真值，不是 errno 字段）。
    Json(json!({
        "state": false,
        "code": code,
        "errno": code,
        "message": message,
        "data": {}
    }))
    .into_response()
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

async fn user_info(State(state): State<Arc<Mutex<StubState>>>) -> Response {
    if let Some(code) = state.lock().unwrap().user_info_inject.clone() {
        let code: i64 = code.parse().unwrap_or(0);
        return err_json_code(code, "injected");
    }
    ok_json(json!({
        "user_id": 1205495,
        "user_name": "stub",
        "rt_space_info": {
            "all_total": {"size": 201834401841285i64},
            "all_use": {"size": 76593764015308i64},
            "all_remain": {"size": 125240637825977i64}
        }
    }))
}

/// `/open/refreshToken`：恒失败（errno 99 = token 过期族——「死
/// refresh_token」形态；probe 的 NeedsReauth 腿消费。成功刷新路径由
/// oauth_state_machine.rs 的专用 mock 覆盖，不经本桩）。
async fn refresh_token() -> Response {
    err_json_code(99, "refresh token invalid")
}

async fn ufile_files(
    State(state): State<Arc<Mutex<StubState>>>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let mut st = state.lock().unwrap();
    if let Some(code) = st.stat_inject.take() {
        // 断言⑤：一次性后端错误注入（stat 的目录解析面）
        let code: i64 = code.parse().unwrap_or(0);
        return err_json_code(code, "injected");
    }
    let cid = q.get("cid").cloned().unwrap_or_else(|| "0".to_string());
    let Some(entries) = st.dirs.get(&cid) else {
        return err_json_code(430004, "目录不存在");
    };
    let rows: Vec<Value> = entries
        .iter()
        .map(|(name, fid, is_dir, size, pc)| {
            json!({
                "fid": fid,
                "fc": if *is_dir { "0" } else { "1" },
                "fs": size,
                "fn": name,
                "pc": pc,
                "sha1": "",
                "upt": 1_700_000_000i64,
            })
        })
        .collect();
    let count = rows.len() as i64;
    Json(json!({"state": true, "errno": 0, "data": rows, "count": count})).into_response()
}

async fn folder_get_info(
    State(state): State<Arc<Mutex<StubState>>>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let st = state.lock().unwrap();
    let fid = q.get("file_id").cloned().unwrap_or_default();
    // 文件：fid_obj 反查；目录：dirs 查找
    if let Some(key) = st.fid_obj.get(&fid) {
        let obj = st.objs.get(key);
        let size = if st.force_zero_size {
            0
        } else {
            obj.map(|o| o.data.len() as i64).unwrap_or(0)
        };
        let name = st
            .dirs
            .values()
            .flatten()
            .find(|(_, f, _, _, _)| f == &fid)
            .map(|(n, _, _, _, _)| n.clone())
            .unwrap_or_default();
        let pc = format!("pc-{fid}");
        return ok_json(json!({
            "file_id": fid,
            "file_name": name,
            "file_category": "1",
            "pick_code": pc,
            "sha1": "",
            "size_byte": size,
            "size": size.to_string(),
        }));
    }
    let name = st
        .dirs
        .values()
        .flatten()
        .find(|(_, f, _, _, _)| f == &fid)
        .map(|(n, _, _, _, _)| n.clone());
    match name {
        Some(n) => ok_json(json!({
            "file_id": fid,
            "file_name": n,
            "file_category": "0",
            "pick_code": "",
            "sha1": "",
            "size_byte": 0,
            "size": "0",
        })),
        None => err_json_code(430004, "文件不存在"),
    }
}

async fn folder_add(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let form = parse_form(&body);
    let pid = form.get("pid").cloned().unwrap_or_else(|| "0".to_string());
    let name = form.get("file_name").cloned().unwrap_or_default();
    let mut st = state.lock().unwrap();
    if !st.dirs.contains_key(&pid) {
        return err_json_code(430004, "父目录不存在");
    }
    if st
        .dirs
        .get(&pid)
        .is_some_and(|v| v.iter().any(|(n, _, _, _, _)| n == &name))
    {
        return err_json_code(430001, "同名已存在");
    }
    let fid = st.next_id.to_string();
    st.next_id += 1;
    st.dirs
        .entry(pid)
        .or_default()
        .push((name.clone(), fid.clone(), true, 0, String::new()));
    st.dirs.entry(fid.clone()).or_default();
    ok_json(json!({"file_name": name, "file_id": fid}))
}

async fn ufile_delete(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let form = parse_form(&body);
    let fid = form.get("file_ids").cloned().unwrap_or_default();
    let mut st = state.lock().unwrap();
    let Some(pos) = st
        .dirs
        .values()
        .flatten()
        .find(|(_, f, _, _, _)| f == &fid)
        .map(|(n, _, _, _, _)| n.clone())
    else {
        return err_json_code(430004, "文件不存在");
    };
    for entries in st.dirs.values_mut() {
        entries.retain(|(n, _, _, _, _)| n != &pos);
    }
    st.dirs.remove(&fid);
    st.fid_obj.remove(&fid);
    ok_json(json!([]))
}

async fn ufile_update(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let form = parse_form(&body);
    let fid = form.get("file_id").cloned().unwrap_or_default();
    let new_name = form.get("file_name").cloned().unwrap_or_default();
    let mut st = state.lock().unwrap();
    let mut found = false;
    for entries in st.dirs.values_mut() {
        for (n, f, _, _, _) in entries.iter_mut() {
            if f == &fid {
                *n = new_name.clone();
                found = true;
            }
        }
    }
    if found {
        ok_json(json!({"file_name": new_name}))
    } else {
        err_json_code(430004, "文件不存在")
    }
}

async fn ufile_move(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let form = parse_form(&body);
    let fid = form.get("file_ids").cloned().unwrap_or_default();
    // SDK 文档形态（115-sdk-go MoveReq：file_ids + to_cid）——真机实证
    // （2026-09-17）to_pid 形态是静默黑洞，桩按文档严格建模（缺参即拒）。
    let Some(to) = form.get("to_cid").cloned() else {
        return err_json_code(701000, "参数错误：缺少 to_cid");
    };
    let mut st = state.lock().unwrap();
    if !st.dirs.contains_key(&to) {
        return err_json_code(430004, "目标目录不存在");
    }
    let mut moved: Option<(String, String, bool, i64, String)> = None;
    for entries in st.dirs.values_mut() {
        if let Some(idx) = entries.iter().position(|(_, f, _, _, _)| f == &fid) {
            moved = Some(entries.remove(idx));
            break;
        }
    }
    match moved {
        Some(row) => {
            st.dirs.entry(to).or_default().push(row);
            ok_json(json!([]))
        }
        None => err_json_code(430004, "文件不存在"),
    }
}

async fn ufile_downurl(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let form = parse_form(&body);
    let vfs = state.lock().unwrap();
    let pc = form.get("pick_code").cloned().unwrap_or_default();
    let known = vfs.dirs.values().flatten().any(|(_, _, _, _, p)| p == &pc);
    if !known {
        return err_json_code(430004, "pick_code 无效");
    }
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

async fn upload_init(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let form = parse_form(&body);
    let name = form.get("file_name").cloned().unwrap_or_default();
    let size: i64 = form
        .get("file_size")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let sign_key = form.get("sign_key").cloned();
    let sign_val = form.get("sign_val").cloned();
    let mut st = state.lock().unwrap();
    st.next_id += 1;
    let pick_code = format!("pc-new-{}", st.next_id);
    if let Some(sv) = sign_val {
        st.sign_vals.push(sv);
    }
    // 二次认证挑战（K69.2 形态：仅首轮）
    if sign_key.is_none() && st.auth_rounds > 0 {
        st.auth_rounds -= 1;
        let end = (size / 2).max(1) - 1;
        return ok_json(json!({
            "pick_code": pick_code,
            "status": 8,
            "sign_key": "stub-sign-key",
            "sign_check": format!("0-{end}"),
        }));
    }
    // 常规 init：创建对象会话
    let key = format!("obj-new-{}", st.next_id);
    st.objs.insert(
        key.clone(),
        StubObj {
            upload_id: String::new(),
            ..StubObj::default()
        },
    );
    let cid = form
        .get("target")
        .and_then(|t| t.strip_prefix("U_1_"))
        .unwrap_or("0")
        .to_string();
    st.pending_names.insert(key.clone(), (name.clone(), cid));
    st.pc_objects.insert(pick_code.clone(), key.clone());
    ok_json(json!({
        "pick_code": pick_code,
        "status": 1,
        "bucket": "stubbucket",
        "object": key,
        "callback": {
            "callback": "{\"callbackUrl\":\"http://cb\"}",
            "callback_var": "{}"
        }
    }))
}

async fn upload_get_token(State(state): State<Arc<Mutex<StubState>>>) -> Response {
    let base = state.lock().unwrap().cdn_base.clone();
    ok_json(json!({
        "endpoint": base,
        "AccessKeyId": "stub-ak",
        "AccessKeySecret": "stub-sk",
        "SecurityToken": "stub-sts",
        "Expiration": "2030-01-01T00:00:00Z"
    }))
}

async fn upload_resume(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let form = parse_form(&body);
    let pc = form.get("pick_code").cloned().unwrap_or_default();
    let st = state.lock().unwrap();
    // 会话复用（115 真机语义）：同一 pick_code → 同一 object（含
    // 已传分片与 uploadId——差集续传的载体）。
    let Some(key) = st.pc_objects.get(&pc).cloned() else {
        return err_json_code(430004, "pick_code 无效");
    };
    ok_json(json!({
        "pick_code": pc,
        "bucket": "stubbucket",
        "object": key,
        "callback": {"callback": "{}", "callback_var": "{}"}
    }))
}

async fn cdn_head(
    State(state): State<Arc<Mutex<StubState>>>,
    axum::extract::Path(pc): axum::extract::Path<String>,
) -> Response {
    let st = state.lock().unwrap();
    let ok = st.dirs.values().flatten().any(|(_, fid, _, size, p)| {
        p == &pc
            && st
                .fid_obj
                .get(fid)
                .and_then(|k| st.objs.get(k))
                .is_some_and(|o| !o.data.is_empty() || *size == 0)
    });
    if !ok {
        return (StatusCode::NOT_FOUND, "no such pick_code").into_response();
    }
    Response::builder()
        .status(StatusCode::OK)
        .header("accept-ranges", "bytes")
        .header("etag", format!("\"etag-{pc}\""))
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn cdn_get(
    State(state): State<Arc<Mutex<StubState>>>,
    axum::extract::Path(pc): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Response {
    let st = state.lock().unwrap();
    // UA 绑定（K69.4 桩面）：取链与下载必须同一 UA
    if st.ua_binding {
        let ua = headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if ua != ck_pan115::UA {
            return (StatusCode::FORBIDDEN, "ua mismatch").into_response();
        }
    }
    let target = st.dirs.values().flatten().find(|(_, _, _, _, p)| p == &pc);
    let data = target
        .and_then(|(_, fid, _, _, _)| st.fid_obj.get(fid))
        .and_then(|k| st.objs.get(k))
        .map(|o| o.data.clone());
    let Some(data) = data else {
        return (StatusCode::NOT_FOUND, "no such pick_code").into_response();
    };
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
                    "content-range",
                    format!("bytes {}-{}/{}", start, end - 1, size),
                )
                .body(axum::body::Body::from(slice))
                .unwrap()
        }
        None => Response::builder()
            .status(StatusCode::OK)
            .body(axum::body::Body::from(data))
            .unwrap(),
    }
}

/// OSS PUT：分片（?partNumber&uploadId）或整对象（PutObject）。
async fn oss_put(
    State(state): State<Arc<Mutex<StubState>>>,
    axum::extract::Path((_bucket, object)): axum::extract::Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let q = query.unwrap_or_default();
    let mut st = state.lock().unwrap();
    let _ = headers;
    st.object_bytes += body.len() as u64;
    if !st.objs.contains_key(&object) {
        return (StatusCode::NOT_FOUND, "no such object").into_response();
    }
    if q.contains("partNumber=") {
        let n: u32 = q
            .split("partNumber=")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        // 分片级失败注入：第 fail_part_after 片之后失败（可重试类）；
        // 计数只记成功落地的片。
        if let Some(limit) = st.fail_part_after {
            if st.part_puts >= limit {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "<Error><Code>InternalError</Code></Error>",
                )
                    .into_response();
            }
        }
        st.part_puts += 1;
        let obj = st.objs.get_mut(&object).expect("checked");
        obj.parts.insert(n, body.to_vec());
        let etag = format!("\"etag-{n}-{}\"", body.len());
        obj.etags.insert(n, etag.clone());
        return Response::builder()
            .status(StatusCode::OK)
            .header("etag", etag)
            .body(axum::body::Body::empty())
            .unwrap();
    }
    // PutObject 整对象 → 固化 + 登记可见
    let data = body.to_vec();
    let size = data.len() as i64;
    {
        let obj = st.objs.get_mut(&object).expect("checked");
        obj.data = data;
        obj.completed = true;
    }
    register_visible(&mut st, &object, size);
    Response::builder()
        .status(StatusCode::OK)
        .header("etag", "\"etag-whole\"")
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn oss_post(
    State(state): State<Arc<Mutex<StubState>>>,
    axum::extract::Path((_bucket, object)): axum::extract::Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let q = query.unwrap_or_default();
    let mut st = state.lock().unwrap();
    let _ = headers;
    if q.contains("uploads") {
        st.next_id += 1;
        let uid = format!("uid-{}", st.next_id);
        if let Some(obj) = st.objs.get_mut(&object) {
            obj.upload_id = uid.clone();
        }
        return Response::builder()
            .status(StatusCode::OK)
            .body(axum::body::Body::from(format!(
                "<InitiateMultipartUploadResult><UploadId>{uid}</UploadId></InitiateMultipartUploadResult>"
            )))
            .unwrap();
    }
    if q.contains("uploadId=") {
        if !st.objs.contains_key(&object) {
            return (StatusCode::NOT_FOUND, "no such object").into_response();
        }
        let xml = String::from_utf8_lossy(&body).to_string();
        let mut order: Vec<u32> = Vec::new();
        let mut rest = xml.as_str();
        while let Some(i) = rest.find("<PartNumber>") {
            rest = &rest[i + "<PartNumber>".len()..];
            if let Some(j) = rest.find("</PartNumber>") {
                if let Ok(n) = rest[..j].parse::<u32>() {
                    order.push(n);
                }
                rest = &rest[j..];
            } else {
                break;
            }
        }
        let mut all = Vec::new();
        {
            let obj = st.objs.get(&object).expect("checked");
            for n in &order {
                if let Some(p) = obj.parts.get(n) {
                    all.extend_from_slice(p);
                }
            }
        }
        let size = all.len() as i64;
        {
            let obj = st.objs.get_mut(&object).expect("checked");
            obj.data = all;
            obj.completed = true;
        }
        register_visible(&mut st, &object, size);
        return Response::builder()
            .status(StatusCode::OK)
            .body(axum::body::Body::from(
                "<CompleteMultipartUploadResult><ETag>\"done\"</ETag></CompleteMultipartUploadResult>",
            ))
            .unwrap();
    }
    (StatusCode::BAD_REQUEST, "unsupported").into_response()
}

/// OSS GET：ListParts（`?uploadId=`）——已传分片的对账面（resume 差集
/// 的「远端真值」）。
async fn oss_list_parts(
    State(state): State<Arc<Mutex<StubState>>>,
    axum::extract::Path((_bucket, object)): axum::extract::Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let q = query.unwrap_or_default();
    let st = state.lock().unwrap();
    let Some(obj) = st.objs.get(&object) else {
        return (
            StatusCode::NOT_FOUND,
            "<Error><Code>NoSuchUpload</Code></Error>",
        )
            .into_response();
    };
    if !q.contains("uploadId=") {
        return (StatusCode::BAD_REQUEST, "not a ListParts call").into_response();
    }
    let mut xml = String::from("<ListPartsResult><IsTruncated>false</IsTruncated>");
    let mut nums: Vec<&u32> = obj.parts.keys().collect();
    nums.sort();
    for n in nums {
        let size = obj.parts[n].len();
        let etag = obj.etags.get(n).cloned().unwrap_or_default();
        xml.push_str(&format!(
            "<Part><PartNumber>{n}</PartNumber><ETag>{etag}</ETag><Size>{size}</Size></Part>"
        ));
    }
    xml.push_str("</ListPartsResult>");
    Response::builder()
        .status(StatusCode::OK)
        .body(axum::body::Body::from(xml))
        .unwrap()
}

/// 文件对象固化后登记进**目标目录**（list 可见面）。
///
/// init 记录的 `target`（`U_1_<cid>`）给出落点；conformance 的写入
/// 路径是 `conformance/a1/...` 这类嵌套路径——驱动侧会先 mkdir 出父
/// 目录（folder/add），这里的 cid 解析用「目录名 → cid」的桩内表。
fn register_visible(st: &mut StubState, object: &str, size: i64) {
    let Some(entry) = st.pending_names.remove(object) else {
        return;
    };
    let (name, cid) = entry;
    let fid = st.next_id.to_string();
    st.next_id += 1;
    let pc = format!("pc-{fid}");
    st.dirs
        .entry(cid)
        .or_default()
        .push((name, fid.clone(), false, size, pc));
    st.fid_obj.insert(fid, object.to_string());
}
