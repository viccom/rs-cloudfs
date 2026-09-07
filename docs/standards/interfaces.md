# 接口规范（Interface Standards）

> 状态：v1.0（2026-09-07）｜ 适用：全部 pub trait/公共 API/协议类型
> 来源：rs-CyDrive CloudTransport 演进得失（provided 方法非 breaking 先例 / 签名演进波及面控制）；PCFS Driver 接口四实现实证 + 错误模型反例

## 1. trait 设计规则

- 层边界 trait 一律 `#[async_trait]` 且 **dyn 兼容**（`Arc<dyn StorageDriver>` 直传）；
- trait 演进优先级：**provided 方法（默认实现）> 新可选 trait > 改签名**；改签名 = breaking，波及面清单必须先列（实现者/调用点/测试基建）并在 commit 中逐条列出；
- 每个层边界 trait 的文档注释必须写清：语义契约、错误语义、并发语义（可否并发调用）、生命周期（连接/刷新谁负责）；
- **能力探测代替 trait 分叉**：可选能力拆独立 trait（`RapidUpload`/`TokenEvents`/`ChangeFeed`…），消费方按 `Capabilities` + trait 探测降级，**绝不因后端缺能力而 panic**（baidu 实例无 INBOUND → 入站 worker 不启动 + 日志声明）。

## 2. StorageDriver 契约要点（D1，详细签名见设计文档）

- `UploadStager`：**commit-on-close** 语义——写入只是 staging，close/commit 才产生远端可见对象（与 WebDAV flush=PUT、PCFS create/close 同构）；实现方必须保证 staging 可丢弃不留垃圾；
- `reader(id, range)`：range 半开区间、越界钳制行为必须文档化并进 conformance kit；不支持 Range 的驱动声明无 `RANGE_READ` 能力；
- `list(dir, page)`：分页语义（游标/offset）统一在 Page 类型，驱动不得自行发明分页参数；
- **分块策略归驱动**（PCFS 实证正确）：telegram 自理 1900MB parts、baidu 自理 4MB superfile；上层只见 Entry 与字节流；加密块（cloudkit-crypto）与存储块正交。

## 3. 错误分类学（R2 的接口面）

```rust
enum StorageError {
    NotFound, Exists,
    Unauthorized { recoverable: bool },   // 驱动内先自救（如刷新 token 重放一次），失败才抛
    RateLimited { retry_after: Option<Duration> },
    QuotaExceeded, Invalid, Unsupported,
    Io(String), Unavailable(String),
}
```

- 每个驱动必须提供**错误映射表**（后端错误码 → StorageError），映射表以测试钉死（mock 端点回真实错误码断言映射）；
- 未知后端错误 → `Io`/`Unavailable` + 保留原始错误码与消息在上下文里（可诊断不丢信息）；
- 已有映射先例：telegram FloodWait → `RateLimited{retry_after}`；baidu 110→驱动内刷新+重放一次，失败 `Unauthorized{true}`，111→`Unauthorized{false}`，-6→`Unauthorized{false}`。

## 4. 协议/序列化类型规则（wire 兼容）

- 协议类型（sync wire/payload/配置）：新增字段必须 `#[serde(default)]` + `#[serde(skip_serializing_if = "Option::is_none")]`（rs-CyDrive secret/client_id 字段先例——None 时与旧格式字节级一致）；
- 每个协议类型变更附**双向兼容测试**：旧形态 JSON 可解析（None 默认）、新形态对旧消费者不炸（未知字段忽略）；
- 协议破坏性变更（如 0.6.0 secret 全端点）必须在 decisions.md 记录升级顺序（先客户端/先服务端）；
- 配置键新增同步三处：KNOWN_TOML_KEYS + legacy json 拒收清单 + validate（rs-CyDrive tier-1 先例）。

## 5. 命名与形态

- crate：`cloudkit-*`（核心/服务）+ `ck-*`（drivers）；二进制名 `cydrive`/`cydrive-sync-server` 保持；
- 方法名承载语义不承载实现（`reader` 而非 `download`）；`Range`/`Page`/`Entry` 等公共词汇类型放 L2 统一定义，禁止各层重复发明；
- 时间戳 f64（epoch 秒）延续 rs-CyDrive 数据模型；ID 类字符串（fs_id/msg_id）禁止经 JSON float（PCFS `%.0f` 教训——Rust 侧 serde string 形态传输）。

## 6. conformance kit 接口（D9）

`cloudkit-storage::conformance_suite!(mock_backend)`：每个驱动（含 mock）跑统一语义集，**最小断言集 v1**（Phase 2 手册可扩不可减）：

1. 上传往返：writer 写 N 字节 → commit → reader 读回逐字节相等（N 覆盖 0 / 1 / 跨块 / 多块）；
2. Range 语义：`reader(range)` 返回恰好 [start,end) 字节；end 越界钳制到 EOF；start≥size 的行为与驱动声明一致；
3. stat/list：上传后 stat 字段正确；list 分页遍历完整且稳定有序；
4. mkdir/delete 幂等：delete 不存在 → NotFound 或幂等 Ok（驱动声明其一并恒定）；mkdir 已存在 → Exists；
5. 错误映射表：对每个声明的后端错误码 mock 回放 → 断言映射到正确 StorageError 变体；
6. rename：文件与目录各一，旧路径 NotFound、新路径可读、内容不变；
7. 断点续传（声明 RESUME 时）：上传中途丢弃 stager → 重新上传同路径只补差集（驱动层可观测断言）；
8. 并发读：两个 reader 并发读同一路径互不干扰。

真机套件 `#[ignore]` 版本共享同一断言集。**新驱动 PR 的通过证据 = 离线套件绿 + 真机套件输出**（local 类豁免真机）。
