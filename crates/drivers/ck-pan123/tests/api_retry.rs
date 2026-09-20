//! HTTP 重试/退避契约测试（Phase 6 / 123-2；§5.15 实测常量——任务 G）。
//!
//! 语义契约（断言即契约，实现者禁改）：
//!
//! - **429（限流类）**：`Retry-After` 头优先（clamp 内）→ 重试，上限
//!   6 次重试（第 7 次上抛 `RateLimited`）；
//! - **5xx（普通类）**：指数退避重试，上限 3 次重试（第 4 次上抛
//!   `Unavailable`）；
//! - **envelope 终态错误**：`Unauthorized`（20101——K76.4 契约级禁
//!   重试）与 `RateLimited`（5113/5114——D5 不绕过）**恒不重试**
//!   （恰一次请求）；
//! - **退避等待真实发生**（重试之间有时间间隔——非忙重试）；
//! - 生产缺省常量钉死（§5.15：限流 6 / 普通 3 / Retry-After clamp
//!   1–60s / 退避封顶 30s）。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use ck_pan123::api::{Pan123Client, RetryConfig};
use cloudkit_storage::StorageError;

const TOKEN: &str = "mock-token-0123456789abcdef";

/// 桩状态：前 N 次请求回指定 HTTP 状态（带可选 Retry-After），之后
/// 回正常 envelope；记录请求时间戳序列。
#[derive(Default)]
struct MockState {
    fail_first: u32,
    status: u16,
    retry_after: Option<u64>,
    requests: Vec<Instant>,
}

struct Mock {
    state: Arc<Mutex<MockState>>,
    base: String,
}

impl Mock {
    async fn start(fail_first: u32, status: u16, retry_after: Option<u64>) -> Mock {
        let state = Arc::new(Mutex::new(MockState {
            fail_first,
            status,
            retry_after,
            requests: Vec::new(),
        }));
        let app = Router::new()
            .route(
                "/api/dydomain",
                get(|| async { Json(json!({"code":0,"data":{"domains":[]}})) }),
            )
            .route("/b/api/user/info", get(user_info))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        Mock {
            state,
            base: format!("http://{addr}"),
        }
    }

    fn requests(&self) -> Vec<Instant> {
        self.state.lock().unwrap().requests.clone()
    }

    fn client(&self) -> Pan123Client {
        Pan123Client::with_tuning(
            TOKEN.to_string(),
            self.base.clone(),
            self.base.clone(),
            Some("mock-login-uuid".to_string()),
            ck_pan123::limiter::LimiterConfig::fast(),
            RetryConfig::fast(),
        )
        .expect("client")
    }
}

async fn user_info(State(state): State<Arc<Mutex<MockState>>>) -> axum::response::Response {
    let mut st = state.lock().unwrap();
    st.requests.push(Instant::now());
    if st.fail_first > 0 {
        st.fail_first -= 1;
        let mut builder =
            axum::http::Response::builder().status(StatusCode::from_u16(st.status).unwrap());
        if let Some(ra) = st.retry_after {
            builder = builder.header("retry-after", ra.to_string());
        }
        return builder.body(axum::body::Body::empty()).unwrap();
    }
    axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            r#"{"code":0,"message":"ok","data":{"UID":7}}"#,
        ))
        .unwrap()
}

/// 429 + Retry-After=0（fast clamp 内）：第 2 次尝试成功。
#[tokio::test]
async fn http_429_retries_with_retry_after_and_recovers() {
    let mock = Mock::start(1, 429, Some(0)).await;
    let client = mock.client();
    let info = client.user_info().await.expect("recovers on retry");
    assert_eq!(info.uid, 7);
    assert_eq!(mock.requests().len(), 2, "exactly one retry");
}

/// 429 持续 7 次（6 次重试用尽）→ `RateLimited` 终态。
#[tokio::test]
async fn http_429_exhausts_the_limited_budget() {
    let mock = Mock::start(7, 429, Some(0)).await;
    let client = mock.client();
    let err = client.user_info().await.expect_err("budget exhausts");
    assert!(
        matches!(err, StorageError::RateLimited { .. }),
        "429 persistence is RateLimited: {err:?}"
    );
    assert_eq!(mock.requests().len(), 7, "1 initial + 6 retries");
}

