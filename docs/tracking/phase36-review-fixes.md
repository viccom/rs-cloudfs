# Phase 3.6 合入后审查修复批 任务跟踪单

> 审查：2026-09-12 对 `fece74c..6fbd90a`（Phase 3.6 全部改动）四分域深度审查 + 主会话逐条核验。
> 修复范围（负责人 2026-09-12 批准）：H2、H1、H3+M1；M2–M5 与 Low 项未批准（挂账）。
> worktree：`fix/phase36-review`（收口 merge 回 main）。基线 main@6fbd90a：workspace 934/0/12、winfsp 腿 117/0/1。

## 审查发现与修复状态

| 级别 | 发现 | 状态 | 修复 commit | 证据摘要 |
|---|---|---|---|---|
| H2 | stale empty-PUT skip 不 bump 终态计数 → 幽灵 outstanding，REMOVE 永久 60s 超时中止（K50 被击穿） | ✅ `7a2050e` | 断言红（`succeeded+degraded==enqueued`：0≠1）→ 绿；`QueueStats::outstanding()` 单点收口（LIST/REMOVE 消费面替换）；degraded 而非新 skipped 计数器（语义同族、无 requeue 副作用、零既有断言破坏） |
| H1 | Ctrl+C 与在途 REMOVE/ADD 交错：entry 孤儿化（无 Drop 兜底/net use 死映射）+ `take()` 双锁 panic 窗口 | ✅ `c8489a6` | take 双锁竞态压力测试 **7/7 复现红**（`removal index should be < len`）→ 单锁绿；REMOVE 排水中触发 gate：红=等满 10s 预算 → 绿=350ms 回 shutdown ERR + entry 放回 + FakeRelease 探针端到端证明停机序清理；gate 后拒新（ADD 回 ERR、LIST 照答）；`InFlightCommands` idle 屏障（RAII + Notify double-check 防 lost-wakeup）+ `ShutdownWatch::fired()` + `MultiVolumeHandle::request_stop()` |
| H3 | 三面 insert 先于挂载（假不变量「从未服务过请求」——axum 并发可路由） | ✅ `86e49a5` | 红：挂载阻塞期间 PROPFIND `/vol/<名>` 207（已可路由）→ 绿：404（先装配后公示）；挂载失败零残留测试 pin 新拆除路径；`rollback_add` 删除吸收为 `tear_down_unpublished`；winfsp 挂载 pass 喂单条目 shadow registry（registry 唯一用途=按 claim 解析 VFS，主会话核证无跨卷冲突检查依赖）；可注入挂载缝 `RuntimeVolumeCommands.mount` |
| M1 | ① handler panic 炸整条通道 ② `exchange_line` 读侧无超时 ③ net use 同步 Command 无界卡 executor | ✅ `68175a8` | ① panic 测试红（第二命令无回复）→ catch_unwind（futures-util 单依赖边）+ `ERR: internal error ... stays up` + 通道存活绿；② 有界交换测试红（挂起→Elapsed）→ 120s `EXCHANGE_BUDGET` + busy 文案 + `exchange_line_bounded` 测试缝；③ `WebDavRelease` spawn_blocking + 30s 预算——**超时分支离线不可测**（真 net use 无法按需挂起），错误路径双测试 pin（lib 单测 + 生产 release 注入 REMOVE 中止），commit 内如实声明 |

## 未修复挂账（未批准，M2–M5 + Low 全集见 decisions 当日条目）

- M2 排空期间入站写入不断流（活跃写入的卷实际不可 REMOVE，ERR 文案不可达成）
- M3 ADD 解析失败 ERR 可能携带凭据值片段（toml 错误 Display 含源行）
- M4 全部卷 enabled=false 时 boot bail 文案不可行动
- M5 测试缺口：控制通道并发语义 / keep-alive 跨摘卷 / 空表行为（rollback_add 缺口已随 H3 的挂载缝补上）

## 批次日志

- 2026-09-12：立项。四分域审查（cli 状态机/协议层/动态分发/config+测试质量）+ 主会话核验：3 High（同一根因族——命令串行化推理被外推到 Ctrl+C 停机任务/axum 请求任务/出表 entry 三个并发世界）+ 5 Medium + Low 若干；两条子代理结论经亲验证伪（大小写 ADD「不可移除卷」不成立——stem 校验先行；`cydrive stop` 客户端挂起不成立——STOP 在连接任务内即时回执，实际挂起的是 status 的 LIST 转发）。
- 2026-09-12：H2→H1→H3+M1 串行落地（三个实现子代理，逐个 diff 审查通过）。执行期事故两起如实记录：①断电致 H1 首派中断（半成品丢弃重置重派，无证据污染）；②主会话建 worktree 时 cwd 残留 examples/config 致 worktree 误落 examples/ 下（git worktree move 复位，主仓状态零污染）——「相对路径命令前先 cd」纪律的又一实例，与已入档的 cargo cwd 教训同源。
