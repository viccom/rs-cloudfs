# Phase 6 pan123 合入前深度审查——发现清单（2026-09-21）

> 审查对象：worktree `feat/pan123-driver` @ `1d18819`（123-0…123-5 十提交，~1.8 万行）。
> 方法：hub-and-spoke 五路子代理精读（认证/API 客户端面、读路径、写路径、测试与桩面、集成装配面）+ 主会话门禁复跑与关键发现逐条代码复核。**只审未改**——零实现代码变更；修复批另行裁决。
> 主会话已直接复核原文的发现标注 ✅（其余为子代理引文 + 交叉验证，未逐条主会话复读）。

## 0. 门禁复核（真实输出）

| 门禁 | 结果 | 证据 |
|---|---|---|
| `cargo test --workspace --no-fail-fast` | **170 suites / 1360 passed / 0 failed / 42 ignored** | 复跑日志（worktree 本地 target）；与 K77.7 声称终态 1360/0/42 逐字吻合 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 绿（exit 0，日志零 warning/error） | 同上 |
| `cargo fmt --all -- --check` | 绿（FMT_OK） | 主会话直跑 |
| `scripts/check_layers` | 绿（15 manifests / 6 driver crates，无 R1 违例） | 主会话直跑 |
| `scripts/scan_secrets` | 绿（全树零命中） | 主会话直跑 |

**执行期事件（方法论记录）**：workspace 测试首跑 E0786（invalid metadata：cloudkit_cli/ck_baidu/cloudkit_core/tokio）——AGENTS「K73 双指纹/内存压力」已知陷阱同族（123-2 批次日志亦记载同形）；冷 worktree target 首跑即触，`cargo clean -p cloudkit-cli -p cloudkit-core -p ck-baidu` + `-j 2` 定向疗法一次治愈。**非代码缺陷**（其后 clippy 与全量测试均绿）。

## 1. 发现总览

**High：0。Medium：12。Low：~27（合并同类）。**

无阻塞合入项；建议合入前（或首批修复）处理：**M1 / M6 / M7 / M9 / M12**（正确性风险面 + 两行顺手项），其余入挂账修复批。

## 2. Medium（12 条）

### M1 重试引擎对全部 POST 无幂等区分——提交点双应用可把「已成功」误报为失败 ✅
`crates/drivers/ck-pan123/src/api.rs:566-718`
dispatch 重试触发面不区分方法幂等性：(a) 5xx（请求必已到达）；(b) 传输错误含 **body 读失败**（`api.rs:693-700`——响应头已收，服务端可能已执行）；(c) 粘性 failover 重放（`api.rs:663` 仅 `is_connect()`，请求未发出，安全）。逐端点双应用后果：
- **upload_complete/v2（提交点，`api.rs:1187`）**：第一次已入库但响应 5xx/读败 → 重试 → 会话已消费 → 服务端错误形态 → `close()` 上抛 Err → **挂载面报「写入失败」但文件实际已上传**。自愈路径存在（本地记录→SessionGone→清→重走，全量重传+dup2 覆盖代价）。
- **s3_complete_multipart（`api.rs:1155`）**：仅容忍 MalformedXML，其他已消费形态直接上抛，与上条叠加成提交点双暴露。
- **upload_request（`api.rs:965`）**：5xx 重试若首次已受理 → 零片孤儿会话（123 无 abort 端点，挂账⑩放大）。
- **trash（`api.rs:853`）**：双 trash 第二次形态真机未采样；回读校验只兜「静默失败」不兜「重试误报」。
- **mkdir（`api.rs:822`）**：双发 → 第二次 5060 → `Exists` 上抛，ensure_parents 失败（下次 list 预检自愈）。
- rename/mod_pid 天然幂等无害。
**修复方向**：提交点族（v2/s3_complete/trash/mkdir）提供不重试通道（dispatch 加幂等标记），或 v2 失败时先 `file_info` 回查真伪再定终态；至少补桩测试钉住现状。

