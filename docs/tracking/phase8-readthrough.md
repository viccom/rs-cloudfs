# Phase 8：read-through 按需逐层索引 任务跟踪单

> 计划：`docs/plans/2026-09-22-readthrough-index.md` ｜ 需求口径：负责人 2026-09-22 拍板路线 C（B 为主 + A 最小集），三点要求：①先出详细可行方案与计划；②六后端+未来驱动共性提炼；③证明架构增强非破坏
> 基线：main@86003a0（workspace **1602/0/52** + 五门禁绿；Phase 7 webdav 已合入并反向复核毕）
> 状态：**RT4 完成（2026-09-22）——RT5 待开工（真机矩阵+文档+收口）**
> worktree：`feat/readthrough-index`，独立 target（共享 CARGO_TARGET_DIR 双指纹既有教训）
> 编号：裁决 K83（待入 decisions）；批次 RT0–RT5 顺序执行，RT3 与 RT4 可并行派发

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| RT0 | 方案+计划落档（无生产代码） | ✅ 2026-09-22 | 计划 `2026-09-22-readthrough-index.md`（requirement-analyzer 四段 + D1–D10 裁决 + 共性五件套 + RT1–RT5 任务分解 + 架构合规证明表）；本跟踪单；三路探查硬事实入计划 §1 | 本批日志 |
| RT1 | L2 探针面 + L3 物化提取 | ✅ 2026-09-22 | `as_driver` 探针（trait 默认 None + 六宽面 transport_face 各一行；telegram 零变化）+ `materialize.rs`（`materialize_entry` 自 rebuild.rs:150-215 逐字段平移，K6/K11 形态保真 + `list_all_pages` 归集器）+ rebuild 改调；rebuild 既有 4 测试零漂移 | commit `834b4fa`；workspace **1607/0/52**（+5）；五门禁绿；批次日志 RT1 节 |
| RT2 | readthrough 原语（read_dir_fresh/stat_fresh/reconcile/DirCache） | ✅ 2026-09-22 | `readthrough.rs`（DirCache：TTL 5s+with_ttl 缝/单飞闸+世代归并/就近失效；reconcile：upsert 侧 in-flight 豁免+双确认 prune+32 上限+NotFound 臂本层删除+stale-if-error）+ Vfs 两薄壳 + 写侧失效三调用点 + `VfsError::EncryptedInstance`；门序 D2 退化→D10 拒收；`sync::is_in_flight_row` 提取单点同源 | commit `aa47649`；readthrough 16/0、rebuild 4/0 零漂移、sync 27/0 零漂移；workspace **1623/0/52**；五门禁绿；批次日志 RT2 节 |
| RT3 | 四消费面接线（网关/仪表盘/winfs；bot 零改动） | ✅ 2026-09-22 | 三面只换数据获取函数：webdav `read_dir`/`metadata`/`open` 读臂改 `stat_fresh`/`read_dir_fresh`（存在性+is_dir 预检保留经 stat_fresh，NotFound/Forbidden 语义不变）；winfsp `dir_entries`（仅 marker=None 腿到达）改 `read_dir_fresh`，`meta_for`/`open_with_read` 经新 `resolve_row_fresh`（精确命中零网络 → stat_fresh → K45 大小写扫描；NotFound 不终判、其余错误传播；guard/miss 判定路径保持纯 db）；web `api_list` 改 `read_dir_fresh`（驱动 NotFound→404、其余错→500；窄面空目录存在性探针保留=A6；`api_files` 零改动 D9 且测试钉零 list；bot `/ls` 零改动 D2） | commit `2d00c64`；三包 153/0、winfsp 腿 118/0+1ign、workspace **1627/0/52**（+5=webdav 3+web 1+winfsp 腿 1）；五门禁绿；批次日志 RT3 节 |
| RT4 | rebuild 三件套（续跑/上限/sweep） | ✅ 2026-09-22 | D8①②③ 全落地：database.rs 新 `rebuild_state(key,value)` KV 表（sync_mirror 同先例，零 files DDL）+ `rebuild_state_get/set/clear` + `sweep_unseen`（is_uploaded=1 且 updated_at<锚点；chunks 同事务显式级联照 delete_file 先例）；rebuild.rs boxed 递归改显式 VecDeque 工作队列 + 每目录完成落盘（pending JSON 队列/scan_started_at 首趟锚点续跑复用/entries_done 累计）+ `RebuildLimits`（max_entries 缺省 20 万/趟、目录粒度检查不重扫已完成目录）+ `RebuildOutcome` 扩 `pruned`+`interrupted: Option<EntriesBudget/TimeBudget>`（Display 含 rerun to continue）+ 完成趟才 sweep 后清检查点；cli `RebuildTuning.max_entries` + 离线路径补 15min 预算（limits(true)）+ 活实例执行器喂 max_entries（time_budget=None 留 R4 监督）+ 三入口文案（main 单/多卷 + RebuildTask Done 臂按 interrupted 分叉）；既有 4 测试：2 处 RebuildOutcome 字面量 ..Default::default() 编译适配 + ghost 行断言按 D8③ 新契约改（唯一语义变更）| commit 见批次日志；rebuild **9/0**（+5）、readthrough 16/0 零漂移、cli rebuild 3/0 + runtime_rebuild 10/0；workspace **1632/0/52**；五门禁绿；批次日志 RT4 节 |
| RT5 | 真机矩阵 + 文档 + 收口（含深度审查批，K78 形态） | ⬜ 待开工 | — | — |

