//! SFTP 驱动参数与配置解析（Phase 4 / SF1）。
//!
//! driver-onboarding §4 的形态：「配置 map → 驱动参数结构体」纯函数
//! 组织——组合根（SF3 的 dispatch 装配）把 config.toml / 卷文件的
//! `sftp_*` 键展平为 `(String, String)` 对传进 [`SftpParams::from_pairs`]
//!，卷文件与 config.toml 走同一解析面（与 cloudkit-core 的严格键校验
//! 前后相继：core 的 KNOWN_TOML_KEYS 挡未知键，本函数在驱动侧再挡
//! 拼写错误的 sftp 键与非法值——早失败、错误可行动）。
//!
//! 错误语义：一切解析/校验失败 → [`StorageError::Invalid`]，文案给出
//! 键名与出路（与 core `validate()` 的 Sftp 分支同一套规则——两道门
//! 是刻意的冗余：core 面向 boot 期，驱动面面向 ADD/工厂期）。

use std::path::PathBuf;

use cloudkit_storage::StorageError;

/// 配置解析失败的归一出口：[`StorageError::Invalid`] 是 L2 冻结的
/// 单元变体（无载荷），可行动文案经 warn 日志到达——
/// [`crate::error::map_session_error`] 的同款双通道裁决。驱动面
/// `from_pairs` 是 core `validate()` 之后的第二道门：常规路径用户
/// 先看到 core 的带文案 `ConfigError`，这里的日志是两道门漂移时的
/// 诊断线。
fn invalid_config(detail: impl std::fmt::Display) -> StorageError {
    tracing::warn!(target: "ck_sftp::config", "sftp params rejected: {detail}");
    StorageError::Invalid
}

/// SSH 缺省端口（`sftp_port` 未设时的值；OpenSSH 常量）。
pub const DEFAULT_PORT: u16 = 22;

/// 卷根缺省（`sftp_root` 未设时的值 = 服务器文件系统根）。
pub const DEFAULT_ROOT: &str = "/";

/// SFTP 驱动参数（D1/D2/D3 拍板结果的载体）。
///
/// 不实现 `Debug`：结构体携带凭据（`password` / `private_key_passphrase`），
/// 派生展开有把凭据印进日志的风险（R3——ck-baidu `BaiduParams` 同款
/// 裁决；错误诊断走脱敏路径，不 dump 参数结构体）。
#[derive(Clone)]
pub struct SftpParams {
    /// SSH 服务器地址（hostname 或 IP）。
    pub host: String,
    /// SSH 端口（缺省 [`DEFAULT_PORT`]）。
    pub port: u16,
    /// 登录用户名。
    pub username: String,
    /// 密码凭据（D1 两形态之一；与 `private_key_path` 至少其一）。
    pub password: Option<String>,
    /// 私钥文件路径（D1 两形态之二）。
    pub private_key_path: Option<PathBuf>,
    /// 私钥解锁口令（加密私钥；无则私钥视为未加密）。
    pub private_key_passphrase: Option<String>,
    /// 服务器 host key 指纹（D2：`SHA256:...` OpenSSH 形态；`None` =
    /// 未接受——驱动拒连并在错误里给出服务器实际指纹与接受途径）。
    pub host_fingerprint: Option<String>,
    /// 卷根（后端绝对路径；缺省 [`DEFAULT_ROOT`]）。
    pub root: String,
}

impl SftpParams {
    /// 从展平的配置键值对解析（纯函数：无 IO、无网络）。
    ///
    /// - 键集 = config.toml 的八个 `sftp_*` 键（见 cloudkit-core
    ///   `KNOWN_TOML_KEYS`）；未知键 → `Invalid`（拼写错误在装配期
    ///   暴露，而不是静默变成缺省值后连不上）；
    /// - `sftp_host` / `sftp_username` 必填非空；
    /// - `sftp_password` / `sftp_private_key_path` 至少其一（D1）；
    /// - `sftp_port` 须在 1..=65535（与 core `validate()` 同规则）；
    /// - `sftp_root` 须以 `'/'` 开头（后端绝对路径，baidu_root 同款）；
    /// - 凭据键空串视为未设置（与 baidu 键的 empty-means-unset 语义
    ///   对齐——UPDATE 的 write-only 叠加依赖这一条）。
    pub fn from_pairs(pairs: &[(String, String)]) -> Result<Self, StorageError> {
        // 已见键的收集（空串 = 未设置的归一在取值时做）
        let mut host: Option<String> = None;
        let mut port: Option<u16> = None;
        let mut username: Option<String> = None;
        let mut password: Option<String> = None;
        let mut private_key_path: Option<String> = None;
        let mut private_key_passphrase: Option<String> = None;
        let mut host_fingerprint: Option<String> = None;
        let mut root: Option<String> = None;
        for (key, value) in pairs {
            let non_empty = || Some(value.clone()).filter(|v| !v.is_empty());
            match key.as_str() {
                "sftp_host" => host = non_empty(),
                "sftp_port" => {
                    let parsed: u16 = value.parse().map_err(|_| {
                        invalid_config(format!(
                            "sftp_port must be a whole number in 1..=65535, got {value:?}"
                        ))
                    })?;
                    port = Some(parsed);
                }
                "sftp_username" => username = non_empty(),
                "sftp_password" => password = non_empty(),
                "sftp_private_key_path" => private_key_path = non_empty(),
                "sftp_private_key_passphrase" => private_key_passphrase = non_empty(),
                "sftp_host_fingerprint" => host_fingerprint = non_empty(),
                "sftp_root" => root = non_empty(),
                other => {
                    return Err(invalid_config(format!(
                        "unknown sftp key {other:?}: the accepted keys are sftp_host, \
                         sftp_port, sftp_username, sftp_password, sftp_private_key_path, \
                         sftp_private_key_passphrase, sftp_host_fingerprint, sftp_root"
                    )));
                }
            }
        }
        let host = host.ok_or_else(|| {
            invalid_config(
                "backend = \"sftp\" requires sftp_host: set the SSH server address in \
                 config.toml (or the volume file)",
            )
        })?;
        let username = username.ok_or_else(|| {
            invalid_config(
                "backend = \"sftp\" requires sftp_username: set the SSH login user in \
                 config.toml (or the volume file)",
            )
        })?;
        let port = port.unwrap_or(DEFAULT_PORT);
        if !(1..=65535).contains(&port) {
            return Err(invalid_config(format!(
                "sftp_port must be in 1..=65535, got {port}"
            )));
        }
        let private_key_path = private_key_path.map(PathBuf::from);
        if password.is_none() && private_key_path.is_none() {
            return Err(invalid_config(
                "backend = \"sftp\" requires an auth credential: set sftp_password or \
                 sftp_private_key_path (password or private key — at least one; the \
                 optional sftp_private_key_passphrase unlocks an encrypted key)",
            ));
        }
        let root = root.unwrap_or_else(|| DEFAULT_ROOT.to_string());
        if !root.starts_with('/') {
            return Err(invalid_config(format!(
                "sftp_root must be a backend-absolute path starting with '/', e.g. \
                 \"/srv/cloudfs\", got {root:?}"
            )));
        }
        Ok(SftpParams {
            host,
            port,
            username,
            password,
            private_key_path,
            private_key_passphrase,
            host_fingerprint,
            root,
        })
    }
}
