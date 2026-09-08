//! errno 映射表逐码回放（Batch B1 红②；R2 错误归一义务）。
//!
//! 码表与逐码注源见 `src/api.rs` 模块文档（驱动侧权威文档注释）；
//! 110/111/-6 三档由 `tests/oauth_state_machine.rs` 覆盖（互指，不重复）。

mod common;

use std::sync::Arc;

use ck_baidu::{factory, BaiduDriver};
use cloudkit_storage::{RelPath, StorageDriver, StorageError};

use common::MockBaidu;

async fn setup() -> (MockBaidu, Arc<BaiduDriver>) {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(common::MOCK_ROOT);
    mock.seed_file(&format!("{}/f.txt", common::MOCK_ROOT), 10, 1757311000);
    let driver = factory(&mock.params(None))
        .await
        .expect("baidu driver connect（uinfo 取 uid）");
    (mock, driver)
}

/// stat 一个**已播种存在**的条目：断言失败必来自注入的 errno 映射，
/// 而非数据缺失——映射语义的可归因性。
async fn stat_seeded(driver: &BaiduDriver) -> Result<cloudkit_storage::Entry, StorageError> {
    driver.stat(&RelPath::new("f.txt").expect("rel path")).await
}

#[tokio::test]
async fn errno_31034_maps_to_rate_limited_without_retry_after() {
    let (mock, driver) = setup().await;
    // K15：client 层单点重试一次（指数退避）。**双注入**钉死「至多重试
    // 一次」——重试两次以上会撞空队列变成功，本断言即失败。
    // 重试退避须测试友好（base ≤1s 量级，client.rs 模块文档契约）。
    mock.inject_errno(31034);
    mock.inject_errno(31034);

    let err = driver
        .list(&RelPath::root(), cloudkit_storage::Page::all())
        .await
        .expect_err("持续 31034 必须报错");
    assert_eq!(
        err,
        StorageError::RateLimited { retry_after: None },
        "31034 → RateLimited{{retry_after:None}}（spike §2/附录 A :143——后端未明示等待时长）"
    );
}

#[tokio::test]
async fn errno_minus9_maps_to_not_found() {
    let (mock, driver) = setup().await;
    mock.inject_errno(-9);

    let err = stat_seeded(&driver).await.expect_err("-9 必须报错");
    assert_eq!(
        err,
        StorageError::NotFound,
        "-9（文件/目录不存在）→ NotFound（spike cleanup 实证）"
    );
}

#[tokio::test]
async fn errno_12_maps_to_invalid() {
    let (mock, driver) = setup().await;
    mock.inject_errno(12);

    let err = stat_seeded(&driver).await.expect_err("12 必须报错");
    assert_eq!(
        err,
        StorageError::Invalid,
        "12（参数错误）→ Invalid（附录 A errno 档）"
    );
}

#[tokio::test]
async fn errno_31326_maps_to_unauthorized_recoverable_true() {
    let (mock, driver) = setup().await;
    mock.inject_errno(31326);

    let err = stat_seeded(&driver).await.expect_err("31326 必须报错");
    assert_eq!(
        err,
        StorageError::Unauthorized { recoverable: true },
        "31326（CDN 下载鉴权失败，spike §5 矩阵）→ recoverable:true——重取 dlink/追 token 可救，B2 两段 fallback 的前提"
    );
}

#[tokio::test]
async fn unknown_errno_maps_to_unavailable_with_code_preserved() {
    let (mock, driver) = setup().await;
    // 47002 = 刻意选的表外码（未入 B1 钉死映射表的真实百度码族）——
    // 未知码契约（R2）：Unavailable 且载荷保留原码，可诊断不丢信息。
    mock.inject_errno(47002);

    let err = stat_seeded(&driver)
        .await
        .expect_err("未知码必须报错而非吞掉");
    match err {
        StorageError::Unavailable(msg) => assert!(
            msg.contains("47002"),
            "未知码的原码必须保留在载荷里：{msg:?}"
        ),
        other => panic!("未知码应映射 Unavailable（含原码），实得 {other:?}"),
    }
}
