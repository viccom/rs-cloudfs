# Phase 0 + Phase 1 执行跟踪单

> **用途**：任务分解 × 状态跟踪 × 跨会话交接三合一。**执行 Agent 每批收口必须更新本表并随 commit 提交**；新会话开工第一件事读本表续跑；负责人随时可查进度。
> 状态图例：⬜ 未开始 ｜ 🔶 进行中（注记当前会话/下一步）｜ ✅ 完成（附证据）｜ ⛔ 受阻（附 decisions 条目）｜ ❌ 取消（附裁决）
> 证据格式：commit hash / 报告路径 / 测试计数（`cargo test --workspace` 总数）。
> 任务定义与验收判据的权威 = [../plans/2026-09-07-phase0-1-execution.md](../plans/2026-09-07-phase0-1-execution.md)（下称「执行计划」）；本表只跟踪，不重复定义。

## 当前焦点

Batch P0 已收口；**Batch S（百度 spike）已收口（2026-09-07，止损未触发，六项全档）**；下一步 = Batch R（trait 瘦身，TDD——百度 errno 三档映射/断点续传参数按 spike 报告 docs/reports/2026-09-07-baidu-spike.md 落地）。

---

## Batch P0：搬迁改名（纯机械，每步全绿）

| # | 任务 | 状态 | 证据/注记 |
|---|---|---|---|
| P0-1 | worktree/分支建立（feat/phase0-1） | ✅ | 负责人预授权 P0 直 main（Kickoff 指令），不开 worktree |
| P0-2 | crate 改名 cydrive-*→cloudkit-*/ck-*（逐 crate 提交，二进制名不变） | ✅ | fe61e8e / 83cc23d / 5f2b986 / 8bc031f / 360a675 / e6581f9 / 50d13b2（core→cloudkit-core、telegram→drivers/ck-telegram、sync→cloudkit-sync-server、webdav→cloudkit-webdav、web→cloudkit-web、platform→cloudkit-platform、cli→cloudkit-cli）；每 commit 前三步门禁全绿，测试计数保持 527；`[[bin]] cydrive`/`cydrive-sync-server`、`CYDRIVE_*` env、`cydrive_sync.db` 等契约未动 |
| P0-3 | `scripts/check_layers` + CI 秘密扫描步骤 | ✅ | 代码/CI 交付 = b9bc044：两脚本入库（POSIX sh，shellcheck 干净；`.gitattributes` 锁 LF）+ ci.yml 接入（checkout fetch-depth:0，fmt 前两 step：layer check R1 / secret scan R3）；check_layers 正例 7 manifests 全绿、反例（临时目录：L3+crate→driver、driver→driver 各抓到 exit 1；组合根豁免/注释行/vendor 排除对照不误报）；scan_secrets 全树零命中、反例五路（push 真实 before / zeros 回退 / PR / 干净区间 OK / 本地全树）均符合预期，行号精确定位 bad.txt:3、短值 `password = "short"` 不误报；不可用 range/非 git 目录=响亮失败非静默 OK（exit 2/128）；三步门禁随批全绿（fmt/clippy 干净，test 527 passed 0 failed） |
| P0-4 | 门禁全绿 + WSL 通道（~/rs-cloudfs）建立 + `--version` 验证 | ✅ | 终态门禁：win fmt/clippy 干净 + `cargo test --workspace` 全 suite 0 failed（527）；WSL `~/rs-cloudfs` clone 自 /mnt/e（git fetch 单向同步法）+ `cargo check` 23.50s 过 + 全量 `cargo test --workspace` **528 passed 0 failed**（+1 为 bin_cli.rs 平台 cfg 既有差异，核查过非改名引入）；`cydrive --version` → `cydrive 0.7.2`（bin 名不变） |

## Batch S：百度 spike（验证驱动，非 TDD）

