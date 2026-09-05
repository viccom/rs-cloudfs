# cydrive 使用文档

> 适用版本：0.5.x ｜ 单二进制，含全部子命令；Windows / Linux（含 WSL）同套 CLI

## 1. 它是什么

cydrive 把一个 Telegram bot 对话变成**无限云盘**：

- **WebDAV 服务**（默认 `127.0.0.1:8080`）→ Windows 资源管理器直接挂成 `Y:` 盘，或 Linux 经 davfs2 挂载；
- **Web 仪表盘**（默认 `127.0.0.1:8088`）→ 浏览器上传/下载/删除/看队列；
- **Bot 命令** → 在 Telegram 对话里 `/stats` `/ls` `/get` …；
- **CLI** → push/pull 大文件、缓存管理、体检、元数据多机同步。

文件按块（默认 1900MB/块）上传到 Telegram；本地盘只保存索引（SQLite）+ 可选的 LRU 磁盘缓存，读取时按需水合。

## 2. 安装与布局

单文件二进制放进任意目录即为工作目录（所有产物生成在 cwd）：

```
your-dir/
├── cydrive(.exe)          # 程序本体
├── config.toml            # 配置（setup/migrate 生成，可手改）
├── cydrive_meta.db        # 盘索引（SQLite）
├── cynet_bot_session.session  # Telegram 会话（免重复登录）
├── Telegram_Cache/        # 磁盘缓存（LRU，受 cache_limit_gb 约束）
└── Telegram_Drive/        # （保留路径，上传走缓存 staging）
```

升级 = 换二进制文件再启动；`stop` 后替换即可，数据格式向前兼容。

## 3. 快速开始

```bash
# ① 向导：bot token（@BotFather 建 bot 获得）、chat_id、代理等，
#    凭据存入系统凭据管理器（Windows 凭据管理器 / macOS 钥匙串 / Linux Secret Service）
./cydrive setup

# ② 体检（配置/db/端口/注册表/Telegram 连通，输出 Ok/Warn 清单）
./cydrive doctor

# ③ 启动：连接 Telegram → WebDAV → 仪表盘 → 自动挂载 Y:（Windows）
./cydrive run

# 另开终端：
./cydrive status    # 查看运行状态
./cydrive stop      # 优雅停机（排干上传、卸载盘）
```

**网络前提（中国大陆环境）**：直连 Telegram 通常不通，需 SOCKS5 代理（如 Clash）。在 `config.toml` 设 `proxy_url = "socks5://127.0.0.1:7897"`。WSL/Linux 内填宿主代理地址（如 `socks5://172.x.x.1:7897`），且代理需开启"允许局域网"。

## 4. 日常使用

### 4.1 盘（WebDAV）

```bash
./cydrive mount      # Windows: 挂到配置盘符（默认 Y:，被占用时自动挑空闲盘符）；Linux: davfs2 挂到 $HOME/CyDrive
./cydrive unmount    # 卸载
./cydrive fix-reg    # Windows 管理员：注册表调优（解除 Explorer 单文件 4GB-1 等限制）+ 重启 WebClient
```

挂上后像本地盘一样拖拽拷入/拷出/播放。首次挂载异常先跑 `fix-reg`（需管理员）再 `mount`。

### 4.2 仪表盘

浏览器开 `http://127.0.0.1:8088`：文件列表、上传（单次上限 1900MB，受理即回、后台传）、下载、删除、队列/统计。API 同路径：`/api/files` `/api/stats` `/api/list?path=` `/api/upload` `/api/delete` `/api/download`（支持 Range）/`/api/queue`。

### 4.3 大文件走 CLI（绕开一切上传限制）

```bash
./cydrive push "D:\movie.mkv" --dest /movies/   # 上传，自动分块；>1900MB 也无碍
./cydrive push "/home/u/big.iso"                # Linux 同样
./cydrive pull /movies/movie.mkv ./out.mkv      # 下载（未缓存时从 Telegram 水合）
```

### 4.4 缓存

```bash
./cydrive cache stats   # 缓存目录、占用、上限
./cydrive cache clear   # 清理已上传文件的缓存副本；未上传完成的暂存副本会保留（数据安全）
```

### 4.5 Bot 命令（Telegram 对话内）

`/help` `/stats`（用量统计）`/search <词>`（前缀搜索）`/get <路径或名>`（发回文件）`/ls [路径]` `/mkdir <路径>` `/rm <路径>` `/quota` `/queue`（上传队列状态）。上传降级会主动 bot 通知。

### 4.6 多机/换机同步（元数据）

配置 `sync_url` 指向自建的 cydrive-sync-server 后：`run` 自动周期同步（默认 300s），或 `cydrive sync` 手动同步一轮。空盘新机器 sync 一次即可看到整个盘的文件列表。部署与服务端运维见 **[sync-server-deployment.md](sync-server-deployment.md)**。不配 `sync_url` 则该功能完全关闭。

