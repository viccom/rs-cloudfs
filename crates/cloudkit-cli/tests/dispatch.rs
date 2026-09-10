//! RED-phase tests for the backend-key transport dispatch (Phase 2
//! Batch B3b 段二b, plan §6 unit 5): `cloudkit_cli::build_backend_transport`
//! and the wiring tails it carries (K12 sync gate / K18 proxy
//! declaration / K13 token-rotation persistence / the doctor probe).
//!
//! Contract under test:
//!
//! - **three backend keys → three transport shapes**: `backend="baidu"`
//!   assembles a `BaiduTransport` (mock xpan backend on loopback; the
//!   volume identity is `baidu:<uk>` and the capability face carries
//!   `remote_delete`), `backend="local"` assembles a `LocalTransport`
//!   over a temp root (with the `local` feature; a binary without the
//!   driver refuses with the K31 rebuild message, pinned below), and
//!   the absent key (the default config) stays telegram — the dispatch
//!   refuses it with guidance naming the dedicated Grammers connect
//!   path (zero change for the legacy arm);
//! - **K12**: the backend's sync namespace (`baidu:<uid>` / `local:<digest>`)
//!   and the local sync-unsupported warning;
//! - **K18**: proxy_url on a baidu/local instance is declared ineffective;
//! - **K13**: a 110-triggered refresh mid-connect persists the rotated
//!   token pair through the config write-back store;
//! - **doctor**: the baidu probe verdicts over the mock backend.

use cloudkit_cli::{
    build_backend_transport, local_sync_unsupported_warning, proxy_ineffective_warning,
    BackendProbe,
};
// Driver-gated enum pins (FT2 / FT3): the `Baidu` / `Local` arm
// type-assertion tests are the only users of the enum name.
#[cfg(any(feature = "baidu", feature = "local"))]
use cloudkit_cli::BackendTransport;
// Baidu-gated surface (FT2): the mock backend, the injected dispatch
// seam and the driver trait only exist with the `baidu` feature.
#[cfg(feature = "baidu")]
use axum::routing::get;
#[cfg(feature = "baidu")]
use axum::Router;
#[cfg(feature = "baidu")]
use ck_baidu::TokenStore;
#[cfg(feature = "baidu")]
use cloudkit_cli::{build_backend_transport_with, BaiduEndpoints};
use cloudkit_core::config::{Backend, CyDriveConfig};
#[cfg(feature = "baidu")]
use std::net::SocketAddr;
#[cfg(feature = "baidu")]
use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(feature = "baidu")]
use std::sync::Arc;

// ------------------------------------------------------ mock xpan backend ---

/// A minimal loopback baidu backend: uinfo (first call errno=110 when
/// `rotate` is set, then a healthy uk), the xpan file family (quota +
/// list share one union body), and the oauth refresh endpoint. The
/// returned counter tracks uinfo calls for the rotation assertions.
#[cfg(feature = "baidu")]
async fn spawn_mock_baidu(rotate: bool) -> (SocketAddr, Arc<AtomicU32>) {
    let uinfo_calls = Arc::new(AtomicU32::new(0));
    let uinfo = Arc::clone(&uinfo_calls);
    let app = Router::new()
        .route(
            "/rest/2.0/xpan/nas",
            get(move || {
                let calls = Arc::clone(&uinfo);
                let rotate = rotate;
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    if rotate && n == 0 {
                        axum::Json(
                            serde_json::json!({"errno": 110, "errmsg": "access token expired"}),
                        )
                    } else {
                        axum::Json(serde_json::json!({"errno": 0, "uk": 424242}))
                    }
                }
            }),
        )
        .route(
            "/rest/2.0/xpan/file",
            get(|| async {
                // quota (used/total) + list (list) union body — both
                // probes are read-only and accept the same shape.
                axum::Json(serde_json::json!({
                    "errno": 0, "used": 12, "total": 1099511627776_i64, "list": []
                }))
            }),
        )
        .route(
            "/oauth/2.0/token",
            get(|| async {
                axum::Json(serde_json::json!({
                    "access_token": "rotated-access",
                    "refresh_token": "rotated-refresh",
                    "expires_in": 2592000
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock baidu");
    });
    (addr, uinfo_calls)
}

/// Endpoints pointing at the loopback mock.
#[cfg(feature = "baidu")]
fn mock_endpoints(addr: SocketAddr) -> BaiduEndpoints {
    let base = format!("http://{addr}");
    BaiduEndpoints {
        api_base: base.clone(),
        oauth_base: base,
        pcs_base: None,
    }
}

/// A validate-clean baidu config (the four K14 keys present).
fn baidu_config() -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Baidu,
        baidu_app_key: Some("test-key".to_string()),
        baidu_app_secret: Some("test-secret".to_string()),
        baidu_access_token: Some("stale-access".to_string()),
        baidu_refresh_token: Some("stale-refresh".to_string()),
        ..CyDriveConfig::default()
    }
}

/// A validate-clean local config over `root`.
fn local_config(root: &std::path::Path) -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Local,
        local_root: Some(root.to_string_lossy().into_owned()),
        ..CyDriveConfig::default()
    }
}

