# 服务生命周期计划：cydrive stop + SIGTERM + systemd/Linux 挂载链

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 让后台运行的 CyDrive 实例可优雅退出——新增 `cydrive stop` 控制通道与 Unix SIGTERM 处理，补齐 systemd 部署资产与 platform 层 Linux 挂载链（gio→davfs2），全部 TDD。

**Architecture:** 每个运行实例在 `db_path` 同目录写端口文件 `cydrive.control`（内容 `127.0.0.1:<ephemeral>`），起一个只绑回环的微型控制 TCP 服务（行协议 `STOP`→`OK`）；三条停机源（Ctrl+C / SIGTERM(cfg unix) / 控制通道 STOP）汇入同一个 shutdown 触发器，统一走既有 RunHandle 优雅排干链。`cydrive stop` 与 run 同 cwd 发现 config → 同规则推导端口文件 → 连接发送 STOP。Linux 挂载链在 platform 层以纯函数（命令构造/后端探测）+ cfg(unix) 真实现 + #[ignore] 真机测试三段式实现（镜像 windows 模块做法）。

**Tech Stack:** tokio（net/sync/signal）、clap 子命令、既有 RunHandle 生命周期；无新 crate 依赖。

**工作目录：** worktree `E:\GitHub\rs-CyDrive-t2`（分支 `feat/service-lifecycle`，自 main `6805ca2` 切出）。

**TDD 委派纪律：** 同前两批（测试作者红→实现者绿→主会话断言漂移审计）；Windows 开发机跑全量门禁，cfg(unix) 专属代码经 WSL Ubuntu-24.04 `cargo check/test` 验证（克隆原生路径，`bash -ic`，文件 cp 单向同步）。

---

## 契约总表（测试作者唯一事实来源）

### C1. `control.rs`（cydrive-cli 新模块）

```rust
pub struct ControlServer { /* addr, join, shutdown */ }
pub const CONTROL_FILE_NAME: &str = "cydrive.control";
pub fn control_file_path(cfg: &CyDriveConfig) -> PathBuf   // db_path.parent()（None/空 → "."）join CONTROL_FILE_NAME
impl ControlServer {
    pub async fn bind(cfg: &CyDriveConfig) -> std::io::Result<Self>  // TcpListener::bind("127.0.0.1:0")；先删 stale 端口文件再写本次 addr（一行 "ip:port\n"）
    pub fn local_addr(&self) -> SocketAddr
    pub async fn run(self, shutdown: impl Fn() + Send + 'static) -> io::Result<()>  // accept 循环；读一行 trim：== "STOP" → 回写 "OK: shutting down\n"、关连接、触发 shutdown、循环继续（接受后续连接不再触发）；其他 → "ERR: unknown command\n"
}
pub async fn send_stop(addr: SocketAddr) -> io::Result<String>  // TcpStream::connect_timeout 3s；写 "STOP\n"；读至 EOF 返回响应行；连接失败原样返回 io::Error
pub fn read_control_addr(cfg: &CyDriveConfig) -> io::Result<SocketAddr>  // 读端口文件内容 trim 解析
```

- **安全边界（注释写明）**：只绑 127.0.0.1；同机攻击者本可 taskkill，回环无认证不新增攻击面；端口文件随 shutdown 删除（RunHandle 停机末尾）。
- 端口文件写入失败的语义：**warn 不致命**（run 照常，只是 stop 不可用）；绑定失败同样降级（控制通道可选组件）。

### C2. `cydrive stop` 子命令（main.rs Command 枚举 + handler）

- `Stop`：discover_config（同 cwd 规则，注释说明 stop 须与 run 同目录运行）→ `read_control_addr`：
  - 文件不存在 → `bail!("no running CyDrive instance found (no {} in the working directory)", CONTROL_FILE_NAME)`，exit 1；
  - `send_stop(addr)` Ok(resp) → 打印 resp + `"shutdown requested; draining uploads (may take a while for large in-flight files) ..."`，exit 0；
  - 连接被拒/超时 → 删 stale 端口文件 + `bail!("no instance responded at {addr}; removed the stale control file")`，exit 1。

### C3. 三源停机触发（cli lib + main.rs run()）

- lib 新增 `pub struct ShutdownWatch`（tokio::sync::watch 内部）：`new()` / `trigger(&self)` / `async fn wait(&self)`（多次触发幂等）。
- `run_with_transport` 尾部增加：绑定 ControlServer（降级语义见 C1）并把端口文件清理挂进 RunHandle 停机序列（WebDAV→WebUI→队列→inbound→**删端口文件**→卸载盘符，紧邻卸载之前）；ControlServer::run 的 shutdown 回调 = `ShutdownWatch::trigger`。
- main.rs `run()`：`ctrl_c()` 与（cfg(unix)）`sigterm()` 用 `tokio::select!` 并联 `watch.wait()`，任一先到即走既有 `handle.shutdown()`；打印来源（"Ctrl+C" / "SIGTERM" / "stop command"）。
- cfg(unix) sigterm 封装：`async fn sigterm() -> io::Result<()>`（tokio::signal::unix::signal(SignalKind::terminate())），放在 cli lib 的 `#[cfg(unix)]` 模块 + `#[cfg(not(unix))]` 提供同名 `pending()` 存根（永不解析）——select 写法跨平台统一。

