# 审查修复跟踪表（review-fixes）

> 计划：docs/plans/2026-09-11-review-fixes.md ｜ 报告：docs/reports/2026-09-11-phase3-winfsp-enc-review.md（终版·经对抗性复核）
> worktree：`fix/review-batch`（收口 merge 回 main）｜ 探针底稿：`verify/review-findings` worktree `crates/cloudkit-winfsp/tests/verify_probe.rs`（13 测试）
> 基线：workspace 891/0/9、winfsp 腿 94/0/1。

| 批次 | 任务 | 覆盖发现 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|---|
| RB1 | case-rename 修复 + 宽限表失效 | winfsp-C1、winfsp-H1 | ✅ | 2026-09-11 完成 | 红：verify_probe C1×4/H1×2 改断正确行为后 6 失败（ACCESS_DENIED 0xC0000022 / COLLISION 0xC0000035 / 行缓存被删 / 陈旧 EOF 8B≠64B）；绿：winfsp 腿 107/0/1（含翻绿 6 测试）、workspace 892/0/9（=基线 891+webdav case-rename 回归 1）、clippy×2/fmt/check_layers/scan_secrets 全过。C1：case-only 判定移到规范化后（含 from==to 原始相等形态）走合法改名路径 + dest.id 守卫；H1：take_live 行 size 校验不符即弃 + delete_after_cleanup/rename_entry（源与目标）/cleanup 提交成功后 grace.invalidate，提交/删除成功同时清本 handle 读状态防 close 重泊陈旧态；verify_probe.rs 转正式回归（头注更新，门控不变，RB2+ 探针 7 个保持断言未修 BUG）。webdav 面：row 查找字节精确，case-variant 目标不可能命中源行——无需修，补回归测试 rename_case_only_lands_row_and_cache_at_the_new_spelling 钉死。 |
| RB2 | 命名卫生与失败语义 | winfsp-H4、M1、M2、M3、H2 | ✅ | 2026-09-11 完成 | 红：verify_probe H2/H4/M1/M2/M3 探针翻正后 5 失败（脏行 open 仍 panic "db rows carry canonical rel paths" / 失败日志仍 "the write was discarded" / /.foo.tmp 副本被截为 b"new-bytes" / /nul.txt 读回 Ok([])（进 NUL 设备）/ 300 宽字符名 fill 仍 err(NTSTATUS -1073741670)）；core vfs 段长测试 2 失败（create_dir 拒绝断言 / put 可行动消息断言）；write.rs create 面 1 失败（0xC0000185 EIO ≠ 0xC0000033）。绿：winfsp 腿 110/0/1（含翻绿 5 探针 + writer 单测 +2 + create 面测试 +1）、workspace 900/0/9（=RB1 基线 892 + core vfs 段长 ×4 + cache 消毒映射单测 ×4）、clippy 两腿/fmt/check_layers/scan_secrets 全过。H4：StagedWriter::commit 失败分支不删任何字节（sync 失败与 put_staged 失败都保留唯一副本）、cleanup 失败日志改「bytes were kept at <位置>; row stays pending; requeued at the next boot」（位置按 final path/sibling 实测选择）、模块/注释语义同步改写；M1：staged_sibling 加随机段 `.{name}.{rand8}.tmp`（RandomState 种子 32bit hex，无新依赖）、Drop/abort 清实际随机名；M2：CacheManager::local_path 每段消毒——`%`→`%25` 先行 + 保留设备名干（CON/PRN/AUX/NUL/COM1-9/LPT1-9，首点前 stem 判定，大小写不敏感）加 `~` 标记前缀（字面 `~` 开头段编码 `%7E` 防歧义）+ 尾点/尾空格逐字符 `%2E`/`%20`；rel_from_disk 精确反向解码（eviction/clear_except 依赖 round-trip），映射表钉住单测（22 保留名×形态 + 单射性 + round-trip + 端到端），只影响磁盘路径不影响 vpath/db，旧副本自然 miss 重水合；M3：fill_dir_info 对 >255 UTF-16 单元名 warn+跳过（返回 Ok(false)，read_directory 条件写入，整目录不再 fail），vfs put 链（put/put_staged/ingest_file/create_dir）加 MAX_SEGMENT_UTF16=255 段长校验（新 VfsError::NameTooLong，可行动消息含段名+上限，校验先于 rename/copy，error.rs→STATUS_OBJECT_NAME_INVALID、webdav→Forbidden），prepare_create 加同上限 FSD 面拒绝（STATUS_OBJECT_NAME_INVALID）；H2：fs.rs 三处 expect（open_with_read/delete_after_cleanup/rename_entry）换 map_err→invalid_name + error! 日志。write.rs 探针 harness 的 staged_path 改 stage_siblings 扫描（随机名不可预测）；verify_probe 头注更新（RB2 探针转正，H3/M6 保持未修断言）。 |
| RB3 | 集成面与测试防线 | cli-H1、cli-H2、cli-M2、cli-M3、stream-H1 | ⬜ | — | — |
| RB4 | 对齐与加固 | cli-M1、stream-M1、stream-M3、M5、M6、Low 顺手项 | ⬜ | — | — |
| 收口 | 真机冒烟 + K52 入档 + merge/push + 双产物 | — | ⬜ | — | — |

## 推翻项（不修，记录在案）

- winfsp-L4（open 急取读状态，主断言不成立）、winfsp-L5（fetch 有界性成立）、cross-L5（gnu 组合在 winfsp-sys build script 即 panic，产物不可达）。

## 挂账（不在本批）

- 每请求 header RTT + PBKDF2 的 LRU 收敛；fs.rs 拆分；窗口数学五处下沉；Phase 3.6 运行态卷管理（K48-K51 另有计划）。

## 批次日志

- 2026-09-11：立项。三路审查 → 对抗性复核（13 探针 + 27 代码链）→ 终版报告；18 项坐实（4 降级）/ 3 项推翻；修复计划四批 + 跟踪表落盘。待负责人批准开工。
- 2026-09-11：RB1 完成（C1+H1）。
- 2026-09-11：RB2 完成（H4+M1+M2+M3+H2）。验证留证一处说明：core vfs 四个段长测试中 put_staged/ingest_file 两个在红阶段即通过（OS 对 >255 字符磁盘名本就报 error 123 拒绝 rename/copy）——它们保留用于钉住修复后的语义（校验先于 rename/copy、被拒时 staged/source 文件原样保留、零发布），真正的红由 put（可行动消息）与 create_dir（拒绝断言）承担。
