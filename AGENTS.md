# rs-CyDrive — Agent 工作须知

## 项目信息
- 项目：CyDrive 的 Rust 完全重写（Telegram 无限云盘：本地 WebDAV 服务挂载 Windows `Y:` 盘 + Web 仪表盘 :8088 + Bot 命令）
- 技术栈：Rust（edition 2024）/ tokio / grammers（MTProto）/ dav-server（WebDAV）/ axum / rusqlite
- **北极星（负责人 2026-09-02 裁决）**：「一个稳定好用的程序」——方向性取舍偏保守/稳定，总则见 docs/decisions.md
- **权威设计文档：`docs/rust-rewrite-design.md`**——crate 划分、选型依据、已核实的 API 面、里程碑与验收标准都在其中，动工前必读
- 行为基线与兼容契约：见 `E:\GitHub\CyDrive`（Python 版）根目录 `AGENTS.md` 的「契约」节——DB schema、加密格式、分块命名/caption、端口、注册表行为，破坏即与现有用户数据不兼容

## 当前阶段
**M1 已完成**（分支 `feat/m1-core`，自 feat/m0-core 切出；M0 内容见 git 历史：workspace + config/cache/chunker/crypto/database/logging/rel_path 七模块 + 互操作契约测试；CI/keyring/deny 归属待裁决见 `docs/decisions.md`）。M1 交付（130 测试全绿）：
- `transport`：`CloudTransport` trait（async_trait，dyn 兼容）+ UploadJob/UploadReceipt/RemoteHandle/IncomingEvent/TransportError（FloodWait{seconds} 归一化）+ `MockTransport`（脚本化错误注入；分块命名 **`{basename含扩展名}.part{000起0基三位}`**、caption `i/n` 1 基——契约 3 勘误版，见 decisions.md 2026-09-02（勘误条）；open/open_range 切片；drain-once incoming）
- `upload_queue`：有界 mpsc + N worker；`decide_retry` 纯函数（指数退避封顶；FloodWait 按服务端秒数精确等待且不计入降级）；连续失败达 max_attempts 降级停试；成功才写 DB+删缓存；0 字节跳传输；`requeue_pending` 只入队本地存在的行
- `vfs`：门面装配——`put`（.tmp+原子 rename→pending 行→入队，enqueue 后无 await 点保证受理语义）、`hydrate`（缓存命中优先→分块合并下载→加密行解密为明文缓存→LRU 驱逐并清被逐行 is_cached）、queue_stats/shutdown 委托
- `MetaDatabase` 已内部 Mutex 化支持 Arc 跨 await（选型裁决见 decisions.md）
- 语义裁决与延后项见 decisions.md 2026-09-02 三条（FloodWait 不降级/持久化失败不重传/upload_failed 列延后到迁移单元）

