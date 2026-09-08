# Phase 2 执行计划（首批驱动三件套 + E2E 硬验收，含 Kickoff 指令）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.
> 性质：实施计划（documentation.md 两档制的可执行档）；权威设计 = `2026-09-07-cloudfusion-foundation.md` v1.2（§4 D1–D10、§5 Phase 2、§7a 凭据策略）；验收依据 = `docs/standards/driver-onboarding.md` v1.0；协议情报 = `docs/reports/2026-09-07-baidu-spike.md`（含附录 B）+ `docs/plans/2026-09-06-multicloud.md` 附录 A（PCFS 文件:行号）。冲突处以 foundation v1.2 与 standards/ 为准并记 decisions。

**任务范围（负责人 2026-09-08 指令）**：ck-local → ck-baidu（B1 骨架+OAuth / B2 上传下载 / B3 接线）→ 三后端 E2E 硬验收 → 收口。**Phase 2.5 多卷不在本计划**（B3 后另立计划，decisions 2026-09-08 方案一裁决）。全程自主、不中途确认；可逆细节自行裁决记 decisions，收尾集中报告。

> **执行跟踪**：`docs/tracking/phase2.md`——任务分解与状态表；**每批收口必须更新该表并随 commit 提交**；新会话开工第一件事读该表续跑。

---

## 0. 现状基线与总体路线

- 基线：main @ 0.8.x，win 625 passed / 0 failed（ignored 6 真机）；`cloudkit-storage`（L2）已交付 StorageDriver 九方法 trait + Capabilities 九位 + StorageError 分类学 + conformance 八断言套件（mock 已跑通）；运行时（Vfs/队列/WebDAV/同步）消费的是 **CloudTransport trait 家族**（telegram 时代接缝，同在 L2）。
- **双面驱动路线（本计划钉死）**：新驱动 crate 实现**两个面**——
  - **StorageDriver 面**（九方法）：过 conformance 套件，是能力位诚实声明（R4）与未来多卷时代的权威接缝；
  - **CloudTransport 面**（connect/upload/upload_stream/open/open_range/delete_remote + capabilities + as_inbound/as_chat 默认 None）：**v1 运行时接缝**，组合根装配 `Arc<dyn CloudTransport>`（driver-onboarding §5 工厂先例）。
  - 两面共享同一 api client 模块（薄壳互调，不重复协议逻辑）。**不建通用 StorageDriver→CloudTransport 适配器**——句柄寻址策略因后端而异（baidu=fs_id 数值、local=路径字符串），通用层需要句柄编解码抽象，只有两个消费者，YAGNI。
- 批次顺序：**Batch L（ck-local）→ B1（ck-baidu 骨架+鉴权+元数据面）→ B2（上传/下载+conformance 全绿）→ B3a（L2 transport 类型演进）→ B3b（组合根接线）→ Batch E2E（三后端硬验收）→ 收口**。每批独立验收判据 + 止损点，批间可停。

## 1. 关键设计裁决（计划钉死；执行期逐条入 decisions.md，有异议按「冲突报告」流程上报）

