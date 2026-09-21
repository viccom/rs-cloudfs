//! QR/sign_in 状态机契约测试（Phase 6 / 123-1）。
//!
//! 语义契约（断言即契约，实现者禁改；黄金参照 = 123-0 真机 +
//! 123panNextGen `session.py:455-617` 深读 K76 + 计划 §8-D5）：
//!
//! - **QR generate**（`GET login.123pan.com/api/user/qr-code/generate`，
//!   web 头——`loginuuid`/`app-version:3`/`platform:web`，无 Bearer）：
//!   `code==0` → `data.uniID`（双拼 `uniId`）+ `data.url`；
//! - **QR result 状态机**（`GET .../qr-code/result?uniID=`）：`code==200`
//!   = 确认态**直返 token**（§5.10 唯一非 0 成功码的第二现场）；
//!   `code==0` 下 `data.loginStatus`：0 等 / 1 已扫待确认 / 2 拒 /
//!   4 过期；未知值 `Unavailable`；
//! - **wx_code**（`POST .../qr-code/wx_code` `{"uniID"}`）：`code==0` →
//!   `data.wxCode`（空串 → `None`——无人扫码真机实证形态）；
//! - **sign_in**（`POST {api}/b/api/user/sign_in`
//!   `{"type":1,"passport","password"}` 明文）：成功码 **200**、token
//!   在 `data.token`；**首存**——成功即 `TokenStore::save_token` 恰一次
//!   （无 refresh 协议下的 K13 形态）；失败码 → `Unavailable` 保留
//!   原码与后端消息（R3：不回显请求体）。
//!
//! mock 形态（axum 内存 login.123pan.com + api 域同 base 不同路径）：
//! result 端点按注入队列出状态（状态机全状态遍历）；sign_in 端点校验
//! 载荷形态并记录。

use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use ck_pan123::api::web_http_client;
use ck_pan123::oauth::{self, QrPoll, TokenStore};
use cloudkit_storage::StorageError;

// ---------------------------------------------------------------------
// mock 后端
// ---------------------------------------------------------------------

#[derive(Default)]
struct MockState {
    /// result 轮询的出队状态（loginStatus 或 code==200 确认包）。
    poll_queue: std::collections::VecDeque<Value>,
    /// wx_code 应答（None = 未挂路由形态由默认 404 承担；Some("") 空）。
    wx_code: Option<String>,
    /// generate 端点的原始应答注入（M2/K78：HTTP 状态 + 原文 body——
    /// 钉 read_envelope 的 2xx 成功门）。
    generate_raw: Option<(u16, String)>,
    recorded: Vec<Recorded>,
}

#[derive(Debug, Clone)]
struct Recorded {
    path: String,
    headers: Vec<(String, String)>,
    query: String,
    body: String,
}

struct MockLogin {
    state: Arc<Mutex<MockState>>,
    base: String,
}

