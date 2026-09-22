# Phase 7 webdav 驱动 · 合入前深度审查发现清单

> 审查形态：三路并行子代理（路1 驱动模块精读 / 路2 测试与桩面 / 路3 集成装配面）+ 主会话门禁独立复跑。
> 审查基线：worktree `feat/webdav-driver` @ 4e6e2cf（WD0–WD5 + 收口批后）。
> 汇总裁决：**1 High + 15 Medium + ~25 Low**；High 与全部 Medium 清偿（逐项 TDD 红→绿）后方合入；Low 挂账（K75 形态后续批裁决）。
> 三路判定：路1「需修后合入」（H1 数据破坏路径）/ 路2「可合入（B+，缺口=回归检测面缺失）」/ 路3「需修后合入（仅文案级两处）」。

## High（1 条）

| # | 发现 | 位置 | 证据与触发 | 修法 | 状态 |
|---|---|---|---|---|---|
| H1 | close ④ 复核 stat **出错**臂调用 restore_scene——旧对象（stash）以 Overwrite:T 移回 final，**覆盖已提交的新版本**（数据破坏）。与同函数内尺寸不符臂已写明的「不 restore（会删掉已提交的新版）」裁决自相矛盾 | `stager.rs:305-311`（对照 restore_scene `staged stash` 覆盖复位 `stager.rs:155-171`） | 覆盖写 close → PUT/MOVE 成功（新版固化）→ ④ 复核 PROPFIND 遇瞬态网络错误 → 旧对象复位覆盖新版 + 调用方收 Err 误判未提交。K67 重放窗同型破坏从另一失败臂复活 | stat-Err 臂不 restore（与尺寸不符臂同型，如实上抛；stash 留为过滤残件）；配「MOVE 后 stat 出错」桩旋钮 TDD 红→绿断言 final 字节仍为新内容 | 待修 |

## Medium（15 条）

### 路1 驱动模块（7 条）

| # | 发现 | 位置 | 修法 | 状态 |
|---|---|---|---|---|
| M1 | 并发 401 协商竞态：每次 401 无条件重置 Digest 会话（nc 归零）——同 nonce challenge 的严格 nc 服务器上双并发请求可产生重复 nc → 一次性伪 `Unauthorized{recoverable:false}`（apache「不查 nc」掩盖了此窗） | `client.rs:361-363` + `auth.rs:53-62` | handle_401 仅当 challenge nonce 与现会话不同（或现状态非 Digest）才重置；双并发 401 + 同 nonce 单测钉死 | 待修 |
| M2 | writer() 在 stash MOVE 成功后、stager 构造（tempfile）失败时不上报不恢复——旧对象停在 list 过滤的 `.old`，final「消失」 | `driver.rs:723-745` | 先建 spool 再 stash（调换序），或构造失败臂复位 stash | 待修 |
| M3 | 响应体全量无界缓冲：错误面 `text()/bytes()` 整读 + **200-回退读面整文件进内存**（apache 倒序 Range 200 全量真形 × 大文件 = OOM 面）；snippet 截断发生在全量读入之后 | `client.rs:475/478/533/540/583/591/663/705/730/751/793` | 错误体有界读（drain_bounded LIMIT 思路）；200-回退按 `end` 封顶/流式跳读；顺删 `window=None` 死路径（Low 第 4 条） | 待修 |
| M4 | config 错误文案回显原始 `webdav_url` 值——userinfo 拒收信息本身把嵌入密码带进错误链/日志（R3 面；core `redact_credential_values` 同纪律的驱动侧绕过） | `config.rs:115-121/92-97`（消费点 `cli/lib.rs:5540-5541`） | 拒收文案不回显 `{value:?}` 原文（只指路键名） | 待修 |
| M5 | xml `entries()` 二次方复杂度：每行 `position()` 线性扫——万级文件目录 ≈5×10⁸ 次比较（rebuild 面可观测劣化；pan115 11.7 万文件教训同型） | `xml.rs:247` | `HashMap<String, usize>` 索引 | 待修 |
| M6 | 单头多 challenge（`Basic realm=..., Digest ...` 逗号并置，RFC 7235 允许）不协商 → 明明有 Digest offer 却误分类「凭据被拒」 | `auth.rs:131-139` + `client.rs:357-374` | 每个头值先做引号感知顶层逗号拆 scheme 段再逐段 parse | 待修 |
| M7 | 207 内层成员失败被「静默消失」→ stat/list 映射成 NotFound，真类应是 Unavailable（计划 §4.4「成员失败按成员映射」偏差） | `xml.rs:246` + `driver.rs:478/547-550` | 条目投影保留「仅非 2xx 块」成员 + stat/list 显式映射（内层 404→NotFound、其余→Unavailable） | 待修 |

### 路2 测试与桩面（6 条）

