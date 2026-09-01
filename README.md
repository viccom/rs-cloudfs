# rs-CyDrive

**CyDrive 的 Rust 完全重写** —— 把 Telegram 变为无限云盘：本地 WebDAV 服务挂载为 Windows `Y:` 盘（或 Linux/macOS 目录），配 Cyberpunk Web 仪表盘与远程 Bot 命令。单二进制、真·流式零磁盘、上传失败不丢数据。

原 Python 版（行为基线）：[CyDrive](https://github.com/thecynetx/CyDrive)

## 状态

设计阶段（M0 引导）。权威设计文档：[docs/rust-rewrite-design.md](docs/rust-rewrite-design.md)
（选型依据、crate 划分、兼容契约、里程碑 M0–M6 与验收标准）

## 核心选型

- **grammers** — Telegram MTProto 客户端（bot 登录 / 流式上传下载）
- **dav-server** — WebDAV 服务（`DavFileSystem` 虚拟后端直查 SQLite）
- **axum** — Web 仪表盘 REST + 嵌入式静态前端
- **rusqlite + tokio-rusqlite** — WAL 元数据引擎（与 Python 版 db 文件兼容）
- **tokio** — 单进程多线程 runtime（无跨线程桥接）

## 许可

MIT（与原项目一致）
