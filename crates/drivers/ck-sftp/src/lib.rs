//! # ck-sftp——SSH/SFTP 存储驱动（L1 驱动 crate，Phase 4 / SF1）。
//!
//! 后端 = 一台 SSH 服务器上的一个根目录（`sftp_root`，缺省 `/`）。
//! 卷身份 `sftp:<user>@<host>:<port>`（D6：同服务器同账号同卷）。
//! 协议栈 russh 0.63 + russh-sftp 3.0（ring 后端，零新增 C 依赖——
//! 计划 §1 选型；版本下限 0.63 = GHSA-47hw-gvq5-r2gm 客户端侧
//! High 修复所在线）。
//!
//! SF0 拍板（计划 §8）：**D1** 认证 = 密码 + 私钥（含 passphrase；
//! keyboard-interactive/ssh-agent 不做）；**D2** host key = 显式接受 +
//! 指纹落盘（配置无指纹 → 拒连 + 可行动文案；不匹配恒拒；绝不无条
//! 件接受）；**D3** 起步单连接（惰性建立，会话内 3.0 流水线并发）。
//!
//! 驱动形态：StorageDriver 九方法（能力位四真六假，逐位依据见
//! driver.rs `capabilities()`）+ [`SftpTransport`] CloudTransport 薄壳
//!（K2 path 寻址 / K6 0 占位，照 ck-local 双面结构）。
//!
//! **批次边界（SF1）**：网络路径（连接/认证/host key/读写）以编译 +
//! clippy 为门，行为测试在 SF2 进程内桩批补；纯函数层（错误映射 /
//! 配置解析 / 能力位 / 路径拼接）本批 TDD 红→绿钉死。
//!
//! 层位置：只依赖 cloudkit-storage（L2）与外部 crate（
//! driver-onboarding §1）；禁依赖 cloudkit-core 及任何 L3+ crate（R1）。

mod client;
mod config;
mod driver;
pub(crate) mod error;
mod transport_face;

use std::sync::Arc;

use cloudkit_storage::StorageError;

pub use config::{SftpParams, DEFAULT_PORT, DEFAULT_ROOT};
pub use driver::{SftpDriver, SftpStager};
pub use transport_face::SftpTransport;

/// 装配工厂：构造驱动（**不连接**——惰性建立是 D3 的既定形态；连接
/// 时机 = 首次操作或 transport 面 `connect` 探活）。
///
/// 阻塞面：无（`SftpDriver::new` 纯构造）。
pub async fn factory(cfg: &SftpParams) -> Result<Arc<SftpDriver>, StorageError> {
    Ok(Arc::new(SftpDriver::new(cfg.clone())?))
}

/// 探连接的结构化结果（doctor 腿；baidu `BackendProbe` 同款形态）。
///
/// D2 的**非交互接受途径**就落在这里：未设指纹时的 `HostKeyUnpinned`
/// 携带服务器实际指纹——doctor 把它渲染成可行动检查项（「把该指纹
/// 复制进卷配置的 sftp_host_fingerprint 键」），用户不需要交互式弹窗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SftpProbe {
    /// 连接 + 认证 + 指纹校验全过（指纹已 pin 且匹配）。
    Alive,
    /// D2-甲：配置未记录指纹——拒连，`actual` 是服务器实际指纹
    /// （复制进 `sftp_host_fingerprint` 即完成显式接受）。
    HostKeyUnpinned(String),
    /// D2：指纹不匹配（MITM 信号）——恒拒；救济 = 显式移除旧指纹。
    HostKeyChanged {
        /// 配置里 pin 的指纹。
        expected: String,
        /// 服务器本次presented的指纹。
        actual: String,
    },
    /// D1 认证失败（密码/私钥均拒或材料不可读）；载荷是不含凭据值的
    /// 诊断文案（R3）。
    AuthFailed(String),
    /// 网络/协议层不可达（超时、拒绝、KEX 失败等）。
    Unreachable(String),
}

