# Phase 2.5 多卷启用（Volume Registry）执行计划

> **For Claude:** REQUIRED SUB-SKILL: 使用 executing-plans 编排执行本计划（hub-and-spoke：主会话拆分派发/审查收口，实现委派子代理，TDD 红→绿留证）。

**Goal:** 一个 cydrive 进程通过 Volume Registry 跑多个存储卷——每卷一份配置文件、每卷独立加密、各自挂一个盘符（V: 本地 + Y: telegram + Z: 百度）、单仪表盘切换/汇总所有卷；单卷旧配置字节兼容。

**Architecture:** 隔离模型沿用已裁决的「每卷一份 db/cache 目录」（R6 零 schema 变更）——卷 = (卷配置文件, 独立卷目录, driver, db, Vfs, 队列, sync worker, CyDriveFs)；进程级共享 WebDAV 单端口（`/vol/<name>` 路径前缀分发）、Web UI 单端口、控制面与 stop gate。装配层（cloudkit-cli）是主战场，core/webdav/web 做受控扩展，storage/drivers/platform 零改动或近零。

**Tech Stack:** 现栈不变（tokio/axum/dav-server/rusqlite）；新增形态均为装配层组合，无新依赖。

**输入裁决链（不可推翻，只可引用）：**
- decisions 2026-09-08「多卷启用裁决=方案一（B3 后接 Phase 2.5）」：Registry + 每实例配置文件 + 每卷加密 + 多盘挂载；
- foundation §4-D6：VolumeId/EntryId 从第一天带卷；§4-D10：sync-server 一台服务全部 namespace；
- driver-onboarding §4 前瞻条款：「单一 backend 键形态将被『每实例一文件』形态叠加而非替换」；
- R1（L2+ 禁 import 驱动符号，组合根豁免）/ R3（凭据红线）/ R6（零 schema 变更）；
- PCFS 反面教训（decisions 2026-09-08 PCFS 研究 + 本计划 §0）：失败静默、无参回退第一卷、共享缓存混装、聚合层 import 驱动错误类型、Stats 全根扫描——全部反向设计。

---

## §0 研究档案（2026-09-08 两轮 Explore 结论，file:line 为 0.9.0 基准）

### PCFS 实例模型（应抄/应避）

| 应抄 | 应避（PCFS 实测坑） |
|---|---|
| 每实例一配置文件 + 目录扫描装配 + `enabled` 开关 | 实例 Config 无 schema 的 `map[string]interface{}`（我们用强类型 CyDriveConfig 子集） |
| Registry 薄（map+锁）与生命周期管理分离；Register/Unregister/Reload 三动词 | 启动失败静默吞错（我们：逐卷状态机 failed+reason 可见于横幅与 /api/volumes） |
| EntryId `"volume:path"` 前缀分发（与 D6 契合） | 无卷参数回退「第一个可用驱动」（Go map 随机序）——我们：显式卷参数，多卷无参 400 |
| CryptoWrapper 装饰器按实例包 driver + 每实例密钥 | 加密状态靠类型断言/Name 后缀嗅探；聚合层 import 具体驱动错误类型 |
| Token 刷新回调注入 driver | 单一共享 entryCache 混装所有实例（我们：每卷独立 db） |
| Unregister 三处一致清理 | Stats 每刷=N 后端全根扫描（我们：仪表盘汇总读各卷 db 元数据） |

### 本仓装配面盘点（单一性硬编码点 → 改动落点）

装配链：`main.rs:531 run()` → `discover_config`（lib.rs:1920-1955，cwd 单 config.toml）→ backend dispatch（main.rs:552-605）→ `run_with_transport_options`（lib.rs:436-650：db 441 / Vfs+队列 448-453 / inbound 468 / webdav 479-497 / web 513-540 / control 546 / sync 574 / mount 584 / stop gate 594-640）→ `RunHandle`（lib.rs:238-263）。

