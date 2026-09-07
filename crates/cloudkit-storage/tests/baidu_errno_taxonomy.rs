//! 百度 errno 三档映射 fixture（test-only，R1：运行时 L2 不认识后端错误码）。
//!
//! 钉死 interfaces §3 / foundation D2 的分类学三档 + 经 mock 驱动回放
//! （断言⑤同款注入路径）。

mod common;

use cloudkit_storage::{MockStorageDriver, StorageDriver, StorageError, VolumeId};

use common::{map_baidu_errno, BAIDU_AUTH_TIERS};

#[test]
fn baidu_errno_110_maps_to_recoverable_unauthorized() {
    // 110 = access token 过期：驱动内自动刷新 + 重放一次仍失败 → recoverable
    assert_eq!(
        map_baidu_errno(110),
        StorageError::Unauthorized { recoverable: true }
    );
}

#[test]
fn baidu_errno_111_maps_to_unrecoverable_unauthorized() {
    // 111 = refresh token 失效：需人工重新授权，勿重试循环
    assert_eq!(
        map_baidu_errno(111),
        StorageError::Unauthorized { recoverable: false }
    );
}

#[test]
fn baidu_errno_minus6_maps_to_unrecoverable_unauthorized() {
    assert_eq!(
        map_baidu_errno(-6),
        StorageError::Unauthorized { recoverable: false }
    );
}

#[test]
fn baidu_errno_three_tiers_are_pairwise_distinct_in_recoverability() {
    let t110 = map_baidu_errno(110);
    let t111 = map_baidu_errno(111);
    let tm6 = map_baidu_errno(-6);
    assert_ne!(t110, t111, "110 与 111 的可恢复性必须可区分");
    assert_eq!(t111, tm6, "111 与 -6 同档（不可恢复）");
}

#[test]
fn baidu_unknown_errno_falls_to_unavailable_with_code_preserved() {
    // 31034（QPS 限额形态之一）等未知码：Unavailable + 原始码保留
    let e = map_baidu_errno(31034);
    assert!(
        matches!(e, StorageError::Unavailable(_)),
        "未知码 → Unavailable"
    );
    assert!(e.to_string().contains("31034"), "载荷保留原始 errno");
}

/// 经 mock 驱动的回放路径（conformance 断言⑤同款注入）：
/// 注入已映射错误 → 驱动对外呈现的必须是该 StorageError 形态。
#[tokio::test]
async fn baidu_tiers_replay_through_driver() {
    let driver = MockStorageDriver::new(VolumeId::parse("baidu:fixture").unwrap());
    // 先放一个可 stat 的对象（writer 真实 commit 后 stat 才有对象）
    let path = cloudkit_storage::RelPath::new("fixture/probe").unwrap();
    let mut stager = driver
        .writer(&path, &Default::default())
        .await
        .expect("writer");
    stager.write(b"probe").await.expect("write");
    stager.close().await.expect("close");

    for errno in BAIDU_AUTH_TIERS {
        driver.fail_next_stat(map_baidu_errno(errno));
        let err = driver.stat(&path).await.expect_err("注入后必须失败");
        assert_eq!(err, map_baidu_errno(errno), "errno {errno} 回放形态");
        // 恰好一次：故障消费后恢复
        driver.stat(&path).await.expect("注入应恰好一次");
    }
}