| # | 裁决 | 理由/依据 |
|---|---|---|
| K1 | **CloudTransport 句柄类型 i32→i64**（UploadReceipt.first_msg_id/chunk_msg_ids、RemoteHandle 同字段、delete_remote 参数） | db 记录与 sync payload **已是 i64**（database.rs:69/98/131、sync.rs PayloadChunk.msg_id）；百度 fs_id 实测 671337245231660（50 bit）超 i32。SQLite 列本就是 INTEGER（i64 容量），schema 零变化（R6 不触）；grammers msg_id 在 ck-telegram 边界 `i64::from` 收窄。波及面清单见 B3a |
| K2 | **RemoteHandle 增 `path: Option<RelPath>`**（additive 字段） | local 是路径寻址后端（无稳定数值句柄）；hydrate 构造点本就持有行的 rel_path（vfs.rs:516），填充零成本。telegram/mock 填 None 行为零变化（护栏测试钉死）。构造点波及：core vfs/inbound、mock、两新驱动 |
| K3 | **delete_remote 签名 `msg_id: i32` → `&RemoteHandle`** | 生产路径**零调用点**（rg 实证：仅 trait 声明 + telegram/mock 实现 + 测试），改签名波及面 = 2 实现 + mock + 测试；local 需按路径删除、baidu 需 fs_id + path 诊断，&RemoteHandle 一次给齐 |
| K4 | **Capabilities 增第 10 位 `remote_delete`**；Vfs::remove_file 与 webdav/web 目录删除路径按位门控接线远端删除 | baidu（权威索引）删除只删本地行会造成云端孤儿 + rebuild 复活——driver-onboarding §9 勿抄清单明令禁止。**顺序：先删远端（幂等重试）成功后再删本地行+缓存**；失败则中止并保留行。telegram/mock 声明 false → 行为零变化（北极星：不动已验证后端） |
| K5 | **baidu BackendHandle = fs_id 十进制字符串**；VolumeId = `baidu:<uid>`（uinfo 取） | fs_id 跨 rename 稳定（PCFS api.go:170-171 先例）；RemoteHandle.msg_id = fs_id 直存（K1），sync 跨实例天然一致（fs_id 是后端真相，非本地代理） |
| K6 | **local BackendHandle = 规范化 rel_path 字符串**；VolumeId = `local:<规范化绝对根路径>` | D6 原文：local 的身份即其根目录。RemoteHandle.path（K2）承载寻址，msg_id 填 0（不语义化） |
| K7 | **上传会话持久化在实例 cwd 状态目录**（`baidu_state/sessions/<hash>.json`：path/size/block_md5/uploadid/完成位图，**随分片完成即刻落盘**）；恢复 = 旧 uploadid 重发一个缺失分片探活，活则差集补传、死则整体重传 | spike §3 实证：重 precreate 不恢复会话；uploadid 会话跨进程分钟级存活。runtime 产物不入库（R7，与 .session 文件同惯例） |
| K8 | **dlink 按 fs_id 缓存，TTL = 60 分钟**；403/31326 两段 fallback：先同 URL 追加 access_token 重试一次 → 再重取 dlink | 附录 B：TTL 下界 ≥96min，60–90min 安全带取下沿保守值；两态 Location（直连/须追 token）spike §5 实证 |
| K9 | **下载器 = 4MiB 有界 Range 分片 + netdisk UA + 4 并发预取流**（单 dlink 复用）；单请求 Range 上限 4MiB 是硬约束（403 矩阵实证） | spike §5/§6 实测参数照抄；PCFS「每次 Seek 重取 dlink」与「全跨度单 Range」均不可抄 |
| K10 | **precreate rtype=3（覆盖语义）**；spike 挂账复核项——B2 真机 `#[ignore]` 用例 + E2E 验证；若 rtype=3 异常 → 回退 rtype=1 + 覆盖前预删（记 decisions） | spike §3.5：rtype=1 是冲突重命名（生成 `_2026…` 副本），WebDAV PUT 覆盖语义要求 rtype=3 |
| K11 | **`cydrive rebuild` 显式子命令**（权威后端 bootstrap，D4 `rebuild_from_backend`）；**明文-only 语义**：加密文件的远端容器无法从后端列举识别（后端只见密文字节+明文名），rebuild 对之产出错误行——**加密+多实例冷启动走 sync**（payload 携带 is_encrypted/scheme 完整行语义）；rebuild 遇 db 外文件且开启加密的实例 → 拒绝并给 sync 指引 | E2E 等价性检查（D10 ②）在明文集上执行；诚实声明能力边界优于产出垃圾行 |
| K12 | **sync namespace 后端感知**：telegram 派生函数**逐字节不变**（兼容钉死，护栏测试）；baidu = `baidu:<uid>`（uinfo）；local = `local:<hash(根路径)>` 且 **sync 任务不启动**（配了 sync_url 也仅 doctor 警告） | foundation A5 修订版 + D10 local 裁决 |
| K13 | **oauth 状态机**：errno 110 → 驱动内刷新+重放一次，仍败 → `Unauthorized{recoverable:true}`；111/-6 → `Unauthorized{recoverable:false}`（人话指引重授权，绝不死循环，§7a）；refresh_token **刷新响应到达即持久化**（旧值即刻作废）——经 CredentialStore 回调（env > config > keyring 链） | spike §1 + 附录 A errno 三档；interfaces §3 先例 |
| K14 | **appkey/secret 无代码默认值**（R3）：config 键 `baidu_app_key`/`baidu_app_secret` + env 覆盖 `CYDRIVE_BAIDU_APP_KEY/SECRET`；token 键 `baidu_access_token`/`baidu_refresh_token`（config 明文允许，sync_secret 先例）+ env `CYDRIVE_BAIDU_ACCESS_TOKEN/REFRESH_TOKEN` | PCFS 硬编码 appkey 是 R3 反面教材原址 |
| K15 | **31034/429 → `RateLimited{retry_after:None}` + client 层单点重试钩子**（一次指数退避重放），不做全链路限速器 | spike §2：当前 appkey 桶零拒绝形态；正式 appkey 复测后如需再议 |
| K16 | **mock baidu 端点 = axum 内存后端**（B1 建元数据面、B2 扩上传/下载面），三步曲表单**字节级断言**（黄金参照 PCFS api.go:488-573 参数表 + spike 实抓样本） | workspace 已有 axum（sync-server 测试先例），不引 wiremock；multicloud B2 TDD 原文要求 |
| K17 | 新配置键**三处同步**（KNOWN_TOML_KEYS + legacy json 拒收 + validate）：`backend`（缺省 telegram 完全兼容）、`baidu_root`（默认 `/apps/cloudfs`）、K14 四键、`local_root`（绝对路径必填） | interfaces §4；测试根裁决 decisions 2026-09-07 |
| K18 | baidu/local **直连**（no_proxy + 强制 IPv4 `local_address(0.0.0.0)`）；实例配了 proxy_url 时对这两后端无效并日志+doctor 声明 | multicloud A6 + spike 网络形态 |

