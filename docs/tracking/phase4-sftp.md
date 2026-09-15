# Phase 4：ssh/sftp 存储驱动任务跟踪单

> 计划：`docs/plans/2026-09-14-sftp-driver.md` ｜ 需求口径：负责人 2026-09-14「自用项目，不对外销售」+ 三条硬要求（编译开关 / 零侵入 / 保持松耦合）
> 基线：main@c9cd4e3（workspace 1067/0/12；winfsp 腿 117/0/1）
> 状态：**方案已批准（2026-09-14 SF0 拍板，K60）——SF1 可开工**
> worktree：待建（建议 `feat/sftp-driver`，跨 ≥3 commit → 按仓库纪律必须 worktree 隔离）

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| SF0 | 决策与门禁前置（无代码） | ✅ 2026-09-14 | 三项全拍板（K60）：**D1 = 密码 + 私钥（含 passphrase；k-i/agent 不做挂账）**；**D2 = 显式接受 + 指纹落盘**（未记录拒连、变更恒拒）；**D3 = 起步单连接**（多卷≠多连接概念澄清落档——多卷为 Phase 2.5 既有能力自动继承；同卷 N 连接分段留 SF4 实测后再议）。选型已于 K59.2–K59.7 入档。计划状态 → 已批准。 | decisions K60；计划 §8 拍板记录 |
| SF1 | 驱动骨架 + 配置接入（无真实网络） | ✅ 2026-09-15 | `ck-sftp` crate 落地（六模块 1945 行：client 连接/认证 D1/host key D2 三态/with_retry 重连骨架、driver 九方法+能力位逐位注码、stager 三硬仗、transport 薄壳、config/error 纯函数）；配置键**四处**清单同步（KNOWN/VOLUME_SCOPED/SECRET_VALUED + LEGACY_REJECTED——后者为派发单遗漏、按既有模式补齐）+ `Backend::Sftp` + `validate()` Sftp 分支；`compiled_drivers()` 重构为 DRIVER_ROWS 表 filter-join 形态（签名 `const fn ->&str` 改 `fn ->String`，既有测试**零改动**通过=零漂移）；4 个编译必需占位 arm（sync/dispatch×2/build_driver，bail 文案指明 SF3 接线）；纯函数层 TDD（ck-sftp 14 测试 + core config_backend 9 新测试）。**执行注记**：子代理中途撞平台限额，半成品（TDD 红相位：`remote_path` todo! + `StorageError::Invalid` 误作载荷变体 7 处编译红）由主会话续作转绿——`Invalid` 是 L2 冻结单元变体，可行动文案改经 `tracing::warn!` 双通道（map_session_error 同款先例）。**风险挂账（SF3 验证）**：stager 按计划 §7 裁决直接写最终路径（无 tmp+rename/stash），staging 窗口目标以部分内容可见——conformance 断言①（close 前不可见）届时红则补 tmp+rename 或豁免声明。 | workspace 1092/0/12（基线 1069+23）；clippy/fmt/check_layers(13 manifests)/scan_secrets 全绿；既有 `compiled_drivers_lists_the_feature_set_in_fixed_order` 零改动通过 |
| SF2 | 测试桩：进程内 SFTP 服务端 | ⏳ 未开工（**骨架已验证**） | 搭建方式与 Windows 可行性已实测跑通（附录 C.2），从"高风险批"降级为"照骨架落地" | `E:\tmp\sftp-harness` 端到端 PASS |
| SF3 | conformance + 装配接线 | ⏳ 未开工 | 离线八断言 + 12 处接入点 + doctor/setup/web 表单/i18n | — |
| SF4 | 真机矩阵 | ⏳ 未开工 | WSL2 localhost:22 作测试服务器；上下行吞吐/Range 播放/断线重连/符号链接/rebuild 收敛 | — |
| SF5 | 吞吐增强（可选） | ⏳ 视 SF4 | 若单连接流水线读不足 → 文件内分段多连接；否则明确销账 | — |

## 立项前研究（2026-09-14，本批已完成）

三轮外部项目只读勘察 + 本仓两项实测探针。**结论已全部写入计划文档**，此处只记索引：

| 研究 | 结论落点 | 关键收获 |
|---|---|---|
| 本仓探针 `E:\tmp\sftp-probe` | 计划 §1.2、附录 C.1 | 依赖树可编译；ring 路线可行；许可证无需新增 deny 条目；踩到 0.63 API 漂移 |
| termscp（MIT） | 计划 §0.1、附录 A.1 | 路线印证（弃 libssh2）；其同步/临时文件架构与 `NoCheckServerKey` 不采纳 |
| aeroftp（GPL，自用前提下不受限） | 计划 §1.3、§4.4、附录 A.2 | **关键**：其 400 行 read-ahead 是为 russh-sftp 2.x 串行读缺陷写的，3.0 已修 → 不复制；10 条实战坑清单可迁移 |
| 本仓桩探针 `E:\tmp\sftp-harness` | 计划 §5 SF2、附录 C.2 | **决定性**：Windows 进程内 SFTP 桩端到端 PASS（list/stat/range/error），不依赖 Docker/HOME |

## 批次日志

