# Phase 3.6 合入后审查修复批 任务跟踪单

> 审查：2026-09-12 对 `fece74c..6fbd90a`（Phase 3.6 全部改动）四分域深度审查 + 主会话逐条核验。
> 修复范围（负责人 2026-09-12 批准）：H2、H1、H3+M1；M2–M5 与 Low 项未批准（挂账）。
> **追加批准（2026-09-12，worktree `fix/phase36-med`，基线 main@18dde15）：M2/M3/M4 三项**（M5 与 Low 仍挂账）。
> worktree：`fix/phase36-review`（收口 merge 回 main）。基线 main@6fbd90a：workspace 934/0/12、winfsp 腿 117/0/1。

## 审查发现与修复状态

| 级别 | 发现 | 状态 | 修复 commit | 证据摘要 |
|---|---|---|---|---|
| H2 | stale empty-PUT skip 不 bump 终态计数 → 幽灵 outstanding，REMOVE 永久 60s 超时中止（K50 被击穿） | ✅ `7a2050e` | 断言红（`succeeded+degraded==enqueued`：0≠1）→ 绿；`QueueStats::outstanding()` 单点收口（LIST/REMOVE 消费面替换）；degraded 而非新 skipped 计数器（语义同族、无 requeue 副作用、零既有断言破坏） |
| H1 | Ctrl+C 与在途 REMOVE/ADD 交错：entry 孤儿化（无 Drop 兜底/net use 死映射）+ `take()` 双锁 panic 窗口 | ✅ `c8489a6` | take 双锁竞态压力测试 **7/7 复现红**（`removal index should be < len`）→ 单锁绿；REMOVE 排水中触发 gate：红=等满 10s 预算 → 绿=350ms 回 shutdown ERR + entry 放回 + FakeRelease 探针端到端证明停机序清理；gate 后拒新（ADD 回 ERR、LIST 照答）；`InFlightCommands` idle 屏障（RAII + Notify double-check 防 lost-wakeup）+ `ShutdownWatch::fired()` + `MultiVolumeHandle::request_stop()` |
| H3 | 三面 insert 先于挂载（假不变量「从未服务过请求」——axum 并发可路由） | ✅ `86e49a5` | 红：挂载阻塞期间 PROPFIND `/vol/<名>` 207（已可路由）→ 绿：404（先装配后公示）；挂载失败零残留测试 pin 新拆除路径；`rollback_add` 删除吸收为 `tear_down_unpublished`；winfsp 挂载 pass 喂单条目 shadow registry（registry 唯一用途=按 claim 解析 VFS，主会话核证无跨卷冲突检查依赖）；可注入挂载缝 `RuntimeVolumeCommands.mount` |
| M1 | ① handler panic 炸整条通道 ② `exchange_line` 读侧无超时 ③ net use 同步 Command 无界卡 executor | ✅ `68175a8` | ① panic 测试红（第二命令无回复）→ catch_unwind（futures-util 单依赖边）+ `ERR: internal error ... stays up` + 通道存活绿；② 有界交换测试红（挂起→Elapsed）→ 120s `EXCHANGE_BUDGET` + busy 文案 + `exchange_line_bounded` 测试缝；③ `WebDavRelease` spawn_blocking + 30s 预算——**超时分支离线不可测**（真 net use 无法按需挂起），错误路径双测试 pin（lib 单测 + 生产 release 注入 REMOVE 中止），commit 内如实声明 |
| M2 | 排空期间入站写入不断流：活跃写入的卷 outstanding 永不归零，60s 预算耗尽后回「retry once the queue drains」——该场景不可达，误导运维 | ✅ `9eb8233`（fix/phase36-med） | 红：`remove_drain_aborts_early_when_new_uploads_keep_arriving` 等满 5s 预算回通用超时文案（`still has 2 upload(s) ... retry once the queue drains`）→ 绿：排水循环检测相邻 poll 间 enqueued 增长，及早回「uploads are still arriving ... close the programs using the volume」专属 ERR（耗时 << 预算，卷保持注册 K50）；纯超时分支文案补「若仍有程序在写该卷请先关闭」——既有 `remove_with_undrained_queue_aborts_and_keeps_the_volume` 断言（ERR + pending）零改动仍绿；gate 检查（H1）优先级高于增长检测 |
| M3 | 解析错误文本携带凭据值：`load_volume_config` 把 toml/serde 错误 Display 原文装进 `ConfigError::Parse.message`（toml 错误含坏行源文本、serde 含反引号值引用）→ 值随 ADD 回复与 tracing 日志流出（R3 关联红线） | ✅ `093cd4d`（fix/phase36-med） | 红：语法错误测试消息含 `SECRET-MARKER-123`、类型错误测试含 `9999999999012345`（真实错误原文留证）→ 绿：两处 Parse 构造点收口 `redact_credential_values`（凭据键名单出现才脱敏：赋值行值段打码保长度/前后 2 字符、未闭合多行串开号掩到消息尾、serde 反引号值段打码但键名保留；无凭据键消息原文返回）；对照测试 pin 普通语法错误零脱敏 |
| M4 | 全部卷 enabled=false 时 boot 撞裸 bail「no volumes to assemble」（RV0 后首次可达），不提 disabled 不指路 | ✅ `5d68c1b`（fix/phase36-med） | 红：`every_volume_disabled_is_an_actionable_boot_error` 收到裸 bail 文案 → 绿：装配入口（main 经 `run_multi_volume` 走到的同一入口）重扫 `discover_volumes` 计数，回「every volume is disabled: all 2 volume file(s) under `volumes` ... flip the key back to true / remove volumes_dir 走单卷」；lib 侧无文件/无 volumes_dir 形态保留结构后盾文案；`bind_multi_webdav` 过时「theoretically unreachable」注释修正 |

