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

## 2026-09-07 Batch E：cloudkit-crypto 独立成 crate + v2 分块 AEAD 格式裁决（E-1/E-2）

- **背景**：foundation D7 要求加密装饰器双格式——v1 GCM 冻结维护（Python 互操作，R6）+ v2 分块 AEAD（流式+随机访问+流式水合）。红 3e46ea8 → 绿 19a2f2f（迁移 23702ea 在前）。
- **① crate 切分与依赖姿态**：`cloudkit-crypto` 独立 crate（不依赖 workspace 内其他 crate），crypto 定位 L2 侧基础能力被 core 消费不算违规（architecture §1.5 注）；core/src/crypto.rs 改薄 re-export shim 维持 `cloudkit_core::crypto::*` 路径（vfs/upload_queue 及测试 import 零改动）。**流式原语取 sync `std::io::Read/Write` 而非 async trait**——零 tokio 依赖保持 crate 轻量，异步适配（spawn_blocking 包裹）归 L4 消费方（E-3 接线时定）。
- **② v1 迁移的「字节不变」证明形态**：函数体逐字迁移（src/v1.rs），共享 CryptoError 上移 crate root（原两变体序与 Display 字符串原样）；Python 互操作向量测试 git mv 随迁（前后各 11 passed），文件 diff 仅 2 行机械路径调整（模块 doc 头 + use 行），断言零改动。GcmV1 经 trait 的流式面为**迁移特征化测试**（格式恒等断言），不要求 TDD 红——红集中在 v2 新语义。
- **③ v2 容器格式（自设计，STREAM 构造）**：头 34B = `magic b"CKCRYPT2" | version 0x01 | reserved 0 | salt 16B | PBKDF2 迭代数 u32BE | chunk_size u32BE`；每块独立 AES-256-GCM，**nonce = 4B 零前缀 || counter_be56 || 末块标志字节**（末块判定进 nonce 域分隔——边界截断后新尾块以 final 位解密必认证失败，这是防截断的核心机制，对齐 Tink STREAM）；**AAD = 完整 34B 头**（salt/KDF 参数/块大小与每块密码学绑定，头部拼接与逐字段改写全拒）。非末块恒 `chunk_size+16`B；末块 `1..=chunk_size+16`（空文件=单个空末块）；结构自定界（`n-1=(body-16)/(chunk+16)` 唯一解且校验末块长落界），decrypt_range 凭总长定位块不触碰数据；**非典范空尾块（n>1 且末块明文 0）按 Malformed 拒绝**——编码器只对空文件发空末块。
- **④ 流式末块判定 = 1 字节前瞻而非持两块**：fill 满一块后 peek 一字节判 EOF——工作集 = 块缓冲 + 1B（优于 hold-back 双缓冲），代价是精确倍数文件以**满末块**收尾（无空尾块，格式非典范形态因此可拒）。粒度证据：9.5MiB@1MiB 读粒度 ≤ 块、写粒度 ≤ 块+tag。
- **⑤ KDF 与 v1 协同而非独立演进**：默认同参数（PBKDF2-HMAC-SHA256 100k、16B 随机 salt），但**迭代数入头**——后续调优不改格式破兼容；v1 保持硬编码（冻结契约）。跨格式 key+nonce 撞车需 2^-128 salt 碰撞，忽略。
- **⑥ 解密侧容器自述优先**：chunk_size 以头内值为准，读者 AeadV2 配置不参与解密（防错配解密出垃圾/误报）；配置只管加密侧。decrypt_range 语义 = storage Range 同款**半开+钳制**（end 钳到明文长、start≥长得空）——与 L2 Range 断言②语义对齐，hydrate 分发（E-4）无需翻译。
- **⑦ 已知风险挂账（待负责人裁）**：头内 PBKDF2 迭代数不可信——伪造头可放大 KDF 工作量（测试翻转高位字节即 ~16.8M 迭代，单测跑 ~60s 的原因）。候选缓解：头内迭代数上限（Malformed 拒绝）或 KDF 参数版本化；本批不动（改动会动已红测试的期望错误类，且上限值选择需裁决）。
- **⑧ 零 serde 先行**：CryptoSchemeId 暂不带 Serialize/Deserialize——E-4 Entry 字段落位时随需加 derive（YAGNI，避免无消费者的依赖面）。
- **验证**：aead_v2 16 / gcm_v1 11 / scheme 5 全绿；workspace 586 passed 0 failed（565 基线 + 21 新增）/ clippy -D warnings / fmt / check_layers 全过；红证据 3e46ea8（aead_v2 15 failed + scheme 1 failed，断言红）。

## 2026-09-07 Batch R 收口（R-3~R-6）+ 并行会话异常记录

- **R-6 冒烟协议裁决**：计划原文「用 D:\Tools 生产实例」+ Kickoff「禁止动生产 db 与部署位」存在张力；发现实例停机态后取最保守路径：备份 db/session/config 到 OS 临时目录 → **新仓 release 产物**（bin 名 cydrive 不变，契约实证）原地冷启动 → /_e2e_smoke/ 前缀写读删（上传消息进生产 chat 一条 + db 增删行，属计划明示授权的非破坏性谨慎操作）→ 优雅停止还原停机态。部署位二进制文件未改动（跑的是 E:\Rs_Codes\rs-cloudfs-p01\target\release\cydrive.exe）。回滚：%TEMP%\cydrive_smoke_backup_20260907 5 文件覆盖还原。
- **PROPPATCH 陷阱条款回归通过**：207 全成功，MiniRedir 整单不回滚。
- **⚠ 并行会话异常（待负责人确认）**：16:18–16:39 窗口（本会话正执行 R-6，未派发任何子代理）分支上出现 Batch E 第一段 4 个 commit（23702ea/3e46ea8/19a2f2f/6eedb25，作者同为 viccom）——疑似负责人或另一会话执行。本会话已派独立子代理复核：detached HEAD 重放红 commit 输出与 commit message 逐字吻合、v1 互操作向量 11 测试迁移前后一致（diff 仅 2 行机械路径）、函数体逐行比对零变化、门禁 586 passed 0 failed + clippy/fmt/check_layers 全过——**质量验证通过后采纳**。若非负责人所为请告知，可整体 revert 该 4 commit。
- R-3/R-4/R-5 细节裁决已在各自 commit 正文与 tracker 行内（波及面清单/映射表/webdav Range 现状裁决/mock 能力注入面）。

## 2026-09-07 Batch E 第二段：E-4 scheme 元数据 schema + E-3 流式上传面（E-3/E-4）

- **背景**：foundation D7 收尾——v2 需要入口（配置键）、身份（Entry 元数据）、通道（流式上传）与回读（按 scheme 分发水合）。E-4 红 e7581c3（前段会话落位的 10 红）→ 绿 64d9eb7；E-3 红 747e23d → 绿 d7fa206。
- **① files 表加列裁决（R6 触点）**：不新建迁移框架——沿用 sync-lite 表的「契约 DDL batch 冻结 byte-identical + Rust 附加 batch」既有模式扩展到列级：`encryption_scheme TEXT NOT NULL DEFAULT 'gcm'` 经 pragma_table_info 探测守卫的 ALTER TABLE 落位（SQLite 无 ADD COLUMN IF NOT EXISTS；守卫保重开/并发采纳幂等）。默认值 gcm 使 E-4 对既有库纯加法：旧行回填即其既有行为（v1），Python 形 INSERT（省略列）继续工作，Python 基线实例读新库天然忽略未知列。FILE_COLUMNS 显式列名 SELECT，逻辑列序不依赖物理列序（迁移列追加在表尾）。
- **② scheme 写入面最小化**：仅两个 origin 写 scheme 列（put 按配置、sync apply 恢复拉取行），其余全部写路径（队列成功持久化/hydrate 缓存翻转/入站索引/目录行）继续 plain upsert_file——insert 走列默认、conflict-update 保留旧值，既有写路径行为零变化（红测钉死 plain upsert 保留语义）。
- **③ sync payload wire 形态**：`scheme: Option<String>`，serde default + skip None——**仅非 gcm 加密行携带**（gcm 行与 pre-E-4 payload 字节一致，由守护绿测试钉死）；旧形态 JSON 解析为 None（=gcm）；旧消费者 serde 丢弃未知键不炸。apply 侧 replace_row 改走 upsert_file_scheme 恢复（None→gcm）。
- **④ 流式上传面 = CloudTransport provided 方法而非可选 trait+能力位**：`upload_stream(job, ByteStream)` 默认 `Err(Unsupported)`（interfaces §1 演进规则：优先 provided 方法）。理由：真实实现者仅 mock/telegram 两面，能力位+探测样板是过度设计；Unsupported 沿用队列既有重试/降级语义。ck-baidu 接入时若流式面普遍缺失再升能力位（tracker 待办）。**复用 UploadJob 而非新类型**：size/chunk_count/chunk_size 与 upload() 同语义（mock 双面共用 finish_upload 校验），local_path 降格为 provenance（线上字节来自流）。
- **⑤ v2 上传的内存模型**：明文 File（spawn_blocking）→ AeadV2::encrypt_stream 逐块 → ChannelWriter 每次 write 一帧 → 容量 4 的 tokio mpsc（背压）→ unfold 成 ByteStream → upload_stream。峰值常驻 = 通道容量 × 1MiB + 加密器一块工作集，与文件大小无关；证据形态 = mock max_stream_frame 峰值帧 ≤ 1MiB+16B（crypto 侧粒度契约钉到传输缝）+ 8.5MiB 数据量级。密文总长按公式预先精确计划（34B 头 + 明文 + n×16B tag；空文件一块 tag-only），驱动 chunk plan 无需先见密文。
- **⑥ v2 重试重跑加密（含 PBKDF2+新盐）**：无密文暂存可复用正是流式路径的定义（零 .enc.tmp）；重试罕见，逐次新盐是密码学保守方向。
- **⑦ hydrate 三路分发**：gcm → 冻结整文件解密路径（零变化，护栏测试：v1 行在 v2 配置下照走 v1）；aead_v2 → 流式水合（staged 密文 → decrypt_stream 逐块 → .dec.tmp 兄弟 → 原子 rename → 删密文；失败自清 .dec.tmp）；未知 scheme → VfsError::UnsupportedEncryptionScheme（点名坏值+两已知值+升级指引——新版本实例加密的文件绝不猜格式）。分发依据是行内 scheme 列，**永不读活配置**（换配置键不断旧文件回读，红测钉死）。
- **⑧ telegram 桥**：StreamReader（ByteStream→tokio AsyncRead，单帧缓冲）+ 每 part take 喂 grammers upload_stream；caption/命名复用 plan_chunk_sends 的 is_encrypted=false 形态（v1 密文上传同款——R6 caption 契约）。流长于计划拒收。tests/stream_reader.rs 冒烟抓住 split_off/split_to 取片方向 bug（帧内字节倒置）——绿前修复。
- **验证**：E-3/E-4 红→绿序列 604/7 → 613 passed 0 failed（6 ignored 既有真机）；clippy -D warnings / fmt / check_layers 全过；v1 护栏零改动（tests/upload_queue.rs 唯一 diff = test_cfg +1 字段机械修；crypto 全部测试文件零 diff）；wire 双向兼容与 DB 迁移采纳由 encryption_scheme_metadata.rs 6 测试钉死。真机 telegram v2 冒烟 = E-5（主会话统一安排）。

## 2026-09-07 E-5 缺陷修复：加密行 hydrate 下载预算（方案 B 裁决 + mock 诚实化）

- **现象**：真机 v2 加密文件 push 成功后 pull 失败——`crypto error: decryption failed`（末块 GCM tag 失败）。db 行 `size=2621440`（明文长）而 v2 容器实长 2621522（34B 头 + 2×(1MiB+16B) + (512KiB+16B)）。
- **根因**：行 size 是 Python 契约（R6）的**明文长**，远端工件却是**密文容器**（恒长于明文：v1 +44B salt/nonce/tag；v2 头+逐块 tag）。hydrate 把 row.size 填进 `RemoteHandle.total_size`，而 telegram `open()` 用 `serve_range(frames, 0, total_size)` 把预算当硬上限——密文被截 82 字节，末块认证必败。**v1 同型缺陷**（密文长 44B 余量，同机制截断）；此前未暴露仅因 v1 加密 pull 从未上过真机（Tier-1 均明文/M2 旧文件，E-5 是首个加密真机冒烟）。实现期以「telegram 同语义裁剪」探针离线复现证实（v2 `Crypto(AuthFailed)`、v1 两路 `Crypto(TooShort/AuthFailed)`）。
- **为何单测没抓到**：MockTransport::open 历来无视 `total_size` 全量吐出——mock 比真后端慷慨，预算错配对一切上游测试不可见；而种子型 hydrate 测试以**密文长**播种行 size 恰好自洽，掩盖了真实上传路径（明文长）与 hydrate 预算的矛盾——两套约定一直并存且互不知情。
- **修复（方案 B，红 2f07c9f → 绿 bab0794）**：hydrate 对 `is_encrypted` 行 `total_size=u64::MAX`（无界），明文行维持 row.size 预算不变。理由：R6 零触碰（db/sync/行语义全不动）、v1 既有断言零改动（v1 冻结契约测试文件对 c32115b 零 diff 自证）、v1 真机同型缺陷一并修复、单点可逆。**无界而非按 chunks 行求和**：telegram `open()` 本就逐 part 全量拉取（`download_span(0, u64::MAX)`），serve_range 只是裁剪器，u64::MAX 即原文直通、零额外 IO；且不依赖 chunks 二级元数据完整性（空表 fallback/跨版本行 size 语义）。密文容器自述长度且 AEAD 认证，解密器自会校验完整性与认证——预算不是正确性来源。
- **mock 语义补齐**：MockTransport::open 增加 serve_range 同语义裁剪（至多 total_size 字节、超读裁掉、预算内短读容忍）——mock 与真机预算语义对齐，堵住盲区；无界预算下裁剪为 no-op，既有测试全量回绿、断言零改动。新测试面：`tests/transport.rs` 8a（mock 裁剪语义）+ `tests/vfs_encrypted_budget.rs`（BudgetTrimTransport 诚实传输层下 v1/v2 加密往返 + 明文控制组——hydrate 契约不依赖 mock 自身诚实化进度，双保险）。
- **待负责人复核（方案 A 备选语义）**：加密行 WebDAV Content-Length 目前=明文长（行 size=明文长的自然推论，与 Python 一致）。若未来希望加密行 Content-Length=密文长（行 size 改存密文/容器长），需明示裁决——那是 Python 契约语义变更（R6 触点）、必须同步改既有 v1 断言（`encrypted_roundtrip` row.size==14 等）且跨版本 sync payload 含义漂移，已在执行期被否一次（2026-09-07，主会话裁决记录于 bab0794 提交正文）。
- **验证**：红 614 passed / 3 failed（既有 613 全绿 + 明文控制组绿）→ 绿 617 passed / 0 failed / 6 ignored；clippy -D warnings / fmt / check_layers（9 manifests）全过。真机 v2 冒烟复验由主会话执行。

## 2026-09-07 Phase 0+1 全量收口（C-1，feat/phase0-1）

- **E-5 真机复验通过**（主会话）：修复版 release 下新推 1.25MB（跨块）与遗留 2.5MB 两文件 push→cache clear→pull 字节全等；清理/停止/配置逐字节还原（enable_encryption=false）/停机态保持——v2 全链（流式加密→telegram→流式解密）真机闭环。
- **版本 0.8.0**（workspace 单点）：trait 破坏性演进（CloudTransport 家族迁 L2 + InboundCap/ChatCap 拆分 + StorageError 归一）+ 新 crate cloudkit-storage/cloudkit-crypto。bin 名 cydrive/cydrive-sync-server 不变。
- **合并策略**：feat/phase0-1 全部 commit 合回 main（本地 merge；无 origin 远端可推）。worktree 移除、分支保留可追溯。不部署生产位（Phase 2 验收后统一裁决，计划原文）。
- **执行期累积的待负责人项**（详见 tracker「待负责人清单」节）：baidu1 链失效、并行会话异常、方案 A 语义备选、KDF DoS 缓解、gen_compat_fixtures 路径、CI 首跑校准等。
- 门禁终态：win fmt/clippy 干净 + 617 passed 0 failed（ignored 6 真机）；wsl E 批态 618 passed 0 failed（+1 平台 cfg 既有差异）；release 全 workspace 构建通过。

## 2026-09-07 C-1 终态验证补遗（构建竞态 + 验证诚实性修正）

- **现象**：版本 bump commit（858b687）后 `cargo test --workspace` 连续三次编译失败（dav_server/base64「required to be available in rlib format」→ E0786 元数据无效），选择性 clean 无效，全清（46.7GiB）后首次并行重建仍失败；但单目标 `cargo test -p cloudkit-cli --test run_e2e` 可编可跑，重试全量即绿。
- **结论**：非代码缺陷（lockfile diff 仅 9 行版本号；d0da789 上 617 绿实证）——判为 Windows 全清后大规模并行 rustc 的瞬时竞态（文件锁类）。处置先例：单目标验证→重试全量。
- **诚实性修正**：收口条目初稿写「win 617 绿」时该结论在 858b687 上**尚未真实跑过**（链式门禁的测试步骤实际失败但被管道退出码掩盖——awk 空输入仍 exit 0 使 && 链继续）。现已实证补全（617 passed 0 failed，85 个 ok 行，零失败）。教训入册：**门禁链中禁用会吞非零退出的管道聚合，测试步骤必须独立判定退出码**。

## 2026-09-07 百度凭据与测试根裁决（负责人指令）+ 新根验证轮

- **负责人裁决**：① `E:\GitHub\rs-CyDrive\test\instances` 下的百度授权即**正式可用凭据**（此前 tracker 挂账「baidu1 链失效需补发」就此销账——baidu 链以 instances 现值为准，spike 侧 token 持续刷新维护即可）；② **测试根 = `/apps` 下新建子目录**（本轮用 `/apps/cloudfs-spike`），PCFS 的 `/apps/privatefs` 为其生产数据**绝不触碰**；③ 协议逻辑不明处**以 PCFS 源码（E:\Go_codes\PrivateCloudFS）为权威参照**——该项目生产可用，代码可能有 bug 但功能全通。
- **工具配合**：spike 增 `BAIDU_SPIKE_REMOTE_DIR` env 覆盖（指定根不探测直接用）+ dlink TTL 探针预算 63→131 分钟（4c0e46b）。
- **新根验证轮（2026-09-07，全部真实输出）**：token 直接可用（ls errno=0）；`/apps` 列 14 条目（privatefs 在列、只读）；`resume abort/continue` 在 `/apps/cloudfs-spike` 复现差集续传（[0,1,2]→只补 [3,4,5,6,7]，create errno=0，final_size_ok，uploadid 会话存活）；cleanup 远端剩余 0（xpan 删除进回收站 10 天保留，API 不可验证回收站——已知限制）；dlink TTL 上界长探针后台进行中（结果回填 spike 报告附录）。
- **对 Phase 2 B 批的指导意义**：ck-baidu 的 E2E 测试根沿用「/apps 下专用子目录」模式；协议实现疑义先查 PCFS 源码再自行试验。

## 2026-09-08 负责人裁决三项 + 两修复（KDF DoS / fixture 脚本）

- **加密行 size 语义维持方案 B（负责人明示）**：行 size=明文长（Python 契约对齐），hydrate 加密行无界预算——tracker「方案 A 备选」销账；未来若需密文长语义须再走 R6 流程。
- **KDF DoS 钳制（一类 #1 修复，红 2c6efc1 → 绿 9faad3e）**：v2 头 PBKDF2 迭代数上界 1M（默认 100k 的 10 倍，留演进余量），解析期拒绝超界头（Malformed，先于任何 KDF 工作）。契约变更：既有 bad_iters 断言由「燃烧后 AuthFailed」改为「解析期 Malformed」，新增 in-cap（200k）篡改腿保留 AAD/密钥绑定覆盖；aead_v2 套件 ~127s→13s（16.8M 伪造腿消失）。
- **gen_compat_fixtures.py 输出路径修复（一类 #4，5a3a319）**：两 fixture 在 Phase 0+E 批已分家（crypto_vector.json→cloudkit-crypto、python_meta.sql→cloudkit-core），脚本仍写旧 cydrive-core 死路径；改双输出目录并真跑端到端验证（两文件落位正确、两消费套件对新输出全绿）。**发现并记录**：`adopts_python_generated_database` 钉死了抓取时的 created_at epoch——重新生成 fixture 必须同步改该断言（fixture 与断言按设计耦合，非缺陷）；本轮还原旧 fixture 零 churn。
- 门禁：fmt/clippy 干净 + workspace 618 passed 0 failed（+1 新测试）；无 cfg 面改动，WSL 不适用。