---

## 2. Batch L：ck-local（conformance 第一公民，TDD）

**目标**：最简真实驱动，把 conformance 套件从「mock 跑通」升级为「真实驱动跑通」；纯新增 crate，零 core 触碰。

**Files**：
- Create: `crates/drivers/ck-local/Cargo.toml`（依赖仅 cloudkit-storage + tokio/futures；**禁 core 及 L3+**，check_layers 拦截）
- Create: `crates/drivers/ck-local/src/lib.rs`（导出 + `pub async fn factory(cfg: &LocalParams) -> Result<Arc<LocalDriver>>`，工厂形态按 driver-onboarding §5）
- Create: `crates/drivers/ck-local/src/driver.rs`（StorageDriver 九方法，std::fs/tokio::fs 实现）
- Create: `crates/drivers/ck-local/src/stager.rs`（UploadStager：后端根下 `.cklocal-staging/` 临时文件，close = 原子 rename 到最终路径——commit-on-close 不可见性天然成立；drop 未 close = abort 清理）
- Test: `crates/drivers/ck-local/tests/conformance.rs`（`conformance_suite!` 宏接入 + `ConformanceHarness` 实现：chunk_size=1、delete_missing=true、empty_range=true、error_table 空——local 无后端错误码，interfaces §6 允许）
- Modify: 根 `Cargo.toml` members 增 `crates/drivers/ck-local`

**设计要点**：list = read_dir 收集 + 按 RelPath 字典序排序 + offset 编码进 PageCursor（不透明令牌）；mkdir 隐式父目录（create_dir_all + 已存在 Exists 判定）；rename 允许降级（未声明 SERVER_SIDE_MOVE 时 copy+delete——但 fs::rename 就是服务端移动，**声明 SERVER_SIDE_MOVE=true**）；reader = tokio File → 64KiB 帧 ByteStream，Range 半开+钳制按 vocab 语义；quota total=None（本地无配额概念，契约允许）；路径安全 = RelPath 校验 + 根 canonicalize 前缀钉死。

**Steps**（TDD 红→绿，每单元一 commit）：
1. 红：conformance.rs 套件接入（`assert_conforms` 对空实现）→ 跑 `cargo test -p ck-local` 证红（编译失败/断言①红）；
2. 绿：driver.rs + stager.rs 逐断言面实现（①→⑧顺序：往返→Range→stat/list→mkdir/delete→错误表空跑→rename→⑦RESUME 未声明自动跳过→并发读）；
3. 门禁：`cargo test -p ck-local` 八断言绿 + workspace 三步门禁 + `scripts/check_layers`；
4. Commit：`feat(ck-local): local StorageDriver — conformance suite first real citizen`（红绿证据贴 commit 正文：红 commit 单独留档）。

