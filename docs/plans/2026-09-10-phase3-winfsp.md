# Phase 3：WinFsp 类本地盘挂载执行计划（简洁高效路线）
> 编者注（2026-09-25）：K89 起 winfsp 已入 cloudkit-cli `default`，本文的「feature 默认关 / GPL-free 隔离」纪律已废止——现状见 decisions.md K89 与 docs/platform-builds.md。

> **For Claude:** REQUIRED SUB-SKILL: 使用 executing-plans 编排执行（hub-and-spoke，TDD 红→绿留证；真机批次主会话执行）。

**Goal:** 用 WinFsp 原生 API 把存储卷暴露为**本地文件系统语义盘符**——Explorer 无感浏览、播放器直接播放大视频秒开秒拖（无 WebClient 整文件缓存/RPC 抖动）、写入走既有上传队列；WebDAV/net use 路径原样保留为默认与回退。

**Architecture:** 薄适配器模式（rclone 验证）——新 crate `cloudkit-winfsp` 只做「句柄表 + Win32/NTSTATUS 翻译 + 生命周期」，全部读写策略复用既有语义核心（open_read 三重门/RangeFile 窗口模型/StagedFile→put_staged/K33 回退）。同步回调经注入的 tokio Handle 桥接（rclone 同款：无超时包装，靠"减少回调+缓存一切"治本）。cargo feature `winfsp` 默认关，主发行物零 GPL 依赖。

**Tech Stack:** winfsp-rs 0.13（native API + async-io feature）、tokio Handle 注入、既有 cloudkit-core/storage/webdav 语义面。

---

## §0 研究档案（前置输入，已入档）

- **docs/reports/2026-09-10-rclone-vfs-winfsp-study.md**：rclone 三路深研（VFS 核心/磁盘缓存/WinFsp 集成）+ 汇合裁决输入。本计划的每个"必抄/跳过"都可溯源到该报告的 file:line。
- **本仓接缝盘点**（SR 批完成后的基座）：open_read 三重门（vfs.rs）、RangeFile 4MiB 窗口（webdav lib.rs:631+，可直接搬 offset 模型）、StagedFile→put_staged（webdav lib.rs:730+/vfs.rs:353）、MockTransport 观测面（open_range_calls）、错误映射三函数（vfs_err/io_err/db_err storage_err）。
- **环境事实**：本机已装 WinFsp（WinFsp.Launcher 服务运行中）——真机调试条件现成；RaiDrive（CBFS 方案）在本机可用作体验对照。
- **rclone 教训 crystallized**：FUSE 兼容层税（atomic_o_trunc/`\?\`/1601 时间/Init 竞态/ACL 失真）→ 我们直绑 native 避开；Flush 高频 → 零副作用原则；Explorer 防打爆 → readdir 带 stat + 元数据零网络。

## §1 裁决 K38–K46（执行期逐条入档 decisions.md）

| # | 裁决 | 理由/来源 |
|---|---|---|
| K38 | **许可与分发=feature 门控隔离**：`cloudkit-winfsp` 为 cli 的 optional dep，feature `winfsp` **默认关**；默认构建/主发行物零 GPL 依赖（deny.toml MIT-only 不破），`--features winfsp` 产物当前仅私有/内部分发（仓库私有，GPL 义务以公开分发为触发；公开化前再裁决：自写 FFI / 推 winfsp-rs 上游补 FLOSS 例外）。CI 增 winfsp 组合腿（用独立 deny 豁免或 skip licenses 检查） | deny.toml 红线 + 既有 driver-features 门控先例 |
| K39 | **绑定路线=winfsp-rs native API**（非 FUSE 兼容层）：单平台无复用诉求；兼容层税清单（研究档案三-1）在 native 下大半不存在。用其 `async-io` feature 的 AsyncFileSystemContext（或 sync trait + Handle::block_on，WF1 实证后定，记回传） | rclone 反面教材 + 深研报告§四-1 |
| K40 | **加法集成**：进程配置新键 `mount_backend = "webdav"`（默认，现行为字节不变）`\| "winfsp"`；WinFsp 未安装/初始化失败 → **K22 式可见降级**回 webdav 挂载（error 日志+横幅声明），绝不静默也不拒启；doctor 增 WinFsp 安装检测（服务/驱动文件探测） | K22 精神 + rclone DLL 缺失运行时检测 |
| K41 | **生命周期语义=rclone 三件套**：Flush 零副作用（只更新簿记，返回成功）；写提交唯一挂点=Release/cleanup；**句柄宽限期 5s**（最后一个 close 后延迟真拆窗口/连接，重开免重建） | 研究档案一（Flush 原则/宽限期）——WinFsp 存活生命线 |
| K42 | **读路径=语义核心复用**：open() 沿 K33 三重门（Stream→窗口 offset 读；Hydrate/Err→整文件水合）；窗口模型照搬 RangeFile（4MiB pos 锚定、惰性 seek、同位 no-op），WinFsp 适配器比 dav-server 的 16KiB 循环更贴（offset+length 直读）；**cache-first 增强（WF0）**：open_read 前置 is_cached 检查，命中→本地文件直供 | K33 沿用 + rclone read-your-cache |
| K43 | **写路径=StagedFile 复刻**：create→staged 临时文件；write(offset)→落 staged；Release→put_staged 提交入既有上传队列；**MiniRedir 特化面（PROPPATCH 207/FakeLs/空 PUT→LOCK 链）零进入 WinFsp 路径**——WinFsp 的 create/overwrite/cleanup 原生语义直接映射 | 写路径与 WebDAV 零耦合（盘点§5） |
| K44 | **Explorer 防打爆**：FindFiles/Readdir 一次带全部 stat（db 行，零网络）；GetFileInfo/GetVolumeInfo 走本地 db/装配期快照；`network_mode = true` 可选键（SMB 仿真盘语义，VolumePrefix 实现）——关缩略图扫描的逃生舱；未知大小 entry 不出现（db 行必有 size） | 研究档案三-2 |
| K45 | **错误模型=自有枚举→NTSTATUS 单点映射表**：NotFound→STATUS_OBJECT_NAME_NOT_FOUND、Exists→STATUS_OBJECT_NAME_COLLISION、缺密码→STATUS_ACCESS_DENIED、传输错→STATUS_IO_DEVICE_ERROR+日志兜底（rclone EIO 兜底等效）；不支持操作（SetSecurity 等可选回调）直接不注册回调=STATUS_NOT_IMPLEMENTED | 研究档案三-4 + 错误映射三函数改写 |
| K46 | **缓存协同 v1=cache-first only**；**区间账本块缓存（rclone Rs 模型，统一水合/直通双轨）挂账 Phase 3.5**——WinFsp 落地验证后按真实体验决定立项（避免一批做两个大系统，简洁优先） | 研究档案§四-2 |

## §2 架构设计

```
┌─ Windows 应用（Explorer / PotPlayer / 任意程序）─────────────┐
│   普通文件 API（CreateFile/ReadFile/WriteFile，本地盘语义）      │
└──────────────┬────────────────────────────────────────────┘
        WinFsp 内核驱动（winfsp-x.sys，NT 缓存管理器加速）