## 2026-09-08 继承挂账修复批（P3 + Low×5 + 64MB 闸 + deny advisories）

- **P3 hydrate 快照回写竞态（红 2ba831c → 绿 47c4fc2）**：hydrate/驱逐三处整行回写（cached_upsert）改为目标列写 `set_cached_flag(id, bool)`（`UPDATE files SET is_cached=?1 WHERE id=?2`，0 行=行已消失良性 Ok）——下载窗口内并发更新（PUT 覆盖/sync 应用）不再被旧快照复活吞噬；与 E-4「契约 upsert 冻结+目标列写」同型，R6 零 schema 变更；cached_upsert 随批删除；红测试用 StallingOpenTransport（Notify 门）确定性复现竞态（无计时器）。
- **Low 清账**：①sync_url host 非空校验（f8040aa，可行动文案，手写最小解析不引 url crate）；②模拟器 pull 排序对齐服务端 version ASC（e93433a——原 (rel_path,version) 排序会掩掉顺序依赖 bug）；③SyncClient trait 四要素契约文档（09fff2c，interfaces §1）；④--help 全量盘点（bc34d43）：两 bin 全子命令对照行为，唯一实证漂移=setup 的 keyring 文案未提 headless 分支可写 config.toml，已修，其余无漂移；⑤凭据门槛：核实**本已统一**（两调用点同源 resolve_sync_secret，无绕过路径）——销账非修复。
- **64MB 并发闸（红 4b010b9 → 绿 08fee1d）**：sync-server push/pull body 聚合段加 `MAX_CONCURRENT_BODY_BUFFERS=2` 信号量（2×64MB=128MB 峰值预算），try_acquire 立即 503（排队会无进展信号烧客户端超时，更不透明）；SSE subscribe 不入闸；router_with_gate 注入面。
- **deny advisories 销账**：`cargo deny check advisories` → `advisories ok`（0.20.2；经 HTTPS_PROXY=127.0.0.1:7897 拉库——**旧仓「github.com 443 不通」限制自此有绕行方案**，直连仍不通）。
- 门禁：fmt/clippy 干净 + workspace **625 passed 0 failed**（618→625：P3 +1 / host 校验 +1（并入 config 套件计数）/ 模拟器 +1 / 闸 +4，按套件聚合）；无 cfg 面改动。
- **剩余验证类挂账**：#[ignore] 真机 ×3、litmus 套件——随真机窗口跑，不阻塞 Phase 2。

## 2026-09-08 PCFS 多实例模型研究（负责人指令）+ 多卷时机裁决待定

- **负责人信号**：PCFS 可同进程运行多个存储实例（同厂多账号/异厂混挂），要求认真参考——对 D5「v1 实例=后端」与 D6「v1 单卷」的既定节奏构成方向性输入，**多卷启用时机待负责人裁决**（本条目挂起）。
- **PCFS 实测形态（探索代理结论，引用为其文件:行号）**：①配置=进程级 config.yaml + 每实例一 JSON（data/instances/{name}.json，含 root_dir/encrypt/encryption_key/token）；②Registry=map[实例名]Driver+RWMutex，启动全量拉起（失败跳过）+fsnotify 热重载；③**无 OS 挂载层**——多实例是 HTTP API `/api/v1/storage/{instance}/{path}` + 前端切换器，非盘符；④加密完全按实例（CryptoWrapper 装饰器+每实例密钥文件）；⑤索引=纯内存 entryCache 惰性填充（重启即失，缓存模式全内容驻内存仅演示级）；⑥路由=driver:path 前缀 ID SplitN 分发；⑦「实例 ACL」实为 View 模式分发白名单（view_acl.json），非能力限制。
- **对 rs-cloudfs 的映射**：PCFS Registry/实例文件/每实例加密装饰器/前缀 ID 隔离可干净映射到 D6 VolumeId（VolumeId 即卷名，EntryId 前缀式天然隔离）；但 PCFS 无盘符——**我们的差异化恰是 OS 挂载**，多卷形态应为「一进程多卷各自挂盘」（U:=卷1, V:=卷2，每卷独立 webdav 路径/端口）或 union 根（D6 原文预留）。索引侧不复刻 PCFS 纯内存形态（我们 db 是持久资产）——倾向每卷独立 db/cache 子目录（R6 零 schema 变更）。
- **PCFS 坑（勿抄清单）**：List 失败静默回退陈旧内存数据；driver.Delete 失败被吞仍删本地索引（云端孤儿）；缓存模式 ETag 短前缀 ID 碰撞；CryptoWrapper 改名致 Name 前缀嗅探脆弱；baidu 根路径硬编码泄漏到聚合层（R1 反面教材原址）。
- **建议（待裁决）**：Phase 2 维持既定顺序（手册→ck-local→ck-baidu，单卷先行——多卷建立在有真实驱动之上），B3 后立即接 **Phase 2.5 多卷启用批**（Registry+实例文件+每卷加密+多盘挂载）；负责人若要求多卷先行也可（会先只有 telegram 一厂可挂）。负责人可用 A4 双进程形态（两目录两 config）先行测试百度加密/不加密双盘，不阻塞。

## 2026-09-08 多卷时机裁决（负责人：方案一）

- **裁决**：Phase 2 维持既定顺序（驱动接入手册 → ck-local → ck-baidu，单卷先行）；**B3 完成后立即接 Phase 2.5 多卷启用批**（Registry + 每实例配置文件 + 每卷加密 + 多盘挂载，PCFS 模式去坑版）。百度加密/不加密双盘的首轮测试用 A4 双进程形态（两目录两 config），不阻塞多卷开发。
- **附带**：telegram+加密过渡测试包同步交付负责人（release 产物 + 配置模板 + 使用说明，仓外目录不入库——R7）。

## 2026-09-08 Batch L：ck-local 交付（conformance 第一真实公民）

- **TDD 序列**：红 ea57136（Unsupported 骨架接入套件，断言① mkdir 红）→ 绿 04659d1（九方法全实现，①–⑥⑧ 绿、⑦未声明自动跳过）→ 修正 8f257e4（他卷 delete 契约对齐）。测试/实现委派隔离；绿阶段 conformance 既有断言零触碰（diff 审计过）。
- **K6 落地形态**：BackendHandle = rel_path 字符串（根=空串，可往返）；VolumeId key = `canonicalize` 绝对形态（Windows `\\?\` 前缀，L2 opaque 合法）。delete 空句柄（卷根）→ Invalid（删除卷根无意义且危险）；他卷句柄 → NotFound（trait 契约）。
- **实现期裁决**（计划授权范围内，可逆）：① staging = 根下 `.cklocal-staging/`（同卷保证 rename 原子），list 过滤该保留名（断言③集合完整性）；② **overwrite 不可见性超集**：chunk_size=1 契约使断言① 出现重复路径，第二次上传时旧已提交对象也须 staging 期不可见——writer 打开时旧文件 stash `.old`（abort/Drop 恢复，close 删）；③ 临时名 `{pid}-{seq}.part` 进程级 AtomicU64；④ **tokio::fs::File::write_all Ok 仅代表入队**后台 blocking 写，close 必须 `flush().await` 后取 metadata 才可信（曾致断言③ flaky，5×500 迭代验证修复）；⑤ 保留字符（`:?*<>|"`）跨平台统一拒绝 Invalid（防「Linux 可建、Windows 不可寻址」条目）；⑥ io 映射表 kind 敏感（NotFound/AlreadyExists/PermissionDenied→Unauthorized{false}/InvalidInput→对应变体，其余 Io）；⑦ rename 目标已存在显式预检 Exists（std::fs::rename 是覆盖语义）；⑧ quota total=None/used=0。
- **能力位**：range_read/server_side_move/authoritative_index=true（逐位注码）；resume/multipart/rapid_upload/change_feed/inbound/chat=false（注理由）+ 九位精确值静态锁测试。
- **验证**：`cargo test -p ck-local` 4 passed / 0 failed；workspace 628 passed / 0 failed（625 基线 +3）/ 6 ignored；clippy -D warnings / fmt / check_layers（10 manifests）全过。
- **止损点未触发**：套件未暴露断言语义缺陷（发现的都是实现侧问题）。

## 2026-09-08 Batch B1：ck-baidu 骨架 + OAuth + errno + 元数据面

- **TDD 序列**：红 9a969ac（12 文件骨架 + MockBaidu axum 内存后端 + 三套件 17 测试全红于 Unsupported）→ 绿 41f25e3（oauth/client/api/driver 六文件 +524/−77）。tests/ 两 commit 间零 diff（假绿检查过）。
- **黄金参照查证修正**（PCFS 源码否定任务情报中的猜测）：filemanager move filelist = `[{path,dest,newname,ondup:"overwrite"}]` 四键 + form `async=1`（PCFS api.go:829-845）而非 `[{from,to}]`；stat = `method=meta&path=<abs>`（api.go:110-113）非 filelist/fs_ids（fs_ids 是姊妹形态，delete 句柄解析采用，api.go:176-179）。move **不轮询 taskid**（PCFS 从不轮询，两源一致，勿发明）。
- **K 萁点落地**：K13（110 刷新重放一次/111,-6 零刷新直达 Unauthorized{false}/刷新 on-arrival 持久化——refresh 单飞锁 + 拿锁后陈旧双检，不浪费一次一换的 refresh_token）；K14（BaiduParams 无 appkey 默认值、不 derive Debug 防凭据 dump）；K15（31034/429 → RateLimited{None} + 单点重试一次、退避固定 80ms）；K16（MockBaidu = axum =0.8.9 内存后端 + 请求记录器 + 错误注入队列 + 一次一换 token 轮换）；K5（VolumeId=baidu:<uid> 经 uinfo /xpan/nas；BackendHandle=fs_id 十进制字符串）。
- **实现裁决**：mock 基建落位 `tests/common/mod.rs`（计划原文 mock_backend.rs——Rust 集成测试共享模块形态限制）；未知 errno → Unavailable 载荷保留 errno=<code>（表外码 47002 钉死）；-8 → Exists 注「mock 建模 + B2 conformance 复核」（PCFS 无处理证据）；list 无分页参数（两源一致）→ driver 内 offset 游标切 Page；mtime=server_mtime（两源一致无 local_mtime）；serde_json 补入依赖（响应解析必需）；R3 三防线（reqwest 错误 without_url/体片段 token 掩码/错误消息不拼参数）。
- **挂起 B2 复核项**：① rename 的 ondup=overwrite 覆盖语义与 trait「目标已存在 → Exists」的偏离（driver.rs 已显式声明，conformance 断言⑥若覆盖则需裁决）；② -8 映射。
- **验证**：ck-baidu 17 passed（oauth 4/errno 5/metadata 字节级 8）；workspace 646 passed 0 failed（628→646）/ 6 ignored；clippy/fmt/check_layers（11 manifests）/scan_secrets 全过。

## 2026-09-08 Batch B2 真网实证：precreate 会话锁定全量 block_list——流式策略改「到齐即传」

- **探针证据**（主会话，干净 curl 探针，netdisk UA + MSYS_NO_PATHCONV）：precreate 部分声明 block_list（1 片）errno=0；superfile2 未声明分片照收；create 带全量（3 片）→ **errno=31363**（与 precreate 声明不一致被拒）。spike 未踩此坑因其全量算 md5 后才 precreate。
- **裁决**：stager 流式策略「write 首块即传（部分 precreate）」→「**到齐即传**」（hint.size 已知且字节到齐 → write 返回前 precreate 全量 + 串行传满块 + 位图落盘；close 传尾块 + create 原样重申）。write 同步落定契约（conformance ⑦）保持；K7 会话恢复三路不变（drop/close-中断后二次 equip 差集补传）。mock create 增建模「block_list 一致性校验」（31363）。
- **附带实证**：create 目录/rtype=3/deep precreate 均真网可用（errno=0）——K10 rtype=3 复核通过；31363 纳入 errno 码表（Invalid 族——参数与预创建不一致）。
- **MSYS 教训**：Git Bash 探针中 `path=/apps/...` 形参被路径改写污染（→ C:/Program Files/Git/apps/...，百度报「文件夹 C: 命名不合法」errno=-7）——首批探针全部作废；干净探针（MSYS_NO_PATHCONV=1）推翻了「create 目录恒 -7」「rtype=3 不可用」两个错误中间结论。教训入 AGENTS 陷阱候选。

## 2026-09-08 Batch B2 真网实证（第四轮）：meta 端点在此 appkey 下全废——stat/delete/Entry 构造全面转 list

- **探针证据**（干净 curl，netdisk UA）：meta&path → error_code=31300 "stream type is not authorized"（无权限）；meta&fs_ids → 31023 param error（多编码变体同）；filemanager delete 的 fs_id 形态 → errno=12 不删除（只支持 path）；已传文件对 meta 轮询 10s 不可见（持续无权限非延迟）。PCFS 生产未暴露系其 entryCache 容错；spike 从未测 meta。
- **裁决**：① stat = list 父目录 + path 精确匹配；② close 的 Entry 构造 = list 父目录 + fs_id 匹配（list_lookup 提为主路径）；③ delete 句柄解析 = fs_id→path 缓存（list/stat/Entry 流量填充，容量 4096）+ 未命中递归 list 扫描兜底；④ meta_by_path/meta_by_fs_id 停用保留（allow(dead_code)，正式 appkey 复测候选）；⑤ mock meta 臂保留无消费者（注释注码）。stat 的「注入 -9 → NotFound」语义在 list 主路径下自动成立（conformance ⑤ / errno_mapping 不弱化）。
- **连带**：E2E/真机的 stat 可见性即时（list 实证）；K5（handle=fs_id 跨 rename 稳定）不变——缓存只是解析层，path 变更后缓存陈旧条目由递归扫描兜底纠偏（delete 失败时缓存失效重扫一次，注码）。
- **待负责人**：正式 appkey（个人开发者）到位后复测 meta 权限（31300 是否消失）——若可用可回切 meta 直查（省 list 流量）；此裁决不影响正确性只影响效率。

## 2026-09-08 Batch B2 真网实证（第五轮）：目录 create 冲突 = errno=0 + 空副本重命名——mkdir/ensure_parents 全面转 list 预检

- **探针证据**（干净 curl，netdisk UA）：create isdir=1 对已存在目录返回 errno=0（成功假象），远端保留原目录并生成 `<名>_20260908_212145` 时间戳后缀**空副本**——非 -8。驱动「-8 → Exists」容错永不触发，ensure_parents 每次撞已存在层即产空目录垃圾；conformance ④ 真网必挂。
- **裁决**：mkdir 与 ensure_parents 全面转 **list 预检**（先 list 父目录：已存在→Exists/跳过，不存在才 create）；-8 分支保留为防御语义。mock create 冲突建模对齐真实（errno=0 + 副本），使「未预检」实现可被离线测试检出。预检 list 流量顺带喂句柄缓存。
- **残留形态记录**：本轮实证期间产生的远端垃圾（cloudfs-b2 下 10 个测试目录 + /apps 两个 suffixed 副本）已全数清理（filemanager delete 复查 n=0）；/apps/cloudfs-b2 保留为 B2 测试根空壳。

## 2026-09-08 Batch B2 收口：ck-baidu 上传/下载全链交付（五轮真网实证链）

- **TDD 序列**：红 a21114a（16 红：上传表单字节级/差集/dlink 缓存/conformance 接线）→ 绿 3696fe6 → 真网返工 31363（c6c22de + ef821e7）/meta 全废（ddbade4）/目录 create 预检（690834a）。终态 ck-baidu 42 passed（conformance ①–⑧ 全绿含 ⑦差集可观测）+ 真机 3/3。
- **五项真网实证**（mock 无法预见、PCFS 未覆盖，各自有 decisions 条目）：① uinfo 用户键=uk 非 uid；② precreate 会话锁定全量 block_list（create 不一致重申 → 31363）→ 上传策略「到齐即传」；③ meta 端点在此 appkey 下全废（31300/31023）→ stat/Entry/delete 全面转 list + fs_id 句柄缓存 + 递归扫描；④ 目录 create 冲突 = errno=0 + 空副本重命名（非 -8）→ mkdir/ensure_parents list 预检；⑤ create 后 meta 索引秒级传播延迟（list 即时）。**MSYS 教训**：Git Bash 探针 path 形参被改写致首批探针全废（-7 假象），干净探针（MSYS_NO_PATHCONV）推翻两个错误中间结论——AGENTS 陷阱清单候选。
- **K 落地**：K7（会话表 (path,size) 定位 + 探活三路 + abort 保留会话）、K8（dlink TTL 60min 默认 + 两段 fallback：追 token→重取 dlink）、K9（4MiB 有界 Range + netdisk UA；下载顺序实现，4 并发预取留 B3b）、K10（rtype=3 真机复核通过——同路径重传覆盖生效无 _2026 副本）、K16（mock axum 建模五轮迭代对齐真实）。
- **能力位终态**：range_read/resume/multipart/server_side_move/rapid_upload/authoritative_index=true（逐位注码 + 九位静态锁）；conformance ⑦ 场景随「到齐即传」中立化（套件缺陷修复，ef821e7）。
- **吞吐记录**：100MB 真机往返 up 6.5-6.7 MB/s / down 2.8-5.1 MB/s（顺序分片实现 + 网络波动；spike qps 复跑零拒绝排除限额形态）；串行上传是 conformance ⑦ 确定性契约（write 同步落定）的代价，整文件 4 并发路径在 B3b transport_face 实现。
- **远端卫生**：全轮测试/探针遗留已清（cloudfs-b2 零条目复查）；/apps/privatefs 全程未触碰；cloudfs-b2 保留为测试根空壳。
- **待负责人**：① 正式 appkey 到位后复测 meta 权限（31300 消失则可回切 meta 直查省流量）；② K10 复核项销账建议（rtype=3 真机通过）。
- **验证**：workspace 671 passed 0 failed（9 ignored 含真机 3+既有 6）；clippy/fmt/check_layers（11 manifests）/scan_secrets 全过。

## 2026-09-09 Batch B3a+B3b 收口：组合根三后端接线完成（K1–K4/K11/K12/K17/K18 落地）

- **B3a（933f45e 红 → 8f96b07 绿）**：句柄 i32→i64（DB↔transport 同宽直通，narrow_msg_id 删除；ck-telegram 边界 i64::from 上行/try_from 显式收窄下行）+ RemoteHandle.path（hydrate 填 Some，telegram/mock None 零变化）+ delete_remote(&RemoteHandle)。断言漂移审计 = 非机械改动 0（38 处机械面逐项留档）；伴生语义 = mock delete 多 chunk all-or-nothing（单 chunk 与旧语义等价）。
- **B3b 段一**：Capabilities 第 10 位 remote_delete（local/baidu=true，telegram/mock=false）；双驱动 CloudTransport 面（ck-local 路径寻址 K2/K6、ck-baidu 整文件 4 并发 worker + first_msg_id=fs_id K5 + upload_stream 全缓冲注码 31363 实证）。
- **B3b 段二a**：K17（backend 枚举缺省 telegram 字节兼容 + 7 键三处同步 + env>file + validate 后端门控文案）；K12（namespace_key_for：telegram 臂黄金向量逐字节钉死护栏、baidu:baidu:uid、local:DefaultHasher 16hex 非安全注码——sync 隔离标识非安全边界且 local 永不启动 sync；is_sync_supported 接线 doctor+任务启动门）；K11（rebuild_from_backend 走 StorageDriver list 面 → upsert is_uploaded=1/chunk_count=1/msg_id 句柄 i64；明文-only 门 + telegram 影子索引拒绝指引 sync；`cydrive rebuild` 子命令）。
- **B3b 段二b**：K4 删除接线（remote_delete 位门控三面 vfs/webdav/web：先删远端（幂等 NotFound 容错+重试一次）成功后删行+缓存，拒绝保行；telegram/mock false 行为零变化——既有测试原样全绿自证）；dispatch（build_driver 统一收编：telegram GrammersTransport 路径零改动 / baidu factory+TokenStore 桥 CredentialStore（K13 on-arrival）/ local factory；能力横幅九+1 位）；setup baidu 分支（粘贴→refresh_tokens 刷新验证→**新 token 齐备才落 backend 键**（半配置实例防线，段二a 裁决②维持严格 validate）/ local 分支）；doctor（baidu 三态 Alive/NeedsReauth→Fail+setup 指引/Unreachable→Warn+直连提示、local root 检查、K12/K18 尾巴）；K18（baidu/local 恒直连，proxy_url 无效 → 装配日志+doctor 声明）。
- **WSL 双平台（B3b 单元 6）**：暴露两处 Linux-only lint——linux.rs 未用导入（**继承债**，e6581f9 改名批起，git diff main...HEAD 空自证）与 doctor.rs cfg(windows) 块 mut（cfg_attr 吸收）。WSL 终态 739 passed / 0 failed（win 738+1 平台 cfg 既有差异）+ 双平台 clippy/fmt/check_layers/scan_secrets 全绿。
- **执行注记**：B3b 段二b 期间子代理额度两次到限切断，进行中实现由主会话接手收尾（clippy 机械修 + doctor 文案补全），实现主体与 TDD 红绿证据链完整。
- **验证**：workspace win 738 passed / 0 failed / 9 ignored；wsl 739 passed / 0 failed；全门禁绿。