// ------------------------------------------------------------- dispatch ---

/// `backend = "baidu"` → a BaiduTransport: the enum arm is the concrete
/// type, the volume identity is `baidu:<uk>` (uinfo's uk), the
/// capability face is the driver's plus `remote_delete`, and the dyn
/// coercion answers the same bits.
#[cfg(feature = "baidu")]
#[tokio::test]
async fn baidu_key_builds_baidu_transport() {
    let (addr, _calls) = spawn_mock_baidu(false).await;
    let dispatched = build_backend_transport_with(
        &baidu_config(),
        &mock_endpoints(addr),
        None,
        std::path::Path::new("."),
    )
    .await
    .expect("baidu dispatch assembles");
    // matches! (not let-else): with `local` gated out the enum has a
    // single variant and the let-else pattern turns irrefutable
    // (FT3 matrix find — same fix FT2 applied to the local test).
    assert!(
        matches!(&dispatched, BackendTransport::Baidu(_)),
        "the baidu key must dispatch to the Baidu arm"
    );
    assert_eq!(
        dispatched.volume(),
        "baidu:424242",
        "the volume identity comes from uinfo's uk (K5)"
    );
    let caps = dispatched.caps();
    assert!(
        caps.remote_delete,
        "the transport face declares remote_delete (K4)"
    );
    assert!(
        caps.authoritative_index && caps.multipart && caps.resume,
        "the driver's capability bits carry over: {caps:?}"
    );
    // The dyn coercion is the same transport (the run flow consumes it).
    let dyn_transport = dispatched.clone_dyn();
    assert_eq!(
        dyn_transport.capabilities().remote_delete,
        caps.remote_delete
    );
}

/// `backend = "local"` → a LocalTransport over the configured root.
/// On-feature only (FT3): without the driver the arm carries the K31
/// rebuild message instead (pinned below).
#[cfg(feature = "local")]
#[tokio::test]
async fn local_key_builds_local_transport() {
    let dir = tempfile::tempdir().expect("temp dir");
    let dispatched = build_backend_transport(&local_config(dir.path()))
        .await
        .expect("local dispatch assembles");
    assert!(
        matches!(&dispatched, BackendTransport::Local(_)),
        "the local key must dispatch to the Local arm"
    );
    assert!(
        dispatched.volume().starts_with("local:"),
        "the local volume identity is its root (K6): {}",
        dispatched.volume()
    );
    let caps = dispatched.caps();
    assert!(
        caps.remote_delete,
        "the transport face declares remote_delete (K4)"
    );
    assert!(caps.server_side_move && caps.authoritative_index);
}

