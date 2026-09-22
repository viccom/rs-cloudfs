# Phase 8：read-through 按需逐层索引 实施计划

> **For Claude:** REQUIRED SUB-SKILL: Use executing-plans to implement this plan task-by-task.
> **负责人批准令**：2026-09-22 负责人拍板路线 C（B 为主 + A 最小集），三点要求写进本计划：①先出详细可行方案与计划（本文件）；②机制必须提炼为六后端 + 未来驱动共享的共性行为（§3）；③必须证明是架构增强而非破坏（§7）。
> **状态**：计划已写，待开工（批次 RT0–RT5 顺序执行，worktree `feat/readthrough-index` 独立 target——共享 CARGO_TARGET_DIR 双指纹既有教训）。
> **编号**：裁决入 decisions.md 用 **K83**（K82 = Phase 7 合入批已占）；阶段命名 **Phase 8**；批次代号 **RT0–RT5**。

**Goal:** 读路径从「本地 db 闸门（miss 即 404）」改为「按需逐层回源 + 物化缓存」（RaiDrive 式体验），rebuild 从「唯一索引手段」降级为「可续跑/有界/prune 的全量校对工具」。

**Architecture:** 一条 `CloudTransport::as_driver` 探针（`as_inbound`/`as_chat` 同款先例）把 `Arc<dyn StorageDriver>` 宽面从类型擦除的窄面里取回；`Vfs` 新增 `read_dir_fresh`/`stat_fresh` 两个共性入口，miss/TTL 过期时经 `StorageDriver::list/stat` 回源 → 复用 rebuild 的 Entry→行物化映射 → 落 `files` 表当缓存。四消费面（网关/仪表盘/winfs/bot）只换数据获取函数，**零驱动代码**。rebuild 重构为迭代工作队列 + 持久化续跑 + 总上限 + 完成趟 sweep prune。

**Tech Stack:** 现有栈零新增依赖（rusqlite/tokio/async_trait/dav-server/winfsp 全不变）；新增面 = `cloudkit-storage`（探针）+ `cloudkit-core`（readthrough 模块 + materialize 提取 + rebuild_state 表）+ 三消费面接线。

---

## 0. 需求分析（requirement-analyzer）

### 需求概述

用户访问哪层就自动索引哪层（回源现查 + 本地缓存），消灭「手动 rebuild + 全量遍历百万文件」的双病灶；六权威后端（baidu/local/sftp/pan115/pan123/webdav）共享同一套机制，未来驱动零成本继承。

### 功能拆解

#### 后端任务（共性核心）
- [ ] L2 探针面：`CloudTransport::as_driver() -> Option<&dyn StorageDriver>`（默认 None；六宽面 transport_face 各一行 Some）——领域语义=窄面到宽面的逃生门
- [ ] L3 物化共性：`materialize.rs`（自 `rebuild.rs::upsert_entry` 提取的**唯一** Entry→行映射）+ `list_all_pages` 分页归集
- [ ] L3 读穿原语：`readthrough.rs`——`read_dir_fresh`（逐层强制 revalidate）/`stat_fresh`（父目录 TTL 窗 + 深路径 1 次 list 兜底）/reconcile（upsert + stat 双确认 prune）/DirCache（TTL+单飞+就近失效）
- [ ] L3 rebuild 校对工具化：迭代工作队列 + `rebuild_state` KV 表持久化续跑 + `max_entries` 总上限 + 完成趟 sweep prune（`updated_at < scan_started` 保护跨续跑）
- [ ] 加密卷显式拒收（与 rebuild 同款闸门，文案指路 sync）

#### 消费面任务（接线，非重写）
- [ ] 网关 CyDriveFs：`metadata`/`read_dir`/`open` 读臂三处改调 `*_fresh`
- [ ] WinFsp：`meta_for`/`dir_entries`（marker=None 强制）/`open_with_read` 三处经 `bridge.block_on` 改调
- [ ] Web API：`api_list` 改调 `read_dir_fresh`（`api_files` 保持索引视图，见 D9）
- [ ] bot `/ls`：**零改动**（telegram 无宽面 → 降级臂 = 现行为，见 D2）

### 技术方案

#### 核心架构

```
Explorer/Y: ──PROPFIND──▶ CyDriveFs ─┐
仪表盘 /api/list ─────────▶ api_list ─┼─▶ Vfs::read_dir_fresh / stat_fresh
WinFsp FSD 回调 ──block_on─▶ meta_for ─┘         │
                                     miss/TTL过期 │ as_driver() + authoritative_index 门
                                                  ▼
                                    StorageDriver::list/stat（回源，六后端统一契约）
                                                  │
                                    materialize::materialize_entry（唯一映射）
                                                  ▼
                                        files 表 = 缓存 + 上传队列 + sync 共用
```

**门禁语义（D2）**：`caps.authoritative_index && as_driver().is_some()` 才回源；否则读穿入口逐字退化为今日的 `db.list_dir`/`db.get_file`（telegram 卷零行为变化）。`authoritative_index` 能力位从「启动横幅独占」升格为第一个真实消费者（R4 能力诚实的兑现）。

