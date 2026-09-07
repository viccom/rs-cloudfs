# RsCloudFS 融合基线设计（rs-CyDrive × PrivateCloudFS → 全新仓库）v1.0

> **For Claude:** 本文档是设计裁决稿，尚非执行计划；执行按 §7 阶段计划逐批转写为 Kickoff。

**定位（2026-09-07 负责人指令）**：在 `E:\Rs_Codes` 新建仓库，fork rs-CyDrive 代码（保留 git 历史），融合 PrivateCloudFS 的优良设计（Go 代码不移植，移植的是**设计与经验**），重构为分层彻底的多云存储平台：**基于统一存储抽象，上层应用（盘符挂载/仪表盘/同步/CLI/未来桌面端）可低成本扩展，新云后端接入即享全部上层能力。**

**北极星延续**：「稳定好用」；融合是重组已验证的资产，不是重写——rs-CyDrive 的 527 测试与生产验证过的子系统（队列/同步/WebDAV/挂载）原样搬运。

---

## 1. 两项目资产盘点（继承清单）

### 从 rs-CyDrive 继承（代码 + 测试原样）

| 资产 | 价值 | 去向 |
|---|---|---|
| CloudTransport seam + MockTransport | 527 离线测试的根基 | 升格为 L2 存储抽象（§3） |
| 上传队列语义（有界队列/N worker/退避/降级/pending 保护/requeue） | 生产验证的可靠性核心 | L4 原样 |
| sync 引擎 + sync-server（LWW/墓碑/SSE 门铃/双活检测） | Telegram 后端的换机生命线 | L4 原样 |
| WebDAV 网关 + FakeLs + Windows 挂载 + davfs2 | **PCFS 完全没有的挂载能力**（核心差异化） | L5 原样 |
| update_hook 变更门铃（单点收口） | 实时同步的触发基建 | 泛化为 L3 MetadataEvent 总线（§4-D8） |
| CredentialStore + keyring 链 | 凭据安全（PCFS 弱默认密钥的反面） | L0 原样 |
| CLI 全家桶 + doctor/setup/migrate | 运维面 | L5 原样 |
| 平台 crate（挂载/注册表 cfg 纪律） | 跨平台纪律 | L5 原样 |

### 从 PrivateCloudFS 继承（设计 + 踩坑经验，非代码）

| 资产 | 价值 | 去向 |
|---|---|---|
| Driver 接口被 4 个真实实现验证 | 抽象形状的实证 | L2 trait 设计参照（§4-D1） |
| Supports 能力位 + 可选扩展接口（TokenCallback/StreamingEncryptedUploader） | 诚实的能力声明 | §4-D3 |
| **多卷聚合**：Registry + `driverName:path` ID + 实例 ACL | 单进程多云挂载的路线图 | §4-D6 VolumeId（v1 预留 v2 启用） |
| CryptoWrapper 装饰器 + CTR 随机访问 | 加密流播机制 | §4-D7（v2 格式用分块 AEAD 实现同等能力） |
| device code OAuth / errno 三档 / IPv4 dial / 三步曲参数 | 百度实战情报 | 已固化于 multicloud 计划附录 A |
| 熔断/降级/Prometheus 指标 | 服务韧性面 | L0 择机引入（非 v1） |
| Windows 自更新五连修 | 踩坑记录 | 未来自更新批的前置阅读 |
| **反面教训**：错误模型未归一化（FS 层 import 具体 driver） | 抽象泄漏实证 | §4-D2 错误分类学的存在理由 |

---

## 2. 目标架构（六层）

```
L5 应用层   cydrive-cli │ webdav-gateway │ web-dashboard │ bot-telegram(专属应用)
           （未来：desktop-tauri │ 更多网关：s3-gw/ftp-gw）
L4 服务层   upload-queue │ sync-engine(+sync-server) │ cache-lru │ crypto(v1 GCM/v2 AEAD)
           │ inbound-index(telegram) │ 通知(bot push)
L3 领域层   vfs(虚拟树/Entry/配额) │ metadata-db(SQLite) │ MetadataEvent 总线
L2 存储抽象 storage：StorageDriver trait + Capabilities + StorageError 分类学
           + AuthProvider(生命周期) + Volume/Registry + 一致性测试套件(conformance kit)
L1 驱动层   drivers：telegram │ baidu │ (未来: 115/123/local/s3)
L0 基础     http(代理/IPv4策略/连接池) │ keyring │ logging │ config
```

