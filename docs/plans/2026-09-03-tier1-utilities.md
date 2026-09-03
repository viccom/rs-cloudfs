# Tier-1 实用功能 Implementation Plan（push/pull、cache 子命令、Bot 命令、降级通知、config 调优键）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 在不改变任何现有默认行为与兼容契约的前提下，交付五个第一档实用功能：CLI `push`/`pull` 数据通道、`cache stats/clear` 子命令、Bot 命令 `/ls /mkdir /rm /quota /queue`、上传降级 bot 通知、config.toml 调优键（upload_workers/queue_capacity/hydrate_timeout）。

**Architecture:** 全部为纯增量改动，挂在既有接缝上：core `Vfs`/`MetaDatabase` 补三个内聚方法（create_dir/remove_file/cache_clear，语义镜像 webdav 层），upload_queue 降级臂补 best-effort `send_text`，bot.rs if/else 链追加分支，cli 子命令枚举追加 + vfs_config 映射。大文件上载走新增 `Vfs::ingest_file`（流式拷贝到缓存 staging 后 put_staged，全程不进内存）。

**Tech Stack:** Rust 2024 / tokio / clap(subcommand) / thiserror / rusqlite（bundled）。质量门禁照搬 M0：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace --no-fail-fast` 全绿。

**工作目录：** 本计划在 worktree `E:\GitHub\rs-CyDrive-t1`（分支 `feat/tier1-utilities`，自 main `c51d759` 切出）内执行。所有命令默认在该目录。

**TDD 委派纪律：** 每个 Task 先由「测试作者」子代理写红测试（只看本计划的契约节，不看实现），跑出红证据；再由「实现者」子代理实现（禁改测试断言），跑出绿证据；主会话核对红→绿证据与断言无漂移后收口 commit。

**行号锚点** 基于 main `c51d759` 的文件状态。

---

## 契约总表（测试作者的唯一事实来源）

### C1. `MetaDatabase::clear_cached_flags`（database.rs，`delete_file` :414 附近追加）

```rust
pub fn clear_cached_flags(&self) -> Result<u64, DbError>
```
- SQL：`UPDATE files SET is_cached = 0 WHERE is_dir = 0 AND is_cached = 1`，返回 `rows_changed`。
- 目录行（is_dir=1）的 is_cached 不动。

### C2. `VfsError` 新增两个 variant（vfs.rs :64 枚举尾部追加，thiserror 风格与既有一致）

```rust
#[error("path already exists: {0}")]
Exists(String),
#[error("parent path is missing or not a directory: {0}")]
ParentMissing(String),
```

### C3. `Vfs::create_dir`（vfs.rs，`requeue_pending` :507 附近追加）

```rust
pub fn create_dir(&self, rel: &RelPath) -> Result<(), VfsError>
```
语义（逐条镜像 `crates/cydrive-webdav/src/lib.rs:205` 的 `create_dir`，但不返回 FsError 而是本枚举）：
1. `rel.is_root()` → `Err(Exists("/"))`；
2. `db.get_file(rel)` 返回 Some（无论文件还是目录）→ `Err(Exists(rel))`；
3. 父目录检查：`rel.parent()` 为 None（一级目录，父为根）→ 通过；Some(parent) 时 `db.get_file(parent)` 必须是 Some 且 `is_dir==true`，否则 `Err(ParentMissing(parent))`；
4. upsert 目录行（webdav Python parity 同款字段）：`FileUpsert { size: 0, mtime: unix_now(), sha256: None, is_dir: true, telegram_msg_id: None, is_uploaded: true, is_cached: true, is_encrypted: false, chunk_count: 0, mime_type: None, rel_path/name/parent_dir 由 rel 推导 }`；
5. **不创建任何文件系统目录**（webdav 层同款；fs 目录由 put/hydrate 的建父目录逻辑惰性补）。
6. `unix_now()`：vfs.rs 内若已有同用途 helper 则复用，否则新增私有 `fn unix_now() -> f64`（SystemTime::now(). unix secs as f64）。

### C4. `Vfs::remove_file`（vfs.rs 同区追加）

```rust
pub async fn remove_file(&self, rel: &RelPath) -> Result<(), VfsError>
```
语义（镜像 `crates/cydrive-webdav/src/lib.rs:263` 的 `remove_file`）：
1. `db.get_file(rel)` None → `Err(NotFound(rel))`；行 `is_dir==true` → `Err(IsDirectory(rel))`；
2. `db.delete_file(rel.as_str())?`；
3. 尽力删缓存副本：`cache.local_path(rel)` 上 `tokio::fs::remove_file`，NotFound 静默，其他错误仅 `tracing::warn!`（不向上传播）；
4. **远端 Telegram 消息刻意不删**（Python parity，注释写明，同 webdav :274-276）。

### C5. `Vfs::cache_clear`（vfs.rs 同区追加）

```rust
pub fn cache_clear(&self) -> Result<u64, VfsError>
```
1. `self.cache.clear_all()` io 错误映射为 VfsError（若无合适既有 variant 则加 `#[error("cache io error: {0}")] CacheIo(String)`）；
2. 调 `db.clear_cached_flags()` 返回清除的行数（u64）。

