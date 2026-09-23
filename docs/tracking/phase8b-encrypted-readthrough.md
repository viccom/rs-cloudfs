# Phase 8-B：加密卷 read-through 放开 任务跟踪单

> 计划：`docs/plans/2026-09-23-encrypted-readthrough.md` ｜ 批准链：负责人 2026-09-23 三点指示 → K84 立项（D10 修订留痕）→ 深度分析五环 + 计划 §0 八项代码查证 → 计划落档
> 基线：`feat/readthrough-index`@77a58cc（Phase 8 RT0–RT5+审查批+文档批已落，workspace 1643/0/60 五门禁绿，**待合入**）
> 状态：**Phase 8-B 全批次完成（EB1–EB4）+ sftp 加密真机腿（2026-09-23，负责人提供 172.27.199.30），待合入**（EB1 2026-09-23 五红→绿+七门禁绿；EB2 五红→绿+六门禁绿+审查回派二红→绿；EB3 四红→绿+六门禁绿；EB4 两阶段验收三腿+Contract-6 修复+真网重跑+web 同步+K85 收口+五门禁与七组合终跑全绿；**K85.6 修复批 + sftp 加密真机两腿全绿（加密模式首落 sftp），无产品缺陷**——详见批次日志 EB4/K85.6 与「sftp 加密真机腿」节）
> worktree：`feat/readthrough-index`（与 Phase 8 同一支，连续批次）；独立 target
> 编号：执行记录入 decisions 用 **K85**；批次 **EB1–EB4**

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| EB0 | 计划期八项代码查证（随计划完成） | ✅ 2026-09-23 | 计划 §0 八条：upsert 冲突集覆盖 cipher 列 / AeadV2::new() 默认分块无配置缝 / RowMetaData.len 承重 / K47 流式分流 / chunks 仅取消息 id / sha256 读面零引用 / 闸两点位+help 文案 / plan A pending 出口 | 计划 §0（本批日志） |
| EB1 | cipher 真相物化（B1+B3+B4：保留语义 upsert + 闭式反推 + 去 H1 退化臂） | ✅ 2026-09-23 | 五红→绿全留证（用例 12 断言红 / T1 编译红 / T2 防御臂断言红 / T4 降级断言红 / T3 尺寸断言红）；`upsert_materialized` 保留集 + `CipherCtx` + `plaintext_len_from_container` 闭式 + readthrough 双签名换 cipher + Vfs 两薄壳同源 ctx + rebuild `_with_ctx` 缝；七门禁绿（workspace **1647/0**、clippy/fmt/layers/secrets 零告警）；既有断言零漂移（rebuild 11、readthrough 其余 20、materialize 既有 5） | 本批日志 EB1 |
| EB2 | 首读内容校验与回写（B2+B6：容器头/本地长度权威 + 定向 UPDATE + 双假说文案） | ✅ 2026-09-23 | 五红→绿全留证（流臂错行红 panic / hydrate size 红 2500≠5000 / gcm 改判红 `Crypto(AuthFailed)` / B6 红回 Stream / 面级红 145904≠150000）+ **审查回派 K84.2 双试二红→绿**（红4 改通路级红 admission 回 Err / 红6 新增红同 Err）；`fix_cipher_columns` 定向三列 + `first_read_admit`（34B 头读→magic/闭式交叉→回写→带窗构造，B4 零额外往返；无 magic → Hydrate 转 K84.2 双试）+ hydrate **双向**改判（gcm 臂遇 magic→v2 / v2 臂无 magic→试 v1 自愈回写 gcm）+ 解密后本地长度回写 + B6 双文案（挂 `Crypto` 模板与 `UnsupportedEncryptionScheme` 扩展，变体零动）+ 网关行重读传导；六门禁绿（workspace **1653/0/60**、clippy/fmt/layers/secrets 零告警）；既有断言零漂移（含 web_e2e 15d gcm 零窗口、vfs 916/979 `Crypto(_)`、fs 35、vfs_open_read 18） | 本批日志 EB2 + 审查回派 |
| EB3 | rebuild 闸放开（B5：ensure_plaintext_instance 删 + 测试翻转 + help 文案） | ✅ 2026-09-23 | 四红→绿全留证（红1 生产缝加密用例被闸拒 panic / 红2 R6 加密臂 K11 文本 panic / 红3 集成腿经生产缝被闸拒 panic / 红4 core 旧测试编译红 E0432 unresolved `ensure_plaintext_instance`——闸没了它失败）；两闸删（cli :4010 活受理 + :4919 离线缝）+ `ensure_plaintext_instance` 函数与 `RebuildError::EncryptedInstance` 变体全删（唯一额外引用 = readthrough match 臂，同批收敛 Serde 单臂）+ 生产 cipher 接线（共享执行缝恒 `Some(CipherCtx::from_cfg(&vfs_config(cfg)))`，活/离线/多卷三路同缝）+ K11 模块文档/vfs 注释/模块预告句残留清零 + help 文案改写实跑；六门禁绿（workspace **1654/0/60**=基线+集成腿、clippy/fmt/layers/secrets 零告警、help 实跑）；既有断言零漂移（core rebuild 其余 10、telegram 用例、CONFIGS encrypted 标记全不动） | 本批日志 EB3 |
| EB4 | 两阶段验收（离线 CI 三腿）+ 真网加密腿重跑 + 文档收口（K85）+ 五门禁终跑 | ✅ 2026-09-23 | 离线三腿（新 `cloudkit-cli/tests/encrypted_readthrough_e2e.rs`，非 ignored）：腿1 两阶段协议**揭真缺陷红**（`row /empty.bin materialized: 2 row(s) present`——Contract 6 让加密空件不落远端）→ 修复后绿（物化三边界 size 精确 + hydrate 逐字节 + Range 三窗口流式逐字节）；腿2 混合方案首读自愈**首跑即绿**（4994 错猜 → admission 不拒 → hydrate 双试 → 回写 gcm/5000 → 读通）；腿3 rebuild 同构对账**首跑即绿**（上传行↔重建行 cipher 字段逐项相等 + 读通）；红→绿修复 = **Contract-6 加密空件例外**（`upload_queue::process_job` 闸：加密+密码+`authoritative_index` 放行容器上传，stale 守卫先行；正反两臂新单测——正臂先红 `left:0 right:1` → 绿、反臂闸精度回归哨；upload_queue 全套 28/28）。**真网加密腿**（K84.4，有界纪律）：pan123 **2/2 绿**（aead_v2 全栈 + WebDAV 明/密双轮，收尾核空）；pan115 腿1 绿（vfs aead_v2 全栈）、腿2 `Unauthorized` → 一轮 probe-refresh `40140120 refresh_token 无效` → **挂负责人真机窗口**。**web 前端同步**（EB3 报告项⑥并入，主会话裁可）：四处收窄仅 telegram + pin 测试 `rebuild_controls_gate_on_telegram_only_after_phase8b` 五断言先红（`volumes_page.rs:639`）→ 改后绿；cloudkit-web 全套 90/0。**文档收口四件套**：decisions **K85**（K85.1–K85.5，含 plan A 关闭=拒绝 + `remote_handle_for` 注释改关闭声明）/ README 机制段与状态表 / AGENTS Phase 8-B 块与计数 / 本单终态。**五门禁+七组合终跑**：workspace **1660/0/60**（186 suites，+6 = 三腿 3 + 单测 2 + pin 1）、clippy 全目标零告警、FMT_OK、check_layers OK、scan_secrets 零命中、七组合裁剪 clippy 全 Finished 零告警 | 本批日志 EB4 |

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