| 硬编码点 | 位置 | 多卷落点 |
|---|---|---|
| backend 单值 dispatch | config.rs:189-199、main.rs:552-605 | 逐卷 dispatch（复用 build_backend_transport） |
| db/cache 单路径锚 cwd | config.rs:293-297、417-419 | 相对路径基准=卷目录（K21） |
| telegram session cwd 固定 stem | lib.rs:684-693 | 传 per-volume 基准目录 |
| baidu sessions_dir="." | lib.rs:1083 | 传卷目录（机制已支持） |
| webdav 单端口根路径 | lib.rs:497、webdav/server.rs:70-75 | 单端口 `/vol/<name>` nest（K20） |
| web 单卷身份 | web/lib.rs:85-116、216-245 | AppState registry 化（K24） |
| drive_letter 单值单挂载 | config.rs:309、lib.rs:1685-1761、1764 | per-volume 盘符+URL（K27） |
| 单控制文件 | control.rs:41-44 | 进程级聚合控制（K25） |
| 单 sync task | lib.rs:574、1560-1670 | 逐卷 spawn（namespace 已键控） |

多实例可行性已确认：Vfs/db(Mutex<Connection>)/队列/sync/DavHandler/AppState 全 per-instance，全仓无阻塞性 static（仅 ck-local staging 序号 AtomicU64，多实例安全）；grammers vendored sqlite 与 rusqlite(bundled) 共符号但多连接无冲突。

测试基建：`cloudkit-cli/tests/run_e2e.rs`（CwdGuard 60-72 + temp_config 90 + run_with_transport 112-114）、`dispatch.rs:41-90 spawn_mock_baidu`（axum mock 后端）、`cloudkit-storage mock.rs MockStorageDriver`、`conformance_suite!` 宏（本批不涉驱动语义，不动）。

---

## §1 设计裁决 K19–K29（执行期逐条入档 decisions.md）

| # | 裁决 | 理由/来源 |
|---|---|---|
| K19 | **配置形态=每卷一文件**：进程级 config.toml 增 `volumes_dir` 键（缺省无=单卷模式，字节兼容）；卷文件 `<volumes_dir>/<name>.toml`，stem=卷名，命名 `^[a-z][a-z0-9_-]{0,31}$`；卷文件=强类型卷作用键子集（复用 CyDriveConfig 解析面），**含进程级键 → validate 拒**；`volumes_dir` 存在但目录缺失/空 → 拒；卷模式与进程级卷作用键（backend/db_path/凭据等）混用 → 拒（可行动文案指引移入卷文件） | onboarding §4 前瞻条款 + PCFS 实证 + 增删卷=增删文件 |
| K20 | **单 WebDAV 端口 + `/vol/<name>/` 前缀**（axum nest_service 剥前缀，RelPath 不得见 `vol`——R1：前缀是进程级概念）；单 web UI 端口。**回退案 K20B**：MiniRedir 子路径挂载真机探针失败 → 每卷一端口（webdav crate 零改动路线），记 decisions | 单进程单心智；端口资源；探针在 MV2 提前退风险 |
| K21 | **每卷一个卷目录** `<volumes_dir>/<name>/`：db/cache/session/baidu_state 相对路径基准=卷目录；零 schema 变更（R6）；单卷模式基准=cwd 不变 | decisions 2026-09-08 方案一原文 |
| K22 | **失败可见降级**：逐卷状态机 starting/running/failed{reason}；一卷失败不拖死进程（横幅 error+/api/volumes 暴露），**全部卷失败 → 进程退出非零**；禁 PCFS 式静默 | PCFS 反训 |
| K23 | **无默认卷**：多卷模式所有卷作用 API 显式带 `volume` 参数；无参 → 400 + 可选卷清单 JSON；单卷模式（无 volumes_dir）行为=现状（/api/stats 无参 16 键不变，冻结契约零漂移） | PCFS「回退第一个」是随机写 |
| K24 | **仪表盘契约**：新增 `GET /api/volumes`（name/backend/volume_id/drive_letter/webdav_url/status/quota_used/quota_total/total_files/total_bytes——读各卷 db 元数据+装配期 quota，**禁后端全根扫描**）；`/api/stats|files|list|upload|delete|queue|download` 增可选 `volume` 查询参数；16 个冻结键原样保留（带参按卷返回）；前端卷切换 tabs+汇总卡 | PCFS Stats 坑 + additive 先例（11→16 键） |
| K25 | **stop 语义=停整进程**：多卷模式进程级控制文件（cwd 下，与单卷同文件名）；`cydrive stop` 写聚合控制面；单卷模式控制文件位置不变 | 用户心智：进程=全部卷 |
| K26 | **sync 逐卷**：spawn_periodic_sync per-volume（NamespaceIdentity 已按卷键控）；sync-server 不动（D10） | 现机制天然支持 |
| K27 | **挂载逐卷**：卷文件 `drive_letter` 键；多卷盘符冲突 → validate 拒；挂载 URL=`http://{webdav_host}:{webdav_port}/vol/<name>`（K20B 时=每卷端口）；unmount 现机制 per-letter 复用 | platform windows.rs:98/111 已全参数化 |
| K28 | **env 覆盖收缩**：`CYDRIVE_*` config 覆盖仅单卷模式生效；卷模式忽略并在日志声明（凭据 env>config>keyring 解析链不变） | 卷模式下 env 全局覆盖会跨卷串味 |
| K29 | **命名与暴露**：内部 `VolumeRegistry`；卷名即 URL 段/盘符挂载段/仪表盘标签三处一致；装配横幅逐卷打能力声明（R-5 纪律），进程级横幅列卷清单+状态 | 术语统一，PCFS driverName 前缀 ID 同构 |

