# K58 审查修复批 任务跟踪单（High×5 + Medium×8）

> 审查：2026-09-14 对 6fbd90a..7e1b2be（K55/K56 修复批 + K57 卷管理面 + UI 批 Rust 触点）四分域深度审查 + 主会话亲验。
> 修复范围（负责人批准）：High 全部 + Medium 全部；Low 挂账不动（择便顺带需注明）。
> worktree：`fix/k58-review`（收口 merge 回 main）。基线 main@7e1b2be：workspace 1045/0/12、winfsp 腿 117/0/1。

## 批次与发现映射

| 批 | 内容 | 修复项 | 状态 | 证据 |
|---|---|---|---|---|
| FA | core 原子写原语 + 脱敏补漏 | H3（temp+sync+rename 三写盘点+save_toml）、M2（proxy_url/sync_url 入 SECRET_VALUED_KEYS）、M5（UPDATE 重读经脱敏） | ⬜ | — |
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
