//! 手搓注入桩的自检套件（Phase 7 / WD2a）——**把桩本身的行为钉死**。
//!
//! 这是「桩照协议真形建模」的证据面：断言值全部来自
//! `docs/tracking/phase7-webdav-fixture.md` 的 WD0 真机矩阵与 WD1 记录
//! 的真形样本（apache 多前缀 XML / IMF-fixdate / RFC 2617 标准向量 /
//! fixture 各腿的状态码与头形态）。桩是新基建，自检直接绿（无红相），
//! 但逐条锁形态防后续漂移——WD2b 起驱动测试建立在「桩可信」之上。
//!
//! Digest 凭据生成说明：WD1 的 auth 纯函数是 `pub(crate)`（WD2b 才接
//! 线），自检在此**直接按 RFC 7616 公式计算 response**（md-5 独立实现，
//! 并以 RFC 2617 §3.5 标准向量钉死数学）——与桩侧验证器互为独立实现，
//! 数学校验而非循环引用。

mod stub;

use std::time::{Duration, Instant};

use md5::{Digest as Md5Digest, Md5};
use stub::{
    spawn_stub, AuthMode, Knobs, ProppatchMode, StubHandle, StubStyle, Vfs, VfsEntry,
    ALLPROP_PROPFIND, MTIME_SEED, MTIME_WRITE,
};

// ------------------------------------------------------------ 测试工具 ---

/// 自检 HTTP 面：no_proxy（loopback 直连——本机代理 env 会劫持）+ 不跟
/// 重定向（301-not-executed 的真机对照形态必须可观察，WD0 spike 同款）。
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("test client")
}

fn method(name: &str) -> reqwest::Method {
    reqwest::Method::from_bytes(name.as_bytes()).expect("method token")
}

fn url_of(handle: &StubHandle, path: &str) -> String {
    format!("{}{}", handle.url.trim_end_matches('/'), path)
}

async fn propfind(handle: &StubHandle, path: &str, depth: &str) -> reqwest::Response {
    client()
        .request(method("PROPFIND"), url_of(handle, path))
        .header("depth", depth)
        .header("content-type", "text/xml; charset=utf-8")
        .body(ALLPROP_PROPFIND)
        .send()
        .await
        .expect("PROPFIND send")
}

/// 传输失败判定：send 失败或读体失败（连接杀/lost-ACK 的两种表现位）。
async fn transport_failed(result: Result<reqwest::Response, reqwest::Error>) -> bool {
    match result {
        Err(_) => true,
        Ok(response) => response.bytes().await.is_err(),
    }
}

fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// RFC 7616 MD5 response（qop=auth 固定——桩恒发 `qop="auth"`）。
#[allow(clippy::too_many_arguments)] // RFC 公式的参数面（WD1 src/auth.rs 同款先例）
fn digest_response(
    user: &str,
    pass: &str,
    realm: &str,
    verb: &str,
    uri: &str,
    nonce: &str,
    nc: u64,
    cnonce: &str,
) -> String {
    let ha1 = md5_hex(&format!("{user}:{realm}:{pass}"));
    let ha2 = md5_hex(&format!("{verb}:{uri}"));
    md5_hex(&format!("{ha1}:{nonce}:{nc:08x}:{cnonce}:auth:{ha2}"))
}

/// 客户端侧 Authorization 头（nc/cnonce 由调用方给——nc 纪律是断言面）。
fn authz_header(
    user: &str,
    pass: &str,
    realm: &str,
    verb: &str,
    uri: &str,
    nonce: &str,
    nc: u64,
) -> String {
    let response = digest_response(user, pass, realm, verb, uri, nonce, nc, "0a4f113b");
    format!(
        "Digest username=\"{user}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", \
         response=\"{response}\", qop=auth, nc={nc:08x}, cnonce=\"0a4f113b\", algorithm=MD5"
    )
}

/// 从 challenge 头提取 nonce（引号内值）。
fn nonce_of(www: &str) -> String {
    let start = www.find("nonce=\"").expect("nonce in challenge") + "nonce=\"".len();
    let end = start + www[start..].find('"').expect("nonce closing quote");
    www[start..end].to_string()
}

fn digest_auth(realm: &str, ttl: Duration, enforce_nc: bool) -> AuthMode {
    AuthMode::Digest {
        user: "spike".to_string(),
        pass: "pw".to_string(),
        realm: realm.to_string(),
        nonce_ttl: ttl,
        enforce_nc,
        stale_after_expiry: true,
    }
}

async fn stub_rclone(vfs: Vfs) -> StubHandle {
    spawn_stub(vfs, AuthMode::None, Knobs::default(), StubStyle::rclone()).await
}

async fn stub_apache(vfs: Vfs) -> StubHandle {
    spawn_stub(vfs, AuthMode::None, Knobs::default(), StubStyle::apache()).await
}

fn seeded_tree() -> Vfs {
    // WD0 样本时刻为 mtime 锚；名字覆盖矩阵⑧的 href 编码面：
    // `&`（XML 转义）、空格（%20）、`+`（字面量）、`ü`（%C3%BC 大写）。
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    vfs.seed_file("/docs/a&b.txt", b"x");
    vfs.seed_file("/docs/a+b.txt", b"y");
    vfs.seed_file("/docs/my file.txt", b"hello");
    vfs.seed_dir("/docs/sub");
    vfs.seed_file("/docs/ü.txt", b"z");
    vfs
}

// ------------------------------------------------------------ 认证面 ---

/// RFC 2617 §3.5 标准向量（RFC 7616 承接的 MD5 基线）——钉死测试侧
/// digest 数学的正确性（桩侧验证器另有 WD1 的同向量单测）。
#[test]
fn rfc2617_vector_pins_the_digest_math() {
    let response = digest_response(
        "Mufasa",
        "Circle Of Life",
        "testrealm@host.com",
        "GET",
        "/dir/index.html",
        "dcd98b7102dd2f0e8b11d0f600bfb0c093",
        1,
        "0a4f113b",
    );
    assert_eq!(response, "6629fae49393a05397450978507c4ef1");
}