### C6. `CyDriveConfig` 三个新键（config.rs）

| 字段 | TOML 键 | 类型 | 默认 | 校验（违者 `ConfigError::Invalid`，风格照既有规则） |
|---|---|---|---|---|
| `upload_workers` | `upload_workers` | `u32` | 2 | `1..=32` |
| `queue_capacity` | `queue_capacity` | `u32` | 256 | `>= upload_workers` 且 `<= 100_000` |
| `hydrate_timeout_secs` | `hydrate_timeout_secs` | `u64` | 180 | `1..=86_400` |

- 三个键加入 `KNOWN_TOML_KEYS`（config.rs :64）；serde default 函数命名 `default_upload_workers` 等，风格与既有字段一致。
- **legacy config.json 不接受这三个键**（维持现状，不进 legacy 已知键集）。
- `save_toml_scrubbed` 序列化自然包含新键（无需特判）。

### C7. `vfs_config` 映射（cli/src/lib.rs :224）

```rust
workers: cfg.upload_workers as usize,          // 字段类型以 VfsConfig 实际为准
queue_capacity: cfg.queue_capacity as usize,
hydrate_timeout: Duration::from_secs(cfg.hydrate_timeout_secs),
```
（其余字段保持不变。）

### C8. Bot 命令（core/src/bot.rs :62 `handle_command` 的 if/else 链）

- HELP_TEXT（:help 文本）追加 5 行：`/ls [path]`、`/mkdir <path>`、`/rm <path>`、`/quota`、`/queue`。
- 匹配沿用既有 `starts_with` 风格（含其怪癖，与 /get 一致）；追加分支插在未知 fallthrough 之前。
- 参数解析：`text.split_whitespace()`，与 /search /get 同款。

| 命令 | 行为 | 回复文本（新文本，无基线） |
|---|---|---|
| `/ls [path]` | path 缺省 `/`。先 `db.get_file`：存在且非目录 → 单行回复该文件；是目录或 path 是目录 → `db.list_dir(path)` 逐项；目录行不存在 → `no such directory: {path}` | 条目格式：目录 `d {name}/`，文件 `f {name} ({size/1024} KB)`（KB 整除，与基线风格一致）；上限 20 行，超出加 `… and {n} more`；空目录回复 `(empty)`；首行 `📁 {path}` |
| `/mkdir <path>` | `vfs.create_dir(rel)` | 成功 `created: {path}`；Exists → `already exists: {path}`；ParentMissing → `parent missing: {path}`；缺参数 → `usage: /mkdir <path>` |
| `/rm <path>` | `vfs.remove_file(rel).await` | 成功 `deleted: {path}`（注明远端消息保留）；NotFound → `no such file: {path}`；IsDirectory → `is a directory: {path}`；缺参数 → `usage: /rm <path>` |
| `/quota` | `db.get_stats()` + `vfs.cache_*`？——**不行：bot.rs 拿不到 CacheManager**。改为 `db.get_stats()` 的 total_bytes/total_files/total_dirs + pending_uploads | 首行 `💾 /{drive_letter}`，`files: {n} ({total_bytes/1024/1024} MB)`、`dirs: {n}`、`pending uploads: {n}`（MB = `(total_bytes / (1024*1024))` 整除，同 /stats 风格） |
| `/queue` | `vfs.queue_stats()`（QueueStats 字段 enqueued/succeeded/retries/degraded/pending，:172 附近） | `queue: enqueued={n} succeeded={n} retries={n} degraded={n} pending={n}` |

