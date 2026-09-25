# ck-baidu / ck-local 深度审查 findings（2026-09-25）

> 背景：负责人指令「对未进行深度审查的存储驱动逐一审查」。盘点结论：sftp（K67）、
> pan115（K73-75）、pan123（K78-79）、webdav（Phase 7 批）有专属审查档案；telegram 有
> rs-CyDrive 血统审查 + review-fixes.md 工程债档案 + Phase 8 密文面覆盖——**唯 ck-baidu
> 与 ck-local 从无驱动级深度审查**，本批补齐。
>
> 方法：三子代理分域精读 ck-baidu（读路径与缓存 / 写路径 / 协议装配测试面，共 7583 行
> 全量）+ 主会话亲审 ck-local（1581 行全量）；**全部 H/M 级主张由主会话拿 file:line
> 复核确认**后入档。只读审查，未改代码。

## ck-baidu（0H* / 4M / 12L / 8I；*H1 代码事实确证、后端行为待真机定谳）

### H1 list 无分页参数——超默认页大小目录静默截断，沿五链路传播 【待真机定谳】

- 代码事实（已亲证）：`api.rs:145-147` query 恒仅 `method=list&dir=<abs>`（无 `num`/`start`）；
  `driver.rs:389-399` 的 offset 游标只对**单次后端响应**切片。模块文档自认「后端无分页参数
  ——注源」，但未记录**截断风险**。
- 假设与不确定度：百度 xpan 公开文档 `num` 默认 1000；若后端按默认页截断，`Listing.next`
  恒 None（total=收到条数），上层认为列举完整——**静默丢数据**。spike/附录 A 均未测条目
  数上界；PCFS 同形（先例非证明）。
- 传播面：① driver.list 丢条目；② scan_dir 冷句柄解析页外 NotFound；③ delete_tree/
  collect_subtree 递归不完整；④ **read-through 物化**（authoritative_index=true，截断固化
  进本地索引）；⑤ rebuild 同源。
- 处置建议：真机 >1000 条目目录探针定谳；若截断 → `api::list` 加 `num=1000&start=<off>`
  循环拉齐 + mock 同步建模页上限（否则「桩照实现抄」）。

### M 级（4）

| # | 断言 | 证据 | 影响 | 建议钉测 |
|---|---|---|---|---|
| M1 | doctor 探针 `token_store=None`——探针期 110 触发的刷新轮换不落盘，**烧毁唯一活 refresh_token**（一次一换、旧值即刻作废）；run/rebuild 路径均传 Some，唯探针漏 | cli/lib.rs:6379（None）对照 5444/5755/6433（Some） | doctor 恰在 token 疑似过期时运行 → 探活成功但下次启动 NeedsReauth，被迫重走 setup | probe 测试用 rotate=true mock + 临时 config 断言新 token 对落盘 |
| M2 | rename 成功后句柄缓存不失效、reader 无陈旧纠偏腿（delete 有纠偏腿+钉测，reader 无）——持句柄读在 rename 后 NotFound，窗口可达进程寿命 | driver.rs:490-537（无 invalidate；invalidate 仅 478/486=delete 面）；265-270 缓存命中直返；download.rs:117 陈旧 path 签发 dlink | read-through 物化行/上层 Entry 持旧句柄 → rename 后读失败无自愈；与模块头「句柄跨 rename 稳定（K5）」漂移 | 桩测：warm 缓存 → rename → reader(旧 fs_id) 读通（现况红） |
| M3 | DlinkCache 无容量上界、无主动清扫（TTL 仅同键 get 惰性剔除）——pan115 K73 M-S5 同型（彼已修 TTL+1024 LRU） | download.rs:47-81（双路独立发现） | 长驻挂载进程大目录工作集无界内存增长 | 容量+驱逐白盒断言 |
| M4 | transport 上传面全量驻内存：`upload`/`upload_stream` 均 `tokio::fs::read` 整文件入 Bytes；与模块头「保内存有界」注释漂移；上游 v2 流式加密的有界设计被打破 | transport_face.rs:187-190/207 | 大文件内存峰值=文件大小×workers（默认2），OOM/页面文件风险 | 改分片读盘源；至少先修注释漂移 |

