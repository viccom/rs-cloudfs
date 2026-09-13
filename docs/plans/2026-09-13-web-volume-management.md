# Web 卷管理面 + 运行时 rebuild 执行计划

> **For Claude:** REQUIRED SUB-SKILL: executing-plans（hub-and-spoke，TDD 红→绿留证）。
> 负责人 2026-09-13 批准开工（含五个小裁决点按推荐落定，见 §2.3）。

**Goal:** web 仪表盘新增存储卷管理（新增/修改/删除/Refresh），使用态与配置态**独立双页**可跳转；rebuild 运行时化（控制通道后台命令 + 安全约束 R1–R6）。

**非目标（挂账）**：周期 rebuild 与 pruning（K11 no-pruning 边界保持，另裁决）；sqlite 配置存储（评估后搁置，见 decisions 2026-09-12 对话记录）；管理面鉴权（Origin 校验先行）；toml_edit 注释保留（接受注释丢失 + UI 明示）。

## §1 架构

### 1.1 调用路径：进程内回调缝（不引 web→cli 依赖）

web 与 cli 同为 L5（Cargo 循环使 import 物理不可能）。web crate 定义注入类型：

```rust
// cloudkit-web（与 control.rs 的 VolumeCommandHandler 同构）
pub type VolumeCommandClient = Arc<
    dyn for<'a> Fn(&'a str) -> Pin<Box<dyn Future<Output = String> + Send + 'a>>
        + Send + Sync + 'static,
>;
```

cli 装配点（`run_multi_with_transports_and_commands`）把控制通道已装的**同一 handler 闭包**再 Arc 一份注入 web state——web 发的命令与控制通道命令进**同一 mpsc 串行队列**（K48 串行化语义原样继承，K50/H1–H3/M1 全部成果免费复用）。REBUILD 例外：accepted 后转后台任务（§1.3 R3）。

### 1.2 协议扩展（控制通道行协议，逐批落地）

| 命令 | 批 | 语义 |
|---|---|---|
| `SHOW <名>` | P0 | 读 `volumes/<名>.toml` → 单行 JSON 回复；**凭据键只回 `{"set": true/false}` 布尔，值永不出后端**（write-only 原则） |
| `REBUILD <名>` | P2 | 后台 rebuild（§1.3），立即回 accepted；加密卷/telegram 卷拒绝（既有文案） |
| `CREATE <名> <json>` | P3 | 校验表单 JSON → 受控生成 toml（只写显式设置的字段）→ ADD 装配；已存在拒 |
| `UPDATE <名> <json>` | P4 | load→改（凭据空=不动）→受控重写 toml→REMOVE+ADD 重装配；rewrite 丢注释在回复中明示 |
| `DESTROY <名>` | P5 | 二段：`DESTROY <名> confirm` 才执行；先 K50 序卸载（运行中）→ 删 toml 行；本地数据目录默认保留（`purge_local=true` 才删），**远端数据永不触碰** |

受控 toml 生成走 core 新函数（`render_volume_toml`：显式字段渲染 + 校验复用 `is_valid_volume_name`/`validate`），**不用 `save_toml`**（铺 29 默认键）——setup 手写模板先例（setup.rs）。

### 1.3 rebuild 安全约束 R1–R6（P2 实现主体）

- **R1 单飞互斥**：卷级 rebuild 状态机（idle→rebuilding→idle）；重入回 `ERR: rebuild already running`。
- **R2 排空门槛（硬约束）**：卷 `queue_stats().outstanding() > 0` 时拒绝（与 REMOVE 排空同判据，H2 的 `outstanding()`）；文案含 pending 数与「LIST 可查」。
- **R3 后台化不占队列**：立即回 `OK: rebuild started in background`；任务 spawn 执行，命令队列不被饿死、120s 预算不适用。
- **R4 有界 + 幂等中止**：默认 15 分钟上限（`rebuild_timeout` 可配）；中止/超时时已 upsert 行保留（幂等 merge，重跑即续）。
- **R5 卷卸载/停机联动**：检查点（每页远端 list 后）查卷在注册表 + watch 未触发，否则自行中止；停机中断日志声明「interrupted; rerun to continue」。
- **R6 范围 gate**：必须单卷名；加密卷 K11 拒（回 sync 指引）、telegram 卷拒（既有）。

rebuild 状态进 `LIST` 输出（卷行追加 `rebuilding` 标记）供 CLI/web 展示。

### 1.4 双页布局（独立页面，整页跳转）

```
/index（使用态，现状）           /volumes（配置态，新页）
sidebar: nav 加 Volumes 项 →     sidebar: nav Cloud Drive ←跳回、Volumes active、
topbar/stats/files-table          storage 卡=实例级总用量（裁决⑤）
                                 topbar'：标题 + [Add Volume] 主按钮
                                 stats-grid'：卷总数/Running/Failed/Pending 四卡
                                 volumes-table：Name|Backend|Status|Drive|Pending|Size|Actions
                                   Actions：[Refresh][Edit][Unmount][Disable][Delete]
                                 表单卡（Create/Update）在表上方展开；两步删除 modal
```