**验收判据**：conformance ①–⑥⑧ 绿（⑦未声明跳过）；能力位逐位注码（`range_read`/`server_side_move`/`authoritative_index` + 依据；**不声明 resume/multipart/rapid_upload**——staging 临时文件不跨进程复活）；workspace 625+ 全绿。
**止损点**：套件本体暴露断言语义缺陷（如分页令牌语义矛盾）→ 停，修 L2 套件（允许——套件是本批的「第一消费者」，发现的缺陷记 decisions），不得改断言迁就驱动。
**量级**：~350 行实现 + ~250 行测试。

## 3. Batch B1：ck-baidu 骨架 + OAuth + 元数据面（TDD）

**目标**：crate 骨架、oauth 状态机、HTTP client（spike api.rs 改造复用）、errno 映射表 mock 钉死、StorageDriver 元数据面（list/stat/mkdir/delete/rename/quota）；writer/reader 暂返回 `Unsupported`。

**Files**：
- Create: `crates/drivers/ck-baidu/Cargo.toml`（依赖 cloudkit-storage、reqwest（rustls，禁默认 native-tls 对齐 sync 先例）、tokio、serde、md-5；禁 core/L3+）
- Create: `crates/drivers/ck-baidu/src/lib.rs`（导出 + 工厂：加载凭据链→connect→uinfo 取 uid→VolumeId）
- Create: `crates/drivers/ck-baidu/src/oauth.rs`（refresh 状态机 + CredentialStore 回调持久化）
- Create: `crates/drivers/ck-baidu/src/client.rs`（HTTP 面：**endpoint base URL 为实例字段**（默认生产常量、测试注入 mock）——api client + stream client 双客户端（K18 IPv4/no_proxy/netdisk UA）、110 刷新重放一次、K15 重试钩子）
- Create: `crates/drivers/ck-baidu/src/api.rs`（协议包装：list/stat(meta)/mkdir(create dir)/filemanager delete+move/precreate/superfile2/create/quota/uinfo/dlink——改造自 `examples/baidu_spike/src/api.rs`，错误归一 StorageError）
- Create: `crates/drivers/ck-baidu/src/driver.rs`（StorageDriver：元数据面实现 + writer/reader `Err(Unsupported)` 占位；BackendHandle=fs_id 字符串 K5）
- Test: `crates/drivers/ck-baidu/tests/mock_backend.rs`（axum 内存 baidu：xpan 路由 + 错误码注入）
- Test: `crates/drivers/ck-baidu/tests/oauth_state_machine.rs`、`errno_mapping.rs`、`metadata_ops.rs`

**Steps**：
1. 红①：oauth 状态机测试（mock 110 一次→刷新→重放成功；110 两次→`Unauthorized{true}`；111/-6→`Unauthorized{false}`；刷新产物回调持久化断言——mock CredentialStore 记录调用）；
2. 绿①：oauth.rs + client.rs 刷新链；
3. 红②：errno 映射表回放测试（码表：110/111/-6/31034/-9(不存在)/12(参数)/31326(下载鉴权)——**以 PCFS client.go 交叉核对为准逐码注源**，mock 注入回放）；
4. 绿②：api.rs 错误归一层；
5. 红③：元数据面表单**字节级断言**（mock 断言收到的 query/form 恰为黄金参照：`method=list&dir=…&access_token=…`；filemanager filelist JSON 形态；mkdir 的 isdir=1 形态）；
6. 绿③：driver.rs 元数据面；
7. 门禁 + commit（每单元红绿证据入 commit 正文）。

**验收判据**：oauth 三态 + 持久化回调测试绿；errno 表逐码回放绿且**码表在驱动文档注释中逐码注源**；元数据操作对 mock 后端形式正确；workspace 门禁全绿；`cargo clippy -D warnings` 干净。
**止损点**：PCFS 源码与 spike 实抓样本冲突 → 以 spike 实抓为准（时效性裁决先例）记 decisions；oauth 链真网冒烟失败（baidu2 token 链失效）→ 停留现场报告待负责人（凭据类阻塞不硬闯）。
**量级**：~700 行实现 + mock ~400 + 测试 ~400。

