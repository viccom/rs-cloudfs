# Phase 8-B：加密卷 read-through 放开 任务跟踪单

> 计划：`docs/plans/2026-09-23-encrypted-readthrough.md` ｜ 批准链：负责人 2026-09-23 三点指示 → K84 立项（D10 修订留痕）→ 深度分析五环 + 计划 §0 八项代码查证 → 计划落档
> 基线：`feat/readthrough-index`@77a58cc（Phase 8 RT0–RT5+审查批+文档批已落，workspace 1643/0/60 五门禁绿，**待合入**）
> 状态：**计划已批准（B1–B6 随批生效）——EB1 完成（2026-09-23，五红→绿 + 七门禁绿）；EB2 完成（2026-09-23，五红→绿 + 六门禁绿）；EB3 待开工**
> worktree：`feat/readthrough-index`（与 Phase 8 同一支，连续批次）；独立 target
> 编号：执行记录入 decisions 用 **K85**；批次 **EB1–EB4**

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| EB0 | 计划期八项代码查证（随计划完成） | ✅ 2026-09-23 | 计划 §0 八条：upsert 冲突集覆盖 cipher 列 / AeadV2::new() 默认分块无配置缝 / RowMetaData.len 承重 / K47 流式分流 / chunks 仅取消息 id / sha256 读面零引用 / 闸两点位+help 文案 / plan A pending 出口 | 计划 §0（本批日志） |
| EB1 | cipher 真相物化（B1+B3+B4：保留语义 upsert + 闭式反推 + 去 H1 退化臂） | ✅ 2026-09-23 | 五红→绿全留证（用例 12 断言红 / T1 编译红 / T2 防御臂断言红 / T4 降级断言红 / T3 尺寸断言红）；`upsert_materialized` 保留集 + `CipherCtx` + `plaintext_len_from_container` 闭式 + readthrough 双签名换 cipher + Vfs 两薄壳同源 ctx + rebuild `_with_ctx` 缝；七门禁绿（workspace **1647/0**、clippy/fmt/layers/secrets 零告警）；既有断言零漂移（rebuild 11、readthrough 其余 20、materialize 既有 5） | 本批日志 EB1 |
| EB2 | 首读内容校验与回写（B2+B6：容器头/本地长度权威 + 定向 UPDATE + 双假说文案） | ✅ 2026-09-23 | 五红→绿全留证（流臂错行红 panic / hydrate size 红 2500≠5000 / gcm 改判红 `Crypto(AuthFailed)` / B6 红回 Stream / 面级红 145904≠150000）+ **审查回派 K84.2 双试二红→绿**（红4 改通路级红 admission 回 Err / 红6 新增红同 Err）；`fix_cipher_columns` 定向三列 + `first_read_admit`（34B 头读→magic/闭式交叉→回写→带窗构造，B4 零额外往返；无 magic → Hydrate 转 K84.2 双试）+ hydrate **双向**改判（gcm 臂遇 magic→v2 / v2 臂无 magic→试 v1 自愈回写 gcm）+ 解密后本地长度回写 + B6 双文案（挂 `Crypto` 模板与 `UnsupportedEncryptionScheme` 扩展，变体零动）+ 网关行重读传导；六门禁绿（workspace **1653/0/60**、clippy/fmt/layers/secrets 零告警）；既有断言零漂移（含 web_e2e 15d gcm 零窗口、vfs 916/979 `Crypto(_)`、fs 35、vfs_open_read 18） | 本批日志 EB2 + 审查回派 |
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

### EB2（2026-09-23，实现子代理，worktree feat/readthrough-index）