| # | 发现 | 位置 | 修法 | 状态 |
|---|---|---|---|---|
| M8 | §4.5-7 超时分层零钉测（13 条负面清单中唯一完全无测试——回归丢 `.timeout()` 全部照绿） | `src/client.rs:54/57/60/75` | lib 纯函数面钉常量字面；真机「慢服务器超时腿」挂账 | 待修 |
| M9 | Range 请求头不可观测：记录器独缺 `range` 字段——驱动丢 Range 头则 200-回退切出同字节、全绿假象（§4.5-9 半失守） | `tests/stub/mod.rs:644-666` | 记录器加 `range` 字段 + reader 测试断言每 GET 携带 `bytes=a-b` | 待修 |
| M10 | 416 决策第二半（offset < 新 size → Unavailable + Err 臂）seam 移除后零覆盖；`read_path.rs:451-453` 注释声称过强（「正式读面覆盖同一契约」实际只盖 EOF 半边） | `driver.rs:682-689` + `tests/read_path.rs:451-471` | 桩加「416 后回长」旋钮补两臂覆盖 + 修正注释 | 待修 |
| M11 | `403 → Unauthorized{false}` 全无覆盖（NAS 真实常态——apache GET 集合 403 即 fixture 真形） | `client.rs:1031` | 桩加 403 旋钮 + 驱动面测试 | 待修 |
| M12 | 206 校验负路径三条全未测（mismatch→Io / body 长度不符→Io / 200 body 不足窗→Io——「无 206 校验」缺陷正面修法本体） | `client.rs:602-618/622-627` | 桩加 206-lie / 200-short 旋钮各一 + 三测试 | 待修 |
| M13 | 错误映射尾部行零覆盖：507→Io / 429 耗尽→Unavailable / MOVE 缺父 409 臂 | `client.rs:1042/798` 等 | 各补一测（MoveMissingParent 加 409 变体；429 耗尽腿） | 待修 |

### 路3 集成装配面（2 条）

| # | 发现 | 位置 | 修法 | 状态 |
|---|---|---|---|---|
| M14 | `PROXY_DIRECT_BACKEND_NOTICE` 把 webdav 列进「恒直连」但驱动实际尊重系统代理 env（刻意不 no_proxy）——proxy_url + webdav 卷的排查方向误导（pan123 落地时更新过此文案，webdav 漏了） | `cli/lib.rs:6024-6030` | webdav 专属文案分支（「忽略 proxy_url 配置键，但尊重标准代理环境变量」） | 待修 |
| M15 | `webdav_backend_probe` 配置不全 detail 带 ~22 个连续字面空格（多行字符串漏 `\` 续行——K75-3 同族病，doctor 用户可见） | `cli/lib.rs:6171` | 补续行符或折行重排 | 待修（主会话直做） |

## Low（~25 条，挂账不阻合入）

路1（11）：digest 头参数未按 RFC 7616 §3.4 转义（用户名含引号即 malformed）/ xml 同 response 内 `href` prop 臂二次解码风险（宜加 href_sinking 门）/ 416 复核 `unwrap_or(0)` 把漏发 getcontentlength 的文件静默读成 EOF / `get_range` `window=None` 死路径（随 M3 删）/ `destination_uri` 导出零生产调用 / reqwest 注释「stream 特性」失真 / map_status 3xx 通用臂不含 Location（stat 尾斜杠腿已兑现）/ stash 注释「清扫孤儿」理由不成立 / xml 解析热循环 String 堆分配 / 通用 409→Invalid 语义可商榷 / transport upload 整读叠加 close 整读 ≈2× RAM（上界声明面）。
路2（9）：`live_*` 命名与 code-style「ignored_*」字面漂移（先例既成）/ bad_url 假定 59999 空闲未注明 / 桩 Allow 列 HEAD 但 route 405 / 桩 PROPFIND 缺 Depth 宽容未在简化声明点名历史教训 / kill 自愈断言 `>=3` 偏弱 / connect_auth 模块头覆盖表 16 vs 24 漂移 / 桩 digest nc 缺失跳过 enforce / 双 ns 一致性测试 `.ckwd-` 种子只在 rclone 侧 / lost-ACK PUT 半边无请求计数对称钉。
路3（5）：双漏斗 trim 空白语义分歧（两侧终局均可行动报错）/ 大写 scheme core 严驱动宽（安全方向）/ Edit 表单 select 对手写大写枚举值显示层漂移（保存不丢值）/ stager spool 落 OS temp 非卷家目录（K21 面——代码注释声明刻意，留裁决）/ setup 向导与 ci.yml 矩阵未扩（均沿先例，信息项）。

## 已核对无发现（三路交叉要点）

锁不跨 await 全合规；URL 穿越防御（含 %252e 双编码）无绕过；reader 窗口数学正确；K75-1 红线（传输类绝不映射 Exists）全出口合规；凭据纪律除 M4 外无泄漏面；错误映射表 §4.4 逐行兑现（M7 偏差除外）；重试白名单 + Retry-After clamp 钉死；能力位 R4 十位诚实（authoritative_index 在「驱动零缓存」语义下成立——rclone 缓存窗是服务端谎言且已挂账）；stager 失败路径矩阵除 H1 外完备；R1/依赖/Cargo 合规；四清单六键齐 + bipartition 钉；零漂移（compiled_drivers strip 链 sound、既有断言值逐字未动）；env 链逐字同形；前端四文件全对齐；dav-server 单 pin；workspace 1570/0/52 + 五组合实测复跑吻合。
