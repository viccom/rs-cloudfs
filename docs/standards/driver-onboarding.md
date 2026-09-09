# 驱动接入手册（Driver Onboarding）

> 状态：v1.0（2026-09-08，Phase 2 前置任务）｜ 强制级别：新驱动 PR 的验收依据
> 来源：foundation §5 Phase 2 前置任务（红队 H4：无手册则「端到端硬验收」无判定依据）；PCFS 四驱动实证 + 其坑清单（decisions 2026-09-08 PCFS 研究）；spike 报告百度参数
> 上游标准：[architecture.md](architecture.md)（红线/分层）、[interfaces.md](interfaces.md)（StorageDriver 契约/conformance 八断言）、[code-style.md](code-style.md)（门禁/TDD）

**一句话**：新后端接入 = 实现一个 L2 驱动 crate + 过 conformance 套件 + 按本手册装配，上层全部能力（挂载/仪表盘/同步/CLI）自动可用。

## 1. 驱动 crate 形态

- 路径 `crates/drivers/ck-<name>/`，lib 名 `ck_<name>`；**只允许依赖 cloudkit-storage（L2）与外部 crate**，禁止依赖 cloudkit-core 及任何 L3+ crate（R1；`scripts/check_layers` 机械拦截——组合根 cloudkit-cli 是唯一豁免）。
- 模块建议（参照 ck-telegram）：`transport.rs`（协议适配）/`client.rs` 或 `api.rs`（HTTP/协议面）/`oauth.rs`（鉴权状态机，如适用）/`lib.rs`（导出 + 工厂函数）。
- 二进制不许出现在驱动 crate（bin 只在 cloudkit-cli / cloudkit-sync-server）。

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
4. 装配期打一行能力声明横幅（info，九位全列——R-5 先例）。

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
