# Phase 7：WebDAV 存储驱动任务跟踪单

> 计划：`docs/plans/2026-09-17-webdav-driver.md` ｜ 需求口径：负责人 2026-09-17「按推荐方案执行；与既有 phase 同策略（计划+跟踪单+TDD）；编译可选；严格遵循项目规范与约束」；**批准：2026-09-21 负责人实施指令（全程自主执行，批次 WD0–WD5 顺延执行）**
> 基线：main@eda60a7（workspace **1394/0/42**，ignored 42 = 真机/平台/真网类；编号漂移修正——立项裁决用 **K80/K81**，webdav 为**第 7 驱动**，见计划头部留痕）
> 状态：**WD4 完成（2026-09-22）——WD5 待开工**
> worktree：`feat/webdav-driver`，独立 target（共享 CARGO_TARGET_DIR 双指纹既有教训）

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| WD0 | 立项落档 + 真机怪癖 spike（无生产代码） | ✅ 2026-09-21 | decisions K80/K81 入档（main `b2d8b67`）；计划落库+批准状态（main `c4a042d`）+ AGENTS 联动（main `da2c4ed`）；`examples/webdav_spike/`（5 模块 + digest/xml 可移植资产）双服务器 11 项怪癖矩阵钉死（附录 C 回填）；fixture 文档落盘（凭据只经 env）；**D2 修订**（generic 只读 mtime，预设降级路径触发）+ D5 维持留痕 | 本批日志 |
| WD1 | 驱动骨架 + 配置接入（无真实网络） | ✅ 2026-09-21 | WD1a：`ck-webdav` 十文件 crate（config/urls/mtime/auth/xml 纯函数层 + client/driver/stager/transport_face 骨架；能力位 R4 逐位依据注码）；45 测试红（42 失败）→绿。WD1b：十三处接入 + **编译器揭出 3 处清单外**（CyDriveConfig 六字段/穷举 round-trip 测试/run_sync_command namespace 臂）全补；feature 三件套 + `compiled_drivers()` 第 7 行（六驱动输出逐字不变，pinned 测试零漂移）+ K31 文案 + twin 双臂 + doctor 骨架臂 + B-M1 env 路由 + web 前端四文件；16 文件 +1048/−25（−25 限 compiled_drivers 测试 cfg 守卫机械重写，pan123 先例） | 本批日志 |
| WD2 | 手搓注入桩 + 客户端核心 + 读路径 | ✅ 2026-09-21 | WD2a 桩（1826 行：RFC 严格 + WD0 真形双 ns 风格整字节钉 + 服务端真实验证 RFC7616 + 10 故障旋钮 + 请求记录器）+ 33 自检；WD2b 客户端（认证状态机恰一次预算/重试白名单/动词读侧/错误映射）+ 读面（stat/list/reader 8MiB 窗口/quota 降级）+ transport_face connect/open；connect_auth 18 + read_path 17 + 纯函数 10 测试，红→绿留证 | 本批日志 |
| WD3 | 写路径 stager + conformance | ✅ 2026-09-22 | client 写侧四动词（恒显式 Overwrite 构造性 bool/绝对 Destination/X-OC-Mtime 仅 nextcloud）+ mkdir stat 预检（rclone201 陷阱）/隐式建父/409 重试 + delete NotFound 恒定 + rename 412→重 stat Exists（K75-1）/缺父三态；stager 严格序（PUT .part→MOVE T→size 复核→清理）+ **断言①裁决=stash 协议上车**（红→绿留证）+ restore_scene + lost-ACK 两半边对账；**dav-server 参照桩揭真缺陷：stat 缺 Depth 头→dav-server 空回应→已修**（双桩制首功）；conformance ①–⑥⑧ 绿（⑦ RESUME 门控）+ error_table 取舍留档；write_path 20 测试 | 本批日志 |
| WD4 | 装配接线 + 裁剪组合 | ✅ 2026-09-22 | 十三处生效路径走查全绿（走查表入批日志）；**挂账①销账**：userinfo 拒收双漏斗（core validate + 驱动 parse_from_map，TDD 红→绿）；doctor probe 真实现（`WebdavProbe` 五态 + `probe()` 两腿编排 + 渲染器纯函数五测 + D3 开洞离线 WARN + main.rs 接线，`TODO(wd4)` 销账）；twin 组合测试 `webdav_combo_dispatch.rs` 四测（M-I2 不误装配/env>file 装配链/namespace 离线推导/组合内成功路径）；rebuild `build_driver` offline 装配断言；裁剪五组合 clippy -D warnings 全过（顺手修 dispatch.rs axum 导入门 pan115 漏导 + pan115_combo clippy lint） | 本批日志 |
| WD5 | WSL2 真机矩阵 | ⬜ | 双服务器（rclone 明文 Basic 全动词 + Apache Digest/PROPPATCH 腿）十腿矩阵：①上传回读逐字 ②Range 跨窗口 ③吞吐 128 MiB ④会话复用 ⑤覆盖写+staging 不可见+abort 恢复 ⑥外部文件可见 ⑦错误腿分类 ⑧digest 全链路 nc 真实递增 ⑨断线白名单自愈 ⑩拒绝腿可行动文案；可选自举冒烟腿（不作判据）；fixture 文档完备 | — |