## 2026-09-09 Batch E2E 收口：baidu/local 硬验收通过 + telegram 腿延后

- **拓扑**：三实例（baidu Z: 8391 / local V: 8393 / baidu 第二实例 8392）+ sync-server 8390，全 OS 临时目录；凭据 env 注入零落盘；/apps/cloudfs-e2e 测试根与 privatefs 隔离（前后列举比对在案）。
- **全部通过腿**：上传/下载往返（字节等）/Range 半开窗口（字节等）/杀进程续传（200MB 中途 kill：会话 50/50 片落盘、重启 0 重传 + create 收尾、会话作废）/删除→远端消失（K4 真机）/双盘并存/sync 收敛（applied 4 + 墓碑 1；b1↔b2 全字段一致含 msg_id=fs_id——K5 跨实例一致真机实证）/rebuild 等价（D10 ②）。
- **E2E 检出力**：当场抓出两处装配缺口并修复——① K7 sessions_dir 生产装配漏接（`..Default::default()` 吞掉）；② K12 一次性 `cydrive sync` 没接 baidu 命名空间（周期任务接了一次性命令漏）。均为「测试绿但装配没接」形态——E2E 硬验收的价值实证。
- **观察项（挂收口/后续）**：① rebuild 与 sync 复制的 chunk_count 簿记差（1 vs 0——upload 队列 persist 对 baidu 单块写 0，rebuild 契约写 1；不影响 hydrate）；② sessions_dir 装配传 `./baidu_state` 与驱动内 `baidu_state/sessions` 拼接形成嵌套路径（功能正确、路径冗余）。
- **披露**：清理时顺带删除 X:/Y: 两条指向 WebDAV 的历史 net use 死记录（生产实例停机态、记录非数据、run 自动重挂；数据零触碰）。
- **telegram 腿**：独立测试 chat 未提供 → 延后（tracker 待负责人 #1；非本批失败）。

## 2026-09-09 Phase 2 收口（0.9.0）：K1–K18 落地索引 + 观察项销账

- **版本 0.9.0**（workspace 单点）：CloudTransport 破坏性演进（K1/K3）+ 新驱动双 crate（ck-local/ck-baidu）+ Capabilities 第 10 位（K4）；bin 名 cydrive/cydrive-sync-server 不变。
- **K1–K18 落地索引**（详情见各批次条目）：K1/K2/K3 → B3a（8f96b07）；K4 → 位 0b65311 + 接线 b46f87c（E2E 真机验证删除→远端消失）；K5/K6 → 卷形态（fs_id/路径句柄，E2E 跨实例 fs_id 一致实证）；K7 → 会话表（B2 c6c22de + 装配接线 dcdf8ca + 嵌套修 a92b628；E2E 杀进程 0 重传实证）；K8/K9 → dlink 缓存/下载器（3696fe6）；K10 → rtype=3（真机复核通过，E2E 覆盖无副本）；K11 → rebuild（d10d285 + chunks 对齐 a92b628；E2E D10 ② 等价）；K12 → namespace（58b8cbc + 一次性命令补 41fbb85；E2E sync 收敛）；K13 → oauth on-arrival（41f25e3 + TokenStore 桥 9ffa672）；K14 → 四键 env 链（6b74a19）；K15 → 重试钩子（41f25e3）；K16 → mock axum（9a969ac 起五轮迭代对齐真实）；K17 → 三处同步（6b74a19）；K18 → 直连声明（9ffa672，E2E 全程直连形态）。
- **E2E 观察项销账（a92b628）**：① sessions_dir 嵌套（装配改传实例 cwd "."，驱动契约不动）② chunk_count 簿记差（receipt 单容器 chunk_msg_ids=[fs_id]/[0] + rebuild 补 chunks 行——upload persist 与 rebuild 完全等价）。
- **门禁终态**：win 738 passed / 0 failed / 9 ignored（真机 baidu 3 + 既有 6）；wsl 739 passed / 0 failed（+1 平台 cfg 既有差异）；clippy/fmt/check_layers（11 manifests）/scan_secrets 全绿；release 全 workspace 构建通过。
- **遗留清单**：① telegram E2E 腿（独立测试 chat 待负责人）；② `#[ignore]` 真机套件随真机窗口复跑（baidu 3 已本轮跑过、既有 6 未跑）；③ litmus 套件（挂账）；④ 正式 appkey 到位后复测 meta 权限（31300）与 spike §2 限额；⑤ 下载 4 并发预取优化（当前顺序实现 ~5MB/s，4 并发 transport 面已在）；⑥ K10 复核项建议销账（rtype=3 真机通过）。
- **不部署生产位**（部署裁决留负责人，0.8.0 先例）。

## 2026-09-09 负责人澄清两项：百度 appkey 归属 + telegram 配置位置

- **百度 appkey 即本人正式凭据**（负责人明示）：PCFS client.go:69-70 的 clientID/clientSecret 是负责人本人的（spike 报告「借用的第三方 appkey」表述有误）。连带修正：① spike §2 QPS/限额结论的适用范围 = 负责人自己的 appkey 桶，「正式 appkey 到位后复测」的挂账语义收窄——**没有新 key 要等**；② meta 端点 31300（stream type is not authorized）若需恢复直查路径，行动项 = 负责人在百度网盘开放平台控制台为本 appkey 申请/开启「文件元信息（meta）」接口权限（当前 appkey 已验证具备 upload/download/list/quota/uinfo 权限，独缺 meta 族），**非换 key**；meta 停用代码（allow(dead_code)）保留即为该复测预留。③ ck-baidu R3 注释中「appkey 无代码默认值、PCFS 硬编码是反面教材」的表述维持——那是对「硬编码位置」的批评（凭据不入源码仓库），不涉及 key 归属正当性。
- **telegram 实测配置实际位置 = D:\Tools\rs-CyDrive**（生产部署实例：config.toml 含 bot_token/chat_id/api_id/api_hash + bot session + 生产 db/cache；E:\GitHub\rs-CyDrive\test\ 不存在）。**§7a 隔离裁决不因此改变**：该 bot/chat 即生产命名空间，跑 E2E 写操作须负责人二选一——明示接受生产 chat 污染（上传的 /_e2e_* 文件会进生产索引、随墓碑清理），或提供独立测试 bot/chat。telegram E2E 腿（tracker #1）等此裁决。

## 2026-09-09 telegram E2E 隔离裁决：负责人明示接受生产 chat 污染（§7a 例外）

- **裁决**：负责人明示「接受生产 chat 污染」——telegram E2E 腿用 D:\Tools\rs-CyDrive 的生产 bot/chat 跑（tracker #1 的二选一已定）。测试文件一律 `/_e2e_*` 前缀命名（聊天 caption 可辨识）。
- **污染面与清理责任划分**：① 聊天消息（=telegram 云端文件）**留在生产 chat**，由负责人事后在客户端手动删除（删消息即删媒体；须选「同时删除双方」）——这是接受的污染本体；② 本地索引行由 E2E 实例收尾自动清理（rm → 行删 → 墓碑）；③ **生产 db 零污染**：E2E telegram 实例不配 sync_url（不连 sync-server）——其 db 永不上传，生产实例重启后不会看到任何 /_e2e_* 行，无需墓碑收敛。
- **运行窗口约束**：E2E 实例与生产实例共用 bot 账号，**不得同时运行**（updates 轮询竞争）；生产实例当前停机态，E2E 结束即停测试实例，生产重启自然恢复。

## 2026-09-09 继承缺陷现场捕获：MiniRedir 空 PUT 工件竞态（幽灵上传 + 缓存误删）

- **现场**：负责人官网核对 demo 时发现 baidu /apps/cloudfs-demo 缺 readme.txt——Z: 盘行显示已传、云端 API 直查只有 2 个文件、db 行 msg_id=None、日志 5× os error 2 后降级。telegram 腿同型（readme 行 msg_id=None，聊天里无消息）；local 腿侥幸存活。**行状态撒谎 + 缓存消失 + 无法自愈**的三重数据完整性缺陷。
- **根因**：MiniRedir 小文件链「空 PUT → LOCK → 完整 PUT」——空 PUT 的 0 字节任务走快速路径（persist_zero_byte 用行现值标记已上传 + delete_local_copy 删缓存 + 零远端动作）。若它在完整 PUT 更新行之后才被调度：幽灵上传 + 误删完整任务要读的缓存。E2E 多轮 cp 未炸纯属调度运气。**同型竞态第二个受害者面**——sha256 面已修（pre-hash，测试 21 的 field log），本次上传面（测试 24）。
- **修复（d3ca0bc）**：process_job 的 0 字节快速路径加 row.size 卫兵（行读取本就新鲜）——非 0 行上的 0 字节任务 = 过期工件，跳过不持久化不删缓存，完整任务接管；真 0 字节文件行为不变。测试 24：门控诚实传输（Notify 门把现场不幸顺序确定性化 + upload 内真实读 local_path 防 mock 慷慨掩盖），红 None≠Some(2) → 绿。
- **验证**：workspace 741 passed / 0 failed；clippy/fmt 全过；重建产物后三实例真机复验——readme.txt 重传后 baidu 远端 3 文件齐全（API 直查）、telegram readme msg_id=76 真消息落地。
- **说明**：缺陷继承自 rs-CyDrive 时代（Python parity 的空文件快速路径 + 成功删缓存语义组合），非 Phase 2 引入；修复属 L4 core 单文件卫兵，对 telegram 零行为变化（其快速路径语义保留）。

## 2026-09-09 Phase 2.5 执行期裁决入档：K19–K28 按计划落地（MV0–MV4）

- **内容**：Phase 2.5 多卷启用（计划 docs/plans/2026-09-08-phase2-5-multivolume.md §1）的 K19–K28 裁决按计划落地——K19 配置形态（每卷一文件 + volumes_dir + 键二分互斥）、K20 单 WebDAV 端口 `/vol/<name>` 前缀（真机探针过，K20B 回退案封存不启用）、K21 每卷主目录（db/cache/session 基准）、K22 失败可见降级、K23 无默认卷（卷作用 API 显式带参）、K24 仪表盘契约（/api/volumes + 16 键冻结）、K25 stop=停整进程（进程级聚合控制面）、K26 sync 逐卷、K27 逐卷盘符挂载（显式声明制）、K28 卷模式 CYDRIVE_* env 覆盖忽略、K29 命名统一。K29 与本条一并视为已执行。
- **执行期修订三项**（均接手期/冒烟实测驱动，可逆）：① drive_letter 冲突检测 presence 化——卷文件未显式写盘符不参与冲突（解析出的默认 "Y:" 是占位非挂载声明），挂载决策归装配；② auto_mount_drive 移入进程级键集（挂载门控读进程配置，原划分使多卷模式无法关闭自动挂载，2fb934d）；③ WebUiConfig.drive_letter 改 Option（未声明盘符卷在 /api/volumes 报 null 而非 Y: 幻影声明，d92b082）。
- **风险**：无（单卷模式字节兼容由每批「单卷回归零漂移」验收项钉死；MV5 真机三卷 E2E 为最终硬验收）。

## 2026-09-09 Phase 2.5 MV5：三卷真机 E2E 通过 + 失败边界澄清

- **E2E**：单进程三卷（local 加密 V: + telegram Y: + baidu Z:）硬验收全过——三盘符子路径挂载、MiniRedir 写读删全链（PROPPATCH 207）、卷隔离、每卷独立 db、local 卷 at-rest 密文（同进程明文卷混跑）、tg msg_id=77、baidu 云端往返、仪表盘 tabs/汇总/切换（浏览器实截）、无默认卷 400、stop 全停。报告 docs/reports/2026-09-08-phase2-5-e2e.md（脱敏）。
- **执行期修复**：baidu_root 误重定基（MV1 缺陷，E2E 首启暴露——后端命名空间路径被当 fs 路径拼卷主目录致 baidu 卷拒启动；修复=不参与 K21 重定基，红→绿留证）。
- **失败边界澄清**（K22 细化）：**dispatch 期配置错误**（validate 拒绝类，如坏 baidu_root）= 大声中止整进程（配置作者修复语义，明确错误信息）；**装配/运行期失败**（db 打不开等）= K22 可见降级不拖死兄弟卷。两级都不静默（PCFS 反训的红线是静默吞错，不是中止）。
- **凭据执行注记**：多卷模式凭据家=卷文件（K13 ConfigTokenStore 回写闭环）；keyring 回填不适用于卷（单卷专属链）；baidu2.json 静态对已过期，现行对经 spike 工具链缓存维护（onboarding §7 路线复用）。

## 2026-09-10 驱动编译开关落地：K30–K32 入档（FT1–FT4，计划 docs/plans/2026-09-09-driver-feature-gates.md）

- **裁决按计划落地**：K30 feature 落点=组合根 cloudkit-cli（`[features] telegram/baidu/local`，default 全开，驱动 crate 零改动）；K31 缺驱动=编译期裁剪+运行期可行动报错——`{TELEGRAM,BAIDU,LOCAL}_DRIVER_REQUIRED` 三常量（缺驱动声明 + rebuild 命令 + backend 改法三段式），push/pull 命令保留不隐藏、off 态 connect 早退报错，doctor 缺驱动跳过对应探活；K32 `--version` 增 `(drivers: ...)` 清单——`cloudkit_cli::compiled_drivers()` 编译期常量风格（cfg 三分支拼字面量，顺序固定 telegram, baidu, local，全关显示 `none`），clap `version` 属性经 `OnceLock` 拼 `&'static str`（clap 的 `string` feature 不为此单开——`From<String> for Str` 是 feature 门控的）。
- **六组合矩阵实证**：build+clippy（`-p cloudkit-cli --no-default-features [--features X] --all-targets -D warnings`）×6 全过；全量 test：default=810 passed（既有 809 断言零漂移 + K32 新测试）、none=801 passed / 0 failed（驱动门控测试 cfg 摘除，断言零改动）；`[patch.crates-io] grammers-session` 无消费者警告未出现（no-default 构建零 warning 实证，计划风险预判销账）。
- **FT1–FT3 实现取舍要点**：① 孪生 dispatch 函数——`connect_stack`/`connect_stack_with_deadline` 每个都成对（cfg on 臂真装配 + cfg off 臂 K31 早退，签名/文档对齐，调用面零分支）；② `BackendTransport` 变体 cfg 门控（`#[cfg(feature)] Baidu/Local`）+ `_ => match *self {}` 不可达兜底臂——feature 全关时枚举非空、match 仍穷尽，编译期保证无幽灵臂；③ setup 菜单按 cfg 驱动集合动态生成（缺驱动的选项不出现，而非出现后报错）。
- **FT4 收口**：CI 增 `features` job（clippy 四腿 none/telegram/baidu/local + workspace `--no-default-features` test 腿，ubuntu 单 OS——feature 选择与 OS 无关，default 腿归 `check` job）；现场捕获 FT3 遗留一处死导入（multivolume_ops.rs 裸 `load_volumes` 仅 local 门控测试消费，非 local 组合 clippy `--all-targets` 红）——同门 cfg 吸收修复，CI 矩阵腿首跑即立功。
- **验证（FT4 批实测）**：`cargo test --workspace --no-fail-fast` default 810/0；`-p cloudkit-cli --no-default-features` 134/0；clippy 四腿+workspace、fmt、check_layers（11 manifests）、scan_secrets 全绿；`--version` 三组合实跑——`cydrive 0.10.0 (drivers: telegram, baidu, local)` / `(drivers: local)` / `(drivers: none)`。

## 2026-09-10 流式读（Range 直通）落地：K33–K37 + 三轮研究入档

- **背景**：负责人反馈百度卷视频不可流播——所有读路径（WebDAV 盘符 / /api/download）均为「整文件水合后供本地文件」（Python parity 继承），764MiB 视频 = 首帧前全量下载 ~38s+，Z: 盘撞 MiniRedir 超时直接失败。
- **三轮研究**：① PCFS 机制=驱动句柄 Seek 重发 Range、HTTP 层 Seek+LimitReader+io.Copy、零落盘零预取 128KB 缓冲；其 baidu 每次 Seek 重走 302+新建 client 是反面教材（本仓 dlink 缓存+有界窗口更优，保留）。② 本仓接缝：open_range 三驱动就绪、dav-server 读契约（metadata 定长→seek→read_bytes 16KiB 循环）与 RangeFile 天然契合、R-5 注释即预留能力位门、sync-server 有 Body::from_stream 先例。③ 互联网验证：rclone 10MB Range 上限/JuiceFS readahead/alist 本地代理——4MiB 窗口+顺序预取与社区实践一致。
- **裁决**：K33 流式=能力位门控直通（range_read 且非加密行；v1 GCM 整文件 AEAD 永不直通；Hydrate 回退语义链等价，R-5 钉测试保绿）；K34 4MiB 按 pos 锚定窗口+整窗聚合+惰性 seek+同位 no-op，流式读不进磁盘缓存（is_cached/evict 零触碰，DlinkCache 兜底重复开窗）；K35 row.size（权威索引）即 Content-Length；K36 /api/download 流式 200/206 显式 CL+Accept-Ranges+三态 RangeBody，416 零远端调用；K37 非目标：bot /get、pull、块缓存/投机预取（挂账）。
- **真机验收（764MiB 真视频）**：API 流式首字节 <1ms、1MiB 全程 159ms、文件中部 seek（600MB 偏移）1MiB 163ms、字节与 WebDAV 路径一致；WebDAV 端口 206 326ms；mp4 ftyp 头合法。
- **客户端发现（非本仓缺陷）**：Windows WebClient 服务 FileSizeLimitInBytes 默认 50MB——Z: 盘上 >50MB 文件 open 即失败（0.4MB 开成功/68.8MB 失败实证）；PotPlayer 经 Z: 盘播大视频需机器级注册表调整（reg add ...\WebClient\Parameters /v FileSizeLimitInBytes /d 0xffffffff + 重启 WebClient 服务），rclone/alist 用户同样必做；URL 路径（PotPlayer 喂 URL/前端播放器）不受此限。
- **测试**：workspace 841 passed / 0 failed（810 基线 + 31 新增，断言零漂移）；R-5 回退钉测试原样绿。

## 2026-09-10 真机发现：net use 盘符上播放器触发 Windows 重定向器整文件缓存（客户端限制，服务端无解）

- **现场**：负责人 cydrive 重启后从 Z: 盘用 PotPlayer 打开 764MiB 视频——网络 20MB/s 持续拉取 10+s 后报「RPC服务器不可用」，V:/Y: 映射同时掉、WebClient 服务无崩溃记录（瞬态故障）。服务端 run.log 全程零错误。
- **判定实验（决定性证据）**：Z: 盘顺序读 68.8MB 文件的**4MB**——期间网络实际接收 **70.3MB**（整文件），读完后 4s 后台流量 0.1MB。即 Windows WebClient/MiniRedir 对播放器型打开模式（随机访问语义）选择**先整文件缓存到本地 TfsStore 再供读**；流式服务器只是按客户端请求的全量顺序读以线速供给（服务端流式机制工作正常——客户端要的是整个文件）。
- **结论与对策**：盘符路径（net use）+ 大视频 + 播放器 = 客户端整文件缓存，服务端无法改变客户端的打开模式（rclone 社区同样结论：net use 挂载不适合大媒体，WinFsp 挂载才行）。**推荐播放路径 = URL 直喂播放器**（`http://127.0.0.1:<webdav_port>/vol/<name>/<file>` 或 `http://.../api/download/<file>?volume=<name>`——两者都实测流式 206/<1ms 首字节）；盘符路径适合小中文件与 Explorer 操作。已在 README 流式节补记此限制。

