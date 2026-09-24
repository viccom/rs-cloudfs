//! 全局令牌桶限流器（Phase 6 / 123-2；K62.3 同形态——ck-pan115
//! limiter 模块先例）。
//!
//! 123 免费账号的可持续 rps 真值未测（跟踪单挂账）——**缺省保守
//! ~2 rps**（burst 4），RebuildTuning 式结构注入（[`LimiterConfig`]）。
//! 与 115 的差异：123 无已知的「整账号封锁码」（770004 形态），故无
//! 硬退避状态机——限流面 = 令牌桶节拍 + HTTP 重试策略（api.rs 的
//! Retry-After/指数退避，§5.15）。
//!
//! - [`RateLimiter::check_wait`]：取一张发送许可——桶内有令牌立即过，
//!   桶空则节拍等待（锁外 sleep，不阻塞并发许可）；
//! - 可测性：毫秒级参数注入（[`LimiterConfig::fast`]）；时间原语用
//!   tokio::time（异步等待不占阻塞线程）。
use cloudkit_storage::{TokenBucket, TokenBucketConfig};

/// 限流器参数（生产缺省 = 保守起步档；测试经 [`LimiterConfig::fast`]
/// 注入毫秒级节拍）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LimiterConfig {
    /// 令牌补充速率（令牌/秒；生产缺省 2.0——真值未测前的保守档）。
    pub rate_per_sec: f64,
    /// 桶容量 = 允许的突发许可数（生产缺省 4）。
    pub burst: u32,
}

impl Default for LimiterConfig {
    fn default() -> Self {
        LimiterConfig {
            rate_per_sec: 2.0,
            burst: 4,
        }
    }
}

impl LimiterConfig {
    /// 毫秒级节拍（测试专用，绝不用于生产）。
    pub fn fast() -> Self {
        LimiterConfig {
            rate_per_sec: 500.0,
            burst: 8,
        }
    }
}

impl From<LimiterConfig> for TokenBucketConfig {
    fn from(cfg: LimiterConfig) -> Self {
        TokenBucketConfig {
            rate_per_sec: cfg.rate_per_sec,
            burst: cfg.burst,
        }
    }
}

/// 全局限流器（api client 持有并共享——dispatch 前统一过门；「全局」
/// 指**每卷一个**）。
///
/// 令牌桶共核经 L2 [`TokenBucket`]（架构审查 D4 项②：两驱动的令牌桶
/// 部分去注释后相同，pan115 的 770004 封锁窗状态机为扩展不进共核）。
///
/// 并发：全方法可并发调用（`&self` + 内部 Mutex）；等待（节拍 sleep）
/// 发生在锁外，一个等待者不阻塞其他许可的推进。
pub struct RateLimiter {
    bucket: TokenBucket,
}

impl RateLimiter {
    /// 以给定参数构造（桶满——进程启动即允许初始突发）。
    pub fn new(cfg: LimiterConfig) -> Self {
        RateLimiter {
            bucket: TokenBucket::new(cfg.into()),
        }
    }

    /// 取一张发送许可：桶内有令牌 → 扣一返回；桶空 → 节拍等待
    /// （锁外 sleep 到下一张令牌可扣）后返回。
    pub async fn check_wait(&self) {
        self.bucket.wait_one().await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn tokens_refill_after_idle() {
        // 闲置一个节拍后令牌回满——burst 全数立即可取。
        let limiter = RateLimiter::new(LimiterConfig {
            rate_per_sec: 500.0,
            burst: 2,
        });
        limiter.check_wait().await;
        limiter.check_wait().await;
        limiter.check_wait().await; // 排空 + 一张节拍等待
        tokio::time::sleep(Duration::from_millis(10)).await;
        let start = std::time::Instant::now();
        limiter.check_wait().await; // 闲置后桶内有存量
        assert!(
            start.elapsed() < Duration::from_millis(10),
            "refilled token passes promptly"
        );
    }
}