## 合入前深度审查批（K78 形态，2026-09-23，K86）

- 三路：主会话精读核心正确性链（materialize/database/vfs/enc_stream/readthrough/rebuild/upload_queue/baidu 生产 diff 逐行）+ A 测试面子代理 + B 集成装配驱动面子代理（均只读 + 现场实跑）。总判 **0 High / 4 Medium / 6 Low / 3 Info**——逐条销账表 = `phase8b-review-findings.md`；decisions K86。
- **修复批（TDD，三子代理并行，文件面不重叠）**：M1 SQL CASE 直接钉测（变异杀实证）/ M2 非默认分块头权威差分 + 流式自愈腿 / M4 webdav 0 字节全链 stub 腿 / L1 baidu `finalize_tail` 补 `drained == 0` 守卫（K85.7 隐性回归，white-box 红→绿 + wire 契约钉）/ L2+L3 超短与结构违例边界钉测 / L4 桩常量解耦（独立字面量 `EMPTY_STRING_MD5`）。
- **挂账（并入下方总表）**：M3 pan115 0 字节真机探针 / L5 并发双首读 / L6 web 文本 pin 残余 / I1 pan123 0 字节真机 / **L7 webdav write_path 两条既有 flake（非本批引入，stash 基线复现）**。

## 风险与未覆盖 —— 终态挂账总表（EB4 收口，随批更新到此为止）

| # | 挂账 | 状态/出路 |
|---|---|---|
| 1 | **B6 v1 无 magic 残余歧义**（给存量明文卷新加密码 + 索引丢 + 配置=gcm → 明文被猜成 v1 密文） | 双假说文案（`Crypto` 模板条件后缀 + `UnsupportedEncryptionScheme` 扩展，含 `cydrive sync` 关键词）+ sync 行级真相出路；远期 = 远端 manifest（Phase 8 挂账延续）；绝不静默吐密文 |
| 2 | **无 chunks 形态不做首读校验**（row-id 回退形态无 ct 分母——生产加密行不产生） | EB2 如实挂账：该形态逐字今日行为，非容器在首窗仍 `Unavailable` 响亮失败，但 Content-Length 修正不在覆盖内 |
| 3 | **首次 PROPFIND 初值窗口**（「配置初值猜错 + 未首读」时显示初值尺寸） | 接受残余（EB2 注明）：一次性、首读后自愈（B2 回写先于字节出门） |
| 4 | **真网加密腿结果**（K84.4 验收项，有界纪律处置） | pan123 **2/2 绿**（aead_v2 全栈 + WebDAV 明/密双轮，收尾核空）；pan115 腿1 绿、**腿2 挂负责人真机窗口**（盘上 refresh 被腿1 内存刷轮换且 e2e `token_store=None` 不回写 → probe-refresh `40140120` 一轮失败 → 恢复需真人扫码重发 token；离线三腿已覆盖机制本体） |
| 5 | **web_e2e 15d 与 gcm 分流裁决指针** | EB2 裁决①（gcm 行账目自洽性分流——不自洽才付 34B 头读）**维持现状**，主会话已批；详见 EB2 批次日志裁决① |
| 6 | ~~**Contract-6 明文空件同款观察（EB4 新发现，未修待裁决）**~~ **已修（K85.6，2026-09-23，负责人裁决「事2 修」）** | 明文 0 字节行在 authoritative 后端同样不落远端、索引丢失后文件名消失——原为 EB4 挂账；**负责人 2026-09-23 裁决为修**（EB4 遗留观察处置），与加密侧同批收口：0 字节闸收窄为「**影子索引后端才跳过**」，权威后端明文/加密都落真实对象（明文 = 真 0 字节对象 + `zero_byte_plain_job` 单片计划；telegram parity 与 MiniRedir stale 守卫一字不动）。三红→绿：单测正臂（`calls.len() left:0 right:1`）+ 影子臂回归哨 + e2e 可见性腿（远端 0 字节对象 + wipe db 后 `read_dir_fresh` 可见）；契约同步 = 原 `zero_byte_job_skips_transport` 拆正/影子两臂、EB4 加密正反臂注释标注新闸形态。裁决全文见 decisions **K85.6**；批次日志见下「K85.6 修复批」 |
| 7 | Phase 8 主链（RT0–RT5+审查批）与 Phase 8-B（EB1–EB4）同分支叠加，均**待合入** | 合入指令到时一并处理；worktree `feat/readthrough-index` 独立 target |
| 8 | **K86 审查批挂账**：pan115 `upload_init(file_size=0)` 服务端接受度**真机探针**（K85.6 后权威后端 0 字节真实起链；百度 errno=2 已证此类特形真实可能）+ pan123 0 字节真机同批 | 挂负责人真机窗口（与上表 #4 pan115 token 重授权同窗口）；离线面：六宽面 0 字节路径逐一追踪无一产生破损会话，local/sftp/baidu(+K85.7)/webdav(γ 腿) 已有钉测 |
| 9 | **K86 审查批挂账（小）**：同文件并发双首读幂等无测试（L5）/ web 门控钉测为源码文本 pin（L6，无 JS harness 前提下最强钉法） | 均低风险知情残余；JS harness 建立后 L6 升级为行为 pin |
| 10 | **K86 审查批新发现：ck-webdav `write_path` 两条既有 flake（非本批引入——γ 批 stash 基线复现）**：`put_insufficient_storage_maps_to_io_with_the_code`（507 重传腿 size 复核时序敏感，2/38 轮）/ `stager_lost_ack_on_move_resumes_as_committed`（lost-ACK 恢复腿偶发传输错误，1/20 轮） | 挂专门批查根因（或与并行构建负载相关）；新 0 字节钉测在全部约 38 轮含失败轮 100% 通过 |

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

