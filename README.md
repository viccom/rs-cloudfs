# rs-cloudfs

**多云存储平台** —— 在统一存储抽象之上，把多个云后端变成一个顺手的本地盘：WebDAV 挂载（Windows `Y:`/`Z:` 盘、Linux davfs2）、Web 仪表盘、多机元数据同步（LWW + SSE 准实时）、CLI 与远程 Bot 命令。单二进制、上传失败不丢数据、断点可续传、支持明文与加密两种存储模式。

**血统**：fork 自 [rs-CyDrive](https://git.metme.top/viccom/rs-CyDrive)（Telegram 无限云盘，Rust 完全重写版，全 git 历史保留），融合 PrivateCloudFS（Go 版多云聚合，位于 `E:\Go_codes\PrivateCloudFS`）的设计与实战经验重构为分层多云架构。行为基线：Python 版 CyDrive 兼容契约（telegram 驱动延续）。

## 架构（六层，详见 [docs/standards/architecture.md](docs/standards/architecture.md)）

```
L5 应用  cli │ webdav 网关 │ web 仪表盘 │ bot(telegram)
L4 服务  上传队列 │ 同步引擎(+sync-server) │ LRU 缓存 │ 加密(v1 GCM/v2 分块 AEAD 流式)
L3 领域  VFS │ 元数据索引(SQLite) │ MetadataEvent 总线
L2 抽象  StorageDriver trait + 能力位 + 错误分类学 + conformance kit
L1 驱动  telegram │ baidu │ local │ (未来: 115/123/s3…)
```

**新后端接入 = 实现一个驱动 + 过 conformance 套件，上层全部能力（挂载/仪表盘/同步/CLI）自动可用。**

## 状态（2026-09-07）

| 阶段 | 内容 | 状态 |
|---|---|---|
| Phase -1 | 规范先行（架构约束/代码/接口/日志/文档五标准 + 本 README/AGENTS） | ✅ 完成 |
| 基线 | fork 自 rs-CyDrive 0.7.2（527 测试绿，telegram 后端生产可用） | ✅ 完成 |
| Phase 0 | crate 改名重排（cloudkit-*/ck-*），纯搬迁 + 层检查/秘密扫描 CI 门禁 | ✅ 完成 |
| Phase 1 | 百度 spike → StorageDriver 抽象落地 → 加密 v2 流式（0.8.0，617+ 测试绿，含真机冒烟两轮） | ✅ 完成 |
| Phase 2 | ck-local + ck-baidu + 组合根接线 + 端到端硬验收（0.9.0，740 测试绿；baidu/local E2E 通过、telegram 腿待独立测试 chat） | ✅ 完成 |
| Phase 2.5 | 多卷启用（Registry + 每实例配置 + 多盘挂载，方案一裁决） | ⬜ 下一步（另立计划） |
| Phase 3 | 115/123/多卷挂载/桌面端/自更新（择机） | ⬜ |

阶段计划与裁决：[docs/plans/2026-09-07-cloudfusion-foundation.md](docs/plans/2026-09-07-cloudfusion-foundation.md) ｜ 历史裁决：[docs/decisions.md](docs/decisions.md)

## 快速开始（三后端：telegram / baidu / local，`backend` 配置键分发）

```powershell
cargo build --release
./cydrive.exe setup     # 选后端：telegram(bot token/chat_id) / baidu(appkey+refresh_token) / local(根目录)
./cydrive.exe doctor    # 体检（baidu：token 探活/直连声明；local：root 可写）
./cydrive.exe run       # WebDAV :8080 → 自动挂载（默认 Y:；config drive_letter 可改）｜ 仪表盘 :8088 ｜ ctrl+c 或 cydrive stop
```

baidu 实例最小配置（config.toml）：`backend = "baidu"` + `baidu_app_key/baidu_app_secret/baidu_refresh_token`（或 env `CYDRIVE_BAIDU_*`，access_token 缺省由 refresh 换取）；`baidu_root` 默认 `/apps/cloudfs`。
local 实例：`backend = "local"` + `local_root = "<绝对路径>"`。
权威后端（baidu/local）冷启动可 `cydrive rebuild` 从后端重建索引（明文集；加密实例走 sync）。
新后端接入指南：[docs/standards/driver-onboarding.md](docs/standards/driver-onboarding.md)（conformance 套件 + 装配点 + E2E 拓扑）。

多机同步（可选）：部署 `cydrive-sync-server`（[部署文档](docs/sync-server-deployment.md)）→ 各机 config.toml 写 `sync_url`/`sync_secret`，上传成功后秒级同步到其他机器。

完整使用文档：[docs/cydrive-usage.md](docs/cydrive-usage.md)

## 许可

MIT（与 rs-CyDrive 一致）。
