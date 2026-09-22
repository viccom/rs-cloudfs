# Phase 8：read-through 按需逐层索引 任务跟踪单

> 计划：`docs/plans/2026-09-22-readthrough-index.md` ｜ 需求口径：负责人 2026-09-22 拍板路线 C（B 为主 + A 最小集），三点要求：①先出详细可行方案与计划；②六后端+未来驱动共性提炼；③证明架构增强非破坏
> 基线：main@86003a0（workspace **1602/0/52** + 五门禁绿；Phase 7 webdav 已合入并反向复核毕）
> 状态：**RT0 计划落档（2026-09-22）——待开工**
> worktree：`feat/readthrough-index`，独立 target（共享 CARGO_TARGET_DIR 双指纹既有教训）
> 编号：裁决 K83（待入 decisions）；批次 RT0–RT5 顺序执行，RT3 与 RT4 可并行派发

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| RT0 | 方案+计划落档（无生产代码） | ✅ 2026-09-22 | 计划 `2026-09-22-readthrough-index.md`（requirement-analyzer 四段 + D1–D10 裁决 + 共性五件套 + RT1–RT5 任务分解 + 架构合规证明表）；本跟踪单；三路探查硬事实入计划 §1 | 本批日志 |
| RT1 | L2 探针面 + L3 物化提取 | ⬜ 待开工 | — | — |
| RT2 | readthrough 原语（read_dir_fresh/stat_fresh/reconcile/DirCache） | ⬜ 待开工 | — | — |
| RT3 | 四消费面接线（网关/仪表盘/winfs） | ⬜ 待开工 | — | — |
| RT4 | rebuild 三件套（续跑/上限/sweep） | ⬜ 待开工 | — | — |
| RT5 | 真机矩阵 + 文档 + 收口（含深度审查批，K78 形态） | ⬜ 待开工 | — | — |

## 批次日志

### RT0（2026-09-22，主会话）

**完成**：三路并行探查（Vfs/db 行语义 / 四消费面 async 边界与装配链 / rebuild×队列耦合×测试双轨）→ 设计定稿 D1–D10 → 计划落档。

**关键探查结论（决定方案形态的四条）**：
1. **物化映射可直接复用**：`rebuild.rs:150-215 upsert_entry` 的 Entry→行映射（K6 句柄降级 `Some(0)`、K11 单容器 chunk）已被读链消费过（K74 pan115 rebuild 腿真机）；`remote_handle_for`（vfs.rs:702-741）对 `Some(0)` 放行、路径形后端用 `RemoteHandle.path` 寻址——read-through 物化行**开箱可读**，无行语义缺口（明文卷）。
2. **探针先例现成**：`as_inbound`/`as_chat`（transport/mod.rs:204-210）= 默认 None + 只降级不 panic；六个 transport_face 全持 `Arc<驱动>` + `into_driver()`（探针一行）；GrammersTransport 无驱动 → 默认 None 天然正确（telegram 零变化）。
3. **零 files DDL 变更可行**：sweep 判据复用既有 `updated_at` 列（upsert 自然刷新 → 跨续跑保护），只加 `rebuild_state` KV 表（sync_mirror 同先例）——R6 冻结契约零触碰。
4. **in-flight 判定原文可复用**：sync.rs:579-580 `!is_uploaded && local_copy_exists`——reconcile 两侧豁免的判据与 sync 语义单点同源。

**自主裁决（未询问，可逆）**：①`read_dir_fresh` 每调用强制 revalidate（D5，RaiDrive 对等）而非 TTL 窗——胜在新鲜度与语义简明，代价是每视图 1 次 list（用户点赞的 J: 盘同款成本）；②stat TTL 窗定 5s（D6，实测定值，`with_ttl` 缝可调）；③删除双确认上限 32 条/目录（D7，超限整批跳过+告警）；④rebuild 单趟上限默认 20 万条目（D8，RebuildTuning 注入可调）；⑤api_files 保持索引视图（D9，防全表 API 暗变成全树遍历）；⑥加密卷 read-through 明确拒收（D10——list 报密文 size，物化即破坏「size=明文」R6 契约与 AEAD 预算数学；指路 sync）。回滚 = revert 本批 commit。

## 风险与未覆盖（随批更新）

- 加密卷 read-through 不做（D10，密文 size→明文换算不可靠）——挂账
- api_files 全表视图新鲜度（D9）——挂账
- ghost 行清理归 K4 delete-wiring 旧账——不扩 scope
- stat TTL 窗 5s = 实测定值，真机后可调
- 待 RT5：真实广域网链路形态（WSL2 回环数字口径，sftp SF5 判例）
