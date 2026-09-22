# Phase 8：read-through 按需逐层索引 任务跟踪单

> 计划：`docs/plans/2026-09-22-readthrough-index.md` ｜ 需求口径：负责人 2026-09-22 拍板路线 C（B 为主 + A 最小集），三点要求：①先出详细可行方案与计划；②六后端+未来驱动共性提炼；③证明架构增强非破坏
> 基线：main@86003a0（workspace **1602/0/52** + 五门禁绿；Phase 7 webdav 已合入并反向复核毕）
> 状态：**RT2 完成（2026-09-22）——RT3 进行中（RT4 排队串行）**
> worktree：`feat/readthrough-index`，独立 target（共享 CARGO_TARGET_DIR 双指纹既有教训）
> 编号：裁决 K83（待入 decisions）；批次 RT0–RT5 顺序执行，RT3 与 RT4 可并行派发

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| RT0 | 方案+计划落档（无生产代码） | ✅ 2026-09-22 | 计划 `2026-09-22-readthrough-index.md`（requirement-analyzer 四段 + D1–D10 裁决 + 共性五件套 + RT1–RT5 任务分解 + 架构合规证明表）；本跟踪单；三路探查硬事实入计划 §1 | 本批日志 |
| RT1 | L2 探针面 + L3 物化提取 | ✅ 2026-09-22 | `as_driver` 探针（trait 默认 None + 六宽面 transport_face 各一行；telegram 零变化）+ `materialize.rs`（`materialize_entry` 自 rebuild.rs:150-215 逐字段平移，K6/K11 形态保真 + `list_all_pages` 归集器）+ rebuild 改调；rebuild 既有 4 测试零漂移 | commit `834b4fa`；workspace **1607/0/52**（+5）；五门禁绿；批次日志 RT1 节 |
| RT2 | readthrough 原语（read_dir_fresh/stat_fresh/reconcile/DirCache） | ✅ 2026-09-22 | `readthrough.rs`（DirCache：TTL 5s+with_ttl 缝/单飞闸+世代归并/就近失效；reconcile：upsert 侧 in-flight 豁免+双确认 prune+32 上限+NotFound 臂本层删除+stale-if-error）+ Vfs 两薄壳 + 写侧失效三调用点 + `VfsError::EncryptedInstance`；门序 D2 退化→D10 拒收；`sync::is_in_flight_row` 提取单点同源 | commit `aa47649`；readthrough 16/0、rebuild 4/0 零漂移、sync 27/0 零漂移；workspace **1623/0/52**；五门禁绿；批次日志 RT2 节 |
| RT3 | 四消费面接线（网关/仪表盘/winfs） | ⬜ 待开工 | — | — |
| RT4 | rebuild 三件套（续跑/上限/sweep） | ⬜ 待开工 | — | — |
| RT5 | 真机矩阵 + 文档 + 收口（含深度审查批，K78 形态） | ⬜ 待开工 | — | — |

## 批次日志

### RT2（2026-09-22，子代理实现 + 主会话审查）

**完成**：commit `aa47649`——①`readthrough.rs` 新建：`DirCache`（单 StdMutex 三 map：marked=stat_fresh TTL 窗 5s、generations=per-dir 完成趟计数、flights=per-dir tokio Mutex 闸；`with_ttl` 测试缝）；`read_dir_fresh`（D2 门两臂退化 → D10 拒收 → 单飞闸内重查到达世代归并 → list_all_pages → upsert 侧 in-flight 豁免物化 → reconcile prune（候选=is_uploaded 且缺列且非 in-flight；>32 整批跳过+warn；逐条 stat 双确认 NotFound 才删）→ mark → db.list_dir；NotFound 臂=本层行+目录行同 helper 双确认后删、深层留 sweep；Err 臂=stale-if-error 有行照常服务/无行 Err 绝不 404）；`stat_fresh`（根合成 → TTL 窗快路径零网络 → 重列父目录恰一次 → driver.stat 兜底；回源错误容忍不落 404）。②`Vfs` 两薄壳 + `dir_cache` 字段（零构造变更）+ 写侧失效三调用点（commit_put/create_dir/remove_file）。③`sync::is_in_flight_row` 提取（判据单点同源；replace 臂布尔等价、多一次 exists syscall 已注释）。④`VfsError::EncryptedInstance`（文案指路 cydrive sync）+ webdav `vfs_err`/winfsp `ntstatus_for` 编译强制扩展（Forbidden/ACCESS_DENIED，RT3 真机复核）。

**TDD 证据**：八组红→绿（组 A-H 各有真实红输出；用例 10 单飞经变异验证补红——删闸内世代重查后 8 次 list）；门退化用例 9 以「upload/open 零调用+行级逐字等价」作证（窄面无 list 可数）。

**主会话审查要点**：门序忠实伪代码（加密+窄面=退化为 db 读、不物化即无 D10 危害；加密+宽面=拒收）；sync.rs replace 臂 `!( !uploaded && exists ) && exists ≡ uploaded && exists` 布尔等价核实；NotFound 臂范围与 D7 红线一致。

**执行期自主裁决（未询问，可逆）**：NotFound 臂的本层行删除也走逐条 stat 双确认（计划「从简」取保守解读——与 prune 臂共用 `delete_confirmed`，NotFound 场景罕见成本可忽略）；加密判定用 `cfg.encryption_password.is_some()`（Vfs 无 CyDriveConfig，取 commit_put 的 is_encrypted 判据同源）；并发归并用「闸内重查到达世代」而非纯 TTL（否则毁 D5 顺序强制刷新）。

**门禁**：readthrough 16/0、rebuild 4/0、sync 27/0 零漂移；workspace **1623/0/52**；clippy/fmt/check_layers/scan_secrets 绿。


