# CyDrive-RS — Rust 完全重写设计文档

> 状态：设计提案（2026-09，两轮生态调研已回填，权威版本在本仓库维护）。
> 基线行为来自 Python 版 CyDrive v2.0（`E:\GitHub\CyDrive`），兼容契约见其根目录 `AGENTS.md`「契约」节。

## 需求概述

在平行路径用 Rust 完全重写 CyDrive：功能对齐 Python 版（Telegram 云盘 + WebDAV `Y:` 盘 + Web 仪表盘 + Bot 命令），性能与可靠性显著优于基线（真·流式零磁盘、上传失败不丢数据、优雅退出），并修复 Python 版已知缺陷（见 AGENTS.md「已知缺陷」节）。现有用户的 `cydrive_meta.db`、Telegram 云端数据、加密文件必须无缝延续。

## 总体架构

单进程、单 tokio 多线程 runtime，取代 Python 版「主线程 + cheroot 线程 + asyncio 线程 + run_coroutine_threadsafe 桥接」的三线程拓扑。WebDAV 与 Web 仪表盘同进程不同端口，全部 async，无跨线程桥。

```
Windows Explorer (net use Y:)          浏览器 / Bot 命令
        │ WebDAV :8080                     │ REST :8088 / MTProto
┌───────┴──────────────┬──────────────────┴──────────────┐
│                 cydrive (单二进制)                       │
│              tokio multi-thread runtime                 │
│  ┌──────────────┐ ┌─────────────┐ ┌──────────────────┐ │
│  │ cydrive-webdav│ │ cydrive-web │ │ cydrive-telegram │ │
│  │ dav-server    │ │ axum        │ │ grammers(MTProto)│ │
│  └──────┬───────┘ └──────┬──────┘ └────────┬─────────┘ │
│         └────────┬───────┴─────────────────┘           │
│           ┌──────┴──────┐                               │
│           │ cydrive-core│  VFS · SQLite 元数据 · LRU 缓  │
│           │  (领域层)    │  存 · AES-GCM · 分块 · 上传队列 │
│           └─────────────┘                               │
│  cydrive-platform: net use / 注册表 / davfs2 / mount_webdav │
└─────────────────────────────────────────────────────────┘
```

关键数据流（与 Python 版对齐，箭头处为改进点）：

- 上传：WebDAV PUT（或 `/api/upload`）→ 写入 `.tmp` 再原子 rename（改进：Python 直写最终路径，半成品对 GET 可见）→ DB `is_uploaded=0` → 有界并发上传队列（改进：Python fire-and-forget 无背压、失败也删缓存）→ 可选加密 → >1900MB 流式分块 → 成功后置 `is_uploaded=1`、记 chunks、**仅成功才删缓存**
- 下载：GET → 加密文件/小文件走 LRU 缓存水合；未加密大文件走**直通流**（`iter_download` → HTTP response，零落盘，Python 版做不到）→ Range 请求按 Telegram part 偏移按需拉取（改进：Web UI 视频可拖动进度条）
- 入站：MTProto updates 流 → 带 media 消息只索引元数据（与 Python 一致）→ Bot 命令 `/stats` `/search`（`/get` 补齐实现，Python 只在帮助里承诺）

## Workspace 与 Crate 划分

```
cydrive-rs/
├── Cargo.toml                  # workspace
├── crates/
│   ├── cydrive-core/           # 纯领域层：VFS、DB、缓存、加密、分块、上传队列
│   ├── cydrive-telegram/       # CloudTransport trait 的 grammers 实现
│   ├── cydrive-webdav/         # DavFileSystem 适配 + dav-server 装配
│   ├── cydrive-web/            # axum REST + 嵌入式静态前端
│   ├── cydrive-platform/       # 挂载/注册表/服务管理（#[cfg] 按平台编译）
│   └── cydrive-cli/            # clap 子命令 + 编排 + setup 向导（bin）
├── tests/                      # 跨 crate 契约测试（crypto 向量、DB 采用、caption 快照）
└── docs/
```

依赖方向单向：`cli → webdav/web/telegram/platform → core`。`core` 不依赖任何网络库，用 trait 注入传输层——这是整个设计里最重要的解耦点（可测试 + 可扩展）。

## 核心抽象：CloudTransport

