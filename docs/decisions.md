# 决策记录（decisions.md）

按全局工作约定记录层级间冲突、执行风险与采纳的裁决；新条目追加在文件末尾。

## 2026-09-01 M0 验收范围与进度注记不一致（CI / keyring / cargo-deny 归属）

- **冲突双方**：`docs/rust-rewrite-design.md` M0 里程碑验收列为「workspace、CI、clippy/rustfmt/deny、config+keyring、tracing、crypto/chunker + 契约向量」；仓库 `AGENTS.md`「当前阶段」（fb10f58）标注 M0 剩余仅 tracing。2026-09-01 会话核实：仓库无 `.github/workflows`、代码无 keyring 引用、无 deny 配置。
- **采纳裁决**：本次会话按进度注记完成 tracing 日志接入（M0 收尾），不擅自扩 scope 补做三项。倾向：keyring 属 M5 `migrate` 凭据迁移环节、CI/cargo-deny 与 M6 发布管线相邻，随 M5/M6 落地并同步修订设计文档 M0 验收行；待项目负责人确认。
- **风险**：按设计文档口径 M0 关闭则三项缺失视为未达标；按进度注记口径关闭则里程碑表与实际漂移。裁决前 M0 状态表述为「tracing 已完成，CI/keyring/deny 待裁决」。
- **附注（次要发现）**：`AGENTS.md` 称「edition 2024」，workspace `Cargo.toml` 实为 `edition = "2021"`；未改动，待裁决时一并更正。

## 2026-09-01 tracing 接入范围裁剪：console-subscriber 暂缓

- **取舍**：设计文档「日志/观测」行含 console-subscriber（tokio-console 调试卡顿）。它需要 tokio runtime 与独立监听端口，属运行期编排能力，而当前 workspace 尚无 bin crate（cydrive-cli 未创建）。
- **采纳裁决**：M0 只做 tracing + tracing-subscriber（pretty/JSON、RUST_LOG）+ tracing-appender 轮转；console-subscriber 随 CLI crate（M1 后）接入。可逆、无契约影响。

## 2026-09-02 MetaDatabase 并发化选型：内部 Mutex 而非 tokio-rusqlite

- **冲突双方**：设计文档选型表写「rusqlite + tokio-rusqlite（后台线程异步句柄）」；上传队列的 tokio worker 只需跨 await 共享 `Arc<MetaDatabase>`，全量异步化会波及全部既有 DB 测试（12 个）与后续所有调用方。
- **采纳裁决**：`conn: Mutex<Connection>` 内部同步化（91b72ef），公共同步 API 不变，行为由既有测试 + 新增 database_sync 并发测试护航。单进程短查询场景足够；M6 性能验收（PROPFIND 10 万条 <100ms）若现争用再升 tokio-rusqlite。
- **风险**：每方法全程持锁，读多场景吞吐受限于单连接——M3 WebDAV 读路径直查 DB 是硬要求，届时若成为瓶颈按 M6 优化项处理。跨方法组合操作无原子性（改动前后一致，非回归）。

## 2026-09-02 上传队列语义细化（设计文档「上传队列」节的三处落地）

- **FloodWait 不计入降级判定**：设计文档只说「指数退避 + FloodWait 按服务端秒数 sleep；连续失败 N 次降级」。落地为：FloodWait 是服务端限速信号（等待后必然可重试），不计入连续失败；仅其他错误连续达 max_attempts 降级。`decide_retry` 纯函数单测锁定该语义。
- **成功路径持久化失败不重传**：transport 已成功后再遇 DB 错误时重传只会产生远端重复数据且救不了 DB，故直接降级（保留本地文件供 requeue），不重试。
- **降级的持久化标记（upload_failed 列）延后**：设计文档要求降级后「DB 标记 upload_failed 并在 WebUI 徽章展示」。当前降级仅停止重试 + 行保持 is_uploaded=0 + 内存 stats；持久化标记需要 schema 迁移（ALTER TABLE ADD COLUMN，模式同 last_access_at 计划），与 WebUI 徽章（M4）同批落地为独立迁移单元，避免本单元夹带 schema 变更。

## 2026-09-02 分块命名契约勘误（M1 规格错误的自我修正）

- **事实**：M1 transport 单元主会话规格将契约 3「分块远端名 `{name}.part{NNN}`」误读为 `{stem}.part{NNN}`（去扩展名）且编号 1 基。对照基线源码（telegram_client.py:155,203-204、chunker.py:36）逐条核实：① `file_name = os.path.basename(clean_rel)` **含扩展名**；② `enumerate(chunks)` 的 part 名 **0 基**（`data.bin.part000` 起）；③ caption `Part: {idx+1}/{n}` **1 基**；④ DB `chunk_index` 0 基（core ChunkRecord 同）。命名源应为虚拟路径 basename（rel_path.name()）而非本地文件名。
- **采纳裁决**：两轮 TDD 修正（64c1391 + 40b89c3）——测试期望先改为基线值（红），MockTransport 命名改为 `rel_path.name()` + 0 基编号，caption 保持 1 基；130 测试全绿。
- **教训与风险**：契约速记符号（`{name}`、`{NNN}`）有歧义时必须回基线源码逐字核实，不能凭直觉解读——M0 chunker 单元（part000、含扩展名）本已正确，M1 规格重新表述时引入偏差。GrammersTransport（M2）实现时命名/caption 必须以本条勘误后的契约为准；caption 全文快照在 M2 用基线源码转录（telethon 依赖使 fixture 自动生成暂不可用，人工转录待双向补强）。

## 2026-09-02 grammers 依赖锁定与 0.10 实际 API 面（设计文档补遗回填）

- **锁定方式**：AGENTS 红线要求 git 依赖锁精确 commit；采纳 crates.io 精确锁 `grammers-client = "=0.10.0"` + `grammers-session = "=0.10.0"`（**配对版本是 0.10/0.10，设计文档猜的 0.7 不成立**）。防漂移强度等价（`=` 锁不允许任何 semver 漂移），且自动化运行不必解析 git rev；升级 = 显式改版本号，与 git-rev 同样显式。
- **0.10 实际 API 面 vs 补遗假设**（实查 registry 源码，docs.rs 0.10 页面 404）：① 无 `Client::connect(Config)`——`SenderPool::new(Arc<Session>, api_id)` + `tokio::spawn(pool.runner.run())` 驱动，api_hash 仅 `bot_sign_in(token, api_hash)` 消费；② `DownloadIter` 非 Iterator/Stream，`chunk_size(i32)/skip_chunks(i32)` 消耗 self，块经 `async fn next(&mut self)` 手动拉取；③ `RpcError { pub name, pub value }` 把秒数拆进 value（`FLOOD_WAIT_31` → name="FLOOD_WAIT"+value=31），错误映射先重构 name_value 再喂 `parse_flood_wait`；④ `stream_updates` 需在构造期消费 pool 的 updates receiver；⑤ client 内建 `AutoSleep`（<60s FloodWait 自动睡后重试），与 Python「FloodWait 自动 sleep 整体重试」语义对齐。这些以 `src/transport.rs` 实现为权威，设计文档补遗节待回填。

## 2026-09-02 session 持久化被链接冲突阻塞（MemorySession 过渡，待项目负责人裁决）

- **冲突**：grammers-session 唯一文件存储 `SqliteSession` → libsql → libsql-ffi 无条件静态捆绑自带 SQLite，与 cydrive-core 的 `rusqlite(bundled)` 在 MSVC 下 LNK2005 符号冲突（同 binary 必炸，已实测）。自定义 `Session` 实现也被挡：DC 引导表 `KNOWN_DC_OPTIONS` 是 `pub(crate)`，外部实现拿不到 DC 地址无法冷启动。
- **采纳裁决（过渡）**：`MemorySession`——每次进程启动重新 `bot_sign_in`（bot token 常在 config，功能等价）；`TransportConfig.session_path` 暂未消费，代码留 `TODO(M2 session-persistence)`。契约 3 的 `cynet_bot_session` 文件名暂不落地，属实现细节级偏差（无用户数据不兼容），但频繁重启场景有被 Telegram 限流的风险。
- **候选终局（需裁决）**：① fork grammers-session 换 rusqlite 后端（工作量小、维护负担在我们）；② 推上游（慢）；③ 接受每次重登（零成本、有限流风险）。倾向 ①，等确认。

## 2026-09-02 项目负责人裁决批复（北极星：稳定好用）

