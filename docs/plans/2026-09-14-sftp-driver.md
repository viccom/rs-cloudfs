# Phase 4 计划：ssh/sftp 存储驱动

> 状态：**已批准（2026-09-14，SF0 三项拍板，见 §8）**（2026-09-14 立）｜ 批次编号建议 SF0–SF5
> 前置研究：三轮外部项目勘察（2026-09-14，均为只读，情报见本文附录 A）
> 上游标准：`docs/standards/architecture.md`（六层 + R1–R7）、`docs/standards/driver-onboarding.md`（新驱动验收依据）、`docs/standards/interfaces.md`（StorageDriver 契约/conformance 八断言）、`docs/plans/2026-09-07-cloudfusion-foundation.md`（D4 权威索引）
> 需求口径（负责人 2026-09-14）：**自用项目，不对外分发**——借鉴外部项目实现时不受许可证传染约束；但"能跑起来、坑最少"是唯一标准，且不得因此降低本仓既有的门禁纪律。

## 0. 一句话方案

新增 L1 驱动 crate `crates/drivers/ck-sftp`，实现 `StorageDriver` + `CloudTransport` 双面（照 `ck-local` 的薄壳形态），以 **`russh` 0.63 + `russh-sftp` 3.0（`ring` 后端）** 为协议栈；组合根以 K30 三 feature 模式接入 `sftp` 开关；**L2 以上零改动**（不改 `StorageDriver`/`Vfs`/上传队列/加密/`rebuild` 的任何一行），现有 telegram/baidu/local 驱动零侵入。

## 0.1 方法论裁决：以哪份研究为主？（负责人问询的直接回答）

**裁决：以 `ck-local`（本仓现有驱动）+ `russh-sftp` 官方 API 为主干，aeroftp 为"坑清单"辅证，termcp 仅作路线印证。三份外部研究都不作为实现蓝本。**

理由是它们与我们的场景差异，而非许可证：

| 参照 | 与我们的架构差异 | 可借鉴的部分 |
|---|---|---|
| **ck-local（本仓）** | **零差异**——同为路径寻址型驱动，双面结构现成 | **主干**：目录结构/双面薄壳/句柄往返/入口式 |
| aeroftp | 同步 trait + 自研 GUI 抽象 + 多点弹窗；为 2.x 写绕行代码 | **坑清单**（§4.4 + 附录 A.2）：句柄 close、size 校验、APPEND 禁令、mtime |
| termcp | 同步 `RemoteFs` trait + 临时文件全量落盘；不校验 host key | **路线印证**（弃 libssh2 选 russh） |
| russh-sftp 官方 API | 与 3.0 实际能力一致 | **实现底座**：`Client::Config` 的并发旋钮、`server::Handler` 默认实现 |

**为何不以 aeroftp 为主**：它的 400+ 行 read-ahead、连接池、句柄预算全是为 russh-sftp **2.x 的串行读缺陷**写的补丁，锁在 2.1 所以不知道 3.0 已修（§1.3）。以它为主干等于**主动复制一份已过时的绕行代码**。它的价值在"哪里会踩坑"，不在"代码长什么样"。

**为何不以 termcp 为主**：它是同步接口 + 临时文件中转的交互式 TUI，与我们的全异步 + 流式挂载架构相反；且它不校验 host key，这一条我们必须反着做。

## 0.2 "像 SFTP 这样的驱动，多久能加一个？"（负责人问询的量化回答）

按本计划的批次拆分，**一个新"路径式"驱动（如 FTP/SMB/NFS）的边际成本大致是 SFTP 的 40–60%**，因为一次性投入已被摊掉：

| 成本项 | 一次性（SFTP 承担） | 后续驱动可摊（或免） |
|---|---|---|
| `compiled_drivers()` 可扩展化改写 | ✔ 需重构 | 免（改完即成固定模式） |
| 进程内测试桩 | ✔ 需从零搭 | **大幅可摊**（SFTP 桩已是网络 FS 通用骨架；FTP 可复用同一套断言体） |
| 12 处装配点 | ✔ 需摸清 | 照抄清单，逐条追加 |
| 驱动本体九方法 | ✔ | 同量级（协议复杂度决定） |
| conformance 八断言 | ✔ | 直接复用 |
| 交互式登录/鉴权（如 115/123 的扫码） | 不适用 | 若新驱动需要，则是**额外的独立成本** |

**关键结论**：本计划的真实产出不只是"支持 SFTP"，而是**把"新增一个存储驱动"变成一条有测试桩、有接入清单、有验收模板的流水线**。SF2（桩）与 SF1（`compiled_drivers` 重构）是这条流水线的一次性投资，这也是它们值得单独成批的原因。


