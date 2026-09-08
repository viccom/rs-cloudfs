# Phase 2 执行跟踪单（首批驱动三件套 + E2E 硬验收）

> 权威定义 = [../plans/2026-09-07-cloudfusion-foundation.md](../plans/2026-09-07-cloudfusion-foundation.md) §5 Phase 2 + [../standards/driver-onboarding.md](../standards/driver-onboarding.md)（验收依据）。
> 状态图例同 phase0-1.md。**Phase 2 执行计划（含 Kickoff）待写**——写毕后本表细化任务行。

| # | 任务 | 状态 | 证据/注记 |
|---|---|---|---|
| P2-0 | 前置任务：驱动接入手册 | ✅ | docs/standards/driver-onboarding.md v1.0（2026-09-08；PCFS 研究坑清单 + spike 百度参数引据） |
| P2-1 | Phase 2 执行计划 + 本表细化 | ⬜ | writing-plans 产物，含 ck-local/B1–B3/E2E 批次与验收判据 |
| P2-2 | ck-local（conformance 第一公民，~300 行） | ⬜ | |
| P2-3 | ck-baidu B1 骨架+OAuth → B2 上传下载 → B3 接线 | ⬜ | spike 参数：差集续传/dlink 60–90min/下载三约束/QPS 形态（报告附录 B） |
| P2-4 | 三后端 E2E 硬验收（telegram/baidu/local 真机全流程 + sync 收敛 + 双盘并存） | ⬜ | 凭据 = E:\GitHub\rs-CyDrive\test\；独立测试 chat 待负责人提供 |
| P2-5 | Phase 2.5 多卷启用（Registry + 每实例配置 + 每卷加密 + 多盘挂载） | ⬜ | 裁决 = 方案一（B3 后立即）；PCFS 模式去坑版（decisions 2026-09-08） |

## 待负责人
1. E2E 前提供独立测试 chat（或明示接受生产 chat 污染）
2. Phase 2 执行计划评审（P2-1 产出后）