| # | 任务 | 状态 | 证据/注记 |
|---|---|---|---|
| S-1 | token 刷新链跑通（refresh_token → 新 access） | ✅ | `examples/baidu_spike refresh`：http=200 expires_in=30d/143ms（baidu1.json 链已被消费失效 → 改用 **baidu2.json**，待负责人知悉）；token 只落 %TEMP%；appkey/secret 运行时解析自 PCFS client.go（值零入库） |
| S-2 | QPS/限额三连测（列目录/分片/下载，记录拒绝形态；PCFS appkey 桶局限标注） | ✅ | list 10 连发（含并发）全 200/errno=0（204–501ms）；superfile2 同分片 10 连发（含并发）全 error_code=0；下载流 5 连发（含并发）全 302+206——**零 429/31034/拒绝**；⚠ 结论仅对 PCFS 第三方 appkey 桶有效（正式 appkey 须复跑，工具已就位） |
| S-3 | 上传三步曲 + 断点续传差集（中途杀进程只补差集） | ✅ | abort 上传 [0,1,2] 后 `std::process::abort()` 硬杀；continue 用**持久化旧 uploadid** 探活成功 → 只补 [3,4,5,6,7]（2.3s）→ create errno=0 = 服务端保留旧分片（差集成立）。任务预设「重 precreate 拿已传列表」实测不成立（同参重 precreate = 新 uploadid + 全量列表）——裁决记 docs/decisions.md 2026-09-07 Batch S 条目。附带：服务端 `md5` 字段为 content-id 非字面 MD5（校验走内容/CDN 头）；rtype=1=冲突重命名（覆盖需 rtype=3，待 B2 复核） |
| S-4 | 秒传 return_type 分支行为 | ✅ | 此 appkey 桶**秒传不触发**：同内容即时 + 延迟 2min+ 复测均 return_type=1（响应含 uploadid+block_list）；B2 仍实现 return_type=2 分支但不作功能依赖 |
| S-5 | dlink+Range 复测 + dlink 有效缓存时长 | ✅ | 302→CDN head/mid/tail 三点 206 字节级匹配；**下载三约束**：有界 Range≤4MiB（8MiB 即 403/31326，位置无关）+ netdisk UA（Mozilla 全 403）+ 禁无 Range/开放 Range；dlink 直连/追加 token 两态并存（fallback 必做）；单 dlink 支撑 256 分片×4 并发 1GiB 下载且 **≥56min 未失效**（探测进程被环境终止时仍 206，上界未测到）→ B2 缓存 TTL 30min + 403 驱动刷新 |
| S-6 | ≥1GB 上/下行吞吐实测 | ✅ | 1GiB：上行 27.4 MB/s（256 分片×4 并发 39.3s，复跑 25.2 一致）、下行 21.7 MB/s（256×4MiB 有界分片×4 流，单 dlink 复用，256/256 206 字节全额）；远超 5MB/s 止损线 |
| S-7 | spike 报告落档（docs/reports/）+ 止损判定 | ✅ | 报告 = docs/reports/2026-09-07-baidu-spike.md（六项：方法/原始输出/结论/B 批参数建议）；**止损未触发**；远端 cleanup 完成（9 文件 + spike 目录全删，复查 errno=-9；删除走回收站如实记录）；门禁 fmt/clippy -D warnings 干净 + test 527 passed 0 failed |

## Batch R：trait 瘦身（纯重构，TDD）