## 1. 选型裁决（三轮研究后的定论）

### 1.1 协议栈：russh + russh-sftp（三个项目交叉印证）

| 参照 | SSH 栈 | crypto 后端 | 与我们的关系 |
|---|---|---|---|
| 本仓探针（实测编译通过） | russh 0.63.3 + russh-sftp 3.0.0 | `ring` | — |
| termscp（MIT，v1.2.0） | `remotefs-ssh` `features=["russh"]`，**已弃用 libssh2** | aws-lc-rs | 印证纯 russh 路线 |
| aeroftp（GPL，v4.1.9） | russh 0.63.1（SFTP 主路径）+ ssh2（仅 rsync 通道） | **`ring`** | 与我们完全一致 |

**裁决定论**：`russh = { version = "0.63", default-features = false, features = ["ring", "rsa", "async-trait"] }` + `russh-sftp = "3.0"`。

- **弃用 aws-lc-rs**：需 C/CMake 工具链，且 `ring 0.17.14` 已在本仓 Cargo.lock（经 rustls）——零新增高风险构建依赖。aeroftp 独立做了同一选择。
- **弃用 ssh2/libssh2**：C 依赖 + 与 `deny.toml`「全 workspace 只链接一个 SQLite C 库」的取向冲突；termcp 已从依赖树移除，aeroftp 仅在 rsync 通道保留。
- **弃用 openssh-sftp-client**：连接层依赖外部 `ssh` 可执行文件，对托盘常驻的自用工具是额外部署面。

### 1.2 版本必须 ≥ 0.63（安全，非版本洁癖）

aeroftp 的推理可直接吸收（其 `Cargo.toml:401-437`）：

- **GHSA-47hw-gvq5-r2gm**（High，CVSS 7.5，2026-08-23）：**客户端侧**缺陷——恶意/被控服务器可对客户端从未打开的 channel ID 触发 channel 级 `Handler` 回调，导致 panic、伪造退出状态、channel 状态失步。对我们这种"连不可信服务器"的挂载卷是**直接威胁**。
- **GHSA-w3jg-pjxf-73p4**（Moderate）：ML-KEM768+X25519 混合密钥交换缺 X25519 零点校验，0.63.0 修复。
- **0.62 线无 backport，且这是算术不是 changelog**：0.62.7 发布于 2026-08-17，早于 0.63.0（08-21）四天——不可能包含其后发布公告的修复。`cargo update` 会停在 0.62 并保持脆弱。
- **`cargo audit` 对三条公告失明**（无 RustSec 编号）：0.62.5 修的三条（含 pre-auth panic：对恶意/损坏服务器的**每一次** SFTP 连接都会触发）无 RustSec 条目。→ **纪律：升级驱动不只看 `cargo deny` 的绿色。**
- RUSTSEC-2026-0154（SSH-agent 帧长度无界分配）影响 `<0.60.3`，0.63.x 不受影响。

### 1.3 russh-sftp 3.0 的关键红利：并发读默认开启（上一轮判断的重大修正）

aeroftp 锁定 **2.1**，为此写了 400+ 行自研 read-ahead，其注释（`sftp.rs:3173-3187`）记录：

> russh-sftp 的 `File` 串行读（每个 `poll_read` 等一个 `SSH_FXP_READ`；upstream AspectUnk/russh-sftp#70），单条 SSH 连接的下载远低于链路带宽，而写是流水线的（`write_nowait`），**同一会话上传比下载快约 3 倍**；它够不到私有的 `RawSftpSession`，加不了对称的 `read_nowait`。

**3.0.0 已修复**——`russh-sftp-3.0.0/src/client/mod.rs:32-55`：

```rust
pub struct Config {
    pub max_packet_len: u32,            // 256 KiB
    pub max_concurrent_reads: usize,    // 默认 16  ← 2.x 缺的就是这个
    pub max_concurrent_writes: usize,   // 默认 16
    pub max_write_packet_len: u32,      // 32 KiB
    pub request_timeout_secs: u64,      // 10s
}
```

`file.rs` 的 `ReadState` 已是完整流水线实现：`request()` 先探测服务器实际读长（`chunk_len` 未知时只发 1 个），探测到后填满到 `max_concurrent_reads` 个 `read_nowait` 在飞，并正确处理短读（丢弃短读之后的请求、从缺口重试）。

**结论：我们不写 read-ahead、不写多连接池对抗串行读。** 若实网仍不足，下一步才考虑 aeroftp 验证过的"N 条独立 SSH 连接做文件内分段"（其路径 `download_intra_file_pooled`，≥250 MiB 启用，实测 300 MiB 从 144.75s → 25.46s）——列为可选增强，不进 SF1–SF3 范围。

