# rs-cloudfs — Agent 工作须知

## 项目信息
- 项目：rs-cloudfs = rs-CyDrive × PrivateCloudFS 融合体——多云存储平台（统一存储抽象之上的 WebDAV 挂载/仪表盘/同步/CLI；后端：telegram / baidu / local，未来 115/123/s3）
- 技术栈：Rust（edition 2021）/ tokio / axum / dav-server / rusqlite(bundled) / grammers(telegram) / hyper-rustls
- **血统**：fork 自 rs-CyDrive（全 git 历史；remote `upstream-cydrive` 只读参照，禁止 push）；PrivateCloudFS（`E:\Go_codes\PrivateCloudFS`，Go）是设计参照系与踩坑情报源（情报附录在 multicloud 计划）
- **北极星**：「一个稳定好用的程序」——重组已验证资产，不重写
- 行为基线与兼容契约：Python 版 CyDrive 契约（DB schema/分块命名/caption/端口）经 rs-CyDrive 继承，telegram 驱动延续遵守（红线 R6）

## 必读（开工前，按序）
1. `docs/plans/2026-09-07-cloudfusion-foundation.md` —— 融合基线设计 v1.1（阶段计划/裁决状态/E2E 凭据策略）
2. `docs/standards/architecture.md` —— 六层架构 + 红线 R1–R7（**L2 以上禁 import 驱动符号**等）
3. `docs/standards/code-style.md` / `interfaces.md` / `logging.md` / `documentation.md` / `driver-onboarding.md` —— 门禁与规范（driver-onboarding = 新驱动 PR 验收依据）
4. `docs/plans/2026-09-06-multicloud.md` —— 百度情报附录 A（端点/参数/errno/dlink/Range 实证）
5. `docs/decisions.md` —— 历史裁决（自 rs-CyDrive 继承，继续追加）
6. `docs/tracking/phase0-1.md` —— 当前任务跟踪单（**开工先读、每批收口更新**）

## 当前阶段
**Phase 3.6 完成（2026-09-12）**：存储卷运行态动态加载/卸载 + 卷级 enabled 键（RV0–RV3 四批，K48–K51 入档 decisions）——RV0 卷级 `enabled` 键（缺省 true=语义自然缺省，发现期跳过+`info!` 声明，禁用卷不占盘符不装配）；RV1 注册表动态化（三面 `RegistryHandle`：cli 主表=真源、webdav/web=投影，变更统一经 cli mutator 汇流；dav per-request 读锁查表分发，端口不变无重绑，锁不跨 await）；RV2 控制通道 `ADD/REMOVE/LIST`（K50 安全序=排空上传→盘符释放→faces 先 workers 后提交，任一步超时/失败中止且卷保持注册——绝不半卸；命令串行处理；REMOVE 只动运行态不碰卷文件，K49；`cydrive status` 多卷面带 LIST 转发）；RV3 真机矩阵六项全过（运行态 ADD baidu 卷 Q: 三面即时可见可读写、REMOVE 排空后三面消失数据跨往返完好、enabled=false 重启跳过、坏凭据 ADD 不伤兄弟卷；执行期发现：winfsp 卸载不受用户态句柄阻挡——占用中止路径在 webdav 腿+注入探针单测；执行期修复：logging::init 上移到配置发现前，发现期 info! 不再被吞）。前置 Phase 3（winfsp 挂载，K38–K46+K52）与 Phase 2.5（0.10.0 多卷/仪表盘）完成；过渡测试包：E:\Rs_Codes\cydrive-0.8.0-testkit（telegram+加密 U:/V:，仓外不入库）。
**审查修复批完成（2026-09-12，K55，fix/phase36-review）**：合入后四分域深度审查 3 High + 5 Medium，批准修复 H1-H3+M1——H2 队列幽灵 outstanding（stale empty-PUT skip 补 degraded 终态计数 + `QueueStats::outstanding()` 单点）；H1 停机交错（`take()` 单锁化竞态 7/7 复现红 + 命令观测停机 gate「gate 后除放回外不变更 live 表」+ `InFlightCommands` idle 屏障 + `request_stop()`）；H3 ADD 改「先装配后公示」（三面 insert 后移到挂载成功后，挂载中 404，`rollback_add` 吸收为 `tear_down_unpublished`，挂载缝 `RuntimeVolumeCommands.mount`）；M1 控制通道鲁棒性（handler panic catch_unwind 通道存活 / 客户端交换 120s 预算 / net use spawn_blocking+30s）。挂账（未批准）：M2-M5 与 Low 见 docs/tracking/phase36-review-fixes.md。

