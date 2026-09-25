//! SF2 行为测试 · 连接/认证/host key 往返（计划 §5 SF2 验收 + §8 D1/D2/D3）。
//!
//! 全部 hermetic：对 [`stub::Stub`]（127.0.0.1 随机端口的进程内 SSH/SFTP
//! 服务端）往返，无真实网络。
//!
//! 覆盖面：
//! - **D2 三态**：未设指纹 → `Unauthorized{false}`（HostKeyUnpinned）；
//!   错指纹 → `Unauthorized{false}`（HostKeyMismatch，恒拒）；对指纹 →
//!   连接成功；
//! - **D1 认证**：密码正确通过；密码错/用户名错 → `Unauthorized{false}`；
//!   私钥形态（加密 PEM + passphrase 经 tempfile）通过；私钥与密码均错
//!   → `Unauthorized{false}`（AuthFailed）；
//! - **D3 单连接**：多次操作只建立一条 SSH 连接（连接计数观测）；
//! - **卷根校验门（复审 2026-09-25）**：connect = 卷根存在且是目录——
//!   不存在/不是目录以可行动错误拒绝（指名 sftp_root 与路径），装配期
//!   connect 门据此拒绝挂载（负责人裁定：不存在就别带病挂载）。

mod stub;

use std::sync::Arc;

use ck_sftp::{SftpDriver, SftpParams, SftpTransport};
use cloudkit_storage::transport::CloudTransport;
use cloudkit_storage::{RelPath, StorageDriver, StorageError};
use stub::{Stub, StubAuth};

/// 测试专用常量凭据（桩侧校验用，非真实凭据——R3）。
const USER: &str = "tester";
const PASSWORD: &str = "stub-only-password";

async fn stub_with_password() -> Stub {
    Stub::start(StubAuth::password(USER, PASSWORD)).await
}

fn pinned_params(stub: &Stub) -> SftpParams {
    let mut pairs = stub.param_pairs();
    pairs.push((
        "sftp_host_fingerprint".to_string(),
        stub.fingerprint().to_string(),
    ));
    SftpParams::from_pairs(&pairs).expect("params parse")
}

/// 首个操作即触发惰性建连（D3：factory 不连接）。
async fn touch(params: &SftpParams) -> Result<(), StorageError> {
    let driver = SftpDriver::new(params.clone()).expect("driver constructs");
    driver.stat(&RelPath::root()).await.map(|_| ())
}

// ------------------------------------------------------------------ D2 ---

/// D2-甲「未接受」：配置无指纹 → 拒连（绝不 TOFU、绝不无条件接受），
/// 错误分类 `Unauthorized { recoverable: false }`。
#[tokio::test]
async fn unpinned_host_key_is_rejected() {
    let stub = stub_with_password().await;
    let params = SftpParams::from_pairs(&stub.param_pairs()).expect("params");
    match touch(&params).await {
        Err(StorageError::Unauthorized { recoverable: false }) => {}
        other => panic!("unpinned host key must be Unauthorized{{false}}, got {other:?}"),
    }
    // 可行动文案经 warn 日志通道（SessionError Display 已在 lib.rs 单测
    // 钉死）；连接从未建立——auth_success_count 保持 0。
    assert_eq!(
        stub.auth_success_count(),
        0,
        "host key rejection precedes auth"
    );
}

/// D2「恒拒」：指纹不匹配（MITM 信号）→ `Unauthorized{false}`。
#[tokio::test]
async fn wrong_fingerprint_is_rejected() {
    let stub = stub_with_password().await;
    let mut pairs = stub.param_pairs();
    // 同形态、不同本体（归一化后必然不等——normalize 只统一写法）
    pairs.push((
        "sftp_host_fingerprint".to_string(),
        "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
    ));
    let params = SftpParams::from_pairs(&pairs).expect("params");
    match touch(&params).await {
        Err(StorageError::Unauthorized { recoverable: false }) => {}
        other => panic!("wrong fingerprint must be Unauthorized{{false}}, got {other:?}"),
    }
    assert_eq!(stub.auth_success_count(), 0);
}

/// D2「匹配静默通过」：对指纹 → 连接成功（首个操作 OK）。
#[tokio::test]
async fn matching_fingerprint_connects() {
    let stub = stub_with_password().await;
    let params = pinned_params(&stub);
    touch(&params).await.expect("correct fingerprint connects");
}

