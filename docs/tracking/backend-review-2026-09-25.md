# 后端全架构深度审查 findings 档案（2026-09-25）

> 审查对象：`main@c7c3ec9`（上次架构审查 `acb9b16` 之后 36 个提交：webdav/sftp/baidu/local 四轮审查修复、winfsp/webdav 改名远端先行、K88/K89、装配根校验门、CI 校准含 tokio::fs 修复）
> 方法：9 路并行只读审查子代理（core+crypto+platform / storage 契约 / 7 驱动分 4 组 / CLI / web·webdav·winfsp·sync 服务面 / 架构红线横切）+ 主会话对全部 High 级结论逐一亲验（读源码复核证据链）
> 基线：fmt/layers(R1)/secrets 三快门禁绿；全量测试两轮被**本地 rustc 崩溃**（round1 alloc_error、round2 STATUS_STACK_BUFFER_OVERRUN，连带 E0463）阻断——环境资源压力（K73 陷阱族）非代码回归，CI 双平台全绿（20bedee/f26e025）为反证；`-j 2` 重试结果见文末补记
> 结论：**架构本体健康**——R1–R7 零违规、依赖图与标准逐点吻合、安全面（CSRF/DNS-rebinding/同源/loopback 降级/凭据脱敏漏斗）纵深完整、七个宽面驱动契约一致；缺陷集中在**近期合入批的边角**与**从未被审计过的路径**。共 **3 High / 17 Medium / 39 Low / 41 Info**。
> 处置：本档案只入档不改码；修复批待负责人批准（建议 H 全修 + M 择修，见文末）。

## High（3，主会话亲验成立）

### H1 ck-baidu 流式上传暂存仍是 tokio::fs 写→零间隙读——6fc56b9 定谳的数据破坏级形态漏列本面
- 位置：`crates/drivers/ck-baidu/src/transport_face.rs:132-186`（`stage_stream`：`tokio::fs::OpenOptions` 开 + `AsyncWriteExt::write_all` 逐帧写）+ `crates/drivers/ck-baidu/src/upload.rs:195-242`（`PartFile::block_md5s`/`part` 用 `tokio::fs::File::open` + `read_exact` 零间隙读回算 md5/分片）
- 形态：`drop(file)` 后立即开**新句柄**读回——drop 只排队 close 任务不等待在途写；Linux 上 write().await 返回时 syscall 可能在途 → md5 算在缺尾/错位数据上 → **错位分片经 31363 锁定的 block_list 永久落服务端**。Windows 免疫。
- 暴露面：baidu 卷加密 v2 流式上传（`upload_v2_stream` → `transport.upload_stream`；aead_v2 是默认加密方案）。
- 引入：`407a88d`（M4 分片读盘重排）重排后的面；6fc56b9 的修复清单（L2 spool/pan115/pan123 + 03a8fbf webdav stager）漏列 ck-baidu，当日「全仓审计 0 风险残留」结论已在 AGENTS/platform-builds 勘误。
- 修法：`stage_stream` 写面与 `PartFile` 读面同批换 std::fs + spawn_blocking（pan123 `spool_range` 同款）；补「写后立即可见」钉测。
- 验证：主会话读双文件源码逐行复核成立；未活体复现（同 6fc56b9 标定记录的形态同构）。

### H2 upload_queue 同路径改写竞态：整行快照回写 + 本地副本删除——新版数据可整体丢失
- 位置：`crates/cloudkit-core/src/upload_queue.rs:624`（job 开始时读一次 row 快照）→ `:823-830`（远端成功后 `persist_success` 用快照整行回写：`uploaded_upsert` 拷 `row.name/mtime/size/is_encrypted` + `is_uploaded: true`，`:961-983`）→ `:830` `delete_local_copy(&job.local_path)`
- 形态：唯一防超车守卫是 0 字节空 PUT 那条（`:659-690`，2026-09-09 field log 只修了 size==0 变体）。非零同路径改写（winfsp/webdav 写者 save 两次，第一次上传在途）→ 旧 job 成功后把**新 pending 行**整行覆盖为旧快照（is_uploaded=true、旧 size/mtime/msg_id），并删掉新 job 还要读的本地字节 → 新 job os error 2 降级，**新版本远端与本地俱失**；反向排序则旧快照覆盖新成功行。窗口=整次上传时长（可达分钟级）。
- 修法：`persist_success` 前重读行，`row.is_uploaded == false && (size/mtime 与快照不符)` 时跳过回写（计 degraded、行留 pending）；或把整行 upsert 改为定向列写（`is_uploaded`/`telegram_msg_id`/`chunk_count`/chunks，对齐 `set_cached_flag`/`fix_cipher_columns` 先例）。
- 验证：主会话读三段源码复核成立；竞态窗代码形态确认，未跑复现测试（修复批须先写红测）。