- 注意：handle_command 签名不变（db/vfs/transport/drive_letter/text 都已具备）。

### C9. 上传降级通知（core/src/upload_queue.rs :578 `RetryDecision::Degrade` 臂）

- 在 `bump(&stats.degraded);` 之后、`return;` 之前插入 best-effort 通知：
```rust
let notice = format!(
    "⚠️ CyDrive: upload failed after {n} attempts: {rel} — kept on disk, will retry on next start",
    n = consecutive_failures, rel = job.rel_path,
);
if let Err(notify_error) = transport.send_text(&notice).await {
    tracing::warn!(%notify_error, rel_path = %job.rel_path, "degrade notification failed");
}
```
- 无配置开关（决策：降级是罕见终态，通知恒开；记录 decisions.md）。
- `send_text` 失败绝不影响降级路径的完成（worker 正常 return）。

### C10. `Vfs::ingest_file`（vfs.rs，push 的核心；大文件不进内存）

```rust
pub async fn ingest_file(&self, rel: &RelPath, source: &Path, mtime: f64) -> Result<u64, VfsError>
```
1. `tokio::fs::metadata(source)` 取 size（u64）——源文件必须存在；
2. staged 路径 = 缓存根下 `rel` 的 sibling tmp 名（与 webdav `staged_sibling` 同规则：`.{name}.tmp`，位于最终缓存目录内；用 `self.cache.local_path(rel)` 的 parent 拼出）；
3. `tokio::fs::copy(source, staged)`（流式，无内存整载）；
4. `self.put_staged(rel, &staged, mtime).await?`（内部建父目录 + 原子 rename + pending 行 + 入队）；
5. 返回 size。
- 失败路径：copy 或 put_staged 失败时尽力删 staged（put_staged 自身失败已在 put 侧清 staged——若实测发现双清无害则保留兜底）。
- **先决条件不检查祖先目录行**：祖先目录行由调用方（CLI push）负责创建（用 C3）。这与 put_staged 现有语义一致（它只建 fs 目录）。

### C11. CLI 子命令（cli/src/main.rs `Command` 枚举 :31 + cli/src/lib.rs 实现）

```rust
/// Upload a local file into the drive (bypasses the 4 GB WebClient and
/// 1900 MB Web UI limits; uploads are chunked automatically).
Push {
    /// Local file to upload.
    path: PathBuf,
    /// Destination path inside the drive (default: /<file name>).
    #[arg(long)]
    dest: Option<String>,
},
/// Download a drive file to a local path (hydrates from Telegram when
/// not cached).
Pull {
    /// Path inside the drive.
    path: String,
    /// Local destination (file path or existing directory).
    out: PathBuf,
},
/// Inspect or clear the local disk cache.
Cache {
    #[command(subcommand)]
    action: CacheAction,
},
// ...
enum CacheAction {
    /// Print cache root, used bytes and the configured limit.
    Stats,
    /// Delete every cached file and clear is_cached flags (uploaded data
    /// stays in Telegram; pending uploads keep their local copy).
    Clear,
}
```