/// 矩阵⑨：challenge 形态（realm/nonce/algorithm=MD5/qop="auth"，nonce
/// 含 =/+ 字符）→ 正确 response 放行 → nc 倒退拒（enforce_nc）→ nc
/// 递增放行——矩阵⑨「nc 单调自守（客户端正确性）」的桩侧基准。
#[tokio::test]
async fn digest_roundtrip_challenge_verify_and_nc_discipline() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    let handle = spawn_stub(
        vfs,
        digest_auth("webdavtest", Duration::from_secs(3600), true),
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;

    // (a) 空手请求 → 401 + challenge（矩阵⑨ apache 形态）。
    let response = propfind(&handle, "/docs/", "0").await;
    assert_eq!(response.status().as_u16(), 401);
    let www = response
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .expect("challenge header")
        .to_string();
    assert!(
        www.starts_with(r#"Digest realm="webdavtest", nonce=""#),
        "{www}"
    );
    assert!(www.contains("algorithm=MD5"), "{www}");
    assert!(www.contains(r#"qop="auth""#), "{www}");
    assert!(
        !www.contains("stale"),
        "fresh challenge carries no stale: {www}"
    );
    let nonce = nonce_of(&www);
    assert!(
        nonce.contains('+') && nonce.contains('='),
        "nonce carries =/+ (recorded form): {nonce}"
    );

    // (b) 正确 response（nc=1）→ 207。
    let authz = authz_header("spike", "pw", "webdavtest", "PROPFIND", "/docs/", &nonce, 1);
    let response = client()
        .request(method("PROPFIND"), url_of(&handle, "/docs/"))
        .header("depth", "0")
        .header("authorization", authz.clone())
        .body(ALLPROP_PROPFIND)
        .send()
        .await
        .expect("authed send");
    assert_eq!(response.status().as_u16(), 207);

    // (c) 同 nc 重发（enforce_nc=true）→ 401。
    let response = client()
        .request(method("PROPFIND"), url_of(&handle, "/docs/"))
        .header("depth", "0")
        .header("authorization", authz)
        .body(ALLPROP_PROPFIND)
        .send()
        .await
        .expect("replay send");
    assert_eq!(response.status().as_u16(), 401);

    // (d) nc=2 → 207。
    let response = client()
        .request(method("PROPFIND"), url_of(&handle, "/docs/"))
        .header("depth", "0")
        .header(
            "authorization",
            authz_header("spike", "pw", "webdavtest", "PROPFIND", "/docs/", &nonce, 2),
        )
        .body(ALLPROP_PROPFIND)
        .send()
        .await
        .expect("nc=2 send");
    assert_eq!(response.status().as_u16(), 207);

    // 记录器：4 请求（1 挑战 + 3 签名），nc 序列 [1, 1, 2]。
    let requests = handle.requests();
    assert_eq!(requests.len(), 4);
    let ncs: Vec<u64> = requests
        .iter()
        .filter_map(|request| request.digest_nc)
        .collect();
    assert_eq!(ncs, vec![1, 1, 2]);
    assert_eq!(requests[3].digest_nonce.as_deref(), Some(nonce.as_str()));
}

/// 矩阵⑨：nonce 过期 → 401 + stale=true（实测在 algorithm 后）+ 新
/// nonce；新 nonce 重算恰一次恢复（D1 stale 再协商腿的桩载体）。
#[tokio::test]
async fn digest_expired_nonce_answers_stale_true_and_renegotiation() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    let handle = spawn_stub(
        vfs,
        digest_auth("webdavtest", Duration::from_millis(50), false),
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;

    let response = propfind(&handle, "/docs/", "0").await;
    assert_eq!(response.status().as_u16(), 401);
    let www = response
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .expect("challenge")
        .to_string();
    let stale_nonce = nonce_of(&www);

    tokio::time::sleep(Duration::from_millis(80)).await;

    let response = client()
        .request(method("PROPFIND"), url_of(&handle, "/docs/"))
        .header("depth", "0")
        .header(
            "authorization",
            authz_header(
                "spike",
                "pw",
                "webdavtest",
                "PROPFIND",
                "/docs/",
                &stale_nonce,
                1,
            ),
        )
        .body(ALLPROP_PROPFIND)
        .send()
        .await
        .expect("expired send");
    assert_eq!(response.status().as_u16(), 401);
    let www2 = response
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .expect("stale challenge")
        .to_string();
    // stale=true 位置不定（实测在 algorithm 后）——包含即可，不锁位次。
    assert!(www2.contains("stale=true"), "{www2}");
    let fresh_nonce = nonce_of(&www2);
    assert_ne!(fresh_nonce, stale_nonce, "expiry must issue a new nonce");

    let response = client()
        .request(method("PROPFIND"), url_of(&handle, "/docs/"))
        .header("depth", "0")
        .header(
            "authorization",
            authz_header(
                "spike",
                "pw",
                "webdavtest",
                "PROPFIND",
                "/docs/",
                &fresh_nonce,
                1,
            ),
        )
        .body(ALLPROP_PROPFIND)
        .send()
        .await
        .expect("renegotiated send");
    assert_eq!(response.status().as_u16(), 207);
}

/// 矩阵⑨：apache 不查 nc 重放（enforce_nc=false——同 nonce 同 nc 二发
/// 仍 207）——「纪律只在客户端」的实证形态。
#[tokio::test]
async fn digest_replay_tolerated_when_nc_unenforced() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    let handle = spawn_stub(
        vfs,
        digest_auth("webdavtest", Duration::from_secs(3600), false),
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;

    let response = propfind(&handle, "/docs/", "0").await;
    let www = response
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .expect("challenge")
        .to_string();
    let nonce = nonce_of(&www);

    for _ in 0..2 {
        let response = client()
            .request(method("PROPFIND"), url_of(&handle, "/docs/"))
            .header("depth", "0")
            .header(
                "authorization",
                authz_header("spike", "pw", "webdavtest", "PROPFIND", "/docs/", &nonce, 1),
            )
            .body(ALLPROP_PROPFIND)
            .send()
            .await
            .expect("same-nc send");
        assert_eq!(
            response.status().as_u16(),
            207,
            "apache tolerates nc replay"
        );
    }
}

/// response-uri 严格校验（apache 形态）：签名的 uri 必须逐字节等于 wire
/// 请求 URI——「digest response-uri 未转义」（负面清单 §4.5-2）的桩侧
/// 暴露面。
#[tokio::test]
async fn digest_signed_uri_mismatch_is_rejected() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    let handle = spawn_stub(
        vfs,
        digest_auth("webdavtest", Duration::from_secs(3600), false),
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;

    let response = propfind(&handle, "/docs/", "0").await;
    let www = response
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .expect("challenge")
        .to_string();
    let nonce = nonce_of(&www);

    // wire 是 "/docs/"，签名用 "/docs"（差一个尾斜杠）→ 401。
    let response = client()
        .request(method("PROPFIND"), url_of(&handle, "/docs/"))
        .header("depth", "0")
        .header(
            "authorization",
            authz_header("spike", "pw", "webdavtest", "PROPFIND", "/docs", &nonce, 1),
        )
        .body(ALLPROP_PROPFIND)
        .send()
        .await
        .expect("mismatched-uri send");
    assert_eq!(response.status().as_u16(), 401);
}

/// Basic：未带 → 401 + `Basic realm="stub"`；错凭据 → 401；正确 → 207
///（矩阵⑨ rclone 侧 challenge 形态）。
#[tokio::test]
async fn basic_challenge_wrong_then_right_credentials() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    let handle = spawn_stub(
        vfs,
        AuthMode::Basic {
            user: "spike".to_string(),
            pass: "pw".to_string(),
        },
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;

    let response = propfind(&handle, "/docs/", "0").await;
    assert_eq!(response.status().as_u16(), 401);
    assert_eq!(
        response
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok())
            .expect("basic challenge"),
        r#"Basic realm="stub""#
    );

    let response = client()
        .request(method("PROPFIND"), url_of(&handle, "/docs/"))
        .header("depth", "0")
        .basic_auth("spike", Some("wrong"))
        .body(ALLPROP_PROPFIND)
        .send()
        .await
        .expect("wrong creds");
    assert_eq!(response.status().as_u16(), 401);

    let response = client()
        .request(method("PROPFIND"), url_of(&handle, "/docs/"))
        .header("depth", "0")
        .basic_auth("spike", Some("pw"))
        .body(ALLPROP_PROPFIND)
        .send()
        .await
        .expect("right creds");
    assert_eq!(response.status().as_u16(), 207);
}

/// NTLM challenge 原样回发（驱动明确不支持——可行动拒绝文案的测试面）。
#[tokio::test]
async fn ntlm_challenge_scheme_is_returned_verbatim() {
    let handle = spawn_stub(
        Vfs::new(),
        AuthMode::Challenge {
            scheme: "NTLM".to_string(),
        },
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let response = propfind(&handle, "/", "0").await;
    assert_eq!(response.status().as_u16(), 401);
    assert_eq!(
        response
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok())
            .expect("ntlm challenge"),
        "NTLM"
    );
}

// ----------------------------------------------------------- PROPFIND 面 ---

/// Classic（rclone）形态 Depth 1 **整字节**钉死：href 编码四形态（%20/
/// &amp;/+/字面、%C3%BC 大写）、IMF-fixdate mtime、目录 getcontentlength
/// 内层 404 propstat、名字序（矩阵⑧左列）。
#[tokio::test]
async fn propfind_classic_depth1_pins_exact_xml() {
    let handle = stub_rclone(seeded_tree()).await;
    let response = propfind(&handle, "/docs/", "1").await;
    assert_eq!(response.status().as_u16(), 207);
    let body = response.text().await.expect("body");

    let dir_entry = concat!(
        r#"<D:response><D:href>/docs/</D:href>"#,
        r#"<D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype>"#,
        r#"<D:getlastmodified>Mon, 21 Sep 2026 10:51:05 GMT</D:getlastmodified>"#,
        r#"</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"#,
        r#"<D:propstat><D:prop><D:getcontentlength>999</D:getcontentlength></D:prop>"#,
        r#"<D:status>HTTP/1.1 404 Not Found</D:status></D:propstat></D:response>"#,
    );
    let file = |href: &str, length: usize| {
        format!(
            concat!(
                r#"<D:response><D:href>{href}</D:href>"#,
                r#"<D:propstat><D:prop><D:resourcetype/>"#,
                r#"<D:getcontentlength>{length}</D:getcontentlength>"#,
                r#"<D:getlastmodified>Mon, 21 Sep 2026 10:51:05 GMT</D:getlastmodified>"#,
                r#"</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"#,
            ),
            href = href,
            length = length,
        )
    };
    let expected = concat!(
        r#"<?xml version="1.0" encoding="utf-8"?>"#,
        r#"<D:multistatus xmlns:D="DAV:">"#,
    )
    .to_string()
        + dir_entry
        + &file("/docs/a&amp;b.txt", 1)
        + &file("/docs/a+b.txt", 1)
        + &file("/docs/my%20file.txt", 5)
        + &dir_entry.replace("/docs/", "/docs/sub/")
        + &file("/docs/%C3%BC.txt", 1)
        + "</D:multistatus>";
    assert_eq!(body, expected);
}

/// Depth 0 只回自条目；空目录 Depth 1 仅 self（矩阵⑧ Depth/空目录腿）。
#[tokio::test]
async fn propfind_classic_depth0_and_empty_dir_self_only() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/empty");
    vfs.seed_file("/side.txt", b"!");
    let handle = stub_rclone(vfs).await;

    let response = propfind(&handle, "/side.txt", "0").await;
    assert_eq!(response.status().as_u16(), 207);
    let body = response.text().await.expect("body");
    assert_eq!(
        body.matches("<D:response>").count(),
        1,
        "Depth 0 = self only"
    );
    assert!(body.contains(r#"<D:href>/side.txt</D:href>"#), "{body}");

    let response = propfind(&handle, "/empty/", "1").await;
    assert_eq!(response.status().as_u16(), 207);
    let body = response.text().await.expect("body");
    assert_eq!(
        body.matches("<D:response>").count(),
        1,
        "empty dir Depth 1 = self only"
    );
    assert!(body.contains(r#"<D:href>/empty/</D:href>"#), "{body}");

    // 不存在 → 404（双服务器一致）。
    let response = propfind(&handle, "/nope/", "0").await;
    assert_eq!(response.status().as_u16(), 404);
}

/// ApacheStyle 形态钉死（矩阵⑧右列 + WD1 记录样本）：多前缀并存 +
/// lp2 私有 ns + 404 块在 200 块之前 + creationdate ISO8601 +
/// `<lp2:executable F/>` 属性形元素 + 文件 `ns0:resourcetype/`。
#[tokio::test]
async fn propfind_apache_style_pins_exact_xml() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    vfs.seed_file("/docs/file.txt", b"abcd");
    let handle = stub_apache(vfs).await;

    let response = propfind(&handle, "/docs/", "0").await;
    assert_eq!(response.status().as_u16(), 207);
    let body = response.text().await.expect("body");
    let expected = concat!(
        r#"<?xml version="1.0" encoding="utf-8"?>"#,
        r#"<D:multistatus xmlns:D="DAV:" xmlns:ns0="DAV:" xmlns:lp1="DAV:" "#,
        r#"xmlns:g0="DAV:" xmlns:lp2="http://apache.org/dav/props/">"#,
        r#"<D:response><D:href>/docs/</D:href>"#,
        // 404 块在前（记录样本顺序）。
        r#"<D:propstat><D:prop><g0:getcontentlength>999</g0:getcontentlength></D:prop>"#,
        r#"<D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>"#,
        r#"<D:propstat><D:prop><lp1:resourcetype><D:collection/></lp1:resourcetype>"#,
        r#"<lp1:getlastmodified>Mon, 21 Sep 2026 10:51:05 GMT</lp1:getlastmodified>"#,
        r#"<lp1:creationdate>2026-09-21T10:51:05Z</lp1:creationdate>"#,
        r#"<lp2:executable F/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"#,
        r#"</D:response>"#,
        r#"</D:multistatus>"#,
    );
    assert_eq!(body, expected);

    // Depth 1 的文件条目片段（前缀混用面）。
    let response = propfind(&handle, "/docs/", "1").await;
    let body = response.text().await.expect("body");
    assert!(body.contains("<ns0:resourcetype/>"), "{body}");
    assert!(
        body.contains("<lp1:getcontentlength>4</lp1:getcontentlength>"),
        "{body}"
    );
    assert!(
        body.contains("<lp1:creationdate>2026-09-21T10:51:05Z</lp1:creationdate>"),
        "{body}"
    );
}

/// quota 点名（RFC 4331）：自条目内层 404 propstat 含两 quota props
///（矩阵⑪——驱动 quota None 降级的实证依据）。
#[tokio::test]
async fn quota_propfind_answers_inner_404() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    let handle = stub_rclone(vfs).await;
    let quota_body = concat!(
        r#"<?xml version="1.0" encoding="utf-8" ?>"#,
        r#"<D:propfind xmlns:D="DAV:"><D:prop>"#,
        r#"<D:quota-used-bytes/><D:quota-available-bytes/>"#,
        r#"</D:prop></D:propfind>"#,
    );
    let response = client()
        .request(method("PROPFIND"), url_of(&handle, "/docs/"))
        .header("depth", "0")
        .body(quota_body)
        .send()
        .await
        .expect("quota propfind");
    assert_eq!(response.status().as_u16(), 207);
    let body = response.text().await.expect("body");
    assert!(
        body.contains(r#"<D:quota-used-bytes/><D:quota-available-bytes/>"#),
        "{body}"
    );
    assert!(body.contains("HTTP/1.1 404 Not Found"), "{body}");
}

// ----------------------------------------------------------- 尾斜杠面 ---

/// 矩阵⑧右列：apache 集合 no-slash → 301 + Location；slashed → 207；
/// 文件 no-slash 正常。
#[tokio::test]
async fn slash_strict_redirects_collections_but_not_files() {
    let handle = stub_apache(seeded_tree()).await;

    let response = propfind(&handle, "/docs", "0").await;
    assert_eq!(response.status().as_u16(), 301);
    assert_eq!(
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .expect("location"),
        "/docs/"
    );

    let response = propfind(&handle, "/docs/", "0").await;
    assert_eq!(response.status().as_u16(), 207);

    let response = propfind(&handle, "/docs/my%20file.txt", "0").await;
    assert_eq!(response.status().as_u16(), 207, "file no-slash is fine");
}

/// 矩阵⑧左列：rclone 尾斜杠全不敏感（目录 no-slash ✓、文件 + 斜杠 ✓）。
#[tokio::test]
async fn slash_insensitive_tolerates_all_forms() {
    let handle = stub_rclone(seeded_tree()).await;
    for path in [
        "/docs",
        "/docs/",
        "/docs/my%20file.txt",
        "/docs/my%20file.txt/",
    ] {
        let response = propfind(&handle, path, "0").await;
        assert_eq!(response.status().as_u16(), 207, "{path}");
    }
}

// ------------------------------------------------------------- MKCOL 面 ---

/// 矩阵⑥：rclone 已存在 → **201 幂等成功陷阱**；RFC/apache → 405；
/// 父缺失 → 409（双服务器一致）；同名文件 → 405（一致）。
#[tokio::test]
async fn mkcol_rclone201_trap_vs_rfc405() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    vfs.seed_file("/docs/file.txt", b"x");
    let rclone = stub_rclone(vfs.clone()).await;
    let response = client()
        .request(method("MKCOL"), url_of(&rclone, "/docs/"))
        .send()
        .await
        .expect("rclone mkcol");
    assert_eq!(response.status().as_u16(), 201, "rclone trap: 201 not 405");

    let rfc = spawn_stub(vfs, AuthMode::None, Knobs::default(), StubStyle::default()).await;
    let response = client()
        .request(method("MKCOL"), url_of(&rfc, "/docs/"))
        .send()
        .await
        .expect("rfc mkcol");
    assert_eq!(response.status().as_u16(), 405);

    // 父缺失 → 409；同名文件 → 405。
    let response = client()
        .request(method("MKCOL"), url_of(&rfc, "/missing-parent/sub/"))
        .send()
        .await
        .expect("missing parent");
    assert_eq!(response.status().as_u16(), 409);
    let response = client()
        .request(method("MKCOL"), url_of(&rfc, "/docs/file.txt/"))
        .send()
        .await
        .expect("file conflict");
    assert_eq!(response.status().as_u16(), 405);

    // SlashStrict：已存在目录 no-slash → 301 不执行（矩阵⑥ apache）。
    let apache = stub_apache(Vfs::new()).await;
    apache.seed_dir("/docs");
    let response = client()
        .request(method("MKCOL"), url_of(&apache, "/docs"))
        .send()
        .await
        .expect("strict mkcol");
    assert_eq!(response.status().as_u16(), 301);
}

// -------------------------------------------------------------- GET 面 ---

/// 矩阵④：200 全量 / 206+Content-Range / EOF 钳制 / 416+`bytes */N` /
/// 倒序 416（rclone 形态）/ 后缀 Range。
#[tokio::test]
async fn get_range_matrix() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/range.bin", b"0123456789".repeat(10).as_slice()); // 100 字节
    let handle = stub_rclone(vfs).await;
    let url = url_of(&handle, "/range.bin");

    let response = client().get(&url).send().await.expect("full get");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        response
            .headers()
            .get("accept-ranges")
            .and_then(|value| value.to_str().ok())
            .expect("accept-ranges"),
        "bytes"
    );
    assert_eq!(response.bytes().await.expect("body").len(), 100);

    let response = client()
        .get(&url)
        .header("range", "bytes=10-19")
        .send()
        .await
        .expect("range get");
    assert_eq!(response.status().as_u16(), 206);
    assert_eq!(
        response
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok())
            .expect("content-range"),
        "bytes 10-19/100"
    );
    assert_eq!(
        response.bytes().await.expect("body").as_ref(),
        b"0123456789".as_slice()
    );

    // EOF 钳制（fixture 腿样本：90-999999 → 90-99/100）。
    let response = client()
        .get(&url)
        .header("range", "bytes=90-999999")
        .send()
        .await
        .expect("clamp get");
    assert_eq!(response.status().as_u16(), 206);
    assert_eq!(
        response
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok())
            .expect("clamped content-range"),
        "bytes 90-99/100"
    );
    assert_eq!(response.bytes().await.expect("body").len(), 10);

    // 越 EOF 起点 → 416 + `bytes */N`（fixture 腿样本 999999-）。
    let response = client()
        .get(&url)
        .header("range", "bytes=999999-")
        .send()
        .await
        .expect("416 get");
    assert_eq!(response.status().as_u16(), 416);
    assert_eq!(
        response
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok())
            .expect("416 content-range"),
        "bytes */100"
    );

    // 倒序（rclone 真形 416；apache 200 由 range_ignore 旋钮覆盖）。
    let response = client()
        .get(&url)
        .header("range", "bytes=50-10")
        .send()
        .await
        .expect("inverted get");
    assert_eq!(response.status().as_u16(), 416);

    // 后缀形态：最后 10 字节。
    let response = client()
        .get(&url)
        .header("range", "bytes=-10")
        .send()
        .await
        .expect("suffix get");
    assert_eq!(response.status().as_u16(), 206);
    assert_eq!(
        response
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok())
            .expect("suffix content-range"),
        "bytes 90-99/100"
    );
}