## 2. 集成前已核实的事实（零侵入的根据）

以下均经本轮通读代码确认，是"L2 以上零改动"这一主张的证据：

1. **L3 `Vfs` 从不直接见 `StorageDriver`**——它只持有 `Arc<dyn CloudTransport>`（`crates/cloudkit-core/src/vfs.rs:333-339`），驱动/stager 世界仅经队列 worker 的 `transport.upload()/upload_stream()` 触达。
2. **写路径无后端分叉**——`commit_put`（`vfs.rs:434-501`）无条件 upsert `files` 行 + 入队，不看 backend/capabilities/multipart；`upload_queue.rs` 内 `capabilities` 零命中。
3. **加密在 core 层**，按 `VfsConfig.encryption_password/_scheme` 工作，驱动内零 crypto 引用 → **新驱动自动继承 gcm / aead_v2（含刚定的流式默认）**，不实现任何加密。
4. **读路径要求 DB 行**——`hydrate`/`open_read` 首先 `db.get_file(rel)`，缺失即 `NotFound`（`vfs.rs:508-512`、`:776-780`）。local 同待遇。这正是百度卷踩过的"外部传入文件不可见，需 rebuild"故事，SFTP 完全同构（`rebuild` 对路径型后端可用；`rebuild.rs` 零 backend 分支）。
5. **`authoritative_index` 在生产代码里只被启动横幅消费**，无逻辑分叉——新驱动声明它只影响横幅与语义自述。
6. **装配是单一后端无关的 core builder**（`lib.rs:526-542`），telegram/baidu/local 走同一 Vfs/队列机械。

## 3. 零侵入的接入面（穷举 12 处，均为**追加**而非改动）

| # | 位置 | 改动性质 |
|---|---|---|
| 1 | `crates/cloudkit-core/src/config.rs:203-211` `Backend` 枚举 | 追加 `Sftp` 变体 |
| 2 | 同上 `:213-222` `as_str()` | 追加 `"sftp"` 臂 |
| 3 | 同上 `:67-106` `KNOWN_TOML_KEYS` | 追加 `sftp_*` 键 |
| 4 | 同上 `VOLUME_SCOPED_KEYS`（`:264+` 附近） | 追加同名键（bipartition 测试会钉完整覆盖） |
| 5 | 同上 `validate()` | 追加 `backend == Sftp` 时的必填/取值校验块 |
| 6 | `crates/cloudkit-cli/Cargo.toml` | 追加 `ck-sftp` optional 依赖 + `sftp` feature + `default` 追加 |
| 7 | 新 crate `crates/drivers/ck-sftp` + workspace `Cargo.toml` members | 新增 |
| 8 | `lib.rs` `BackendTransport` 枚举 + 5 个方法臂（`volume`/`caps`/`sync_namespace_key`/`clone_dyn`/`web_quota_snapshot`） | 追加 arm |
| 9 | `lib.rs` 两个 dispatch 函数（`build_backend_transport_with` / no-driver twin） | 追加 arm + `build_sftp_transport` helper |
| 10 | `lib.rs` `SFTP_DRIVER_REQUIRED` 文案（照 `:101-112` 三件套形态）+ `compiled_drivers()` | 追加（**注意**：`compiled_drivers` 现为 3 位 cfg 元组穷举，第 4 位需重写为可扩展形态） |
| 11 | `lib.rs` `build_driver()`（rebuild 用）/ `sync.rs` `is_sync_supported` / doctor / setup | 追加 arm |
| 12 | web 卷表单 + i18n（`volumes.html` / `volumes.js` / `i18n.js`） | 追加 backend 选项与字段 |

**关键约束**：`compiled_drivers()` 目前是 `match (bool, bool, bool)` 的 8 臂穷举（`lib.rs:122-137`），加第 4 个驱动会变成 16 臂。SF1 需把它改写为可扩展形态（如按固定顺序拼接的 `const` 片段数组），**并保持既有 3 驱动组合的输出字符串逐字不变**（现有测试 `compiled_drivers_lists_the_feature_set_in_fixed_order` 是零漂移基线）。

## 4. 驱动设计要点（ck-sftp）

### 4.1 形态

照 `ck-local` 的双面结构（这是最贴近的类比——SFTP 就是"网络上的 local"）：

