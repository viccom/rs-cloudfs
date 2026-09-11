# 审查修复跟踪表（review-fixes）

> 计划：docs/plans/2026-09-11-review-fixes.md ｜ 报告：docs/reports/2026-09-11-phase3-winfsp-enc-review.md（终版·经对抗性复核）
> worktree：`fix/review-batch`（收口 merge 回 main）｜ 探针底稿：`verify/review-findings` worktree `crates/cloudkit-winfsp/tests/verify_probe.rs`（13 测试）
> 基线：workspace 891/0/9、winfsp 腿 94/0/1。

| 批次 | 任务 | 覆盖发现 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|---|
| RB1 | case-rename 修复 + 宽限表失效 | winfsp-C1、winfsp-H1 | ✅ | 2026-09-11 完成 | 红：verify_probe C1×4/H1×2 改断正确行为后 6 失败（ACCESS_DENIED 0xC0000022 / COLLISION 0xC0000035 / 行缓存被删 / 陈旧 EOF 8B≠64B）；绿：winfsp 腿 107/0/1（含翻绿 6 测试）、workspace 892/0/9（=基线 891+webdav case-rename 回归 1）、clippy×2/fmt/check_layers/scan_secrets 全过。C1：case-only 判定移到规范化后（含 from==to 原始相等形态）走合法改名路径 + dest.id 守卫；H1：take_live 行 size 校验不符即弃 + delete_after_cleanup/rename_entry（源与目标）/cleanup 提交成功后 grace.invalidate，提交/删除成功同时清本 handle 读状态防 close 重泊陈旧态；verify_probe.rs 转正式回归（头注更新，门控不变，RB2+ 探针 7 个保持断言未修 BUG）。webdav 面：row 查找字节精确，case-variant 目标不可能命中源行——无需修，补回归测试 rename_case_only_lands_row_and_cache_at_the_new_spelling 钉死。 |
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