```rust
pub trait CloudTransport: Send + Sync {
    async fn connect(&self) -> Result<(), TransportError>;                     // bot_sign_in
    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt>;          // 流式、分块、caption、进度回调
    async fn open(&self, file: &RemoteHandle) -> Result<ByteStream>;           // 全量流式下载
    async fn open_range(&self, file: &RemoteHandle, off: u64, len: u64) -> Result<ByteStream>;
    async fn delete_remote(&self, msg_id: i32) -> Result<()>;
    fn incoming(&self) -> IncomingStream;                                      // stream_updates：入站文件 + Bot 命令
}
```

- 生产实现 `GrammersTransport` 包装 `grammers_client::Client`；测试实现 `MockTransport`（内存 Vec<u8> + 脚本化错误注入：FloodWait、断连、半途失败）
- trait 同时是未来扩展点：多 chat/频道条带化存储、断点续传重用 `Uploaded`（grammers 上传后 24h 内可复用）都在实现层做，core 不感知
- `open_range` 实现已核实可行且无需 raw invoke：grammers `DownloadIter` 原生提供 `chunk_size(i32)` + `skip_chunks(i32)`（跳过 N 块实现任意偏移；chunk_size 须为 MIN_CHUNK_SIZE 整倍数、范围 MIN..=MAX，Telegram 约定 4096B..=512KB）——offset 换算 `skip_chunks(offset / chunk)` 后顺序迭代即可

## 兼容契约（重写期间冻结，来自 AGENTS.md，破坏即不兼容）

1. SQLite schema（files/chunks 两表、rel_path 唯一、parent_dir 字符串树、WAL）——Rust 版直接打开 Python 版生成的 `cydrive_meta.db`
2. 加密文件格式 `[16B salt][12B nonce][AES-256-GCM ct+tag]`，PBKDF2-HMAC-SHA256 100k 迭代、32B key、AAD 空
3. 分块命名 `{name}.part{NNN}`（三位零填充）+ caption 文本格式（含 rel_path 与 `i/n`，是远端唯一的重组元数据）
4. 端口 8080（WebDAV）/ 8088（WebUI）；旧 REST 路由与 JSON 字段（`/api/stats` 的 `drive_letter/chat_id/is_configured` 等）
5. Telegram 约束：bot token 登录、api_id=6 公开凭据、单消息 2GB 上限 → 分块阈值 1900MB、`files.telegram_msg_id` 存第 0 块
6. Windows 侧：注册表 `FileSizeLimitInBytes=0xFFFFFFFF`、`BasicAuthLevel=2` + 重启 WebClient；`net use Y: http://127.0.0.1:8080 /persistent:no`；盘符偏好 `Y:` 回退 `Z,Y,X,W,V,U,T,S`
7. 行为语义：文件夹 available 恒 10TB / used=DB 总量、ETag=sha256（无引号）否则 `mtime-size`、0 字节 PUT 跳过、SHA-256 仅 ≤100MB 计算、FloodWait 自动等待重试

契约测试落 `tests/`：crypto 双向向量（Python 生成 .enc 样本 → Rust 解密；Rust 加密 → Python 验证，迁移期临时脚本）、用 Python 生成的真实 db 文件做采用测试、caption/命名做字符串快照。

## 技术选型（2026-09 实查）

### 核心（决定架构的四个）