```
crates/drivers/ck-sftp/
  src/lib.rs            导出 + pub fn factory(SftpParams) -> Result<Arc<SftpDriver>>
  src/driver.rs         StorageDriver 九方法 + 错误映射表
  src/client.rs         会话建立/认证/host key/重连
  src/transport_face.rs CloudTransport 薄壳（照 ck-local/src/transport_face.rs）
  src/config.rs         参数结构体（配置 map → 纯函数解析）
```

### 4.2 能力位（R4：逐位依据）

| 位 | 取值 | 依据 |
|---|---|---|
| `range_read` | **true** | SFTP 原生 offset 读（`File` 实现 `AsyncSeek`+`AsyncRead`；3.0 流水线读）→ 支撑 K47 流式解密播放 |
| `resume` | false | 无远端分片会话；断点续传由上层 `.enc.tmp`/缓存层承担，驱动不声明 |
| `multipart` | false | SFTP 无分块上传原语（单流写入） |
| `server_side_move` | **true** | SFTP 有原生 `rename`（服务端单侧，O(1)） |
| `rapid_upload` | false | 无内容寻址去重 |
| `authoritative_index` | **true** | 远端文件系统即真相（同 local/baidu）→ rebuild 可用 |
| `change_feed` | false | SFTP 无变更推送 |
| `inbound` / `chat` | false | 非 bot 后端 |
| `remote_delete` | **true** | `delete_remote` 经 transport 面真删远端（照 local） |

### 4.3 错误映射表（R2，mock/桩回放钉死）

| SFTP 状态 / 传输错误 | StorageError |
|---|---|
| `SSH_FX_NO_SUCH_FILE` | `NotFound` |
| `SSH_FX_PERMISSION_DENIED` | `Unauthorized { recoverable: false }` |
| `SSH_FX_FAILURE`（已存在等） | 按上下文 `Exists` / `Invalid` |
| 认证失败（password/key 均拒） | `Unauthorized { recoverable: false }` |
| 连接/会话丢失、超时 | `Unavailable` |
| 其他未知 | `Io`（保留原始码与消息，R2 要求） |

**注意**（aeroftp 教训，`:102-120`）：`exists`/`stat` **不得**把 `PermissionDenied`、`ConnectionLost` 混同为"不存在"——否则不可读父目录下的根会被误判为路径缺口。本仓 `VfsError` 分类学对此敏感。

### 4.4 三处硬仗纪律（直接吸收的实战教训）

1. **每次 `open` 必须有 awaited `close`（含失败/提前返回分支）**。aeroftp 实测：drop 只排入 `close_nowait`，warm 复用会超出服务端 per-session 句柄上限，5000 文件扫描出现 "Limit exceeded: handle limit reached"。**测试须在服务端计数活句柄**。
2. **上传 `close` 后校验远端大小**（比对本地 size，短/零即判失败）。这是 aeroftp 从 0 字节上传 bug 中活下来的唯一持久修复（`:1050-1085`）。
3. **续传/覆盖写绝不用 `APPEND`**；用 `OpenFlags::WRITE | CREATE`（无 TRUNCATE）+ 显式 seek，且**offset 用重新 stat 的真实远端大小钳制**（`:2282-2295`、`:1139-1148`）。理由："部分服务器设置 APPEND 时忽略 seek 而在 EOF 写"。

### 4.5 host key 策略（已拍板 2026-09-14：显式接受 + 指纹落盘，见 §8-D2）

技术底座已明确：`russh::keys::known_hosts::{check_known_hosts, learn_known_hosts}` 提供现成解析/写入（aeroftp 的 `host_key_check.rs` 即基于它，含 566 行实现与 11 个单测——**仅作设计参照，代码不抄**）。三态语义 `known | unknown | changed`、`changed` 带 1-based 行号、原子写（temp+rename）。

**与 aeroftp 的关键差异**：它是交互式桌面应用，可弹窗让用户看指纹；**我们是无人值守挂载卷，不能弹窗**。因此必须在两个非交互形态中选一：

- **D2-甲（推荐）显式接受 + 落盘指纹**：首次遇到未知 host key 时，若卷配置未记录指纹 → **拒绝连接**并返回可行动的 `Unauthorized`，要求用户显式确认（`cydrive` 子命令或 web 面板）；确认后指纹写入卷配置（或独立 trust 文件），后续静默校验，不匹配即拒。
- **D2-乙 静默 TOFU**：首次自动记录并继续。方便，但把首连窗口的 MITM 风险留给了用户。

**无论选哪个，都必须拒绝 `Ok(true)` 的无条件接受**（termcp 的做法），因为我们是长期自动重连的后台卷，与"用户在终端里主动连自己的服务器"的交互式场景风险不同。

## 5. 批次划分（每批独立可验收，配套任务单）