### EB3（2026-09-23，worktree feat/readthrough-index）

**TDD 四组红→绿（真实输出，按执行序）**：
- 红1（cli 生产缝加密用例——闸先红）：`encrypted_instance_rebuilds_with_cipher_truth_rows` → `panicked … encrypted instance rebuilds through the production seam (B5): rebuild refuses encrypted instances: the backend only sees ciphertext containers … use `cydrive sync` instead …`；接线后绿 = cli rebuild **4 passed**（含翻转用例 + 集成腿）。
- 红2（R6 控制通道加密臂——闸先红）：`rebuild_scope_gates_refuse_synchronously` → `panicked … the encrypted volume is accepted (B5): ERR: rebuild refuses encrypted instances …`；绿 = **1 passed**。
- 红3（集成腿——真 local 卷经生产缝，闸先红）：`encrypted_local_volume_rebuilds_and_reads_through` → `panicked … encrypted local volume rebuilds through the production seam (B5): rebuild refuses encrypted instances …`；绿 = 4/4 一并转绿（空件 ct=50→size 0、目录行明文、K47 流式/hydrate 双臂读通均过）。
- 红4（core 旧测试——闸没了它失败，编译红）：删 `ensure_plaintext_instance` 后 `cargo test -p cloudkit-core --test rebuild` → `error[E0432]: unresolved import cloudkit_core::rebuild::ensure_plaintext_instance … no 'ensure_plaintext_instance' in 'rebuild'`；翻转为 `encrypted_instances_rebuild_with_cipher_truth`（加密实例 → `_with_ctx` 成功 + is_encrypted/scheme/闭式 size/chunk=容器长；明文实例三参入口逐字不变 = raw 后端长无旗）→ rebuild **11 passed**（翻转 1 + 其余 10 零漂移）。

**落地件**：
1. **cli lib.rs**：`:4010` 活受理闸删（R6 注释同步为 telegram 单闸）+ `:4919` 离线缝闸删 + **生产 cipher 接线** = `Some(CipherCtx::from_cfg(&vfs_config(cfg)))` 落 `run_rebuild_with_driver_and_limits`；`rebuild_volume` / `run_rebuild_with_driver` doc 同步。
2. **core rebuild.rs**：`ensure_plaintext_instance` 函数 + `RebuildError::EncryptedInstance` 变体**全删**（非挂注释——见裁决②）+ K11 模块文档加密拒收段改写为「加密卷同走 materialize 真相语义（Phase 8-B / B5）」+ `_with` / `_with_ctx` doc 更新；`walk_one_dir` 的 cipher 缝 EB1 已接（本批把生产 cfg 接上即通）。
3. **main.rs help**：`Encrypted instances are refused — use cydrive sync for those.` → `Encrypted volumes rebuild the same way: rows carry per-file cipher truth and first-read validation repairs any wrong guess.`（`cargo run -q -p cloudkit-cli -- rebuild --help` 实跑输出已核）。
4. **残留清零（代码面）**：core tests/readthrough.rs 模块预告句、runtime_rebuild R6 doc、rebuild.rs `_with_ctx` doc 中的函数名引用全部收敛；终扫 `rg "ensure_plaintext"` 在 `crates/` 下**零命中**。

**执行期自主裁决（如实入档）**：
- **① cipher 构造点收敛为单一共享执行缝**：任务书列两调用点，但活实例 `:4010` 是纯受理闸（不执行 walk）——活路径 walk 经 RuntimeRebuild 缺省闭包（`run_rebuild_command_with_limits`）汇入离线同一缝。故 cipher 只在 `run_rebuild_with_driver_and_limits` 构造一次即覆盖三路（离线 `cydrive rebuild` / 活 `REBUILD` 后台 / 多卷 `run_rebuild_multi`），形态 `Some(CipherCtx::from_cfg(&vfs_config(cfg)))`：判据与 Vfs 薄壳**同源**（`vfs_config` 的 enable 门控 + `from_cfg` 读密码在/方案），**恒传 Some**——明文实例 `ctx.enabled=false` 走 B1 保留语义（与现查逐字一致，T4 不变量对 rebuild 同样成立），不走「明文 None / 加密 Some」的分裂形态。
- **② `RebuildError::EncryptedInstance` 全删而非挂注释**：编译现实 = 唯一额外引用是 readthrough.rs 的 match 臂（本就在计划 Files 内，同步收敛为 `Serde` 单臂）+ cli 闸旁注释（随闸删除），无其他消费者 → 全删成立。**`VfsError::EncryptedInstance` 是另一枚举**（防御臂、webdav:1018/winfsp:39 match 消费）——保留变体，仅清除其 doc 中的 `ensure_plaintext_instance` 引用。
- **③ R6 测试重构（断言原文保留 + 结构确定化）**：注入 `rebuild_probe` parking seam + `RebuildTuning::fast()`——原 default executor 下 REBUILD enc 接受即真跑 walk，受理面断言被后台不确定性污染；seam 化后 OK/标记/STOP 中止全确定。臂序调整为 telegram/ghost/malformed 先行 →「refusals accepted nothing」断言保持字面真 → 再接受 enc（OK + 标记 + `until(calls==1)` 防受理/执行竞态）→ STOP 后关停拒 + `until(cancelled)` 见证 R5 中止。telegram/CONFIGS encrypted/usage/停机断言**原文保留**。
- **④ 集成腿落 `cloudkit-cli/tests/rebuild.rs`（非 core）**：R1 禁 core 依赖 driver——真 local 卷（TempDir 后端 + 现造 v2 容器 + LocalTransport 读通）只能在 composition root；`#[cfg(feature = "local")]` 门控（dispatch.rs 先例）。经**生产缝** `run_rebuild_with_driver` 进入（内部即 `rebuild_from_backend_with_ctx` + 生产 cipher 构造），红→绿真实覆盖 EB3 接线本体（直调 `_with_ctx` 则 EB1 已绿、无红）。
- **⑤ help 文案不含内部批次编号（B1/B2）**：匹配既有 clap help 纯用户向风格（既有 help 无内部编号），语义句保留「per-file cipher truth + first-read validation repairs any wrong guess」。
- **⑥ 残留扫描分类**：`crates/` 代码面清零；docs 命中（plans 2026-09-22/23、decisions K84.4、tracking web-volume-management P2 行、phase8-readthrough 过渡态注记）= **历史档案不改写**。**报告项（本批未改）**：web 前端同类文案/门控——i18n `volumes.note.refresh_encrypted`（双语）+ volumes.js `v.encrypted` 静默注 + app.js `row.encrypted` 按钮禁用 + `index.rebuild_unsupported` "(telegram/encrypted)"——指定扫描模式（大小写敏感）未命中、计划 Task 3 Files 未列；语义上 B5 后后端已收、web UI 仍隐藏按钮且「refuses」文案失真，**建议 EB4 文档收口或单独小批同步**（详见回传风险节）。

