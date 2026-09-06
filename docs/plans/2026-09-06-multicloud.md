# rs-CyDrive 多云后端支持（Phase 0 spike → 百度网盘 → 双后端并存）设计 + 实施计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**定位（2026-09-06 会话裁决）**：从「Telegram 单后端」演进为「可插拔多云后端」；首个新后端 = 百度网盘（负责人 SVIP + 个人开发者 appkey，实测带宽可跑满）。参照系 = 负责人自己的 Go 项目 `E:\Go_codes\PrivateCloudFS`（百度/115/123 driver 已实战，情报见附录 A）。

**Goal:** ① spike 用自有 appkey 验证 PCFS 经验之外的自有变量（QPS/dlink 缓存/秒传/吞吐）；② CloudTransport 瘦身为对象存储抽象（两个真实实现提炼）；③ `cydrive-baidu` 后端全功能（上传断点续传/Range 下载/OAuth 刷新）；④ 双后端并存形态 = 每后端一个实例（Y:/Z: 两盘），进程内不混装。

**北极星不变**：「一个稳定好用的程序」——百度后端可用性不达 Telegram 水准（吞吐/限速）则不转正，spike 设止损点。

---

## 架构裁决（本计划钉死的决策）

### A1. CloudTransport 瘦身（Batch R，纯重构，527 测试护航）

现状 trait 是「聊天存储」抽象（incoming/send_text/send_document 是 Telegram 概念）。瘦身后：

```rust
// 核心四操作 + 元数据（所有后端必须实现）
trait CloudTransport {
    async fn connect(&self) -> Result<(), TransportError>;
    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, TransportError>;
    async fn open(&self, handle: &RemoteHandle) -> Result<ByteStream, TransportError>;
    async fn open_range(&self, handle: &RemoteHandle, range) -> Result<ByteStream, TransportError>;
    async fn delete_remote(&self, handle: &RemoteHandle) -> Result<(), TransportError>;
    fn capabilities(&self) -> TransportCaps;   // PCFS Supports 式能力位
}
// 可选能力拆独立 trait（实现方按需选装；消费方能力探测降级）
trait InboundCap { async fn incoming(&self) -> ...; }        // Telegram 有，百度无
trait ChatCap    { async fn send_text/send_document(...); }  // Bot 命令面，Telegram 专属
```

- **能力位**（镜像 PCFS `driver.go:53-59`）：`RANGE_READ / RESUME / MULTIPART / INBOUND / CHAT`；消费方（bot worker/webdav）启动时探测，无能力则禁用对应功能并日志声明，绝不 panic。
- **错误归一化**（PCFS 最大反例：上层 import 具体 driver 做错误断言，`lazy_multifs.go:4`）：`TransportError` 增 `AuthExpired { recoverable: bool }`——百度 errno 110（可恢复→transport 内自动刷新重放一次）/111（不可恢复→重授权指引）/‑6 映射于此；**上层永不认识百度错误类型**。
- **分块策略归 transport**：core 的 chunker/契约模块（caption/part_name）留在 `cydrive-telegram`；`UploadJob` 的分块对 transport 是黑盒——Telegram transport 按 1900MB 切并写 chunks 行，百度 transport 整文件上传（**chunk_count=1 优雅退化**，见 A3）。

### A2. 鉴权生命周期（TokenCallback 模式 → keyring）

OAuth 是 bot_token 时代没有的新状态机：transport 内持 token + 自动刷新（110 时刷新并重放原请求一次，PCFS `client.go:196-203` 先例）；刷新成功经回调持久化进 **CredentialStore**（keyring，扩 service 条目 `cydrive-baidu`）；111 失效 = 可行动错误指 `cydrive setup`（device code 流程）。凭据来源优先级沿用 env > config > keyring。

### A3. 数据模型零破坏（chunk 退化）

对象存储后端 chunk_count=1、chunks 表单行——DB schema、sync payload（含 chunks JSON）、WebDAV、web 全部零改动兼容。**已验证**：sync 引擎对 chunk 数无假设，hydrate 单块路径即合并路径。

### A4. 实例即后端（v1 不做进程内多后端）

一个 cydrive 实例 = 一个后端（config 顶层新键 `backend = "telegram" | "baidu"`，默认 telegram 保持完全兼容）。双后端 = 两个目录两份 config 两个盘符（Y:/Z:）。跨后端 union 视图/文件迁移 = 明确不做（v2 议）。

### A5. sync namespace 派生改为后端感知