## 2026-09-10 Phase 3 WinFsp 类本地盘挂载落地：K38–K46 入档（WF0–WF5）

- **K38 许可与分发=feature 门控隔离**：feature `winfsp` 默认关；默认依赖图零 winfsp（`cargo tree` 反断言入 CI）；deny.toml `[graph] exclude=["winfsp","winfsp-sys"]`；CI 增 windows-only winfsp 腿（build+clippy+test）。winfsp-rs 0.13.1 为 GPL-3.0 且无 FLOSS 例外——`--features winfsp` 产物当前仅私有分发（仓库私有，GPL 义务以公开分发为触发），公开化前再裁决（自写 FFI / 推上游补 FLOSS 例外）。
- **K39 绑定路线=winfsp-rs 0.13 native API + sync trait + tokio Handle::block_on 桥**：spike 双形态实证后定——AsyncFileSystemContext 只把 read/write/read_directory 三回调异步化且 open/getattr 仍同步、文档少；sync+block_on 覆盖全部回调且 spike 真机跑通（async_ticks=69 计数证据、无死锁）。构建期坑：MSVC-only（winfsp-sys 对 gnu 工具链 panic）、bindgen 需 libclang（本机 LIBCLANG_PATH 指 pip 包）、delayload 的 rustc-link-arg 须落在最终产物 crate 的 build.rs。
- **K40 加法集成+可见降级**：进程级键 `mount_backend = "webdav"|"winfsp"` 默认 webdav（既有行为字节不变）；探测链=注册表 InstallDir→DLL 文件存在→LoadLibraryW **绝对路径**预加载（裸名在 stock 安装必败）→winfsp_init()；未编译 feature/未装 WinFsp → error 日志+横幅+逐卷标注 `webdav (fallback: …)` 回退 net use，绝不拒启（真机双场景验证：未编译 feature 腿实证）；doctor 增 WinFsp 检测（Warn never Fail）。
- **K41 生命周期三件套（rclone 原则）**：Flush 零副作用（只动簿记不动数据）；Release/cleanup 唯一写提交点（take-once 结构性幂等）；句柄宽限期 5s（可注入；容量 64 条上限、最旧淘汰；惰性过期——每次触碰清扫，不 spawn 定时器）。
- **K42 读路径=K33 三重门+窗口模型+cache-first（WF0）**：`open_read` 前置 `is_cached`→命中返回 `Hydrate` 本地直供（webdav/api/winfsp 三面共享同一路由）；WindowReader 搬 RangeFile 模型（4MiB 锚定请求 offset、聚合有界窗口、短读合法、不越 EOF）；K34「cached still streams」测试条款随本裁决改判（仅该腿，冷行条款保留）。
- **K43 写路径=StagedFile 复刻**：`.{name}.tmp` 兄弟 staging、任意 offset 写、cleanup→put_staged 提交入既有上传队列；disposition 在 create_options 高 8 位（FSD 按其分派——FILE_OPEN 只进 open、OPEN_IF/OVERWRITE_IF miss 才 create、覆盖是独立 Overwrite 事务只替换不播种）；**MiniRedir 特化面（PROPPATCH 207/空 PUT→LOCK）零进入 WinFsp 路径**。
- **K44 Explorer 防打爆**：read_directory 一次带全 stat（db 行零网络）；GetVolumeInfo 装配期快照；`network_mode` 键未实现（挂账——DriveType=3 本地盘形态，缩略图扫描影响留真机观察，VolumePrefix 实现=SMB 仿真待后续裁决）。
- **K45 错误模型=单点映射表**：VfsError 13 变体+StorageError 10 变体穷举 match（无 `_` 兜底臂）；EIO 兜底带 `tracing::error!`；特化映射：QuotaExceeded→STATUS_DISK_FULL、UploadPending→STATUS_SHARING_VIOLATION（Explorer 重试语义）、Timeout→STATUS_IO_TIMEOUT、ParentMissing→STATUS_PATH_NOT_FOUND。**执行期发现并修复的 FSD 大小写形态**：rename 源名与 cleanup 删除名会以**大写**送达（FSD 大小写不敏感解析的归一形态）→ 适配层 resolve_row 精确命中+父目录扫描回退（歧义保持 miss）——`6b25ede` 修复，红→绿 4 测试+真机验证。
- **K46 缓存协同 v1=cache-first only**：区间账本块缓存（rclone Rs 模型，统一水合/直通双轨）挂 Phase 3.5——WinFsp 落地后按真实体验决定立项（避免一批做两个大系统）。
- **真机验收（2026-09-10，三卷单进程 V:local加密/Y:tg/Z:baidu，WinFsp 运行时已装）七项全过**：①文件操作矩阵全通（dir/读小文件/copy-in 写入/读回/delete——net use 腿 rename 双卷**功能失败**、winfsp 143-169ms 可用）；②764MB 视频播放探针（PowerShell FileStream 计时）：open = net use **55409ms** → winfsp 缓存热 11ms/冷流式 7ms，随机 seek×4（25%/50%/90%/10%）冷 280-333ms/跳、缓存热 ~2ms，探针总时 55.8s → 0.29s（热）/2.9s（冷）；③三卷 winfsp 原生挂载（Win32_LogicalDisk FileSystem="cydrive"、DriveType=3、net use 列表为空、横幅逐卷标注）；④写链 Z: 写 32B→上传排空（db is_uploaded=1、本地副本按队列语义清除）→**冷读回逐字匹配**（baidu 云端权威）、V: local root 直查逐字匹配；⑤`cydrive stop` 一次停三卷（进程 0、盘符全消、net use 清空）；⑥回退腿：默认 exe（未编译 feature）+ `mount_backend="winfsp"` → `tracing::error!`（含 rebuild 指引）+ 横幅声明 + 逐卷 `webdav (fallback: …)` 标注 + net use 实挂 V/Y/Z，不拒启；⑦对比结论：net use open 55.4s 根源 = WebClient redirector 整文件缓存税（代价前置在 open），winfsp 把它变成 per-window 按需。
- **执行期发现/已知形态**：① `6b25ede` FSD 大小写修复（详见 K45）；② 百度**新鲜上传**文件的冷读存在数分钟「CDN 302 窗口」（driver 单跳跟随 dlink 302，CDN 二跳 302 报 `cdn unexpected http 302`→EIO；webdav/winfsp 两面同受影响；等待数分钟自愈；属 ck-baidu 驱动既有行为，非 Phase 3 引入）——挂账观察非阻塞；③ pending 期守卫：上传未排空时 rename/delete 被拒（SHARING_VIOLATION，防孤立唯一副本，与 webdav 面同语义）——Explorer「复制完立即重命名」会撞上（数秒窗口），rclone 式延迟 rename 挂 Phase 3.5 候选。
- **门禁与计数**：默认 workspace 867 passed / 0 failed / 9 ignored（841 基线 + WF0 4 + WF4 22）；winfsp 腿 92 测试（`cargo test -p cloudkit-winfsp --features winfsp`，需 LIBCLANG_PATH，WF4 后 88 + 6b25ede 4）；cli feature 腿 157 passed / 0 failed / 2 ignored；clippy×3 / fmt / check_layers（12 manifests）/ scan_secrets 全绿；默认图 `cargo tree` 零 winfsp 断言实证。

## 2026-09-10 加密文件 Range 流式读（实时解密窗口）落地：K47 入档（Phase 3.5-a，计划 docs/plans/2026-09-10-enc-range-streaming.md）

- **K47 加密读窗口化 = aead_v2 行经 core `DecryptingTransport` 走既有窗口流式（透明层 = PCFS CryptoWrapper 同位形）**：`open_read` 门拆分——`aead_v2 + range_read + size>0` → `Stream` 且 `total_size=row.size`（明文坐标，K35；handle 本体保留 `u64::MAX` 密文哨兵），transport 包 `DecryptingTransport`（明文窗口 → 密文 chunk 跨度一次内访 → 逐 chunk AEAD 验签解密切片；34B header + PBKDF2 派生在首个 `open_range` 懒做，OnceLock 缓存每 handle 一次）；面（webdav RangeFile / winfsp WindowReader）零改动、不感知加密。
- **裁决依据**：负责人目标「PCFS 同款加密 Range 实时流式」；其条件分支「当前实现复杂则改 CTR」经核实**不成立**——`AeadV2Window`（open/chunk_size/n_chunks_for_plain/ciphertext_span/decrypt_chunk，E1 39b0313）原语已备且与真实编码器测试互验，接线有界（E2 38689b2）；CTR 需新增 scheme 且丢认证，AGENTS 明列「纯 CTR 无认证」为 PCFS 反面教材。
- **PCFS 对照**：PCFS = CTR counter 算术 seek + CDN Range + 实时 XOR，实时解密成立但**无认证**、首读 2 次串行 HTTP；本实现每 chunk AEAD 验签（严格强于 PCFS），首读 = 34B header + 明文 span 两次内访，PBKDF2 100k 每 handle 一次由 K41 宽限表 + LRU 摊薄（key 派生缓存挂账，计划 §3）。
- **门矩阵（读路径，E2 定型）**：aead_v2 + range + 密码 → Stream / total_size=明文；gcm / 未知 scheme / 无 range / 0B / cached（WF0）→ Hydrate 原样；密码门在 WF0 探针之前不变。**既有加密钉测试零改判**：vfs_open_read 2/3/11、webdav fs_adapter 6e、winfsp read ⑩⑪、smoke 3f 全部原样存活（种子行未写 scheme，列默认 "gcm"）。
- **E3 面 CI 证据（2026-09-10）**：webdav `encrypted_range_get_streams_without_full_hydrate`（Range 100-299 → 逐字节精确切片、open_range 形态 `[(0,34),(34,span)]`、零 open、全文逐字节）；winfsp `aead_v2_rows_stream_windows_across_crypto_chunks`（16B 面窗跨 1MiB 密文 chunk 边界逐字节 + 精确密文 span 向量）与 `aead_v2_grace_reopen_reuses_the_parked_state_header_pulled_once`（K41 宽限重开 header 只拉一次）。红态 = E3-PROBE（aead_v2 门短路回 Hydrate，临时探针已 revert、`rg E3-PROBE` 零残留）下两面三测试全红（`open_range_calls` 实际为空 vs 期望向量）——断言区分流式/水合实证。真机验收（加密卷 open 时长/冷 seek 每跳一窗/哈希比对）归主会话。

## 2026-09-11 审查修复批落地：K52 入档（RB1–RB4，计划 docs/plans/2026-09-11-review-fixes.md）

- **K52 审查修复批 = 终版报告 18 项坐实发现四批修毕 + 探针底稿 13 测试全部转正为回归套件（红→绿留证、断言零漂移）**。RB1（commit 77504da）：winfsp-C1 case-only rename 走合法改名路径（规范化后大小写不敏感判定含 from==to 原始相等形态；rename_path 新拼写 + 本地 fs::rename 翻转缓存拼写；覆盖分支加 dest.id != row.id 守卫——永不删被改名行）+ winfsp-H1 宽限表（GraceEntry 记泊入时行 size，take_live 不符即弃；delete/rename 两侧/cleanup 提交成功后 invalidate；**执行期延伸**：提交/删除成功同清本 handle 读状态——否则随后 close 把事前状态按刷新后 size 重泊，失效被架空）；webdav 面评估不需同修（row 查找字节精确，case-variant 目标不可能命中源行），补回归钉死。RB2（4c1bd96）：H4 提交失败保字节 + 日志/注释改如实语义（行 pending、下次启动 requeue）；M1 staging 兄弟名加随机段 `.{name}.{rand8}.tmp`（Drop 清实际随机名）；M2 `CacheManager::local_path` 映射层消毒（`%`→`%25` 先行 + 保留设备名干 22 名加 `~` 标记前缀（字面 `~` 段编码 `%7E` 防歧义）+ 尾点/尾空格 `%2E`/`%20` token 化；rel_from_disk 精确反解供驱逐匹配；vpath/db 零影响，旧副本 miss 重水合）；M3 vfs 写面段长上限 MAX_SEGMENT_UTF16=255（NameTooLong 可行动错误，校验先于 rename/copy）+ 枚举超长名 warn+跳过不 fail 整目录；H2 三处 expect→map_err。RB3（38bf7bb）：cli-H1 unmount 先探测实际映射（复用 current_mount_for，决策提纯函数 winfsp_unmount_step 三分支：降级映射实测盘符释放/无映射出 in-process note/webdav 走原路）；cli-H2 mount_cmd_winfsp 接 ensure_not_running 双启守卫；cli-M2 join 失败档补占用门控降级（`used_drive_letters()` 只读探测——空闲 fallback、被占声明性说明，防 `net use /delete` 夺占符者映射）；cli-M3 CI 加 `cargo test -p cloudkit-cli --features winfsp`；stream-H1 web 层 aead_v2 Range 流式 E2E（206+明文 Content-Length+密文坐标 mock 形态断言；变异验证 start+1 → 测试红 → 还原绿，证明捕获力——该层正是 2026-09-11 线上开区间 bug 所在，自此有回归防线）。RB4（9e6a04b）：cli-M1 doctor/mount WinFsp 探测对齐同一选择语义（L5 禁互依不可抽公共函数，两侧对齐实现；DLL 缺失 continue 探下一候选 InstallDir；mount 侧 260 固定 buffer 改 ERROR_MORE_DATA 动态分配）；stream-M1 三面短读统一「短读=错误」（web RangeBody 短窗 Err；webdav fill_window 校验+warn；enc_stream 原为基准）；stream-M3 decrypt_chunk debug_assert 升运行时 Err + ciphertext_span 补文档契约（升 Result 需动三面公共 API 且调用方恒界内，二选一取后者）；M5 注释修正最小达标；M6 缺父目录 open/get_security 臂改 PATH_NOT_FOUND；winfsp-L6 with_stream_window 加 64MiB 上限；cross-L7 deny.toml 注释明示 advisories/bans 豁免范围（K38 补偿三要素）；L1 卷标固定 "CyDrive"、L2 空 claims 文案按真实原因分流。
- **降级与推翻记录（对抗性复核终判）**：winfsp-H2 High→Medium（winfsp-rs 全回调 catch_panic，进程不死仅单操作失败）、cli-H2 High→Medium（WAL+busy_timeout 并发不损坏 + 守卫防的直接危害不存在）、winfsp-M6/stream-M3 Medium→Low（窄面/全部调用方恒界内）。推翻项不修（非 bug）：winfsp-L4（open 急取读状态）、winfsp-L5（fetch 循环恒有界）、cross-L5（winfsp-sys build script 对 gnu 即 panic，产物不可达）。**原审查 H3（rename 进他 handle staged 路径 → chimera 行）探针留证但终版报告未列入修复清单**——挂账观察，见跟踪表挂账节。
- **真机冒烟（2026-09-11，用户实例 baidu 单卷 Z: aead_v2 + 修复版 exe）三 case 全过**：①case 改名——混合大小写形态实走修复路径（db 行拼写随改名移动、内容逐字完好、上传态保留），全大写形态被 FSD/Win32 层短路为安全成功 no-op（回调不入、行零触碰、零损伤；对照普通改名正常入回调——两路径并存为本机实证），C1 的破坏/ACCESS_DENIED 形态真机不可达；②删除重建即开（H1）：8B 读→删→5s 宽限窗内重建 64B→立即重开读回完整新内容；③网页播放回归：764MB 加密视频 8485 /vol/ 与 8486 /api/download 双面 206+明文 Content-Range（总长=明文总量，K35），头 1KB（ftyp）与 400MB 中段三面（webdav/api/Z: winfsp）逐字节一致，越界终点按 RFC 9110 正确钳制。过程留证：中段 Range 首测返回整尾曾疑回归，裸 socket 复测 + 代码比对判定为探针手误（终点值 4,000,001,023 > EOF 801,209,235），服务器行为正确。
- **门禁与计数**：workspace 911/0/9（开工基线 891/0/9 + 新增 20：webdav case-rename 1、core vfs 段长 4、cache 消毒映射 4、cli unmount 决策 3、web E2E 1、web 短读 1、webdav 短读 1、crypto 运行时检查 1、platform 探测对齐 4）；winfsp 腿 114/0/1（基线 94 + verify_probe 13 转正 + writer 2 + create 面 1 + mount 探测 3 + clamp 1）；cli winfsp 腿（=新 CI 步）160 passed/2 ignored；clippy×3 / fmt / check_layers（12 manifests）/ scan_secrets 全绿；驱动裁剪构建零漂移；verify_probe.rs 门控不变（`cfg(all(windows, feature = "winfsp"))`），头注声明各探针翻正批次与 H3 现状。

## 2026-09-11 遗留 Low 项清尾批：K53 入档（cleanup/low-items，f37a198）

- **K53 清尾批裁决**（审查遗留 Low 信息项，零/低风险优先策略第一批）：① **VOLUME_SERIAL 按卷名派生**——`derive_volume_serial(name)`（手写 FNV-1a 32bit，零依赖；0 重映射为 1），同卷名跨重启稳定、并发挂载卷间互异；卷名取 `mount_with` 的 label 参数**截断前**完整名（多卷=卷名，单卷="CyDrive" 恒定，注释说明单卷每字母至多一挂载、唯一性只在并发卷间有意义）。② **宽限表见证升格 RowWitness{size, mtime}**——闭环 RB1 残余窄缝：同尺寸不同 mtime 的 5s 窗内重建不再复用陈旧读状态（verify_probe 新增 H1 第三探针红→绿；红刻意走流式腿——hydrate 腿 LocalReader 每次实读磁盘不可观测陈旧性，流式 reader 的预填充内存窗口才暴露）。两侧 mtime 同源（同一 db 列同一读取路径），精确相等即见证语义。③ **unix_to_filetime 整数域换算**——整秒与小数量化分离（frac round + 1e7 进位）、tick 加法链 saturating：消除 f64 域 2-tick 网格漂移（红证据 1_700_000_000.051 → 旧 …510002 ≠ 新 …510001）与 release 下 1e300 回绕。④ **INIT 注释重写**（mount.rs，如实描述 probe/缓存/失败语义）。⑤ **M2 反解歧义注释留证不修**：旧缓存字面 `~`+保留干段名与新编码标记同形，固有歧义无法在不引入第三种转义下消除；新写入经 `%7E` 免疫，旧副本 miss 重水合自愈。⑥ **LetterInUse 文案判定达标不改**（指明冲突+两条出路）。
- **delete_pending 半死代码维持现状**：子代理核验证伪「全死」前提——mark_delete 生产调用 ×2（set_delete 流）+ 测试读 ×4 钉契约，属「生产写不读、测试读」形态；激活需先设计 set_delete × cleanup-delete 竞争语义（行为变更，超出清尾批授权）。挂账记录，审查原文允许「或不处理」。
- **全历史秘密审查销账（待人工清单 #3 尾注）**：gitleaks 8.30.1 全历史 388 提交/4.47MB，8 命中逐一比对**全部为公开 Cynet Android api_hash 常量**（与 config.rs 默认值同源，含示例文件注释、spike example、旧 cydrive-core 默认值与测试断言、AGENTS 历史行）——**零真实凭据**，公开化前的历史审查项有据销账。
- **门禁与计数**：winfsp 腿 117/0/1（+3：derive_volume_serial 单测、H1 第三探针、metadata 换算测试）；workspace 911/0/9 不变（新测试全在 feature 腿）；clippy×2/fmt/check_layers/scan_secrets 全绿。

## 2026-09-11 负责人裁决四项（基线设计收口 + telegram E2E 定向）

