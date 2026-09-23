# Phase 8-B：加密卷 read-through 放开（B 方案）实施计划

> **For Claude:** REQUIRED SUB-SKILL: Use executing-plans to implement this plan task-by-task.
> **批准链**：负责人 2026-09-23 三点指示（文档非宪法 / 配置即真相源模型确认 / 认可 B 并要求深度分析）→ K84 立项（D10 修订留痕）→ 深度分析五环验证 + 本计划 §0 八项代码查证 → 本计划落档报批。
> **状态**：计划已写，待负责人批准后开工（批次 EB1→EB2→EB3→EB4，同 worktree `feat/readthrough-index` + 独立 target）。**计划批准 = 同批批准 §1 的 B1–B6 设计裁决与 rebuild 闸同批放开的范围项（K84.4 决策点，分析推荐同批）。**
> **编号**：执行记录入 decisions.md 用 **K85**；阶段命名 **Phase 8-B**；批次代号 **EB1–EB4**；跟踪单 `docs/tracking/phase8b-encrypted-readthrough.md`。

**Goal:** 加密卷获得与明文卷完全同构的按需索引能力——清空本地索引后逐层现查即重建账本、下载在线流式解密，rebuild 对加密卷同样可用；混合方案与配置错由首读内容校验自愈。

**Architecture:** 物化时按「逐文件真相三层优先」写行（既有行 cipher 真相 > 实例配置初值），尺寸用闭式公式按行上方案反推；首次打开时以**密文容器自述**（v2 头 `CKCRYPT2` magic + 自描述参数；解密后本地明文长度）为权威校验并回写行，先于任何字节出门；rebuild 与 read-through 共用同一物化与真相逻辑，`ensure_plaintext_instance` 闸同批移除。

**Tech Stack:** 零新增依赖（cloudkit-crypto 既有 v1/v2 容器格式、rusqlite 定向 UPDATE、既有 `upsert_file_scheme` 缝）。

---

## 0. 已钉事实（本轮代码查证，file:line 为 worktree `feat/readthrough-index`@77a58cc 基线）

1. **上传行的 cipher 真相写法**：`vfs.rs:500-506` pending 行 `is_encrypted = encryption_password.is_some()` + `upsert_file_scheme(cfg.encryption_scheme)`——B 的物化同源复制该动作即可。
2. **`upsert_file` 冲突集无条件覆盖 `is_encrypted` 与 `size`（database.rs:477,483）**——今日无害（H1 门挡着），B 开门后**盲猜回写会造成「首读修正→下次列举回冲」震荡**：物化必须走**新的保留语义 upsert**，既有行的 cipher 真相（is_encrypted/scheme/修正后 size）永不被列举猜测回退。`encryption_scheme` 不在 `upsert_file` 列集（E-4 列默认 'gcm'），scheme 只经 `upsert_file_scheme`（database.rs:523）落。
3. **v2 容器恒默认分块**：`vfs.rs:261` `cloudkit_crypto::AeadV2::new()` 无参构造 = `DEFAULT_CHUNK_SIZE` 1MiB（v2.rs:85）；`chunk_size_mb=1900` 是**上传队列分段**（telegram 1900MB），与容器分块无关；**本应用无容器分块配置缝** → 给定方案后尺寸反推闭式精确。
4. **容器格式（权威）**：v1 = `[16B 盐][12B nonce][GCM 密文+16B tag]`（冻结 Python 契约，tag 在尾、**无 magic**）；v2 = 34B 头（`CKCRYPT2` magic@0、version@8、salt@10、迭代数@26、分块大小@30）+ 分块流（每块 `chunk_size+16B tag`，结构自定界 `n_chunks-1=(body_len-16)/(chunk_size+16)`，v2.rs:5-41）。**v1 尺寸闭式 = 密文长−44；v2 尺寸闭式 = 头参数代入**；空文件（v1 ct=44 / v2 ct=50）与整倍数分块是格式白纸黑字的边界。
5. **读路径对行字段的真实依赖**：`sha256` 读路径零引用（仅三处写 None）→ 物化 `sha256=None` 无害（coalesce 保留）；chunks 行消费 = `remote_handle_for`（vfs.rs:729-745）取 `telegram_msg_id` 组句柄（**不用 chunk.size 做读数学**；路径形后端 `RemoteHandle.path` 寻址，`msg_id=None` 的 id-less chunk 在 Read 策略会报错——物化 File 臂已写 K6 0 占位，加密同形）；**`row.size` 承重面 = 网关 `RowMetaData.len`（webdav lib.rs:892，Content-Length/PROPFIND 尺寸）+ K47 流式门（`size>0`）+ `DecryptingTransport` 的 `plain_len`（open_read，vfs.rs:843-856）** → 尺寸反推正确性是 HTTP Range/长度契约的承重墙。
6. **加密读的既有分发（B 不改其机制，只保证喂对行）**：`open_read` 密码门（无密码=MissingPassword 可行动错）→ WF0 缓存优先 → K47 分流（`aead_v2 + range_read + size>0` → `DecryptingTransport` 流式；其余 → Hydrate 全量下载解密；gcm 恒 Hydrate——整文件 AEAD 不可切窗）；hydrate 分发按**行上方案**（vfs.rs:637-652），未知方案已有 `UnsupportedEncryptionScheme` 可行动错。`DecryptingTransport` 位于 `enc_stream.rs:76`。
7. **rebuild 闸调用点 = 两处**：`cloudkit-cli/lib.rs:4010`（活实例路径）与 `:4919`（离线路径）；`ensure_plaintext_instance` 定义 rebuild.rs；`cydrive rebuild --help` 文案含 "Encrypted instances are refused"。telegram 拒 rebuild 是另一条（`TELEGRAM_REBUILD_REFUSAL`，transport-only，**不动**）。
8. **remote_handle 注释挂着的 pending-owner-review「plan A（row size=密文长）」**：本计划方向（row size 维持明文、闭式反推）即其裁决出口——EB4 在 decisions 记录其关闭（拒绝 plan A，理由 = 网关 Content-Length/Range 契约与 R6 size=明文）。

