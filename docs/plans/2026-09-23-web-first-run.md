# Web 首启引导（first-run bootstrap）实施计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.
> 执行模式：本会话 hub-and-spoke（负责人 2026-09-23 对 Phase 8-B 的既定选择，本次沿用）；worktree `feat/web-first-run`（独立 target）；**不 push、合入指令负责人另下**。

**Goal:** `cydrive run` 在完全无配置的目录里能启动——自动生成最小进程配置进 init 模式（只起 Web 控制面），用户经既有 /volumes 页面从零创建第一个存储卷。

**Architecture:** 全部新逻辑藏在一道门后：cwd 既无 config.toml 也无 config.json 才触发。有配置的路径（单卷/多卷/web/控制通道/七驱动）逐字不变。init 模式 = 生成配置 + 放行零卷 boot + web 空态引导文案；卷的创建/增删改完全复用 P3 既有能力（CREATE 表单 + 控制通道 ADD + `write_config_atomically`）。

**Tech Stack:** Rust（cloudkit-cli boot 链 + cloudkit-core config 原语）/ 既有 Web UI 静态页（零新前端面）。

---

## 0. 探查钉死的事实（2026-09-23，HEAD=fc5df7e，file:line 可复核）

1. **run 的 boot 链**：`main.rs:948-966` `async fn run()` → `discover_config_with_volumes()` → `Single` 臂 `run_single_volume`（main.rs:970）/ `Multi` 臂 `run_multi_volume`（main.rs:1021，逐卷装配）→ `run_multi_with_transports_and_commands`（lib.rs:1818，webdav/web/控制通道 bind 全在此）。
2. **零卷拒绝有两道门**（⚠️ 不是一处）：
   - 门 A（discovery 期，core crate）：`crates/cloudkit-core/src/config.rs:1001-1008` `discover_volumes` 空目录 → `ConfigError::Invalid("volumes directory … contains no *.toml volume files …")`；经 `load_volumes`（config.rs:1038）← `discover_config_with_volumes_and_store`（lib.rs:7672）——**空卷目录在 discovery 就 fail，到不了 boot**。
   - 门 B（boot 期，cli crate）：`crates/cloudkit-cli/src/lib.rs:1877-1884` `runtimes.is_empty()` → `no_enabled_volumes_message`（lib.rs:2259）bail。
3. **无配置现状**：`discover_config_with_volumes_and_store` 尾部（lib.rs:7691-7694）bail `"no config found in the current directory…"`。**该函数本身不改**——bootstrap 在 main.rs `run()` 里先于 discovery 探测。
4. **进程级 config.toml 无渲染函数**；唯一生成先例 = `crates/cloudkit-cli/src/setup.rs:803-813` 手写常量 `MULTI_PROCESS_TOML`（volumes_dir=volumes、webdav 8080、web 8088、enable_web_ui=true，刻意不用 `save_toml`——全量铺设卷键过不了混装 guard `ensure_no_volume_keys_in_process`，config.rs:1075）。init 模板对齐此形态 + `write_config_atomically`（config.rs:923，K58 H3）落盘。
5. **默认端口 = webdav 8080 / web UI 8088**（config.rs:1479-1481 Default impl；setup --multi 模板同值）。⚠️ 修正：先前会话对负责人说的「8485/8486」是负责人自配值，非程序默认。**init 用 8088。**
6. **web 多卷装配已是「空注册表可起」**：`bind_multi_web_ui`（lib.rs:2208-2249）enable_web_ui on + 空注册表正常 bind（HostAllowlist 从 bound addr 派生，非 loopback 写路由 403——K58 H5 现成）；`bind_multi_webdav`（lib.rs:2286）空卷集直接 None（8080 不 bind，零冲突面）。`/volumes` 路由注册在 cloudkit-web/src/lib.rs:582。
7. **前端空态已有**：`volumes.js` `renderVolumesTable`（:193-213）零卷渲染 `volumes.empty` 文案 + 「＋ 添加卷」按钮 → `openVolumeForm('create')` → `POST /api/volumes`（:899）→ 成功刷新。**init 模式零新前端面**，只改空态文案为引导语（i18n.js en :126 / zh :396）。
8. **测试缝先例**：`tests/multivolume_config.rs`（chdir+tempdir+`InMemoryStore` 直测 discovery）；`tests/run_e2e.rs`（tempdir + 预连接 MockTransport + `webdav_port = 0` 进程内 boot 全栈；场景 5「空目录 discovery 报错」测的是 discover 函数本身——bootstrap 在 main.rs 层，**该既有测试不受影响**）；`tests/runtime_rebuild.rs`（直调 `run_multi_with_transports_and_commands`——签名变更需同步其调用点）；`tests/volumes_page.rs`（`RegistryHandle::new(vec![])` 起路由 + 源码文本 pin）。
9. **自动开浏览器零先例**（全仓无 webbrowser/open 依赖）——新小函数，R5 对齐 lib.rs 既有「函数内并列 `#[cfg(unix)]`/`#[cfg(not(unix))]` 块」样式（lib.rs:7035 先例）。
10. `auto_mount_drive` 是 process-scoped 键（setup.rs:825-827 注释），不会被混装 guard 拒。