### L 级（12）

1. open_writer 预检 `if let Ok` 吞所有 list 错误（应只放行 NotFound）——upload.rs:289-296
2. `baidu_root="/"` 卷 stat(卷根) 恒 NotFound（parent_abs("/")="/" 永不匹配）——driver.rs:413-419
3. CDN 下载流无读超时（stream client 仅 connect_timeout 20s；api client 有 60s）——client.rs:112-119；停滞连接永久挂起，superfile2 同暴露
4. rename 的 filemanager move 恒 `async=1` 受理语义不轮询——完成性窗口未文档化——api.rs:266/248
5. 上传收尾 lookup_entry 依赖「list 即时可见」，索引延迟窗内已落盘却报 NotFound——upload.rs:652-654/737-742
6. 目录 rename 补搬循环中途失败留半搬移残局（重试入口失效）——driver.rs:515-534
7. stager 内存缓冲无上界（「本地 staging」实为纯内存 Vec）——upload.rs:594-595；**生产无消费者**（全仓 rg 验证，生产走 transport 面），暴露面=接口契约
8. `UploadStager` trait 文档与 baidu abort 保留会话的实现冲突（abort 实选 Drop 语义且有钉测）——stager.rs:39 vs upload.rs:659-664
9. precreate 缺 uploadid 静默装备空串——api.rs:346-350 + upload.rs:364
10. create lost-ACK 重放窗：全 done 会话无探活即重发 create（假失败风险，rs-CyDrive PROPPATCH 同型）——upload.rs:551-561/643-653
11. cdn_get 非 206 错误片段不掩码 access_token（与三处掩码防线不一致）——download.rs:220-231
12. 杂项：dlink TTL 注释双标（lib.rs ≥96min vs download.rs ≥56min 陈旧）/ resume 位注释挂已销账裁决 / superfile2 无 31034 重试不对称未声明 / list 响应畸形静默（filter_map 丢条目、缺 list 键=空目录、quota 缺字段取 0）

### I 级（8）

K7 会话表无 GC（abort 保留 by design 但无超龄回收，磁盘+内存）；flush_streaming 差集循环无 EMPTY_MD5 过滤（恰巧安全，依赖隐式不变量）；并发双 writer 同 (path,size) 会话位图互踩（差集退化重传，不丢数据）；cdn_get 不校验 Content-Range 起点；探针 Unauthorized{true} 桶入 Unreachable；mask() 短值露 10 字符（真实 token ≥32 实险极低）；api::list 解析失败条目静默丢弃无观测；不可解析句柄两面错误分类不一致（Invalid vs NotFound，各声明有据）。

### 测试盲区（「桩照实现抄」例外清单）

mock 未建模：① list 页大小上限（H1 本体）；② 上传/下载中途 110 轮换（mock 有能力零用例）；③ CDN 5xx/传输中断（fallback 只认 403）；④ 特殊字符文件名 wire 往返（decode_component 有实现零喂过）；⑤ rename 目录腿后端单调用递归形态（-9 容错臂从未执行）；⑥ filemanager 异 kind overwrite；⑦ errno=2/10/31300/31023 已知码无钉测。无覆盖路径：upload_whole_file 探活死亡/并发失败臂、stage_stream 异常臂、HandleCache 超容全清、非 JSON 掩码分支、doctor 探针轮换/超时/111 腿。

### 核验为可靠（正面清单，摘）

CDN 三约束全落实（UA/≤4MiB/禁全量 GET，五钉测）；dlink TTL+两段 fallback 与文档一致；delete 纠偏腿真实有钉测；HandleCache 4096 有界锁纪律正确；mkdir/ensure_parents list 预检防 errno=0 ghost；路径穿越防线闭合（词汇层+abs_of+回显过滤三重）；R3 三防线完备（without_url/掩码/不实现 Debug）；110 单飞双检+on-arrival 持久化；errno 映射表逐码一致；fs_id 句柄双向一致（materialize 链路同构）；**K85.7 三构造点+免传三处、K86-L1 守卫全部在位**；到齐即传（31363）在位；rtype=3 两处；秒传腿零绕行；K7 差集续传原子落盘；能力位十位诚实（rapid_upload 合法性依据 capability.rs 明文）；装配面 K30/K31/K32/M-I2 全过；mock 建模质量总体优良。

