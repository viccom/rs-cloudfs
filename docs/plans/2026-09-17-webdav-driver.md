# Phase 7 计划：WebDAV 存储驱动

> 状态：**已批准（2026-09-21 负责人实施指令——该指令即计划批准令，全程自主执行）——批次编号 WD0–WD5，立项裁决入 decisions K80/K81**
> 批准时漂移修正（2026-09-21，主仓 main@eda60a7 起）：①**编号**——计划原拟 K77/K78 已被 Phase 6 占用（K77=收口、K78=审查修复批、K79=Low 收尾批），WD0 立项裁决改 **K80（选型/方案）+ K81（D1–D6 拍板）** 入档；②**序数**——pan123 已合入（main=eda60a7，现 6 驱动），webdav 是**第 7 个驱动**：DRIVER_ROWS 追加第 7 行、Backend 枚举追加第 6 个新变体（现 6 变体）、第 7 个 feature——本计划中「第 6 驱动/现 5 变体」等表述一律按现状执行，§5 WD1 验收基线同理为「六驱动既有输出逐字不变」。
> 前置研究：两轮（2026-09-16 WebDAV 客户端横向调研——OpenList gowebdav fork 深读；2026-09-17 rs-f4ss 深度审计——负责人自有项目，含缺陷清单 `E:\GitHub\rs-f4ss\docs\REVIEW_FIX_PLAN_2026-09-17.md`）
> 上游标准：`docs/standards/architecture.md`（六层 + R1–R7）、`docs/standards/driver-onboarding.md` v1.1 §1–§9（宽面全量公民路线——WebDAV 有按路径枚举面）、`docs/standards/interfaces.md`（StorageDriver 契约/conformance 八断言）、`docs/standards/code-style.md`（门禁/TDD）
> 需求口径（负责人 2026-09-17）：按推荐方案执行；与既有 phase 同策略（计划+跟踪单+TDD）；**编译可选（feature 门控，与其他驱动平权）**；严格遵循项目规范与约束。

## 0. 一句话方案

新增 L1 驱动 crate `crates/drivers/ck-webdav`（宽面 `StorageDriver` + `CloudTransport` 双面，照 ck-sftp 形态），协议层为**自铸薄异步客户端**（reqwest + quick-xml，~800 行）——骨架移植 rs-f4ss 已验证资产（负责人自有项目，零许可负担），以两轮研究的缺陷清单为负面清单正面修法；组合根以 K30 feature 三件套接入 `webdav` 开关（第 6 驱动，`compiled_drivers()` 已表化零重构）；L2 以上零改动，既有五驱动零侵入。

**定位**：WebDAV 是「通用挂载协议」——一朝接入，rclone serve / OpenList/Alist / Nextcloud / 群晖 DSM / Apache mod_dav / 另一个 rs-cloudfs 实例（本仓 cloudkit-webdav 自举）全部成为可用后端。

## 0.1 方法论裁决：以哪份研究为主

**裁决：以 rs-f4ss 的 `backend/common.rs` 骨架为主干，OpenList gowebdav fork 为认证协商的正面结构 + 负面细节清单，rclone webdav.go 为服务端怪癖矩阵情报源，reqwest_dav 评估后不采。**

| 参照 | 与我们的关系 | 采纳部分 | 不采纳部分 |
|---|---|---|---|
| **rs-f4ss（负责人自有）** | 同为 reqwest 薄客户端 + StorageDriver 形态 | **主干**：`HttpClient`（重试白名单/连接池调优/`build_url` 路径穿越防御 + 双斜杠折叠）、超时分层 | Basic-only 认证、`read_full_and_slice` 全量下载回退、无 206 校验（审计缺陷清单逐条正面修） |
| OpenList gowebdav fork | Go，生态里最实战的 WebDAV 客户端 | 401→Digest **恰一次重试**协商结构；流式 PUT 带 ContentLength | digest nc 恒 1 / response-uri 未转义 / challenge 逗号天真解析 / nonce 过期死亡（12h cron 重建客户端兜底）/ parseModified 单格式 / 无超时无校验 |
| rclone webdav.go | 最全的服务端怪癖集合 | vendor 键设计（nextcloud/generic）、`X-OC-Mtime`、PROPPATCH 容错纪律、quota props（RFC 4331）best-effort | OC-Chunked 旧分块协议、锁定/版本扩展 |
| reqwest_dav v0.3.3（2026-03 仍更新） | 唯一活跃的 Rust 异步 WebDAV 客户端 | — | **不采**：能替我们省的（PROPFIND 解析 + Basic/Digest 头部，约 300 行）恰是最薄部分；怪癖矩阵/覆盖映射/窗口纪律/stager 全不覆盖，digest 正确性无从掌控，桩故障注入还要绕它的抽象 |
| RFC 4918/4331/7616/9110 | 协议真值 | 断言依据；手搓桩按 RFC 严格建模（K74「桩照实现抄」第三例的防线） | — |

