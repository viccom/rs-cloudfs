# Web 首启引导（first-run bootstrap）任务跟踪单

> 计划：`docs/plans/2026-09-23-web-first-run.md` ｜ 需求口径：负责人 2026-09-23「开工。程序启动后，用户通过web界面可以从零开始增加配置。程序当前本身也是支持动态增删改存储卷的配置的」
> 基线：main@fc5df7e（Phase 8 + 8-B 已合入，workspace 1673/0/62）
> worktree：`feat/web-first-run`，独立 target；hub-and-spoke；TDD 红→绿；不 push、合入另令
> 编号：裁决 K87（收口时入 decisions）

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| FR0 | 计划落档（本文 + 计划文档，探查锚点入 §0） | ✅ 2026-09-23 | 三处关键修正：默认端口 8080/8088（非 8485/8486）；零卷拒绝两道门（core discovery + cli boot）；自动开浏览器零先例需新函数 | 本批日志 |
| FR1 | cli boot：bootstrap_first_run_cwd + discover_first_run_config + 门 B 放行 + 横幅/浏览器 | ✅ 2026-09-23 | 实现代理完成大部（网络断连后主会话接手验证收尾）：lib.rs +187（模板/bootstrap/discovery 变体/open_browser 双 cfg/门 B 条件化/all_failed 空真皮/横幅/浏览器移交）+ main.rs +27（run() 分叉 + first_run 透传）+ 钉测五条（模板逐字节 pin/bootstrap 三臂/discovery）+ first_run_e2e 三腿（空卷 boot web 200 / false 臂既有 bail 钉 / 全链 POST 建卷落盘）+ 八个既有测试文件机械补 false | cli **259/0/12**；commit `2883f1e` |
| FR2 | web 空态引导文案（i18n en/zh + pin） | ✅ 2026-09-23 | 主会话直做：volumes.empty 双语改引导语 + pin 测试（fetch i18n.js 断言新在旧删），红→绿 | volumes_page **42/0**；commit `bb56349` |
| FR3 | 文档收口：K87 + README/AGENTS + 五门禁 + 组合 | ✅ 2026-09-23 | decisions K87 + README 快速开始「零配置开箱」行 + AGENTS 当前阶段 K87 块 + 计数 1673→**1682** + 五门禁 + 组合 clippy | 本节日志 |

## 批次日志

### FR0（2026-09-23，主会话）

探查代理（Explore）拿齐九项锚点；计划落档。关键设计裁决 D1–D6 见计划文档（触发门 = cwd 无任何配置文件；生成物 = 手写模板 + write_config_atomically；两道门放行 = 新 discovery 变体 + first_run 参数，既有函数零改动、既有调用点机械补参）。

### 端口裁决 + FR2（2026-09-23，主会话直做——子代理撞平台限额窗口）

- **端口裁决**：负责人指令「默认端口改为 8485/8486」——**全局默认**（非仅 init 模板）：core `Default` impl（8080/8088→8485/8486）+ setup --multi 模板 + 两份 examples 注释 + README/usage 文档 + core 测试钉三处，TDD 红（2 FAILED：default_matches_python_config / env_override_invalid_numbers）→ 绿（`cargo test -p cloudkit-core --test config` 39/0）。commit `c6e2590`。
- **FR2 空态文案**：`volumes.empty` en/zh 改引导语（指向「＋ 添加卷」）+ 新 pin 测试 `empty_volume_state_copy_points_at_add_volume`（fetch i18n.js 断言新文案在/旧文案删），红→绿，volumes_page **42/0**。commit `bb56349`。
- 注：首次派发的 FR1/FR2 代理在探查期撞平台 5 小时限额（22:05:06 重置）零残留；FR2 改由主会话直做（≤10 行），FR1 重派。

## 终态（FR3 收口，2026-09-23）

- **workspace 1682/0/62**（-j 2；workspace 冷构建遇 K73 内存陷阱 os error 1455 一次，降并发定向 clean 后过）+ clippy/fmt/check_layers/scan_secrets 绿 + baidu/telegram 组合 clippy 绿。
- 分支 `feat/web-first-run` HEAD 待推/待合入指令。

## 风险与未覆盖（随批更新）

- 控制通道在空注册表的 bind 形态 FR1 实现时核实
- 浏览器自动打开不做自动化断言（spawn 副作用），仅钉双平台可编译（R5）
- v1 不做：多步向导 / 非 loopback 引导 / 端口自动改选
