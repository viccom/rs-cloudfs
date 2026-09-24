# L2 共享 helper 抽取（去重批）— 设计计划

> 状态：v1.0（2026-09-23）｜ 批准：负责人指令「① 只做上面 150 行版本」
> 范围：独立 worktree `rs-cloudfs-dd`（branch `feat/l2-shared-helpers`）
> 跟踪单：`docs/tracking/l2-shared-helpers.md`

## 1. 背景与目标

架构审查 D4 发现 pan115/pan123/baidu 三驱动间存在协议无关的重复实现。
负责人已拍板：**只做 150 行版本**——抽取 3 件协议无关的纯机制，不碰
PathCache（类型不同 + pan123 反向索引）与刷新单飞（pan123 无 refresh，不适用）。

**北极星约束**：L2 `cloudkit-storage` 是纯契约层（2,156 行，零 workspace 依赖）。
本批向 L2 注入实现代码，须克制——只收确证为「纯机制、零协议语义」的件。

## 2. 抽取清单（已复核边界）

| 项 | 内容 | 共核位置 | 已知差异（合并时必须保留） |
|---|---|---|---|
| ① spool stager `write()` | u64 溢出守卫→append→write_all→超承诺拒→到齐即传 | pan115 `upload.rs:600-632` ≡ pan123 `upload.rs:625-657`（去注释逐行相同） | 无；触发传输的回调由调用方注入 |
| ② Limiter 令牌桶 | `refill` + 令牌等待循环 | pan115 `limiter.rs` ↔ pan123 `limiter.rs` | pan115 独有 770004 封锁窗状态机（`report_limit`/`report_ok`/`blocked_remaining`）——作为可选扩展保留，pan123 用基础版 |
| ③ SessionStore 磁盘层 | file_path 摘要 + tmp+rename + load 回填 | pan115/pan123/baidu 三处 | **键构造各驱动自留**（baidu `path\|size` 两元是协议决定，见 upload.rs:130）；**tmp 命名统一到 baidu 的 `pid+seq` 形态**（修并发隐患，非纯搬运） |

## 3. 关键设计裁决

- **D-1 SessionStore 键不泛化进 L2**：`path|size` vs `path|size|hash` 是协议
  决定（baidu precreate 时刻 block_list 可能不全）。L2 只收「给定 key 字符串
  → 落盘路径/原子写/读回填」的磁盘层；key 由调用方算出。
- **D-2 tmp 命名统一为 `pid+seq`**：baidu 现形态最正确（同进程并发同 key
  不互踩）；pan115/pan123 迁移即行为收敛，属既修隐患非纯搬运。落盘文件名
  摘要（hash→hex）各驱动沿用自家算法（文件名不跨驱动共享，无兼容问题）。
- **D-3 Limiter 拆基础+扩展两层**：基础 = 令牌桶节拍；扩展 = 可选封锁窗。
  pan123 不引入封锁窗字段（保持最小）。
- **D-4 行为等价为第一约束**：三驱动的现有离线测试（conformance + 桩矩阵 +
  各模块单测）是钉死基准，断言零漂移；抽取后全量必须原样绿。

## 4. 验证策略

- **TDD 红→绿**：抽取以「现有测试全绿」为绿基准；每个新增 L2 件附自身单测。
- **离线全量**：ck-baidu / ck-pan115 / ck-pan123 全套（conformance + 桩 + 单测）。
- **真机腿**：三驱动真机矩阵为 `#[ignore]`，需负责人扫码窗口——**本批挂账**，
  不阻塞合入，但跟踪单显式记录。
- **五门禁**：workspace test / clippy -D warnings / fmt / check_layers / scan_secrets。

## 5. 回滚

独立 worktree + 独立 target；任何一步失败 = 丢弃 worktree，主仓零影响。
合入前 main 不动。