---

## §2 配置形态规格（MV0 验收蓝本）

**进程级 `config.toml`（多卷模式）**——只留进程级键（键名以 config.rs 现状为准）：

```toml
volumes_dir = "volumes"
webdav_host = "127.0.0.1"
webdav_port = 8080
enable_web_ui = true
web_ui_host = "127.0.0.1"
web_ui_port = 8088
```

**`volumes/local.toml`**（卷作用键；相对路径基准=卷目录）：

```toml
backend = "local"
local_root = "root"
drive_letter = "V"
```

**`volumes/tg.toml`** / **`volumes/baidu.toml`**：backend=telegram/baidu + 各自驱动键/凭据键 + encrypt 键 + `drive_letter = "Y"`/`"Z"`。凭据值照旧不入库（R3）：卷文件引用 keyring/env 的解析链在驱动 resolve 时生效，与单卷同。

**键分类**（config.rs 落地为常量，测试钉死）：
- 进程级键：`volumes_dir`、`webdav_host`、`webdav_port`、`enable_web_ui`、`web_ui_host`、`web_ui_port`、日志级等全局键；
- 卷作用键：`backend` + 全部驱动参数/凭据键 + `encrypt` 键组 + `db_path`/`cache_path`/`cache_limit_gb` + `drive_letter` + sync 键组 + 队列 worker 键；
- 两集合在 KNOWN_TOML_KEYS 内二分全覆盖，卷文件解析走同一 load 严格校验面（未知键拒收同文案）。

---

## §3 批次任务（每批 TDD 红→绿留证；门禁=三步+check_layers+scan_secrets）

### MV0 配置与发现（cloudkit-core/config.rs + cloudkit-cli/lib.rs discover）

**Files:** Modify `crates/cloudkit-core/src/config.rs`（KNOWN_TOML_KEYS/字段/Default/validate）、`crates/cloudkit-cli/src/lib.rs:1920-1955`（discover）；Test `crates/cloudkit-core/src/config.rs` 单测区 + `crates/cloudkit-cli/tests/`。

1. 红：① `volumes_dir` 键解析（有/无两态）；② `load_volume_config(path)` 合法卷文件→VolumeConfig（含卷名与基准目录解析）；③ 卷文件含进程级键→Err 可行动文案；④ 卷名非法（大写/`/`/空/超 32）→Err；⑤ `discover_volumes(dir)` 目录枚举按名稳定排序；⑥ volumes_dir 配置但目录缺失/空→validate Err；⑦ 卷模式+进程级卷作用键混用→Err；⑧ 两卷同盘符→Err；⑨ 既有单卷 config 全部测试零漂移（跑全量比对计数）。
2. 绿：键分类常量 + `VolumeConfig` + `discover_volumes` + validate 分支 + cli discover 接线（卷模式返回进程配置+卷清单）。
3. 验收：`cargo test -p cloudkit-core -p cloudkit-cli --no-fail-fast` 新测试绿+旧测试零漂移；workspace 三步门禁。
4. Commit：`feat(phase2-5): MV0 volume config files, discovery and validation`

