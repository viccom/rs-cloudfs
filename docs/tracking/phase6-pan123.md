# Phase 6：123 网盘存储驱动（pan123）任务跟踪单

> 计划：`docs/plans/2026-09-14-pan123-driver.md` ｜ 需求口径：自用（K59.1）+ 全量公民 + 编译开关 + 零侵入
> 基线：main@c819ee8（workspace 1069/0/12；winfsp 腿 117/0/1）
> 状态：**123-0 读路径腿完成（2026-09-20，四组真机实证）；写路径腿（⑤ upload 全链语义 ⑥ 密文空 etag）待开工 → 123-1 解锁条件就绪**
> 前置依赖：与 Phase 4/5 共享的 `compiled_drivers()` 可扩展化重构（谁先到谁做，只做一次）
> worktree：待建（建议 `feat/pan123-driver`）

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| 123-0 | web API 验证与采样 spike（路线已定 K64；2026-09-20 按端点代际情报改写） | 🔄 **读路径腿完成（2026-09-20）** | 四组完成：① 端点代际复验 ✅（**新旧并存全活**——dydomain 活且答 `www.123pan.cn`、list/trash/rename/download_info/upload_request 双代际全活、info 活、`Page` 分页实证、**无签名即通**）② 认证 ✅（密码 sign_in 明文形态 + code==200 + token 90 天；**Bearer 单头即足**、Cookie 单发 20101/401 拒；QR 三端点 generate/result/wx_code 无人工腿通——确认态留 123-4）③ list/trash 往返 ✅（envelope 双拼实证 `InfoList` vs `infoList`、时间=ISO8601 字符串、5060 采样（data 带 etag/size/updated_at）、**正确载荷 trash 回读校验通过**（intoRecycle 确认）；**静默陷阱今日不复现**——多余键/裸 dict 两形态均真删，最小载荷纪律维持）④ download ✅（traffic/check 数字：`originalRemainTraffic=10735321088`≈9.99GiB、`isTrafficExceeded=false`、`isBlocked=true`；user/info：`SpacePermanent=2199023255552`(2TiB)/`DirectTraffic=0`/`Vip=false`，report/info 只有 vipType 族**不是空间配额**；下载三跳链：web-pro2 HTML → params base64 自解码 → **210+JSON redirect_url** → 镜像 206 Range 逐字节 MATCH、dlink 二次 GET 仍 206；5113/5114 未遇（额度未耗尽））。残留：⑤ upload 全链语义（duplicate 1 vs 2/分片状态保留）⑥ 密文空 etag = 写路径腿待开工 | `examples/pan123_spike`（13 子命令，重跑即证）；证据日志 `E:\GitHub\rs-CyDrive\test\pan123-spike-download-evidence.log`；§3.1 回填 |
| 123-1 | 认证与驱动骨架 | ⏸ | QR 三端点 + TokenStore + 动态域名发现 + 四件套 + 配置键三处同步 | — |
| 123-2 | 读路径 | ⏸ | list/stat/mkdir/trash/rename + download_info 缓存 + Range 流读 + traffic 预检 + 令牌桶限流 | — |
| 123-3 | 写路径 | ⏸ | upload_request/分片 presign/PUT/complete + duplicate 语义 + MD5 预计算 + 超时自适应 | — |
| 123-4 | conformance + 装配 | ⏸ | 假 123 API + 假 presigned PUT 桩八断言 + 12 装配点 + doctor（token/流量余量）+ web 表单 | — |
| 123-5 | 真机矩阵 | ⏸ | 上传往返/Range 播放/秒传/分片差集/rebuild 限速/加密卷/E2E 三面/流量限额触顶（可选） | — |

## 立项前研究（2026-09-14，已完成）

| 研究 | 结论落点 | 关键收获 |
|---|---|---|
| pan123-rs（MIT，本轮克隆实测） | 计划 §1.1/附录 A.1 | **web 路线 Rust 全套现成**（2862 行：全端点/双头认证/上传三段流/Range 续传/令牌桶）；reqwest 0.12 同栈；签名已简化（随机 key，PCFS CRC32 为备胎）；blocking 形态是移植主项 |
| PCFS（Go） | 计划附录 A.3 | 驱动形态对照 + sign.go 备胎 + 两段下载/IPv4 |
| 开放平台生态（alist 123_open 等） | 计划 §1.2 | 免审秒发但直链空间/VIP 门槛——对挂载+流式播放是硬伤，路线 A 降为备选 |
| **123panNextGen**（GPLv3，2026-09-20 三路子代理深读） | 计划 §3.1/§5.10–17/§8-D5/附录 A.4 | **端点代际真相**（2026-08-29 重组实证 → pan123-rs 形态部分过时；漂移监控哨 = 其提交流）；稳定/性能采纳面：envelope 双成功码与双拼解析、trash 静默失败陷阱、双会话分离、上传七步序（upload_complete 不可省）、dlink 一次性 + CDN JSON 重定向、Retry-After 退避常量、续传五元组；**D5 = web 身份合规**（安卓模拟与 URL 重写绕过不采纳，证据留档） |