### M2 非 2xx（非 429/5xx）HTTP 状态被完全忽略 + `Envelope.code` 缺省 0——网关类 JSON 错误体误判成功 ✅
`api.rs:678-718` + `api.rs:97-104`
只拦 429/5xx；403/418/404 等直接进 envelope 解析，且 `code: #[serde(default)]`——**无 `code` 键的 JSON 体（WAF/网关的 `{"error":"forbidden"}`）解析为 code=0 → `is_ok()` 放行**，下游报 "missing" 类误导错误、真实拒绝原因丢失。`read_envelope`（`api.rs:174-189`）同形态。errno_mapping 测试未覆盖此形态。
**修复方向**：HTTP 非 2xx 时拒绝 envelope 成功判定（状态码并入错误文本）；或 `code` 键要求显式存在。

### M3 FileEntry 时间字段显式 null → 整页 list 解析失败（单条目毒丸）✅
`crates/drivers/ck-pan123/src/models.rs:121-146`
`deserialize_unix_secs` 的 Visitor 只实现 i64/u64/f64/str，无 `visit_unit`——JSON null 直接 `Err`；`#[serde(default)]` 只管键缺失不管显式 null。`list_page` 单道 `from_value::<Vec<FileEntry>>`（`api.rs:780`）——**服务端任何一条目回 `"UpdateAt": null` 即毒死整个目录列表**（对照 sftp K67 服务端形态容错哲学）。
**修复方向**：Visitor 补 `visit_null → Ok(0)`。

### M4 DlinkCache 无条目上界、无 LRU、过期条目永不淘汰（M-S5 同族）✅
`crates/drivers/ck-pan123/src/download.rs:66-95`
HashMap 无容量检查；`get` 的 TTL filter 只影响命中不删条目；除同键覆盖与 delete 显式 invalidate 外永不移除。长期挂载进程扫库大量文件时无界增长（~300B/条）。同文件族 pathcache.rs 已按 M-S5 落 `DIR_CAP=1024`+最旧驱逐——dlink 面复制了 **pan115 未修的先例**（K73 只修了 pathcache）。
**修复方向**：对齐 pathcache 的容量上限+最旧驱逐；顺带清 pan115 同款。

### M5 traffic/check 自身失败 → reader 硬错（fail-closed），与 probe 面同端点降级分叉，零测试 ✅
`download.rs:222`
`client.traffic_check(...).await?` 把检查端点任何失败（传输错/未映射码/非 JSON）当作下载硬错——检查端点病态时本可成功的下载被单点杀死。同端点在 probe 面（`lib.rs:690-701`）尽力而为降级 `None`。**同仓两面对同一端点一个 fail-open 一个 fail-closed，无裁决记录无测试钉**（桩 `traffic_check_code` 旋钮只被 probe 用例用过）。D5 红线「限额不绕过」由 `isTrafficExceeded:true` + download_info 5113/5114 两道拦截保证，检查端点故障放行不违反 D5。
**修复方向**：非限额信号降级放行（download_info 兜底），或补契约测试钉死 fail-closed 是有意裁决。

### M6 206 窗口应答无字节长度/Content-Range 终点校验——越窗多给字节直透消费者 ✅
`download.rs:355-373`
只校验 Content-Range 起始偏移前缀（`bytes {start}-`）。交叉验证先例：**baidu**（`ck-baidu/src/download.rs:236-241`）有 `bytes.len() != expect` 精确长度校验；**pan115**（`ck-pan115/src/download.rs:298`）前缀含窗口终点。123 请求 `[start, win_end)` 若服务端对 end 钳制/忽略回**超窗 body**，多余字节经 `tx.send` 全部透传——消费者按 `end-start` 拼接（VFS 跨块窗口解密）即错位=数据损坏面。未实证 123 CDN 有此形态（故 Medium 非 High；baidu 加这道防线正因其 CDN 有此怪癖家族）。
**修复方向**：至少校验 `body.len() <= win_end - pos`（截断到窗口）或 baidu 式精确校验。

### M7 2xx 非 206 + JSON 即重定向体——200 全量的 JSON 文件内容可被当协议指令 ✅
`download.rs:375-401`
JSON 重定向分支对**任何 2xx 非 206**（含 200）生效（注释自认）。真机实证形态是 HTTP 210+JSON。当服务端对 Range 回 200 全量且文件本身是 JSON：内容含 `data.redirect_url` 键 → 从内容指定 URL 拉回字节当文件数据；不含 → 报 "CDN JSON body without a redirect url"——真实病因「Range 被忽略」（`download.rs:413-416` 防线）被前置拦截，归因错误。
**修复方向**：JSON 分支限定 `status == 210`（零成本防御；200-HTML 中继腿保留）。

