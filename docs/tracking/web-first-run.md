# Web 首启引导（first-run bootstrap）任务跟踪单

> 计划：`docs/plans/2026-09-23-web-first-run.md` ｜ 需求口径：负责人 2026-09-23「开工。程序启动后，用户通过web界面可以从零开始增加配置。程序当前本身也是支持动态增删改存储卷的配置的」
> 基线：main@fc5df7e（Phase 8 + 8-B 已合入，workspace 1673/0/62）
> worktree：`feat/web-first-run`，独立 target；hub-and-spoke；TDD 红→绿；不 push、合入另令
> 编号：裁决 K87（收口时入 decisions）

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| FR0 | 计划落档（本文 + 计划文档，探查锚点入 §0） | ✅ 2026-09-23 | 三处关键修正：默认端口 8080/8088（非 8485/8486）；零卷拒绝两道门（core discovery + cli boot）；自动开浏览器零先例需新函数 | 本批日志 |
| FR1 | cli boot：bootstrap_first_run_cwd + discover_first_run_config + 门 B 放行 + 横幅/浏览器 | ⬜ 待开工 | — | — |
| FR2 | web 空态引导文案（i18n en/zh + pin） | ⬜ 待开工 | — | — |
| FR3 | 文档收口：K87 + README/AGENTS + 五门禁 + 组合 | ⬜ 待开工 | — | — |

## 批次日志

### FR0（2026-09-23，主会话）

探查代理（Explore）拿齐九项锚点；计划落档。关键设计裁决 D1–D6 见计划文档（触发门 = cwd 无任何配置文件；生成物 = 手写模板 + write_config_atomically；两道门放行 = 新 discovery 变体 + first_run 参数，既有函数零改动、既有调用点机械补参）。

### 端口裁决 + FR2（2026-09-23，主会话直做——子代理撞平台限额窗口）

- **端口裁决**：负责人指令「默认端口改为 8485/8486」——**全局默认**（非仅 init 模板）：core `Default` impl（8080/8088→8485/8486）+ setup --multi 模板 + 两份 examples 注释 + README/usage 文档 + core 测试钉三处，TDD 红（2 FAILED：default_matches_python_config / env_override_invalid_numbers）→ 绿（`cargo test -p cloudkit-core --test config` 39/0）。commit `c6e2590`。
- **FR2 空态文案**：`volumes.empty` en/zh 改引导语（指向「＋ 添加卷」）+ 新 pin 测试 `empty_volume_state_copy_points_at_add_volume`（fetch i18n.js 断言新文案在/旧文案删），红→绿，volumes_page **42/0**。commit `bb56349`。
- 注：首次派发的 FR1/FR2 代理在探查期撞平台 5 小时限额（22:05:06 重置）零残留；FR2 改由主会话直做（≤10 行），FR1 重派。

## 风险与未覆盖（随批更新）

- 控制通道在空注册表的 bind 形态 FR1 实现时核实
- 浏览器自动打开不做自动化断言（spawn 副作用），仅钉双平台可编译（R5）
- v1 不做：多步向导 / 非 loopback 引导 / 端口自动改选