| 用途 | 选型 | 版本/状态（实查） | 理由与要点 |
|---|---|---|---|
| Telegram MTProto | **grammers-client** | 0.10.0（2026-07-02），月下载 ~4.8 万，活跃；仓库主址已迁 Codeberg（GitHub 为镜像），官方建议 git 依赖锁 commit | Rust 唯一成熟 MTProto 客户端。已实查 API：`bot_sign_in(token, api_hash)`、`iter_download()`（流式块迭代）、`download_media()`、`upload_file(path)` / **`upload_stream(&mut AsyncRead, size, name)`**（WebDAV PUT 可直通 Telegram，不落盘）、`send_message`/`send_album`（带 caption/document）、**`stream_updates()`**（0.10 更新流，配 `UpdatesConfiguration`）、**`RetryPolicy` trait + `AutoSleep`**（库内建 flood-wait 自动重试，可挂自定义退避策略）。`Client` 是 `SenderPool` 包装（单池多 handle 并发）。注意官方声明 minor 版本会变更 TL layer（视同 breaking）→ 必须锁精确版本。Bot API HTTP 接口不可替代：下载上限 20MB/上传 50MB |
| WebDAV 服务 | **dav-server** | 0.11.0，messense（rsvg/tiktoken 维护者）维护的 webdav-handler-rs 续命 fork；通过 litmus basic/copymove/props/locks/http 全套（RFC4918 基础全过）；内置 Range/条件请求/partial PUT | 提供 `DavFileSystem` trait（仿 Go x/net/webdav），我们实现虚拟文件系统后端；原生 `http`/`http_body` 类型 → 与 hyper 直连，另有 actix/warp 适配。自带 `FakeLs`（Windows/macOS 挂载所需的最小 LOCK 支持）。opendal 官方集成同款 trait，成熟度有旁证。**trait 面已实查**（见「深度调研补遗」）：必需仅 `open`/`read_dir`/`metadata` 三个方法，其余（create_dir/remove_dir/remove_file/rename/copy/`get_quota`）为带 NotImplemented 默认的 provided 方法，按需覆写 |
| HTTP 框架（仪表盘） | **axum** | 0.8.9（2026-04） | tokio 官方系；路由/multipart/静态文件（tower-http）全备 |
| SQLite | **rusqlite + tokio-rusqlite** | rusqlite 为事实标准；tokio-rusqlite 提供后台线程异步句柄 | 社区实测 sqlx 对 SQLite 慢 ~17% 且本质仍是线程池包壳；rusqlite bundled 免系统依赖。WAL + `PRAGMA foreign_keys=ON`（修复 Python 未开的 CASCADE）+ `INSERT ... ON CONFLICT ... RETURNING id`（修复 Python lastrowid bug）+ `PRAGMA user_version` 做增量迁移 |

### 支撑

| 用途 | 选型 | 说明 |
|---|---|---|
| 异步运行时 | tokio（multi-thread, rt-multi-thread） | 全生态基座 |
| 加密/哈希 | RustCrypto：`aes-gcm`、`pbkdf2`、`sha2`、`hmac` | AES-NI 加速；格式逐字节兼容 Python `cryptography` 输出 |
| 流式分块 | 自实现（core 内，基于 `tokio::io` 10MB 缓冲切 `.part{NNN}`） | grammers `upload_stream` 需预知 size；WebDAV PUT 有 Content-Length，可边切边上传单块 |
| CLI | clap 4（derive）+ dialoguer（向导）+ indicatif（进度条）+ comfy-table（表格） | 对齐 rich 的体验 |
| 配置 | serde + `config.json` 只读兼容 + 新 `config.toml`；`figment` 分层（env `CYDRIVE_*` > 文件 > 默认） | 首次启动检测旧 config.json 自动迁移 |
| 凭据保管 | **keyring 3.x**（keyring-core + 平台 store crate：`windows-native-keyring-store` / Secret Service / macOS Keychain） | 3.x 改为分层架构：启动时 `set_default_store(...)` 注册平台后端。bot_token 与加密口令迁出明文 JSON（修复 Python 缺陷）；文件里只留非敏感项 |
| Windows 注册表 | winreg | fix-reg 的键值照契约复刻，`#[cfg(windows)]` |
| 文件监控（可选新功能） | notify 8.1 + notify-debouncer-full | Python 版 watcher 是死代码；Rust 版作为可选 `sync-folder` 模式（本地目录双向同步），默认关闭 |
| 日志/观测 | tracing + tracing-subscriber（JSON/pretty 切换）+ tracing-appender 轮转；console-subscriber（tokio-console 调试卡顿） | `RUST_LOG` 常规控制 |
| 错误处理 | thiserror（core/各库）+ anyhow（cli 顶层） | Result 模式，禁止 unwrap 上生产路径 |
| HTTP 客户端 | reqwest | 公网 IP 探测（ipify 等，兼容 get_display_ip 行为） |
| 前端 | 移植现有 `web_ui/static`（原生 HTML/JS/CSS） | `rust-embed` 嵌入二进制，单文件分发；保留旧路由，新增目录浏览 API |
| 测试 | tokio::test、tempfile、wiremock、proptest（分块/加密性质测试）、criterion（哈希/路径基准） | 契约测试见上文 |
| 发布 | cargo-dist（多平台二进制）+ cargo-deny/audit + clippy/rustfmt + GitHub Actions（windows/linux 矩阵） | 全选型均为 MIT/Apache-2.0，与项目 MIT 兼容 |