impl MockLogin {
    async fn start() -> MockLogin {
        let state = Arc::new(Mutex::new(MockState::default()));
        let app = Router::new()
            .route("/api/user/qr-code/generate", get(qr_generate))
            .route("/api/user/qr-code/result", get(qr_result))
            .route("/api/user/qr-code/wx_code", post(qr_wx_code))
            .route("/b/api/user/sign_in", post(sign_in))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock listener");
        let addr = listener.local_addr().expect("mock addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("mock serve") });
        MockLogin {
            state,
            base: format!("http://{addr}"),
        }
    }

    fn push_poll(&self, reply: Value) {
        self.state.lock().unwrap().poll_queue.push_back(reply);
    }

    fn set_wx_code(&self, code: &str) {
        self.state.lock().unwrap().wx_code = Some(code.to_string());
    }

    /// M2/K78：注入 generate 端点的原始应答（HTTP 状态 + 原文 body）。
    fn set_generate_raw(&self, status: u16, body: &str) {
        self.state.lock().unwrap().generate_raw = Some((status, body.to_string()));
    }

    fn recorded(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().recorded.clone()
    }

    fn http() -> reqwest::Client {
        web_http_client("mock-login-uuid").expect("web identity client")
    }
}

fn record(state: &Arc<Mutex<MockState>>, headers: &HeaderMap, path: &str, query: &str, body: &str) {
    let mut row = Vec::new();
    for name in [
        "authorization",
        "loginuuid",
        "app-version",
        "platform",
        "origin",
    ] {
        if let Some(v) = headers.get(name) {
            row.push((name.to_string(), v.to_str().unwrap_or("").to_string()));
        }
    }
    state.lock().unwrap().recorded.push(Recorded {
        path: path.to_string(),
        headers: row,
        query: query.to_string(),
        body: body.to_string(),
    });
}

async fn qr_generate(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
) -> axum::response::Response {
    record(&state, &headers, "generate", "", "");
    let raw = state.lock().unwrap().generate_raw.take();
    if let Some((status, body)) = raw {
        return axum::http::Response::builder()
            .status(axum::http::StatusCode::from_u16(status).expect("injectable status"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .expect("raw injectable response");
    }
    (
        axum::http::StatusCode::OK,
        Json(
            json!({"code": 0, "data": {"uniID": "mock-uni-id", "url": "https://www.123pan.com/wx-app-login.html?uniID=mock-uni-id"}}),
        ),
    )
        .into_response()
}

async fn qr_result(
    State(state): State<Arc<Mutex<MockState>>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> (axum::http::StatusCode, Json<Value>) {
    let query = format!("uniID={}", q.get("uniID").cloned().unwrap_or_default());
    record(&state, &headers, "result", &query, "");
    let reply = state
        .lock()
        .unwrap()
        .poll_queue
        .pop_front()
        .unwrap_or_else(|| json!({"code": 0, "data": {"loginStatus": 0}}));
    (axum::http::StatusCode::OK, Json(reply))
}

async fn qr_wx_code(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
    body: String,
) -> (axum::http::StatusCode, Json<Value>) {
    record(&state, &headers, "wx_code", "", &body);
    let wx = state.lock().unwrap().wx_code.clone().unwrap_or_default();
    (
        axum::http::StatusCode::OK,
        Json(json!({"code": 0, "data": {"wxCode": wx}})),
    )
}

async fn sign_in(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
    body: String,
) -> (axum::http::StatusCode, Json<Value>) {
    record(&state, &headers, "sign_in", "", &body);
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let ok = parsed.get("type").and_then(Value::as_i64) == Some(1)
        && parsed.get("passport").and_then(Value::as_str) == Some("mock-passport")
        && parsed.get("password").and_then(Value::as_str) == Some("mock-password-0");
    if ok {
        (
            axum::http::StatusCode::OK,
            Json(json!({"code": 200, "data": {"token": "mock-token-from-signin"}})),
        )
    } else {
        (
            axum::http::StatusCode::OK,
            Json(json!({"code": 400, "message": "mock bad credentials"})),
        )
    }
}

// ---------------------------------------------------------------------
// 观测面：TokenStore 记录器
// ---------------------------------------------------------------------

#[derive(Default)]
struct RecordingTokenStore {
    calls: Mutex<Vec<String>>,
}

impl TokenStore for RecordingTokenStore {
    fn save_token(&self, token: &str) {
        self.calls.lock().unwrap().push(token.to_string());
    }
}

impl RecordingTokenStore {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------
// 状态机用例
// ---------------------------------------------------------------------

#[tokio::test]
async fn qr_generate_returns_the_session_with_web_headers_only() {
    let mock = MockLogin::start().await;
    let http = MockLogin::http();

    let session = oauth::qr_generate(&http, &mock.base)
        .await
        .expect("generate succeeds");
    assert_eq!(session.uni_id, "mock-uni-id");
    assert!(
        session.url.starts_with("https://www.123pan.com/"),
        "the QR content is the scan landing URL: {}",
        session.url
    );

    // web 头纪律：三头齐 + 无 Bearer（QR 面无 token）
    let recorded = mock.recorded();
    let row = &recorded[0];
    let header = |name: &str| {
        row.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(header("loginuuid").as_deref(), Some("mock-login-uuid"));
    assert_eq!(header("app-version").as_deref(), Some("3"));
    assert_eq!(header("platform").as_deref(), Some("web"));
    assert_eq!(
        header("authorization"),
        None,
        "the QR face carries no Bearer token"
    );
}

#[tokio::test]
async fn qr_poll_walks_the_full_login_status_state_machine() {
    let mock = MockLogin::start().await;
    let http = MockLogin::http();

    // 0 等 / 1 已扫待确认 / 2 拒
    mock.push_poll(json!({"code": 0, "data": {"loginStatus": 0}}));
    mock.push_poll(json!({"code": 0, "data": {"loginStatus": 1}}));
    mock.push_poll(json!({"code": 0, "data": {"loginStatus": 2}}));
    assert_eq!(
        oauth::qr_poll(&http, &mock.base, "uni")
            .await
            .expect("poll 0"),
        QrPoll::Waiting
    );
    assert_eq!(
        oauth::qr_poll(&http, &mock.base, "uni")
            .await
            .expect("poll 1"),
        QrPoll::Scanned
    );
    assert_eq!(
        oauth::qr_poll(&http, &mock.base, "uni")
            .await
            .expect("poll 2"),
        QrPoll::Refused
    );

    // 4 过期
    mock.push_poll(json!({"code": 0, "data": {"loginStatus": 4}}));
    assert_eq!(
        oauth::qr_poll(&http, &mock.base, "uni")
            .await
            .expect("poll 4"),
        QrPoll::Expired
    );

    // 确认态：code==200 直返 token（唯一非 0 成功码）
    mock.push_poll(json!({"code": 200, "data": {"token": "mock-token-qr"}}));
    assert_eq!(
        oauth::qr_poll(&http, &mock.base, "uni")
            .await
            .expect("confirm"),
        QrPoll::Confirmed {
            token: "mock-token-qr".to_string()
        }
    );

    // 防御臂：code==0 下 loginStatus==3 且带 token → 同归 Confirmed
    mock.push_poll(json!({"code": 0, "data": {"loginStatus": 3, "token": "mock-token-3"}}));
    assert_eq!(
        oauth::qr_poll(&http, &mock.base, "uni")
            .await
            .expect("status 3"),
        QrPoll::Confirmed {
            token: "mock-token-3".to_string()
        }
    );

    // 未知 loginStatus → Unavailable（不静默）
    mock.push_poll(json!({"code": 0, "data": {"loginStatus": 9}}));
    let err = oauth::qr_poll(&http, &mock.base, "uni")
        .await
        .expect_err("unknown status");
    assert!(matches!(err, StorageError::Unavailable(_)), "{err:?}");

    // 确认态无 token → 协议异常上抛（不编造）
    mock.push_poll(json!({"code": 200, "data": {}}));
    let err = oauth::qr_poll(&http, &mock.base, "uni")
        .await
        .expect_err("token missing");
    assert!(matches!(err, StorageError::Unavailable(_)), "{err:?}");

    // 轮询键：uniID 恒在查询串上
    let recorded = mock.recorded();
    assert!(
        recorded
            .iter()
            .filter(|r| r.path == "result")
            .all(|r| r.query == "uniID=uni"),
        "every poll carries the uniID query key"
    );
}

#[tokio::test]
async fn wx_code_empty_when_unscanned_and_present_when_scanned() {
    let mock = MockLogin::start().await;
    let http = MockLogin::http();

    // 无人扫码：空串 → None（真机实证形态）
    mock.set_wx_code("");
    assert_eq!(
        oauth::qr_wx_code(&http, &mock.base, "uni")
            .await
            .expect("empty wxCode"),
        None
    );
    // 有码：Some
    mock.set_wx_code("mock-wx-code");
    assert_eq!(
        oauth::qr_wx_code(&http, &mock.base, "uni")
            .await
            .expect("wxCode"),
        Some("mock-wx-code".to_string())
    );

    // 载荷恰一个 uniID 键（123panNextGen `_qr_headers` 形态）
    let recorded = mock.recorded();
    for row in recorded.iter().filter(|r| r.path == "wx_code") {
        assert!(
            row.body.contains("\"uniID\"") && !row.body.contains("passport"),
            "the wx_code payload carries exactly the uniID key: {}",
            row.body
        );
    }
}

#[tokio::test]
async fn sign_in_pins_the_wire_form_and_persists_on_first_save() {
    let mock = MockLogin::start().await;
    let http = MockLogin::http();
    let store = Arc::new(RecordingTokenStore::default());

    let token = oauth::sign_in(
        &http,
        &mock.base,
        "mock-passport",
        "mock-password-0",
        Some(store.as_ref()),
    )
    .await
    .expect("sign_in succeeds");
    assert_eq!(token, "mock-token-from-signin");

    // wire 形态钉死：type==1 + 明文双字段（spike 实证）
    let recorded = mock.recorded();
    let row = recorded
        .iter()
        .find(|r| r.path == "sign_in")
        .expect("recorded");
    let body: Value = serde_json::from_str(&row.body).expect("json body");
    assert_eq!(body.get("type").and_then(Value::as_i64), Some(1));
    assert_eq!(
        body.get("passport").and_then(Value::as_str),
        Some("mock-passport")
    );
    assert_eq!(
        body.get("password").and_then(Value::as_str),
        Some("mock-password-0")
    );

    // 首存恰一次（无 refresh——不会再有第二次保存机会）
    assert_eq!(store.calls(), vec!["mock-token-from-signin".to_string()]);

    // 错误凭证：失败码上抛。123-2 起 400 参数类 → `Invalid`（任务 G
    // errno 表——错凭证即服务端的参数类拒绝；R3「不回显请求体」由
    // map_rejection 的构造保证——错误文本只拼 code+message，钉在
    // errno_mapping 的 map_rejection_payloads_never_echo_the_data_member）。
    let err = oauth::sign_in(&http, &mock.base, "wrong", "wrong", None)
        .await
        .expect_err("bad credentials");
    assert_eq!(
        err,
        StorageError::Invalid,
        "400 parameter-class rejections are Invalid (123-2 errno table)"
    );
}

/// M2（K78）：`read_envelope` 的 2xx 成功门——HTTP 403 + `code:0` 信封
/// （网关把拒绝写成成功形态）→ `Unavailable`，不得产出会话。
#[tokio::test]
async fn generate_over_non_2xx_with_an_ok_envelope_is_unavailable() {
    let mock = MockLogin::start().await;
    mock.set_generate_raw(403, r#"{"code":0,"data":{"uniID":"x","url":"https://x/"}}"#);
    let http = MockLogin::http();

    let err = oauth::qr_generate(&http, &mock.base)
        .await
        .expect_err("the HTTP gate must reject a non-2xx success envelope");
    match err {
        StorageError::Unavailable(detail) => {
            assert!(detail.contains("403"), "carries the HTTP status: {detail}");
            assert!(detail.contains("generate"), "carries the stage: {detail}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

/// P2（K79）：QR 确认**防御臂**的 token 键双拼（对齐 `parse_token` 的
/// `token/Token` 双拼纪律——§5.10）。防御臂 = code==0 但 data 已带
/// token 也收敛 Confirmed；大写 `Token` 键此前被臂上小写单拼漏掉，
/// 误报 Waiting。
#[tokio::test]
async fn qr_confirm_defense_arm_accepts_both_token_key_spellings() {
    let mock = MockLogin::start().await;
    let http = MockLogin::http();

    // 小写键：既有行为钉（防御臂的原形态——loginStatus 未报 3 也归
    // Confirmed）。
    mock.push_poll(json!({"code": 0, "data": {"loginStatus": 0, "token": "lower"}}));
    assert_eq!(
        oauth::qr_poll(&http, &mock.base, "uni")
            .await
            .expect("lowercase key confirms"),
        QrPoll::Confirmed {
            token: "lower".to_string()
        }
    );

    // 大写键 + loginStatus:3：状态机 Some(3) 臂 + parse_token 双拼本已
    // 归 Confirmed（回归钉——K79 任务的字面形态）。
    mock.push_poll(json!({"code": 0, "data": {"loginStatus": 3, "Token": "cap-three"}}));
    assert_eq!(
        oauth::qr_poll(&http, &mock.base, "uni")
            .await
            .expect("status 3 with a capital Token"),
        QrPoll::Confirmed {
            token: "cap-three".to_string()
        }
    );

    // 大写键 + loginStatus 未报 3（防御臂的真正缺口）：与大写前的小写
    // 形态同语义——应 Confirmed 而非 Waiting。
    mock.push_poll(json!({"code": 0, "data": {"loginStatus": 0, "Token": "cap-zero"}}));
    assert_eq!(
        oauth::qr_poll(&http, &mock.base, "uni")
            .await
            .expect("capital key confirms through the defense arm"),
        QrPoll::Confirmed {
            token: "cap-zero".to_string()
        }
    );

    // 大写键、无 loginStatus（缺状态 = 等待包的形态）：token 在即确认。
    mock.push_poll(json!({"code": 0, "data": {"Token": "cap-only"}}));
    assert_eq!(
        oauth::qr_poll(&http, &mock.base, "uni")
            .await
            .expect("a capital token alone confirms"),
        QrPoll::Confirmed {
            token: "cap-only".to_string()
        }
    );
}