- **§9-2 旧仓策略 → 裁定：rs-CyDrive 正式冻结**（仅 hotfix；新仓 rs-cloudfs 为唯一主线；upstream-cydrive 维持只读参照）。事实状态本已如此（旧仓末次提交 2026-09-07 即融合设计文档本身），本次升格为正式裁决。**生产部署位切换时机未裁**，作为独立待排期小批挂账（D:\Tools\rs-CyDrive 旧二进制 → 新仓 0.10.0+ 部署：migrate 迁移配置 + telegram 真机验收 + 切换；切换前生产持续缺 2026-09-10/11 的流式与挂载修复）。
- **§9-3 bot-telegram 层级 → 裁定：按建议确认**（L5 应用形态、v1 不拆 crate，维持 ck-telegram 驱动层现状；新仓无 bot 应用层代码，将来若立项 bot 功能再议拆分）。
- **§9-4 Phase 1 内 R/E 顺序 → 裁定：R→E 已自然落地，项销账**（K42 窗口读模型先行、K47 aead_v2 装饰器随后，与建议顺序一致且已真机验证——无需再做选择）。
- **telegram E2E 腿 → 裁定：选项 B 独立测试 bot/chat**（§7a 默认「不接受生产污染」维持有效；负责人将登录 Telegram 经 @BotFather 创建测试 bot，测试配置落 `E:\GitHub\rs-CyDrive\test\`（§7a 原定凭据路径，目录需新建）；bot 就位前 telegram E2E 批不开工。所有 E2E 写操作仍限定 `/_e2e/` 前缀 + 收尾清理）。

## 2026-09-11 telegram E2E 批落地：K54 入档（feat/tg-e2e，1c7c425）

- **K54 telegram 真网 E2E = 驱动级 2 + vfs 级 1，三测试全绿（真 bot @cydrive_test_bot / 真 chat / 真 SOCKS5 代理）**：tg1 登录→4.5MB 多部件上传→stat 全对→下载逐字一致→清理核空（24.1s）；tg2 range_read=true 实证（跨部件/骑边界窗口逐字节精确、尾窗 EOF 钳制）+ 覆盖写实测为 transport 层 append-only（同 rel_path 新消息集旧集按 id 仍可取——与 K4/remote_delete=false 的 VFS 覆盖语义分层一致，测试按观测行为钉死）+ 删除核空（15.3s）；tg_vfs 全栈 vfs+AEAD_V2+telegram 加密往返 + 跨加密块边界窗口解密逐字（10.1s）。捕获力验证：上传侧 1 字节变异 → 红于第 4660 字节精确命中 → 还原绿。
- **运行时裁决/经验**：① **稳定 session 是卫生**——fresh-session 反复重试会累积服务端 auth 限流（RateLimited{retry_after:1576s} 实录），session 落 temp 目录复用快路径；② 代理装配经 ConnectionParams::proxy_url（SOCKS5 7897），无代理本机不可达，实证必要；③ 无凭据环境 SKIP 非 fail（CI 友好）；④ workspace ignored 9→12（+真网 3），#[ignore] 真机清单重分类完毕——**telegram 腿无遗留**，baidu 真机×3/winssf Q:/platform×3/keyring/perf/sync https 各随其窗口。
- **验收矩阵状态**：telegram 驱动自此具备可重复的自动化真网验收（上传/回读/Range/覆盖/删除/加密全栈），与 baidu（负责人真机）、local（负责人真机）并列。生产切换小批（旧仓→新仓）的 telegram 真机验收前提就此备齐。

## 2026-09-12 Phase 3.6 运行态卷管理落地：K48–K51 入档（feat/runtime-volumes，计划 docs/plans/2026-09-10-runtime-volumes.md）

- **K48 触发源=控制通道显式命令**（`ADD/REMOVE/LIST` 行协议，`STOP`/`PING` 分支一字未动；accept loop 串行处理——ADD/REMOVE 永不交错）；不做文件监视/周期重扫（半写 toml/凭据轮换歧义面全避开，脚本可驱动）。
- **K49 REMOVE 只动运行态、不碰卷文件**；持久禁用=`enabled = false`（RV0 键，serde 缺省 true 为语义自然缺省；序列化仅 false 时落盘——进程级 setup 产物不携带卷作用键，K19 guard 不拒自身输出）。
- **K50 卸载安全序=排空上传（队列计数差 enqueued−succeeded−degraded，60s 预算）→ 盘符释放（winfsp=MountHandle unmount 自带 10s 消失轮询 / webdav=net use /delete，DriveRelease trait 可注入）→ 提交（faces 先、workers 后）**；任一步超时/失败→可行动 ERR、卷保持注册不动（绝不半卸）。失败的启动残条（无队列无挂载）按运行态垃圾可直接摘除。
- **K51 共享注册表**——执行期落点修正：条目类型是各 L5 crate 自有类型（DavHandler/VolumeUiEntry/VolumeRuntime），单一 core 级表=向上依赖违反 R1 → **三个 per-crate `RegistryHandle` 面：cli 主表=真源，webdav/web 面为投影，变更统一经 cli mutator（boot 与 ADD/REMOVE）一次三表**。dav 分发=per-request 读锁查表（锁不跨 await，handler 随条目生死、锁态寿命=注册期——spike 证实 dav-server 0.11 分发本就 per-request、DavHandler Clone 共享 Arc 内 FakeLs 锁态），端口不变无重绑。启动零漂移由既有 multivolume 测试断言零改动全存活钉住。
- **RV2 执行期两处计划接缝修正**：① `MultiVolumeHandle` 启动期三快照（stop_units/sync_tasks/mounted）合并为 `VolumeLiveTable` 活表随卷增删；stop 序列 net use 释放从聚合后置并入逐卷段（每卷 host 先于其 VFS 的序不变，multivolume_e2e 全存活钉住）。② 计划所称 `cydrive volumes status` 子命令不存在（`cydrive volumes` 为平面报告命令）——LIST 转发按「不新增子命令面」落 `cydrive status` 多卷输出（离线回退零改动）。
- **RV3 真机发现（占用语义修正）**：**winfsp 卸载不受用户态句柄阻挡**——独占句柄（FileShare.None）占用卷上文件时 REMOVE 仍成功（数据先排空故零丢失，陈旧句柄随 FSD 消亡）；「占用→超时→中止」路径真实存在于 webdav/net use 腿 + 注入探针单测钉住，winfsp 腿的语义=卸载总成功。安全属性「绝不半卸」两种腿下均成立。
- **RV3 真机执行期修复：logging::init 上移至 `run()` 发现配置之前**——发现期（load_volumes）的 info!（RV0 跳过声明等）此前撞上未装 subscriber 被静默吞掉，违反「不静默」要求；真机红（重启日志无声明行）→绿（声明行可见）。
- **RV3 真机矩阵六项全过**（examples/config 用户实例 4 卷 U/V/Y/Z + 临时卷 rv3tmp@Q:，baidu 明文卷独立远端子树 /apps/cloudfs-rv3tmp，测试文件经产品链路自清）：①运行态 ADD baidu 卷→`OK ... mounted Q: via winfsp`、盘符跨进程可见可读写、/vol 207、仪表盘行即时出现；②REMOVE 三面消失+排空断言（pending=0）+数据跨装卸往返逐字完好；③占用句柄（发现如上）；④enabled=false 重启跳过（Assembling 4、Q: 不占、声明行可见）；⑤坏凭据 ADD→ERR（errno=20017）兄弟卷零影响；⑥status 静态配置面/运行态 LIST 面分离清晰、doctor 20 ok/0 fail。
- **环境新陷阱：共享 CARGO_TARGET_DIR 跨 worktree 同名包产物污染**——主仓与 worktree 包名版本相同（0.10.0），共享 target 下指纹按 name+version 键合，交叉构建会把 A 树 rmeta 喂给 B 树编译（实例：主仓 cli 源码对 worktree 版 web/webdav 的 RegistryHandle 报 E0308）。纪律：worktree 构建用独立 target；共享 target 的主仓重建前先 `cargo clean`。

## 2026-09-12 Phase 3.6 合入后深度审查 + 修复批落地：K55 入档（fix/phase36-review）

- **K55 审查裁决**：四分域深度审查（cli 状态机/协议层/动态分发/config+测试质量）+ 主会话逐条核验。3 High 同根因——「命令串行化」的推理被外推到它不覆盖的并发世界（Ctrl+C 停机任务、axum 请求任务、被 take 出表的 entry）。两条子代理结论经亲验证伪：大小写 ADD「不可移除卷」不成立（config.rs stem 校验先于读文件，注册面名字恒一致）；「cydrive stop 客户端挂起」不成立（STOP 在连接任务内即时回执；实际挂起的是 status 的 LIST 转发——它走命令队列）。
- **H2（7a2050e）**：stale empty-PUT skip 补 `bump(&stats.degraded)` + `QueueStats::outstanding()` 单点收口——前置潜伏计数器缺口（2026-09-09 实录场景）被 RV2 排空判据首次激活为「卷永远卸不掉」；degraded 而非新计数器：语义同族（未上传即终态）、无 requeue 副作用、零既有断言破坏。
- **H1（c8489a6）**：停机交错三件套——① `take()` 单锁化（双锁竞态压力测试 7/7 复现红）；② 命令观测停机 gate（入口拒新：ADD/REMOVE 拒、LIST 照答；REMOVE 排水循环+提交点观测：放回 entry+shutdown ERR；ADD 注册前观测：拆装配件）核心不变量=「gate 后命令除放回外不再变更 live 表」；③ stop task `InFlightCommands` idle 屏障（RAII + Notify double-check）——`take_all` 必然看到全部 entry。执行期发现：控制通道 STOP 无法复现该竞态（accept loop 命令臂阻塞 stop_rx 分支）——测试经新 `request_stop()` 面（Ctrl+C 真实拓扑）触发。
- **H3（86e49a5）**：ADD 改「先装配后公示」——三面 insert 后移到挂载成功后（红证：挂载阻塞期 PROPFIND 207→绿 404）；挂载失败走 `tear_down_unpublished`（rollback_add 删除吸收）；winfsp 挂载 pass 喂单条目 shadow registry（registry 在 pass 内唯一用途=按 claim 解析 VFS，主会话核证）；新增可注入挂载缝 `RuntimeVolumeCommands.mount`（一并补上审查 M5 最重的 rollback 零覆盖缺口）。
- **M1（68175a8）**：控制通道鲁棒性三件——handler panic 包 `AssertUnwindSafe(...).catch_unwind()`（futures-util 单依赖边）回 internal-error ERR 通道存活；客户端交换 120s 预算（`EXCHANGE_BUDGET`，REMOVE 60s 排空+10s unmount+余量）+ `exchange_line_bounded` 测试缝；`WebDavRelease` net use 改 spawn_blocking + 30s 预算（超时分支离线不可测，commit 内如实声明，错误路径双测试 pin）。
- **挂账（未批准修复）**：M2 排空期入站写入不断流（ERR 文案不可达）、M3 解析错误 ERR 凭据值片段风险、M4 全禁用 boot 文案、M5 余下测试缺口（并发语义/keep-alive 跨摘卷/空表）与 Low 全集——权威清单见 docs/tracking/phase36-review-fixes.md。

## 2026-09-12 审查修复批二（Medium 清偿）：K56 入档（fix/phase36-med，M2/M3/M4/M5 + M3 补全）

- **K56 范围裁决**：负责人确认 Medium 亦为真 bug，批准清偿剩余 M2–M5（M1 已随 K55 批修掉）；Low 项继续挂账。
- **M2（9eb8233）**：REMOVE 排水循环新增「写入仍在到达」检测（相邻 poll 间 enqueued 增长即中止，gate 检查优先于增长检测）——活跃写入的卷及早回专属可行动文案（关闭占用程序后重试）而非等满 60s 预算给不可达成的建议；纯超时分支文案补 close-writers-first 子句。红→绿：排水期二次 ingest，等满 5.36s 通用超时 → 1 poll 内专属 ERR。
- **M3（093cd4d + 补全 b78ece1）**：解析错误凭据脱敏收口在 config.rs 构造点（非回复面）——`redact_credential_values`：消息含凭据类键名才脱敏（赋值行值段/未闭合多行串/serde 反引号值段，保前后 2 字符），普通语法错误原文保留；`load_volume_config`（卷文件）与 `load_toml_with_keys`（单卷 config.toml，同根因补全）全部 Parse 构造点覆盖。红→绿 ×3 + 对照 ×2。
- **M4（5d68c1b）**：全禁用 boot 专门文案——装配入口对空卷表重扫 `discover_volumes` 计数，报「N 个卷文件全部 disabled + 目录 + 改回 true/走单卷模式两条出路」；`bind_multi_webdav` 过时注释修正。红→绿 e2e。
- **M5（685e554）**：三个特征测试钉住已声明行为（均直接绿，如实标注 pin）——①慢 REMOVE 在途时控制通道整体让位（PING/LIST 回复都在 REMOVE 完成后），②摘到空表实例继续服务（LIST 0 卷/端口在/PING 活），③**keep-alive 同连接跨摘卷**（207→remove→同连接 404，裸 TCP keep-alive helper，RV1 per-request 分发声明的直接钉子）；webdav 测试 helper 增强 chunked 解码（multistatus 无 Content-Length，纯测试侧）。
- **审查结论修正（K55 条目的证伪②撤回）**：K55 批曾裁定「cydrive stop 客户端挂起不成立——STOP 在连接任务内即时回执」。M5 特征测试实证：**连接任务的 accept 本身在 handler await 期间不被 poll**——PING/STOP 的回执同样要等在途命令完成（TCP 握手仅在内核 backlog 成功）。K55 M1 的 120s 客户端预算恰好兜住该挂起面；若未来要求 PING/STOP 真即时（探活不被长命令阻塞），需把 PING/STOP 挪出 accept loop，属行为变更留裁决（tracking 挂账）。
- **批次执行事故如实记录**：M3/M4 首 push 前因 fmt 门禁差两行 soft reset 重 commit（dba923f/83223a0 → 093cd4d/5d68c1b，内容不变）；M5 测试期发现并如实区分一处探针问题（read_response 不认 chunked）与产品行为。

## 2026-09-13 Web 卷管理面 + 运行时 rebuild 落地：K57 入档（feat/web-volume-mgmt，计划 docs/plans/2026-09-13-web-volume-management.md）

- **K48 修订（触发源扩展）**：卷管理触发源 = 控制通道命令（原）**+ web 管理面**——web 经进程内回调缝（`VolumeCommandClient`，与 `VolumeCommandHandler` 逐 token 同型）把命令汇入**同一 mpsc 串行队列**（K48 串行化语义/K50/H1-H3/M1 全套设施免费继承）；协议扩 `SHOW/CONFIGS/ENABLE/DISABLE/REBUILD/CREATE/UPDATE/DESTROY` 八命令（REBUILD 受理后转后台不占队列）。
- **K49 修订（分档解禁）**：REMOVE 运行态语义不变；CREATE/UPDATE 允许程序写卷 toml——core `render_volume_toml`（显式键保序受控渲染，**不用 save_toml**——铺 29 默认键不可用；toml `preserve_order` 特性开启，已核全仓无键序依赖）；**凭据 write-only**（SHOW 只回 `{"set":bool}`，UPDATE 载荷缺失/空=保留、非空=覆盖，存储凭据值不经手任何日志/回复——M3 纪律延伸到命令载荷）；**DESTROY 永不触碰远端数据**，本地数据目录默认保留、`purge_local` 第三词显式才删；toml 重写丢注释在回复/UI 明示（裁决①：不引 toml_edit）。
- **K11 增补（rebuild 运行时化）**：运行时 rebuild 允许（WAL 多连接并发设计内；可见性=消费面每请求直查 db 即时生效）；`REBUILD <名>` 后台命令带 **R1-R6 强制约束**（单飞互斥/排空门槛=H2 `outstanding()` 与 REMOVE 同源/后台化不占队列/15min 有界幂等中止/R5 检查点随 REMOVE 与停机联动/加密卷 K11 拒+telegram 拒）；`cydrive rebuild` 多卷检测活实例转发；**no-pruning 边界重申**（周期化与死行清理另行裁决）。运行时并发安全前提 2026-09-12 会话核实（WAL/busy_timeout=5000、requeue 只挑 is_uploaded=0、窄竞态被 R2 消除）。
- **安全增界（管理写路由）**：`same_origin_guard`（Origin/Referer 同源校验，非同源 403）挂 /api/volumes 写族；**非 loopback 绑定自动降级只读**（`web_ui_host` 非 127.0.0.1/::1 → 写族 403 + 启动 warning），远程管理需显式新进程键 `allow_remote_admin = true`（K19 三清单同步）。
- **双页布局（负责人裁决：使用态与配置态独立页面）**：`/volumes` 配置页（同 CSS token 体系：实例概览四卡 + 卷表 Name|Backend|Status|Drive|Pending|Size|Actions + 展开式表单卡 + 两步删除 modal）与 `/` 使用态整页跳转互链；单卷模式 `/volumes` 给说明页；顺路修复使用态选中卷被 REMOVE 后 current 悬空 UI 冻结缺陷。
- **批次与验证**：P0-P6 七批全绿（6972651/72bdfe7/4e07ff2+bf338c2/00c522b/63daf3b/782d63d），workspace 955→1039（+84 测试），winfsp 腿 117/0/1 持平，clippy×3/fmt/裁剪构建/check_layers/scan_secrets 全绿；前端以 jsdom DOM harness 钉住（P0 22 项、P1 16、P5 31 等），真浏览器视觉面待真机窗口复核（挂账）。**执行事故如实记录**：P3/P4 派发两次撞账号 5 小时限额——一次实为异步完成（commit 落盘但报告未达，主会话按未审查处理：亲跑门禁 1026/0/12 + 安全点抽查后放行）；一次中断留半成品（重置丢弃重派）。
- **挂账**：周期 rebuild、pruning 策略、rebuild_timeout 可配键、管理面鉴权（Origin 之上）、真浏览器视觉复核、写路由超时分支真机验证。

## 2026-09-14 K58 审查修复批落地：5 High + 8 Medium 清偿（fix/k58-review）

- **审查范围**：6fbd90a..7e1b2be（K55/K56 修复批 + K57 卷管理面 + UI 批 Rust 触点）四分域深度审查 + 主会话亲验；子代理矛盾裁决一处（卷管理 POST 实为 axum 默认 2MB 上限——1900MiB DoS 报告项剔除）。
- **H1+H2（命令执行模型重构，FB/85ff4be）**：`volume_command_handler` 闭包改 **spawn 执行任务 + oneshot 回复 + 单 permit Mutex**——web 超时/断连只弃等待（hyper 实测会 drop 在途 future 的斩杀面消除，红实证=abort 后卷楔死）；全入口（control/web×web）恢复 K48 串行（红实证=并发双 REMOVE 撞结构 ERR）；死锁论证（扁平星形等待图/等待侧零锁/停机 idle 屏障等 guard 有界）入代码注释；九处「serialized」假注释重写。
- **H3（FA/b5a1120）**：core `write_config_atomically`（tmp+sync+rename）收敛五写盘点（write_volume_enabled/save_toml×2/CREATE/UPDATE）——撕裂窗口（全实例拒启/行边界静默丢键翻转加密语义）闭合。
- **H4（随 FB）**：panic 防护移入执行任务，tracing 只记 payload 摘要——control.rs 原 `command=%line` 日志（CREATE/UPDATE 凭据明文落盘点）移除。
- **H5（FC/1b7f8ae）**：same_origin_guard 加 **Host 白名单**（绑定后 SocketAddr 推导，wildcard 收窄为回环拼写——LAN 主机名管理须改绑具体地址，已入 doc）；rebinding 形态红实证（destroy 409 漏过/upload 200 直达）→ 双 403；/api/upload 存量 CSRF 一并收口。
- **Medium 清偿**：M1 worker panic catch_unwind+degraded 终态（幽灵 outstanding 消除，FE/c19db0a）；M2 proxy_url/sync_url（+增补 baidu_app_key）入 SECRET_VALUED_KEYS；M3 Edit 门撤除（disabled/REMOVE 后修复回路恢复）；M4a rebuild 检查点身份判据（Vfs 堆地址+钉活防 ABA）；M4b 行走中 outstanding 复查（可恢复中断）；M5 UPDATE 重读经脱敏漏斗；M6 前端 action-in-flight 锁；M7 add_volume 结构化 (String,bool) 成败。
- **验证**：FA-FE 五批串行、逐批 diff 审查；逐项复核表（13 项×钉住测试）入 docs/tracking/k58-review-fixes.md；workspace 1045→1067（+22 测试）、winfsp 腿 117/0/1 持平、clippy×3/fmt/裁剪构建/check_layers/scan_secrets 全绿。
- **执行事故如实记录**：两次账号限速中断（FA/FD 首派）——FD 半成品重置重派；FA 顺带裁决 baidu_app_key 入脱敏清单（前端既有 write-only 渲染与后端 SHOW 不一致，主会话一行增补 968d492）。

## 2026-09-14 K59：ssh/sftp 驱动立项（方案与选型，计划 docs/plans/2026-09-14-sftp-driver.md）

- **K59.1 立项口径（负责人 2026-09-14 明示）**：本项目**自用、不对外销售/分发**——外部项目借鉴不受许可证传染约束；但「能跑起来、坑最少」是唯一标准，且不因此降低本仓既有门禁纪律（TDD 红→绿、conformance、三步门禁、凭据红线均照旧）。
- **K59.2 选型裁决**：`russh` 0.63 + `russh-sftp` 3.0，**`ring` crypto 后端**（`default-features = false, features = ["ring", "rsa", "async-trait"]`）。依据：① 纯 Rust 无 C 依赖，`ring 0.17.14` 已在本仓 lock（经 rustls）→ 零新增高风险构建依赖（与 deny.toml「全 workspace 只链接一个 SQLite C 库」取向一致）；② 三个独立项目交叉印证（本仓探针、termscp 弃 libssh2 转 russh、aeroftp SFTP 主路径用 russh + **ring**）；③ 弃 ssh2/libssh2（C 依赖）、弃 openssh-sftp-client（连接层依赖外部 `ssh` 可执行文件）。
- **K59.3 版本下限 ≥ 0.63（安全，非版本洁癖）**：russh 0.63.1 修复 **GHSA-47hw-gvq5-r2gm**（High/CVSS 7.5，**客户端侧**——恶意服务器可对客户端从未打开的 channel ID 触发 channel 级回调，导致 panic/伪造退出码/状态失步；对"连接不可信服务器"的挂载卷是直接威胁）与 GHSA-w3jg-pjxf-73p4（ML-KEM768+X25519 缺 X25519 零点校验）。**0.62 线无 backport**——0.62.7（2026-08-17）早于 0.63.0（08-21），不可能含其后公告的修复，`cargo update` 会停在 0.62 并保持脆弱。**纪律**：三条早于 0.62.5 的公告无 RustSec 编号，`cargo audit`/`cargo deny` 对它们**失明**——升级驱动不得只看门禁绿色（推理源自 aeroftp `Cargo.toml:401-437`，已交叉核实）。
- **K59.4 不复制 aeroftp 的 400 行 read-ahead（重要修正）**：aeroftp 锁定 russh-sftp **2.1**，为该版本 `File` **串行读**（每个 `poll_read` 等一个 `SSH_FXP_READ`，upstream #70）写了窗口管理 + 多连接池 + 句柄预算。**3.0.0 已修复**——`Config::max_concurrent_reads` 默认 **16**，`file.rs` 的 `ReadState::request` 为完整流水线实现（含短读缺口重试）。故本仓**不写 read-ahead、不写多连接池**；若 SF4 实网仍不足，下一步才考虑 aeroftp 验证过的文件内分段多连接（≥250MiB 启用，其 300MiB 实测 144.75s→25.46s），列为 SF5 可选。
- **K59.5 零侵入边界（架构前提）**：L2 以上**零改动**——经通读确认 6 条依据（`Vfs` 只持有 `Arc<dyn CloudTransport>`；写路径 `commit_put` 无后端分叉；加密在 core 层按 `VfsConfig` 工作、驱动零 crypto；读路径首查 db 行；`authoritative_index` 仅启动横幅消费；装配为单一后端无关 builder）。接入面穷举 **12 处，均为追加而非改动**。唯一需重构项：`compiled_drivers()` 现为 3 位 cfg 元组 8 臂穷举，第 4 驱动需改写为可扩展形态，**3 驱动组合输出须逐字不变**（既有测试为零漂移基线）。
- **K59.6 测试桩在 Windows 实测可行（决定性证据）**：`russh_sftp::server::Handler` **仅 `unimplemented()` 为必需方法**（其余 20 个全有默认实现）→ 无需 aeroftp 那种手写 SFTP v3 包循环。仓外探针 `E:\tmp\sftp-harness` 在 Windows 上端到端 PASS（connect→auth→subsystem→readdir→stat→**Range 读逐字节正确**→错误分支），**不依赖 Docker/testcontainers、不依赖 `$HOME`、不依赖外部 ssh**——正面绕开 aeroftp 同类桩只能跑 Unix 的坑（它靠重定向 `HOME` 隔离 known_hosts）。**桩侧语义要求**：`readdir` 必须第二轮返回 `StatusCode::Eof`，否则客户端 `read_dir` 永挂。
- **K59.7 host key 必须校验（拒绝 termcp 做法）**：termcp 全仓无 known_hosts、裸用 `NoCheckServerKey`；aeroftp 有完整 TOFU（`russh::keys::known_hosts` + 三态 + 1-based 行号 + 原子写）。**我们两者都不能照搬**：这是无人值守、长期自动重连的挂载卷，与"用户在终端主动连自己服务器"的交互式场景风险不同，也无 GUI 可弹窗 → 需**非交互 TOFU 形态**，在 SF0 拍板（推荐：显式接受 + 指纹落盘，未记录即拒连并给可行动错误）。
- **状态**：方案与跟踪单落库（`docs/plans/2026-09-14-sftp-driver.md` + `docs/tracking/phase4-sftp.md`），**待负责人批准后开工 SF1**；SF0 三项待拍板（D1 认证方式 / D2 host key 形态 / D3 是否起步即多连接）。
- **本批性质**：只读勘察 + 仓外探针（`E:\tmp\sftp-probe`、`E:\tmp\sftp-harness`），仓库 git status 全程干净，无源码改动。

## 2026-09-14 K60：SF0 三项拍板——Phase 4 方案获批，SF1 解锁

- **K60.1 D1 认证方式 = 密码 + 私钥**（负责人：「便于实现自动化」）。私钥含解锁 passphrase（`sftp_private_key_passphrase` 凭据键——不支持加密私钥等于半个私钥支持）；**keyboard-interactive 与 ssh-agent 不做**（v1 挂账：aeroftp 教训，个别服务器只收 k-i，遇到再议）。`sftp_password`/`sftp_private_key_passphrase` 入 `SECRET_VALUED_KEYS`；凭据链 env > config > keyring 照 R3。
- **K60.2 D2 host key = 显式接受 + 指纹落盘**（方案 §4.5 甲案）：未记录指纹 → 拒连 + 可行动错误（指明接受途径）；接受 = 显式用户行为后指纹持久化、后续静默校验；**指纹变更恒拒**（MITM 信号），救济 = 显式移除旧指纹重新接受。禁止无条件接受（termcp `NoCheckServerKey` 反例，2026-09-14 会话核实其全仓无 known_hosts）。
- **K60.3 D3 起步多连接 = 否 + 概念澄清**：负责人答复中「不同认证各起一个实例」指**多卷模式**（不同服务器/账号各一卷——Phase 2.5 既有能力，SFTP 作为普通卷自动继承，零额外工作）；D3 本意是**同一卷内为吞吐开 N 条并行 SSH 连接做文件内分段**（aeroftp `download_intra_file_pooled` 路线）。拍板：起步单连接（russh-sftp 3.0 `max_concurrent_reads: 16` 流水线读），SF4 实测吞吐不足再立项 SF5。
- **效果**：计划状态 待批准 → **已批准**；SF0 验收达成（选型 K59.2–K59.7 + 本条 D1/D2/D3）；SF1（驱动骨架 + 配置接入 + `compiled_drivers()` 可扩展化）解锁可开工。落档同步：计划 §8 改拍板记录、跟踪单 SF0 行 ✅、AGENTS 待人工清单第 6 项销账。


## 2026-09-14 K61：Phase 5（pan115）立项——方案落库待批，路线裁决门槛挂 115-0

- **K61.1 双仓库研究完成（2026-09-14，只读）**：① `E:\Go_codes\PrivateCloudFS`（Go，私有 web API 路线，`SheltonZhu/115driver v1.2.3`）；② `E:\GitHub\115-plus-desktop`（Rust+Tauri，MIT，v1.1.0-alpha 活跃至 2026-08-19，**官方开放平台路线**，`ali-oss-rs` + sha1 + reqwest）。
- **K61.2 对前轮评估的修正**：115 从「只有猫鼠一条腿」改判**两条腿**——官方开放平台（proapi.115.com，device-code/QR 认证，115-plus-desktop 实证 Rust 可行）+ 私有 web API（PCFS，sign.go 可直译保底）；Rust 生态从「零现成物」改判「`ali-oss-rs`（MIT，crates.io）可直接依赖 + 传输实战参照齐备」。
- **K61.3 路线裁决门槛（写死防悬空）**：115-0 spike 按三选一裁决——① 凭证可得 + downurl/Range/QPS 实测过 → 路线 A（开放平台）；② 凭证不可得 → 呈负责人风控取舍，批准则路线 B（web API）；③ 不可得且不接受 → 挂起销账。**依据**：2026-08-09「115 API 开放平台暂停服务」报道 + 115-plus-desktop README「没有开放平台的可加 QQ 群获取」暗示新申请通道收紧，而存量应用持续可用。
- **K61.4 选型**：主路线 = 开放平台（官方文档、无签名逆向、与 ck-baidu oauth.rs 同形态的 token 设施复用）；OSS 分片上传用 `ali-oss-rs` 不自研（**核对 reqwest 版本树防双大版本**）；秒传 SHA1+pre_sha1+preid（密文永不命中，位按原语声明）；加密零参与（core 层继承）。crate/feature/backend/配置前缀统一 **`pan115`**（crate 名不可数字开头）。
- **K61.5 架构口径**：全量公民（StorageDriver + conformance，照 ck-baidu 四件套结构）；能力位 range_read/multipart/server_side_move/rapid_upload/authoritative_index/remote_delete 计划为 true，**resume 待断言⑦验证后点亮**；conformance 桩 = 假开放平台 HTTP 服务端 + **假 OSS 端点**（get_token 返回的 OSS 端点指向 loopback——端点可控是关键设计）。装配 12 处与 SFTP 同清单；**前置依赖 Phase 4 SF1 的 `compiled_drivers()` 可扩展化**（顺序对调则所有权对调，只做一次）。
- **K61.6 硬纪律（两仓库实战坑）**：downurl UA 绑定（开放平台要浏览器形态 UA；web 路线要完整头回传；SheltonZhu #80 空 UA 场景）；CDN 429 分段级重试+respawn；OSS 续传双坑（PartAlreadyExist 循环 #29 / UploadId 重置 #30，修法 2026-08-19 最新）；上传后 size 校验；IPv4 dial。
- **状态**：方案与跟踪单落库（`docs/plans/2026-09-14-pan115-driver.md` + `docs/tracking/phase5-pan115.md`），**待负责人批准后开工 115-0**；D1-D4 待拍板（路线/删除语义/根目录/限速参数形态，计划 §8）。本批只读勘察，无源码改动。

## 2026-09-14 K62：Phase 5（pan115）拍板——方案获批，115-0 解锁

- **K62.1 D2 删除语义 = 进回收站，回收站接口不引入**：`/open/ufile/delete` 原生语义即进回收站，直接采用；回收站端点族（list/restore/清空）v1 不引入，误删恢复走 115 官方客户端/网页。`remote_delete = true` 照旧（远端状态真实变更，回收站形态是自用安全垫）；web 表单与 DESTROY 文案明示。
- **K62.2 D3 根目录 = 可设置、缺省网盘根**：`pan115_root` 接受 folder id、缺省 `"0"`；validate 不强制专用子目录（自用裁量），doctor 对根卷提示误删边界。
- **K62.3 D4 QPS = 实现期合理定值（负责人授权「参考其他项目，合理设置即可」）**：`RebuildTuning` 式结构注入（不加 config 键）；初始保守值——列目录簇 ~2 QPS 起步、downurl 结果缓存（TTL 115-0 实测定，参照 baidu dlink 60min 先例形态）；CDN 429 自适应分段重生（115-plus-desktop 先例）兜底；spike 校准后数字钉进跟踪单。
- **K62.4 D1 路线 A/B 保持 spike 门槛**：K61.3 三选一规则不变，负责人可随时直裁。
- **效果**：Phase 5 方案 待批准 → **已批准**；**115-0（路线 spike 与裁决）解锁可开工**。落档：计划 §8 改拍板记录 + 头部状态、跟踪单状态行 + 批次日志、AGENTS 项目信息行 + 当前阶段段。

## 2026-09-14 K63：Phase 6（pan123）立项——方案落库待批，路线裁决挂 123-0

- **K63.1 参照研究完成（2026-09-14）**：`E:\GitHub\pan123-rs`（buladuo，**MIT**，v0.1.0，最后提交 2026-06-26，本轮专门克隆实测——workspace = pan123-sdk **2862 行** + pan123-cli，**web API 路线 Rust 全套**：全端点/QR 三端点认证（双头 Bearer+sso-token）/上传三段流（upload_request→逐分片 presigned PUT→complete）/Range 续传/令牌桶限流/5060 处理）+ PCFS（Go，形态对照 + sign.go 备胎）+ alist 123_open（开放平台参照）。
- **K63.2 对前轮评估的更新**：123 的 web 路线从「直译 PCFS」升为「**移植现成 Rust SDK**」——依赖栈与我们高度重合（reqwest 0.12 + rustls 同版本）；**移植主项 = blocking→async 全量转换**（pan123-sdk 是 `reqwest::blocking`）。签名形态实证分歧：pan123-rs 用随机 key（服务端校验放松）vs PCFS CRC32 替换表——按 pan123-rs 起步、PCFS 备胎。
- **K63.3 新增产品级约束（两发现）**：① **每日下载流量限额**（`traffic/check` 端点显式存在；pan123-rs README 明示不破解、会员 9 元/月解除）——对挂载卷下载量是硬约束，额度数字 123-0 实测；② **MD5 etag 前置**（upload_request 必填）——队列暂存件预计算可解，密文空 etag 行为 spike 确认。
- **K63.4 路线裁决门槛**：123-0 双腿并测（web API 全链 vs 开放平台直链行为）后三选一——web 过且限额可接受 → 路线 B（**初步倾向**：Rust 现成 + 无直链门槛 + 会员可消解限额）；web 不可行且开放平台直链可用 → 路线 A；皆不可行 → 挂起销账。负责人可随时直裁。
- **K63.5 架构口径**：全量公民（照 ck-baidu 四件套）；能力位 range_read/multipart/server_side_move/rapid_upload/authoritative_index/remote_delete 计划为 true，resume 待分片状态保留验证后点亮；删除 = trash `intoRecycle`（**建议沿用 Phase 5 D2 拍板形态**：进回收站、回收站接口不引入）；根目录可设置缺省网盘根（建议沿用 Phase 5 D3）；上传 duplicate 语义待 D4 拍板（建议覆盖模式，百度 rtype=3 同课题）；conformance 桩 = 假 123 API + **假 presigned PUT 接收端**（presign URL 指向 loopback，与 Phase 5 假 OSS 同理）。OSS 上传零自研签名（presigned 是服务端签的，**不引 ali-oss-rs**）。
- **K63.6 硬纪律**：blocking→async 转换主项；动态域名发现（/api/dydomain，失败回退 api.123278.com）；双认证头 + Origin/Referer 必带；5060→duplicate 二次请求；PUT 超时 `max(300s, mb×2s)`；traffic 前置检查给 RateLimited 人话错误；上传后 size 校验；IPv4 dial（pan123-rs 未处理，我们照 baidu 形态加）。
- **状态**：方案与跟踪单落库（`docs/plans/2026-09-14-pan123-driver.md` + `docs/tracking/phase6-pan123.md`），**待负责人批准后开工 123-0**。本批只读（pan123-rs 克隆至仓外 `E:\GitHub\`），主仓零改动。

## 2026-09-14 K64：Phase 6（pan123）拍板——方案获批，D1 直裁 web API，123-0 解锁

- **K64.1 D1 路线 = web API（路线 B，负责人直裁）**：K63.4 的双腿并测门槛就此关闭；路线 A（开放平台）降为纯文字参照不再 spike。前置事实性条件有二，均由 123-0 首项验证/实测钉死：① web API 形态有效性（pan123-rs 最后提交 2026-06-26，~3 月未动）；② 每日下载流量限额额度数字（负责人接受该约束，会员可消解）。
- **K64.2 D2 删除语义 = 沿用 Phase 5 D2 形态**：`trash` 的 `intoRecycle` 原生回收站语义直接采用，回收站端点族不引入，误删恢复走 123 官方端；`remote_delete = true`；表单/DESTROY 文案明示。
- **K64.3 D3 根目录 = 沿用 Phase 5 D3 形态**：`pan123_root` 可设置（folder id）、缺省网盘根 `"0"`、validate 不强制专用子目录、doctor 提示根卷误删边界（与回收站构成双兜底）。
- **K64.4 D4 上传 duplicate 语义 = 覆盖模式**：对齐 writer 契约覆盖语义（百度 rtype=3 同课题先例）——5060 已存在时以 duplicate 覆盖参数重发；「取消报错」「自动改名副本」形态不得漏出到挂载面；覆盖后旧文件/秒传交互行为 123-0 实测并钉进错误映射表。
- **效果**：Phase 6 方案 待批准 → **已批准**；**123-0 解锁可开工**（职责收窄：路线已定，spike = web API 验证与采样，§6 已同步改写）。落档：计划头部/§1.3/§6/§8、跟踪单状态行+任务行+批次日志、AGENTS 项目信息行+当前阶段段。三阶段态势：Phase 4 SF1 / Phase 5 115-0 / Phase 6 123-0 均可开工，`compiled_drivers()` 重构谁先到谁做（只做一次）。

## 2026-09-14 K65：Phase 5 补充研究——115 凭证门槛消失（OpenList 生态实证），AppKey 表述修正

- **触发**：负责人问询 OpenList-APIPages 在线站「使用 OpenList 提供的参数」选项是否意味着官方源码内置 115 app_id/app_key。专门克隆三仓核实（`E:\GitHub\OpenList`、`E:\GitHub\OpenList-APIPages`、`E:\GitHub\115-sdk-go`，均仓外）。
- **K65.1 答案：源码不含明文凭据**。APIPages `wrangler.jsonc` 的 `cloud115_uid/key` 为打码占位，部署时 env 注入（`OPLIST_CLOUD115_UID/KEY`）；`server_use=true` = 服务端密钥代换码 + 主动清空凭据 cookie 防泄漏。OpenList 主仓 115_open 驱动配置面仅 access/refresh 双 token（`meta.go`），自身零 client 凭据。
- **K65.2 决定性技术事实（115-sdk-go `auth.go` 逐行核实）**：device-code PKCE 流全程无 secret——`AuthDeviceCode`（client_id + code_challenge）→ `CodeToToken`（uid + code_verifier）→ **`RefreshToken` 载荷仅 `refresh_token` 一字段**；常规 API 仅 Bearer（`request.go:40-42`）。app_key 只在浏览器 OAuth 重定向流（APIPages 的 `authorize→authCodeToToken`）被服务端使用。
- **K65.3 凭证路径丙（新，推荐起步）**：公共 client_id（非机密，PKCE 设计使然）自铸 token 对 → refresh_token 自持无限续期——运行期零 app_key、零外部依赖、无需自注册。原 K61.3 的「凭证可得性 = 胜负手」**降级为「选哪个 app 身份起步」**；三选一规则不变，预期结论倾向路线 A。
- **K65.4 残余风险（入计划 §7 + 115-0）**：① app 身份连坐——token 绑定他人 app（OpenList/115-plus-desktop），115 封该 app 则同身份 token 全灭（换身份重扫即恢复，无人值守卷断一次；缓解 = `pan115_client_id` 可配置 + doctor 探活指引）；② 共享限额池——若限额按 app 计则公共身份全用户共享（115-0 实测定级：按 token 还是按 app）；③ upload `sign_key/sign_val` 二次认证是否涉 app 级签名未决（115-0 ④）。
- **K65.5 表述修正（对本计划 v1 与会话记录）**：§1.1 原「AppID/AppKey + device-code 流」不准确——app_key 非扫码路线所需；配置键 `pan115_app_id/app_key` 相应调整为 `pan115_client_id`（非机密、驱动可带缺省值）+ 路径甲专用 `pan115_app_key`（可选）。已同步计划 §1.1/§1.3/§4.3/§6/§7/附录 A.5/B。
- **K65.6 附带情报**：OpenList 主仓 = 40+ 网盘驱动 Go 活参照（123/115/quark/189/139/aliyun 全有），对 Phase 6（pan123 路线 A 侧写）与未来驱动均有情报价值；115-sdk-go 的 `const.go` 端点常量表（proapi/passportapi/qrcodeapi 三域 14 端点）可直接对照写 api.rs。本批只读，主仓零改动。

## 2026-09-15 K66：Phase 4（sftp）执行完成——SF1–SF4 落地，SF5 销账

- **K66.1 批次结果**：SF1 驱动骨架 + 配置接入 → SF2 进程内桩 + 41 行为测试 → SF3 conformance 八断言 + 12 处装配接线 + 6 组合裁剪构建 → SF4 WSL2 OpenSSH 真机矩阵 11/11。workspace 1141/0/23（sftp 真机 11 个 `#[ignore]`）；`docs/tracking/phase4-sftp.md` 逐批证据。选型/拍板沿 K59/K60，未变。
- **K66.2 断言①的实现裁决（计划 §7 的修订）**：计划 §7 曾写「不做远程临时文件 + rename 的原子上传」，但 conformance 断言①（interfaces §6，close 前目标不可见）**无豁免旋钮**且离线红。裁决 = **实现 `.part` 暂存 + rename 固化 + `.old` stash**（与 ck-local 同构：writer 落 `<final>.cksftp-<pid>-<seq>.part`、覆盖场景旧版 stash 成 `.old`、close=part→final rename + 远端 size 校验 + 删 stash、abort=删 part + stash→final 恢复、close 失败路径 async `restore_scene()` 恢复现场）。同目录命名是硬要求（SFTP rename 原子性只在同设备成立）。暂存件经 `is_staging_artifact` 过滤对 list 不可见。计划 §7 该条按本裁决修订，§7 其余（不做 ssh-agent/证书、不做 SCp、不做 HTTP-Range 服务端）不变。
- **K66.3 真机揭出的缺陷与修复（数据破坏级）**：驱动原用 `metadata`（SSH_FXP_STAT **跟随**链接）判条目类型 → ①`stat`/`list` 对 link-to-dir 报 `Dir`；②`list(link-to-dir)` **枚举链接目标**（实测列出 `/etc` 的 207 个条目）；③递归 `delete` **下潜进链接目标** —— 与 aeroftp GAP-A02 同型。**修复 = 目录性判定与递归删除改走 lstat**（`symlink_metadata`，不跟随）：`stat`/`list` 报链接本体形态、`list(link-to-dir)` → `Invalid`（walkable 判定拒绝下潜）、递归只删链本体；`reader` 保持跟随（读链即读目标的用户预期）。红→绿留证于 `live_symlink_to_dir_is_never_followed_by_delete`（修前 "listed 207 entries" panic）+ `live_recursive_delete_does_not_follow_links`（修后链接目标内容完好）。桩同步补 lstat/symlink 支持（`symlink_to_dir_is_not_traversed` 离线 5 断言钉死）——**此修复是 SF4 批次的核心产出，超出原矩阵清单**。
- **K66.4 SF5 销账**：不做文件内分段多连接（aeroftp `download_intra_file_pooled` 思路）。依据 = SF4 实测单连接吞吐（WSL2 回环：上行 140.5 MiB/s、下行 67.7 MiB/s；russh-sftp 3.0 会话内 `max_concurrent_reads: 16` 已补 2.x 串行读缺陷）。**口径留档**：回环数字不代表公网；若未来公网场景实测不足，作新批次重新立项（先例数据在手）。
- **K66.5 装配与运维面**：`sftp` feature（K30 形态，默认开，裁剪即全栈 russh 不进依赖图）；`SFTP_DRIVER_REQUIRED`；`compiled_drivers` 追加 sftp 行（既有 3 驱动输出逐字不变）；`BackendTransport::Sftp` + 五方法；`build_sftp_transport`/`sftp_params`（凭据 R3 链：`CYDRIVE_SFTP_PASSWORD`/`CYDRIVE_SFTP_PRIVATE_KEY_PASSPHRASE` env>file）；rebuild 臂；`is_sync_supported` 扩 Sftp（远端文件系统即真相——**sftp 不跑同步任务**，`SFTP_SYNC_UNSUPPORTED` 专文）；doctor 腿（`ck_sftp::probe` 结构化五态 → `sftp_connectivity_check`：**未 pin 指纹 = Warn 带实际指纹与接受键名（D2 的非交互接受途径）**、指纹变更 = Fail）；web 卷表单 + i18n 中英各 10 键。
- **K66.6 未询问的决定（自主模式，最可逆方向）**：①`stat`/`list` 采 lstat 观察（一致性优先：`kind == Dir` 的消费方绝不能把链接当目录；读目标有 `reader` 通道）；②setup 交互向导不做（doctor 显示指纹 + toml/表单编辑已足够，SF4 确认可操作）；③env 链只开两个凭据键（对齐 baidu「仅凭据键有 env」）；④真机套件 `#[ignore]` + env 凭据（R3），不做 CI 接入；⑤符号链接 fixture 用「服务器侧预置 + 缺则 panic」形态（不静默跳过）。
- **K66.7 回滚点**：整批在分支 `feat/sftp-driver`（4 commits：37360ba / 4d3e539+71abc2e / 3f007f7 / 49de14e），merge 前可整支丢弃；merge 后回滚 = revert 那一个 merge commit（合并 commit 版本）；单驱动禁用 = `--no-default-features` 去掉 `sftp`（其余功能零影响——接线全在追加臂）。
- **K66.8 未覆盖项（挂账）**：真实公网链路（延迟/丢包下的断线重连形态、吞吐）、符号链接链（多层链）与 symlink 的 rebuild 走查形态、`k-i`/ssh-agent 认证（沿 SF0 挂账）、SFTP 服务器的非 OpenSSH 实现（如嵌入式服务器 attrs 不可信形态——aeroftp 教训 7 的 STAT 回退未实现，遇服务器再议）。