## 批次日志

### RT4（2026-09-22，子代理实现）

**完成**：commit（本批）——D8 三件套——
①**持久化续跑**：`database.rs` 新 `rebuild_state(key TEXT PRIMARY KEY, value TEXT)`（独立 `IF NOT EXISTS` 批次，R6 零 files DDL 变更）+ `rebuild_state_get/set/clear`（sync_mirror 三件套同形）；`rebuild.rs` 的 K11 boxed 递归 `rebuild_dir` 重构为显式 `VecDeque<RelPath>` 工作队列——每完成一个目录即把「剩余队列 JSON + scan_started_at + entries_done」落盘；`scan_started_at` 由**首趟**写入、续跑复用绝不重置（sweep 保护 3c 的根基）；中断臂一律不落盘（磁盘检查点=最后一个完成目录的状态，被弹出未列的目录仍在队列上）；损坏检查点降级 warn+全新扫描（可证安全：重扫重物化即免疫）。**完成趟（队列空）**：`sweep_unseen` → `rebuild_state_clear`；**未完成趟**（预算/超时/List 错）绝不 sweep 不清队。
②**总上限+离线预算**：core 新 `RebuildLimits{max_entries=200_000, time_budget:Option<Duration>}` + `rebuild_from_backend_with` 缝（三参旧签名保留=既有测试零漂移）；**目录粒度检查**（pop 后查）——一趟绝不在目录中间因 entries 停、已完成目录绝不重列（续跑测试以 list 计数钉死=目录总数）；deadline 额外在页间检查（中途停=目录推回队首重列，幂等）。`RebuildOutcome` 扩 `pruned:usize` + `interrupted:Option<RebuildInterrupted>`（EntriesBudget/TimeBudget，Display=「the entries/time budget ran out; rerun to continue」）；cli `RebuildTuning` 加 `max_entries`（缺省 20 万）+ `limits(with_time_budget)` 换算缝；**离线路径补 15min 预算**（`run_rebuild_command`/`run_rebuild_with_driver` 走 `limits(true)`）；活实例生产执行器喂 `limits(false)`（墙钟归 R4 监督既有形态）。三入口文案：main.rs 单卷臂与多卷 Ok 臂按 interrupted 分叉（含 rerun `cydrive rebuild` 指引），完成态追加「N stale row(s) pruned」；`RebuildTask` Done 臂 split——interrupted=「rebuild interrupted: … rerun REBUILD to continue (cursor is persisted)」+warn，完成=「finished: …; N stale row(s) pruned」+info；控制通道 REBUILD/web Refresh 本就是受理+后台任务汇报形态，经 RebuildTask 正确呈现（零接线）。
③**完成趟 sweep**：`database.rs::sweep_unseen(scan_started_at)->usize` = `DELETE FROM files WHERE is_uploaded=1 AND updated_at < ?`（本趟/早趟 upsert 行 updated_at≥锚点天然免疫；in-flight is_uploaded=0 排除；updated_at NULL 保守保留）+ chunks 同事务显式子删（delete_file 先例，不依赖 FK 级联）——三重保护齐活；sweep 的 files DELETE 会敲门 sync doorbell（对端学到删除，期望语义）。