- **北极星总则**：负责人明确目标「一个稳定好用的程序」——此后所有方向性取舍以稳定/保守优先于快与新特性，本条为总则。
- **session 持久化终局（批准 fork）**：落地形式采 **in-tree vendor**——`crates/vendor/grammers-session` 作为 workspace 路径成员 + 根 Cargo.toml `[patch.crates-io]` 把 grammers-session 统一重定向到该副本（grammers-client 的传递依赖也用同一份，类型合一），存储后端 libsql→rusqlite(bundled)（与 cydrive-core 同一 SQLite，消除 MSVC LNK2005）。相比外部 GitHub fork：技术效果等同（重启免登录、契约 3 `cynet_bot_session` 文件落地），但不引入外部仓库、零漂移、完全可回滚，更符合北极星。代价：上游升级需手动 re-vendor，接受（grammers 节奏慢且我们本就精确锁版本）。
- **M0 三项归属确认**：keyring→M5 `migrate`（修复 Python 明文凭据缺陷，该问题 Python 版同样存在）；CI→M6（开始 push 前无收益）；cargo-deny→M6 但允许作为任意运行的填充单元提前。设计文档 M0 验收行同步修订。
- **日期勘误**：本文件此前 3 条误记为 2026-09-03/09-04（实际系统日期未到），已订正为 2026-09-02。

## 2026-09-02 负责人指令：加快进度、可用优先（队列重排）

- **指令**：「加快进度，我需要可用的程序」。北极星不变（稳定好用），门禁（fmt/clippy/test 全绿）仍为底线不为速度牺牲；但**队列按可用性重排**。
- **采纳裁决**：最快到可运行程序的路径是 M3 垂直切片——A. cydrive-webdav 的 DavFileSystem 适配（Vfs 之上，离线 TDD 全测）→ B. dav-server+hyper 服务装配 + 离线 HTTP 冒烟 → C. cydrive-cli 最小 run 编排 → D. Windows net use 挂载 + fix-reg。A–D 达成即「可运行程序」（真机 bot token 冒烟仍待人工）。M2 非必要单元（入站 Bot 命令、加密上传路径）让位延后；M4/M5/M6 顺延。自动化频率 2h→1h。垂直切片期间新 crate 直接建于 feat/m2-telegram 分支（减摩擦），切片完成后恢复里程碑分支惯例。

## 2026-09-02 CloudTransport trait 演进：Bot 回复能力（send_text/send_document provided 方法）

- **需求**：Bot 命令单元（/stats /search /help 补 /get）需要向 chat 回复文本与发送文档，CloudTransport 原面（存储语义）无此能力。
- **采纳裁决**：以 **provided 方法 + 默认 Unsupported** 追加 `send_text(&self, &str)` / `send_document(&self, name, bytes)`（async_trait provided 方法，非 breaking——既有实现者零改动）；MockTransport 覆写并记录供离线断言；GrammersTransport 真接线（NOTE(real-machine)）。备选「独立 BotSurface trait」被否：实现方总是同一 transport，拆 trait 徒增装配复杂度。回复目标=配置 chat（与入站 chat 过滤一致）。
- **/get 语义（新定义，基线只承诺未实现）**：`/get <token>`——先按精确 rel_path 查；miss 则按名搜索，唯一命中用之，0/多命中分别回复 not-found/ambiguous；命中后 hydrate 再 send_document。

## 2026-09-02 加密上传跨兼容裁决批复（负责人选定）

- **裁决**：按 **Python 整文件加密语义**实现上传加密（明文→整文件 v1 格式加密→密文再分块上传），保双向兼容（Rust 写的加密文件 Python 可解、Python 写的 Rust 可解——hydrate 已然，上传侧补齐对称）；设计文档「每 .part 独立加密」的流式改进**列为 v2**（记录为后续可选，需格式版本协商，不进当前里程碑）。
- **语义细节（镜像 telegram_client.py:170-186）**：加密条件 = enable_encryption **且** password 同时成立（CLI 映射须补 AND）；sha256 对**明文**计算（≤100MB）；DB 行 size=明文大小、chunk_count=密文块数、is_encrypted=true；chunk 行 size=密文块大小；caption/命名照常（与内容无关）；加密临时文件成功失败均清理；明文缓存副本仍仅成功后删（保持既有修复，不回退 Python 无条件删缺陷）。

## 2026-09-02 M6 发布管线自动化执行降级记录（诚实留痕）

- **cargo-deny advisories 未本地运行**：本机 github.com 443 完全不可达（Codeberg 可达），RustSec advisory-db 克隆两次失败（Connection reset / 连接超时）。licenses / sources / bans 三项在 Windows host 与 x86_64-unknown-linux-gnu 双 target 下全部通过（GPL 家族零出现；唯一 MPL-2.0 = htmlescape@0.3.1，经 keyring，文件级弱 copyleft 可二进制分发）。advisories 首跑留待 CI 或有网环境；deny.toml 的 advisories v2 配置已就位。
- **CI 工作流已写未触发**：`.github/workflows/ci.yml`（win+ubuntu 矩阵，fmt/clippy -D warnings/test）按红线未 push，激活待人工。本地以相同三步门禁全绿（272 passed / 0 failed）等效预验；Linux 侧 apt 依赖按分析预计为零（rusqlite bundled 自带 C 编译即可、keyring 走 zbus 纯 Rust），首跑校准点已注释标注。
- **cargo-dist 配置提交未实跑**：`[workspace.metadata.dist]`（v0.32.0，2026-05-21 最新，ci=false 手动发布模式）；工具未安装，plan/build 留给发布时刻。
- **vendor LICENSE 补齐**：grammers-session 的 LICENSE-MIT/LICENSE-APACHE 从 Codeberg 上游 raw（HTTP 200）原样取回入库，VENDOR.md 状态已同步。

## 2026-09-02 真机首跑发现：连接期零输出 + 本机网络不可达 Telegram（10060）

- **现象与根因**：用户 `cydrive run` 零输出退出、无 Y 盘。复现确认：Telegram 连接阶段无任何日志（UX 缺陷），Ctrl+C 在 ctrl_c handler 安装前触发即硬杀进程（无输出）；底层错误为 os error 10060——**本机直连 Telegram DC 超时**（网络/地区封锁特征）。修复（connect_guard 3 测试 + main.rs/lib.rs UX）：启动横幅、连接进度 println、`connect_with_deadline` 90 秒死线 + 人话诊断（VPN/代理/token 三因）、连接期 Ctrl+C 干净退出、挂载结果醒目打印。本机实测输出完整。
- **待负责人裁决**：grammers 0.10 未配代理参数，Python telethon 支持 `proxy=`——若用户网络常态封锁 Telegram，需评估 ①系统级 VPN/TUN（零改动，当前方案）vs ②内建 SOCKS/MTProto 代理支持（新功能单元，涉及 mtsender 连接器改造）。用户确认网络形态后定。

## 2026-09-03 代理裁决落地：SOCKS5（负责人选定 Clash Verge 7897）；MTProto fake-TLS 延后

- **裁决**：负责人环境 = Clash Verge 本地混合端口 127.0.0.1:7897。采纳 SOCKS5 路线（grammers `proxy` feature + `ConnectionParams.proxy_url` + `CyDriveConfig.proxy_url`/`CYDRIVE_PROXY_URL`）。负责人原 MTProto 密钥经识别为 **fake-TLS（dd 型）伪装密钥**（42 hex，尾部嵌 ASCII 域名）——直接吃它需实现 fake-TLS 握手 + Intermediate 传输切换（grammers 连接硬编码 Full），工程量大且协议参考受网络限制，**列为 v2 待需求确认**。
- **真机验证（2026-09-03，本机）**：经 7897 用负责人真实 bot token 完成 bot_sign_in（session 落盘），全栈启动；仪表盘 200 / api/stats 200 / WebDAV PROPFIND 207。首次真实 Telegram 互通达成。

## 2026-09-03 Tier-1 实用功能批（TDD，worktree feat/tier1-utilities）：四处执行期裁决

