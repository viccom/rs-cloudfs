//! WD2b 认证/连接/重试白名单行为测试（手搓桩消费——WD2a `stub/mod.rs`
//! 「消费手册」）。
//!
//! 覆盖对照（计划 §4.5 负面清单 / §4.4 映射表 / §8-D1 / 矩阵⑨）：
//!
//! | 用例 | 依据 |
//! |---|---|
//! | auto Basic 预发一发即中 | §4.5-8 / D1 |
//! | auto Digest 协商恰一次 | D1（重放恰一次） |
//! | nc 每 nonce 单调递增 | §4.5-1（OpenList nc 恒 1 的正面修法） |
//! | stale nonce 再协商恰一次 | §4.5-4（nonce 过期死亡的正法） |
//! | 协商后仍 401 → Unauthorized{false} | §4.4 |
//! | auth=basic 不协商 | §4.2 键语义 |
//! | auth=digest 不发 Basic | §4.2（避免明文泄漏） |
//! | NTLM challenge 可行动拒绝 | §4.4 / §7 |
//! | 无凭据 401 文案提键名 | §4.2 |
//! | 错凭据（Basic/Digest 双腿） | §4.4 |
//! | Digest response-uri 用 wire 形态 | §4.5-2（桩 enforce） |
//! | 5xx 传输面白名单自愈/耗尽 | §4.5-10 |
//! | 429 消费 Retry-After（clamp 1–60） | K79.3 |
//! | PUT 永不自动重试 | §4.5-10（非幂等） |
//! | connect 拒绝 → Unavailable | §4.4 传输行 |
//!
//! 断言纪律：记录器（`requests()`）给请求序/nc/nonce 结构化观测——
//! 「恰一次」类断言全部落在请求计数上（401 次数 = 到达请求数 - 成功数）。

mod stub;

use std::time::Duration;

use cloudkit_storage::transport::CloudTransport;
use cloudkit_storage::{RelPath, StorageDriver, StorageError};
use stub::{spawn_stub, AuthMode, Knobs, RecordedRequest, StubHandle, StubStyle, Vfs};

use ck_webdav::{parse_from_map, WebdavDriver, WebdavParams, WebdavTransport};

// ------------------------------------------------------------ 测试工具 ---

fn params(url: &str, pairs: &[(&str, &str)]) -> WebdavParams {
    let mut map = std::collections::HashMap::new();
    map.insert("webdav_url".to_string(), url.to_string());
    for (key, value) in pairs {
        map.insert(key.to_string(), value.to_string());
    }
    parse_from_map(&map).expect("test params parse")
}

/// 好凭据 + 缺省 auth=auto。
fn driver(handle: &StubHandle, pairs: &[(&str, &str)]) -> WebdavDriver {
    let mut all = vec![("webdav_username", "spike"), ("webdav_password", "pw")];
    all.extend_from_slice(pairs);
    WebdavDriver::new(params(&handle.url, &all)).expect("driver constructs")
}

fn digest_auth(ttl: Duration, enforce_nc: bool) -> AuthMode {
    AuthMode::Digest {
        user: "spike".to_string(),
        pass: "pw".to_string(),
        realm: "webdavtest".to_string(),
        nonce_ttl: ttl,
        enforce_nc,
        stale_after_expiry: true,
    }
}

fn wrong_digest_auth() -> AuthMode {
    AuthMode::Digest {
        user: "spike".to_string(),
        pass: "WRONG".to_string(),
        realm: "webdavtest".to_string(),
        nonce_ttl: Duration::from_secs(3600),
        enforce_nc: false,
        stale_after_expiry: true,
    }
}

fn seeded_root() -> Vfs {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    vfs.seed_file("/docs/f.txt", b"body");
    vfs
}

async fn stat_file(driver: &WebdavDriver) -> Result<cloudkit_storage::Entry, StorageError> {
    driver
        .stat(&RelPath::new("docs/f.txt").expect("rel path"))
        .await
}

fn count<'a>(
    requests: &'a [RecordedRequest],
    method: &'a str,
) -> impl Iterator<Item = &'a RecordedRequest> + 'a {
    requests
        .iter()
        .filter(move |request| request.method == method)
}