> 纪律：每批 TDD 红→绿留证；跟踪单 `docs/tracking/phase4-sftp.md` 每批收口更新并随 commit 提交。

### SF0 — 决策与门禁前置（无代码）
- 落 decisions：选型（russh+ring+3.0）、版本下限 0.63 的安全理由、`compiled_drivers` 重写、license 前提（自用）
- 敲定 §8 三个待拍板项（认证方式 / host key 形态 / 是否起步多连接）
- **验收**：decisions 条目 + 本计划获批

### SF1 — 驱动骨架 + 配置接入（无真实网络）
- `ck-sftp` crate 落地：`StorageDriver` 九方法 + 四维文档 + 错误映射表
- 配置键三处同步（`KNOWN_TOML_KEYS` / `VOLUME_SCOPED_KEYS` / `validate`）+ 卷模式互斥规则
- `compiled_drivers()` 改写为可扩展形态，**3 驱动组合输出逐字不变**（零漂移）
- **验收**：workspace 三步门禁绿（test/clippy/fmt）+ check_layers 绿 + `compiled_drivers` 既有测试零改动通过

### SF2 — 测试桩：进程内 SFTP 服务端（关键路径）
- **本计划阶段已实测跑通**（见附录 C：Windows 端到端 PASS），SF2 从"要估工的高风险批"降级为"照已验证骨架落地"
- 用 `russh::server` + `russh_sftp::server::run` 搭 hermetic 桩。**比预想简单得多**：`russh_sftp::server::Handler` 只有 `unimplemented()` 是必需方法，其余 20 个全有默认实现——**不需要** aeroftp 那种手写 SFTP v3 包循环（它是为了注入 STAT 延迟才那么做的）
- **Windows 约束已正面解决**：桩不依赖 `$HOME`、不依赖 Docker、不依赖 known_hosts（信任策略由客户端 handler 注入）。aeroftp 同类桩只能跑 Unix 是因为它用重定向 `HOME` 隔离 known_hosts——**我们避开这个做法**，把 known_hosts/信任文件路径做成显式参数
- **桩侧必守语义（实测踩到）**：`readdir` 必须在第二轮返回 `StatusCode::Eof`，否则客户端 `read_dir` 循环永不终止。这是 SFTP 协议语义（循环到 EOF 才停），不是库缺陷
- **验收**：桩完成 connect → auth → subsystem → list/stat/range-read/error 往返；CI 可见的 hermetic 测试跑绿

### SF3 — conformance + 装配接线
- 过 `cloudkit_storage::conformance_suite!(...)` 离线八断言（interfaces §6）
- 12 处接入点全部落地（含 doctor / setup / web 卷表单 / i18n / rebuild 的 `build_driver` 臂）
- `SFTP_DRIVER_REQUIRED` 文案 + `--version` 驱动清单
- **验收**：conformance 八条绿（贴输出）+ 三驱动既有测试零漂移 + 裁剪构建（K30）通过

### SF4 — 真机矩阵
- 本地 WSL2 现成可当测试服务器（`localhost:22`）；凭据按仓库红线（env 优先，不入代码/日志）
- 矩阵：上传→回读逐字 / Range 跨窗口读（支撑流式播放）/ 大文件吞吐实测 / 断线重连 / 覆盖写 / 符号链接契约 / rebuild 收敛
- **验收**：`#[ignore]` 真机测试输出留证 + 吞吐数字记录（对比 aeroftp 的 300 MiB 基线）

### SF5（可选，按 SF4 实测决定）— 吞吐增强
- 若单连接（3.0 流水线读 ×16）在实网不足：引入文件内分段多连接（照 aeroftp 验证过的思路）
- 若不需要则明确销账，不悬空

## 6. 风险与缓解

| 风险 | 缓解 |
|---|---|
| 依赖增量重（russh 全协议栈：ECC/RSA/ML-KEM/cipher） | feature 门控（K30 三件套）+ `ring` 复用既有依赖；裁剪构建可完全排除 |
| 构建需 MSVC/libclang 类约束 | **不引入 C 依赖**（ring 纯 Rust，已在 lock） |
| 3.0 是较新大版本，API 曾漂移（0.63 把回调签名改为 `PublicKeyOrCertificate`，本轮实测踩到） | 版本精确约束 + 编译期探针测试；升级走 PR 审查 |
| 进程内 SFTP 桩工作量被低估 | **已实测跑通**（附录 C）——`russh_sftp::server::Handler` 仅 `unimplemented()` 必需，无需手写包循环；Windows 可行且不依赖 HOME/Docker |
| DB 行是读路径前提，外部传入文件不可见 | 与百度卷同构：rebuild 即可收敛；文档/UI 明示预期（已有先例，非新增问题） |
| `compiled_drivers` 改写引入漂移 | 既有测试作为零漂移基线，改写前后逐字比对 |