## 设计裁决（D1–D6，随计划批准生效）

- **D1 触发条件**：`run` 子命令 + cwd 无 config.toml 且无 config.json。其余子命令（status/stop/rebuild/…）在无配置目录维持既有报错。
- **D2 生成物**：手写模板常量 `FIRST_RUN_PROCESS_TOML`（volumes_dir/webdav 8080/web 8088/enable_web_ui/auto_mount_drive + 头注释声明「首次运行生成；卷经 Web 界面添加」）经 `write_config_atomically` 落 config.toml + `create_dir_all("volumes")`。**已有配置文件时恒不触碰**（探测在前，生成在后，无覆盖路径）。
- **D3 init 模式放行的两道门**：新 discovery 变体 `discover_first_run_config()`（载入刚生成的 config.toml → 混装 guard → validate → 直接返回 `Multi { volumes: vec![] }`，**绕过门 A 但不改 `discover_volumes`/既有 discover 函数**）；`run_multi_with_transports_and_commands` 增 `first_run: bool` 参数（门 B 改 `runtimes.is_empty() && !first_run` 时 bail；first_run 时空注册表继续 → webdav None → web bind 空注册表 → 控制通道 `LIST` 回 `OK: 0 volume(s)`）。既有调用点（main.rs:1084、runtime_rebuild.rs、run_e2e 若有）一律补 `false`——**行为零变化，签名机械更新**。
- **D4 引导呈现**：控制台 info! 横幅（已生成配置 + 请打开 http://127.0.0.1:8088）；web bind 成功后自动开浏览器（`open_browser` 新函数，失败仅 warn 不致命；Windows `cmd /c start` + CREATE_NO_WINDOW，unix `xdg-open`）；前端空态文案改引导语（en/zh 双语）。
- **D5 单卷臂不涉 init**：生成的配置带 volumes_dir → 恒 Multi；Single 臂零触碰。
- **D6 安全面**：bind 恒 loopback（模板写死 127.0.0.1）；`allow_remote_admin` 缺省 false（非 loopback 写路由 403 既有）；凭据经 CREATE 表单落卷文件（write-only/脱敏既有纪律）。端口被占 → 既有 bind 失败路径（多卷臂 K22 降级 error! + 实例继续跑；控制台横幅已先提示 URL，bind 失败时 warn 指路编辑 config.toml）。

## 验收对照

| 标准 | 验证 |
|---|---|
| 有配置路径零变化 | 既有全量断言零漂移 + multivolume_config/run_e2e 既有用例原样绿 |
| 无配置 → bootstrap → init boot → web 可建卷 | 新 e2e 腿：空 tempdir → bootstrap → `run_multi_with_transports_and_commands(first_run=true)` 空卷注入 → web GET /volumes 200 → POST /api/volumes 建卷（web 测试缝）→ 卷文件落盘 |
| 两道门放行的精确性 | 门 A：`discover_first_run_config` 单测（含「已有配置时 bootstrap 不触发」反臂）；门 B：first_run=true 空卷 boot 绿 + first_run=false 空卷 bail 既有文案钉 |
| 生成物合法性 | 模板文本 pin（键集合/端口/头注释）+ `load_toml_with_keys` 解析回读断言 |
| 门禁 | 每批五门禁 + 收口 baidu/telegram 组合 clippy |