// ------------------------------------------------------------ 认证面 ---

/// §4.5-8/D1：auto + 凭据 = Basic 预发。Basic 服务器上一发即中——记录器
/// 全程恰 1 个请求（零次 401 往返）。
#[tokio::test]
async fn auto_basic_preflight_succeeds_on_the_first_shot() {
    let handle = spawn_stub(
        seeded_root(),
        AuthMode::Basic {
            user: "spike".to_string(),
            pass: "pw".to_string(),
        },
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle, &[]);
    let entry = stat_file(&driver).await.expect("Basic preflight wins");
    assert_eq!(entry.size, 4);
    let requests = handle.requests();
    assert_eq!(requests.len(), 1, "no 401 round-trip: {requests:?}");
    assert!(
        requests[0]
            .auth
            .as_deref()
            .is_some_and(|a| a.starts_with("Basic")),
        "the single request carries the preemptive Basic header: {:?}",
        requests[0].auth
    );
}

/// D1：auto 遇 Digest 服务器——Basic 预发 401 → Digest 协商 → 重发**恰
/// 一次**成功。记录器：2 个请求（1 次 Basic + 1 次签名 Digest）。
#[tokio::test]
async fn auto_negotiates_digest_and_resends_exactly_once() {
    let handle = spawn_stub(
        seeded_root(),
        digest_auth(Duration::from_secs(3600), false),
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle, &[]);
    stat_file(&driver).await.expect("negotiated digest wins");
    let requests = handle.requests();
    assert_eq!(requests.len(), 2, "exactly one resend: {requests:?}");
    assert!(
        requests[0]
            .auth
            .as_deref()
            .is_some_and(|a| a.starts_with("Basic")),
        "first shot is the Basic preflight: {:?}",
        requests[0].auth
    );
    assert_eq!(requests[1].digest_nc, Some(1), "resend signs nc=1");
}

/// §4.5-1：nc 每 nonce 单调递增——auth=digest 下三个顺序 stat，签名序列
/// [1, 2, 3]（首发匿名腿不带 nc）。桩侧 enforce_nc=true：任何倒退即 401，
/// 测试绿即证明客户端纪律。
#[tokio::test]
async fn digest_nc_increments_monotonically_per_nonce() {
    let handle = spawn_stub(
        seeded_root(),
        digest_auth(Duration::from_secs(3600), true),
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle, &[("webdav_auth", "digest")]);
    for _ in 0..3 {
        stat_file(&driver).await.expect("stat under digest");
    }
    let ncs: Vec<u64> = handle
        .requests()
        .iter()
        .filter_map(|request| request.digest_nc)
        .collect();
    assert_eq!(ncs, vec![1, 2, 3], "strictly increasing nc per nonce");
}

/// §4.5-4：nonce 过期 → 401+stale=true → 换新 nonce 重算恰一次恢复（无
/// OpenList 的 12h cron 重建形态）。记录器：4 请求（首轮 2 + 过期轮 2），
/// 末两轮 nonce 必须不同。
#[tokio::test]
async fn stale_nonce_renegotiation_recovers_exactly_once() {
    let handle = spawn_stub(
        seeded_root(),
        digest_auth(Duration::from_millis(50), false),
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle, &[("webdav_auth", "digest")]);
    stat_file(&driver).await.expect("first stat negotiates");
    tokio::time::sleep(Duration::from_millis(80)).await; // nonce 过期
    stat_file(&driver)
        .await
        .expect("stale renegotiation recovers without rebuilding the client");
    let requests = handle.requests();
    assert_eq!(requests.len(), 4, "one stale resend only: {requests:?}");
    let stale_nonce = requests[2]
        .digest_nonce
        .clone()
        .expect("signed stale nonce");
    let fresh_nonce = requests[3].digest_nonce.clone().expect("fresh nonce");
    assert_ne!(stale_nonce, fresh_nonce, "the server issued a new nonce");
    assert_eq!(
        requests[3].digest_nc,
        Some(1),
        "nc restarts at 1 on the new nonce"
    );
}

