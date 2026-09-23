# Phase 8-B：加密卷 read-through 放开 任务跟踪单

> 计划：`docs/plans/2026-09-23-encrypted-readthrough.md` ｜ 批准链：负责人 2026-09-23 三点指示 → K84 立项（D10 修订留痕）→ 深度分析五环 + 计划 §0 八项代码查证 → 计划落档
> 基线：`feat/readthrough-index`@77a58cc（Phase 8 RT0–RT5+审查批+文档批已落，workspace 1643/0/60 五门禁绿，**待合入**）
> 状态：**计划落档（2026-09-23）——待负责人批准计划后开工**
> worktree：`feat/readthrough-index`（与 Phase 8 同一支，连续批次）；独立 target
> 编号：执行记录入 decisions 用 **K85**；批次 **EB1–EB4**

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| EB0 | 计划期八项代码查证（随计划完成） | ✅ 2026-09-23 | 计划 §0 八条：upsert 冲突集覆盖 cipher 列 / AeadV2::new() 默认分块无配置缝 / RowMetaData.len 承重 / K47 流式分流 / chunks 仅取消息 id / sha256 读面零引用 / 闸两点位+help 文案 / plan A pending 出口 | 计划 §0（本批日志） |
| EB1 | cipher 真相物化（B1+B3+B4：保留语义 upsert + 闭式反推 + 去 H1 退化臂） | ⬜ 待批开工 | — | — |
| EB2 | 首读内容校验与回写（B2+B6：容器头/本地长度权威 + 定向 UPDATE + 双假说文案） | ⬜ 待批开工 | — | — |
| EB3 | rebuild 闸放开（B5：ensure_plaintext_instance 删 + 测试翻转 + help 文案） | ⬜ 待批开工 | — | — |
| EB4 | 两阶段验收（离线 CI 三腿）+ 真网加密腿重跑 + 文档收口（K85）+ 五门禁终跑 | ⬜ 待批开工 | — | — |

## 批次日志

### EB0（2026-09-23，主会话计划期查证）

**决定计划形态的两条**：
1. **`upsert_file` ON CONFLICT 无条件覆盖 `is_encrypted`/`size`**（database.rs:477,483）——B 若按配置盲猜复用该方法，首读修正在下次目录列举即被回冲（修正-回冲震荡）→ 立 B1/B3：物化专用保留语义 upsert，既有行 cipher 真相永不被列举猜测回退。
2. **`AeadV2::new()`（vfs.rs:261）恒默认 1MiB 分块、应用无容器分块配置缝**（`chunk_size_mb=1900` 是上传队列分段）→ 给定方案后尺寸反推**闭式精确**，唯一不确定量 = 方案本身 → 物化期零内容嗅探（B4，O(1)/层保持）+ 首读容器校验权威（B2）。

**其余六条**：上传行 cipher 写法同源点（vfs.rs:500-506）；容器格式权威（v1 冻结无 magic / v2 34B 自描述头）；`row.size` 承重面 = 网关 Content-Length（RowMetaData.len）+ K47 门 + DecryptingTransport.plain_len；加密读分流既有机制不动（密码门→WF0→K47→DecryptingTransport/Hydrate）；rebuild 闸 = cli:4010/:4919 + rebuild.rs 定义 + --help 文案（telegram 拒收另一条不动）；remote_handle 注释的 plan A pending 由本方向关闭（EB4 入 K85）。

**设计推导要点（入计划 §1）**：读法上「列表在信息论上不知道单文件真相（v1 无 magic）」→ 真相三层优先（既有行 > 配置初值）；「内容是唯一权威」→ 首读校验先于字节出门；「安全方向 = 响亮失败」→ B6 双假说文案 + sync 出路，绝不静默吐密文当明文。

## 风险与未覆盖（随批更新）

- v1 无 magic 残余歧义（B6：加密码晚于存量明文 + 索引丢 + 配置=gcm）——文案+sync 双出路，远期 manifest 挂账延续
- 首次 PROPFIND 在「初值猜错未首读」窗口显示初值尺寸（一次性自愈，EB2 注明接受残余）
- 真网加密腿依赖 token 可用性（K77 轮换纪律）；离线三腿已覆盖机制本体
- Phase 8 主链（RT0–RT5+审查批）仍为**待合入**态——EB 批同分支叠加，合入指令到时一并处理
