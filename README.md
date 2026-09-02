# rs-CyDrive

**CyDrive 的 Rust 完全重写** —— 把 Telegram 变为无限云盘：本地 WebDAV 服务挂载为 Windows `Y:` 盘，配 Web 仪表盘与远程 Bot 命令。单二进制、上传失败不丢数据、断电可续传。

原 Python 版（行为基线）：[CyDrive](https://github.com/thecynetx/CyDrive)

## 状态（2026-09-02）

**M0–M6 全部自动化项已交付**：272 个离线测试全绿（fmt / clippy -D warnings / cargo test 三步门禁），`release` 单文件二进制 13.1MB（前端资产编译期嵌入）。剩余为真机验收项（真实 bot token 冒烟、Explorer 挂盘实测等），见 [AGENTS.md](AGENTS.md)「待人工总清单」节。

## 快速开始

```powershell
# 构建（或直接取 target/release/cydrive.exe）
cargo build --release

# 首次配置（交互向导；凭据存入 Windows 凭据管理器，配置文件不留明文）
cydrive.exe setup

# —— 从 Python 版迁移的用户（同数据目录执行；DB/缓存零拷贝续用）——
cydrive.exe migrate

# 首次使用建议（管理员）：WebClient 4GB 上限 + Basic 认证调优
cydrive.exe fix-reg

# 启动：WebDAV :8080 + Web 仪表盘 :8088 + 自动挂载 Y: 盘；Ctrl+C 优雅退出
cydrive.exe run
```

## 子命令

| 命令 | 作用 |
|---|---|
| `run` | 全栈启动（含启动时 pending 上传重入队、入站文件索引、Bot 命令应答） |
| `mount` / `unmount` | `net use` 映射盘符（偏好 `Y:`，回退 `Z,Y,X,W,V,U,T,S`） |
| `fix-reg` | 注册表调优（`FileSizeLimitInBytes=4GB-1`、`BasicAuthLevel=2`）+ 重启 WebClient |
| `migrate` | Python 版 `config.json` → 脱敏 `config.toml` + 凭据入系统凭据库 + 采用现有 db/缓存 |
| `stats` | 存储统计表格 |
| `doctor` | 一键诊断（config/db/缓存/端口/注册表/WebClient） |
| `setup` | 交互式配置向导 |

配置优先级：环境变量 `CYDRIVE_*` > `config.toml` > 系统凭据库。会话持久化（重启免重登），文件名沿用契约 `cynet_bot_session.session`。

## 功能与兼容

- **WebDAV**（`:8080`，loopback 无认证）：虚拟文件系统直查 SQLite，Explorer 拖入/改名/删除/Range 下载；10TB 配额与 ETag 语义与 Python 版契约一致
- **Web 仪表盘**（`:8088`）：沿用 Python 版前端（零改动嵌入）；上传（大文件无 2MB 限制）/浏览/删除/下载（支持 Range 拖进度）/统计/队列徽章
- **Bot**：`/stats` `/search` `/get`（`/get` 为补齐实现——Python 版只承诺未实现）；往 Bot 发文件即入盘（元数据即时可见，按需水合）
- **加密**：AES-256-GCM v1 格式与 Python 版**双向兼容**（互相可解）；上传侧整文件加密、下载侧自动解密
- **对 Python 版已知缺陷的修复**：上传失败不删本地数据、断电后 pending 自动续传、MOVE/重命名可用（Python 版为 500）、有界上传队列带重试/降级、优雅退出、凭据不明文落盘

## 架构

```
crates/
  cydrive-core/       领域层：VFS / SQLite 元数据 / LRU 缓存 / AES-GCM / 分块 /
                      上传队列 / 入站索引 / Bot 命令 / 凭据抽象
  cydrive-telegram/   CloudTransport 的 grammers 实现（契约 caption/命名/Range 换算）
  cydrive-webdav/     dav-server 适配 + hyper 服务
  cydrive-web/        axum 仪表盘 + rust-embed 前端
  cydrive-platform/   net use / 注册表 / WebClient（#[cfg] 平台隔离）
  cydrive-cli/        clap 子命令 + run 编排（bin: cydrive）
  vendor/grammers-session/  上游 in-tree vendor（libsql→rusqlite 后端，
                      [patch.crates-io] 重定向；见 VENDOR.md）
```

关键设计（trait 注入传输层、兼容契约冻结清单、历史裁决）见
[docs/rust-rewrite-design.md](docs/rust-rewrite-design.md) 与 [docs/decisions.md](docs/decisions.md)。

## 开发

```bash
cargo test --workspace --no-fail-fast   # 272 测试 + 3 真机 #[ignore] 项
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo test -p cydrive-core --test perf_read_path -- --ignored --nocapture  # 10万行读路径基准
```

## 许可

MIT（与原项目一致）；vendor 的 grammers-session 遵循其上游 MIT OR Apache-2.0（双 LICENSE 文件已入库）。
