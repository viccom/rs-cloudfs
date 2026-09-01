# rs-CyDrive — Agent 工作须知

## 项目信息
- 项目：CyDrive 的 Rust 完全重写（Telegram 无限云盘：本地 WebDAV 服务挂载 Windows `Y:` 盘 + Web 仪表盘 :8088 + Bot 命令）
- 技术栈：Rust（edition 2024）/ tokio / grammers（MTProto）/ dav-server（WebDAV）/ axum / rusqlite
- **权威设计文档：`docs/rust-rewrite-design.md`**——crate 划分、选型依据、已核实的 API 面、里程碑与验收标准都在其中，动工前必读
- 行为基线与兼容契约：见 `E:\GitHub\CyDrive`（Python 版）根目录 `AGENTS.md` 的「契约」节——DB schema、加密格式、分块命名/caption、端口、注册表行为，破坏即与现有用户数据不兼容

## 当前阶段
**M1 已完成**（分支 `feat/m1-core`，自 feat/m0-core 切出；M0 内容见 git 历史：workspace + config/cache/chunker/crypto/database/logging/rel_path 七模块 + 互操作契约测试；CI/keyring/deny 归属待裁决见 `docs/decisions.md`）。M1 交付（130 测试全绿）：
- `transport`：`CloudTransport` trait（async_trait，dyn 兼容）+ UploadJob/UploadReceipt/RemoteHandle/IncomingEvent/TransportError（FloodWait{seconds} 归一化）+ `MockTransport`（脚本化错误注入；分块命名 **`{basename含扩展名}.part{000起0基三位}`**、caption `i/n` 1 基——契约 3 勘误版，见 decisions.md 2026-09-03；open/open_range 切片；drain-once incoming）
- `upload_queue`：有界 mpsc + N worker；`decide_retry` 纯函数（指数退避封顶；FloodWait 按服务端秒数精确等待且不计入降级）；连续失败达 max_attempts 降级停试；成功才写 DB+删缓存；0 字节跳传输；`requeue_pending` 只入队本地存在的行
- `vfs`：门面装配——`put`（.tmp+原子 rename→pending 行→入队，enqueue 后无 await 点保证受理语义）、`hydrate`（缓存命中优先→分块合并下载→加密行解密为明文缓存→LRU 驱逐并清被逐行 is_cached）、queue_stats/shutdown 委托
- `MetaDatabase` 已内部 Mutex 化支持 Arc 跨 await（选型裁决见 decisions.md）
- 语义裁决与延后项见 decisions.md 2026-09-02 三条（FloodWait 不降级/持久化失败不重传/upload_failed 列延后到迁移单元）

**M2 进行中**（分支 `feat/m2-telegram`，自 feat/m1-core 切出）：新 crate `cydrive-telegram`。已完成首单元（3f878e2 红 + 9607457 绿，15 测试）：纯契约模块——`caption.rs`（单文件/多块 caption **Python 逐字快照**：Path:/File:/Part: 标签、KB 整除、加密后缀；`clean_rel_path` 按基线求值顺序（先 strip '/' 后替换 '\'，反斜杠输入产出 `//a/b` 是基线真实行为）；`part_document_name` 复用 core `chunker::part_name`）、`flood.rs`（`parse_flood_wait`：FLOOD_WAIT_N 秒数解析、裸 FLOOD_WAIT=0、其余 None）、`range.rs`（`range_plan`：skip/head/take 换算 + chunk_size 4096 整倍数且 4096..=512KB 校验）。
M2 剩余：grammers 依赖接入（AGENTS 红线要求 git 锁精确 commit；若以 crates.io `=0.10.0` 精确锁替代须记 decisions.md）→ `GrammersTransport` glue（upload 走 `upload_stream`+caption/命名用上述纯模块、open/open_range 用 `DownloadIter` 的 chunk_size/skip_chunks、connect=bot_sign_in、错误映射走 `parse_flood_wait`）→ 入站 stream_updates 索引 + Bot 命令（含补齐 /get）→ 真机三档 smoke（100MB/2GB/3GB）登记「待人工」。设计文档「深度调研补遗」grammers 节是 API 权威。

## 常用命令（仓库根）
```
cargo test --workspace --no-fail-fast            # 全部 145 测试（core 130 + telegram 15）
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