## 1. 设计裁决（B1–B6，随本计划批准生效；执行期不得漂移）

| # | 裁决 | 理由与边界 |
|---|---|---|
| **B1 逐文件真相三层优先**：物化写 cipher 列时 **既有行的 cipher 真相（is_encrypted/scheme/其 size 推导）> 实例配置初值**——既有行只在「无 cipher 真相」（新行）时用配置猜；`is_encrypted=1` 的行**永不被列举猜测降级**，配置错/加密码晚于存量明文行等歧义一律由 B2 首读校正 + B6 sync 出路兜底 | 列表在信息论上不知道单文件真相（v1 无 magic）；上传/sync/首读修正是真相源，列举只能填空不能覆盖。`is_encrypted=1` 不降级与「删密码后旧行保持加密+MissingPassword」的既有语义同轨 |
| **B2 首读内容校验（权威，先于字节出门）**：stream 臂在 `DecryptingTransport` 构造/首窗处以容器头（34B，窗口数学本就要读）校验 magic+参数→真值尺寸与行不符→**定向回写行（scheme+size）并用真值继续**；hydrate 臂解密完成后以**本地明文文件长度**为真值回写，gcm 标签行遇 `CKCRYPT2` 头→改判 v2 重新 hydrate | 内容是唯一权威；回写用定向 UPDATE（只动 cipher 语义列+updated_at，不碰 coalesce 列）；一切纠正发生在任何字节/Content-Length 交给客户端之前 |
| **B3 物化专用 upsert**：新增 `upsert_materialized`（或等价命名）——INSERT 用配置初值（含 cipher 列），ON CONFLICT **保留既有 cipher 真相**（is_encrypted/scheme 保留；size 按**保留后的 scheme** 用闭式从 `entry.size`（密文长）重推——远端内容变更跟随，真相不丢）；`materialize_entry` 增加 cipher 上下文参数（`Option<CipherCtx{password_is_set, scheme}>`，来自 Vfs cfg——与上传行 `vfs.rs:500` 同源） | 单一新方法、**零既有写方影响**（upsert_file/upload/sync 路径一字不动）；R6 零 DDL（全部既有列）；read-through 与 rebuild 共用（D4 同轨） |
| **B4 不做列举期逐文件嗅探**：物化**不**读文件头/不逐文件 Range——O(1)/层（A3）在加密卷同样保持；一切内容校验在 B2 首读 | N×34B 小读在限速后端（pan123 2rps）首视图即分钟级放大；首读必然发生，校验点放那里零额外轮次 |
| **B5 rebuild 闸同批放开**：删除 `ensure_plaintext_instance` 与两处调用 + `--help`/K11 文案，rebuild 对加密卷走同一 B3 物化（全量校对语义与明文卷一致）；telegram 拒收（`TELEGRAM_REBUILD_REFUSAL`）不动 | K84.4 决策点按分析推荐拍板：信息论障碍同源、解法同源，一开一不开 = 「现查能建、rebuild 拒」语义分裂；行形状契约由 B3/B2 保证 |
| **B6 残余歧义如实挂账**：「给存量明文卷新加密码 + 索引已丢 + 配置=gcm」场景中，明文文件被猜成 v1 加密 → 首读解密失败报**单一可行动错**（文案含双假说：密钥/方案错 或 该文件实为明文（建议 `cydrive sync` 取逐文件真相））；不静默吐密文当明文、不吞错 | v1 无 magic 是冻结格式的代价；安全方向 = 响亮失败；sync 行级携带 is_encrypted/scheme 是系统既有出路 |