// ------------------------------------------------------------------ D1 ---

/// D1 密码形态：正确密码通过（桩 auth_password 校验用户名 + 密码）。
#[tokio::test]
async fn correct_password_authenticates() {
    let stub = stub_with_password().await;
    let params = pinned_params(&stub);
    touch(&params).await.expect("password auth connects");
    assert_eq!(stub.auth_success_count(), 1);
}

/// D1 密码错误 → AuthFailed → `Unauthorized{false}`。
#[tokio::test]
async fn wrong_password_is_rejected() {
    let stub = stub_with_password().await;
    let mut pairs = stub.param_pairs();
    pairs.push(("sftp_password".to_string(), "definitely-wrong".to_string()));
    pairs.push((
        "sftp_host_fingerprint".to_string(),
        stub.fingerprint().to_string(),
    ));
    let params = SftpParams::from_pairs(&pairs).expect("params");
    match touch(&params).await {
        Err(StorageError::Unauthorized { recoverable: false }) => {}
        other => panic!("wrong password must be Unauthorized{{false}}, got {other:?}"),
    }
}

/// D1 用户名错误（密码对）→ 桩的用户名校验同样拒绝。
#[tokio::test]
async fn wrong_username_is_rejected() {
    let stub = stub_with_password().await;
    let mut pairs = stub.param_pairs();
    pairs.push(("sftp_username".to_string(), "intruder".to_string()));
    pairs.push((
        "sftp_host_fingerprint".to_string(),
        stub.fingerprint().to_string(),
    ));
    let params = SftpParams::from_pairs(&pairs).expect("params");
    match touch(&params).await {
        Err(StorageError::Unauthorized { recoverable: false }) => {}
        other => panic!("wrong username must be Unauthorized{{false}}, got {other:?}"),
    }
}

/// D1 私钥形态（自动化主形态）：临时生成 Ed25519 密钥 → 加密 PKCS8 PEM
/// 落 tempfile → passphrase 解锁 → authenticate_publickey 通过。
/// 驱动语义：key-only（无 sftp_password 键）。
#[tokio::test]
async fn private_key_with_passphrase_authenticates() {
    use russh::keys::{Algorithm, PrivateKey};

    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("gen key");
    let dir = tempfile::tempdir().expect("tempdir");
    let key_path = dir.path().join("id_ed25519");
    let passphrase = b"unlock-phrase";
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&key_path).expect("create key file");
        russh::keys::encode_pkcs8_pem_encrypted(&key, passphrase, 16, &mut file)
            .expect("encode encrypted pem");
        file.flush().expect("flush");
    }

    // 桩只授权这把公钥（无密码——证明私钥面独立成立）
    let auth = StubAuth::password(USER, PASSWORD).with_public_key(key.public_key());
    let stub = Stub::start(auth).await;
    let pairs = vec![
        ("sftp_host".to_string(), "127.0.0.1".to_string()),
        ("sftp_port".to_string(), stub.addr().1.to_string()),
        ("sftp_username".to_string(), USER.to_string()),
        (
            "sftp_private_key_path".to_string(),
            key_path.to_string_lossy().to_string(),
        ),
        (
            "sftp_private_key_passphrase".to_string(),
            String::from_utf8_lossy(passphrase).to_string(),
        ),
        (
            "sftp_host_fingerprint".to_string(),
            stub.fingerprint().to_string(),
        ),
    ];
    let params = SftpParams::from_pairs(&pairs).expect("params");
    touch(&params).await.expect("public key auth connects");
}

/// D1「两者皆错」：未授权私钥 + 错密码 → AuthFailed → `Unauthorized{false}`
///（authenticate 的私钥优先、密码兜底、均败归一）。
#[tokio::test]
async fn unauthorized_key_and_wrong_password_yield_unauthorized() {
    use russh::keys::{Algorithm, PrivateKey};

    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("gen key");
    let dir = tempfile::tempdir().expect("tempdir");
    let key_path = dir.path().join("id_ed25519");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&key_path).expect("create key file");
        russh::keys::encode_pkcs8_pem(&key, &mut file).expect("encode pem");
        file.flush().expect("flush");
    }

    // 桩授权**另一把**钥匙；密码形态也启用但驱动给错密码
    let decoy = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("gen decoy");
    let auth = StubAuth::password(USER, PASSWORD).with_public_key(decoy.public_key());
    let stub = Stub::start(auth).await;
    let pairs = vec![
        ("sftp_host".to_string(), "127.0.0.1".to_string()),
        ("sftp_port".to_string(), stub.addr().1.to_string()),
        ("sftp_username".to_string(), USER.to_string()),
        ("sftp_password".to_string(), "wrong-password".to_string()),
        (
            "sftp_private_key_path".to_string(),
            key_path.to_string_lossy().to_string(),
        ),
        (
            "sftp_host_fingerprint".to_string(),
            stub.fingerprint().to_string(),
        ),
    ];
    let params = SftpParams::from_pairs(&pairs).expect("params");
    match touch(&params).await {
        Err(StorageError::Unauthorized { recoverable: false }) => {}
        other => panic!("both auth methods failing must be Unauthorized{{false}}, got {other:?}"),
    }
}