**M2 进行中**（分支 `feat/m2-telegram`，自 feat/m1-core 切出）：新 crate `cydrive-telegram`。已完成首单元（3f878e2 红 + 9607457 绿，15 测试）：纯契约模块——`caption.rs`（单文件/多块 caption **Python 逐字快照**：Path:/File:/Part: 标签、KB 整除、加密后缀；`clean_rel_path` 按基线求值顺序（先 strip '/' 后替换 '\'，反斜杠输入产出 `//a/b` 是基线真实行为）；`part_document_name` 复用 core `chunker::part_name`）、`flood.rs`（`parse_flood_wait`：FLOOD_WAIT_N 秒数解析、裸 FLOOD_WAIT=0、其余 None）、`range.rs`（`range_plan`：skip/head/take 换算 + chunk_size 4096 整倍数且 4096..=512KB 校验）。
M2 已完成第三单元（70617a9，编译验证 wiring，无新测试）：`src/transport.rs`——`GrammersTransport` 实现 `CloudTransport`：connect（`SenderPool::new`+`tokio::spawn(pool.runner.run())` 驱动、`bot_sign_in`、chat 解析 `PeerId::from_bot_api_dialog_id`）、upload（`plan_chunk_sends` → 每块 `File::seek+take` 流式切片 → `upload_stream` → `send_message` 带 caption/document → UploadReceipt；加密 TODO(M2 encryption)）、open（逐 part 全量拉取 + serve_range；已知限制：整文件内存缓冲，M3 走 VFS 水合）、open_range（**part 边界从远端 document size 现场推导**——RemoteHandle 不带 chunk 计划；每相交 part `range_plan`+`chunk_size/skip_chunks`+`serve_range`）、delete_remote（0 删→NotFound）、错误映射（RpcError{name,value} 重构后走 `parse_flood_wait`；Dropped→Disconnected；client 内建 AutoSleep 与 Python FloodWait 语义对齐）；`incoming()` 为 todo!()（留给入站单元）。依赖 `grammers-client = "=0.10.0"` + `grammers-session = "=0.10.0"`（crates.io 精确锁替代 git-rev；0.10 实际 API 面与设计文档补遗差异 + **session 持久化已裁决落地（见第四单元）**）。
M2 已完成第四单元（48f41ab 文档 + 4d1c771 实现，4 新测试）：**session 持久化**——in-tree vendor `crates/vendor/grammers-session`（上游 0.10.0 拷贝，仅 SqliteSession 的 libsql→rusqlite(bundled) 移植，write-through/表结构逐字保留；VENDOR.md 记来源与 re-vendor 指引）+ 根 `[patch.crates-io]` 重定向（`cargo tree -i` 证实全图单一份，MSVC LNK2005 消除）+ transport 接线 `SqliteSession::open(cfg.session_path)`（契约 3 `cynet_bot_session` 文件落地、重启免登录）。vendor 为 **workspace exclude 的外部 path dep**（cap-lints allow，fmt/clippy 门禁不覆盖上游代码）。⚠ M6 发布前需为 vendor 补上游 LICENSE 文本（crates.io 包不含，VENDOR.md 已注明来源）。
M2 剩余（让位于垂直切片，可用优先，见下）：入站 stream_updates 索引 + Bot 命令（含补齐 /get，`pool.updates` receiver 须构造期消费，transport.rs 有 NOTE(inbound) 锚点）→ 加密上传路径（TODO(M2 encryption)）→ 真机三档 smoke（100MB/2GB/3GB）+ FloodWait 注入「待人工」。**grammers 0.10 API 以 `src/transport.rs` 实现为权威**，设计文档补遗节待回填。

