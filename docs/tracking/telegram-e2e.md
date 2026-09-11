# telegram E2E 任务跟踪单

> 裁决：2026-09-11 选项 B 独立测试 bot（decisions.md 当日条目）｜ 凭据：`E:\GitHub\rs-CyDrive\test\config.toml`（bot_token + chat_id=7854959927，gitignore 内不入库）｜ 已知事实：**本机访问 Telegram 必须走代理** `socks5://127.0.0.1:7897`（Bot HTTP API 已实证；MTProto 同理，transport 配置必带 proxy_url）。
> worktree：`feat/tg-e2e` ｜ 基线：main@3a6c8f1（workspace 911/0/9、winfsp 腿 117/0/1）。
> 批次范围：ck-telegram 驱动对真网（真 bot/真 chat）的端到端验收——登录、上传回读逐字、Range 读、list/stat、覆盖/删除、`/_e2e/` 前缀隔离 + 收尾清理。写操作**只允许** `/_e2e/` 前缀（§7a）；收尾删除并核空。

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| TG0 | E2E harness：凭据加载（test 路径/env）、代理装配、临时 MTProto session、`/_e2e/` 隔离助手 + 失败也执行的清理守卫 | ⬜ | — | — |
| TG1 | 登录 + 上传→list/stat→回读逐字（多块文件） | ⬜ | — | — |
| TG2 | Range 读（按驱动能力声明）+ 覆盖写语义 + 删除 + vfs 级加密往返（可选拉伸） | ⬜ | — | — |
| TG3 | 旧 `#[ignore]` 真机测试清尾（能跑则跑并记录）+ 文档收口 + merge/push | ⬜ | — | — |

## 批次日志

- 2026-09-11：立项。凭据与代理自检当日完成（getMe/getUpdates/sendMessage 全通，message_id 3）。待开工。