/// range_ignore 旋钮：Range 头被无视回 200 全量（apache 忽略 Range 的
/// 强化形态——驱动 200 截断回退路径的测试面）。
#[tokio::test]
async fn get_range_ignore_answers_200_full() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", b"0123456789".repeat(10).as_slice());
    let style = StubStyle {
        range_ignore: true,
        ..StubStyle::rclone()
    };
    let handle = spawn_stub(vfs, AuthMode::None, Knobs::default(), style).await;
    let response = client()
        .get(url_of(&handle, "/f.bin"))
        .header("range", "bytes=10-19")
        .send()
        .await
        .expect("ignored range");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.bytes().await.expect("body").len(), 100);
}

/// GET 集合：rclone → 200+HTML 目录页；apache slashed → 403、no-slash
/// → 301（矩阵「附加观察」）。
#[tokio::test]
async fn get_collection_forms_per_server() {
    let rclone = stub_rclone(Vfs::new()).await;
    rclone.seed_dir("/docs");
    let response = client()
        .get(url_of(&rclone, "/docs/"))
        .send()
        .await
        .expect("rclone dir get");
    assert_eq!(response.status().as_u16(), 200);
    let body = response.text().await.expect("body");
    assert!(
        body.contains("<html>"),
        "rclone serves an HTML index: {body}"
    );

    let apache = stub_apache(Vfs::new()).await;
    apache.seed_dir("/docs");
    let response = client()
        .get(url_of(&apache, "/docs/"))
        .send()
        .await
        .expect("apache dir get");
    assert_eq!(response.status().as_u16(), 403);
    let response = client()
        .get(url_of(&apache, "/docs"))
        .send()
        .await
        .expect("apache dir no-slash get");
    assert_eq!(response.status().as_u16(), 301);
}