/// The absent `backend` key is telegram (byte-compat), and the dispatch
/// refuses it with guidance naming the dedicated connect path — the
/// legacy arm stays exactly where it was (main.rs's GrammersTransport
/// block), never reassembled here. On-feature only: without the driver
/// the arm carries the K31 rebuild message instead (pinned below).
#[cfg(feature = "telegram")]
#[tokio::test]
async fn default_config_stays_telegram_and_dispatch_refuses_with_guidance() {
    let default = CyDriveConfig::default();
    assert_eq!(default.backend, Backend::Telegram, "absent key = telegram");
    let err = match build_backend_transport(&default).await {
        Ok(_) => panic!("the telegram arm is not this dispatch's product"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("telegram"),
        "the refusal names the backend: {message}"
    );
    assert!(
        message.contains("run"),
        "the refusal points at the run flow's dedicated connect path: {message}"
    );
}

/// Off-feature pin (FT1 / K31): in a binary built without the telegram
/// driver, the dispatch's telegram arm carries the actionable rebuild
/// message — not the legacy run-flow guidance (the run flow cannot
/// connect it either).
#[cfg(not(feature = "telegram"))]
#[tokio::test]
async fn missing_telegram_driver_refuses_with_the_rebuild_message() {
    let default = CyDriveConfig::default();
    assert_eq!(default.backend, Backend::Telegram, "absent key = telegram");
    let err = match build_backend_transport(&default).await {
        Ok(_) => panic!("the telegram arm is not this dispatch's product"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains(cloudkit_cli::TELEGRAM_DRIVER_REQUIRED),
        "the off-feature refusal is the K31 rebuild message: {message}"
    );
}

/// Off-feature pin (FT2 / K31): in a binary built without the baidu
/// driver, the dispatch's baidu arm carries the actionable rebuild
/// message — not a connect attempt (there is no driver to connect).
#[cfg(not(feature = "baidu"))]
#[tokio::test]
async fn missing_baidu_driver_refuses_with_the_rebuild_message() {
    let err = match build_backend_transport(&baidu_config()).await {
        Ok(_) => panic!("a driver-less binary cannot assemble the baidu arm"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains(cloudkit_cli::BAIDU_DRIVER_REQUIRED),
        "the off-feature refusal is the K31 rebuild message: {message}"
    );
}

/// Off-feature pin (FT3 / K31): in a binary built without the local
/// driver, the dispatch's local arm carries the actionable rebuild
/// message — not an assembly attempt (there is no driver to
/// initialise).
#[cfg(not(feature = "local"))]
#[tokio::test]
async fn missing_local_driver_refuses_with_the_rebuild_message() {
    let err = match build_backend_transport(&local_config(std::path::Path::new("C:/ignored"))).await
    {
        Ok(_) => panic!("a driver-less binary cannot assemble the local arm"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains(cloudkit_cli::LOCAL_DRIVER_REQUIRED),
        "the off-feature refusal is the K31 rebuild message: {message}"
    );
}

// ------------------------------------------------- K12: sync namespace ---

/// The dispatched baidu transport's sync namespace IS its volume
/// identity (K12: the uid is a stable non-secret account id).
#[cfg(feature = "baidu")]
#[tokio::test]
async fn baidu_sync_namespace_is_the_volume_identity() {
    let (addr, _calls) = spawn_mock_baidu(false).await;
    let dispatched = build_backend_transport_with(
        &baidu_config(),
        &mock_endpoints(addr),
        None,
        std::path::Path::new("."),
    )
    .await
    .expect("baidu dispatch assembles");
    assert_eq!(dispatched.sync_namespace_key(), "baidu:424242");
}

/// The local namespace is `local:<16-hex digest>` (K12: the raw path
/// never ships to the server). Local-feature only (FT3): the dispatch
/// refuses without the driver (pinned above).
#[cfg(feature = "local")]
#[tokio::test]
async fn local_sync_namespace_is_a_path_digest() {
    let dir = tempfile::tempdir().expect("temp dir");
    let dispatched = build_backend_transport(&local_config(dir.path()))
        .await
        .expect("local dispatch assembles");
    let key = dispatched.sync_namespace_key();
    let digest = key.strip_prefix("local:").expect("local: prefix");
    assert_eq!(digest.len(), 16, "16 hex digits: {key}");
    assert!(
        digest.chars().all(|c| c.is_ascii_hexdigit()),
        "hex digest: {key}"
    );
}

/// K12 tail: a local instance with sync_url gets the unsupported
/// warning; every other shape stays silent.
#[test]
fn local_sync_unsupported_warning_gates_on_backend_and_sync_url() {
    let mut cfg = local_config(std::path::Path::new("C:/ignored"));
    cfg.sync_url = Some("http://sync.example.org:8290".to_string());
    let warning = local_sync_unsupported_warning(&cfg).expect("local + sync_url must warn");
    assert!(
        warning.contains("local") && warning.contains("not supported"),
        "the warning names the local backend and the unsupported sync: {warning}"
    );

    cfg.sync_url = None;
    assert!(
        local_sync_unsupported_warning(&cfg).is_none(),
        "no sync_url — nothing to warn about"
    );

    let mut baidu = baidu_config();
    baidu.sync_url = Some("http://sync.example.org:8290".to_string());
    assert!(
        local_sync_unsupported_warning(&baidu).is_none(),
        "baidu syncs — no warning"
    );
}

// --------------------------------------------------- K18: proxy notice ---

/// K18: proxy_url on baidu/local is declared ineffective (direct
/// connection enforced); on telegram it stays a live setting.
#[test]
fn proxy_ineffective_warning_gates_on_backend() {
    let mut cfg = baidu_config();
    cfg.proxy_url = Some("socks5://127.0.0.1:7890".to_string());
    let warning = proxy_ineffective_warning(&cfg).expect("baidu + proxy_url must warn");
    assert!(
        warning.contains("direct"),
        "the warning states the direct connection: {warning}"
    );

    let local = {
        let mut c = local_config(std::path::Path::new("C:/ignored"));
        c.proxy_url = Some("socks5://127.0.0.1:7890".to_string());
        c
    };
    assert!(proxy_ineffective_warning(&local).is_some());

    let telegram = CyDriveConfig {
        proxy_url: Some("socks5://127.0.0.1:7890".to_string()),
        ..CyDriveConfig::default()
    };
    assert!(
        proxy_ineffective_warning(&telegram).is_none(),
        "telegram consumes proxy_url — no warning"
    );
}

// ------------------------------------------------- K13: token rotation ---

/// A capturing TokenStore for the persistence assertions.
#[derive(Default)]
#[cfg(feature = "baidu")]
struct CapturedTokens {
    saved: std::sync::Mutex<Vec<(String, String)>>,
}

#[cfg(feature = "baidu")]
impl TokenStore for CapturedTokens {
    fn save_tokens(&self, access: &str, refresh: &str) {
        self.saved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((access.to_string(), refresh.to_string()));
    }
}

/// A 110 on the first uinfo triggers the refresh machine mid-dispatch;
/// the rotated pair reaches the TokenStore (K13: on-arrival persistence
/// — the new refresh_token is the only live value) and the replay
/// connects.
#[cfg(feature = "baidu")]
#[tokio::test]
async fn token_rotation_persists_through_the_dispatch_store() {
    let (addr, uinfo_calls) = spawn_mock_baidu(true).await;
    let store = Arc::new(CapturedTokens::default());
    let dispatched = build_backend_transport_with(
        &baidu_config(),
        &mock_endpoints(addr),
        Some(store.clone()),
        std::path::Path::new("."),
    )
    .await
    .expect("the refreshed replay connects");
    assert_eq!(dispatched.volume(), "baidu:424242");

    assert!(
        uinfo_calls.load(Ordering::SeqCst) >= 2,
        "the 110 was answered by a refresh + replay"
    );
    let saved = store
        .saved
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(
        saved,
        vec![("rotated-access".to_string(), "rotated-refresh".to_string())],
        "the rotated pair was persisted on arrival"
    );
}

/// The production write-back store: a rotation updates exactly the two
/// token keys in the target config.toml (K14 — plaintext token keys in
/// config are allowed; the sync_secret precedent).
#[cfg(feature = "baidu")]
#[tokio::test]
async fn config_token_store_updates_the_two_token_keys() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    let cfg = baidu_config();
    cfg.save_toml(&path).expect("seed config.toml");
    let store = cloudkit_cli::ConfigTokenStore::new(path.clone());

    store.save_tokens("new-access", "new-refresh");

    let reloaded = CyDriveConfig::load_toml(&path).expect("reload config");
    assert_eq!(reloaded.baidu_access_token.as_deref(), Some("new-access"));
    assert_eq!(reloaded.baidu_refresh_token.as_deref(), Some("new-refresh"));
    assert_eq!(reloaded.baidu_app_key.as_deref(), Some("test-key"));
    assert_eq!(reloaded.backend, Backend::Baidu, "the backend key survives");
}

// ------------------------------------------------------- doctor probe ---

/// The doctor probe against the healthy mock: Alive (uinfo at factory,
/// quota + a root list — all read-only).
#[cfg(feature = "baidu")]
#[tokio::test]
async fn baidu_backend_probe_alive_against_mock() {
    let (addr, _calls) = spawn_mock_baidu(false).await;
    let probe =
        cloudkit_cli::baidu_backend_probe_with(&baidu_config(), &mock_endpoints(addr)).await;
    assert!(
        matches!(probe, BackendProbe::Alive),
        "the healthy mock answers Alive, got: {probe:?}"
    );
}

/// Verdict mapping: Alive → Ok; NeedsReauth → Fail with the setup
/// guidance; Unreachable → Warn (retry / network).
#[test]
fn baidu_connectivity_verdicts() {
    let ok = cloudkit_cli::doctor::baidu_connectivity_check(&BackendProbe::Alive);
    assert_eq!(ok.status, cloudkit_cli::doctor::CheckStatus::Ok);
    assert!(
        ok.detail.contains("token"),
        "names the token liveness: {}",
        ok.detail
    );

    let reauth = cloudkit_cli::doctor::baidu_connectivity_check(&BackendProbe::NeedsReauth(
        "refresh token invalid".to_string(),
    ));
    assert_eq!(reauth.status, cloudkit_cli::doctor::CheckStatus::Fail);
    assert!(
        reauth.detail.contains("setup"),
        "the re-auth guidance names cydrive setup: {}",
        reauth.detail
    );

    let unreachable = cloudkit_cli::doctor::baidu_connectivity_check(&BackendProbe::Unreachable(
        "network unreachable".to_string(),
    ));
    assert_eq!(unreachable.status, cloudkit_cli::doctor::CheckStatus::Warn);
}