## 7. 明确不做（本阶段范围外）

- ssh-agent / SSH 证书认证（aeroftp 的 SFTP provider 同样不支持；agent 只在它 rsync 通道有）——除非 SF0 拍板要
- 远程临时文件 + rename 的原子上传（aeroftp 也不做：直接写目标路径）
- 面向播放器的 HTTP-Range 服务端（aeroftp **没做到**——其 CLI `serve http` 是整文件读入内存再切片。**我们的 `range_read` 是驱动级真流式，恰好超过它**）
- 分块上传/秒传/变更推送（SFTP 无此原语）
- SCp 协议（只做 SFTP subsystem）

## 8. 拍板记录（SF0，2026-09-14 负责人已决）

- **D1 认证方式 = 密码 + 私钥**（负责人原话「便于实现自动化」）。私钥支持含解锁 passphrase（`sftp_private_key_passphrase` 凭据键，加密私钥不支持等于半个私钥支持）。**keyboard-interactive 与 ssh-agent 不做**（v1 范围外挂账——aeroftp 教训：个别服务器（SourceForge 类）只收 keyboard-interactive，遇到再议）。凭据按 R3 走 env > config > keyring，`sftp_password` / `sftp_private_key_passphrase` 入 `SECRET_VALUED_KEYS` 脱敏。
- **D2 host key 形态 = 显式接受 + 指纹落盘**（§4.5 甲案）：未记录指纹的 host key → **拒连**并返回可行动错误（指明接受途径）；接受动作 = 显式用户行为，指纹持久化后静默校验；**指纹变更 → 恒拒**（MITM 信号），救济 = 显式移除旧指纹重新接受。绝不无条件接受（termcp 教训）。
- **D3 起步多连接 = 否**。**概念澄清（落档防复混）**：负责人答复中的「不同认证各起一个实例」指的是**多卷模式**（不同服务器/账号各一卷，Phase 2.5 既有能力，SFTP 作为普通卷自动继承——无需任何额外工作）；D3 问的是**同一卷内为吞吐开 N 条并行 SSH 连接做文件内分段**（aeroftp 的 `download_intra_file_pooled`，其 300MiB 实测 144.75s→25.46s）。拍板：**起步单连接**（russh-sftp 3.0 会话内 `max_concurrent_reads: 16` 流水线读），SF4 真机实测吞吐，不足再立项 SF5。

## 9. 验收总纲（driver-onboarding §8 清单对照）

- [ ] crate 形态/依赖方向合规（check_layers 绿）
- [ ] StorageDriver 九方法 + 四维文档 + 错误映射表（桩回放钉死）
- [ ] Capabilities 逐位依据注码
- [ ] 配置键三处同步 + validate 文案可行动
- [ ] conformance 离线八条绿（贴输出）
- [ ] 真机套件输出（SF4 矩阵）
- [ ] workspace 三步门禁全绿 + 驱动裁剪构建腿
- [ ] 装配点/doctor/setup 分支 + 能力横幅
- [ ] 文档联动：README 状态表 / AGENTS 计数 / 本计划 + tracking 更新

---

## 附录 A：三轮外部研究情报（2026-09-14，只读勘察）

### A.1 三项目对比

| | termscp | aeroftp | 本仓探针 |
|---|---|---|---|
| 版本 | v1.2.0（2026-09-03） | v4.1.9-116（2026-09-14） | — |
| 许可证 | MIT | **GPL-3.0-or-later** | — |
| 形态 | 交互式 TUI | Tauri 桌面 GUI | — |
| SSH 栈 | `remotefs-ssh` + russh | russh（SFTP 主路径）+ ssh2（rsync） | russh 0.63.3 |
| crypto | aws-lc-rs | **ring** | **ring** |
| host key | **完全不校验**（`NoCheckServerKey`，全仓无 known_hosts） | **完整 TOFU**（`host_key_check.rs` 566 行 + 11 单测） | 待设计 |
| Range 读 | 无（同步 trait + 临时文件全量落盘） | 有但 `Vec` 物化 + 100 MiB 上限；**无播放器流式** | `AsyncSeek` 流水线可用 |
| 测试桩 | Docker/testcontainers | **进程内 russh server**（Unix-only） | — |
| 关键缺陷 | — | 锁 2.1 撞上串行读，自研 400 行绕行 | — |

### A.2 可迁移教训清单（概念层，跨项目通用）