- 库层可测函数（cli/src/lib.rs，与 run_with_transport 同区）：
  - `pub async fn connect_stack(cfg: &CyDriveConfig) -> Result<Stack>`：镜像 run() 的装配（fail-fast connect → db → cache → vfs），**不起** WebDAV/WebUI/mount/inbound/requeue；`Stack { db, vfs, transport }`，`shutdown()` = `vfs.shutdown()`（排干队列）。
  - `pub async fn push_file(vfs: &Vfs, local: &Path, dest: &RelPath) -> Result<u64>`：校验 dest 非根；自深向浅为缺失祖先补 `vfs.create_dir`（Exists 静默吞掉）；`vfs.ingest_file`。
  - `pub async fn pull_file(vfs: &Vfs, rel: &RelPath, out: &Path) -> Result<PathBuf>`：`vfs.hydrate` → out 是已存在目录则 `out.join(rel.name())`，否则 out 即目标；`tokio::fs::copy`（若 out 已存在且是文件则覆盖）。
  - `pub fn cache_stats(cfg: &CyDriveConfig) -> Result<()>` / `pub fn cache_clear_cmd(cfg: &CyDriveConfig) -> Result<()>`：打开 db + CacheManager（不开 transport）；stats 打印 root/used/limit（人类可读，复用 stats_cmd 的容量格式化若可复用）；clear = `cache.clear_all()` + `db.clear_cached_flags()`，打印 freed 字节（clear 前先 total_size）与清掉的标志行数。
- push 主流程（main.rs `push_cmd`）：connect_stack → push_file → `stack.shutdown().await`（排干直到该文件到终态）→ 读 `db.get_file(dest)`：`is_uploaded==true` → `uploaded: {dest} ({size_human})`；否则 `queued but not uploaded yet (degraded or still pending); will retry on next run`。
- pull 主流程：connect_stack → pull_file → 打印 `pulled: {rel} -> {out} ({bytes} bytes)` → shutdown（无在途任务，立即返回）。
- `CYDRIVE_*` env 与凭据回填走既有 `discover_config()`，无新键。

---

## 任务分解（每任务红→绿→commit）

### Task 1: core 存储操作（C1–C5）

**Files:**
- Modify: `crates/cydrive-core/src/vfs.rs`（VfsError 两 variant + create_dir/remove_file/cache_clear/unix_now）
- Modify: `crates/cydrive-core/src/database.rs`（clear_cached_flags）
- Test: `crates/cydrive-core/tests/vfs_ops.rs`（新文件）

**Step 1（红）** 测试作者写 `tests/vfs_ops.rs`，用例：
- `create_dir_happy_path_creates_dir_row`（根下建目录 → get_file 断言 is_dir/size=0/is_uploaded/is_cached；再 list_dir("/") 含该行）
- `create_dir_nested_requires_parent_dir_row`（父不存在 → ParentMissing；父是文件 → ParentMissing）
- `create_dir_rejects_root_and_existing`（root → Exists；同名已有文件行 → Exists；同名目录行 → Exists）
- `remove_file_deletes_row_and_cache_copy`（put 小文件 → 缓存文件存在 → remove_file → get_file None + 缓存文件不存在）
- `remove_file_not_found_and_is_directory`
- `cache_clear_empties_tree_and_clears_file_flags_only`（put 两个文件 + 建一个目录行 → cache_clear → 缓存根无文件残留、返回值 == 2、两文件行 is_cached=0、目录行 is_cached 仍=1）
- `ingest_file_streams_into_queue_without_reading_into_memory`（用 ≥1MB 源文件 ingest → Mock 收到 upload、行 is_uploaded、缓存副本被成功后删除、返回值 == 源 size）——注意此用例需要 MockTransport + Vfs::new 装配（照抄 tests/vfs.rs 现有装配模式）
- `ingest_file_missing_source_errors_and_leaves_no_staging`

跑 `cargo test -p cydrive-core --test vfs_ops` → **预期编译失败（方法不存在）**，记录红证据。

**Step 2（绿）** 实现者按 C1–C5、C10 实现最小实现，复跑 → 全绿；再跑 `cargo test -p cydrive-core` 全绿（无回归）。

**Step 3（commit）**
```bash
git add crates/cydrive-core
git commit -m "feat(core): vfs create_dir/remove_file/cache_clear/ingest_file + db clear_cached_flags"
```

### Task 2: config 调优键（C6）

**Files:**
- Modify: `crates/cydrive-core/src/config.rs`
- Test: `crates/cydrive-core/tests/config.rs`（追加用例）