## 2026-09-15 K67：Phase 4 合入后深度审查——9 项修复（5 驱动 + 4 集成），既有功能零破坏

负责人指令：对 Phase 4 全部代码改动做深度代码审查（E2E 已完成，修复须不破坏既有功能与逻辑）。三路审查（主会话精读 ck-sftp src 六文件 + 子代理 A 测试/桩面 + 子代理 B 集成面），两子代理均无 High。

- **K67.1 驱动修复五项（全部红→绿留证）**：①**symlink 卷根不可用**（H 级——`list`/`stat` 的 lstat 预检拒绝 symlink 根，整卷 Invalid；修复 = 卷根走跟随 stat，卷内条目维持 lstat，GAP-A02 防线不重开）；②**不可寻址名可见**（`\`/`\0` 名句柄往返必败 + 非 UTF-8 名被 russh-sftp 协议层 lossy 成 U+FFFD——修复 = `name_is_addressable` 过滤，「list 产出即可寻址」，ck-local 同源硬化）；③**rename 竞态臂报 Io**（修复 = 镜像 mkdir 竞态臂复查归一 Exists）；④**close 重放窗数据丢失**（H 级——rename ACK 丢失重放后 restore_scene 会把已提交新版本覆盖回旧版；修复 = NotFound + final 已就位且 size 吻合 → 按已提交继续）；⑤**SSH IO 死形态分类**（TCP 拒连/重置/超时按 `io::ErrorKind` 归 Unavailable——重连骨架触发形态对齐）。
- **K67.2 集成修复四项**：①**`CYDRIVE_SFTP_*` env 违背 K28**（装配点直读绕过多卷跳过——跨卷凭据串味；修复 = 两键挪进 `with_env_overrides`（单卷链、空串=清除对齐 baidu），cli 撤 env 直读，多卷免疫由构造保证）；②`sftp_private_key_path` 相对路径 K21 rebase（原锚进程 CWD）；③web 编辑回填漏 `sftp_port`；④`app.js`/`system.js` 标签表漏 sftp 行。
- **K67.3 真机复验**：u18（172.27.199.30）**12/12**——11 旧腿零回归 + 新增 ⑥c symlink 根腿（H1 的 E2E 证明）；吞吐 130.3↑/63.2↓ MiB/s 与修复前同档。
- **K67.4 既有功能零破坏的证据**：workspace 1149/0（基线 1141 + 8 新测试，既有断言零漂移）；conformance 八断言绿；裁剪构建（local,baidu）绿；clippy/fmt/layers/secrets 全绿。
- **K67.5 挂账（审查发现、本批不修——桩加固/覆盖缺口，非当前掩盖）**：桩 read 不校验句柄可读性（A-M1）、reader early-EOF 分支桩不可达（A-M2，快照 buf 模型）、quota 真值分支零覆盖（A-M3，桩不声明 statvfs 且无开关）、symlink×rename 组合桩不支持含 writer stash 对 symlink 目标的双侧空白（A-M4）、真机断线重连腿仅有桩覆盖（A-M5——重连行为对真实 OpenSSH TCP 形态的验证仍空）、`is_staging_artifact` 对用户合法文件 `report.cksftp-0-0.part` 的隐藏边界（已在 lib 单测文档化钉死）、非 UTF-8 名 lossy 的根因在 russh-sftp buf.rs（上游形态，驱动侧不可见化已是本仓能做的全部）。
- **K67.6 回滚点**：分支 `fix/phase4-review`（单 commit），merge 前可整支丢弃；merge 后 `git revert -m 1 <merge>`；env 路由单独回退 = 撤 with_env_overrides 两键 + sftp_params 恢复直读（改动各 ≤10 行）。

## 2026-09-15 K68：尺寸优化编译档 release-min（性能/稳定性零妥协）

负责人要求在不影响性能与稳定性的前提下做优化编译。裁决 = 只加链接期优化，不碰任何运行时语义：

- **K68.1 profile 形态**：`[profile.release-min]`（独立命名档，默认 release 零变化）：`lto="fat"` + `codegen-units=1` + `strip="symbols"`，显式继承 `release` 的 `opt-level=3`。
- **K68.2 两项刻意不做**：①`panic="abort"` 禁用——控制通道 handler 与 web 执行任务的 panic 防护依赖 `catch_unwind`（K55-H1/K58-H4），abort 会废掉该防线；②`opt-level="z"/"s"` 不做——性能零妥协是本次前提。
- **K68.3 实测数据（sftp,winfsp 组合）**：18 MB → **15 MB**（-17%）；winfsp 真机挂载冒烟全绿（X: 挂载→rebuild→穿透盘符读 real.bin 字节正确→`cydrive stop` 干净卸载无残留）；吞吐取证 u18 128 MiB **300.8↑/317.8↓ MiB/s**（release-min，`cargo test --profile release-min`）——同机既有矩阵数字 130/63 系 **debug 测试档**，release 族全面更快（LTO 无性能损耗，符合预期）。
- **K68.4 可诊断性**：MSVC 链接器仍生成独立 PDB，`strip="symbols"` 只去 exe 内符号表——线上崩溃仍可回溯符号。
- **K68.5 回滚**：删 Cargo.toml 的 `[profile.release-min]` 节即回退（零代码面影响；release 档从未改动）。

## 2026-09-16 K69：Phase 5 115-0 spike 收口——路线 A go（路径丙自铸成立），QPS 770004 账号级限流实测，OSS 层采用 spike 自研签名

按计划 §1.3 裁决规则①执行（自主授权会话）：凭证取得 + downurl/Range/QPS/upload 实测全过 → **路线 A（官方开放平台）go**；路线 B（私有 web API）不启用，其情报保留为设计保底。

- **K69.1 凭证路径丙全链成立**：公共 client_id **`100197303`**（OpenList 官方托管 app——取自 api.oplist.org `server_use=true` 登录端点返回的 authorize 跳转 URL，公开非机密值；115-plus-desktop 源码不含 AppID（.env 空占位），APIPages 源码打码）→ device-code PKCE 扫码 → token 对 → Bearer user/info ✅ → refreshToken 自续一次 ✅（轮换落盘复验）。运行期零 app_key、零外部服务。**token 有效期 7200s**；refresh 严禁频繁（官方频控）。备选身份恢复路径：`pan115_client_id` 可配置，换身份重扫即恢复。
- **K69.2 K65 未决项销账——`sign_key/sign_val` 是用户级挑战而非 app 级签名**：init 响应携带 `sign_key`+`sign_check`("start-end" 闭区间)，`sign_val` = 该区间文件字节 SHA1 大写hex，回带重发 init 即过（真机：秒传验证链首轮即触发，**常态路径而非罕见**，驱动必须实现循环）。app_key 与上传二次认证无关 → 路径丙不受限。
- **K69.3 QPS 限流实测（D4 定数依据）**：持续 ~4 rps 可行；**5 rps 下 10 秒内 22% 请求被拒**；触发后整账号（**跨端点族**，user/info 同封）返回 `770004 "已达到当前访问上限"`，封锁窗口 **≥10 分钟**（实测 10 分钟未解除即停止探测，保守按 ≥10min 设计）。OpenList 官方驱动默认自限 **1 rps**（burst 1）。**D4 落值：全局 limiter 缺省 1 rps（列目录族），770004 → 硬退避（初值 5 分钟，指数，无风暴重试）**。真实限流码是 770004（20130827/911 形态来自桌面版代码未真机复现，一并入错误表）。
- **K69.4 downurl/CDN 约束钉死**：直链与取链 UA **逐字节绑定**（错配恒 403，双向实测），但 **UA 形态本身不约束**（browser/spike/空 UA 三态全通）——驱动选固定 UA 常量即可，无需伪造浏览器。HEAD 探测 accept-ranges/etag 可用；Range 206 + Content-Range 起始吻合校验；**CDN etag = 文件 MD5**（完整性旁证）；CDN 403=限流（退避+降并发）≠ 401/410=URL 过期（重取直链）。downurl 缓存 TTL 未测（115-5 冒烟顺带），缺省 30min 保守值。
- **K69.5 OSS 层决策：用 spike 自研签名层，不引入 ali-oss-rs**：版本树核对通过（ali-oss-rs 0.2.5 → reqwest ^0.12.12 同大版本，无冲突——回退条款的触发条件并未成立），但 115-0 spike 已产出**真机验证过**的 OSS V1 手写签名（5 操作：Put/Initiate?sequential/UploadPart/ListParts/Complete+callback，534 行，含 115 特有行为与两个签名陷阱——URL 尾斜杠归一、子资源排序），零新依赖。采用自研层的依据 = 计划 §2 回退形态 + 零依赖 + 已实证；回滚 = 换 ali-oss-rs（版本树已验兼容）。ListParts 分页（>1000 片）在驱动实现时补全。
- **K69.6 秒传/哈希面**：秒传命中真机验证（同 SHA1 重 init → status==2，需过二次认证）；**伪造哈希 init 不被拒**（服务端在 complete/callback 侧校验）——「省略哈希直传」优化不做（风险不对称，明文/密文卷都照算真实哈希，密文永不秒传无害）。preid = 前 128KiB SHA1；全大写 hex。
- **K69.7 错误码采样表（真机）**：`40199002`=QR 过期/key invalid（qrcodeapi，快拒）；`40101017`=未确认换 token；`40140123`=access_token 格式错；`401*`/`99`=token 过期（刷+重放一次）；`770004`=账号级访问上限；`911`=需人工验证（未真机复现）；`430004`=文件不存在（SDK 侧）；错误包 **HTTP 200 + envelope 双形态**（proapi `state:false` 布尔 / passportapi `state:0` 数字）——HTTP 状态码不可作判据。
- **K69.8 上传链实证**：PutObject（3MiB）1.8s / 三片 multipart（12MiB）2.3s（OSS 直传，callback JSON 回应）；complete 后 size 复核纪律有效；resume 差集补片（1 复用 2 补）过。分片 min 5MiB、10000 片上限、sequential 子资源、callback 挂 Complete/Put（`x-oss-callback(-var)` base64 头）。
- **K69.9 扫码停点协议记录（自主会话专用）**：QR 有效期 **~5 分钟**（签发→40199002 快拒），等待循环按 <5min 周期自动换码（同路径 PNG 恒新鲜），长轮询 30s/次。两轮窗口共 ~6.5h 无扫码、第三轮 13:05 捕获（80+1 周期、authDeviceCode 81 次无频控）。
- **K69.10 回滚**：整批在 worktree 分支 `feat/pan115-driver`（merge 前 `git checkout main && git worktree remove` 即弃）；spike 为 workspace-excluded 不入 CI。账号侧残留：`/_e2e_pan115/` 空目录保留 + 回收站 4 个测试文件（D2 语义可恢复）。

## 2026-09-16 K70：Phase 5 完成（115-0…115-5）——pan115 驱动上线，装配批两处真实缺陷修复

Phase 5 五批次全落地（worktree `feat/pan115-driver`，`0b53c14`→`32060be`）。115-0 的路线裁决见 K69（路线 A go）。

- **K70.1 批次链**：115-0 spike（auth 腿 `0b53c14` + 探针腿 `acebe05`）→ 115-1 骨架/认证/配置（`65efd51`）→ 115-2 读路径（`12ecddd`）→ 115-3 写路径（`cfbf907`）→ 115-4a conformance（`26bc85d`）+ 4b 全装配（`05a7c7d`）→ 115-5 真机冒烟（`32060be`）。
- **K70.2 装配批揭出的两个真实设计缺陷（非桩面瑕疵）**：①**`stat` 曾被路径缓存整层服务**——后端错误永远浮不出来（conformance ⑤ 不可满足）；修复 = 末级新鲜查询（前缀仍缓存，list 的零网络缓存保护不破）。②**分片传输中途失败不落会话**——差集续传资产丢失（断言⑦ 失效）；修复 = 每片落地即持久化、失败路径同样落盘，并按 115「init 锁定 block_list」语义实现**到齐即传**（`write` 到齐承诺量即推传输链，`close` 仍是提交点）。
- **K70.3 真机揭出的生产级缺陷**：**OSS endpoint 带 scheme 未剥离**——115 `get_token` 下发 `https://oss-….aliyuncs.com`，port 时丢了 spike 的剥离步骤（`probes.rs:611`），`host()` 拼出 `bucket.https://…` → OSS `SecondLevelDomainForbidden`（真机首跑即中）。修复 = `normalize_endpoint`；同时**测试缝判据改按 loopback host**（原按「含 `://`」会把生产流量误导向 path-style——同一缺陷的另一半）。教训：**桩测不出的形态只有真机揭**，115-5 的最小冒烟价值即在此。
- **K70.4 sync 裁决**：pan115 支持 sync（baidu 式接入，`sync_namespace_key = pan115:<uid>`），依据 = 计划 §4.3 明文「sync_namespace 同 baidu 形态」+ 零 core 改动最可逆（回滚 = 单臂改回 bail）。
- **K70.5 conformance ⑤ 表形态裁决**：`error_table` 只放可被「注入恰好一次后恢复」契约验证的码（`430004 → NotFound`）；**刻意不放 `770004`**——账号级上限触发的是 D4 硬退避窗（驱动主动进入封锁态），与断言⑤的一次性语义天然冲突，强塞只能靠削弱断言通过；映射由 `tests/errno_mapping.rs` 单测钉。
- **K70.6 真机数字（115-5 最小冒烟）**：上传 200KiB → 回读逐字节 + close 内 size 复核；Range `[100000,150000)` 逐字节；秒传第二轮**同 fid**（status==2 实证）。作业纪律全守（只 /_e2e_pan115/、清理后核空、1rps 限速、未碰分享/离线/视频族）。
- **K70.7 未覆盖（如实挂账）**：完整矩阵的断点续传杀进程恢复、rebuild 收敛耗时、加密卷 aead_v2 往返、E2E 三面（WebDAV/web/winfsp）、>5MiB 多分片真机上传（本次三项走 PutObject 与秒传路径）、目录 rename 真机形态（驱动按 move+update 实现未验）；setup 扫码向导未做（oauth.rs 三端点已备，无 GUI 向导——挂账待裁）。真机测试入口 = `crates/drivers/ck-pan115/tests/live_matrix.rs`（`#[ignore]`，凭据只经 env）。
- **K70.8 回滚**：整批在 worktree 分支 `feat/pan115-driver`（单批 merge，merge 前 `git worktree remove` 即弃；merge 后 `git revert -m 1 <merge>`）；spike `examples/pan115_spike` workspace-excluded 不入 CI。账号侧残留：`/_e2e_pan115/` 空目录保留 + 回收站若干测试文件（D2 语义可恢复）。