## 1. 选型与依赖

### 1.1 依赖增量（几乎零新增）

| 依赖 | 状态 | 用途 |
|---|---|---|
| reqwest 0.12（`default-features=false, rustls-tls [+stream]`） | **lock 既有**（ck-pan115 同款特性面先例） | HTTP/WebDAV 动词 |
| **quick-xml** | **新增——本计划唯一新增 crates.io 直接依赖**（MIT，无传递依赖，deny allow-list 已含 MIT） | multistatus 流式解析 |
| md-5 0.10 | **lock 既有**（RustCrypto，与 sha1/sha2/hmac 同族） | Digest MD5（auth） |
| url 2.5 | lock 既有 | URL/Destination 构造与编码 |
| httpdate 1 | lock 既有（ck-pan115 同款） | mtime IMF-fixdate 解析（另两格式手写小解析器） |
| rand 0.10 | lock 既有（ck-pan115 同款） | digest cnonce |
| bytes / futures-core / futures-util / tokio(sync,time,fs,io-util) / async-trait / tracing / cloudkit-storage | 全部既有 | 常规 |
| dev-deps：axum `=0.8.9`（手搓注入桩，ck-pan115 桩同款）+ **dav-server `=0.11.0`（参照桩，版本 pin 与 cloudkit-webdav 对齐防双版本）** + tokio(rt,macros,net) + tempfile + futures-util | 既有 | 测试 |

Basic 头经 reqwest `RequestBuilder::basic_auth` 内建，不需 base64 直接依赖。

### 1.2 测试桩：双桩制（协议真值分离，K74 防线）

| 桩 | 实现 | 职责 |
|---|---|---|
| **手搓注入桩** `tests/stub/` | axum + 内存 VFS，**按 RFC 4918/7616 文本严格建模**（pan115 假开放平台先例形态）+ 注入旋钮：401 Digest challenge（含 stale=true）/恶意畸形 multistatus/Range 请求回 200 全量/MOVE 412/PROPPATCH 405/连接杀/慢滴流/lost-ACK | 行为回归 + 故障注入 + 错误映射钉死 |
| **参照桩** | dev-dep dav-server（真实现、非我们手写）+ LocalFs(tempdir)，装配照 `crates/cloudkit-webdav/src/server.rs` 现成形态 | conformance 八断言注入面 + 独立第二实现（防「桩照实现抄」共享盲区） |

真机再叠两个独立实现（rclone serve webdav / Apache mod_dav，§5 WD5）——离线双桩 + 真机双服务器，四实现交叉。

## 2. 集成前已核实的事实（零侵入依据）

继承 sftp 计划 §2 六条（本批未复跑，WD1 执行时逐条复核锚点）：Vfs 只持 `Arc<dyn CloudTransport>`；写路径无后端分叉；加密在 core 层自动继承；读路径要求 DB 行（外部文件靠 rebuild 收敛——rebuild.rs 零 backend 分支）；`authoritative_index` 只影响横幅；装配是单一 core builder。**增量事实（Phase 5 后新形态）**：dispatch 已统一为 `cloudkit_cli::dispatch_unified_backend_volume` 单一 match（K73 M-I2）——第 6 驱动接臂不再需要 twin 双改，但 twin 组合测试模式保留（WD4）。

## 3. 接入面穷举（13 处，均为追加而非改动）

| # | 位置 | 改动 |
|---|---|---|
| 1 | `crates/cloudkit-core/src/config.rs` `Backend` 枚举 + `as_str()` + `crates/cloudkit-core/src/sync.rs:270` `is_sync_supported` match（扩枚举连带，L2 内） | 追加 `Webdav("webdav")` + sync 门控臂（webdav=true） |
| 2 | 同上 `KNOWN_TOML_KEYS` | 追加 `webdav_*` 六键 |
| 3 | 同上 `VOLUME_SCOPED_KEYS` | 同步六键（bipartition 测试钉完整覆盖） |
| 4 | 同上 `SECRET_VALUED_KEYS` | 追加 `webdav_password` |
| 5 | 同上 legacy json 拒收清单（LEGACY_REJECTED，SF1 执行注记先例） | 同步六键 |
| 6 | 同上 `validate()` | 追加 `backend == Webdav` 校验块（§4.2） |
| 7 | `crates/cloudkit-cli/Cargo.toml` | `ck-webdav` optional dep + `webdav = ["dep:ck-webdav"]` + `default` 追加 |
| 8 | 新 crate `crates/drivers/ck-webdav` + workspace members | 新增（+1 manifest，check_layers 复跑全清单） |
| 9 | `cloudkit-cli/src/lib.rs` `BackendTransport` 枚举 + 方法臂 + `dispatch_unified_backend_volume` 单一 match | 追加 Webdav 臂 + `build_webdav_transport` helper |
| 10 | 同上 `WEBDAV_DRIVER_REQUIRED` 三段式文案（K31）+ `compiled_drivers()` DRIVER_ROWS 追加行（K32；**既有五驱动输出逐字不变**，零漂移基线测试） | 追加 |
| 11 | 同上 `build_driver()`（rebuild，lib.rs:4755 调用点）+ doctor probe 分支（`doctor.rs` + `main.rs` 调用点，sftp :798 先例） | 追加臂 |
| 12 | `with_env_overrides`：`CYDRIVE_WEBDAV_PASSWORD`（**B-M1 教训：env 路由必须走 with_env_overrides，装配点禁直读**）+ `resolve_volume_settings`（webdav 无相对路径键，预期零改动——复核即可） | 追加 |
| 13 | web 卷表单：`volumes.html` radio+字段组 / `volumes.js` 映射表 / `i18n.js` 中英键 / **`app.js`+`system.js` backend 标签表**（K67 B-M4 别漏行） | 追加 |

