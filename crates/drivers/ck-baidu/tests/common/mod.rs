//! MockBaidu——axum 内存百度后端（K16；Phase 2 Batch B1 测试基建）。
//!
//! **落位注码**：Phase 2 执行计划 §3 Files 原文写 `tests/mock_backend.rs`——
//! Rust 集成测试的多套件共享基建必须落 `tests/common/mod.rs`（各套件
//! `mod common;` 引用），本模块即该计划的 mock baidu 后端落位。
//!
//! 行为语义（黄金参照：spike 实抓 + PCFS 源码，分歧以 spike 为准）：
//!
//! - **业务端点**（`/rest/2.0/xpan/file`、`/rest/2.0/xpan/nas`）校验
//!   query `access_token == current`，不匹配 → HTTP 200 + `{"errno":110}`
//!   （xpan 家族以 errno 而非 HTTP 状态承载业务错误）；
//! - **错误注入队列**优先于 token 校验与方法分发（下一个业务请求顶
//!   错误）——注意注入须在 driver 构造（uinfo 首调）之后进行，否则会
//!   顶掉 connect；
//! - **oauth 端点**（`/oauth/2.0/token`）校验 `refresh_token == current`：
//!   匹配则**一次一换**（轮换出全新 access/refresh 对，模拟 spike §1
//!   「refresh_token 一次一换、旧值即刻作废」实证），陈旧值 → HTTP 400 +
//!   `{"error":"invalid_grant"}`；
//! - **请求记录器**存 raw query/body 与关键头（content-type/user-agent），
//!   供三套件做表单**字节级断言**（`tests/metadata_ops.rs`）。

#![allow(dead_code)] // 三个测试二进制各自编译本模块，未用到的访问器按二进制豁免

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use serde_json::{json, Value};

use ck_baidu::{BaiduParams, TokenStore};

// ---------------------------------------------------------------------------
// 路径与常量（断言与编排用）
// ---------------------------------------------------------------------------

pub const XPAN_FILE: &str = "/rest/2.0/xpan/file";
pub const XPAN_NAS: &str = "/rest/2.0/xpan/nas";
pub const OAUTH_TOKEN: &str = "/oauth/2.0/token";

/// 测试根（与生产缺省 `baidu_root` 同形态，K17）。
pub const MOCK_ROOT: &str = "/apps/cloudfs";
/// 假凭据（占位形态，绝非真实凭据——R3）。
pub const MOCK_APP_KEY: &str = "mock-app-key";
pub const MOCK_APP_SECRET: &str = "mock-app-secret";
pub const INITIAL_ACCESS_TOKEN: &str = "mock-access-0";
pub const INITIAL_REFRESH_TOKEN: &str = "mock-refresh-0";
/// uinfo 返回的测试 uid（VolumeId 断言用）。
pub const MOCK_UID: i64 = 1400000001;
/// spike common.rs:16 的 netdisk UA——client 构造照抄，wire 断言防漂移。
pub const NETDISK_UA: &str = "netdisk;P2SP;2.2.91.136;android-android";

// ---------------------------------------------------------------------------
// 状态模型
// ---------------------------------------------------------------------------

/// 内存文件树条目（形态对齐后端 list/meta 响应项字段名）。
#[derive(Debug, Clone)]
pub struct MockEntry {
    pub path: String,
    pub isdir: bool,
    pub size: i64,
    pub fs_id: i64,
    pub server_mtime: i64,
    pub server_filename: String,
    pub md5: String,
}

/// 请求记录（raw query/body + 关键头——字节级断言的观测面）。
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub http_method: String,
    pub path: String,
    pub query: String,
    pub body: String,
    pub content_type: Option<String>,
    pub user_agent: Option<String>,
}

struct MockState {
    entries: Vec<MockEntry>,
    access_token: String,
    refresh_token: String,
    recorded: Vec<RecordedRequest>,
    inject: VecDeque<i64>,
    refresh_count: u64,
    uid: i64,
    quota_used: i64,
    quota_total: i64,
    next_fs_id: i64,
}

/// mock 后端句柄（访问器均同步短临界区，无 await 持锁）。
pub struct MockBaidu {
    state: Arc<Mutex<MockState>>,
    base_url: String,
}

