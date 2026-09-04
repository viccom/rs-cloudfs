# Tier-1 代码审查修复计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 修复 2026-09-03 深度代码审查的可修项：H1 连接死线、H2 pending 删除保护、M1/M2 DRY 收敛、三项 Low，全程 TDD、逐项复核。

**工作目录：** worktree `E:\GitHub\rs-CyDrive-t1`（分支 `feat/tier1-utilities` 续跑，修复 commit 追加在同一分支）。

**明确不修（记录于 decisions.md）：** M3 create_dir TOCTOU（webdav 既有同款模式，需事务设计，单服务并发面极小）；M4 持久化失败降级不通知（该路径消息语义不同——上传已成功，不能复用 "upload failed" 文案）；L 锁文件中断的 cache clear 继续语义（行为变更，现中止态可恢复）；L unix_now 第三份拷贝（已注释自认）；L /ls startswith 前缀怪癖（基线忠实）；L 一次性命令无 tracing subscriber（println 已补偿）。

---

## F1（H1 + M2）：connect 死线 + TransportConfig 去重

**契约：**
- `pub fn transport_config_from(cfg: &CyDriveConfig, cwd: &Path) -> TransportConfig`（cli lib.rs）：从 cfg 组装 TransportConfig（api 常量、session 路径 `{DEFAULT_SESSION_STEM}.session` 挂 cwd、proxy_url）。`main.rs run()` 与 `connect_stack` 均改用之。
- `pub async fn connect_stack_with_deadline(cfg: &CyDriveConfig, deadline: Duration) -> Result<Stack>`：connect 段用既有 `connect_with_deadline` 包裹（90s 由 `connect_stack` 默认传入；连接失败沿 run() 的人话诊断路径）。
- **红测试**（cli tests，新文件 `connect_deadline.rs`）：本地起一个「接受 TCP 但永不回复」的 TcpListener 作为假 SOCKS5 代理，cfg.proxy_url 指向它、bot_token="1:AAAA..."（形合法）、chat_id=1；`connect_stack_with_deadline(&cfg, Duration::from_secs(2))` 必须在 ~10s 内返回 Err（死线胜出，而不是 90s/永久挂起），错误信息含诊断指引特征（照 connect_guard.rs 既有断言风格）。
- 绿后：connect_guard 既有用例 + data_channel 全绿。

## F2（H2）：pending 上传删除保护（三面一致）

**契约（细化裁决）：拒绝删除当且仅当「pending 且本地缓存副本仍在」**——副本已消失的幽灵 pending 行（字节既不在本地也不在远端）必须**允许**删除，否则永远清不掉。

- core `vfs.rs`：`VfsError` 增 `#[error("upload still pending: {0}")] UploadPending(String)`；`remove_file` 在行存在、非目录、`!is_uploaded` 且 `cache.local_path(rel)` 文件存在时返回 `UploadPending`（副本不存在则放行——幽灵行清理路径）。
- bot `bot.rs` `/rm`：`UploadPending` → 回复 `still uploading, try again after it finishes: {token}`。
- webdav 适配层 `remove_file`（自有实现，不走 Vfs）：同判定 → `FsError::Forbidden`（副本消失放行）。
- web `/api/delete`：同判定 → 409 Conflict + 正文提示（照该路由既有错误正文风格）。
- **红测试**：core `remove_file_refuses_pending_with_local_copy`（put 不排干 → remove → Err(UploadPending)、行在、副本在）+ `remove_file_allows_ghost_pending_row`（删掉副本后 remove → Ok、行没了）；bot `rm_pending_upload_replies_still_uploading`；webdav（fs_adapter 或 smoke）`delete_pending_upload_forbidden`；web `delete_pending_upload_conflict`。

## F3（M1）：cache clear 语义单点化（纯重构，无新红）

- core `vfs.rs` 增 `pub fn clear_cache_preserving_pending(db: &MetaDatabase, cache: &CacheManager) -> Result<u64, VfsError>`（搬入现 Vfs::cache_clear 的 pending→keep→clear_except→flags 主体）。
- `Vfs::cache_clear` 与 cli `cache_clear_cmd` 均委托之（cli 保留自己的 before/after total_size 计算 freed）。
- 验证 = 现有两组测试（vfs_ops、data_channel）保绿，无行为变化；diff 审查确认两处旧副本删除。

## F4（Low 三项）

- `push_file` 前置门禁：source 是目录 → `bail!("the source is a directory; push uploads single files: {path}")`。红测试：push 目录 → Err 且 DB 无行无目录行。
- `push_cmd` dest 解析失败提示追加 `drive paths start with "/"`（改 with_context 文案；不新增测试，行走既有 invalid 路径）。
- 补测试 `pull_file_overwrites_existing_file`（out 为已存在文件 → 覆盖且字节正确）。

## F5：门禁 + 入档

`cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace --no-fail-fast` 全绿；decisions.md 记 H2 裁决（含幽灵行细化）与不修清单；AGENTS.md tier-1 段补一行修复记录。

## F6：真机 redeploy 回归

重建 `target/debug/cydrive.exe` → 停在跑服务（旧 tier-1 二进制）→ 覆盖 `D:\Tools\rs-CyDrive\cydrive.exe` → 重启服务 → 真机快速回归：`/api/stats` 200、PROPFIND 207、Y: 在线、push 小文件 + cache stats 正常、pending 保护冒烟（push 后立即 pull 行内验证太重，改为跑 F2 的真实单测为准 + 服务日志无错）。