### M8 close 盲信 write 失败遗留的部分传输态——缺片直入 ⑥⑦，size 校验在「服务端回声称 size」形态下失防 ✅
`crates/drivers/ck-pan123/src/upload.rs:618-620,638`
`run_transfer()` 步骤④中途失败上抛后 `transfer` 已是 `Some`（部分 done）且无「已齐」标记；此后 close 只在 `transfer.is_none()` 时重跑——部分失败态直接拿 `t.done` 走 ⑥`s3_complete`（4 键形不携带分片表，服务端是否拒绝缺片 complete **未经真机实证**）→ ⑦v2。桩的 v2 回会话声称 size → `verify_size` 可能通过 → 半截数据落库成功。**降档说明**：当前生产调用面（transport_face/conformance）write 失败即早退 drop，不可达；pan115 同构（但其 complete 携带 parts 列表，防线强一档）。
**修复方向**：TransferState 加「⑤已确认」标记，close 对未确认态先复跑 ⑤ 对账（与 resume 复用同一逻辑）；真机补「缺片 complete 服务端形态」一腿。

### M9 app.js / system.js 后端标签表漏 pan123 行（顺带 pan115 同欠）——K67「JS 标签表漏行」同族 ✅
`crates/cloudkit-web/static/js/app.js:50-55`、`system.js:52-57`
两表止于 sftp；`/api/stats` 的 `backend="pan123"` 查不到键 → app.js 回退「你的云盘」/ system.js 回退裸串。i18n 键 `backend.pan123` 已备好（i18n.js:174/435）、volumes.js 已入表——恰这两张邻接表漏行。pan115 自 Phase 5 同欠。显示级降级（有回退不误标）。
**修复方向**：两表各补 `pan123` + `pan115` 两行（一次清两代欠账）。

### M10 CDN/PUT 限流退避面：行为不一致且零测试覆盖
`download.rs:135-139,254,184-199` + `upload.rs:714-751`
①退避梯度 1/2/4s 只在 `fetch_window`（后续窗口）——**首窗口与冷解析首 GET 的 429 直接上抛**（恰是每次打开的必经路径）；②PUT 面独立重试循环的 429 臂/`Retry-After` clamp（`upload.rs:740`）零桩用例（write_path 只注入 5xx）；③CDN 面退避后恢复/梯度用尽冷解析/缓存 URL 中途变 Redirect 三形态零测试（桩 mirror 无 429/403/变链旋钮）。跟踪单 123-2 行声明的「403/429 退避梯度」为完成项但无钉。
**修复方向**：首窗口/冷解析复用梯度；桩补限流旋钮钉「梯度重试→成功」与「用尽→上抛」两腿。

### M11 跨父 rename 两步非原子——部分变更无提示无回滚 ✅
`lib.rs:558-573`
mod_pid 成功 + rename 失败（竞态/网络错）→ 文件已在目标父目录但保持旧名——旧路径 resolve NotFound、新名不可见，用户视角「消失」实为移位。无回滚无文案。测试只有成功路径；mod_pid 真机未验（挂账④）。
**修复方向**：rename 腿失败时错误文案明示「文件可能已移至目标目录（保留旧名）」，或 mod_pid 回滚。

### M12 `v2_fail_times` 死旋钮——⑦ upload_complete/v2 失败腿无任何测试
`tests/stub_common/mod.rs:189-191`
全测试文件零使用（write_path 的 resume 第一腿失败实际用 `s3_complete_fail_times`=⑥ 步）。close 里 v2 失败后的行为——会话保留/重开差集续传/v2 重试语义——完全无钉。⑥ 步有测试、⑦ 步没有，提交链最后一公里缺口。`session_part_sizes`（`stub_common:347-355`）同为零使用死 helper。
**修复方向**：补「第一腿 v2 失败 → 第二腿零重传续完」用例（旋钮即为此造），或删死旋钮。

## 3. Low（~27 条，合并同类）