// ------------------------------------------------------------- MOVE 面 ---

/// 矩阵⑤：F+目标存在 → 412（apache 记录 HTML 体文案）；T → 204 覆盖；
/// 缺头 + 目标存在 → rclone 412 / RFC 缺省 T；目录 MOVE → 201。
#[tokio::test]
async fn move_overwrite_semantics() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/a.txt", b"MOVE-A");
    vfs.seed_file("/b.txt", b"DEST-OLD");
    let handle = stub_rclone(vfs.clone()).await;

    // Overwrite:F + 目标存在 → 412 + apache 记录体文案。
    let response = client()
        .request(method("MOVE"), url_of(&handle, "/a.txt"))
        .header("destination", url_of(&handle, "/b.txt"))
        .header("overwrite", "F")
        .send()
        .await
        .expect("F move");
    assert_eq!(response.status().as_u16(), 412);
    let body = response.text().await.expect("body");
    assert!(body.contains("Destination is not empty"), "{body}");
    assert!(handle.exists("/a.txt"), "412 must not execute the move");

    // Overwrite:T → 204，内容迁移、源消失（fixture 腿断言组）。
    let response = client()
        .request(method("MOVE"), url_of(&handle, "/a.txt"))
        .header("destination", url_of(&handle, "/b.txt"))
        .header("overwrite", "T")
        .send()
        .await
        .expect("T move");
    assert_eq!(response.status().as_u16(), 204);
    assert_eq!(handle.take("/b.txt").as_deref(), Some(b"MOVE-A".as_slice()));
    assert!(!handle.exists("/a.txt"));

    // 缺 Overwrite 头 + 目标存在：rclone 预置 → 412（偏离 RFC 缺省）。
    handle.seed_file("/c.txt", b"C");
    handle.seed_file("/d.txt", b"D");
    let response = client()
        .request(method("MOVE"), url_of(&handle, "/c.txt"))
        .header("destination", url_of(&handle, "/d.txt"))
        .send()
        .await
        .expect("headerless move (rclone)");
    assert_eq!(
        response.status().as_u16(),
        412,
        "rclone treats missing header as F"
    );

    // RFC 缺省（default 风格 RfcT）→ 覆盖 204（c/d 需在新桩上重种——
    // 前面的 seed 落在旧桩的 VFS 里）。
    let mut vfs = Vfs::new();
    vfs.seed_file("/c.txt", b"C");
    vfs.seed_file("/d.txt", b"D");
    let handle = spawn_stub(vfs, AuthMode::None, Knobs::default(), StubStyle::default()).await;
    let response = client()
        .request(method("MOVE"), url_of(&handle, "/c.txt"))
        .header("destination", url_of(&handle, "/d.txt"))
        .send()
        .await
        .expect("headerless move (rfc)");
    assert_eq!(response.status().as_u16(), 204);

    // 目录 MOVE（源+Destination 均带尾斜杠）→ 201 + 子树迁移。
    let handle = stub_rclone(Vfs::new()).await;
    handle.seed_file("/d1/inner.txt", b"inner");
    let response = client()
        .request(method("MOVE"), url_of(&handle, "/d1/"))
        .header("destination", url_of(&handle, "/d2/"))
        .send()
        .await
        .expect("dir move");
    assert_eq!(response.status().as_u16(), 201);
    assert_eq!(
        handle.take("/d2/inner.txt").as_deref(),
        Some(b"inner".as_slice())
    );
}

