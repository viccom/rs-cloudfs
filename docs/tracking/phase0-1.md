# Phase 0 + Phase 1 执行跟踪单

> **用途**：任务分解 × 状态跟踪 × 跨会话交接三合一。**执行 Agent 每批收口必须更新本表并随 commit 提交**；新会话开工第一件事读本表续跑；负责人随时可查进度。
> 状态图例：⬜ 未开始 ｜ 🔶 进行中（注记当前会话/下一步）｜ ✅ 完成（附证据）｜ ⛔ 受阻（附 decisions 条目）｜ ❌ 取消（附裁决）
> 证据格式：commit hash / 报告路径 / 测试计数（`cargo test --workspace` 总数）。
> 任务定义与验收判据的权威 = [../plans/2026-09-07-phase0-1-execution.md](../plans/2026-09-07-phase0-1-execution.md)（下称「执行计划」）；本表只跟踪，不重复定义。

## 当前焦点

P0-3 已收口（2026-09-07）；下一步 = P0-4（门禁全绿 + WSL 通道 ~/rs-cloudfs + `cydrive --version` 验证）。

---

## Batch P0：搬迁改名（纯机械，每步全绿）

| # | 任务 | 状态 | 证据/注记 |
|---|---|---|---|
| P0-1 | worktree/分支建立（feat/phase0-1） | ✅ | 负责人预授权 P0 直 main（Kickoff 指令），不开 worktree |
| P0-2 | crate 改名 cydrive-*→cloudkit-*/ck-*（逐 crate 提交，二进制名不变） | ✅ | fe61e8e / 83cc23d / 5f2b986 / 8bc031f / 360a675 / e6581f9 / 50d13b2（core→cloudkit-core、telegram→drivers/ck-telegram、sync→cloudkit-sync-server、webdav→cloudkit-webdav、web→cloudkit-web、platform→cloudkit-platform、cli→cloudkit-cli）；每 commit 前三步门禁全绿，测试计数保持 527；`[[bin]] cydrive`/`cydrive-sync-server`、`CYDRIVE_*` env、`cydrive_sync.db` 等契约未动 |
| P0-3 | `scripts/check_layers` + CI 秘密扫描步骤 | ✅ | 代码/CI 交付 = b9bc044：两脚本入库（POSIX sh，shellcheck 干净；`.gitattributes` 锁 LF）+ ci.yml 接入（checkout fetch-depth:0，fmt 前两 step：layer check R1 / secret scan R3）；check_layers 正例 7 manifests 全绿、反例（临时目录：L3+crate→driver、driver→driver 各抓到 exit 1；组合根豁免/注释行/vendor 排除对照不误报）；scan_secrets 全树零命中、反例五路（push 真实 before / zeros 回退 / PR / 干净区间 OK / 本地全树）均符合预期，行号精确定位 bad.txt:3、短值 `password = "short"` 不误报；不可用 range/非 git 目录=响亮失败非静默 OK（exit 2/128）；三步门禁随批全绿（fmt/clippy 干净，test 527 passed 0 failed） |
| P0-4 | 门禁全绿 + WSL 通道（~/rs-cloudfs）建立 + `--version` 验证 | ✅ | 终态门禁：win fmt/clippy 干净 + `cargo test --workspace` 全 suite 0 failed（527）；WSL `~/rs-cloudfs` clone 自 /mnt/e（git fetch 单向同步法）+ `cargo check` 23.50s 过 + 全量 `cargo test --workspace` **528 passed 0 failed**（+1 为 bin_cli.rs 平台 cfg 既有差异，核查过非改名引入）；`cydrive --version` → `cydrive 0.7.2`（bin 名不变） |

## Batch S：百度 spike（验证驱动，非 TDD）

| # | 任务 | 状态 | 证据/注记 |
|---|---|---|---|
| S-1 | token 刷新链跑通（refresh_token → 新 access） | ⬜ | |
| S-2 | QPS/限额三连测（列目录/分片/下载，记录拒绝形态；PCFS appkey 桶局限标注） | ⬜ | |
| S-3 | 上传三步曲 + 断点续传差集（中途杀进程只补差集） | ⬜ | |
| S-4 | 秒传 return_type 分支行为 | ⬜ | |
| S-5 | dlink+Range 复测 + dlink 有效缓存时长 | ⬜ | |
| S-6 | ≥1GB 上/下行吞吐实测 | ⬜ | |
| S-7 | spike 报告落档（docs/reports/）+ 止损判定 | ⬜ | ⛔止损 → R/E 不受影响继续 |

## Batch R：trait 瘦身（纯重构，TDD）

| # | 任务 | 状态 | 证据/注记 |
|---|---|---|---|
| R-1 | cloudkit-storage：StorageDriver/Capabilities/StorageError/VolumeId（TDD） | ⬜ | 百度 errno 三档映射单测预埋 |
| R-2 | conformance_suite 框架 + mock 跑通八断言（interfaces §6，不得缩减） | ⬜ | |
| R-3 | CloudTransport 演进 + InboundCap/ChatCap 拆分（波及面清单先行） | ⬜ | |
| R-4 | ck-telegram 适配 + →core 反向依赖解除（过渡豁免表销账） | ⬜ | |
| R-5 | 消费方能力探测降级（bot worker/webdav，无能力禁用不 panic） | ⬜ | |
| R-6 | 真机 Telegram 冒烟（生产实例谨慎协议：/_e2e_smoke/ + 清理） | ⬜ | |

## Batch E：加密 v2 流式（TDD）

| # | 任务 | 状态 | 证据/注记 |
|---|---|---|---|
| E-1 | cloudkit-crypto：CryptoScheme trait + v1 GCM 迁入（字节零变化，互操作测试护航） | ⬜ | |
| E-2 | v2 分块 AEAD 核心（STREAM 构造；roundtrip/tamper 拒绝/跨块 Range） | ⬜ | |
| E-3 | 流式加密上传接线（零 .enc.tmp；内存峰值断言） | ⬜ | |
| E-4 | `encryption_scheme` 配置键 + Entry/payload scheme 字段 + hydrate 按 scheme 分发 | ⬜ | legacy json 拒收；旧实例忽略新字段 |
| E-5 | 真机 v2 冒烟（telegram 加密小文件上传/下载往返） | ⬜ | |

## 收口（全批后）

| # | 任务 | 状态 | 证据/注记 |
|---|---|---|---|
| C-1 | 版本 0.8.0 + decisions/AGENTS 入档 + release 构建验证（不部署生产位） | ⬜ | |
| C-2 | 收尾汇报（改动/证据/未询问决定与回滚/待负责人清单） | ⬜ | |

---

## 待负责人清单（执行期累积，收尾汇报汇总）

1. （空——执行 Agent 遇疑问/裁决点时追加于此并继续可继续部分）