**现实核对（计划 Task 2 要求的两点，先查后做）**：
1. **`enc_stream.rs` 现实：头是懒读的**——`DecryptingTransport` 构造零 I/O，34B 头 + KDF 发生在**首次 `open_range`**（enc_stream.rs 「Laziness」节），不是构造期。而 B2 要求校验先于 Content-Length（`open_read` 返回前）→ 校验无法「挂在首窗同一读」而不提前。解法 = 把那一读**提前到 admission**（`Vfs::first_read_admit` 拉头→验 magic→KDF），解析出的窗口经**新构造 `DecryptingTransport::new_with_window`** 喂回（`OnceLock` 直接落种子）——对流式臂仍是**一次 (0,34) 读、一次 KDF**，序列与今日逐字相同（fs 6h 既有 `open_range_calls == [(0,34),(span…)]` 断言原样绿），零额外往返兑现；红1 另钉 admission 恰一次 (0,34)。
2. **ct 总长（闭式分母）的现实来源 = chunks 行之和**：v2 头只有 magic/version/salt/迭代数/分块（§0.4，**无长度字段**），`CloudTransport` 无 stat/size 面，`handle.total_size` 对加密行是 u64::MAX 哨兵（E-5）——唯一在库的容器长 = `chunks.size`（三方同源：upload `persist_success` 按密文边界记 chunk 尺寸 / `materialize_entry` 记 `entry.size` 密文长 / sync 载荷携带）。§0.5「读面不消费 chunk.size」的边界是寻址/窗口数学，本处是**一次性校验分母**，不越界。无 chunk 行（row-id 回退形态，生产加密行不产生）→ 无分母 → 逐字今日行为（如实挂账：该形态不做首读校验，非容器在首窗仍以 `Unavailable` 响亮失败，但 Content-Length 修正不在该形态覆盖内）。

**落地件**（7 代码/测试文件 + 本跟踪单）：
1. **`database.rs::fix_cipher_columns(id, scheme, size)`**：定向 UPDATE 只动 `encryption_scheme/size/updated_at`（`set_cached_flag` 同族「定向列写」形态；sha256/msg_id/mime/mtime/is_cached 零触碰）；updated_at 照刷（D8③ sweep 免疫）；**doorbell 不压制**（size/scheme 是 sync 载荷列——修正须经 chokepoint hook 传播对端）；affected 0 = 行并发消失作良性 Ok；幂等（同值重写）。
2. **`vfs.rs::open_read` 分流 + `first_read_admit`（B2 流臂）**：加密行先 range 能力/密码/句柄，chunks 可得时按**分流判据**决定是否头读（见裁决①）；`first_read_admit` = 一次 34B 头读 → magic？→ 是：`AeadV2Window::open`（KDF 恰一次）+ `plaintext_len_with_chunk(ct, 头分块)`（头权威，与 `plaintext_len_from_container(ct, aead_v2)` 默认分块反推交叉核对，不一致 info! 头胜）→ `fix_cipher_columns(aead_v2, 真值)`（不符才写）→ 真值 0 回 hydrate 0 臂 → `new_with_window` 带窗出 Stream（`total_size`=真值 plain_len，K35 承重）；**无 magic** → 行标 aead_v2：B6 Err（不回写——真值不可知）/ 其余标签：`Hydrate`（交 hydrate 臂）。
3. **`vfs.rs::hydrate`（B2 hydrate 臂两处）**：① gcm 分发**先看暂存头 8B**（`staged_has_v2_magic`）→ 有 magic 改走 `hydrate_v2` + `dispatched_scheme=aead_v2`，无 magic 才 v1 decrypt（v1 头 16B 随机盐，magic 检查零成本）；② 解密完成落盘后、`set_cached_flag` 前：本地明文长度 ≠ 行 size 或分发 scheme ≠ 行标签 → `fix_cipher_columns(行id, dispatched_scheme, 本地长度)`；明文臂不参与（远端长=行长度既有契约）。
4. **`materialize.rs`**：新 pub `plaintext_len_with_chunk(ct, chunk) -> Option<i64>`（结构敏感核心，头分块入参；违例 None=调用方不回写）；`plaintext_len_from_container` v2 臂改为委托（防御臂回 ct 契约逐值保持，T2 原样绿）。
5. **`enc_stream.rs`**：`DecryptingTransport::new_with_window`（B4 注释钉「同一读仅提前、KDF 恰一次」）。
6. **B6 文案两落点（裁决②③）**。
7. **`webdav/src/lib.rs` 网关行重读（裁决④）**。