## 4. 驱动设计要点

### 4.1 crate 形态（七模块）

```
crates/drivers/ck-webdav/
  src/lib.rs            导出 + factory(WebdavParams)
  src/config.rs         WebdavParams（配置 map → 纯函数解析，六键）
  src/client.rs         薄客户端：动词面（PROPFIND/GET/PUT/MKCOL/DELETE/MOVE/PROPPATCH/OPTIONS）
                        + 重试白名单 + 超时分层 + build_url（rs-f4ss 骨架移植）
  src/auth.rs           Basic 预发 + Digest（challenge 解析/nc 计数/cnonce/response）+ 401 恰一次协商
  src/xml.rs            multistatus 解析（quick-xml，命名空间宽容 D:/无前缀双形态）
                        + href 百分号解码 + mtime 三格式
  src/driver.rs         StorageDriver 九方法 + 能力位 + 错误映射表
  src/stager.rs         commit-on-close writer（§4.6）
  src/transport_face.rs CloudTransport 薄壳（照 ck-local/ck-sftp）
```

模块差异注记：**无 `error.rs`**（ck-sftp 有——SSH 错误是字符串形态，需 `CONNECTION_LOSS_MARKERS` 单一来源清单；reqwest 错误是**类型化**的 `is_connect/is_timeout/is_decode`，映射逻辑分别住 client.rs 与 driver.rs 即可，无字符串信标清单需求）。setup 向导无新分支（无交互式鉴权面——凭据即用户名密码，toml/web 表单可完成全部配置；sftp SF3 判例口径）。

### 4.2 配置键（六键 × 四处清单）

| 键 | 必填 | 语义 |
|---|---|---|
| `webdav_url` | ✔ | http(s) 完整 URL，**可含挂载子路径**（子路径即卷根——无独立 root 键，YAGNI）；末尾斜杠容忍；host 非空 |
| `webdav_username` | 与 password 成对 | 认证用户名 |
| `webdav_password` | 与 username 成对 | SECRET；env 链 `CYDRIVE_WEBDAV_PASSWORD`（env > file > keyring） |
| `webdav_auth` | 缺省 `auto` | `auto`/`basic`/`digest`；auto = Basic 预发 → 401 Digest challenge 协商 |
| `webdav_vendor` | 缺省 `generic` | `generic`/`nextcloud`；只影响 mtime 写策略（§8-D4），v1 不嗅探 |
| `webdav_accept_invalid_certs` | 缺省 `false` | 自签 NAS 场景开洞；true 时启动 warn + doctor 提示；非凭据键不入 SECRET 清单 |

validate 拒：URL 缺失/非 http(s)/host 空；username/password 只给其一（rs-f4ss `build_auth_header` 同款成对规则）；auth/vendor 非法枚举值。可行动文案照 K31 风格。

### 4.3 能力位（R4 逐位依据）

| 位 | 取值 | 依据 |
|---|---|---|
| `range_read` | **true** | HTTP Range 原生（206 校验 + 200 截断回退，§4.7）→ 支撑 K47 流式播放 |
| `resume` | false | 无远端分片会话（PUT 整体原子）；上层 `.enc.tmp` 承担 |
| `multipart` | false | 无分块上传原语（OC-Chunked 明确不做，§7） |
| `server_side_move` | **true** | MOVE 原生（服务端单侧） |
| `rapid_upload` | false | etag 非普遍稳定，不做内容寻址去重 |
| `authoritative_index` | **true** | 远端即真相（同 local/baidu/sftp/pan115）→ rebuild 可用 |
| `change_feed` | false | 无变更推送 |
| `inbound` / `chat` | false | 非 bot 后端 |
| `remote_delete` | **true** | DELETE 经 transport 面真删（WebDAV DELETE 即终删——协议无回收站；Nextcloud trashbin 是服务端行为不可依赖） |

