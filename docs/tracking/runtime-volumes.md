# Phase 3.6 运行态卷管理 任务跟踪单

> 计划：docs/plans/2026-09-10-runtime-volumes.md ｜ 裁决 K48–K51 ｜ worktree：`feat/runtime-volumes`（收口 merge 回 main）。
> 基线：main@5927a25（workspace 911/0/12 ignored——含 telegram 真网 E2E 3 个；winfsp 腿 117/0/1）。2026-09-11 复核：§0 六接缝经 RB1-RB4/telegram E2E/Low 清尾后全部成立（serve_volumes 精确化至 webdav server.rs:95）；兼容表述已按 1.0 前不兼容政策清除。

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| RV0 | 卷级 enabled 键（发现期跳过） | ⬜ | — | — |
| RV1 | 注册表动态化 + WebDAV/仪表盘动态分发 | ⬜ | — | — |
| RV2 | 控制通道 ADD/REMOVE/LIST + 卸载安全序 | ⬜ | — | — |
| RV3 | 真机验收 + 文档收口 + merge/push | ⬜ | — | — |

## 批次日志

- 2026-09-10：立项。负责人批准动态加载/卸载方向（`enabled` 键此前已批准暂缓，合流本批为 RV0）；接缝侦察（VolumeRegistry 启动期 Vec/serve_volumes 一次性路由/控制通道仅 STOP）；K48–K51 裁决定稿（显式控制命令、REMOVE 不碰卷文件、卸载安全序中止不半卸、共享注册表三面同源）；计划与跟踪单落盘。待负责人批准开工。
