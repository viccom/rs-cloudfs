# 驱动接入手册（Driver Onboarding）

> 状态：v1.2（2026-09-23 增补 §11 未来驱动共性义务；v1.1 = 2026-09-14 增补 §10 transport-only 驱动类；v1.0 = 2026-09-08 Phase 2 前置任务）｜ 强制级别：新驱动 PR 的验收依据
> 来源：foundation §5 Phase 2 前置任务（红队 H4：无手册则「端到端硬验收」无判定依据）；PCFS 四驱动实证 + 其坑清单（decisions 2026-09-08 PCFS 研究）；spike 报告百度参数
> 上游标准：[architecture.md](architecture.md)（红线/分层）、[interfaces.md](interfaces.md)（StorageDriver 契约/conformance 八断言）、[code-style.md](code-style.md)（门禁/TDD）

**一句话**：新后端接入 = 实现一个 L2 驱动 crate + 过 conformance 套件 + 按本手册装配，上层全部能力（挂载/仪表盘/同步/CLI）自动可用。**例外**：后端没有「按路径枚举」面的，走 §10 transport-only 类（telegram 先例）——那是设计事实不是欠债。

## 1. 驱动 crate 形态

- 路径 `crates/drivers/ck-<name>/`，lib 名 `ck_<name>`；**只允许依赖 cloudkit-storage（L2）与外部 crate**，禁止依赖 cloudkit-core 及任何 L3+ crate（R1；`scripts/check_layers` 机械拦截——组合根 cloudkit-cli 是唯一豁免）。
- 模块建议（参照 ck-telegram）：`transport.rs`（协议适配）/`client.rs` 或 `api.rs`（HTTP/协议面）/`oauth.rs`（鉴权状态机，如适用）/`lib.rs`（导出 + 工厂函数）。
- 二进制不许出现在驱动 crate（bin 只在 cloudkit-cli / cloudkit-sync-server）。
- **组合根 feature 门控三件套（K30，telegram/baidu/local 先例）**：新驱动接入时在 `crates/cloudkit-cli/Cargo.toml` 声明 optional 依赖（`ck-<name> = { path = ..., optional = true }`）+ 同名 feature（`<name> = ["dep:ck-<name>"]`）+ `default` 追加 `<name>`；并在 `crates/cloudkit-cli/src/lib.rs` 补 K31 文案常量 `<NAME>_DRIVER_REQUIRED`（三段式：缺驱动声明 + rebuild 命令 + backend 改法）与 K32 `compiled_drivers()` 清单臂（顺序固定追加 + cfg 门控断言测试）。裁剪组合随 CI feature 矩阵腿验收（ci.yml `features` job：clippy×单驱动子集 + workspace `--no-default-features` test）。

**dev-dependency 例外（Phase 8，K83）**：驱动 crate 的 `[dev-dependencies]` 允许 `cloudkit-core`（L3）用于集成测试（read-through 共性冒烟先例：ck-local `readthrough_smoke.rs`、ck-sftp `live_readthrough.rs`）——`check_layers` 只查生产依赖边；生产依赖图的禁令不变（驱动运行时仍只依赖 cloudkit-storage）。

## 2. StorageDriver 实现义务（D1/§2 契约摘要）

1. **九方法**：volume/capabilities/list/stat/mkdir/delete/rename/reader(range)/writer(hint)/quota——`#[async_trait]` dyn 兼容，语义/错误/并发/生命周期四维文档齐（interfaces §1）。
2. **UploadStager = commit-on-close**：写入只是 staging，close 才产生远端可见对象；staging 可丢弃不留垃圾。
3. **Range 半开区间 + 越界钳制**；不支持 Range → 不声明 `RANGE_READ`。
4. **list 分页统一走 Page**，驱动不得自造分页参数。
5. **分块策略归驱动**（telegram 自理 1900MB、baidu 自理 4MB superfile）；加密块与存储块正交。
6. **错误映射表（R2）**：每个后端错误码 → StorageError 变体，**映射表以 mock 回放测试钉死**；未知错误 → Io/Unavailable 且保留原始码与消息。已有先例：FloodWait→RateLimited{retry_after}；百度 110→驱动内刷新重放一次否则 Unauthorized{true}、111/-6→Unauthorized{false}。
7. **鉴权生命周期**：token 状态机在驱动内（自动刷新 + 经 CredentialStore 回调持久化，env > config > keyring 链）；刷新后立即持久化（百度 refresh_token 一次一换，spike 实证）。

