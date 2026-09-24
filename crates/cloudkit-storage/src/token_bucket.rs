//! 令牌桶限流共核（L2 共享机制件）。
//!
//! **契约来源**：ck-pan115/ck-pan123 的 limiter 在去注释后令牌桶部分相同
//! （`refill` + 「桶内扣一/桶空节拍等待」循环，架构审查 D4 项②）。pan115
//! 在此基础上叠加 770004 硬退避状态机（`report_limit`/`report_ok`/封锁窗），
//! 属协议特化——保留在驱动侧作可选扩展，不进本共核。
//!
//! **只收纯机制**：按流逝时间连续补充令牌（封顶容量）、取一张许可（桶内
//! 立即扣一；桶空锁外节拍等待）。等待（`tokio::time::sleep`）发生在锁外，
//! 一个等待者不阻塞其他许可的推进。

use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

/// 令牌桶参数（生产缺省各驱动自行落值——K69.3 落 1rps/123 落 2rps；
/// 本共核不定默认，由调用方显式给定）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenBucketConfig {
    /// 令牌补充速率（令牌/秒）。
    pub rate_per_sec: f64,
    /// 桶容量 = 允许的突发许可数。
    pub burst: u32,
}

/// 按流逝时间连续补充令牌（封顶容量），就地更新 `(tokens, last_refill)`。
///
/// 共享的 refill 数学（L2 共核；ck-pan115 的封锁窗状态机与本 [`TokenBucket`]
/// 的独立 Mutex 都复用此式——前者因「封锁窗判定 + 令牌消耗须同一锁内
/// 原子」无法整体套用 `TokenBucket`，仅 refill 数学是真共核）。
pub fn refill_tokens(
    tokens: &mut f64,
    last_refill: &mut Instant,
    cfg: &TokenBucketConfig,
    now: Instant,
) {
    let elapsed = now.saturating_duration_since(*last_refill).as_secs_f64();
    *tokens = (*tokens + elapsed * cfg.rate_per_sec).min(cfg.burst as f64);
    *last_refill = now;
}

/// 桶的可变状态（Mutex 保护；等待发生在锁外）。
#[derive(Debug)]
struct TokenBucketState {
    /// 当前令牌数（浮点：按流逝时间连续补充）。
    tokens: f64,
    /// 上次补充时刻。
    last_refill: Instant,
}

impl TokenBucketState {
    /// 按流逝时间补充令牌（封顶容量）。
    fn refill(&mut self, cfg: &TokenBucketConfig, now: Instant) {
        refill_tokens(&mut self.tokens, &mut self.last_refill, cfg, now);
    }
}

/// 令牌桶（api client 持有并共享——「全局」指**每卷一个**）。
///
/// 并发：全方法可并发调用（`&self` + 内部 Mutex）；等待（节拍 sleep）
/// 发生在锁外。
pub struct TokenBucket {
    cfg: TokenBucketConfig,
    state: Mutex<TokenBucketState>,
}

impl TokenBucket {
    /// 以给定参数构造（桶满——进程启动即允许初始突发）。
    pub fn new(cfg: TokenBucketConfig) -> Self {
        let now = Instant::now();
        TokenBucket {
            cfg,
            state: Mutex::new(TokenBucketState {
                tokens: cfg.burst as f64,
                last_refill: now,
            }),
        }
    }

    /// 取一张发送许可：桶内有令牌 → 扣一返回；桶空 → 节拍等待（锁外
    /// sleep 到下一张令牌可扣）后返回。
    pub async fn wait_one(&self) {
        loop {
            let wait = {
                let mut state = self.state.lock().await;
                let now = Instant::now();
                state.refill(&self.cfg, now);
                if state.tokens >= 1.0 {
                    state.tokens -= 1.0;
                    return;
                }
                // 桶空：等到下一张令牌（锁外 sleep——不阻塞并发许可）
                Duration::from_secs_f64((1.0 - state.tokens) / self.cfg.rate_per_sec)
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// 试取一张发送许可（不等待）：桶内有令牌 → 扣一返回 `true`；桶空 →
    /// `false`（调用方据以决定是否进入节拍等待/上报退避——pan115 的封锁窗
    /// 状态机在扩展层消费此面）。
    pub async fn try_one(&self) -> bool {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        state.refill(&self.cfg, now);
        if state.tokens >= 1.0 {
            state.tokens -= 1.0;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast() -> TokenBucketConfig {
        TokenBucketConfig {
            rate_per_sec: 500.0,
            burst: 2,
        }
    }

    #[tokio::test]
    async fn burst_allows_immediate_permits_then_paces() {
        let bucket = TokenBucket::new(fast());
        bucket.wait_one().await;
        bucket.wait_one().await;
        assert!(!bucket.try_one().await, "burst 排空后桶应空");
    }

    #[tokio::test]
    async fn tokens_refill_after_idle() {
        let bucket = TokenBucket::new(fast());
        bucket.wait_one().await;
        bucket.wait_one().await;
        assert!(!bucket.try_one().await, "先排空");
        tokio::time::sleep(Duration::from_millis(10)).await; // 500rps → 10ms 补 5 张，封顶 2
        assert!(bucket.try_one().await, "闲置后桶内应有存量");
    }

    #[tokio::test]
    async fn wait_one_paces_when_the_bucket_is_empty() {
        let bucket = TokenBucket::new(fast());
        bucket.wait_one().await;
        bucket.wait_one().await;
        let start = std::time::Instant::now();
        bucket.wait_one().await; // 节拍等待 ~2ms（1/500rps）
        assert!(
            start.elapsed() >= Duration::from_millis(1),
            "桶空须节拍等待"
        );
    }
}
