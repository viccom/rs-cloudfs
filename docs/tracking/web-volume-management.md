# Web 卷管理面 + 运行时 rebuild 任务跟踪单

> 计划：docs/plans/2026-09-13-web-volume-management.md ｜ K48/K49/K11 修订（计划 §2，收口入档 decisions）｜ worktree：`feat/web-volume-mgmt`（收口 merge 回 main）。
> 基线：main@e7a1543（workspace 955/0/12；winfsp 腿 117/0/1）。

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| P0 | /volumes 双页骨架 + 回调缝 + SHOW + pending 扩列 + current 回退修复 | ✅ | 全部落地：`/volumes` 管理页（volumes.html/js + 四统计卡 + 七列表 + 实例级总用量卡）与单卷说明页（裁决④）；`VolumeCommandClient` 缝（web 类型 + `serve_multi_with_commands` 变体 + cli 装配注入同一 handler Arc）；控制通道 `SHOW <name>`（core `volume_show_json`：显式键单行 JSON，`SECRET_VALUED_KEYS` 只回 `{"set":bool}`）+ `GET /api/volumes/{name}/config`（5s 超时，注册表门控，ERR→404/缝缺→503）；`/api/volumes` 行扩 `pending`（`queue_stats().outstanding()`，Failed=null）；Origin middleware（同源校验挂 `/api/volumes` 族，跨源 403，读/页面路由不拦）；index nav + tabs 行尾「＋」+ localStorage 记 current + current 悬空回退第一个 running。 | 见批次日志 2026-09-13 P0 行 |
| P1 | 卸载 + Enable/Disable + Origin 生效 + allow_remote_admin | ⬜ | — | — |
| P2 | REBUILD 后台化（R1–R6）+ CLI 转发 | ⬜ | — | — |
| P3 | CREATE + 受控 toml 生成 + 新增表单 | ⬜ | — | — |
| P4 | UPDATE + write-only 凭据 + 编辑表单 | ⬜ | — | — |
| P5 | DESTROY 两段确认 + purge_local | ⬜ | — | — |
| P6 | Refresh from remote 按钮 | ⬜ | — | — |

## 批次日志

- 2026-09-13：立项。负责人批准（「认可，请执行实施计划方案（需落库），然后开工」）；方案经三轮设计收敛（web 管理面设计 → sqlite 评估搁置 → rebuild 并入 + R1–R6 约束 → 双页独立布局）；五个小裁决点按推荐落定（计划 §2.3）。前置事实：rebuild 运行时并发已核安全（WAL/busy_timeout、可见性即时、窄竞态由 R2 排空门槛消除——2026-09-12 会话核实）。
- **2026-09-13 P0 完成**（worktree `feat/web-volume-mgmt`，commit `feat(web): /volumes page skeleton, command seam, and SHOW`）：
  - **红→绿时间线**（真红为主，逐条真实输出见 commit 正文与下述证据）：
    - core `volume_show_json` ×2 测试：编译红（`unresolved import ... volume_show_json`，E0432）→ 实现后 `2 passed`；
    - web `volumes_page` ×10：先落最小编译 shim（类型别名 + `serve_multi_with_commands` 构造器存缝不消费，行为零变化）→ **断言红** `9 failed / 1 passed`（绿的是 origin 放行护栏，按设计先绿）→ 实现后 `10 passed`；
    - cli `web_volume_mgmt` ×3：**断言红**（`SHOW` → `ERR: unknown command`；`/api/volumes` 行缺 `pending`：`pending: Null ≠ Number(1)`）→ 实现后 `3 passed`；
    - 补 1 条 green-since-birth 钉子（`/static/js/volumes.js` 经 rust-embed 真实送达，防新静态文件漏嵌入）。
  - **门禁**：workspace `cargo test --no-fail-fast` **971 passed / 0 failed / 12 ignored**（基线 955/0/12 + 新增 16：core 2 + web 11 + cli 3）；clippy `-D warnings` 干净；fmt 干净；`check_layers` OK（12 manifests）；`scan_secrets` OK；winfsp 腿（LIBCLANG_PATH）**117 passed / 0 failed**（= 基线）。
  - **机械适配清单**（编译必需/同构重构，无断言漂移）：web lib.rs 三处 `AppState::Multi { volumes }` 模式补 `..`；cli `add_volume` 内联路径安全预检提取为 `RuntimeVolumeControl::name_is_path_safe`（SHOW 共用，行为等价）；web Cargo.toml tokio 加 `time` feature。既有 `serve_multi` 调用方（multivolume.rs 两处、cli bind 处）**零适配**（新增 `serve_multi_with_commands` 变体，沿 run/run_with_commands 先例）。
  - **前端手动验证**（浏览器 use 主代理专属，本批以 jsdom DOM 断言替代 + 如实标注）：harness 对真实 app.js/volumes.js/模板跑 22 项断言全过——localStorage 恢复选中卷、REMOVE 后 current 悬空回退第一个 running 并触发 reload、tabs 行尾「＋」指向 /volumes、失败卷 chip 禁用带 tooltip；管理页四卡数值（3/2/1/pending=3）、总用量卡聚合、表格行（盘符 `-`、pending `-`、Failed 徽标带原因 tooltip、Actions 列空、Add Volume 禁用占位 title="coming in P3"）。已知 harness 侧坑：jsdom 的 innerText setter 不做字符串 coercion（真浏览器会），比对前 String() 化。真实浏览器视觉面（布局/字体渲染）未验证，待后续批真机窗口顺带复核。
  - **未单测路径**（如实记录）：缝超时 5s 分支无专门单测（5s 预算使测试-hostile；快速路径被 e2e 覆盖；挂起命令的 503 文案在实现中核对）。
  - **风险边界确认**：SHOW 对已摘除卷仍可答（读文件不查注册表——按 §1.2 语义）；但 web 端点先查注册表，摘除后 404（e2e 钉死）——两层语义已在代码注释与本表写明。