impl MockBaidu {
    /// 启动内存后端（127.0.0.1 随机端口），返回 (句柄, base_url)。
    pub async fn start() -> (MockBaidu, String) {
        let state = Arc::new(Mutex::new(MockState {
            entries: Vec::new(),
            access_token: INITIAL_ACCESS_TOKEN.to_string(),
            refresh_token: INITIAL_REFRESH_TOKEN.to_string(),
            recorded: Vec::new(),
            inject: VecDeque::new(),
            refresh_count: 0,
            uid: MOCK_UID,
            quota_used: 123456789,
            quota_total: 1099511627776,
            next_fs_id: 671337245231600, // spike 实测量级（50bit fs_id 家族）
        }));
        let app = axum::Router::new()
            .route(XPAN_FILE, get(xpan_file).post(xpan_file))
            .route(XPAN_NAS, get(xpan_nas))
            .route(OAUTH_TOKEN, get(oauth_token))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock baidu");
        let addr = listener.local_addr().expect("mock baidu local addr");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock baidu accept loop");
        });
        let base_url = format!("http://{addr}");
        (
            MockBaidu {
                state,
                base_url: base_url.clone(),
            },
            base_url,
        )
    }

    // -- 种子/编排访问器 ----------------------------------------------------

    /// 播种完整条目（全字段控制）。
    pub fn seed_entry(&self, entry: MockEntry) {
        self.state.lock().unwrap().entries.push(entry);
    }

    /// 播种目录，返回分配的 fs_id。
    pub fn seed_dir(&self, path: &str) -> i64 {
        let mut st = self.state.lock().unwrap();
        let fs_id = st.next_fs_id;
        st.next_fs_id += 1;
        st.entries.push(MockEntry {
            path: path.to_string(),
            isdir: true,
            size: 0,
            fs_id,
            server_mtime: 1757000000,
            server_filename: path.rsplit('/').next().unwrap_or_default().to_string(),
            md5: String::new(),
        });
        fs_id
    }

    /// 播种文件，返回分配的 fs_id（Entry.handle 断言用）。
    pub fn seed_file(&self, path: &str, size: i64, server_mtime: i64) -> i64 {
        let mut st = self.state.lock().unwrap();
        let fs_id = st.next_fs_id;
        st.next_fs_id += 1;
        st.entries.push(MockEntry {
            path: path.to_string(),
            isdir: false,
            size,
            fs_id,
            server_mtime,
            server_filename: path.rsplit('/').next().unwrap_or_default().to_string(),
            md5: format!("{fs_id:032x}"),
        });
        fs_id
    }

    /// 顶替当前有效 access_token（使旧值失效——110 场景编排）。
    pub fn set_access_token(&self, token: &str) {
        self.state.lock().unwrap().access_token = token.to_string();
    }

    /// 注入 errno：下一个**业务**请求（xpan/file、xpan/nas）顶此错误。
    pub fn inject_errno(&self, errno: i64) {
        self.state.lock().unwrap().inject.push_back(errno);
    }

    pub fn set_quota(&self, used: i64, total: i64) {
        let mut st = self.state.lock().unwrap();
        st.quota_used = used;
        st.quota_total = total;
    }

    // -- 观测访问器 ----------------------------------------------------------

    /// 请求记录快照（按到达序）。
    pub fn recorded(&self) -> Vec<RecordedRequest> {
        self.state.lock().unwrap().recorded.clone()
    }

    pub fn refresh_count(&self) -> u64 {
        self.state.lock().unwrap().refresh_count
    }

    /// 当前有效 token 对（oauth 刷新后即轮换值）。
    pub fn current_tokens(&self) -> (String, String) {
        let st = self.state.lock().unwrap();
        (st.access_token.clone(), st.refresh_token.clone())
    }

    /// 构造指向本 mock 的驱动参数（api/oauth base 均注入 mock URL）。
    pub fn params(&self, token_store: Option<Arc<dyn TokenStore>>) -> BaiduParams {
        let (access, refresh) = self.current_tokens();
        BaiduParams {
            app_key: MOCK_APP_KEY.to_string(),
            app_secret: MOCK_APP_SECRET.to_string(),
            access_token: Some(access),
            refresh_token: Some(refresh),
            root: MOCK_ROOT.to_string(),
            api_base: self.base_url.clone(),
            oauth_base: self.base_url.clone(),
            token_store,
        }
    }
}