## 常用命令（仓库根）
```
cargo test --workspace --no-fail-fast            # 944 测试（Phase 3.6 + 审查修复批 K55：H1-H3/M1；ignored 12 = 真机/平台/真网类）
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo build -p cloudkit-cli --no-default-features --features local,baidu   # 驱动裁剪构建（K30 三 feature）；缺驱动构建运行期报可行动错误（K31 rebuild 指引），cydrive --version 显示驱动清单（K32）
LIBCLANG_PATH=D:/Python312/Lib/site-packages/clang/native cargo test -p cloudkit-winfsp --features winfsp   # winfsp 腿（117 测试，ignored 1 = 真机挂载；需 libclang+MSVC，见已知陷阱）
scripts/check_layers                             # R1 层依赖门禁（CI 同款；动 Cargo.toml 依赖后必跑）
scripts/scan_secrets                             # R3 秘密扫描门禁（CI 同款；本地模式=全树扫描）
```

## 硬性规则（摘要，全文见 standards/）
- 七条架构红线（architecture.md §2）——违反即返工
- 凭据红线：`test/` 全目录 gitignore；凭据只从 `E:\GitHub\rs-CyDrive\test\` 或 env 读；**任何凭据值不入代码/文档/日志/提交**（负责人后期轮换授权）
- 质量：TDD 红→绿留证、断言零漂移、workspace 级门禁、真机测试 `#[ignore]`
- **大任务纪律（负责人 2026-09-07 指令）**：代码量较大的任务必须以 TDD 思想为指导；**每个计划必须配套任务单与跟踪记录表**（`docs/tracking/<phase>.md`——任务分解×状态×完成情况×证据，每批收口更新并随 commit 提交）
- 提交：conventional commits、不加署名尾注、批次 worktree 隔离
- 文档：行为变更同批更新 README/AGENTS 计数/相关 docs；裁决入 decisions.md

## 已知陷阱（自 rs-CyDrive 继承 + 融合新增）
- Windows Git Bash：`ls`/`tree`/`du`/`ps` 别名禁用（用 `fd`/`rg`）；wsl.exe 复杂命令须 `.sh` 脚本路线（引号吞噬）
- 探索子代理拿结论，主会话不整读大文件（hub-and-spoke）
- 百度 API：强制 IPv4 dial、errno 110/111/-6 三档（详见 multicloud 附录 A）；**当前第三方 appkey 下 meta 端点全废（31300/31023）——stat/Entry/delete 走 list + fs_id 句柄缓存 + 递归扫描**（decisions 2026-09-08 第四轮）；**目录 create 撞已存在 = errno=0 + 空副本重命名（非 -8）——mkdir/ensure_parents 必须 list 预检**；**precreate 会话锁定全量 block_list（create 不一致重申 → 31363）——流式上传只能「到齐即传」**；**uinfo 用户键 = uk 非 uid**
- **百度下载三约束（spike §5 矩阵）**：有界 Range ≤4MiB + netdisk UA + 禁全量 GET（违反任一 → 403/31326）；dlink TTL ≥96min 实测、缓存 60min + 两段 fallback（追 token→重取）；**rtype=3 覆盖语义已真机复核**（同路径重传覆盖、无 _2026 副本）
- **Git Bash 探针教训**：curl 形参中 `/apps/...` 被 MSYS 路径改写污染（→ C:/Program Files/Git/apps/...，百度报 -7 假象）——真网探针必须 `MSYS_NO_PATHCONV=1`
- **Explorer 上传链路**：空 PUT→LOCK→PUT→PROPPATCH——**PROPPATCH 必须全成功（207）而非 405**，否则 MiniRedir 整单回滚「看似失败实则已传」（rs-CyDrive 2026-09-03 真机首验最贵教训，百度 E2E 直接承重）
- **Windows 运行中的 release exe 锁文件**：替换构建报 `os error 5`（拒绝访问）——**先 `cydrive stop`/停进程再 rebuild release**（MV3 冒烟实测，MV2 真机探针同源）
- **多卷模式 `CYDRIVE_*` env 覆盖被忽略**（K28）：全局 env 覆盖会跨卷串味，卷模式 discover 直接跳过并在 tracing 声明；凭据 env>file>keyring 解析链在驱动 resolve 时不变——排查「env 不生效」先看是否卷模式
- **winfsp 腿构建需 libclang + MSVC**：winfsp-sys 的 bindgen 依赖 libclang（本机 pip 包装于 `D:/Python312/Lib/site-packages/clang/native`，构建时 `LIBCLANG_PATH` 指它）；winfsp-sys 对 gnu 工具链 panic——只能 MSVC 构建
- **FSD 大小写形态（K45 执行期实证）**：winfsp 回调里 rename 源名与 cleanup 删除名会以**大写**送达（FSD 大小写不敏感解析的归一形态）——适配层 resolve_row 精确命中 + 父目录扫描回退（歧义保持 miss），6b25ede 修复
- **共享 CARGO_TARGET_DIR 跨 worktree 产物污染（RV3 实证）**：主仓与 worktree 的包名+版本相同（0.10.0），共享 target 下指纹按 name+version 键合——交叉构建会把 A 树编译的 rmeta 喂给 B 树的源码（症状：对 A 树才有的类型报 E0308）。**纪律：worktree 构建用独立 target 目录；merge 回主仓后重建产物前先 `cargo clean` 共享 target**
- 实现期陷阱查 `docs/rust-rewrite-design.md`「深度调研补遗」节（axum 2MB body 上限/grammers FloodWait 藏点/dav-server Bytes-Seek 模型/WebClient 4GB-1/挂载 Basic 认证）
- PCFS 反面教材勿抄：错误类型跨层泄漏、硬编码密钥、纯 CTR 无认证、注释掉的调试日志