### 已评估未采用

- **sqlx**：异步 SQLite 走线程池，无真异步收益，慢于 rusqlite，弃用
- **Bot API HTTP 框架（frankenstein/teloxide）**：20MB/50MB 硬上限，无法承载存储流量
- **WinFsp + winfsp crate / fuser**：绕开 WebClient 限制的真内核级虚拟盘，`winfsp` crate 提供 `FileSystemContext` trait 绑定；但需装驱动，违背「无内核驱动」卖点 → 列为 v2 可选后端（CloudTransport/挂载层已解耦，后补不动架构）
- **WsgiDAV 等价物之外的 WebDAV 库**：Rust 生态里 dav-server 是唯一通过 litmus 的维护中实现，无二选

## 关键设计细节

### 数据模型（core）

沿用 files/chunks schema（契约），Rust 侧映射：

- `RelPath` newtype：强制 `/` 开头、`/` 分隔、拒绝 `..`/空段/反斜杠——所有路径污染在类型层拦截（Python 版靠调用点自觉）
- upsert 用 `RETURNING id`；删除开事务（`foreign_keys=ON` 后仍显式删 chunks 保持与旧库兼容）
- LRU 缓存：**放弃 atime 语义**（Windows 常见 `NtfsDisableLastAccessUpdate`，atime 不可靠）→ in-memory `last_access: HashMap<RelPath, Instant>` + DB 新增列 `last_access_at`（迁移加列，默认值兼容旧库），行为等价、跨平台可靠
- 缓存目录布局照旧镜像 rel_path（契约，旧缓存目录可直接续用）

### 上传队列（core）

- `UploadQueue`：`tokio::sync::mpsc<UploadJob>` + N 个 worker（`Semaphore`，默认并发 2 个文件；每文件分块串行 + 块内 512KB part 并发由 grammers SenderPool 自理）
- 持久化即 DB：`is_uploaded=0` 的行就是队列；启动时扫描重入队（改进：Python 断电后 pending 文件若缓存已被删则永久丢失）
- 重试：指数退避（1s/2s/4s...上限 5min）+ FloodWait 按服务端秒数 sleep；连续失败 N 次降级为 DB 标记 `upload_failed` 并在 WebUI 徽章展示，不无限循环
- 加密在大文件下不再整文件落临时盘：流式 PBKDF2 → 每 `.part` 独立加密为 v1 兼容格式（每块自带 salt/nonce，格式对单块成立——Python 端按块解密亦兼容）

### WebDAV 层（cydrive-webdav）

- 实现 `DavFileSystem`：必需方法仅 `read_dir`/`metadata`/`open`（直查 DB，复用索引，10 万条 PROPFIND 目标 <100ms）；覆写 `create_dir`/`remove_dir`/`remove_file`/`rename`（写 DB；`copy` 首版返回 NotImplemented，Explorer 的拖拽复制由 PUT 实现）；`get_quota` 返回 `(used=DB 统计, Some(used + 10TB))` 对齐 Python 的 available=10TB 契约
- **`DavFile` 是基于 `Bytes` 的 seek/read 模型而非 AsyncRead**（已实查）：必需方法 `read_bytes(count) -> Bytes`、`write_bytes`/`write_buf`、`seek(SeekFrom) -> u64`、`flush`、`metadata`。dav-server 自己在其上实现 Range。我们的读取端按策略二选一：
  - `HydratedFile`：加密文件、chunked 文件、小于阈值（默认 64MB）的文件 → 水合进 LRU 后包装 `tokio::fs::File`（天然支持 seek）
  - `PassthroughFile`：其余 → 内部持有 `DownloadIter` + 位置游标 + 顺序缓冲（默认 8MB 预取窗口）；`seek(Start(n))` 映射为 `chunk_size(4096).skip_chunks(n / 4096)` 后重置迭代器，`read_bytes` 从缓冲顺序供给——Range/顺序读都成立，内存有界
