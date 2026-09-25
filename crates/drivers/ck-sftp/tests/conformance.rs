//! ck-sftp conformance 套件接入（Phase 4 / SF3；interfaces §6 / D9）。
//!
//! 被测对象 = 全量实现驱动（SF1 骨架 + SF2 桩行为测试的产物），后端
//! 注入 = SF2 的进程内桩（hermetic，回环 127.0.0.1:0，无真实网络）。
//! `conformance_suite_offline` 跑断言①–⑥⑧；⑦ RESUME 未声明（计划
//! §4.2：断点续传由上层缓存承担）由能力位门控自动跳过（local 先例）。
//!
//! harness 形态声明：
//! - `chunk_size = 1`：SFTP 单流写入无分块（无分块驱动报 1）；
//! - `delete_missing = NotFound`：stat 预检天然给出（local 同款声明）；
//! - `empty_range = 空流`：start>=size 不开句柄直接空流（driver.rs）；
//! - `error_table` 两码选码理由（断言⑤注入路径是 **stat**，走桩的一次
//!   性状态码注入面 `Stub::fail_next_stat`——映射经真实协议回放）：
//!   - `SSH_FX_NO_SUCH_FILE → NotFound`：NotFound 判定只认此码（error.rs
//!     红线——PermissionDenied/连接类绝不折叠为 NotFound）；
//!   - `SSH_FX_PERMISSION_DENIED → Unauthorized{false}`：权限拒绝的
//!     无自救形态；
//!   - 刻意不选 `SSH_FX_NO_CONNECTION`/`SSH_FX_CONNECTION_LOST`
//!     （Unavailable 族）：驱动的 with_retry 重连骨架会吸收单次
//!     Unavailable 并重试成功（单注入被重试消费后 stat 成功，断言⑤
//!     的「stat 必须失败」不成立）——重连腿由 write_path.rs 的
//!     `next_operation_reconnects_after_connection_kill` 独立钉死
//!     （baidu conformance 刻意不选 110 的同款互指不重复）；
//!   - 刻意不选 `SSH_FX_FAILURE`（→ Io 保留原始码与消息）：桩侧
//!     `StatusCode → Status.error_message` 的文本由 russh-sftp 服务端
//!     运行时生成、不经测试控制——逐字 pin 会把库内部文案钉进断言；
//!     Failure 腿的映射已由 lib.rs `status_mapping_follows_the_plan_table`
//!     单测钉死（含消息保留断言）。
//!
//! **断言① 的离线绿语义边界（如实声明）**：桩的写句柄是 commit-on-close
//! 模型（VFS 在 close 时才见到写缓冲）——staging 窗口内 stat 目标 =
//! NotFound，断言①离线绿。真实 OpenSSH 服务器上 pwrite 立即可见、
//! TRUNCATE 在 open 时生效：直写目标路径（计划 §7 裁决）在真机上的
//! staging 窗口以部分内容可见——该形态差是 SF4 真机矩阵的既定检查项
//! （届时 stash 方案或豁免裁决，见跟踪单 SF3 批次日志）。

mod stub;

use async_trait::async_trait;
use ck_sftp::{SftpDriver, SftpParams};
use cloudkit_storage::conformance::{ConformanceHarness, ErrorReplay};
use cloudkit_storage::{Capabilities, StorageDriver, StorageError};
use russh_sftp::protocol::StatusCode;
use stub::{Stub, StubAuth};

/// 无分块驱动：chunk 边界报 1（ConformanceHarness::chunk_size 契约）。
const CHUNK: u64 = 1;

const USER: &str = "conformance";
const PASSWORD: &str = "stub-only-password";

struct SftpHarness {
    /// 桩活到 harness 生命结束（Drop 随测试结束回收监听任务）。
    _stub: Stub,
    driver: SftpDriver,
}

impl SftpHarness {
    async fn new() -> Self {
        let stub = Stub::start(StubAuth::password(USER, PASSWORD)).await;
        let mut pairs = stub.param_pairs();
        pairs.push((
            "sftp_host_fingerprint".to_string(),
            stub.fingerprint().to_string(),
        ));
        let params = SftpParams::from_pairs(&pairs).expect("params");
        let driver = SftpDriver::new(params).expect("driver");
        SftpHarness {
            _stub: stub,
            driver,
        }
    }
}

