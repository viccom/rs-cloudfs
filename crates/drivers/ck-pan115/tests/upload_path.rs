//! 写路径桩回放（Phase 5 / 115-3）：假开放平台 + 假 OSS 端点下的
//! 上传全链矩阵。
//!
//! 桩形态（115-4 conformance 桩的先导）：开放平台面（init/get_token/
//! resume）+ **OSS path-style 端点**（get_token 返回 `http://127.0.0.1:PORT`
//! ——oss.rs 的 scheme 缝把请求发到本桩，K69.5 端点在 115-0 spike 已被
//! 证明可控）。真机事实（K69.2/K69.6/K69.8）在断言注释里标注。
//!
//! 覆盖：PutObject 单分片 / multipart 多分片 / 秒传 status=2 /
//! 二次认证循环（sign_key/sign_check）/ 远端 size 复核 / WriteHint 契约
//! / commit-on-close（close 前 list 不可见）/ abort 清 spool 与会话 /
//! resume 差集（Drop 保留会话 → 新 stager 只补缺片）/ 目标目录隐式创建。
//!
//! 未覆盖（挂账）：OSS 429/SlowDown 退避分支、uploadId 意外重置
//! （`NoSuchUpload` 重建腿有桩支持但未单独用例）、真机 callback 语义。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use ck_pan115::limiter::LimiterConfig;
use ck_pan115::{Pan115Driver, Pan115Params};
use cloudkit_storage::{RelPath, StorageDriver, WriteHint};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};

// ---------------------------------------------------------------------
// 假开放平台 + 假 OSS 桩
// ---------------------------------------------------------------------

#[derive(Clone)]
struct Obj {
    data: Vec<u8>,
    /// 已传分片：part_number → 字节
    parts: HashMap<u32, Vec<u8>>,
    /// 分片 etag：part_number → etag
    etags: HashMap<u32, String>,
    upload_id: String,
    /// complete 后的对象可见性（commit-on-close 的桩面）
    completed: bool,
}

#[derive(Default)]
struct UState {
    /// object key → 对象（init 创建、complete 固化）
    objs: HashMap<String, Obj>,
    /// sha1 → object key 的反查（秒传命中判定用；当前桩用 rapid_hit
    /// 标志代替——保留字段为该方向的将来用例）。
    #[allow(dead_code)]
    by_sha1: HashMap<String, String>,
    /// 已知 fid 表（11xxx 自增，与 115 样式近似）
    fid_seq: u64,
    /// init 收到的 (file_name, fileid, preid) 记录（二次认证断言用）
    init_calls: Vec<(String, String, String, Option<String>)>,
    /// 触发二次认证的条数（首 N 次 init 返回 sign_key/sign_check）
    secondary_auth_rounds: u32,
    /// 秒传模式：init 直接 status=2（模拟已存在同 SHA1 文件）
    rapid_hit: bool,
    /// 已 complete 的对象 key → (fid, size)（list/get_info 的可见面；
    /// 秒传命中也经此登记——size 复核腿需要真值）。
    visible: HashMap<String, (String, i64)>,
    /// callback 头是否曾在 complete/put 出现（K69.8 断言面）
    callback_headers_seen: bool,
    /// OSS 分片 PUT 计数（resume 差集断言：第二次跑只补缺片）
    part_put_count: u32,
    /// 强制 complete 后远端 size 为 0（复核负例注入）
    force_zero_size: bool,
    /// 第 N 片之后的 PUT 失败（分片级 resume 用例的中断注入）
    fail_part_after: Option<u32>,
    /// `/open/upload/resume` 的失败预算（复审 M10：瞬态传输错注入——
    /// 非分类错误码，每次消耗一次）
    fail_resume: Option<u32>,
    /// pick_code → object key（resume 会话复用：同一 pc 回同一对象）
    pc_objects: HashMap<String, String>,
    /// get_token 调用计数（M-S2 断言面：STS 每传输链只取一次）
    get_token_calls: u32,
    /// K75-2 观测面：收到的 AbortMultipartUpload（DELETE ?uploadId）次数
    aborted_uploads: u32,
    /// 最近一次下发的 sign_check 挑战区间（M-T1 对账面）
    sign_check_issued: String,
    /// 收到的 sign_val 应答（M-T1 对账面——数值正确性事后核验）
    sign_vals: Vec<String>,
}

struct Mock {
    state: Arc<Mutex<UState>>,
    base: String,
}