/// §4.4：Digest 协商后仍 401（错凭据）→ `Unauthorized{recoverable:false}`
/// ——恰一次重发后即终局，不再无限挑战。
#[tokio::test]
async fn wrong_digest_credentials_end_in_unauthorized() {
    let handle = spawn_stub(
        seeded_root(),
        wrong_digest_auth(),
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle, &[]);
    let error = stat_file(&driver).await.expect_err("wrong password");
    assert!(
        matches!(error, StorageError::Unauthorized { recoverable: false }),
        "{error:?}"
    );
    // 恰一次重发后终局：匿名（或 Basic）+ 一次签名 Digest。
    let requests = handle.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
}

/// §4.2：auth=basic 只发 Basic、401 即 Unauthorized（不协商）——记录器
/// 全程 1 个请求。
#[tokio::test]
async fn basic_mode_never_negotiates() {
    let handle = spawn_stub(
        seeded_root(),
        digest_auth(Duration::from_secs(3600), false),
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle, &[("webdav_auth", "basic")]);
    let error = stat_file(&driver)
        .await
        .expect_err("basic mode must not negotiate");
    assert!(
        matches!(error, StorageError::Unauthorized { recoverable: false }),
        "{error:?}"
    );
    assert_eq!(handle.requests().len(), 1, "no second request");
}

/// §4.2/D1：auth=digest 直接无认证首发吃 challenge（避免 Basic 明文泄
/// 漏）——首个请求 authorization 为空。
#[tokio::test]
async fn digest_mode_first_shot_is_anonymous() {
    let handle = spawn_stub(
        seeded_root(),
        digest_auth(Duration::from_secs(3600), false),
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle, &[("webdav_auth", "digest")]);
    stat_file(&driver)
        .await
        .expect("digest first shot negotiates");
    let requests = handle.requests();
    assert_eq!(
        requests[0].auth, None,
        "the first request carries no Basic header (no plaintext leak)"
    );
}

/// §4.4/§7：NTLM challenge → `Unauthorized{false}`（NTLM/Kerberos 明确
/// 不做——可行动文案在 warn 日志通道，R3 双通道形态）。
#[tokio::test]
async fn ntlm_challenge_is_rejected_as_unauthorized() {
    let handle = spawn_stub(
        seeded_root(),
        AuthMode::Challenge {
            scheme: "NTLM".to_string(),
        },
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle, &[]);
    let error = stat_file(&driver).await.expect_err("NTLM unsupported");
    assert!(
        matches!(error, StorageError::Unauthorized { recoverable: false }),
        "{error:?}"
    );
    assert_eq!(handle.requests().len(), 1, "no negotiation attempt");
}