## 3. 能力位声明纪律（R4）

- Capabilities 只声明**经 conformance 套件 + 真机验证**的能力；local 类驱动豁免真机项（本地 FS 无远端语义）。
- 未接线的能力位宁缺勿滥，逐位注明依据（ck-telegram 先例：四位声明+注码）。
- 消费方按 `capabilities()` + `as_inbound()/as_chat()` 探测降级，**缺能力绝不 panic**（interfaces §1）。

## 4. 驱动配置 schema 模式

- config 顶层 `backend = "telegram" | "baidu" | "local" | ...`（缺省 telegram 完全兼容，A4/D5）；驱动专属键命名 `<driver>_<param>`。
- **新键三处同步**（interfaces §4）：KNOWN_TOML_KEYS + legacy json 拒收清单 + validate()；非法值给可行动文案。
- **凭据值不入库不入日志**（R3；scripts/scan_secrets CI 闸）；刷新产物走 keyring/OS 临时目录。
- **多卷形态（Phase 2.5，已落地）**：「每实例一文件」与单一 `backend` 键形态**叠加而非替换**——进程级 config.toml 增 `volumes_dir` 键，每卷一份 `volumes/<name>.toml`（卷作用键子集，含进程级键即拒；卷模式与进程级卷作用键混用即拒，K19 互斥规则）；单卷 config（无 volumes_dir）字节兼容照旧。驱动配置解析器保持「配置 map → 驱动参数结构体」纯函数组织（卷文件与 config.toml 走同一 load 严格校验面，未知键拒收同文案）。

## 5. 工厂注册与装配点

1. 驱动 crate 导出 `pub fn factory(cfg: &DriverParams) -> Result<Arc<dyn CloudTransport>, ...>` 形态的构造函数（ck-telegram 先例）。
2. 组合根 `cloudkit-cli`：`backend` 键 dispatch 到工厂 → 装配 Vfs/队列/WebDAV（同层依赖豁免点，唯一允许 import 驱动符号的 crate）。
3. doctor/setup 相应分支（setup 引导鉴权流程、doctor 增驱动检查项——B3 范围）。
4. 装配期打一行能力声明横幅（info，十位全列——R-5 先例）。

## 6. conformance 验收（D9，不得缩减）

- **离线套件**：`cloudkit_storage::conformance_suite!(<mock 或内存后端>)` 八条全绿（interfaces §6：上传往返 0/1/跨块/多块、Range 语义、stat/list 分页稳定有序、mkdir/delete 幂等声明恒定、错误映射回放、rename 文件+目录、RESUME 差集（声明时）、并发读）。
- **真机套件**：同断言集 `#[ignore] ignored_*` 版本 + 命名 `#[ignore]` 前置注明（env/凭据）；local 类豁免。
- **PR 通过证据 = 离线套件绿 + 真机套件输出**（真机跑法见 §7）。

## 7. E2E 拓扑与凭据注入