## 任务分解

### FR1（cli boot 链）：bootstrap + init 模式放行 + 自动开浏览器

**Files:**
- Modify: `crates/cloudkit-cli/src/main.rs:948-966`（run() 插入 bootstrap 探测与 init discovery 分叉）、`main.rs:1021-1084`（run_multi_volume 透传 first_run）
- Modify: `crates/cloudkit-cli/src/lib.rs:1818`（签名 +first_run、门 B 条件、横幅、浏览器触发）、lib.rs:7634 附近（`FIRST_RUN_PROCESS_TOML` 常量 + `bootstrap_first_run_cwd()` + `discover_first_run_config()` + `open_browser()`）
- Test: `crates/cloudkit-cli/tests/multivolume_config.rs`（bootstrap/discovery 四用例）、`crates/cloudkit-cli/tests/runtime_rebuild.rs`（调用点补参）、新 `crates/cloudkit-cli/tests/first_run_e2e.rs`（bootstrap→init boot→web GET/POST 建卷全链）

**TDD 序**（红→绿留证，逐条）：
1. 红1：`bootstrap_first_run_cwd` 空目录 → config.toml 存在 + 文本含全部键 + volumes/ 目录存在；已有 config.toml → 返回 false 且文件 mtime/内容不动。
2. 红2：`discover_first_run_config` bootstrap 后 → `Multi{volumes: empty}`；process cfg 字段回读（8080/8088/enable_web_ui）。
3. 红3：`run_multi_with_transports_and_commands(first_run=true)` 空注入 → boot 成功（web 起在 ephemeral 端口、GET /volumes 200）；`first_run=false` 同参 → 既有 bail 文案不变（既有测试即钉）。
4. 红4：first_run e2e——bootstrap → init boot → `POST /api/volumes` 建 local 卷 → GET /api/volumes 含新卷 + 卷文件落盘 volumes/ 下。
5. 绿后收尾：`open_browser` 单测只钉「构造的命令在 unix/windows 两 cfg 下可编译」（R5 对方平台可编译红线）；运行行为不自动化断言（spawn 副作用），代码注释注明。
6. Commit: `feat(cli): first-run bootstrap——无配置目录自动生成最小配置进 init 模式（web 首启引导 FR1）`

### FR2（web 空态引导）：i18n 文案 + pin 更新

**Files:**
- Modify: `crates/cloudkit-web/static/js/i18n.js`（`volumes.empty` en/zh 改引导语）
- Test: `crates/cloudkit-web/tests/volumes_page.rs`（若 pin 了旧文案则同步；补一条空态文案含引导动作的 pin）

**步骤**：查既有 pin → 改文案（en: "No volumes yet — use ＋ Add Volume above to create your first one." zh: 「尚无存储卷——点击上方「＋ 添加卷」创建第一个卷。」）→ pin 红→绿 → volumes_page 全套绿。
Commit: `feat(web): 首启空态引导文案（web 首启引导 FR2）`

### FR3（收口）：文档 + 门禁

- decisions **K87**（first-run bootstrap 设计 D1–D6 + 端口修正 8088 留痕）；README（快速开始段补「零配置直接 run」）；AGENTS（当前阶段块 + 测试计数）；跟踪单 `docs/tracking/web-first-run.md` 终态。
- 五门禁 + baidu/telegram 组合 clippy。
- Commit: `docs(web-first-run): K87 入档 + 文档联动（收口）`

## 风险与挂账预登记

- 端口 8080/8088 被占：多卷臂 K22 降级（error! + 实例继续），横幅与实际 bind 不一致的窗——first_run 的 web bind 失败时补一条 warn 指路（FR1 内做掉，不挂账）。
- 控制通道端口在 init 模式的绑定形态未在探查中展开——FR1 实现时核（若控制通道 bind 也是硬错误需同样确认空注册表行为；预计不受影响，RuntimeVolumeCommands 与卷无关）。
- 浏览器自动打开在服务器/无 GUI 环境：spawn 失败仅 warn（D4 已定）。
- v1 不做：初始化向导多步流、非 loopback 引导、端口自动改选——需要时再立项。