- **A1 cache clear 保护 pending 上传**（执行中发现计划缺陷）：原契约会连 pending 上传的本地 staging 副本（未上传数据的唯一副本）一起删——数据丢失风险，违背「稳定」北极星。修订：clear 只删 `is_uploaded=1` 行的缓存副本并只清这些行的 `is_cached`；`cache clear` 与 `Vfs::cache_clear` 同语义（pending_file_paths → clear_except → clear_cached_flags）。CLI Clear 的 help 文本明示该行为。
- **/help 文本不属于 Python 兼容契约**：新增 /ls /mkdir /rm /quota /queue 需要更新 /help 输出，与「逐字基线」旧用例互斥。裁决：bot 回复文本是 Rust 侧扩展面，基线仅限 caption/分块命名/DB schema/端口；旧用例常量同步更新（test commit ca8f240）。
- **降级通知恒开无开关**：上传降级是罕见终态且此前完全静默（仅 /api/queue 可见），bot 推送通知（send_text best-effort，失败仅 warn）对稳定目标净收益为正，不加配置项（YAGNI）。
- **legacy config.json 拒收三个新调优键**（upload_workers/queue_capacity/hydrate_timeout_secs）：其余未知键维持静默忽略不变；新键出现在 json 中报 Parse 并提示改用 config.toml——防「新键写错地方被静默吞掉」。
- **执行过程失误留痕**：Task 1 只跑 `-p cydrive-core` 门禁漏检 VfsError 新 variant 打断 cydrive-webdav 穷举 match（Task 4 红测试作者发现），主会话直修补臂（e2ccf8c：Exists→FsError::Exists、ParentMissing→FsError::NotFound 沿 require_dir_parent 的 409 语义）。教训已吸收：跨 crate enum 变更后门禁必须 workspace 级。

## 2026-09-03 Tier-1 真机端到端验证（D:\Tools\rs-CyDrive 真实部署，经 Clash 7897）

**全绿清单**：doctor 6ok2warn；cache stats/clear（A1 语义真机确认：4 文件标志清零、目录行保留、清后可重新水合）；push 8MB/100MB/2GB（多块 1900+148MiB，msg 59/60，883s ≈2.3MB/s）；pull 8MB/745KB(M2 老文件跨版本互操作)/2MB(1MB×2 块重组 SHA256 MATCH)；`CYDRIVE_CHUNK_SIZE_MB=1` 环境覆盖生效；新二进制 `run` 全栈（requeue=0/仪表盘 10 文件 2.31GB/PROPFIND 207 中文正确编码/Y: 自动挂载 4 测试文件可见）；Y: 经 WebClient 复制回读 MATCH（清缓存后按需多块水合）。部署位二进制已更新（旧版备份 .m2.bak）。

**发现①hydrate_timeout 默认 180s 在真实带宽下过小**：实测下载方向仅 ~0.45MB/s（上传 2.3MB/s 的 1/5，Clash 节点不对称）→ ~80MB 以上文件 180s 必超时；2GB 回拉 1800s 仍超（需 ~80min）。**已用本批交付的 `hydrate_timeout_secs` 配置键缓解**（部署配置设 1800）。建议负责人裁决默认值上调（如 1800）——基线语义是 Python WebDAV 线程 180s，但 Python 用户同样会在此超时。
**发现②vendor session 无 WAL/busy_timeout**（storages/sqlite.rs 无 journal_mode 设置）→ **服务运行中不可并发跑 CLI 传输命令**（push/pull 同开 session 有锁冲突风险），运维约束已验证遵守（全程串行）。
**发现③2GB 全量回拉留待人工**：多块重组链路已由 1MB×2 小文件等价验证（同一代码路径），全量拉取仅剩带宽时间问题（~80min），不再阻塞。
**运维小注**：Git Bash 下 `--dest /path` 会被 MSYS 路径改写吃掉，须 `MSYS_NO_PATHCONV=1`。

## 2026-09-04 代码审查修复批（H1/H2/M1/M2 + Low 三项；不修清单留档）

- **H1 connect 死线**：`connect_stack_with_deadline`（push/pull 复用 run() 的 `connect_with_deadline` + 人话诊断，90s 常量上提 `CONNECT_DEADLINE`）；红测试 = 本地沉默 SOCKS 代理（accept 后 pending，不写不关）2s 死线胜出。
- **H2 pending 删除保护（负责人授权「逐一修复」采纳）**：三面一致——core `Vfs::remove_file`（新 `VfsError::UploadPending`）、webdav DELETE（Forbidden）、web `/api/delete`（409）。**细化裁决：仅当「pending 且本地副本仍在」才拒绝**；副本已消失的幽灵 pending 行（字节两侧皆无）允许删除，否则永远清不掉。副作用 = Task1 旧用例 remove_file 基线更新（排干+水合后再删，1469c95）。bot /rm 回复 "still uploading, try again after it finishes"。
- **M1**：`clear_cache_preserving_pending` 自由函数单点化（Vfs 方法与 CLI 命令委托），A1 语义与 warn 日志全仓单份。
- **M2**：`transport_config_from` 收敛 run()/connect_stack 两处装配。
- **Low**：push 目录源前置门禁（在写任何祖先行之前，杜绝孤儿行）+ dest 路径提示（drive paths start with "/"）+ pull 覆盖已存在文件用例补缺。
- **不修（理由）**：M3 create_dir TOCTOU（webdav 既有同款模式、单服务并发面极小、需事务设计）；M4 持久化失败降级不通知（该路径上传已成功，"upload failed" 文案语义不符，需要独立文案时再加）；cache clear 遇锁文件中止（可恢复态，继续清需行为设计）；unix_now 第三份拷贝（已注释自认）；/ls startswith 前缀怪癖（基线忠实）；一次性命令无 tracing subscriber（println 补偿）。`connect_failure_hint` 文案过时（"no built-in proxy yet" 与已落地 SOCKS5 不符）记为待办小修。
- 门禁：workspace 325 passed / 0 failed（修复批 +8 测试）。

## 2026-09-04 二复审结论（独立子代理全量重审修复批）

无 Critical/High。F1 错误链/死线覆盖、F2 三面判定逐字段等价性、基线更新、F3/F4 均以证据通过；脚本无泄密路径（token 只从凭据管理器读、输出前替换）。遗留登记：**M-1 ghost 行复活竞态**（remove_file 放行 ghost 行后 worker 成功 upsert 可复活该行为 uploaded 孤儿——不丢数据、非本批引入；后续方向：删行时同步取消队列 job 或 worker 成功 upsert 前校验行存在）；**M-2 connect_failure_hint 过时文案**已当日修复（改提 proxy_url，commit 见下）；L 级：getme.py 对非 UTF-16 blob 裸异常/token 校验弱于 ps1 版、local_copy_exists 同步 stat、grammers runner 超时后 detached（进程即退无泄漏）。

## 2026-09-04 服务生命周期批（feat/service-lifecycle）：裁决与发现记录

- **控制通道安全模型**：`cydrive.control` 端口文件 + 回环 TCP，仅绑 127.0.0.1、无认证——同机攻击者本可 taskkill，回环无认证不新增攻击面；文件为运行期产物（.gitignore）。`cydrive stop` 与 run 同 cwd 约定，报错文案指明。三停机源（Ctrl+C / SIGTERM / stop）经 ShutdownWatch 门闩汇流，停机序列由 run_with_transport 内 spawn 的唯一 stop 任务执行——任意源触发恰好执行一次，无双跑竞态。
- **SIGTERM 测试策略**：不做进程内信号注入测试（不稳定），采用 `#[cfg(unix)] #[ignore]` 真信号用例（kill -TERM 自身 → sigterm future 限期解析），WSL Ubuntu-24.04 实跑 PASS（ff9390b）。`cydrive_cli::sigterm()` 非 unix 存根 pending()。
- **发现：doctor 的 M5-2 遗留 cfg 缺口**（4f5a158 修复）：`platform_checks` 用 `cfg!(windows)` 运行期判断但块内调用 `#[cfg(windows)]` 函数 → unix 编译 E0425；本批 WSL 首次对 cli crate 做 unix 编译验证时暴露（此前 doctor 从未在 unix 编过）。改为 `#[cfg]` 属性门控，Windows 行为零变化。
- **Linux 自动挂载接线延后**：本批交付 platform 纯函数（detect gio→davfs2 + 命令构造）+ cfg(target_os=linux) mount_drive/unmount_drive + CLI mount/unmount unix 分支（--path）；`run` 的 Linux 自动挂载与挂载点/davfs2 secrets 配置键**延后**（涉配置语义与 sudo 裁决，下一批）。
- **davfs2 真机往返留待**：WSL 环境 davfs2 挂载不生效（/dev/fuse 在、mount 无报错但 /proc/mounts 无记录——WSL FUSE 组合的环境限制）；wsgidav 测试服务方案已验证可行（PROPFIND 207）。`ignored_davfs_mount_unmount_roundtrip` 需真实 Linux 主机跑。
- **systemd unit**（deploy/cydrive.service）：SIGTERM 原生处理后无需 KillSignal 覆盖；TimeoutStopSec=600（大文件排干）；RestartSec=5 防 100ms 紧密重启循环；EnvironmentFile 方案承载无头凭据（doctor 的 headless 提示与 run 流程各司其职）。未在真实 systemd 实跑（静态编写，键集按契约+惯例）。
- 门禁：Windows workspace 341 通过；WSL cli+platform 67 通过（+2 真机 ignored）+ workspace check 干净。