/// 探连接（doctor 腿的直连探针）：建立一次完整连接（TCP/KEX/指纹/
/// 认证/SFTP 握手）后立即断开，把 [`crate::client::SessionError`] 的
/// 细节归一为 [`SftpProbe`]——`Availability` 之外的关键信息（实际
/// 指纹）在这里以结构化形态浮到 doctor，而不是只进日志。
pub async fn probe(params: &SftpParams) -> SftpProbe {
    match client::probe_connect(params).await {
        Ok(()) => SftpProbe::Alive,
        Err(error) => match error {
            crate::error::SessionError::HostKeyUnpinned { actual } => {
                SftpProbe::HostKeyUnpinned(actual)
            }
            crate::error::SessionError::HostKeyMismatch { expected, actual } => {
                SftpProbe::HostKeyChanged { expected, actual }
            }
            crate::error::SessionError::AuthFailed { detail, .. } => SftpProbe::AuthFailed(detail),
            crate::error::SessionError::Ssh(transport) => SftpProbe::Unreachable(format!(
                "ssh transport to {}:{} failed: {transport}",
                params.host, params.port
            )),
            crate::error::SessionError::Sftp(sftp) => {
                SftpProbe::Unreachable(format!("sftp subsystem failed: {sftp}"))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloudkit_storage::{RelPath, VolumeId};

    // ----------------------------------------------------- config.rs ---

    fn pair(key: &str, value: &str) -> (String, String) {
        (key.to_string(), value.to_string())
    }

    fn base_pairs() -> Vec<(String, String)> {
        vec![
            pair("sftp_host", "nas.lan"),
            pair("sftp_username", "cloudfs"),
            pair("sftp_password", "pw"),
        ]
    }

    #[test]
    fn from_pairs_resolves_all_eight_keys_with_defaults() {
        let pairs = vec![
            pair("sftp_host", "nas.lan"),
            pair("sftp_port", "2222"),
            pair("sftp_username", "cloudfs"),
            pair("sftp_password", "pw"),
            pair("sftp_private_key_path", "C:/keys/id_ed25519"),
            pair("sftp_private_key_passphrase", "phrase"),
            pair(
                "sftp_host_fingerprint",
                "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            ),
            pair("sftp_root", "/srv/cloudfs"),
        ];
        let params = SftpParams::from_pairs(&pairs).expect("complete pairs parse");
        assert_eq!(params.host, "nas.lan");
        assert_eq!(params.port, 2222);
        assert_eq!(params.username, "cloudfs");
        assert_eq!(params.password.as_deref(), Some("pw"));
        assert_eq!(
            params.private_key_path,
            Some(std::path::PathBuf::from("C:/keys/id_ed25519"))
        );
        assert_eq!(params.private_key_passphrase.as_deref(), Some("phrase"));
        assert_eq!(
            params.host_fingerprint.as_deref(),
            Some("SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        );
        assert_eq!(params.root, "/srv/cloudfs");
    }

    #[test]
    fn from_pairs_applies_port_and_root_defaults() {
        let params = SftpParams::from_pairs(&base_pairs()).expect("minimal pairs parse");
        assert_eq!(params.port, DEFAULT_PORT, "port defaults to 22");
        assert_eq!(params.root, DEFAULT_ROOT, "root defaults to /");
        assert_eq!(params.host_fingerprint, None, "fingerprint optional");
    }

    #[test]
    fn from_pairs_requires_host_and_username() {
        for (name, broken) in [
            (
                "sftp_host",
                vec![pair("sftp_username", "u"), pair("sftp_password", "p")],
            ),
            (
                "sftp_username",
                vec![pair("sftp_host", "h"), pair("sftp_password", "p")],
            ),
        ] {
            assert!(
                matches!(SftpParams::from_pairs(&broken), Err(StorageError::Invalid)),
                "missing {name} must be rejected with Invalid"
            );
        }
        // 空串 = 未设置（empty-means-unset）
        let mut empty_host = base_pairs();
        empty_host[0].1 = String::new();
        assert!(matches!(
            SftpParams::from_pairs(&empty_host),
            Err(StorageError::Invalid)
        ));
    }

    #[test]
    fn from_pairs_requires_password_or_private_key() {
        let pairs = vec![pair("sftp_host", "h"), pair("sftp_username", "u")];
        assert!(
            matches!(SftpParams::from_pairs(&pairs), Err(StorageError::Invalid)),
            "no auth credential must be rejected with Invalid"
        );

        // key-only 形态通过；空串 key_path 不算
        let key_only = vec![
            pair("sftp_host", "h"),
            pair("sftp_username", "u"),
            pair("sftp_private_key_path", "/keys/id_ed25519"),
        ];
        SftpParams::from_pairs(&key_only).expect("key-only auth parses");
        let empty_key = vec![
            pair("sftp_host", "h"),
            pair("sftp_username", "u"),
            pair("sftp_private_key_path", ""),
        ];
        assert!(SftpParams::from_pairs(&empty_key).is_err());
    }

    #[test]
    fn from_pairs_rejects_bad_port_and_relative_root() {
        let mut bad_port = base_pairs();
        bad_port.push(pair("sftp_port", "0"));
        assert!(SftpParams::from_pairs(&bad_port).is_err(), "port 0");

        let mut not_a_port = base_pairs();
        not_a_port.push(pair("sftp_port", "not-a-number"));
        assert!(SftpParams::from_pairs(&not_a_port).is_err(), "port NaN");

        let mut relative_root = base_pairs();
        relative_root.push(pair("sftp_root", "srv/data"));
        assert!(
            SftpParams::from_pairs(&relative_root).is_err(),
            "relative root"
        );
    }

    #[test]
    fn from_pairs_rejects_unknown_keys() {
        // 拼写错误在装配期暴露（sftp_pasword ≠ sftp_password）
        let mut typo = base_pairs();
        typo.push(pair("sftp_pasword", "x"));
        assert!(
            matches!(SftpParams::from_pairs(&typo), Err(StorageError::Invalid)),
            "a typoed key must be rejected, not silently dropped"
        );
    }

    // ----------------------------------------------------- driver.rs ---

    use crate::driver::remote_path;
    use cloudkit_storage::StorageDriver;

    fn skeleton_params() -> SftpParams {
        SftpParams::from_pairs(&base_pairs()).expect("params")
    }

    #[test]
    fn remote_path_joins_root_and_rel() {
        let root = "/srv/data";
        assert_eq!(remote_path(root, &RelPath::root()), "/srv/data");
        assert_eq!(
            remote_path(root, &RelPath::new("a/b.txt").expect("rel")),
            "/srv/data/a/b.txt"
        );
        // 尾斜杠折叠 + 服务器根
        assert_eq!(
            remote_path("/srv/data/", &RelPath::new("a").expect("rel")),
            "/srv/data/a"
        );
        assert_eq!(remote_path("/", &RelPath::new("a").expect("rel")), "/a");
        assert_eq!(remote_path("/", &RelPath::root()), "/");
    }

    #[test]
    fn driver_volume_identity_and_no_connect_on_construct() {
        // 构造不碰网络（D3 惰性）——本测试能跑过就是证明：无服务器可连。
        let params = skeleton_params();
        let driver = SftpDriver::new(params).expect("constructs without connecting");
        let volume = VolumeId::new("sftp", "cloudfs@nas.lan:22").expect("volume");
        assert_eq!(*driver.volume(), volume);
    }

    #[test]
    fn capabilities_declare_the_planned_bits() {
        // 计划 §4.2 逐位：range_read / server_side_move /
        // authoritative_index / remote_delete 真；其余假。
        let driver = SftpDriver::new(skeleton_params()).expect("driver");
        let caps = driver.capabilities();
        assert!(caps.range_read);
        assert!(caps.server_side_move);
        assert!(caps.authoritative_index);
        assert!(caps.remote_delete);
        assert!(!caps.resume);
        assert!(!caps.multipart);
        assert!(!caps.rapid_upload);
        assert!(!caps.change_feed);
        assert!(!caps.inbound);
        assert!(!caps.chat);
    }

    // ----------------------------------------------------- error.rs ---

    use crate::error::{
        looks_like_connection_loss, map_session_error, map_sftp_error, map_status,
        normalize_fingerprint, SessionError,
    };
    use russh_sftp::client::error::Error as SftpClientError;
    use russh_sftp::protocol::{Status, StatusCode};

    fn status(code: StatusCode, message: &str) -> Status {
        Status {
            id: 1,
            status_code: code,
            error_message: message.to_string(),
            language_tag: "en".to_string(),
        }
    }

    #[test]
    fn status_mapping_follows_the_plan_table() {
        // NoSuchFile → NotFound
        assert!(matches!(
            map_status(&status(StatusCode::NoSuchFile, "no such file")),
            StorageError::NotFound
        ));
        // PermissionDenied → Unauthorized{false}——绝不折叠为 NotFound
        assert!(matches!(
            map_status(&status(StatusCode::PermissionDenied, "denied")),
            StorageError::Unauthorized { recoverable: false }
        ));
        // NoConnection / ConnectionLost → Unavailable——绝不折叠为 NotFound
        assert!(matches!(
            map_status(&status(StatusCode::ConnectionLost, "lost")),
            StorageError::Unavailable(_)
        ));
        assert!(matches!(
            map_status(&status(StatusCode::NoConnection, "down")),
            StorageError::Unavailable(_)
        ));
        // Failure（等上下文类）→ Io 保留原始码与消息
        let mapped = map_status(&status(StatusCode::Failure, "generic failure"));
        match mapped {
            StorageError::Io(detail) => {
                assert!(
                    detail.contains("generic failure"),
                    "keeps the message: {detail}"
                );
            }
            other => panic!("Failure must map to Io, got {other:?}"),
        }
    }

    #[test]
    fn sftp_client_error_mapping_spans_all_variants() {
        // Timeout → Unavailable
        assert!(matches!(
            map_sftp_error(SftpClientError::Timeout),
            StorageError::Unavailable(_)
        ));
        // 字符串 IO：连接死信 → Unavailable；否则 Io（保留消息）
        assert!(matches!(
            map_sftp_error(SftpClientError::IO("connection reset by peer".to_string())),
            StorageError::Unavailable(_)
        ));
        assert!(matches!(
            map_sftp_error(SftpClientError::IO("disk on fire".to_string())),
            StorageError::Io(_)
        ));
        // Limited（limits@openssh）→ RateLimited{None}（无后端时长）
        assert!(matches!(
            map_sftp_error(SftpClientError::Limited("handle limit reached".to_string())),
            StorageError::RateLimited { retry_after: None }
        ));
        // UnexpectedPacket / UnexpectedBehavior → Io；会话死信号例外
        //（"sender dropped" = 在途请求的回复通道随连接死亡消失 →
        // Unavailable——SF2 桩断连测试的修复面）
        assert!(matches!(
            map_sftp_error(SftpClientError::UnexpectedPacket),
            StorageError::Io(_)
        ));
        assert!(matches!(
            map_sftp_error(SftpClientError::UnexpectedBehavior(
                "odd server".to_string()
            )),
            StorageError::Io(_)
        ));
        assert!(matches!(
            map_sftp_error(SftpClientError::UnexpectedBehavior(
                "sender dropped".to_string()
            )),
            StorageError::Unavailable(_)
        ));
        // Status 透传 map_status
        assert!(matches!(
            map_sftp_error(SftpClientError::Status(status(
                StatusCode::NoSuchFile,
                "gone"
            ))),
            StorageError::NotFound
        ));
    }

    #[test]
    fn connection_loss_marker_list_is_the_single_source() {
        assert!(looks_like_connection_loss("write failed: broken pipe"));
        assert!(looks_like_connection_loss("Connection Closed"));
        // 栈内实测的会话死信号（SF2 桩断连钉死）
        assert!(looks_like_connection_loss("sender dropped"));
        assert!(looks_like_connection_loss("channel closed"));
        assert!(!looks_like_connection_loss("disk full"));
        assert!(!looks_like_connection_loss(""));
    }

    #[test]
    fn fingerprint_normalization_tolerates_paste_shapes() {
        assert_eq!(normalize_fingerprint("SHA256:AbC123"), "SHA256:ABC123");
        assert_eq!(normalize_fingerprint("sha256:abc123"), "SHA256:ABC123");
        assert_eq!(normalize_fingerprint("abc123"), "SHA256:ABC123");
        assert_eq!(normalize_fingerprint("  SHA256:abc123 \n"), "SHA256:ABC123");
        // 指纹本体逐字符比较（归一化只统一写法）
        assert_ne!(
            normalize_fingerprint("SHA256:AAAA"),
            normalize_fingerprint("SHA256:AAAB")
        );
    }

    #[test]
    fn session_error_mapping_routes_host_key_and_auth_to_unauthorized() {
        // host key 未接受 → Unauthorized{false}，文案带实际指纹与接受途径
        let mapped = map_session_error(SessionError::HostKeyUnpinned {
            actual: "SHA256:XYZ".to_string(),
        });
        match mapped {
            StorageError::Unauthorized { recoverable: false } => {}
            other => panic!("HostKeyUnpinned must be Unauthorized{{false}}, got {other:?}"),
        }
        // 指纹不匹配 → 同上（恒拒）
        let mapped = map_session_error(SessionError::HostKeyMismatch {
            expected: "SHA256:OLD".to_string(),
            actual: "SHA256:NEW".to_string(),
        });
        assert!(matches!(
            mapped,
            StorageError::Unauthorized { recoverable: false }
        ));
        // 认证失败 → Unauthorized{false}
        let mapped = map_session_error(SessionError::AuthFailed {
            user: "u".to_string(),
            detail: "rejected".to_string(),
        });
        assert!(matches!(
            mapped,
            StorageError::Unauthorized { recoverable: false }
        ));
    }
}
