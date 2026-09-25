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
// Driver-gated enum pins (FT2 / FT3 / SF3): the `Baidu` / `Local` /
// `Sftp` arm type-assertion tests are the only users of the enum name.
#[cfg(any(feature = "baidu", feature = "local", feature = "sftp"))]
use cloudkit_cli::BackendTransport;
// Baidu-gated surface (FT2): the mock backend, the injected dispatch
// seam and the driver trait only exist with the `baidu` feature.
// Router is shared by the pan115/pan123 mocks below (123-5：pan123-only
// 组合的 --all-targets clippy 面揭出——原 baidu-only 门在无 baidu 组合
// 下漏导；WD4：pan115 门同理漏导——K74 的 pan115 mock 用 Router，组
// 合 local,webdav,pan115 的 --all-targets clippy 揭出，门扩三驱动)。
#[cfg(any(feature = "baidu", feature = "pan115"))]
use axum::routing::get;
#[cfg(any(feature = "baidu", feature = "pan115", feature = "pan123"))]
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

/// A validate-clean sftp config (loopback host; the dispatch's factory
/// only constructs — D3 lazy connect — so no server is needed here).
fn sftp_config() -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Sftp,
        sftp_host: Some("127.0.0.1".to_string()),
        sftp_port: Some(2222),
        sftp_username: Some("tester".to_string()),
        sftp_password: Some("stub-only-password".to_string()),
        sftp_host_fingerprint: Some(
            "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
        ),
        sftp_root: Some("/srv/cloudfs".to_string()),
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

