//! D4 限流落值（K69.3 实测裁决）：全局令牌桶 + 770004 硬退避状态机。
//!
//! 实测依据（decisions K69.3，2026-09-16）：
//!
//! - 持续 ~4 rps 可行、5 rps 下 10 秒内 22% 请求被拒；触发后**整账号**
//!   （跨端点族，user/info 同封）返回 `770004`，封锁窗 **≥10 分钟**
//!   （实测 10 分钟未解除即停止探测，保守按 ≥10min 设计）；
//! - OpenList 官方驱动自限 1 rps——本驱动取同款保守值；
//! - **落值**：令牌桶缺省 1 rps（容量 2 = 允许 1 次突发重试）；
//!   `770004` → 本地硬退避（初值 300s，指数 ×2，上限 1800s——比实测
//!   下界 10min 长一档起步，封顶 30min，风暴绝无重试机会）。
//!
//! 原语三件：
//!
//! - [`RateLimiter::check_wait`]：取一张发送许可——令牌桶节拍等待；
//!   处于 770004 封锁窗内则**立即**拒绝（返回剩余时长，绝不把封锁窗
//!   sleep 掉：调用方以 `RateLimited` 上抛，风暴重试在封锁期内一个
//!   请求也不发）；
//! - [`RateLimiter::report_limit`]：一个 `770004` 到达——封锁窗按
//!   指数阶梯推进并返回本次窗口；
//! - [`RateLimiter::report_ok`]：一个成功请求——重置退避阶梯（成功
//!   即限流状态解除；封锁窗过期后的放行同样重置——恢复语义双入口，
//!   幂等）。
//!
//! 可测性：毫秒级参数注入（[`LimiterConfig::fast`] 形态 = RebuildTuning
//! 先例——结构注入而非时钟替身）；时间原语用 tokio::time（异步等待
//! 不占阻塞线程）。

use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

/// 限流器参数（生产缺省 = K69.3 落值；测试经 [`LimiterConfig::fast`]
/// 注入毫秒级窗口）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LimiterConfig {
    /// 令牌补充速率（令牌/秒；生产缺省 1.0）。
    pub rate_per_sec: f64,
    /// 桶容量 = 允许的突发许可数（生产缺省 2）。
    pub burst: u32,
    /// 770004 封锁窗初值（生产缺省 300s）。
    pub block_initial: Duration,
    /// 封锁窗上限（指数 ×2 封顶；生产缺省 1800s）。
    pub block_max: Duration,
}

impl Default for LimiterConfig {
    fn default() -> Self {
        LimiterConfig {
            rate_per_sec: 1.0,
            burst: 2,
            block_initial: Duration::from_secs(300),
            block_max: Duration::from_secs(1800),
        }
    }
}

impl LimiterConfig {
    /// 毫秒级窗口（测试专用，绝不用于生产）。
    pub fn fast() -> Self {
        LimiterConfig {
            rate_per_sec: 50.0,
            burst: 2,
            block_initial: Duration::from_millis(40),
            block_max: Duration::from_millis(160),
        }
    }
}

/// 桶与退避阶梯的可变状态（Mutex 保护；等待发生在锁外）。
#[derive(Debug)]
struct LimiterState {
    /// 当前令牌数（浮点：按流逝时间连续补充）。
    tokens: f64,
    /// 上次补充时刻。
    last_refill: Instant,
    /// 770004 封锁窗截止时刻；`None` = 未封锁。
    blocked_until: Option<Instant>,
    /// 下一次 `report_limit` 的封锁窗时长（指数阶梯的当前档）。
    block_next: Duration,
}