/// §4.2：无凭据配置遇 401 → `Unauthorized{false}`（文案提键名的部分是
/// warn 通道纯函数，lib.rs 单测钉字面）。
#[tokio::test]
async fn missing_credentials_maps_to_unauthorized() {
    let handle = spawn_stub(
        seeded_root(),
        AuthMode::Basic {
            user: "spike".to_string(),
            pass: "pw".to_string(),
        },
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let driver = WebdavDriver::new(params(&handle.url, &[])).expect("anonymous driver");
    let error = stat_file(&driver).await.expect_err("no credentials");
    assert!(
        matches!(error, StorageError::Unauthorized { recoverable: false }),
        "{error:?}"
    );
}

/// §4.4：Basic 服务器 + 错凭据 → `Unauthorized{false}`。
#[tokio::test]
async fn wrong_basic_credentials_are_unauthorized() {
    let handle = spawn_stub(
        seeded_root(),
        AuthMode::Basic {
            user: "spike".to_string(),
            pass: "pw".to_string(),
        },
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let driver = WebdavDriver::new(params(
        &handle.url,
        &[
            ("webdav_username", "spike"),
            ("webdav_password", "not-the-password"),
        ],
    ))
    .expect("driver");
    let error = stat_file(&driver).await.expect_err("wrong basic password");
    assert!(
        matches!(error, StorageError::Unauthorized { recoverable: false }),
        "{error:?}"
    );
}

/// §4.5-2：Digest response 的 uri 必须用 **wire 形态**（百分号编码的
/// path+query，与发出去的逐字节一致）——桩的严格校验（apache 形态）
/// 在空格/非 ASCII 路径上放行即证明。
#[tokio::test]
async fn digest_signed_uri_matches_the_wire_form_on_encoded_paths() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/my file.txt", b"encoded");
    vfs.seed_file("/ü+.txt", b"unicode");
    let handle = spawn_stub(
        vfs,
        digest_auth(Duration::from_secs(3600), false),
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle, &[("webdav_auth", "digest")]);
    for path in ["my file.txt", "ü+.txt"] {
        let entry = driver
            .stat(&RelPath::new(path).expect("rel path"))
            .await
            .expect("signed wire URI must match the request byte-for-byte: {path}");
        assert_eq!(entry.size, 7, "{path}");
    }
}

// --------------------------------------------------------- 重试白名单 ---

/// §4.5-10：PROPFIND 在白名单内——503×2 后第 3 次成功（记录器 3 个请求）。
#[tokio::test]
async fn transient_5xx_self_heals_within_the_retry_budget() {
    let handle = spawn_stub(
        seeded_root(),
        AuthMode::None,
        Knobs {
            transient_5xx: 2,
            ..Knobs::default()
        },
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle, &[]);
    stat_file(&driver)
        .await
        .expect("retry whitelist heals 503x2");
    assert_eq!(count(&handle.requests(), "PROPFIND").count(), 3);
}

/// §4.5-10/§4.4：5xx 耗尽（> 3 次重试）→ `Unavailable`。
#[tokio::test]
async fn five_hxx_exhaustion_maps_to_unavailable() {
    let handle = spawn_stub(
        seeded_root(),
        AuthMode::None,
        Knobs {
            transient_5xx: 10,
            ..Knobs::default()
        },
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle, &[]);
    let error = stat_file(&driver)
        .await
        .expect_err("5xx outlasts the budget");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    // 4 次尝试 = 首发 + 3 次重试（上限纪律）。
    assert_eq!(count(&handle.requests(), "PROPFIND").count(), 4);
}

/// K79.3：429 消费 Retry-After（秒数 clamp 1–60，这里 1s）后退避重试成
/// 功——记录器 2 个请求。
#[tokio::test]
async fn rate_limited_consumes_retry_after() {
    let handle = spawn_stub(
        seeded_root(),
        AuthMode::None,
        Knobs {
            rate_limit_429: Some((1, Some(1))),
            ..Knobs::default()
        },
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle, &[]);
    stat_file(&driver)
        .await
        .expect("429 + Retry-After: 1 heals after the honored delay");
    assert_eq!(count(&handle.requests(), "PROPFIND").count(), 2);
}

/// §4.5-10：PUT **永不自动重试**（非幂等——重放可能重复写效果）。kill
/// 旋钮断掉 PUT 的响应，之后不得有第二个 PUT 到达。WD3 起经正式写面
/// 驱动（writer → close 的 PUT `.part` 腿；kill 在 writer 返回后经运行
/// 期旋钮面武装——效果未落、如实上抛、close 失败路径清理暂存件）。
#[tokio::test]
async fn put_is_never_auto_retried() {
    let handle = spawn_stub(
        Vfs::new(),
        AuthMode::None,
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle, &[]);
    let hint = cloudkit_storage::WriteHint {
        size: Some(7),
        ..Default::default()
    };
    let mut stager = driver
        .writer(&RelPath::new("f.bin").expect("rel path"), &hint)
        .await
        .expect("writer");
    // writer 的 stat 预检已完成（PROPFIND 重试链是 1+3 次尝试——启动期
    // 固定计数的 kill 会被它吃光或致死）；此刻经旋钮的**运行期可调面**
    // 武装恰一枚 kill——close 链在 PUT 前无 PROPFIND（父=卷根短路），
    // 这一枚必然落在 PUT 上。
    handle
        .knobs
        .kill_connections
        .store(1, std::sync::atomic::Ordering::SeqCst);
    stager.write(b"payload").await.expect("spool write");
    let error = stager.close().await.expect_err("killed PUT surfaces");
    assert!(
        matches!(error, StorageError::Unavailable(_) | StorageError::Io(_)),
        "{error:?}"
    );
    let requests = handle.requests();
    let puts: Vec<_> = count(&requests, "PUT").collect();
    assert_eq!(puts.len(), 1, "no auto-retry for non-idempotent verbs");
    // kill 旋钮在路由前断连（效果未落——「效果已落」是 lost_ack 旋钮的
    // 语义）：失败如实上抛、远端无残留（含 .part——close 失败路径的
    // 现场恢复清理）、无第二次尝试。
    assert!(
        !handle.exists("/f.bin"),
        "the killed PUT must leave no remote residue"
    );
    assert!(
        handle
            .snapshot()
            .keys()
            .all(|path| !path.contains(".ckwd-")),
        "no staging residue after the failed close"
    );
}

/// §4.4 传输行：connect 拒绝（错端口/无服务）→ `Unavailable`，不 panic；
/// 传输错误也在白名单内（3 次重试后才终局）。
#[tokio::test]
async fn connection_refused_maps_to_unavailable_without_panicking() {
    // 127.0.0.1:1 无监听（tcpmux 不在本机）——连接拒绝即刻发生。
    let driver = WebdavDriver::new(params("http://127.0.0.1:1/", &[])).expect("driver");
    let error = stat_file(&driver).await.expect_err("refused port");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
}

// ------------------------------------------------------ transport 面 ---

/// transport connect（OPTIONS 探活 + 认证一轮）：错凭据 → Unauthorized。
#[tokio::test]
async fn transport_connect_rejects_wrong_credentials() {
    let handle = spawn_stub(
        seeded_root(),
        AuthMode::Basic {
            user: "spike".to_string(),
            pass: "pw".to_string(),
        },
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let driver = WebdavDriver::new(params(
        &handle.url,
        &[("webdav_username", "spike"), ("webdav_password", "WRONG")],
    ))
    .expect("driver");
    let transport = WebdavTransport::new(std::sync::Arc::new(driver));
    let error = transport.connect().await.expect_err("wrong credentials");
    assert!(
        matches!(error, StorageError::Unauthorized { recoverable: false }),
        "{error:?}"
    );
}

/// transport connect：好凭据（含 Digest 协商轮）→ Ok——「真连接检查」。
#[tokio::test]
async fn transport_connect_accepts_good_credentials_through_digest() {
    let handle = spawn_stub(
        seeded_root(),
        digest_auth(Duration::from_secs(3600), false),
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let driver = driver(&handle, &[]);
    let transport = WebdavTransport::new(std::sync::Arc::new(driver));
    transport
        .connect()
        .await
        .expect("OPTIONS probe + digest negotiation connects");
    assert!(
        count(&handle.requests(), "OPTIONS").count() >= 1,
        "the probe is an OPTIONS round"
    );
}

// ------------------------------------------------------- doctor 探活 ---

/// WD4：[`ck_webdav::probe`] 的行为面（doctor 五态的网络腿——判定文案
/// 的纯函数面在 cloudkit-cli doctor 渲染器，真机腿归 WD5）。
///
/// 五态在桩上的可达性：
/// - `Alive` / `ReachableNoAuth` / `CredentialsRejected` / `Unreachable`
///   四态由桩直接钉死；
/// - `TlsUntrusted` 需要自签 https 服务器（桩是纯 http），离线不可达
///   ——该臂只由「宽校验探测成功 + 严格探测连接类失败」的编排保证，
///   归 WD5 真机腿（自签 fixture）。
///
/// 好凭据 + 追加键（probe 的参数直构——驱动无 params 访问器）。
fn cred_params(handle: &StubHandle, extra: &[(&str, &str)]) -> WebdavParams {
    let mut all = vec![("webdav_username", "spike"), ("webdav_password", "pw")];
    all.extend_from_slice(extra);
    params(&handle.url, &all)
}

#[tokio::test]
async fn doctor_probe_reports_alive_with_dav_class_and_allow() {
    let handle = spawn_stub(
        seeded_root(),
        AuthMode::Basic {
            user: "spike".to_string(),
            pass: "pw".to_string(),
        },
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let probe = ck_webdav::probe(&cred_params(&handle, &[])).await;
    match &probe {
        ck_webdav::WebdavProbe::Alive { dav_class, allow } => {
            assert_eq!(dav_class.as_deref(), Some("1"), "the stub's DAV class");
            assert!(
                allow.as_deref().is_some_and(|a| a.contains("PROPFIND")),
                "the Allow summary carries the verb face: {allow:?}"
            );
        }
        other => panic!("good credentials must be Alive, got {other:?}"),
    }
}

#[tokio::test]
async fn doctor_probe_reports_reachable_without_auth_when_anonymous_works() {
    let handle = spawn_stub(
        seeded_root(),
        AuthMode::None,
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    // 无凭据配置 + 服务器免认证 → 「可用但建议配置凭据」态。
    let probe = ck_webdav::probe(&params(&handle.url, &[])).await;
    assert!(
        matches!(probe, ck_webdav::WebdavProbe::ReachableNoAuth),
        "anonymous access must surface as ReachableNoAuth, got {probe:?}"
    );
}

#[tokio::test]
async fn doctor_probe_reports_rejected_credentials_with_the_reason() {
    // Digest 协商后仍拒（错密码）——detail 携带 R3 通道的同一文案
    // （键名指路）。
    let handle = spawn_stub(
        seeded_root(),
        wrong_digest_auth(),
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let probe = ck_webdav::probe(&cred_params(&handle, &[])).await;
    match &probe {
        ck_webdav::WebdavProbe::CredentialsRejected { detail } => {
            assert!(
                detail.contains("webdav_username") && detail.contains("webdav_password"),
                "the rejection names the credential keys: {detail}"
            );
        }
        other => panic!("wrong credentials must be CredentialsRejected, got {other:?}"),
    }
}

#[tokio::test]
async fn doctor_probe_rejects_credentials_even_when_options_is_auth_exempt() {
    // WD5 真机揭出（rclone serve webdav）：部分服务器对 OPTIONS 免认证
    //（CORS preflight 语义）——错凭据的 OPTIONS 仍 200 + DAV/Allow 头。
    // probe 的「Alive = 认证通过」判定不得只依赖 OPTIONS：须以真认证动
    // 词复核（PROPFIND），否则错配凭据的卷在 doctor 里显示为健康。
    let handle = spawn_stub(
        seeded_root(),
        AuthMode::Basic {
            user: "spike".to_string(),
            pass: "pw".to_string(),
        },
        Knobs {
            options_unauthenticated: true,
            ..Knobs::default()
        },
        StubStyle::rclone(),
    )
    .await;
    let probe = ck_webdav::probe(&params(
        &handle.url,
        &[("webdav_username", "spike"), ("webdav_password", "WRONG")],
    ))
    .await;
    match &probe {
        ck_webdav::WebdavProbe::CredentialsRejected { .. } => {}
        other => panic!(
            "wrong credentials on an OPTIONS-exempt server must still be \
             CredentialsRejected, got {other:?}"
        ),
    }
}

#[tokio::test]
async fn doctor_probe_reports_basic_mode_refusal_as_rejected_too() {
    // auth=basic 显式模式 + 服务器只收 Digest → Basic 拒细分（同样归
    // CredentialsRejected，detail 区分形态）。
    let handle = spawn_stub(
        seeded_root(),
        digest_auth(Duration::from_secs(3600), false),
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let probe = ck_webdav::probe(&cred_params(&handle, &[("webdav_auth", "basic")])).await;
    match &probe {
        ck_webdav::WebdavProbe::CredentialsRejected { detail } => {
            assert!(
                detail.contains("basic"),
                "the Basic refusal is distinguishable: {detail}"
            );
        }
        other => panic!("basic-mode refusal must be CredentialsRejected, got {other:?}"),
    }
}

#[tokio::test]
async fn doctor_probe_reports_an_unreachable_server() {
    // 127.0.0.1:1 无监听——连接拒绝即刻发生。
    let probe = ck_webdav::probe(&params("http://127.0.0.1:1/", &[])).await;
    match &probe {
        ck_webdav::WebdavProbe::Unreachable { detail } => {
            assert!(!detail.is_empty(), "the detail carries the transport error");
        }
        other => panic!("a refused port must be Unreachable, got {other:?}"),
    }
}
