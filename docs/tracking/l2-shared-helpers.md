# L2 共享 helper 抽取 — 任务跟踪单

> 计划：`docs/plans/2026-09-23-l2-shared-helpers.md`
> worktree：`rs-cloudfs-dd`（branch `feat/l2-shared-helpers`，基于 main `acb9b16`）
> 批准范围：只做 150 行版本（spool write / Limiter 共核 / SessionStore 磁盘层）

## 任务分解 × 状态

| 项 | 内容 | 状态 | 证据 |
|---|---|---|---|
| DD-0 | worktree + 计划 + 跟踪单 | ✅ 完成（2026-09-23） | branch `feat/l2-shared-helpers` |
| DD-1 | spool stager `write()` 抽取 | ✅ 完成 | `spool.rs`（`spool_append_write` + `SpoolStage`，4 测试）；pan115/pan123 `write()` 各 −30 行委托 |
| DD-2 | Limiter 令牌桶共核 | ✅ 完成 | `token_bucket.rs`（`TokenBucket` + `refill_tokens`，3 测试）；pan123 委托 `TokenBucket`，pan115 保封锁窗状态机仅复用 `refill_tokens` |
| DD-3 | SessionStore 磁盘层 + tmp pid+seq | ✅ 完成 | `session_disk.rs`（`SessionDiskStore`，4 测试含并发 tmp 名）；三驱动磁盘层统一委托 |
| DD-4 | 三驱动离线全量绿 + 五门禁 | ✅ 完成 | 三驱动 280/0/12；workspace 1696/0/62 |
| DD-5 | 收口 + commit（不 push） | ⏳ 进行中 | — |

## 关键裁决落实记录

- **D-1（键不泛化进 L2）**：三驱动各自保留 `key()`（baidu `path|size` 两元 /
  pan115、pan123 `path|size|hash` 三元）与 `digest()`（baidu/pan123 MD5、
  pan115 Sha1）；L2 只收「digest_hex → 路径/原子写/读回填」。✅
- **D-2（tmp pid+seq 统一）**：`SessionDiskStore` 用 `pid+seq` tmp 名；baidu
  原形态即此（无变更），pan115/pan123 由裸 `json.tmp` 收敛（**既修并发隐患**）。
  专项测试 `concurrent_writes_to_the_same_key_use_distinct_tmp_names` 钉死。✅
- **D-3（Limiter 基础+扩展）**：pan123 完整委托 `TokenBucket`；pan115 因
  「封锁窗判定 + 令牌消耗须同一 Mutex 原子」（`check_wait`/`blocked_remaining`/
  `report_limit` 共享一把锁）**不能整体套 `TokenBucket`**，仅 `refill` 数学复用
  `refill_tokens`——这是复核后的诚实边界，不为去重而引入 TOCTOU。✅
- **D-4（行为等价）**：三驱动现有离线测试（conformance + 桩矩阵 + 单测）
  全绿，断言零漂移。✅

## 净效果

- 三驱动 −193 行重复实现；L2 新增 449 行（含完整契约文档 + 11 个专属测试）。
- 行数非净减（L2 副本带文档/测试），但**逻辑单一来源**：spool 写面、令牌桶
  节拍、会话磁盘机制各只剩一处实现。
- `Cargo.lock` 仅 +`tempfile`（L2 dev-dep，沿用 workspace 既有模式）。

## 五门禁（DD-4 实跑）

- workspace `cargo test --no-fail-fast -j 2`：**1696 passed / 0 failed / 62 ignored**
  （基线 1685 + 11 新增：spool 4 + token_bucket 3 + session_disk 4）
- `cargo clippy --workspace --all-targets -- -D warnings`：绿
- `cargo fmt --all -- --check`：绿
- `scripts/check_layers`：OK（16 manifests，7 driver crates，无 R1 违规）
- `scripts/scan_secrets`：OK

## 真机挂账（不阻塞合入）

- **ck-pan115 真机矩阵**（6 腿 `#[ignore]`）：需负责人扫码窗口（refresh token
  上次已被 leg 1 轮换）。
- **ck-pan123 真机矩阵**（3 腿）+ **pan123_e2e**（5 腿）：token 需在位。
- **ck-baidu 真机**：凭据链可用，本批未主动跑（统一留待窗口）。
- 本批三处改动均为协议无关的本地机制（spool 落盘 / 令牌节拍 / 会话落盘），
  真机风险面低于协议改动；但仍按纪律挂账，待窗口补齐真机腿。

## 风险与未覆盖

- L2 从「纯契约层」开始承载实现（spool/token_bucket/session_disk 三件 + tokio
  `fs/io-util/sync` 特性）——这是北极星约束下的刻意权衡，已克制为「只收纯
  机制」；后续若再有共享需求，先评估是否同样属纯机制。
- pan115 limiter 的封锁窗状态机未抽取（协议特化 + 原子性约束），文档已声明。
- baidu `SessionStore` 的 `Default` derive 随重构移除（全仓无 `::default()`
  调用点，dead derive）。
