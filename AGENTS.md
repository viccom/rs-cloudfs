# rs-cloudfs — Agent 工作须知

## 项目信息
- 项目：rs-cloudfs = rs-CyDrive × PrivateCloudFS 融合体——多云存储平台（统一存储抽象之上的 WebDAV 挂载/仪表盘/同步/CLI；后端：telegram / baidu / local，未来 115/123/s3）
- 技术栈：Rust（edition 2021）/ tokio / axum / dav-server / rusqlite(bundled) / grammers(telegram) / hyper-rustls
- **血统**：fork 自 rs-CyDrive（全 git 历史；remote `upstream-cydrive` 只读参照，禁止 push）；PrivateCloudFS（`E:\Go_codes\PrivateCloudFS`，Go）是设计参照系与踩坑情报源（情报附录在 multicloud 计划）
- **北极星**：「一个稳定好用的程序」——重组已验证资产，不重写
- 行为基线与兼容契约：Python 版 CyDrive 契约（DB schema/分块命名/caption/端口）经 rs-CyDrive 继承，telegram 驱动延续遵守（红线 R6）

## 必读（开工前，按序）
1. `docs/plans/2026-09-07-cloudfusion-foundation.md` —— 融合基线设计 v1.1（阶段计划/裁决状态/E2E 凭据策略）
2. `docs/standards/architecture.md` —— 六层架构 + 红线 R1–R7（**L2 以上禁 import 驱动符号**等）
3. `docs/standards/code-style.md` / `interfaces.md` / `logging.md` / `documentation.md` —— 门禁与规范
4. `docs/plans/2026-09-06-multicloud.md` —— 百度情报附录 A（端点/参数/errno/dlink/Range 实证）
5. `docs/decisions.md` —— 历史裁决（自 rs-CyDrive 继承，继续追加）
6. `docs/tracking/phase0-1.md` —— 当前任务跟踪单（**开工先读、每批收口更新**）

## 当前阶段
**Phase -1 规范先行已完成（2026-09-07）**；仓库 = fork 基线代码（cydrive 0.7.2，527 测试绿）+ 治理文档集。**下一步 = Phase 0 + Phase 1**（`docs/plans/2026-09-07-phase0-1-execution.md`——含预授权 Kickoff 指令，覆盖搬迁改名/百度 spike/trait 瘦身/加密 v2 流式）→ Phase 2（驱动接入手册 → ck-local + ck-baidu + 端到端硬验收）。

## 常用命令（仓库根）
```
cargo test --workspace --no-fail-fast            # 617 测试（E-5 修复后；ignored 6 为既有真机）
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
scripts/check_layers                             # R1 层依赖门禁（CI 同款；动 Cargo.toml 依赖后必跑）
scripts/scan_secrets                             # R3 秘密扫描门禁（CI 同款；本地模式=全树扫描）
```

## 硬性规则（摘要，全文见 standards/）
- 七条架构红线（architecture.md §2）——违反即返工
- 凭据红线：`test/` 全目录 gitignore；凭据只从 `E:\GitHub\rs-CyDrive\test\` 或 env 读；**任何凭据值不入代码/文档/日志/提交**（负责人后期轮换授权）
- 质量：TDD 红→绿留证、断言零漂移、workspace 级门禁、真机测试 `#[ignore]`
- **大任务纪律（负责人 2026-09-07 指令）**：代码量较大的任务必须以 TDD 思想为指导；**每个计划必须配套任务单与跟踪记录表**（`docs/tracking/<phase>.md`——任务分解×状态×完成情况×证据，每批收口更新并随 commit 提交）
- 提交：conventional commits、不加署名尾注、批次 worktree 隔离
- 文档：行为变更同批更新 README/AGENTS 计数/相关 docs；裁决入 decisions.md

## 已知陷阱（自 rs-CyDrive 继承 + 融合新增）
- Windows Git Bash：`ls`/`tree`/`du`/`ps` 别名禁用（用 `fd`/`rg`）；wsl.exe 复杂命令须 `.sh` 脚本路线（引号吞噬）
- 探索子代理拿结论，主会话不整读大文件（hub-and-spoke）
- 百度 API：强制 IPv4 dial、dlink 须追加 access_token、errno 110/111/-6 三档（详见 multicloud 附录 A）
- **Explorer 上传链路**：空 PUT→LOCK→PUT→PROPPATCH——**PROPPATCH 必须全成功（207）而非 405**，否则 MiniRedir 整单回滚「看似失败实则已传」（rs-CyDrive 2026-09-03 真机首验最贵教训，百度 E2E 直接承重）
- 实现期陷阱查 `docs/rust-rewrite-design.md`「深度调研补遗」节（axum 2MB body 上限/grammers FloodWait 藏点/dav-server Bytes-Seek 模型/WebClient 4GB-1/挂载 Basic 认证）
- PCFS 反面教材勿抄：错误类型跨层泄漏、硬编码密钥、纯 CTR 无认证、注释掉的调试日志

## 待人工清单
1. 基线设计 §9-2/9-3/9-4 三项建议待负责人确认（旧仓冻结时点 / bot 分 crate 时点 / R-E 批序）
2. Phase 2 E2E 前提供 `E:\GitHub\rs-CyDrive\test\` telegram 测试配置（**独立测试 chat**，见 foundation §7a 隔离裁决；baidu token 已验证可用）
3. 新仓远端 origin 待建（当前仅 upstream-cydrive 只读、push 已禁用）
4. 自 rs-CyDrive 继承的挂账（旧仓 decisions.md 末条）：P3 hydrate 快照回写竞态、复审 Low 项（--help 文案余项/凭据门槛统一/sync_url host 校验/SyncClient trait 文档/64MB 并发闸）、#[ignore] 真机测试 ×3、litmus 套件、deny advisories、scripts/gen_compat_fixtures.py 契约 fixture 流程