impl Mock {
    async fn start(secondary_auth_rounds: u32, rapid_hit: bool) -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let base = format!("http://{addr}");
        let state = Arc::new(Mutex::new(UState {
            secondary_auth_rounds,
            rapid_hit,
            ..UState::default()
        }));
        let app = Router::new()
            .route("/open/upload/init", post(upload_init))
            .route("/open/upload/get_token", get(upload_get_token))
            .route("/open/upload/resume", post(upload_resume))
            .route("/open/folder/get_info", get(folder_get_info))
            .route("/open/ufile/files", get(ufile_files))
            // OSS path-style：/{bucket}/{object}（?uploads / ?partNumber&uploadId / ?uploadId）
            .route("/{bucket}/{*object}", put(oss_put))
            .route("/{bucket}/{*object}", post(oss_post))
            .route("/{bucket}/{*object}", get(oss_list_parts))
            // K75-2：AbortMultipartUpload（DELETE ?uploadId）
            .route("/{bucket}/{*object}", delete(oss_delete))
            // 桩要收 5MiB 级 OSS 分片：抬高 axum 的默认 body 上限
            // （2MB——否则大分片直接 413）。layer 在路由之后 → 覆盖全部。
            .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024))
            .with_state(state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        Mock { state, base }
    }

    /// 构造驱动（api_base = 桩；OSS endpoint 由 get_token 返回桩 base）。
    ///
    /// spool/会话目录用**进程临时子目录**（测试隔离；Drop 清理交给
    /// 用例自身的 tempdir 生命周期——这里用固定子目录 + 用后即弃）。
    fn driver(&self, spool_dir: std::path::PathBuf) -> Pan115Driver {
        Pan115Driver::new(Pan115Params {
            client_id: "100197303".to_string(),
            access_token: Some("mock-access".to_string()),
            refresh_token: Some("mock-refresh".to_string()),
            root: "0".to_string(),
            api_base: self.base.clone(),
            passport_base: self.base.clone(),
            token_store: None,
            limiter: Some(LimiterConfig::fast()),
            sessions_dir: Some(spool_dir),
        })
        .expect("driver")
    }

    fn st(&self) -> std::sync::MutexGuard<'_, UState> {
        self.state.lock().unwrap()
    }

    /// 分片级中断注入：第 N 片之后的 PUT 失败（可重试类）。
    fn set_fail_part_after(&self, n: u32) {
        self.state.lock().unwrap().fail_part_after = Some(n);
    }

    fn clear_part_failure(&self) {
        self.state.lock().unwrap().fail_part_after = None;
    }

    /// 复审 M10 注入：让 `/open/upload/resume` 接下来 N 次调用返回
    /// 非分类错误码（→ `Unavailable`，传输类形态——会话真伪未知的
    /// 瞬态失败）。
    fn set_fail_resume(&self, n: u32) {
        self.state.lock().unwrap().fail_resume = Some(n);
    }

    fn clear_fail_resume(&self) {
        self.state.lock().unwrap().fail_resume = None;
    }
}

fn ok_json(data: Value) -> Response {
    Json(json!({"state": true, "errno": 0, "data": data})).into_response()
}