## 4. Batch B2：ck-baidu 上传/下载 + conformance 全绿（TDD）

**目标**：三步曲上传（UploadStager + 差集续传会话 K7）、下载器（K8/K9）、conformance 八断言全绿（含 ⑦RESUME 差集可观测）、能力位全量声明。

**Files**：
- Create: `crates/drivers/ck-baidu/src/upload.rs`（stager：staging 边写边算 4MiB 分片 MD5 → close = precreate(rtype=3, K10) → 4 并发 superfile2 worker → create；return_type==2 直接收尾；会话表落盘 K7；探活/差集/整体重传三路）
- Create: `crates/drivers/ck-baidu/src/download.rs`（dlink 缓存 + 4MiB 有界 Range 4 并发预取 ByteStream + 403/31326 两段 fallback；206 响应头 content-md5/crc32 增量校验记录）
- Modify: `crates/drivers/ck-baidu/src/driver.rs`（writer/reader 接真实现；`reader(range)` 半开+钳制；Range>4MiB 语义=驱动内部分片拼接（对上层透明））
- Create: `crates/drivers/ck-baidu/src/transport_face.rs`（CloudTransport 面：connect/upload(整文件三步曲)/upload_stream(同路)/open/open_range/delete_remote(&RemoteHandle)/capabilities——**B3a 类型演进后补齐签名**，本批先以 StorageDriver 面为完成判据）
- Test: `crates/drivers/ck-baidu/tests/conformance.rs`（harness：mock 后端 + `backend_bytes_received` 观测点（mock 记分片字节）+ error_table 复用 B1 + chunk_size=4MiB）
- Test: `crates/drivers/ck-baidu/tests/upload_resume.rs`（差集：预置会话+已完成位图 → 断言只传缺失分片；会话死亡 → 整体重传兜底）、`dlink_cache.rs`（TTL 过期重取；403 两段 fallback 序列）
- Test: `crates/drivers/ck-baidu/tests/real_machine.rs`（`#[ignore]` 真机套件：同断言集接生产 endpoint + 凭据 env 注入 + rtype=3 覆盖复核 K10 + ≥100MB 往返吞吐记录）

**Steps**：
1. 红①：三步曲表单字节级断言（precreate form 六字段、superfile2 multipart+query 五参数、create form——黄金参照 PCFS api.go:488-573 + spike §3 样本）；
2. 绿①：upload.rs 正常路径（return_type=1 全量上传 → create errno=0 → Entry）；
3. 红②：差集续传（mock 预置会话状态断言第二次 writer 只 POST 缺失 partseq）+ 会话死亡兜底；绿②；
4. 红③：dlink 缓存/TTL/fallback（mock dlink 端点：首次 302、TTL 后旧 URL 403、追加 token 后 206、dlink 重取恢复）；绿③：download.rs；
5. 红④：conformance 八断言接入对 mock 后端；绿④：逐断言修至全绿；
6. 门禁 + `#[ignore]` 真机套件**本批至少跑一次**（复跑指南照 spike：token 从 `E:\GitHub\rs-CyDrive\test\instances\baidu*.json` 刷新维护，测试根 `/apps/cloudfs-b2`，cleanup 随套件）；
7. Commit 分单元（红绿证据入正文）。

**验收判据**：conformance ①–⑧ **全绿不得缩减**（⑦差集：`backend_bytes_received` 观测 + 重传 ≤ 差集上界断言）；能力位全量注码（`range_read`/`resume`/`multipart`/`server_side_move`(filemanager move)/`rapid_upload`(return_type=2 路径已实现但不依赖)/`authoritative_index`；不声明 `change_feed`/`inbound`/`chat`，逐位注理由）；真机 `#[ignore]` 输出留档（脱敏）。
**止损点**：mock 与真网行为分歧（表单被拒）→ 回 spike 工具复验证协议（工具就位），不盲改；真网吞吐 <5MB/s 或新限额形态 → 记录 + 复跑 spike §2（改 env 即可），不可用则报负责人裁决百度降级。
**量级**：~800 行实现 + 测试 ~500。

## 5. Batch B3a：L2 transport 类型演进（纯重构，TDD 迁移护航）