**门禁证据**（全绿）：
- core 定向：`cargo test -p cloudkit-core --test rebuild --test encrypted_read --test readthrough --test materialize` → **11 / 5 / 21 / 9 passed，0 failed**；
- `cargo test -p cloudkit-cli` → 全套 0 failed（首跑命中已知 **E0786 mmap / os error 1455 页面文件太小** 陷阱 → 按 AGENTS 纪律 `cargo clean -p cloudkit-cli` 定向清（8811 文件 / 16.5GiB）+ `-j 2` 重跑转绿）；
- `cargo test --workspace --no-fail-fast -j 4` → **TOTAL passed: 1654, failed: 0, ignored: 60**（185 suites；基线 1653 + 集成腿 1，零新增 ignored），exit 0；
- `cargo clippy --workspace --all-targets -j 4 -- -D warnings` → Finished 零告警；
- `cargo fmt --all && cargo fmt --all -- --check` → FMT_OK（两测试文件被格式化，复验通过）；
- `scripts/check_layers` → OK（16 manifests，零 R1）；`scripts/scan_secrets` → OK（零命中）；
- help 实跑：`cargo run -q -p cloudkit-cli -- rebuild --help` → 输出新句「Encrypted volumes rebuild the same way: rows carry per-file cipher truth and first-read validation repairs any wrong guess」（无 "refused" 残句）。

**风险与未覆盖（EB3 增量，随批更新）**：
- **web 前端门控/文案未同步**（报告项⑥）：i18n `volumes.note.refresh_encrypted` 双语 + volumes.js `v.encrypted` 静默注 + app.js `row.encrypted` 按钮禁用 + `index.rebuild_unsupported` "(telegram/encrypted)"——B5 后后端已收、web UI 仍隐藏按钮且文案失真；计划 Files 外、指定扫描模式未命中，本批未改。**→ EB4 已收口**（四处收窄仅 telegram + pin 五断言红→绿，见 EB4 批次日志 C 节）。
- README「加密卷永不回源物化」机制段 = EB4 文档收口范围（本批按计划分工未动）。**→ EB4 已收口**（终态语义四件套之②）。
- 真网 pan115/pan123 加密 e2e 重跑仍挂负责人真机窗口（EB4 验收项）。**→ EB4 已处置**（pan123 2/2 绿；pan115 腿1 绿、腿2 refresh 失效挂账——终态总表第 4 行）。

### EB4（2026-09-23，收口批：两阶段验收 + 真网重跑 + web 同步 + 文档 + 终跑）

**A. 离线两阶段验收三腿**（新 `crates/cloudkit-cli/tests/encrypted_readthrough_e2e.rs`，非 ignored 进常规 CI；local 驱动 + TempDir 后端 + 实例密码，真 crypto 全链零网络；内容 64 位 LCG 按轮钟表种子——K72/K77.6 纪律）：

**红→绿真实输出（按执行序）**：
- 首跑**编译零红**（一次通过——harness 照 pan115_e2e/rebuild.rs 形态，API 面全部命中）；**腿2、腿3 首跑即绿**（协议面的红证据在前批：EB1 五红 / EB2 五红+回派二红 / EB3 四红——本腿只承担 e2e 级协议验收，不伪造红）；
- **腿1 红（真缺陷）**：`panicked … row /empty.bin materialized: 2 row(s) present`——阶段① 三上传 drain 成功（`[leg1] stage 1: three encrypted uploads drained`），阶段② 全新 db 物化只见 2 行（exact + docs），**空件不在远端**。根因 = Contract 6「0-byte uploads never touch the remote」对加密空件同样生效：0 明文字节的 v2 容器（50B）是真实载荷，跳过后端永久缺文件——索引丢失后名字从 read-through/rebuild 视野消失（负责人两阶段协议的空件边界直接揭穿）；
- **单测红（同缺陷闸正臂）**：新 `encrypted_zero_byte_row_still_uploads_its_container` → `assertion left == right failed … left: 0 right: 1`（`stream_upload_calls()` 零调用）；反臂 `encrypted_zero_byte_keeps_the_skip_on_a_shadow_index_backend` **先即绿**（默认 mock `authoritative_index=false` 保持跳过——闸的精度基线）；
- **修复（生产代码，超出计划 Task 4 Files——按「修 bug 先写重现测试再修」TDD 授权与验收硬要求执行，如实入档 K85.5）**：`upload_queue::process_job` 的 Contract-6 臂重构——stale 空 PUT 守卫**先行**（加密/明文共用，superseded 语义不动）→ 闸 `row.is_encrypted && cfg.encryption_password.is_some() && transport.capabilities().authoritative_index` 才放行穿透到既有 v1 staging / v2 流式路径（cipher_job 重算分块计划，enqueued 零计划永不达 transport）；telegram（影子索引）与明文行逐字保持原契约（Python parity / MiniRedir 语义）；`vfs.rs commit_put` 零计划注释同步；
- **修后绿**：e2e **3 passed; 0 failed**（腿1 含物化三边界 size 精确断言 `empty=0 / exact=1048576 / cross=1060921` + hydrate 逐字节 + 整窗 + Range 三窗口（头/跨 1MiB 容器块边界/尾）流式逐字节）；`upload_queue` **28 passed; 0 failed**（新 2 + 既有 26 含 `zero_byte_job_skips_transport`/stale 双守卫零漂移）；敏感面中检 `cloudkit-core + cloudkit-webdav + cloudkit-web` 全 0 failed（Explorer 空 PUT 占位守卫、webdav smoke 合同 6 腿零漂移）；
- 执行期**测试自身一处修**（非产品缺陷）：腿1 中窗越界 `range end index 1114112 out of range for slice of length 1060921`——cross 文件仅 1MiB+12345，128KiB 中窗放不进跨界后余量 → 改 `64KiB + CROSS_TAIL` 止于 EOF（跨界断言保留）。
- **腿3 与 EB3 集成腿的去重说明**（计划允许如实去重/协议面差异保留）：EB3 `cli/tests/rebuild.rs::encrypted_local_volume_rebuilds_and_reads_through` = 手搓容器种子 + 生产缝 `run_rebuild_with_driver`；本腿协议面差异三点保留——①后端内容来自**真实加密上传路径**（非手搓 fs::write）②wipe 前**捕获上传行 truth、wipe 后对账**（上传行 ≡ 重建行 cipher 字段逐项同构——rebuild 与现查同轨的 e2e 级钉）③直调 `rebuild_from_backend_with_ctx`（生产 `CipherCtx::from_cfg(vfs_config(cfg))` 形态）+ 嵌套目录全树 walk。