**TDD 证据**：红=新测试先行，`cargo test -p cloudkit-core --test rebuild` 编译失败 20 错（`rebuild_from_backend_with/RebuildLimits/RebuildInterrupted/rebuild_state_get/outcome 新字段`全不存在）；绿=9/9（既有 4 + 新 5：①续跑 max_entries=50 首趟 root+d1..d4 停、二趟恰 +1 list 总数=6=可列目录数、检查点三键清空；②entries 上限中断文案含两段语义+stale 行带 chunks 幸存；③sweep 三重保护 a/b/d+pruned 计数+chunks 级联空；③c 跨续跑早趟行幸存（首趟锚点复用防回归钉）；④TimeBudget 零预算趟零 list、绝不 sweep）。邻面回归 readthrough 16/0、cli rebuild 3/0、runtime_rebuild 10/0 全零漂移。

**既有测试适配（如实报告）**：①`RebuildOutcome` 两处字面量（core tests/rebuild.rs 两测试+lib.rs 单测缝+runtime_rebuild.rs 一处）补 `..Default::default()`——纯编译适配断言语义不动；②runtime_rebuild.rs 三处 `RebuildTuning{...}` 字面量补 `..RebuildTuning::default()`——同上；③**唯一语义变更**：`rebuild_upserts_over_stale_rows_and_reports_counts` 的 ghost 行断言由「survives（K11 no-pruning）」改为「被完成趟 sweep 删除」——本批特性（D8③）明文推翻 K11 不 prune 契约，模块文档同步改写；stale pending 行幸存断言原样保留（is_uploaded=0 豁免）。

**执行期自主裁决（未询问，可逆）**：①上限检查放目录粒度而非页间（单目录可超上限，文档明示；换页间检查会让续跑测试的 list 计数不确定）；②活实例路径 time_budget=None（R4 abort 监督已是墙钟权威，core 再设同值预算只会抢跑改变出口形态）；③新增 `RebuildError::Serde` 变体（检查点序列化错误的显式承载，readthrough 归一 match 补防御臂）；④sweep 不suppress doorbell（删除传播对端=期望行为）。

**门禁**：workspace `-j 4` **1632/0/52**（基线 1627+5）；clippy --workspace --all-targets -D warnings 绿（修 while_let_loop+doc_lazy_continuation 后）；fmt --check 绿；check_layers 16 manifests 绿；scan_secrets 绿。


### RT3（2026-09-22，子代理实现）

**完成**：三面接线（每处=换数据获取函数，零机制代码落消费面）——
①**webdav**（`cloudkit-webdav/src/lib.rs`）：`read_dir` 的行预检 `self.row(&rel)?` → `vfs.stat_fresh().await`（保留原预检双语义：缺失→NotFound、文件→Forbidden；根跳过），列表 `db.list_dir` → `vfs.read_dir_fresh().await`（D5 每 PROPFIND 现查）；`metadata` 非根臂 → `stat_fresh`（根恒合成 RowMetaData::root 不动——stat_fresh 根在窄面退化臂会 NotFound，不能用于根）；`open` 读臂行获取 → `stat_fresh`（写臂/其余臂不动）；加密卷经 `vfs_err` 的 `EncryptedInstance→Forbidden`（RT2 已扩，D10 语义）。
②**winfsp**（`cloudkit-winfsp/src/fs.rs`）：`dir_entries` 经 `bridge.block_on(vfs.read_dir_fresh(rel))`——它只被 `prepare_enumeration` 的 `marker=None` 臂调用，「仅首次枚举强制刷新」结构性成立，marker 协议逐字不变（面级测试钉：marker 续页零 list）；`meta_for`/`open_with_read` 行获取改走新私有 `resolve_row_fresh`（=精确 get_file 零网络快路径 → `bridge.block_on(stat_fresh)` → K45 大小写扫描下沉为共享 `scan_parent_insensitive`；stat_fresh 的 NotFound 不终判——FSD 大写形态在 K45 扫描仍可解，其余错误照 `fsp_error` 传播）；`resolve_row` 本体保持纯 db（`row`/`require_dir_parent`/`lookup_miss`/delete/rename 解析等 guard 路径零网络），K44 元数据网络空闲契约在窄面逐字保持（metadata.rs `assert_no_transport_calls` 全绿）。
③**web**（`cloudkit-web/src/lib.rs`）：`api_list` 改 `volume.vfs.read_dir_fresh(&rel).await`（async handler 直持有 `Arc<Vfs>`，同形接入）；错误映射 NotFound→404「Directory not found」（原文案）、其余→500（绝不造空列表）；「空+无行→404」存在性探针保留——窄面退化臂的 `db.list_dir` 区分不了存在空目录与缺失（A6 零漂移），宽面根空盘 404 冻结语义同款保留；`api_files` 一字未动（D9），测试钉死其零 list 调用。
④**bot `/ls` 零改动**（D2：telegram 窄面走退化臂）。