**目标**：K1/K2/K3 三项 L2 类型演进落地，为 B3b 接线铺平；**telegram 行为零变化**。

**Files**：
- Modify: `crates/cloudkit-storage/src/transport/mod.rs`（UploadReceipt/RemoteHandle i64 + `path: Option<RelPath>` 字段 + delete_remote 签名 `&RemoteHandle`；模块文档映射表更新）
- Modify: `crates/cloudkit-storage/src/transport/mock.rs`（同步签名/字段；path 字段忽略或直存）
- Modify: `crates/drivers/ck-telegram/src/transport.rs`（i32→`i64::from` 收窄边界；delete_remote 新签名；path=None）
- Modify: `crates/cloudkit-core/src/vfs.rs`（hydrate 构造 RemoteHandle 填 `path: Some(rel_path)`——本就持有行；msg_ids i64 直通，删 `i64::from` 中转）
- Modify: `crates/cloudkit-core/src/upload_queue.rs`（receipt i64 直存，删 `i64::from(receipt.first_msg_id)` 一处）
- Modify: `crates/cloudkit-core/src/inbound.rs` 及涉及 RemoteHandle 构造的调用点（波及面清单在红 commit 列全）
- Test: 既有测试的 i32→i64 字面量机械更新（**逐处列出，证明零语义漂移**）；新增护栏：telegram/mock 路径 path=None 行为不变断言

**Steps**：
1. 波及面清单先行（rg 全部构造点/消费点，commit 正文逐条列）；
2. 红：新增护栏测试（path 字段默认 None 不影响既有流）；
3. 绿：类型演进一次完成（breaking 波及面在单 commit 内闭合，interfaces §1 纪律）；
4. 门禁：workspace 全绿 + **既有 625 测试除机械字面量外零断言改动**（diff 审计）+ check_layers。

**验收判据**：workspace 全绿；断言漂移审计 = 仅 i32→i64 字面量与构造点补字段（清单留档）；telegram/mock 能力与行为零变化。
**止损点**：演进暴露 core 隐性 i32 假设（如 webdav 层）→ 波及点一并修（属同批机械面），超出「机械」判断的语义变化 → 停记 decisions。
**量级**：~200 行净变更（清单驱动）。

## 6. Batch B3b：组合根接线（config/工厂/setup/doctor/namespace/rebuild/删除接线）

**目标**：三后端经 `backend` 键 dispatch 全栈可用；删除语义接远端；权威 bootstrap 落地。

**Files**：
- Create: `crates/drivers/ck-local/src/transport_face.rs`（CloudTransport 面：upload=writer+close 复制、open=reader 按 path（K2/K6）、delete_remote=stat+delete、connect=根可写探测、capabilities=驱动位+remote_delete）
- Modify: `crates/drivers/ck-baidu/src/transport_face.rs`（B3a 后签名闭合：upload 整文件三步曲、msg_id=fs_id（K5）、delete_remote=fs_id 删除、as_inbound/as_chat 默认 None）
- Modify: `crates/cloudkit-core/src/config.rs`（K17 新键三处同步 + validate 文案；`backend` 缺省 telegram）
- Modify: `crates/cloudkit-cli/src/lib.rs`（backend dispatch：telegram 既有路径不动 / baidu / local → 工厂 → `Arc<dyn CloudTransport>`；装配期能力横幅九+1 位全列（R-5 先例）；proxy_url 对 baidu/local 无效声明（K18））
- Modify: `crates/cloudkit-core/src/sync.rs`（namespace_key 后端感知 K12：telegram 派生逐字节不变的护栏测试 + baidu/local 派生）
- Create: `crates/cloudkit-core/src/rebuild.rs`（`rebuild_from_backend(driver, db, root)`：递归 list → upsert 行（is_uploaded=1/chunk_count=1/msg_id=fs_id/mtime=Entry）——明文-only 语义 K11）
- Modify: `crates/cloudkit-cli`（`cydrive rebuild` 子命令；setup 增 baidu 分支（粘贴 refresh_token+appkey/secret → 刷新验证 → CredentialStore 持久化）与 local 分支（local_root 引导）；doctor 增 baidu（token 探活/root 可写/proxy 提示）与 local（root 存在可写）检查项）
- Modify: `crates/cloudkit-core/src/vfs.rs` + `crates/cloudkit-webdav/src/lib.rs` + `crates/cloudkit-web`（删除路径 K4 门控接线：remote_delete 位真 → 先 driver 删远端再删行；假 → 现行为）