// ------------------------------------------------------------------ D3 ---

/// D3 起步单连接：构造不连接（惰性）+ 连续多次操作复用同一条 SSH 连接
///（连接计数 = 1）。
#[tokio::test]
async fn sequential_operations_reuse_one_connection() {
    let stub = stub_with_password().await;
    let params = pinned_params(&stub);
    let driver = SftpDriver::new(params).expect("driver");
    assert_eq!(stub.connection_count(), 0, "construction does not connect");

    for i in 0..5 {
        driver
            .stat(&RelPath::root())
            .await
            .unwrap_or_else(|e| panic!("stat #{i}: {e:?}"));
    }
    assert_eq!(
        stub.connection_count(),
        1,
        "five sequential ops must reuse the single lazy connection"
    );
}

// ------------------------------------------------- 卷根校验门（复审）---

/// 复审修复（2026-09-25，负责人真机报障裁定「不存在就别带病挂载」）：
/// connect = 卷根校验——装配期的 connect 门据此拒绝挂载。根不存在必须
/// 以**可行动**错误拒绝（指名 sftp_root 键与路径），绝不裸 NotFound
///（报障形态：挂载成功后每个上传 not found 重试到 degrade）。
fn rooted_pairs(stub: &Stub, root: &str) -> Vec<(String, String)> {
    let mut pairs = stub.param_pairs();
    pairs.push((
        "sftp_host_fingerprint".to_string(),
        stub.fingerprint().to_string(),
    ));
    pairs.push(("sftp_root".to_string(), root.to_string()));
    pairs
}

fn transport_for(pairs: &[(String, String)]) -> SftpTransport {
    let params = SftpParams::from_pairs(pairs).expect("params parse");
    SftpTransport::new(Arc::new(
        SftpDriver::new(params).expect("driver constructs"),
    ))
}

#[tokio::test]
async fn connect_rejects_a_missing_volume_root_with_actionable_text() {
    let stub = stub_with_password().await;
    let transport = transport_for(&rooted_pairs(&stub, "/no-such-volume-root"));
    match transport.connect().await {
        Err(StorageError::Unavailable(msg)) => {
            assert!(
                msg.contains("/no-such-volume-root") && msg.contains("sftp_root"),
                "the refusal names the root path and the config key: {msg}"
            );
        }
        other => panic!("missing volume root must be an actionable Unavailable, got {other:?}"),
    }
    // 认证已成功——失败发生在根校验，不是连不上（区分诊断面）。
    assert_eq!(stub.auth_success_count(), 1);
}

#[tokio::test]
async fn connect_rejects_a_file_volume_root() {
    let stub = stub_with_password().await;
    stub.add_file("/plain-file", b"x");
    let transport = transport_for(&rooted_pairs(&stub, "/plain-file"));
    match transport.connect().await {
        Err(StorageError::Unavailable(msg)) => {
            assert!(
                msg.contains("not a directory") && msg.contains("/plain-file"),
                "the refusal names the shape and the path: {msg}"
            );
        }
        other => panic!("a file volume root must be refused, got {other:?}"),
    }
    assert_eq!(stub.auth_success_count(), 1);
}

#[tokio::test]
async fn connect_accepts_a_directory_volume_root() {
    let stub = stub_with_password().await;
    stub.add_dir("/volume-root");
    let transport = transport_for(&rooted_pairs(&stub, "/volume-root"));
    transport
        .connect()
        .await
        .unwrap_or_else(|e| panic!("a directory volume root must connect: {e:?}"));
    assert!(stub.auth_success_count() >= 1);
}
