# Phase 5：115 网盘存储驱动（pan115）任务跟踪单

> 计划：`docs/plans/2026-09-14-pan115-driver.md` ｜ 需求口径：自用（K59.1）+ 全量公民 + 编译开关 + 零侵入
> 基线：main@52c3e41（workspace 1069/0/12；winfsp 腿 117/0/1）
> 状态：**115-0 完成（2026-09-16，K69：路线 A go）——115-1 可开工**
> 前置依赖：Phase 4（SFTP）已落地（main@f843cd9 含 K66/K67）——`compiled_drivers()` 可扩展化已就位，115-1 仅加臂
> worktree：`feat/pan115-driver`（E:/Rs_Codes/rs-cloudfs-pan115，独立 target）

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| 115-0 | 路线 spike 与裁决 | ✅ 完成（2026-09-16，K69） | 五项全过，**裁决路线 A go**：① 路径丙全链成立（client_id **100197303** OpenList 托管 app → PKCE 扫码 → Bearer user/info → refresh 自续轮换落盘）② UA **逐字节绑定**（错配恒 403）但形态不约束（空 UA 通）；HEAD/Range/206、etag=MD5 全过 ③ **770004 账号级限流**（~4rps 可持续、5rps 10s 内 22% 拒、封 ≥10min、跨端点族）→ D4 落值 1rps+硬退避 ④ 上传双路+callback+size 复核+resume 差集全过；**sign_key=用户级挑战非 app 签名**（K65 未决销账）⑤ 秒传命中过；伪造哈希 init 不拒（complete 侧校验）→ 省哈希优化不做 | spike 真机全输出（`examples/pan115_spike`，workspace-excluded）；上传 3MiB 1.8s/12MiB 2.3s；扫码停点协议见 K69.9；错误码表 K69.7 |
| 115-1 | 认证与驱动骨架 | ✅ 完成（2026-09-16，commit 65efd51） | ck-pan115 五模块（api/oauth/oss/limiter/lib，~2.4k 行）：envelope 双形态 + K69.7 错误分类（401*/99 一次刷+重放、770004 硬退避、911 fail-fast）；PKCE 三端点 + 单飞 refresh + ConfigTokenStore 回写；OSS V1 签名层 port（K69.5 不引 ali-oss-rs）；D4 限速器落值（1rps+300s 起退避）；九方法占位。配置四清单 + Backend::Pan115 + validate；编译面 feature 三件套 + DRIVER_ROWS + 占位臂（SF1 深度）| workspace **1189/0/24**（基线 1149+40）；clippy/fmt/layers/secrets 全绿；双裁剪腿构建过（local,baidu 无 pan115 / pan115-only）|
| 115-2 | 读路径 | ✅ 完成（2026-09-16，commit 12ecddd） | pathcache（路径↔cid 解析 + 单层目录索引缓存，热路径零 API 已断言）+ download（dlink TTL 缓存 30min 保守值 + 4MiB 有界窗口流；UA 绑定/HEAD 探测/206+Content-Range 校验/403→RateLimited 退避/空窗口形态）+ 九方法除 writer 全接线（list 全分页+稳定排序+offset 游标；mkdir 隐式父+Exists 预检；delete/rename 复合句柄 fid:pc:parent；rename 同父 update/跨父 move；reader 钳制+目录拒；quota rt_space_info；connect 取 uid）| tests/read_path.rs **14/14**（内存 VFS+loopback axum 桩回放）；workspace **1203/0/24**；五门禁全绿 |
| 115-3 | 写路径 | ✅ 完成（2026-09-16，commit cfbf907） | upload.rs：commit-on-close stager（本地 spool 两遍读形态）——hash→init→**K69.2 二次认证循环**（sign_check 区间 SHA1，上限 3 轮）→get_token→PutObject/Initiate?sequential+分片→Complete(callback 头)→**远端 size 复核**；resume 会话表（path\|size\|sha1 三元键 + 磁盘层）：逐片落盘、ListParts 对账、只补缺片、NoSuchUpload 重建（K69.8 #30）；Drop 保会话/abort 清除；oss.rs 加 scheme 缝（桩面；签名零影响）| tests/upload_path.rs **12/12**（假开放平台+假 OSS 端点）；workspace **1215/0/24**；五门禁全绿 |
| 115-4 | conformance + 装配 | ✅ 完成（2026-09-16，26bc85d + 05a7c7d） | **4a**：conformance 八断言绿（假开放平台+假 OSS 桩，get_token 端点 loopback）+ 115 特化分片级差集用例；执行期修复两个真实缺陷（stat 新鲜度查询——缓存吞后端错误使断言⑤不可满足；中断路径不落会话使差集失效）+ 实现「到齐即传」。**4b**：transport_face（Pan115Transport）+ factory 改 connect（真 uid 身份，占位 `pan115:pending` 杜绝进生产）+ Pan115Probe 四变体；CLI 全装配（BackendTransport 臂+五方法、pan115_params、build_pan115_transport(+_with/_with_endpoints 缝)、ConfigTokenStore 双 impl、dispatch 双臂、build_driver、sync=baidu 式、doctor 探测+通断检查、PROXY 文案）；main.rs per-backend dispatch + state_dir 放宽；web 表单/i18n（含 D2 回收站提示）| workspace **1221/0/24**；clippy/fmt/layers/secrets 全绿；local,baidu / pan115-only / sftp,pan115 三裁剪腿全过 |
| 115-5 | 真机矩阵 | ⏸ | 上传往返/Range 播放/秒传命中/断点续传（杀进程恢复）/rebuild 收敛（QPS 限速）/加密卷/E2E 三面 | — |

