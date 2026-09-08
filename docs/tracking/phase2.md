# Phase 2 执行跟踪单（首批驱动三件套 + E2E 硬验收）

> 权威定义 = [../plans/2026-09-08-phase2-execution.md](../plans/2026-09-08-phase2-execution.md)（执行计划 + K1–K18 设计裁决）；
> 设计依据 = [../plans/2026-09-07-cloudfusion-foundation.md](../plans/2026-09-07-cloudfusion-foundation.md) §5 Phase 2；验收依据 = [../standards/driver-onboarding.md](../standards/driver-onboarding.md)。
> 状态图例同 phase0-1.md（⬜ 待做 / 🟨 进行 / ✅ 完成+证据 / ⛔ 阻塞）。**每批收口更新本表并随 commit 提交。**

| # | 任务 | 状态 | 证据/注记 |
|---|---|---|---|
| P2-0 | 前置任务：驱动接入手册 | ✅ | docs/standards/driver-onboarding.md v1.0（2026-09-08；PCFS 研究坑清单 + spike 百度参数引据） |
| P2-1 | Phase 2 执行计划 + 本表细化 | ✅ | docs/plans/2026-09-08-phase2-execution.md（2026-09-08；批次 L/B1/B2/B3a/B3b/E2E/收口 + K1–K18 裁决 + Kickoff 预授权指令；**负责人已批准（2026-09-08），按 Kickoff 开工**） |
| P2-2 | Batch L：ck-local（StorageDriver 面 + conformance 第一公民） | ✅ | 红 ea57136 → 绿 04659d1 → 契约修 8f257e4；ck-local 4 passed（①–⑥⑧ 绿/⑦未声明跳过/能力位九位静态锁/他卷 delete NotFound 契约钉）；workspace 628 passed 0 failed（6 ignored）+ clippy/fmt/check_layers 全过；裁决入 decisions 2026-09-08 Batch L 条目（K6 形态/tokio write_all 入队语义/overwrite stash 等 8 项） |
| P2-3a | Batch B1：ck-baidu 骨架 + OAuth 状态机 + errno 映射 + 元数据面 | ⬜ | 计划 §3；验收 = oauth 三态/errno 逐码回放/表单字节级断言（mock baidu = axum）绿；spike api.rs 改造复用 |
| P2-3b | Batch B2：三步曲上传（差集续传）+ 下载器（dlink 缓存）+ conformance 全绿 | ⬜ | 计划 §4；验收 = 八断言全绿（⑦差集可观测）+ 真机 #[ignore] 跑一次（rtype=3 复核）；spike 附录 B 参数（TTL 60min/4MiB Range/netdisk UA/4 并发） |
| P2-3c | Batch B3a：L2 transport 类型演进（句柄 i32→i64 + RemoteHandle.path + delete_remote 签名） | ⬜ | 计划 §5；验收 = workspace 全绿 + 波及面清单 + telegram 行为零变化（断言漂移审计=仅机械字面量） |
| P2-3d | Batch B3b：组合根接线（backend dispatch/config 三处同步/setup/doctor/namespace 后端感知/rebuild/remote_delete 删除接线） | ⬜ | 计划 §6；验收 = 三后端 mock 全栈装配绿 + telegram 缺省零改动 + WSL 双平台 |
| P2-4 | Batch E2E：三后端硬验收（telegram Y:/baidu Z:/local V: 真机全流程 + sync 收敛 + rebuild 等价 + 多盘并存 + 清理脱敏） | ⬜ | 计划 §7；凭据 = E:\GitHub\rs-CyDrive\test\；产出 docs/reports/2026-09-08-phase2-e2e.md；**telegram 腿前置 = 独立测试 chat（待负责人）** |
| P2-6 | 收口：版本 0.9.0 + 文档联动（README/AGENTS/onboarding）+ decisions 入档 + release 构建验证 | ⬜ | 计划 §8；不部署生产位（部署裁决留负责人） |
| P2-5 | Phase 2.5 多卷启用（Registry + 每实例配置 + 每卷加密 + 多盘挂载） | ⬜ | 裁决 = 方案一（B3 后立即，decisions 2026-09-08）；**P2-6 收口后另立计划**，不在本表执行 |

## 待负责人
1. **E2E 前提供独立测试 chat**（或明示接受生产 chat 污染）——只阻塞 P2-4 的 telegram 腿，不阻塞 baidu/local 腿
2. Phase 2 执行计划评审（P2-1 产出已就绪，批准前不写实现代码）
3. 执行期集中上报项（Kickoff 自主模式下记录于 decisions「待负责人」）：rtype=3 真机复核结果（异常则回退 rtype=1+预删）、正式 appkey 到位后复测 spike §2 限额结论