依赖纪律（比两个前身都严）：**L2 之上禁止 import 任何 L1 符号**（PCFS 的 lazy_multifs.go:4 反例永久禁止——用 CI 检查或 crate 边界天然保证：L2/L1 分 crate，L3+ 不依赖 drivers/*）。

## 3. 新仓库结构（fork 映射）

```
E:\Rs_Codes\rs-cloudfs          # 仓库名待裁决（§9）
├── crates/
│   ├── cloudkit-storage/       # 新：L2（trait/caps/errors/auth/registry/conformance）
│   ├── cloudkit-core/          # ← cydrive-core 改造（L3+L4：vfs/db/queue/cache/sync引擎/crypto）
│   ├── cloudkit-crypto/        # 新：加密方案 trait + gcm-v1(兼容) + aead-v2(流式)
│   ├── drivers/
│   │   ├── ck-telegram/        # ← cydrive-telegram
│   │   └── ck-baidu/           # 新（multicloud 计划 B1/B2）
│   ├── cloudkit-sync-server/   # ← cydrive-sync
│   ├── cloudkit-webdav/        # ← cydrive-webdav
│   ├── cloudkit-web/           # ← cydrive-web
│   ├── cloudkit-platform/      # ← cydrive-platform
│   └── cloudkit-cli/           # ← cydrive-cli
```

Phase 0 只做**纯搬迁 + 改名**（每步测试全绿，git 历史 mv 跟踪），行为零变化；`cydrive` 二进制名保留（用户部署位不变）。

## 4. 核心设计决策（D1–D10）

**D1 StorageDriver trait**（合并两版之长）：
```rust
#[async_trait]
pub trait StorageDriver: Send + Sync {
    fn volume(&self) -> &VolumeId;                       // §D6
    fn capabilities(&self) -> Capabilities;
    async fn list(&self, dir: &RelPath, page: Page) -> Result<Vec<Entry>, StorageError>;
    async fn stat(&self, path: &RelPath) -> Result<Entry, StorageError>;
    async fn mkdir(&self, path: &RelPath) -> Result<(), StorageError>;
    async fn delete(&self, id: &EntryId) -> Result<(), StorageError>;
    async fn rename(&self, from: &RelPath, to: &RelPath) -> Result<(), StorageError>;
    async fn reader(&self, id: &EntryId, range: Option<Range>) -> Result<ByteStream, StorageError>;
    async fn writer(&self, path: &RelPath, hint: &WriteHint) -> Result<UploadStager, StorageError>;
    async fn quota(&self) -> Result<Quota, StorageError>;
}
```
- `UploadStager`：commit-on-close 语义（rs-CyDrive 的 flush=PUT 提交点与 PCFS 的 create/close 上传**本来同构**，统一于此）；`WriteHint` 携带 size/明文哈希/秒传提示（RapidUpload 可选 trait 消费）。
- 可选 trait：`TokenEvents`（鉴权生命周期回调 → CredentialStore 持久化）、`RapidUpload`、`ChangeFeed`（后端变更推送，百度无/未来 local 有）。
- 分块策略归驱动（PCFS 实证正确）：telegram 自理 1900MB parts；baidu 自理 4MB superfile；**加密块（D7）与存储块正交**，core 只认识加密块。

**D2 StorageError 分类学**（PCFS 最大教训的正面答案）：
`NotFound / Exists / Unauthorized{recoverable} / RateLimited{retry_after} / QuotaExceeded / Invalid / Io / Unavailable / Unsupported`
Telegram FloodWait → `RateLimited{retry_after}`；百度 110→驱动内自动刷新重放一次否则 `Unauthorized{true}`、111→`Unauthorized{false}`。**L3+ 永远只见 StorageError**。

**D3 Capabilities 能力位**：`RANGE_READ / RESUME / MULTIPART / SERVER_SIDE_MOVE / RAPID_UPLOAD / AUTHORITATIVE_INDEX / CHANGE_FEED / INBOUND / CHAT`。消费方启动探测降级（baidu 实例无 INBOUND → 入站 worker 不启动并日志声明，绝不 panic）。

**D4 影子索引 vs 权威索引（本会话最深洞察的制度化）**：
- `AUTHORITATIVE_INDEX=true`（baidu/115/123/local）：后端 list 即真相 → 新机器 bootstrap = 列目录重建 db；sync 是可选加速器；
- `=false`（telegram）：索引只存在于本地 db + sync 协议（bot 读不了历史）→ sync 是必需基建。
- db 层提供 `rebuild_from_backend()`（权威驱动专用）；VFS 元数据解析顺序：本地 db → (权威) 后端 → (影子) sync。

**D5 实例模型演进**：v1 保持「实例=后端」（config `backend` 键，A4 裁决不变）；v1.5+ 按需演进「实例=卷集合」。

**D6 VolumeId**（PCFS `driverName:path` 的 Rust 化）：`EntryId = (VolumeId, BackendHandle)`，VolumeId = `"telegram:<ns>" | "baidu:<uid>"`。**v1 只有一个卷，但 ID 格式从第一天就带卷**——这是「上层应用方便扩展」的关键预留：多卷时代 WebDAV 网关可挂 `Y:=卷1, Z:=卷2` 或 union 根，L3/L4/L5 代码零返工。

**D7 加密层 = 装饰器 + 双格式**：`CryptoScheme` trait（`encrypt_stream` / `decrypt_range`）；v1 GCM 整文件（Python 互操作，冻结维护）；**v2 分块 AEAD（STREAM 构造，256KB–1MB 块）= 流式上传（零 .enc.tmp、常量内存）+ 随机访问（加密视频拖播，PCFS 同体验但有认证）+ 流式水合**；口令+PBKDF2 延续；scheme 入 Entry 元数据 + sync payload 可选字段。v2 批即原计划 E 批，提前至百度落地前。

**D8 MetadataEvent 总线**：update_hook 门铃泛化——`files 表变更`事件（含 volume 维度）供 sync（推）、web hooks（未来）、bot 通知订阅；取代「唤醒埋点」的一切手工面。

**D9 一致性测试套件（conformance kit，两前辈都没有的东西）**：cloudkit-storage 提供 `conformance_suite!()` 宏/函数——每个驱动跑同一套语义测试（上传往返/Range/删除语义/错误映射/断点续传），Mock 后端离线跑 + `#[ignore]` 真机套件。**新驱动过套件 = 全部 L4/L5 能力自动可用**——这是「接入即扩展」的质量保障，比任何文档承诺都硬。

**D10 sync 定位**（沿用 2026-09-06 会话结论）：cydrive-sync-server = 虚拟视图层实例间复制协议；影子索引后端必需、权威索引后端可选加速（省配额+实时+大盘冷启动）；一台 server 服务全部 namespace；客户端按后端定默认策略。

## 5. 阶段计划（吸收既有 multicloud 计划为 Phase 1–2）

| 阶段 | 内容 | 门 | 量级 |
|---|---|---|---|
| **Phase 0 搬迁** | 建仓库 E:\Rs_Codes、fork 保历史、crate 改名重排（§3）、依赖纪律（L2/L1 分 crate）落地 | 全测试绿 + 双平台构建 + 生产位零变化（旧仓继续跑） | 纯搬迁 ~1 天 |
| **Phase 1 = multicloud S+R+E** | 百度 spike（凭据已验证）→ trait 瘦身（D1–D4 落地 cloudkit-storage）→ 加密 v2 流式（D7） | spike 报告/止损点；527→N 测试全绿；GCM 文件行为不变 | S~300 / R~800 / E~800 |
| **Phase 2 = ck-baidu** | B1 骨架+OAuth → B2 上传下载（断点续传差集/Range/dlink 缓存）→ B3 接线+真机双盘并存验收；conformance kit 首跑 | B3 验收：Z: 盘 Explorer 全流程 + Y:/Z: 并存 + sync 各自收敛 | ~2000 |
| **Phase 3 特性移植（择机）** | 115/123/local 驱动（PCFS 情报直引）；多卷挂载（D6 启用）；熔断/指标；desktop-tauri；自更新（先读 PCFS 五连修） | 各自立项 | 按需 |

## 6. 不做（v1 基线）

跨后端 union 视图与文件迁移；进程内多后端（D5 保持实例=后端）；PCFS 的 Full/View 中心节点模式（sync-server 已覆盖多机，不引入第二套）；纯 CTR 无认证格式（仅当需要 PCFS 字节互操作时再议）；重写任何已验证子系统。

## 7. 风险登记

- **双仓过渡期**：搬迁后旧仓冻结策略须裁决（§9-2），避免双头维护；
- **纯搬迁的隐性破坏**：以「每步全测试绿 + git mv 历史跟踪」约束，禁一把梭；
- **改名 churn**：crate 名改、二进制名 `cydrive` 不改（部署位零感知）；文档/脚本随批更新；
- **trait 设计过早固化**：D1 在 Phase 1 R 批以「telegram + mock 两实现」提炼，baidu 落地后允许一次修订（spike 先行正是为此）；
- **PCFS 经验的时效性**：附录 A 情报截至 2026-09-06，百度 API 变化以 spike 实测为准。

## 8. 与既有文档的关系

`docs/plans/2026-09-06-multicloud.md`（S/R/E/B 批定义 + PCFS 情报附录）**整体并入本计划 Phase 1–2**，其 A1–A6 裁决被 D1–D10 吸收或修订（A5 namespace 修订为「telegram 派生不变、baidu 带前缀构造」，理由见 multicloud 计划 2026-09-06 会话讨论：零现有实例影响）。sync 定位分析并入 D10。

## 9. 待负责人裁决项

1. **仓库名**：建议 `rs-cloudfs`（Rs_Codes 目录命名惯例 + 承继两项目血统）；crate 前缀 `cloudkit-*` 可换；
2. **旧仓策略**：建议 Phase 0 完成后 rs-CyDrive 冻结（仅 hotfix），新仓为唯一主线；生产部署位（D:\Tools/WSL/云 sync-server）随 Phase 2 完成后统一切换；
3. **bot-telegram 的层级归属**：建议维持 L5 应用形态但 v1 不拆 crate（留在 ck-telegram 内的 feature 面上），拆分等第三个后端出现再定；
4. **Phase 1 内 E 批（加密 v2）与 R 批顺序**：建议 R→E（装饰器架在新 trait 上），如更急于解决加密大文件上传痛点可 E 提前（独立成立）。
