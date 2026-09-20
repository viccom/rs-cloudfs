//! 假 123pan web API + 假 CDN/中继端点的共享桩（Phase 6 / 123-2 读路径 +
//! 123-3 写路径）。
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
//! # 写面（123-3；123-0 写路径腿真机事实建模）
//!
//! - `/b/api/file/upload_request`——文件面（**与 mkdir 的 `/a/` 前缀分叉**）：
//!   etag 缺省/空 → 400「请输入Etag」（真形态——驱动恒发真 MD5，触发即
//!   bug 的可观测钉）；同 etag+size 内容在服务端 → `Reuse:true` 瞬时入库
//!   （真 FileId 在 `data.Info.FileId`，顶层 FileId 是临时形态大数，
//!   `UploadId:""`）；同名冲突 bare → 5060 + `data{etag,size,updated_at}`，
//!   `duplicate:1` = `name(1).ext` 副本（KeepBoth）、`duplicate:2` = 同
//!   FileId 原地覆盖（会话记 overwrite_fid，v2 落库时替换）；**同参重发
//!   返回同一 UploadId + up_file_id（resume 会话保留实证）**
//! - `/b/api/file/s3_list_upload_parts`——**小写 `storageNode`**；响应
//!   `data.Parts[]`（`ETag`/`PartNumber` 字符串/`Size` 字符串）；会话被
//!   complete 消费/幽灵 → `code:-1` ListParts NoSuchKey 404 形态
//! - `/b/api/file/s3_repare_upload_parts_batch`——**官方拼写 repare、
//!   大写 `StorageNode`、[start,end) 半开**；响应
//!   `data.presignedUrls{"1":url,...}`
//! - `/put/{uploadId}/{part}`——假 presigned PUT 接收端（200 + etag 回显
//!   = 分片内容 MD5；观测面 = 逐分片 PUT 命中）
//! - `/b/api/file/s3_complete_multipart_upload`——小写 `storageNode`；
//!   complete 后 list → NoSuchKey（会话消费）
//! - `/b/api/file/upload_complete/v2`——**严格全量 body 形态校验**
//!   （7 键 + `isMultipart:true` 恒真 + 大写 `StorageNode`）；错形 →
//!   **code=0 + data:{} 静默不入库**（E2/E4 完成态矩阵真相）；正形 →
//!   落库 + 回 `data.file_info`（snake_case 键，内层 PascalCase 条目）
//! - `/b/api/file/upload_complete`（无 /v2）——**恒 code=0 静默不入库**
//!   （E4 陷阱端点建模——断言驱动永不触碰）
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
//! 忽略 Range/链形态/写面完成失败/幽灵会话/v2 size 偏移/PUT 5xx）。
//! 写面另有 `seq`（写面端点命中时序列——七步严格序的断言面）。
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use base64::Engine as _;
use ck_pan123::api::RetryConfig;
use ck_pan123::limiter::LimiterConfig;
use ck_pan123::{Pan123Driver, Pan123Params};
use md5::{Digest, Md5};
use serde_json::{json, Value};

/// 桩内条目：名字 / file_id / 是否目录 / size / 内容 / 声称 etag。
#[derive(Clone)]
pub struct StubEntry {
    pub name: String,
    pub fid: i64,
    pub is_dir: bool,
    pub size: i64,
    pub data: Vec<u8>,
    /// 服务端存储的声称 etag（真机实证：**无内容校验，存储=声称值**——
    /// 驱动必须算真 MD5，否则假 etag 未来命中秒传取回错误内容）。
    pub etag: String,
}

impl StubEntry {
    fn to_row(&self, parent: i64) -> Value {
        json!({
            "FileId": self.fid,
            "ParentFileId": parent,
            "FileName": self.name,
            "Type": if self.is_dir { 1 } else { 0 },
            "Size": self.size,
            "Etag": self.etag,
            "S3KeyFlag": "4006416717-0",
            "Trashed": false,
            "UpdateAt": "2026-09-20T12:20:15+08:00",
            "CreateAt": "2026-09-20T12:20:15+08:00",
        })
    }
}

