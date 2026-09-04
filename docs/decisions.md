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