## ck-local（0H / 3M / 2L / 1I；主会话亲审 1581 行全量）

| # | 断言 | 证据 | 影响 | 建议钉测 |
|---|---|---|---|---|
| M1 | symlink 全程跟随、无 lstat 面——stat/list/delete/reader 全用 `fs::metadata`（跟随）：①卷内 symlink 指向卷外 → reader/stat 越根读（containment 破坏，与全队「路径只能落卷内」模型不一致；sftp K67 已立 lstat 先例）；②symlink-to-dir 被 list 报 Dir、delete 走 remove_dir_all（Unix ENOTDIR → 删除失效） | driver.rs:103-115（entry_from_meta）/299（delete 判 dir）/345（reader） | 越根读不超出「对根有直接 FS 访问权者」既有能力（VFS 无 symlink 操作面，植入需直接 FS 访问）→ 定 M（语义不一致+删除失效）非 H（安全） | sftp 同款修法：stat/list/delete 改 symlink_metadata 报本体形态，reader 保持跟随；conformance 补 symlink 断言 |
| M2 | Windows 大小写改名撞 Exists 预检：`metadata(to).is_ok()` 在大小写不敏感 FS 上命中**源自身** → `/Mixed.TXT`→`/mixed.txt` 恒 Exists | driver.rs:325-327 | 挂载面（winfsp 大小写臂/webdav adapter）对本地卷做大小写改名必败（Exists→Unavailable→EIO/5xx）；BUG2 修复前是假成功（更糟），现显式失败仍需修 | Windows 钉测：case-only rename 走通（dest 命中且与源同文件时放行，FS rename 翻拼写） |
| M3 | stash 崩窗孤儿无恢复：进程在 writer 打开（旧版已搬 `.old`）后死亡 → 最终路径缺失 + 旧版滞留暂存区；`LocalDriver::new`/factory 只 create_dir_all，**无启动清扫** | driver.rs:437-452（stash 搬走）+ lib.rs factory（无清扫） | authoritative_index 卷上该文件从卷视图消失（行被 reconcile 剪除），旧字节只在 `.cklocal-staging/`，需手工恢复 | factory 时清扫：`.old` 且最终路径缺 → 恢复；`.part` → 删（预置 stale 文件钉测） |
| L1 | transport `upload` 整文件面 `tokio::fs::read` 全量入内存（upload_stream 面已是流式）——与 baidu M4 同型 | transport_face.rs:173 | legacy upload 面大文件内存尖峰 | 核对队列取面；同 baidu M4 处置 |
| L2 | list 全量载入后内存分页——大目录每页 O(n) 枚举；契约允许（排序稳定性所需），性能观察项 | driver.rs:227-263 | — | 挂账不修 |
| I1 | Windows 保留名（CON/PRN/AUX/NUL/COM1-9/LPT1-9）未过滤——创建时报 io 错而非可行动 Invalid | driver.rs:69-71（只滤字符） | 可行动性 | has_reserved_name 追加 |

### 核验为可靠（ck-local 正面清单）

路径拼接防穿越闭合（组件级校验+保留字符硬化+join_validated 产出即可寻址）；staging 保留名过滤；K6 句柄往返 + transport path 寻址闭环；rename 覆盖语义预检（OS 原子覆盖与 trait Exists 契约的正确调和）；reader 提前 EOF 报错（并发截断防御）；stager close 的 flush→size 承诺→原子 rename 序 + abort/Drop 幂等收尾 + finished 旗标；upload_stream 超计划拒绝；能力位十位逐位 pin；K11 单 chunk 占位 receipt。

## 处置状态

