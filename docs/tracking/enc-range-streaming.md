# Phase 3.5-a 加密 Range 流式读 任务跟踪单

> 计划：docs/plans/2026-09-10-enc-range-streaming.md ｜ 裁决 K47 ｜ 参照：PCFS Explore 报告（会话 2026-09-10）
> worktree：`feat/enc-range`（收口 merge 回 main）。

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| E1 | crypto 公开窗口解密 API | ⬜ | — | — |
| E2 | DecryptingTransport + open_read 门改造 | ⬜ | — | — |
| E3 | 面测试 + 真机验收 + 文档收口 | ⬜ | — | — |

## 批次日志

- 2026-09-10：立项。PCFS 加密读路径查证（CTR 算术 seek 实时解密成立但无认证）；本仓 aead_v2 原语 `decrypt_range`/`decrypt_chunk` 已备；K47 裁决定稿（aead_v2 窗口化，不用 CTR）。计划与跟踪单落盘。