## 2026-09-04 setup 无头凭据缺陷修复（负责人裁决：修 bug、token 不换）

- **缺陷**：WSL/服务器无 Secret Service 时，setup 降级 InMemoryStore + tracing::warn——但 setup 不初始化日志订阅（审查已记录的 L 级缺口），警告不可见；向导打印「secrets live in the credential store」成功假象，token 实际随进程消亡（真机 WSL 复现）。migrate 对同场景是硬失败（「迁进易失内存比失败更危险」），setup 漏掉了同等处理。
- **修复**：`persist_setup(Option<&dyn CredentialStore>)`——None=headless 模式，secrets 经 `save_toml` 落 config.toml（文件值优先级链已支持）；main.rs setup_cmd 无 keyring 时 println 可见警告 + dialoguer Confirm 二选一（同意→文件模式；拒绝→带指引中止），删除静默 InMemory 路径。TDD：红= persist_setup_headless_writes_secrets_into_config（含空 keyring 下 discover 取文件值断言）。
- 已合并 main（474512f）+ 推送；WSL ~/cydrive/cydrive 已更新为修复版。run 仍受 WSL 网络拓扑限制：proxy_url 需指向 Windows 宿主 IP 且 Clash 开允许局域网。

## 2026-09-04 davfs2 真机往返关闭（WSL 实测通过）+ Linux 挂载部署前提

- **往返 PASS**：`ignored_davfs_mount_unmount_roundtrip`（wsgidav 8081 假服务 + `CYDRIVE_TEST_MOUNT_URL`）在 WSL Ubuntu-24.04 通过；此前「留待真实 Linux 主机」的判断不成立。
- **根因与前提**：davfs2 默认交互式认证（无 tty 即静默失败，即此前手动 mount「exit 0 但未生效」的假象来源）；无认证 WebDAV 服务端场景需 `/etc/davfs2/davfs2.conf` 写 `ask_auth 0`（或 secrets 文件配凭据）。已实测：`cydrive mount`（WSL）经 davfs2 挂载 `/root/CyDrive`，写/读/删全通。
- **真机确认**：Linux `run` 不自动挂载（按设计），`cydrive mount`（默认 `$HOME/CyDrive`，`--path` 覆盖）即挂载命令；后端探测选了 davfs2（gio 在无 gvfs 守护的 WSL 未被误选——此前担忧未成真，但「gio 探测只验二进制存在」的强化仍列为可选后续）。

## 2026-09-04 status 子命令 + Linux 自动挂载批（feat/status-and-automount，负责人两项设计已批）

- **`cydrive status`**（C1-C3）：控制协议增 PING→`OK: cydrive <version>`（不触停机）；`parse_net_use_mapping`/`parse_proc_mounts_davfs` 双平台挂载解析纯函数 + `current_mount_for` cfg 薄壳；`StatusReport`/`collect_status`/`render_status` 数据-渲染分离全测。语义边界：端口探测是机器级真相（谁监听都报 listening）、实例检测是 cwd 级（控制文件按 cwd 解析）——已在测试注释固化。
- **`mount_point` 可选配置键**（C4）：绝对路径校验（`/` 开头），None=$HOME/CyDrive 默认；Windows 忽略不报错；legacy json 拒收。此前延后的「挂载点配置语义」就此落定。
- **Linux run 自动挂载**（C5，此前延后项落地）：`auto_mount_target` 决策纯函数（platform 新增对 core 的 path dep，仅此用途）；启动链 = stale 清理（unmount_stale_for 幂等）→ mount_drive（非交互：root/setuid 直接工作，普通用户失败仅 warn + 提示手动，绝不出 sudo 密码提示）；停机链卸载挂载点（EBUSY/not-mounted warn 静默）；RunHandle 新增 `mounted_point` 字段（mounted_letter 的 Unix 对偶，不复用避免语义漂移）。
- **真机验证（WSL）**：`ignored_unix_automount_roundtrip` PASS（wsgidav 8081 + davfs2 ask_auth 0 前提）；win workspace 352 / wsl cli+platform 全绿。
- 后续可选：gio 探测强化（验 gvfs 可用而非仅二进制存在）、status 显示 pid/uptime、JSON 输出——均记 YAGNI 未做。

## 2026-09-04 WSL 自动挂载挂死与 stop 失灵根因修复（fix/davfs-timeout-pidfile）

- **根因（真机取证）**：davfs2 的 mount.davfs 在 `/var/run/mount.davfs/<挂载点>.pid` 留 PID 文件；服务异常退出/SIGKILL 后残留 → 后续挂载撞「found PID file ... ended irregular」（快速失败形态），或 mount.davfs 挂起（挂死形态——孤儿进程实测卡了数小时）。我们的 mount/unmount Command 调用**无超时** → 挂死形态直接卡死 run 启动（无 banner）与 stop 停机链（二次 stop 仍 OK 的原因：停机任务阻塞在序列中）。
- **修复**：①`run_with_timeout`（15s 预算，100ms 轮询 try_wait，超时 kill——错误明示 timed out；mount/unmount/unmount_stale_for 全部走此通道）；②mount 失败信息含 PID 残留时：PID 已死 → 删文件自动重试一次，成功回报「(after clearing a stale pid file)」；PID 活着不动（真挂载进程）。
- **测试**：`davfs_pid_file_path`/`parse_davfs_pid_file_hint` 纯函数（跨平台；parse 首版踩了路径含点的坑，改空白定界+去尾点）+ `run_with_timeout_kills_hanging_child`（cfg linux，WSL PASS：sleep 30 于 1s 被杀）。
- **WSL 真机全链**：天然残留 PID（6304 死进程）存在时 `run` → 自动清理 → 挂载成功 → banner 打印 → `status` 显示 mount → `stop` → 日志 stop command received → 卸载（findmnt 空）→ 进程退出、控制文件清理。工作区 354 全绿。

## 2026-09-04 status mount 检测 Windows 解析修复 + 双平台验收闭环

- **bug**：`parse_net_use_mapping` 找 `http://` 字面量，但 `net use` 实际渲染 UNC 形式（`\127.0.0.1@8289\DavWWWRoot`，80 端口省 @port）→ 挂载存在也恒报 not mounted。当初纯函数测试样本凭空构造未对真机。修复 = http→UNC 换算后匹配；测试基线改用真机输出（zh-CN locale）。
- **复用教训（与 davfs 同日第二次）**：外部命令输出解析的测试样本必须来自真机抓取，不得手写想象格式——已两例（mount.davfs stderr、net use UNC）。
- **验收**：Windows stop/status/mount 显示用户亲测闭环；WSL automount/status/stop 此前已验。main @ 79f97a1，workspace 354。

## 2026-09-04 rescan 可行性 spike 负结果：bot 读历史被服务端拒绝（BOT_METHOD_INVALID）

- **实测（examples/history_spike.rs，真机 bot session 副本）**：`messages.getHistory`（iter_messages）与 `messages.search`（search_messages）均返回 400 BOT_METHOD_INVALID——Telegram 平台级限制，与文档一致；正常应用层错误，无封禁风险（spike 本身即证明：连发两次被拒调用，bot 与会话毫发无损）。
- **保留的不对称**：`messages.getMessages`（按 ID 批量取，get_messages_by_id）对 bot 放行且生产长期使用——bot 拿得到「已知 ID 的消息」，拿不到「历史列表」。
- **架构推论**：rescan（扫历史重建索引）**出局**；多实例/换机视图同步的阶梯变为：① 手动重发文件重建索引（零代码，现状可用）；② `cydrive export-meta / import-meta`（~200 行，索引文件拷贝导入，无基础设施）；③ 云端元数据同步（负责人提案：服务器以派生键聚合各实例推送，绕开本闸门，~2000-3500 行，LWW+墓碑一致性）。②③ 待负责人按需求频率裁决。

## 2026-09-04 sync-lite 批（feat/sync-lite，0.5.0）：云端元数据同步落地与执行期裁决

