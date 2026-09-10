# 流式读 任务跟踪单

> 计划：docs/plans/2026-09-10-streaming-reads.md ｜ 裁决 K33–K37
> worktree：`feat/streaming-reads`（收口 merge 回 main）。

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| SR0 | MockTransport 观测面 + RemoteHandle helper + Vfs::open_read 接缝 | ⬜ | — | — |
| SR1 | WebDAV RangeFile + 窗口聚合 + 三重门分派 | ⬜ | — | — |
| SR2 | /api/download 流式 Body + 头语义 | ⬜ | — | — |
| SR3 | 真机验收（PotPlayer/仪表盘/前端）+ 收口 | ⬜ | — | — |

## 批次日志

- 2026-09-10：立项。三轮研究完成（PCFS/本仓接缝/互联网同类），K33–K37 裁决定稿，计划与跟踪单落盘。