## 立项前研究（2026-09-14，已完成）

| 研究 | 结论落点 | 关键收获 |
|---|---|---|
| PCFS（Go，私有 web API 路线） | 计划 §1.2/附录 A.3 | 驱动形态对照本；sign.go 51 行带测试可直译；cookie 认证；CDN 完整头回传教训 |
| 115-plus-desktop（Rust，开放平台路线，MIT） | 计划 §1.1/附录 A.2/A.4 | **修正上轮"115 只有一条腿"判断**：官方开放平台路线 Rust 可行且活跃；TS 层=端点规格书；传输引擎实战（CDN 429 重生/OSS 续传双坑修法）；`ali-oss-rs` 引荐 |
| 风险背景核实 | 计划 §1.1/§7 | 2026-08-09 开放平台暂停服务报道 + 该仓库 QQ 群获取凭证暗示——**凭证可得性成为 115-0 第一项** |

## 批次日志

- **2026-09-16 115-0 完成（K69，路线 A go）**：自主会话执行。client_id 来源修正（115-plus-desktop `.env` 空占位 → 改取 api.oplist.org 托管站 authorize 跳转公开值 100197303）；spike 两批落地（auth 腿 `0b53c14` + 探针腿 `acebe05`，真机矩阵：上传双路/秒传/UA 矩阵/resume 差集/伪造哈希/QPS 升压）。**执行期钉死的关键事实**：QR 窗口 ~5min（40199002 快拒）、get.status 长轮询 30s/次、错误包 HTTP 200 + envelope 双形态（state:true 布尔 / state:1 数字）、真实限流码 770004（非 20130827）且**账号级**、OSS V1 签名两陷阱（URL 尾斜杠、子资源排序）、CDN etag=MD5、sign_key 用户级挑战。**决策**：OSS 层用 spike 自研签名（ali-oss-rs 版本树核对通过但零依赖自研已真机验证——K69.5）；D4 定值 1rps+770004 硬退避 5min 起步（K69.3）。未测：downurl 直链 TTL（115-5 顺带）、限流按 token 还是按 app（单身份无法测——挂账，缓解=client_id 可换）。测试目录 /_e2e_pan115/ 已清空保留；回收站 4 个测试文件（D2 语义）。
- **2026-09-14 K65 补充研究（凭证门槛消失）**：负责人问询 OpenList-APIPages「使用 OpenList 提供的参数」触发，专门克隆三仓核实（OpenList / OpenList-APIPages / 115-sdk-go）。**结论**：源码不含明文凭据（打码占位 + 部署时 env 注入）；但 **device-code PKCE 流全程无 secret**（`RefreshToken` 载荷仅 refresh_token 一字段）→ **凭证路径丙**成立：公共 client_id 自铸 + refresh 自持，运行期零 app_key/零外部依赖——「凭证可得性 = 胜负手」降级为「选 app 身份」。残余风险三项（app 身份连坐/共享限额池/upload 二次认证未决）入计划 §7 与 115-0 ①③④。配置键调整 `pan115_app_id/app_key` → `pan115_client_id`（非机密带缺省）+ `pan115_app_key`（路径甲专用可选）。计划同步六处（§1.1/§1.3/§4.3/§6/§7/附录 A.5/B）；decisions K65 入档。
- **2026-09-14 拍板收口（K62，方案获批）**：D2 = 删除进回收站、回收站接口不引入（误删恢复走官方端）；D3 = `pan115_root` 可设置、缺省网盘根 `"0"`；D4 = QPS 实现期合理定值（RebuildTuning 式注入 + 列目录 ~2 QPS 起步 + downurl 缓存 TTL 待 spike + CDN 429 自适应重生）；D1 路线保持 spike 门槛（§1.3 规则）。**115-0 解锁可开工**。落档四处：decisions K62、计划 §8/头部、本表、AGENTS。
- **2026-09-14 立项前置研究完成**（只读勘察，仓库 git status 干净）：
  - 双仓库分工定调（计划 §0.1）：115-plus-desktop = 主路线规格书 + Rust 传输参照；PCFS = 驱动形态对照 + 路线 B 保底；ck-baidu = 结构模板（api/oauth/errno mock/假服务端/TokenStore）。
  - **对上轮评估的修正入档**：115 从"一条腿"改判"两条腿"（开放平台 + web API）；Rust 生态从"零现成物"改判"`ali-oss-rs`（MIT）+ 传输参照齐备"——量级估计比 ck-baidu 省约三分之一（走路线 A 前提下）。
  - 计划落库三件：本跟踪单 + `docs/plans/2026-09-14-pan115-driver.md` + decisions K61。