#### 数据模型

- **零 `files` DDL 变更**（R6 Python 契约冻结）：sweep 判据复用既有 `updated_at` 列（扫描 upsert 自然刷新 → 跨续跑保护，无需 scan_id 列）。
- 新增一张 KV 表 `rebuild_state(key TEXT PRIMARY KEY, value TEXT)`（Rust 自加表，`sync_mirror`/`sync_state` 同先例）：`pending`（JSON 目录队列）/`scan_started_at`/`entries_done`。
- reconcile 的删除判据 = sync.rs:579-580 的 in-flight 判定原文复用：`!is_uploaded && cache.local_path().exists()` 的行**两侧都不碰**（upsert 侧跳过、prune 侧跳过）。

#### 安全与性能

- **删除永不基于单次观测（D7）**：候选删除逐条 `driver.stat` 双确认（NotFound 才删），单目录上限 32 条，超限整批跳过删除 + 告警（疑似列表性故障，权威清账走 rebuild 完成趟 sweep）。
- **单飞 + TTL（D5/D6）**：同目录并发回源归并为一次；`stat` 风暴靠父目录 5s 窗收敛成一次 list。
- **AList 上游放大（用户实测场景）**：每层每视图恰一次 list（winfs marker=None 强制刷新但每个枚举只发生一次——K44 枚举快照语义兜着），永不做递归。
- 注册表锁纪律不变：回源 IO 永不在 `lookup`/`volume`/`find` 表方法内发生（RV1「锁不跨 await」自动满足）。

### 验收标准

- [ ] A1（用户场景复现）：空索引 webdav 卷挂 Y:，Explorer 逐层进入即见远端内容，**零 rebuild**；根见 8 目录、进子层见子层
- [ ] A2（对照 RaiDrive）：RaiDrive J: 盘增/删文件 → Y: 重进该目录或刷新后一致（删除经 stat 双确认）
- [ ] A3（规模）：任一层访问的网络调用数 = O(1)/层（list 1 次；stat 风暴归并 1 次父 list）；百万文件卷浏览不触发全树遍历（驱动调用计数断言）
- [ ] A4（rebuild 续跑）：扫描中途中断 → rerun 不重扫已完成目录（`driver.list` 调用计数断言）；`max_entries` 到顶优雅停 + 明示「rerun to continue」
- [ ] A5（rebuild sweep）：完整趟后只删「扫描期间未再触碰且 `is_uploaded=1`」的行；in-flight 行、本趟 upsert 行、跨续跑早趟 upsert 行全部无伤（红测试钉死）
- [ ] A6（一致性）：六后端同一 Vfs seam 测试矩阵绿；telegram 卷全链路行为逐字不变（既有断言零漂移）
- [ ] A7（加密卷）：显式拒收 + 可行动文案（指路 sync），不物化半错语义行
- [ ] A8（门禁）：workspace 全绿 + clippy -D warnings + fmt + check_layers + scan_secrets + 裁剪组合构建绿

### 风险评估

| 风险 | 应对 |
|---|---|
| winfs FSD 线程被慢回源阻塞（bridge 无超时是 rclone 教训既定形态） | 回源只发生在 marker=None 枚举与 miss 路径；单飞+TTL 归并风暴；驱动层自身超时是硬顶 |
| 谎报/残缺列表被当真 → 误删行 | 无错不删 + 逐条 stat 双确认 + 32 条上限整批跳过（D7）；权威清账另有 rebuild sweep |
| 上游（AList 七挂载）瞬断 → 列表空洞 | 同上 + `Err → stale-if-error`（有旧缓存照常服务，绝不因瞬态错清库） |
| reconcile 与上传队列竞态 | in-flight 判定两侧跳过（sync.rs 既有语义）；upload persist 后 updated_at 自然保护 |
| sweep tombstone 风暴经 sync 波及同伙 | sweep 判据保守（3 重保护）；同伙 ghost 规则本就跳过 pending；真删 = 远端确实没了，语义正确 |
| api_files 全表视图语义 | 明确保持「索引视图」（D9），文件页浏览走 api_list 才是新鲜面 |

---

## 1. 背景与已证事实（探查结论，file:line 为基线）

### 1.1 病灶（负责人实测 + 代码取证双确认）