`namespace_key(token, chat)` 是 Telegram 形态。改为 `namespace_key(backend, account_id)`：telegram = `telegram:bot_token:chat_id`、baidu = `baidu:uid`（baidu uinfo API 取）。**兼容性裁决**：派生串加 backend 前缀 = 现有 Telegram 命名空间全变 → 各实例需清 sync 两表重推（既有 §7 流程），一次性的可接受成本；v1 也可选「不加前缀、baidu 用独立 db 天然隔离」——**spike 后按实现复杂度二选一，倾向加前缀**（显式优于隐式）。

### A6. 网络与运维

- **强制 IPv4 dial**（Windows DNS 优先 IPv6 连百度出问题，PCFS `client.go:18-24` 注释原文）——传输层公共调优，telegram transport 不动。
- 128KB 读缓冲 + keep-alive 池 + 禁压缩（PCFS `driver.go:327-340` 调优组合）。
- 百度端点直连国内，**不走 proxy_url**（与 Telegram 相反；config 的 proxy_url 对 baidu 后端无效并文档声明）。

---

## 任务批次

### Batch S：spike（验证驱动，examples crate，非 TDD——照 `examples/history_spike.rs` 先例）

用自有 appkey 验证 PCFS 情报未覆盖的自有变量，产出 `docs/reports/` 报告：

1. **OAuth device code 全流程**（headless 授权 → access/refresh token）
2. **QPS/限速实测**（自有 appkey 与 PCFS 借用的第三方 appkey 限额桶不同，必须自测）：列目录 10 连发、上传分片 10 连发、下载 5 连发，记录 429/31034/任何拒绝形态
3. **上传三步曲 + 断点续传差集**：precreate 响应的已传分片列表解析 → 中途杀进程重传只补差集（PCFS 没做的改进点）
4. **秒传分支**：precreate `return_type` 命中秒传时的行为（PCFS 盲区，未处理）
5. **dlink + Range**：302→Location→`Range: bytes=start-` 回 206 确认（PCFS 已证，复测）+ **dlink 有效缓存时长**（PCFS 每次 Seek 重新换 URL 未缓存；我们测时效以省 API 往返）
6. **吞吐**：≥1GB 文件上传+下载实测（SVIP 跑满带宽验证；下载方向重点）

**止损点**：下载吞吐 < 5MB/s 或 QPS 限制使列目录/分片上传不可用 → 停止，报告留档，百度后端降级为 v2 待议。

### Batch R：trait 瘦身（纯重构，TDD 迁移测试护航）

A1 的接口改造 + GrammersTransport 适配新面（incoming/send_* 拆到 InboundCap/ChatCap impl）+ MockTransport 同步 + bot worker/webdav 消费方能力探测降级。**行为零变化验收**：workspace 测试全绿 + 真机 Telegram 冒烟一次。

### Batch B1：cydrive-baidu 骨架 + 鉴权

crate 结构（对照 cydrive-telegram）：`oauth.rs`（device code + 刷新 + CredentialStore 回调）/`client.rs`（HTTP 面 + IPv4 dial + errno 映射 TransportError）/`transport.rs`（CloudTransport impl 骨架）。TDD：oauth 状态机/errno 三档映射/dial 配置全离线（mock HTTP）。

### Batch B2：上传/下载实现

- 上传：precreate → 4MB 分片 MD5 → superfile2（并发 worker）→ create；**断点续传差集**（Batch S 结论落地）；分片失败指数退避（复用 decide_retry 语义）
- 下载：download API → 302 禁重定向取 Location → dlink（带缓存，时效按 Batch S 实测）→ Range 206；`open`/`open_range` 映射
- TDD：三步曲表单格式（mock 百度端点，字节级断言——PCFS `api.go:488-573` 参数表为黄金参照）/差集逻辑/dlink 缓存失效

### Batch B3：cli 接线 + 真机验收

config `backend` 键（legacy json 拒收）+ setup 增百度分支（device code 引导）+ doctor 增百度检查项 + sync namespace A5 落地 + mount/stats/status 适配。真机验收：实例 C（百度）挂 Z: 盘 Explorer 拖入/拖出/播放 ↔ Telegram 实例 Y: 并存互不干扰 + sync 各自收敛。

### 门禁与收口

每批老规矩：TDD 红→绿、断言漂移审计、win workspace + wsl 双平台门禁、版本递增发布、decisions/AGENTS 入档。

## 不做（v1）

- 跨后端 union 视图/透明迁移/按策略落盘（v2 议）
- 进程内多后端并发（A4：实例即后端）
- 115/123 网盘后端（PCFS 已有 Go 实现，需求出现时按本计划的 trait 形态移植）
- 加密文件流播（PCFS 的 CTR+计数器推导随机访问解密是现成数学，`stream/ctr.go:235-273`——v2 与加密格式 v2 一起议）
- 百度文件操作全量镜像（search/share/缩略图等 xpan 长尾 API——用到再加）