## 批次日志

### WD4（2026-09-22，worktree feat/webdav-driver）

**完成**：装配生效路径复核收口 + doctor probe 真实现 + twin 组合测试 + 裁剪五组合。前次派发撞使用限额中断的遗留（userinfo 两测试红相位在工作树）本次先确认红再落实现。

- **挂账①销账（userinfo 双漏斗，TDD 红→绿）**：core `validate()` Webdav 块 authority 含 `@` 即拒 + 驱动 `normalize_base_url` 的 `Url::username()/password()` 非空即拒——文案同形（键名指路 webdav_username/webdav_password，明示「凭据不进卷 URL」）；core 侧为第一道漏斗（无 url crate，authority 字符串判定），驱动侧为第二道（url crate 类型化判定）。两测试红（实测输出留证）→绿。
- **doctor probe（`TODO(wd4)` 销账）**：驱动侧新增 `WebdavProbe` 五态枚举 + `probe()`（`WebdavProbe::Alive{dav_class,allow}`/`CredentialsRejected{detail}`/`ReachableNoAuth`/`Unreachable{detail}`/`TlsUntrusted{detail}`）——**两腿编排**：宽校验腿（诊断客户端，证书不校验）失败即定论（401→②读协商路径实记的分类；传输失败→④），网络证可达后跑严格腿，其连接类失败只能是证书校验→⑤（行为分类，零字符串信标）；`accept_invalid_certs=true` 时单腿。401 细分（缺凭据/Basic 拒/NTLM/协商后拒）经 `RejectionReason` 记录面（handle_401 决策表单一来源，`StorageError::Unauthorized` 无载荷契约不动——R3 双通道加第三投影）；detail 复用 R3 文案函数同源。CLI 侧 `webdav_backend_probe`（45s 外墙，baidu 先例）+ `webdav_connectivity_check` 渲染器（纯函数五态→五条可行动文案：①Ok 带 DAV/Allow 摘要 ②Fail 指凭据键 ③Warn 建议配凭据 ④Fail 带 URL/网络/代理清单 ⑤Warn 指路 accept_invalid_certs+安全提示——sftp HostKeyUnpinned 显式接受语义）。D3 离线 WARN 入 `backend_checks` Webdav 臂。`options()` 重构为 `options_probe()` 丢投影薄壳（transport connect 语义零漂移）；**D3 warn 从 `WebdavClient::new` 挪到 `WebdavDriver::new`**（探活诊断客户端不走装配面，warn 跟用户配置走才不误导）。
- **测试取舍**：五态渲染=纯函数+dispatch.rs 单测钉字面（含两头缺席/头摘要断言）；驱动 probe 行为=connect_auth 五测桩上钉（Alive 带头投影/匿名/错密码 digest 拒/basic 模式拒/拒连）；**`TlsUntrusted` 桩不可达**（纯 http 桩无法产证书错误）——编排不变量由代码路径保证，真机自签腿归 WD5；doctor 既有形态无 `#[ignore]` 真机腿，webdav 同制（WD5 一起跑）。
- **twin 组合测试**（`webdav_combo_dispatch.rs`，pan115 模式，`not(baidu)+webdav` 文件级门→默认构建编译为空）：①非 webdav 后端各拿各的 K31 文案且无 webdav 误装配痕迹（per-backend 细分 cfg）②组合内 webdav 卷装配成功路径（identity=sync_namespace=web_volume=`webdav:spike@http://127.0.0.1:1/dav/`）③env>file 装配链（文件半边缺失由 `CYDRIVE_WEBDAV_PASSWORD` 经 with_env_overrides 补齐→装配成功即证；env 清空→lone username 拒装配点名双键——M-I1 等价面，无 token 刷新态）④`run_sync_command` Webdav namespace 臂离线推导（死 sync 端点失败落 sync pass，错误链无 webdav 连接痕迹）。四组合实跑全绿（webdav / webdav,sftp / local,webdav,pan115 / sftp,pan123,webdav）+ pan115 twin 在第四组合 1/1。
- **十三处走查**（处→生效证据）：①Backend::Webdav+as_str+is_sync_supported（`!matches!(Local|Sftp)` 天然含 webdav+文档段）✓ ②KNOWN_TOML_KEYS 六键（config.rs:118-123）✓ ③VOLUME_SCOPED_KEYS（:179-184）✓ ④SECRET_VALUED_KEYS webdav_password（:406）✓ ⑤LEGACY_REJECTED 六键+`legacy_json_rejects_all_new_keys` 测试✓ ⑥validate() 块（含本批 userinfo）+ config_backend 13 webdav 测试✓ ⑦Cargo.toml feature 三件套（:26/:33/:97）✓ ⑧ck-webdav crate+16 manifests check_layers✓ ⑨BackendTransport 五臂+build_webdav_transport+dispatch 单 match 臂✓ ⑩WEBDAV_DRIVER_REQUIRED+DRIVER_ROWS 第 7 行+`compiled_drivers_lists_the_feature_set_in_fixed_order` WD1b mirror 断言（webdav 末位/全七序/webdav-only）✓ ⑪build_driver 双臂（本批补 offline 装配+walk 拨号断言）+ doctor probe（本批真实现+main.rs 接线）✓ ⑫with_env_overrides CYDRIVE_WEBDAV_PASSWORD（config.rs:1816+测试）✓ ⑬web 四文件✓（下条）。
- **web 四文件静态走查**：volumes.html radio `value="webdav"`+`vf-group-webdav` 六字段组（auth select auto/basic/digest、vendor select generic/nextcloud、certs checkbox+警示 hint，i18n 属性齐全）；volumes.js VF_CRED_GROUPS/VF_REQUIRED（仅 url 必填）/VF_STRINGS 五字符串键+certs checkbox 后端门控收集（846-852）/编辑预填（SHOW 回填+write-only 占位）；i18n.js 中英各 10 键对齐（179-188/450-459）；app.js:57+system.js:59 backend.webdav 标签表（B-M4 两处均在）。观察项（不修，P3 既有形态）：vfCollectPayload 收集全 VF_STRINGS 不按可见组过滤——跨后端误输键由服务端 validate 兜底，所有后端同型。
- **裁剪五组合**（build+clippy --all-targets -D warnings，全过）：`local,baidu` / `webdav` / `webdav,sftp` / `local,webdav,pan115` / `sftp,pan123,webdav`。组合门揭出两处既有缺陷并修复：**dispatch.rs axum 导入门 pan115 漏导**（K74 pan115 mock 用 Router/get，门只盖 baidu|pan123——`local,webdav,pan115` 组合 E0433；门扩 pan115，get 门按 pan123 mock 自带 use 收敛为 baidu|pan115）+ **pan115_combo_dispatch.rs field_reassign_with_default**（文件级 cfg 使默认构建看不见它；结构体更新语法修）。
- **能力横幅**：`transport_capabilities_line`（info 十位全列）在单卷 boot（lib.rs:663）与多卷 `build_volume_runtime`（lib.rs:2333）经 `transport.capabilities()` 通用打点——WebdavTransport caps 臂既有，webdav 零补线即获横幅（照先例格式=同一行）。
- **auth.rs 三处「WD1a 骨架」陈旧注释清理**（挂账②尾款：WD2 已接线，函数实际在用、构建零告警；注释谎报状态）。