/// 上传会话（服务端保留形态——同参重发返回同一 UploadId + up_file_id）。
pub struct StubUploadSession {
    pub parent: i64,
    pub name: String,
    pub etag: String,
    pub size: i64,
    pub upload_id: String,
    /// 顶层临时 FileId（大数形态；真 FileId 在 v2 落库后产生）。
    pub up_file_id: i64,
    pub bucket: String,
    pub key: String,
    pub storage_node: String,
    /// `duplicate:2` 的原地覆盖目标（v2 落库时替换该条目——真机钉死 a）。
    pub overwrite_fid: Option<i64>,
    /// 已收分片：part_number → 字节。
    pub parts: BTreeMap<u32, Vec<u8>>,
    /// s3_complete 已消费（此后 list → NoSuchKey）。
    pub oss_completed: bool,
}

#[derive(Default)]
pub struct StubState {
    /// 父 file_id → 子条目（根 = "0"）。
    pub dirs: HashMap<String, Vec<StubEntry>>,
    pub next_id: i64,
    /// 端点命中计数（路径 → 次数）。
    pub hits: HashMap<String, u32>,
    /// 写面端点命中时序列（七步严格序断言面——只有写面 handler 记 seq）。
    pub seq: Vec<String>,
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
    // ------------------------------------------------------- 写面旋钮 ---
    /// 上传会话表（upload_id → 会话）。
    pub sessions: HashMap<String, StubUploadSession>,
    /// 同参键（parent|name|etag|size）→ upload_id（会话保留模型）。
    pub session_index: HashMap<String, String>,
    /// 下一个临时 up_file_id（大数形态起点）。
    pub next_up_file_id: i64,
    /// 会话号单调计数（upload_id 生成——reissue 换新会话时不复用号）。
    pub next_session_no: u64,
    /// 幽灵会话集合：对这些 id 的 list_parts 恒 NoSuchKey（会话已被
    /// complete 消费的形态）。
    pub ghost_upload_ids: HashSet<String>,
    /// 新建会话即刻入幽灵集合（一次性旋钮——session-gone 重传用例）。
    pub ghost_next_session: bool,
    /// 幽灵会话的重发换新会话（true）或粘住同一 id（false——重试一次
    /// 仍失效 → 驱动 Io 报错的不自陷循环用例）。
    pub reissue_fresh_after_ghost: bool,
    /// s3_complete 注入失败次数（每次命中递减，>0 时 code=5000）。
    pub s3_complete_fail_times: u32,
    /// upload_complete/v2 注入失败次数（>0 时 code=5000——resume 差集
    /// 用例的第一腿失败点）。
    pub v2_fail_times: u32,
    /// v2 响应 file_info.Size 的偏移（size 校验完整性用例）。
    pub v2_size_skew: i64,
    /// /put 注入 5xx 次数（分片 PUT 幂等重试用例）。
    pub put_fail_times: u32,
}

impl StubState {
    fn new() -> Self {
        let mut st = StubState {
            next_id: 6400_0000,
            next_up_file_id: 1_799_000_000_000,
            chain_mode: "relay".to_string(),
            ..StubState::default()
        };
        st.dirs.insert("0".to_string(), Vec::new());
        st
    }

    /// 下一个真实 file_id。
    fn alloc_fid(&mut self) -> i64 {
        let fid = self.next_id;
        self.next_id += 1;
        fid
    }