quota 方法：RFC 4331 `quota-used/bytes`、`quota-available-bytes` PROPFIND best-effort，失败 `None` 降级（sftp quota None 降级先例）。

### 4.4 错误映射表（R2，双桩回放钉死）

| WebDAV/HTTP 形态 | StorageError |
|---|---|
| 404（认证协商后） | `NotFound` |
| 401 协商后仍拒 / 403 | `Unauthorized { recoverable: false }` |
| NTLM/Negotiate challenge（不支持） | `Unauthorized { false }` + 可行动文案（明示不支持） |
| 405（MKCOL 目标已存在） | `Exists` |
| 409（MKCOL 父缺失） | 驱动内隐式建父后重试一次；仍败 → `NotFound`（父） |
| 412（MOVE `Overwrite: F`） | **重 stat 复核后** `Exists`——K75-1 纪律：只认显式 precondition，传输类 Io/Unavailable **绝不**映射 Exists |
| 416（Range 越界） | stat 复核：offset ≥ size → 空流（EOF 语义，对齐 sftp）；否则 `Unavailable`（保留原文） |
| 200 应答 Range 请求（服务器忽略 Range） | body 覆盖请求区间 → 截断继续；不足 → `Io`（保留原文） |
| 429 / 5xx（重试耗尽后） | `Unavailable`（保留状态码） |
| connect/timeout/连接中断 | `Unavailable`（与 with_retry 重试白名单协同） |
| multistatus 内层 per-resource status | 逐条解析映射（207 整体成功但成员失败按成员映射） |
| XML/编码解析失败 | `Io`（保留原文片段，截断脱敏） |
| 507 Insufficient Storage | `Io`（保留码） |

### 4.5 客户端机制——两轮研究负面清单的正面修法（逐条钉测试）