/// 矩阵⑤⑩：目标父缺失 rclone 403 / apache 500；相对 Destination
/// rclone 接受 / apache 400 拒；apache 目录源 no-slash → 301 不执行。
#[tokio::test]
async fn move_missing_parent_relative_destination_and_strict_form() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/m.txt", b"M");
    let rclone = stub_rclone(vfs.clone()).await;

    let response = client()
        .request(method("MOVE"), url_of(&rclone, "/m.txt"))
        .header("destination", url_of(&rclone, "/missing-parent/m.txt"))
        .send()
        .await
        .expect("rclone missing parent");
    assert_eq!(response.status().as_u16(), 403);

    // 相对 Destination（rclone 接受腿，矩阵⑩）。
    let response = client()
        .request(method("MOVE"), url_of(&rclone, "/m.txt"))
        .header("destination", "/m2.txt")
        .send()
        .await
        .expect("rclone relative dest");
    assert_eq!(response.status().as_u16(), 201);
    assert!(handle_has(&rclone, "/m2.txt"));

    let apache = stub_apache(vfs).await;
    apache.seed_file("/m.txt", b"M");
    let response = client()
        .request(method("MOVE"), url_of(&apache, "/m.txt"))
        .header("destination", url_of(&apache, "/missing-parent/m.txt"))
        .send()
        .await
        .expect("apache missing parent");
    assert_eq!(response.status().as_u16(), 500);

    let response = client()
        .request(method("MOVE"), url_of(&apache, "/m.txt"))
        .header("destination", "/m3.txt")
        .send()
        .await
        .expect("apache relative dest");
    assert_eq!(response.status().as_u16(), 400);
    assert!(handle_has(&apache, "/m.txt"), "400 must not move");

    // apache：目录源 no-slash → 301 且不执行。
    apache.seed_file("/dir3/inner.txt", b"i");
    let response = client()
        .request(method("MOVE"), url_of(&apache, "/dir3"))
        .header("destination", url_of(&apache, "/dir4/"))
        .send()
        .await
        .expect("strict dir move");
    assert_eq!(response.status().as_u16(), 301);
    assert!(
        handle_has(&apache, "/dir3/inner.txt"),
        "301 must not execute"
    );
    assert!(!handle_has(&apache, "/dir4"));
}

fn handle_has(handle: &StubHandle, path: &str) -> bool {
    handle.exists(path)
}

// ----------------------------------------------------------- DELETE 面 ---

/// 矩阵⑦：文件 204 → 再删 404；集合递归 204（子项同灭）；apache
/// no-slash 集合 → 301 不执行。
#[tokio::test]
async fn delete_file_dir_and_strict_redirect() {
    let handle = stub_rclone(Vfs::new()).await;
    handle.seed_file("/del.txt", b"delete-me");
    let response = client()
        .delete(url_of(&handle, "/del.txt"))
        .send()
        .await
        .expect("file delete");
    assert_eq!(response.status().as_u16(), 204);
    let response = client()
        .delete(url_of(&handle, "/del.txt"))
        .send()
        .await
        .expect("re-delete");
    assert_eq!(response.status().as_u16(), 404);

    handle.seed_file("/deldir/inner.txt", b"inner");
    let response = client()
        .delete(url_of(&handle, "/deldir/"))
        .send()
        .await
        .expect("dir delete");
    assert_eq!(response.status().as_u16(), 204);
    let snapshot = handle.snapshot();
    assert!(!snapshot.contains_key("/deldir"), "{snapshot:?}");
    assert!(!snapshot.contains_key("/deldir/inner.txt"), "{snapshot:?}");

    let apache = stub_apache(Vfs::new()).await;
    apache.seed_file("/keep/inner.txt", b"keep");
    let response = client()
        .delete(url_of(&apache, "/keep"))
        .send()
        .await
        .expect("strict delete");
    assert_eq!(response.status().as_u16(), 301);
    assert!(apache.exists("/keep/inner.txt"), "301 must not execute");

    // 桩防线：根不可删。
    let response = client()
        .delete(apache.url.clone())
        .send()
        .await
        .expect("root delete");
    assert_eq!(response.status().as_u16(), 403);
}

// -------------------------------------------------------------- PUT 面 ---

/// 新建 201（mtime = 写效果锚）/覆盖 204/集合 404|409 按服务器/父缺失
/// 409（从严形态）/PROPFIND 回读 getcontentlength + getlastmodified。
#[tokio::test]
async fn put_create_overwrite_collection_parent() {
    let handle = stub_rclone(Vfs::new()).await;
    handle.seed_dir("/docs");

    let response = client()
        .put(url_of(&handle, "/docs/new.bin"))
        .body("payload-v1")
        .send()
        .await
        .expect("new put");
    assert_eq!(response.status().as_u16(), 201);
    // 快照断言（take 是消费语义——文件还要留给覆盖腿与 PROPFIND 回读）。
    assert_eq!(
        handle.snapshot().get("/docs/new.bin"),
        Some(&VfsEntry::File {
            bytes: b"payload-v1".to_vec(),
            mtime: MTIME_WRITE
        })
    );

    // PROPFIND 回读：长度 + 写效果 mtime（IMF-fixdate 真格式）。
    let response = propfind(&handle, "/docs/new.bin", "0").await;
    let body = response.text().await.expect("body");
    assert!(
        body.contains("<D:getcontentlength>10</D:getcontentlength>"),
        "{body}"
    );
    assert!(body.contains("Mon, 21 Sep 2026 10:52:05 GMT"), "{body}");
    assert_eq!(
        MTIME_WRITE - MTIME_SEED,
        60,
        "write anchor is +60s off the seed"
    );

    let response = client()
        .put(url_of(&handle, "/docs/new.bin"))
        .body(b"payload-v2-longer".to_vec())
        .send()
        .await
        .expect("overwrite put");
    assert_eq!(response.status().as_u16(), 204);
    assert_eq!(
        handle.take("/docs/new.bin").as_deref(),
        Some(b"payload-v2-longer".as_slice())
    );

    // PUT 到集合：rclone 404 / apache 409。
    let response = client()
        .put(url_of(&handle, "/docs/"))
        .body("x")
        .send()
        .await
        .expect("rclone put on collection");
    assert_eq!(response.status().as_u16(), 404);
    let apache = stub_apache(Vfs::new()).await;
    apache.seed_dir("/docs");
    let response = client()
        .put(url_of(&apache, "/docs/"))
        .body("x")
        .send()
        .await
        .expect("apache put on collection");
    assert_eq!(response.status().as_u16(), 409);
    let body = response.text().await.expect("body");
    assert!(body.contains("Cannot PUT to a collection"), "{body}");

    // 父缺失 → 409（从严：漏建父的驱动缺陷必须现形）。
    let response = client()
        .put(url_of(&handle, "/missing/f.txt"))
        .body("x")
        .send()
        .await
        .expect("missing parent put");
    assert_eq!(response.status().as_u16(), 409);
}