## 2026-09-16 K71：pan115 E2E 补批——加密全栈 + WebDAV 双模式真机全过（负责人指令批）

- **K71.1 加密全栈**（`pan115_vfs_aead_v2_full_stack_roundtrip`，K54 tg 先例 + pan115 特化）：1.5MiB 明文跨两个 1MiB VFS 块 → 行元数据（uploaded/encrypted/aead_v2/明文 size）→ hydrate 逐字节 → open_read 跨块窗口解密 → **远端密文核验**（驱动绕过 VFS 直读远端：1572930B vs 明文 1572864B——+66B = v2 容器 per-chunk 开销；明文首 64B 特征段在远端全文不出现）。
- **K71.2 WebDAV E2E 双模式**（`pan115_webdav_roundtrip_plain_and_encrypted`）：`run_with_transport` 注入真 Pan115Transport、webdav `:0` 临时端口、auto-mount/web-ui 关——明文与加密（aead_v2+随机口令）各一轮 MKCOL→PUT(256KiB)→GET 逐字节→PROPFIND 207 含目标名；加密轮 GET 面 = 解密后明文逐字节。收尾 driver 删远端（D2 回收站）+ NotFound 核验。
- **K71.3 执行期注意点（后续 E2E 复用）**：①RFC 4918——PUT 到父集合不存在的路径 409，先 MKCOL；②`cfg.validate()` 生产口径拒 `webdav_port=0`，而 run 流程支持 :0 临时端口——测试 boot 不调 validate（run_e2e 先例同）；③Windows 上写完主动 `shutdown()` 会把连接整断，靠 `Connection: close` 服务端收尾。
- **K71.4 flaky 观察项（未定位，如实记录）**：cargo clean 后首轮全量中 `ck-pan115::upload_path::size_mismatch_after_complete_is_refused` 失败一次（首建高争用窗口）；单跑 5/5、全量复跑 2/2 绿。疑点在 `resolve_new_row` 的 5s 轮询窗，未证实——不动代码，留观察。
- **K71.5 回滚**：本批仅新增 `crates/cloudkit-cli/tests/pan115_e2e.rs`（#[ignore] 真机测试，不入 CI）+ 三处文档；`git revert <commit>` 即整批回退。