**B. 真网加密腿重跑**（`#[ignore]` + `--test-threads=1 --nocapture`；凭据只经 env、从 `E:\GitHub\rs-CyDrive\test\` 读入 shell 变量——值零落 argv/日志/回传）：
- **pan123 2/2 绿**：`pan123_vfs_aead_v2_full_stack_roundtrip`（行 uploaded/encrypted/aead_v2/size=1572864 + hydrate 1572864B 逐字节 + 跨块窗口 + 远端密文核验 1572930B=+66B 容器开销）+ `pan123_webdav_roundtrip_plain_and_encrypted`（明轮 GET 262144B 逐字 → 密轮 GET 262144B 逐字 → 双删核空 `remote cleaned`），28.5s，K79.6 测试根 `CYDRIVE_PAN123_TEST_ROOT=64409220`（`e2e_pan123_root_3800`）必填在案；
- **pan115 腿1 绿**：`pan115_vfs_aead_v2_full_stack_roundtrip` 同五断言全过 + 远端核空；
- **pan115 腿2 挂账（有界纪律处置，如实不谎报）**：`pan115_webdav_roundtrip_plain_and_encrypted` 首连 `Unauthorized { recoverable: true }`——环境事实：盘上 access 七天前取得（7200s 窗早已过），**腿1 的驱动 connect 已在内存中完成 refresh 并把盘上 refresh_token 轮换失效**，而 e2e `token_store=None` 不回写；按纪律执行**一轮** `pan115-spike probe-refresh`（探测+刷新工具，K74.1 形态）→ `refreshToken: rejected http=200 state=0 code=40140120 errno=40140120 message=refresh_token 无效` → 恢复需真人扫码重发（超有界纪律，不死磕）→ **挂账「pan115 webdav 加密轮待负责人真机窗口（先 probe-refresh 轮换落盘再跑，K74.1 操作序）」**。操作教训入档：真网多腿连跑前应**先** probe-refresh 轮换落盘再跑（e2e 内存刷不回写——K74.1「先轮换落盘」的次序价值本批反向实证）。

**C. web 前端同步（EB3 报告项⑥并入，主会话裁可）**：
- **先查测试**：`cloudkit-web` 三测试文件无任何钉 JS 门控/文案的断言（`configs_endpoint_parses_the_sparse_rebuild_markers` 只钉后端 `encrypted` 稀疏标记 JSON——后端标记保留不动）→ 无既有断言需同步；本批**新增** Rust 级内容 pin `rebuild_controls_gate_on_telegram_only_after_phase8b`（i18n 双语 narrowing + 死键删除 + volumes.js/app.js 门控收窄共五断言）；
- **pin 红**：`panicked at crates\cloudkit-web\tests\volumes_page.rs:639`（首断言——i18n 仍含 `(telegram/encrypted)`）→ **四处编辑** → 绿；
- **前后对照**：

| 位置 | 改前 | 改后 |
|---|---|---|
| `i18n.js:59` EN `index.rebuild_unsupported` | `…no backend to walk (telegram/encrypted)…` | `…no backend to walk (telegram)…` |
| `i18n.js:330` ZH 同键 | `…（telegram/加密卷）…` | `…（telegram）…` |
| `i18n.js:142/413` `volumes.note.refresh_encrypted` 双语 | 「Refresh refuses encrypted instances…」/「加密实例拒绝刷新…」 | **键删除**（两语言，B5 后失真且无消费者） |
| `volumes.js refreshControl` | `if (v.encrypted)` → 静默 note + 失真 tooltip；函数注释述 K11 加密拒 | **分支删除**，注释改写 B5 终态（加密卷同式可刷；telegram 唯一静默注） |
| `app.js:271` | `row.backend === 'telegram' \|\| row.encrypted` 禁用 Rebuild | `row.backend === 'telegram'`（仅 telegram 禁用） |

- **clippy 当场修 2**：pin 测试 `redundant_locals`（closure 内 `let addr = addr` → 直捕）+ e2e 头注释 `doc_lazy_continuation`（行首 `+` 被解析为列表标记 → 改顿号连接）；
- **测试情况**：pin 红→绿（五断言）；`cargo test -p cloudkit-web` 全套 **90 passed / 0 failed**（含 web_e2e 32、multivolume 16、volumes_page 41——15d gcm 既有断言零漂移）。

**D. 文档收口四件套**：① `docs/decisions.md` 末尾 **K85**（K85.1 四批总述+终态数字 / K85.2 两处计划自修含缺口发现过程 / K85.3 plan A 关闭=拒绝+`remote_handle_for` 注释改关闭声明 / K85.4 B5 同批+web 反分裂 / K85.5 残余与挂账如实录）；② `README.md` 四处（Phase 8-B 状态行新增+Phase 8 行尾标注、rebuild 段去「加密实例走 sync」、机制段换终态语义「加密卷与明文卷同构 read-through」、/volumes Actions 列仅 telegram）；③ `AGENTS.md` 当前阶段顶部 Phase 8-B 完成块 + 计数行 1643→**1660**；④ 本单状态行/EB4 行/EB4 批次日志/风险节改**终态挂账总表**（七行）+ EB3 增量三项标注收口。

**E. 五门禁 + 七组合终跑**（全部编辑完成后终态复跑，真实输出）：
- `cargo test --workspace --no-fail-fast -j 4` → **TOTAL passed: 1660, failed: 0, ignored: 60, suites: 186**，exit 0（基线 1654 + 新 6；ignored 60 不变——零新增 `#[ignore]`）；
- `cargo clippy --workspace --all-targets -j 4 -- -D warnings` → `Finished dev profile … in 11.78s` 零告警；
- `cargo fmt --all -- --check` → **FMT_OK**；
- `scripts/check_layers` → `OK - 16 manifests checked (7 driver crate(s)), no R1 violations`；
- `scripts/scan_secrets` → `OK - no pattern matches [full tree]`；
- 七组合裁剪 clippy（全量之后跑，双指纹纪律）→ telegram/baidu/local/sftp/pan115/pan123/webdav **七行全部 `Finished dev profile` 零告警**。

