# K58 审查修复批 任务跟踪单（High×5 + Medium×8）

> 审查：2026-09-14 对 6fbd90a..7e1b2be（K55/K56 修复批 + K57 卷管理面 + UI 批 Rust 触点）四分域深度审查 + 主会话亲验。
> 修复范围（负责人批准）：High 全部 + Medium 全部；Low 挂账不动（择便顺带需注明）。
> worktree：`fix/k58-review`（收口 merge 回 main）。基线 main@7e1b2be：workspace 1045/0/12、winfsp 腿 117/0/1。

## 批次与发现映射

| 批 | 内容 | 修复项 | 状态 | 证据 |
|---|---|---|---|---|
| FA | core 原子写原语 + 脱敏补漏 | H3（temp+sync+rename 三写盘点+save_toml）、M2（proxy_url/sync_url 入 SECRET_VALUED_KEYS）、M5（UPDATE 重读经脱敏） | ✅ 2026-09-12 | 红：M2 两测断言红（SHOW 原样出 proxy_url 值 / parse 错误引文泄 userinfo）+ H3/M5 编译红（E0432/E0603/E0425）→ 绿 8 新测；门禁 test 642/0/4 + clippy -D warnings 过 + fmt clean（core+cli）；web volumes_page 36/0/0 加验（SHOW 面经 seam 实测） |
| FB | 命令串行化 + 取消安全 | H1+H2（handler 闭包 spawn+oneshot+单 permit——web 超时只弃等待；全入口恢复 K48 串行；虚假注释修正）、M7（add_volume 结构化成败替代文本前缀匹配） | ✅ 2026-09-12 | 红：H1 e2e（web 客户端 abort→卷楔死，LIST 永列 stuck）+ H2 e2e（并发 REMOVE×2 第二个撞结构 ERR `no live runtime state`）+ panic e2e（web 腿裸连接 reset，空回复）+ M7 三处 E0308 编译红 → 绿 3 e2e + 1 单元；门禁 test 727/0/4 + clippy -D warnings 过 + fmt clean（cli+web+core） |
| FC | web 安全 + 可用性 | H5（Host 白名单防 rebinding + /api/upload 挂 guard 收口存量 CSRF）、M3（Edit 门撤除——disabled 卷可拉 SHOW）、M6（前端 action-in-flight 锁防轮询重绘击穿） | ⬜ | — |
| FD | REBUILD 加固 | M4a（R5 检查点实例身份判据）、M4b（checkpoint 复查 outstanding） | ⬜ | — |
| FE | worker panic 隔离 | M1（process_job catch_unwind + panic 补 degraded 终态——幽灵 outstanding 消除） | ⬜ | — |

复核（负责人要求）：每批主会话 diff 审查 + 收口全门禁 + 逐项针对性复核验证，结论入下表。

| 项 | 复核结论 |
|---|---|
| （收口时填） | — |

## 批次日志