**行为/防御类**：空 206 body 静默截断流无信号（`download.rs:156-158`，随 M6 一并处理）；429 耗尽终态 `retry_after` 透传未 clamp（`api.rs:613-614`）；CDN 退避不消费已解析 Retry-After（`download.rs:322-329`）；退避梯度用尽无条件丢弃可能仍活的 dlink（`fetch_window:205-207`）；list_all 病态形态无页数上限（`pathcache.rs:235-249`）；并发同 fid 双冷解析无单飞（`open_range:127-142`，先例同款）；mkdir/rename 预检命中缓存行不新鲜化——外部删改后 10min 窗口误报 Exists（`lib.rs:450-457,547-554`）；30x 相对 Location 硬错（`download.rs:336-347`）；退避 jitter 固定 ≤500ns 形同虚设且与注释不符（`api.rs:446-457`）；SessionGone "404" 子串过宽（误报方向安全，`api.rs:1229-1231`）；dydomain 产物无域白名单（`api.rs:342-345`，对照 pan115 normalize_endpoint 教训）；Retry-After HTTP-date 形态不解析（安全降级，`api.rs:685-687`）；`Envelope: Debug` 派生是未来泄漏面（当前零打印点已核）；qr `data.url` 缺失静默空串渲染废码（`oauth.rs:114`）；QR 确认防御臂只认小写 token 键（`oauth.rs:142` vs `parse_token` 双拼）；QR 轮询无总超时（`setup.rs:662-696`）；failover 重放不重新过令牌桶（`api.rs:649-677`）；`cid.parse().unwrap_or(0)` 静默归网盘根 ×3（`upload.rs:279,802`、`lib.rs:562`——当前不变式下不可达，防御方向应报错）；repare 签 [首缺片,末缺片+1) 全区间非逐连续区间（`upload.rs:419-421`，万片文件极端形态未测，挂账⑨子面）；`up_file_id` 顶层只认数值形态与其他字段双形态不对称（`api.rs:1031-1035`，v2 发 0 会以 file_info 缺失 Unavailable 揭出）；会话 tmp 残留/并发同键互踩（`upload.rs:179-182`——损坏自愈存在，pan115 同款继承）；掩码空针 replace 理论路径（`api.rs:708`，生产不可达）。

**测试/桩类**：conformance.rs:210-211 与 write_path.rs:18-19 头注释仍是 K77.1 已证伪的旧 resume 模型（断言正确、注释误导——write_path.rs:324-327 已是正确表述，两文件不一致）；`session_part_sizes` 死 helper（随 M12）；`dlink_cache_expires_and_invalidates` 名不副实——TTL 过期分支无注入缝（`DLINK_TTL` 常量无 `with_ttl`，对照 pathcache 有缝有测）；write_path `content()` u8 LCG 周期仅 8——同 size 用例桩 Reuse 碰撞隐患（live 已换 64 位 LCG，桩面未跟进）；s3_complete 的 MalformedXML 容忍分支无测试（真机「无害」形态语义变化无从发现）；「本地记录在但服务端已消费」resume 臂无离线测试（`upload.rs:307-317`）；真机测试根缺省 "0" 与自身引用的 K74 教训矛盾（`live_matrix.rs:103`/`pan123_e2e.rs:66`，忘设 env 时矩阵在网盘根建删）；/v2 桩「严格 7 键」实为「至少含 7 键」且不校验值形态（`stub_common:1253-1264`）；`pacing_scales_with_permits` 相对断言无下界差保证（`limiter.rs:73-76`，理论 flaky）；mkdir 面 type≠1 桩不拒（`stub_common:572-595`，靠下游间接红）。

**集成/文档类**：doctor root 显式 `"0"` 时文案仍称 "is unset"（`doctor.rs:536-543`）；README「36 键全览」引用过期（`README.md:53`，sftp/pan115 批起即欠，先例遗留）；e2e 默认 root="0" 时 `_e2e_pan123/` 集合目录残留（文件全清仅空目录，`pan123_e2e.rs:117-130`）。

## 4. 核对过且干净的关键面（摘）