**提交**：`test(phase8b): 两阶段验收三腿 + Contract-6 加密空件例外 + 真网加密腿重跑 + web 门控收窄（EB4）` 与 `docs(phase8b): K85 收口——README/AGENTS/跟踪单终态（EB4）`。

### K85.6 修复批（2026-09-23，实现子代理，worktree feat/readthrough-index）

**授权**：负责人 2026-09-23 明示「事2 修」——EB4/K85.5 挂账的「明文 0 字节在权威后端不落远端」由观察转为修复。契约修订范围 = `upload_queue::process_job` 的 0 字节闸（见 decisions K85.6）。

**先查后做（宽面驱动 0 字节 writer/stager 语义核对，任务要求的「务必核对」项）**：六宽面驱动的 writer 面对 0 字节**均产生合法远端对象**，逐驱动证据：
- **ck-local**：`store_bytes` → `LocalStager::write(empty)+close`（`tokio::fs::write_all(&[])` 合法 + `metadata().len()==0` 匹配 `hinted_size=0`）——既有绿测 `transport_face.rs::upload_empty_file_roundtrip`（0 字节上传 + open 回空）直接钉；
- **ck-sftp**：`SftpStager::close` 的 `hinted == written` 在空件时 0==0 通过，远端 size 复核 0==0 通过（该驱动的「0 字节上传 bug 唯一持久修复」正是这条复核）——无 0 字节拒绝臂；
- **ck-webdav**：`client.put` 空 `Bytes` 合法（`put_timeout(0)=CONTROL_TIMEOUT`）+ stager 的 `hinted != written`（0==0）与 ④stat 复核（`remote_size == written` 0==0）通过；
- **ck-baidu**：`data.chunks(4MiB)` 对空切片得空 block_list（~~合法；残余风险如实录：空 block_list 的 precreate/create 未在本桩面覆盖，真机未实测~~ **→ 已证伪并修复（K85.7，2026-09-23）**：空数组 block_list 被服务端 **errno=2** 恒拒，真形 = `[EMPTY_MD5]` 空串 MD5（主会话活 token 真网三步探针定形），桩面已按真形补钉、驱动已修——见下「baidu 0 字节 wire 真形修复」批次日志与 decisions K85.7；真网复验腿挂主会话挂载实例重传）；
- **ck-pan115**：`size(0) <= part_size` → 单分片 `put_object_path` 对空 spool 发合法空 PutObject；`finish_entry` 的 size 复核 0==0 通过；
- **ck-pan123**：既有绿测 `write_path.rs::empty_file_roundtrips_via_a_single_empty_part`（恰 1 次 empty part PUT + complete + 回读空）直接钉——**唯一有专门空件测试的驱动**。
结论：无需驱动侧改动，放行条件成立。

**落地件**（生产 1 文件 + 测试 3 文件 + 文档 2 文件）：
1. **`upload_queue.rs`**：0 字节闸 `encrypted_container_is_payload = row.is_encrypted && password.is_some() && authoritative_index` → **`authoritative_payload = transport.capabilities().authoritative_index`**（加密不再必需）；新增 `zero_byte_plain_job`（`chunk_count: 1`）——0 字节明文的 `commit_put` 计划是零分片，而各驱动对空对象回报**单片 receipt**，故明文臂把单片有效计划传给 transport（加密臂本就各自在 staging/stream 步重 plan，语义不动）；Contract 6 注释改写为「0 字节不触远端**仅限影子索引后端**；权威后端落真实对象——明文 0 字节对象与加密容器同为可枚举载荷」；stale 空 PUT 守卫（`row.size != 0` → degraded）一字未动。
2. **`cloudkit-core/tests/upload_queue.rs`**：原 `zero_byte_job_skips_transport`（默认 mock = 影子形态却断言跳过）**拆为两臂**——`zero_byte_job_uploads_to_an_authoritative_backend`（正臂：`authoritative_index=true`，钉上传恰一次 + `mock.message(1)` 存在且空 + 行 uploaded/size 0/msg_id=1 + 本地副本删）、`zero_byte_job_still_skips_a_shadow_index_transport`（影子臂：原断言语义逐字保留 + 补 stream 面零调用）；EB4 加密正反臂语义不变，仅注释标注 K85.6 新闸形态。
3. **`cloudkit-cli/tests/zero_byte_authoritative_e2e.rs`**（新，非 ignored）：明文实例 + local 驱动——0 字节上传排空 → **远端核验**（后台目录确有该对象且长 0）→ wipe db → `read_dir_fresh` → **该文件在物化列表中出现**（缺陷的用户可见形态）+ 非空对照同现。

**三红→绿真实输出（红=修复前实跑）**：
- 红 a（`upload_queue.rs::zero_byte_job_uploads_to_an_authoritative_backend`）：`assertion left == right failed: a 0-byte row on an authoritative backend must land a real remote object — left: 0 right: 1`（上传零次）→ 绿；
- 红 c（`zero_byte_authoritative_e2e.rs`）：`panicked … the 0-byte object must exist on an authoritative backend (Contract 6 skip would leave it remote-absent)`（后端目录无该对象）→ 绿；
- 红（实现期中间态，如实录）：放宽闸后首次跑正臂红 `left: 3 right: 1`——mock 的 `finish_upload` 对空文件按 **1 chunk** 计，而 job 的零分片计划不匹配 → `Unavailable` → 3 次重试后降级；这正是「零计划不得到达 transport」的实证，补 `zero_byte_plain_job` 后绿；
- 绿：`upload_queue` **29/29**；`zero_byte_authoritative_e2e` **1/1**；`encrypted_read` 5 / `materialize` 9 / `readthrough` 21 / `rebuild` 11 / `encrypted_readthrough_e2e` 3 全 0 failed（既有断言零漂移）。

**提交**：`fix(upload): 明文 0 字节在权威后端落真实对象——Contract 6 收窄至影子索引后端（K85.6）`。

### sftp 加密真机腿（2026-09-23，负责人提供 172.27.199.30）

**背景**：EB4 的两阶段加密验收落在离线 local/TempDir（`cloudkit-cli/tests/encrypted_readthrough_e2e.rs` 三腿）与真网 pan115/pan123 上；sftp 是**唯一宽面驱动尚无加密真机覆盖**者。负责人 2026-09-23 提供真机 sftp 服务器（`root@172.27.199.30:22`，测试根 `/srv/cydrive-rt-enc`，ED25519 指纹 `SHA256:mpap…IeQ` 已由主会话明文腿钉过），本条把 EB4 腿 1 / 腿 2 的协议语义搬到真 sftp 上——**加密模式首次落到 sftp 驱动**。