| # | 缺陷出处 | 正面设计 |
|---|---|---|
| 1 | OpenList digest **nc 恒 1** | per-nonce 单调 nc 计数器（client 内会话级状态） |
| 2 | OpenList digest response **uri 未转义** | 用实际请求 URI（含 query）按 RFC 7616 规范形态 |
| 3 | OpenList challenge **逗号天真解析** | 引号内逗号感知的 `WWW-Authenticate` 解析器（带引号参数），样本集单测钉死 |
| 4 | OpenList **nonce 过期死亡**（12h cron 重建客户端兜底） | 401 + `stale=true` → 换新 nonce 重算**恰一次**；无 cron 无死亡形态 |
| 5 | OpenList parseModified **单格式静默回 epoch** | 三格式：IMF-fixdate（httpdate crate）/ RFC850 / asctime（手写小解析器），失败 debug 日志不静默 |
| 6 | OpenList **无 206 Content-Range 校验** | 逐窗口校验 `Content-Range` 与请求吻合；§4.4 的 200 截断回退 |
| 7 | OpenList **无客户端超时** | 分层：connect 15s / 控制面（PROPFIND/MKCOL/MOVE/DELETE/PROPPATCH/OPTIONS）30s / 窗口 GET 120s（8 MiB 有界 → 超时安全） |
| 8 | rs-f4ss **Basic-only** | D1 双形态协商 |
| 9 | rs-f4ss `read_full_and_slice` **全量下载回退** | 禁用——窗口 GET 是唯一读路径 |
| 10 | rs-f4ss 重试白名单（**正面资产**） | 直接移植：仅 GET/HEAD/PROPFIND 幂等重试（5xx + connect/timeout，上限 3，指数退避）；PUT/MOVE/MKCOL/DELETE/PROPPATCH 永不自动重试 |
| 11 | rs-f4ss `build_url` 穿越 Defense（**正面资产**） | 移植：组件拒 `..`/`.`/`\0` + 百分号编码穿越拒 + 双斜杠折叠；Destination 头 = 绝对 URI 百分号编码（url crate） |
| 12 | K67 **不可寻址名可见** | list 产出过滤：`\`/`\0`/非 UTF-8 lossy 名（href 解码后）——「list 产出即可寻址」 |
| 13 | K74 **桩照实现抄** | §1.2 双桩 + WD5 双真机服务器 |

### 4.6 写路径：commit-on-close stager（与 ck-local/ck-sftp 同构）

- **open_writer(path, hint)**：本地 NamedTempFile spool（tempfile，drop 自动清理）；写入超 hint → `Invalid`（sftp hint 契约同款）
- **close**：spool → `PUT <final>.ckwd-<pid>-<seq>.part`（**Content-Length 已知**——chunked PUT 接受度不定，v1 不赌，D5）→ 覆盖场景 stash 语义 + `MOVE .part → final` → **stat 复核 size == written**（aeroftp 硬仗②）→ generic：PROPPATCH lastmodified best-effort（405/507 静默，D2）/ nextcloud：PUT 时带 `X-OC-Mtime` 搭车 → 清理
- **断言①预案（ck-sftp SF3 判例）**：conformance ①「close 前目标不可见」在**覆盖写场景**要求旧对象也不可见 → sftp 用 `.old` stash 判例；WebDAV 等价物 = open 时若 final 存在 → `MOVE final → .ckwd-*.old`，close 成功删 stash、abort 恢复。**WD3 先跑断言①实测形态，红则按预案上 stash 协议**（带着判决书进场，不预建复杂度）
- **重放窗防线（K67 H2 同型）**：MOVE ACK 丢失重放 → .part 不在 + final 就位且 size 吻合 → 按已提交继续；手搓桩注入 lost-ACK 回放钉死
- **abort**：删 spool + DELETE .part（若已 PUT）；final 从未被直接触碰
- `is_staging_artifact`：`.ckwd-` 前缀过滤（list 集合完整性，断言③）

### 4.7 读路径：有界窗口纪律

- `stat` = PROPFIND Depth 0（404 → NotFound；`resourcetype/collection` 判目录；`getcontentlength` 缺失按 0）
- `list` = PROPFIND Depth 1 → 剔除 self 条目（href == 请求路径，**同时容忍尾斜杠差异**）→ name 字典序（PROPFIND 无分页原语，`Page` 在驱动内切片）→ §4.5-12 过滤
- `reader` = stat 先行（start ≥ size → 空流不开 GET）→ 串行窗口循环 `[offset, min(offset+8 MiB, size))` → `GET Range: bytes=a-b` → 206 校验 / 200 截断（§4.4）→ futures ByteStream 流出；窗口间失败由幂等白名单重试自愈（M-S3 思想的白名单内简化版）
- 窗口 8 MiB 常量（真机吞吐腿校验后可调，不做配置键）

## 5. 批次划分（每批独立可验收）

> 纪律：每批 TDD 红→绿留证；跟踪单 `docs/tracking/phase7-webdav.md` 每批收口更新并随 commit 提交；worktree `feat/webdav-driver`（跨 ≥3 commit → 仓库纪律隔离；**独立 target 目录**——共享 CARGO_TARGET_DIR 双指纹陷阱既有教训）。

### WD0 — 立项落档 + 真机怪癖 spike（无生产代码）
- decisions K76（选型/方案：自铸客户端 + rs-f4ss 骨架 + reqwest_dav 不采）+ K77（D1–D6 拍板）入档；本计划状态 → 已批准；AGENTS 联动（必读/当前阶段/计数）
- **spike（`examples/webdav_spike/`，pan115_spike 先例；WSL2 双服务器）**：rclone serve webdav + Apache mod_dav（含 mod_dav_fs + AuthType Digest）搭 fixture（步骤落 `docs/tracking/phase7-webdav-fixture.md`，凭据只经 env）。钉死怪癖矩阵：①PROPPATCH lastmodified 两服务器行为 ②rclone 是否认 X-OC-Mtime ③chunked PUT 接受度（D5 复核输入）④Range 206/416/忽略-Range-200 形态 ⑤MOVE Overwrite F/T 与 412 形态、目录 MOVE ⑥MKCOL 405/409 形态 ⑦DELETE 204/404/集合递归 ⑧PROPFIND Depth 0/1、空目录 multistatus、命名空间前缀变体、href 编码、getlastmodified 格式 ⑨digest challenge/stale 形态 ⑩Destination 相对 vs 绝对容忍度
- **验收**：decisions + 计划批准状态 + 附录 C（怪癖矩阵表）回填 + fixture 文档；D2/D5 若被证据推翻则修订本计划并留痕

### WD1 — 驱动骨架 + 配置接入（无真实网络）
- `ck-webdav` crate 七模块落地（动词面占位/纯函数层先行：config 解析、url 构造、mtime 三格式、challenge 解析）
- §3 十三处接入 + feature 三件套 + `compiled_drivers()` 第 6 行（**五驱动既有输出逐字不变**，零漂移基线）
- **验收**：workspace 四门禁绿（test/clippy/fmt/check_layers）+ scan_secrets 绿 + 既有测试零漂移

### WD2 — 手搓注入桩 + 客户端核心 + 读路径
- 桩：axum 内存 VFS（RFC 严格建模）+ §1.2 全部注入旋钮
- 客户端：auth 双形态（负面清单 1–4、8）+ xml（5、11）+ PROPFIND/GET Range（6、7、9）+ 重试白名单（10）
- 驱动读面：list（12、13 过滤）/stat/reader 窗口流
- **验收**：读路径行为测试全绿（connect/auth 协商 9+ / read_path 覆盖负面清单逐条）；红→绿留证入跟踪单

### WD3 — 写路径 + conformance
- stager（§4.6）+ 错误映射表钉死（§4.4 全行回放）
- conformance 八断言（**dav-server 参照桩**注入；RESUME 声明门控跳过，sftp 先例）——断言①实测形态裁决（stash 预案在案）
- **验收**：八断言绿（贴输出）+ 断言①裁决留证 + 既有断言零漂移

### WD4 — 装配接线 + 裁剪组合
- §3 十三处全部生效路径 + doctor probe（OPTIONS 探测 + auth 五态）+ web 表单/i18n/标签表 + sync 门控
- 裁剪组合构建（K30 腿）：`local,baidu` / `webdav` / `webdav,sftp` / `not(baidu)+webdav`（**twin dispatch 测试照 pan115_combo_dispatch.rs 模式**，M-I1/M-I2 教训）等
- **验收**：组合构建全过 + 裁剪下 K31 文案测试绿 + workspace 四门禁绿

### WD5 — WSL2 真机矩阵
- 双服务器：rclone serve webdav（明文 Basic + 覆盖全部动词）+ Apache mod_dav（**Digest 腿 + PROPPATCH 腿**）
- 矩阵腿：①上传→回读逐字多档尺寸 ②Range 跨窗口逐字节 + 越界/空流 ③吞吐 128 MiB（上下行）④会话复用 20 次混合 ⑤覆盖写 + staging 窗口不可见 + abort 恢复 ⑥外部文件 list 立即可见 ⑦错误腿（401/403/404/412→StorageError 分类）⑧digest 全链路（nc 递增真实验证）⑨断线重连（白名单自愈）⑩拒绝腿（错凭据/坏 URL 可行动文案）
- 可选加腿：rs-cloudfs→自身 cloudkit-webdav 回环自举冒烟（只作冒烟不作判据——故障归因混淆）
- **验收**：`#[ignore]` 真机套件全绿留证 + 吞吐数字 + fixture 文档完备

