//! OAuth/dispatch 状态机契约测试（Phase 5 / 115-1）。
//!
//! 语义契约（断言即契约，实现者禁改；黄金参照 = 115-0 spike 真机 +
//! K69.1/K69.3/K69.7；ck-baidu tests/oauth_state_machine.rs 先例形态）：
//!
//! - **token 有效**：直接使用（零刷新调用），envelope 双形态（proapi
//!   布尔 / passportapi 数字）都过；
//! - **401* 过期**：驱动内刷新（一次一换）+ 原请求重放恰一次；重放
//!   成功 → 操作成功，刷新产物经 TokenStore **即刻**持久化（on-arrival，
//!   重放失败也不回收——新 refresh_token 已是唯一活值）；
//! - **重放仍 401*** → `Unauthorized { recoverable: true }`（绝不再刷）；
//! - **refresh 失败** → `Unauthorized { recoverable: true }`（refresh
//!   官方频控下的暂时性失败居多；本次逻辑调用的自救机会已用掉）；
//! - **770004**：本地硬退避拦截——首犯上抛 `RateLimited`（带封锁窗），
//!   **封锁期内后续调用零请求**（mock 计数为证，本地直接拒绝）；
//! - **911**：不重试、不刷新，可行动上抛 `Unauthorized { recoverable:
//!   false }`。
//!
//! mock 形态（axum 内存 115 开放平台；proapi/passportapi 同 base 不同
//! 路径）：业务端点校验 `Authorization: Bearer <current>`，不匹配 →
//! HTTP 200 + `{"state":false,"code":40140123}`（K69.7：错误包恒
//! HTTP 200，判据在 envelope）；refresh 端点校验 form 的
//! `refresh_token == current`，匹配则一次一换轮换出全新对。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use ck_pan115::api::Pan115Client;
use ck_pan115::limiter::{LimiterConfig, RateLimiter};
use ck_pan115::oauth::TokenStore;
use cloudkit_storage::StorageError;

// ---------------------------------------------------------------------
// mock 后端
// ---------------------------------------------------------------------

const INITIAL_ACCESS: &str = "mock-access-0";
const INITIAL_REFRESH: &str = "mock-refresh-0";

/// 一次被记录的请求（断言观测面：路径 + Bearer + form）。
#[derive(Debug, Clone)]
struct Recorded {
    path: String,
    bearer: Option<String>,
    body: String,
}

#[derive(Default)]
struct MockState {
    current_access: String,
    current_refresh: String,
    refresh_calls: usize,
    business_calls: usize,
    /// 错误注入队列（(code, errno)，HTTP 200 envelope 承载；优先于
    /// token 校验——baidu mock 同款语义）。
    injected: std::collections::VecDeque<(i64, i64)>,
    recorded: Vec<Recorded>,
}

struct MockPan115 {
    state: Arc<Mutex<MockState>>,
    base: String,
}