**形态**：`crates/drivers/ck-sftp/tests/live_readthrough.rs` 追加两腿（同文件的 `#[ignore]` 真机纪律、env helper、stamp 唯一名、K72/K77.6 按轮随机 LCG 载荷、收尾核空全部复用）：

| 腿 | 内容 | 结果 |
|---|---|---|
| `live_encrypted_readthrough_smoke` | 三阶段：①加密实例（aead_v2 + 按轮唯一密码）经真 sftp + 上传队列写三边界文件（空件 / 恰 1MiB / 跨块 1MiB+12345，嵌套 `docs/sub`）→ drop Vfs 切阶段 → **全新空 db** 逐层 `read_dir_fresh` 现查物化 → 尺寸闭式精确 + `stat_fresh` 深跳 + hydrate/流式逐字节 → 驱动侧递归清理核空 | **绿**（8.1s） |
| `live_encrypted_mixed_scheme_self_heals` | 远端预置真 v1(gcm) 容器（`crypto::encrypt` 现造后经驱动直传，不经 Vfs）→ 实例 cfg=aead_v2 + 全新 db 按配置猜错标签 → 首读 admission 回 `Hydrate`（K84.2）→ hydrate 双试自愈回写 `scheme=gcm`、`size=ct−44` → 读通逐字节 | **绿** |

**真机证据（`--ignored --test-threads=1 --nocapture` 尾部实跑）**：
- 腿 1：`[encl1] remote containers: empty=50 exact=1048626 cross=1060987 (bytes)` → `[encl1] stage 2: materialized from an empty db — empty=0 exact=1048576 cross=1060921` → `[encl1] hydrate: empty + 1MiB byte-exact through real-sftp decrypt` → `[encl1] open_read: full window + two range windows byte-exact (aead_v2 streaming)` → `[SUMMARY] rt5-enc sftp|3 encrypted uploads on real sftp (remote ct 50/1048626 /1060987)|empty db materialized 0/1048576/1060921 byte-exact|hydrate + range stream decrypt byte-exact|workdir recursively removed and verified gone`；
- 腿 2：`[encl2] dual-try self-heal on real sftp: scheme aead_v2→gcm, size 4994→5000, byte-exact` → `[SUMMARY] rt5-enc-scheme sftp|v1 container seeded remotely (5044B)|empty db guessed aead_v2/4994|admission Hydrate|dual-try healed gcm/5000, byte-exact|workdir removed and verified gone`；
- 合跑：`test result: ok. 3 passed; 0 failed; 0 ignored`（明文腿 1 + 加密腿 2，`live_readthrough` 套件 3 腿）。

**尺寸闭式对账（远端容器长 ↔ 行反推明文长，互为交叉验证）**：

| 文件 | 远端 v2 容器 | 行 `size`（闭式反推） | 关系 |
|---|---|---|---|
| 空件 | 50 B | 0 | `34 + 16`（头 + 单空块 tag） |
| 恰 1MiB | 1 048 626 B | 1 048 576 | `34 + 1MiB + 16`（头 + 一满块 + tag） |
| 跨块 | 1 060 987 B | 1 060 921 | `34 + (1MiB+16) + (12345+16)`（头 + 满块 + 短尾块） |

**读通方式**：空件与恰 1MiB 走 **hydrate 全量**（`open_read` 对空件回 `Hydrate`——K47 `size>0` 门；1MiB 在 cache 冷态亦 hydrate）逐字节等于原明文；跨块 1 060 921 B 走 **aead_v2 流式**（真 sftp 申报 `range_read=true`）——整窗 + 头窗 `(0,+64KiB)` + 跨 1MiB 容器块边界窗 `(1MiB−64KiB,+64KiB+12345 止于 EOF)` 三读逐字节，K35 `total_size` = 闭式反推明文长。**两种方式都断言了**（任务给的「或至少 hydrate」为下限，本条双覆盖）。

**混合方案腿取舍**：**未降级**——实现成本低（`cloudkit_core::crypto::encrypt` 现造 5 044 B v1 容器 + `driver.writer` 直传，两处调用照 EB4 腿 2），故按完整形态落地并真机反证（离线 `encrypted_read` 已有 5 测试钉机制，本条额外覆盖真 sftp 的 hydrate 腿与远端 v1 容器）。

**执行期揭出并修复的测试侧问题（非产品缺陷）**：
1. `Vfs::create_dir` 只写本地行、不建远端，且要求父行已在——真机工作目录全新，直建嵌套 `docs/sub` 行报 `ParentMissing`。修法 = 远端目录树先经**驱动 `mkdir`** 落盘，再用**逐层 `read_dir_fresh`** 物化本地行（比手写父链更贴近真实使用，顺带覆盖空索引实例下新目录的可见性）。
2. **失败路径清理的 Drop guard 形态修正（实测钉住）**：初版在 `Drop` 里用 `Handle::try_current()` 判据 + 新 runtime `block_on`——`#[tokio::test]` 的 panic unwind 就发生在 **runtime 线程**上，`try_current()` 恒有值且嵌套 `block_on` 会 panic，guard 实测**完全没生效**（注入 panic 探针后远端残留 3 项）。改为**独立 OS 线程**（`std::thread::spawn`）内建一次性 runtime 跑 `remove_tree`，主线程 `join` 等收尾——注入探针复验：残留从 3 项降到 1 项（仅剩两腿共享的 `sf4rt` 前缀容器目录，与明文腿同形态），**guard 生效**。正常路径显式 cleanup 先行 + `disarm()` → Drop 变 no-op，零额外成本。

**真机揭出的产品缺陷**：**无**——加密 read-through 全链（容器上传/空件 50B 真落远端/空索引物化闭式尺寸/首读容器校验/流式解密/混合方案双试自愈）在 sftp 上首跑即全绿，与 local/pan123 行为一致。这同时**真机复核了 K85.6 的加密空件例外**（`empty=50` 断言直接钉「0 明文字节的 50B v2 容器真在远端」，Contract-6 跳过对加密卷不成立）。

**门禁（本批终跑）**：`cargo test -p ck-sftp` **68 passed / 0 failed**（`live_readthrough` 3 ignored）；`cargo test --workspace --no-fail-fast -j 4` → **passed=1662, failed=0, ignored=62, suites=187**（基线 1660/0/60 + 新 2 `#[ignore]`），exit 0；clippy 全目标零告警；FMT_OK；check_layers OK（16 manifests）；scan_secrets OK（零命中）。