## 未修复挂账（未批准：M5 + Low 全集见 decisions 当日条目）

- M5 测试缺口：控制通道并发语义 / keep-alive 跨摘卷 / 空表行为（rollback_add 缺口已随 H3 的挂载缝补上）

## 批次日志

- 2026-09-12：立项。四分域审查（cli 状态机/协议层/动态分发/config+测试质量）+ 主会话核验：3 High（同一根因族——命令串行化推理被外推到 Ctrl+C 停机任务/axum 请求任务/出表 entry 三个并发世界）+ 5 Medium + Low 若干；两条子代理结论经亲验证伪（大小写 ADD「不可移除卷」不成立——stem 校验先行；`cydrive stop` 客户端挂起不成立——STOP 在连接任务内即时回执，实际挂起的是 status 的 LIST 转发）。
- 2026-09-12：H2→H1→H3+M1 串行落地（三个实现子代理，逐个 diff 审查通过）。执行期事故两起如实记录：①断电致 H1 首派中断（半成品丢弃重置重派，无证据污染）；②主会话建 worktree 时 cwd 残留 examples/config 致 worktree 误落 examples/ 下（git worktree move 复位，主仓状态零污染）——「相对路径命令前先 cd」纪律的又一实例，与已入档的 cargo cwd 教训同源。
- 2026-09-12（fix/phase36-med）：M2→M3→M4 串行落地（TDD 红→绿各留证，commit 9eb8233/093cd4d/5d68c1b）。执行期事项如实记录：①M2 首版红测试漏了 mock `upload_action` 脚本一次性语义（耗尽即 Ok）——第二个上传意外成功致 pending=1，加第二条脚本后红证据干净（pending=2）；②M3/M4 commit 因 fmt 门禁差两行重写（soft reset 重commit，哈希 093cd4d/5d68c1b 替代原 dba923f/83223a0，未推送零影响）。门禁（独立 target）：core 393/0/1、cli 181/0/3、clippy -D warnings 过、fmt 过；workspace 949/0/12（944+5 新测试）。M2 已知未覆盖点：脱敏收口仅在 `load_volume_config` 两构造点（任务范围），`CyDriveConfig::load_toml`（单卷 config.toml，同文件同型暴露面）未动——挂账待负责人裁决。