1. **手动触发**：rebuild 只有三个手动入口（`cydrive rebuild` main.rs:674 / 控制通道 `REBUILD` control.rs:137 / web Refresh cloudkit-web/lib.rs:688），启动/加卷/ENABLE 全不触发；`cydrive sync` 只做实例间行复制（sync.rs:1-30），**不扫后端**。
2. **全量遍历**：`rebuild.rs:102-146` 从卷根递归 `driver.list`，无总数/深度上限（仅每页 512）；15min 预算只在活实例 RebuildTask（lib.rs:4140-4208），**离线路径零预算**（lib.rs:4834-4850）；无持久化游标，超时重跑从根再来（lib.rs:4235-4246）；**不 prune**（rebuild.rs:26-28），远端已删行永留。
3. **读路径零 read-through**：网关 `db.list_dir`（cloudkit-webdav/lib.rs:244，模块头自认 "PROPFIND never touches the network"）、仪表盘 `api_list`（cloudkit-web/lib.rs:2346，空+无行→404）、winfs `dir_entries`（fs.rs:773-780）、VFS 读 `get_file().ok_or(NotFound)`（vfs.rs:776-780）。空库 = 空根（Y: 盘实测复现）。
4. **保鲜为零**：无 watcher、无定时后端重扫；`change_feed`/`authoritative_index` 只进启动横幅（lib.rs:500-507），无行为分叉——architecture.md「本地 db → (权威)后端 → (影子)sync」的解析顺序是**从未落地的设计承诺**，本计划即落地它。

### 1.2 有利事实（决定方案形态）

- **驱动层早已 per-path 现查**：pan115/pan123 pathcache（DIR_TTL=600s/DIR_CAP=1024）、baidu stat=list 父目录、webdav PROPFIND Depth0/1、sftp/local 逐路径——回源成本天然 O(1)/层，缺的只是 L4 读穿。
- **物化映射现成**：`rebuild.rs:150-215 upsert_entry` 就是 Entry→行的完整映射（含 K6 句柄降级 `Some(0)`、单容器 chunk 行 K11 形态），E2E 已证可被读链消费（K74 pan115 rebuild 腿 + `remote_handle_for` 对 `Some(0)` 放行、路径形后端用 `RemoteHandle.path` 寻址 vfs.rs:736-739）。
- **探针先例**：`CloudTransport::as_inbound`/`as_chat`（transport/mod.rs:204-210）= 默认 None + 消费方只降级绝不 panic 的既定形态；六个 transport_face 全部持有 `Arc<XxxDriver>` + `into_driver()`（ck-local/111-127、ck-baidu/96-113、ck-sftp/93-109、ck-pan115/88-104、ck-pan123/90-106、ck-webdav/97-113），**探针一行即可**；GrammersTransport 无驱动（telegram 窄面）→ 默认 None 天然正确。
- **测试材料齐**：`MockStorageDriver`（cloudkit-storage/src/mock.rs:40，L2 一等公民）+ 真 sqlite + 真 Vfs 的 harness 即 core/tests/rebuild.rs 形态；三消费面各有 TempDir+MockTransport harness（fs_adapter.rs:163 / read.rs:106 / multivolume.rs:97）；「行 + 远端字节分开播种」helper（web multivolume.rs:259-302）正是 read-through 测试形状。
- **无迁移机制也无碍**：`CREATE IF NOT EXISTS` + pragma 探针 ALTER 先例（database.rs:340-353）；本计划零 files 列变更，只加一张新表。

---

## 2. 设计裁决（D1–D10，执行期不得漂移；变更须回负责人）

| # | 裁决 | 理由与边界 |
|---|---|---|
| **D1 回源面 = `as_driver` 探针**，不并列持有 Arc、不给 `CloudTransport` 加 list | 窄面保窄（driver-onboarding §10 两班制哲学）；`as_inbound` 先例零新发明；6×1 行 + 默认 None；类型擦除后 `Arc<dyn StorageDriver>` 唯一合法取回路径 |
| **D2 门 = `authoritative_index && as_driver().is_some()`**；否则 `*_fresh` 逐字退化为 `db.*` 现行为 | telegram（窄面）零变化（A6）；能力位首个真实消费者（R4 兑现） |
| **D3 共性落点 = `Vfs::read_dir_fresh`/`stat_fresh` 两入口**，`readthrough.rs` 模块实现；消费面零机制代码 | 「提炼共性」的落点：机制一份、接线六后端 × 四面共用；未来驱动 = 实现 StorageDriver + transport_face 一行探针（§3） |
| **D4 物化映射唯一**：`materialize.rs::materialize_entry`（自 rebuild.rs:150-215 平移），rebuild 与 read-through 共用 | DRY 硬要求；K6/K11 形态单点维护 |
| **D5 `read_dir_fresh` 每次调用强制 revalidate**（RaiDrive 对等语义）；winfs 只在 marker=None 调用（prepare_enumeration 枚举快照兜着）、网关每 PROPFIND 一次 | 用户点赞的 J: 盘体验 = 每视图一次现查；枚举内续页零网络 |
| **D6 `stat_fresh` 走「父目录 TTL 窗（5s）+ 窗内行命中直出」**；过期 = 重列父目录（**不是**逐路径 stat）→ 仍无行 → `driver.stat` 兜底 | Explorer stat 风暴（百子项）归并为 1 次父 list；深跳路径 = 1 次 list 即命中（driver.list 按路径直址，非根游走）；「逐层自动遍历当前层」与负责人描述逐字吻合 |
| **D7 删除 = stat 双确认**：候选缺失行逐条 `driver.stat`，NotFound 才删；单目录 >32 候选整批跳过删除+告警；`Err`/非 NotFound 一律保留；in-flight（`!is_uploaded && local_copy_exists`，sync.rs:579-580 原文）两侧永不触碰 | 单次观测的列表可能是谎报/残缺（AList 上游瞬断）；「瞬态错误绝不落永久状态」红线的删除面落地；权威批量清账归 rebuild sweep |
| **D8 rebuild 三件套**：①迭代工作队列 + `rebuild_state` 持久化（每目录完成落盘）；②`RebuildTuning.max_entries`（默认 20 万/趟）+ 既有 15min 预算**同时补到离线路径**；③完成趟（队列清空）sweep：`DELETE FROM files WHERE is_uploaded=1 AND updated_at < scan_started_at`——`scan_started_at` 跨续跑持久（保护早趟 upsert 的行），chunks 级联删 | ①「rerun to continue」从口号变真；②总上限 = 负责人「100 万/1000 万」病灶的直接闸门；③零 files 列变更（updated_at 技巧），三重保护（is_uploaded/updated_at/仅完成趟） |
| **D9 `api_files` 保持索引视图**，不做回源（全表 = 全树 = rebuild 语义）；文件浏览面 `api_list` 才接 read-through | 防止「全表 API」暗中变全树遍历；语义明示 |
| **D10 加密卷显式拒收**（`ensure_plaintext_instance` 同款闸门 + 可行动文案指路 sync）；in-flight/ghost 行（`is_uploaded=0`）prune 侧永不删 | 远端是密文容器：list 给的 size 是密文尺寸，物化成行会破坏「size=明文」R6 契约与 AEAD 预算数学（rebuild.rs:30-36 注释原文同判）；ghost 归 K4 delete-wiring 旧账，不扩scope |

