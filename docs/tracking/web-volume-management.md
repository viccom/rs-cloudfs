# Web 卷管理面 + 运行时 rebuild 任务跟踪单

> 计划：docs/plans/2026-09-13-web-volume-management.md ｜ K48/K49/K11 修订（计划 §2，收口入档 decisions）｜ worktree：`feat/web-volume-mgmt`（收口 merge 回 main）。
> 基线：main@e7a1543（workspace 955/0/12；winfsp 腿 117/0/1）。

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| P0 | /volumes 双页骨架 + 回调缝 + SHOW + pending 扩列 + current 回退修复 | ✅ | 全部落地：`/volumes` 管理页（volumes.html/js + 四统计卡 + 七列表 + 实例级总用量卡）与单卷说明页（裁决④）；`VolumeCommandClient` 缝（web 类型 + `serve_multi_with_commands` 变体 + cli 装配注入同一 handler Arc）；控制通道 `SHOW <name>`（core `volume_show_json`：显式键单行 JSON，`SECRET_VALUED_KEYS` 只回 `{"set":bool}`）+ `GET /api/volumes/{name}/config`（5s 超时，注册表门控，ERR→404/缝缺→503）；`/api/volumes` 行扩 `pending`（`queue_stats().outstanding()`，Failed=null）；Origin middleware（同源校验挂 `/api/volumes` 族，跨源 403，读/页面路由不拦）；index nav + tabs 行尾「＋」+ localStorage 记 current + current 悬空回退第一个 running。 | 见批次日志 2026-09-13 P0 行 |
| P1 | 卸载 + Enable/Disable + Origin 生效 + allow_remote_admin | ✅ | 全部落地：core `render_volume_toml`（显式键保序受控重渲染，toml `preserve_order`）+ `write_volume_enabled`（全漏斗校验→受控重写）；cli `ENABLE/DISABLE/CONFIGS` 三命令（DISABLE=先写盘后走 `remove_volume` 同一 K50 序【直接调用复用】，写盘失败不动运行态；ENABLE=写盘后走 `add_volume`；CONFIGS=配置全集行 `名 backend enabled=x running/absent`，坏文件行内 `invalid` 不炸全集；三者入 H1 停机 gate【ENABLE/DISABLE 拒新，CONFIGS 只读】）；web `POST /api/volumes/{name}/{remove,disable,enable}`（经缝转发、120s 写预算、成功 `{"ok":true,"reply":...}` / ERR→409 文本透传、缝缺 503、Origin guard 覆盖）+ `GET /api/volumes/configs`（CONFIGS 包端点 JSON 化：good `{name,backend,enabled,running}` / 坏 `{name,invalid,reason}`）+ 非 loopback 降级（`serve_multi_with_remote_admin`：非回环 bind 且无 `allow_remote_admin=true` → 写路由族 403 点名键，读/页面不拦，启动 eprintln warning；新进程键入 K19 三清单 + validate 无规则 + usage 文档行）；前端卷表数据源改 configs+runtime 合并（disabled 灰化行+徽章、stopped 徽章、invalid 徽章带原因、Actions 列生效：Unmount/Disable 单步 confirm、Enable 直发；按钮 loading 转圈；完成后立即刷新+toast（ok/err 4s 自灭，ERR 文案原样展示）；configs 503 时降级 runtime-only 渲染）。 | 见批次日志 2026-09-13 P1 行 |
| P2 | REBUILD 后台化（R1–R6）+ CLI 转发 | ⬜ | — | — |
| P3 | CREATE + 受控 toml 生成 + 新增表单 | ⬜ | — | — |
| P4 | UPDATE + write-only 凭据 + 编辑表单 | ⬜ | — | — |
| P5 | DESTROY 两段确认 + purge_local | ⬜ | — | — |
| P6 | Refresh from remote 按钮 | ⬜ | — | — |

## 批次日志

