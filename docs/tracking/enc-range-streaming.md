# Phase 3.5-a 加密 Range 流式读 任务跟踪单

> 计划：docs/plans/2026-09-10-enc-range-streaming.md ｜ 裁决 K47 ｜ 参照：PCFS Explore 报告（会话 2026-09-10）
> worktree：`feat/enc-range`（收口 merge 回 main）。

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| E1 | crypto 公开窗口解密 API | ✅ | `AeadV2::window_reader` + `AeadV2Window`（open/chunk_size/n_chunks_for_plain/plain_tail/ciphertext_span/decrypt_chunk），私有 Header/Layout/decrypt_chunk 复用不外泄；ciphertext_span 返回 (offset,length) 含 34B header | 39b0313；红：aead_v2_window 5/7 断言红（`no empty tail chunk left: 1 right: 2` 等）→ 绿 `7 passed`；与真实编码器互验 + 确定性窗口 == 全解密切片 |
| E2 | DecryptingTransport + open_read 门改造 | ✅ | `core/src/enc_stream.rs`：懒拉 header（首个 open_range）+ OnceLock 缓存 + 逐 chunk 验签；open_read 门拆分（aead_v2+range+size>0 → Stream 且 total_size=row.size；gcm/未知/无range/0B → Hydrate）；密码门与 WF0 次序不变 | 38689b2；红：enc_stream 4/6 + vfs_open_read 1 断言红（`must stream (K47)`）→ 绿；workspace 887/0/9（基线 867+20）、winfsp 92/0/1、clippy/fmt/check_layers/scan_secrets 全过 |
| E3 | 面测试 + 真机验收 + 文档收口 | ⬜ | — | — |

## 批次日志

- 2026-09-10：立项。PCFS 加密读路径查证（CTR 算术 seek 实时解密成立但无认证）；本仓 aead_v2 原语 `decrypt_range`/`decrypt_chunk` 已备；K47 裁决定稿（aead_v2 窗口化，不用 CTR）。计划与跟踪单落盘。
- 2026-09-10 E1：39b0313 窗口 API 红→绿（编译桩断言红）。设计点：`ciphertext_span` 取 (offset,length) 语义（open_range 直用）；exact-multiple 无空尾 chunk 与 `Layout::derive` 经真实容器互验；`AeadV2Window` Send+Sync 编译期钉。
- 2026-09-10 E2：38689b2 包装器+门改造红→绿（透传桩断言红）。既有加密钉测试全部存活（vfs_open_read 2/3/11、webdav fs_adapter 6e、smoke 3f——种子行未写 scheme，列默认 "gcm"，密码门先于门拆分）；测试修两处（mock upload 计数含种子、错误面在 open_range 调用点浮出），断言零漂移。