| # | 任务 | 状态 | 证据/注记 |
|---|---|---|---|
| R-1 | cloudkit-storage：StorageDriver/Capabilities/StorageError/VolumeId（TDD） | ✅ | 红 489852e → 绿 0625e65；types 语义 20 测试绿；百度 errno 三档 fixture 6 测试绿（110→Unauthorized{true}/111、-6→Unauthorized{false}，test-only，R1）；偏离草图裁决见绿提交正文（Listing/Box<dyn UploadStager>/ByteStream Stream 形态等 6 项） |
| R-2 | conformance_suite 框架 + mock 跑通八断言（interfaces §6，不得缩减） | ✅ | 八条齐：①往返+commit-on-close 不可见 ②Range 半开/钳制/start≥size 声明 ③分页完整稳定有序 ④mkdir/delete 幂等声明恒定 ⑤错误表回放（mock 经百度三档码） ⑥rename 文件+目录 ⑦RESUME 差集（bytes_received 可观测，重传≤差集上界） ⑧并发读；tests/ 红绿两提交零 diff（断言零漂移）；套件实现期当场抓住 mock 的 ensure_parents 把末段建目录的 bug（①断言红）；workspace 门禁 555 passed 0 failed |
| R-3 | CloudTransport 演进 + InboundCap/ChatCap 拆分（波及面清单先行） | ✅ | 红 93a8981 → 绿 8f8e784（波及面清单 + TransportError→StorageError 映射表均在绿提交正文）；trait 家族（核心面 connect/upload/open/open_range/delete_remote + capabilities() 必选 + as_inbound/as_chat provided 默认 None）+ InboundCap/ChatCap 拆分迁 cloudkit-storage（L2）；core 以 re-export shim 维持 `cloudkit_core::transport::*`/`rel_path`/`chunker::part_name` 路径（消费方 import 零改动）；新增 7 语义测试（transport_traits）；既有 555 全绿护栏在迁移期抓到单块命名分支丢失（part000 仅多块生效）当场修复；门禁 562 passed 0 failed / clippy / fmt / check_layers 全过 |
| R-4 | ck-telegram 适配 + →core 反向依赖解除（过渡豁免表销账） | ✅ | Cargo.toml `rg 'cloudkit-core'` 为空（注释措辞一并避让）；grammers 三 trait + capabilities()=INBOUND/CHAT/RANGE_READ/MULTIPART（R4 过渡期依据注于代码：驱动单测+生产真机，conformance 前置属 Phase 2，宁缺勿滥位逐个注明）；map_invocation_error 产 StorageError（FloodWait→RateLimited{retry_after}）；vpath/part_name 随迁 L2 供驱动侧引用；architecture §1.5 行已销账 |
| R-5 | 消费方能力探测降级（bot worker/webdav，无能力禁用不 panic） | ✅ | 注：R-3 绿提交已就地完成 inbound worker/上传降级通知的探测降级（缺位 warn 不 panic）；本条收口 webdav 侧 + 启动声明面：红 08f459b → 绿 5d98157。**webdav Range 现状裁决**：读路径恒走 Vfs::hydrate→transport.open() 整读（open_range 生产调用者为 0），Range 由本地缓存 seek 切片——无 RANGE_READ 的 transport 行为逐字节一致，按计划二选一取「维持现状 + 注释声明」，smoke 3b（全 none 能力位 transport → GET Range 仍 206 字节级一致）钉死防未来转发路径漏查；启动能力声明：run_with_transport 在 WebDAV bind 前打一行九位全列 info（bind 失败也留声明；消费方缺 INBOUND/CHAT 的 warn 已存在不重复打），横幅测试经 set_global_default+进程级 sink 捕获（thread-local set_default 输给并行 Interest 缓存竞争，4/4 复跑稳定）；MockTransportBuilder::capabilities 注入面（默认三 bit 不变）；门禁 565 passed 0 failed（562+3 新增）/ clippy / fmt / check_layers 全过 |
| R-6 | 真机 Telegram 冒烟（生产实例谨慎协议：/_e2e_smoke/ + 清理） | ⬜ | |

## Batch E：加密 v2 流式（TDD）

| # | 任务 | 状态 | 证据/注记 |
|---|---|---|---|
| E-1 | cloudkit-crypto：CryptoScheme trait + v1 GCM 迁入（字节零变化，互操作测试护航） | ✅ | 迁移 23702ea：crate 独立（无 workspace 内依赖，sync std::io）+ trait/Id 语义契约成文 + core shim（`cloudkit_core::crypto::*` 零改动）；互操作向量测试 git mv 随迁前后各 11 passed、diff 仅 2 行机械路径（断言零改动）；workspace 565 与基线一致 |
| E-2 | v2 分块 AEAD 核心（STREAM 构造；roundtrip/tamper 拒绝/跨块 Range） | ✅ | 红 3e46ea8 → 绿 19a2f2f：格式=34B 头（magic/version/salt/迭代数/chunk_size 护栏 64KiB..=4MiB）+ 每块 GCM（nonce=counter_be56+末块标志域分隔、AAD=全头）；测试面 roundtrip 八尺寸/六类 tamper 全拒/decrypt_range 11 区间与全量切片逐字节一致/9.5MiB 流式粒度断言/护栏边界；终态 aead_v2 16 + scheme 5 新测试，workspace 586 passed 0 failed/clippy/fmt/check_layers 全过；格式裁决入 decisions.md（含 KDF DoS 放大挂账待裁） |
| E-3 | 流式加密上传接线（零 .enc.tmp；内存峰值断言） | ⬜ | |
| E-4 | `encryption_scheme` 配置键 + Entry/payload scheme 字段 + hydrate 按 scheme 分发 | ⬜ | legacy json 拒收；旧实例忽略新字段 |
| E-5 | 真机 v2 冒烟（telegram 加密小文件上传/下载往返） | ⬜ | |

## 收口（全批后）

| # | 任务 | 状态 | 证据/注记 |
|---|---|---|---|
| C-1 | 版本 0.8.0 + decisions/AGENTS 入档 + release 构建验证（不部署生产位） | ⬜ | |
| C-2 | 收尾汇报（改动/证据/未询问决定与回滚/待负责人清单） | ⬜ | |

---

## 待负责人清单（执行期累积，收尾汇报汇总）

1. （空——执行 Agent 遇疑问/裁决点时追加于此并继续可继续部分）