- 写入端：`write_bytes` 追加写缓存目录 `.tmp` 文件，`flush`（= dav-server 语义上的 PUT 完成）时原子 rename → DB `is_uploaded=0` → 入上传队列
- 认证：默认 localhost 无认证（契约）；bind 非 loopback 时强制 Basic auth 或显式 `--allow-unauth-lan`（Windows 侧 `BasicAuthLevel=2` 已就绪，实践中 `net use` + Basic 认证是可靠组合）。可选实现 `GuardedFileSystem` trait 接管认证
- 挂载怪癖备忘：Windows WebClient 需要 OPTIONS/PROPFIND 在根路径正确响应（dav-server 已覆盖）；WebClient 的 WqlEventQuery 会周期性探测已挂载盘符产生持续 PROPFIND 流量——属预期行为，读路径必须廉价（DB 直查因此是硬要求）

### Web 仪表盘（cydrive-web）

- 旧路由原样保留（`/api/files`、`/api/stats`、`/api/upload`、`/api/delete`、`/api/download/{filename}`），前端零改动可用
- **必踩坑（已实查）**：axum `Multipart` 提取器默认请求体上限 **2MB**——上传路由必须显式 `DefaultBodyLimit::max(N)`（N 取配置的 chunk 上限 1900MB+），否则大文件上传静默 413
- 新增：`GET /api/list?path=/...`（目录树浏览，解决旧版只能看根目录平铺）、`/api/download` 支持 Range（视频 seeking）、`GET /api/queue`（上传队列状态）、`DELETE` 语义收敛为单次 DB 删除（修复 Python 调两次 delete_file）
- 静态资源 `rust-embed` 进二进制，发行单文件

### 平台层与 CLI

- 子命令对齐：`run`、`mount`、`unmount`、`fix-reg`、`stats`、`setup`；新增 `migrate`（导入旧 config.json + 指向旧 db/cache 目录 + keyring 迁移凭据）与 `doctor`（端口占用/注册表/WebClient/Telegram 连通性一键诊断）
- Windows：winreg 写注册表 + `sc`/`net stop;net start` 重启 WebClient + `net use` 挂载（契约）；Linux/mac：gio → davfs2 → mount_webdav 顺序尝试（对齐 Python），失败进入 headless 模式提示
- 优雅退出：ctrl_c → HTTP 两服务 graceful（drain in-flight，等上传任务至多 60s）→ telegram `disconnect()` + session save → unmount → 进程退出（修复 Python 不关异步线程/不存 session 的缺陷）

### 测试策略

1. 单元：core 全覆盖（路径类型、LRU、队列状态机、分块边界、加解密往返）
2. 契约：`tests/compat/`（crypto 双向向量、Python 生成 db 采用、caption/命名快照、ETag 格式）
3. 集成：MockTransport 下 WebDAV 全方法 + WebUI 全路由 + 上传/下载状态机（含断连/FloodWait 注入）
4. 一致性：`litmus` basic+copymove+props+http 套件跑 dav-server 适配层
5. 真机清单（人工）：Windows Explorer 拖入 3GB 文件夹、Explorer 内播放 MP4、`net use` 断网重连、VPS 模式浏览器上传、手机 Bot 收图后 Explorer 出现

## 功能拆解（里程碑）

| 里程碑 | 内容 | 验收标准 |
|---|---|---|
| M0 基础 | workspace、CI、clippy/rustfmt/deny、config+keyring、tracing、crypto/chunker + 契约向量 | Python 加密的样本 Rust 能解；分块命名/合并与 Python 字节一致；`cargo test` 全绿 |
| M1 core | VFS/DB（含旧库采用+迁移）/LRU/上传队列 + MockTransport | Python 生成的 db 打开读写通过；队列状态机单测覆盖断连/FloodWait/重试 |
| M2 telegram | GrammersTransport：登录/上传/下载/Range/入站索引/Bot 命令（含补齐 `/get`） | 真实 bot token smoke：收发 100MB/2GB/3GB 三档文件；FloodWait 注入自动恢复 |
| M3 webdav | DavFileSystem 适配 + 挂载 + fix-reg | litmus 全过；Explorer 实测拖入/删除/重命名/属性；Range 下载正确 |
| M4 web | 仪表盘移植 + 新 API + Range 流媒体 | 旧前端不改动可跑；1080p MP4 浏览器内可拖进度条；上传/队列徽章实时 |
| M5 平台/迁移 | mount/unmount/fix-reg/stats/setup/migrate/doctor | 从 Python 版真实数据无损切换（db+缓存+config 一次 `migrate` 完成） |
| M6 性能/发布 | 直通流、并发分块、观测面板、cargo-dist 多平台产物 | PROPFIND 10 万条 <100ms；2GB 上传吞吐 ≥ Python 版实测；断电重启 pending 自动续传；优雅退出无僵尸挂载 |