- **交付**：crate `cydrive-sync`（lib+bin `cydrive-sync-server`：SyncStore LWW 引擎 + wire 协议 + axum /v1/push /v1/pull + env 配置 + deploy/cydrive-sync.service）+ core `sync.rs` 纯内核（payload 序列化/row_hash/namespace_key/push-diff/pull-apply/sync_once + SyncClient trait）+ config 两键（sync_url/sync_interval_secs，legacy json 拒收）+ MetaDatabase `sync_mirror`/`sync_state` 纯增量表 + cli `cydrive sync` 子命令与 run 周期任务。workspace 436 测试（win）/343（wsl）全绿；TDD 全程红→绿、断言零漂移。
- **max_pulled 只由 pull 推进（对计划原文的有意识修正）**：计划写「push → 更新 max_pulled」，但 push 响应的 max_version 可能已越过他实例并发推送行的版本，拿它推进游标会永久漏拉；改为 pull-only 游标 + push 后对整批行取 mirror.server_version=max_version（批内版本原子递增使「取 max」安全）。代价：下一轮 pull 会多回自己刚推的行，被幂等闸（version ≤ mirror.server_version 跳过）消化。
- **payload 排除本地不可恢复字段**：is_cached（本地运行时旗标，apply 恒置 0）、id（本地 rowid）、created_at/updated_at（upsert_file 不可显式恢复，若入 payload 则 apply 后本地 hash 永远 ≠ mirror hash → 双端无限互推）；mtime（用户可见时间）原样携带——收敛与空转 push=0 的前提。
- **墓碑闭环**：pull 端墓碑删 files/chunks/mirror 三处；push 端推出墓碑后同样删 mirror（否则「mirror 有本地无→再推墓碑」死循环）。已观察到的边界：推墓碑方下轮会一次性回声处理自己的墓碑（mirror 已删、version>0），无害且收敛。
- **ghost-pending 语义**：payload 含 is_uploaded=0 的行照常传播；pull 端无本地字节副本即跳过（不写行不写 mirror），源端后来上传成功（hash 变）会以更高版本重新可达。
- **secret 只 gate push**：wire 契约 pull 请求无 secret 字段（计划原文如此），读取暴露面由监听边界（默认 127.0.0.1/反代）承担；客户端 secret 走 env `CYDRIVE_SYNC_SECRET`（不加 config 键，计划外最小补面）。
- **其他小裁定**：服务端 push body 上限 64MB（axum 默认 2MB 装不下全盘首次推送）；HTTP 客户端每请求 300s 宽超时；run 周期任务 shutdown 用 abort（引擎幂等可重跑）；`sync_mirror_all` ORDER BY rel_path 确定性。
- **真机验收（2026-09-04，本机+WSL，生产 db 只读拷贝、事后哈希核验未变）**：server 0.0.0.0:18390；实例 A（生产 db 副本 9 行/10 chunks，含中文名与 2GB 多块行）push 9 → 空盘 B pull 9 全应用，双库全字段一致；B 侧 db 直改一行（LWW 后推者胜，双侧 size/mtime 一致）+ 删一行（墓碑双侧消失）；幂等轮全零；WSL 空盘实例 C（172.17.96.1 跨 NAT 到宿主 server）pull 9 = 8 应用 + 1 墓碑，与 A 全字段一致、二轮空转。
- **待人工**：cydrive-sync-server 部署到负责人服务器（deploy/cydrive-sync.service 已就绪，SYNC_SECRET 建议 + TLS 归反代）；生产各机 config.toml 增 sync_url 指向该服务器。

## 2026-09-05 sync-lite 二复审（独立双审查员）+ sync-server 参数守卫修复

- **修复（本轮已落地，main 0ee8fa4 已推送）**：cydrive-sync-server 此前忽略一切命令行参数、`--help/--version` 也会静默起服务（真机残留进程实证）；修复 = lib 纯函数 `decide_startup`（无参 Run / `--version`-`-V` / `--help`-`-h` / 其余 Invalid）+ main 在一切初始化前消费（Invalid→exit 2），TDD 红（真二进制 5s 死线证 bug）→绿（33 测试），部署件已刷新。
- **复审结论（两名独立子审查员对 e82c984..0ee8fa4 全量改动，静态精读+交叉核实+测试复跑；交付时未修，待负责人裁决修复批）**：纯引擎/服务端/cli 的事务原子性（push 单事务核实）、SQL 参数化、锁中毒恢复、abort 安全、secret 不泄漏、幂等自愈等面**通过**；`push 整批取 max` 前提（服务端原子版本分配）核实成立。**High×4**：①墓碑不删本地缓存副本→同名重建后 hydrate 磁盘探测命中旧字节（sync.rs 墓碑分支）；②remote-wins 清缓存以行旗标 is_cached 为准而 hydrate 命中以磁盘为准——不对称使残留必被服务（且测试把旗标语义钉成契约，源头是批次规格即我方 prompt 的规格疏漏）；③hydrate 用开头快照整行回写——下载窗口内远端更新被回滚并经 push 传播吞掉（该竞态在 sync 之前的单机 PUT 覆盖场景即存在，sync 放大）；④客户端仅支持 http（hyper-util build_http 核实）但 config 接受 https:// 且生产部署形态（TLS 反代）需要 https——执行「待人工」部署步骤前必须解决（加 rustls 或 v1 拒收 https+文档化隧道/LAN 形态）。**Medium**：毒 payload 行整场 return Err 卡死 namespace 游标（其余行全部不可达）；payload 无版本机制（混版本窗口 hash 漂移互推，属内容哈希方案的固有权衡，建议演进纪律文档化）；push_diff O(n×m)（10 万行库分钟级）；服务端 64MB 整包缓冲无并发闸（反代层缓解+文档）。**Low**：--help 文案误称 secret 覆盖 pull（实际只 gate push）、300s 超时不覆盖响应 body、RUST_LOG 缺省 ERROR 致 journal 无感、日志打配置地址非实际绑定、args() 非 Unicode panic、手动 sync 与 run 周期任务凭据门槛不一致、模拟器 pull 排序与真服务端不一致。**裁决为设计语义（不改）**：远端墓碑 vs 本地 in-flight 上传→上传成功复活=「后发动作胜」的 LWW 一致语义（与计划声明的并发上传语义同族）；自推墓碑一次性回声已声明。
- **修复批建议（待授权）**：P1=High①②+毒行跳过（core，含旗标契约测试更正）；P2=High④（https 拒收+文档 或 rustls，需负责人选）；P3=High③（hydrate 尾部改 mark-cached-only+重读比对，独立 TDD 单元，属生产热路径需谨慎）；Low 项随批捎带。

## 2026-09-05 复审修复批 P1+P2（fix/sync-review-p1p2，0.5.1）：缓存清理磁盘基准 / in-flight 保护 / 毒行跳过 / https

- **P1（High①②+毒行，core）**：统一原则=缓存清理以**磁盘存在**为准（与 hydrate 命中同基准，消除旗标/磁盘不对称）；**in-flight 上传（pending 且副本在盘）源文件绝不被 sync 破坏**（A-H4 裁决的「后发动作胜」语义落地）。落地：墓碑分支 best-effort 删缓存（pending 在飞保留）；覆盖分支清理条件改磁盘存在（非 pending）；毒 payload/非法路径行改 warn+计数 skipped_invalid+continue，游标照常推进（不再整场 Err 卡死 namespace）。契约更正：旧测试 `apply_overwrite_keeps_unflagged_cache_file` 钉的是错误旗标语义（源头为当初批次规格），按 High② 裁决替换为磁盘基准断言；e2e 用例在未修复内核上独立红复现后转绿。
- **P2（High④，负责人授权修复，A/B 选项由本批选 A=真 https）**：sync 客户端加 hyper-rustls 0.27.9（default-features=false 排除 aws-lc-rs，features native-tokio/http1/tls12/ring/webpki-roots）——https 走 rustls+Mozilla 根（公网 CA 受信、自签不支持→文档指隧道），http 行为零变化（既有 e2e 零改动全过）。真网络红→绿实证（对 git.metme.top TLS 握手+证书验证+HTTP 往返 0.07s，红=旧 build_http 的 scheme 错误）。实现陷阱留痕：hyper-rustls 公有 `wrap_connector` 不动内层 `enforce_http`（默认 true 会先拒 https），须镜像上游 build() 的 `enforce_http(false)`。新依赖 9 包全宽松许可；deny.toml 白名单补 `ISC` 与 `CDLA-Permissive-2.0`（webpki-roots 根证书数据许可），licenses/bans/sources 实跑 ok（advisories 离线惯例未跑）。
- **门禁**：win workspace 449 / wsl 三 crate 356 全绿（各 +13）；fmt/clippy 零警告；ring 于 WSL 构建无碍。
- **遗留（Low 项未修，待批）**：--help secret 文案、超时不覆盖 body 读取、RUST_LOG 缺省、日志实际绑定地址、args_os、凭据门槛统一、模拟器排序、O(n×m) diff、64MB 并发闸；P3（hydrate 快照回写）另列待办。

## 2026-09-05 五 BUG 修复批（fix/review-followup-batch，0.5.2）：解密残留/超时默认/运维盲区/body 超时/diff 性能