**门序终态**：D2 门（`authoritative_index && as_driver()`）不变；H1 的加密退化臂**移除**（加密卷与明文卷同路）；D10 语义 = B1+B2 的物化纪律（永不物化错账、读永不整面拒）。窄面（telegram 加密卷）仍走 D2 退化，零变化。

## 2. 任务分解（EB1→EB4；每批 TDD 红→绿 + 五门禁 + 跟踪单收口随 commit）

### Task 1（EB1）：cipher 真相物化（B1+B3+B4 落地）

**Files:**
- Modify: `crates/cloudkit-core/src/database.rs`（新 `upsert_materialized`；对照 `upsert_file` database.rs:462-493 与 `upsert_file_scheme`:523 的 SQL 形态写 ON CONFLICT 保留集）
- Modify: `crates/cloudkit-core/src/materialize.rs`（`CipherCtx`、尺寸闭式 `plaintext_len_from_container`、`materialize_entry` 签名扩展）
- Modify: `crates/cloudkit-core/src/readthrough.rs`（移除 H1 加密退化臂；把 cipher 上下文从 Vfs 传入）
- Modify: `crates/cloudkit-core/src/vfs.rs`（两薄壳改传 `CipherCtx::from_cfg(&self.cfg)`——判据 `encryption_password.is_some()` + `encryption_scheme`，与 vfs.rs:500 同源）
- Modify: `crates/cloudkit-core/src/rebuild.rs`（`walk_one_dir` 的物化调用传 cipher 上下文——**此时闸还在**，EB3 才删；本批先让函数能构造 ctx）
- Test: `crates/cloudkit-core/tests/materialize.rs` 追加 + `crates/cloudkit-core/tests/readthrough.rs` 追加

**Step 1 红测试**（核心不变量先行）：

```rust
// materialize.rs 追加（骨架；Entry/FileRecord 构造照本文件既有 helper 适配）：
// T1 同构：物化行 ≡ 上传行（cipher 语义列逐字段）
#[test]
fn materialized_encrypted_row_is_field_identical_to_the_upload_row() {
    // 同一 entry（密文长 1_048_626 = 1MiB 明文 + 34B 头 + 16B tag）分别走
    // ①上传侧形状（is_encrypted=true + upsert_file_scheme(aead_v2)，照 vfs.rs:500-506）
    // ②物化侧（CipherCtx{set:true, scheme:aead_v2} + entry.size=密文长）
    // 断言：is_encrypted/encryption_scheme/size 全等（size == 1_048_576）
}
// T2 闭式边界（格式文档语义）：
#[test]
fn container_size_backsolves_plaintext_exactly() {
    assert_eq!(plaintext_len_from_container(44, GCM), 0);      // v1 空
    assert_eq!(plaintext_len_from_container(50, AEAD_V2), 0);   // v2 空（34+16）
    assert_eq!(plaintext_len_from_container(44 + 9, GCM), 9);
    assert_eq!(plaintext_len_from_container(34 + 16 + 1_048_576 + 16, AEAD_V2), 1_048_576); // 恰整倍数满尾块
    assert_eq!(plaintext_len_from_container(34 + 5 + 16 + 1_000_000 + 16, AEAD_V2), 1_048_581 - 34 - 16); // 多块+短尾（算式按公式写死数值）
}
// T3 既有真相保留（防震荡——B1 核心）：
#[test]
fn relisting_never_downgrades_a_corrected_encrypted_row() {
    // 播种：is_encrypted=true, scheme=gcm, size=真值（模拟首读修正后的行）
    // 实例 cfg = aead_v2 + 密码在；read_dir_fresh 回源同名 entry
    // 断言：行的 scheme 仍 gcm、size 仍真值（按 gcm 闭式重推，非按 cfg 猜）、is_encrypted 仍 true
}
// T4 新行才用配置初值 + 明文实例不写坏 legacy 加密行：
#[test]
fn config_guess_fills_only_absent_truth() { /* 无行 → cfg 推导值落库；既有 is_encrypted=1 行在明文实例回源后仍 is_encrypted=1（B1 永不降级）*/ }
// readthrough 追加：H1 退化臂移除后——加密+宽面实例 read_dir_fresh **回源物化**（替换
// encrypted_instances_degrade_to_the_index_read… 用例 12：断言 list 调用=1 + 行落库 is_encrypted=true）
```