- 本批**只审查不修复**（负责人裁决必修项后开修复批）。
- 优先级建议：H1 真机探针（数据完整性承重）> M1/M2/M3（baidu，各有钉测形态）> local M1/M2/M3 > 其余 L/I。
- telegram 免审理由：rs-CyDrive 血统多轮审查 + review-fixes.md 工程债档案（H3+Low×5+K53 清偿）+ Phase 8 D10/K84 密文面审查覆盖，非无审查。

## 修复批（2026-09-25，fix/baidu-review worktree，负责人指令「按建议执行」）

### H1 真机定谳 + 修复（list 分页截断）

**三轮真机探针定谳**（生产卷文件 token，测试目录 `_probe_pag*` 自建自清、residual 全 0）：

1. 无参 `method=list` **默认页 1000 静默截断**（1010 播种裸查恰 1000）——五链路传播面坐实（list/scan_dir/删除递归/read-through 物化/rebuild）。
2. 分页方言：`start`+`limit` **成对**才窗口生效（`start=500&limit=600` → 恰 484 条窗口、first=f0517）；`limit` 单独出现、或与 `order`/`web` 搭档时**被后端忽略**（恒回默认页）。修复只发成对 start/limit、不发 order。
3. **修复后组合验证**（一次性实例 `cydrive rebuild` 真机）：1010 播种（p1=1000+p2=10）→ `rebuild complete: 1010 file row(s)` + stats=1010（修复前必 1000）。

修复：`api::list` 逐页拉齐（短页即止 + fs_id 去重防线跳出伪分页死循环）；mock 按真形建模页上限（`LIST_PAGE_DEFAULT=1000` + 成对窗口）；新钉测 ×2（1005 全量可见 / 分页方言 wire 钉）；两处金身 query 钉测同步（list/stat 增 start/limit 对）。驱动内 offset 游标不变（现在切在全量上）。

### M1 doctor 探针烧 refresh_token（修）

`baidu_backend_probe_with` 增第三参 `token_store: Arc<dyn TokenStore>`（生产包装传 `ConfigTokenStore::default()`——与探针 cfg 同源的 cwd config.toml）；测试：轮换 mock + 临时 config 断言新 token 对落盘（红=两参签名编译失败）。**同批发现并修复**：quota API 在本 appkey 下恒 `error_code=3 "Unsupported open api"`（2026-09-25 真机实证）——探针的 quota 腿删除（list 已足证 token 活性，且此前该腿会让 doctor 误报 Unreachable）。

### M2 rename 后句柄缓存（修）

- rename 成功后 `HandleCache::move_subtree` **改写**缓存子树路径（文件腿自身/目录腿自身+全部后代；「改写」优于「失效」——持旧 fs_id 读零重扫流量，钉测断言 list 计数不变）。
- reader 补 **-9 纠偏腿**（镜像 delete 腿：失效+重扫+重试恰一次）——外部搬移（绕过驱动的竞态窗）同款自愈。
- mock 增 `rename_path` 旋钮（外部搬移场景桩面）；红 ×3（文件腿/目录腿/纠偏腿全 NotFound）→ 绿 15/15；外部搬移的 delete 纠偏腿补独立钉测（原测试走 driver.rename——缓存改写后不再撞 -9，纠偏语义由新腿保钉）。

### M3 DlinkCache 无上界（修）

`DLINK_CACHE_CAP=1024`（HandleCache 同款取舍：超容先清过期、仍满全清——条目廉价可重取）；单测钉 len ≤ cap（红：无界）。

### M4 transport 上传面全量驻内存（修）

`upload_whole_file` 的 `data: Bytes` 换 `PartFile`（path+size）：md5 预计算流式逐 CHUNK 窗口、分片上传按需开句柄读盘（每次独立句柄，无跨任务锁）；transport 两臂不再 `fs::read` 整文件——`upload` 走 metadata 计划校验，`upload_stream` 的 staging 落盘后**不再读回**（清理卫士 `StreamStagingGuard` 持有到上传结束）。任意大文件内存峰值 = CHUNK × workers。PartFile 单测（窗口数学 + 流式 md5 与全缓冲逐块等值）；既有三分片 partseq 齐集钉测承重行为面。

### 探针期新事实（勘误/挂账）