## 深度调研补遗（第二轮，2026-09，全部实查）

### grammers 0.10 已核实的 API 面

- 错误模型：`InvocationError::{Session, Rpc(RpcError), Io, Deserialize, Transport, Dropped, InvalidDc, Authentication}`——**没有独立 FloodWait 变体**；flood-wait 以 `Rpc(RpcError)` 形式出现，用 `err.is("FLOOD_WAIT")` 匹配（支持尾部 `*` 通配），等待秒数从 RPC 错误名（`FLOOD_WAIT_N`）解析。`Transport::BadStatus(429)` 表示连接数过多——上传并发度有服务端上限，队列并发默认取保守值 2
- 重试：`RetryPolicy` trait（`should_retry(RetryContext)`）+ 内建 `AutoSleep`（flood-wait/slow-mode 自动重试一次）与 `NoRetries`——我们的指数退避实现为自定义 `RetryPolicy`，与队列级重试分层（库级处理单次 RPC，队列级处理整文件重试）
- 更新流：`Client::stream_updates() -> UpdateStream`（配 `UpdatesConfiguration`），Bot 入站文件与命令的统一入口
- 下载：`DownloadIter::{chunk_size(i32), skip_chunks(i32), next() -> Option<Vec<u8>>}`；chunk_size 必须是 `MIN_CHUNK_SIZE`(4096) 整倍数且在 `MIN..=MAX`(512KB) 内，默认 MAX
- session：`grammers-session` 提供 `Session` trait 与多种存储；Client drop 时状态同步进 session 对象，**须显式调用存储的 save 方法落盘**（优雅退出流程的一环）
- 版本策略（官方文档声明）：patch 不改 TL layer，**minor 会改 layer（视同 breaking）**；且发布节奏慢，官方推荐 git 依赖锁 commit → Cargo.toml 用 `grammers-client = { git = "...", rev = "..." }` 精确锁定

### dav-server 0.11 已核实的 trait 面

- `DavFileSystem`（dyn-compatible）：必需 `open(path, OpenOptions)` / `read_dir(path, ReadDirMeta)` / `metadata(path)`；provided（默认 `FsError::NotImplemented`）`symlink_metadata` / `create_dir` / `remove_dir` / `remove_file` / `rename` / `copy` / `have_props`（默认 false）/ `patch_props` / `get_props` / `get_prop` / **`get_quota() -> (used: u64, Option<total: u64>)`**
- `DavFile`（`Debug + Send + Sync`，dyn-compatible）：必需 `metadata` / `write_buf(Box<dyn Buf>)` / `write_bytes(Bytes)` / `read_bytes(count) -> Bytes` / `seek(SeekFrom) -> u64` / `flush`；provided `redirect_url`（未来可用于把大文件 GET 重定向到直链场景）
- 配套类型：`DavDirEntry`、`DavMetaData`（len/类型/时间戳）、`FsError`（含 `NotImplemented`/`NotFound`/`Exists`/`Forbidden` 等 HTTP 语义映射）、`FsFuture`/`FsStream` 别名
- litmus 通过范围：basic/copymove/props/locks/http（RFC4918 基础全套）；`proppatch` 默认 feature 开着但我们的 FS 返回不实现即可

### 平台与限制（已核实）

- **Telegram 尺寸**：免费/单对象硬上限 2GB（同类项目 Telegram-Drive 精确按 **2,000,000,000 字节**封顶），Premium 4GB；HTTP Bot API 另有 50MB 上传/20MB 下载限制（与 MTProto 无关）→ 1900MB 分块阈值维持
- **Windows WebClient**：`FileSizeLimitInBytes` 为 DWORD，最大 `0xFFFFFFFF`——**资源管理器经 WebDAV 上传单文件硬上限 4GB-1，平台级无解**（Python 版同样受限，README 未言明）。>4GB 文件走 Web UI 或 rclone 路径（不经过 WebClient）；文档需明示。挂载实践：`net use` + Basic 认证 + `BasicAuthLevel=2` 是可靠组合（无认证在部分 Windows 版本会被拒）；WebClient 会对已挂载盘周期性探测（WqlEventQuery 触发的持续 PROPFIND），读路径必须廉价