**TDD 证据**：逐面先红后绿——红：webdav `readthrough_empty_db_propfind…`（`left: []` vs `["Documents","Photos","readme.txt"]`）+ `readthrough_keeps_the_read_dir_guards…`（NotFound vs Forbidden）+ `readthrough_open_read…`（`open a remote-only file: NotFound`）；winfsp `readthrough_empty_db_enumeration…`（`left: []`）；web `api_list_readthrough…`（404 vs 200）。绿：同用例全过 + 计数断言（webdav 2 视图=2 list；winfsp 枚举=1 list、marker 续页仍 1、open=+1 父重列；web api_list=1、api_files 后仍 1）。宽面 transport 替身按 RT2 `WideTransport<D>`+计数驱动形态逐面自铸（测试 helper 不跨 crate 复用）；`cloudkit-storage`+`async-trait` 以 **dev-dependencies** 落三包（生产图零变化，check_layers 16 manifests 复核）。

**执行期自主裁决（未询问，可逆，回传说明）**：①webdav `read_dir` 保留「存在性+is_dir」预检但改经 stat_fresh（原 row() 预检的 NotFound/Forbidden 语义面级测试钉死）——代价：宽面冷路径（父窗过期或行缺失）多 1 次父重列，导航形（先列父再进子）稳态每视图恰 1 次 list，真机矩阵复核 Explorer 实际流量；②winfsp stat_fresh 的 NotFound 不终判而落 K45 扫描（大写形态不可被精确 NotFound 否决），EncryptedInstance/传输错传播（加密卷 open 面=ACCESS_DENIED 拒绝而非造空，D10）；③api_list 驱动 NotFound 与「空+无行」404 分立（原文案不动）。

**门禁**：三包 `cargo test -p cloudkit-webdav -p cloudkit-web -p cloudkit-winfsp` **153/0**；winfsp 腿 `--features winfsp` **118/0**+1ign（117+1）；workspace `-j 4` **1627/0/52**（基线 1623+5）；clippy -D warnings / fmt --check / check_layers（16 manifests）/ scan_secrets 全绿。

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

- **webdav read_dir 冷路径双 list**（RT3）：stat_fresh 预检在父 TTL 窗过期或行缺失时多一次父重列（导航形稳态每视图恰 1 次，面级测试钉 2 视图=2 list）；Explorer 实际流量归真机矩阵复核（RT5）
- **winfsp resolve_row_fresh 的 NotFound 落 K45 扫描**（RT3）：精确 NotFound 不终判（大写形态保护）；加密+宽面卷 open 面=ACCESS_DENIED（D10 传播），Explorer 呈现待真机复核（RT2 既有挂账承接）
- **DirCache flights/generations map 无上界**（RT2）：每目录一条小记录，百万目录卷 ≈ 数 MB 慢增长；计划未要求上界——真机矩阵后评估是否加 pan115 式 LRU
- **EncryptedInstance 消费面映射（Forbidden/ACCESS_DENIED）**为 RT2 选定的保守类，RT3 接线后真机复核 Explorer 呈现
- **门序执行期解读**：加密+窄面实例（telegram 加密卷）经 D2 门退化为 db 读（不物化即无 D10 危害），D10 拒收文案只对加密+宽面组合发声——rebuild 的 ensure_plaintext_instance 无差别拒，read-through 因退化臂安全而不需要同款无差别拒（收口审查批复核）
- **ck-webdav conformance 偶发（RT1 观察，既有负载敏感）**：三次全量 `-j 4` 跑中一次 `conformance_suite_offline` 失败（loopback 临时端口参照桩），隔离复跑 8/8 绿、其后两次全量绿；本批对 ck-webdav 唯一改动是 conformance 不调用的 provided 方法，无因果——挂账观察，若复现考虑另立降并发/重试裁决
- 加密卷 read-through 不做（D10，密文 size→明文换算不可靠）——挂账
- api_files 全表视图新鲜度（D9）——挂账
- ghost 行清理归 K4 delete-wiring 旧账——不扩 scope
- stat TTL 窗 5s = 实测定值，真机后可调
- 待 RT5：真实广域网链路形态（WSL2 回环数字口径，sftp SF5 判例）
