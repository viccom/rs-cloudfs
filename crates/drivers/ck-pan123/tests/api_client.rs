//! 域名管理与 api client 契约测试（Phase 6 / 123-1）。
//!
//! 语义契约（断言即契约，实现者禁改；黄金参照 = 123-0 真机 + 计划
//! §5.2/§5.17）：
//!
//! - **dydomain 动态发现**：首请求前 `GET {primary}/api/dydomain`
//!   一次性解析，`data.domains[0]` 取代主域（真机回
//!   `["www.123pan.cn"]`）；**任何失败（404/非 JSON/code!=0/空表/
//!   传输）回退缺省主域**——构造不连网、发现不死锁；
//! - **粘性 fallback（§5.17）**：主域**连接错误**后会话级切备域
//!   `api.123278.com` 且**不回切**——切备域后主域复活也不再回流
//!   （防两域振荡）；
//! - 一次 dispatch 在 failover 后**恰重试一次**（备域上重放同一请求）。

use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use ck_pan123::api::{Pan123Client, UA};
use cloudkit_storage::StorageError;

const TOKEN: &str = "mock-token-0123456789abcdef";

#[derive(Default)]
struct MockState {
    hits: Vec<String>,
    /// `/api/dydomain` 是否挂路由（false = 404 → 发现失败腿）。
    serve_dydomain: bool,
    /// dydomain 应答的 domains 列表。
    dydomains: Vec<String>,
}

struct Mock {
    state: Arc<Mutex<MockState>>,
    base: String,
}

impl Mock {
    async fn start() -> Mock {
        Self::start_with(MockState::default()).await
    }