- **K77.1 resume 修复扎实**：本地五元组优先（load→list→差集/gone 清记录 re-request 恰一次不自陷）；桩同步改真值建模（有片会话重发铸新）；live_matrix 服务端对账级验证。
- **K77.2 trash 修复扎实**：四键载荷（operation:true）+ 桩 400 真形 + 回读校验三件齐。
- 七步严格序时序断言钉死；**duplicate 全仓唯二值 None/Some(2)**（无任何路径发 1，D4 达成）；MD5 无条件本地重算（`hint.content_hash` 生产代码零出现）；分片数学（5MiB/48.8GiB 边界/余量/Content-Length 同源）。
- errno 表与测试一一对应；envelope 双成功码分脸；K76.4 无 refresh 契约（Unauthorized{false} 恒不重试、无回写桥）。
- D5 合规：无安卓头、无签名、无 URL 重写绕过（测试断言钉）；transfer/API 双会话分离。
- 三跳解析主干（params 自解码零 GET/210 JSON/封顶/负证断言）；粘性 failover 恰一次不回切；dydomain 三腿回退。
- R1 全绿（core/web 零 ck_pan123 符号，cli 引用全 feature 门控）；M-I1/M-I2 同族对 pan123 结构性免疫（combo_dispatch 双面钉）；四处键清单/六处 volumes 前端表/K28 env 纪律齐全。
- 凭据卫生：scan_secrets 全树零命中 + 代码面 rg 双证（token/密码/etag 不入日志错误串；`Pan123Params` 无 Debug；sign_in 密码三防线）。
- 断言漂移全链复核：仅两处改动均有据（conformance ④ 契约演进 commit 2f0987c；upload_request 2→1 = K77.1 commit 8ecf790）——无第三处无据漂移。
- 桩保真 14 项真形要点逐项对齐（trash 四键/v2 严格+静默/s3 键大小写三分叉/会话重发规则/dup1 副本真相建模/Reuse 形态/字符串数字形态/envelope 双码/时间双态/210 JSON/`/download-v2` 负证/传输裸面/web 头组）。

## 5. 未覆盖/存疑点（如实）

1. **六组合裁剪构建未复跑**（M-I2 验收面）——结构审查可信（`let _ = (spec, home)` 消警 + pan115 先例逐字镜像 + combo 测试钉），但本次只跑了默认 feature 全量；合入后主仓跑全量门前注意共享 target 清理（K73 教训）。
2. **conformance 八断言与 live_matrix/e2e 用例内容**未逐断言复核（三腿审查分别覆盖了 harness 声明诚实性与 K72/凭据纪律抽验，非逐行）。
3. **真机挂账 11 项维持**（跟踪单终态挂账总表——5113/5114 真身、mod_pid 跨父、空文件链、dlink TTL 真值等），与本审查无冲突。
4. M8 定级依赖的未知数：**缺片 complete 的服务端真形**（拒绝还是接受、file_info.size 是声称值还是实际值）——需真机一腿。
5. 双 trash 第二次的真机形态、CDN 越窗多给是否真实存在（M6 升 High 与否的判据）。
6. 「桩照实现抄」全量排查覆盖了主要 wire 面（trash/v2/s3/dup/Reuse/会话规则/envelope），未穷举全部 19 个端点的每一键。

## 6. 结论

**无 High。实现与计划 §5 硬纪律、K76/K77 两代真机教训的修复质量整体扎实；门禁五项独立复跑全绿（1360/0/42 与声称吻合）。** 12 条 Medium 中 M1/M6/M7 是正确性风险面（提交点误报/窗口越界/内容当协议），M9 是两行顺手项，M12 是提交链最后一公里的测试缺口——建议合入前或首批修复；其余与 ~27 条 Low 入修复批裁决。pan115 侧同款欠账（M4 DlinkCache、M9 标签表、M2 同族防御）可一并清理。

## 7. 修复批销账（K78，2026-09-21）——12 Medium 全清

四轮串行（Round A api/models → B download → C upload → D rename）+ M9 主会话直做；逐项 TDD 红→绿留证（证据 = 各轮报告 + 本表）；未 commit 前全量门禁五项复跑全绿。