/// 5xx 持续 4 次（3 次重试用尽）→ `Unavailable` 终态。
#[tokio::test]
async fn http_5xx_exhausts_the_ordinary_budget() {
    let mock = Mock::start(4, 503, None).await;
    let client = mock.client();
    let err = client.user_info().await.expect_err("budget exhausts");
    match &err {
        StorageError::Unavailable(detail) => {
            assert!(detail.contains("503"), "{detail}");
        }
        other => panic!("5xx persistence is Unavailable, got {other:?}"),
    }
    assert_eq!(mock.requests().len(), 4, "1 initial + 3 retries");
}

/// 5xx 三次后恢复：普通预算内成功。
#[tokio::test]
async fn http_5xx_recovers_within_budget() {
    let mock = Mock::start(3, 500, None).await;
    let client = mock.client();
    let info = client.user_info().await.expect("recovers");
    assert_eq!(info.uid, 7);
    assert_eq!(mock.requests().len(), 4);
}

/// 重试之间真实退避（非忙重试）：fast 档基数 5ms——第 1/2 次重试前的
/// 间隔分别 ≥ 5ms/10ms（指数 5→10；sleep 只增不减，下界确定性成立）。
#[tokio::test]
async fn retries_back_off_before_resending() {
    let mock = Mock::start(2, 500, None).await;
    let client = mock.client();
    let _ = client.user_info().await.expect("ok");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 3);
    let gap1 = reqs[1].duration_since(reqs[0]);
    let gap2 = reqs[2].duration_since(reqs[1]);
    assert!(
        gap1 >= Duration::from_millis(5),
        "the first backoff waits at least the fast base: {gap1:?}"
    );
    assert!(
        gap2 >= Duration::from_millis(10),
        "the second backoff grows exponentially: {gap2:?}"
    );
}

/// envelope 终态错误恒不重试：20101（Unauthorized——K76.4 契约级）。
#[tokio::test]
async fn envelope_20101_never_retries() {
    let state = Arc::new(Mutex::new(0u32));
    let state2 = Arc::clone(&state);
    let app = Router::new()
        .route("/api/dydomain", get(|| async { Json(Value::Null) }))
        .route(
            "/b/api/user/info",
            get(move || {
                let state = Arc::clone(&state2);
                async move {
                    *state.lock().unwrap() += 1;
                    Json(json!({"code": 20101, "message": "未登录", "data": {}}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    let client = Pan123Client::with_tuning(
        TOKEN.to_string(),
        format!("http://{addr}"),
        format!("http://{addr}"),
        None,
        ck_pan123::limiter::LimiterConfig::fast(),
        RetryConfig::fast(),
    )
    .expect("client");
    let err = client.user_info().await.expect_err("20101");
    assert_eq!(err, StorageError::Unauthorized { recoverable: false });
    assert_eq!(
        *state.lock().unwrap(),
        1,
        "no retry for terminal envelope errors"
    );
}

/// envelope 5113（流量限额——D5 不绕过）恒不重试。
#[tokio::test]
async fn envelope_5113_never_retries() {
    let state = Arc::new(Mutex::new(0u32));
    let state2 = Arc::clone(&state);
    let app = Router::new()
        .route("/api/dydomain", get(|| async { Json(Value::Null) }))
        .route(
            "/b/api/user/info",
            get(move || {
                let state = Arc::clone(&state2);
                async move {
                    *state.lock().unwrap() += 1;
                    Json(json!({"code": 5113, "message": "流量限额", "data": {}}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    let client = Pan123Client::with_tuning(
        TOKEN.to_string(),
        format!("http://{addr}"),
        format!("http://{addr}"),
        None,
        ck_pan123::limiter::LimiterConfig::fast(),
        RetryConfig::fast(),
    )
    .expect("client");
    let err = client.user_info().await.expect_err("5113");
    assert!(matches!(err, StorageError::RateLimited { .. }), "{err:?}");
    assert_eq!(*state.lock().unwrap(), 1);
}

/// 生产缺省常量钉死（§5.15）。
#[test]
fn production_retry_constants_are_pinned() {
    let cfg = RetryConfig::default();
    assert_eq!(cfg.limited_max, 6, "rate-limit retries <= 6");
    assert_eq!(cfg.ordinary_max, 3, "ordinary retries <= 3");
    assert_eq!(cfg.backoff_cap, Duration::from_secs(30), "backoff cap 30s");
    assert_eq!(cfg.retry_after_min, Duration::from_secs(1));
    assert_eq!(cfg.retry_after_max, Duration::from_secs(60));
}
