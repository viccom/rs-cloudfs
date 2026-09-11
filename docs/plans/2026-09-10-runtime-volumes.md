# Phase 3.6：存储卷运行态动态加载/卸载 + 卷级 enabled 键 执行计划

> **For Claude:** REQUIRED SUB-SKILL: executing-plans（hub-and-spoke，TDD 红→绿留证；真机批次主会话执行）。

**Goal:** 不重启进程，动态装载/卸载存储卷：控制通道新增 `ADD <名>` / `REMOVE <名>` / `LIST` 命令，运行态装配/卸下卷（db/传输/VFS/上传队列/盘符/WebDAV 子路径/仪表盘 tab 全部跟随）；卷级 `enabled` 键作为**启动期**启用/禁用的持久化形态（已批准、暂缓实施的那条，与本批合流）。单卷模式零改动。

**非目标（挂账）**：文件监视/自动热加载（触发源用显式控制命令，见 K48）；`cydrive volumes enable/disable` 的 toml 编辑 CLI（含凭据文件的程序化改写风险，v1 手编文件）；REMOVE 的持久化（运行态卸载不碰卷文件，重启后按文件回来——持久禁用=手编 `enabled = false`）。

## §0 已核实接缝（2026-09-10 主会话侦察）

- `cloudkit-cli/src/lib.rs:864` `VolumeRegistry { volumes: Vec<VolumeRuntime> }`——启动期一次构建的平铺 Vec；`volume(name)`/`status_list()` 是仅有的消费面（mount pass、横幅、MV3 `/api/volumes`）。
- `WebDavServer::serve_volumes(Vec<(String, CyDriveFs)>, addr)`（webdav lib.rs）——启动期把每卷路由一次性挂进 axum Router；无动态分发层。
- 控制通道（`control.rs`）：loopback 行协议，**仅 `STOP`** 一命令（122 行 `if line == "STOP"`）；`ControlServer::bind(cfg)` 回调触发 shutdown gate。扩命令的自然位置就在这个行协议。
- `MultiVolumeHandle { registry, watch, stop_task, sync_tasks: Vec<Option<JoinHandle>>, webdav_addr, web_ui_addr, mounted_letters, mounted_volumes }`（lib.rs:971）——sync 任务与挂载列表也是启动期快照。
- 每卷装配闭环已独立（K21/K22）：独立 db/CacheManager/transport/Vfs/上传队列；`Vfs::shutdown()` 每卷排空停止（stop 序列在用）；winfsp `MountHandle` 自带 mount/unmount + 就绪/消失双轮询（WF4）。
- 卷发现 `discover_volumes`（config.rs:442）平铺扫 `*.toml`；`VOLUME_SCOPED_KEYS` 白名单缺 `enabled`（写了会拒收——RV0 补）。
- dashboard `/api/volumes` 与多卷 web 测试：`crates/cloudkit-web/{src/lib.rs,tests/multivolume.rs}`——RV1 详读后接线。

## §1 裁决 K48–K51（执行期入档 decisions.md）

| # | 裁决 | 理由 |
|---|---|---|
| K48 | **触发源=控制通道显式命令**（`ADD/REMOVE/LIST` 行协议），不做文件监视/周期重扫 | 显式运维面语义明确、零 watcher 依赖、脚本可驱动（`cydrive` CLI 后续包一层即可）；热重载式"改文件自动生效"的歧义面（半写的 toml/凭据轮换中）全避开 |
| K49 | **REMOVE 只动运行态、不碰卷文件**；持久禁用=`enabled=false`（RV0 键）手编 | 含凭据卷文件的程序化改写（toml 往返丢注释/错缩进风险）不进 v1；运行态与持久化正交，各自可测 |
| K50 | **卸载安全序=排空上传→winfsp unmount（10s 消失轮询）→注册表摘除**；任一步超时/失败→报可行动错误、**卷保持注册不动**（中止移除，绝不半卸） | pending 守卫同哲学：宁可拒绝也不留半状态；WinFsp unmount 无 force（句柄占用=用户责任，宽限期兜大半）——占用即卸载超时即中止 |
| K51 | **注册表改造=`Arc<RwLock<有序卷表>>` 共享给 WebDAV 动态分发/仪表盘/横幅**，dav 路由改 per-request fallback 分发 | 三消费面读同一真源，加/卸立即可见；避免"路由重建+换 listener"的重绑复杂度（端口不变，dav-server 的每卷 DirRouter 本就可延迟解析） |

## §2 架构