| # | 修复形态 | 关键证据（红→绿） | 备注 |
|---|---|---|---|
| M1 | `dispatch_post_json_single` 单次通道接入三提交点（v2/s3_complete/trash）；mkdir/upload_request 维持可重试（注释钉裁决） | 红：v2 注入 5xx 旧码**重试成功**（双应用实锤）；绿：恰一次请求+终态上抛；GET 对照腿维持重试 | 自愈故事入 `dispatch_single` 文档注释（SessionGone→清记录→re-request） |
| M2 | HTTP 状态门（is_ok 且非 2xx → Unavailable；envelope 显式错误码仍走 errno 映射）+ `Envelope.code` 必填；`read_envelope` 同门（is_auth_ok 脸） | 红：403+code:0 放行 / 网关无 code JSON 误判 / QR 面同缺陷；绿：errno_mapping 15/0 + 回归守卫（401+envelope → Unauthorized 维持） | errno_mapping 既有注释「空对象不炸」一行过时 → 主会话顺手修正 |
| M3 | 时间 Visitor 补 `visit_unit`/`visit_none` → Ok(0) | 红：`"UpdateAt": null` 毒死整页；绿：null→0 + 整页不炸 + 缺键 default 维持 | — |
| M4 | `DLINK_CAP=1024`：先清过期仍满则驱逐最旧（pathcache 先例同款） | 红：cap+1 插入最旧未被驱逐；绿：驱逐+最近保留 | — |
| M5 | traffic/check 三臂：超额硬停维持 / `Err` → warn 降级放行（与 probe 面对齐） | 红：未映射码 555001 杀死下载；绿：放行+数据正确；既有硬停腿零回归 | D5 不受影响（限额拦截在 isTrafficExceeded + download_info 5113/5114 两道） |
| M6 | window_get 206：`body.len() > expected` → 错；空体且 expected>0 → 错；**少给非空续窗保留**；消费循环空窗防御层 | 红：越窗 16B 透传「成功」/ 空 206 静默短流；绿：两形态可观测错误 + 多窗/Range 零回归 | **Low L1 一并销**（空 body 静默截断） |
| M7 | JSON 重定向分支限定 `status == 210`；200+JSON 落 Range-ignored 防线 | 红：文件内容 `data.redirect_url` 被真实跟随（内容注入实锤）；绿：不跟随+归因正确 | — |
| M8 | `TransferState.confirmed` + `reconcile_parts` 公共 helper（③④⑤抽取）+ close 未确认态先 `refresh_present_from_server` 再复核 | 红：④中途失败后吞错 close **报成功**（桩 v2 声称 size 骗过校验，远端 416）；绿：复核补缺+远端逐字节+seq 钉时序 | 正常腿不加双 list（七步序契约零漂移） |
| M9 | app.js/system.js 两标签表各补 pan115+pan123 两行（i18n 键已在） | rg 复核四处表齐 | 主会话直做；顺带清 pan115 先例欠账 |
| M10 | a：`window_get_backoff` 首窗/冷解析梯度（接线 open_range 缓存命中 + follow_hops 每跳）；b：PUT 429 恢复+耗尽两腿（桩 `put_429_times` 旋钮） | a 红：首窗 429 裸上抛/仅 1 次尝试；绿 3/3。b：恢复腿 PUT=分片+2、耗尽腿 =分片+6 提交族零命中 | M10a 真睡 3s/测试（paused-time 弃用：tokio test-util 不在 dev-deps + 120s 超时定时器风险，取舍留档） |
| M11 | rename 失败腿（仅跨父）经 `partial_move_error` 拼接 hint（载荷型 Io/Unavailable 同形拼接、契约类原样透传+warn）；失败路径两父级缓存失效 | 红：部分变更无提示；绿：文案含旧名/目标目录 + 桩状态直查（目标父旧名行/源父空）+ 旋钮解除后新位置重试成功 | 不做 mod_pid 回滚（回滚自身可失败，裁决入 K78） |
| M12 | `v2_fail_times` 激活：第一腿⑦失败 → 第二腿恢复链钉死 | 表征绿：SessionGone（⑥已消费）→清记录→re-request 恰一次→全量重传→成功+逐字节（恢复链=「假失败+干净恢复」，注释留档为何无法差集） | 顺带删死 helper `session_part_sizes`（Low L1 第二处） |

