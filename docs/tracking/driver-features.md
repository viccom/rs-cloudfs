# 驱动编译开关 任务跟踪单

> 计划：docs/plans/2026-09-09-driver-feature-gates.md ｜ 裁决 K30–K32
> worktree：`feat/driver-features`（收口 merge 回 main）。

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| FT1 | telegram 门控 | ✅ | imports/传输面/连接路径（connect_stack 孪生函数）/push-pull 早退/文案分支 + 测试门控 | b222330；no-default 构建过、telegram 单开过、默认零漂移 |
| FT2 | baidu 门控 | ✅ | Endpoints off 态兜底/params/store impl/probe/枚举/build 臂/setup 菜单与向导/doctor 调用点 + 测试门控 | 8acd2b4；telegram+local 组合过 |
| FT3 | local 门控 + 六组合矩阵 | ✅ | 枚举/build 臂/dispatch 用例；六组合矩阵实测 | 9d00a85；build+clippy×6 全过，全量 test default=809 零漂移 / none 过 |
| FT4 | version/CI/文档/双产物/收口 | ✅ 完成 | commit c2e23bf：--version 驱动清单（compiled_drivers() const fn + clap version_line OnceLock，三组合实跑 telegram,baidu,local / local / none）；CI 增 features job（clippy×4 腿 + workspace none test 腿，check job 一字未动；矩阵腿当场抓出 FT3 遗留死导入已修）；README 裁剪构建节/AGENTS/onboarding §1 三件套/decisions K30–K32 入档。双产物交付与 merge/push 主会话执行 | 默认 810 passed/0 failed（809+1 新测试）；全关 801 passed/0 failed；clippy/fmt/check_layers/scan_secrets 全过；--version 三组合真实输出见 decisions |

## 批次日志

- 2026-09-09：立项。Explore 盘点完成（触点档案见计划），计划与跟踪单落盘。
- 2026-09-09：FT1–FT3 三批合入（b222330 → 8acd2b4 → 9d00a85），三驱动 feature 门控完成；六组合 build+clippy 实测全过，默认组合 809 零漂移。
- 2026-09-10：FT4 实现批（主会话收口前）——K32 `--version` 驱动清单（`compiled_drivers()` 红→绿，红=E0425 编译红）；CI `features` job（clippy×4 腿 + workspace `--no-default-features` test 腿）；README/AGENTS/onboarding §1/decisions K30–K32 文档联动。现场捕获并修 FT3 遗留死导入（multivolume_ops.rs 裸 `load_volumes`，同门 cfg 吸收）。实测：default workspace 810/0（809 零漂移+1 新测试）、no-default workspace 801/0、cli no-default 134/0、clippy 四腿+workspace、fmt、check_layers、scan_secrets 全绿；`--version` 三组合实跑（telegram, baidu, local / local / none）。**FT4 行留主会话收口（双产物交付 + merge + push）。**