- **BUG① 解密失败密文残留最终缓存路径（正确性，授权偏离 Python 同款缺陷）**：原实现密文先 rename 进最终路径再解密，失败后密文占位、磁盘探测命中永远返回密文。修复=结构反转：密文留 .tmp staging 解密、成功后 write_atomic 原子晋级明文，最终路径在解密成功前从不被触碰；失败兜底删 staging 覆盖全部失败类。错密码（可重试：修好条件后同远端行重新水合成功）与损坏密文（GCM tag 翻转）双路径测试钉死。
- **BUG② hydrate_timeout 默认 180→1800**：真机带宽 ~0.45MB/s 下 80MB+ 必超时（2026-09-03 发现①裁决落地）；显式 180 仍合法，VfsConfig 默认链同步对齐。默认值契约变更，既有断言随授权更新。
- **BUG③ 服务端运维盲区三件**：a) 日志改打实际绑定地址（local_addr，:0 时不再误导；bin 级 e2e 读子进程 stdout 断言实际端口且可连接）；b) systemd unit 补 Environment=RUST_LOG=info（EnvFilter 缺省 ERROR 导致 journal 无感的修复）；c) args_os+lossy 防非 Unicode 参数 panic（进程级真红：旧二进制 panic 复现）。
- **BUG④ 客户端超时纳入完整 body 读取**：原 300s 只罩到响应头，慢速滴流 body 可挂死手动 sync；修复=发送+响应头+collect 全程单一超时窗口（push/pull 双路径），注入 seam `with_request_timeout` 供测试；滴流端点回归测试（裸 TCP 逐块写，红=15s 防护超时挂死）。
- **BUG⑤ push_diff O(n×m)→哈希 join**：mirror 建 HashMap<&str,&str> 一次，万行级库从分钟级 CPU 回到线性；行为等价测试（乱序输入/三类行/双墓碑/高版本同哈希，任意排列输出恒等）钉语义冻结，红由 BUG①② 承担（纯性能重构无法在行为层红，如实注明）。
- **门禁**：win workspace 458 / wsl 三 crate 365 全绿（+9 测试）；fmt/clippy 零警告。
- **遗留**：P3（hydrate 快照回写）待办未动；Low 余项（--help 文案、凭据门槛统一、sync_url host 校验、SyncClient trait 文档、模拟器排序、64MB 并发闸）仍挂账。

## 2026-09-05 secret 全端点 + 服务端日志批（feat/sync-secret-and-logs，0.6.0）：协议变更

- **负责人三项指令**：①SYNC_SECRET 覆盖 pull+push（此前 pull 无鉴权，公网部署元数据可被任意读取）；②服务端默认日志（用户实测手动跑二进制无任何输出——RUST_LOG 未设时 EnvFilter 缺省 ERROR；现未设时缺省 info + push/pull 请求日志：ns 前 8 字符/行数/max_version/耗时，403 warn 绝不记 secret 值）；③客户端 secret 便利配置——config.toml 新键 `sync_secret`（程序写入路径经 scrubbed 脱敏，手写允许；家庭级便利裁决）+ 优先级 env CYDRIVE_SYNC_SECRET > config > None。
- **wire 协议变更（0.6.0 破坏面）**：PullRequest 增 `secret: Option<String>`（serde default + skip_serializing_if None——无 secret 时与旧格式字节一致）；服务端配 secret 时 pull 403 闸与 push 同型（文案可行动，指向 sync_secret/CYDRIVE_SYNC_SECRET）。兼容矩阵：新客户端→旧服务端 ✓（旧端忽略未知字段）；**旧客户端→新服务端（配 secret）pull 403**——升级顺序：先客户端后服务端再设 secret。core SyncClient::pull 签名 +secret 参数（trait 演进，实现×2/调用点×3 机械波及，断言零漂移；跨批活红灯 secret_gate_end_to_end 断言未动自然回绿）。
- **不设 secret 仍开放两端点**（保留内网/隧道形态，非破坏）；服务端 config.rs 零改动，--help 文案更正（顺带修掉「secret 只 gate push」的过时文案 Low 项）。
- 门禁：win workspace 475 / wsl 三 crate 382 全绿；fmt/clippy 零警告。
- 遗留 Low 项不变（P3、--help 余项、凭据门槛统一等）。

## 2026-09-05 准实时同步批（feat/sync-realtime，0.7.0）：SSE 门铃 + client_id + 本地变更唤醒

