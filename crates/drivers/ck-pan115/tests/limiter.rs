//! limiter.rs 契约测试（Phase 5 / 115-1；D4 落值 = K69.3 实测裁决）。
//!
//! 语义契约（断言即契约，实现者禁改）：
//!
//! - **令牌桶**：`rate_per_sec` 补充、容量 `burst`——前 `burst` 张许可
//!   立即发出（突发），之后按节拍间隔放行；
//! - **770004 硬退避**：`report_limit` 后进入封锁窗（`blocked_until`），
//!   封锁期内 `check_wait` **立即**拒绝（返回剩余时长，绝不 sleep——
//!   风暴重试在封锁期内不得再发任何请求）；
//! - **指数与上限**：连续 `report_limit` 的封锁窗 `initial → ×2 → … →
//!   max` 封顶后不再增长；
//! - **恢复**：封锁窗过期后的下一次 `check_wait` 放行，且退避指数
//!   重置回 initial（`report_ok` 成功上报同样重置——成功即限流状态
//!   解除）。
//!
//! 可测性：毫秒级参数注入（`LimiterConfig::fast`），形态照
//! `RebuildTuning::fast` 先例（结构注入而非 pause/时钟替身——仓库
//! 既有时间敏感测试的一致形态）。生产缺省值由独立测试钉死（不实际
//! 等待 300s）。

use std::time::Duration;

use ck_pan115::limiter::{LimiterConfig, RateLimiter};

/// 毫秒级窗口（20ms 节拍、40ms 起步退避、160ms 上限——指数两档即可
/// 观测 40→80→160 的增长与封顶）。
fn fast() -> LimiterConfig {
    LimiterConfig {
        rate_per_sec: 50.0,
        burst: 2,
        block_initial: Duration::from_millis(40),
        block_max: Duration::from_millis(160),
    }
}

#[tokio::test]
async fn burst_allows_immediate_permits_then_paces() {
    let limiter = RateLimiter::new(fast());
    let pace = Duration::from_millis(20); // 1 / 50 rps

    // 突发 2：立即放行
    let start = tokio::time::Instant::now();
    limiter.check_wait().await.expect("burst permit 1");
    limiter.check_wait().await.expect("burst permit 2");
    assert!(
        start.elapsed() < pace / 2,
        "burst permits must be immediate, took {:?}",
        start.elapsed()
    );

    // 第 3 张：按节拍等待（至少大半个节拍间隔）
    let start = tokio::time::Instant::now();
    limiter.check_wait().await.expect("paced permit 3");
    assert!(
        start.elapsed() >= pace * 3 / 4,
        "the (burst+1)-th permit must pace at 1/rate, took {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn bucket_refills_after_idle() {
    let limiter = RateLimiter::new(fast());
    limiter.check_wait().await.expect("burst 1");
    limiter.check_wait().await.expect("burst 2");
    // 空闲超过一个节拍 → 桶重新攒出许可（等 1.5 节拍）
    tokio::time::sleep(Duration::from_millis(30)).await;
    let start = tokio::time::Instant::now();
    limiter.check_wait().await.expect("refilled permit");
    assert!(
        start.elapsed() < Duration::from_millis(10),
        "an idle bucket must serve immediately, took {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn report_limit_grows_exponentially_and_caps() {
    let limiter = RateLimiter::new(fast());

    // 40ms → 80ms → 160ms（上限）→ 160ms（封顶后不再增长）
    assert_eq!(limiter.report_limit().await, Duration::from_millis(40));
    assert_eq!(limiter.report_limit().await, Duration::from_millis(80));
    assert_eq!(limiter.report_limit().await, Duration::from_millis(160));
    assert_eq!(
        limiter.report_limit().await,
        Duration::from_millis(160),
        "capped at block_max"
    );

    // 封锁期内的可见状态：剩余 ≤ 刚上报的窗口
    let remaining = limiter
        .blocked_remaining()
        .await
        .expect("blocked after report_limit");
    assert!(
        remaining <= Duration::from_millis(160),
        "remaining must not exceed the fresh window, got {remaining:?}"
    );
}

#[tokio::test]
async fn check_wait_rejects_immediately_during_block() {
    let limiter = RateLimiter::new(fast());
    limiter.report_limit().await;

    // 硬退避：立即拒绝（绝不 sleep 掉封锁窗——那是调用方的 RateLimited
    // 上抛路径），剩余时长为正
    let start = tokio::time::Instant::now();
    let remaining = limiter
        .check_wait()
        .await
        .expect_err("block window must reject without waiting");
    assert!(
        start.elapsed() < Duration::from_millis(10),
        "check_wait must return immediately during a block, took {:?}",
        start.elapsed()
    );
    assert!(
        remaining > Duration::ZERO,
        "the rejection must carry the remaining window, got {remaining:?}"
    );
}

#[tokio::test]
async fn expired_block_releases_and_resets_the_backoff_ladder() {
    let limiter = RateLimiter::new(fast());
    // 走两档：40 → 80
    limiter.report_limit().await;
    limiter.report_limit().await;
    // 等封锁窗（80ms）过期
    tokio::time::sleep(Duration::from_millis(90)).await;
    limiter
        .check_wait()
        .await
        .expect("an expired block must release the next permit");
    // 指数重置：下次 770004 又从 initial 起步
    assert_eq!(
        limiter.report_limit().await,
        Duration::from_millis(40),
        "backoff ladder resets after the window expires"
    );
}

#[tokio::test]
async fn report_ok_resets_the_backoff_ladder() {
    let limiter = RateLimiter::new(fast());
    limiter.report_limit().await; // 40
    limiter.report_limit().await; // 80
    limiter.report_ok().await; // 成功 = 限流状态解除
    assert_eq!(
        limiter.report_limit().await,
        Duration::from_millis(40),
        "report_ok must reset the ladder to the initial window"
    );
}

#[test]
fn default_config_pins_the_d4_values() {
    // K69.3 实测定值（D4 落值）：全局 1 rps、burst 2、770004 封锁
    // 300s 起步指数 ×2、上限 1800s。
    let cfg = LimiterConfig::default();
    assert_eq!(cfg.rate_per_sec, 1.0, "1 rps default");
    assert_eq!(cfg.burst, 2, "burst 2 default");
    assert_eq!(cfg.block_initial, Duration::from_secs(300));
    assert_eq!(cfg.block_max, Duration::from_secs(1800));
}