**Step 1（红）** 追加用例（照该文件既有风格）：
- `toml_new_tuning_keys_parse_with_defaults_when_absent`（无三键 → 默认 2/256/180）
- `toml_new_tuning_keys_roundtrip`（显式值解析一致）
- `toml_upload_workers_out_of_range_rejected`（0 与 33 → Invalid）
- `toml_queue_capacity_below_workers_rejected`（capacity=1, workers=2 → Invalid）
- `toml_hydrate_timeout_zero_rejected`
- `legacy_json_unknown_new_key_rejected`（json 里塞 upload_workers → 维持 legacy 键集，报错）

跑 → 红（字段不存在编译失败或解析拒绝）。

**Step 2（绿）** 按 C6 实现；复跑本文件 + `cargo test -p cydrive-core --test config` 全绿。

**Step 3（commit）**
```bash
git commit -am "feat(core): expose upload_workers/queue_capacity/hydrate_timeout config keys"
```

### Task 3: Bot 命令 + 降级通知（C8–C9）

**Files:**
- Modify: `crates/cydrive-core/src/bot.rs`、`crates/cydrive-core/src/upload_queue.rs`
- Test: `crates/cydrive-core/tests/bot.rs`（追加）、`crates/cydrive-core/tests/upload_queue.rs`（追加）

**Step 1（红）** bot.rs 追加（照既有装配：内存 db + Vfs + MockTransport）：
- `help_text_lists_new_commands`（/help 回复含 "/ls"、"/quota"、"/queue"）
- `ls_root_lists_entries_with_cap`（建 25 个文件行 → 首行 `📁 /`、条目 20 行、末行 `… and 5 more`；文件条目格式 `f {name} ({kb} KB)`、目录 `d {name}/`）
- `ls_specific_file_replies_single_line`；`ls_missing_dir_errors`
- `mkdir_creates_and_reports_exists`（两次 mkdir 同路径第二次 already exists）
- `rm_deletes_file_and_mentions_remote_kept`（put→rm→get_file None；回复含 "deleted" 与 remote 相关字样）；`rm_missing_errors`；`rm_directory_errors`
- `quota_reports_counts_and_bytes`；`queue_reports_counters`（enqueue 若干 job 后断言 counters 行）
upload_queue.rs 追加：
- `degrade_sends_bot_notification`（upload_script 全 Fail 到降级 → `mock.sent_texts()` 恰含一条含 rel_path 与 "failed" 的通知）
- `degrade_notification_failure_is_swallowed`（测试本地包一层 send_text 恒 Err 的 transport 装饰器 → 降级流程照常完成、degraded 计数=1、无 panic）

跑对应 --test → 红。

**Step 2（绿）** 按 C8–C9 实现；复跑两个测试文件 + `cargo test -p cydrive-core` 全绿（既有 bot/queue 用例零改动零回归）。

**Step 3（commit）**
```bash
git commit -am "feat(core): bot /ls /mkdir /rm /quota /queue + degrade notification"
```

### Task 4: CLI 接线（C7 + C11）

**Files:**
- Modify: `crates/cydrive-cli/src/lib.rs`（vfs_config 映射 + Stack/push_file/pull_file/cache_*）、`crates/cydrive-cli/src/main.rs`（Command 枚举 + 三个 handler）
- Test: `crates/cydrive-cli/tests/data_channel.rs`（新文件）、`crates/cydrive-cli/tests/ops.rs`（vfs_config 映射用例若更贴切可放此处）

**Step 1（红）** data_channel.rs（MockTransport 装配照 run_e2e.rs 模式，**不起真 transport**——直接构造 Stack 的测试路径：把 Stack 字段或构造函数设计成测试可见，或对 push_file/pull_file 直接以 Vfs 为参测试）：
- `push_file_enqueues_and_uploads_to_mock`（push 小文件 → message_names 含 dest、行 is_uploaded、返回 size）
- `push_file_creates_missing_ancestor_rows`（dest=`/a/b/c.txt` → list_dir("/") 含 a、list_dir("/a") 含 b，均 is_dir）
- `push_file_missing_source_errors`
- `pull_file_roundtrips_bytes`（push → 删缓存副本模拟冷缓存 → pull → 目标文件字节 == 原始）
- `pull_file_into_existing_directory_uses_file_name`
- `pull_file_missing_rel_errors`
- `cache_clear_frees_files_and_flags`（put 两文件 → cache_clear_cmd 逻辑（函数级）→ 缓存空、行 is_cached=0）
- `vfs_config_maps_new_tuning_keys`（cfg 三键 → VfsConfig 对应字段）
跑 → 红。