**五红→绿真实输出（红=修复前实跑）**：
- 红1 `stream_arm_repairs_a_wrong_scheme_row_before_serving`：`panicked … wrong-scheme row over a range-capable transport must be repaired into the stream arm`（open_read 回了 Hydrate）；
- 红2 `hydrate_repairs_size_from_local_plaintext_length`：`assertion left == right failed: 行 size 已按本地明文长度回写  left: 2500 right: 5000`；
- 红3 `gcm_labelled_v2_content_redispatches`：`gcm 标签行遇 CKCRYPT2 内容必须改判 v2 成功，而非 v1 解密失败: Crypto(AuthFailed)`；
- 红4 `v2_labelled_non_container_fails_actionably`：`panicked … B6：非容器内容必须响亮失败，绝不返回 Stream/字节`（open_read 回了 Ok(Stream)）；
- 红5（fs_adapter）：`Content-Length = 首读修正后的明文真值  left: 145904 right: 150000`；
- 绿：`encrypted_read` **4 passed; 0 failed**；`fs_adapter` **35 passed; 0 failed**（既有 34 + 新 1）。
- 执行期既有套件立功一次：materialize T2 在 `plaintext_len_with_chunk` 初版（`checked_sub` 防溢出不防负值）下红 `left: -10 right: 40` → 改显式 `body < V2_TAG` 下界守卫 → T2 绿（零漂移套件抓到实现回归）。

**执行期自主裁决（如实入档，B1–B6 逐条不违背）**：
- **① gcm 行头读分流判据（计划红1 × 既有 web_e2e 15d 的互斥解）**：红1 要求「gcm 标签 + chunks + v2 内容 → open_read 流式回写」，既有 `web_e2e::download_encrypted_row_hydrates_through_full_open` 要求「gcm 行（v1 内容 + chunks）零 open_range」——两者除**内容**外形状全同，而「先读内容才能知道内容」本身破坏零 I/O 断言：无条件头读必炸其一（零漂移与红1 二选一的死锁）。解 = **行账目自洽性分流**：gcm 行仅当 `plaintext_len_from_container(ct, gcm) != row.size`（标签/尺寸/容器长三方矛盾=行形状坏了）才付费 34B 头读（红1 播种 `size=plain/2` 走此臂——计划骨架只定「size=错」未定错法）；自洽 gcm 行（v1 生产常态 size=明文、ct=size+44）零内容读按标签走 hydrate，内容歧义由 **B2 hydrate 条款**（magic 先行改判 + 解密失败 B6 文案，红3 钉）接管——B2 两条臂各司其职，红1/红3 与 15d/测试13 全部同时成立。aead_v2 与未知标签行有 chunks 即恒校验（B6「先于任何字节」/ 无法分发则内容定分发）。**代价如实**：混合方案里「size 恰按 gcm 闭式自洽、内容实为 v2」的行在 range 面走 hydrate 改判而非流式自愈（读通与回写不损，仅不流式）——与 B2 hydrate 条款同向。**主会话已批（EB2 审查）**：逐案推演无漏洞——自洽但内容实为 v2 的 gcm 行由红3 机制（hydrate 先行 magic）自愈为 v2 后下次即可流式；15d 断言零改动 = 零漂移纪律保住，维持现状。
- **② 流臂 B6 文案落点 = `UnsupportedEncryptionScheme` 扩展**（计划明示的两选项之一）：扩出「标签与字节不匹配——双假说：密钥/方案配置错，或实为明文/异期方案；run `cydrive sync`」，**逐字保留** `unknown_scheme_hydrate_fails_with_an_actionable_error` 钉住的三要素（方案名 + `gcm` + `aead_v2`）；变体不动（R2）。**EB2 审查回派后注**：K84.2 使流臂对无 magic 内容改回 `Hydrate`（见回派小节 a），该扩展文案自回派起只挂**未知标签分发臂**（hydrate `unknown` 分支）——文案与变体保留，触发面收窄。
- **③ hydrate gcm 解密失败 B6 文案 = 改 `VfsError::Crypto` 模板、变体零动**：`vfs.rs` 916/979 两测试钉死 `matches!(…, Err(VfsError::Crypto(_)))`（错密码/损坏密文两形态）→ 该面上「换变体」即漂移、「逐点改文案」在 `Crypto(#[from] CryptoError)` tuple 变体上无机制 → 唯一保分类路径 = 模板扩展（条件式后缀：「若密码与方案配置皆对，则字节实为明文/异期容器——run `cydrive sync`」；对确凿错密码读来仍是原语义）——「改文案不改分类」的字面兑现。同一后缀顺带覆盖 webdav `if let Ok` 吞掉流臂 B6 后 hydrate 腿的 `BadMagic` 文案（残余：webdav 面 B6 经 hydrate 腿措辞出口；winfsp 不吞错、直出流臂原文案）。
- **④ 网关行重读补进 `webdav/src/lib.rs`**（计划 Files 未列、红5 隐含要求）：`open_read` Stream 臂与 hydrate 臂返回后 `self.row(&rel)?.unwrap_or(row)` 重读一次——`RowMetaData::from_row`（Content-Length/K35 承重）取回写**后**的真值；行并发消失保留原快照。无回写时重读=同值，既有 fs/web 断言零漂移。web 下载面（`streaming_download_response` 用 open_read 返回的 `total_size`）与 winfsp（直用 `total_size`）由构造已传导，未动。
- **红1 断言增补（非骨架项）**：admission 恰一次 `(0,34)` + `sha256/msg_id` coalesce 原值保留——B4 零往返与「定向 UPDATE」的直接钉，骨架「回写（scheme/size 对）+ coalesce 列未动」的可执行化。