**验证**：userinfo 两测试红（实测 `webdav_url_must_not... FAILED` / `config_rejects_userinfo... FAILED`）→绿；workspace **1569/0/42**（1558 基线+11：core 1+驱动 lib 1+connect_auth 5+dispatch 4；既有断言零漂移；组合 4 测试只在裁剪组合跑，四组合各 4/4+pan115 twin 1/1）；五组合 clippy -D warnings 全绿（E0460/E0464 陷阱预防性 `cargo clean -p` 后跑全量）；fmt/check_layers（16 manifests 7 drivers）/scan_secrets 绿。回滚 = revert 本批 commit。

**挂账**：①`TlsUntrusted` 探活臂无离线覆盖（自签 https fixture 归 WD5）②doctor 探活对 `accept_invalid_certs=true` 卷不再细分证书态（已开洞=无校验失败可言，设计使然）③vfCollectPayload 跨组收集观察项（P3 既有，若立项修复应全后端一起）。

### WD3（2026-09-22，worktree feat/webdav-driver）

**完成**：写路径全量 + conformance（dav-server 参照桩上线）。前次派发撞使用限额中断，桩侧观测面（lost_ack_skip/stat_size_delta/三头记录）已留工作树——本次修复其借用缺陷后续用。

- **断言①裁决（§4.6 预案路径）**：无 stash 版先跑 → dav-server 参照桩上覆盖写腿红（staging 窗口旧对象可见）→ **stash 协议上车**（writer 开时 `MOVE final→.ckwd-<pid>-<seq>.old`；close 成功删/失败与 abort 经 `restore_scene` 恢复——sftp 判例同款「删自己的、还别人的」）→ 绿。
- **参照桩揭真缺陷**：client.stat 原不发 `Depth` 头——dav-server 0.11 对无 Depth 的 PROPFIND 回空 multistatus → `ensure_parents` 把卷根误判缺失 → `MKCOL /` 500。修复 = stat 恒显式 `Depth: 0`（RFC 4918 §9.1）。手搓桩「缺头按 Depth 1」宽收掩盖了它——**双桩制的参照腿首功**（「桩照实现抄」防线实证）。
- **stager**：严格序（建父→PUT .part（Content-Length，X-OC-Mtime 仅 nextcloud 搭车、generic 零 PROPPATCH——记录器断言）→MOVE T→**stat size 复核**（不符→Unavailable；`stat_size_delta` 旋钮注入）→清理）；lost-ACK 两半边对账（PUT 断 ACK 重放 vs MOVE 断 ACK .part 不在+final 就位+size 吻合=按已提交继续——K67 H2 防线）；abort 恢复现场。
- **mkdir**：stat 预检（rclone MKCOL-201 幂等陷阱——桩双模式钉）+ 409 隐式建父重试恰一次；**rename**：Overwrite:F + 412→重 stat Exists + 缺父三态（403/409/500→建父重试；父被文件占位→Exists）；**delete**：NotFound 恒定声明（conformance ④）。
- **error_table 取舍**：404→NotFound + 401→Unauthorized{false}；刻意不选 5xx/429（重试白名单自愈消费掉单注入——sftp 刻意不选同款互指）；重试腿由 connect_auth 独立钉。
- **桩增强**：`transient_5xx_move` 独立计数旋钮（共享计数被 PROPFIND 重试链吃光——实证后新增）+ 自检 3 个（36 总）。

