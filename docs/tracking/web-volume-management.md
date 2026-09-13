# Web 卷管理面 + 运行时 rebuild 任务跟踪单

> 计划：docs/plans/2026-09-13-web-volume-management.md ｜ K48/K49/K11 修订（计划 §2，收口入档 decisions）｜ worktree：`feat/web-volume-mgmt`（收口 merge 回 main）。
> 基线：main@e7a1543（workspace 955/0/12；winfsp 腿 117/0/1）。

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| P0 | /volumes 双页骨架 + 回调缝 + SHOW + pending 扩列 + current 回退修复 | ⬜ | — | — |
| P1 | 卸载 + Enable/Disable + Origin 生效 + allow_remote_admin | ⬜ | — | — |
| P2 | REBUILD 后台化（R1–R6）+ CLI 转发 | ⬜ | — | — |
| P3 | CREATE + 受控 toml 生成 + 新增表单 | ⬜ | — | — |
| P4 | UPDATE + write-only 凭据 + 编辑表单 | ⬜ | — | — |
| P5 | DESTROY 两段确认 + purge_local | ⬜ | — | — |
| P6 | Refresh from remote 按钮 | ⬜ | — | — |

## 批次日志

- 2026-09-13：立项。负责人批准（「认可，请执行实施计划方案（需落库），然后开工」）；方案经三轮设计收敛（web 管理面设计 → sqlite 评估搁置 → rebuild 并入 + R1–R6 约束 → 双页独立布局）；五个小裁决点按推荐落定（计划 §2.3）。前置事实：rebuild 运行时并发已核安全（WAL/busy_timeout、可见性即时、窄竞态由 R2 排空门槛消除——2026-09-12 会话核实）。
