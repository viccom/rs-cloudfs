# telegram E2E 任务跟踪单

> 裁决：2026-09-11 选项 B 独立测试 bot（decisions.md 当日条目）｜ 凭据：`E:\GitHub\rs-CyDrive\test\config.toml`（bot_token + chat_id=7854959927，gitignore 内不入库）｜ 已知事实：**本机访问 Telegram 必须走代理** `socks5://127.0.0.1:7897`（Bot HTTP API 已实证；MTProto 同理，transport 配置必带 proxy_url）。
> worktree：`feat/tg-e2e` ｜ 基线：main@3a6c8f1（workspace 911/0/9、winfsp 腿 117/0/1）。
> 批次范围：ck-telegram 驱动对真网（真 bot/真 chat）的端到端验收——登录、上传回读逐字、Range 读、list/stat、覆盖/删除、`/_e2e/` 前缀隔离 + 收尾清理。写操作**只允许** `/_e2e/` 前缀（§7a）；收尾删除并核空。

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| TG0 | E2E harness：凭据加载（test 路径/env）、代理装配、临时 MTProto session、`/_e2e/` 隔离助手 + 失败也执行的清理守卫 | ✅ | 2026-09-11 | f37a198→1c7c425：凭据只读 test 路径/env + 无凭据 SKIP 实测（假路径→SKIP 非 fail）；代理经 ConnectionParams::proxy_url 装配（双连接实证必要）；session 落 temp/tg-e2e-sessions（稳定 session 快路径生效）；`/_e2e/tg-e2e-<ts>-<tag>/` + panic 前必清理 + checker 独立连接验空 |
| TG1 | 登录 + 上传→list/stat→回读逐字（多块文件） | ✅ | 2026-09-11 | tg1 PASS 24.1s：login @cydrive_test_bot → 4.5MB/5 parts → stat 全对 → 下载 4718592 字节逐字一致 → CLEANUP 5 条 0 失败。捕获力：上传侧 1 字节变异 → 红于第 4660 字节（精确命中）→ 还原绿 |
| TG2 | Range 读（按驱动能力声明）+ 覆盖写语义 + 删除 + vfs 级加密往返（可选拉伸） | ✅ | 2026-09-11 | tg2 PASS 15.3s：range_read=true 实证——1MiB 窗跨 3 parts/256KiB 窗骑部件边界逐字节精确、尾窗 EOF 钳制；覆盖写=transport 层 append-only（同 rel_path 新消息集，旧集按 id 仍可取——与 K4/remote_delete=false 的 VFS 分层一致，测试按观测行为钉死）；删除核空。tg_vfs PASS 10.1s：vfs+AEAD_V2+telegram 全栈，上传 encrypted=true chunks=2，hydrate 逐字一致 + 跨加密块边界窗口解密逐字一致 |
| TG3 | 旧 `#[ignore]` 真机测试清尾（能跑则跑并记录）+ 文档收口 + merge/push | ✅ | 2026-09-11 | telegram 腿无遗留（本批 3 个即全集）；其余 #[ignore] 分类记录（baidu 真机×3 需 env 凭据、winfsp Q:、platform×3、keyring、perf opt-in、sync https/SIGTERM——均随各自真机窗口）。文档收口（decisions K54/AGENTS）+ merge/push 本批完成 |

## 批次日志

- 2026-09-11：立项。凭据与代理自检当日完成（getMe/getUpdates/sendMessage 全通，message_id 3）。待开工。
- 2026-09-11：TG0–TG2 完成（1c7c425）。过程事件：首轮 tg1 撞 RateLimited{retry_after:1576s}——前任 fresh-session 反复重试在服务端累积 auth 限流；按纪律等待 26 分钟窗口后首跑三连绿未复发。经验已入测试 doc 注释：勿 fresh-session 重试，稳定 session 是卫生。前任超时成因即反复重试+长网络等待；续作代理核验草稿零修正（712+331 行一次编译通过）。门禁：workspace 911/0/12 ignored（基线 9+真网 3）、winfsp 117/0/1、clippy×2/fmt/check_layers/scan_secrets 全绿。