- 复用全部现有 CSS tokens/组件（btn/stat-card/files-table/badge/modal），零新视觉体系。
- `volume-tabs` 行尾「＋」→ `/volumes#add` 锚点（P3 起表单自动展开）；`localStorage` 记使用态 current 卷。
- 整页 `<a>` 跳转（无框架形态的正确做法）；两页 nav 写死各自 active。
- 单卷模式：`/volumes` 在 multi router 挂载；单卷访问给说明页「单卷模式：卷由 config.toml 定义」（裁决④）。
- 顺带修复：使用态选中卷被 REMOVE 后 current 悬空、UI 冻结——`renderVolumeTabs` 后 current 不在表则回退第一个 running。

### 1.5 安全增界（管理写路由，P1 起生效）

- **Origin/Referer 同源校验** middleware（裁决③）：管理写路由族（POST /api/volumes/*）非同源请求 403；P0 先就位中间件本体。
- 绑定非 loopback（`web_ui_host != 127.0.0.1`）时管理面自动降级只读；远程管理需显式 `allow_remote_admin = true`（进程键，入 K19 清单）+ 启动 warning。

## §2 裁决

### 2.1 K48 修订（触发源扩展）
触发源 = 控制通道命令（原）**+ web 管理面**——两者经同一回调汇入同一串行队列；协议扩 `SHOW/REBUILD/CREATE/UPDATE/DESTROY`；REBUILD accepted 后转后台（R3）不占队列。

### 2.2 K49 修订（分档解禁）
REMOVE 运行态语义不变。CREATE/UPDATE 允许程序写卷 toml：**凭据 write-only**（读面只回布尔，写面空=不改）；DESTROY 可删 toml，**永不自动删远端数据**，本地数据目录默认保留、`purge_local` 显式才删；toml 重写丢注释须在回复/UI 明示。

### 2.3 五个小裁决点（负责人 2026-09-13 认可按推荐落定）
① toml 注释丢失：接受 + 明示，不引 toml_edit；② DESTROY 本地数据目录默认保留；③ Origin 校验（非 CSRF token）；④ 单卷 `/volumes` 说明页；⑤ 配置态侧栏 = 实例级总用量卡。

### 2.4 K11 增补（rebuild 运行时化）
运行时 rebuild 允许（WAL 多连接并发设计内；可见性=消费面每请求直查 db）；R1–R6 为强制约束（R2 排空门槛为硬行为）；no-pruning 边界重申。

## §3 批次（worktree `feat/web-volume-mgmt`；串行；每批 TDD 红→绿 + 全门禁）

| 批 | 内容 | Commit |
|---|---|---|
| P0 | 双页骨架：`/volumes` 页（volumes.html/js + CSS 复用 + 跳转 + localStorage + current 回退修复）；回调缝（web 注入类型 + cli 装配注入）；`SHOW` 命令 + `GET /api/volumes/{name}/config` 端点（脱敏 JSON）；`/api/volumes` 行扩 `pending`（`vfs.queue_stats().outstanding()`）；Origin middleware 就位；单卷说明页 | `feat(web): /volumes page skeleton, command seam, and SHOW` |
| P1 | 卸载（经既有 REMOVE）+ Enable/Disable 开关（UPDATE 窄形态：只改 enabled 键）+ loading/toast + 非 loopback 降级 + `allow_remote_admin` 键 | `feat(web): runtime unmount and enable/disable from the dashboard` |
| P2 | `REBUILD <名>` 后台任务化（R1–R6）+ LIST rebuilding 标记 + `cydrive rebuild` 运行实例转发 | `feat(volumes): background rebuild over the control channel` |
| P3 | `CREATE` + 受控 toml 生成（core `render_volume_toml`）+ 新增表单（backend 动态字段） | `feat(volumes): volume creation from the dashboard` |
| P4 | `UPDATE` 全量（write-only 凭据）+ 编辑表单（SHOW 预填）+ 重装配提示 | `feat(volumes): volume editing with write-only credentials` |
| P5 | `DESTROY` 两段确认 + 删除 modal + `purge_local` | `feat(volumes): volume deletion with a typed confirmation` |
| P6 | 卷表 Actions 列「Refresh from remote」按钮（rebuilding 态跟随） | `feat(web): refresh-from-remote button on the volume table` |

每批门禁 = workspace 三步 + winfsp 腿三步（LIBCLANG_PATH）+ check_layers + scan_secrets；前端改动以 mock 双卷 e2e + 浏览器面手动验证留证。

## §4 风险

| 风险 | 缓解 |
|---|---|
| 回调缝把 web 请求延迟暴露给命令队列（LIST 等在途 REMOVE 后） | web 调用带超时（M1 `exchange_line_bounded` 同值语义）；/volumes 页轮询只读端点为主 |
| SHOW 泄露凭据 | 构造点白名单序列化（只回布尔）；e2e 断言响应无任何凭据值形态 |
| 双页 HTML 骨架漂移 | CSS 全复用；HTML 骨架重复仅 sidebar/topbar（接受，无组件系统） |
| REBUILD 后台任务与停机/卸载竞态 | R5 检查点（H1 gate 同构模式）；幂等中止 |
| CREATE 表单校验与 core validate 漂移 | 表单只做 UX 级预检；权威校验全在 CREATE 命令（错误原文回显） |