**Step 2** 跑红：`cargo test -p cloudkit-core --test materialize --test readthrough` → 编译失败/断言失败留证。

**Step 3 最小实现**：
- `pub struct CipherCtx { pub enabled: bool, pub scheme: &'static str }`（`CipherCtx::from_cfg`）；
- `pub fn plaintext_len_from_container(ct: i64, scheme: &str) -> i64`：gcm = `ct-44`；aead_v2 = 按 1MiB（`AeadV2::default_chunk_size()` 或常量，EB1 内核对 v2.rs 是否已有 pub 暴露，无则 `pub const CONTAINER_CHUNK: i64 = 1024*1024` 并注 `vfs.rs:261 AeadV2::new()` 同源）闭式：`body=ct-34; n_minus_1=(body-16)/(CHUNK+16); last=(body-16)%(CHUNK+16); pt = n_minus_1*CHUNK + last`（空件 body=16 → 0；校验 last≤CHUNK，违例回 `ct` 并 warn——防御臂）；未知 scheme → 保守返回 `ct`+warn（B2 首读会修）。
- `materialize_entry(db, entry, cipher: Option<&CipherCtx>)`：cipher=None（明文实例）保持**今日逐字行为**（走 upsert_file）；cipher=Some → File 臂改走 `upsert_materialized(entry_fields with is_encrypted=true, scheme=cfg.scheme, size=plaintext_len_from_container(entry.size, **真相 scheme**))`——真相 scheme 取法：`db.get_file(&vpath)` 既有行若 `is_encrypted=1` 用其 scheme（并以该 scheme 重推 size），否则用 ctx.scheme；Dir 臂不涉 cipher（目录行与加密无关，仍 upsert_file，但 is_encrypted 写 false——**目录行永不明文化**，测试钉）。
- `upsert_materialized` SQL：INSERT 列含 cipher 列；ON CONFLICT = `upsert_file` 的集**去掉 `is_encrypted=excluded.*` 与 `size=excluded.*`**，改为 `is_encrypted = CASE WHEN files.is_encrypted=1 THEN 1 ELSE ?guess END`、`size = ?derived`（derived 由 Rust 按真相 scheme 算好传入——Rust 侧已读既有行，SQL 无需自算）、`encryption_scheme` 仅在 `files.is_encrypted=0` 时写入（首次建立真相）；updated_at 照刷（sweep 免疫）。
- readthrough：删 `if encrypted_instance { return db.list_dir… }` 臂（及 stat_fresh 同款），签名 `encrypted_instance: bool` 换 `cipher: Option<CipherCtx>`（或 `&CipherCtx`+内部 None 分派）；Vfs 两薄壳构造传入。
- rebuild.rs `walk_one_dir`：同签名适配（cipher 构造缝留给 EB3 接闸删除后的启用）。

**Step 4 跑绿 + 回归**：上列 + `cargo test -p cloudkit-core --test rebuild`（既有 9 测试零漂移——rebuild 传 cipher=Some 但测试实例无密码时 ctx.enabled=false 走旧路，**用例不改**）+ readthrough 其余 16 零漂移。