- 2026-09-14：审查完成（四分域 + 主会话亲验 5 High；裁决子代理矛盾：卷管理 POST 实为 axum 默认 2MB 上限——1900MiB DoS 项剔除）。立项本批。
- 2026-09-12（FA）：H3/M2/M5 落地。**H3**：core 新 `write_config_atomically`（config.rs pub fn——同目录 `.<name>.tmp` 写前删残留 → write_all → sync_all → rename 原子替换，rename 失败删临时返错）；五写盘点收敛：core `write_volume_enabled`/`save_toml`/`save_toml_scrubbed` + cli CREATE/UPDATE（save_toml×2 为审查建议顺带，config.toml 撕裂同病）。原子性可测取舍：Windows 只读位对目录不阻止文件创建、ACL 不可经 std——写阶段失败以「临时路径被目录占据」构造（原文件逐字节不动）、rename 阶段失败以「目标为目录」构造（临时文件清理断言），五种真实 fs 态全覆盖，无需注入式拆分。**M2**：`proxy_url`/`sync_url` 入 `SECRET_VALUED_KEYS`（URL 可携 userinfo）——SHOW 折叠/redact 掩值自动跟进（同表驱动）；前端最小修补：volumes.js `vf-sync-url` 从 plain 预填组移入 writeOnly 占位符组（否则 `{"set":true}` 对象预填渲染为 [object Object]）；proxy_url 无表单字段无需前端改动。既有测试无 proxy_url/sync_url 的 SHOW 值断言——零机械适配。**M5**：`redact_credential_values` 提 pub（doc 注单一漏斗原则），cli UPDATE 重读抽 `parse_explicit_table_redacted`（路径文案不变，错误经漏斗）。测试：core 新 8（H3×5 + M2×2 + M5 直连×1）+ cli 新 1。
- 2026-09-12（FB）：H1+H2+M7 落地（新 `tests/command_serialization.rs` + lib 内单元）。**H1（取消安全）**：`volume_command_handler` 闭包改 `tokio::spawn` 执行任务 + oneshot 回复——闭包返回的 future 只 `rx.await`；web 路由预算耗尽/连接断开只 drop 等待端，命令跑到自然结束（与控制通道客户端放弃语义对齐）；`InFlightGuard` 移入执行任务——idle 屏障覆盖真实执行全程（stop task `wait_idle` 仍覆盖每个 mid-K50 entry 持有，原 H1 不变量不动）。红测试构造形态（如实）：web 路由预算是常量不可注入，用「spawn HTTP 客户端任务 → drain 挂起窗口（RateLimited 5s 同 M5 手法）中 abort 客户端任务」——hyper 1.11 实测会 drop 在途 handler future（红实证：abort 后 LIST 永列 stuck，楔死；绿：~5s 后命令自然完成、卷消失、shutdown 干净）。**H2（全入口串行）**：执行任务先取 `command_gate`（选 `tokio::sync::Mutex<()>` 单 permit——guard 语义直白「持有即串行」，无 owned-permit 边角；死锁论证入代码注释：任务先持 gate 再只在 surface.handle 内 await、无路径回流本闭包（DISABLE/UPDATE/DESTROY 直调方法不走闭包）；等待侧（accept loop/web handler）不持任何锁等 oneshot——等待图是围绕单一 mutex 的扁平星形，无环；stop 的 idle 屏障等 guard 不等 gate，排队中的命令轮到即命中 `watch.fired()` 首查、一个 poll tick 内答完释放 guard——`wait_idle` 有界）。红实证：并发 web REMOVE×2 第二个撞 `no live runtime state` 结构 ERR；绿：一成一拒（拒者得 `no volume registered`）。control 命令臂语义零漂移（M5 characterization「PING 也不回」钉住——accept loop 仍 await 一个回复才收下一条）。**panic 防护层次重梳**：真正防护移入执行任务（AssertUnwindSafe+catch_unwind+tracing 记 payload，回 `HANDLER_PANIC_REPLY`）；control.rs 原 catch 保留为纵深（其 M1 测试自注入 panic handler 直测该层，零漂移）；`HANDLER_PANIC_REPLY` 提 pub(crate) 双面共用同文案。红实证：web 腿裸连接 reset（空回复）；绿：web 409 + 同款 ERR 文案 + 双面存活。**M7**：`add_volume` 返回 `(String, bool)`（bool=装配+挂载 verdict；选 tuple 而非 Result——ADD/ENABLE 直取 `.0` 零适配，CREATE/UPDATE 按 bool 分类、strip 降为纯展示糖（漂移时回退整段 trimmed 文本））；三处调用方机械适配。红=三处 E0308 编译红（文案未漂移，行为红不可钉——按任务预案降级为直接单元测 bool 三态（成功/dispatch 失败/挂载回滚），**特征测试性质如实入测试注释**：文案不可注入，漂移免疫本身无法以改写文案方式钉死）。**注释修正**：lib.rs 装配块「funnel into the same serialized execution」假陈述、run_multi/RuntimeVolumeControl/rebuild×2/停机 barrier 注释、control.rs 模块头/类型 doc/命令臂注释——全部按 spawn+permit 真实机制重写。测试：cli 新 3 e2e + 1 单元；全存活硬门禁：runtime_volumes 20 / control_channel 11 / web_volume_mgmt 7 / volume_create_update 11 / volume_destroy 9 / rebuild 3 / runtime_rebuild 7 全绿。