- **refresh_token 可重用**：2026-09-25 实测旧 RT 再次刷新成功（`has_access/has_refresh` 双真）——AGENTS「一次一换、旧值即刻作废（spike §1 实证）」记录**勘误**（保守落盘行为维持不变，无行为影响）。
- quota API `error_code=3`（本 appkey 无该 API 权限）——已随 M1 从探针移除；`doctor` 的 quota 展示面（若有）挂账复查。
- 探针过程教训（不入库的脚本纪律）：bash 变量含 `--noproxy *` 展开吞文件表；`-X POST -G` 混用把表单体搬进 query；`set -u` 下脚本引用外部变量。均已即时修正，零残留。

### 验证

worktree `fix/baidu-review`：ck-baidu + cloudkit-cli **324/0**（mock 级全绿）；clippy -D warnings 绿；fmt 绿；真机三轮探针 + 修复后 rebuild 组合验证 PASS；远端 `_probe_*` 目录全清（residual=0）；生产卷文件 token 未轮换未修改（活性全程可用）。

## ck-local 修复批（2026-09-25 深夜，main 直做，负责人指令「开工」）

### M1 symlink lstat 面（修；红证 Linux/WSL ×3）

- `stat`/`mkdir` 预检/`delete`/`rename` 预检全改 `symlink_metadata`（本体形态，
  sftp K67 先例）；`reader` **保持跟随**（读链接指向的内容——显式注释 + 特性化钉子）；
  `writer` stash 判定保持跟随（目录性按真实目标）。list 靠 `DirEntry::metadata`
  的 std lstat 语义天然本体——特性化钉子防改回。
- **红证（WSL Ubuntu-24.04）**：断链 stat NotFound（修复前）→ 本体 File；活链
  stat 报目标内容长 3 → 本体目标串长；断链 delete NotFound → 可删。
- **实证勘误（审查断言②推翻）**：现行 std `remove_dir_all` 对 symlink-to-dir
  已是 O_NOFOLLOW 安全、只删链（WSL rustc 探针：Ok + 链消失 + 目标完好）——
  审查报告的「Unix ENOTDIR 删除失效」不成立于现行工具链；目录链删除腿留特性化
  钉子。本体分支仍修 Windows junction 形态（remove_file 拒绝 reparse point →
  remove_dir 回落）。
- 教训：审查报告的「平台语义」类断言必须现行工具链实证后再修——本次若盲修
  ENOTDIR 会写一条永假的注释。

### M2 Windows 大小写改名（修；红证 Windows ×1）

`rename` 目标预检命中时：canonicalize(from)==canonicalize(to) 且名字非逐字相同
→ 放行让 `fs::rename` 翻拼写（winfsp C1 同款判据下沉到驱动面）。Unix 防过宽
钉测：两个 case 变体是不同文件 → 照常 Exists 且目标不动。

### M3 staging 崩窗孤儿启动清扫（修；红证 Windows ×2）

- `writer` stash 前先落 **sidecar**（`.old.meta` = 卷内相对路径）——meta 写失败
  即整体失败（绝不留无 sidecar 的 .old）；stager 三路收尾（close/abort/Drop）
  sidecar 与 `.old` 同生共死。
- `LocalDriver::new` 构造期 `sweep_staging`（名单制：只碰
  `.part`/`.probe`/`.old`/`.old.meta`）：最终路径在 → 清陈旧对；缺 → 恢复旧版本
  （父目录防御重建、恢复失败留待下次重试）；无 sidecar 的 `.old` 当垃圾清；
  前提声明 = 同根单写者（驱动全程无锁的既有假设）。

### 验证

- Windows：ck-local **18/0**（sweep 白盒 2 + M2 腿 1 + 既有 15）；clippy
  -D warnings 绿（too_many_arguments 以 `stashed: Option<(PathBuf, PathBuf)>`
  打包消解）、fmt 绿。
- Linux（WSL Ubuntu-24.04 原生克隆）：**24/0**（local_edge_cases 7/7 含全部
  unix 腿）；红→绿全程双平台留证。
- workspace **1713/0/62**（+3）；check_layers / scan_secrets 绿。