**Step 5 Commit**：`feat(core): 加密物化 cipher 真相三层优先 + 容器尺寸闭式反推（Phase 8-B EB1）`

### Task 2（EB2）：首读内容校验与回写（B2+B6 落地）

**Files:**
- Modify: `crates/cloudkit-core/src/enc_stream.rs`（`DecryptingTransport` 构造/首窗：容器头校验+真值尺寸回写缝）
- Modify: `crates/cloudkit-core/src/vfs.rs`（`open_read` 加密流臂接入校验；hydrate 完成后本地长度回写 + gcm 标签行遇 magic 改判；新 `fix_cipher_columns` 定向 UPDATE）
- Modify: `crates/cloudkit-core/src/database.rs`（`fix_cipher_columns(id, scheme, size)`——只动 `encryption_scheme/size/updated_at`）
- Modify: 拒收文案臂（`UnsupportedEncryptionScheme` 保留；新增/扩「解密失败双假说」文案于 hydrate gcm 失败处——**改文案不改分类**）
- Test: `crates/cloudkit-core/tests/readthrough.rs` 或新 `tests/encrypted_read.rs`

**红测试（骨架）**：
1. `stream_arm_repairs_a_wrong_scheme_row_before_serving`：播种 `scheme=gcm、size=错` 的行 + 远端真 v2 容器 → `open_read` 流式读窗口 → 字节逐字节等于明文 + 行已回写 `scheme=aead_v2、size=真值`（定向 UPDATE 断言 sha256/coalesce 列未动）。
2. `hydrate_repairs_size_from_local_plaintext_length`：播种错 size 行 → 走 Hydrate 全量 → 读通 + 行 size=明文长度。
3. `gcm_labelled_v2_content_redispatches`：行标 gcm、内容 `CKCRYPT2` → hydrate 改判 v2 成功（而非 v1 解密失败）。
4. `v2_labelled_non_container_fails_actionably`（B6 文案钉）：行标 aead_v2、内容无 magic 且 v1 形失败 → Err 含 `cydrive sync` 关键词；**断言绝不返回原文密文字节**。
5. 面级（webdav harness）：错 size 行 → GET → Content-Length 与所发字节数一致（首读修正传导）。

**实现要点**：校验点在**任何响应字节/Content-Length 之前**（`open_read` 返回前完成流臂校验；hydrate 在 `write_atomic` 落盘后、`set_cached_flag` 前回写）；`fix_cipher_columns` 幂等；DecryptingTransport 若已在内部读头（enc_stream.rs 实现期核对），校验挂同一读、**零额外网络往返**（B4 对齐）。

**Commit**：`feat(core): 加密读首读内容校验与行回写（Phase 8-B EB2）`

### Task 3（EB3）：rebuild 闸放开（B5）+ 联动清理

**Files:**
- Modify: `crates/cloudkit-cli/src/lib.rs`（删 :4010/:4919 两处 `ensure_plaintext_instance` 调用；`cydrive rebuild --help` 文案去 "Encrypted instances are refused" 改为正常描述 + 加密语义一句）
- Modify: `crates/cloudkit-core/src/rebuild.rs`（删 `ensure_plaintext_instance` 定义与 K11 模块文档加密拒收段——改写为「加密卷同走 materialize 真相语义（Phase 8-B）」；`walk_one_dir` 接上 cipher 上下文真实构造）
- Modify: `crates/cloudkit-core/src/readthrough.rs`（模块文档「rebuild 拒收语义独立存在」引用句清理）
- Test: `crates/cloudkit-core/tests/rebuild.rs`（`encrypted_instances_are_refused_with_sync_guidance` **翻转**：加密实例 rebuild 成功 + 行 cipher 字段正确——红→绿留证；telegram `TELEGRAM_REBUILD_REFUSAL` 用例**不动**）

**验证**：`cargo test -p cloudkit-core --test rebuild` + `cydrive rebuild --help` 实跑截图输出 + 全量五门禁。

**Commit**：`feat(rebuild): 放开加密卷全量校对——ensure_plaintext_instance 闸移除（Phase 8-B EB3）`

### Task 4（EB4）：两阶段验收 + 真网重跑 + 文档收口

