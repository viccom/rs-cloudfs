# Phase 5：115 网盘存储驱动（pan115）任务跟踪单

> 计划：`docs/plans/2026-09-14-pan115-driver.md` ｜ 需求口径：自用（K59.1）+ 全量公民 + 编译开关 + 零侵入
> 基线：main@52c3e41（workspace 1069/0/12；winfsp 腿 117/0/1）
> 状态：**方案已落库，待负责人批准**（115-0 的 D1 路线裁决是开工后第一件事）
> 前置依赖：Phase 4（SFTP）SF1 的 `compiled_drivers()` 可扩展化——若 Phase 5 先行，该重构所有权移至本阶段（只做一次）
> worktree：待建（建议 `feat/pan115-driver`）

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| 115-0 | 路线 spike 与裁决 | ⏸ | 五项：凭证可得性（胜负手）/downurl UA+Range/QPS/upload 全链+resume/秒传哈希可否省略；`examples/pan115_spike`（workspace exclude 照 baidu_spike 先例）；产出路线裁决（计划 §1.3 三选一）+ errno 采样表 | — |
| 115-1 | 认证与驱动骨架 | ⏸ | oauth.rs（device-code/QR/refresh+TokenStore；路线 B 则 cookie）+ 四件套骨架 + 配置键三处同步 + SECRET_VALUED_KEYS 增补 + ali-oss-rs 版本树核对 | — |
| 115-2 | 读路径 | ⏸ | list/stat/mkdir/delete/rename + downurl 流读（HEAD 探测/Range/429 分段重生） | — |
| 115-3 | 写路径 | ⏸ | upload init/get_token/ali-oss-rs 分片/complete/resume/秒传 + 硬纪律 3/4/5 | — |
| 115-4 | conformance + 装配 | ⏸ | 假开放平台 + 假 OSS 桩（get_token 端点指向 loopback）八断言全绿 + 12 装配点 + web 表单/扫码引导 | — |
| 115-5 | 真机矩阵 | ⏸ | 上传往返/Range 播放/秒传命中/断点续传（杀进程恢复）/rebuild 收敛（QPS 限速）/加密卷/E2E 三面 | — |

## 立项前研究（2026-09-14，已完成）

| 研究 | 结论落点 | 关键收获 |
|---|---|---|
| PCFS（Go，私有 web API 路线） | 计划 §1.2/附录 A.3 | 驱动形态对照本；sign.go 51 行带测试可直译；cookie 认证；CDN 完整头回传教训 |
| 115-plus-desktop（Rust，开放平台路线，MIT） | 计划 §1.1/附录 A.2/A.4 | **修正上轮"115 只有一条腿"判断**：官方开放平台路线 Rust 可行且活跃；TS 层=端点规格书；传输引擎实战（CDN 429 重生/OSS 续传双坑修法）；`ali-oss-rs` 引荐 |
| 风险背景核实 | 计划 §1.1/§7 | 2026-08-09 开放平台暂停服务报道 + 该仓库 QQ 群获取凭证暗示——**凭证可得性成为 115-0 第一项** |

## 批次日志

- **2026-09-14 立项前置研究完成**（只读勘察，仓库 git status 干净）：
  - 双仓库分工定调（计划 §0.1）：115-plus-desktop = 主路线规格书 + Rust 传输参照；PCFS = 驱动形态对照 + 路线 B 保底；ck-baidu = 结构模板（api/oauth/errno mock/假服务端/TokenStore）。
  - **对上轮评估的修正入档**：115 从"一条腿"改判"两条腿"（开放平台 + web API）；Rust 生态从"零现成物"改判"`ali-oss-rs`（MIT）+ 传输参照齐备"——量级估计比 ck-baidu 省约三分之一（走路线 A 前提下）。
  - 计划落库三件：本跟踪单 + `docs/plans/2026-09-14-pan115-driver.md` + decisions K61。

## 风险与未覆盖（如实记录）

- **未验证项**：凭证可得性、downurl 的 UA 约束矩阵、CDN Range/206、QPS 限额、上传全链、秒传哈希可否省略——全部属 115-0 spike，本批未做。
- **未验证项**：`ali-oss-rs` 与本仓 reqwest 0.12 的版本兼容（其使用方 lock 显示 reqwest 0.13——可能双版本并存；115-1 核对，不可接受则按其源码自实现分片签名）。
- **平台政策风险**：2026-08 暂停事件的后续不可预测；缓解=限速+路线 B 保底设计（api.rs 内聚使切换成本=换认证与签名层）。
- **待人工决策**：D1 路线（spike 后）/ D2 删除语义（回收站 vs 永久）/ D3 根目录缺省 / D4 QPS 限速参数形态（计划 §8）。