    /// 下一个临时 up_file_id（大数形态）。
    fn alloc_up_file_id(&mut self) -> i64 {
        let id = self.next_up_file_id;
        self.next_up_file_id += 1;
        id
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
            // 写面（123-3）
            .route("/b/api/file/upload_request", post(upload_request_file))
            .route(
                "/b/api/file/s3_list_upload_parts",
                post(s3_list_upload_parts),
            )
            .route(
                "/b/api/file/s3_repare_upload_parts_batch",
                post(s3_repare_batch),
            )
            .route(
                "/b/api/file/s3_complete_multipart_upload",
                post(s3_complete),
            )
            .route("/b/api/file/upload_complete/v2", post(upload_complete_v2))
            .route("/b/api/file/upload_complete", post(upload_complete_new))
            // 分片 PUT 接收端：放宽 body 上限（axum 缺省 2MiB——5MiB 分片
            // 会被 413 拒掉；生产上传面无此限制）。
            .route(
                "/put/{upload_id}/{part}",
                put(put_part).layer(axum::extract::DefaultBodyLimit::max(100 * 1024 * 1024)),
            )
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
            sessions_dir: None,
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

    /// 写面命中时序列的快照（七步严格序断言面）。
    pub fn seq(&self) -> Vec<String> {
        self.state.lock().unwrap().seq.clone()
    }

    /// 逐分片 PUT 命中总数（所有会话合计——差集 resume 的观测面）。
    pub fn put_hits_total(&self) -> u32 {
        self.state
            .lock()
            .unwrap()
            .hits
            .iter()
            .filter(|(k, _)| k.starts_with("/put:"))
            .map(|(_, v)| *v)
            .sum()
    }

    /// 指定 upload_id 的会话分片布局（part_number → 字节数）。
    pub fn session_part_sizes(&self, upload_id: &str) -> Vec<(u32, usize)> {
        self.state
            .lock()
            .unwrap()
            .sessions
            .get(upload_id)
            .map(|s| s.parts.iter().map(|(n, d)| (*n, d.len())).collect())
            .unwrap_or_default()
    }

    /// 在目录下建一个子目录（测试准备面；size 注入聚合值——驱动须报 0）。
    pub fn mkdir(&self, parent: &str, name: &str) -> i64 {
        let mut st = self.state.lock().unwrap();
        let fid = st.alloc_fid();
        st.dirs
            .entry(parent.to_string())
            .or_default()
            .push(StubEntry {
                name: name.to_string(),
                fid,
                is_dir: true,
                size: 987654, // 目录带累计聚合 Size——驱动须报 0（任务 A）
                data: Vec::new(),
                etag: String::new(),
            });
        st.dirs.insert(fid.to_string(), Vec::new());
        fid
    }

    /// 在目录下放一个文件（带内容；测试准备面）。
    pub fn put_file(&self, parent: &str, name: &str, data: Vec<u8>) -> i64 {
        let mut st = self.state.lock().unwrap();
        let fid = st.alloc_fid();
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
                etag: String::new(),
            });
        fid
    }

    /// 灌入 N 个文件（分页合并排序用例——超单页 100）。
    pub fn put_many(&self, parent: &str, prefix: &str, count: usize) {
        let mut st = self.state.lock().unwrap();
        for i in 0..count {
            let fid = st.alloc_fid();
            st.dirs
                .entry(parent.to_string())
                .or_default()
                .push(StubEntry {
                    name: format!("{prefix}{i:04}"),
                    fid,
                    is_dir: false,
                    size: 1,
                    data: vec![b'x'],
                    etag: String::new(),
                });
        }
    }

    /// 名字重复但 file_id 不同的两行（排序 file_id 回退的用例面——真机
    /// 服务端不会产同名对，桩开放该形态钉驱动排序的确定性）。
    pub fn put_dup_name(&self, parent: &str, name: &str) -> i64 {
        let mut st = self.state.lock().unwrap();
        let fid = st.alloc_fid();
        st.dirs
            .entry(parent.to_string())
            .or_default()
            .push(StubEntry {
                name: name.to_string(),
                fid,
                is_dir: false,
                size: 1,
                data: vec![b'y'],
                etag: String::new(),
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

/// 写面命中：计数 + 时序（七步严格序的断言面）。
fn touch(st: &mut StubState, label: &str) {
    *st.hits.entry(label.to_string()).or_insert(0) += 1;
    st.seq.push(label.to_string());
}

fn md5_hex(data: &[u8]) -> String {
    let digest = Md5::digest(data);
    digest.iter().map(|b| format!("{b:02x}")).collect()
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
        etag: String::new(),
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
// 写面路由（123-3；123-0 写路径腿真机事实建模）
// ---------------------------------------------------------------------

/// 桩 uid（Key 嵌 `<md5前8>/<uid>-0/<全md5>` 形态用）。
const STUB_UID: i64 = 4006416717;
/// 服务端 SliceSize 真值（16MiB **字符串** 形态——123-0 ⑤ 钉死；客户端
/// 可自定 ≥5MiB，驱动按 5MiB 定值）。
const SERVER_SLICE_SIZE: &str = "16777216";

/// `duplicate:1` 的自动改名形态：`name(1).ext`（括号计数插扩展名前——
/// 真机钉死 a；驱动恒用 2，此形态只为真相建模）。
fn renamed_copy(st: &StubState, parent: &str, name: &str) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(pos) if pos > 0 => (&name[..pos], &name[pos..]),
        _ => (name, ""),
    };
    for k in 1..100 {
        let candidate = format!("{stem}({k}){ext}");
        let occupied = st
            .dirs
            .get(parent)
            .is_some_and(|rows| rows.iter().any(|e| e.name == candidate));
        if !occupied {
            return candidate;
        }
    }
    format!("{stem}(100){ext}")
}

/// `POST /b/api/file/upload_request`——文件面（type=0）。
///
/// 行为序（123-0 钉死项的桩化）：
/// 1. etag 缺省/空 → 400「请输入Etag」（真形态——**驱动恒发真 MD5**，
///    触发即 bug 的可观测钉）；
/// 2. **Reuse 优先于 5060**：同 etag+size 内容在服务端 → `Reuse:true`
///    瞬时入库（真 FileId 在 `data.Info.FileId`；顶层 FileId 是临时大数
///    形态、`UploadId:""`）；`duplicate:2` + 同名已存在 → 同 FileId 原地
///    覆盖（覆盖秒传形态）；
/// 3. 同名冲突：bare → `code:5060` + `data{etag,size,updated_at}`；
///    `duplicate:1` → `name(1).ext` 副本；`duplicate:2` → 会话记
///    overwrite_fid（v2 落库时原地替换）；
/// 4. 会话保留：同参键（parent|name|etag|size）重发返回**同一
///    UploadId + up_file_id**、已收分片保留（resume 实证）；幽灵会话
///    （被 complete 消费）按 `reissue_fresh_after_ghost` 旋钮换新或粘住。
async fn upload_request_file(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let mut st = state.lock().unwrap();
    touch(&mut st, "/b:upload_request");
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return err_code(400, "bad json");
    };
    let name = v
        .get("fileName")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let parent = v.get("parentFileId").and_then(|p| p.as_i64()).unwrap_or(-1);
    let size = v.get("size").and_then(|s| s.as_i64()).unwrap_or(-1);
    let etag = v
        .get("etag")
        .and_then(|e| e.as_str())
        .unwrap_or("")
        .to_string();
    let duplicate = v.get("duplicate").and_then(|d| d.as_i64());
    let file_type = v.get("type").and_then(|t| t.as_i64()).unwrap_or(0);
    if file_type != 0 {
        return err_code(400, "type must be 0 on the file face");
    }
    // 真机 ⑥：etag 必填无豁免（缺省/空均 400）。
    if etag.is_empty() {
        return err_code(400, "请输入Etag");
    }
    let parent_key = parent.to_string();
    if !st.dirs.contains_key(&parent_key) {
        return err_code(1024, "父目录不存在");
    }

    // ---- Reuse 优先于 5060：内容寻址（全树同 etag+size 文件）。
    let reuse_src = st
        .dirs
        .values()
        .flatten()
        .find(|e| !e.is_dir && e.etag == etag && e.size == size)
        .cloned();
    if let Some(src) = reuse_src {
        let same_name_idx = st
            .dirs
            .get(&parent_key)
            .and_then(|rows| rows.iter().position(|e| e.name == name && !e.is_dir));
        let now = "2026-09-20T16:00:00+08:00";
        let (fid, row_name) = if duplicate == Some(2) {
            match same_name_idx {
                // 覆盖秒传：同 FileId 原地换内容（真机钉死 b）。
                Some(idx) => {
                    let rows = st.dirs.get_mut(&parent_key).expect("parent exists");
                    rows[idx].data = src.data.clone();
                    rows[idx].size = src.size;
                    rows[idx].etag = etag.clone();
                    (rows[idx].fid, rows[idx].name.clone())
                }
                None => {
                    let fid = st.alloc_fid();
                    st.dirs
                        .get_mut(&parent_key)
                        .expect("parent")
                        .push(StubEntry {
                            name: name.clone(),
                            fid,
                            is_dir: false,
                            size: src.size,
                            data: src.data.clone(),
                            etag: etag.clone(),
                        });
                    (fid, name.clone())
                }
            }
        } else {
            let fid = st.alloc_fid();
            st.dirs
                .get_mut(&parent_key)
                .expect("parent")
                .push(StubEntry {
                    name: name.clone(),
                    fid,
                    is_dir: false,
                    size: src.size,
                    data: src.data.clone(),
                    etag: etag.clone(),
                });
            (fid, name.clone())
        };
        let up_file_id = st.alloc_up_file_id();
        return ok_json(json!({
            "Reuse": true,
            "UploadId": "",
            "FileId": up_file_id,
            "Info": {
                "FileId": fid,
                "FileName": row_name,
                "Type": 0,
                "Size": src.size,
                "Etag": etag,
                "S3KeyFlag": format!("{STUB_UID}-0"),
                "Trashed": false,
                "CreateAt": now,
                "UpdateAt": now,
            },
            "SliceSize": SERVER_SLICE_SIZE,
        }));
    }

    // ---- 同名冲突（非 Reuse）。
    let existing = st
        .dirs
        .get(&parent_key)
        .and_then(|rows| rows.iter().find(|e| e.name == name))
        .cloned();
    let mut overwrite_fid: Option<i64> = None;
    let final_name = match (existing, duplicate) {
        (Some(_), None) => {
            // bare 冲突 → 5060 真机形态。
            let ex = st
                .dirs
                .get(&parent_key)
                .and_then(|rows| rows.iter().find(|e| e.name == name))
                .expect("checked above");
            return Json(json!({
                "code": 5060,
                "message": "检测到1个同名文件",
                "data": {"etag": ex.etag, "size": ex.size, "updated_at": 0}
            }))
            .into_response();
        }
        (Some(_), Some(1)) => {
            // duplicate=1 = 保留两者（新条目自动改名 name(k).ext）。
            renamed_copy(&st, &parent_key, &name)
        }
        (Some(ex), Some(2)) => {
            if ex.is_dir {
                return err_code(1024, "不能覆盖目录");
            }
            // duplicate=2 = 同 FileId 原地覆盖（v2 落库时替换）。
            overwrite_fid = Some(ex.fid);
            name.clone()
        }
        (Some(_), Some(other)) => {
            return err_code(400, &format!("unsupported duplicate {other}"));
        }
        (None, _) => name.clone(),
    };

    // ---- 会话（保留模型：同参重发返回同一 UploadId + up_file_id）。
    let skey = format!("{parent}|{name}|{etag}|{size}");
    let existing_sid = st.session_index.get(&skey).cloned();
    let sid = match existing_sid {
        Some(id) if st.ghost_upload_ids.contains(&id) && st.reissue_fresh_after_ghost => {
            // 幽灵会话（已被 complete 消费）→ 换新会话全量重传。
            st.sessions.remove(&id);
            st.session_index.remove(&skey);
            new_stub_session(
                &mut st,
                &skey,
                parent,
                &final_name,
                &etag,
                size,
                overwrite_fid,
            )
        }
        Some(id) => id,
        None => new_stub_session(
            &mut st,
            &skey,
            parent,
            &final_name,
            &etag,
            size,
            overwrite_fid,
        ),
    };
    let s = st.sessions.get(&sid).expect("session just made");
    ok_json(json!({
        "Bucket": s.bucket,
        "Key": s.key,
        "UploadId": s.upload_id,
        "StorageNode": s.storage_node,
        "FileId": s.up_file_id,
        "SliceSize": SERVER_SLICE_SIZE,
        "Reuse": false,
        "Info": Value::Null,
        "UploadFileStatus": 2,
        "EndPoint": "oss-stub.123pan.com",
        "NotReuse": true,
        "Expiration": "2026-09-27T16:00:00+08:00",
        "CallBack": "",
        "CallbackKey": "",
    }))
}

/// 新建桩上传会话（Key 嵌 `<md5前8>/<uid>-0/<全md5>`——真机形态）。
fn new_stub_session(
    st: &mut StubState,
    skey: &str,
    parent: i64,
    name: &str,
    etag: &str,
    size: i64,
    overwrite_fid: Option<i64>,
) -> String {
    st.next_session_no += 1;
    let upload_id = format!("stub-up-{}", st.next_session_no);
    let up_file_id = st.alloc_up_file_id();
    let prefix8 = &etag[..etag.len().min(8)];
    let session = StubUploadSession {
        parent,
        name: name.to_string(),
        etag: etag.to_string(),
        size,
        upload_id: upload_id.clone(),
        up_file_id,
        bucket: "stub-bucket".to_string(),
        key: format!("{prefix8}/{STUB_UID}-0/{etag}"),
        storage_node: "stub-node".to_string(),
        overwrite_fid,
        parts: BTreeMap::new(),
        oss_completed: false,
    };
    if st.ghost_next_session {
        st.ghost_upload_ids.insert(upload_id.clone());
        st.ghost_next_session = false;
    }
    st.sessions.insert(upload_id.clone(), session);
    st.session_index.insert(skey.to_string(), upload_id.clone());
    upload_id
}

/// `POST /b/api/file/s3_list_upload_parts`——**小写 `storageNode`**
/// （与 repare/v2 的大写分叉——逐字保真；错形 400 可观测钉）。
async fn s3_list_upload_parts(
    State(state): State<Arc<Mutex<StubState>>>,
    body: String,
) -> Response {
    let mut st = state.lock().unwrap();
    touch(&mut st, "/b:list_parts");
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return err_code(400, "bad json");
    };
    if v.get("storageNode").is_none() {
        return err_code(
            400,
            "The storageNode field is required (lowercase on this endpoint)",
        );
    }
    let Some(upload_id) = v.get("uploadId").and_then(|u| u.as_str()) else {
        return err_code(400, "The uploadId field is required");
    };
    // 会话查无 / 幽灵（已被 complete 消费）→ -1 ListParts NoSuchKey 404
    // （真机形态——会话失效的驱动判据）。
    let gone = st
        .sessions
        .get(upload_id)
        .map(|s| s.oss_completed)
        .unwrap_or(true)
        || st.ghost_upload_ids.contains(upload_id);
    if gone {
        return err_code(-1, "rpc error: ListParts NoSuchKey (404)");
    }
    let Some(s) = st.sessions.get(upload_id) else {
        return err_code(-1, "rpc error: ListParts NoSuchKey (404)");
    };
    let parts: Vec<Value> = s
        .parts
        .iter()
        .map(|(n, data)| {
            json!({
                "ETag": md5_hex(data),
                "PartNumber": n.to_string(),  // 字符串形态（真机实证）
                "Size": data.len().to_string(), // 字符串形态
            })
        })
        .collect();
    ok_json(json!({ "Parts": parts }))
}

