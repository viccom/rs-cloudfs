//! 错误码映射契约测试（Phase 6 / 123-1；认证面真机采样钉死——后续
//! 批扩读写面码）。
//!
//! 语义契约（断言即契约，实现者禁改；黄金参照 = 123-0 spike 真机采样
//! `examples/pan123_spike` + 跟踪单 123-0 批次日志 + K76.4/D5）：
//!
//! - **envelope 顶层双拼**：`code/Code`、`message/Message`、`data/Data`
//!   双形态都必须解析（2026-08-29 服务端代际重组后的实证混拼）；
//! - **双成功码**：`code==0` 通用成功（业务面判据）；`code==200` 仅
//!   认证类成功（sign_in 与 QR 确认态——真机实证唯一非 0 成功码）；
//!   业务面 `is_ok` 对 200 **不放行**；
//! - **认证面分类表**：`20101`（未登录——list 面）/ `401`（未登录
//!   ——user 面，"cookie token is empty"）→ `Unauthorized{
//!   recoverable:false}`（**web API 无 refresh**，K76.4——重扫码是
//!   唯一出路，绝不重试/刷新）；
//! - **未知码** → `Unavailable` 且载荷保留原始码与消息（R2 可诊断）；
//! - **非 JSON 响应**（HTML 错误页/空体）→ `Unavailable` 不崩穿，
//!   截断片段经当前 token 掩码（R3——错误页可能回显请求头）。

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use ck_pan123::api::{classify, map_rejection, Envelope, ErrKind, Pan123Client};
use cloudkit_storage::StorageError;

// ------------------------------------------------------ envelope 双拼 ---