---

## 3. 共性提炼（负责人要求②：六后端 + 未来驱动的优雅共性）

机制与驱动的接口面**只有** `StorageDriver` 契约（list depth-1/字典序稳定/NotFound vs 空表/K67 产出即可寻址——conformance 八断言已钉）。共性五件套：

1. **探针**（L2，1 处 trait + 6×1 行 impl）：`as_driver`——窄面取宽面的唯一通道；
2. **门**（L3，1 处）：`authoritative_index && as_driver().is_some()`；
3. **物化**（L3，1 处）：`materialize_entry`——rebuild 与 read-through 共用的唯一 Entry→行映射；
4. **reconcile 原语**（L3，1 处）：upsert + 双确认 prune + in-flight 豁免；
5. **两入口**（L3，1 处）：`read_dir_fresh`/`stat_fresh`——四消费面与未来消费面共用。

**未来驱动（s3 等）的增量成本 = 0 行 read-through 代码**：实现 StorageDriver（conformance 已要求）+ transport_face 持 `Arc<驱动>` 并给出 `as_driver` 一行探针，即自动获得按需索引全能力。此约定入 `driver-onboarding.md`（RT5 文档批）。

---

## 4. 任务分解（RT0–RT5）

> 批次纪律（负责人 2026-09-07 指令）：TDD 红→绿留证、断言零漂移、每批收口更新 `docs/tracking/phase8-readthrough.md` 并随 commit 提交、五门禁每批必跑。
> worktree：`feat/readthrough-index` + **独立 target 目录**。测试命令统一 `cargo test --workspace --no-fail-fast -j 4`。

### Task 1（RT1）：L2 探针面 + L3 物化提取

**Files:**
- Modify: `crates/cloudkit-storage/src/transport/mod.rs`（`as_inbound` 旁加 `as_driver`）
- Modify: `crates/drivers/ck-{local,baidu,sftp,pan115,pan123,webdav}/src/transport_face.rs`（各 +3 行）
- Create: `crates/cloudkit-core/src/materialize.rs`
- Modify: `crates/cloudkit-core/src/rebuild.rs`（`upsert_entry` 平移改调用）
- Modify: `crates/cloudkit-core/src/lib.rs`（`mod materialize`）
- Test: `crates/cloudkit-core/tests/materialize.rs`（新）+ `crates/cloudkit-storage/tests/types.rs`（探针）

**Step 1 写红测试**（探针默认 None + 宽面 Some）：

```rust
// crates/cloudkit-storage/tests/types.rs 追加
#[test]
fn as_driver_probe_defaults_to_none_and_reports_the_wide_face() {
    // 窄面 mock：默认 None（telegram 形态）
    let t = cloudkit_storage::transport::mock::MockTransport::builder().build();
    assert!(t.as_driver().is_none());
}
```

