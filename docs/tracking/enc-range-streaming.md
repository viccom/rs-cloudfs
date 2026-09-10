# Phase 3.5-a 加密 Range 流式读 任务跟踪单

> 计划：docs/plans/2026-09-10-enc-range-streaming.md ｜ 裁决 K47 ｜ 参照：PCFS Explore 报告（会话 2026-09-10）
> worktree：`feat/enc-range`（收口 merge 回 main）。

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| E1 | crypto 公开窗口解密 API | ✅ | `AeadV2::window_reader` + `AeadV2Window`（open/chunk_size/n_chunks_for_plain/plain_tail/ciphertext_span/decrypt_chunk），私有 Header/Layout/decrypt_chunk 复用不外泄；ciphertext_span 返回 (offset,length) 含 34B header | 39b0313；红：aead_v2_window 5/7 断言红（`no empty tail chunk left: 1 right: 2` 等）→ 绿 `7 passed`；与真实编码器互验 + 确定性窗口 == 全解密切片 |
| E2 | DecryptingTransport + open_read 门改造 | ✅ | `core/src/enc_stream.rs`：懒拉 header（首个 open_range）+ OnceLock 缓存 + 逐 chunk 验签；open_read 门拆分（aead_v2+range+size>0 → Stream 且 total_size=row.size；gcm/未知/无range/0B → Hydrate）；密码门与 WF0 次序不变 | 38689b2；红：enc_stream 4/6 + vfs_open_read 1 断言红（`must stream (K47)`）→ 绿；workspace 887/0/9（基线 867+20）、winfsp 92/0/1、clippy/fmt/check_layers/scan_secrets 全过 |
| E3 | 面测试 + 真机验收 + 文档收口 | ✅ | CI 面与文档完成：webdav 1 + winfsp 2 个 aead_v2 流式读测试（E3-PROBE 红态→revert 绿，断言以 mock 调用形态区分流式/水合）；decisions K47、README 流式节、AGENTS 计数（888/94）落盘。**真机验收与 merge 归主会话** | 批次日志 2026-09-10 E3 |

## 批次日志

- 2026-09-10：立项。PCFS 加密读路径查证（CTR 算术 seek 实时解密成立但无认证）；本仓 aead_v2 原语 `decrypt_range`/`decrypt_chunk` 已备；K47 裁决定稿（aead_v2 窗口化，不用 CTR）。计划与跟踪单落盘。
- 2026-09-10 E1：39b0313 窗口 API 红→绿（编译桩断言红）。设计点：`ciphertext_span` 取 (offset,length) 语义（open_range 直用）；exact-multiple 无空尾 chunk 与 `Layout::derive` 经真实容器互验；`AeadV2Window` Send+Sync 编译期钉。
- 2026-09-10 E2：38689b2 包装器+门改造红→绿（透传桩断言红）。既有加密钉测试全部存活（vfs_open_read 2/3/11、webdav fs_adapter 6e、smoke 3f——种子行未写 scheme，列默认 "gcm"，密码门先于门拆分）；测试修两处（mock upload 计数含种子、错误面在 open_range 调用点浮出），断言零漂移。
- 2026-09-10 E3（CI 面，子代理执行）：两面 aead_v2 流式读测试红→绿。种法：`AeadV2::new()` 默认 1MiB chunk、明文 2MiB+700000（3 chunk 非整除尾），密文容器入 mock 单消息，行 `is_encrypted=true`+`upsert_file_scheme("aead_v2")`+`size=明文长`。红态：E3-PROBE 探针（`vfs.rs` open_read 门临时短路回 Hydrate，跑完 revert，`rg E3-PROBE` 零残留）下两面三测试全红——webdav `left: [] right: [(0, 34), (34, 1048592)]`、winfsp 同型：水合路径字节正确但 `open_range_calls` 为空，判别力 = 断言钉 mock 调用形态（open_calls 恒空 + open_range_calls 精确密文坐标向量 [(0,34) header, (34,span)]）。绿态：webdav fs_adapter 30/0（Range 100-299 精确切片、全文逐字节、双 handle header 各拉一次）；winfsp read 12/0（16B 面窗跨 1MiB 密文 chunk 边界逐字节、精确 span 向量、K41 宽限重开零新调用 header 只拉一次）。门禁：workspace 888/0/9（基线 887+1）、winfsp 腿 94/0/1（92+2）、clippy -D warnings / fmt / check_layers（12 manifests）/ scan_secrets 全过。文档：decisions K47、README 流式节一句、AGENTS 计数、本单。**真机验收（加密卷 open 时长/冷 seek 每跳一窗/哈希比对）与 feat/enc-range merge 归主会话。**
- 2026-09-10 E3（真机，主会话）：独立实例（winfsp Q: + baidu 新路径 `/apps/cloudfs-encrange` + aead_v2 随机口令，24MiB 随机文件上传排空 `is_uploaded=1, is_encrypted=1, scheme='aead_v2'`）。**清缓存后冷态**：OPEN = **11ms**（对照 K46 形态整文件水合同规格 ≈9s）；TTFB 64K = 1020ms（header 34B + PBKDF2 100k + 首 4MiB 密文窗）；随机 seek 五点（25/50/90/10/60%）各 275–672ms = 每跳一个密文窗口往返；全文读 2971ms，SHA256 `d06bdd4f…4750b1d` 与源逐字节一致；热重开+TTFB 561ms = header+首窗重拉（流式按设计不落缓存副本，缓存化属 K46 账本）。收尾：远端文件删净（db 行清空）、`cydrive stop` 一次卸 Q:、run.log 0 ERROR、实例目录（含凭据副本）已擦除。