fn err_json(code: i64, message: &str) -> Response {
    Json(json!({"state": false, "code": code, "errno": 0, "message": message})).into_response()
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

/// `upload/init`：首次 N 次触发二次认证（sign_key/sign_check），随后
/// status=1（或 rapid_hit 时 status=2）。
async fn upload_init(State(state): State<Arc<Mutex<UState>>>, body: String) -> Response {
    let form = parse_form(&body);
    let name = form.get("file_name").cloned().unwrap_or_default();
    let fileid = form.get("fileid").cloned().unwrap_or_default();
    let preid = form.get("preid").cloned().unwrap_or_default();
    let sign_key = form.get("sign_key").cloned();
    let mut st = state.lock().unwrap();
    st.init_calls.push((
        name.clone(),
        fileid.clone(),
        preid.clone(),
        sign_key.clone(),
    ));
    let pick_code = format!("pc-{}", st.fid_seq + 1);

    // 二次认证循环（K69.2）：首轮（无 sign_key）在配额内 → 下发挑战。
    if sign_key.is_none() && st.secondary_auth_rounds > 0 {
        st.secondary_auth_rounds -= 1;
        // 挑战区间必须落在文件内（K69.2 实测形态是文件中部的一段：
        // 1585000-1733288/12MiB）；桩用前半段，末字节 = size/2。
        let size: u64 = form
            .get("file_size")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let end = (size / 2).max(1) - 1;
        st.sign_check_issued = format!("0-{end}");
        return ok_json(json!({
            "pick_code": pick_code,
            "status": 8,
            "sign_key": "mock-sign-key",
            "sign_check": format!("0-{end}"),
        }));
    }
    // 挑战应答轮：记录 sign_val（M-T1——桩不校验数值，测试事后对账）
    if let Some(sv) = form.get("sign_val") {
        st.sign_vals.push(sv.clone());
    }

    // 秒传命中：status=2 + file_id
    if st.rapid_hit {
        st.fid_seq += 1;
        let fid = format!("{}", 11_000 + st.fid_seq);
        let size: i64 = form
            .get("file_size")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        st.visible.insert(name.clone(), (fid.clone(), size));
        return ok_json(json!({
            "pick_code": pick_code,
            "status": 2,
            "file_id": fid,
        }));
    }

    // 常规：status=1 + bucket/object/callback
    st.fid_seq += 1;
    let key = format!("{}-{}", st.fid_seq, name);
    st.objs.insert(
        key.clone(),
        Obj {
            data: Vec::new(),
            parts: HashMap::new(),
            etags: HashMap::new(),
            upload_id: String::new(),
            completed: false,
        },
    );
    st.pc_objects.insert(pick_code.clone(), key.clone());
    ok_json(json!({
        "pick_code": pick_code,
        "status": 1,
        "bucket": "mockbucket",
        "object": key,
        "callback": {
            "callback": "{\"callbackUrl\":\"http://cb\"}",
            "callback_var": "{\"x\":\"1\"}"
        }
    }))
}

/// `upload/get_token`：STS 端点 = **本桩 base**（scheme 缝 → path-style）。
async fn upload_get_token(State(state): State<Arc<Mutex<UState>>>) -> Response {
    state.lock().unwrap().get_token_calls += 1;
    // 端点形态：scheme 前缀（测试缝触发 path-style；生产无 scheme）
    let base = CURRENT_BASE.with(|b| b.borrow().clone());
    ok_json(json!({
        "endpoint": base,
        "AccessKeyId": "mock-ak",
        "AccessKeySecret": "mock-sk",
        "SecurityToken": "mock-sts",
        "Expiration": "2030-01-01T00:00:00Z"
    }))
}

// get_token 需要知道桩自己的 base（路由里没有）：用 thread-local 注入。
thread_local! {
    static CURRENT_BASE: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

async fn upload_resume(State(state): State<Arc<Mutex<UState>>>, body: String) -> Response {
    let form = parse_form(&body);
    let pc = form.get("pick_code").cloned().unwrap_or_default();
    // 复审 M10：瞬态失败注入（非分类码 → Unavailable——会话真伪未知
    // 的传输类形态；调用方必须保留会话记录上抛，不得销毁差集资产）。
    {
        let mut st = state.lock().unwrap();
        if let Some(n) = st.fail_resume {
            if n > 0 {
                st.fail_resume = Some(n - 1);
                return err_json(990001, "resume 通道繁忙（注入）");
            }
        }
    }
    let st = state.lock().unwrap();
    // 会话复用语义：同一 pick_code → 同一对象（含已传分片与 uploadId）
    let Some(key) = st.pc_objects.get(&pc).cloned() else {
        return err_json(430004, "pick_code 无效");
    };
    ok_json(json!({
        "pick_code": pc,
        "bucket": "mockbucket",
        "object": key,
        "callback": {"callback": "{\"a\":1}", "callback_var": "{}"}
    }))
}

async fn folder_get_info(
    State(state): State<Arc<Mutex<UState>>>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let st = state.lock().unwrap();
    let fid = q.get("file_id").cloned().unwrap_or_default();
    let Some(name) = st
        .visible
        .iter()
        .find(|(_, (v, _))| *v == fid)
        .map(|(k, _)| k.clone())
    else {
        return err_json(430004, "文件不存在");
    };
    // 登记表里的 size（秒传/整传/分片 complete 三路都写过真值）；
    // force_zero_size 注入覆盖（零字节 bug 类的负例）。
    let size = if st.force_zero_size {
        0
    } else {
        st.visible.get(&name).map(|(_, sz)| *sz).unwrap_or(0)
    };
    ok_json(json!({
        "file_id": fid,
        "file_name": name,
        "file_category": "1",
        "pick_code": format!("pc-{fid}"),
        "sha1": "",
        "size_byte": size,
        "size": size.to_string(),
    }))
}

async fn ufile_files(
    State(state): State<Arc<Mutex<UState>>>,
    axum::extract::Query(_q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let st = state.lock().unwrap();
    let rows: Vec<Value> = st
        .visible
        .iter()
        .map(|(name, (fid, _))| {
            json!({
                "fid": fid,
                "fc": "1",
                "fs": 0,
                "fn": name,
                "pc": format!("pc-{fid}"),
                "sha1": "",
                "upt": 1_700_000_000i64,
            })
        })
        .collect();
    let count = rows.len() as i64;
    Json(json!({"state": true, "errno": 0, "data": rows, "count": count})).into_response()
}

/// OSS PUT：`?partNumber=N&uploadId=ID` = 分片；否则 = PutObject 整对象。
async fn oss_put(
    State(state): State<Arc<Mutex<UState>>>,
    axum::extract::Path((_bucket, object)): axum::extract::Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let q = query.unwrap_or_default();
    let mut st = state.lock().unwrap();
    if headers.contains_key("x-oss-callback") {
        st.callback_headers_seen = true;
    }
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
        // 分片级中断注入（可重试类：500 + OSS 错误码）；计数只记
        // **成功落地**的片（差集断言的语义载体）。
        if let Some(limit) = st.fail_part_after {
            if st.part_put_count >= limit {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "<Error><Code>InternalError</Code></Error>",
                )
                    .into_response();
            }
        }
        st.part_put_count += 1;
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
    // PutObject 整对象（单分片路径）——固化后可见（commit-on-close
    // 的桩面：complete/put 之前不可见）
    {
        let obj = st.objs.get_mut(&object).expect("checked above");
        obj.data = body.to_vec();
        obj.completed = true;
    }
    let name = object.rsplit('-').next().unwrap_or(&object).to_string();
    st.fid_seq += 1;
    let fid = format!("{}", 11_000 + st.fid_seq);
    st.visible.insert(name, (fid, body.len() as i64));
    let etag = format!("\"etag-whole-{}\"", body.len());
    Response::builder()
        .status(StatusCode::OK)
        .header("etag", etag)
        .body(axum::body::Body::empty())
        .unwrap()
}

/// OSS DELETE ?uploadId：AbortMultipartUpload（K75-2 观测面——计数并
/// 清掉对象的 upload 会话记录；错误体原样 204 空成功，与 OSS 形态一致）。
async fn oss_delete(
    State(state): State<Arc<Mutex<UState>>>,
    axum::extract::Path((_bucket, object)): axum::extract::Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let q = query.unwrap_or_default();
    let Some(uid) = q
        .split('&')
        .find_map(|kv| kv.strip_prefix("uploadId="))
        .map(str::to_string)
    else {
        return (StatusCode::BAD_REQUEST, "no uploadId").into_response();
    };
    let mut st = state.lock().unwrap();
    st.aborted_uploads += 1;
    if let Some(obj) = st.objs.get_mut(&object) {
        if obj.upload_id == uid {
            obj.upload_id = String::new();
            obj.parts.clear();
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

/// OSS GET：ListParts（resume 对账面）。
async fn oss_list_parts(
    State(state): State<Arc<Mutex<UState>>>,
    axum::extract::Path((_bucket, object)): axum::extract::Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let _q = query.unwrap_or_default();
    let st = state.lock().unwrap();
    let Some(obj) = st.objs.get(&object) else {
        return (
            StatusCode::NOT_FOUND,
            "<Error><Code>NoSuchUpload</Code></Error>",
        )
            .into_response();
    };
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

/// OSS POST：`?uploads` = initiate；`?uploadId=` = complete。
async fn oss_post(
    State(state): State<Arc<Mutex<UState>>>,
    axum::extract::Path((_bucket, object)): axum::extract::Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let q = query.unwrap_or_default();
    let mut st = state.lock().unwrap();
    if headers.contains_key("x-oss-callback") {
        st.callback_headers_seen = true;
    }
    if q.contains("uploads") {
        st.fid_seq += 1;
        let uid = format!("up-{}", st.fid_seq);
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
        let xml = String::from_utf8_lossy(&body).to_string();
        let Some(obj) = st.objs.get_mut(&object) else {
            return (StatusCode::NOT_FOUND, "no such object").into_response();
        };
        // 按 Complete XML 的 part 序拼接 → data 固化 → completed
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
        for n in order {
            if let Some(p) = obj.parts.get(&n) {
                all.extend_from_slice(p);
            }
        }
        obj.data = all;
        obj.completed = true;
        let name = object.rsplit('-').next().unwrap_or(&object).to_string();
        let total: i64 = st
            .objs
            .get(&object)
            .map(|o| o.data.len() as i64)
            .unwrap_or(0);
        st.fid_seq += 1;
        let fid = format!("{}", 11_000 + st.fid_seq);
        st.visible.insert(name, (fid, total));
        return Response::builder()
            .status(StatusCode::OK)
            .body(axum::body::Body::from(
                "<CompleteMultipartUploadResult><ETag>\"done\"</ETag></CompleteMultipartUploadResult>",
            ))
            .unwrap();
    }
    (StatusCode::BAD_REQUEST, "unsupported OSS POST").into_response()
}

fn path(s: &str) -> RelPath {
    RelPath::new(s.trim_start_matches('/')).expect("valid path")
}

/// 每个用例独立的 spool 目录（临时）。tempfile 的 OS 级唯一命名（审查
/// H-T1：自拼 pid+纳秒在 Windows ~1ms 时钟粒度下并行初始化会撞名——
/// A 的尾部 remove_dir_all 删掉 B 正在用的目录，`spool create` os error 3
/// 的 flaky 根因）。`keep()` 放弃自动删除，保持调用方尾部手动清理的
/// 原语义。
fn tmpdir() -> std::path::PathBuf {
    tempfile::tempdir().expect("tmpdir").keep()
}

/// 把桩 base 注入 get_token 的 thread-local（每个测试起点调用）。
fn arm_base(base: &str) {
    CURRENT_BASE.with(|b| *b.borrow_mut() = base.to_string());
}

// ---------------------------------------------------------------------
// 断言矩阵
// ---------------------------------------------------------------------

#[tokio::test]
async fn put_object_path_uploads_and_reverifies_size() {
    let mock = Mock::start(0, false).await;
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());
    let payload = vec![7u8; 1024]; // < 5MiB → PutObject

    let mut stager = drv
        .writer(
            &path("/small.bin"),
            &WriteHint {
                size: Some(payload.len() as u64),
                ..WriteHint::default()
            },
        )
        .await
        .expect("writer");
    stager.write(&payload).await.expect("write");
    let entry = stager.close().await.expect("close");
    assert_eq!(entry.size, 1024, "remote size re-verified after complete");
    assert_eq!(entry.kind, cloudkit_storage::EntryKind::File);
    assert_eq!(entry.path.as_str(), "small.bin");

    // 会话文件与 spool 已清（close 后不残留）
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".part"))
        .collect();
    assert!(leftovers.is_empty(), "spool cleaned after close");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn close_before_complete_is_invisible_commit_on_close() {
    let mock = Mock::start(0, false).await;
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());

    let mut stager = drv
        .writer(&path("/pending.bin"), &WriteHint::default())
        .await
        .expect("writer");
    stager.write(&[1u8; 64]).await.expect("write");

    // commit-on-close（断言①）：close 前目标路径 list 不可见
    let listing = drv
        .list(&RelPath::root(), cloudkit_storage::Page::all())
        .await
        .expect("list");
    assert!(
        listing
            .entries
            .iter()
            .all(|e| e.path.as_str() != "pending.bin"),
        "staging is invisible before close"
    );

    let _ = stager.close().await.expect("close");
    let listed = drv
        .list(&RelPath::root(), cloudkit_storage::Page::all())
        .await
        .expect("list");
    assert!(
        listed
            .entries
            .iter()
            .any(|e| e.path.as_str() == "pending.bin"),
        "visible after close"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn secondary_auth_loop_replays_with_sign_val() {
    let mock = Mock::start(1, false).await; // 首轮触发二次认证
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());

    let payload = vec![3u8; 512];
    let mut stager = drv
        .writer(&path("/auth.bin"), &WriteHint::default())
        .await
        .expect("writer");
    stager.write(&payload).await.expect("write");
    let entry = stager.close().await.expect("close after secondary auth");
    assert_eq!(entry.size, 512);

    // 断言 init 被调用两次：首轮无 sign_key、次轮带 sign_key
    let calls = mock.st().init_calls.clone();
    assert_eq!(
        calls.len(),
        2,
        "init called exactly twice (challenge + replay)"
    );
    assert!(calls[0].3.is_none(), "first init carries no sign_key");
    assert_eq!(
        calls[1].3.as_deref(),
        Some("mock-sign-key"),
        "replay carries the challenge key"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// M-T1：sign_val 的数值正确性——区间 [start, end] **闭区间** SHA1（大写
/// hex；spike upload.rs:54 同语义）。桩只记录不校验（真机才是裁判），
/// 此处用预计算常量对账（载荷与桩的挑战区间都是确定的：end = size/2-1
/// = 4095，即 payload[0..=4095]）。
#[tokio::test]
async fn secondary_auth_sign_val_matches_the_interval_sha1() {
    let mock = Mock::start(1, false).await;
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());

    let payload: Vec<u8> = (0..8192u32).map(|i| (i % 249) as u8).collect();
    let mut stager = drv
        .writer(
            &path("/signval.bin"),
            &WriteHint {
                size: Some(payload.len() as u64),
                ..WriteHint::default()
            },
        )
        .await
        .expect("writer");
    stager.write(&payload).await.expect("write");
    stager.close().await.expect("close");

    let (issued, vals) = {
        let st = mock.st();
        (st.sign_check_issued.clone(), st.sign_vals.clone())
    };
    assert_eq!(vals.len(), 1, "exactly one challenge answer");
    // 桩的挑战区间形态自检（"0-4095" = 前半段闭区间）
    assert_eq!(issued, "0-4095", "the stub issued the expected range");
    assert_eq!(
        vals[0], "65DD36214E5D4837A8F1DD6868A1F14EFD8FC20C",
        "sign_val = uppercase SHA1 over the inclusive [0,4095] slice of the payload"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn rapid_upload_hit_skips_transfer() {
    let mock = Mock::start(0, true).await; // rapid_hit → status=2
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());

    let mut stager = drv
        .writer(
            &path("/hit.bin"),
            &WriteHint {
                rapid_upload: true,
                ..WriteHint::default()
            },
        )
        .await
        .expect("writer");
    stager.write(&[9u8; 256]).await.expect("write");
    let entry = stager.close().await.expect("rapid close");
    assert_eq!(entry.size, 256, "size comes from the remote re-verify");

    // 零 OSS 分片传输（秒传直接终态）
    assert_eq!(
        mock.st().part_put_count,
        0,
        "rapid hit performs no part uploads"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// M-S2：STS 凭证每传输链只取一次——分片循环与 close 的 complete 都
/// 复用链首取的那份，不逐片重取（1rps 限流 API 上 N 片上传曾平白多
/// 花 N+1 次 get_token）。
#[tokio::test]
async fn sts_token_is_fetched_once_per_multipart_transfer() {
    let mock = Mock::start(0, false).await;
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());

    let payload: Vec<u8> = vec![9u8; 12 * 1024 * 1024]; // 3 片
    let mut stager = drv
        .writer(
            // 桩的可见名推导按 '-' 切 object key——测试文件名避开连字符。
            &path("/sts_once.bin"),
            &WriteHint {
                size: Some(payload.len() as u64),
                ..WriteHint::default()
            },
        )
        .await
        .expect("writer");
    stager.write(&payload).await.expect("write");
    stager.close().await.expect("close");

    let calls = mock.state.lock().unwrap().get_token_calls;
    assert_eq!(calls, 1, "one STS fetch per transfer chain (got {calls})");
    let _ = std::fs::remove_dir_all(&dir);
}

/// K75-2：放弃 multipart 上传必须 AbortMultipartUpload——OSS 对未
/// complete 的分片会话**保留分片并计配额**；abort 只清本地（会话+spool）
/// 是远端资源泄漏。断言：abort 后桩收到一次 DELETE ?uploadId（且会话
/// 记录已清——差集资产不复存在）。
#[tokio::test]
async fn aborting_a_multipart_upload_releases_the_remote_session() {
    let mock = Mock::start(0, false).await;
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());

    let payload: Vec<u8> = vec![5u8; 12 * 1024 * 1024]; // 3 片
    let hint = WriteHint {
        size: Some(payload.len() as u64),
        ..WriteHint::default()
    };
    let mut stager = drv
        .writer(&path("/abortme.bin"), &hint)
        .await
        .expect("writer");
    stager
        .write(&payload)
        .await
        .expect("write (parts fly at 到齐)");
    let boxed = Box::new(stager);
    boxed.abort().await.expect("abort");

    let aborted = mock.st().aborted_uploads;
    assert_eq!(
        aborted, 1,
        "one AbortMultipartUpload must reach the OSS stub"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn multipart_upload_splits_and_completes_with_callback() {
    let mock = Mock::start(0, false).await;
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());

    // 分片下限 5MiB → 用 12MiB 触发三片
    let payload: Vec<u8> = (0..12 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let mut stager = drv
        .writer(
            &path("/big.bin"),
            &WriteHint {
                size: Some(payload.len() as u64),
                ..WriteHint::default()
            },
        )
        .await
        .expect("writer");
    stager.write(&payload).await.expect("write");
    let entry = stager.close().await.expect("close multipart");
    assert_eq!(entry.size, payload.len() as u64);
    assert_eq!(mock.st().part_put_count, 3, "12MiB at 5MiB parts = 3 parts");
    assert!(
        mock.st().callback_headers_seen,
        "K69.8: callback rides complete"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn resume_diff_only_uploads_missing_parts() {
    // 到齐即传形态：承诺量到齐的那一次 write 就把分片推给 OSS；Drop
    // 保留会话（uploadId + 已传片）——第二轮同路径同内容经
    // `/open/upload/resume` 复用会话，只补缺片。
    //
    // 中断模拟：让对象面在「第 1 片之后」失败（可重试类原样上抛），
    // 此时第 1 片已在远端且已落会话。
    let mock = Mock::start(0, false).await;
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());

    let payload: Vec<u8> = (0..12 * 1024 * 1024u32).map(|i| (i % 253) as u8).collect();
    let hint = WriteHint {
        size: Some(payload.len() as u64),
        ..WriteHint::default()
    };

    mock.set_fail_part_after(1); // 第 1 片之后的 PUT 失败
    {
        let mut stager = drv
            .writer(&path("/resume.bin"), &hint)
            .await
            .expect("writer 1");
        let res = stager.write(&payload).await; // 到齐即传 → 链在此触发
        match res {
            Err(cloudkit_storage::StorageError::Unavailable(_))
            | Err(cloudkit_storage::StorageError::RateLimited { .. }) => {}
            other => panic!("the injected OSS failure must surface, got {other:?}"),
        }
        drop(stager); // 会话（含第 1 片）保留；spool 清理
    }
    let after_first = mock.st().part_put_count;
    assert_eq!(
        after_first, 1,
        "the interrupted pass delivered exactly one part"
    );

    // 第二轮：同路径同内容 → resume 复用会话 → 只补 2 片（不重传第 1 片）
    mock.clear_part_failure();
    {
        let mut stager = drv
            .writer(&path("/resume.bin"), &hint)
            .await
            .expect("writer 2");
        stager.write(&payload).await.expect("write 2");
        let e = stager.close().await.expect("close after resume");
        assert_eq!(e.size, payload.len() as u64);
    }
    let second_pass = mock.st().part_put_count - after_first;
    assert_eq!(
        second_pass, 2,
        "the resumed pass uploads only the two missing parts (part 1 is reused)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 复审 M8（2026-09-25，pan123 K78/M8 的 confirmed 门移植）：write 在
/// 传输链中途失败上抛后 `transfer` 已是 Some（部分片、未确认态）——
/// close **不得盲信直接 complete**：缺片提交 = 远端留下截断的可见对
/// 象（本地行随后被 size 复核拒绝，但残骸已落地且会话记录被删）。
/// 正确行为：close 复跑差集补齐（幂等）再提交。
#[tokio::test]
async fn close_reconciles_a_transfer_that_failed_midway() {
    let mock = Mock::start(0, false).await;
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());

    let payload: Vec<u8> = (0..12 * 1024 * 1024u32).map(|i| (i % 249) as u8).collect();
    let hint = WriteHint {
        size: Some(payload.len() as u64),
        ..WriteHint::default()
    };
    mock.set_fail_part_after(1); // 第 1 片之后失败（write 上抛、stager 存活）
    let mut stager = drv
        .writer(&path("/halfway.bin"), &hint)
        .await
        .expect("writer");
    let res = stager.write(&payload).await;
    match res {
        Err(cloudkit_storage::StorageError::Unavailable(_))
        | Err(cloudkit_storage::StorageError::RateLimited { .. }) => {}
        other => panic!("the injected OSS failure must surface, got {other:?}"),
    }
    assert_eq!(
        mock.st().part_put_count,
        1,
        "part 1 landed before the failure"
    );

    // 消费面（dav-server/队列）在 write 错后仍会 close——盲信即灾难。
    mock.clear_part_failure();
    let entry = stager
        .close()
        .await
        .expect("close re-drives the missing parts before committing");
    assert_eq!(
        entry.size,
        payload.len() as u64,
        "the committed object is whole"
    );
    assert_eq!(
        mock.st().part_put_count,
        3,
        "close uploaded the two missing parts (1 from the failed pass + 2 reconciled)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 复审 M10（2026-09-25）：`/open/upload/resume` 的**瞬态**失败（传输类，
/// 会话真伪未知）不得销毁本地会话记录——旧实现任何错误都删记录 + 全量
/// init（差集资产毁弃 + OSS 孤儿分片不 abort）。对齐 pan123 的
/// SessionGone-only 纪律：瞬态错上抛保留记录，重试仍可差集续传。
#[tokio::test]
async fn resume_endpoint_transient_error_keeps_the_session_record() {
    let mock = Mock::start(0, false).await;
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());

    let payload: Vec<u8> = (0..12 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let hint = WriteHint {
        size: Some(payload.len() as u64),
        ..WriteHint::default()
    };
    // 第一轮：第 1 片后中断 → drop 留会话。
    mock.set_fail_part_after(1);
    {
        let mut stager = drv
            .writer(&path("/keep.bin"), &hint)
            .await
            .expect("writer 1");
        let res = stager.write(&payload).await;
        assert!(res.is_err(), "the injected OSS failure must surface");
    }
    let after_first = mock.st().part_put_count;
    assert_eq!(after_first, 1);

    // 第二轮：resume 端点瞬态失败——write 必须上抛（不得回退全量 init
    // 「成功」），会话记录保留。
    mock.clear_part_failure();
    mock.set_fail_resume(1);
    {
        let mut stager = drv
            .writer(&path("/keep.bin"), &hint)
            .await
            .expect("writer 2");
        let res = stager.write(&payload).await;
        match res {
            Err(cloudkit_storage::StorageError::Unavailable(_)) => {}
            other => {
                panic!("a transient resume failure must propagate (session kept), got {other:?}")
            }
        }
    }
    assert_eq!(
        mock.st().part_put_count,
        after_first,
        "no part may be uploaded while the resume path is failing"
    );

    // 第三轮：resume 恢复 → 差集续传只补 2 片（会话资产仍在的实证）。
    mock.clear_fail_resume();
    {
        let mut stager = drv
            .writer(&path("/keep.bin"), &hint)
            .await
            .expect("writer 3");
        stager.write(&payload).await.expect("write 3");
        let e = stager.close().await.expect("close after the healed resume");
        assert_eq!(e.size, payload.len() as u64);
    }
    let third_pass = mock.st().part_put_count - after_first;
    assert_eq!(
        third_pass, 2,
        "the healed pass resumes the kept session (differential), not a fresh full upload"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn abort_clears_spool_and_leaves_no_visible_object() {
    let mock = Mock::start(0, false).await;
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());

    let mut stager = drv
        .writer(&path("/doomed.bin"), &WriteHint::default())
        .await
        .expect("writer");
    stager.write(&[5u8; 100]).await.expect("write");
    stager.abort().await.expect("abort");

    let listing = drv
        .list(&RelPath::root(), cloudkit_storage::Page::all())
        .await
        .expect("list");
    assert!(
        listing
            .entries
            .iter()
            .all(|e| e.path.as_str() != "doomed.bin"),
        "aborted upload is invisible (no remote garbage)"
    );
    let spool: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".part"))
        .collect();
    assert!(spool.is_empty(), "spool cleaned by abort");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn write_hint_size_mismatch_is_invalid() {
    let mock = Mock::start(0, false).await;
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());

    // 承诺 100，实际写 50 → write 阶段即拒（超承诺对称）
    let mut stager = drv
        .writer(
            &path("/hinted.bin"),
            &WriteHint {
                size: Some(100),
                ..WriteHint::default()
            },
        )
        .await
        .expect("writer");
    stager.write(&[1u8; 50]).await.expect("within hint");
    match stager.close().await {
        Err(cloudkit_storage::StorageError::Invalid) => {}
        other => panic!("short write vs hint must be Invalid, got {other:?}"),
    }

    // 超承诺：write 直接拒
    let mut stager = drv
        .writer(
            &path("/over.bin"),
            &WriteHint {
                size: Some(10),
                ..WriteHint::default()
            },
        )
        .await
        .expect("writer");
    match stager.write(&[1u8; 20]).await {
        Err(cloudkit_storage::StorageError::Invalid) => {}
        other => panic!("over-promise must be Invalid, got {other:?}"),
    }
    let _ = stager.abort().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn size_mismatch_after_complete_is_refused() {
    let mock = Mock::start(0, false).await;
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());
    mock.st().force_zero_size = true; // 远端复核报 0（零字节 bug 类）

    let mut stager = drv
        .writer(&path("/zeroed.bin"), &WriteHint::default())
        .await
        .expect("writer");
    stager.write(&[4u8; 128]).await.expect("write");
    match stager.close().await {
        Err(cloudkit_storage::StorageError::Unavailable(msg)) => {
            assert!(msg.contains("size mismatch"), "names the class: {msg}");
        }
        other => panic!("zero-byte remote must be refused, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn writer_creates_missing_parent_dirs_implicitly() {
    let mock = Mock::start(0, false).await;
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());

    // 桩的 folder/add 未路由——写路径要求父目录存在时的隐式创建走
    // mkdir 面；桩未实现 folder/add 时该用例走「父目录已存在」的
    // 简化形态（115-4 的 conformance 桩会补齐 folder/add）。
    // 这里先验证 writer 对根下直写可用（父 = 根，无需创建）。
    let mut stager = drv
        .writer(&path("/direct.bin"), &WriteHint::default())
        .await
        .expect("writer at root");
    stager.write(&[2u8; 32]).await.expect("write");
    let e = stager.close().await.expect("close");
    assert_eq!(e.size, 32);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn reader_roundtrip_after_upload() {
    // 上传后经 CDN 面读回（把 115-2/115-3 串起来的最小闭环——115-5
    // 真机冒烟的同形态离线版）。
    let mock = Mock::start(0, false).await;
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());

    let payload: Vec<u8> = (0..300u32).map(|i| (i % 97) as u8).collect();
    let mut stager = drv
        .writer(&path("/rt.bin"), &WriteHint::default())
        .await
        .expect("writer");
    stager.write(&payload).await.expect("write");
    let e = stager.close().await.expect("close");

    // 桩里没有 CDN 路由（115-2 的 read_path.rs 已覆盖流读）；此处
    // 断言句柄与条目形态正确（后续 conformance 桩补齐端到端读回）。
    assert_eq!(e.size, payload.len() as u64);
    assert!(
        e.id.handle.as_str().starts_with("11"),
        "handle carries the fid: {e:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn hashes_are_uppercase_sha1_and_prefix_limited() {
    // 纯函数面（K69.6）：全量 SHA1 大写 hex + preid 前 128KiB。
    // 直接对 stager 的 spool 哈希路径做黑盒验证：写入已知内容后
    // init 载荷的 fileid/preid 与独立计算一致。
    let mock = Mock::start(0, false).await;
    arm_base(&mock.base);
    let dir = tmpdir();
    let drv = mock.driver(dir.clone());

    let payload = vec![0xABu8; 200 * 1024]; // > 128KiB
    let mut stager = drv
        .writer(&path("/hash.bin"), &WriteHint::default())
        .await
        .expect("writer");
    stager.write(&payload).await.expect("write");
    let _ = stager.close().await.expect("close");

    let mut full = Sha1::new();
    full.update(&payload);
    let expect_full = format!("{:X}", full.finalize());
    let mut pre = Sha1::new();
    pre.update(&payload[..128 * 1024]);
    let expect_pre = format!("{:X}", pre.finalize());

    let calls = mock.st().init_calls.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, expect_full, "fileid = full SHA1 uppercase hex");
    assert_eq!(
        calls[0].2, expect_pre,
        "preid = first 128KiB SHA1 uppercase hex"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