### MV1 Registry 装配核心（cloudkit-cli/lib.rs + main.rs）

**Files:** Modify `crates/cloudkit-cli/src/lib.rs`（run_with_transport_options 拆 per-volume 构建函数 + VolumeRegistry + RunHandle 聚合）、`src/main.rs:531-612`（dispatch 循环+横幅）、`src/lib.rs:684-693/1083`（session/baidu sessions 基准目录参数化）；Test `crates/cloudkit-cli/tests/multivolume_e2e.rs`（新）。

1. 红：① 两 mock 卷同进程（run_e2e 基建扩展：tempdir+config.toml+volumes/a.toml,b.toml+两个 MockTransport）——卷 A 上传后卷 B list 不可见、两卷 db 文件分居各自卷目录、queue_stats 按卷独立；② 失败可见——三卷其一构建失败→registry 状态 failed{reason}+其余 running；全部失败→Err 非零退出语义；③ stop gate：一次 stop 全卷停（含 sync task/队列 worker drain 语义沿用）；④ telegram session 落卷目录（mock grammer 不起真连接，断言传参路径）；⑤ 单卷回归：run_with_transport 现测试零漂移。
2. 绿：`VolumeRegistry{volumes: Vec<VolumeRuntime>}`（VolumeRuntime=spec+transport+vfs+status）；`run_multi_with_transports(process_cfg, specs)` 供测试注入；生产 run() 从 discover 构建；RunHandle 聚合 webdav/web_ui/卷清单/mounted letters/stop。
3. 验收：cli 测试全绿；三步门禁。
4. Commit：`feat(phase2-5): MV1 volume registry assembly with per-volume isolation`

### MV2 WebDAV 前缀路由 + 逐卷挂载（cloudkit-webdav + cli 装配）

**Files:** Modify `crates/cloudkit-webdav/src/server.rs`（增 serve_volumes）、`crates/cloudkit-cli/src/lib.rs:479-497,1685-1764`（单端口装配+mount URL）；Test `crates/cloudkit-webdav/tests/multivolume.rs`（新）。

1. 红：① serve_volumes([('a',fs_a),('b',fs_b)]) 单端口——PUT `/vol/a/x.txt` 仅卷 A 可见；GET `/vol/b/x.txt` 404；PROPFIND `/vol/a/` 与 `/vol/a` 双形态 207；无前缀 PUT `/x.txt` 不落任何卷（404）；② 前缀不泄漏：驱动/mock fs 收到的路径不含 `vol`（R1 断言）；③ 逐卷 CyDriveFs 独立（staging 目录互不干扰）。
2. 绿：axum Router + `nest_service("/vol/<name>", DavHandler)`（dav-server Service 兼容形态；若 crate 不实现 tower Service 则包一层调用 `DavHandler::handle` 的 adapter——双 URL 形态测试钉死）；旧 `serve()` 零改动。
3. cli 装配：多卷模式 serve_volumes 单端口；`default_mount_url` → per-volume `/vol/<name>`；mount_if_configured 逐卷（盘符偏好链沿用）。
4. **真机探针（风险前置）**：local 单卷起 serve_volumes 形态，`net use V: http://127.0.0.1:<port>/vol/<name>`，Explorer 空PUT→LOCK→PUT→PROPPATCH(207) 链路+往返读。失败 → 启用 K20B 回退（每卷端口），记 decisions 并修订 K20/K24/K27 相关验收。
5. 验收：webdav 测试绿+探针通过记录；三步门禁。
6. Commit：`feat(phase2-5): MV2 single-port webdav volume routing and per-volume mounts`

### MV3 Web 仪表盘多卷（cloudkit-web + 前端）

**Files:** Modify `crates/cloudkit-web/src/lib.rs`（AppState/WebUiConfig/路由）、`templates/index.html`、`static/js/app.js`、`static/css/style.css`；Test web crate 集成测试。

