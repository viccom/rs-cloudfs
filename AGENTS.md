# rs-CyDrive — Agent 工作须知

## 项目信息
- 项目：CyDrive 的 Rust 完全重写（Telegram 无限云盘：本地 WebDAV 服务挂载 Windows `Y:` 盘 + Web 仪表盘 :8088 + Bot 命令）
- 技术栈：Rust（edition 2024）/ tokio / grammers（MTProto）/ dav-server（WebDAV）/ axum / rusqlite
- **权威设计文档：`docs/rust-rewrite-design.md`**——crate 划分、选型依据、已核实的 API 面、里程碑与验收标准都在其中，动工前必读
- 行为基线与兼容契约：见 `E:\GitHub\CyDrive`（Python 版）根目录 `AGENTS.md` 的「契约」节——DB schema、加密格式、分块命名/caption、端口、注册表行为，破坏即与现有用户数据不兼容

## 当前阶段
M1 进行中（分支 `feat/m1-core`，自 feat/m0-core 切出）。M0 已完成：workspace + cydrive-core 七模块（config / cache / chunker / crypto / database / logging / rel_path），含与 Python 版的互操作契约测试（`tests/compat/fixtures/`：真实 Python CyCrypto 密文向量、真实 MetaDatabase SQL dump；由 `scripts/gen_compat_fixtures.py` 再生成）；设计文档 M0 验收行还列有 CI / keyring / cargo-deny 三项未做，归属待裁决（见 `docs/decisions.md`）。
M1 已完成：`transport` 模块——`CloudTransport` trait（async_trait，dyn 兼容）+ 配套类型（UploadJob/UploadReceipt/RemoteHandle/IncomingEvent、ByteStream/IncomingStream 别名、TransportError 含 FloodWait{seconds} 归一化）+ `MockTransport`（脚本化错误注入 FloodWait/Fail/FailAfterChunks；分块命名 `{stem}.part{NNN}` 与 caption 含 rel_path+i/n 契约在 Mock 侧强制；open/open_range 切片语义；drain-once incoming；检视 API 供后续队列/WebDAV 测试用），13 个测试。
M1 下一步：上传队列状态机（UploadQueue：mpsc + Semaphore 并发 2、`is_uploaded=0` 行即队列/启动重入队、指数退避 + FloodWait 按服务端秒数 sleep、连续失败降级标记）→ VFS 装配（见设计文档）。

## 常用命令（仓库根）
```
cargo test -p cydrive-core --no-fail-fast   # 全部 105 测试
cargo clippy -p cydrive-core --all-targets -- -D warnings
cargo fmt --all -- --check
python scripts/gen_compat_fixtures.py       # 重新生成互操作 fixture（需能 import E:\GitHub\CyDrive）
```

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