    async fn start_with(state0: MockState) -> Mock {
        let state = Arc::new(Mutex::new(state0));
        let app = Router::new()
            .route("/api/dydomain", get(dydomain))
            .route("/b/api/user/info", get(user_info))
            .fallback(fallback_404)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock listener");
        let addr = listener.local_addr().expect("mock addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("mock serve") });
        Mock {
            state,
            base: format!("http://{addr}"),
        }
    }

    fn hits(&self) -> Vec<String> {
        self.state.lock().unwrap().hits.clone()
    }
}

async fn fallback_404() -> (axum::http::StatusCode, &'static str) {
    (axum::http::StatusCode::NOT_FOUND, "not found")
}

async fn dydomain(
    State(state): State<Arc<Mutex<MockState>>>,
) -> (axum::http::StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    state.hits.push("/api/dydomain".to_string());
    if !state.serve_dydomain || state.dydomains.is_empty() {
        // code!=0 / 空列表形态：发现失败
        return (
            axum::http::StatusCode::OK,
            Json(json!({"code": 0, "data": {"domains": []}})),
        );
    }
    let domains = state.dydomains.clone();
    (
        axum::http::StatusCode::OK,
        Json(json!({"code": 0, "data": {"domains": domains, "ucenterDomain": "user.123pan.cn"}})),
    )
}

async fn user_info(
    State(state): State<Arc<Mutex<MockState>>>,
    Query(_q): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> (axum::http::StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    state.hits.push("/b/api/user/info".to_string());
    let _ = headers;
    (
        axum::http::StatusCode::OK,
        Json(json!({"code": 0, "data": {"UID": 7}})),
    )
}

/// dydomain 解析产物指向另一个 mock：域名切换可端到端观测。
#[tokio::test]
async fn dydomain_resolution_replaces_the_primary() {
    let target = Mock::start().await; // B：解析结果指向的域
    let state = MockState {
        serve_dydomain: true,
        dydomains: vec![target.base.clone()], // 带 scheme 的 loopback 形态
        ..MockState::default()
    };
    let probe = Mock::start_with(state).await; // A：构造主域

    let client = Pan123Client::new(
        TOKEN.to_string(),
        probe.base.clone(),
        probe.base.clone(),
        None,
    )
    .expect("client");

    // dydomain 在 A 上解析 → 后续请求全部落在 B 上
    let info = client
        .user_info()
        .await
        .expect("user/info via the resolved domain");
    assert_eq!(info.uid, 7);
    let probe_hits = probe.hits();
    assert_eq!(
        probe_hits,
        vec!["/api/dydomain".to_string()],
        "only the bootstrap probe touches the construction primary"
    );
    let target_hits = target.hits();
    assert_eq!(
        target_hits,
        vec!["/b/api/user/info".to_string()],
        "the resolved domain serves the business call"
    );

    // 一次性：第二个请求不再探测 dydomain
    client.user_info().await.expect("second call");
    assert_eq!(probe.hits().len(), 1, "dydomain resolves exactly once");
    assert_eq!(target.hits().len(), 2);
}

/// dydomain 失败（404/空列表）→ 维持缺省主域继续服务。
#[tokio::test]
async fn dydomain_failure_falls_back_to_the_default_primary() {
    let mock = Mock::start().await; // 无 dydomain 数据 → 空列表形态
    let client = Pan123Client::new(
        TOKEN.to_string(),
        mock.base.clone(),
        mock.base.clone(),
        None,
    )
    .expect("client");

    let info = client
        .user_info()
        .await
        .expect("default primary keeps serving");
    assert_eq!(info.uid, 7);
    let hits = mock.hits();
    assert!(hits.contains(&"/api/dydomain".to_string()), "{hits:?}");
    assert!(hits.contains(&"/b/api/user/info".to_string()), "{hits:?}");
}

/// 主域连接错误 → 粘性切备域重试一次；主域复活**不回切**。
#[tokio::test]
async fn connection_error_fails_over_stickily_and_never_returns() {
    let fallback = Mock::start().await; // B：备域（恒活）
                                        // A：主域 mock——serve 任务可中止以制造「连接错误」
    let state = Arc::new(Mutex::new(MockState {
        serve_dydomain: false,
        ..MockState::default()
    }));
    let app = Router::new()
        .route("/api/dydomain", get(dydomain))
        .route("/b/api/user/info", get(user_info))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind primary");
    let primary_addr = listener.local_addr().expect("primary addr");
    let serve = tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    let client = Pan123Client::new(
        TOKEN.to_string(),
        format!("http://{primary_addr}"),
        fallback.base.clone(),
        None,
    )
    .expect("client");

    // 阶段 1：主域活着——bootstrap dydomain（空列表→维持主域）+ 业务
    // 请求都落在主域。
    let info = client
        .user_info()
        .await
        .expect("primary serves while alive");
    assert_eq!(info.uid, 7);
    assert!(
        fallback.hits().is_empty(),
        "fallback untouched while primary lives"
    );

    // 阶段 2：杀主域（中止 serve → 端口关闭 → 连接拒绝）
    serve.abort();
    serve.await.expect_err("aborted");

    // 阶段 3：连接错误 → 粘性切备域，同一请求在备域重放成功。
    let info = client
        .user_info()
        .await
        .expect("the in-flight call retries once on the fallback domain");
    assert_eq!(info.uid, 7);
    let fb = fallback.hits();
    assert_eq!(
        fb,
        vec!["/b/api/user/info".to_string()],
        "exactly one replay"
    );

    // 阶段 4：粘性——后续请求恒备域（即便主域可能复活）。
    let info = client
        .user_info()
        .await
        .expect("sticky fallback keeps serving");
    assert_eq!(info.uid, 7);
    assert_eq!(
        fallback.hits().len(),
        2,
        "still on the fallback, never back"
    );
}

/// 备域也连不通（两域全灭）→ `Unavailable` 终态（可诊断，不 panic）。
#[tokio::test]
async fn both_domains_dead_surfaces_unavailable() {
    // 绑后即弃的端口：连接拒绝
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let dead_addr = probe.local_addr().expect("dead addr");
    drop(probe);

    let client = Pan123Client::with_tuning(
        TOKEN.to_string(),
        format!("http://{dead_addr}"),
        format!("http://{dead_addr}"),
        None,
        ck_pan123::limiter::LimiterConfig::fast(),
        // 重试关断：本用例只钉「终态 Unavailable」，避免 Windows 回环
        // 拒连延迟 × 重试次数的套件拖慢（重试行为由 api_retry.rs 钉）。
        {
            let mut cfg = ck_pan123::api::RetryConfig::fast();
            cfg.ordinary_max = 0;
            cfg
        },
    )
    .expect("client");
    let err = client.user_info().await.expect_err("both domains dead");
    assert!(
        matches!(err, StorageError::Unavailable(ref d) if d.contains("user/info")),
        "transport failure surfaces as Unavailable with the stage: {err:?}"
    );
}

/// web 身份头集合（D5）：客户端默认头带浏览器 UA + platform:web +
/// app-version:3 + loginuuid；**无安卓头**。
#[tokio::test]
async fn web_identity_headers_ride_every_request() {
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = Arc::clone(&seen);
    let app = Router::new().route(
        "/b/api/user/info",
        get(move |headers: HeaderMap| {
            let seen = Arc::clone(&seen2);
            async move {
                let mut row = Vec::new();
                for name in [
                    "user-agent",
                    "platform",
                    "app-version",
                    "loginuuid",
                    "origin",
                ] {
                    if let Some(v) = headers.get(name) {
                        row.push((name.to_string(), v.to_str().unwrap_or("").to_string()));
                    }
                }
                seen.lock().unwrap().push((
                    "hit".to_string(),
                    row.iter()
                        .map(|(k, v)| format!("{k}|{v}"))
                        .collect::<Vec<_>>()
                        .join(" / "),
                ));
                (
                    axum::http::StatusCode::OK,
                    axum::Json(json!({"code":0,"data":{"UID":1}})),
                )
            }
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
        Some("deadbeef".to_string()),
    )
    .expect("client");
    client.user_info().await.expect("info");

    let snapshot = seen.lock().unwrap().clone();
    let row = snapshot.first().expect("a recorded hit").1.clone();
    assert!(row.contains(&format!("user-agent|{UA}")), "{row}");
    assert!(row.contains("platform|web"), "{row}");
    assert!(row.contains("app-version|3"), "{row}");
    assert!(row.contains("loginuuid|deadbeef"), "{row}");
    assert!(row.contains("origin|https://yun.123pan.cn"), "{row}");
    assert!(
        !row.to_lowercase().contains("android"),
        "D5: no android identity"
    );
}
