# Phase 8-B 加密 read-through 合入前深度审查 findings（K78 形态）

> 审查基线：`feat/readthrough-index`@ac43799（范围 = `780a8a4..ac43799` 全部 12 提交）；三路并行——主会话精读核心正确性链（materialize/database/vfs/enc_stream/readthrough/rebuild/upload_queue+baidu 两 K85 修复的生产 diff 逐行）+ A 测试面 + B 集成装配驱动面子代理，全部只读 + 现场实跑（A 路五个 crate 目标套 155/0；B 路 ck-baidu 全套 + volumes_page 41/0 + fs_adapter 35/0 + 两裁剪组合 check 绿）。
> 总判：**0 High / 4 Medium / 6 Low / 3 Info**。B1–B6/K84.2/K85.6/K85.7 七项行为主张六项被现有测试钉死（变异重引入会红），唯一未钉死层 = B1 的 SQL CASE（M1）；生产代码正确性链未发现 High/Medium 缺陷。

## Medium

| # | 来源 | 内容 | 裁决 |
|---|---|---|---|
| M1 | A-F1 | `upsert_materialized` 的 SQL CASE 保留集**无直接钉测**——`materialize_entry` 先 `get_file` 读出既有真相再传参，单线程下 CASE 与 `excluded` 直写逐字节同结果（删 CASE 全套仍绿）；该层只在 `get_file→upsert` 并发窗内真正生效（首读 `fix_cipher_columns` 与回源物化竞态的最后防线） | **修复批**：绕过 Rust 面直接调 `upsert_materialized` 传相异 excluded（is_encrypted=false/scheme=aead_v2/size=500）→ 断言既有加密行 flag/scheme 保留、size=excluded |
| M2 | A-F2 | 非默认分块 v2 容器的「头权威」修复臂**零覆盖**——`first_read_admit` 用容器头自述分块 `plaintext_len_with_chunk(ct, header_chunk)` 反推，变异成默认 1MiB 反推全绿；该分支是「物化闭式按默认分块猜错」场景的唯一自愈路径（生产今天不产非默认分块，为此防御而写=不测等于没有） | **修复批**：①64KiB 分块容器的 `plaintext_len_with_chunk` vs `plaintext_len_from_container` 差分钉测；②错 size 行 + 64KiB 容器 → `open_read` 流式 total_size=真值 + 行回写 |
| M3 | B-F1 | pan115 0 字节上传**桩与真机双无覆盖**——`upload_init(file_size=0)` 服务端接受度未实证（百度 errno=2 已证明「服务端拒 0 字节特形」真实可能；若拒则重试耗尽→降级→0 字节在权威后端永不落盘=功能缺陷级；不产生破损会话）；桩对 file_size=0 无专测无拒绝建模 | **挂账**：负责人真机窗口补 0 字节探针（pan115 token 失效中，与 K85.5 挂账同窗口） |
| M4 | B-F2 | webdav 0 字节 PUT + `.part`/MOVE 固化链**无任何 0 字节专测**（write_path 的 upload_bytes 辅助全用非空 pattern）；路径推演安全（空体 PUT RFC 4918 合法、stat 复核 `unwrap_or(0)==0`） | **修复批**：stub 0 字节全链腿（上传→固化→回读空→不留 staging 残留） |

## Low

**顺手修（随修复批）**：
- L1（B-F3）baidu `finalize_tail` 空缓冲分支缺 `drained == 0` 守卫——**K85.7 引入的隐性回归**：hinted 整块文件（size 恰为 4MiB 整数倍）到齐路径 drain 后 buffer 清空、`tail_included` 仍 false，close 无条件复调走进空缓冲分支给 `block_md5` **追加多余 EMPTY_MD5**（当前被三重耦合吸收：收尾免传跳过 / create 用 session_blocks 快照 / 会话表只在分片成功后落盘——无外部症状但脆弱）→ TDD 红→绿补守卫。
- L2（A-F3）超短加密内容边界（<34B 却标加密）无测试——短头无 magic → Hydrate → v1 双试失败 B6 文案，行为推断合理但无钉。
- L3（A-F6）结构违例臂（有 magic 但 ct 与头分块矛盾，`derived=None`）无测试——该臂回写 scheme、保留 row.size 出流式窗（窗口数学随后响亮失败），截断/篡改场景零覆盖。
- L4（A-F5）K85.7 桩与断言引用驱动导出的 `EMPTY_MD5` 常量（轻度同源：常量值漂移时桩+实现+断言三者同漂全绿）→ 桩内豁免判据与断言期望改独立字面量 `"d41d8cd98f00b204e9800998ecf8427e"`（真网探针钉死的值）。