## 5. 配置参考（config.toml）

| 键 | 默认 | 说明 |
|---|---|---|
| `bot_token` / `chat_id` | 必填 | bot 身份与目标对话。**同 token+chat = 同一个盘**（同步命名空间亦由此派生） |
| `api_id` / `api_hash` | 内置 | Telegram 应用凭据，一般不动 |
| `proxy_url` | 无 | SOCKS5 代理，如 `socks5://127.0.0.1:7897`；留空清除 |
| `storage_path` / `cache_path` / `db_path` | `./Telegram_Drive` `./Telegram_Cache` `./cydrive_meta.db` | 路径布局 |
| `webdav_host` / `webdav_port` | `127.0.0.1` / `8080` | WebDAV 监听 |
| `web_ui_host` / `web_ui_port` | `127.0.0.1` / `8088` | 仪表盘监听；`enable_web_ui = false` 可关 |
| `drive_letter` | `Y:` | Windows 挂载盘符 |
| `auto_mount_drive` | `true` | run 时自动挂载、stop 时自动卸载 |
| `mount_point` | `$HOME/CyDrive` | Linux 挂载点（绝对路径；Windows 忽略） |
| `chunk_size_mb` | `1900` | 分块大小 |
| `cache_limit_gb` | `20` | 本地缓存上限（LRU 驱逐） |
| `upload_workers` / `queue_capacity` | `2` / `256` | 上传并发与队列容量 |
| `hydrate_timeout_secs` | `1800` | 单次下载水合的超时（大文件+慢带宽环境调大） |
| `enable_encryption` + `encryption_password` | `false` | 两者**同时**设置才启用上传加密（与 Python 版互操作） |
| `sync_url` | 无 | 元数据同步服务端地址（http/https）；不设=关 |
| `sync_interval_secs` | `300` | run 内自动同步周期（1..=86400） |

**凭据优先级**：环境变量 > config.toml > 系统凭据管理器（service `cydrive`）。token/加密密码推荐放凭据管理器（setup 自动做），config 里留空。

**常用环境变量**：`CYDRIVE_BOT_TOKEN` `CYDRIVE_CHAT_ID` `CYDRIVE_PROXY_URL` `CYDRIVE_SYNC_URL` `CYDRIVE_SYNC_SECRET`（同步密钥，**只走环境变量不进 config**）`CYDRIVE_CHUNK_SIZE_MB` `CYDRIVE_DRIVE_LETTER` `CYDRIVE_WEBDAV_PORT` `CYDRIVE_WEB_UI_PORT` `CYDRIVE_ENABLE_ENCRYPTION` `RUST_LOG`（日志级别，如 `info`/`debug`）。

旧版 `config.json` 仍可被发现并提示迁移（`cydrive migrate`）；新调优键写在 json 里会被拒收——请用 toml。

## 6. 行为边界与已知语义

- **删除**：从盘上移除的是索引行；Telegram 对话里的原始消息**保留**（与 Python 版一致的语义，误删可用 /get 或手机端救回文件）。
- **上传限制**：Windows Explorer 单文件硬上限 4GB-1（WebClient 平台限制，`fix-reg` 已解到该上限）；Web UI 单次 1900MB；**CLI push 无限制**。
- **读取延迟**：未缓存文件首次打开需从 Telegram 下载（受代理带宽约束，实测 ~0.5-2MB/s 量级）；大文件注意 `hydrate_timeout_secs`。
- **加密**：整文件 v1 格式加密后分块；Python 版与 Rust 版互相可解。已上传的旧明文文件不受影响。
- **同机互斥**：服务运行中（run）避免同时跑 push/pull 等传输命令（session 锁冲突风险）；stop/status 按同一工作目录约定工作。

## 7. 故障排查

| 现象 | 处置 |
|---|---|
| `run` 卡在连接/报 10060 超时 | 代理未开或 `proxy_url` 不对；WSL 用宿主 IP 且开"允许局域网" |
| Y: 盘不出现 | `cydrive fix-reg`（管理员）→ `cydrive mount`；检查 WebClient 服务运行 |
| Explorer 拷贝中途失败回滚 | 多为网络闪断，队列会自动重试；`cydrive status` 看队列 |
| 下载大文件超时 | 调大 `hydrate_timeout_secs` |
| Linux 挂载失败（davfs2） | `/etc/davfs2/davfs2.conf` 设 `ask_auth 0`（本地服务无认证场景） |
| 端口冲突 | `CYDRIVE_WEBDAV_PORT`/`CYDRIVE_WEB_UI_PORT` 或 config 改端口 |
| 一键体检 | `cydrive doctor`（配置/DB/端口/注册表/Telegram 连通全查） |
| 从 Python 版迁移 | `cydrive migrate`（json→toml、凭据入钥匙串、db/缓存原地采用，幂等可重跑） |

深入的设计取舍与裁决记录见 `docs/decisions.md`；同步服务端部署见 `docs/sync-server-deployment.md`。