**垂直切片（可用优先，2026-09-02 负责人指令 + decisions.md 队列重排条；自动化已提频至每小时）**：
- **A 已完成（bd5e877，19 新测试，工作区 178 全绿）**：`cydrive-webdav`——`CyDriveFs` 实现 dav-server 0.11 `DavFileSystem`（**trait 非 async_trait，手写 FsFuture/Box::pin**；其余与设计文档差异见提交信息：symlink_metadata 转发 metadata、DavMetaData 是 DynClone、etag() 需覆写、mime 无 FS 接口由库按扩展名推导）：metadata/read_dir/open 读写/create_dir/remove_dir/remove_file/rename/get_quota（used=DB 总量、total=used+10TB 契约）；写入 `.tmp` staging + **flush=PUT 提交点**（dav-server 语义核实）原子入队；copy NotImplemented（Explorer 复制走 PUT）。基线镜像：DELETE 不删远端（同 Python）、MKCOL 同；**MOVE 基线本来就是坏的（500），定义稳健语义**：`MetaDatabase::rename_path` 原位 UPDATE（保 file_id→chunks 链接防孤儿化）+ 缓存子树移动 + 远端不动。core 增 `Vfs::put_staged`（大文件不整文件读回）+ `rename_path`，均带测试。边界：range PUT 安全但不做 RFC 补丁合并；rename 后远端 caption 过期（无碍重组）。
- **B 已完成（1660ba5，9 冒烟测试，工作区 187 全绿）**：`src/server.rs`——`WebDavServer::serve(fs, addr)`（dav-server 0.11 **无自带 hyper 装配**，照 examples/hyper.rs 自起：TcpListener → accept loop → http1 serve_connection；`DavHandler` 直接对接 hyper 1.x Body）+ **FakeLs 锁系统**（不装则 ALLOW 不含 LOCK/UNLOCK，Explorer 挂载必需）+ 方法集 WEBDAV_RW + principal("cydrive") + `local_addr()`（:0 → 实际端口）+ 幂等 graceful shutdown（hyper 1.x `Connection::graceful_shutdown`）。冒烟：PROPFIND 207/GET 字节与头/Range 206/PUT→队列全周期/0 字节 PUT/MKCOL 201 重复 405/DELETE 204 不删远端/MOVE 改名/OPTIONS DAV 头与 ALLOW。实测语义记录：OPTIONS 对 collection 的 ALLOW 不含 PUT（dav-server 策略）；hyper 用 "1"（Cargo.lock 锁 1.11.1，semver 稳定不 exact-pin）。
- **C 已完成（4f09bec，6 端到端测试，工作区 193 全绿；`target/debug/cydrive.exe` 已产出）**：`cydrive-cli`（bin 名 `cydrive`，clap 唯一子命令 run）——`discover_config`（cwd: config.toml → legacy config.json → 带指引的 Err，env overrides）→ validate/is_configured 门 → logging init → `GrammersTransport::connect` → `run_with_transport`（**transport 注入 seam**：e2e 全走 MockTransport，真机路径编译验证）→ db/cache → Vfs → **requeue_pending（WebDAV 前，断电恢复）** → `WebDavServer::serve` → ctrl_c → RunHandle::shutdown（WebDAV 排干 → 队列排干）。e2e：PROPFIND/PUT 全周期到 Mock 远端、pending 启动重入队、toml 优先/legacy 兼容、缺配置可行动错误、优雅停机拒连。core 增 `Vfs::requeue_pending` 委托（+10 行，e2e 驱动）。与 Python cli.py 顺序差异三条（connect 提前=fail-fast、requeue=新增修复、退出排干=加强）见提交信息。
- **D 已完成（bb0c6c9，11 新测试 + 2 #[ignore] 真机项，工作区 204 全绿）**：`cydrive-platform`——纯逻辑（normalize/pick 盘符链 `Z,Y,X,W,V,U,T,S` 基线镜像含全占用回退、bitmask 解码、mount/unmount 命令构造、契约常量）+ `windows.rs`（cfg(windows) 真实现：sc 查启 WebClient、winreg 写 `FileSizeLimitInBytes=0xFFFFFFFF`/`BasicAuthLevel=2` + net stop/start、mount 先卸载清冲突再 `net use /persistent:no`、盘符 std 探测 A..Z）+ `windows_stub.rs`（cfg(not(windows)) 全 Unsupported，Linux 可编译由 cfg 纪律保证）。CLI 接线：`mount/unmount/fix-reg` 子命令（flag>config 解析纯函数可测）+ `run_with_transport` 自动挂载（auto_mount_drive 且 Windows；失败仅 warn 服务继续）+ `RunHandle.mounted_letter`（shutdown 末尾卸载）。真机项 `#[ignore]`：`ignored_mount_unmount_roundtrip`（CYDRIVE_TEST_MOUNT_URL）、`ignored_optimize_webdav_registry`（管理员）。
- **垂直切片 A–D 全部完成 =「可运行程序」达成**（2026-09-02）：`target/debug/cydrive.exe`，`cydrive run/mount/unmount/fix-reg`。真机验收待人工：bot token config 冒烟 + Explorer 挂盘实测 + ignored 真机测试（管理员）。
- **稳定性补齐 E1+E2 已完成（61b5a05 + cc292e3，5 新测试，工作区 209 全绿）**：E1 `hydrate` 超时——`VfsConfig.hydrate_timeout`（默认 180s，基线并发语义）包裹 transport.open 起的全部下载/写盘/解密，超时 `VfsError::Timeout` 且**统一清理所有失败路径的 .tmp**（含既有错误路径补修）；MockTransport 增 `open_delay` 测试旋钮。E2 上传侧 sha256——≤100MB（`SHA256_MAX_BYTES` + `should_hash` 纯函数）成功上传后哈希明文入行（失败仅 warn 不降级），**sha 版 ETag（契约 6）自此可达**。⚠ 已知边界（Python 同构，未修）：解密失败时密文残留最终缓存路径，后续命中返回密文。
- **M2 遗留①入站索引已完成（4a52949，7 新测试，工作区 216 全绿）**：`core::inbound`（`spawn_inbound_worker`：File→`Vfs::index_inbound` 根目录元数据索引（Python 基线逐字段镜像：同名覆盖/嵌套名原样/空名回退 `Telegram_File_{id}.bin`/chunk_count=1/mime 恒 None 差距已注）；Command 日志占位待 Bot 单元；Err 事件不杀 worker）+ **transport `incoming()` 真接线**（构造期保留 `pool.updates`，`stream_updates`+`map_update`：chat 过滤、media 优先于 text、Document/Sticker/Photo→File；`NOTE(real-machine)` 待真机验证）+ CLI 集成（serve 前启动 worker，RunHandle 私有字段 join）。MockTransport 增强：`incoming_results` Err 注入（原 incoming 契约零改动）。
- **M2 遗留②Bot 命令已完成（2f334b0，10 新测试，工作区 226 全绿；trait 演进裁决见 decisions.md 2026-09-02 条）**：`core::bot::handle_command`——/help /stats /search **基线文本逐字镜像**（含 startswith 怪癖、GB/MB 分支、15 行上限、KB 整除）+ **/get 补齐**（精确路径→唯一名命中→hydrate→send_document；miss/ambiguous/失败均有回复，新文本已标注）；CloudTransport 增 provided 方法 `send_text`/`send_document`（默认 Unsupported，非 breaking），MockTransport 覆写记录（`sent_texts`/`sent_documents` 检视）；worker Command 臂接 dispatch（Err 仅 warn）；`Vfs::db()` 访问器；GrammersTransport 真接线仅编译验证（NOTE(real-machine)，真机待验）。
- **M2 遗留③上传加密已完成（13d38d4，7 新测试，工作区 233 全绿；负责人批复整文件语义、流式列 v2，见 decisions.md）**：队列侧整文件加密——`UploadQueueConfig.encryption_password` + 行 is_encrypted 双条件；sha256 对**明文**、行 size=明文/chunk_count=密文块数（密文=明文+44B 恒定开销，块边界按密文）、`{name}.enc.tmp` 成败均清、明文缓存仍仅成功删；CLI `vfs_config`（已 pub）补 `enable_encryption AND password` 映射；**离线全闭环证明**：put→上传密文（mock 内可解密回明文）→hydrate 回明文。0 字节不加密（契约）。已知微差：跨重试复用同一密文文件（Python 每次重加密，字节等价）。
- **M2 全部闭环（2026-09-02）**：传输壳/session 持久化/纯契约模块/入站索引/Bot 命令（/get 补齐）/上传加密。真机 smoke（100MB/2GB/3GB + Bot 命令 + FloodWait）仍待人工。
- **回补队列（下一步）**：M4 Web 仪表盘（axum :8088，契约 1 六路由：GET /、/api/files、/api/stats、POST /api/upload（multipart 字段 `file`+DefaultBodyLimit 陷阱）、/api/delete {filename}、/api/download/{filename}；前端 rust-embed 嵌入 Python 版 static；新增 /api/list 目录树、Range 下载、/api/queue）→ M5 平台全量 migrate/doctor + keyring → M6 性能/发布（CI、cargo-deny 可提前、vendor LICENSE 补文本、cargo-dist、release build 首验）
M2 已完成第二单元（34e2476 红 + 356009c 绿，10 测试）：纯适配逻辑——`plan.rs`（`plan_chunk_sends`：单/多块发送计划，name/caption/byte_len 全走契约模块，纯函数不触盘）、`stream.rs`（`serve_range`：RangeStream 状态机，head-skip/take 裁剪、迭代器耗尽不报错，coerce 到 ByteStream；为此 crate 直接依赖 bytes/futures-core，版本同 cydrive-core）、`config.rs`（`DEFAULT_API_ID=6`/`DEFAULT_API_HASH="eb06d4abfb49dc3eeb1aeb98ae0f581e"`/`DEFAULT_SESSION_STEM="cynet_bot_session"` 契约常量 + `TransportConfig`）。grammers 接入与 transport 壳是下一步（编译验证的 wiring，纯逻辑已全部就绪）。

## 常用命令（仓库根）
```
cargo test --workspace --no-fail-fast            # 全部 233 测试（core 163 + telegram 29 + webdav 30 + cli 17 + platform 9；另有 2 个 #[ignore] 真机项）
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