**Steps**（每单元红→绿一 commit）：
1. 红/绿：config 新键三处同步 + validate 文案测试；
2. 红/绿：namespace_key 护栏（telegram 旧派生值逐字节断言钉死）+ baidu/local 新派生；
3. 红/绿：rebuild（mock driver 播种后端树 → rebuild → db 行断言路径/size/msg_id；加密实例拒绝并给 sync 指引）+ `cydrive rebuild` CLI；
4. 红/绿：删除接线（mock remote_delete=true：远端删成功→行删；远端删失败→行保留错误可行动；=false：现行为零变化——三面 vfs/webdav/web）；
5. 红/绿：工厂 dispatch（三 backend 键 → 三 transport 类型断言 + 缺省 telegram 兼容断言）+ setup/doctor 分支；
6. 门禁：workspace 三步 + check_layers + scan_secrets + WSL（config/dispatch 涉跨面，双平台验证）。

**验收判据**：`backend = "baidu"`（mock 注入）全栈装配测试绿；telegram 缺省路径全部既有测试零改动通过；rebuild/删除语义各有红绿证据；doctor/setup 文案可行动；门禁双平台绿。
**止损点**：删除接线波及 sync 墓碑语义（远端删+本地行删的唤醒/推送时序）→ 以「db update_hook 单点门铃」既有机制推演（0.7.2 收口先例），发现真竞态停记 decisions。
**量级**：~550 行实现 + 测试 ~350。

## 7. Batch E2E：三后端硬验收（真机，非 TDD——验证驱动）