## 规模与风险

- 估量：Batch S ~300 行（examples+报告）；R ~600 行重构；B1+B2 ~1500-2000 行；B3 ~500 行；测试约四成
- 最大风险：①自有 appkey 的限额形态未知（Batch S 首验，有止损点）；②OAuth 状态机复杂度（device code 轮询/过期竞态——PCFS `oauth.go:180-264` 参照）；③dlink 时效策略不当导致 403（spike 实测定参数）；④**法律/条款**：个人开发者 appkey 的使用范围须符合百度开放平台协议（负责人自查，代码侧不碰红线功能）
- 既有功能零破坏面：backend 默认 telegram，所有既有测试/真机路径行为不变（Batch R 重构除外——纯等价改造）

---

## Kickoff 指令（新会话粘贴即执行；负责人预授权 TBD——**Batch S 需先提供凭据**）

> **执行 docs/plans/2026-09-06-multicloud.md（多云后端，Batch S 起）**。模式：全程自主，不中途确认；可逆细节自行裁决结尾集中报告；仅破坏性操作或真实范围变更停下。要求：
> 1. 先读本计划全文 + AGENTS.md + docs/decisions.md；worktree `E:\GitHub\rs-CyDrive-t11`，分支 `feat/multicloud`（自 main 切出）。
> 2. **Batch S 前置凭据**：env `BAIDU_APP_KEY`/`BAIDU_SECRET_KEY` + 负责人完成 device code 授权提供的 access/refresh token（或 spike 内引导授权）；凭据绝不入库/入日志。
> 3. Batch S 按「验证驱动」执行（examples + 报告，非 TDD）；止损点触发即停留现场报告。
> 4. 后续批次严格 TDD + hub-and-spoke 子代理；门禁 win+wsl 双平台。
> 5. 中止条件：spike 止损点、契约矛盾、需要动 Python 兼容红线。

---

## 附录 A：PrivateCloudFS 百度情报索引（2026-09-06 研究固化，引用为该项目文件:行号）

| 情报 | 出处 |
|---|---|
| 端点：xpan/file（list/meta/search/quota/delete/move/precreate/create）、pcs/superfile2、xpan/file?method=download、openapi/oauth/2.0、xpan/nas?method=uinfo | `drivers/baidu/client.go:27`、`api.go:279,309,350,621-653,740,783`、`oauth.go:19-20`、`internal/baiduauth/config.go:372` |
| 三步曲精确参数：`path&size&isdir=0&autoinit=1&rtype=1&block_list=[url转义JSON数组]`、x-www-form-urlencoded；superfile2 multipart 字段 `file`、不设 Content-Length | `api.go:488-493,518-573` |
| 分片 4MB（官方 SDK 同款）；两遍读盘算 MD5；4 并发分片 worker | `api.go:23,386-402,462` |
| 断点续传：**未实现**（失败分片仅 log continue、盲调 create、不读 precreate 已传列表）——我们的改进点 | `api.go:448-451` |
| dlink+Range：download API 禁自动重定向 → 302 Location → `Range: bytes=start-` 期待 206，实测可用；每次 Seek 重换 dlink（无缓存——我们测时效优化） | `driver.go:314-402,618-751` |
| errno 语义：110=access 过期（自动刷新+重放一次）、111=refresh 过期（重授权）、-6=鉴权失败 | `client.go:196-203`、`lazy_multifs.go:19-21` |
| OAuth：授权码（localhost:4080 回调）/**device code（headless 可用，`authorization_pending` 轮询）**/PKCE 半成品有 bug（base64 非 S256，勿抄） | `oauth.go:23,180-264,268-289,335-403` |
| token 持久化：TokenCallback 回调上层（Full 写文件/View 仅内存）；PCFS 落盘密钥有硬编码弱默认（反例勿学，我们走 keyring） | `driver.go:61-65`、`client.go:119-129`、`baiduauth/config.go:26-65` |
| Windows 强制 IPv4 dial（CGO/DNS 优先 IPv6 连百度问题）；128KB 读缓冲/禁压缩/keep-alive | `client.go:18-24`、`driver.go:327-340` |
| fs_id 优先于 path（更可靠）；`%.0f` 格式化 fs_id 防科学计数法 | `api.go:170-171,896` |
| 秒传 return_type 分支：**未处理**（盲区，spike 验） | `api.go:279+` |
| QPS/31034：PCFS 全库零处理（借用第三方 appkey 未撞限额——自有 appkey 必须自测） | 全库 rg |
| 抽象反例：FileSystem 层 import 具体 baidu driver 做错误断言（错误模型未归一化的代价） | `lazy_multifs.go:4` |
| 错误日志脱敏 access_token | `client.go:217-228` |