┌──────────────┴────────────────────────────────────────────┐
│ cloudkit-winfsp（新 L5 crate，feature=winfsp 默认关）          │
│  · FileSystemContext 适配器：getattr/findfiles/read/write/     │
│    cleanup/rename/setinfo → 语义核心调用                        │
│  · fh 句柄表（槽位复用，rclone cmount/fs.go 形态）              │
│  · CreateDisposition→打开意图矩阵（K43）；NTSTATUS 错误表（K45）│
│  · 生命周期：mount 点规整/就绪轮询/unmount/盘符（rclone 必抄清单）│
│  · async 桥：tokio Handle 注入（回调线程 block_on 或 async trait）│
└──────────────┬────────────────────────────────────────────┘
┌──────────────┴────────────────────────────────────────────┐
│ 语义核心（既有，零改动或最小改动）                                │
│  读：Vfs::open_read 三重门 → RangeFile 窗口 / hydrate 回退      │
│  写：StagedFile → Vfs::put_staged → 既有上传队列                │
│  元数据：db 行（baidu/local 权威索引）+ 装配期快照               │
└─────────────────────────────────────────────────────────┘
```

**新 crate 依赖**：仅 cloudkit-core + cloudkit-storage（L5 应用层，check_layers 合规）；winfsp = "0.13"（optional 于 cli，feature 传递）。

**关键映射表（适配器职责，全部单测钉死）**：
- CreateDisposition（CREATE_NEW/CREATE_ALWAYS/OPEN_EXISTING/OPEN_ALWAYS/TRUNCATE_EXISTING）→ 意图 {create, overwrite, open_ro, open_rw, truncate}（K43 矩阵）
- 读：GetFileSecurity/FindFiles 零网络（db）；Read(offset,len)→窗口；seek 语义由 offset 直读天然覆盖
- 写：Write→staged（任意 offset）；Cleanup(close)=提交点；Flush=簿记
- 宽限期：最后 close 起 5s 计时器，期间重开复用 staged/窗口句柄

## §3 批次（worktree `feat/winfsp`；WF0–WF2 可并行度低建议串行，每批 TDD 红→绿+三步门禁）

### WF0 cache-first 快赢（半小时级，独立价值）
- `Vfs::open_read` 前置 `cache.is_cached(rel)` 检查→命中返回 `StreamSource::Hydrate`（webdav/api 两面自动本地直供+record_access 续 LRU）；红→绿测试（缓存命中走本地、未命中走 Stream、加密行语义不变）；真机复验：Z: 盘重开已缓存 764MB 视频=本地速度零远端流量
- Commit: `feat(cache): cache-first for streaming reads`

### WF1 cloudkit-winfsp 骨架 + 只读元数据面
- crate 脚手架/feature 接线（cli optional+deny 豁免面）/async 桥（Handle 注入，两种形态实证选型记回传）/NTSTATUS 错误表（VfsError/io/StorageError→表，单测钉死）/getattr+findfiles（db 行直读，零网络——集成测试断言 transport 零调用）
- CI：winfsp feature 组合 build+clippy 腿接入
- Commit: `feat(winfsp): crate skeleton, async bridge, readonly metadata face`

### WF2 读路径（窗口模型 + 宽限期）
- open（K33 三重门分派）→ 窗口 offset 读句柄（RangeFile 模型搬运，惰性 seek/同位 no-op/EOF 语义）+ 句柄宽限期；mock 集成测试（open_range_calls 窗口断言复用 SR 观测面；加密/无能力回退 hydrate；缓存命中走 WF0 本地路径）
- Commit: `feat(winfsp): streaming read path with window model and handle grace`

### WF3 写路径 + 文件系统操作面
- Disposition→意图矩阵；staged 写（任意 offset）/Release 提交/cleanup；rename/can_delete/set_basic_info/remove（pending-upload 守卫沿用）；create_dir；GetVolumeInfo/SetVolumeLabel（进程级名）
- 测试：mock 全生命周期（创建→写→关→上传队列入队断言→重开读回）
- Commit: `feat(winfsp): write path via staged commits and fs operations`

### WF4 挂载生命周期 + cli 集成
- mount 点规整器（盘符选择复用 pick_drive_letter/`\?\` 拒收/volname 截断）+ 就绪/unmount 双轮询（10s 容忍）+ host start/stop；`mount_backend` 键三处同步+validate；run 多卷挂载分支（winfsp 臂+webdav 回退 K40）；stop 卸载序列；doctor WinFsp 检测；`cydrive mount/unmount` 分支；横幅（backend 标注）
- 测试：配置键/分派/回退逻辑 CI 面；挂载本体 `#[ignore]` 真机
- Commit: `feat(winfsp): mount lifecycle and cli integration`