**拓扑**（每实例独立 cwd 目录 + 独立端口/盘符；配置模板由本批产出至 OS 临时目录，不入库 R7）：
- **telegram**：实例 A（Y:）——既有测试集回归 + Explorer 上传/下载（PROPPATCH 207 陷阱条款复核）；**前置 = 独立测试 chat**（tracker 待负责人 #1，未提供则本腿延后并报告，不阻塞其余）；
- **baidu**：实例 B（Z:）——Explorer 上传/下载/**Range 播放**（媒体文件拖播）/断点续传中途杀进程恢复（复刻 spike §3 场景于真实挂载链路）；测试根 `/apps/cloudfs-e2e`（**绝不触碰 /apps/privatefs**）；rebuild 等价性检查（D10 ②：明文集上 rebuild 重建索引 vs sync 复制索引路径/size/msg_id 等价）；
- **local**：实例 C（V:）——挂载/写读往返/删除；
- **sync**：A↔B 各自收敛（等 telegram 测试 chat；baidu↔baidu 第二实例用同账号第二 cwd 亦可验）+ 双/三盘并存互不干扰（写 Z: 不影响 Y:/V:）；
- **收尾**：`/_e2e_*` 前缀全清理（删所建行 + 等 sync 墓碑收敛 + baidu 远端清理复查 errno=-9）；日志/报告**脱敏后**入库。

**产出**：`docs/reports/2026-09-08-phase2-e2e.md`（每腿：方法/原始输出摘要/结论；token 掩码前 6 后 4）。
**验收判据**：foundation §5 Phase 2 门全过（三后端真机全流程 + sync 收敛 + 双盘并存）；轮换演练检查（凭据失效给 人话指引不死循环）。
**止损点**：任一腿不可用（协议分歧/凭据/环境）→ 留现场证据、其余腿继续、报告集中列；**测试 chat 未到位 = telegram 腿整体延后**（负责人挂账项，非本计划失败）。

## 8. 收口

- 版本 **0.9.0**（workspace 单点；CloudTransport 签名破坏性演进（K1/K3）+ 新驱动双 crate）；
- 文档联动：README 状态表（三后端）、AGENTS（阶段/计数/陷阱——含「baidu 下载三约束」「dlink TTL」「rtype=3」条目）、driver-onboarding 如有修订；
- decisions.md：K1–K18 逐条入档 + 执行期取舍；tracker/phase2.md 全表收口；
- `cargo build --release` 全 workspace 产物验证；**不部署生产位**（部署裁决留负责人，0.8.0 先例）；
- 遗留清单：Phase 2.5 多卷计划另立（方案一裁决）；正式 appkey 复测 spike §2（负责人 appkey 到位时）；litmus/`#[ignore]` 真机×3 随真机窗口。

---

## 9. 全批共同纪律

1. **TDD 红→绿留证**（红 commit 单独留档或红输出贴绿 commit 正文）；**断言零漂移**（改断言必须对应授权裁决并逐处列出）；测试/实现委派隔离（测试作者不见实现；实现者禁改红断言）；
2. workspace 三步门禁每批必过（`cargo test --workspace --no-fail-fast` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt --all -- --check`）+ `scripts/check_layers`（动依赖后必跑）+ `scripts/scan_secrets`；
3. **conformance 八断言不得缩减**；规范冲突以 standards/ 为准并记 decisions；
4. 凭据只从 `E:\GitHub\rs-CyDrive\test\` 或 env 读；任何凭据值不入代码/文档/日志/提交（token 掩码前 6 后 4）；`test/` 目录 gitignore；
5. 提交 conventional commits 不加署名尾注；批次 worktree 隔离（分支 `feat/phase2` 自 main 切出）；
6. 中止条件：止损点触发、重派两次仍不绿、契约矛盾、需动 Python 兼容红线（R6）——停、留现场、写清结论；
7. 每批收口更新 `docs/tracking/phase2.md` 并随 commit 提交。

---

## Kickoff 指令（新会话粘贴即执行；负责人 2026-09-08 预授权：全程自主，不中途确认）

> **执行 `E:\Rs_Codes\rs-cloudfs` 的 Phase 2 全量**（docs/plans/2026-09-08-phase2-execution.md，Batch L→B1→B2→B3a→B3b→E2E→收口）。模式：全程自主，**不向用户请求任何确认**；可逆实现细节自行裁决并在 decisions.md 记录理由（计划 §1 的 K1–K18 即预裁决集，执行期逐条入档）；真实疑问/范围变更/止损点触发 → 记录到 decisions.md「待负责人」清单并继续可继续的部分，收尾报告集中列出；破坏性操作（动生产 db/部署位/删除既有数据/触碰 `/apps/privatefs`）一律禁止。
> 1. 开工先读（按序）：`AGENTS.md` → `docs/tracking/phase2.md` → `docs/plans/2026-09-07-cloudfusion-foundation.md`（v1.2 权威）→ 本计划 → `docs/standards/driver-onboarding.md`（验收依据）→ `docs/reports/2026-09-07-baidu-spike.md`（含附录 B）→ `docs/decisions.md` 自 2026-09-07 起条目。工作目录 `E:\Rs_Codes\rs-cloudfs`，worktree 隔离（分支 `feat/phase2` 自 main 切出）。
> 2. 实现参数情报：`examples/baidu_spike/src/api.rs` 是已真网验证的协议面（可直接改造复用）；`E:\Go_codes\PrivateCloudFS` 是协议权威参照（decisions 2026-09-08 有勿抄坑清单）；协议疑义先查 PCFS 源码再自行试验，仍分歧以 spike 实抓为准并记 decisions。
> 3. 凭据（只读，绝不入库/入日志/入提交）：百度 refresh_token 在 `E:\GitHub\rs-CyDrive\test\instances\baidu*.json`（现值即正式凭据；appkey/secret 只读参照 PCFS `client.go` 或经 env 注入）；telegram E2E 前置 = 独立测试 chat（未提供则 telegram 腿延后并报告，不阻塞 baidu/local 腿）。
> 4. 纪律：TDD 红→绿留证、断言零漂移、测试/实现委派隔离；workspace 三步门禁每批必过 + check_layers + scan_secrets；conformance 八断言不得缩减；规范冲突以 standards/ 为准并记 decisions；E2E 写操作限定 `/_e2e_*` 前缀 + 收尾清理 + 报告脱敏。
> 5. 每批收口更新 `docs/tracking/phase2.md`（状态+证据）并提交；收尾汇报：改动摘要/每批验证证据（真实输出）/未询问的决定与回滚/疑问与待负责人清单。