```rust
// crates/cloudkit-core/tests/materialize.rs（新文件）
//! materialize_entry = Entry→行的唯一映射（D4）——rebuild 与 read-through 共用。
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::materialize::{materialize_entry, MaterializedRow};
use cloudkit_storage::{Entry, EntryId, EntryKind, RelPath};
use tempfile::TempDir;

fn entry(path: &str, kind: EntryKind, handle: &str, size: u64) -> Entry {
    Entry {
        id: EntryId::new("vol", handle).unwrap(),
        path: RelPath::parse(path).unwrap(),
        kind,
        size,
        mtime: 1_700_000_000.0,
    }
}

#[test]
fn path_shaped_handle_degrades_to_the_k6_placeholder_and_reads_back() {
    let dir = TempDir::new().unwrap();
    let db = MetaDatabase::open(&dir.path().join("meta.db")).unwrap();
    let e = entry("docs/a.txt", EntryKind::File, "/dav/docs/a.txt", 5);
    let row = materialize_entry(&db, &e).unwrap();
    assert_eq!(row.telegram_msg_id, Some(0), "path handle → K6 0 占位");
    assert_eq!(row.is_uploaded, true);
    assert_eq!(row.is_encrypted, false);
    assert_eq!(db.get_file("/docs/a.txt").unwrap().unwrap().size, 5);
}

#[test]
fn numeric_handle_lands_as_the_msg_id_with_a_single_container_chunk() {
    let dir = TempDir::new().unwrap();
    let db = MetaDatabase::open(&dir.path().join("meta.db")).unwrap();
    let e = entry("a.bin", EntryKind::File, "12345", 9);
    let row = materialize_entry(&db, &e).unwrap();
    assert_eq!(row.telegram_msg_id, Some(12345));
    let chunks = db.get_chunks_by_file_id(row.id).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].telegram_msg_id, Some(12345));
    assert_eq!(chunks[0].size, 9);
}

#[test]
fn re_materializing_bumps_updated_at_and_keeps_the_coalesce_columns() {
    // sweep 保护（D8③）靠 updated_at 刷新——本测试钉住该不变量
    let dir = TempDir::new().unwrap();
    let db = MetaDatabase::open(&dir.path().join("meta.db")).unwrap();
    let e = entry("a.bin", EntryKind::File, "1", 1);
    let first = materialize_entry(&db, &e).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = materialize_entry(&db, &e).unwrap();
    assert!(second.updated_at >= first.updated_at, "重物化必须刷新 updated_at");
    assert_eq!(second.is_uploaded, true);
}
```

**Step 2 跑红**：`cargo test -p cloudkit-core --test materialize && cargo test -p cloudkit-storage --test types` → 编译失败（模块不存在）= 红。

**Step 3 最小实现**：

```rust
// crates/cloudkit-core/src/materialize.rs
//! Entry → files 行的**唯一**物化映射（Phase 8 / D4）。
//!
//! 平移自 rebuild.rs::upsert_entry（K6 句柄降级 / K11 单容器 chunk 形态
//! 原样保留）；rebuild 与 readthrough 共用——映射语义只许在这一处维护。
use cloudkit_storage::{Entry, EntryKind};

use crate::database::{FileUpsert, FileRecord, MetaDatabase};
use crate::rebuild::RebuildError;

/// 物化产物（调用方拿行做后续判定，如 read-through 的 TTL 标记）。
pub type MaterializedRow = FileRecord;

pub fn materialize_entry(db: &MetaDatabase, entry: &Entry) -> Result<MaterializedRow, RebuildError> {
    // 词汇（卷相对 "docs/a.txt"）→ 行键（"/docs/a.txt"）：files 表
    // rel_path 约定，所有写方同形（原 rebuild.rs:155-163 注释平移）。
    let rel_path = format!("/{}", entry.path.as_str());
    let name = entry.path.file_name().unwrap_or_default().to_string();
    let parent_dir = match entry.path.parent() {
        Some(parent) => format!("/{}", parent.as_str()),
        None => "/".to_string(),
    };
    match entry.kind {
        EntryKind::File => {
            // K6：fs_id 形句柄直接进 i64；路径形降级 0 占位。
            let msg_id = entry.id.handle.as_str().parse::<i64>().unwrap_or(0);
            let file_id = db.upsert_file(&FileUpsert {
                rel_path, name, parent_dir,
                size: entry.size as i64,
                mtime: entry.mtime,
                sha256: None,
                is_dir: false,
                telegram_msg_id: Some(msg_id),
                is_uploaded: true,
                is_cached: false,
                is_encrypted: false,
                chunk_count: 1,
                mime_type: None,
            })?;
            // K11：单容器 chunk 行 = 上传 persist 单元素 receipt 同形。
            db.upsert_chunk(file_id, 0, msg_id, entry.size as i64, None)?;
            db.get_file(&format!("/{}", entry.path.as_str()))
                .map_err(RebuildError::from)?
                .ok_or_else(|| RebuildError::Db("materialized row vanished".into()))
        }
        EntryKind::Dir => {
            let file_id = db.upsert_file(&FileUpsert {
                rel_path, name, parent_dir,
                size: 0, mtime: entry.mtime,
                sha256: None,
                is_dir: true,
                telegram_msg_id: None,
                is_uploaded: true,
                is_cached: true,
                is_encrypted: false,
                chunk_count: 0,
                mime_type: None,
            })?;
            let _ = file_id;
            db.get_file(&format!("/{}", entry.path.as_str()))
                .map_err(RebuildError::from)?
                .ok_or_else(|| RebuildError::Db("materialized dir row vanished".into()))
        }
    }
}
```