1. **每次 open 必须 awaited close**（aeroftp `sftp.rs:1800-1808`）：drop 只排队 `close_nowait` → 句柄上限被打爆（5000 文件实测）。
2. **上传后校验远端 size**（`:1050-1085`）：0 字节 bug 的唯一持久修复。
3. **续传禁用 APPEND**（`:2282-2295`）+ **offset 用真实远端 size 钳制**（`:1139-1148`）：防"服务器在 EOF 写"与"越过短文件损坏"。
4. **上传后保留 mtime**（`:1087-1090`）：否则每次同步都因 `remote_mtime > local_mtime` 误重传。
5. **"连接已死"的字符串清单必须单一来源**（`types.rs:2127-2131`）：aeroftp 曾有 4 份拷贝互相不一致，一份把 broken pipe 当磁盘错误而拒绝重试。russh 家族不做传输层错误类型标注，我们会遇到同样问题。
6. **`exists` 不得把权限/IO 失败当不存在**（`:102-120`）。
7. **嵌入式服务器 attrs 不可信**（`:1169-1173`、`:1492-1500`）：READDIR 属性缺失需 STAT 回退；mode 无文件类型位必须探测，否则会把 symlink-to-dir 走进去（其 GAP-A02）。
8. **符号链接契约**：link-to-dir 同时报 `is_dir` 与 `is_symlink`，walkable 判定拒绝下潜；递归删除从不跟随链接（参照其 `live_sftp_symlink_contract.rs` 断言形态）。
9. **warm 复用要有退休上限**（其 worker 128 文件退休，注释称"照 rclone 池的做法"）。
10. **能力位要与实现配对**（其 `sftp.rs:2172-2207`）：它曾有 `supports_resume()` 门控 CLI 的 `--partial`，而 `resume_upload` 只在 GUI 可达——"在一侧实现能力却在两侧宣告"。

### A.3 研究过程中对前两轮结论的修正（留档，避免重复踩）

| 议题 | 第一轮判断 | 二轮后 | 三轮后（定论） |
|---|---|---|---|
| 流式 Range 读 | "调 API 就有" | 修正为"要自己写桥" | **确认要自己写，但 3.0 的 `AsyncSeek`+流水线读使其成为可控工程** |
| 串行读吞吐 | 未识别 | 未识别 | **2.x 是真缺陷；3.0 `max_concurrent_reads: 16` 已修 → 省掉 400 行绕行** |
| 测试桩 | "白送红利" | "要估工" | **可行（aeroftp 做了两个），但 Windows 上的 HOME 隔离坑必须正面解决** |
| host key | "建议 TOFU" | "termcp 不做" | **aeroftp 有完整可参照设计；我们需非交互形态** |
| 许可证 | 未考虑 | —— | **自用前提下不受限（负责人 2026-09-14 裁定），但仅限借鉴思路，仍不逐字搬运第三方代码** |

## 附录 B：本计划涉及的关键源码位置（供实施期直接跳转）

- 驱动契约：`crates/cloudkit-storage/src/driver.rs:27-83`（`StorageDriver`）
- 能力位：`crates/cloudkit-storage/src/capability.rs:13-49`
- transport 家族：`crates/cloudkit-storage/src/transport/mod.rs:154-211`
- 最贴近的实现参照：`crates/drivers/ck-local/src/driver.rs`、`crates/drivers/ck-local/src/transport_face.rs`
- 接入手册：`docs/standards/driver-onboarding.md`（§1 crate 形态 / §2 九方法义务 / §3 能力位纪律 / §4 配置 schema / §5 工厂装配 / §6 conformance / §7 E2E 凭据 / §8 验收清单 / §9 勿抄清单 / §10 transport-only 驱动类——telegram 先例；SFTP 走 §1–§9 全量公民路线）
- 接入点：`lib.rs:122-137`（`compiled_drivers`）、`:101-112`（`*_DRIVER_REQUIRED`）、`:4900-4977`（`BackendTransport`）、`:5044-5056`（`build_local_transport`）、`:5066-5115`（dispatch）、`:5348-5388`（`build_driver`）
- 配置键：`crates/cloudkit-core/src/config.rs:67-106`（`KNOWN_TOML_KEYS`）、`:203-222`（`Backend`）、`VOLUME_SCOPED_KEYS`
- 零侵入依据：`crates/cloudkit-core/src/vfs.rs:333-339`、`:434-501`、`:508-512`、`:776-780`

## 附录 C：本计划阶段的实测验证（Windows，2026-09-14）

三项探针均在**仓外**（`E:\tmp\sftp-probe`、`E:\tmp\sftp-harness`），仓库 `git status` 保持干净。这不是纸面推断，是运行输出的原件。

