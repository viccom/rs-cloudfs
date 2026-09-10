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
**Phase 2.5 完成（2026-09-09，0.10.0）**：多卷启用（Volume Registry，方案一裁决）——MV0 配置/发现（volumes_dir + 每卷一文件，K19）→ MV1 Registry 装配（K21 卷主目录隔离/K22 失败可见/K25 单 stop gate）→ MV2 WebDAV 单端口 `/vol/<name>` 前缀路由 + 逐卷挂载（K20 真机探针过、K27）→ MV3 仪表盘多卷（/api/volumes + 卷 tabs + K23 卷参）→ MV4 CLI 运维面（volumes/doctor/status 逐卷 + setup --multi 骨架）+ 文档联动，K19–K28 入档 decisions。**三卷真机 E2E 全过（单进程 V:local加密+Y:tg+Z:baidu、单端口 /vol/<name>、单仪表盘 tabs/汇总、stop 全停）**，feat/phase2-5 收口 merge main。前置 Phase 2（0.9.0）完成，telegram E2E 生产 chat 污染已获负责人明示接受（§7a 例外）。过渡测试包：E:\Rs_Codes\cydrive-0.8.0-testkit（telegram+加密 U:/V:，仓外不入库）。

## 常用命令（仓库根）
```
cargo test --workspace --no-fail-fast            # 807 测试（0.10.0+Phase 2.5 多卷批；ignored 9 = 真机/平台类）
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo build -p cloudkit-cli --no-default-features --features local,baidu   # 驱动裁剪构建（K30 三 feature）；缺驱动构建运行期报可行动错误（K31 rebuild 指引），cydrive --version 显示驱动清单（K32）
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
- 实现期陷阱查 `docs/rust-rewrite-design.md`「深度调研补遗」节（axum 2MB body 上限/grammers FloodWait 藏点/dav-server Bytes-Seek 模型/WebClient 4GB-1/挂载 Basic 认证）
- PCFS 反面教材勿抄：错误类型跨层泄漏、硬编码密钥、纯 CTR 无认证、注释掉的调试日志

## 待人工清单
1. 基线设计 §9-2/9-3/9-4 三项建议待负责人确认（旧仓冻结时点 / bot 分 crate 时点 / R-E 批序）
2. **telegram E2E 腿二选一**：telegram 实测配置在 `D:\Tools\rs-CyDrive`（生产实例，bot/chat 即生产命名空间；E:\GitHub\rs-CyDrive\test\ 不存在）——负责人明示接受生产 chat 污染跑 E2E，或提供独立测试 bot/chat（§7a 隔离裁决）；baidu appkey 即负责人本人凭据（PCFS client.go:69-70，decisions 2026-09-09 澄清），meta 31300 若要恢复直查须在百度开放平台为本 key 开 meta 权限
3. ~~新仓远端 origin 待建~~ **已建**：origin = github.com/viccom/rs-cloudfs（**私有**，2026-09-09 建，main + feat/phase0-1 + feat/phase2 已推；历史含 rs-CyDrive 时代旧提交，公开化前建议做一次全历史秘密审查）
4. 自 rs-CyDrive 继承的挂账——**2026-09-08 修复批后仅剩验证类**：P3 hydrate 快照回写竞态已修（47c4fc2，目标列写 set_cached_flag）；Low×5 已清（sync_url host 校验 f8040aa/模拟器排序 e93433a/SyncClient trait 文档 09fff2c/--help 实证漂移 bc34d43/64MB 并发闸 08fee1d；凭据门槛核实本已统一于 resolve_sync_secret）；gen_compat_fixtures.py 已修（5a3a319，重生成需同步改 database.rs 钉死的 created_at 断言）；deny advisories 已过（`advisories ok`，经 7897 代理拉库——github.com 直连不通的既有限制自此有绕行方案）。**剩余：#[ignore] 真机测试 ×3、litmus 套件（均验证类，随真机窗口跑）**