**Step 2（绿）** 按 C7/C11 实现；`cargo test -p cydrive-cli` 全绿 + `cargo build -p cydrive-cli` 产出 bin；手工冒烟 `cydrive.exe push --help` / `pull --help` / `cache --help` 输出正常（真机 push 冒烟留人工项）。

**Step 3（commit）**
```bash
git commit -am "feat(cli): push/pull data channel + cache stats/clear subcommands + tuning key wiring"
```

### Task 5: 全量门禁 + 文档收口

**Step 1:** 依次跑（全部必须绿/无输出）：
```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
```
**Step 2:** `docs/decisions.md` 追加一条：降级通知恒开（无开关）+ `/rm` 沿用「远端消息不删」基线 + legacy json 不收新键的裁决。
**Step 3:** 仓库 `AGENTS.md` 「当前阶段」追加 tier-1 交付摘要（测试计数更新）。
**Step 4:**
```bash
git commit -am "docs: tier-1 utilities done (push/pull, cache cmds, bot cmds, degrade notify, tuning keys)"
```

---

## 修订 A1（2026-09-03，主会话裁决）：cache clear 保护 pending 上传

Task 1 实现者发现：C5 原文会让 `cache_clear` 连 **pending 上传的本地 staging 副本**一起删——那是未上传数据的唯一副本，属数据丢失风险，与北极星「稳定」冲突。修订如下：

- **C5'** `Vfs::cache_clear` 语义改为：只删除 `is_uploaded = 1` 行对应的缓存副本，并只对这些行清 `is_cached`；`is_uploaded = 0`（pending）行的本地副本与标志**原样保留**。实现路径：
  - `MetaDatabase::clear_cached_flags` 的 SQL 追加 `AND is_uploaded = 1`（C1 修订）；
  - `MetaDatabase::pending_file_paths() -> Result<Vec<String>, DbError>`（新增，`SELECT rel_path FROM files WHERE is_uploaded = 0 AND is_dir = 0`）；
  - `CacheManager::clear_except(&self, keep: &[RelPath]) -> io::Result<()>`（cache.rs 新增，镜像 clear_all 但跳过 keep 中的路径；目录照旧清理）；
  - `Vfs::cache_clear` = pending_file_paths → cache.clear_except → clear_cached_flags，返回值仍为清除的标志行数。
- **Task 1 测试 6 改写（新红）**：文件 A 走 put+排干（uploaded）后 hydrate 回缓存（is_cached=1）→ 文件 B 仅 put 不排干（pending）→ `cache_clear` → 返回 1；A 的缓存副本被删、is_cached=0；B 的缓存副本仍在、is_cached 仍 1、行完好；随后 shutdown 排干 B 仍能正常上传成功。
- **C11 修订**：`CacheAction::Clear` docstring 改为 "Delete cached copies of uploaded files; pending-upload staging copies are preserved."
- decisions.md 记录该裁决（Task 5）。

## 明确不做（YAGNI / 边界）

- 递归 push/pull 目录、push 进度条、pull 断点续传——不做。
- 远端删除联动、rescan——第二档，不在本计划。
- `CYDRIVE_UPLOAD_WORKERS` 等 env 覆盖——不做（只开 TOML）。
- legacy config.json 的新键支持——不做（新键是 Rust 时代扩展）。
- bot /rm 删目录、递归删——不做（只删文件）。

## 风险与回滚

- 全部改动为增量，不触碰 hydrate/put/队列既有路径的核心断言；任何回归以 `cargo test --workspace` 为准绳，回滚 = worktree 分支整体废弃（main 无感）。