### 同类项目参照：Telegram-Drive（caamer20，5k star，活跃）

技术栈与我们高度重合（Tauri 2 + Rust + **Grammers** + **Tokio + SQLite** + Actix Web + React 19），是 grammers 路线在生产环境的最强旁证。可直接借鉴的工程决策：

- 持久化上传/下载队列：跨重启恢复、独立并发控制、暂停/恢复/退避/network-waiting 状态机、flood-wait 处理（与我们的 UploadQueue 设计同构，验证了方向）
- 下载经私有临时文件 + 原子发布 + 尺寸校验（与我们 `.tmp`+rename 策略一致）
- WebDAV/REST 默认**仅 loopback、默认关闭**、能力 URL/哈希化 API key——值得吸收为我们的默认安全姿态（差异：我们必须默认开 8080 以维持 `Y:` 盘契约）
- 加密文件在 WebDAV 上 **fail-closed**（读/改名/复制全拒绝，只在应用内流式解密）——印证「GCM 整文件验证导致加密文件无法直通流」的判断；他们用流式 XChaCha20-Poly1305（TDENC2 信封）解决应用内流式，思路可供我们 v2 加密格式参考
- Folder Sync 三态合并（本地树/远端树/上次同步树）+ >50% 批量删除熔断——我们可选 sync-folder 模式的设计蓝本
- 验证门禁 `cargo fmt --check + clippy -D warnings + cargo test`、cargo-deny——照搬进 M0

差异化定位（避免重造）：他们是**用户账号 + 桌面 GUI 应用**；我们是 **bot token + 系统级 WebDAV 盘符挂载 + 零磁盘流式 + Python 版数据无缝迁移**，服务/VPS 场景是主战场。

### 低风险断言（未逐项联网核实，实现时以 docs.rs 为准）

winreg（SetValueEx/REG_DWORD）、notify 8.x + notify-debouncer-full、rust-embed、clap 4 derive、dialoguer、indicatif、comfy-table、tracing 系、thiserror/anyhow、reqwest、tempfile、proptest、criterion、wiremock、cargo-dist、tokio-rusqlite `Connection::call`（后台线程调用模式，已核实）。以上均为长期稳定、多替代品可选的常规依赖，选型风险低。

## 风险评估

- **grammers pre-1.0、API 会破坏**（0.10 仍是 breaking 序列，官方声明 minor 即换 TL layer；仓库已迁 Codeberg，docs.rs 曾现 404）：git 依赖锁精确 commit + `CloudTransport` trait 隔离（升级只动一个 crate）+ vendor 预案（必要时 fork）。缓解后风险可控
- **Windows WebClient 兼容怪癖**（挂载对 PROPFIND 字段/认证/超时敏感）：dav-server 过 litmus 且 opendal 同款生产在用；M3 安排真机 Explorer 清单逐项验证；必要时对照 wsgidav 行为抓包对齐
- **Range 下载实现复杂度**（raw GetFile offset 分页、part 大小协商）：M2 单独 spike + 性质测试（随机 offset 往返一致）
- **加密文件无法直通流**（v1 格式 GCM tag 在尾部，必须整文件验证）：接受现状——加密文件仍走水合路径；v2 流式分块加密格式列为后续可选（保持 v1 解密兼容）
- **2GB bot 上限与 Premium 4GB**：契约固定 1900MB 阈值，不因 Premium 改默认（可配置项提供但默认不变）；另注意 WebClient 4GB-1 平台上限（见补遗）意味着 >4GB 文件只能经 WebUI/rclone 上传
- **dav-server/keyring 等单人维护库**：messense 为知名维护者、dav-server 被 opendal 官方集成；锁版本 + cargo-deny 审计 + 必要时 fork，常规预案
- **`upload_stream` 需预知 size**：WebDAV PUT 带 Content-Length 满足；chunked transfer PUT（无长度）降级为缓存落盘后再传（Python 行为），标警示日志

## 明确不做 / 后续可选

- WinFsp/fuser 内核级虚拟盘（v2 后端候选，架构已留位）
- 多账号/多 chat 条带化（CloudTransport 实现层扩展）
- 本地文件夹双向实时同步（notify 选型已备，`sync-folder` 可选模式，默认关）
- 加密 v2 流式格式（先保 v1 兼容）
