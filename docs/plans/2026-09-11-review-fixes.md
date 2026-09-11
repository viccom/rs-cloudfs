# 审查修复执行计划（review-fixes）

> **For Claude:** REQUIRED SUB-SKILL: executing-plans（hub-and-spoke，TDD 红→绿留证）。依据：docs/reports/2026-09-11-phase3-winfsp-enc-review.md（终版·经对抗性复核）。跟踪表：docs/tracking/review-fixes.md。

**Goal:** 修复审查坐实的 18 项发现（按复核后终判严重度分四批），每批 TDD 红→绿 + 全门禁；探针底稿（`verify/review-findings` worktree 的 `verify_probe.rs`，13 测试）转为红测试起点。推翻项不修；挂账项不进本计划。

**基线**：main@当前 HEAD（workspace 891/0/9、winfsp 腿 94/0/1）；默认构建零漂移红线照旧。

## 批次（worktree `fix/review-batch`；串行；每批独立 commit）

### RB1 数据安全核心（winfsp-C1 + H1）
- **C1**：`rename_entry` dest 命中后比对 `dest.id == row.id` → 同 rowid 即 case-only rename：跳过删除分支、直接 `rename_path`（新拼写）+ 本地 `fs::rename`（大小写不敏感文件系统上即改名）；`from==to` 判定移到规范化后按大小写不敏感比较，命中即走 case-rename 路径（修复形态①的 ACCESS_DENIED）。webdav 同构代码评估是否同批（dav 路径大小写语义不同，若不修记录差异）。
- **H1**：`GraceTable::take_live` 增加行校验参数（open 处传 row size，不符即弃）；`delete_after_cleanup`/`rename_entry`（源与目标两侧）/cleanup 提交成功后 `grace.invalidate(rel)`。
- 红测试：verify_probe 的 C1×4/H1×2 改写为断言**正确行为**（先红）；补 webdav 面 case-rename 回归若适用。
- Commit: `fix(winfsp): case-only rename and grace-table invalidation (review C1+H1)`

### RB2 命名与失败语义（winfsp-H4 + M1 + M2 + M3 + H2）
- **H4**：cleanup 提交失败的日志/注释语义修正（字节保留、行 pending、下次启动 requeue）；`StagedWriter::commit` 失败分支不删已迁移字节。
- **M1**：staging 兄弟名加随机段（`.{name}.{rand8}.tmp`）或探测换名；Drop 清理同步。
- **M2**：`CacheManager::local_path` 映射层消毒——保留设备名（CON/PRN/AUX/NUL/COM1-9/LPT1-9，含 `name.ext` 变体判定）与尾点/尾空格做编码（如 `~xx` 前缀 + 百分号编码）；只影响磁盘路径不影响 vpath/db；旧缓存路径迁移不要求（编码后旧副本自然 miss 重新水合）。
- **M3**：`fill_dir_info` 对 >255 宽字符名单独 `warn!` + 跳过（不 fail 整目录）；create/入库面（vfs put 链）加段长上限校验（可行动错误）。
- **H2**：三处 `.expect` → `map_err`（fs.rs:805/1180/1251）。
- 红测试：H4（enqueue 失败注入断言字节保留+日志语义）、M1（冲突截断探针反转）、M2（NUL/尾点探针反转）、M3（超长名枚举跳过探针）、H2（脏行不 panic）。
- Commit: `fix(winfsp): naming hygiene and commit-failure semantics (review H4+M1+M2+M3+H2)`

### RB3 集成面与测试防线（cli-H1 + cli-H2 + cli-M2 + cli-M3 + stream-H1）
- **cli-H1**：unmount 先探测实际映射（复用 `current_mount_for`），有则删、无则出 note；note 文案区分降级情形。
- **cli-H2**：`mount_cmd_winfsp` 的 `build_stack` 前加 `ensure_not_running`。
- **cli-M2**：join 失败档补降级行为（WebDAV 单卷挂载）或至少声明性 println；与 `Ok(Err)` 档对齐。
- **cli-M3**：CI winfsp job 加 `cargo test -p cloudkit-cli --features winfsp`。
- **stream-H1**：web 层 aead_v2 流式 E2E 测试（206 + 明文 Content-Length + mock 调用形态断言，照 webdav 6h 形状）。
- Commit: `fix(cli,test): unmount actual-mount probing, mount guard, web enc E2E (review batch 3)`

### RB4 对齐与加固（cli-M1 + stream-M1 + M3 + M5 + M6 + Low 顺手项）
- **cli-M1**：doctor（platform）与 mount 探测对齐降级顺序（DLL 缺失 continue 探测第二 subkey；去固定 260 buffer 限制或放大）。
- **stream-M1**：三面短读统一"短读=错误"（web `RangeBody` 短窗改 Err；webdav `fill_window` 校验长度 + 日志）。
- **stream-M3**：debug_assert 升运行时 `Result` 或补文档契约。
- **M5**：物化标志快速失败或至少修正模块注释；**M6**：`resolve_row` miss 且父目录行缺失时返回 PATH_NOT_FOUND（open/get_security 臂）。
- 顺手：cross-L7 注释补明示；L1（卷标）/L2（空 claims 提示）文案修正；winfsp-L6 `with_stream_window` 加上限。
- Commit: `fix: cross-face alignment and hardening (review batch 4)`

### 收口（主会话）
- 全门禁复跑（默认 891+新增/0/9、winfsp 腿、clippy×2、fmt、check_layers、scan_secrets）。
- 真机冒烟（用户实例）：case 改名三形态 Explorer 实操 + 删除重建即开（H1）+ 网页播放回归。
- decisions 入档（K52：审查修复批裁决汇总——含推翻项记录与降级理由）；tracker 全绿；merge main + push + 双产物重建。
- Commit: docs + merge。

## 风险

| 风险 | 缓解 |
|---|---|
| C1 修复改变 rename 语义面（case-rename 现在合法） | 红测试钉三形态 + webdav 面回归全跑 |
| M2 消毒改变缓存路径形态 | 旧缓存自然 miss 重水合（可接受，测试钉新映射）；不影响 db/vpath |
| cli-H1 unmount 探测与 net use 输出解析耦合 | 复用 `current_mount_for`（status 路径已投产） |
| RB4 短读改 Err 影响 web 现行为 | 三驱动自钳制（复核证据），改 Err 仅在未来驱动违约时可见——测试钉 |