- **负责人裁决**：轮询（平均 5 分钟）不满足实用，要求准实时；批准「门铃模型」设计——SSE 只通知（max_version+origin），不推数据，数据语义仍单源于 push/pull；300s 轮询保留为兜底（SSE 全链路故障时退化为现状，最终一致不破）。
- **client_id（每实例身份，与 namespace 数据身份分离）**：`sync_client_id(id CHECK(id=0), client_id)` 表 get-or-create 32hex（rand，不引 uuid crate）；push/pull/subscribe 携带（wire 可选字段，旧版双向兼容）；用途=origin 回声跳过+日志可见。namespace 仍回答「哪份数据」，client_id 回答「哪台机器」。
- **服务端**：`POST /v1/subscribe`（与 pull 同 secret 闸；流式 text/event-stream + Cache-Control + **X-Accel-Buffering: no**）；EventHub=每 ns broadcast(16)（Lagged 记 warn——门铃丢一条无害，下条或兜底补）；心跳 `: keepalive` 默认 20s（`SYNC_HEARTBEAT_SECS` 旋钮）；订阅断开确定性清理（frame 管道 closed 感知 + receiver_count==0 同锁清理防泄漏）；push 事务提交后 publish。
- **客户端**：Vfs 持 `Arc<Notify>`，`wake_sync()`=notify_one（permit 语义：pass 进行中的唤醒不丢、合并突发）；埋点=put/remove_file/create_dir/index_inbound/**上传 persist_success**（UploadQueueConfig.sync_wake 注入，queue 不感知 Vfs）+ WebDAV MOVE（43e0cf9 补的缺口——B 子代理申报 rename_path 漏埋点，主会话直修红→绿）；同步任务三源 select!（兜底 interval / 本地 notified / SSE 门铃）；SSE 断线退避 1s→60s、重连即补一轮 pass；帧解析跨 chunk 缓冲切帧、坏行 warn 不断连。
- **端到端延迟**：上传成功→本端秒级 push→对端门铃→pull 应用 ≈ 1-3s（对比轮询平均 5min）。
- **部署注意（openresty/nginx）**：SSE 经反代需确认 `proxy_read_timeout >= 60s`；服务端已发 X-Accel-Buffering: no + 20s 心跳，openresty 默认 proxy_buffering 对该头响应关闭缓冲，一般免改；若自建层仍缓冲则显式 `proxy_buffering off`。
- **门禁**：win workspace 510 / wsl 三 crate 416 全绿（+35 测试）；fmt/clippy 零警告。
- 已知边界留痕：`MetaDatabase::rename_path` 仅 WebDAV MOVE 路径调用（已埋点覆盖）；无 keepalive 的死流 reader 任务至进程退出回收（注释声明）；subscribe 长连接无上限（家庭规模）。

## 2026-09-05 准实时批二复审（独立双审查员，54c8a0f..3eb56ce）：High×3 + Medium×2

- **结论**：服务端侧可发布（EventHub 锁序/same_channel 清理/pump 退出路径经交错推演+真连接测试双重验证无竞态无泄漏；secret 三端点闸/403 前置/日志脱敏/心跳承诺与代码一致；wire 兼容矩阵实测成立；src 零 unwrap）；客户端侧**需修后发布**——三条 High 都在主用户路径上静默掏空准实时卖点（均不破最终一致，300s 兜底仍在）。
- **High-1 WebDAV DELETE 漏唤醒**：`webdav remove_file/remove_dir` 直写 db（Explorer 删除主路径）——删除墓碑最长 300s 才推。**High-2 Web UI /api/delete 漏唤醒**：`vfs.db().delete_file` 直写（自称复用 Vfs::remove_file 裁决却绕过了 Vfs 层与 wake）。修法各一行 + enable 模式测试（43e0cf9 MOVE 先例）。
- **High-3 SSE 读无活性检测**：reader `frame().await` 无超时、TCP 无 keepalive——半开连接（NAT 过期/睡眠唤醒/切换 Wi-Fi）下门铃永挂、退避重连一次都不触发，功能死亡至进程重启。修法：frame 读套 timeout（≥3× 心跳，60-90s 无帧断流走既有重连链）。服务端 20s 心跳本可作活性探针而未用。
- **Medium×2**：①SSE 帧切分只认 `

` 且 pending 缓冲无上限——CRLF 合规流永不切帧 + 恶意慢速流内存放大（既有 CRLF 测试只测帧内容解析给了虚假覆盖信心）；修法=识别 `

`/`

`/`` + 64KB 上限。②**db 整库拷贝到第二台机器 → 两机同 client_id → 门铃双向互静默**（两层 origin 跳过都命中；db 拷贝是本项目文档化/验收用过的操作）——正确性无损、实时性静默退化且不可诊断；候选缓解：服务端同 (ns,client_id) 双活检测踢旧连接（~20 行）/至少 warn 日志，客户端 doctor 检查或 client_id 重置途径。
- **Low 若干**：SYNC_HEARTBEAT_SECS 巨值 panic 循环（加上界 1..=86400）、serde 400 回显理论渗漏面、客户端 connect await 无停机门、debug 日志全量 origin（应对齐前 8 字符）、setup 重跑静默抹手写 sync_secret、enqueue 失败路径不 ring、测试缺口（Lagged/双订阅断一/陈旧 reap/双活）。
- **审查排除的疑点（附依据）**：三源 select! 无丢唤醒（tokio 文档舞步逐路径推演）；重连无风暴；hydrate/LRU/cache-clear/sync-apply 不 ring 的正确性（payload 字段集+测试钉死）；Bot /mkdir /rm 与 web upload 走 Vfs 已覆盖；唤醒热路径原子级无误唤醒；服务端 publish 在 store 锁外、锁序单一。
- 修复批建议待负责人裁决：P1=High-1/2/3（三处均小修）；P2=Med-1 帧切分；P3=Med-2 client_id 双活（含语义裁决）；Low 捎带。

## 2026-09-05 准实时复审修复批 P1+P2+P3（fix/realtime-review-fixes，0.7.1）

- **P1 三 High**：①WebDAV DELETE（remove_file/remove_dir）补唤醒（Explorer 删除的墓碑即时推送）；②web /api/delete 改走 `Vfs::remove_file`（消灭路由内复制的 pending 保护逻辑、顺带清缓存副本；**IsDirectory 分支保留直删+补唤醒**——Vfs 无 remove_dir、UI/基线支持目录删除，裁决为纯文件路径走 Vfs）；③SSE reader 的 frame 读套 90s 空闲死线（半开连接自杀式断流→既有重连链自愈；服务端 20s 心跳为活性探针；注入 seam `with_frame_idle_timeout`）。
- **P2（Med-1）**：帧切分识别 `

`/`

`/`` 三合法界（新签名返回 (content_len, term_len)，纯 LF 流切点逐字节不变、单测钉死）；`pending` 缓冲 64KB 上限（超限 warn+断流重连）。审查指出的「CRLF 测试只测内容解析」误信源已补流级三测。
- **P3（Med-2）**：EventHub 增 per-(ns,client_id) 活跃订阅计数（与 receiver 创建同临界区；pump 退出唯一清理点递减、先于 receiver drop）；双活（≥2）时**不跳过且事件 origin 置 None 下发**（客户端二次跳过自然放行、回声走幂等闸；单活保持省一轮 pass 优化不变）；双活出现即 warn（ns/client 各前 8 字符，提示 db 拷贝嫌疑）——静默退化变可诊断。客户端零改动。
- **门禁**：win workspace 524 / wsl 三 crate 427 全绿（+14）；fmt/clippy 零警告；三单元红→绿断言零漂移（P1 单元 3 绿 commit 的新增对照测试为增量非改动）。
- **遗留**：Low 项未动（SYNC_HEARTBEAT 上界 panic 循环、serde 400 回显、connect 停机门、debug 全量 origin、setup 抹 secret、enqueue 失败唤醒、测试缺口四项）——decisions 上一条挂账。

## 2026-09-05 唤醒收口批（refactor/wake-chokepoint，0.7.2）：files 表变更门铃单点化

- **动机**：手工 wake_sync 埋点是「靠人记得」的设计缺陷——0.7.0 审查已实证产出两条漏埋 High（后补）；负责人裁决按架构改进收口。
- **实现**：MetaDatabase 构造时装 rusqlite `update_hook`（需 `hooks` feature）——INSERT/UPDATE/DELETE 且表名=files 即 `notify_one`（同步上下文安全）；Notify **db 自持**（db 先于任何消费者存在，生命周期=钩子，pub API `vfs.sync_notifier()` 委托零变化）；**抑制位** RAII guard（`#[must_use]`，Drop 恢复，非嵌套语义=至多良性多响一次永不丢响）；消费方=sync apply 整段（丢推不可能论证注释：applied 行与 mirror hash 相等不推）+ hydrate 三处 is_cached 写 + clear_cached_flags（is_cached 不入 payload 的原则一致化）。
- **埋点退役**：Vfs 四处/upload_queue ring+字段/webdav 三处/web 一处全部删除；新写路径自动获得唤醒（覆盖面净增：webdav MKCOL 直写此前就无埋点）。webdav MKCOL note：等下——MKCOL 走 Vfs::create_dir？收口后无所谓直写与否，钩子统管。
- **已知后果与修复**：`notify_one` 无等待者存 permit → 5 个既有正向 rings 测试（seed 写库存 permit）被陈旧回声空洞化（子代理如实申报）——主会话补 drain 纪律（`drain_stale_wake_permits` 辅助，50ms 窗耗尽存留 permit）恢复判别力（b2f4286）；真实证明力=db_wake.rs 三测（直写 db 也响/抑制静默+恢复/chunks 写不误响）。
- **门禁**：win 527 / wsl 430 全绿；既有全部行为护栏测试零改动通过（等价性证明）。rusqlite +hooks feature（非默认，无版本变化）。
- **后续待办不变**：百度 spike 等凭据；trait 瘦身等 spike 结论；Low 项挂账。

## 2026-09-07 rs-cloudfs 建仓与 Phase -1 规范先行（负责人六项指令）

- **建仓**：E:\Rs_Codess-cloudfs fork 自 rs-CyDrive@9a691f2（全历史保留；remote 改名 upstream-cydrive 且 push URL 置 no-push 防误推）；test/ 凭据目录未随 fork 入库（gitignore 两仓同规则）。
- **规范先行（负责人指令「代码未动，规范先行」）**：docs/standards/ 五份（architecture/code-style/interfaces/logging/documentation）+ AGENTS/README 重写 + 融合基线设计 v1.0→v1.2。规范素材 = rs-CyDrive 全部生产纪律 + PrivateCloudFS 正反经验（错误泄漏/硬编码密钥/CTR 无认证/调试残留为反例；能力位/TokenCallback/conformance 思想为正面）。
- **红队复审（独立子代理）修复集**：H1 本条目与提交落盘；H2 architecture §1.5 过渡豁免清单（ck-telegram→core 反向依赖 Phase 1 R 解除、crate 名 Phase 0、R4 conformance 前置 Phase 2、core 合体长期豁免+组合根豁免）；H3 multicloud 计划加 SUPERSEDED banner（Kickoff 作废、附录 A 仍有效、接口形状以 D1-D10 为准）；H4 conformance 最小断言集八条落 interfaces.md + Phase 2 前置任务「驱动接入手册」；M1 local 入分层图/crate 树；M2 D6 local 卷形态（规范化根路径）+ D10 权威后端 sync 验收口径；M3 Phase 1 R 批验收补真机 Telegram 冒烟；M4 AGENTS 补 PROPPATCH 教训/补遗节指针/继承挂账；M5/M6 Phase 0 交付 check_layers 脚本与 CI 秘密扫描；M7 §7a E2E 隔离裁决（独立测试 chat + /_e2e/ 前缀 + 收尾清理）；M8 层级规则组合根豁免与 crypto 定位；L1-L7 全修（README 链接/章节序/门禁口径统一/参数守卫条款/R4 local 豁免/文档两档制）。
- **待负责人确认**：基线设计 §9-2/9-3/9-4（旧仓冻结时点 / bot 分 crate 时点 / R-E 批序）。

## 2026-09-07 Phase 0 搬迁改名批收口（P0-A/B/C，直 main）

- **P0 直 main 裁决**：Kickoff 预授权「P0 可直 main（机械批酌情）」，采纳直 main——每 commit 三步门禁全绿 + git mv 保历史，机械噪音不进 feature 分支；S/R/E 自 main 切 `feat/phase0-1` worktree 执行。
- **改动**：7 crate 改名（cloudkit-core/-sync-server/-cli/-platform/-web/-webdav + drivers/ck-telegram，fe61e8e..50d13b2，119 文件）；bin 名 `cydrive`/`cydrive-sync-server`、`CYDRIVE_*` env、`cydrive_sync.db` 等用户契约零变化；scripts/check_layers（R1 机械闸，组合根豁免 cloudkit-cli）+ scripts/scan_secrets（R3 最小闸，扫事件 diff 新增行，紧模式防误报）+ CI 双 step 接入（b9bc044）。
- **取舍**：①telegram 落 crates/drivers/ 子目录（foundation §3 结构）；②scan_secrets 模式大小写敏感（-i 会命中 gen_compat_fixtures.py:31 已知假密码，误报会阻断 CI）——局限已注于脚本头；③examples 形态：Batch S spike 将以 workspace-excluded 独立 crate 落 examples/baidu_spike（真网调用不进 CI 门禁，零 workspace 依赖耦合）。
- **门禁证据**：win 每改名 commit 前 fmt/clippy/test 全绿（527 不变）×7 + 终态全绿；WSL clone ~/rs-cloudfs + cargo check 过 + 全量 test 528 passed 0 failed（+1 = bin_cli.rs 平台 cfg 既有差异，rg 核查非改名引入）；`cydrive --version` = `cydrive 0.7.2`。
- **待负责人**：scripts/gen_compat_fixtures.py 输出路径仍指旧 crates/cydrive-core（fixture 文件已随 git mv 迁移，脚本不在本批授权内未动）——修复时点待裁决；CI workflow 从未真实触发（仓库无 origin），首跑需校准。
- **回滚**：git revert fe61e8e..b9bc044 各 commit（纯机械可逆）；WSL 通道可整目录删除重建。

## 2026-09-07 Batch S 百度 spike：断点续传取「持久化 uploadid」而非「重 precreate 已传列表」

- **冲突双方**：multicloud 计划 Batch S 任务原文预设「precreate 返回已传分片列表、重传只补差集」 vs 实测 API 行为——相同 path/size/block_list 二次 precreate 返回**新 uploadid + 全量待传列表**（旧会话分片不随之返回）。
- **采纳裁决**：B2 断点续传 = precreate 后**立即持久化 uploadid**（path/size/block_md5/完成位图随分片落盘），恢复时用旧 uploadid 重发一个缺失分片探活，活则只补差集（spike 实证：phase1=[0,1,2] 后 phase2 仅传 [3..7]，create errno=0 即服务端确认旧分片保留）。
- **风险**：uploadid 会话有效期未知（spike 仅验证跨进程分钟级存活）；若过期则整体重传（兜底路径已实现）。附带发现一并约束 B2：服务端 `md5` 字段为内部 content-id 非字面 MD5（校验走内容比对/CDN content-md5 头）；rtype=1 为冲突重命名（覆盖语义需 rtype=3，待复核）；CDN 下载仅授权 ≤4MiB 有界 Range + netdisk UA。
- **证据**：docs/reports/2026-09-07-baidu-spike.md §3。

## 2026-09-07 Batch R：cloudkit-storage 契约面六项裁决（D1 草图偏离）

- **背景**：R-1/R-2 落地 L2 存储抽象（红 489852e → 绿 0625e65，TDD）；D1 签名是草图，以下偏离点逐项裁决（正文亦见绿提交）。
- **① list 返回 `Listing{entries, next: Option<PageCursor>}` 而非草图 `Vec<Entry>`**：游标型后端必须回吐续读位置，裸 Vec 使「分页语义统一在 Page」不可实现；offset 型后端以不透明令牌编码 offset 即可归一。
- **② writer 返回 `Box<dyn UploadStager>`，close/abort 消费 `self: Box<Self>`**：dyn 兼容（`Arc<dyn StorageDriver>` 直传）+ 暂存器终态语义；`write(&[u8])` 而非 Bytes——trait object 调用面低摩擦，零拷贝诉求由 reader 侧 Bytes 承担。
- **③ `ByteStream = Pin<Box<dyn futures Stream<Item=Result<Bytes, StorageError>> + Send>>`**：流中途错误必须承载分类学（R2），`tokio::io::AsyncRead` 做不到；AsyncRead 适配归 L4（WebDAV 层）。
- **④ Capabilities 为 bool 字段 struct 而非 bitflags**：零新依赖；九位逐字段文档注释承载 R4 诚实语义（含 conformance 断言映射）；子集比较 `contains` 逐位判定。
- **⑤ conformance 接入形态 = `assert_conforms(&dyn ConformanceHarness)` 函数 + `conformance_suite!` 宏糖**：错误回放（断言⑤）经 harness 注入符号码 + 期望映射——L2 运行时不认识任何后端错误码（R1）；百度 errno 三档只以 test-only fixture 钉死（110→Unauthorized{true}（自动刷新重放一次后仍失败）、111/-6→Unauthorized{false}），真实映射表归 ck-baidu（Batch B1）。
- **⑥ RESUME 断言⑦观测点取「中断之后」**：重传量只计第二次上传实际发给后端的字节（第一段流量是正常上传成本非差集）；声明 RESUME 却不可观测 → 套件判失败（R4：声明即必须可验证）。
- **验证**：tests/ 红绿两提交零 diff（`git diff 489852e 0625e65 -- crates/cloudkit-storage/tests/` 为空）；workspace 门禁 555 passed 0 failed；套件在实现期当场抓住 mock `ensure_parents` 把目标末段建成目录的 bug（断言① commit-on-close 不可见性红）——套件检出力的一次真实行使。

## 2026-09-07 Batch R 第二段：CloudTransport 家族迁 L2 与错误归一（R-3/R-4）

- **背景**：trait 住在 cloudkit-core 造成 ck-telegram→core 反向依赖（architecture §1.5 过渡豁免首行）；D2 要求 L3+ 永远只见 StorageError。红 93a8981 → 绿 8f8e784（波及面清单与变体映射表全文在绿提交正文）。
- **① trait 拆分形态**：核心面 = connect/upload/open/open_range/delete_remote + `capabilities()`（**必选**，镜像 StorageDriver 强制诚实声明）+ `as_inbound()`/`as_chat()`（provided 默认 None，免 dyn upcasting）；incoming 移入 `InboundCap`、send_text/send_document 移入 `ChatCap`（默认实现返回 `StorageError::Unsupported`，替换原 transport 本地「not supported」Remote 错误）。
- **② 错误归一映射**：NotConnected→Invalid（非法状态类）、FloodWait{seconds}→RateLimited{retry_after:Some}（逐秒保留）、Disconnected/Remote→Unavailable（消息保留）、NotFound(i32)→NotFound（id 载荷舍去——分类学无载荷变体，诊断上下文由调用点日志承担；属本批唯一载荷损失点，已逐类列出）、Io→Io(String)（storage error.rs 增 `From<io::Error>`）。Unauthorized 预留 Phase 2 baidu 三档（telegram bot 无自动自救流，不发明行为）。
- **③ 随迁 L2 的契约载荷**：`vpath::RelPath`（VFS 绝对式路径——UploadJob 字段所需；与 `vocab::RelPath` 卷相对路径并存是有意的，归一留 Phase 2 议）+ `part_name`（分块命名契约 3，mock 与 telegram 共用单一实现）；core 以 re-export shim 维持 `cloudkit_core::{rel_path, transport, chunker::part_name}` 路径——core/webdav/web/cli 测试 import 零改动。
- **④ 消费方最小适配（R-5 的子集就地完成）**：bot.rs 处理器签名换 `&dyn ChatCap`（调用点零改动）；inbound worker 内 as_inbound/as_chat 探测（缺位 warn 不 panic，无 INBOUND 位则 worker 不启动）；上传降级通知经 as_chat（缺位日志跳过）。webdav/启动日志声明面留 R-5。
- **⑤ 能力位声明（R4 过渡期）**：grammers = INBOUND/CHAT/RANGE_READ/MULTIPART（依据：驱动单测 + rs-CyDrive 生产真机；conformance 前置属 Phase 2，已在代码注释注明；未声明位逐个列明理由——telegram 为影子索引 D4 故无 AUTHORITATIVE_INDEX 等）；Mock = INBOUND/CHAT/RANGE_READ（恰为上游测试行使面）。
- **护栏证据**：既有 555 测试全程保持绿；迁移期护栏当场抓住 mock 单块命名分支丢失（`hello.txt` 被写成 `hello.txt.part000`）——修复后全绿；终态 562 passed 0 failed（+7 新语义测试）/ clippy -D warnings / fmt / check_layers 全过；`rg 'cloudkit-core' crates/drivers/ck-telegram/Cargo.toml` 为空（注释措辞一并避让字面命中）。
- **取舍记录**：transport::ByteStream（Send+Sync，历史接缝）与 vocab::ByteStream（Send，StorageDriver 家族）两类型并存——bounds 延续迁移前形态保 WebDAV 消费面零变化；transport 模块符号不在 storage crate root re-export（防与词汇类型遮蔽）。