#[test]
fn envelope_accepts_dual_cased_top_level_keys() {
    // 小写形态（list/info/user 族真机形态）
    let lower: Envelope =
        serde_json::from_str(r#"{"code":0,"message":"ok","data":{"InfoList":[],"Total":0}}"#)
            .expect("lowercase envelope parses");
    assert!(lower.is_ok());
    assert_eq!(lower.message, "ok");
    assert!(
        lower.data.get("InfoList").is_some(),
        "data survives parsing"
    );

    // 大写形态（代际混拼实证）
    let upper: Envelope =
        serde_json::from_str(r#"{"Code":5060,"Message":"检测到1个同名文件","Data":{"etag":"x"}}"#)
            .expect("PascalCase envelope parses");
    assert!(!upper.is_ok());
    assert_eq!(upper.code, 5060);
    assert_eq!(upper.message, "检测到1个同名文件");

    // 字段缺省容错：裸 code 不炸（message/data 缺省；code 键本身必填——
    // M2/K78 后缺 code 的 JSON 落终态 Unavailable，`{}` 不再解析成功）
    let bare: Envelope = serde_json::from_str(r#"{"code":0}"#).expect("bare envelope");
    assert!(bare.is_ok());
    assert_eq!(bare.message, "", "message defaults empty");
    assert!(bare.data.is_null(), "data defaults null");
}

#[test]
fn dual_success_codes_split_by_face() {
    // 0 = 通用成功（两脸都认）
    let ok: Envelope = serde_json::from_str(r#"{"code":0}"#).expect("ok");
    assert!(ok.is_ok());
    assert!(ok.is_auth_ok());

    // 200 = 仅认证类成功（sign_in / QR 确认态）
    let auth: Envelope =
        serde_json::from_str(r#"{"code":200,"data":{"token":"x"}}"#).expect("auth");
    assert!(!auth.is_ok(), "the business face must NOT pass code==200");
    assert!(auth.is_auth_ok(), "the auth face passes code==200");

    // 其他值两脸都不认
    for code in [1i64, 400, 403, 5060] {
        let env: Envelope = serde_json::from_str(&format!(r#"{{"code":{code}}}"#)).expect("env");
        assert!(!env.is_ok(), "code {code}");
        assert!(!env.is_auth_ok(), "code {code}");
    }
}

// ---------------------------------------------------------- 分类表 ---

#[test]
fn classify_covers_the_sampled_auth_codes() {
    // 未登录族：错误码按端点族分叉（真机实证）
    assert_eq!(classify(20101), ErrKind::NotLoggedIn, "list face");
    assert_eq!(classify(401), ErrKind::NotLoggedIn, "user face");
}

/// 123-2 读写面扩充（真机采样入表）：5060 同名冲突 / 5113+5114 流量
/// 限额 / -1 rpc 形态 / 400 参数类。
#[test]
fn classify_covers_the_read_write_face_codes() {
    assert_eq!(classify(5060), ErrKind::NameConflict, "mkdir/upload face");
    assert_eq!(
        classify(5113),
        ErrKind::TrafficExceeded,
        "daily traffic cap"
    );
    assert_eq!(
        classify(5114),
        ErrKind::TrafficExceeded,
        "daily traffic cap twin"
    );
    assert_eq!(classify(-1), ErrKind::RpcFailure, "rpc MalformedXML family");
    assert_eq!(classify(400), ErrKind::BadParams, "parameter validation");
    // 真正的未知码仍是 Rejected（终态 Unavailable 保留原码）
    for code in [0i64, 1, 105, 47002] {
        assert_eq!(classify(code), ErrKind::Rejected, "code {code}");
    }
}

#[test]
fn map_rejection_routes_each_kind_to_its_storage_error() {
    // 未登录：无 refresh（K76.4）——不可恢复，重扫码是唯一出路
    assert_eq!(
        map_rejection(20101, "未登录"),
        StorageError::Unauthorized { recoverable: false }
    );
    assert_eq!(
        map_rejection(401, "cookie token is empty"),
        StorageError::Unauthorized { recoverable: false }
    );

    // 5060 同名冲突 → Exists（mkdir 预检竞争窗口的兜底终态）
    assert_eq!(
        map_rejection(5060, "检测到1个同名文件"),
        StorageError::Exists
    );

    // 5113/5114 → RateLimited（D5：流量限额不绕过）
    assert_eq!(
        map_rejection(5113, "流量限额"),
        StorageError::RateLimited { retry_after: None }
    );
    assert_eq!(
        map_rejection(5114, "流量超限"),
        StorageError::RateLimited { retry_after: None }
    );

    // -1 rpc 形态 → Io 保留原码与消息（可诊断）
    match map_rejection(-1, "rpc MalformedXML") {
        StorageError::Io(detail) => {
            assert!(detail.contains("-1"), "{detail}");
            assert!(detail.contains("MalformedXML"), "{detail}");
        }
        other => panic!("rpc failure is Io, got {other:?}"),
    }

    // 400 参数类 → Invalid
    assert_eq!(
        map_rejection(400, "The Fids field is required"),
        StorageError::Invalid
    );

    // 未知码：Unavailable 载荷保留原始码与后端消息（R2）
    match map_rejection(47002, "boom") {
        StorageError::Unavailable(detail) => {
            assert!(detail.contains("47002"), "keeps the raw code: {detail}");
            assert!(
                detail.contains("boom"),
                "keeps the backend message: {detail}"
            );
        }
        other => panic!("unknown code must map to Unavailable, got {other:?}"),
    }
}

#[test]
fn map_rejection_payloads_never_echo_the_data_member() {
    // R3 防御钉：错误消息只拼 code/message——token 端点成功响应的 data
    // 内含凭据，错误文本绝不能带 data。
    let err = map_rejection(47002, "boom");
    match err {
        StorageError::Unavailable(detail) => {
            assert!(
                !detail.contains("token") && !detail.contains("password"),
                "no credential field names in payloads: {detail}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

// ------------------------------------------------ client 回放（mock） ---
//
// mock 形态（axum 内存 123pan web API）：错误包承载于 envelope（判据
// 在 code，不在 HTTP 状态码）；user 面/业务面各一条腿。

const TOKEN: &str = "mock-token-0123456789abcdef";

#[derive(Default)]
struct MockState {
    /// 错误注入队列（顶掉下一次业务请求的 envelope）。
    injected: std::collections::VecDeque<Value>,
    /// 原始应答注入（M2/K78：HTTP 状态 + 原文 body——钉 HTTP 门与
    /// code 键必填的网关形态）。
    injected_raw: std::collections::VecDeque<(u16, String)>,
    recorded: Vec<Recorded>,
}

#[derive(Debug, Clone)]
struct Recorded {
    path: String,
    bearer: Option<String>,
}

struct Mock123 {
    state: Arc<Mutex<MockState>>,
    base: String,
}

impl Mock123 {
    async fn start() -> Mock123 {
        let state = Arc::new(Mutex::new(MockState::default()));
        let app = Router::new()
            .route("/b/api/user/info", get(user_info))
            .route("/api/file/list/new", get(list_new))
            .route("/b/api/file/rename", post(rename))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock listener");
        let addr = listener.local_addr().expect("mock addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("mock serve") });
        Mock123 {
            state,
            base: format!("http://{addr}"),
        }
    }

    fn inject(&self, body: Value) {
        self.state.lock().unwrap().injected.push_back(body);
    }

    /// M2/K78：注入原始应答（HTTP 状态 + 原文 body——网关形态腿）。
    fn inject_raw(&self, status: u16, body: &str) {
        self.state
            .lock()
            .unwrap()
            .injected_raw
            .push_back((status, body.to_string()));
    }

    fn recorded(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().recorded.clone()
    }

    fn client(&self) -> Pan123Client {
        Pan123Client::new(
            TOKEN.to_string(),
            self.base.clone(),
            self.base.clone(),
            Some("mock-login-uuid".to_string()),
        )
        .expect("client constructs")
    }
}

async fn handle_common(
    state: &Arc<Mutex<MockState>>,
    headers: &HeaderMap,
    path: &str,
) -> Option<axum::response::Response> {
    let mut state = state.lock().unwrap();
    state.recorded.push(Recorded {
        path: path.to_string(),
        bearer: headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
    });
    if let Some((status, body)) = state.injected_raw.pop_front() {
        let resp = axum::http::Response::builder()
            .status(StatusCode::from_u16(status).expect("injectable status"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .expect("raw injectable response");
        return Some(resp);
    }
    if let Some(injected) = state.injected.pop_front() {
        return Some((StatusCode::OK, Json(injected)).into_response());
    }
    None
}

async fn user_info(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Some(reply) = handle_common(&state, &headers, "/b/api/user/info").await {
        return reply;
    }
    (
        StatusCode::OK,
        Json(json!({"code": 0, "data": {"UID": 42, "SpacePermanent": 2199023255552i64}})),
    )
        .into_response()
}

async fn list_new(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Some(reply) = handle_common(&state, &headers, "/api/file/list/new").await {
        return reply;
    }
    (
        StatusCode::OK,
        Json(json!({"code": 0, "data": {"InfoList": [], "Total": 0}})),
    )
        .into_response()
}

async fn rename(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Some(reply) = handle_common(&state, &headers, "/b/api/file/rename").await {
        return reply;
    }
    (StatusCode::OK, Json(json!({"code": 0, "data": {}}))).into_response()
}

#[tokio::test]
async fn list_face_20101_maps_to_unauthorized_not_recoverable() {
    let mock = Mock123::start().await;
    let client = mock.client();

    mock.inject(json!({"code": 20101, "message": "未登录"}));
    let err = client
        .dispatch_get("/api/file/list/new", &[], "list/new")
        .await
        .expect_err("20101 must surface");
    assert_eq!(
        err,
        StorageError::Unauthorized { recoverable: false },
        "no refresh exists (K76.4) - the re-scan is the only way out"
    );
    // 恰一次请求（无刷新/重试循环）
    assert_eq!(mock.recorded().len(), 1);
}

#[tokio::test]
async fn user_face_401_maps_to_unauthorized_not_recoverable() {
    let mock = Mock123::start().await;
    let client = mock.client();

    mock.inject(json!({"code": 401, "message": "cookie token is empty"}));
    let err = client.user_info().await.expect_err("401 must surface");
    assert_eq!(
        err,
        StorageError::Unauthorized { recoverable: false },
        "user-face not-logged-in is the same terminal state"
    );
    assert_eq!(mock.recorded().len(), 1, "no retry, no loop");
}

#[tokio::test]
async fn user_info_parses_and_the_bearer_header_rides() {
    let mock = Mock123::start().await;
    let client = mock.client();

    let info = client.user_info().await.expect("user/info succeeds");
    assert_eq!(info.uid, 42);
    assert_eq!(info.space_permanent, 2199023255552);
    let recorded = mock.recorded();
    assert_eq!(recorded[0].path, "/b/api/user/info");
    assert_eq!(
        recorded[0].bearer.as_deref(),
        Some(format!("Bearer {TOKEN}").as_str()),
        "the single Bearer head carries the token (spike: bearer alone suffices)"
    );
}

#[tokio::test]
async fn unknown_code_maps_to_unavailable_keeping_the_code() {
    let mock = Mock123::start().await;
    let client = mock.client();

    // 47002 = 真未知码（5060/5113/5114/-1/400 已在 123-2 入表——本用例
    // 钉未知码的 R2 可诊断形态）。
    mock.inject(json!({"code": 47002, "message": "boom"}));
    let err = client
        .dispatch_post_json("/b/api/file/rename", &json!({"fileId": 1}), "rename")
        .await
        .expect_err("47002 must surface");
    match err {
        StorageError::Unavailable(detail) => {
            assert!(detail.contains("47002"), "{detail}");
            assert!(detail.contains("rename"), "carries the stage: {detail}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[tokio::test]
async fn non_json_response_maps_to_unavailable_with_a_masked_snippet() {
    // web-pro2 中继页等 HTML 形态：serde 不得崩穿；截断片段里若回显了
    // 请求头中的 token，必须先掩码（R3）。
    let app = Router::new().route(
        "/api/file/list/new",
        get(|| async {
            (
                StatusCode::OK,
                "<html><body>gateway error: Bearer ".to_string() + TOKEN + "</body></html>",
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    let client = Pan123Client::new(
        TOKEN.to_string(),
        format!("http://{addr}"),
        format!("http://{addr}"),
        None,
    )
    .expect("client");
    let err = client
        .dispatch_get("/api/file/list/new", &[], "list/new")
        .await
        .expect_err("HTML must not parse as an envelope");
    match err {
        StorageError::Unavailable(detail) => {
            assert!(detail.contains("non-json"), "{detail}");
            assert!(
                !detail.contains(TOKEN),
                "the echoed token must be masked, not verbatim: {detail}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

// ------------------------------------------ M2（K78）HTTP 门 + code 必填 ---

/// M2 ①：HTTP 403 + 网关 JSON（**无 code 键**，WAF `{"error":"forbidden"}`
/// 形态）→ `Unavailable` 终态——不得经 `code` 缺省 0 误判成功（旧形态：
/// UserInfo 全字段 default → 静默 Ok(uid=0)）。
#[tokio::test]
async fn gateway_json_without_code_over_http_403_is_terminal_unavailable() {
    let mock = Mock123::start().await;
    let client = mock.client();

    mock.inject_raw(403, r#"{"error":"forbidden"}"#);
    let err = client
        .user_info()
        .await
        .expect_err("a code-less gateway body must never pass as success");
    match err {
        StorageError::Unavailable(detail) => {
            assert!(detail.contains("403"), "carries the HTTP status: {detail}");
            assert!(
                detail.contains("forbidden"),
                "carries the (masked) body snippet: {detail}"
            );
            assert!(
                !detail.contains("data parse"),
                "not downstream parse noise — the gate fires at dispatch: {detail}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert_eq!(mock.recorded().len(), 1, "terminal, no retry");
}

/// M2 ①b：HTTP 403 + **完整 code:0 信封**（网关把拒绝写成成功形态）→
/// HTTP 门终态 `Unavailable`——成功判定必须伴随 2xx。
#[tokio::test]
async fn an_ok_envelope_over_non_2xx_is_still_terminal_unavailable() {
    let mock = Mock123::start().await;
    let client = mock.client();

    mock.inject_raw(403, r#"{"code":0,"message":"ok","data":{"UID":42}}"#);
    let err = client
        .user_info()
        .await
        .expect_err("an ok envelope must not pass over a non-2xx status");
    match err {
        StorageError::Unavailable(detail) => {
            assert!(detail.contains("403"), "carries the HTTP status: {detail}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert_eq!(mock.recorded().len(), 1);
}

/// M2 ②（回归守卫）：HTTP 401 + envelope 显式 `code:401` → 仍
/// `Unauthorized{recoverable:false}`——真机 user/info 面 = HTTP 401 +
/// envelope 形态，HTTP 门不得吞掉 errno 映射。
#[tokio::test]
async fn http_401_with_an_explicit_envelope_still_maps_to_unauthorized() {
    let mock = Mock123::start().await;
    let client = mock.client();

    mock.inject_raw(401, r#"{"code":401,"message":"cookie token is empty"}"#);
    let err = client.user_info().await.expect_err("401 must surface");
    assert_eq!(
        err,
        StorageError::Unauthorized { recoverable: false },
        "the envelope error mapping survives the HTTP gate"
    );
    assert_eq!(mock.recorded().len(), 1, "no retry, no loop");
}

/// M2 ③：HTTP 200 + 无 code 键 JSON → `Unavailable` 终态（`code` 键
/// 必填——message/data 的缺省容错保留）。
#[tokio::test]
async fn http_200_body_without_a_code_key_is_terminal_unavailable() {
    let mock = Mock123::start().await;
    let client = mock.client();

    mock.inject_raw(200, r#"{"error":"forbidden"}"#);
    let err = client
        .user_info()
        .await
        .expect_err("code is a mandatory envelope key");
    match err {
        StorageError::Unavailable(detail) => {
            assert!(
                detail.contains("code"),
                "the message distinguishes the missing-code form: {detail}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert_eq!(mock.recorded().len(), 1, "terminal, no retry");
}
