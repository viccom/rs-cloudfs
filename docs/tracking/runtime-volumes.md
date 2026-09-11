# Phase 3.6 运行态卷管理 任务跟踪单

> 计划：docs/plans/2026-09-10-runtime-volumes.md ｜ 裁决 K48–K51 ｜ worktree：`feat/runtime-volumes`（收口 merge 回 main）。
> 基线：main@5927a25（workspace 911/0/12 ignored——含 telegram 真网 E2E 3 个；winfsp 腿 117/0/1）。2026-09-11 复核：§0 六接缝经 RB1-RB4/telegram E2E/Low 清尾后全部成立（serve_volumes 精确化至 webdav server.rs:95）；兼容表述已按 1.0 前不兼容政策清除。

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| RV0 | 卷级 enabled 键（发现期跳过） | ✅ | `enabled` 入 `KNOWN_TOML_KEYS`+`VOLUME_SCOPED_KEYS`+`LEGACY_REJECTED_KEYS`（interfaces §4 三处同步，validate 无新增规则——bool 无非法值）；`CyDriveConfig.enabled: bool`（serde default true，`skip_serializing_if` 只在 false 时落盘——进程级 setup 产物不携带该键，K19 guard 不会拒自己的输出）；`load_volumes` 对 `enabled=false` 卷 `info!` 声明后跳过（不进盘符冲突校验/装配/横幅）；config_volumes.rs +9 测试（键分区/禁用跳过+日志声明捕获/缺省启用/进程级 guard 拒收+端到端特征/盘符不占/false 往返+缺省保存干净/legacy 拒收） | 红：断言红 5（unknown key `enabled`×2、guard Ok()、键缺失、legacy 静默接受）+ 编译红 E0609×5 + 日志测试变异红（info!→debug! 捕获失败）；绿：config_volumes 26/0；workspace 920/0/12（基线 911+9）；winfsp 腿 117/0/1；clippy/fmt/check_layers/scan_secrets 全绿 |
| RV1 | 注册表动态化 + WebDAV/仪表盘动态分发 | ⬜ | — | — |
| RV2 | 控制通道 ADD/REMOVE/LIST + 卸载安全序 | ⬜ | — | — |
| RV3 | 真机验收 + 文档收口 + merge/push | ⬜ | — | — |

## 批次日志

- 2026-09-11：**RV0 落地**（feat/runtime-volumes）——卷级 `enabled` 键 + 发现期跳过（TDD 红→绿：断言红 5/编译红 E0609×5/日志变异红 → config_volumes 26/0、workspace 920/0/12、winfsp 117/0/1 全门禁绿）。设计要点：缺省 true 为语义自然缺省（非兼容考量）；序列化仅在 false 时落盘（进程级 setup 产物不携带卷作用键）；跳过策略落 `load_volumes`（解析点），`discover_volumes` 保持纯文件列举契约；特征测试钉住「进程级出现 enabled 恒被拒」（实现前 unknown-key 拒、实现后 K19 guard 拒）。README/标准文档无键清单枚举面，按收口指令不新增文档（用户面文档随 RV3 收口）。

- 2026-09-10：立项。负责人批准动态加载/卸载方向（`enabled` 键此前已批准暂缓，合流本批为 RV0）；接缝侦察（VolumeRegistry 启动期 Vec/serve_volumes 一次性路由/控制通道仅 STOP）；K48–K51 裁决定稿（显式控制命令、REMOVE 不碰卷文件、卸载安全序中止不半卸、共享注册表三面同源）；计划与跟踪单落盘。待负责人批准开工。