### H3 多卷离线 rebuild 的 token 轮换写进**进程** config.toml——下次启动被 K19 混键守卫拒启
- 位置：`crates/cloudkit-cli/src/lib.rs:6521-6528`（baidu 臂 `ConfigTokenStore::default()` = 路径 `"config.toml"`，`:6188-6194`）+ `:6575-6578`（pan115 臂同款）+ `:6196-6224`（`save_tokens`：load_toml(进程 config)→改 baidu_*>整结构 save_toml）+ `Path::new(".")` state_dir（K21 漂移）
- 形态：多卷模式 `run_rebuild_multi`（`:4948-4966`，`main.rs:793` 探活失败进离线路径）逐卷 rebuild 期间 access token 过期触发刷新（停机后典型形态）→ 轮换对写进进程 config.toml 的**卷域键** → 下次 boot `ensure_no_volume_keys_in_process` 拒启；卷文件里的旧 pair 与进程文件里的新 pair 分家。单卷模式不受影响（cwd config.toml 即卷配置，写回正确）。
- 修法：`build_driver` 穿卷 `spec.file_path`（对齐 `dispatch_unified_backend_volume` 的 `ConfigTokenStore::new(spec.file_path)` 形态，`:5769/5780`）+ state_dir 用卷家目录锚。
- 验证：主会话读四处源码复核成立（Default→"config.toml"、save 形态、两臂调用、多卷循环链路）；`7843-7845` 仓库自注释明证 save_toml 会铺卷域键。

## Medium（17）