## 6. 风险与缓解

| 风险 | 缓解 |
|---|---|
| **服务端实现离散度**（本驱动真正风险面：协议简单、实现五花八门） | 四实现交叉（双桩+双真机）；WD0 怪癖矩阵先行；conformance 只锁契约语义不锁服务器怪癖 |
| quick-xml 解析面被恶意/畸形响应打穿 | 流式解析不整读；畸形 multistatus 样本集测试（手搓桩注入）；错误截断脱敏 |
| 401 协商状态与连接池交互（nonce/nc 跨连接） | auth 状态集中 client 内单一来源；单测钉死 nc 单调性 |
| dav-server dev-dep 版本漂移双版本 | `=0.11.0` pin 与 cloudkit-webdav 对齐 |
| 断言①覆盖写形态与 stash 预案不符 | WD3 实测裁决预案（sftp 判例），不预建复杂度 |
| 共享 target 双指纹 / 页面文件耗尽 | worktree 独立 target + 组合检查后 `cargo clean -p` 纪律（AGENTS 既有） |
| 依赖增量 | 唯一新增 quick-xml（MIT 无传递）；deny advisories 复跑 |

## 7. 明确不做（本阶段范围外）

- NTLM / Kerberos / 客户端证书认证（challenge 明确拒绝 + 可行动文案）
- OC-Chunked（Nextcloud 旧分块）/ 任何分块上传
- 秒传 / etag 内容寻址去重（etag 非普遍稳定）
- 多源并行分片读（D6 起步单连接池；真机吞吐不足再立项，sftp SF5 判例口径）
- 直传/chunked PUT 快路径（D5 挂账；WD0 若证 chunked 普遍接受再议）
- LOCK/UNLOCK、版本控制、Class 2 强制要求（只要求 Class 1 + 基本动词；无锁 PUT 是普遍形态）
- WebDAV Sync (RFC 6578) / 变更推送
- vendor 自动嗅探（D4：显式键，不嗅探）

## 8. 拍板记录（D1–D6，负责人 2026-09-17 会话已批准倾向，随本计划批准正式生效 → decisions K77）