## 批次日志

- **2026-09-20 123-0 读路径腿（spike(pan123) commit，本 worktree）**：`examples/pan123_spike`（workspace exclude，13 子命令：sign-in / probe-dydomain / probe-matrix / probe-qr / probe-list / probe-mkdir / gen-file / probe-upload / probe-trash / probe-trash-trap / probe-download / probe-heads / probe-user / cleanup；凭据只从仓外 json 读，token 持久化 `pan123-tokens.json`，输出全脱敏）。**四组真机实证（免费测试账号，直连无代理，IPv4 dial）**：
  - **① 端点代际**：总结论 = **新旧并存全活**——`/api/dydomain` 在 login/www/api278 三域同答 code=0 且 `domains:["www.123pan.cn"]`（现行主域实证 + 动态发现可用）；list `/api/`（新）与 `/b/`（老）在 www 与 api278 均 code=0；trash/rename 空载荷回 validation 错误（端点活）；mkdir `/a/api/file/upload_request` type=1 即建（FileId 在 `data.Info`）；download_info 新 `/a/`（回 `data.DownloadUrl`=web-pro2 中继）与老 `/b/api/v2/`（回 `dispatchList+downloadPath`）**两代都活但形状分叉**；upload_request 两代都活（5060 时 `/b/` 带 timestamp/trace_id、`/a/` 不带）；`/b/api/file/info` **活**（键小写 `infoList`，list 是 `InfoList`——双拼实证）；`upload_complete` 新形态 `{fileId}` 单键即收。**签名：无签名即通**（不带/带随机 key 均 code=0）。
  - **② 认证**：`POST {www}/b/api/user/sign_in {"type":1,"passport","password"}` **明文密码**、成功码 **200**（唯一非 0 成功码）；token JWT 90 天（`expire` ISO8601 + `refresh_token_expire_time` int 字段存在，无 refresh 端点——重登语义维持 D5/计划裁决）；**双头必要性：Bearer 单头即足**（both/bearer 通、cookie-only → list `20101 未登录`、user/info `401 "cookie token is empty"`——**错误码按端点分叉**，错误映射表原料）；token 探活轻端点：list(limit=1) 或 user/info 均可。QR 无人工腿：generate（uniID+url）→ result 轮询 `loginStatus:0`（0-4 状态机现行形态）→ wx_code 无扫码回空 `wxCode:""`——三端点 web 头全通，**确认态 code==200+token 需人扫，留 123-4**。
  - **③ list/trash 往返**：list envelope `data{InfoList,IsFirst,Len,Next,SearchFileDesc,Total}`；**Page 分页实证**（page1/page2 零重叠，`Total` 全量）；条目字段 PascalCase（FileId/FileName/Type/Size/UpdateAt/CreateAt），**时间 = ISO8601 字符串**（`2026-09-20T12:20:15+08:00`，mkdir Info 带纳秒精度）；目录条目带累计 Size（聚合语义）。5060 采样：mkdir 同名重发 → `code=5060 msg="检测到1个同名文件…"` `data{etag,size,updated_at}`。**trash 正确载荷**（`fileTrashInfoList:[{"FileId":N}]` 大写 F + `event:"intoRecycle"`）→ code=0 + 回读不在 + `trashed=true` 视图可见 = **intoRecycle 确认**；**静默陷阱复现尝试失败**：多余键 dict-in-list 与裸 dict 两形态均 code=0 且**真删**（服务端已放松；最小载荷+回读校验维持为纵深防御）。
  - **④ 下载与流量**：traffic/check 真 fid → `isTrafficExceeded:false`、`originalRemainTraffic:10735321088`（≈9.99GiB 免费日额）、`isBlocked:true`（含义未明，流量未超下正常下载——挂账）、`clientFileSize:1048576`、desc1/desc2/desc3=10GB/20GB/50（营销位）；空 fids → code=400 `The Fids field is required`。user/info：`SpacePermanent:2199023255552`（2TiB）、`SpaceUsed` 实时、`DirectTraffic:0`、`ShareTraffic:0`、`Vip:false`、无 `unlimited` 键（VIP 形态挂账）；`report/info` 只回 `developSub/packType/vipSub/vipType`——**空间配额真身在 user/info**（计划 §3 quota 行修正）。下载链三跳：web-pro2 中继（HTTP 200 text/html 5344B 无 href——123panNextGen 的 [:500] href 窗口抓不到）→ `params=` base64 **自解码**（对服务端自然返回的 URL 做纯解析，**非** D5 排除的 auto_redirect=0 注入绕过）→ CDN 域 **HTTP 210 + `{"code":0,"data":{"redirect_url":...}}`** → 镜像域（`pd1.cjjd19.com`）**206 + Content-Range `bytes 100-2097151/2097152`**，与本地源文件**逐字节 MATCH**；**dlink 复用**：同 URL 二次 GET 仍 206（至少会话内不限次）。**5113/5114 未遇**（额度未耗尽，免费 10GiB/日）。
  - **写路径顺带发现**（非本腿验收项，123-3 原料）：完整上传链走通（upload_request `/b/` → presign（presignedUrls["1"]，AWS4 签名）→ 纯 PUT（etag 回显=MD5）→ `s3_list_upload_parts` → `s3_complete_multipart_upload` **单分片时 code=-1 rpc MalformedXML 但无害** → `upload_complete{fileId}` → list 回读可见）；**秒传实证**：同 etag 重传 → `Reuse=true`（顶层 FileId=0，真值疑在 `Info`——写路径腿钉）；reuse 建的条目 list 有分钟级缓存滞后。
  - **清扫**：`e2e_pan123_` 前缀严格清扫 + 根目录回读核空（`Total:0`）。