// ---------------------------------------------------------------------------
// axum 处理器
// ---------------------------------------------------------------------------

type Shared = Arc<Mutex<MockState>>;

fn recorded_request(
    http_method: &str,
    path: &str,
    raw_query: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> RecordedRequest {
    RecordedRequest {
        http_method: http_method.to_string(),
        path: path.to_string(),
        query: raw_query.to_string(),
        body: String::from_utf8_lossy(body).into_owned(),
        content_type: headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        user_agent: headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
    }
}

/// 业务端点公共闸门：记录 → 注入顶错 → token 校验（110）。`Some` = 短路响应。
fn business_gate(
    state: &Shared,
    http_method: &str,
    path: &str,
    raw_query: &str,
    pairs: &[(String, String)],
    headers: &HeaderMap,
    body: &Bytes,
) -> Option<Response> {
    let mut st = state.lock().unwrap();
    st.recorded.push(recorded_request(
        http_method,
        path,
        raw_query,
        headers,
        body,
    ));
    if let Some(errno) = st.inject.pop_front() {
        return Some(Json(json!({"errno": errno, "errmsg": "injected"})).into_response());
    }
    let current = st.access_token.clone();
    drop(st);
    let token_ok = pairs
        .iter()
        .any(|(k, v)| k == "access_token" && v == &current);
    if !token_ok {
        return Some(Json(json!({"errno": 110, "errmsg": "invalid access token"})).into_response());
    }
    None
}

fn errno_json(errno: i64) -> Response {
    Json(json!({"errno": errno})).into_response()
}

fn parent_of(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(parent, _)| parent)
}

fn entry_json(e: &MockEntry) -> Value {
    json!({
        "fs_id": e.fs_id,
        "path": e.path,
        "server_filename": e.server_filename,
        "size": e.size,
        "isdir": if e.isdir { 1 } else { 0 },
        "md5": e.md5,
        "server_mtime": e.server_mtime,
    })
}