- 2026-09-13：立项。负责人批准（「认可，请执行实施计划方案（需落库），然后开工」）；方案经三轮设计收敛（web 管理面设计 → sqlite 评估搁置 → rebuild 并入 + R1–R6 约束 → 双页独立布局）；五个小裁决点按推荐落定（计划 §2.3）。前置事实：rebuild 运行时并发已核安全（WAL/busy_timeout、可见性即时、窄竞态由 R2 排空门槛消除——2026-09-12 会话核实）。
- **2026-09-13 P0 完成**（worktree `feat/web-volume-mgmt`，commit `feat(web): /volumes page skeleton, command seam, and SHOW`）：
  - **红→绿时间线**（真红为主，逐条真实输出见 commit 正文与下述证据）：
    - core `volume_show_json` ×2 测试：编译红（`unresolved import ... volume_show_json`，E0432）→ 实现后 `2 passed`；
    - web `volumes_page` ×10：先落最小编译 shim（类型别名 + `serve_multi_with_commands` 构造器存缝不消费，行为零变化）→ **断言红** `9 failed / 1 passed`（绿的是 origin 放行护栏，按设计先绿）→ 实现后 `10 passed`；
    - cli `web_volume_mgmt` ×3：**断言红**（`SHOW` → `ERR: unknown command`；`/api/volumes` 行缺 `pending`：`pending: Null ≠ Number(1)`）→ 实现后 `3 passed`；
    - 补 1 条 green-since-birth 钉子（`/static/js/volumes.js` 经 rust-embed 真实送达，防新静态文件漏嵌入）。
  - **门禁**：workspace `cargo test --no-fail-fast` **971 passed / 0 failed / 12 ignored**（基线 955/0/12 + 新增 16：core 2 + web 11 + cli 3）；clippy `-D warnings` 干净；fmt 干净；`check_layers` OK（12 manifests）；`scan_secrets` OK；winfsp 腿（LIBCLANG_PATH）**117 passed / 0 failed**（= 基线）。
  - **机械适配清单**（编译必需/同构重构，无断言漂移）：web lib.rs 三处 `AppState::Multi { volumes }` 模式补 `..`；cli `add_volume` 内联路径安全预检提取为 `RuntimeVolumeControl::name_is_path_safe`（SHOW 共用，行为等价）；web Cargo.toml tokio 加 `time` feature。既有 `serve_multi` 调用方（multivolume.rs 两处、cli bind 处）**零适配**（新增 `serve_multi_with_commands` 变体，沿 run/run_with_commands 先例）。
  - **前端手动验证**（浏览器 use 主代理专属，本批以 jsdom DOM 断言替代 + 如实标注）：harness 对真实 app.js/volumes.js/模板跑 22 项断言全过——localStorage 恢复选中卷、REMOVE 后 current 悬空回退第一个 running 并触发 reload、tabs 行尾「＋」指向 /volumes、失败卷 chip 禁用带 tooltip；管理页四卡数值（3/2/1/pending=3）、总用量卡聚合、表格行（盘符 `-`、pending `-`、Failed 徽标带原因 tooltip、Actions 列空、Add Volume 禁用占位 title="coming in P3"）。已知 harness 侧坑：jsdom 的 innerText setter 不做字符串 coercion（真浏览器会），比对前 String() 化。真实浏览器视觉面（布局/字体渲染）未验证，待后续批真机窗口顺带复核。
  - **未单测路径**（如实记录）：缝超时 5s 分支无专门单测（5s 预算使测试-hostile；快速路径被 e2e 覆盖；挂起命令的 503 文案在实现中核对）。
  - **风险边界确认**：SHOW 对已摘除卷仍可答（读文件不查注册表——按 §1.2 语义）；但 web 端点先查注册表，摘除后 404（e2e 钉死）——两层语义已在代码注释与本表写明。