- **D1 认证 = Basic（预发）+ Digest（401 challenge 协商，恰一次重试；stale nonce 再协商恰一次）**。auto 缺省；NTLM/Kerberos 明确拒绝。明文 HTTP + Basic 时启动 warn（凭据裸奔提示）。
- **D2 mtime = 只读真源 `getlastmodified`；generic 不写 mtime**（**WD0 修订 2026-09-21**，触发计划预设降级路径：rclone 对 PROPPATCH 双形态皆 207+内层 403、apache 假成功存 dead prop 真实 mtime 不变——两家均不能真写，证据 = 附录 C ①/fixture 文档）。nextcloud = PUT 携 `X-OC-Mtime` 保留（fixture 无 Nextcloud 未实证、零成本搭车；vendor 键语义不变，仍只影响此头是否发送）。原案「generic PROPPATCH best-effort」撤销——对 rclone 白吃 403、对 apache 制造假成功日志，均无价值。
- **D3 TLS = rustls 严格校验默认** + 卷键 `webdav_accept_invalid_certs`（缺省 false）开洞；true 时启动 warn + doctor 提示。
- **D4 vendor 键 = generic（缺省）/ nextcloud 两值**，只影响 mtime 写策略；v1 不嗅探（Server 头嗅探是脆弱面）。
- **D5 上传 = v1 全量本地 spool → PUT(.part) 带 Content-Length → MOVE 固化**；chunked 直传快路径挂账（WD0 证 chunked 普遍接受再议。**WD0 复核 2026-09-21**：双 fixture 均接受 chunked PUT（附录 C ③），但样本仅二不构成「普遍」——v1 维持 Content-Length 路线，挂账维持）。
- **D6 连接 = 单 reqwest Client（池内并发）**，读窗口 8 MiB 串行；同卷多连接分片留 WD5 实测后再议（sftp SF5 判例）。

## 9. 验收总纲（driver-onboarding §8 清单对照）

- [ ] crate 形态/依赖方向合规（check_layers 绿，+1 manifest）
- [ ] StorageDriver 九方法 + 四维文档 + 错误映射表（双桩回放钉死）
- [ ] Capabilities 逐位依据注码（§4.3）
- [ ] 配置键四处清单同步 + validate 文案可行动（§4.2）
- [ ] conformance 离线八条绿（dav-server 参照桩，贴输出）
- [ ] 真机套件输出（WD5 双服务器矩阵）
- [ ] workspace 四步门禁全绿（test/clippy/fmt/layers）+ scan_secrets + 裁剪组合构建腿
- [ ] 装配点/doctor 分支 + 能力横幅 + `compiled_drivers()` 零漂移
- [ ] 文档联动：README 状态表 / AGENTS 计数 / 本计划 + tracking / driver-onboarding 如需修订

---

## 附录 A：两轮前置研究情报摘要（2026-09-16/17）

### A.1 OpenList gowebdav fork（Go，~1170 行 + 适配层 109 行）

**正面结构（采纳）**：stdlib-only；流式 PUT 带 ContentLength；401→Digest 恰一次重试协商；`Link()` 直链旁路；非可寻 PUT 无缓冲。
**负面清单（§4.5 逐条正面修）**：digest nc 恒 1 / response-uri 未转义 / challenge 逗号天真解析（带引号参数含逗号即碎）/ nonce 过期后客户端永久 401——上游用 **12h cron 重建客户端**兜底（症状治疗典型）/ parseModified 只认单格式静默回 epoch / 无 206 Content-Range 校验 / 无客户端超时 / 无 ctx。

### A.2 rs-f4ss（负责人自有，Rust；深度审计 2026-09-17）

**移植资产**：`backend/common.rs` 的 `HttpClient`（连接池调优 connect 15s/pool 120s/tcp_keepalive 60s）、`should_retry_request` 幂等白名单 + 指数退避（含「PUT/MOVE 不可重试」注释与测试）、`build_url` 路径穿越防御（组件拒 + `%2e%2e` 编码穿越拒 + 尾斜杠 pop + 双斜杠折叠）、drain_response 连接复用纪律。
**审计缺陷（正面修，不继承）**：Basic-only、`read_full_and_slice` 全量下载回退、无 206 校验、moka 缓存层与 EMA 预取不在驱动职责面（本仓 VFS 层已有窗口机械，驱动不复制）。
**合规基础**：负责人自有项目 + sftp 计划需求口径（自用项目借鉴不受许可传染；仍不逐字搬运，按本仓风格重构移植）。

### A.3 rclone webdav.go（怪癖矩阵情报源，不移植代码）

vendor 键三分（nextcloud/owncloud/other）决定 mtime 策略与 `X-OC-Mtime`；PROPPATCH 容错纪律；quota props best-effort；大量服务器 workaround（404-empty-dir、modtime 不可设、Digest 变体）——WD0 怪癖矩阵的核对基准。

## 附录 B：关键源码位置锚点（2026-09-20 已逐条核实；行号为当前 main@bbc38aa，WD1 开工时仍须复核）