/// `/rest/2.0/xpan/file`：query `method` 分发（list/meta/quota/create/filemanager）。
async fn xpan_file(
    State(state): State<Shared>,
    method: Method,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let raw_query = raw.unwrap_or_default();
    let pairs = parse_urlencoded(&raw_query);
    if let Some(resp) = business_gate(
        &state,
        method.as_str(),
        XPAN_FILE,
        &raw_query,
        &pairs,
        &headers,
        &body,
    ) {
        return resp;
    }
    let qp = |k: &str| pairs.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone());
    match (method.as_str(), qp("method").unwrap_or_default().as_str()) {
        ("GET", "list") => {
            let dir = qp("dir").unwrap_or_default();
            let st = state.lock().unwrap();
            if !st.entries.iter().any(|e| e.isdir && e.path == dir) {
                return errno_json(-9); // 目录不存在
            }
            let list: Vec<Value> = st
                .entries
                .iter()
                .filter(|e| parent_of(&e.path) == Some(dir.as_str()))
                .map(entry_json)
                .collect();
            Json(json!({"errno": 0, "list": list})).into_response()
        }
        ("GET", "meta") => {
            let st = state.lock().unwrap();
            let found = if let Some(fs_ids) = qp("fs_ids") {
                // 形态 "[<id>,…]"（PCFS api.go:176-179）
                let inner = fs_ids.trim_start_matches('[').trim_end_matches(']');
                inner.split(',').find_map(|tok| {
                    let id: i64 = tok.trim().parse().ok()?;
                    st.entries.iter().find(|e| e.fs_id == id)
                })
            } else {
                let p = qp("path").unwrap_or_default();
                st.entries.iter().find(|e| e.path == p)
            };
            match found {
                Some(e) => Json(json!({"errno": 0, "list": [entry_json(e)]})).into_response(),
                None => Json(json!({"errno": -9, "list": []})).into_response(),
            }
        }
        ("GET", "quota") => {
            let st = state.lock().unwrap();
            Json(json!({"errno": 0, "used": st.quota_used, "total": st.quota_total}))
                .into_response()
        }
        ("POST", "create") => {
            let form = parse_urlencoded(&String::from_utf8_lossy(&body));
            let fp = |k: &str| form.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone());
            let (Some(path), Some(isdir)) = (fp("path"), fp("isdir")) else {
                return errno_json(-7);
            };
            if isdir != "1" {
                // B1 只建模目录创建；文件（isdir=0）走 B2 三步曲
                return errno_json(-7);
            }
            let mut st = state.lock().unwrap();
            if st.entries.iter().any(|e| e.path == path) {
                return errno_json(-8); // 真实 errno：file or directory already exists（B2 断言④覆盖映射）
            }
            if !st
                .entries
                .iter()
                .any(|e| e.isdir && Some(e.path.as_str()) == parent_of(&path))
            {
                return errno_json(-7); // mock 严格语义：驱动须逐级隐式建父目录
            }
            let fs_id = st.next_fs_id;
            st.next_fs_id += 1;
            st.entries.push(MockEntry {
                server_filename: path.rsplit('/').next().unwrap_or_default().to_string(),
                path,
                isdir: true,
                size: 0,
                fs_id,
                server_mtime: 1757000000,
                md5: String::new(),
            });
            Json(json!({"errno": 0, "fs_id": fs_id})).into_response()
        }
        ("POST", "filemanager") => {
            let opera = qp("opera").unwrap_or_default();
            let form = parse_urlencoded(&String::from_utf8_lossy(&body));
            let filelist = form
                .iter()
                .find(|(a, _)| a == "filelist")
                .map(|(_, b)| b.clone())
                .unwrap_or_default();
            let Ok(items) = serde_json::from_str::<Vec<Value>>(&filelist) else {
                return errno_json(-7);
            };
            let mut st = state.lock().unwrap();
            match opera.as_str() {
                "delete" => {
                    let mut info = Vec::new();
                    for item in &items {
                        let p = item
                            .get("path")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        match st.entries.iter().position(|e| e.path == p) {
                            Some(i) => {
                                st.entries.remove(i);
                                info.push(json!({"errno": 0, "path": p}));
                            }
                            None => info.push(json!({"errno": -9, "path": p})),
                        }
                    }
                    Json(json!({"errno": 0, "info": info})).into_response()
                }
                "move" => {
                    let mut info = Vec::new();
                    for item in &items {
                        let p = item
                            .get("path")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let dest = item
                            .get("dest")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let newname = item
                            .get("newname")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        match st.entries.iter_mut().find(|e| e.path == p) {
                            Some(e) => {
                                e.path = format!("{dest}/{newname}");
                                e.server_filename = newname;
                                info.push(json!({"errno": 0, "path": p}));
                            }
                            None => info.push(json!({"errno": -9, "path": p})),
                        }
                    }
                    // async=1 形态：响应带 taskid；驱动不轮询（两源一致）
                    Json(json!({"errno": 0, "taskid": 1, "info": info})).into_response()
                }
                _ => errno_json(-7),
            }
        }
        _ => errno_json(-7),
    }
}

/// `/rest/2.0/xpan/nas?method=uinfo`——uid 来源（VolumeId 构造）。
async fn xpan_nas(
    State(state): State<Shared>,
    method: Method,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let raw_query = raw.unwrap_or_default();
    let pairs = parse_urlencoded(&raw_query);
    if let Some(resp) = business_gate(
        &state,
        method.as_str(),
        XPAN_NAS,
        &raw_query,
        &pairs,
        &headers,
        &body,
    ) {
        return resp;
    }
    let uid = state.lock().unwrap().uid;
    Json(json!({"errno": 0, "uid": uid, "uname": "mockuser", "avatar": ""})).into_response()
}