```
控制通道（loopback 行协议：STOP 之外新增）
  ADD <名> ──► 读 volumes/<名>.toml（enabled 必须 true）→ 单卷装配
  │            （db/传输/VFS/队列→winfsp 或 webdav 挂载→注册表插入→横幅刷新）
  REMOVE <名> ─► K50 安全序 → 注册表摘除 → 盘符卸、/vol/<名> 404、tab 消失
  LIST ───────► 卷名×状态×盘符×backend 一行一卷
┌─ 三消费面读同一真源（K51）─────────────────┐
│ VolumeRegistry（Arc<RwLock<Vec<VolumeRuntime>>>>） │
│  ├ WebDAV：/vol/<名> fallback → RwLock 读锁现查     │
│  ├ 仪表盘：/api/volumes + tabs 动态                 │
│  └ 横幅/doctor：status_list() 现查                  │
└────────────────────────────────────────────┘
启动路径：discover_volumes 过滤 enabled=false（跳过+info!）→ 既有装配
```

- ADD 失败（凭据错/盘符冲突/db 打不开）= 该卷不加入 + 可行动错误回复，兄弟卷零影响（K22 运行态版）。
- `ADD` 重名/已注册 → 拒绝并提示先 `REMOVE`；`REMOVE` 不存在 → 拒绝。
- 挂载 backend 沿用进程配置 `mount_backend`（winfsp/webdav/降级路径全复用 WF4 逻辑）。

## §3 批次（worktree `feat/runtime-volumes`；串行；每批 TDD 红→绿 + 全门禁）

### RV0 卷级 enabled 键（小批独立价值）
- `VOLUME_SCOPED_KEYS` + `CyDriveConfig` 加 `enabled: bool`（serde default true——缺省即启用，语义的自然缺省，非兼容考量）；`discover_volumes` 解析时跳过 `enabled=false` 的卷（`info!` 声明，不静默）；禁用卷不参与盘符冲突校验/装配/横幅；config.toml（进程级）出现该键仍拒（沿用 K19 分区）。
- 测试：禁用跳过、缺省启用、进程级出现拒收、禁用卷不占盘符。
- Commit: `feat(config): per-volume enabled key (skip at discovery)`

### RV1 注册表动态化 + WebDAV/仪表盘动态分发
- `VolumeRegistry` → `Arc<RwLock<...>>` 共享句柄（`RegistryHandle`）；`WebDavServer` 增动态 serve 模式（per-request `/vol/<名>` 分发，读锁查表；卷表 miss → 404）；dashboard `/api/volumes`/tabs 改读注册表句柄；横幅 `status_list` 现查。
- 启动路径行为零漂移（启动装配后表内容与现在完全一致——既有 multivolume 测试全部存活即证）。
- 测试：mock 两卷起服 → 表中摘一卷 → `/vol/<名>` 404 + 存活卷不受影响 + `/api/volumes` 少一行；动态插卷 → 路由立即可达。
- Commit: `feat(volumes): shared runtime registry with dynamic webdav/dashboard dispatch`

### RV2 控制通道 ADD/REMOVE/LIST + 卸载安全序
- `control.rs` 行协议扩三命令（错误路径返回可行动文本，不炸连接——`STOP` 语义不动）；cli 装配层抽「单卷装配」「单卷安全卸载」两函数（复用既有每卷装配/停序代码路径，不复制）；`MultiVolumeHandle.sync_tasks` 随卷增减（每卷自管 JoinHandle，废弃启动期 Vec 快照）。
- ADD/REMOVE 全链 TDD（mock 传输）：装配→可见→卸载→不可见→盘符消失→队列排空断言；K50 中止路径（unmount 超时→卷保持）用可注入轮询假探针测。
- `cydrive volumes` CLI 只读加 `LIST` 转发（`cydrive volumes status` 现有面扩运行态，不新增子命令面）。
- Commit: `feat(volumes): runtime add/remove/list over the control channel`

### RV3 真机验收 + 收口（主会话）
- 真机矩阵：①运行态 ADD baidu 卷（Q:）→ Explorer 可见可读写；②REMOVE → 盘符消失/`/vol` 404/tab 消失/上传排空后进程内无残留任务；③REMOVE 占用句柄的卷 → 超时拒绝、卷完好；④`enabled=false` 重启跳过；⑤ADD 失败（坏凭据）不伤兄弟卷；⑥横幅/doctor 一致性。
- 文档：decisions K48–K51、README（运行态卷管理+控制命令）、AGENTS 阶段推进+计数、tracker 全绿；merge main + push + 双产物。
- Commit: docs + merge（真机证据入 tracker）

## §4 风险

| 风险 | 缓解 |
|---|---|
| dav-server Router 动态分发的行为细节（每卷 DirRouter 生命周期） | RV1 先 spike 后实现；启动态零漂移由既有 multivolume 测试全存活钉住 |
| 控制通道并发（ADD 中 REMOVE 同名） | 命令处理串行化（单任务消费 or 注册表写锁内做状态机） |
| 卸载时上传大文件排空慢 | K50 超时中止可重试；`LIST` 显示 pending 计数辅助判断 |
| sync_tasks/挂载列表启动期快照残留 | RV2 把两者随卷自管，`MultiVolumeHandle` 只持聚合视图 |
| 横幅是启动期打印 | 运行态变更走 `println!`+日志即时通告（横幅语义=启动快照，不追改） |
