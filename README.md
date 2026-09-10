# rs-cloudfs

**多云存储平台** —— 在统一存储抽象之上，把多个云后端变成一个顺手的本地盘：WebDAV 挂载（Windows `Y:`/`Z:` 盘、Linux davfs2）、Web 仪表盘、多机元数据同步（LWW + SSE 准实时）、CLI 与远程 Bot 命令。单二进制、上传失败不丢数据、断点可续传、支持明文与加密两种存储模式。

**血统**：fork 自 [rs-CyDrive](https://git.metme.top/viccom/rs-CyDrive)（Telegram 无限云盘，Rust 完全重写版，全 git 历史保留），融合 PrivateCloudFS（Go 版多云聚合，位于 `E:\Go_codes\PrivateCloudFS`）的设计与实战经验重构为分层多云架构。行为基线：Python 版 CyDrive 兼容契约（telegram 驱动延续）。

## 架构（六层，详见 [docs/standards/architecture.md](docs/standards/architecture.md)）

```
L5 应用  cli │ webdav 网关 │ web 仪表盘 │ bot(telegram)
L4 服务  上传队列 │ 同步引擎(+sync-server) │ LRU 缓存 │ 加密(v1 GCM/v2 分块 AEAD 流式)
L3 领域  VFS │ 元数据索引(SQLite) │ MetadataEvent 总线
L2 抽象  StorageDriver trait + 能力位 + 错误分类学 + conformance kit
L1 驱动  telegram │ baidu │ local │ (未来: 115/123/s3…)
```

**新后端接入 = 实现一个驱动 + 过 conformance 套件，上层全部能力（挂载/仪表盘/同步/CLI）自动可用。**

## 状态（2026-09-07）

| 阶段 | 内容 | 状态 |
|---|---|---|
| Phase -1 | 规范先行（架构约束/代码/接口/日志/文档五标准 + 本 README/AGENTS） | ✅ 完成 |
| 基线 | fork 自 rs-CyDrive 0.7.2（527 测试绿，telegram 后端生产可用） | ✅ 完成 |
| Phase 0 | crate 改名重排（cloudkit-*/ck-*），纯搬迁 + 层检查/秘密扫描 CI 门禁 | ✅ 完成 |
| Phase 1 | 百度 spike → StorageDriver 抽象落地 → 加密 v2 流式（0.8.0，617+ 测试绿，含真机冒烟两轮） | ✅ 完成 |
| Phase 2 | ck-local + ck-baidu + 组合根接线 + 端到端硬验收（0.9.0，740 测试绿；baidu/local E2E 通过、telegram 腿待独立测试 chat） | ✅ 完成 |
| Phase 2.5 | 多卷启用（Registry + 每实例配置 + 多盘挂载，方案一裁决） | ✅ 完成（0.10.0，810 测试绿；单进程三卷真机 E2E：local 加密 V: + tg Y: + baidu Z:，全过） |
| Phase 3 | 115/123/多卷挂载/桌面端/自更新（择机） | ⬜ |

阶段计划与裁决：[docs/plans/2026-09-07-cloudfusion-foundation.md](docs/plans/2026-09-07-cloudfusion-foundation.md) ｜ 历史裁决：[docs/decisions.md](docs/decisions.md)

## 快速开始（三后端：telegram / baidu / local，`backend` 配置键分发）

```powershell
cargo build --release
./cydrive.exe setup     # 选后端：telegram(bot token/chat_id) / baidu(appkey+refresh_token) / local(根目录)
./cydrive.exe doctor    # 体检（baidu：token 探活/直连声明；local：root 可写）
./cydrive.exe run       # WebDAV :8080 → 自动挂载（默认 Y:；config drive_letter 可改）｜ 仪表盘 :8088 ｜ ctrl+c 或 cydrive stop
```

baidu 实例最小配置（config.toml）：`backend = "baidu"` + `baidu_app_key/baidu_app_secret/baidu_refresh_token`（或 env `CYDRIVE_BAIDU_*`，access_token 缺省由 refresh 换取）；`baidu_root` 默认 `/apps/cloudfs`。
local 实例：`backend = "local"` + `local_root = "<绝对路径>"`。
权威后端（baidu/local）冷启动可 `cydrive rebuild` 从后端重建索引（明文集；加密实例走 sync）。
新后端接入指南：[docs/standards/driver-onboarding.md](docs/standards/driver-onboarding.md)（conformance 套件 + 装配点 + E2E 拓扑）。

### 按需裁剪驱动（feature 门控）

三个驱动都是可选依赖（feature：`telegram` / `baidu` / `local`，默认全开 = 默认构建行为不变）：

```powershell
cargo build --release                                              # 全量（默认三驱动）
cargo build --release --no-default-features --features local       # 纯本地
cargo build --release --no-default-features --features local,baidu # 本地+百度
```

缺驱动的二进制运行到对应表面时得到可行动报错（给出 rebuild 命令与 backend 改法，而非隐藏命令）；`cydrive --version` 显示本构建的驱动清单，如 `cydrive 0.10.0 (drivers: telegram, baidu, local)`，全关构建显示 `(drivers: none)`。

### 流式读（视频直接播放）

非加密文件 + 支持 Range 的后端（baidu/local/telegram）的读取走 **Range 直通**：请求哪段拉哪段（4MiB 窗口），不再整文件下载后才能播放——764MiB 视频首字节 <1ms、1MiB 片段 ~0.2s。生效面：WebDAV 盘符、仪表盘播放器、`/api/download` URL（可直接喂 PotPlayer/VLC）。加密文件与不支持 Range 的后端自动回退整文件模式。

> **盘符路径播放大视频的固有限制**：Windows 的 WebDAV 重定向器对播放器打开的大文件会先整文件缓存（实测读 4MB 实际拉全文件），764MB 视频会以全速下载十几秒后可能触发客户端 RPC 故障。**大视频请用 URL 直喂播放器**（PotPlayer/VLC 打开 URL：`http://127.0.0.1:8485/vol/baidu/<文件名>`，真流式秒开）；盘符适合常规文件操作。

> Windows 挂载盘符（Z: 等）播放 >50MB 视频需一次性调整 WebClient 服务限制（机器级，rclone/alist 用户同样需要）：
> ```
> reg add HKLM\SYSTEM\CurrentControlSet\Services\WebClient\Parameters /v FileSizeLimitInBytes /t REG_DWORD /d 0xffffffff /f
> net stop webclient && net start webclient
> ```

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
- 运维：`cydrive volumes` 列卷清单、`cydrive doctor` 逐卷体检、`cydrive status` 逐卷 db 统计、`cydrive setup --multi` 生成骨架；`cydrive stop` 一次停全部卷。
- 注意：多卷模式下 `CYDRIVE_*` 配置覆盖 env 被忽略（K28，防跨卷串味）；单卷模式行为与旧版字节兼容。

多机同步（可选）：部署 `cydrive-sync-server`（[部署文档](docs/sync-server-deployment.md)）→ 各机 config.toml 写 `sync_url`/`sync_secret`，上传成功后秒级同步到其他机器。

完整使用文档：[docs/cydrive-usage.md](docs/cydrive-usage.md)

## 许可

MIT（与 rs-CyDrive 一致）。