（注：`RebuildError` 的 `Db` 变体/From 以现状为准适配——平移时保持 rebuild.rs 的错误形状不变；`list_all_pages` 归集器同批进 materialize.rs：

```rust
/// depth-1 全页归集（read-through 用；rebuild 的流式页循环保持原样——
/// 20 万级单目录内存 ≈ 数 MB，可整目录归集）。
pub async fn list_all_pages(
    driver: &dyn cloudkit_storage::StorageDriver,
    dir: &RelPath,
) -> Result<Vec<Entry>, cloudkit_storage::StorageError> {
    let mut out = Vec::new();
    let mut cursor = cloudkit_storage::PageCursor::Start;
    loop {
        let listing = driver.list(dir, cloudkit_storage::Page { limit: 512, cursor }).await?;
        out.extend(listing.entries);
        match listing.next {
            Some(next) => cursor = next,
            None => return Ok(out),
        }
    }
}
```
）

探针（transport/mod.rs，`as_chat` 后）：

```rust
    /// 探测**宽面**（StorageDriver——list/stat 面；默认无）。窄面
    /// （CloudTransport）刻意不含列表面（driver-onboarding §10 两班制），
    /// 本探针是类型擦除后取回宽面的唯一通道（Phase 8 / D1）；语义同
    /// [`CloudTransport::as_inbound`]：消费方拿 `None` 只降级，绝不 panic。
    fn as_driver(&self) -> Option<&dyn crate::driver::StorageDriver> {
        None
    }
```

六处 transport_face（以 webdav 为例，其余同形）：

```rust
    fn as_driver(&self) -> Option<&dyn cloudkit_storage::StorageDriver> {
        Some(self.driver.as_ref())
    }
```

rebuild.rs 的 `upsert_entry` 删除、`rebuild_dir` 内改调 `materialize::materialize_entry`（outcome 计数留在 rebuild 侧）。

**Step 4 跑绿**：上列测试 + `cargo test -p ck-webdav -p ck-local --lib`（探针编译）+ `cargo test -p cloudkit-core --test rebuild`（既有 4 测试零漂移）。

**Step 5 Commit**：`feat(storage): as_driver 探针 + materialize 唯一映射提取（Phase 8 RT1）`

### Task 2（RT2）：readthrough 原语（核心批，TDD 密集）

**Files:**
- Create: `crates/cloudkit-core/src/readthrough.rs`
- Modify: `crates/cloudkit-core/src/vfs.rs`（`read_dir_fresh`/`stat_fresh` 薄壳 + 写侧失效点）
- Modify: `crates/cloudkit-core/src/lib.rs`（`mod readthrough`）
- Test: `crates/cloudkit-core/tests/readthrough.rs`（新，MockStorageDriver + 真 sqlite + 真 Vfs）

**语义定稿（实现与测试的唯一依据）：**

```rust
// read_dir_fresh(dir)：
//   门不过 → return db.list_dir(dir)                 // D2 逐字退化
//   单飞(dir) {
//     driver.list 全页归集
//       Ok(entries) → in-flight 行两侧跳过（D7/D10）→ materialize 每条
//                    → prune 候选 = 本目录 is_uploaded=1 且不在 entries 且无 in-flight
//                      逐条 driver.stat 双确认（NotFound 才删；>32 候选整批跳过+warn）
//                    → DirCache.mark(dir) → return db.list_dir(dir)
//       NotFound    → 删本目录行+子树（同双确认语义从简：仅本层行，子树留 sweep）→ NotFound
//       Err(e)      → stale-if-error：db 有行 → warn + db.list_dir；无行 → Err(e)   // 绝不 404
//   }
//
// stat_fresh(rel)：
//   rel 是根 → 合成根元数据（与现消费面一致）
//   行命中 && DirCache.fresh(parent) → 行直出（零网络）
//   否则 read_dir_fresh(parent)（D6 风暴归并）→ 行命中 → 行
//   仍无行 → driver.stat(rel) → materialize → 行；NotFound → NotFound
//
// 写侧失效（就近失效，pan115 先例）：commit_put/create_dir/remove_file 后
//   DirCache.invalidate(parent)——保证本侧增删立即可见（D5 强制刷新本就兜底，失效是双保险）
```

**Step 1 红测试（按批拆分，每条先红后绿）**：