**挂账**：
- L5（A-F4）同文件并发双首读无测试（两笔 34B 头读 + 两次同值幂等 `fix_cipher_columns`；幂等 + affected-0 良性契约注释已声明，风险低）。
- L6（B-F4）web 门控钉测是**源码文本 pin 而非行为 pin**（`!volumes_js.contains("v.encrypted")`）——无 JS harness 前提下最强钉法，但过强（未来合法使用 `v.encrypted` 如加密锁图标会误红）且不防 gate 改写；知情残余。
- L7（γ 批执行期发现，**非本批引入**——γ 在 stash 基线上复现）ck-webdav `write_path` 两条既有 flake：`put_insufficient_storage_maps_to_io_with_the_code`（507 旋钮耗尽后的重传腿 size 复核见 0——疑连接复用/时序敏感，压测 2/38 轮）与 `stager_lost_ack_on_move_resumes_as_committed`（lost-ACK 恢复路径偶发 `error sending request`，基线 1/20 轮）；γ 的新钉测在全部约 38 轮（含失败轮）100% 通过。根因未查（或与审查批并行构建的负载放大有关），挂账待专门批。

## Info（留痕）

- I1（B-F5）pan123 0 字节真机未测（桩全绿且注释自声明；etag=空串 MD5 为合法 hex，端点拒收风险低于百度空数组形态）——与 M3 同批真机窗口。
- I4（β-1 执行偏差裁决，主会话追认）任务书原案的红测试（断言 precreate 收到的 block_list）在缺陷代码上**结构性不可红**——hinted 整块文件的 precreate 在 write 到齐路径就已带正确列表发出，close 复调的追加只污染 `block_md5` 内态，被三重巧合吸收（wire 面零症状，执行代理在缺陷代码上实跑 wire 断言为 ok 实证）。红测试改 src white-box 单测直驱 `finalize_tail` 内态（真实红输出留证：`left: [m0, m1, d41d8…] right: [m0, m1]`），wire 契约钉另由 `hinted_exact_multiple_file_block_list_is_exactly_two_real_blocks` 承担（六字段 exact 断言 + 显式「不含空串 MD5」）。追认理由：白盒是缺陷唯一可观测面，wire 钉保持对外契约——两层合起来比原案更强。
- I2（A-F7）既有断言的授权变更：readthrough 用例 12 随 K84 裁决由「加密零回源」翻转为「回源+真相落库」（跟踪单已注明）；`vfs_open_read.rs` K47 矩阵走无 chunks 遗留形态、`encrypted_read.rs` 走 chunks 校验面——两形态分治无空转。
- I3（主会话精读观察）三条：①aead_v2 行每次 `open_read` 的 34B 头读 + PBKDF2 派生成本与 8-B 前同形（`DecryptingTransport::new` 内部本就做同一次读+KDF，8-B 只是提前，未加量）；②`materialize_entry` 的 `get_file→upsert` 窗内与并发 commit_put 的极窄竞态——CASE 保 flag/scheme，size 若按旧真相派生错则由首读 B2 自愈；③`upsert_materialized` 的 `telegram_msg_id=coalesce(excluded, files)` 对 0（非 NULL）恒覆盖——幂等形态（同一后端 handle 解析恒同值），非缺陷。

## 三路查证无问题项（摘）

v2 尺寸闭式三分支数学逐项验证（空件 body=16→0 / 恰整倍数 last=chunk→k·chunk / 结构违例 None）；CASE 表达式引用冲突前旧行值（SQLite ON CONFLICT 语义正确）；`fix_cipher_columns` 定向三列 + 幂等 + doorbell 不压制（sync 载荷列=真相）；admission 分流（自洽 gcm 行零内容读优化 / aead_v2 恒校验 / 无 magic 统一转 Hydrate 双试）；K84.2 双试两方向 + hydrate 回写位置（解密完成后、set_cached_flag 前）+ B6 文案挂既有变体；K85.7 EMPTY_MD5 三处免传完备 + 不误走 superfile2（真网 return_type=1 实证）+ 重试无脏会话资产；K85.6 门控影子臂逐字保留 + `zero_byte_plain_job` 单块有效计划 + 加密臂各自重 plan；六宽面 0 字节路径逐一追踪**无一产生破损会话**（local/sftp 既有绿测、baidu 已修、webdav/pan115/pan123 覆盖差异见 M3/M4/I1）；rebuild 三入口（CLI/控制通道/web）+ vfs 两缝 = **CipherCtx::from_cfg 四缝同源**；`TELEGRAM_REBUILD_REFUSAL` 四处原样保留；`ensure_plaintext_instance`/`RebuildError::EncryptedInstance` 彻底删除（残留引用全为消费面防御 match 臂）；`AeadV2Window::open` 容忍超长头（只读前 34B）——传输层多返回不致敏；web i18n 死键全仓 grep 干净；webdav lib.rs +7 行 = 首读回写后行重读（Content-Length 真值传导，行消失保留旧快照）；decisions K84/K85.1/K85.6/K85.7 与代码现状逐条相符；两裁剪组合（baidu / telegram）check 绿。
