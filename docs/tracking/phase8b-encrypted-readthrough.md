# Phase 8-B：加密卷 read-through 放开 任务跟踪单

> 计划：`docs/plans/2026-09-23-encrypted-readthrough.md` ｜ 批准链：负责人 2026-09-23 三点指示 → K84 立项（D10 修订留痕）→ 深度分析五环 + 计划 §0 八项代码查证 → 计划落档
> 基线：`feat/readthrough-index`@77a58cc（Phase 8 RT0–RT5+审查批+文档批已落，workspace 1643/0/60 五门禁绿，**待合入**）
> 状态：**计划已批准（B1–B6 随批生效）——EB1 完成（2026-09-23，五红→绿 + 七门禁绿）；EB2 待开工**
> worktree：`feat/readthrough-index`（与 Phase 8 同一支，连续批次）；独立 target
> 编号：执行记录入 decisions 用 **K85**；批次 **EB1–EB4**

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| EB0 | 计划期八项代码查证（随计划完成） | ✅ 2026-09-23 | 计划 §0 八条：upsert 冲突集覆盖 cipher 列 / AeadV2::new() 默认分块无配置缝 / RowMetaData.len 承重 / K47 流式分流 / chunks 仅取消息 id / sha256 读面零引用 / 闸两点位+help 文案 / plan A pending 出口 | 计划 §0（本批日志） |
| EB1 | cipher 真相物化（B1+B3+B4：保留语义 upsert + 闭式反推 + 去 H1 退化臂） | ✅ 2026-09-23 | 五红→绿全留证（用例 12 断言红 / T1 编译红 / T2 防御臂断言红 / T4 降级断言红 / T3 尺寸断言红）；`upsert_materialized` 保留集 + `CipherCtx` + `plaintext_len_from_container` 闭式 + readthrough 双签名换 cipher + Vfs 两薄壳同源 ctx + rebuild `_with_ctx` 缝；七门禁绿（workspace **1647/0**、clippy/fmt/layers/secrets 零告警）；既有断言零漂移（rebuild 11、readthrough 其余 20、materialize 既有 5） | 本批日志 EB1 |
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

### EB1（2026-09-23，实现子代理，worktree feat/readthrough-index）

**落地件**（单提交 8 文件 +620/−104 = 7 个代码/测试文件 +588/−104 + 本跟踪单）：
1. **`database.rs::upsert_materialized`**（B3）：INSERT 含 cipher 列；ON CONFLICT 保留集 = `is_encrypted = CASE WHEN files.is_encrypted=1 THEN 1 ELSE excluded END`（既有 1 永不降级、初值只填空）、`encryption_scheme` 仅 `files.is_encrypted=0` 时写、`size = excluded.size`（= Rust 侧闭式算好的真相尺寸——INSERT/CONFLICT 同值的 `?derived` 等价形态，SQL 零反推计算）、updated_at 照刷（sweep 免疫）。`upsert_file`/`upsert_file_scheme`/上传/sync 路径一字未动。
2. **`materialize.rs`**：`CipherCtx { enabled, scheme }` + `from_cfg(&VfsConfig)`（与 vfs.rs:500 同源判据）；`plaintext_len_from_container`（v1 = ct−44；v2 = 1MiB 闭式 `n−1=(body−16)/(CHUNK+16)`、`last=(body−16)%(CHUNK+16)`、`pt=n−1·CHUNK+last`；空件/整倍数按 v2.rs 格式文档；三防御臂——v1 短于 44、v2 短于 34+16、尾块超 CHUNK、未知方案 → 保守回 ct + warn）；chunk 常量复用 `cloudkit_crypto::v2::{HEADER_SIZE, TAG_SIZE, DEFAULT_CHUNK_SIZE}`（与 `AeadV2::new()` 默认 1MiB 同源注释钉住）；`materialize_entry(db, entry, cipher: Option<&CipherCtx>)`——`None` = 今日逐字行为，`Some(ctx)` = 真相三层优先（既有 `is_encrypted=1` 行 scheme 保留并按其重推 size > ctx 初值；明文真相 entry.size 原样；Dir 臂恒 upsert_file 明文）；chunks 行 size 维持 `entry.size`（密文——加密上传 receipt 同记密文分块，读面不消费 chunk.size）。
3. **`readthrough.rs`**：read_dir_fresh/stat_fresh 两处 H1 加密退化臂删除；签名 `encrypted_instance: bool` → `cipher: Option<CipherCtx>`（按值 Copy，内部 `as_ref()` 分派进 materialize 缝）；D2 门/单飞/双确认/D7 上限/stale-if-error/in-flight 语义一字未动；模块文档 D10 条目改写（rebuild 拒收句保留——闸 EB3 才删）。
4. **`vfs.rs` 两薄壳**：`Some(CipherCtx::from_cfg(&self.cfg))` 传入（password.is_some() + cfg.encryption_scheme）。
5. **`rebuild.rs`**：`walk_one_dir` 接 cipher 参数；新 `rebuild_from_backend_with_ctx(driver, db, root, limits, cipher: Option<CipherCtx>)`——`rebuild_from_backend`/`rebuild_from_backend_with` 三参/四参签名与全部调用点零改动、缺省 `None` = 禁用走旧路；闸与 cli 调用点本批未动（EB3）。
6. **测试**：T1 同构 / T2 闭式边界+防御臂 / T3 防震荡 / T4 只填空（tests/materialize.rs +4）；用例 12 替换为「加密+宽面回源物化」（list=1 + is_encrypted/scheme/size 真相语义 + Dir 恒明文；D10→K84 计划明文授权的断言变更）。