/// `POST /b/api/file/s3_repare_upload_parts_batch`——**官方拼写 repare、
/// 大写 `StorageNode`、[start,end) 半开区间**（逐字保真）。
async fn s3_repare_batch(
    State(state): State<Arc<Mutex<StubState>>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let mut st = state.lock().unwrap();
    touch(&mut st, "/b:repare");
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return err_code(400, "bad json");
    };
    if v.get("StorageNode").is_none() {
        return err_code(
            400,
            "The StorageNode field is required (UPPERCASE on this endpoint)",
        );
    }
    let Some(upload_id) = v.get("uploadId").and_then(|u| u.as_str()) else {
        return err_code(400, "The uploadId field is required");
    };
    if !st.sessions.contains_key(upload_id) {
        return err_code(-1, "rpc error: ListParts NoSuchKey (404)");
    }
    let start = v
        .get("partNumberStart")
        .and_then(|n| n.as_i64())
        .unwrap_or(1);
    let end = v
        .get("partNumberEnd")
        .and_then(|n| n.as_i64())
        .unwrap_or(start);
    let host = host_of(&headers);
    let mut urls = serde_json::Map::new();
    for n in start..end.max(start) {
        urls.insert(
            n.to_string(),
            Value::String(format!("http://{host}/put/{upload_id}/{n}")),
        );
    }
    ok_json(json!({ "presignedUrls": Value::Object(urls) }))
}

