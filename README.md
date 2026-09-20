# rs-cloudfs

**多云存储平台** —— 在统一存储抽象之上，把多个云后端变成一个顺手的本地盘：WebDAV 挂载（Windows `Y:`/`Z:` 盘、Linux davfs2）、Web 仪表盘、多机元数据同步（LWW + SSE 准实时）、CLI 与远程 Bot 命令。单二进制、上传失败不丢数据、断点可续传、支持明文与加密两种存储模式。

**血统**：fork 自 [rs-CyDrive](https://git.metme.top/viccom/rs-CyDrive)（Telegram 无限云盘，Rust 完全重写版，全 git 历史保留），融合 PrivateCloudFS（Go 版多云聚合，位于 `E:\Go_codes\PrivateCloudFS`）的设计与实战经验重构为分层多云架构。行为基线：Python 版 CyDrive 兼容契约（telegram 驱动延续）。

## 架构（六层，详见 [docs/standards/architecture.md](docs/standards/architecture.md)）

```
L5 应用  cli │ webdav 网关 │ web 仪表盘 │ bot(telegram)
L4 服务  上传队列 │ 同步引擎(+sync-server) │ LRU 缓存 │ 加密(v1 GCM/v2 分块 AEAD 流式，新卷默认 v2)
L3 领域  VFS │ 元数据索引(SQLite) │ MetadataEvent 总线
L2 抽象  StorageDriver trait + 能力位 + 错误分类学 + conformance kit
L1 驱动  telegram │ baidu │ local │ sftp │ pan115 │ pan123 │ (未来: s3…)
```

**新后端接入 = 实现一个驱动 + 过 conformance 套件，上层全部能力（挂载/仪表盘/同步/CLI）自动可用。**驱动分两类（[driver-onboarding §10](docs/standards/driver-onboarding.md)）：后端有「按路径枚举」面的走 `StorageDriver` 宽面 + conformance（baidu/local/sftp）；没有的走 `CloudTransport` 窄面（telegram 先例——远端是消息，bot 读历史被平台拒绝，索引只存在于本地 db + sync）。**两类在编译开关上完全平权**（见下文 feature 门控）。

## 状态（2026-09-20）

| 阶段 | 内容 | 状态 |
|---|---|---|
| Phase -1 | 规范先行（架构约束/代码/接口/日志/文档五标准 + 本 README/AGENTS） | ✅ 完成 |
| 基线 | fork 自 rs-CyDrive 0.7.2（527 测试绿，telegram 后端生产可用） | ✅ 完成 |
| Phase 0 | crate 改名重排（cloudkit-*/ck-*），纯搬迁 + 层检查/秘密扫描 CI 门禁 | ✅ 完成 |
| Phase 1 | 百度 spike → StorageDriver 抽象落地 → 加密 v2 流式（0.8.0，617+ 测试绿，含真机冒烟两轮） | ✅ 完成 |
| Phase 2 | ck-local + ck-baidu + 组合根接线 + 端到端硬验收（0.9.0，740 测试绿；baidu/local E2E 通过、telegram 腿待独立测试 chat） | ✅ 完成 |
| Phase 2.5 | 多卷启用（Registry + 每实例配置 + 多盘挂载，方案一裁决） | ✅ 完成（0.10.0，810 测试绿；单进程三卷真机 E2E：local 加密 V: + tg Y: + baidu Z:，全过） |
| Phase 3 | WinFsp 类本地盘挂载（WF0–WF5，K38–K46） | ✅ 完成（867 测试绿；三卷真机验收七项全过，764MB 视频 open 55.4s→7-11ms；115/123/桌面端/自更新仍择机） |
| Phase 3.6 | 存储卷运行态装卸 + 卷级 enabled 键（RV0–RV3，K48–K51）+ 两轮审查修复（K55/K56、K58 5H+8M） | ✅ 完成 |
| 卷管理面 | Web `/volumes` 页 + 卷全生命周期命令协议（K57：SHOW/ENABLE/DISABLE/REBUILD/CREATE/UPDATE/DESTROY，凭据 write-only）+ 运行时后台 rebuild | ✅ 完成 |
| Web 体验 | ArtPlayer 内置播放器 + 复制链接 + 注入修复 + **默认加密方案 aead_v2**（新卷流式播放默认可用） | ✅ 完成（1069 测试绿） |
| Phase 4 | ssh/sftp 存储驱动（russh 0.63 + russh-sftp 3.0，ring 后端；K59/K60） | ✅ 完成（SF1–SF4：conformance 八断言绿；WSL2 OpenSSH 真机矩阵 11/11，吞吐上行 140.5 / 下行 67.7 MiB/s；SF5 多连接增强明确销账） |
| Phase 5 | 115 网盘存储驱动（ck-pan115：官方开放平台 device-code PKCE，K61/K62/K65/K69） | ✅ 完成（115-0…115-5：conformance 八断言绿 + 全装配接线；真机最小冒烟 3/3——上传回读逐字节 / Range 窗口逐字节 / 秒传同 fid 命中；完整矩阵列 `#[ignore]` 留证） |
| Phase 6 | 123 网盘存储驱动（ck-pan123：web API 直裁 + web 身份合规 D5，K63/K64/K76/K77） | ✅ 完成（123-0…123-5：conformance 八断言绿 + 12 装配点 + 六组合裁剪零告警；真机矩阵 3/3 + E2E 5/5——上传回读逐字节 / Range 窗口 / 秒传 Reuse / 分片差集 resume / 加密 aead_v2 全栈 / WebDAV 双模式 / setup 向导 / doctor。删除 = 回收站语义（D2 trash）；**免费档每日下载流量 ≈10GiB**（会员消解；驱动 traffic 预检 + 5113/5114 人话指引）） |

阶段计划与裁决：[docs/plans/2026-09-07-cloudfusion-foundation.md](docs/plans/2026-09-07-cloudfusion-foundation.md) ｜ 历史裁决：[docs/decisions.md](docs/decisions.md)

## 快速开始（六后端：telegram / baidu / local / sftp / pan115 / pan123，`backend` 配置键分发）

```powershell
cargo build --release
./cydrive.exe setup     # 选后端：telegram(bot token/chat_id) / baidu(appkey+refresh_token) / local(根目录) / pan115·pan123(扫码或账密换发 token)
./cydrive.exe doctor    # 体检（baidu：token 探活/直连声明；local：root 可写；sftp：连接探活+主机密钥指纹；pan115：开放平台连接探活；pan123：token 探活+每日流量余量）
./cydrive.exe run       # WebDAV :8080 → 自动挂载（默认 Y:；config drive_letter 可改）｜ 仪表盘 :8088 ｜ ctrl+c 或 cydrive stop
```

baidu 实例最小配置（config.toml）：`backend = "baidu"` + `baidu_app_key/baidu_app_secret/baidu_refresh_token`（或 env `CYDRIVE_BAIDU_*`，access_token 缺省由 refresh 换取）；`baidu_root` 默认 `/apps/cloudfs`。
local 实例：`backend = "local"` + `local_root = "<绝对路径>"`。
sftp 实例：`backend = "sftp"` + `sftp_host` / `sftp_username` + 认证（`sftp_password` **或** `sftp_private_key_path`，可选 `sftp_private_key_passphrase`）；`sftp_port` 默认 22、`sftp_root` 默认 `/`。**首次连接必须先接受服务器主机密钥**：`cydrive doctor` 会打印服务器实际指纹（D2：未接受前驱动拒连，绝无静默 TOFU），把该值填进 `sftp_host_fingerprint` 即完成接受；指纹此后变更会被恒拒（MITM 信号）。凭据可经 env `CYDRIVE_SFTP_PASSWORD` / `CYDRIVE_SFTP_PRIVATE_KEY_PASSPHRASE` 覆盖文件值（单卷模式；多卷模式下与其他 `CYDRIVE_*` 一样被忽略——K28，见下文注意事项）。卷文件里的 `sftp_private_key_path` 相对路径锚定该卷 home 目录（K21，同 db/cache）。
pan115 实例：`backend = "pan115"` + `pan115_token`（setup 向导扫码一次换发，refresh 自持）+ 可选 `pan115_root`（默认网盘根）。
pan123 实例：`backend = "pan123"` + `pan123_token`（setup 向导扫码或账密换发；token 90 天、无 refresh，失效重扫）+ 可选 `pan123_root`（纯数字目录 id，默认根）。**删除进 123 回收站**（D2 语义，远端可恢复）；**免费档每日下载流量 ≈10GiB**——超额得 `RateLimited`（会员消解），`cydrive doctor` 显示当日余量。
**全参数示例配置**（凭据已脱敏占位，可用 `cydrive status` 验证解析）：单卷 [`examples/single-volume.example.toml`](examples/single-volume.example.toml)（36 键全览，注释分组）；多卷 [`examples/multi-volume/`](examples/multi-volume/)（进程级 `config.example.toml` + 4 卷矩阵 `volumes/`：baidu-enc / baidu-plain / local-enc / local-plain——同后端多卷×加密开关，层次在文件布局：进程级键与卷级键分文件，见下节）。
权威后端（baidu/local）冷启动可 `cydrive rebuild` 从后端重建索引（明文集；加密实例走 sync）。多卷模式下若实例在运行，rebuild 自动经控制通道转发为各卷的后台 `REBUILD <名>`（受理即回，进度看 `LIST` 的 `rebuilding` 标记；实例不在线则照旧离线重建）。
新后端接入指南：[docs/standards/driver-onboarding.md](docs/standards/driver-onboarding.md)（conformance 套件 + 装配点 + E2E 拓扑）。

### 按需裁剪驱动（feature 门控）

六个驱动都是可选依赖（feature：`telegram` / `baidu` / `local` / `sftp` / `pan115` / `pan123`，默认全开 = 默认构建行为不变）：

```powershell
cargo build --release                                              # 全量（默认六驱动）
cargo build --release --no-default-features --features local       # 纯本地
cargo build --release --no-default-features --features local,baidu # 本地+百度
```

缺驱动的二进制运行到对应表面时得到可行动报错（给出 rebuild 命令与 backend 改法，而非隐藏命令）；`cydrive --version` 显示本构建的驱动清单，如 `cydrive 0.10.0 (drivers: telegram, baidu, local, sftp, pan115, pan123)`，全关构建显示 `(drivers: none)`。裁剪掉 `sftp` 时整个 russh 协议栈都不进依赖图。

**三平台构建**：Windows（原生，主力）/ Linux（原生，WSL2 实测含 sftp 真机连通）均可直接编译运行；macOS 编译面已验证（交叉工具链可出 Mach-O 二进制，挂载功能未实现、运行未实机验证，SDK 许可有灰色地带）——完整指南见 [docs/platform-builds.md](docs/platform-builds.md)（命令、实测数字、四个 macOS 交叉坑的解、坑速查表）。

### 流式读（视频直接播放）

非加密文件 + 支持 Range 的后端（baidu/local/telegram）的读取走 **Range 直通**：请求哪段拉哪段（4MiB 窗口），不再整文件下载后才能播放——764MiB 视频首字节 <1ms、1MiB 片段 ~0.2s。生效面：WebDAV 盘符、仪表盘播放器（**ArtPlayer**，本地内置无外联 CDN，支持常见音视频格式 + 播放失败自动回退原生素材）、`/api/download` URL（可直接喂 PotPlayer/VLC；文件管理页每行有「复制链接」按钮一键取直链）。**加密默认 `aead_v2`**（2026-09-14 起，新卷默认——分块 AEAD 按需拉密文窗口实时解密、逐 chunk 验签，同样支持 Range 流式读）；`gcm` v1 保留显式可选（整文件模式，Python 基线兼容形态），不支持 Range 的后端自动回退整文件模式。

> **盘符路径播放大视频的固有限制**：Windows 的 WebDAV 重定向器对播放器打开的大文件会先整文件缓存（实测读 4MB 实际拉全文件），764MB 视频会以全速下载十几秒后可能触发客户端 RPC 故障。**大视频请用 URL 直喂播放器**（PotPlayer/VLC 打开 URL：`http://127.0.0.1:8485/vol/baidu/<文件名>`，真流式秒开）；盘符适合常规文件操作。

> Windows 挂载盘符（Z: 等）播放 >50MB 视频需一次性调整 WebClient 服务限制（机器级，rclone/alist 用户同样需要）：
> ```
> reg add HKLM\SYSTEM\CurrentControlSet\Services\WebClient\Parameters /v FileSizeLimitInBytes /t REG_DWORD /d 0xffffffff /f
> net stop webclient && net start webclient
> ```

### WinFsp 类本地盘挂载（mount_backend = "winfsp"，Phase 3）

把存储卷挂成**本地文件系统语义盘符**（Win32_LogicalDisk `FileSystem="cydrive"`），绕开 WebDAV 重定向器的整文件缓存税——764MB 视频 open 从 net use 的 55.4s 降到 7-11ms，随机拖动按 4MiB 窗口拉取（冷 seek ~0.3s/跳、缓存热 ~2ms）。

- **前置条件**：安装 [WinFsp 运行时](https://winfsp.dev/)。未安装（或二进制未编译 winfsp feature）时自动回退 webdav/net use 挂载（error 日志 + 横幅声明 + 逐卷 fallback 标注，绝不拒启）；`cydrive doctor` 含 WinFsp 检测项。
- **开启方式**：config.toml 进程级键 `mount_backend = "winfsp"`（默认 `"webdav"`，行为不变；多卷模式下各卷盘符统一走该后端）。
- **构建**：`cargo build -p cloudkit-cli --features winfsp`（feature 默认关；需 MSVC 工具链 + libclang——winfsp-sys 的 bindgen 依赖）。
- **许可注意**：该 feature 引入 GPL-3.0 的 winfsp-rs（无 FLOSS 例外）——默认构建零 winfsp 依赖、不受影响；`--features winfsp` 产物当前仅私有分发（K38）。
- **当前形态**：读 = 4MiB 窗口流式 + 缓存命中本地直供；写 = staged 临时文件，关闭句柄时提交入既有上传队列（上传排空前 rename/delete 会被短暂拒绝——防孤立唯一副本，与 webdav 面同语义）。

### 多卷模式（一个进程多个存储卷，Phase 2.5）

config.toml 只留进程级键 + `volumes_dir`；每卷一份 `volumes/<name>.toml`（卷名=文件名，卷内相对路径落在各自的 `volumes/<name>/` 主目录）。三卷示例（local + telegram + baidu）：

```toml
# config.toml（进程级）
volumes_dir = "volumes"
webdav_host = "127.0.0.1"
webdav_port = 8080
enable_web_ui = true
web_ui_port = 8088
```

```toml
# volumes/local.toml —— 卷作用键（backend/凭据/db_path/drive_letter...）
backend = "local"
local_root = "root"          # 相对路径 → volumes/local/root
drive_letter = "V"           # 显式声明才挂载（auto_mount_drive 门控）
```

```toml
# volumes/tg.toml
backend = "telegram"
bot_token = "..."            # 凭据照旧不入库（R3）
chat_id = 123456789
# encrypt/加密键组与单卷写法相同，按卷独立加解密
drive_letter = "Y"
```

```toml
# volumes/baidu.toml
backend = "baidu"
baidu_app_key = "..."
baidu_app_secret = "..."
baidu_refresh_token = "..."
drive_letter = "Z"
```

- **挂载**：单 WebDAV 端口，每卷一个子路径 `http://127.0.0.1:8080/vol/<name>`（声明了 `drive_letter` 的卷按 `cydrive run` 自动挂载为各自盘符）。
- **仪表盘**：单端口 `:8088`，卷切换 tabs + 跨卷汇总；API 带 `?volume=<name>`（多卷下无参卷作用 API 返回 400 + 卷清单）。
- **卷管理页 `/volumes`**（配置态，与使用态首页互跳）：卷表（Name/Backend/Status/Drive/Pending/Size + Actions）+ 四张统计卡 + 实例级总用量卡，4s 轮询 `/api/volumes`（行带 `pending` 队列计数；Failed 卷为 `null`）。Actions 列：[Refresh]（从远端后端后台重建索引；telegram/加密卷不支持，tooltip 指 `cydrive sync`）、[Edit]、[Unmount]/[Enable]/[Disable]、[Delete]（删除卷，两步确认 modal：第一步预告面板（将删卷文件+注册/盘符；**本地数据目录默认保留、远端数据永不触碰**）+ purge_local 复选框，第二步输入卷名逐字匹配才能确认）。**新增/编辑表单**（Add Volume 按钮 / `/volumes#add` 锚点 / 表内 [Edit]）：backend 三选一切换凭据组（telegram token+chat / baidu 四键 / local 根目录），盘符可空（=不声明）、高级折叠区（加密三键/chunk/sync 三键——留空=不写键走默认）；校验权威在后端命令，ERR 原文红框回显且表单保留值；编辑由 `SHOW` 预填（**凭据只读占位符「已设置（留空=不修改）/未设置」，值永不出后端**），保存=REMOVE+ADD 重装配（顶部明示 + pending 上传时 confirm + 注释丢失提示）。写路由族 `POST /api/volumes`（新增）、`POST /api/volumes/<name>`（编辑）与 `POST /api/volumes/<name>/destroy`（删除，body `{"confirm":bool,"purge_local":bool}`——confirm 缺省 false 回预告与 `confirm_required:true`，true 才执行）经进程内回调缝走与控制通道同一串行队列（180s 装配预算 / destroy 120s）；非 loopback 绑定默认只读（`allow_remote_admin = true` 显式解禁）。只读配置端点 `GET /api/volumes/<name>/config` 同缝走 `SHOW`（**凭据值永不出后端**——回复里凭据键只有 `{"set": true/false}`）。单卷实例访问 `/volumes` 得到说明页。`/api/volumes*` 族挂同源（Origin/Referer）校验，跨源 403。
- 运维：`cydrive volumes` 列卷清单、`cydrive doctor` 逐卷体检、`cydrive status` 逐卷 db 统计 + 在线实例的运行态卷表、`cydrive setup --multi` 生成骨架；`cydrive stop` 一次停全部卷。
- **运行态装卸**（不停进程）：回环控制通道支持 `ADD <name>`（按 `volumes/<name>.toml` 装配并挂载，`enabled = false` 或凭据/盘符有问题则拒绝且不影响兄弟卷）、`REMOVE <name>`（排空上传 → 卸盘符 → 摘除，超时/占用即中止、卷保持完好；不碰卷文件，重启后按文件回来）、`ENABLE/DISABLE <name>`（写 `enabled` 键 + 装配/卸载——持久禁用的命令形态）、`REBUILD <name>`（后台重建索引，受理即回）、`CREATE <name> <json>` / `UPDATE <name> <json>`（受控生成/重写卷 toml：键空间=卷级键、写盘前全量校验（拒绝不落盘）、凭据 write-only（UPDATE 载荷空串/缺失=保留原值）、UPDATE 保存=REMOVE+ADD 重装配且回复明示手写注释丢失）、`LIST`（卷名/状态/盘符/backend/pending 一行一卷）、`SHOW <name>`（卷文件显式配置的单行 JSON，凭据只回 set 布尔）、`CONFIGS`（卷文件全集行）、`DESTROY <name>`（两段删除：裸命令=零副作用预告（含保留/远端声明与确认指路）；`DESTROY <name> confirm` 才执行——运行中先走 REMOVE 同一安全序（排空拒绝=整体拒绝绝不半删）再删卷文件（幂等）；`confirm purge_local` 第三词才连带删本地数据目录（`volumes/<name>/` 整目录；失败不回滚但回复明示残留路径）；**远端数据永不触碰**）。命令串行处理，回复 `OK:`/`ERR:` 可行动文本；`cydrive status` 的多卷输出已带 LIST 转发。
- 注意：多卷模式下 `CYDRIVE_*` 配置覆盖 env 被忽略（K28，防跨卷串味）；单卷模式行为与旧版字节兼容。

多机同步（可选）：部署 `cydrive-sync-server`（[部署文档](docs/sync-server-deployment.md)）→ 各机 config.toml 写 `sync_url`/`sync_secret`，上传成功后秒级同步到其他机器。

完整使用文档：[docs/cydrive-usage.md](docs/cydrive-usage.md)

## 许可

MIT（与 rs-CyDrive 一致）。