- **2026-09-13 P1 完成**（worktree `feat/web-volume-mgmt`，commit `feat(web): runtime unmount and enable/disable from the dashboard`）：
  - **红→绿时间线**（真红为主，逐条真实输出见 commit 正文）：
    - core ×7：**编译红**（E0432 `no render_volume_toml in config` / `no write_volume_enabled in config`；E0609 `no field allow_remote_admin`）→ 实现后首跑 4 failed，其中 3 个为**断言红真发现**：`toml::Table` 默认 BTreeMap 按字典序——保序规格不可达，启用 toml `preserve_order` feature（仅 core 依赖 toml，波及面=config 面；已核无测试依赖 keys 迭代序）后 `405 passed / 0 failed`；余 1 failed 为预期机械更新（`process_scoped_keys_are_the_process_globals` 精确清单加键）；
    - cli ×6（runtime_volumes）：**断言红** `6 failed / 14 passed`，全部 `ERR: unknown command`（ENABLE/DISABLE/CONFIGS 未路由）→ 实现后 `20 passed / 0 failed`；
    - web ×6（volumes_page）：**编译红**（E0599 `serve_multi_with_remote_admin` 不存在）→ 实现后 2 failed 修两处（configs reason 括号剥离属解析精度；Windows 不许 connect 0.0.0.0→测试改连 127.0.0.1:port）→ web 全套 `62 passed / 0 failed`；
    - cli e2e ×1（web_volume_mgmt）：green-since-birth 钉子（组件各自已红→绿，本测试钉**组合**：web 端点→缝→真 handler→文件+注册表双面一致）；`4 passed / 0 failed`。
  - **门禁**：workspace `cargo test --no-fail-fast` **991 passed / 0 failed / 12 ignored**（基线 971 + 新 20：core 7 + cli 7 + web 6）；clippy `-D warnings` 干净（执行期新 lint `permissions_set_readonly_false` 于测试恢复行加 scoped allow + 注释）；fmt 干净；winfsp 腿（LIBCLANG_PATH）**117 passed / 0 failed**（= 基线）；`check_layers` OK（12 manifests——core Cargo.toml 动了 toml feature 后必跑项）；`scan_secrets` OK。
  - **DISABLE 卸载复用路径**：未抽新公共函数——`disable_volume` 写盘成功后**直接调用** `self.remove_volume(name)`（同一方法、同一 K50 序、同一 H1 观察点），OK 文案由 DISABLE 包装（含文件路径与跨重启语义），ERR 原文透传并前置「文件已写+幂等重试」指引；写盘段抽 `persist_enabled_flag`（ENABLE/DISABLE 共用，经 core `write_volume_enabled` 全漏斗校验）。
  - **allow_remote_admin 键落点**：`CyDriveConfig` 新字段（默认 false=安全缺省，`skip_serializing_if` 缺省不序列化——setup 写出的 config 字节不变）+ K19 三清单同步（KNOWN_TOML/PROCESS_SCOPED/LEGACY_REJECTED）+ validate 无规则（bool）+ `docs/cydrive-usage.md` 配置表行；web 侧新构造器 `serve_multi_with_remote_admin`（`serve_multi_with_commands` 委托 false），cli `bind_multi_web_ui` 传入 `process_cfg.allow_remote_admin`。
  - **前端手动验证**（浏览器 use 属主代理专属；本批沿 P0 先例以真实 volumes.js 的 DOM stub harness 替代并如实标注）：16/16 断言过——configs+runtime 合并行（Running+盘符 V:+pending+size 联查、Disabled 灰化行仅 Enable、Stopped 徽章、Invalid 徽章带原因且无按钮）、四卡与总用量卡聚合、Unmount confirm 后 POST `/api/volumes/a/remove`、成功/ERR toast 文案（ERR 原文）、confirm 拒绝不发请求、Enable 无 confirm、configs 503 降级 runtime-only。harness 侧坑（如实记录）：stub getElementById 看不到 createElement 产物（每 toast 各建 host，收集器跨 host 扁平化）；`onVolumeActionClick` 是 fire-and-forget（与浏览器一致），断言前需 setImmediate 排空微任务链。真实浏览器视觉面未验证，待后续批真机窗口顺带复核。
  - **机械适配清单**（编译必需/预期更新，无断言漂移）：core tests/config.rs 全量 struct 字面量补 `allow_remote_admin: true`（E0063）；config_volumes.rs `process_scoped_keys_are_the_process_globals` 期望清单加键（key_partition 联动，计划预期项）；web lib.rs `AppState::Multi` 模式两处补 `..`（新字段 writes_allowed）；clippy 新 lint scoped allow ×1（上述）。fmt 批量重排 5 文件（纯格式）。
  - **未单测路径**（如实记录）：写路由 120s 超时分支无专门单测（时长测试-hostile；快速路径与缝缺失/Origin/非回环降级均有覆盖，503 文案在实现中核对——沿 P0 的 5s 缝超时同类豁免先例）；`show_seam`/`run_volume_action` 的 panic 路径由控制通道 M1 catch_unwind 既有测试背书。
