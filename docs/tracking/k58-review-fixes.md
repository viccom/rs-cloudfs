# K58 审查修复批 任务跟踪单（High×5 + Medium×8）

> 审查：2026-09-14 对 6fbd90a..7e1b2be（K55/K56 修复批 + K57 卷管理面 + UI 批 Rust 触点）四分域深度审查 + 主会话亲验。
> 修复范围（负责人批准）：High 全部 + Medium 全部；Low 挂账不动（择便顺带需注明）。
> worktree：`fix/k58-review`（收口 merge 回 main）。基线 main@7e1b2be：workspace 1045/0/12、winfsp 腿 117/0/1。

## 批次与发现映射

| 批 | 内容 | 修复项 | 状态 | 证据 |
|---|---|---|---|---|
| FA | core 原子写原语 + 脱敏补漏 | H3（temp+sync+rename 三写盘点+save_toml）、M2（proxy_url/sync_url 入 SECRET_VALUED_KEYS）、M5（UPDATE 重读经脱敏） | ✅ 2026-09-12 | 红：M2 两测断言红（SHOW 原样出 proxy_url 值 / parse 错误引文泄 userinfo）+ H3/M5 编译红（E0432/E0603/E0425）→ 绿 8 新测；门禁 test 642/0/4 + clippy -D warnings 过 + fmt clean（core+cli）；web volumes_page 36/0/0 加验（SHOW 面经 seam 实测） |
| FB | 命令串行化 + 取消安全 | H1+H2（handler 闭包 spawn+oneshot+单 permit——web 超时只弃等待；全入口恢复 K48 串行；虚假注释修正）、M7（add_volume 结构化成败替代文本前缀匹配） | ⬜ | — |
| FC | web 安全 + 可用性 | H5（Host 白名单防 rebinding + /api/upload 挂 guard 收口存量 CSRF）、M3（Edit 门撤除——disabled 卷可拉 SHOW）、M6（前端 action-in-flight 锁防轮询重绘击穿） | ⬜ | — |
| FD | REBUILD 加固 | M4a（R5 检查点实例身份判据）、M4b（checkpoint 复查 outstanding） | ⬜ | — |
| FE | worker panic 隔离 | M1（process_job catch_unwind + panic 补 degraded 终态——幽灵 outstanding 消除） | ⬜ | — |

复核（负责人要求）：每批主会话 diff 审查 + 收口全门禁 + 逐项针对性复核验证，结论入下表。

| 项 | 复核结论 |
|---|---|
| （收口时填） | — |

## 批次日志

- 2026-09-14：审查完成（四分域 + 主会话亲验 5 High；裁决子代理矛盾：卷管理 POST 实为 axum 默认 2MB 上限——1900MiB DoS 项剔除）。立项本批。
- 2026-09-12（FA）：H3/M2/M5 落地。**H3**：core 新 `write_config_atomically`（config.rs pub fn——同目录 `.<name>.tmp` 写前删残留 → write_all → sync_all → rename 原子替换，rename 失败删临时返错）；五写盘点收敛：core `write_volume_enabled`/`save_toml`/`save_toml_scrubbed` + cli CREATE/UPDATE（save_toml×2 为审查建议顺带，config.toml 撕裂同病）。原子性可测取舍：Windows 只读位对目录不阻止文件创建、ACL 不可经 std——写阶段失败以「临时路径被目录占据」构造（原文件逐字节不动）、rename 阶段失败以「目标为目录」构造（临时文件清理断言），五种真实 fs 态全覆盖，无需注入式拆分。**M2**：`proxy_url`/`sync_url` 入 `SECRET_VALUED_KEYS`（URL 可携 userinfo）——SHOW 折叠/redact 掩值自动跟进（同表驱动）；前端最小修补：volumes.js `vf-sync-url` 从 plain 预填组移入 writeOnly 占位符组（否则 `{"set":true}` 对象预填渲染为 [object Object]）；proxy_url 无表单字段无需前端改动。既有测试无 proxy_url/sync_url 的 SHOW 值断言——零机械适配。**M5**：`redact_credential_values` 提 pub（doc 注单一漏斗原则），cli UPDATE 重读抽 `parse_explicit_table_redacted`（路径文案不变，错误经漏斗）。测试：core 新 8（H3×5 + M2×2 + M5 直连×1）+ cli 新 1。