- 驱动契约：`crates/cloudkit-storage/src/driver.rs:28`（StorageDriver）、`capability.rs:13`（Capabilities）、conformance 宏在 `cloudkit-storage/src/lib.rs:64`（注入先例 = ck-sftp `tests/conformance.rs`，断言①–⑥⑧、⑦ RESUME 门控跳过）
- 装配：`crates/cloudkit-cli/src/lib.rs`——`*_DRIVER_REQUIRED` 文案族 `:92-128`（现 5 条）、`compiled_drivers` + DRIVER_ROWS `:148-156`（`(bool, &str)` filter 形态）、`BackendTransport` `:4957`、`dispatch_unified_backend_volume` `:5319`、`build_backend_transport_with` `:5440/:5528`、`build_driver` 定义（调用点 `:4755`）
- 配置：`crates/cloudkit-core/src/config.rs`——Backend 枚举 `:227`（现 5 变体）、KNOWN_TOML_KEYS `:67`、LEGACY_REJECTED `:137`、VOLUME_SCOPED_KEYS `:327`、SECRET_VALUED_KEYS `:435`、with_env_overrides `:1637`、validate `:1802`；**sync 门控在 core**：`crates/cloudkit-core/src/sync.rs:270` `is_sync_supported`（非 cli——接入手册 §2 第 6 条的 core 侧连带）
- doctor/setup：`crates/cloudkit-cli/src/doctor.rs` + `main.rs:798`（sftp_connectivity_check 调用点）；`setup.rs`（run_setup_pan115 先例——webdav 无向导需求，见 §4.1 注记）
- conformance：`cloudkit_storage::conformance_suite!`（interfaces §6；ck-sftp `tests/conformance.rs` 注入先例）
- dav-server 参照桩装配：`crates/cloudkit-webdav/src/server.rs`
- rs-f4ss 移植源：`E:\GitHub\rs-f4ss\crates\rs-f4ss-core\src\backend\common.rs`、`backend/webdav.rs`；缺陷清单 `E:\GitHub\rs-f4ss\docs\REVIEW_FIX_PLAN_2026-09-17.md`
- twin 组合测试先例：`crates/cloudkit-cli/tests/pan115_combo_dispatch.rs`（WD4 复制模式）
- fixture 先例：`docs/tracking/phase4-sftp-fixture.md`（WD0 产出 `phase7-webdav-fixture.md` 同形态）

## 附录 C：WD0 spike 输出（2026-09-21 回填；证据全文 = `docs/tracking/phase7-webdav-fixture.md` 怪癖矩阵表）

| 怪癖 | rclone serve webdav v1.60.1 | Apache mod_dav 2.4.58 | 判定/对策 |
|---|---|---|---|
| ① PROPPATCH 写 mtime | 207+内层 403（双形态皆拒） | 207+内层 200 但存 dead prop（真实 mtime 不变）；getlastmodified 形态→内层 409 | **D2 降级：generic 只读 mtime**（§8-D2 已修订留痕） |
| ② X-OC-Mtime | 忽略 | 忽略 | nextcloud 值保留搭车（fixture 无 NC 未实证） |
| ③ chunked PUT | 接受 | 接受 | D5 维持（样本仅二，挂账） |
| ④ Range | 206/钳制 206/416；倒序→416 | 206/钳制 206/416；**倒序→200 全量** | 200 截断回退必须实现 |
| ⑤ MOVE | F→412/T→204；**缺 Overwrite 头+存在→412**（偏离 RFC 缺省 T）；目录 201；**VFS 缓存不可见窗 ≈5min**（子项 404/500、窗内 DELETE 撒谎；数据已落盘） | F→412/T→204；目录 no-slash→301 不执行；slashed→201；缺父→500 | 恒显式 Overwrite；目录腿全尾斜杠；rclone 窗口挂账 |
| ⑥ MKCOL | **已存在→201（幂等成功陷阱）**；缺父 409 | 已存在 slashed→405；缺父 409 | **mkdir 先 stat 预检**（baidu 同型先例） |
| ⑦ DELETE | 204/404/递归 204（尾斜杠不敏感） | slashed 204；no-slash 集合→301 不执行 | 集合 DELETE 尾斜杠 |
| ⑧ PROPFIND | 尾斜杠全不敏感（文件+/也 207）；`D:` 前缀；href 大写 `%C3%BC`+`&amp;` | **集合 no-slash→301**；同文档 `D:/ns0:/lp1:/g0:` 多前缀并存；ISO8601 creationdate | 集合 PROPFIND 尾斜杠；**local-name 解析**；href 反转义+解码 |
| ⑨ 认证 | Basic challenge `realm="rclone"` | Digest challenge；stale=true 位置不定；**nc 重放不查**；过期→401+stale→新 nonce 一次恢复 | 引号感知+顺序无关解析；nc 单调自守；stale 恰一次（D1 实证可行） |
| ⑩ Destination | 相对 URI 接受 | 相对 URI→400 拒 | 恒绝对 URI |
| ⑪ quota RFC4331 | 内层 404 | 内层 404 | None 降级实证 |