- 凭据源：`E:\GitHub\rs-CyDrive\test\`（telegram 实例配置 + baidu token；负责人后期轮换）；**独立测试 chat 隔离裁决**（foundation §7a）——与生产同 bot/chat 时默认不接受污染，由负责人提供测试对或明示。
- 全部写操作限定 `/_e2e_<driver>/` 前缀；收尾必须清理（删所建行并等 sync 墓碑收敛）；日志/报告脱敏后入库。
- 百度测试根 = `/apps` 下专用子目录（负责人 2026-09-07 裁决；**绝不触碰 /apps/privatefs**）；token 经 spike 工具链刷新维护。
- 凭据失效（errno 111/401 类）→ 人话指引停机，不得死循环重试（§7a）。

## 8. 新驱动 PR 验收清单

- [ ] crate 形态/依赖方向合规（check_layers 绿）
- [ ] StorageDriver 九方法 + 四维文档 + 错误映射表（mock 钉死）
- [ ] Capabilities 逐位依据注码（R4）
- [ ] 配置键三处同步 + validate 文案可行动
- [ ] conformance 离线八条绿（证据贴输出）
- [ ] 真机套件输出（或 local 豁免声明）
- [ ] workspace 三步门禁全绿 + WSL（涉 cfg 面时）
- [ ] 装配点/doctor/setup 分支 + 能力横幅
- [ ] 文档联动：README 状态表/AGENTS 计数/本手册如需修订

## 9. PCFS 移植注意（勿抄清单，详见 decisions 2026-09-08）

- List 失败**禁止**静默回退陈旧缓存（必须显式 Unavailable）；
- 驱动删除失败**禁止**吞错仍清本地索引（云端孤儿）；
- 禁止 ETag/短前缀构造 EntryId（用稳定后端句柄）；禁止以 Name 前缀嗅探驱动类型（用 Capabilities/类型）；
- 驱动内部路径前缀（如 baidu 根）**不得泄漏到聚合层**（R1）。

## 10. transport-only 驱动类（telegram 先例；2026-09-14 增补）

不是所有后端都能诚实实现 §2 的九方法。**新驱动接入前先回答一个问题：后端有没有「按路径枚举/寻址」的面？**——即能否**只给定路径**（不借助任何本地索引）实现 `list(dir)` 与 `stat(path)`。

- **有** → 走 §1–§9 的全量公民路线（local / baidu / 115 / 123 / sftp…）；
- **没有** → 本节的 transport-only 类。

这个二分是 foundation **D4（影子索引 vs 权威索引）**的驱动面投影：权威索引后端（list 即真相）对应 `StorageDriver` 宽面；影子索引后端（索引只存在于本地 db + sync 协议）对应 `CloudTransport` 窄面。**trait 家族长成两班制，正是为了让「没有文件系统面孔的后端」可以诚实存在**（R4「能力位必须诚实」的 trait 面版本）。

### 判定标准与先例证据

- **判定**：远端存储是否可在「只给定路径」时枚举/定位对象。文件系统型后端天然满足；**消息形后端不满足**——路径↔对象的映射只能靠本地索引维持。
- **telegram 是先例**：远端是聊天消息，caption 是唯一的远端路径链接（`ck-telegram/src/caption.rs` 模块头自述）。按路径枚举需要遍历聊天历史——**2026-09-04 真机 spike 证伪**：bot 会话调 `messages.getHistory` / `messages.search` 均返回 400 `BOT_METHOD_INVALID`（Telegram 平台级限制，与文档一致；见 decisions.md 2026-09-04 条目与 `examples/history_spike.rs`）。rescan 出局，`list`/`stat` 在后端层面**不可实现**。

### transport-only 驱动的义务（替代 §2 的九方法义务）

1. **实现 `CloudTransport` 核心面**：connect / upload（±upload_stream）/ open / open_range / delete_remote / capabilities；可选 `InboundCap` / `ChatCap`（telegram 是唯一先例，两者都实现——这也是宽面驱动没有的能力）。
2. **能力位照 R4 诚实**：telegram 声明 `range_read` / `multipart` / `inbound` / `chat` 四位（`ck-telegram/src/transport.rs` capabilities），不声明做不到的。
3. **错误映射表照 R2**：`map_invocation_error` 先例（FloodWait→`RateLimited{retry_after}`）。
4. **索引职责归 L3**：路径↔消息映射住在元数据库 + `Vfs`（`remote_handle_for` 从 chunks 行组装句柄）；**索引复制走 sync**——影子索引后端的 sync 是**必需基建**而非加速器（D4）。
5. **编译开关照 §1 全套无差别**（K30 三件套：feature 门控 / 缺驱动文案 / `compiled_drivers()` 臂 / 无驱动孪生函数）——**两班制只在驱动契约面，不在裁剪面**。

### 禁止清单（为什么不能「补齐成全量公民」）

- **不实现 `StorageDriver`**：九方法中 `list`/`stat`/`mkdir`/`rename` 无后端语义；唯一绕路 = 驱动内自建影子索引，即复制 L3 元数据库职责 + 双索引一致性负担 + R6 兼容契约红线风险，纯负收益（2026-09-14 会话逐方法分析否决）。
- **不接 conformance 套件**：断言③（stat/list 分页）④（mkdir 幂等）⑥（rename 文件+目录）无法诚实通过，而套件「可扩不可减」（§6）。
- **不进统一 dispatch**：`build_backend_transport_with` 的 telegram 臂**带着 feature 也 bail 是刻意的**——连接由 run 流程自有路径持有；且 boot 臂与运行时 ADD 臂的失败语义差异是**功能性的**（boot = Ctrl+C 竞速 + 人话指引 + `exit(1)`；ADD = 可应答 Err，控制命令绝不杀实例），**勿为视觉对称合并两臂**。
- **无 rebuild**：`TELEGRAM_REBUILD_REFUSAL` 文案先例（影子索引无后端可走，"this db IS the authoritative index"）；新机器 bootstrap 走 sync 或 import-meta，不走后端枚举。

### transport-only 类验收清单（替代 §8）

- [ ] crate 形态/依赖方向合规（check_layers 绿）
- [ ] CloudTransport 面 + 错误映射表（mock/桩回放钉死）
- [ ] Capabilities 逐位依据注码（R4）
- [ ] 配置键三处同步 + validate 文案可行动（§4 照旧）
- [ ] transport 面契约测试（`ck-telegram/tests/` 先例：contract / adapter / stream_reader）
- [ ] 连接路径有 deadline 界 + 失败人话指引（`connect_with_deadline` 先例——网络阻塞的静默挂起是真机事故教训）
- [ ] 文档联动：README 状态表 / AGENTS 计数 / 本节如需修订

## 11. 未来驱动的共性义务（read-through，Phase 8；2026-09-23 增补）

新驱动（s3 等）**零行 read-through 代码**即自动获得按需逐层索引（读路径 miss/TTL 过期自动回源物化）——机制五件（探针/门/物化/reconcile/两入口）全在 L2/L3 共性面，驱动侧只是三个既有义务的自然延伸：

1. **实现 `StorageDriver` 九方法**（§2）——conformance 八断言已要求；list depth-1/字典序稳定/NotFound vs 空表即 read-through 的回源契约，无额外语义；
2. **transport_face 持 `Arc<驱动>` 并给 `as_driver` 一行探针**：`fn as_driver(&self) -> Option<&dyn StorageDriver> { Some(self.driver.as_ref()) }`（六宽面驱动同款先例；缺省实现 = None，探针是窄面到宽面的唯一通道）；
3. **Capabilities 诚实声明 `authoritative_index`**（R4）——该能力位自 Phase 8 起是 read-through 回源门的真实判据：声明了却撒谎 = 按需索引不生效，诚实成为可执行约束。

机制锚点（勿在驱动内复刻）：探针 `CloudTransport::as_driver`（cloudkit-storage）；门 + reconcile + 两入口 = `cloudkit-core::readthrough`（`read_dir_fresh`/`stat_fresh`）；物化 = `cloudkit-core::materialize::materialize_entry`（唯一 Entry→行映射，rebuild 与 read-through 共用）。

**§10 transport-only 驱动天然豁免**：窄面无宽面可探（`as_driver` 恒 None），read-through 门退化为纯 db 读——telegram 卷读行为不变（设计事实照旧，不是欠债）。