### WF5 真机验收 + 收口（主会话）
- 验收矩阵（真机，对照 net use 留证）：①Explorer 浏览/拷贝/删除/重命名流畅 ②**PotPlayer 直接播放 764MB 视频——秒开+任意拖动**（核心验收）③多卷盘符并存（V/Y/Z winfsp 化或混合）④写入→上传队列→云端可见全链 ⑤cydrive stop 干净卸载全部盘符 ⑥WinFsp 未装场景回退 webdav（可临时停服务模拟或代码路径验证）⑦与 net use/Z: 行为对比记录
- 文档：README（winfsp 模式+前置条件）/AGENTS 阶段推进/onboarding 无涉/decisions K38–K46 全入档/tracker 全绿
- Commit + merge main + push；轻量发行物例行重建（winfsp 版与默认版双产物）

## §4 风险与回退

| 风险 | 概率 | 缓解/回退 |
|---|---|---|
| winfsp-rs async trait 成熟度（AsyncFileSystemContext 文档少） | 中 | WF1 双形态实证（sync+block_on 兜底——rclone 同款阻塞回调形态）；都不可用则升级为自写薄 FFI（K38 备选提前） |
| 回调阻塞拖慢 WinFsp 线程池→Explorer 卡 | 中 | 语义核心全异步+窗口有界；元数据面零网络（K44）；实测卡顿再引入 rclone「无超时」反例的 bounded 包装 |
| GPL 合规争议 | 低 | K38 隔离（默认关+私有分发）；公开化前专项裁决 |
| WinFsp 安装门槛 | 确定 | K40 自动回退 webdav + doctor 指引；README 前置条件说明 |
| Explorer/杀毒极端交互（rclone 血泪清单） | 中 | 必抄清单全量落 WF4（规整/轮询/network-mode/宽限期）；多挂载 VolumePrefix 唯一性校验 |
| 单卷回归漂移 | 低 | 每批 workspace 全绿+断言零改动纪律；winfsp feature 组合入 CI |

## §5 收口清单

- [ ] K38–K46 入档 decisions；tracker（docs/tracking/phase3-winfsp.md）全绿
- [ ] 默认构建行为零漂移（841 基线+新增，webdav/net use 路径原样）
- [ ] winfsp feature 组合 CI 腿；三步门禁+check_layers+scan_secrets
- [ ] 真机验收矩阵全过（§3-WF5）+ 对比留证
- [ ] merge main + push + 双产物（默认/带 winfsp）
