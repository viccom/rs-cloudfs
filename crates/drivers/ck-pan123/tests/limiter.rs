//! 令牌桶限流器契约测试（Phase 6 / 123-2；K62.3 同形态——pan115
//! tests/limiter.rs 先例）。
//!
//! 语义契约（断言即契约，实现者禁改；黄金参照 = 计划 §5.15 + 任务 G）：
//!
//! - **令牌桶节拍**：`rate_per_sec` 之上的并发许可按节拍排队——burst
//!   容量内的许可立即通过，超出者等到下一张令牌；
//! - **缺省保守值 ~2rps**（123 免费账号真值未测——起步保守，RebuildTuning
//!   式可注入）；
//! - 毫秒级参数注入（[`LimiterConfig::fast`]）——测试不睡生产节拍。

use std::time::{Duration, Instant};

use ck_pan123::limiter::{LimiterConfig, RateLimiter};

/// burst 容量内的许可立即通过（无节拍等待）。
#[tokio::test]
async fn burst_permits_pass_immediately() {
    let limiter = RateLimiter::new(LimiterConfig {
        rate_per_sec: 50.0,
        burst: 3,
    });
    let start = Instant::now();
    for _ in 0..3 {
        limiter.check_wait().await;
    }
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "burst permits must not pace: {:?}",
        start.elapsed()
    );
}

/// 超出 burst 的许可按 rate 节拍等待（第 burst+1 张等约 1/rate）。
#[tokio::test]
async fn beyond_burst_paces_at_the_configured_rate() {
    let limiter = RateLimiter::new(LimiterConfig {
        rate_per_sec: 50.0, // 20ms/令牌
        burst: 2,
    });
    limiter.check_wait().await;
    limiter.check_wait().await;
    let start = Instant::now();
    limiter.check_wait().await;
    let elapsed = start.elapsed();
    // 20ms 节拍：调度噪声余量下界 10ms（低于它 = 没有真正节拍）。
    assert!(
        elapsed >= Duration::from_millis(10),
        "the over-burst permit must pace: {elapsed:?}"
    );
}

/// 连续排空后的等待随需求量增长（多张令牌 = 多个节拍，线性）。
#[tokio::test]
async fn pacing_scales_with_permits() {
    let limiter = RateLimiter::new(LimiterConfig {
        rate_per_sec: 100.0, // 10ms/令牌
        burst: 1,
    });
    limiter.check_wait().await;
    let one = {
        let start = Instant::now();
        limiter.check_wait().await;
        start.elapsed()
    };
    let three = {
        let start = Instant::now();
        for _ in 0..3 {
            limiter.check_wait().await;
        }
        start.elapsed()
    };
    assert!(
        three > one,
        "3 permits ({three:?}) must wait longer than 1 ({one:?})"
    );
}

/// 生产缺省 = 保守 2 rps（burst 4）——数值钉死（真值未测前的起步档）。
#[test]
fn production_defaults_are_conservative() {
    let cfg = LimiterConfig::default();
    assert_eq!(cfg.rate_per_sec, 2.0, "task G: ~2 rps to start");
    assert_eq!(cfg.burst, 4, "a modest burst head-room");
}

/// 测试缝：fast 配置的节拍在毫秒级（供集成桩回放使用）。
#[tokio::test]
async fn fast_config_paces_at_millisecond_scale() {
    let limiter = RateLimiter::new(LimiterConfig::fast());
    for _ in 0..LimiterConfig::fast().burst {
        limiter.check_wait().await;
    }
    let start = Instant::now();
    limiter.check_wait().await;
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "fast pacing stays test-friendly: {:?}",
        start.elapsed()
    );
}