## 待人工清单
1. ~~基线设计 §9-2/9-3/9-4 三项建议待负责人确认~~ **已裁决（2026-09-11，decisions.md 当日条目）**：§9-2 rs-CyDrive 正式冻结（~~生产切换时机未裁~~ **负责人收回自行安排（2026-09-11）——仓库任务清单销账**；切换所需的 telegram 真机验收前提已由 K54 备齐）；§9-3 确认 v1 不拆 crate；§9-4 R→E 已自然落地销账
2. ~~卷级 `enabled` 启用/禁用键~~ **已落地（Phase 3.6 / RV0，2026-09-12）**：volume-scoped 布尔键（缺省 true），发现期跳过 + `info!` 声明，禁用卷不占盘符不进 `/vol/<名>`；同批落地运行态动态加载/卸载（控制通道 `ADD/REMOVE/LIST`，K48–K51——负责人 2026-09-10 的运行态装卸可行性问询一并销账）。
2. **telegram E2E 腿——已定向（2026-09-11 裁决选项 B：独立测试 bot/chat）**：**凭据已就位（2026-09-11）**：测试 bot @cydrive_test_bot，配置落 `E:\GitHub\rs-CyDrive\test\config.toml`（bot_token + chat_id，gitignore 内），Bot HTTP API 连通性自检通过（getMe/getUpdates/sendMessage，经代理）。**已知事实：本机访问 Telegram 必须走代理**（`proxy_url = "socks5://127.0.0.1:7897"`，MTProto 同理）——E2E 批的卷/transport 配置必带。**E2E 批已落地（2026-09-11，feat/tg-e2e → main，decisions K54）**：驱动级 2 + vfs 加密全栈 1，真网三连绿（上传/回读逐字/Range 跨部件/覆盖 append-only 语义钉死/清理核空）；运行经验：稳定 session 勿 fresh-session 重试（RateLimited 1576s 实录）。现状参照：生产配置在 `D:\Tools\rs-CyDrive`；baidu appkey 即负责人本人凭据（PCFS client.go:69-70，decisions 2026-09-09 澄清），meta 31300 若要恢复直查须在百度开放平台为本 key 开 meta 权限
3. ~~新仓远端 origin 待建~~ **已建**：origin = github.com/viccom/rs-cloudfs（**私有**，2026-09-09 建，main + feat/phase0-1 + feat/phase2 已推）。~~公开化前建议做一次全历史秘密审查~~ **已做（2026-09-11，gitleaks 8.30.1 全历史 388 提交）**：8 命中全部为公开 Cynet Android `api_hash` 常量（与 config.rs 默认值同源），**零真实凭据**——公开化前的历史审查此项销账。
4. 自 rs-CyDrive 继承的挂账——**2026-09-08 修复批后仅剩验证类**：P3 hydrate 快照回写竞态已修（47c4fc2，目标列写 set_cached_flag）；Low×5 已清（sync_url host 校验 f8040aa/模拟器排序 e93433a/SyncClient trait 文档 09fff2c/--help 实证漂移 bc34d43/64MB 并发闸 08fee1d；凭据门槛核实本已统一于 resolve_sync_secret）；gen_compat_fixtures.py 已修（5a3a319，重生成需同步改 database.rs 钉死的 created_at 断言）；deny advisories 已过（`advisories ok`，经 7897 代理拉库——github.com 直连不通的既有限制自此有绕行方案）。**剩余：#[ignore] 真机测试 ×3、litmus 套件（均验证类，随真机窗口跑）**
5. **工程债清单（权威记录 = docs/tracking/review-fixes.md 挂账节 + docs/decisions.md K53）**：原审查 H3（rename 进 staged 路径 chimera，探针留证）、header RTT+PBKDF2 LRU、fs.rs 拆分、窗口数学 AeadV2Window 下沉、delete_pending 半死代码维持现状（生产写/测试读，激活需语义设计）。2026-09-11 已清一批（K53）：VOLUME_SERIAL 按卷名派生、宽限表 size+mtime 双见证（RB1 窄缝闭环）、unix_to_filetime 整数域、M2 反解歧义注释留证、LetterInUse 文案达标。