**随批顺手项**：errno_mapping 过时注释 1 行（M2 关联）；`session_part_sizes` 死 helper 删除。**Low 其余 ~25 条未动**（本批范围 = Medium 12 条；Low 留待后续裁决——含 pan115 侧同款 DlinkCache 上界欠账，注：M4 已在 pan123 侧清偿，pan115 侧仍未清）。

**终态验证**：workspace **1382/0/42**（+22 测试）、clippy `-D warnings` 绿、fmt 绿、check_layers 绿（15 manifests/6 驱动）、scan_secrets 绿。

## 8. Low 收尾批销账（K79，2026-09-21）——有价值 Low 全清

范围 = 负责人批准的「建议修 8 条 + pan115 DlinkCache 顺带 + 零破坏顺手项」。两路并行子代理（pan123 八项 P1–P9 / pan115 两项 Q1–Q2）+ 主会话三处直做（doctor 文案/README/pathcache 残留）。

| 项 | 修复形态 | 证据/备注 |
|---|---|---|
| dydomain 域白名单 | `is_allowed_api_host/_base`（loopback 三形态豁免；其余点分后缀命中 `.123pan.cn/.123pan.com/.123278.com` 含裸域 + 必须 https；尾缀伪装不命中）；应用在 resolve_domain 唯一入口 | 红：毒 dydomain 产物经 failover 被激活；绿：回退缺省主域。合法子域接受面由纯函数矩阵钉（hermetic 套件不发真网） |
| QR 防御臂 token 双拼 | `["token","Token"]` 对齐 parse_token | 红：大写 Token + loginStatus:0 漏进 Waiting；绿：Confirmed |
| `cid.parse().unwrap_or(0)` ×4→显式 Invalid | `parse_cid(stage)` helper（error! 通道留原值 + Invalid）；**4 站点**（upload×2/lib rename/lib mkdir——第 4 处系执行期发现的同型站点） | 真实红→绿 ×3 条（pub 结构体绕过 from_pairs 构造非数字 root 可驱动）；生产不可达如实声明 |
| ↳ 同类残留收口 | pathcache.rs:240 list_all 同修（主会话）；**lib.rs:763 probe root_fid 裁决不修**（流量余量账号级、fid=0 功能等价、只读零危害——报错反令诊断面丢信息） | 裁决入本表 |
| 429 终态 retry_after clamp | dispatch/dispatch_single/PUT 耗尽三终态统一 clamp(1s,60s) | 红：9999s 透传；绿：60s/1s |
| CDN 退避消费 Retry-After | `cdn_backoff_delay`（解析值优先 clamp 1–60s，否则梯度）接 fetch_window + window_get_backoff | 红：总耗时 3.03s（梯度）→绿：2.03s（消费 1s×2）；M10a 既有用例维持绿 |
| Envelope 手工 Debug | data 恒打 `<redacted>`（code/message 照打） | 红：`{env:?}` 展开 token；绿：不出现 |
| write_path LCG 64 位 | content() 内部 64 位状态（live_matrix 同款） | 测试基建；write_path 19/19 |
| 陈旧 resume 注释 | write_path/conformance 头部 + lib capabilities 注 + stub_common ×4 处 + live_matrix:298 | 纯注释；conformance:158 零片段表述经核对准确保留 |
| 真机测试根必填 | pan123 live_matrix + pan123_e2e + pan115 live_matrix（含 :257 live_params 硬编码 root="0" 的同族收口）env 缺失即 panic（K74 文案） | #[ignore] 套件离线零影响 |
| pan115 DlinkCache 上界 | DLINK_CAP=1024（M4 方案平移，注释按 pan115 风格重写） | 红 E0425→绿；ck-pan115 全绿 |
| doctor 文案 / README | "is unset (or set to \"0\")"；README 去过期「36 键」计数 | 主会话直做；无测试钉文案（已核） |

**剩余未修（裁决挂账）**：findings §3 其余 ~14 条 Low 维持不修裁决（理论路径/安全方向已确认/纯风格）；lib.rs:763 probe root_fid（上表裁决）；download 终态 retry_after 透出形态（M10a 测试钉死的行为，统一 clamp 属行为变更留裁决）。

**终态验证**：workspace **1394/0/42**（+12 测试）、clippy/fmt/check_layers/scan_secrets 绿；既有断言零漂移。