```rust
// crates/cloudkit-core/tests/readthrough.rs 关键用例（完整文件实现期写全）：
// 1. miss 回源物化：db 空 + mock 远端 8 目录 → read_dir_fresh("/") 见 8 项 + 行落库 + 每层恰 1 次 list（A3）
// 2. 深跳 1 次 list：stat_fresh("/a/b/c.txt") 父未索引 → 恰 1 次 list("/a/b") + 行命中（D6）
// 3. stat 风暴归并：连续 100 次 stat_fresh(子项) → 1 次父 list（D6 TTL 窗）
// 4. 强制刷新：远端加文件 → read_dir_fresh 立即可见（无 TTL 等待，D5）
// 5. 删除双确认：远端删 1 文件 → 第一次 read_dir_fresh 后行已删（stat NotFound 确认）；
//    stat 谎报 Ok → 行保留（D7）
// 6. 大批缺失保护：远端列表空洞（>32 缺失）→ 全部行保留 + warn（D7 上限）
// 7. in-flight 豁免：is_uploaded=0 且本地副本存在的行，两侧都不被触碰（D7/D10）
// 8. stale-if-error：list 瞬态 Err + 有旧缓存 → 照常返回旧列表；无缓存 → Err 非 NotFound（风险表）
// 9. 门退化：as_driver=None（MockTransport 默认）→ 行为与今日 db.list_dir 逐字一致（A6）
// 10. 单飞：并发 8 次 read_dir_fresh(同目录) → 恰 1 次 list
// 11. 写侧失效：commit_put 后 stat_fresh 即见新行（失效+强制双保险）
```

**Step 2–4** 每条用例：写测试 → 跑红 → 最小实现 → 跑绿（循环 11 次，红绿输出贴批日志）。

**Step 5 Commit**：`feat(core): read-through 按需逐层索引原语（Phase 8 RT2）`

### Task 3（RT3）：四消费面接线

**Files:**
- Modify: `crates/cloudkit-webdav/src/lib.rs`（metadata:259-270 / read_dir:229-257 / open 读臂:165 前）
- Modify: `crates/cloudkit-winfsp/src/fs.rs`（meta_for:806 / dir_entries:773 / open_with_read:880）
- Modify: `crates/cloudkit-web/src/lib.rs`（api_list:2346）
- Test: 三面既有 harness 各加「远端有货 + 空库 → 面上可见」用例（fs_adapter.rs / read.rs / multivolume.rs 形态照抄）

**每处接线 = 换数据获取函数**（示例，webdav read_dir）：

```rust
// 前：self.row(&rel)?; let entries = self.db.list_dir(rel.as_str())?;
// 后：let entries = self.vfs.read_dir_fresh(&rel).await.map_err(vfs_err)?;  // miss 自动物化
```

winfs 侧 = `self.bridge.block_on(self.vfs.read_dir_fresh(&rel))`（bridge 语义不变，D 风险表已述）。

**测试要点**：A1 面级复现（空库 + mock 远端 → PROPFIND/枚举/api 直接见内容）+ 既有断言零漂移。
**Commit**：`feat(faces): 网关/仪表盘/winfs 读路径接入 read-through（Phase 8 RT3）`

### Task 4（RT4）：rebuild 校对工具化（三件套）

**Files:**
- Modify: `crates/cloudkit-core/src/rebuild.rs`（迭代队列 + `rebuild_state` 读写 + sweep）
- Modify: `crates/cloudkit-core/src/database.rs`（`rebuild_state` KV 表 + `sweep_unseen` 查询）
- Modify: `crates/cloudkit-cli/src/lib.rs`（`RebuildTuning.max_entries` + 离线路径补预算 + 完成文案）
- Test: `crates/cloudkit-core/tests/rebuild.rs` 追加（续跑/上限/sweep 三组红测试）

**Step 1 红测试**：

```rust
// 1. 续跑（A4）：断言中断后 rerun 不重扫已完成目录——MockStorageDriver 包一层 list 调用计数
//    首趟限 max_entries=50 → 完成 3 目录即断；二趟完成 → 总 list 次数 = 目录总数（无重复）
// 2. 总上限（A4）：max_entries 到顶 → outcome 明示 "entries budget ran out; rerun to continue"
// 3. sweep 三重保护（A5）：
//    a. 扫描前存在且远端已删 → 完成趟后行删（chunks 级联）
//    b. 本趟 upsert 的行（updated_at ≥ scan_started）→ 保留
//    c. 跨续跑早趟 upsert 的行 → 保留（scan_started_at 从首趟持久——本条即防回归）
//    d. in-flight 行（is_uploaded=0）→ 保留
// 4. 未完成趟（预算/超时中断）→ 绝不 sweep
```

**Step 5 Commit**：`feat(rebuild): 游标续跑 + 总上限 + 完成趟 sweep（Phase 8 RT4）`

### Task 5（RT5）：真机矩阵 + 文档 + 收口

**Files:**
- Test: `crates/drivers/ck-webdav/tests/live_readthrough.rs`（新，`#[ignore]` 真机腿）
- Modify: `README.md`（机制说明）/ `AGENTS.md`（计数与陷阱）/ `docs/standards/driver-onboarding.md`（§11 未来驱动共性义务）/ `docs/standards/architecture.md`（解析顺序从「设计意图」改「已落地」）/ `docs/decisions.md`（K83 入档）
- Modify: `docs/tracking/phase8-readthrough.md`（终态账）