**远端清理核对**：两腿各自显式 `remove_tree` + `stat` 核空（`NotFound` 断言）；跑完 `/srv/cydrive-rt-enc` 仅余两腿共享的 `sf4rt` 前缀容器目录（明文腿同形态，`rmdir` 后 **0 项**——`[find] REMOTE_EMPTY` 实测）。

**风险与未覆盖（本批增量）**：
- 断线重连/跨会话 resume 的加密形态未在本腿覆盖（sftp 驱动无 resume 能力位，会话自愈由 live_matrix 腿⑩与 write_path 桩测试覆盖；加密只是多一层本地容器变换，不改变连接语义）。
- 真机为局域网环回（~ms RTT），**公网高延迟下的 hydrate 300s 预算**未实测（本腿 hydrate_timeout 放宽到 300s；环回使该值宽松，公网需按 E-5 预算另行核）。
- 测试根 `CYDRIVE_SFTP_TEST_ROOT` 必填（K79.6）；本腿未设 `CYDRIVE_SFTP_TEST_PASSWORD`（走 `KEY_PATH` 私钥形态——D1 两形态之一，密码形态由既有 live_matrix 覆盖）。

**提交**：`test(sftp): 加密 read-through 两阶段真机腿（Phase 8-B 验收延伸）`。

### baidu 0 字节 wire 真形修复（2026-09-23，实现子代理，worktree feat/readthrough-index）

**背景**：K85.6 把 0 字节闸收窄到「影子索引才跳过」后，baidu 真网首跑揭出：明文 0 字节行到达 `ck-baidu` 后构造 `block_md5=[]`，precreate 序列化为**空数组** block_list——服务端**恒拒 errno=2**（`Unavailable` 可重试 ×5 耗尽 → 行降级，权威后端 0 字节文件不落盘，恰是 K85.6 要治的「文件名消失」形态）。mock 桩不校验 block_list 形态故桩面全绿——「桩照服务端真形建模」防线缺口的镜像（桩照实现抄的第四例）。

**主会话真网探针定形（活 token，MSYS_NO_PATHCONV curl 三步全通）**：0 字节文件 precreate 的 `block_list=["d41d8cd98f00b204e9800998ecf8427e"]`（**空串 MD5，非空数组**）→ errno=0 + return_type=1 + uploadid；**无 superfile2**（无分片可传）；create **原样重申**同形（31363 约束同款）→ errno=0 + fs_id，真网落盘/list 可见核验通过。

**落地件**（6 文件，TDD 先红后绿）：
1. **红**：`tests/upload_form_bytes.rs` 空件测试改按服务端真形断言（precreate 恰 `[EMPTY_MD5]` + create 同形重申 + 零 superfile2）——红证：驱动实发 `"[]"`（`参数 block_list=["d41d…"] 应恰出现一次：[… ("block_list", "[]")]` left: 0 right: 1）；
2. **桩补钉**：`tests/common/mod.rs` mock precreate 空数组 block_list → **errno=2**（真形建模）；create 对 `[EMPTY_MD5]` 唯一块免分片校验（满块恒 4MiB、尾块恒 1..4MiB−1，空串 MD5 只能出自 0 字节）——第二红证：`transport_face::upload_empty_file_roundtrip` `Unavailable("baidu errno=2: ")`（与真网缺陷形态同款，双路径实证缺陷面）；
3. **绿**：`src/upload.rs` 定 `pub const EMPTY_MD5`（lib.rs 导出供测试引用），0 字节构造点改单元素声明——stager 路径 `finalize_tail` 空缓冲分支 + `close` 无条件调 `finalize_tail`（补「writer 打开即 close」的 hinted 空件腿）+ transport 整文件路径 `upload_whole_file` 空数据分支；上传循环免传跳过三处（`finalize_and_upload_remaining` 查缺补传 / `upload_whole_file` 探活 find / 差集 missing filter）；
4. **文档联动**：`upload.rs` 模块头新增「0 字节 wire 真形」节、`api.rs` precreate 文档改「0 字节 = 恰 `[EMPTY_MD5]` 一元素」、`transport_face.rs` 空件用例注释同步。

**零涉及证明**：加密路径零改动（加密空件容器 44/50B 非零载荷走既有三步曲——sftp 真机腿 `empty=50` 已真机验证该形态）；非 baidu 驱动零涉及（diff 只触 ck-baidu 六文件）；影子索引跳过臂零改动（`upload_queue` 影子臂 `zero_byte_job_still_skips_a_shadow_index_transport` 零漂移绿）。

**红→绿证据（真实输出尾部）**：
- 红 1：`assertion left == right failed: 参数 block_list=["d41d8cd98f00b204e9800998ecf8427e"] 应恰出现一次：[("path", "/apps/cloudfs/empty.bin"), …, ("block_list", "[]")] — left: 0 right: 1`；
- 红 2：`panicked at transport_face.rs:224: upload: Unavailable("baidu errno=2: ")`；
- 绿：`upload_form_bytes` **4 passed**（含新 `empty_upload_declares_empty_string_md5_block_list_without_superfile2`）；ck-baidu 全套 **50 passed / 0 failed / 3 ignored**（conformance 2 + transport_face 8 + upload_resume 8 全零漂移）。

**门禁（本批终跑）**：`upload_queue` 0 字节四臂 **4/4**（正臂/影子臂/加密正反臂）；`zero_byte_authoritative_e2e` **1/1**；`cargo test --workspace --no-fail-fast -j 4` → **passed=1662, failed=0, ignored=62**（187 suites；与本分支基线同数——baidu 桩面 +1 测试系改名改写非新增计数口径差异如实录：原 1 测试改写 + 断言扩 create 腿，总数不变）；clippy 全目标零告警；FMT_OK；check_layers OK（16 manifests）；scan_secrets OK（零命中）。

**风险与未覆盖（本批增量）**：
- **真网复验挂主会话**：活 token 真网三步探针已定形（curl 全通 + 落盘核验），但驱动修复后的**端到端真网重传**（挂载实例 0 字节写入 → baidu 落盘）待主会话实测——本批按任务边界不跑真网。
- errno=2 未入 `map_errno` 映射表（仍走 `_` → `Unavailable` 可重试）：修复后驱动不再发出该形态，映射表不加防御臂（真网若再现 errno=2 即新事实再议）。
- 桩的 errno=2 建模按「空数组/缺字段同拒」从严——真网对缺字段形态的响应未探（驱动从不发缺字段形态，无消费面）。

**提交**：`fix(baidu): 0 字节上传 wire 真形——block_list=[空串MD5] 替代空数组（真网 errno=2 实测，Phase 8-B）`。
