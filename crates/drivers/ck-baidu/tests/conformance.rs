//! ck-baidu conformance 套件接入（Batch B2 红④；interfaces §6 / D9）。
//!
//! 红阶段：writer/reader 尚为 `Unsupported` 占位 → `conformance_suite_offline`
//! 在断言①（上传往返）红；绿阶段验收 = ①–⑧ **全绿不得缩减**（⑦ RESUME
//! 由能力位门控：绿阶段点亮 `resume` 位后自动启用）。
//!
//! harness 形态声明：
//! - `chunk_size = 4MiB`（百度分块策略，spike §2/§6）；
//! - `backend_bytes_received = Some(superfile2 分片净荷累计)`——声明
//!   RESUME 即必须可观测（R4；套件因 None 判败）；
//! - `delete_missing = NotFound`（B1 驱动文档声明：fs_id 直查解析）；
//! - `empty_range = 空流`（download Range 越界钳制语义）；
//! - `error_table` 三对选码理由（conformance ⑤ 注入路径是 **stat**，走
//!   xpan 家族 errno 注入队列——选测试友好码）：
//!   - `-9 → NotFound`：无副作用的最基础映射（spike cleanup 实证）；
//!   - `31034 → RateLimited{None}`：K15 单点重试存在——**双注入**使终态
//!     失败（单注入会被重试吃掉，断言「stat 必须失败」不成立）；
//!   - `31326 → Unauthorized{true}`：B2 下载鉴权码经 stat 路径回放（映射
//!     表驱动侧统一，无 client 自救钩子——单注入即终态）；
//!   - 刻意不选 110（会触发 oauth 刷新+重放，副作用与 ⑤ 的「恰一次注入
//!     消费」语义纠缠——110 族已由 oauth_state_machine.rs 钉死）与
//!     111/-6（终态 Unauthorized 属 oauth 族，同上互指不重复）。

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use ck_baidu::{factory, BaiduDriver};
use cloudkit_storage::conformance::{ConformanceHarness, ErrorReplay};
use cloudkit_storage::{StorageDriver, StorageError};

/// 百度分块边界（4MiB；spike §2/§6——上/下载统一）。
const CHUNK: u64 = 4 * 1024 * 1024;

struct BaiduHarness {
    mock: common::MockBaidu,
    driver: Arc<BaiduDriver>,
}

impl BaiduHarness {
    async fn new() -> Self {
        let (mock, _base) = common::MockBaidu::start().await;
        mock.seed_dir(common::MOCK_ROOT);
        // sessions_dir=None：纯内存会话——conformance ⑦ 是单进程内同
        // driver 的中断恢复形态（drop stager → 同 driver 再 writer），
        // 不依赖跨进程会话表（跨进程由 upload_resume.rs 以 Some(dir) 钉）。
        let driver = factory(&mock.params(None))
            .await
            .expect("baidu driver connect（uinfo 取 uid）");
        BaiduHarness { mock, driver }
    }
}

#[async_trait]
impl ConformanceHarness for BaiduHarness {
    fn driver(&self) -> &dyn StorageDriver {
        &*self.driver
    }

    fn chunk_size(&self) -> u64 {
        CHUNK
    }

    fn delete_missing_yields_not_found(&self) -> bool {
        // B1 驱动文档声明（driver.rs「delete 幂等形态」）：fs_id 直查解析，
        // 不存在 → NotFound（恒定）。
        true
    }

    fn empty_range_yields_empty_stream(&self) -> bool {
        // download Range 越界钳制：start>=size → 空流（恒定）。
        true
    }

    fn error_table(&self) -> Vec<ErrorReplay> {
        vec![
            ErrorReplay {
                backend_code: "-9".to_string(),
                expected: StorageError::NotFound,
            },
            ErrorReplay {
                backend_code: "31034".to_string(),
                expected: StorageError::RateLimited { retry_after: None },
            },
            ErrorReplay {
                backend_code: "31326".to_string(),
                expected: StorageError::Unauthorized { recoverable: true },
            },
        ]
    }

    async fn inject_backend_error(&self, backend_code: &str) {
        let code: i64 = backend_code
            .parse()
            .expect("error_table 码必须是可注入的数字 errno");
        // 31034：client 层 K15 单点重试一次——双注入使「重试也失败」成为
        // 终态（单注入被重试消费后 stat 成功，断言⑤失败）；其余码单注入
        // 即终态（无 client 自救钩子）。
        let times = if code == 31034 { 2 } else { 1 };
        for _ in 0..times {
            self.mock.inject_errno(code);
        }
    }

    async fn backend_bytes_received(&self) -> Option<u64> {
        // superfile2 分片净荷累计（声明 RESUME 即可观测——R4）。
        Some(self.mock.bytes_received_total())
    }
}

cloudkit_storage::conformance_suite!(BaiduHarness::new().await);

/// 能力声明静态锁（R4 诚实性；B2 绿阶段新增——ck-local 先例）：九位
/// 精确值钉死，防未来漂移。动态验证由 `conformance_suite_offline` 全套
/// 跑通承担（⑦ RESUME 由 `resume` 位门控启用）。
#[tokio::test]
async fn capabilities_are_the_declared_set() {
    let (mock, _base) = common::MockBaidu::start().await;
    mock.seed_dir(common::MOCK_ROOT);
    let driver = factory(&mock.params(None))
        .await
        .expect("baidu driver connect");
    assert_eq!(
        driver.capabilities(),
        cloudkit_storage::Capabilities {
            range_read: true,          // 断言②全套（4MiB 有界窗口拼接）
            resume: true,              // 断言⑦（K7 差集会话表 + 位图复用）
            multipart: true,           // superfile2 4MiB 分片后端原生分块
            server_side_move: true,    // filemanager move 单侧搬移（断言⑥）
            rapid_upload: true,        // return_type=2 路径（不依赖——spike §4）
            authoritative_index: true, // list 即网盘真相（断言③）
            change_feed: false,        // 无变更推送通道（拉取式后端）
            inbound: false,            // 无 bot 入站通道
            chat: false,               // 无对话通道
        }
    );
}