## 批次日志（续）

- **2026-09-16 115-4 完成（4a 26bc85d + 4b 05a7c7d）**：conformance 八断言 + 全装配接线。**执行期两个真实缺陷修复**（4a 暴露）：①`stat` 原经路径缓存整层服务——后端错误永远浮不出来（断言⑤不可满足），改为末级新鲜查询（前缀仍缓存）；②分片传输中途失败不落会话——差集续传资产丢失（断言⑦失效），改为每片落地即持久化 + 失败路径同样落盘，并按 115 的 init 会话锁定语义实现「到齐即传」（`write` 到齐承诺量即推传输链，`close` 仍是提交点）。**装配批**：驱动面新增 transport_face/probe；`factory` 由 `new` 改 `connect`（VolumeId 真 uid）；CLI 13 处接线；web 4 处；测试 +4（dispatch 三 + crate transport/probe）。**裁决**：sync = baidu 式接入（计划 §4.3 明文；零 core 改动最可逆）；error_table 只留 430004（770004 是硬退避状态而非一次性失败，与断言⑤「恰好一次」契约冲突——映射由 errno_mapping.rs 钉）。子代理通道两次失败改主会话直做（记录在案）。

## 风险与未覆盖（如实记录）

- **未验证项**：凭证可得性、downurl 的 UA 约束矩阵、CDN Range/206、QPS 限额、上传全链、秒传哈希可否省略——全部属 115-0 spike，本批未做。
- **未验证项**：`ali-oss-rs` 与本仓 reqwest 0.12 的版本兼容（其使用方 lock 显示 reqwest 0.13——可能双版本并存；115-1 核对，不可接受则按其源码自实现分片签名）。
- **平台政策风险**：2026-08 暂停事件的后续不可预测；缓解=限速+路线 B 保底设计（api.rs 内聚使切换成本=换认证与签名层）。
- **待人工决策**：D1 路线（spike 后）/ D2 删除语义（回收站 vs 永久）/ D3 根目录缺省 / D4 QPS 限速参数形态（计划 §8）。