/// Off-feature pin (Phase 4 / SF3, K31 shape): in a binary built
/// without the sftp driver, the dispatch's sftp arm carries the
/// actionable rebuild message — not an assembly attempt (there is no
/// driver to construct).
#[cfg(not(feature = "sftp"))]
#[tokio::test]
async fn missing_sftp_driver_refuses_with_the_rebuild_message() {
    let err = match build_backend_transport(&sftp_config()).await {
        Ok(_) => panic!("a driver-less binary cannot assemble the sftp arm"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains(cloudkit_cli::SFTP_DRIVER_REQUIRED),
        "the off-feature refusal is the K31 rebuild message: {message}"
    );
}

/// With the driver: `backend = "sftp"` now runs the **connect gate** at
/// assembly (2026-09-25 owner ruling after a real-machine outage: a volume
/// root that does not exist must fail the boot with an actionable message —
/// never mount and then fail every upload; the offline-assembly D3 pin this
/// test used to carry is superseded, and the capability bits stay pinned in
/// ck-sftp's conformance table). No usable server on the loopback port →
/// the gate refuses and the message names the volume.
#[cfg(feature = "sftp")]
#[tokio::test]
async fn sftp_key_assembly_runs_the_connect_gate() {
    let cfg = sftp_config();
    cfg.validate().expect("the test config validates");
    let err = build_backend_transport(&cfg)
        .await
        .err()
        .expect("the sftp arm refuses when the volume root is unreachable");
    let message = err.to_string();
    assert!(
        message.contains("the sftp volume is not usable"),
        "the connect gate names the failure: {message}"
    );
    // The K12 sync ruling: sftp never runs the sync task (the remote
    // filesystem is the source of truth — same as local).
    assert!(
        !cloudkit_core::sync::is_sync_supported(&Backend::Sftp),
        "sftp is not a sync backend"
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
    let dir = tempfile::tempdir().expect("temp dir");
    let probe = cloudkit_cli::baidu_backend_probe_with(
        &baidu_config(),
        &mock_endpoints(addr),
        Arc::new(cloudkit_cli::ConfigTokenStore::new(
            dir.path().join("config.toml"),
        )),
    )
    .await;
    assert!(
        matches!(probe, BackendProbe::Alive),
        "the healthy mock answers Alive, got: {probe:?}"
    );
}

/// Review M1（2026-09-25，baidu 深度审查）：探针期的 110 轮换必须落盘。
/// 修复前 `baidu_backend_probe_with` 传 `token_store = None`——刷新成功、
/// 探针报 Alive，但一次一换的旧 refresh_token 已作废而新值未持久化，
/// config.toml 里留着死 token（doctor 恰在 token 疑似过期时被运行）。
/// 红（本测试以三参签名编写，对修复前的两参签名编译失败）→ 绿：轮换
/// 对经注入的 ConfigTokenStore 写进临时 config.toml。
#[cfg(feature = "baidu")]
#[tokio::test]
async fn baidu_backend_probe_persists_the_rotated_token_pair() {
    let (addr, _calls) = spawn_mock_baidu(true).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    baidu_config().save_toml(&path).expect("seed config.toml");
    let store = cloudkit_cli::ConfigTokenStore::new(path.clone());

    let probe = cloudkit_cli::baidu_backend_probe_with(
        &baidu_config(),
        &mock_endpoints(addr),
        Arc::new(store),
    )
    .await;
    assert!(
        matches!(probe, BackendProbe::Alive),
        "the rotated replay connects, got: {probe:?}"
    );

    let reloaded = CyDriveConfig::load_toml(&path).expect("reload config");
    assert_eq!(
        reloaded.baidu_access_token.as_deref(),
        Some("rotated-access"),
        "the rotated access_token reached the config file"
    );
    assert_eq!(
        reloaded.baidu_refresh_token.as_deref(),
        Some("rotated-refresh"),
        "the rotated refresh_token reached the config file (the only live value now)"
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

// ------------------------------------------------------------ pan115 -------

/// A validate-clean pan115 config (placeholder token pair; the dispatch
/// connects — tests point the endpoints at a loopback mock).
fn pan115_config() -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Pan115,
        pan115_access_token: Some("stub-access".to_string()),
        pan115_refresh_token: Some("stub-refresh".to_string()),
        pan115_client_id: None,
        pan115_root: Some("0".to_string()),
        ..CyDriveConfig::default()
    }
}

/// Loopback mock answering only `GET /open/user/info` (the connect leg);
/// returns the `state:true` boolean envelope with a uid and space block.
#[cfg(feature = "pan115")]
async fn pan115_user_info_mock() -> String {
    let app = Router::new().route(
        "/open/user/info",
        get(|| async {
            axum::Json(serde_json::json!({
                "state": true,
                "errno": 0,
                "data": {
                    "user_id": 7742,
                    "user_name": "mock",
                    "rt_space_info": {
                        "all_total": {"size": 1000},
                        "all_use": {"size": 100},
                        "all_remain": {"size": 900}
                    }
                }
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    format!("http://{addr}")
}

/// Without the driver: the K31 rebuild message — not an assembly attempt.
#[cfg(not(feature = "pan115"))]
#[tokio::test]
async fn missing_pan115_driver_refuses_with_the_rebuild_message() {
    let err = match build_backend_transport(&pan115_config()).await {
        Ok(_) => panic!("a driver-less binary cannot assemble the pan115 arm"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains(cloudkit_cli::PAN115_DRIVER_REQUIRED),
        "the off-feature refusal is the K31 rebuild message: {message}"
    );
}

/// With the driver: the dispatch connects (uid → `pan115:<uid>` — the
/// real identity, never the `pan115:pending` placeholder), and the
/// capability face carries the driver bits plus `remote_delete`.
#[cfg(feature = "pan115")]
#[tokio::test]
async fn pan115_key_builds_pan115_transport_with_the_account_identity() {
    let cfg = pan115_config();
    cfg.validate().expect("the test config validates");
    let base = pan115_user_info_mock().await;
    let dispatched =
        cloudkit_cli::build_pan115_transport_with_endpoints(&cfg, &base, &base, None, None)
            .await
            .expect("the pan115 arm assembles against the mock");
    assert!(matches!(dispatched, BackendTransport::Pan115(_)));
    assert_eq!(
        dispatched.volume(),
        "pan115:7742",
        "the volume identity is pan115:<uid> from user/info"
    );
    let caps = dispatched.caps();
    assert!(caps.range_read && caps.multipart && caps.server_side_move);
    assert!(caps.rapid_upload && caps.authoritative_index && caps.resume);
    assert!(
        caps.remote_delete,
        "the transport face declares remote_delete"
    );
    // The sync namespace follows the baidu shape: the raw volume id.
    assert_eq!(dispatched.sync_namespace_key(), "pan115:7742");
}

/// A dead token pair on the assembly leg surfaces as a classified error
/// (401* envelope → Unauthorized), not a panic — the doctor leg renders
/// it into the re-scan guidance.
#[cfg(feature = "pan115")]
#[tokio::test]
async fn pan115_assembly_classifies_a_dead_token_pair() {
    // The 401* legs: user/info rejects, and the refresh it triggers also
    // rejects (a dead refresh_token — the NeedsReauth shape). Without the
    // refresh route the driver's attempt 404s and the classification
    // would mask as Unreachable.
    let app = Router::new()
        .route(
            "/open/user/info",
            get(|| async {
                (
                    axum::http::StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "state": false,
                        "code": 40140123,
                        "errno": 0,
                        "message": "access_token 格式错误"
                    })),
                )
            }),
        )
        .route(
            "/open/refreshToken",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "state": 0,
                    "code": 99,
                    "errno": 99,
                    "message": "refresh token invalid"
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    let base = format!("http://{addr}");

    let cfg = pan115_config();
    let probe = cloudkit_cli::pan115_backend_probe_with_endpoints(&cfg, &base, &base).await;
    assert!(
        matches!(probe, ck_pan115::Pan115Probe::NeedsReauth),
        "a dead token pair classifies as NeedsReauth, got {probe:?}"
    );
}

// ------------------------------------------------------------ pan123 -------

/// A validate-clean pan123 config (placeholder token; the dispatch
/// connects — tests point the bases at a loopback mock).
fn pan123_config() -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Pan123,
        pan123_token: Some("stub-token-90d".to_string()),
        pan123_root: Some("0".to_string()),
        ..CyDriveConfig::default()
    }
}

/// Loopback mock answering only `GET /b/api/user/info` (the connect leg);
/// the pan123 envelope (`code==0` + PascalCase data fields).
#[cfg(feature = "pan123")]
async fn pan123_user_info_mock(dead: bool) -> String {
    use axum::routing::get;
    let app = if dead {
        Router::new().route(
            "/b/api/user/info",
            get(|| async {
                axum::Json(serde_json::json!({
                    "code": 401, "message": "cookie token is empty", "data": {}
                }))
            }),
        )
    } else {
        Router::new().route(
            "/b/api/user/info",
            get(|| async {
                axum::Json(serde_json::json!({
                    "code": 0, "message": "ok",
                    "data": {
                        "UID": 4006416717i64,
                        "SpacePermanent": 2199023255552i64,
                        "SpaceUsed": 1073741824i64,
                        "Vip": false
                    }
                }))
            }),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    format!("http://{addr}")
}

/// Without the driver: the K31 rebuild message — not an assembly attempt.
#[cfg(not(feature = "pan123"))]
#[tokio::test]
async fn missing_pan123_driver_refuses_with_the_rebuild_message() {
    let err = match build_backend_transport(&pan123_config()).await {
        Ok(_) => panic!("a driver-less binary cannot assemble the pan123 arm"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains(cloudkit_cli::PAN123_DRIVER_REQUIRED),
        "the off-feature refusal is the K31 rebuild message: {message}"
    );
}

/// With the driver: the dispatch connects (uid → `pan123:<uid>` — the
/// real identity, never the `pan123:pending` placeholder), the capability
/// face carries the driver bits (resume lit by the 123-4 conformance ⑦)
/// plus `remote_delete`, and the sync namespace is the raw volume id.
#[cfg(feature = "pan123")]
#[tokio::test]
async fn pan123_key_builds_pan123_transport_with_the_account_identity() {
    use cloudkit_cli::BackendTransport;
    let cfg = pan123_config();
    cfg.validate().expect("the test config validates");
    let base = pan123_user_info_mock(false).await;
    let dispatched =
        cloudkit_cli::build_pan123_transport_with_endpoints(&cfg, &base, &base, &base, None)
            .await
            .expect("the pan123 arm assembles against the mock");
    assert!(matches!(dispatched, BackendTransport::Pan123(_)));
    assert_eq!(
        dispatched.volume(),
        "pan123:4006416717",
        "the volume identity is pan123:<uid> from user/info"
    );
    let caps = dispatched.caps();
    assert!(caps.range_read && caps.multipart && caps.server_side_move);
    assert!(caps.rapid_upload && caps.authoritative_index);
    assert!(
        caps.resume,
        "resume is lit (123-4 conformance ⑦ diff-set verified)"
    );
    assert!(
        caps.remote_delete,
        "the transport face declares remote_delete"
    );
    // The sync namespace follows the baidu/pan115 shape: the raw volume id.
    assert_eq!(dispatched.sync_namespace_key(), "pan123:4006416717");
}

/// A dead token on the assembly leg surfaces as a classified error
/// (401 envelope → Unauthorized{recoverable:false} — the web API has no
/// refresh, K76.4), not a panic — the doctor leg renders it into the
/// re-scan guidance.
#[cfg(feature = "pan123")]
#[tokio::test]
async fn pan123_assembly_classifies_a_dead_token() {
    let base = pan123_user_info_mock(true).await;
    let err = match cloudkit_cli::build_pan123_transport_with_endpoints(
        &pan123_config(),
        &base,
        &base,
        &base,
        None,
    )
    .await
    {
        Ok(_) => panic!("a dead token must refuse the assembly"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("pan123"),
        "the failure names the backend: {message}"
    );
    // The underlying classification is Unauthorized (probe renders the
    // same leg) — pin the routed probe verdict.
    let probe = pan123_dead_probe(&base).await;
    assert!(
        matches!(probe, ck_pan123::Pan123Probe::NeedsReauth),
        "a dead token classifies as NeedsReauth, got {probe:?}"
    );
}

#[cfg(feature = "pan123")]
async fn pan123_dead_probe(base: &str) -> ck_pan123::Pan123Probe {
    // The probe takes driver params directly (the seam shape of the
    // pan115 leg): point them at the dead mock.
    let params = ck_pan123::Pan123Params {
        token: Some("stub-token-90d".to_string()),
        root: "0".to_string(),
        api_base: base.to_string(),
        fallback_base: base.to_string(),
        login_base: base.to_string(),
        limiter: None,
        retry: None,
        sessions_dir: None,
    };
    ck_pan123::probe(&params).await
}

// ------------------------------------------------------------- webdav -------
//
// Phase 7 / WD1b: the dispatch wiring over the WD1a skeleton — the
// factory constructs OFFLINE (D6: the reqwest pool connects lazily),
// so the with-driver arm needs no server; the verb faces stay
// Unsupported placeholders until WD2/WD3 (asserted in ck-webdav's own
// tests). The volume identity mirrors the WD1a driver:
// `webdav:<user>@<normalized-base-url>`.

/// A validate-clean webdav config (anonymous would also do — the pair
/// shape exercises the identity's user segment).
fn webdav_config() -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Webdav,
        webdav_url: Some("https://nas.lan:5006/dav".to_string()),
        webdav_username: Some("spike".to_string()),
        webdav_password: Some("pw".to_string()),
        ..CyDriveConfig::default()
    }
}

/// Without the driver: the K31 rebuild message — not an assembly attempt.
#[cfg(not(feature = "webdav"))]
#[tokio::test]
async fn missing_webdav_driver_refuses_with_the_rebuild_message() {
    let err = match build_backend_transport(&webdav_config()).await {
        Ok(_) => panic!("a driver-less binary cannot assemble the webdav arm"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains(cloudkit_cli::WEBDAV_DRIVER_REQUIRED),
        "the off-feature refusal is the K31 rebuild message: {message}"
    );
}

/// With the driver: the dispatch assembles the WebdavTransport offline
/// (D6 — no server, no network), the volume identity is
/// `webdav:<user>@<base-url>` with the trailing slash the WD1a config
/// layer normalises in, the capability face mirrors the StorageDriver
/// bits (WD2b wiring — range_read/server_side_move/authoritative_index/
/// remote_delete true, resume/multipart false), and the sync
/// namespace is the raw volume id (the pan115/pan123 shape).
#[cfg(feature = "webdav")]
#[tokio::test]
async fn webdav_key_builds_webdav_transport_offline() {
    use cloudkit_cli::BackendTransport;
    let cfg = webdav_config();
    cfg.validate().expect("the test config validates");
    let dispatched = build_backend_transport(&cfg)
        .await
        .expect("the webdav arm assembles offline (D6 lazy connect)");
    assert!(matches!(dispatched, BackendTransport::Webdav(_)));
    assert_eq!(
        dispatched.volume(),
        "webdav:spike@https://nas.lan:5006/dav/",
        "the identity is <user>@<normalized-base-url>"
    );
    let caps = dispatched.caps();
    assert!(
        caps.range_read
            && caps.server_side_move
            && caps.authoritative_index
            && caps.remote_delete
            && !caps.resume
            && !caps.multipart
            && !caps.change_feed
            && !caps.inbound
            && !caps.chat
            && !caps.rapid_upload,
        "WD2b: the transport face mirrors the StorageDriver capability bits \
         (got {caps:?})"
    );
    assert_eq!(
        dispatched.sync_namespace_key(),
        "webdav:spike@https://nas.lan:5006/dav/",
        "the sync namespace is the raw volume id (the pan115/pan123 shape)"
    );
}

/// With the driver but an invalid config: the assembly surfaces the
/// driver-side parse error (the second gate — the core `validate` pass
/// runs at load time in production; here the empty password reads as
/// unset on the flatten, so the driver's map face sees a lone username
/// and names BOTH keys), never a panic.
#[cfg(feature = "webdav")]
#[tokio::test]
async fn webdav_assembly_surfaces_the_driver_parse_gate() {
    let mut cfg = webdav_config();
    cfg.webdav_password = Some(String::new()); // empty-means-unset on the flatten
    let err = match build_backend_transport(&cfg).await {
        Ok(_) => panic!("a lone username must refuse the webdav assembly"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("webdav_username") && message.contains("webdav_password"),
        "the driver gate names both keys: {message}"
    );
}

/// WD4: the doctor's five-state verdict rendering — the pure function
/// over [`ck_webdav::WebdavProbe`] (the network leg itself is the
/// driver's `probe`, stub-pinned in ck-webdav's connect_auth suite; the
/// live-matrix leg is WD5). Every state names its way out (K31 style);
/// no credential material ever appears in the text.
#[cfg(feature = "webdav")]
#[test]
fn webdav_connectivity_verdicts_cover_the_five_states() {
    use ck_webdav::WebdavProbe;
    use cloudkit_cli::doctor::{webdav_connectivity_check, CheckStatus};

    // ① reachable + authenticated: reports the DAV class / Allow summary.
    let alive = webdav_connectivity_check(&WebdavProbe::Alive {
        dav_class: Some("1, 2".to_string()),
        allow: Some("OPTIONS, GET, PUT, PROPFIND".to_string()),
    });
    assert_eq!(alive.status, CheckStatus::Ok, "{alive:?}");
    assert!(
        alive.detail.contains("1, 2") && alive.detail.contains("PROPFIND"),
        "the Alive detail carries the DAV class and Allow summary: {}",
        alive.detail
    );
    // Header-absent servers keep the Ok (RFC leaves both optional).
    let alive_minimal = webdav_connectivity_check(&WebdavProbe::Alive {
        dav_class: None,
        allow: None,
    });
    assert_eq!(alive_minimal.status, CheckStatus::Ok, "{alive_minimal:?}");

    // ② credentials rejected: Fail + the credential keys.
    let rejected = webdav_connectivity_check(&WebdavProbe::CredentialsRejected {
        detail: "webdav credentials were rejected".to_string(),
    });
    assert_eq!(rejected.status, CheckStatus::Fail, "{rejected:?}");
    assert!(
        rejected.detail.contains("webdav_username") && rejected.detail.contains("webdav_password"),
        "the rejection points at the credential keys: {}",
        rejected.detail
    );

    // ③ reachable without auth: usable, with the configure-credentials
    // suggestion (a Warn, not a Fail — anonymous servers are legal).
    let anonymous = webdav_connectivity_check(&WebdavProbe::ReachableNoAuth);
    assert_eq!(anonymous.status, CheckStatus::Warn, "{anonymous:?}");
    assert!(
        anonymous.detail.contains("webdav_username"),
        "the suggestion names the credential keys: {}",
        anonymous.detail
    );

    // ④ unreachable: Fail + the URL / network / proxy checklist.
    let unreachable = webdav_connectivity_check(&WebdavProbe::Unreachable {
        detail: "connection refused".to_string(),
    });
    assert_eq!(unreachable.status, CheckStatus::Fail, "{unreachable:?}");
    assert!(
        unreachable.detail.contains("webdav_url")
            && unreachable.detail.contains("connection refused"),
        "the unreachable detail keeps the cause and names the checklist: {}",
        unreachable.detail
    );

    // ⑤ TLS certificate problem: reachable-but-untrusted — the escape
    // hatch key plus the security note (the explicit-accept Warn, the
    // sftp HostKeyUnpinned semantic).
    let tls = webdav_connectivity_check(&WebdavProbe::TlsUntrusted {
        detail: "certificate validate failed".to_string(),
    });
    assert_eq!(tls.status, CheckStatus::Warn, "{tls:?}");
    assert!(
        tls.detail.contains("webdav_accept_invalid_certs") && tls.detail.contains("true"),
        "the TLS state names the escape-hatch key: {}",
        tls.detail
    );
    assert!(
        tls.detail.contains("not verified") || tls.detail.contains("security"),
        "the TLS state carries the risk note: {}",
        tls.detail
    );
}

/// The doctor probe over an incomplete config never dials: the flatten
/// gate turns the missing URL into an Unreachable probe value naming the
/// config (the validate pass gives the actionable verdict first, the
/// sftp probe's rule).
#[cfg(feature = "webdav")]
#[tokio::test]
async fn webdav_backend_probe_reports_an_incomplete_config_without_dialing() {
    let mut cfg = webdav_config();
    cfg.webdav_url = None;
    let probe = cloudkit_cli::webdav_backend_probe(&cfg).await;
    match probe {
        ck_webdav::WebdavProbe::Unreachable { detail } => {
            assert!(
                detail.contains("webdav_url"),
                "the config-incomplete detail names the key: {detail}"
            );
        }
        other => panic!("an incomplete config must not dial, got {other:?}"),
    }
}

/// The offline D3 leg: `webdav_accept_invalid_certs = true` earns its
/// own WARN line (the hatch never hides); the default config stays clean.
#[cfg(feature = "webdav")]
#[test]
fn webdav_accept_invalid_certs_earns_the_offline_tls_warning() {
    use cloudkit_cli::doctor::CheckStatus;
    let mut cfg = webdav_config();
    cfg.webdav_accept_invalid_certs = Some(true);
    let checks = cloudkit_cli::doctor::backend_checks(&cfg);
    assert!(
        checks.iter().any(|check| {
            check.name == "webdav_accept_invalid_certs" && check.status == CheckStatus::Warn
        }),
        "the enabled hatch must surface as its own WARN: {checks:?}"
    );
    let clean = cloudkit_cli::doctor::backend_checks(&webdav_config());
    assert!(
        clean
            .iter()
            .all(|check| check.name != "webdav_accept_invalid_certs"),
        "the default (strict TLS) config must stay clean: {clean:?}"
    );
}

/// WD4: the rebuild face's `build_driver` webdav arm assembles OFFLINE
/// (the factory constructs, D6) and the walk itself hits the network —
/// against a refused port the failure is the webdav transport error
/// after its retry budget, never a K31 refusal and never a panic. This
/// is the StorageDriver-face twin of the transport-face offline
/// assertion above (the pan123 dispatch.rs precedent's shape).
#[cfg(feature = "webdav")]
#[tokio::test]
async fn rebuild_driver_arm_assembles_webdav_offline_and_the_walk_dials() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = CyDriveConfig {
        backend: Backend::Webdav,
        webdav_url: Some("http://127.0.0.1:1/dav".to_string()),
        webdav_username: Some("spike".to_string()),
        webdav_password: Some("pw".to_string()),
        db_path: dir.path().join("meta.db").display().to_string(),
        cache_path: dir.path().join("cache").display().to_string(),
        ..CyDriveConfig::default()
    };
    let err = match cloudkit_cli::run_rebuild_command(&cfg).await {
        Ok(_) => panic!("a walk against a refused port cannot succeed"),
        Err(err) => err,
    };
    let message = format!("{err:#}");
    assert!(
        message.contains("webdav") && message.contains("127.0.0.1:1"),
        "the walk failed on the webdav transport leg: {message}"
    );
    assert!(
        !message.contains(cloudkit_cli::WEBDAV_DRIVER_REQUIRED),
        "the driver is compiled in — no K31 refusal may appear: {message}"
    );
}
