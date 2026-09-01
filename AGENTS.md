# rs-CyDrive — Agent 工作须知

## 项目信息
- 项目：CyDrive 的 Rust 完全重写（Telegram 无限云盘：本地 WebDAV 服务挂载 Windows `Y:` 盘 + Web 仪表盘 :8088 + Bot 命令）
- 技术栈：Rust（edition 2024）/ tokio / grammers（MTProto）/ dav-server（WebDAV）/ axum / rusqlite
- **权威设计文档：`docs/rust-rewrite-design.md`**——crate 划分、选型依据、已核实的 API 面、里程碑与验收标准都在其中，动工前必读
- 行为基线与兼容契约：见 `E:\GitHub\CyDrive`（Python 版）根目录 `AGENTS.md` 的「契约」节——DB schema、加密格式、分块命名/caption、端口、注册表行为，破坏即与现有用户数据不兼容

## 当前阶段
M0 之前的引导阶段（尚无 Cargo workspace）。首个任务按设计文档 M0 里程碑搭 workspace 脚手架。

## 硬性规则
- 兼容红线（设计文档「兼容契约」节）不经用户明确同意不得改动；契约测试必须双向（Python 生成样本 ↔ Rust 实现）
- grammers 用 git 依赖锁精确 commit（官方 minor 即换 TL layer，视同 breaking）
- 质量门禁（照搬 M0 定义）：`cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 全绿才算完成；`cargo deny check` 把关许可证
- 错误处理：库 crate 用 thiserror，CLI 顶层用 anyhow，Result 模式；生产路径禁止 unwrap/expect
- 平台代码 `#[cfg(windows)]`/`#[cfg(unix)]` 隔离，Windows 模块必须保证在 Linux 可编译（对应 Python 版 git 历史 feaac0b 的教训）
- 运行期产物绝不提交：config.toml/json、*.session、*.db*、Telegram_Cache/ 等（见 .gitignore）
- 提交信息 conventional commits（feat/fix/docs:），与两个仓库现状一致

## 已知陷阱（实现时直接查设计文档「深度调研补遗」节）
- axum Multipart 默认 2MB 体积上限——上传路由必须显式 DefaultBodyLimit
- grammers FloodWait 藏在 `InvocationError::Rpc` 里，用 `err.is("FLOOD_WAIT")` 判定、从错误名解析秒数；库级重试用 RetryPolicy trait
- dav-server 的 DavFile 是 Bytes/seek 模型（read_bytes/seek/flush），不是 AsyncRead——直通流需自行包装 DownloadIter
- Windows WebClient：资源管理器上传单文件硬上限 4GB-1（注册表 DWORD 极限），平台级无解；挂载需 Basic 认证 + BasicAuthLevel=2