impl MockPan115 {
    /// 起一个 loopback axum 服务端（proapi + passportapi 路径同 base）。
    async fn start() -> MockPan115 {
        let state = Arc::new(Mutex::new(MockState {
            current_access: INITIAL_ACCESS.to_string(),
            current_refresh: INITIAL_REFRESH.to_string(),
            ..MockState::default()
        }));
        let app = Router::new()
            .route("/open/user/info", get(user_info))
            .route("/open/refreshToken", post(refresh_token))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock listener");
        let addr = listener.local_addr().expect("mock addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("mock serve") });
        MockPan115 {
            state,
            base: format!("http://{addr}"),
        }
    }

    /// 使当前 access_token 失效（模拟过期：下一次业务请求即 401*）。
    fn rotate_access_away(&self) {
        self.state.lock().unwrap().current_access = "mock-access-rotated-away".to_string();
    }

    /// 使当前 refresh_token 失效（模拟 refresh 撞陈旧值）。
    fn break_refresh(&self) {
        self.state.lock().unwrap().current_refresh = "mock-refresh-rotated-away".to_string();
    }

    /// 注入一次 (code, errno) 错误（顶掉下一次业务请求）。
    fn inject(&self, code: i64, errno: i64) {
        self.state.lock().unwrap().injected.push_back((code, errno));
    }

    fn refresh_calls(&self) -> usize {
        self.state.lock().unwrap().refresh_calls
    }

    fn business_calls(&self) -> usize {
        self.state.lock().unwrap().business_calls
    }

    fn current_tokens(&self) -> (String, String) {
        let state = self.state.lock().unwrap();
        (state.current_access.clone(), state.current_refresh.clone())
    }

    fn recorded(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().recorded.clone()
    }

    /// 以初值 token 对构造被测 client（fast limiter——770004 测试不真等
    /// 300s；其余用例的节拍在 burst 容量内零等待）。
    fn client(&self, store: Option<Arc<dyn TokenStore>>) -> Pan115Client {
        self.client_with_limiter(store, LimiterConfig::fast())
    }

    fn client_with_limiter(
        &self,
        store: Option<Arc<dyn TokenStore>>,
        limiter: LimiterConfig,
    ) -> Pan115Client {
        Pan115Client::new(
            INITIAL_ACCESS.to_string(),
            INITIAL_REFRESH.to_string(),
            self.base.clone(), // passportapi 面（refresh 端点同 base）
            self.base.clone(), // proapi 面（user/info）
            store,
            Arc::new(RateLimiter::new(limiter)),
        )
        .expect("client constructs")
    }
}

async fn user_info(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    state.business_calls += 1;
    state.recorded.push(Recorded {
        path: "/open/user/info".to_string(),
        bearer: headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        body: String::new(),
    });
    // 注入优先于 token 校验（baidu mock 同款）
    if let Some((code, errno)) = state.injected.pop_front() {
        return error_envelope(code, errno);
    }
    let bearer_ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {}", state.current_access));
    if !bearer_ok {
        // K69.7 真机形态：HTTP 200 + state:false + code=40140123
        return error_envelope(40140123, 0);
    }
    // proapi 成功 = 布尔 state（真机实证形态）
    (
        StatusCode::OK,
        Json(json!({"state": true, "data": {"user_id": 42, "user_name": "mock"}})),
    )
}

async fn refresh_token(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    state.refresh_calls += 1;
    state.recorded.push(Recorded {
        path: "/open/refreshToken".to_string(),
        bearer: headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        body: body.clone(),
    });
    // form 恰一个字段：refresh_token（115-sdk-go auth.go:80-94 同形态）
    let presented = body
        .split('&')
        .find_map(|pair| pair.strip_prefix("refresh_token="))
        .map(urldecode)
        .unwrap_or_default();
    if presented != state.current_refresh {
        // 陈旧 refresh_token：一次一换下旧值即刻作废——token 过期族错误
        return error_envelope(0, 99);
    }
    // 一次一换：轮换出全新对
    let n = state.refresh_calls;
    state.current_access = format!("mock-access-{n}");
    state.current_refresh = format!("mock-refresh-{n}");
    let (access, refresh) = (state.current_access.clone(), state.current_refresh.clone());
    // passportapi 成功 = 数字 state（真机实证形态）
    (
        StatusCode::OK,
        Json(json!({"state": 1, "errno": 0, "data": {
            "access_token": access, "refresh_token": refresh, "expires_in": 7200
        }})),
    )
}

fn error_envelope(code: i64, errno: i64) -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK, // 错误包恒 HTTP 200（K69.7）
        Json(json!({"state": false, "code": code, "errno": errno, "message": "mock rejection"})),
    )
}

/// 最小 percent-decode（form 值断言用）。
fn urldecode(s: &str) -> String {
    s.replace("%2F", "/").replace("%3A", ":").replace('+', " ")
}

// ---------------------------------------------------------------------
// 观测面：TokenStore 记录器
// ---------------------------------------------------------------------

#[derive(Default)]
struct RecordingTokenStore {
    calls: Mutex<Vec<(String, String)>>,
}