// ---------------------------------------------------------- PROPPATCH 面 ---

/// 矩阵①三形态：rclone 403-in-207 + cannot-modify-protected-property；
/// apache dead-prop 假成功（内层 200 但 mtime 不变）；apache
/// getlastmodified 内层 409（字面 `HTTP/1.1 409 (status)`，判码不判
/// 文案）。三形态 VFS mtime 均不动（真实 mtime 无法写——D2 降级依据）。
#[tokio::test]
async fn proppatch_three_modes_leave_mtime_untouched() {
    let body = concat!(
        r#"<?xml version="1.0" encoding="utf-8" ?>"#,
        r#"<D:propertyupdate xmlns:D="DAV:"><D:set><D:prop>"#,
        r#"<D:getlastmodified>Thu, 01 Jan 2026 12:00:00 GMT</D:getlastmodified>"#,
        r#"</D:prop></D:set></D:propertyupdate>"#,
    );
    for (mode, expected_fragment) in [
        (
            ProppatchMode::Rclone403In207,
            "cannot-modify-protected-property",
        ),
        (
            ProppatchMode::ApacheDeadProp,
            "<D:status>HTTP/1.1 200 OK</D:status>",
        ),
        (
            ProppatchMode::Apache409,
            "<D:status>HTTP/1.1 409 (status)</D:status>",
        ),
    ] {
        let mut vfs = Vfs::new();
        vfs.seed_file_with_mtime("/f.txt", b"v1", MTIME_SEED);
        let style = StubStyle {
            proppatch: mode,
            ..StubStyle::rclone()
        };
        let handle = spawn_stub(vfs, AuthMode::None, Knobs::default(), style).await;
        let response = client()
            .request(method("PROPPATCH"), url_of(&handle, "/f.txt"))
            .header("content-type", "text/xml; charset=utf-8")
            .body(body)
            .send()
            .await
            .expect("proppatch");
        assert_eq!(response.status().as_u16(), 207, "{mode:?}");
        let text = response.text().await.expect("body");
        assert!(text.contains(expected_fragment), "{mode:?}: {text}");
        match handle.snapshot().get("/f.txt") {
            Some(VfsEntry::File { mtime, .. }) => {
                assert_eq!(*mtime, MTIME_SEED, "{mode:?}: real mtime must not change");
            }
            other => panic!("{mode:?}: file vanished: {other:?}"),
        }
    }
}

// ----------------------------------------------------------- OPTIONS 面 ---

/// 200 + Allow（全动词）+ `DAV: 1`。
#[tokio::test]
async fn options_headers() {
    let handle = stub_rclone(Vfs::new()).await;
    let response = client()
        .request(method("OPTIONS"), handle.url.clone())
        .send()
        .await
        .expect("options");
    assert_eq!(response.status().as_u16(), 200);
    let allow = response
        .headers()
        .get("allow")
        .and_then(|value| value.to_str().ok())
        .expect("allow header");
    for verb in [
        "OPTIONS",
        "GET",
        "PUT",
        "DELETE",
        "PROPFIND",
        "PROPPATCH",
        "MKCOL",
        "MOVE",
    ] {
        assert!(allow.contains(verb), "allow lists {verb}: {allow}");
    }
    assert_eq!(
        response
            .headers()
            .get("dav")
            .and_then(|value| value.to_str().ok())
            .expect("dav header"),
        "1"
    );
}

// ------------------------------------------------------------ 故障旋钮 ---

/// malformed_multistatus：207 + 截断 XML（无闭合——驱动解析不崩不静默）。
#[tokio::test]
async fn malformed_multistatus_knob() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    let knobs = Knobs {
        malformed_multistatus: true,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let response = propfind(&handle, "/docs/", "0").await;
    assert_eq!(response.status().as_u16(), 207);
    let body = response.text().await.expect("truncated body");
    assert!(body.contains("<D:multistatus"), "{body}");
    assert!(!body.contains("</D:multistatus>"), "{body}");
    assert!(!body.contains("</D:response>"), "{body}");
}

/// transient_5xx：前 N 次 PROPFIND/GET 回 503，随后自愈（重试白名单面）。
#[tokio::test]
async fn transient_5xx_self_heals() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.txt", b"body");
    let knobs = Knobs {
        transient_5xx: 2,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    for expected in [503u16, 503, 200] {
        let response = client()
            .get(url_of(&handle, "/f.txt"))
            .send()
            .await
            .expect("get");
        assert_eq!(response.status().as_u16(), expected);
    }
    // 503 只作用于 PROPFIND/GET：PUT 不消耗计数（第三次 GET 已 200 的
    // 交叉验证——计数被前两次 GET 用尽）。
    let response = client()
        .put(url_of(&handle, "/g.txt"))
        .body("x")
        .send()
        .await
        .expect("put");
    assert_eq!(response.status().as_u16(), 201);
    assert_eq!(handle.requests().len(), 4);
}

/// rate_limit_429：前 N 次回 429 + Retry-After（可选秒数）。
#[tokio::test]
async fn rate_limit_429_with_retry_after() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    let knobs = Knobs {
        rate_limit_429: Some((1, Some(7))),
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let response = client()
        .put(url_of(&handle, "/docs/f.txt"))
        .body("x")
        .send()
        .await
        .expect("limited put");
    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .expect("retry-after"),
        "7"
    );
    let response = client()
        .put(url_of(&handle, "/docs/f.txt"))
        .body("x")
        .send()
        .await
        .expect("second put");
    assert_eq!(response.status().as_u16(), 201);
}

/// unexpected_301：文件 PROPFIND → 301 + Location；目录不受影响。
#[tokio::test]
async fn unexpected_301_on_file_propfind_only() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.txt", b"x");
    vfs.seed_dir("/docs");
    let knobs = Knobs {
        unexpected_301: true,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let response = propfind(&handle, "/f.txt", "0").await;
    assert_eq!(response.status().as_u16(), 301);
    assert_eq!(
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .expect("location"),
        "/f.txt/"
    );
    let response = propfind(&handle, "/docs/", "0").await;
    assert_eq!(response.status().as_u16(), 207, "collections are exempt");
}

/// kill_connections：首块即断（send 或读体失败），计数消耗后恢复。
#[tokio::test]
async fn kill_connection_then_recovery() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.txt", b"body");
    let knobs = Knobs {
        kill_connections: std::sync::atomic::AtomicUsize::new(1),
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let result = client().get(url_of(&handle, "/f.txt")).send().await;
    assert!(
        transport_failed(result).await,
        "killed request must fail in transport"
    );
    assert_eq!(
        handle
            .knobs
            .kill_connections
            .load(std::sync::atomic::Ordering::Acquire),
        0,
        "the kill is consumed"
    );
    let response = client()
        .get(url_of(&handle, "/f.txt"))
        .send()
        .await
        .expect("recovered get");
    assert_eq!(response.status().as_u16(), 200);
}

/// lost-ACK on PUT：效果已落（VFS 可见）+ 响应丢失（传输错误）+ 重放
/// 拿到正常响应（204 覆盖）——K67 H2 重放窗的 PUT 腿。
#[tokio::test]
async fn lost_ack_put_effect_landed_response_lost() {
    let knobs = Knobs {
        lost_ack_after_effect: true,
        ..Knobs::default()
    };
    let handle = spawn_stub(Vfs::new(), AuthMode::None, knobs, StubStyle::rclone()).await;
    let result = client()
        .put(url_of(&handle, "/f.txt"))
        .body("first-write")
        .send()
        .await;
    assert!(
        transport_failed(result).await,
        "lost-ACK response must fail in transport"
    );
    // 快照断言（take 会删文件——重放腿需要文件在位走 204 覆盖语义）。
    assert_eq!(
        handle.snapshot().get("/f.txt"),
        Some(&VfsEntry::File {
            bytes: b"first-write".to_vec(),
            mtime: MTIME_WRITE
        }),
        "the effect landed before the connection died"
    );
    // 重放（一次性旋钮已消耗）：文件已存在 → 204 覆盖语义。
    let response = client()
        .put(url_of(&handle, "/f.txt"))
        .body("replay-write")
        .send()
        .await
        .expect("replay put");
    assert_eq!(response.status().as_u16(), 204);
    assert_eq!(
        handle.take("/f.txt").as_deref(),
        Some(b"replay-write".as_slice())
    );
}

