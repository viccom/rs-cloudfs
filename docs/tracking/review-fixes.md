# 审查修复跟踪表（review-fixes）

> 计划：docs/plans/2026-09-11-review-fixes.md ｜ 报告：docs/reports/2026-09-11-phase3-winfsp-enc-review.md（终版·经对抗性复核）
> worktree：`fix/review-batch`（收口 merge 回 main）｜ 探针底稿：`verify/review-findings` worktree `crates/cloudkit-winfsp/tests/verify_probe.rs`（13 测试）
> 基线：workspace 891/0/9、winfsp 腿 94/0/1。

| 批次 | 任务 | 覆盖发现 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|---|
| RB1 | case-rename 修复 + 宽限表失效 | winfsp-C1、winfsp-H1 | ⬜ | — | — |
| RB2 | 命名卫生与失败语义 | winfsp-H4、M1、M2、M3、H2 | ⬜ | — | — |
| RB3 | 集成面与测试防线 | cli-H1、cli-H2、cli-M2、cli-M3、stream-H1 | ⬜ | — | — |
| RB4 | 对齐与加固 | cli-M1、stream-M1、stream-M3、M5、M6、Low 顺手项 | ⬜ | — | — |
| 收口 | 真机冒烟 + K52 入档 + merge/push + 双产物 | — | ⬜ | — | — |

## 推翻项（不修，记录在案）

- winfsp-L4（open 急取读状态，主断言不成立）、winfsp-L5（fetch 有界性成立）、cross-L5（gnu 组合在 winfsp-sys build script 即 panic，产物不可达）。

## 挂账（不在本批）

- 每请求 header RTT + PBKDF2 的 LRU 收敛；fs.rs 拆分；窗口数学五处下沉；Phase 3.6 运行态卷管理（K48-K51 另有计划）。

## 批次日志

- 2026-09-11：立项。三路审查 → 对抗性复核（13 探针 + 27 代码链）→ 终版报告；18 项坐实（4 降级）/ 3 项推翻；修复计划四批 + 跟踪表落盘。待负责人批准开工。