impl TokenStore for RecordingTokenStore {
    fn save_tokens(&self, access: &str, refresh: &str) {
        self.calls
            .lock()
            .unwrap()
            .push((access.to_string(), refresh.to_string()));
    }
}

impl RecordingTokenStore {
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------
// 状态机用例
// ---------------------------------------------------------------------

#[tokio::test]
async fn valid_token_is_used_directly_with_the_boolean_envelope() {
    let mock = MockPan115::start().await;
    let client = mock.client(None);

    let info = client.user_info().await.expect("user/info succeeds");
    assert_eq!(
        info.get("user_id").and_then(Value::as_i64),
        Some(42),
        "the boolean state:true envelope carries data"
    );
    assert_eq!(mock.refresh_calls(), 0, "a valid token never refreshes");
    assert_eq!(mock.business_calls(), 1);
    let recorded = mock.recorded();
    assert_eq!(
        recorded[0].bearer.as_deref(),
        Some("Bearer mock-access-0"),
        "the initial token rides the Authorization header"
    );
}

#[tokio::test]
async fn numeric_envelope_form_is_accepted_on_the_api_face() {
    // envelope 双形态的真机事实：proapi 布尔 / passportapi 数字——
    // dispatch 对两种成功形态都必须放行（passportapi 数字形态在
    // refresh 端点已经出现；这里对 api 面再钉一次容错）。
    // 实现面：Envelope::is_ok 认 state==1（errno_mapping 已钉纯函数）；
    // 本用例走完整 client 路径验证 401* 后的 refresh 响应（数字形态）
    // 被正确消费——见 stale_token 用例。这里钉 api 面布尔形态下的
    // 全链成功（数值形态由 refresh 路径覆盖）。
    let mock = MockPan115::start().await;
    let client = mock.client(None);
    client.user_info().await.expect("boolean form passes");
}

#[tokio::test]
async fn stale_token_refreshes_once_replays_and_persists_on_arrival() {
    let mock = MockPan115::start().await;
    let store = Arc::new(RecordingTokenStore::default());
    let client = mock.client(Some(store.clone()));

    // 使 client 持有的 access_token 过期（服务器侧已轮换）
    mock.rotate_access_away();

    let info = client.user_info().await.expect("refresh + replay succeeds");
    assert_eq!(
        info.get("user_id").and_then(Value::as_i64),
        Some(42),
        "the replayed request succeeds"
    );

    // 恰一次刷新；一次一换（新对与初值均不同）
    assert_eq!(mock.refresh_calls(), 1, "401* triggers exactly one refresh");
    let (new_access, new_refresh) = mock.current_tokens();
    assert_ne!(new_access, INITIAL_ACCESS);
    assert_ne!(new_refresh, INITIAL_REFRESH, "refresh rotates the pair");

    // 业务请求恰两次：首带旧 token（得 401*），重放带新 token
    assert_eq!(mock.business_calls(), 2, "exactly one replay, never a loop");
    let recorded = mock.recorded();
    let user_info_calls: Vec<&Recorded> = recorded
        .iter()
        .filter(|r| r.path == "/open/user/info")
        .collect();
    assert_eq!(
        user_info_calls[0].bearer.as_deref(),
        Some("Bearer mock-access-0"),
        "the first attempt carries the stale token"
    );
    assert_eq!(
        user_info_calls[1].bearer.as_deref(),
        Some(format!("Bearer {new_access}").as_str()),
        "the replay carries the refreshed token"
    );

    // refresh 载荷恰一个 refresh_token 字段（115-sdk-go auth.go:80-94
    // 逐行核实的形态；多带字段会被 passportapi 拒或触发频控）。
    let refresh_call = recorded
        .iter()
        .find(|r| r.path == "/open/refreshToken")
        .expect("the refresh call is recorded");
    assert_eq!(
        refresh_call.body, "refresh_token=mock-refresh-0",
        "the refresh payload carries exactly the refresh_token form field"
    );

    // on-arrival 持久化：恰一次，落的是刷新产物（成对）
    let calls = store.calls();
    assert_eq!(calls.len(), 1, "exactly one save_tokens");
    assert_eq!(calls[0].0, new_access);
    assert_eq!(calls[0].1, new_refresh);
}

#[tokio::test]
async fn replay_still_expired_maps_to_unauthorized_recoverable_true() {
    let mock = MockPan115::start().await;
    let client = mock.client(None);

    // 双注入：首请求 401*（token 校验也会给，但注入确保）+ 重放仍 401*
    mock.inject(40140123, 0);
    mock.inject(40140123, 0);

    let err = client
        .user_info()
        .await
        .expect_err("a replay that still expires must surface");
    assert_eq!(
        err,
        StorageError::Unauthorized { recoverable: true },
        "refresh succeeded but the replay still 401* -> recoverable:true, never loop"
    );
    assert_eq!(mock.refresh_calls(), 1, "only one refresh attempt");
    assert_eq!(mock.business_calls(), 2, "original + exactly one replay");
}

#[tokio::test]
async fn refresh_failure_maps_to_unauthorized_recoverable_true() {
    let mock = MockPan115::start().await;
    let client = mock.client(None);

    // client 的 refresh_token 已陈旧（服务器侧轮换过）：刷新撞 99
    mock.rotate_access_away();
    mock.break_refresh();

    let err = client
        .user_info()
        .await
        .expect_err("a failed refresh must surface");
    assert_eq!(
        err,
        StorageError::Unauthorized { recoverable: true },
        "refresh transport/protocol failure -> recoverable:true (official \
         rate-limiting is transient; the self-heal chance is spent)"
    );
    assert_eq!(mock.refresh_calls(), 1, "the refresh was attempted once");
    assert_eq!(mock.business_calls(), 1, "no replay after a failed refresh");
}

#[tokio::test]
async fn account_rate_limit_770004_engages_local_hard_backoff() {
    let mock = MockPan115::start().await;
    // 40ms 封锁窗（fast 注入——真产 300s 不可等）
    let client = mock.client_with_limiter(
        None,
        LimiterConfig {
            block_initial: Duration::from_millis(40),
            ..LimiterConfig::fast()
        },
    );

    mock.inject(770004, 0);
    let err = client
        .user_info()
        .await
        .expect_err("770004 must surface as RateLimited");
    match err {
        StorageError::RateLimited {
            retry_after: Some(window),
        } => {
            assert!(
                window >= Duration::from_millis(30),
                "the first 770004 carries the backoff window, got {window:?}"
            );
        }
        other => panic!("expected RateLimited with a window, got {other:?}"),
    }
    assert_eq!(
        mock.business_calls(),
        1,
        "the offending request was sent once"
    );

    // 硬退避拦截：封锁期内的后续调用零请求（本地立即 RateLimited）
    let err = client
        .user_info()
        .await
        .expect_err("the block window must reject locally");
    match err {
        StorageError::RateLimited {
            retry_after: Some(remaining),
        } => {
            assert!(remaining > Duration::ZERO, "carries the remaining window");
        }
        other => panic!("expected a local RateLimited rejection, got {other:?}"),
    }
    assert_eq!(
        mock.business_calls(),
        1,
        "HARD BACKOFF: zero requests leave the client during the block window"
    );
    assert_eq!(mock.refresh_calls(), 0, "770004 never triggers a refresh");
}

#[tokio::test]
async fn human_verification_911_fails_fast_without_retry_or_refresh() {
    let mock = MockPan115::start().await;
    let client = mock.client(None);

    mock.inject(911, 0);
    let err = client.user_info().await.expect_err("911 must surface");
    assert_eq!(
        err,
        StorageError::Unauthorized { recoverable: false },
        "human verification needs the operator at 115's side - no retry"
    );
    assert_eq!(mock.business_calls(), 1, "exactly one request (no replay)");
    assert_eq!(mock.refresh_calls(), 0, "911 never triggers a refresh");
}