### C.1 依赖树与 API 形态可编译（`E:\tmp\sftp-probe`）

`russh = { default-features = false, features = ["ring", "rsa", "async-trait"] }` + `russh-sftp = "3.0"` 完整解析并编译通过：

```
Compiling russh v0.63.3
Compiling russh-sftp v3.0.0
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.60s
```

- 验证了驱动会用的全部调用面：`client::connect` → `authenticate_password` → `channel_open_session` → `request_subsystem("sftp")` → `SftpSession::new` → `metadata` / `read_dir` / `open+seek+read_exact` / `create+write_all` / `fs_info`
- **依赖树用 ring 而非 aws-lc-rs**，`ring 0.17.14` 已在本仓 `Cargo.lock`（经 rustls）→ 零新增高风险构建依赖
- 许可证审计：探针全树 193 包逐条比对 `deny.toml` allow-list，**无新增条目需求**（`Unlicense OR MIT`、`MIT OR Apache-2.0 OR LGPL-2.1-or-later` 等含 `OR` 的表达式均有 MIT 分支且 MIT 在表内；`ring` 的 `Apache-2.0 AND ISC` 两项亦在表内）
- 过程中踩到真实 API 漂移：0.63 把 server-key 回调签名改为 `russh::keys::PublicKeyOrCertificate`（0.62 及以前是 `&ssh_key::PublicKey`）→ 已记入风险表

### C.2 进程内 SFTP 服务端在 Windows 端到端跑通（`E:\tmp\sftp-harness`，**决定性**）

**实测输出（`cargo run --bin step` 原件）：**

```
[step 0 start]
[port] 49290
[step 1 server up]
[step 2 connected]
[auth] success=true
[step 3 authenticated]
[step 4 channel open]
[step 5 subsystem requested]
[step 6 sftp session up]
[readdir] ["probe.bin"]
[step 7 readdir ok]
[step 8 stat ok]
[range] 4096 bytes @ 100000: all correct
[step 9 RANGE READ ok (offset honored, window only)]
[step 10 missing-file error ok]

[RESULT] PASS - Windows in-process SFTP harness: list/stat/range/error all green
```

**这份输出的意义（逐条对应方案里的风险）**：

1. **SF2 的风险被消除**：进程内桩在 **Windows**（`x86_64-pc-windows-gnu`）上端到端可用——**不依赖 Docker、不依赖 testcontainers、不依赖 `$HOME`、不依赖外部 `ssh` 进程**。这就绕开了 aeroftp 同类桩只能跑 Unix 的那个坑（它靠重定向 `HOME` 隔离 known_hosts）。
2. **搭建成本远低于预期**：`russh_sftp::server::Handler` **只有 `unimplemented()` 是必需方法**，其余 20 个（open/close/read/write/readdir/stat/rename/mkdir…）全有默认实现 → **不需要** aeroftp 那种手写 SFTP v3 包循环（它是为了给 STAT 注入延迟才自己写的）。
3. **Range 读被实证**：`seek(100_000)` + `read_exact(4096)` 返回的每一个字节都与其在文件中的预期位置一致（按 `i % 251` 校验），证明**offset 被服务端正确遵守、且窗口外数据未被拉取**——这是 K47 流式解密播放路径的前置条件。
4. **错误映射可测**：缺失文件走 `NoSuchFile` 错误分支，为 §4.3 映射表的桩回放测试提供了现成手段。

**桩侧踩到的唯一语义坑（已写入 SF2 验收要求）**：`readdir` 必须在第二轮返回 `StatusCode::Eof`。客户端 `read_dir` 按协议循环到 EOF 才终止，桩若一直返回条目会静默挂死（本探针首次运行即 20s 超时，定位后修正）。这是 SFTP 协议语义，不是库缺陷，但**每个写桩的人都会先踩一次**。

### C.3 与三份外部研究的交叉核对

- aeroftp 对 russh + `ring` 的选择、对 0.63 安全下限的推理（GHSA-47hw-gvq5-r2gm / GHSA-w3jg-pjxf-73p4）→ **与本仓探针结论一致**
- aeroftp 记录的 2.x 串行读缺陷 → 3.0 `Config::max_concurrent_reads: 16` 已修（`russh-sftp-3.0.0/src/client/mod.rs:32-55`；`file.rs` 的 `ReadState::request` 为流水线实现）→ **我们不复制其 400 行绕行**
- termcp 的 mit 许可与"弃 libssh2 选 russh" → **路线印证**；其 `NoCheckServerKey` 与同步/临时文件架构 → **不采纳**