**门禁证据**（全绿）：
- `cargo test -p cloudkit-core --test readthrough --test materialize --test rebuild` → **21 / 9 / 11 passed，0 failed**；
- `cargo test -p cloudkit-webdav --test fs_adapter` → **35 passed，0 failed**；
- 另跑敏感面：`vfs_open_read 18 / vfs 18 / vfs_aead_v2 4 / vfs_encrypted_budget 3 / enc_stream 8 / database 14 / web_e2e 32` 全 0 failed（15d 恢复绿）；
- `cargo test --workspace --no-fail-fast -j 4` → **passed=1652, failed=0, ignored=60**（基线 1647 + 本批新 5；ignored 不变、零新增 `#[ignore]`），exit 0；
- `cargo clippy --workspace --all-targets -j 4 -- -D warnings` → Finished 零告警（一处 doc 列表续行缩进当场修）；
- `cargo fmt --all && cargo fmt --all -- --check` → FMT_OK（格式化后全量 workspace 复跑仍 1652/0）；
- `scripts/check_layers` → OK（16 manifests，零 R1 违例）；`scripts/scan_secrets` → OK（零命中）。

### EB2 审查回派（2026-09-23，同 worktree，K84.2 双试方向补齐）

**审查结论**：EB2 主体通过（workspace 1652/0、五门禁绿、fix_cipher_columns/头权威交叉/webdav 行重读都对）；裁决①（gcm 自洽分流）**批准，维持现状**（批准句已补进上方裁决①）；回派一个必补缺口——