### C4. platform Linux 挂载链（`crates/cydrive-platform`，镜像 windows 三段式）

- lib.rs 纯函数（全平台可测）：
  - `detect_mount_backend(gio: bool, davfs2: bool) -> Option<&'static str>`：gio 优先 → "gio"，其次 "davfs2"，皆无 → None（设计文档 :161 顺序）。
  - `gio_mount_command(url: &str, mount_point: &str) -> Vec<String>`：`["gio","mount","-d","webdav://<host-port>", ...]`——以设计文档/Python 基线实际命令为准（实现者读 E:\GitHub\CyDrive 对应源码取准确参数，plan 不锁死）；unmount `gio mount -u`。
  - `davfs_mount_command(url, mount_point) -> Vec<String>`：`["sudo","mount","-t","davfs",url,mount_point]`；unmount `["sudo","umount",mount_point]`。
- `linux.rs`（cfg(target_os = "linux")）：`mount_drive(mount_point, url) -> Result<String>` = which 探测（`Command::new("which")`）→ 选后端 → 执行命令 → 返回描述；`unmount_drive` 对应。`macos_stub.rs`/既有 not-windows 存根补 Unsupported（cfg 纪律：Linux 模块保证 Windows 侧可编译，反之亦然）。
- `run_with_transport` 的 auto-mount 分支：`cfg!(target_os = "linux")` 时走 `platform::linux::mount_drive("/mnt/cydrive" 或配置挂载点?——**不新增配置键**，YAGNI：沿用 `drive_letter` 字段语义不匹配 Linux；裁决：Linux auto_mount 仅当 `mount_point` 推导默认 `./cydrive_mount` 绝对化？**再简：本批不做 run 自动挂载接线，只交付 platform 能力 + 手动 `cydrive mount <path>`？**——见下「范围裁决」）。
- **范围裁决（计划内显式）**：本批 platform 层交付**纯函数 + cfg(unix) 真实现 + #[ignore] 真机测试**；`run` 的 Linux 自动挂载接线与配置键设计（挂载点、davfs2 凭据文件）**延后到下一批**（涉及 davfs2 secrets 配置与 sudo 语义，单独裁决）。CLI `mount`/`unmount` 子命令在 Linux 上接 platform::linux（`--path` 覆盖默认 `/mnt/cydrive`？——最小：mount 子命令 unix 分支用 `<mount_point 参数>`，Windows 分支不变）。

### C5. systemd 部署资产（`deploy/cydrive.service` + 同目录 README 一段）

- unit：`After=network-online.target`、`WorkingDirectory=/opt/cydrive`、`ExecStart=/opt/cydrive/cydrive run`、`EnvironmentFile=-/opt/cydrive/cydrive.env`（示例注释给 `CYDRIVE_BOT_TOKEN`/`CYDRIVE_PROXY_URL`——缺口②的无头凭据方案，env 优先级链已支持）、`TimeoutStopSec=600`（SIGTERM→排干大文件）、`Restart=on-failure`。**不需要 KillSignal 覆盖**（SIGTERM 已原生处理，这正是 T1 的意义）。
- 运行期产物不入库原则不受影响（deploy/ 是模板非产物）。

### C6. doctor 增补（轻）

- doctor 的 config 检查项后追加一条（纯逻辑可测）：当 `bot_token` 非空且**来源为文件/env**（即 keyring 不可用也不影响 is_configured）→ Ok；当 token 为空且 keyring 探测失败 → 给 Warn「headless 环境（服务/计划任务）下 keyring 不可用：把 token 写入 config.toml 或用 EnvironmentFile 注入 CYDRIVE_BOT_TOKEN」。实现需要 discover_config 暴露「token 来源」信息——**最小做法**：doctor 内自建 KeyringStore 可用性探测（get 一个不存在的键测试错误类型）+ cfg.bot_token.is_empty() 判断，不改 discover_config 签名。

---

## 任务序列（每任务红→绿→commit；T1–T3 为核心链，T4–T6 依赖递减）

### Task 1：ControlServer + stop 子命令（C1+C2）
红（新文件 `tests/control_channel.rs`）：`bind_writes_control_file`（bind 后文件存在且解析回 local_addr）、`stop_roundtrip_triggers_shutdown`（bind + run(shutdown 闭包置 AtomicBool) + send_stop → 响应 "OK: shutting down" 且 1s 内 AtomicBool==true）、`unknown_command_replies_err`、`stop_without_control_file_is_actionable_error`（临时 cwd 无文件 → cmd 逻辑函数返回 Err 含 "no running CyDrive instance"）、`stop_against_stale_file_reports_and_cleans`（文件写死端口 127.0.0.1:1 → send 失败路径 → 报错+文件被删）。绿：control.rs + main.rs Stop 臂 + stop_cmd。commit `feat(cli): control channel + cydrive stop`。

### Task 2：三源停机接线（C3）
红（`tests/run_e2e.rs` 追加 + lib 单测）：`shutdown_watch_multi_trigger_idempotent`（lib 单测：两次 trigger，wait 只解析一次语义正确）、`run_instance_stops_via_control_channel`（e2e：MockTransport 起 run_with_transport → 读端口文件 → send_stop → 限期内 WebDAV 端口拒连 + RunHandle 完成 + 端口文件已删）。sigterm 存根在 not(unix) 编译为 pending——用 `#[cfg(unix)]` 测试 `sigterm_helper_compiles`（占位，真验证在 WSL）。绿：ShutdownWatch + run_with_transport 接线 + run() select 重写。commit `feat(cli): unified shutdown (ctrl-c/sigterm/stop) with control server wired into run`。