impl LimiterState {
    /// 按流逝时间补充令牌（封顶容量）。
    fn refill(&mut self, cfg: &LimiterConfig, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * cfg.rate_per_sec).min(cfg.burst as f64);
        self.last_refill = now;
    }

    /// 封锁窗是否已过期；过期则清封锁并重置退避阶梯（恢复双入口之一）。
    fn expire_block(&mut self, now: Instant) -> bool {
        match self.blocked_until {
            Some(until) if now >= until => {
                self.blocked_until = None;
                self.block_next = Duration::ZERO; // 归零 = 下次 report 从 initial 起步
                true
            }
            _ => self.blocked_until.is_none(),
        }
    }
}

/// 全局限流器（api client 持有并共享——「全局」指**每卷一个**，跨端点
/// 族生效，与 770004 的整账号封锁语义对齐）。
///
/// 并发：全方法可并发调用（`&self` + 内部 Mutex）；等待（节拍 sleep）
/// 发生在锁外，一个等待者不阻塞其他许可的推进。
pub struct RateLimiter {
    cfg: LimiterConfig,
    state: Mutex<LimiterState>,
}

impl RateLimiter {
    /// 以给定参数构造（桶满——进程启动即允许初始突发）。
    pub fn new(cfg: LimiterConfig) -> Self {
        let now = Instant::now();
        RateLimiter {
            cfg,
            state: Mutex::new(LimiterState {
                tokens: cfg.burst as f64,
                last_refill: now,
                blocked_until: None,
                block_next: Duration::ZERO,
            }),
        }
    }

    /// 取一张发送许可：
    ///
    /// - 未封锁且桶内有令牌 → `Ok(())`（扣一）；
    /// - 未封锁但桶空 → 节拍等待（锁外 sleep 到下一张令牌可扣）后 `Ok`；
    /// - 处于 770004 封锁窗 → **立即** `Err(剩余时长)`——绝不等待，
    ///   调用方以此上抛 `RateLimited`（硬退避：封锁期内零请求）。
    pub async fn check_wait(&self) -> Result<(), Duration> {
        loop {
            let wait = {
                let mut state = self.state.lock().await;
                let now = Instant::now();
                state.refill(&self.cfg, now);
                if !state.expire_block(now) {
                    // 仍封锁：立即拒绝并携带剩余窗
                    let until = state
                        .blocked_until
                        .expect("expire_block false implies blocked");
                    return Err(until.saturating_duration_since(now));
                }
                if state.tokens >= 1.0 {
                    state.tokens -= 1.0;
                    return Ok(());
                }
                // 桶空：等到下一张令牌（锁外 sleep——不阻塞并发许可）
                Duration::from_secs_f64((1.0 - state.tokens) / self.cfg.rate_per_sec)
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// 上报一个 `770004`：封锁窗按指数阶梯推进（`initial → ×2 → … →
    /// max` 封顶），返回本次窗口时长。
    pub async fn report_limit(&self) -> Duration {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        // 阶梯推进：ZERO（未封锁/已重置）→ initial；否则 ×2 封顶
        let window = if state.block_next.is_zero() {
            self.cfg.block_initial
        } else {
            state.block_next.saturating_mul(2).min(self.cfg.block_max)
        };
        state.block_next = window;
        let until = now + window;
        // 连续上报只延长不缩短（乱序到达的上报不提前解锁）
        state.blocked_until = Some(state.blocked_until.map_or(until, |u| u.max(until)));
        window
    }

    /// 上报一个成功请求：重置退避阶梯（成功 = 限流状态解除；不解除
    /// 已开启的封锁窗——那是时间到才开的门，`report_ok` 只影响下一次
    /// `report_limit` 的起步档）。
    pub async fn report_ok(&self) {
        self.state.lock().await.block_next = Duration::ZERO;
    }

    /// 当前封锁窗剩余时长（`None` = 未封锁）——诊断/测试观测面。
    pub async fn blocked_remaining(&self) -> Option<Duration> {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        state.refill(&self.cfg, now);
        if !state.expire_block(now) {
            let until = state
                .blocked_until
                .expect("expire_block false implies blocked");
            Some(until.saturating_duration_since(now))
        } else {
            None
        }
    }
}