/// `/oauth/2.0/token`——refresh 端点（一次一换轮换模型）。
async fn oauth_token(
    State(state): State<Shared>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let raw_query = raw.unwrap_or_default();
    let pairs = parse_urlencoded(&raw_query);
    state.lock().unwrap().recorded.push(recorded_request(
        "GET",
        OAUTH_TOKEN,
        &raw_query,
        &headers,
        &body,
    ));
    let qp = |k: &str| pairs.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone());
    if qp("grant_type").as_deref() != Some("refresh_token") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "unsupported_grant_type", "error_description": "mock supports refresh_token only"})),
        )
            .into_response();
    }
    let mut st = state.lock().unwrap();
    if qp("refresh_token").as_deref() != Some(st.refresh_token.as_str()) {
        // 陈旧 refresh_token：一次一换下旧值即刻作废（spike §1 实证形态）
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant", "error_description": "Invalid Refresh Token"})),
        )
            .into_response();
    }
    st.refresh_count += 1;
    let n = st.refresh_count;
    let access = format!("mock-access-{n}");
    let refresh = format!("mock-refresh-{n}");
    st.access_token = access.clone();
    st.refresh_token = refresh.clone();
    Json(json!({"access_token": access, "refresh_token": refresh, "expires_in": 2592000, "scope": "basic"}))
        .into_response()
}

// ---------------------------------------------------------------------------
// 解析与断言助手（字节级断言的落点）
// ---------------------------------------------------------------------------

/// 解析 `application/x-www-form-urlencoded` 形态串（query 与 form body 同
/// 规则：`%XX` 解码 + `+`→空格；reqwest `.query()`/`.form()` 均按此序列化）。
pub fn parse_urlencoded(pairs: &str) -> Vec<(String, String)> {
    pairs
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (decode_component(k), decode_component(v)),
            None => (decode_component(pair), String::new()),
        })
        .collect()
}

fn decode_component(s: &str) -> String {
    fn hex_val(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => match (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push((hi << 4) | lo);
                    i += 3;
                }
                _ => {
                    out.push(b[i]);
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 断言参数集**恰为**期望集（无多余、无缺失、无重复）——字节级断言的
/// 精确集合语义（「恰 method=list&dir=…&access_token=… 三参数」）。
pub fn assert_exact_pairs(actual: &[(String, String)], expected: &[(&str, &str)]) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "参数个数不符（应恰为 {expected:?}）：{actual:?}"
    );
    for (k, v) in expected {
        let hits = actual.iter().filter(|(ak, av)| ak == k && av == v).count();
        assert_eq!(hits, 1, "参数 {k}={v} 应恰出现一次：{actual:?}");
    }
}

/// 按 (HTTP 方法, 路径, raw query 子串) 过滤请求记录。
///
/// 子串匹配仅用于 `method=list` / `opera=delete` 等 ASCII 安全值；参数
/// 值断言一律经 [`parse_urlencoded`] 解码后精确匹配。
pub fn filter_recorded<'a>(
    recorded: &'a [RecordedRequest],
    http_method: &str,
    path: &str,
    raw_query_contains: &[&str],
) -> Vec<&'a RecordedRequest> {
    recorded
        .iter()
        .filter(|r| {
            r.http_method == http_method
                && r.path == path
                && raw_query_contains.iter().all(|frag| r.query.contains(frag))
        })
        .collect()
}

/// 断言请求体为 application/x-www-form-urlencoded（表单操作的编码形态）。
pub fn assert_form_encoded(req: &RecordedRequest) {
    assert!(
        req.content_type
            .as_deref()
            .unwrap_or_default()
            .starts_with("application/x-www-form-urlencoded"),
        "应为 form 编码请求：{:?}",
        req.content_type
    );
}

// ---------------------------------------------------------------------------
// TokenStore 录制桩（oauth 持久化断言的观测面）
// ---------------------------------------------------------------------------

/// 记录 save_tokens 调用序列的 TokenStore（K13 持久化断言用）。
#[derive(Default)]
pub struct RecordingTokenStore {
    calls: Mutex<Vec<(String, String)>>,
}

impl RecordingTokenStore {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    pub fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

impl TokenStore for RecordingTokenStore {
    fn save_tokens(&self, access_token: &str, refresh_token: &str) {
        self.calls
            .lock()
            .unwrap()
            .push((access_token.to_string(), refresh_token.to_string()));
    }
}