### Task 3：SIGTERM（C3 unix 部分，WSL 验证）
实现 `#[cfg(unix)] sigterm()`（无独立红：信号注入测试不稳定，采用 seam+集成验证策略，decisions 记录）。验证 = WSL：同步 worktree → `cargo check -p cydrive-cli` → `cargo test -p cydrive-cli`（unix 分支编译并跑）→ 真机：起 `cydrive run`（Mock 不可用则用 doctor/`--help` 级别即可？**不**：WSL 无凭据，起 run 会失败——改为直接跑一个最小 bin 冒烟：`cargo run --example` 无——**用集成测试真发信号**：`#[cfg(unix)] #[ignore]` 测试 `sigterm_graceful_shutdown_e2e`：进程内起 run_with_transport（MockTransport），`libc::raise(SIGTERM)`（cli dev-deps 加 libc? 避免新依赖：`Command::new("kill").arg("-TERM").arg(id)`——std::process::id()），断言优雅停机完成。WSL `cargo test -- --ignored` 跑它）。commit `feat(cli): SIGTERM joins the shutdown sources (unix)`。

### Task 4：platform Linux 挂载链（C4）
红（`tests/platform.rs` 追加，纯函数全平台跑）：`detect_backend_prefers_gio`、`detect_backend_falls_back_davfs2`、`detect_backend_none`、`davfs_mount_command_shape`、`davfs_unmount_command_shape`、`gio_command_shape`（参数以 Python 基线核实后为准，测试钉构造结果）。绿：纯函数 + linux.rs（cfg target_os=linux）+ 存根纪律 + mount/unmount 子命令 unix 分支接线（Windows 分支零改动）。WSL：`cargo check -p cydrive-platform` + `cargo test -p cydrive-platform` + `#[ignore]` davfs2 真机测试（若 WSL 未装 davfs2：`apt install davfs2` 后跑；不可用则记录跳过原因）。commit `feat(platform): linux mount chain (gio/davfs2) pure logic + cfg(unix) impl`。

### Task 5：systemd 资产 + doctor 增补（C5+C6）
doctor 红测试：`doctor_warns_headless_keyring_with_empty_token`（token 空 + keyring 探测失败注入 → Warn 文案含 CYDRIVE_BOT_TOKEN）。绿：doctor 增补 + deploy/cydrive.service（+ 同文件内注释即文档，不另建 README）。commit `feat(cli,deploy): systemd unit + headless keyring doctor hint`。

### Task 6：全量门禁 + WSL 汇总 + 文档收口
Windows：fmt/clippy/test 全绿；WSL：`cargo check --workspace` + `cargo test -p cydrive-cli -p cydrive-platform`（含 Task3/4 ignored 项）。decisions.md：sigterm 测试策略裁决、Linux auto-mount 接线延后裁决、控制通道安全模型。AGENTS.md 阶段记录。commit `docs: service-lifecycle batch record`。

---

## 明确不做（YAGNI）

- run 的 Linux 自动挂载接线与挂载点/davfs2 凭据配置键（下一批，涉配置语义裁决）；
- 控制协议扩展（status/reload 等，只有 STOP）；
- Windows Service 包装（NT Service 仍不合适，本批 stop 通道已覆盖后台退出需求）；
- `cydrive stop` 跨机/跨 cwd 发现（同 cwd 约定 + 报错文案指明）；
- macos 挂载真实现（存根 Unsupported，与既有 windows_stub 同纪律）。

## 风险与回滚

- run_with_transport 增加可选组件（控制通道），失败一律降级 warn 不影响服务启动——e2e 既有用例验证无回归；
- 端口文件是运行期产物（.gitignore 追加 `cydrive.control`）；
- 整批隔离在 feat/service-lifecycle 分支，回滚=废弃分支。