- **2026-09-20 增补研究 + D5 拍板（123panNextGen 三路子代理深读，主仓零代码改动）**：负责人指令在实施 Phase 6 前深度研究 `E:\GitHub\123panNextGen`。三路 Explore（协议层/传输层/文件操作面）+ 主会话综合，产出计划附录 A.4。**三项改变局面的发现**：①流量限额的协议真相——限额绑定客户端身份，绕过链实证存在（安卓身份 + 5113/5114 软处理 + web-pro2 URL 重写），**D5 负责人裁决 = web 身份合规路线，不采纳绕过**（证据留档）；②**2026-08-29 端点大重组**（`/a/api/`、`/b/api/` 前缀代际化）→ pan123-rs（2026-06-26）线形态部分过时，计划新增 §3.1 代际警示表，123-0 首组验证改为现行形态复验；③**token 无 refresh 机制**——错误映射从「重登/刷新一次」改为 `Unauthorized{recoverable:false}` + 重扫码指引。**稳定/性能采纳面**入计划 §5.10–17（八条新硬纪律）+ §3.1；**D4 出现证据矛盾**（duplicate 1 vs 2 哪个是覆盖——两参照注释相左）已升级为 123-0 ⑤ 的真机钉死项。落档：计划头部/§0.1/§3/§3.1/§5/§6/§7/§8/附录 A.4 + 本表四处。
- **2026-09-14 拍板收口（K64，方案获批）**：D1 = **web API（直裁，双腿门槛关闭**，路线 A 降为文字参照）；D2/D3 = 沿用 Phase 5 拍板形态（trash 回收站语义不引回收站接口 / 根目录可设置缺省网盘根）；D4 = duplicate 覆盖模式（对齐 writer 契约，rtype=3 先例；覆盖行为 123-0 实测钉表）。**123-0 解锁**，职责收窄为 web API 验证与采样（计划 §6 已改写：首项 = 形态有效性确认——pan123-rs ~3 月未动）。落档四处：decisions K64、计划 §1.3/§6/§8/头部、本表、AGENTS。
- **2026-09-14 立项前置研究完成**（pan123-rs 为专门克隆实测：`E:\GitHub\pan123-rs`，MIT；主仓全程零改动）：
  - 三参照分工定调（计划 §0.1）：pan123-rs = 主路线 Rust 规格书（可移植源）；PCFS = 形态对照 + 签名备胎；alist 123_open = 备选路线参照。
  - **对前轮评估的更新**：web 路线从「直译 PCFS ~ck-baidu 量级」降为「**移植现成 Rust SDK**（主项 = blocking→async 转换）」；新增产品级约束发现——**每日下载流量限额**（非会员；会员 9 元/月解除）与 **MD5 etag 前置**。
  - 计划落库三件：本跟踪单 + `docs/plans/2026-09-14-pan123-driver.md` + decisions K63。
  - D2/D3 建议沿用 Phase 5 已拍板形态（trash 天然回收站语义 / 根目录可设置缺省网盘根），D4 为本阶段新增（duplicate 覆盖语义）。

## 风险与未覆盖（如实记录）

- ~~**未验证项**：全部技术断言来自代码阅读，**零真机验证**~~ **读路径腿已真机验证（2026-09-20，123-0 四组）**；余下未验证项集中在写路径：duplicate 1 vs 2 覆盖语义、分片状态保留（resume 可行性）、密文空 etag、QR 确认态（需人扫）。
- **挂账（读路径腿遗留观察项）**：traffic/check 的 `isBlocked:true` 含义未明（流量未超 + 下载正常）；user/info 无 `unlimited` 键的 VIP 形态（现账号非会员）；5113/5114 真身（免费日额 10GiB 未耗尽）；Reuse 响应 FileId 落点（顶层 0，疑在 `Info`）；`/b/api/file/delete`（回收站永久删）未测；reuse 条目 list 缓存滞后的窗口长度。
- **产品级约束已钉数字（2026-09-20）**：免费日下载额度 ≈10GiB（`originalRemainTraffic` 实测 10735321088B）；D5 维持合规 + 会员消解路径。
- **待人工决策**：~~D1 路线 / D2 / D3 / D4~~ **全部已决**（K64 + D5 2026-09-20）；余留仅 spike 内实测钉值（duplicate 1 vs 2、分片状态保留）——均属 D4/计划既定框架内的验证项，非新裁决。