| # | 域 | 位置 | 标题 | 修法方向 |
|---|---|---|---|---|
| M1 | core | cache.rs:243-285 + vfs.rs:632 | `evict_lru` 无 pending 感知，可删掉在途上传的唯一字节副本（`clear_except` 有 keep-set、evict 没有） | 把 pending 路径喂给淘汰作 keep-set（镜像 clear_except） |
| M2 | core | config.rs:1671-1676, 1709-1713 | `load_legacy_json` 解析错误绕过脱敏漏斗；且漏斗按键名触发，serde_json 类型错误不带字段名 → 凭据值可原样进 boot 错误与日志（R3） | 用 serde_json 错误的 line/column 掩码值段后再构造 Parse |
| M3 | core | readthrough.rs:393-399 + database.rs:629 | `stat_fresh` 兜底 stat 臂物化无 in-flight 豁免（read_dir 循环有、这里没有）→ 窄 TOCTOU 可把 pending 行翻成 is_uploaded=true | 物化前按同款 `row_in_flight` 复查 |
| M4 | webdav 驱动 | driver.rs:657-663 | rename 建父重试臂把传输/认证/5xx 全折叠 `NotFound`（消费面 vfs.rs:1421 按 NotFound 照搬本地行）→ 本地/远端分裂、文件从视图消失 | 重试臂走与首发同形的分类，仅「重试仍 409 缺父」保留 NotFound（父） |
| M5 | webdav 驱动 | client.rs:660-668 | get_range 200-回退按**绝对窗口终点**缓冲（`end` 非 `end-start`）→ 高偏移窗口整前缀进内存（OOM 面只关一半）+ O(n²) 网络流量 | 流式消费丢弃 `[0,start)` 只累积窗长 |
| M6 | sftp | driver.rs:520-529, 711-723 | stash rename lost-ACK 重放被判「旧对象消失」→ stash=None → 后续失败时旧版唯一副本困死 `.old`、final 永空（与已挂账 Drop-不复位同后果不同路径） | NotFound 臂先 `symlink_metadata(&old_remote)`——`.old` 名带 pid-seq 唯一可探测 |
| M7 | local | driver.rs:71-73, 587-592 | `has_reserved_char` 漏 `\`，`RelPath::join` 也不拒 → Linux 卷内 `a\b` 名可**见不可寻址**，违反「list 产出即可寻址」（K67/M1 声称已达成的 invariant） | `join_validated` 增补 `\`（复用 sftp `name_is_addressable` 谓词） |
| M8 | pan115 | upload.rs:622-655 | `close()` 对中途失败/未尝试的分片盲目 complete——pan123 的 K78/M8 reconcile 修复**未移植** | 移植 confirmed 门 + `close_reconciles_a_transfer_that_failed_midway` 钉测 |
| M9 | pan115 | download.rs:151-176, 272-331 | 读流空窗静默 `break`（零信号截断）+ 403 自愈臂 `pos += 窗长` 而非实收字节 → 跳字节 | 空/超窗 206 拒收（pan123 M6 形态）+ `pos += bytes.len()` |
| M10 | pan115 | upload.rs:228-311 | resume 臂对**任何** `upload_resume` 错误（含 770004 窗/传输错）都删本地会话记录 → 差集资产毁弃 + OSS 孤儿分片不 abort | 只在「会话确不可续」形态删记录（对齐 pan123 SessionGone-only） |
| M11 | pan115 | api.rs:400-406 + oss.rs:315-324 + upload.rs:570-577 | OSS 分片 PUT 继承共享 60s 总超时且 status-0 传输错不可重试 → ≳100GiB 文件在普通上行必死 | OSS 请求独立超时（pan123 `max(300s, MB×2s)` 形态）+ 传输错可重试 |
| M12 | pan123 | api.rs:273-299 | 真机 24010 配额错误仍未映射（rg 全仓零命中）→ 语义类缺失、无钉测 | errno 表增 24010 + 可行动文案 + errno_mapping 钉测 |
| M13 | baidu | driver.rs:493-509 | 陈旧 fs_id 句柄缓存 + 路径被外部复用 → `delete` 可删错对象（-9 纠偏腿覆盖不到此形态） | 删前 list 父目录按 fs_id+path 双核对 |
| M14 | cli | lib.rs:2071-2086 + control.rs:335-363 | panic 路径日志打印原始命令行——CREATE/UPDATE 载荷含凭据，直接击穿 M3/R3 日志不变量 | 载荷类命令 panic 摘要化（只记关键字+名） |
| M15 | cli | main.rs:1148-1181, 1344-1357 | boot 期卷 connect 失败中止**整次多卷 boot**（`process::exit(1)`）——与 K22「坏卷不伤兄弟」文档矛盾；af20d9f/70ad935 装配门收紧后触发面放大 | connect 容忍进装配环（Failed 行+继续）或钉成显式裁决并改文档 |
| M16 | webdav 服务面 | cloudkit-webdav/src/lib.rs:983-989 | StagedFile 固定 `.{name}.tmp` 暂存名——winfsp M1 已修的随机后缀形态**未移植**：编辑器临时文件形态的远端 PUT `/foo` 会截断 pending 行 `/.foo.tmp` 的唯一字节副本 | 移植随机化 sibling 生成器 + Drop/abort 清理 |
| M17 | cli | lib.rs 全文件 8704 行 | 单文件 ≥10 个可分离关注点（生命周期/命令面/rebuild/挂载/装配/探针/首启），镜像已列名的 fs.rs 拆分债 | 按 pub 面不变量拆子模块（工程债，可与任一修复批同车或单独裁决） |

## Low（39，一行一条）

**core（4）**：DirCache flights/generations 无界增长（挂账已知项，readthrough.rs:56-137）；persist_success 末块 size 可为负（receipt 谎报时，upload_queue.rs:1028-1036，按 upload_v2_stream 先例拒绝）；sweep_unseen 表级无走根前缀约束（database.rs:876，当前调用方恒传卷根、未来误用即越域删，加前缀谓词或 seam 断言）；hydrate 阻塞式 std 写在 async 上下文（vfs.rs:660-668，既有取舍，文档化即可）。
**storage/L2（9）**：Range 文档 `start>=end→Invalid` 与代码/测试（空窗合法）矛盾（vocab.rs:119,128）；conformance ②无 RANGE_READ 能力门（conformance.rs:96）；rename 断言只有 happy path——NotFound/Exists/后代三错误腿零断言（:397-473）；delete 他卷句柄/writer→目录/close 欠写/abort 孤儿等条款无断言（整文件）；token_bucket `rate=0` 除零 panic 路径（token_bucket.rs:94，构造期校验）；webdav stager 复刻 spool 写机制未委托 L2（已发生一次双地修复成本，委托 `spool_append_write`）；baidu 会话序列化失败 `unwrap_or_default()` 写空串毁会话文件（upload.rs:171，对齐兄弟「失败跳过」）；session_disk tmp rename 失败残file 无清扫（session_disk.rs:70-72）；RelPath serde transparent 绕过 `new` 校验（vocab.rs:22-24，摄入面候选=sync wire，custom Deserialize）。
**webdav 驱动（3）**：Digest 头构造无 quoted-string 转义（auth.rs:305-321）；quota used+available 无 checked_add（driver.rs:829-833）；垃圾 contentlength 静默投影 size 0（xml.rs:324 + driver.rs:340，解析失败留痕或单条目失败）。
**sftp+local（4）**：sftp File 级 io 错误绕过连接死信清单（driver.rs:601-641，过 `looks_like_connection_loss`）；ensure_parents create_dir 无竞态/lost-ACK 归一臂（:156-174）；local sweep 信任 `.old.meta` 内容可被引出卷根（服务化部署提权向量，sweep 侧 `RelPath::new` 校验，driver.rs:194-210）；local rename 大小写判定 `None==None` 当 case-only——双悬空链静默覆盖（driver.rs:411-417，要求两侧 canonicalize 均成功）。
**pan115/123（3）**：装配根探针不查 is_dir/file_category——根指向文件 id 时过门成半死卷（lib.rs:245-257 双驱动）；pan115 根级目录名碰撞预检只有缓存无冷 list（upload.rs:833-837）；fetch_window 零日志（真机盲诊，download.rs:217-329）。
**baidu（2）**：list 页内条目解析失败计入 page_len 可致静默提前终止（api.rs:176-189，page_len 用原始 len）；session_disk 写面 tokio::fs（经 L2 引入，良性降级但与修复哲学不自洽，注释或同批 std 化）。
**cli（2）**：setup --multi 骨架用裸 `fs::write` 非 `write_config_atomically`（setup.rs:855-857）；多卷 cwd 下 `cydrive sync` 报 telegram 形态误导错误（main.rs:475-478）。
**服务面（7）**：webdav `vfs_err` 不映射 `Transport(Exists)`——MKCOL BUG3 腿回 500 非 405（lib.rs:1051-1058）；webdav StagedFile 无 Drop 清理——孤儿 .tmp 累积（:863-881）；webdav accept 循环任何错误静默 break（server.rs:232-239）；webdav `webdav_host` 无非 loopback 降级/警告（对比 web 面 allow_remote_admin 先例，server.rs:150）；winfsp WindowReader 容忍短窗（违反 stream-M1 裁决，winfsp/reader.rs:124-143）；web 上传整体内存缓冲至 1900MB（lib.rs:1751-1761，Python parity 取舍）；sync-server `/v1/subscribe` 在 body-gate 与密钥检查之外——未认证大 body 有内存效应（router.rs:160-391）。
**架构（5）**：`mount_backend` 字段 rustdoc 描述已移除的 net-use 回退为现行行为（config.rs:1237-1245）；MountBackend 枚举 doc 新旧默认自相矛盾（:285-296）；cli lib.rs:522 注释残留 8080 端口；两测试注释「8080/8088 default」措辞陈旧；死枚举变体 `MountedBackend::WebDavFallback` 零构造（:6933-6939）。

## Info（41，归类摘要）

架构层（6）：cli Cargo.toml winfsp dep 注释与 features 表矛盾（K38 陈旧 vs K89）；architecture.md §4 crate 清单缺 4 驱动+winfsp；driver-onboarding §5.4「九位」应为十位；rust-rewrite-design.md 历史 8080/8088；Cargo.lock 双版本密码学依赖（pan115 0.10 线为 OSS V1 签名钉住，已知取舍）；web 面零 tracing 事件。
core（4）：read_dir_fresh 每条目 N+1 SQL（LIST_MAX 内界）；open_read 加密路径重复取 chunks；sweep 单观测删（无 per-row stat 双确认，D8③ 已裁决形态）；`VfsError::EncryptedInstance` 无生产者的防御臂（文档化决策）。
webdav 驱动（4）：416 复核臂 content_length None 静默判 EOF；绝对 URI 形态 multistatus href 不支持（RFC 允许，IIS 类服务器，桩加旋钮）；stash 后裸 Drop 无兜底（公共 trait 面文档化或孤儿扫描）；`http://`+Basic 预发无警告（内网明文 NAS 真实场景，装配 warn 即可）。
sftp+local（4）：sftp 指纹比对大小写折叠（量级 2^-220 不可利用，卫生项）；already_committed 0 字节可与外物吻合（极窄）；local 构造期清扫删活兄弟 .part（同根单写者假设无机械护栏）；FIFO/socket 按 File 报 kind、reader 无界挂起（sftp 侧有 30s 超时收敛）。
pan115/123（4）：目录 rename 文档与代码/真机形态漂移（update-only）；session_gone 按 "404" 子串匹配过宽；pan115 目录 size 报 fs 值（他驱动报 0）；OSS 203 当成功形态（callback 失败迟显）。
baidu（6）：refresh_token「一次一换」注释已被真机勘误推翻未同步；telegram TransportConfig derive Debug 携带 bot_token（未来 `{:?}` 即泄漏，BaiduParams 先例）；FLOOD_PREMIUM_WAIT 族新名不识别；open_range size 缺省 0 静默错位；多分片重试孤儿消息（Python 基线同形态）；（+已挂账确认 9 条见 baidu-local-review-findings.md，不重复计）。
cli（3）：首启 boot 忽略遗留 volumes/*.toml；tokio 运行时后 `std::env::set_var`（edition 2024 将 unsafe）；rebuilds mutex 中毒处理三处不一致。
storage（5）：spool 每写 open/close+全量拷贝（性能，量级可接受）；两驱动 unique_tag 逐字重复且无 pid；wait_one 无 FIFO 公平（设计取舍）；文档「list 不可见」断言①只测 stat；session_disk tokio::fs 注释钉住分析。
服务面（5）：percent-编码 {name} 可携带额外命令词（is_valid_volume_name 校验即可）；create/update 重复键 write_family_gates 阶梯三份拷贝；sync-server 非常数时间密钥比较+body 携带（已裁决家庭级）；sync sqlite 同步在 tokio worker（gate 限 2 并发）；静态资产无 Cache-Control。

## Verified good（各域主验结论汇总）

- **R1–R7 零违规**：依赖图与标准逐点吻合（composition root 之外零 ck_* 引用；core/storage 不识驱动；驱动互不依赖；check_layers 16 manifests 全过）。
- **K30/K89 特性门控零漂移**：7 驱动+winfsp 全 gated（96+20+10+12 处）、7 个 K31 常量齐、DRIVER_ROWS 表喂 --version、CI 裁剪矩阵在跑。
- **安全纵深完整**：web 面 Origin/Referer+Host 白名单（绑定后 SocketAddr 推导）+loopback 写降级+预算三层全验；K58 web 缝 spawn+oneshot+单 permit 未回归；控制通道 loopback-only+双 panic 包围；凭据脱敏漏斗三构造点覆盖 7 驱动全键（漏斗缺口见 M2）；D2 host-key 三态、UA/IPv4/Range 三约束、digest 状态机、XML 无实体展开均复核正确。
- **数据面核心数学正确**：闭式尺寸反推两方案逐边界精确（对照 v2 Layout 手推）；`upsert_materialized` 密码真相 SQL 级保留；rebuild 续跑协议（checkpoint/anchor/queue 次序+M3 地板）；reconcile 双确认 prune 三面豁免；sftp/local symlink 契约无下潜通道；pan123 七步链+本地五元组优先 resume；baidu H1 分页修复+成对方言+fs_id 去重防线；winfsp K45 大小写归一+FSD 三臂；Destination 头改写边界（段前缀匹配）。
- **上传队列终态账目与停机交错**：每 claimed job 恰一终态计数（panic 路径含）；outstanding 排空谓词成立；H2 幽灵类无残留路径。

## 处置建议（待负责人裁决）

1. **必修批（建议立即）**：H1（baidu 暂存 std 化——与 6fc56b9 同族数据破坏级）、H2（同路径改写守卫——field log 同族先例）、H3（rebuild token 落错文件——boot 拒启级）+ M2/M14（两条 R3 凭据日志缺口，小改）。
2. **择修批**：M1/M4-M16 按真机暴露面排序（pan115 四条自成一组：M8-M11 同 crate 同批最经济；webdav M4/M5 与 M16 同面；M15 需产品裁决 connect 容忍 vs fail-fast）。
3. **Low/Info**：随批顺手清偿 or 挂账（baidu 已挂账 9 条维持；storage 断言缺口批可单独做「conformance 补腿」小批）。
4. M17（lib.rs 拆分）为工程债，建议独立裁决排期。

---
## 补记：测试基线

- round1（9 子代理并发时）：rustc alloc_error（编译器 OOM）→ E0463 级联。
- round2（1 子代理并发）：rustc STATUS_STACK_BUFFER_OVERRUN（0xc0000409，hashbrown/interner 路径）→ 同级联。两轮均为**本地编译器资源崩溃**，非代码回归；CI windows/ubuntu 双平台在 20bedee、f26e025 全绿为反证。
- round3（`-j 2` 降并发重跑）：**exit 0 全绿**——round1/2 定谳为环境资源压力（K73 陷阱族），基线无恙。

## 必修批复核与修复（2026-09-25，worktree `fix/review-mustfix`，负责人令「确保一定是 BUG 才修复」）

**复核结论：五项全部定谳为真 BUG**（主会话逐环亲验机制链，非仅采信子代理）：

| # | 复核定谳关键证据（亲验） | 红测证据 | 修法 |
|---|---|---|---|
| H1 | tokio 1.53.1 `poll_write`（file.rs:741-770）spawn 阻塞写后**立即返回 Ready**；tokio File **无 Drop 实现**（drop 不等在途写）；baidu `upload_stream→stage_stream→drop→finish_upload→block_md5s`（新句柄零间隙读回）链路逐行读通 | WSL 钉测红：staging 返回瞬间盘上 **6094848/6291456（缺 192 KiB = 0.75 帧）**；Windows 绿（免疫平台，钉测可跑） | `stage_stream_to_disk` 写面 std::fs + spawn_blocking（L2 spool 同款）；错误文案逐字保留；WSL 红→绿 |
| H2 | `put_staged→commit_put` 新 pending 行+enqueue（vfs.rs:563-572）可在 job1 在途时发生；`persist_success` 用 :624 快照整行回写（:1011-1027）+ `delete_local_copy` 无守卫直删（:1045-1055）；唯一守卫只盖 size==0 | 门控传输确定性红：**Some(1) ≠ Some(2)**（旧任务快照回写抢行、超车任务因本地副本被删而降级） | Ok 臂前置超车守卫：重读行，size/mtime 与快照不符→pending 形不回写不删本地（计 degraded）、已上传形不回写（计 succeeded）；行中途消失→不复活（upsert 复活已删行同批堵住） |
| H3 | 链 `main.rs:793→run_rebuild_multi→run_rebuild_command:4932→build_driver→ConfigTokenStore::default()`（"config.toml"）；save_tokens 整结构回写；baidu_access_token 在 VOLUME_SCOPED_KEYS（:385）；`ensure_no_volume_keys_in_process` 返 Err 拒启（:1093）；**live 执行器（lib.rs:2004 经 RuntimeRebuild）同样走 build_driver——live REBUILD 轮换同病** | 桩红：store 路径恒 "config.toml" ≠ 卷文件 | `rebuild_token_store(Option<&Path>)`；`RuntimeRebuild` 类型与 with_limits/build_driver 穿参；run_rebuild_multi 与 rebuild_volume（RebuildTask.secrets）两路都传 `spec.file_path`；单卷语义不变（None→cwd config.toml） |
| M2 | `load_legacy_json` 两处 Parse 直出 `err.to_string()`（:1671/:1709）不过漏斗；漏斗按键名触发（:566）而 serde_json 类型错误只带值不带键名 | 红：错误消息原文 `invalid type: integer \`123456789012345\`, expected a string`——**值裸奔且无键名**（双缺口一次坐实） | 凭据键值非 string/null 预扫描拒绝（消息只报键名）；两处 Parse 构造均过 `redact_credential_values`（单一漏斗规则） |
| M14 | lib.rs:2079 与 control.rs:352 两处 panic 日志打 `command = %line` 原始命令行；CREATE/UPDATE 载荷「MAY carry credentials」（:3090 契约自认）——K58-H4 本应消灭的落盘点 | 桩红：透传泄漏 SUPERSECRET | `loggable_command_line`：CREATE/UPDATE 裁第三段载荷只留关键字+卷名；两 catch 位接线；无载荷命令透传 |

**验证**：五红（四本地 + H1 WSL）→ 五绿（本地目标测试 + 触碰面回归 upload_queue 30/config 40/cli lib 13/runtime_rebuild 10/dispatch 23 全绿 + **WSL H1 红→绿**）+ 三触碰 crate clippy `-D warnings` 绿 + fmt 绿。worktree 全量：**192 套件 / 1876 通过 / 0 失败**（`-j 2`；默认并发下 rustc 崩溃一次——与基线 round1/2 同形态的环境问题，K73 解法降并发后全绿）。

**批次链**：`bce6cfd`（五红留证）→ `e13b57b`（五修复全绿）。

## Medium 批复核与修复（2026-09-25，worktree `fix/review-medium`，负责人令「必修 7 条再次复核确认真 BUG 后逐一修复；其余零破坏低成本顺手修」）

**复核结论：必修 7 条全部再次亲核源码定谳为真 BUG**（复核记录见批次链 commit message；M5 的资源缺陷为输出等价型——消费面逐字相同、缺陷纯在内存峰位，红走 seam 级）：

| # | 复核定谳（机制链） | 红证据 | 修复 |
|---|---|---|---|
| M4 | 重试臂 `_ => Err(NotFound)` 折叠一切（driver.rs:657-663 旧）——传输/认证/5xx 与「父仍在的 403」全部变 NotFound | 桩红×2：503 落重试 → 得 `not found`（应 Unavailable）；父在 403 → 得 `not found`（应 Unauthorized） | 重试臂与首发同形分类：ParentSuspect 重 stat 父（缺→NotFound(父)/在→通用表/文件占→Exists）、Err 原样透传；桩新增 `transient_5xx_move_skip` + `false_403_move_with_parent` 两注入面 |
| M5 | 200-回退 `read_capped(end)` 按绝对窗口终点缓冲（client.rs:660）——高偏移窗口整前缀进内存 | seam 红（函数缺失编译红）+ 高偏移输出等价护栏 | `read_200_window`：流式按块丢弃 `[0,start)` 前缀、只累积窗长；覆盖不足 Io（消息保留实收字节数——既有钉测零漂移）；reqwest 增 `stream` 特性（wrap_stream 合成块流面） |
| M7 | `RelPath::new` 拒 `\` 而 `join` 不拒 + `has_reserved_char` 漏 `\` → Linux 卷 `a\b` 名可见不可寻址 | 白盒红：`join_validated(root,"a\b")` 得 Some | 过滤表增 `\`（sftp `name_is_addressable` 同源）；注释纠偏（旧注释误称词汇层已拒故可不列） |
| M8 | write 中途失败后 transfer=Some（部分片）→ close 跳过 run_transfer 盲 complete 缺片 → 远端**截断可见对象**落地 + 会话删除（size 复核在残骸之后才拒） | 红实测：close 得 `Unavailable("upload size mismatch: local 12582912 vs remote 5242880")` + part_put_count=1 残骸 | `transfer_incomplete()` 门（rapid=完整/multipart 看片覆盖/单片看 put_done）→ close 先补差集（幂等）再提交 |
| M9 | `get_window` 无 body 长度校验：空体 → 读流静默 `break`（零信号截断）；越窗多给透传；403 自愈臂 `pos += 窗长` 而非实收（短给即跳字节） | 红×3：空体得 0 字节 Ok 流；越窗得整 body 数据帧；自愈短给实测缺 4MiB−1 字节（left 4195539 / right 8389842） | get_window 增两防线（pan123 M6 同款：越窗拒/空体拒/少给非空保留）+ 删静默 break + 自愈臂 `pos += bytes.len()`；桩新增空体/越窗/later-403/later-短体注入面（短体预算只在真正出 206 体时消耗——403 短路不吃） |
| M12 | classify 表无 24010 → 落 Rejected 泛化 Unavailable（真机 2026-09-24 撞过：多空间配额形态） | 编译红（ErrKind::QuotaExceeded 不存在）+ 两条钉测 | `24010 → QuotaExceeded → Unavailable` 带行动指引（多空间配额独立，换目录/清容量，重试无意义） |
| M16 | `staged_sibling` 固定 `.{name}.tmp` 是合法虚拟路径——远端行 `/.foo.tmp` 缓存副本恰在该处时，编辑器临时文件形态 PUT `/foo` truncate 掉 pending 行唯一副本；且 StagedFile 无 Drop 清理（Low 同面） | 红×2：两个并发写手只得 1 个暂存件（后者 truncate 前者）；不 flush 丢弃留孤儿 .tmp | 移植 winfsp M1 随机段 `.{name}.{rand8}.tmp` + `committed` 标记 + Drop 清理；winfsp writer 分歧注释同步收敛 |

**顺手修批（零破坏低成本，负责人令「其他的如是零破坏低成本顺手修」）**：

| # | 定谳 | 红 | 修复 | 破坏面 |
|---|---|---|---|---|
| M1 | evict_lru 无 keep-set——LRU 淘汰可删 pending 上传唯一副本（clear_except 有保护、淘汰没有） | seam 红（evict_lru_except 缺失）+ 旧路径无 keep-set 源码实证；两条保护钉测 | `evict_lru_except(keep)`（evict_lru 委托空 keep）+ hydrate 喂 `pending_file_paths` | 零（只多保护；keep 条目仍计容量预算） |
| M6 | stash rename lost-ACK 重放（首 rename 已生效、重放如实撞源不在）被判「旧对象消失」→ stash=None → 失败不复位 → 旧版困死 .old、final 永空 | 红实测：abort 后 reader NotFound（旧版 3000 字节困死） | NotFound 臂 lstat 探测 `.old` 唯一名（pid-seq 段——只可能是我们的 rename 造的）；桩新增 apply-then-NoSuchFile 注入面 | 零（仅 NotFound 角落多一次探测） |
| M10 | resume 臂**任何**错误都删本地会话记录 → 瞬态错（传输/限流/5xx）也毁差集资产 + 回退全量 init | 红实测：瞬态错后 write 返回 Ok（全量重传 3 片）而非上抛保会话 | 对齐 pan123 SessionGone-only：瞬态错上抛保留记录；仅 object 换代（确不可续）删记录 | 零（原「回退 init」形态仅剩确不可续分支；瞬态错由上层重试纪律兜底） |
| M11（超时半边） | OSS 数据面 PUT 继承共享 60s 总超时——大分片普通上行必死 | seam 红（data_timeout 缺失）+ 四边界钉测 | `max(300s, MiB×2s)` 每请求超时（典型 5MiB 分片命中 300s 基线）；控制面动词不受影响 | 零（纯放宽）。**retryable 半边维持 K75.4 裁决不动**（status=0 传输错不标可重试——M10 保会话后重试语义已自洽） |
| M13 | 陈旧 fs_id 缓存 × 路径被外部复用 → delete 按路径删**错对象**且「成功」（-9 纠偏只盖路径已空形态） | 红实测：外部搬 a→b + 新对象落 a 原位 → 旧实现删掉 a 原位新对象、真身 b 幸存 | 缓存命中先列父目录「路径+fs_id」双核对，不过 → 失效+扫描真路径 | 两处 warm 零流量钉测同步为「恰一次核对 list」（**语义变更非漂移**：删错对象不可逆 > 零流量钉——本行为裁决记录） |

**不动/放弃**：M15（boot 全有全无 vs 容忍 Failed 行）维持待产品裁决；M3（极窄 TOCTOU 自愈）与 M17（lib.rs 拆分工程债）按「没有价值的放弃」销账。

**新观察（本批揭出 → 同日查证后撤销）**：批内曾记「webdav/pan115/pan123/baidu 的 list 未过滤远端 `\` 名（M7 同型暴露面）」——**经逐驱动源码查证该记录不实，撤销**：webdav（xml.rs `is_addressable_name`，list/stat 双面过滤）/ pan115（`entry_from_row` `name_is_addressable` + debug 日志）/ pan123（同款 + warn 日志）三驱动**均有 K67 血统的显式守卫**（分别来自 Phase 7 WD / K73 / K78 批）；baidu 无显式守卫但 `rel_from_abs` → `RelPath::new` 拒 `\` → list/stat 的 filter_map 天然跳过（**不可见**而非「可见不可寻址」，与 K67「不可寻址即不可见」纪律殊途同归）；telegram 窄面无 list。**全后端无 M7 同型暴露面**。残余化妆级差异（不构成缺陷，随手项）：baidu/webdav 的跳过无日志（pan115 debug / pan123 warn）；四驱动可寻址谓词各持一份拷贝（未来可与 L2 共享件合并去重——挂 pending 的 `feat/l2-shared-helpers` 若合入时顺手）。教训已记：K90 原句系凭审查期印象下笔、未先查证——与「承重结论必须亲验」纪律相悖，本条即更正。

**验证**：每项红→绿独立留证（见上表红证据列 + 各 commit）；触碰面回归全绿（ck-webdav 63+28+29+48、cloudkit-webdav 全、ck-pan115 全、ck-local 全、ck-pan123 全、ck-sftp 全、ck-baidu 全、cloudkit-core cache 12）；worktree 全量 `-j 2` 见批次收尾记录。

**批次链**：`8f9f826`（webdav M4+M5+M16）→ `9a9bd94`（pan115 M8+M9+M10+M11）→ `2843bb7`（local M7 + pan123 M12）→ `70074ab`（core M1 + sftp M6 + baidu M13）。
