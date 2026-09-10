# Phase 3 WinFsp 任务跟踪单

> 计划：docs/plans/2026-09-10-phase3-winfsp.md ｜ 裁决 K38-K46 ｜ 研究：docs/reports/2026-09-10-rclone-vfs-winfsp-study.md
> worktree：`feat/winfsp`（收口 merge 回 main）。

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| WF0 | cache-first（流式读缓存优先） | ✅ | `open_read` 在密码门之后、K33 三重门之前加 `cache.is_cached(rel)` 探针 → 命中返回 `Hydrate`（webdav/api 两面经既有 hydrate 腿本地直供 + record_access 续 LRU；`open_read` 自身保持纯路由：不水合、不 record_access、不驱逐）。语义红线原样：加密行+无密码仍 `MissingPassword`（密码门在探针前）、未缓存明文仍 `Stream`、加密行/无 range/0 字节仍 `Hydrate`。测试：core 4 例（cached→Hydrate 零远端、uncached→Stream、encrypted+cached→MissingPassword、K34「cached still streams」条款随 K42 改判）+ webdav fs_adapter 6g（缓存命中响应体来自本地、transport 零调用） | 红：`cargo test -p cloudkit-core --test vfs_open_read` → `FAILED. 9 passed; 2 failed`，原文 `a cached plaintext row serves locally (WF0 cache-first)` / `cached row falls back to the local plaintext (WF0 cache-first)`；`cargo test -p cloudkit-webdav --test fs_adapter` → `FAILED. 28 passed; 1 failed`，served bytes = 远端 pattern（非本地）。绿：`cargo test --workspace --no-fail-fast` → **845 passed / 0 failed / 9 ignored**（841 基线 + 4 新增）；clippy/fmt/check_layers/scan_secrets 全过 |
| WF1 | cloudkit-winfsp 骨架+async 桥+只读元数据面 | ⬜ | — | — |
| WF2 | 读路径（窗口模型+宽限期） | ⬜ | — | — |
| WF3 | 写路径（staged）+fs 操作面 | ⬜ | — | — |
| WF4 | 挂载生命周期+cli 集成 | ⬜ | — | — |
| WF5 | 真机验收+收口 merge/push | ⬜ | — | — |

## 批次日志

- 2026-09-10：立项。研究入档（rclone 三路深研报告）；K38-K46 裁决定稿；计划与跟踪单落盘。待负责人批准开工。
- 2026-09-10：WF0 完成（cache-first，K42 落地）——`open_read` 密码门后加 `is_cached` 探针，命中返回 `Hydrate`，本地直供与 LRU 续期由 hydrate 腿承接；K34 测试条款「cached row still streams」随 K42 改判（仅该腿，冷行「不进缓存/不水合」条款保留）。红→绿留证，全门禁绿（845/0/9）。