**K84.2 缺口（审查发现）**：计划红 4 原文「内容无 magic **且 v1 形失败** → Err」预设先试 v1、成功则自愈；负责人批准的 K84.2「方案猜错由读时回退兜底（同份密文本地双试，零额外下载）→ 回写行」——而 EB2 主体的 `first_read_admit` 对「行标 aead_v2 + 无 magic」**直接 Err**，跳过了试：混合期场景（配置已切 aead_v2、老文件是 v1、索引已丢按配置猜成 v2）卡死在 admission，v1 解密一试即知且可回写自愈；附带 winfsp 直通面（不吞 open_read 错）也随之直出 Err 卡死。

**修法（B2/B6 框架内，裁决不动）**：
- **a) `first_read_admit` 无 magic 臂**：行标 aead_v2 的 `Err(UnsupportedEncryptionScheme)` 改 **`Ok(StreamSource::Hydrate)`**——34B 头本就试不出 v1（v1 tag 在文件尾，全量内容只有 hydrate 拿得到）；Hydrate 信号零字节、admission 不做内容级判决也不回写，「绝不返回字节」语义不破，最终成败由 hydrate 裁决；gcm/未知标签路径不变（本就 Hydrate）。
- **b) hydrate v2 臂补反向双试**（对称于已落的 gcm 臂 magic 先行）：staged **有 magic** → `hydrate_v2`（既有路）；**无 magic → 试 `crypto::decrypt`（v1）** → 成功：按明文落盘收尾 + `dispatched_scheme = gcm` → 既有回写条件（标签 aead_v2 ≠ gcm）触发 `fix_cipher_columns(id, gcm, 本地明文长度)` **自愈**（size = ct−44 真值）→ set_cached_flag 照常；失败 → `VfsError::Crypto`（B6 文案落点：**沿用已扩的 Crypto 模板**——条件式双假说后缀含 `cydrive sync`，分类零动，staged 由外层失败清理路径回收零残留）。
- **c) 测试**：红 4 按计划原文真义改通路级（admission 断 `Ok(Hydrate)` + 恰一次 (0,34) 零全量 open；随后 hydrate 断**最终 Err 含 `cydrive sync`** + 双假说关键词 + 缓存树零残留 + 行不回写 + `is_cached` 不置位）；新增**红 6（K84.2 双试钉）**：行标 aead_v2 + 远端真 v1 容器（`v1::encrypt` 现造）→ `open_read` 回 `Ok(Hydrate)`（钉 winfsp 直通面不卡死）→ hydrate 读通逐字节 + 行回写 `scheme=gcm`、`size=ct−44` + coalesce 列保留。
- **d) 零漂移**：red1（gcm 不一致臂）路径未动；15d/916/979/fs/vfs_open_read 复跑全绿（下）。

**红→绿真实输出**：
- 红 4（改后）：`panicked … 非容器内容的 admission 不做内容级判决: UnsupportedEncryptionScheme { scheme: "aead_v2", path: "/liar.bin" }`（主体实现回 Err——按新断言红）；
- 红 6：`panicked … v1 内容在 aead_v2 标签下必须拿到 Hydrate 信号: UnsupportedEncryptionScheme { scheme: "aead_v2", path: "/old-v1.bin" }`；
- 绿：`encrypted_read` **5 passed; 0 failed**（红1/2/3/5 零漂移 + 红4改/红6 双绿）。

**门禁证据**（回派后全绿）：
- 定向：`readthrough 21 / materialize 9 / rebuild 11 / fs_adapter 35 / web_e2e 32` + 敏感面（`vfs_open_read 18 / vfs 23 / vfs_aead_v2 4 / vfs_encrypted_budget 3 / enc_stream 8 / database 14`）全 0 failed；
- `cargo test --workspace --no-fail-fast -j 4` → **passed=1653, failed=0, ignored=60**（1652 + 红6），exit 0；
- `cargo clippy --workspace --all-targets -j 4 -- -D warnings` → Finished 零告警；
- `cargo fmt --all && cargo fmt --all -- --check` → FMT_OK；
- `scripts/check_layers` → OK（16 manifests，零 R1）；`scripts/scan_secrets` → OK（零命中）。