/// lost-ACK on MOVE：效果已落 + 响应丢失；重放发现源已不在 → 404
///（K67 H2：重放窗内「源缺失 + 目标就位」= 已提交的判据面）。
#[tokio::test]
async fn lost_ack_move_replay_finds_source_gone() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/a.txt", b"MOVE-CONTENT");
    let knobs = Knobs {
        lost_ack_after_effect: true,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let result = client()
        .request(method("MOVE"), url_of(&handle, "/a.txt"))
        .header("destination", url_of(&handle, "/b.txt"))
        .header("overwrite", "T")
        .send()
        .await;
    assert!(
        transport_failed(result).await,
        "lost-ACK move must fail in transport"
    );
    assert_eq!(
        handle.take("/b.txt").as_deref(),
        Some(b"MOVE-CONTENT".as_slice()),
        "the move landed before the connection died"
    );
    assert!(!handle.exists("/a.txt"));

    // 重放 MOVE：源缺失 → 404（客户端须结合「目标就位 + size 吻合」判
    // 已提交——本断言给出该腿的服务端真值）。
    let response = client()
        .request(method("MOVE"), url_of(&handle, "/a.txt"))
        .header("destination", url_of(&handle, "/b.txt"))
        .header("overwrite", "T")
        .send()
        .await
        .expect("replay move");
    assert_eq!(response.status().as_u16(), 404);
}

/// lost_ack_after_effect_skip：让过前 N 个效果型请求后才断 ACK——
/// stager close 链（PUT .part → MOVE 固化）要把 lost-ACK 打在 MOVE 上
/// 的 WD3 注入面（skip=1：PUT 正常 201，MOVE 效果已落但 ACK 断）。
#[tokio::test]
async fn lost_ack_skip_breaks_the_next_effect_request() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/a.txt", b"MOVE-CONTENT");
    let knobs = Knobs {
        lost_ack_after_effect: true,
        lost_ack_after_effect_skip: 1,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    // 首个效果型请求（PUT）消耗 skip 槽：正常应答、无断连。
    let response = client()
        .put(url_of(&handle, "/part.txt"))
        .body("staged")
        .send()
        .await
        .expect("PUT passes through the skip slot");
    assert_eq!(response.status().as_u16(), 201);
    // 下一个效果型请求（MOVE）断 ACK：效果已落、响应丢失。
    let result = client()
        .request(method("MOVE"), url_of(&handle, "/a.txt"))
        .header("destination", url_of(&handle, "/b.txt"))
        .header("overwrite", "T")
        .send()
        .await;
    assert!(
        transport_failed(result).await,
        "the ACK loss must land on the MOVE, not the PUT"
    );
    assert_eq!(
        handle.take("/b.txt").as_deref(),
        Some(b"MOVE-CONTENT".as_slice()),
        "the move effect landed before the connection died"
    );
}

/// stat_size_delta（一次性）：下一个**文件目标**的 PROPFIND 自条目把
/// getcontentlength 谎报为 真实+delta（负值钳 0）；只影响自条目、只
/// 谎报一次——stager close 的 size 复核注入面。
#[tokio::test]
async fn stat_size_delta_lies_once_on_the_file_selfentry() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", b"0123456789");
    vfs.seed_file("/sub/child.bin", b"xyz");
    let knobs = Knobs {
        stat_size_delta: Some(3),
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs.clone(), AuthMode::None, knobs, StubStyle::rclone()).await;
    let body = propfind(&handle, "/f.bin", "0")
        .await
        .text()
        .await
        .expect("body");
    assert!(
        body.contains("<D:getcontentlength>13</D:getcontentlength>"),
        "the self-entry lies: real 10 + delta 3"
    );
    // 一次性：第二次恢复真值。
    let body = propfind(&handle, "/f.bin", "0")
        .await
        .text()
        .await
        .expect("body");
    assert!(
        body.contains("<D:getcontentlength>10</D:getcontentlength>"),
        "the lie is one-shot"
    );
    // 子条目不受影响（旋钮只作用于自条目）。
    let knobs2 = Knobs {
        stat_size_delta: Some(100),
        ..Knobs::default()
    };
    let handle2 = spawn_stub(vfs, AuthMode::None, knobs2, StubStyle::rclone()).await;
    let body = propfind(&handle2, "/sub", "1")
        .await
        .text()
        .await
        .expect("body");
    assert!(
        body.contains("<D:getcontentlength>3</D:getcontentlength>"),
        "children entries carry the true length"
    );
}

/// transient_5xx_move：前 N 次 MOVE 回 503（与 transient_5xx 独立计数
/// ——PROPFIND/GET 不消耗它，MOVE 也不消耗前者）。
#[tokio::test]
async fn transient_5xx_move_hits_only_moves() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/a.txt", b"x");
    let knobs = Knobs {
        transient_5xx_move: 1,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    // PROPFIND/GET 不消耗 MOVE 计数。
    let response = client()
        .get(url_of(&handle, "/a.txt"))
        .send()
        .await
        .expect("get");
    assert_eq!(response.status().as_u16(), 200);
    // 首个 MOVE 吃 503（效果未落——故障门在路由前）。
    let response = client()
        .request(method("MOVE"), url_of(&handle, "/a.txt"))
        .header("destination", url_of(&handle, "/b.txt"))
        .header("overwrite", "T")
        .send()
        .await
        .expect("move");
    assert_eq!(response.status().as_u16(), 503);
    assert!(handle.exists("/a.txt"), "no effect before the fault gate");
    // 计数耗尽后恢复。
    let response = client()
        .request(method("MOVE"), url_of(&handle, "/a.txt"))
        .header("destination", url_of(&handle, "/b.txt"))
        .header("overwrite", "T")
        .send()
        .await
        .expect("move retry");
    assert_eq!(response.status().as_u16(), 201);
    assert!(handle.exists("/b.txt"));
}

/// slow_drip：分块延迟但不丢字节（窗口读不卡死/超时面的载体）。
#[tokio::test]
async fn slow_drip_delivers_full_body() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/drip.bin", vec![7u8; 10_000].as_slice());
    let knobs = Knobs {
        slow_drip: Some(Duration::from_millis(20)),
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let started = Instant::now();
    let response = client()
        .get(url_of(&handle, "/drip.bin"))
        .send()
        .await
        .expect("drip get");
    assert_eq!(response.status().as_u16(), 200);
    let body = response.bytes().await.expect("drip body");
    assert_eq!(body.len(), 10_000);
    assert!(body.iter().all(|byte| *byte == 7));
    // 3 块 × 20ms ≈ 60ms（宽放 40ms 下限 + 10s 上限防悬挂）。
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(40),
        "drip must delay: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "drip must not hang: {elapsed:?}"
    );
}

/// kill_propfinds：PROPFIND 专属连接杀——效果型写动词（PUT/MOVE）不
/// 消耗计数，PROPFIND 头后 body 即断（H1「MOVE 固化后 stat 复核断连」
/// 注入面的计数纪律）。
#[tokio::test]
async fn kill_propfinds_spares_write_verbs_and_breaks_propfind_bodies() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/a.txt", b"x");
    let knobs = Knobs {
        kill_propfinds: std::sync::atomic::AtomicUsize::new(1),
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    // PUT 不消耗计数：正常 201。
    let response = client()
        .put(url_of(&handle, "/b.txt"))
        .body("y")
        .send()
        .await
        .expect("PUT passes the PROPFIND-only kill");
    assert_eq!(response.status().as_u16(), 201);
    // PROPFIND：send 层即传输错误（body 首块即断）。
    let result = client()
        .request(method("PROPFIND"), url_of(&handle, "/a.txt"))
        .header("depth", "0")
        .send()
        .await;
    assert!(
        transport_failed(result).await,
        "the PROPFIND body must die on the first chunk"
    );
    // 计数耗尽后恢复 207。
    let response = propfind(&handle, "/a.txt", "0").await;
    assert_eq!(response.status().as_u16(), 207);
}