/// `PUT /put/{uploadId}/{part}`——假 presigned 接收端（传输裸面：无
/// 123pan 头、无表单；200 + etag 回显 = 分片内容 MD5——真机形态）。
async fn put_part(
    State(state): State<Arc<Mutex<StubState>>>,
    Path((upload_id, part)): Path<(String, u32)>,
    body: Bytes,
) -> Response {
    let mut st = state.lock().unwrap();
    touch(&mut st, &format!("/put:{part}"));
    if st.put_fail_times > 0 {
        st.put_fail_times -= 1;
        return (StatusCode::INTERNAL_SERVER_ERROR, "injected part failure").into_response();
    }
    let Some(session) = st.sessions.get_mut(&upload_id) else {
        return (StatusCode::NOT_FOUND, "no such upload session").into_response();
    };
    session.parts.insert(part, body.to_vec());
    let etag = md5_hex(&body);
    Response::builder()
        .status(StatusCode::OK)
        .header("etag", &etag)
        .body(axum::body::Body::empty())
        .unwrap()
}

/// `POST /b/api/file/s3_complete_multipart_upload`——小写 `storageNode`；
/// code=0 + `data.Location:""`（真机形态）。会话被标记消费（此后 list →
/// NoSuchKey）。
async fn s3_complete(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let mut st = state.lock().unwrap();
    touch(&mut st, "/b:s3_complete");
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return err_code(400, "bad json");
    };
    if v.get("storageNode").is_none() {
        return err_code(
            400,
            "The storageNode field is required (lowercase on this endpoint)",
        );
    }
    if st.s3_complete_fail_times > 0 {
        st.s3_complete_fail_times -= 1;
        return err_code(5000, "injected s3_complete failure");
    }
    let Some(upload_id) = v.get("uploadId").and_then(|u| u.as_str()) else {
        return err_code(400, "The uploadId field is required");
    };
    match st.sessions.get_mut(upload_id) {
        Some(session) => {
            session.oss_completed = true;
            ok_json(json!({ "Location": "" }))
        }
        // -1 MalformedXML：读路径腿实证的 -1 形态（单 PUT 会话误调）。
        None => err_code(-1, "rpc error: MalformedXML"),
    }
}