**Files:**
- Test: 新 `crates/cloudkit-cli/tests/encrypted_readthrough_e2e.rs`（**非 ignored，进常规 CI**——local 驱动 + TempDir 后端 + 实例密码，真 crypto 全链）：
  - **腿 1 用户两阶段协议**：①加密上传若干文件（含空件/恰 1MiB 整倍数/跨块）→ 退（drop Vfs/db）→ ②新开 db（**wipe**）→ `read_dir_fresh` 逐层物化（断言行 cipher 字段+尺寸闭式精确）→ 下载逐字节等于原明文 + Range 三窗口逐字节。
  - **腿 2 混合方案自愈**：远端预置 v1 容器（`crypto::encrypt` 现造）+ 实例 cfg=aead_v2 → 首读回写 scheme=gcm + 读通。
  - **腿 3 rebuild 全量**：wipe 后 `rebuild_from_backend` → 全树行 cipher 字段正确 → 读通（B5 验收）。
- Test: `#[ignore]` 真网腿——pan115_e2e/pan123_e2e **加密腿重跑**（凭据 env 自 `E:\GitHub\rs-CyDrive\test\` 读，值不落盘；K77 token 操作纪律；H1 挂账就此销账）。
- Modify: `README.md`（机制段「加密卷永不回源物化」→「加密卷同构 read-through：逐文件真相保留 + 首读容器校验自愈」）/ `AGENTS.md`（Phase 8-B 块 + 计数）/ `docs/decisions.md`（**K85**：执行记录——B1–B6 落地、**plan A（row size=密文长）pending-owner-review 关闭=拒绝**（网关 Content-Length/Range 与 R6 size=明文承重，B6 残余歧义入挂账）、B5 rebuild 同批、真网结果）/ `docs/tracking/phase8b-encrypted-readthrough.md` 终态账。
- 五门禁终跑 + 七组合裁剪 clippy（同 Phase 8 收口口径）。

**Commit**：`test+docs(phase8b): 两阶段验收与真网重跑 + K85 收口（Phase 8-B EB4）`

## 3. 验收对照

| 标准 | 验证 |
|---|---|
| 两阶段协议（负责人原话） | EB4 腿 1（离线 CI 级）+ pan115/pan123 真网加密腿重跑 |
| 混合方案自愈 | EB4 腿 2 + EB2 测试 1/3 |
| 尺寸/方案承重面正确 | EB1 T1/T2 + EB2 面级 Content-Length 测试 + 既有 Range e2e 零漂移 |
| 逐文件真相不回冲（防震荡） | EB1 T3/T4（红→绿） |
| rebuild 与现查语义一致 | EB3 测试翻转 + EB4 腿 3 |
| telegram/明文卷零变化 | 全量既有断言零漂移 + D2 退化臂测试原样绿 |
| 门禁 | 每批五门禁 + EB4 终跑 + 七组合 |

## 4. 执行顺序与依赖

EB1（物化真相）→ EB2（首读校验——依赖 EB1 的行语义）→ EB3（闸删除——依赖 EB1 物化在加密下正确）→ EB4（验收收口——依赖前三）。hub-and-spoke 派发同 Phase 8 纪律：主会话拆分/审查/合并，实现落子代理，TDD 红→绿逐条留证。

## 5. 架构合规

R1 零新增边（全在 L3+L5）；R2 错误分类学不动（新文案挂既有变体）；R6 零 DDL（全既有列；size=明文由闭式保证——**这正是拒绝 plan A 的理由**）；R4 不涉；锁纪律（回源 IO 仍在 handler 锁外）；D7/D8 的 in-flight、双确认、sweep 三重保护与加密物化正交不动。

## 6. 挂账预登记

- v1 无 magic 的残余歧义（B6 文案出路 + sync 出路；远期 = 远端元数据面 manifest，§8 Phase 8 挂账延续）。
- 首次 PROPFIND 在「配置初值猜错 + 未首读」窗口显示初值尺寸（一次性、首读后自愈）——EB2 面测试注明为接受残余。
- mtime = 云盘所报（与明文物化同口径，非本批新增）。
- 真网加密腿若因 token 轮换不可用：如实挂账负责人真机窗口，离线腿 1–3 已覆盖机制本体。