/// digest_challenge_reuse_nonce：并发 challenge 同 nonce 的真形回放
///（M1 注入面）——连续两次无凭据请求的 challenge 携**同一** nonce；
/// 缺省形态（旋钮关）每 challenge 铸新 nonce。
#[tokio::test]
async fn digest_challenge_reuse_nonce_serves_the_same_nonce() {
    let vfs = seeded_tree();
    let knobs = Knobs {
        digest_challenge_reuse_nonce: true,
        ..Knobs::default()
    };
    let handle = spawn_stub(
        vfs,
        digest_auth("webdavtest", Duration::from_secs(3600), false),
        knobs,
        StubStyle::rclone(),
    )
    .await;
    let nonce_of = |www: &str| -> String {
        let position = www.find("nonce=\"").expect("nonce param") + "nonce=\"".len();
        let rest = &www[position..];
        rest[..rest.find('"').expect("closing quote")].to_string()
    };
    let first = client()
        .get(url_of(&handle, "/docs/"))
        .send()
        .await
        .expect("first");
    assert_eq!(first.status().as_u16(), 401);
    let nonce1 = nonce_of(first.headers()["www-authenticate"].to_str().expect("ascii"));
    let second = client()
        .get(url_of(&handle, "/docs/"))
        .send()
        .await
        .expect("second");
    assert_eq!(second.status().as_u16(), 401);
    let nonce2 = nonce_of(
        second.headers()["www-authenticate"]
            .to_str()
            .expect("ascii"),
    );
    assert_eq!(
        nonce1, nonce2,
        "the reused challenge must carry the same nonce"
    );
    handle.shutdown().await;
}

/// served_bytes 计数流（M3 观测面）：无延迟路径（CountingStream）的
/// 文件 GET 全量读完时，流出字节 = body 实长（如实累计）。
#[tokio::test]
async fn served_bytes_counts_streamed_file_bodies() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/a.bin", vec![9u8; 200_000].as_slice());
    let handle = spawn_stub(vfs, AuthMode::None, Knobs::default(), StubStyle::rclone()).await;
    let response = client()
        .get(url_of(&handle, "/a.bin"))
        .send()
        .await
        .expect("get");
    assert_eq!(response.status().as_u16(), 200);
    let body = response.bytes().await.expect("body");
    assert_eq!(body.len(), 200_000);
    let served = handle
        .knobs
        .served_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(served, 200_000, "the counter tracks the delivered body");
    handle.shutdown().await;
}

/// combined_basic_digest_challenge：单头并置 challenge（M6 注入面）——
/// 无凭据 401 的 WWW-Authenticate 头为 `Basic realm="stub", Digest ...`
/// 并置单形，Digest 段可独立解析出 nonce。
#[tokio::test]
async fn combined_challenge_header_carries_basic_and_digest_in_one_value() {
    let vfs = seeded_tree();
    let knobs = Knobs {
        combined_basic_digest_challenge: true,
        ..Knobs::default()
    };
    let handle = spawn_stub(
        vfs,
        digest_auth("webdavtest", Duration::from_secs(3600), false),
        knobs,
        StubStyle::rclone(),
    )
    .await;
    let response = client()
        .get(url_of(&handle, "/docs/"))
        .send()
        .await
        .expect("get");
    assert_eq!(response.status().as_u16(), 401);
    let www = response.headers()["www-authenticate"]
        .to_str()
        .expect("ascii header")
        .to_string();
    assert!(
        www.starts_with(r#"Basic realm="stub", "#) && www.contains("Digest realm="),
        "one header value carrying both schemes: {www}"
    );
    // Digest 段携带 nonce（可被逐段解析消费）。
    let digest_segment = www
        .split(r#"Basic realm="stub", "#)
        .nth(1)
        .expect("digest segment");
    assert!(digest_segment.contains("nonce=\""), "{digest_segment}");
    handle.shutdown().await;
}

/// member_500_once（M7 注入面，一次性）：下一个 PROPFIND 的文件自条
/// 目改为「仅内层 500 块」的失败成员；第二次恢复 200 真形。
#[tokio::test]
async fn member_500_once_yields_a_500_only_propstat() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/a.txt", b"x");
    let knobs = Knobs {
        member_500_once: true,
        ..Knobs::default()
    };
    let handle = spawn_stub(vfs, AuthMode::None, knobs, StubStyle::rclone()).await;
    let body = propfind(&handle, "/a.txt", "0")
        .await
        .text()
        .await
        .expect("body");
    assert!(
        body.contains("HTTP/1.1 500"),
        "the member is a 500-only failure: {body}"
    );
    assert!(
        !body.contains("200 OK"),
        "no authoritative block may survive: {body}"
    );
    // 一次性：恢复 200 真形。
    let body = propfind(&handle, "/a.txt", "0")
        .await
        .text()
        .await
        .expect("body");
    assert!(body.contains("200 OK"), "{body}");
    handle.shutdown().await;
}

// -------------------------------------------------------------- 记录器 ---

/// 打码形态（scheme + 前 8 字符）、body_len、Digest nc/nonce 提取。
#[tokio::test]
async fn request_recorder_masks_auth_and_extracts_digest_fields() {
    let mut vfs = Vfs::new();
    vfs.seed_dir("/docs");
    let handle = spawn_stub(
        vfs,
        AuthMode::Basic {
            user: "spike".to_string(),
            pass: "pw".to_string(),
        },
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    // base64("spike:pw") = "c3Bpa2U6cHc=" → 打码 "Basic c3Bpa2U6…"。
    let _ = propfind(&handle, "/docs/", "0").await; // 401（无凭据）
    let response = client()
        .request(method("PROPFIND"), url_of(&handle, "/docs/"))
        .header("depth", "0")
        .basic_auth("spike", Some("pw"))
        .body(ALLPROP_PROPFIND)
        .send()
        .await
        .expect("authed");
    assert_eq!(response.status().as_u16(), 207);
    let response = client()
        .put(url_of(&handle, "/docs/f.bin"))
        .body(vec![0u8; 10])
        .basic_auth("spike", Some("pw"))
        .send()
        .await
        .expect("put");
    assert_eq!(response.status().as_u16(), 201);

    let requests = handle.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].method, "PROPFIND");
    assert_eq!(
        requests[0].auth, None,
        "unauthenticated request records no auth"
    );
    assert_eq!(requests[1].method, "PROPFIND");
    assert_eq!(requests[1].auth.as_deref(), Some("Basic c3Bpa2U6…"));
    assert_eq!(requests[1].digest_nc, None, "basic requests carry no nc");
    assert_eq!(requests[2].method, "PUT");
    assert_eq!(requests[2].path, "/docs/f.bin");
    assert_eq!(requests[2].body_len, 10);
    // Digest nc/nonce 提取面在 digest_roundtrip 用例已钉（[1,1,2] 序列）。
}

// --------------------------------------------------------------- 停机 ---

/// shutdown 后监听停止（新连接失败）。
#[tokio::test]
async fn shutdown_stops_the_listener() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.txt", b"x");
    let handle = spawn_stub(vfs, AuthMode::None, Knobs::default(), StubStyle::rclone()).await;
    let response = client()
        .get(url_of(&handle, "/f.txt"))
        .send()
        .await
        .expect("pre-shutdown get");
    assert_eq!(response.status().as_u16(), 200);
    let url = handle.url.clone();
    handle.shutdown().await;
    let result = client().get(&url).send().await;
    assert!(result.is_err(), "listener must be gone after shutdown");
}