**TDD 红→绿五组证据**（执行序 case12 → T1 → T2 → T4 → T3，T4 先于 T3 使共享的真相解析实现各得真实红）：
- 用例 12 红：`list_calls left:0 right:1`（H1 臂仍退化）→ 删臂+接 cipher 后 21/21 绿；
- T1 红：`E0061 takes 2 arguments but 3 supplied`（+E0432 未解析导入）→ Stage1（CipherCtx/闭式/三参签名）后绿；
- T2 红：`未知方案防御臂 left:1184 right:1234`（计划 5 条格式边界在朴素式下已过，防御臂缺口）→ Stage2（三防御臂 + 显式 aead_v2 分支）后绿；
- T4 红：`明文实例回源绝不降级既有加密行, got is_encrypted=false`（B1 洞本尊）→ Stage3（`upsert_materialized` CASE 保留集，cipher=Some 全走新方法）后绿；
- T3 红：`按保留的 gcm 闭式重推 left:3 right:9`（scheme 已由 CASE 保 gcm、size 仍按 cfg 猜）→ Stage4（既有行真相读取：scheme 保留并按其重推 size）后绿。

**执行期自主裁决（如实入档，均不触碰 B1–B6）**：
- **计划 T2 骨架算式笔误修正**：`34+16+1_048_576+16`（=1048642）与 `34+5+16+1_000_000+16`(=1000071)/`1_048_581−34−16`（=1048531）三处与 §0.4 格式权威及 T1 的 `1_048_626` 自相矛盾——按格式文档改正字面量（恰整倍数 = `34+1MiB+16`；多块短尾 = `34+(1MiB+16)+(1_000_000+16)`），断言语义（空件/整倍数/多块短尾/防御臂）与计划意图逐条保留；
- **`?derived` 形态等价**：以 `size=excluded.size` 落 SQL（FileUpsert.size 契约 = Rust 真相尺寸，INSERT 与 CONFLICT 绑同值），语义 = 计划「Rust 算好传入、SQL 无需自算」，非从 entry 盲取；
- **readthrough 签名形态**：`Option<CipherCtx>` 按值（Copy）——计划给了 `Option<CipherCtx>` 与 `&CipherCtx` 两可，选与既有 `bool` 按值形态最贴近者；
- **rebuild 缺省 ctx**：`None`（= `materialize_entry` 逐字旧路，「缺省=禁用走旧路」的最强兑现，9→11 既有 rebuild 测试零漂移由构造保证）；`Some{enabled:false}` 明文实例形态由 Vfs 薄壳供给（T4 钉的正是该路径）；
- **T2 防御臂断言入 T2**（计划骨架未列、Step 3 规格列了）：规格 → 测试，防回归。

**门禁证据**（全绿）：
- `cargo test -p cloudkit-core --test materialize --test readthrough --test rebuild` → **9 / 21 / 11 passed，0 failed**；
- `cargo test -p cloudkit-web --test multivolume -p cloudkit-webdav --test fs_adapter` → **16 / 34 / 11 passed，0 failed**（webdav multivolume 随 `--test multivolume` 一并执行）；
- `cargo test --workspace --no-fail-fast -j 4` → **TOTAL passed: 1647, failed: 0**（基线 1643 + T1–T4；ignored 60 未动——本批零新增 `#[ignore]`）；
- `cargo clippy --workspace --all-targets -j 4 -- -D warnings` → Finished 零告警（一处 doc 列表缩进当场修）；
- `cargo fmt --all && cargo fmt --all -- --check` → FMT_OK（3 文件格式化后复跑三定向套件 9/21/11 仍绿）；
- `scripts/check_layers` → OK（16 manifests，零 R1 违例）；`scripts/scan_secrets` → OK（零命中）。

## 风险与未覆盖（随批更新）

- v1 无 magic 残余歧义（B6：加密码晚于存量明文 + 索引丢 + 配置=gcm）——文案+sync 双出路，远期 manifest 挂账延续
- 首次 PROPFIND 在「初值猜错未首读」窗口显示初值尺寸（一次性自愈，EB2 注明接受残余）
- 真网加密腿依赖 token 可用性（K77 轮换纪律）；离线三腿已覆盖机制本体
- Phase 8 主链（RT0–RT5+审查批）仍为**待合入**态——EB 批同分支叠加，合入指令到时一并处理