1. 红：① `/api/volumes` 返回卷清单（含 status/quota/db 统计——mock 两卷断言字段）；② `/api/stats?volume=a` 返回该卷 16 键（冻结键逐键断言）；③ 多卷无参 → 400+`{"volumes":[...]}`；④ 单卷模式无参 → 现 16 键回归；⑤ `/api/files|list|queue|upload|delete|download` 带 volume 参数路由到对应卷 vfs。
2. 绿：AppState{registry, process_cfg}；WebUiConfig 拆进程级+卷清单注入；handlers 增 volume 解析（K23 语义）。
3. 前端：卷切换 tabs（单卷模式不渲染）+ 汇总卡（Σfiles/bytes 跨卷）+ 全部 fetch 带当前卷参数；挂载指引按当前卷显示 webdav_url/drive_letter。
4. 验收：web 测试绿+前端冒烟（起实例人工核验留证）；三步门禁。
5. Commit：`feat(phase2-5): MV3 multi-volume dashboard with volume switching`

### MV4 CLI 运维面 + 文档联动

**Files:** Modify `crates/cloudkit-cli/src/main.rs/doctor.rs/setup.rs`；文档 README.md/AGENTS.md/standards/driver-onboarding.md（§4 核对）/decisions.md（K19–K29 入档）。

1. 红→绿：① `cydrive volumes` list（扫 volumes_dir：卷名/backend/drive_letter/enabled/上次运行状态不可得时不臆造）；② doctor 逐卷检查（卷目录可写/db/凭据解析/盘符与端口冲突）；③ status 多卷（逐卷 db stats 表）；④ setup 多卷骨架引导（生成进程 config+volumes/local.toml 示例，最小实现）。
2. 文档联动：README 快速开始增三卷示例；AGENTS 阶段推进+陷阱增补（MiniRedir 子路径挂载实测结论、卷模式 env 收缩）；onboarding §4 与 K19 对表核对。
3. 验收：三步门禁+`scripts/check_layers`+`scripts/scan_secrets`。
4. Commit：`feat(phase2-5): MV4 cli volume ops, doctor and docs`

### MV5 E2E 硬验收 + 收口

1. 真机单进程三卷：local（卷目录 root）+ telegram（负责人测试配置，生产 chat 污染授权沿用 2026-09-08 §7a 例外）+ baidu（token）；V/Y/Z 三盘并存；验收项：三盘 Explorer 上传/下载/复制互不干扰（含跨盘复制）、空 PUT 链路 PROPPATCH 207、仪表盘切换+汇总正确、加密卷（tg）独立加解密、`cydrive stop` 一次全停+三盘符卸载。
2. E2E 纪律：写操作限定 `/_e2e_*` 前缀卷内路径；收尾清理（远端 `_e2e_` 目录+本地临时）；报告脱敏入 `docs/reports/2026-09-08-phase2-5-e2e.md`。
3. 收口：worktree merge 回 main、push origin、AGENTS/README 计数与阶段更新、tracker 全绿、decisions 补齐执行期裁决。
4. Commit：`feat(phase2-5): MV5 three-volume E2E hard acceptance` + merge commit。

---

## §4 风险与回退

| 风险 | 概率 | 缓解/回退 |
|---|---|---|
| MiniRedir 子路径挂载兼容性（K20 主案） | 中 | MV2 步骤 4 真机探针**前置**；失败走 K20B 每卷端口（webdav crate 零改动，改动面仅 cli 装配与 URL 生成），decisions 记录 |
| dav-server 与 axum Service 集成形态 | 低 | handle() adapter 兜底，双 URL 形态测试钉死 |
| 控制面聚合改动破坏单卷 `cydrive stop` | 低 | 单卷路径零改动原则+回归测试；多卷控制文件独立新增 |
| 盘符/端口资源冲突 | 低 | validate 静态校验（盘符重复/端口相同）+运行时 mount 失败走 K22 可见降级 |
| grammers/rusqlite 符号共存 | 已排除 | 研究确认：vendored 共符号仅禁第二份 SQLite，多连接无冲突 |
| 大范围重构引发 741 测试漂移 | 中 | 每批「单卷回归零漂移」为验收项；run_with_transport_options 只拆函数不改语义 |

## §5 收口清单

- [ ] K19–K29 全部入档 decisions.md（含执行期修订）
- [ ] tracker（docs/tracking/phase2-5.md）全绿+证据链
- [ ] README/AGENTS/onboarding 文档联动
- [ ] workspace 三步门禁 + check_layers + scan_secrets 最终跑（真实输出留证）
- [ ] E2E 报告（脱敏）+ 清理确认
- [ ] merge main + push origin