#[async_trait]
impl ConformanceHarness for SftpHarness {
    fn driver(&self) -> &dyn StorageDriver {
        &self.driver
    }

    fn chunk_size(&self) -> u64 {
        CHUNK
    }

    fn delete_missing_yields_not_found(&self) -> bool {
        // stat 预检形态（write_path.rs `delete_file_and_missing_not_found`
        // 行为钉死）：删除不存在路径恒 NotFound。
        true
    }

    fn empty_range_yields_empty_stream(&self) -> bool {
        // driver.rs reader：start>=size / 空窗口不开远程句柄直接空流。
        true
    }

    fn error_table(&self) -> Vec<ErrorReplay> {
        vec![
            ErrorReplay {
                backend_code: "SSH_FX_NO_SUCH_FILE".to_string(),
                expected: StorageError::NotFound,
            },
            ErrorReplay {
                backend_code: "SSH_FX_PERMISSION_DENIED".to_string(),
                expected: StorageError::Unauthorized { recoverable: false },
            },
        ]
    }

    async fn inject_backend_error(&self, backend_code: &str) {
        let code = match backend_code {
            "SSH_FX_NO_SUCH_FILE" => StatusCode::NoSuchFile,
            "SSH_FX_PERMISSION_DENIED" => StatusCode::PermissionDenied,
            other => panic!("error_table 码必须是可注入的状态码，got {other:?}"),
        };
        // M3（sftp-review）：注入打在 **lstat** 旋钮上——K67 把
        // driver.stat 的非根路径实现为 symlink_metadata（SSH_FXP_LSTAT），
        // 注入必须打在驱动 stat 实际发出的协议动词上（修复前靠 lstat
        // 处理器偷吃 stat 槽位假绿，拆分后此处即真相）。
        self._stub.fail_next_lstat(code);
    }
}

cloudkit_storage::conformance_suite!(SftpHarness::new().await);

/// 能力声明静态锁（R4 诚实性；ck-local / ck-baidu conformance 同款）：
/// 十位精确值钉死，防未来漂移。动态验证由 `conformance_suite_offline`
/// 全套跑通承担（⑦ RESUME 由 `resume = false` 门控跳过）。
#[test]
fn capabilities_are_the_declared_set() {
    let mut pairs = vec![
        ("sftp_host".to_string(), "nas.lan".to_string()),
        ("sftp_username".to_string(), "cloudfs".to_string()),
        ("sftp_password".to_string(), "pw".to_string()),
    ];
    pairs.push((
        "sftp_host_fingerprint".to_string(),
        "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
    ));
    let params = SftpParams::from_pairs(&pairs).expect("params");
    let driver = SftpDriver::new(params).expect("driver");
    assert_eq!(
        driver.capabilities(),
        Capabilities {
            range_read: true,          // 断言②全套（offset 读 + 越界钳制 + 空窗口）
            resume: false,             // 无远端分片会话（断言⑦门控跳过）
            multipart: false,          // SFTP 无分块上传原语（单流写入）
            server_side_move: true,    // SFTP rename 服务端单侧移动（断言⑥）
            rapid_upload: false,       // 无内容寻址去重后端
            authoritative_index: true, // 远端文件系统即真相（断言③；rebuild 可用）
            change_feed: false,        // SFTP 无变更推送
            inbound: false,            // 无入站通道
            chat: false,               // 无对话通道
            remote_delete: true,       // delete_remote 真删卷内文件（K4）
        }
    );
}

/// 工厂冒烟（ck-local conformance 同款）：async 装配入口可用、卷身份
/// `sftp:<user>@<host>:<port>`（D6）。**不连网**——factory 纯构造
/// （D3 惰性建立），本测试能跑过本身就是证明。
#[tokio::test]
async fn factory_bootstraps_sftp_volume_without_connecting() {
    let params = SftpParams::from_pairs(&[
        ("sftp_host".to_string(), "nas.lan".to_string()),
        ("sftp_username".to_string(), "cloudfs".to_string()),
        ("sftp_password".to_string(), "pw".to_string()),
    ])
    .expect("params");
    let driver = ck_sftp::factory(&params)
        .await
        .expect("factory constructs without connecting");
    assert_eq!(driver.volume().scheme(), "sftp");
    assert_eq!(driver.volume().key(), "cloudfs@nas.lan:22");
}