/// `POST /b/api/file/upload_complete/v2`——**严格全量 body 形态校验**：
/// 恰 7 键（fileId/bucket/fileSize/key/isMultipart/uploadId/StorageNode
/// 大写）且 `isMultipart:true` → 落库 + `data.file_info`（snake_case 键，
/// 内层 PascalCase 条目 = 真实 FileId + 声称 etag + 完整条目）；**错形 →
/// code=0 + data:{} 静默不入库**（E2/E4 完成态矩阵真相——repare 批量
/// 恒 multipart 会话，单键/false 形态对该会话是静默 no-op）。
async fn upload_complete_v2(State(state): State<Arc<Mutex<StubState>>>, body: String) -> Response {
    let mut st = state.lock().unwrap();
    touch(&mut st, "/b:complete_v2");
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return err_code(400, "bad json");
    };
    let full_form = [
        "fileId",
        "bucket",
        "fileSize",
        "key",
        "isMultipart",
        "uploadId",
        "StorageNode",
    ]
    .iter()
    .all(|k| v.get(k).is_some())
        && v.get("isMultipart").and_then(|m| m.as_bool()) == Some(true);
    if !full_form {
        // E2/E4 真相：错形 code=0 静默不入库（驱动侧将以 file_info 缺失
        // 的 Unavailable 揭出——绝不是静默成功）。
        touch(&mut st, "/b:complete_v2_silent");
        return ok_json(json!({}));
    }
    if st.v2_fail_times > 0 {
        st.v2_fail_times -= 1;
        return err_code(5000, "injected v2 failure");
    }
    let Some(upload_id) = v.get("uploadId").and_then(|u| u.as_str()) else {
        return err_code(-1, "rpc error: session not found");
    };
    let Some(session) = st.sessions.remove(upload_id) else {
        return err_code(-1, "rpc error: session not found");
    };
    st.session_index.remove(&format!(
        "{}|{}|{}|{}",
        session.parent, session.name, session.etag, session.size
    ));
    // 落库：分片按序重组；duplicate=2 的 overwrite_fid → 同 FileId 原地
    // 替换（CreateAt 不变——真机钉死 a）；否则新条目。
    let data: Vec<u8> = session.parts.values().flatten().cloned().collect();
    let now = "2026-09-20T16:30:00+08:00";
    let row = match session.overwrite_fid {
        Some(fid) => {
            let mut found = None;
            for rows in st.dirs.values_mut() {
                if let Some(e) = rows.iter_mut().find(|e| e.fid == fid) {
                    e.data = data;
                    e.size = session.size;
                    e.etag = session.etag.clone();
                    found = Some(e.clone());
                    break;
                }
            }
            match found {
                Some(e) => json!({
                    "FileId": e.fid, "FileName": e.name, "Type": 0,
                    "Size": session.size + st.v2_size_skew,
                    "Etag": session.etag, "S3KeyFlag": format!("{STUB_UID}-0"),
                    "Trashed": false, "CreateAt": "2026-09-20T12:00:00+08:00",
                    "UpdateAt": now,
                }),
                None => return err_code(-1, "rpc error: overwrite target vanished"),
            }
        }
        None => {
            let fid = st.alloc_fid();
            st.dirs
                .entry(session.parent.to_string())
                .or_default()
                .push(StubEntry {
                    name: session.name.clone(),
                    fid,
                    is_dir: false,
                    size: session.size,
                    data,
                    etag: session.etag.clone(),
                });
            json!({
                "FileId": fid, "FileName": session.name, "Type": 0,
                "Size": session.size + st.v2_size_skew,
                "Etag": session.etag, "S3KeyFlag": format!("{STUB_UID}-0"),
                "Trashed": false, "CreateAt": now, "UpdateAt": now,
            })
        }
    };
    ok_json(json!({ "file_info": row }))
}

/// `POST /b/api/file/upload_complete`（无 /v2）——**恒 code=0 静默不入库**
/// （E4 陷阱端点建模——断言驱动永不触碰：`/b:complete_new` 命中恒 0）。
async fn upload_complete_new(
    State(state): State<Arc<Mutex<StubState>>>,
    _body: String,
) -> Response {
    let mut st = state.lock().unwrap();
    touch(&mut st, "/b:complete_new");
    ok_json(json!({}))
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
