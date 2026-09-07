//! MockStorageDriver 跑 conformance 套件全部八条（interfaces §6）。

mod common;

use async_trait::async_trait;
use cloudkit_storage::conformance::{assert_conforms, ConformanceHarness, ErrorReplay};
use cloudkit_storage::{Capabilities, MockStorageDriver, StorageDriver, VolumeId};

use common::map_baidu_errno;

const CHUNK: u64 = 16;

struct MockHarness {
    driver: MockStorageDriver,
}

impl MockHarness {
    fn new() -> Self {
        MockHarness {
            driver: MockStorageDriver::with_chunk_size(
                VolumeId::parse("local:/mock-conformance").expect("测试卷 id"),
                CHUNK,
            ),
        }
    }
}

#[async_trait]
impl ConformanceHarness for MockHarness {
    fn driver(&self) -> &dyn StorageDriver {
        &self.driver
    }

    fn chunk_size(&self) -> u64 {
        CHUNK
    }

    fn delete_missing_yields_not_found(&self) -> bool {
        true
    }

    fn empty_range_yields_empty_stream(&self) -> bool {
        true
    }

    /// 断言⑤回放表：百度 errno 三档 fixture（R1——映射在测试侧，
    /// mock 只接收已映射的错误注入）。
    fn error_table(&self) -> Vec<ErrorReplay> {
        common::BAIDU_AUTH_TIERS
            .iter()
            .map(|&errno| ErrorReplay {
                backend_code: errno.to_string(),
                expected: map_baidu_errno(errno),
            })
            .collect()
    }

    async fn inject_backend_error(&self, backend_code: &str) {
        let errno: i32 = backend_code
            .parse()
            .unwrap_or_else(|e| panic!("fixture 码必须是整数 errno: {backend_code}: {e}"));
        self.driver.fail_next_stat(map_baidu_errno(errno));
    }

    async fn backend_bytes_received(&self) -> Option<u64> {
        Some(self.driver.bytes_received())
    }
}

#[tokio::test]
async fn offline_conformance_suite_mock() {
    assert_conforms(&MockHarness::new()).await;
}

/// 套件能力位门控的旁证：mock 的能力声明必须是「套件已验证」的形态
/// （R4 诚实性的最小静态检查；动态验证由上面整套跑通承担）。
#[test]
fn mock_capabilities_are_the_declared_set() {
    let driver = MockStorageDriver::new(VolumeId::parse("local:/mock").unwrap());
    assert_eq!(
        driver.capabilities(),
        Capabilities {
            range_read: true,
            resume: true,
            multipart: false,
            server_side_move: true,
            rapid_upload: false,
            authoritative_index: true,
            change_feed: false,
            inbound: false,
            chat: false,
        }
    );
}