**真机矩阵（WSL2 双服务器 fixture 照 phase7-webdav-fixture.md 重建；AList 场景复现优先）**：
1. **A1/A2 用户场景**（webdav×AList 形态）：空库挂 Y: 逐层即见；对照组 RaiDrive 增删 → Y: 一致（A2 删除腿）
2. **A3 规模腿**：根下大目录（千级条目）浏览，tracing 数回源调用 = O(1)/层
3. **A4 续跑真机**：真服务器大目录 + max_entries 断趟 → rerun 续完
4. sftp/local 各一腿冒烟（共性面跨后端）
5. 台账：wsl 通道脚本纪律（单行+字面路径+零变量）照 fixture 文档执行

**收口清单（不可跳）**：五门禁终跑 + 裁剪组合构建 + 文档联动 + 跟踪单终态账 + 深度审查批（K78 形态，High/Medium 全清偿后才许申请合入）。

---

## 5. 验收对照

| 标准 | 验证方式 |
|---|---|
| A1 | RT3 面级测试 + RT5 真机腿 1 |
| A2 | RT5 真机腿 1 删除腿 |
| A3 | RT2 用例 1/2/3（调用计数断言）+ RT5 真机腿 2 |
| A4 | RT4 用例 1/2 + RT5 真机腿 3 |
| A5 | RT4 用例 3a–3d（红→绿） |
| A6 | RT2 用例 9 + 全量既有断言零漂移复跑 |
| A7 | RT2 加密卷拒收用例 + 文案钉 |
| A8 | 每批五门禁 + 收口终跑 |

## 6. 执行顺序与依赖

RT1 → RT2（依赖 materialize+探针）→ RT3（依赖 readthrough 两入口）与 RT4（独立于 RT3，依赖 materialize）可并行派发 → RT5 依赖全部。批次内 TDD 微循环；hub-and-spoke 派发（主会话拆分/审查/合并，实现落子代理）。

## 7. 架构合规证明（负责人要求③：增强而非破坏）

| 红线/契约 | 判定 | 证据 |
|---|---|---|
| R1 层依赖 | ✅ 零新增违规边 | 探针在 L2（storage 内部 driver.rs 引用）；readthrough 在 L3（core→L2 既有合法边）；消费面只调 `Vfs` 方法；`check_layers` 16 manifests 收口必跑 |
| R2 错误映射 | ✅ 不动 | 回源错误走既有 `StorageError`/`VfsError` 归一；`Err → stale-if-error` 不落 NotFound（404 语义只给确认不存在） |
| R3 凭据 | ✅ 零涉及 | read-through 不触碰凭据面（驱动内部自理） |
| R4 能力诚实 | ✅ 反增强 | `authoritative_index` 从横幅装饰升格为行为门——能力位撒谎 = 功能不生效，诚实成为可执行约束 |
| R6 Python 契约（files DDL 冻结） | ✅ 零列变更 | sweep 复用 `updated_at`；新表 `rebuild_state` 是 Rust 自加表（`sync_mirror` 同先例）；物化映射平移自 rebuild（K6/K11 形态原样） |
| R7 conformance | ✅ 不动 | `StorageDriver` 契约零变更；conformance 八断言原样 |
| 锁纪律（RV1） | ✅ 自动满足 | 回源 IO 在 handler/回调本体（锁外）；DirCache 内部 `StdMutex` 短临界区 + 单飞 `tokio::sync::Mutex` 不跨表锁 |
| K44 winfs 快照语义 | ✅ 保持 | `get_file_info` 零改动；回源只进 meta_for/dir_entries/open_with_read |
| 上传队列/sync 语义 | ✅ 豁免保护 | in-flight 判定复用 sync.rs:579-580 原文；`upsert_file` 的 coalesce 列（sha256/msg_id/mime）天然不被物化冲掉 |
| 行为变化面 | 仅两处，均增强 | ①读路径 miss 从 404 变回源（用户要的本体）；②rebuild 完成趟可 prune（D8③ 三重保护）——telegram 卷与加密卷零变化 |

**破坏性变更纪律**（版本兼容纪律：pre-1.0 不做迁移）：`rebuild_state` 新表对旧库幂等（CREATE IF NOT EXISTS）；无旧数据迁移、无兼容分支。

---

## 8. 挂账预登记（执行期如实增补）

- 加密卷 read-through（需先解决密文 size→明文 size 的可靠换算或远端元数据面，D10 明确不做）
- api_files 全表视图的新鲜度（D9 保持索引视图；若需全树新鲜 = rebuild 语义，另议）
- ghost 行（`is_uploaded=0` 且无本地副本）清理由 K4 delete-wiring 旧账统一处理
- stat 的 TTL 窗值（5s）为实测定值，真机矩阵后可调（`with_ttl` 测试缝已留）