## 2026-09-16 K72：115 live 用例的 SHA1 去重短路教训（K70.7 收口批执行期实证；补档 2026-09-17，深度审查 M-T5 销账）

- **K72.1 去重与名字无关**：115 的秒传按**内容 SHA1 全局去重**（K69.6 语义延伸）——live 用例若用固定名 + 固定内容，历史轮次残留（或跨用例同内容，含离线桩跑过的同 pattern）会让 init 直接命中秒传，**绕过待测路径且真机无桩计数不可见**。纪律：真机用例一律 stamp 唯一名 + 按轮随机内容（live_matrix ⑤⑥ 形态；④ 原漏此纪律，2026-09-17 审查 M-T3 对齐）。
- **K72.2 会话/秒传的判定面**：固定形态下「零分片零会话」是命中秒传的可观测信号（⑥ 的会话文件断言即以此兜底——resume 路径被短路时 `session_files.len() == 1` 断言先红）。
- **K72.3 关联**：同批删除 `debug_rename_landing` 排查探针（无断言、不清理，`--ignored` 会连带执行——审查 M-T2）；其调查目的（跨父 move 后落点/索引延迟）已由 ⑤ 的断言化用例覆盖。回滚随 `fix/phase5-review` 分支。

## 2026-09-17 K73：Phase 5 合入后深度审查修复批——2 High + 12 Medium 全清偿（fix/phase5-review）

- **K73.1 审查形态**：三路并行（主会话精读 ck-pan115 九模块 + 子代理测试/桩面 + 子代理集成/装配面，关键发现主会话亲验）；在制未提交改动纳入审查范围。产出 2 High + 12 Medium + Low 若干，逐项销账表 = `docs/tracking/phase5-review-fixes.md`。
- **K73.2 实质修复五处驱动缺陷**：①delete 句柄 parent 段恒空（ghost 缓存 + API 形态未验，M-S1）；②STS 每分片重取（N+2 次 1rps 限流调用，M-S2）；③直链 401/410 不自愈（长流硬死，M-S3）；④rename 冷目录预检穿透（Exists 契约丢失，M-S4）；⑤pathcache 无 TTL/无上界（外部删改永久陈旧 + rebuild 内存无界，M-S5）。全部 TDD 红→绿留证。
- **K73.3 裁剪组合双缺口**（M-I1/M-I2）：`not(baidu)+pan115` 曾丢 TokenStore 回写（一次一换下 = 重启失授权）+ 多卷 dispatch 把非 pan115 卷误装配——修复 = dispatch_unified_backend_volume 移入 lib 成可测缝 + 单一 match 全组合通用 + twin 对齐主 twin；组合构建下红→绿钉死（pan115_combo_dispatch.rs）。
- **K73.4 测试面可信度**：H-T1 flaky 根因（自拼 tmpdir 撞名）tempfile 化（压测 2/26 失败 → 0/30）；H-T2 probe 假覆盖（名实不符 + 注入空转）重写为四变体真实触发；M-T1 sign_val 数值常量钉；M-T4 normalize_endpoint 生产分支正面钉。dev-dep sha1 触发 E0464 双 rlib → 预计算常量替代（同性质零依赖面）。
- **K73.5 自主模式裁决记录**：工作区在制改动（setup 向导 + live ④⑤⑥ + e2e 腿）是多项 Medium 的修复载体，先修其自身问题（探针/④随机化/K72 补档/门禁红/poll 上限）commit 为 15195ce 再叠修复批——不动 main、不 push，`git branch -D fix/phase5-review` 即整批回退。`examples/sftp-config/`（Phase 4 运行遗留，含内网 IP/root 名）判定不入库，留负责人处置。
- **K73.6 挂账**：Low 项与真机待验项见跟踪单「Low 挂账」「真机待验项」两节（rename 宽映射 / OSS status=0 重试分类 / setup IPv4 对齐 / 真机 parent_id 与直链 TTL 等）。

## 2026-09-17 K74：Phase 5 审查修复批真机验证——10/10 全绿；揭出 move wire-form 静默错置缺陷（to_pid → to_cid）

- **K74.1 凭据定位与轮换**：真机 token 对在 `E:\GitHub\rs-CyDrive\test\pan115-tokens.json`（spike `Paths::tokens()` 落盘位；授权目录）；`probe-refresh` 先行一次（轮换对回写盘上 + user/info 验活）——测试全程落在 7200s 有效窗内，无内存刷新消耗盘上一次一换对。
- **K74.2 move wire-form 缺陷（审查漏网，真机才揭）**：`/open/ufile/move` 的目标参数官方 SDK 形态是 **`to_cid`**（115-sdk-go MoveReq / 115-plus-desktop file.ts 双参照一致），驱动误发 `to_pid`——错误包恒 HTTP 200 的信封文化下**静默接受但移动错置**（文件落到账号根，get_info 活着、目标列表不可见）。修复 = `to_cid` + 两桩改按 SDK 文档严格建模（缺参即拒——「桩照实现抄参数」与 M-T1 同类的第三例）。**旧形态在用户账号根累积了 21 件测试碎片，已按严格命名模式清扫核空（remaining: 0，D2 回收站可恢复）。**
- **K74.3 探针方法论教训**：v1–v3 探针曾误判「后端根列表索引黑洞」——实为探针把文件 move 到账号根却在 `_e2e_pan115` 里找；v4 绕过驱动直查端点三形态（cid=0 / 无 cid / 全参）一次证伪。**纪律：调查「不可见」先核对观察点与操作目标是否同一目录；结论必须出自直查端点的原始响应。**
- **K74.4 rebuild 契约实证**：`run_rebuild_with_driver` 从 **driver 的卷根**走（cfg 的 `pan115_root` 只做装配面一致性）——测试曾传 root="0" driver + scoped cfg，首轮 1rps 全账号 11.7 万文件 25 分钟未走完；scoped driver 后重建本体 **2.0s**（files=2/dirs=1）。e2e 腿同步落 K72 纪律（stamp 唯一名 + 按轮随机内容）。
- **K74.5 真机全绿板**：live_matrix **6/6**（①上传回读 ②Range 窗口 ③秒传同 fid ④12MiB multipart 5.1s ⑤目录 rename 双腿 ⑥断点续传：真 OSS ListParts 3 片 + 同 uploadId 复用 + 12MiB 逐字节）；pan115_e2e **4/4**（加密全栈远端密文核验 / WebDAV 明密双轮 / rebuild 收敛 / setup 流三端点探测）。**销账**：M-S1 新句柄 delete 真机形态 ✓（SUMMARY 三段句柄 + cleanup 删除成功）、M-S3 桩建模待真机 401/410 实发（未遇，挂账维持）、④随机化重跑 ✓、downurl 直链 TTL 真值未测（挂账维持）。
- **K74.6 回滚**：随 `fix/phase5-review` 分支（3c5ab51 + fe4844a）。

## 2026-09-17 K75：Low 挂账收尾批——2 实质修复 + 4 一行级搭车；7 条裁决不修

- **K75.1 rename 错误映射收窄**：move 臂的 `Io/Unavailable/Invalid → Exists` 映射整体删除——传输失败伪装成「目标已占用」会把用户引向覆盖操作（与 M-S4 同族的契约问题）。红→绿：502/非 JSON 传输形态注入下 rename 不再误报 Exists、源文件原位未动；后端明确拒绝码（430001 同名等）仍如实上抛。
- **K75.2 放弃上传的远端释放**：`oss::abort_multipart`（DELETE ?uploadId，V1 签名走既有 oss_execute）+ stager abort 先释放远端再清本地——OSS 对未 complete 的分片**保留并计配额**，此前每次放弃 multipart 上传都泄漏。红→绿：桩收到恰一次 AbortMultipartUpload。abort 失败仅告警不阻塞（用户已决定放弃）；真 kill（Drop 都不跑）场景仍由 OSS 生命周期规则兜底——诚实边界。
- **K75.3 一行级批**：open_writer 幂等 create_dir_all（防御）；setup_http_client 补 IPv4 绑定（K18 对齐——向导与驱动在 IPv6-preferring 网络行为一致）；conformance 临时目录 tempfile 化（H-T1 同族）；doctor 两臂 22/18 连续字面空格改 `\` 续行 + `_cfg_marker`/`let _ = &mut st` debris 清除。
- **K75.4 裁决不修（查证支撑，非省事）**：①OSS status=0 不进 retryable——`upload_queue::decide_retry` 对任何错误按退避梯重试，标签不影响行为；②多卷 limiter 相加——4rps 实测余量足够，跨卷共享的结构改动不买行为；③callback 响应体丢弃——resolve_new_row 兜底；④桩面三条——分别有真机未现/双覆盖/纯函数钉；⑤真 kill 形态——K75.2 已消解主危害；⑥https-loopback——无消费方；⑦lib.rs:6836 WebClient hint 空格——非 pan115 面。
- **K75.5 验证**：ck-pan115 78/0（+2 红→绿用例）；workspace 全量/clippy/fmt/layers/secrets 五门禁绿（数字见 AGENTS）。回滚随分支。

## 2026-09-20 K76：Phase 6 pan123 增补研究 + D5 拍板——web 身份合规路线；端点代际警示（123panNextGen 深读）

- **K76.1 研究形态**：负责人指令在实施 Phase 6 前深度研究 `E:\GitHub\123panNextGen`（第三方 123 桌面客户端，PySide6/GPLv3，~21k 行，最后提交 2026-09-17，活过 2026-08-29 端点重组）。三路 Explore 子代理（协议层/传输层/文件操作面）+ 主会话综合，零代码改动；情报全文 = pan123 计划附录 A.4。
- **K76.2 D5 拍板（负责人 2026-09-20，承接 K64-D1）**：客户端身份与流量姿态 = **web 身份合规路线**——认证照 pan123-rs web 形态；**不采纳**安卓协议模拟（platform:android 设备指纹族）与 URL 重写流量绕过链（5113/5114 软处理 + web-pro2 代理 + auto_redirect=0；完整证据链留档计划附录 A.4 备未来姿态变化，当前不进任何代码）；5113/5114 → `RateLimited` 硬错误 + 人话指引，维持会员消解路径。
- **K76.3 端点代际发现（对 K64 前提的修订）**：123 服务端 2026-08-29 端点大重组（前缀即代际：list 无前缀、trash/rename/建目录/download_info 走 `/a/api/`、upload 族/delete/traffic 走 `/b/api/`）+ 主域迁 `www.123pan.cn`（`api.123278.com` 降级为粘性 fallback）→ **pan123-rs（2026-06-26）线形态部分过时**；123-0 spike 首组验证改为现行形态复验（计划 §3.1 代际警示表）。附加 spike 项：info 端点存活性、签名要求是否放松（123panNextGen 全程无签名）、duplicate 1 vs 2 覆盖语义真机钉死（两参照注释矛盾，D4 框架内）。
- **K76.4 token 无 refresh 实证**：123 web API 无刷新端点（123panNextGen 同样只能凭密码重登）——pan123 错误映射修订：token 失效 → `Unauthorized{recoverable:false}` + 重扫码可行动指引（原「重登/刷新一次」不成立，也不为此扩密码凭据面）。
- **K76.5 稳定/性能采纳面**（入计划 §5.10–17）：envelope 双成功码（code==0/200）与字段双拼解析、trash 最小 payload 静默失败陷阱（`code=0` 但不删——数据完整性级，回读校验 + 逐端点键名保真）、传输/API 双会话分离（presigned PUT 无 123pan 头）、上传七步严格序（`upload_complete` 漏掉 = 文件不入库；>64MB sleep 3s）、dlink 一次性纪律 + CDN JSON 重定向体、Retry-After 优先退避常量、时间双态解析、粘性域名 fallback。**漂移监控哨**：以 123panNextGen 仓库提交流为前哨警报源（§7 风险表）。
- **K76.6 落档**：pan123 计划（头部/§0.1/§3/§3.1/§5/§6/§7/§8/附录 A.4）+ 跟踪单（123-0 行改写/研究表/批次日志/风险节）。Phase 7 webdav 计划原拟占用的 K76/K77 编号顺延为 K77/K78（webdav 未批准未落 decisions，无实际冲突）。

## 2026-09-20 K77：Phase 6 完成（123-0…123-5）——pan123 驱动上线，resume 模型真机证伪改本地会话优先

Phase 6 六批次全落地（worktree `feat/pan123-driver`，`6bd6ef5`→`e0dbcc5`）。批次链：123-0 两腿 spike（读腿 `6bd6ef5` + 写腿 `e54970a`）→ 123-1 骨架/认证（`3a61b1c`）→ 123-2 读路径（`829725a`）→ 123-3 写路径（`98fc25d`）→ 123-4 conformance+装配（`2f0987c`）→ 123-5 真机矩阵（`8ecf790` 修复 + `e0dbcc5` 测试）。装配批揭出驱动缺陷：delete 非数字句柄破坏 conformance ④ 幂等契约（归入查无实体 → Ok）。D5 web 身份合规路线（K76.2）全程执行。

- **K77.1 resume 模型证伪（数据面级，123-5 三轮裸实验）**：spike 钉死的「同参重发 `upload_request` 返回同一 UploadId」**只在零分片时成立**——PUT 首片后同参重发恒铸新会话（旧 tuple 仍可 list_parts，成为远端孤儿泄漏）。123-3 按 spike 建的 re-request 主路径对已传分片**永不可达差集**（每次重开 = 全量重传 + 会话累积泄漏）。修复 = **本地五元组优先**（`establish_session` 先取本地会话记录对账旧 tuple：Parts 即差集续传 / gone 即清记录走 re-request——pan123-rs/123panNextGen 本地持久化模型即为此；spike 之所以看到会话保留，是因为中止在零片态）。红→绿：桩改真形（有片会话的重发铸新）后 conformance `pan123_partial_parts_resume_only_missing` + suite 红 → 绿；write_path 的 upload_request 计数断言随模型修正 2→1（差集 PUT 计数断言不动）。真机对账：服务端 list 恰 [(1,5MiB)]、同 UploadId、补缺 1.7s vs 全量 5.3s。
- **K77.2 trash 载荷真值（「桩照实现抄」第三例）**：`operation:true` 必填（缺则 400「请输入Operation」）——123-0 spike 恒发四键形故未踩到，123-2 按跟踪单摘要抄漏两键形；真机 curl A/B 钉死（`{fileTrashInfoList, operation:true}` 即 code=0，`event`/`driveId` 可省）。三例全录：pan115 `to_cid`（K74.2）/ pan115 sign 键形（K73 M-T1）/ 本次 trash operation——**教训：wire 形态必须出自服务端真形或官方 SDK 建模，驱动实现与二手摘要都不足为据；「桩照实现抄」会把实现缺陷铸成桩钉**。
- **K77.3 sign_in token 并存上限（FIFO-2 观测）**：同账号新 mint 的 token 至多两枚并存，第 3 枚踢掉最旧（T1/T2 并存、T3 mint 后 T1 死 Unauthorized、T4/T5 mint 后 T4 仍活 = 连续 mint 踢最旧）。操作纪律：多测试轮换 token 时并行有效数 ≤2；被踢的旧 token 直接 Unauthorized，无缓冲期。
- **K77.4 流量触顶腿跳过裁决**：不主动耗尽当日 ≈10GiB 免费下载额度来采 5113/5114 真身（不可逆消耗 + 阻塞当日后续验证窗口）——5113/5114 真身维持挂账（桩按 D5 形态建模，映射 `RateLimited`+人话指引由 errno_mapping 单测钉死）。余量计数器滞后粗粒度实证：10.5MiB 下载的 15s 窗采样 Δ0~14,713B 不等、全天 ≈95MiB 实际下载对应计数 9.99→9.77GiB——非即时、非全量扣减（真机硬断言只保余量不增）。
- **K77.5 端点代际并存事实收口（123-0 实证，修订 K76.3 表述）**：`/api/`、`/a/`、`/b/` 三前缀**并存全活而非代际替换**——2026-08-29「重组」实为「并存分叉」。驱动取用清单：list=`/api`（new）、mkdir/trash/rename/download_info=`/a`、upload 族/traffic/user_info/mod_pid=`/b`、complete=`/v2`。单选不做双发（已验证形态钉死；漂移由 123panNextGen 提交流监控哨兜底，计划 §7）。
- **K77.6 K72 复现（去重纪律第三验）**：123-5 首轮 resume 腿「expected Ticket got Reuse」红——u8 种子确定性内容跨轮撞 etag 触发服务端秒传短路（且真机无桩计数可见）；换 64 位 LCG（pan115_e2e 同款）修复。**真机用例 stamp 唯一名 + 按轮随机内容，两条缺一即被秒传短路**。
- **K77.7 终态与回滚**：六批次完成态 + 证据全录跟踪单 `docs/tracking/phase6-pan123.md`；workspace 终态 **1360/0/42**（收尾批复跑，170 suites 全 ok；ignored 42 = sftp 12 + pan115 6+4 + pan123 3+5 + 真网/平台类）；clippy/fmt/check_layers/scan_secrets 绿 + 裁剪六组合零告警（123-4 批）。遗留挂账终表 = 跟踪单「终态挂账总表」（5113/5114 真身、QR 人扫、空文件链真机、mod_pid 跨父、dlink TTL 真值、token 淘汰精确策略等，均验证/观察类）。回滚：整批在 worktree 分支 `feat/pan123-driver`（merge 前 `git worktree remove` 即弃；merge 后 `git revert -m 1 <merge>`）；spike `examples/pan123_spike` workspace-excluded 不入 CI；账号侧：专用测试根 `e2e_pan123_root_3800` 与 `e2e_pan123` 前缀已 trash 清扫 + 根 Total=0 核空（回收站残留为 D2 语义可恢复）。本条占用 K77 后，Phase 7 webdav 计划原拟的 K77/K78 编号再顺延为 K78/K79（webdav 未批准未落 decisions，无实际冲突）。