**验证**：workspace **1558/0/42**（1532 基线+26：write_path 20+selfcheck 3+conformance 2+upload 1；既有断言零漂移——connect_auth/read_path 各 1 个 seam 测试随 seam 移除原地重写）；clippy/fmt/check_layers/scan_secrets 绿；dead_code 锚与 TODO(wd3) 清零。回滚 = revert 本批 commit。

**挂账**：①close 整读 spool 进内存 PUT（D5 最简实现；流式 file body 挂账真机吞吐批复核后议——stager 模块文档「已知上界」节声明）；②X-OC-Mtime 值源 = spool mtime（WriteHint 无 mtime 字段的诚实值）；③真机 stash 腿/lost-ACK 形态/Nextcloud 实服 X-OC-Mtime = WD5。

### WD2（2026-09-21，a+b 两子批，worktree feat/webdav-driver）

**完成**：WD2a 子代理（手搓桩）+ WD2b 子代理（客户端核心+读路径），主会话 diff 审查后合批提交。

- **WD2a 桩**（`tests/stub/mod.rs` 1826 行 + `tests/stub_selfcheck.rs` 1587 行/33 测试）：内存 VFS + axum 动词面（PROPFIND 双 ns 风格**整字节钉死**——apache 多前缀/lp2/ISO8601/目录 404 块置前照 fixture 真实样本；MKCOL rclone201/Rfc405 双模式；MOVE 三旋钮族；Range 200/206/钳制/416 矩阵）+ **服务端真实 RFC 7616 验证**（response-uri 逐字节校验/nc 单调 enforce 可配/stale 可配）+ 10 故障旋钮（畸形 multistatus/连接杀/慢滴流/**lost_ack_after_effect 一次性**（K67 H2 重放窗关键旋钮）/transient_5xx/rate_limit_429/unexpected_301）+ 请求记录器（authorization 打码 + digest_nc 序列观测）。桩自检含 RFC 2617 §3.5 标准向量（桩与驱动**独立实现**数学，非循环引用）。简化声明：矩阵未记真值处从严（PUT 父缺失 409 等），桩模块头逐条列明。
- **WD2b 客户端+读面**（client.rs 871/driver.rs 523/transport_face.rs 177/xml.rs 286）：认证状态机（auto=Basic 预发→401 Digest 协商**恰一次**（初协商与 stale 共用每请求预算）；NTLM/Negotiate 拒绝+可行动文案；auth=basic/digest 定向模式；nc 每 nonce 单调（enforce_nc 桩上绿）；response-uri wire 逐字节）；重试白名单（GET/HEAD/PROPFIND/OPTIONS only；5xx/429/传输类；上限 3 指数退避封顶 30s；Retry-After clamp 1–60s（K79.3）；PUT 永不重试——测试钉死）；错误映射 `map_status`/`map_transport`（§4.4 表；reqwest 类型化判定无字符串信标；XML 失败→Io 带 ≤200B 片段）；读面 stat（Depth0+缺 contentlength→0+坏 mtime debug 不静默）/list（Depth1 恒尾斜杠+剔 self 容忍尾斜杠差异+K67 过滤+`.ckwd-` 暂存件过滤+字典序+Page 切片）/reader（stat 先行空流零 GET+8MiB 串行窗口+206 Content-Range 校验+200 截断回退+416→stat 复核）/quota RFC4331 best-effort None 降级；transport_face connect（OPTIONS+60s deadline）/open/open_range。
- **测试**：connect_auth 18（协商恰一次/nc 序列 [1,2,3]/stale 恰一次/uri wire/白名单计数/PUT 不重试/传输类分类/文案字面）+ read_path 17（双 ns 一致/根 href/跨窗逐字/空流零 GET/200 截断/416/畸形→Io/301/慢滴流/自愈/quota）+ lib 纯函数 10。红（35 失败）→绿全过。
- **实现期修复**：根 href 归一缺陷（`"/".trim_end_matches('/')` 空串致卷根 self 误剔+根列表 NotFound）——红→绿过程内揭出并修复。
- **test seam ×2**（`test_put`/`test_get_range`，`#[doc(hidden)]`，baidu 先例）：写侧面与 416 passthrough 的 WD3 前桥接，WD3 随批移除。
- **偏差**（裁决留痕）：Unauthorized 无载荷变体→可行动文案走 R3 双通道（warn 日志+纯函数字面单测）；416 决策半边（stat 与窗口间并发收缩）桩不可确定性注入——协议半边 seam 直测+决策逻辑 driver 内注释，真机腿留 WD5；WD1a 占位断言按其声明生命周期随批更新（lib 2 个/dispatch 1 个）。

**验证**：`cargo test --workspace -j 4` = **1532/0/42**（1455 基线+33 桩自检+44 新增；既有断言零漂移）；clippy/fmt/check_layers/scan_secrets 绿。回滚 = revert 本批 commit。

### WD1（2026-09-21，a+b 两子批，worktree feat/webdav-driver）

**完成**：WD1a 子代理（crate 骨架+纯函数层）+ WD1b 子代理（组合根接入面），主会话 diff 审查后合批提交。

- **WD1a**：`crates/drivers/ck-webdav`（Cargo.toml workspace 继承 + 十源文件 2528 行）——config（六键纯函数解析 + K31 文案）/urls（穿越防御：raw+解码双检 `..`/`.`/`\0`、双斜杠折叠、字面 `%`→`%25`、collection_url 尾斜杠、Destination 恒绝对 URI）/mtime（三格式 + RFC 9110 两位年边界 + 月长校验）/auth（spike digest 移植 + AuthState D1 骨架）/xml（spike 移植 + href 反转义/百分号解码 + 非 2xx propstat 不进投影 + `is_addressable_name` K67 过滤）/client（reqwest 构建 + 超时分层三常量 + 九动词 `TODO(wd2)` 占位）/driver（九方法骨架 + 能力位 R4 逐位依据 + quota None）/stager/transport_face。**TDD**：红 `3 passed; 42 failed` → 绿 `45 passed; 0 failed`（红相位 3 个天然绿负向断言，pan123 先例同型）。
- **WD1b**：16 文件 +1048/−25。锚点复核（附录 B 漂移表入批报告——web 表单实际在 templates/ 非 static/）；六处 config 清单 + validate 块（query/fragment 拒收为计划外超集，K31 精神）；sync 门控 `!matches!(Local|Sftp)` 形态下 Webdav 天然 true（零改写 + 文档段 + 测试钉——零漂移优先）；feature 三件套 + WEBDAV_DRIVER_REQUIRED + DRIVER_ROWS 第 7 行 + BackendTransport::Webdav 五臂 + dispatch_unified_backend_volume 单 match 臂 + twin 双臂 + build_driver 双臂 + doctor 骨架臂（`TODO(wd4)`）+ with_env_overrides `CYDRIVE_WEBDAV_PASSWORD`（B-M1）+ resolve_volume_settings 零改动复核 + web 四文件。
- **编译器揭出 3 处清单外接入点**（穷尽性机械证明）：CyDriveConfig 六 serde 字段 / tests/config.rs 穷举 round-trip / run_sync_command namespace 臂（baidu 式）。

**验证**：`cargo test --workspace -j 4` = **1455/0/42**（1394 基线 + 45 + 16；既有断言零漂移）；clippy/fmt/check_layers（16 manifests 7 drivers）/scan_secrets 绿；裁剪两腿 `local,baidu` / `webdav` 构建过 + off-feature K31 测试实跑绿。

**自主裁决（未询问）**：①factory 签名按仓库先例 `async fn factory(&WebdavParams) -> Result<Arc<WebdavDriver>, StorageError>`（指令模板与仓库形态冲突时从仓库）；②rand 0.10（workspace 无 [workspace.dependencies] 节，pan115 运行时先例）；③tempfile 为运行时依赖（stager 字段需要）；④`webdav_accept_invalid_certs` core 侧 `Option<bool>`（web 表单 bool 链 serde 兼容），宽容解析留驱动侧 map 面；⑤sync 门控零改写（文档+测试钉替代显式臂）；⑥`https:///dav/` 被 url crate 解析为 host="dav" —— 空 host 测试改用 `https://:5006/dav/` 真错误形态。回滚 = revert 本批 commit。

**挂账（WD4/审查批裁决）**：①~~webdav_url 理论可含 userinfo——SHOW 回显与 sync namespace 泄漏面~~ **已销（WD4：双漏斗拒收，core validate + 驱动 parse_from_map，TDD 红→绿）**；②~~WD1a 骨架 `#[allow(dead_code)]` 锚 18 处~~ **已销（WD2 批移除 allow 属性；WD4 清理 auth.rs 残留 3 处陈旧注释）**。

### WD0（2026-09-21，worktree feat/webdav-driver）

**完成**：立项三 commit 落 main（计划+跟踪单 `c4a042d` / decisions K80+K81 `b2d8b67` / AGENTS 联动 `da2c4ed`）→ WSL2 双服务器 fixture（rclone serve webdav v1.60.1 :8080 Basic + Apache 2.4.58 mod_dav :8081 Digest + /dav-stale 2s nonce 腿）→ curl 双轮探针 + `examples/webdav_spike` Rust 化矩阵（十腿+quota+cleanup，reqwest+quick-xml+md-5+httpdate 依赖面同驱动计划）→ 附录 C 回填 11 项 + fixture 文档 → D2 修订/D5 维持。

**怪癖矩阵要点**（全文 = fixture 文档）：
- **rclone MKCOL 已存在→201**（幂等成功陷阱）→ mkdir 必须 stat 预检（baidu 同型先例）
- **apache 集合 URL 无尾斜杠→301 不执行**（PROPFIND/MKCOL/DELETE/MOVE 全动词）→ 集合操作恒带尾斜杠；rclone 尾斜杠全不敏感
- **rclone 目录 MOVE 后 VFS 缓存不可见窗 ≈5min**（子项 404/500、窗内 DELETE 204 撒谎留残；数据已落盘；`--vfs-cache-mode writes` dir-cache-time 缺省）——spike Rust 腿新揭，curl 探针未见
- **apache 倒序 Range→200 全量**（忽略 Range）→ 200 截断回退实证必需
- **digest**：challenge 引号感知解析必要性实证（stale=true 位置不定）；**apache 不查 nc 重放**；过期→401+stale→新 nonce 重算恰一次恢复（D1 全链路真机验证 ✓）
- rclone 缺 Overwrite 头+目标存在→412（偏离 RFC 缺省 T）；apache 相对 Destination→400 / rclone 接受
- quota RFC4331 双 404 → None 降级实证；chunked PUT 双接受（D5 维持，样本仅二）
- 副发现：rclone 解析器容忍畸形 XML 声明、apache expat 400——请求体构造必须严格良构

**自主裁决（未询问）**：
1. **D2 降级**（generic 只读 mtime）——计划 §8 预设修订路径，触发条件实证成立（两家均不能真写 mtime）；nextcloud X-OC-Mtime 搭车保留。回滚 = 恢复 D2 原文。
2. **连接形态 = VM-IP 直连**（Windows→WSL2 localhostForwarding 本机当前失效，实测 000；VM IP 经 `hostname -I` 取）——fixture 文档记录；若后续 forwarding 恢复可换 127.0.0.1，无代码影响。
3. **D5 维持**（不因双样本升格 chunked 为默认）——保守可逆。
4. spike 增补 futures-core 声明（trait-only，reqwest stream 特性本就拉入，零新增编译产物）。

**执行期坑（沉淀）**：①`--noproxy *` 在 shell 变量展开被 glob（两轮探针假信号根源——unset 代理 env 或数组传参）；②Git Bash→wsl.exe 内联复杂命令引号被吞（AGENTS 已有明训，A/B 验证时再踩——.sh 路线不可省）；③htdigest 640 root:root → apache worker 读不到 → 500（chown root:www-data）；④WSL bashrc 的 curl 包装函数对探针产生双跑噪音（代码后缀为真值，已交叉验证）。

## 风险与未覆盖（随批更新）

- ~~待 WD0：真实服务器怪癖矩阵未钉~~ **已钉（附录 C）**；新增挂账：**rclone 目录 MOVE 不可见窗**（WD5 矩阵断言设计须避开 rename 后立即 list 的断言形态；驱动层不做补偿——服务器缓存行为非协议语义）
- chunked PUT 普遍性：仅双样本（挂账维持，D5）
- nextcloud vendor 路径（X-OC-Mtime）：fixture 无 Nextcloud 实例，未实证（零成本搭车，挂账）
- 待 WD5：真实广域网链路形态与吞吐（WSL2 回环数字口径，sftp SF5 判例）