- **2026-09-15 SF1 完成**（worktree `feat/sftp-driver`，独立 target `E:\Rs_Codes\rs-cloudfs-sftp-target`）：
  - **执行模式偏离记录**：原计划 hub-and-spoke 委派子代理；子代理启动后中途撞平台 5 小时使用限额（2026-09-15 10:00 重置）被掐断，留下未 commit 的 TDD 红相位半成品（改动形态与派发单吻合：config 键四处清单、compiled_drivers DRIVER_ROWS 表、ck-sftp 六模块、`remote_path` todo! 红测试在位）。主会话按「未审查 PR」处置：逐文件审查（重点测试断言零漂移——`tests/config.rs` 仅 struct 字面量补字段、既有断言未动）、完成红→绿（`remote_path` 实现、`invalid_config` 归一 7 处 `StorageError::Invalid(format!)` 编译错——`Invalid` 为 L2 冻结单元变体无载荷，文案经 `tracing::warn!` 双通道）、clippy 4 错修复（field_reassign/doc_lazy×2/question_mark）、fmt 归一。
  - **未询问的决定（自主裁决，最可逆方向）**：①`StorageError::Invalid` 无载荷的文案通道选 warn 日志（对齐 crate 内 map_session_error 已有先例，不改 L2 类型）；②stager 维持计划 §7 字面裁决直写最终路径（未擅自升级 tmp+rename），断言①风险显式挂 SF3；③TDD 红相位中断导致「红输出留证」缺失，以「测试对实现的约束力审读 + 全量绿」替代，如实记录；④doctor/main.rs 的 `Backend::Sftp` 占位 arm 属枚举扩展的编译必需连带（非越界），真检查留 SF3。
  - 验证：workspace `cargo test --no-fail-fast` **1092/0/12**（基线 1069 + ck-sftp 14 + core 9）；clippy `-D warnings` 绿；fmt 绿；check_layers 绿（13 manifests / 4 driver crate）；scan_secrets 绿；依赖树 russh 0.63.3 + russh-sftp 3.0.0（版本下限合规，ring 复用）。

- **2026-09-14 SF0 完成**（负责人拍板，K60 入档）：D1 = 密码 + 私钥（便于自动化；含 passphrase，k-i/agent 挂账）；D2 = 显式接受 + 指纹落盘（未记录拒连、变更恒拒、禁止无条件接受）；D3 = 起步单连接——**概念澄清**：负责人所述「不同认证各起一个实例」= 多卷模式（Phase 2.5 既有，SFTP 自动继承），非 D3 所问的同卷吞吐多连接；后者拍板不做，SF4 实测后再议 SF5。落档四处：decisions K60、计划 §8（待拍板→拍板记录）、本表 SF0 行、AGENTS 待人工清单销账。
- **2026-09-14 立项前置研究完成**（本批不开工实现；只读勘察 + 仓外探针，仓库 `git status` 全程干净）：
  - **负责人问询三点**，均在计划正文给出直接回答：①「以哪个项目为主」→ 计划 §0.1 方法论裁决（以 `ck-local` + russh-sftp 官方 API 为主干，aeroftp 作坑清单，termcp 仅路线印证；核心理由是 aeroftp 的绕行代码为 2.x 写、3.0 已修，以它为主干等于主动复制过时代码）；②「加一个驱动多久」→ 计划 §0.2 量化（后续路径式驱动边际成本约为 SFTP 的 40–60%，因 `compiled_drivers` 改写与测试桩是一次性投资）；③「编译开关 + 零侵入 + 松耦合」→ 计划 §2/§3（零侵入依据经通读代码确认 6 条；接入面穷举 12 处，均为追加而非改动）。
  - **实测验证（不是纸面推断）**：① 依赖树编译 PASS（ring 路线，193 包许可证逐条比对 deny allow-list 无新增需求）；② **Windows 进程内 SFTP 服务端端到端 PASS**——`[RESULT] PASS - Windows in-process SFTP harness: list/stat/range/error all green`，Range 读的 4096 字节逐字节校验正确。
  - **执行期发现的坑（已写入计划）**：桩的 `readdir` 必须在第二轮返回 `StatusCode::Eof`，否则客户端 `read_dir` 永挂（首次运行 20s 超时暴露）；这一条与 aeroftp 同类桩只能跑 Unix 的原因（重定向 `HOME`）一起构成 SF2 的验收要求。
  - **修正前两轮结论两处**（计划附录 A.3 留档）：串行读缺陷 3.0 已修（省掉 400 行绕行）；测试桩确证可行且成本低于预估。

## 风险与未覆盖（如实记录）

- **未验证项**：真实 SSH 服务器（OpenSSH）的兼容性、大文件实网吞吐、断线重连、符号链接语义、`rebuild` 收敛——全部属 SF4 真机矩阵，本批未做。
- **未验证项**：`russh-sftp` 3.0 是本年度较新的大版本（3.0.0 发布于 2026-09-08），本批只验证了客户端+服务端基本往返与 Range 读；其并发读流水线（`max_concurrent_reads: 16`）的实网收益未实测。
- **依赖增量**：russh 协议栈较重（ECC/RSA/ML-KEM/cipher 全套），是 SSH 协议本身的重量；缓解靠 feature 门控（K30）——裁剪构建可完全排除，与现有 telegram/baidu 裁剪同机制。
- **待人工决策**：D1 认证方式、D2 host key 非交互形态（计划 §8）——在 SF0 解决，未决不开工 SF1。
- **架构前提的边界**：「L2 以上零改动」基于接入面追加式接入；若 SF1 执行期发现 `compiled_drivers()` 之外还有隐藏的第 4 驱动耦合点，须在批次日志如实记录并回报。