### RT1（2026-09-22，子代理实现 + 主会话审查）

**完成**：commit `834b4fa`——①L2 探针 `CloudTransport::as_driver`（默认 None，`as_inbound`/`as_chat` 同款形态；六宽面 transport_face 字段名统一 `driver: Arc<XxxDriver>`，各 +1 行 `Some(self.driver.as_ref())`；GrammersTransport 未动=telegram 零变化）；②`materialize.rs` 新建（`materialize_entry` 平移保真：K6 非 i64 句柄→`Some(0)`、K11 单容器 chunk、coalesce 列留 NULL、词汇→行键 `/` 前缀约定；返回 `MaterializedRow = FileRecord` 供 read-through 消费；`list_all_pages` depth-1 全页归集 limit 512）；③rebuild.rs 删 `upsert_entry` 改调，outcome 计数留 rebuild 侧（时机语义与原版一致：物化成功才计）。

**TDD 红绿证据**：三条红（`unresolved import cloudkit_core::materialize` / `no method named as_driver` ×2）→ 绿（materialize 3/0、types 21/0、rebuild 4/0 零漂移、六驱动 lib 全绿）。

**适配说明（计划骨架→现实）**：`RelPath::parse`→`RelPath::new`；`EntryId::new(VolumeId, BackendHandle)`；「materialized row vanished」臂经 `RebuildError::from(DbError::from(QueryReturnedNoRows))` 发声（`DbError` 私有字段无法从 &str 构造、`RebuildError` 形状不变）；宽面 Some 腿钉在 ck-local transport_face 测试（可离线构造），余五驱动以 lib 测试+clippy 覆盖。

**门禁**：workspace 1607/0/52（基线 1602+5）、clippy -D warnings、fmt --check、check_layers（16 manifests）、scan_secrets 全绿。


### RT0（2026-09-22，主会话）

**完成**：三路并行探查（Vfs/db 行语义 / 四消费面 async 边界与装配链 / rebuild×队列耦合×测试双轨）→ 设计定稿 D1–D10 → 计划落档。

**关键探查结论（决定方案形态的四条）**：
1. **物化映射可直接复用**：`rebuild.rs:150-215 upsert_entry` 的 Entry→行映射（K6 句柄降级 `Some(0)`、K11 单容器 chunk）已被读链消费过（K74 pan115 rebuild 腿真机）；`remote_handle_for`（vfs.rs:702-741）对 `Some(0)` 放行、路径形后端用 `RemoteHandle.path` 寻址——read-through 物化行**开箱可读**，无行语义缺口（明文卷）。
2. **探针先例现成**：`as_inbound`/`as_chat`（transport/mod.rs:204-210）= 默认 None + 只降级不 panic；六个 transport_face 全持 `Arc<驱动>` + `into_driver()`（探针一行）；GrammersTransport 无驱动 → 默认 None 天然正确（telegram 零变化）。
3. **零 files DDL 变更可行**：sweep 判据复用既有 `updated_at` 列（upsert 自然刷新 → 跨续跑保护），只加 `rebuild_state` KV 表（sync_mirror 同先例）——R6 冻结契约零触碰。
4. **in-flight 判定原文可复用**：sync.rs:579-580 `!is_uploaded && local_copy_exists`——reconcile 两侧豁免的判据与 sync 语义单点同源。

**自主裁决（未询问，可逆）**：①`read_dir_fresh` 每调用强制 revalidate（D5，RaiDrive 对等）而非 TTL 窗——胜在新鲜度与语义简明，代价是每视图 1 次 list（用户点赞的 J: 盘同款成本）；②stat TTL 窗定 5s（D6，实测定值，`with_ttl` 缝可调）；③删除双确认上限 32 条/目录（D7，超限整批跳过+告警）；④rebuild 单趟上限默认 20 万条目（D8，RebuildTuning 注入可调）；⑤api_files 保持索引视图（D9，防全表 API 暗变成全树遍历）；⑥加密卷 read-through 明确拒收（D10——list 报密文 size，物化即破坏「size=明文」R6 契约与 AEAD 预算数学；指路 sync）。回滚 = revert 本批 commit。

## 风险与未覆盖（随批更新）

- **DirCache flights/generations map 无上界**（RT2）：每目录一条小记录，百万目录卷 ≈ 数 MB 慢增长；计划未要求上界——真机矩阵后评估是否加 pan115 式 LRU
- **EncryptedInstance 消费面映射（Forbidden/ACCESS_DENIED）**为 RT2 选定的保守类，RT3 接线后真机复核 Explorer 呈现
- **门序执行期解读**：加密+窄面实例（telegram 加密卷）经 D2 门退化为 db 读（不物化即无 D10 危害），D10 拒收文案只对加密+宽面组合发声——rebuild 的 ensure_plaintext_instance 无差别拒，read-through 因退化臂安全而不需要同款无差别拒（收口审查批复核）
- **ck-webdav conformance 偶发（RT1 观察，既有负载敏感）**：三次全量 `-j 4` 跑中一次 `conformance_suite_offline` 失败（loopback 临时端口参照桩），隔离复跑 8/8 绿、其后两次全量绿；本批对 ck-webdav 唯一改动是 conformance 不调用的 provided 方法，无因果——挂账观察，若复现考虑另立降并发/重试裁决
- 加密卷 read-through 不做（D10，密文 size→明文换算不可靠）——挂账
- api_files 全表视图新鲜度（D9）——挂账
- ghost 行清理归 K4 delete-wiring 旧账——不扩 scope
- stat TTL 窗 5s = 实测定值，真机后可调
- 待 RT5：真实广域网链路形态（WSL2 回环数字口径，sftp SF5 判例）
