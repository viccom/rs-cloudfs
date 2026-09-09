# Phase 2 执行跟踪单（首批驱动三件套 + E2E 硬验收）

> 权威定义 = [../plans/2026-09-08-phase2-execution.md](../plans/2026-09-08-phase2-execution.md)（执行计划 + K1–K18 设计裁决）；
> 设计依据 = [../plans/2026-09-07-cloudfusion-foundation.md](../plans/2026-09-07-cloudfusion-foundation.md) §5 Phase 2；验收依据 = [../standards/driver-onboarding.md](../standards/driver-onboarding.md)。
> 状态图例同 phase0-1.md（⬜ 待做 / 🟨 进行 / ✅ 完成+证据 / ⛔ 阻塞）。**每批收口更新本表并随 commit 提交。**

| # | 任务 | 状态 | 证据/注记 |
|---|---|---|---|
| P2-0 | 前置任务：驱动接入手册 | ✅ | docs/standards/driver-onboarding.md v1.0（2026-09-08；PCFS 研究坑清单 + spike 百度参数引据） |
| P2-1 | Phase 2 执行计划 + 本表细化 | ✅ | docs/plans/2026-09-08-phase2-execution.md（2026-09-08；批次 L/B1/B2/B3a/B3b/E2E/收口 + K1–K18 裁决 + Kickoff 预授权指令；**负责人已批准（2026-09-08），按 Kickoff 开工**） |
| P2-2 | Batch L：ck-local（StorageDriver 面 + conformance 第一公民） | ✅ | 红 ea57136 → 绿 04659d1 → 契约修 8f257e4；ck-local 4 passed（①–⑥⑧ 绿/⑦未声明跳过/能力位九位静态锁/他卷 delete NotFound 契约钉）；workspace 628 passed 0 failed（6 ignored）+ clippy/fmt/check_layers 全过；裁决入 decisions 2026-09-08 Batch L 条目（K6 形态/tokio write_all 入队语义/overwrite stash 等 8 项） |
| P2-3a | Batch B1：ck-baidu 骨架 + OAuth 状态机 + errno 映射 + 元数据面 | ✅ | 红 9a969ac（12 文件骨架 + MockBaidu + 17 测试红于 Unsupported）→ 绿（本 commit）；ck-baidu 17 passed（oauth 三态+持久化 4 / errno 逐码回放 5 / 表单字节级断言 8）；workspace 646 passed 0 failed（6 ignored）+ clippy/fmt/check_layers/scan_secrets 全过；裁决 = delete 句柄走 meta fs_ids 直查（少一份驱动状态）、-8 无 PCFS 证据按「mock 建模+B2 复核」入表、K15 退避固定 80ms、rename ondup=overwrite 显式声明待 B2 断言⑥复核 |
| P2-3b | Batch B2：三步曲上传（差集续传）+ 下载器（dlink 缓存）+ conformance 全绿 | ✅ | 红 a21114a → 绿 3696fe6 → 真网返工三轮（31363 到齐即传 c6c22de + ⑦ 场景中立化 ef821e7 + meta 全废转 list ddbade4 + 目录 create 预检 690834a）；ck-baidu 42 passed（conformance ①–⑧ 含 ⑦差集）+ 真机 #[ignore] 3/3（rtype=3 覆盖无副本=K10 复核通过；100MB 吞吐 6.5↑/2.8↓ MB/s，下行低为顺序实现+网络波动，spike qps 复跑零拒绝排除限额形态，B3b 4 并发优化）；远端测试根零遗留；workspace 671 passed 0 failed（9 ignored）；**五项真网实证入 decisions**（uk 字段/31363 会话锁定/meta 31300 无权限/目录 create 冲突重命名/索引延迟） |
| P2-3c | Batch B3a：L2 transport 类型演进（句柄 i32→i64 + RemoteHandle.path + delete_remote 签名） | ✅ | 红 933f45e（护栏 2 条编译红：E0560/E0609 path 字段 ×6 + E0308 delete_remote(&handle) ×2）→ 绿 8f96b07（单 commit 闭合全部 breaking 波及，18 文件）；波及面清单全文入 commit 正文（216 rg 命中：trait 定义 1 / 实现 3+6 stub / 生产消费 vfs+upload_queue / 测试机械面）；断言漂移审计 = 非机械改动 0（path:None ×15、签名 i32→i64 ×2、转换包装删除 ×17、K3 调用形状 ×4，逐项留档）；语义决策 = mock delete 多 chunk all-or-nothing（单 chunk 与旧语义等价，contract 11 断言零改动，B3b 消费方注记）；workspace 673 passed 0 failed（9 ignored，671+2 护栏）+ clippy/fmt/check_layers/scan_secrets 全过；telegram/mock 能力与行为零变化（生产 delete_remote 零调用点） |
| P2-3d | Batch B3b：组合根接线（backend dispatch/config 三处同步/setup/doctor/namespace 后端感知/rebuild/remote_delete 删除接线） | ✅ | **段一**（L2 第 10 位 remote_delete 0b65311 + ck-local transport_face 7e84ff9 + ck-baidu transport_face 4 并发整文件 e50978a）；**段二a**（K17 6b74a19 / K12 58b8cbc / K11 rebuild d10d285）；**段二b**（K4 删除接线红 a647b36 → 绿 b46f87c：先远端后行+缓存、拒绝保行、三面 vfs/webdav/web 按 remote_delete 位门控，telegram/mock false 零变化；dispatch+setup+doctor 红 61f37d6 → 绿 9ffa672：build_driver 统一收编三后端分发、setup baidu 刷新验证一次一换落盘/local 引导、doctor 三探活+K12/K18 尾巴、sync 启动门 local 不启动、能力横幅九+1 位）；**WSL 双平台**（522f7bc：Linux-only lint 两处修——linux.rs 未用导入系继承债 e6581f9 起自证；WSL 739 passed 0 failed + 双平台 clippy/fmt/layers/secrets 绿） |
| P2-4 | Batch E2E：三后端硬验收（telegram Y:/baidu Z:/local V: 真机全流程 + sync 收敛 + rebuild 等价 + 多盘并存 + 清理脱敏） | ✅（telegram 腿延后） | baidu/local 全腿通过：往返/Range/杀进程续传（50 片 0 重传+create 收尾）/删除→远端消失（K4）/双盘并存/sync 收敛（b1↔b2 全字段一致含 fs_id）/rebuild 等价（D10②，chunk_count 簿记差观察项）；E2E 抓出两处装配缺口当场修（K7 sessions_dir 8704380 前后/K12 一次性 sync 41fbb85）；清理完成（远端 n=0、privatefs 未动、盘符/进程全停）；报告 docs/reports/2026-09-08-phase2-e2e.md；**telegram 腿 2026-09-09 补跑通过**（负责人接受生产 chat 污染 §7a 例外；往返/Range/PROPPATCH 207/三盘并存 Y+Z+V 全过；生产 db 零污染设计；聊天侧消息负责人客户端手删） |
| P2-6 | 收口：版本 0.9.0 + 文档联动（README/AGENTS/onboarding）+ decisions 入档 + release 构建验证 | ✅ | 0.9.0 workspace 单点；README 状态表/快速开始三后端 + AGENTS 阶段/计数/陷阱（百度 meta 全废/目录 create 预检/31363/下载三约束/MSYS 探针）联动；E2E 两观察项销账（a92b628：sessions_dir 嵌套 + chunk_count 簿记对齐）；K1–K18 落地索引入 decisions；release 全 workspace 构建通过；不部署生产位 |
| P2-5 | Phase 2.5 多卷启用（Registry + 每实例配置 + 每卷加密 + 多盘挂载） | ⬜ | 裁决 = 方案一（B3 后立即，decisions 2026-09-08）；**P2-6 收口后另立计划**，不在本表执行 |

## 待负责人
1. **E2E 前提供独立测试 chat**（或明示接受生产 chat 污染）——只阻塞 P2-4 的 telegram 腿，不阻塞 baidu/local 腿
2. Phase 2 执行计划评审（P2-1 产出已就绪，批准前不写实现代码）
3. 执行期集中上报项（Kickoff 自主模式下记录于 decisions「待负责人」）：rtype=3 真机复核结果（异常则回退 rtype=1+预删）、正式 appkey 到位后复测 spike §2 限额结论
