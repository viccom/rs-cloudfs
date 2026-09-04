# status 子命令 + Linux 自动挂载 实施计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** ①`cydrive status` 跨平台查询实例/端口/挂载状态（控制协议加 PING）；②Linux `run` 自动挂载（mount_point 配置键 + 非交互挂载 + stale 清理 + 停机卸载）。负责人已批两项设计（2026-09-04）。

**Architecture:** status = 数据收集函数（可测）+ 纯渲染函数 + main 薄壳；平台挂载检测 = 纯解析函数（net use / /proc/mounts）+ cfg 薄壳。自动挂载复用 platform::linux 既有 mount/unmount，挂载点取 `mount_point` 键或 `$HOME/CyDrive`；非交互（命令不带 sudo，失败仅 warn——root/setuid 场景直接工作）。

**工作目录：** worktree `E:\GitHub\rs-CyDrive-t3`（分支 `feat/status-and-automount`，自 main `e8dd194`）。TDD 纪律与前批一致（红→绿→断言漂移审计）。

---

## 契约

### C1 控制协议 PING（cli control.rs）
- `run()` 协议增：读入 `PING` → 回写 `OK: cydrive {CARGO_PKG_VERSION_OF_CYDRIVE_CLI}\n`（`env!("CARGO_PKG_VERSION")`），**不**触发 shutdown；STOP 语义不变；未知命令回 ERR 不变。
- `pub async fn send_ping(addr) -> io::Result<String>`（与 send_stop 共享读写骨架，抽私有 helper）。
- 红测试（tests/control_channel.rs 追加）：`ping_replies_version_without_stopping`（PING → 响应含 "OK: cydrive"；AtomicBool 未置位；随后 STOP 仍触发）。

### C2 平台挂载状态解析（platform lib.rs 纯函数 + 薄壳）
- `pub fn parse_net_use_mapping(output: &str, url: &str) -> Option<String>`：net use 文本中找含 url 的行，取该行中的单字母盘符 token（`Y:`/`Z:` 形态）→ Some("Y:")；无 → None。
- `pub fn parse_proc_mounts_davfs(output: &str, url: &str) -> Option<String>`：`/proc/mounts`/`mount` 文本中找 `<mountpoint> <url> fuse` 行 → Some(mountpoint)；无 → None。
- 薄壳 `pub fn current_mount_for(url: &str) -> Option<String>`：windows 跑 `net use` 调 parse；target_os=linux 读 `/proc/mounts` 调 parse；其余平台 None。cfg 纪律照旧（现有 stub 模式）。
- 红测试（tests/platform.rs 追加，纯函数全平台跑）：`parse_net_use_finds_letter_for_url`（含正常/多映射/无匹配三断言）、`parse_proc_mounts_finds_mountpoint`（含 davfs 行匹配、无关 fuse 行不误配、无匹配）。

### C3 `cydrive status` 子命令（cli lib.rs + main.rs）
- lib：`pub struct StatusReport { pub instance: Option<String>, pub control: Option<String>, pub webdav: Option<String>, pub dashboard: Option<String>, pub mount: Option<String> }`（字段语义：instance=Some(版本串) 运行中；control=Some(addr)/stale 描述；webdav/dashboard=Some(url) 监听中；mount=Some("Y:"/"/root/CyDrive") 已挂载）。
- `pub async fn collect_status(cfg: &CyDriveConfig) -> StatusReport`：control_file+send_ping（ping 成功→instance=Some(resp trim)；文件在 ping 失败→instance=None+control=Some("stale")；无文件→control=None）；webdav/dashboard TcpStream connect_timeout 1s 探测（dashboard 仅 enable_web_ui）；mount=platform::current_mount_for(&default_mount_url(cfg))。
- `pub fn render_status(r: &StatusReport, webdav_url: &str, dash_url: Option<String>) -> String`（纯函数，行格式钉死）：
```
instance:   running (OK: cydrive 0.3.0)        | instance:   not running
control:    127.0.0.1:38869                    | (无文件时省略 control 行；stale: control:  127.0.0.1:1 (stale))
webdav:     http://127.0.0.1:8289 listening    | webdav:     http://127.0.0.1:8289 not reachable
dashboard:  http://127.0.0.1:8288 listening    | dashboard:  disabled
mount:      Y: -> http://127.0.0.1:8289        | mount:      not mounted
```
- main.rs：`Status` 子命令（无参数）→ discover_config → collect → println!(render)。doc 注明与 run 同 cwd 才能探测实例。
- 红测试（新 tests/status.rs）：`collect_status_full_picture`（tempdir cfg + 起 ControlServer + 占 webdav/dashboard 端口的 TcpListener → collect 断言五字段）+ `collect_status_all_down`（无文件无监听 → instance None/control None/webdav None/mount None）+ `render_status_format_pinned`（构造 StatusReport 钉整段输出字面量）。

### C4 `mount_point` 配置键（core config.rs）
- `pub mount_point: Option<String>`（toml `mount_point`，serde 默认 None）；KNOWN_TOML_KEYS 追加；LEGACY_REJECTED_KEYS 追加（legacy json 拒收）；validate：Some 时必须以 `/` 开头（绝对路径；doc 注明 Linux 挂载语义，Windows 忽略不报错）。
- 既有 `toml_roundtrip_preserves_full_config` 穷举字面量需机械补 `mount_point: None`（授权的机械改动）。
- 红测试（tests/config.rs 追加）：`toml_mount_point_parses_and_defaults`（缺省 None；显式 "/mnt/cydrive" 解析一致）、`toml_mount_point_requires_absolute`（"relative/path" → Invalid）、`legacy_json_rejects_mount_point`。

### C5 Linux 自动挂载 + 停机卸载（cli lib.rs + platform linux.rs）
- 纯函数 `pub fn auto_mount_target(cfg: &CyDriveConfig, home: &Path) -> Option<PathBuf>`（platform 或 cli lib，放 platform lib.rs 更贴：cfg.auto_mount_drive 为 false → None；否则 cfg.mount_point 绝对路径化 或 default_mount_point(home)）。
- platform linux.rs 增 `pub fn unmount_stale_for(url: &str)`：parse_proc_mounts_davfs 找指向 url 的挂载 → `umount`（输出吞掉，失败静默——幂等清理）。
- cli `mount_if_configured` 增 `#[cfg(unix)]` 分支：target=auto_mount_target → unmount_stale_for(url) → linux::mount_drive(target, url)；失败 tracing::warn + println 提示手动（镜像 Windows 语义：服务继续）。RunHandle 停机链 unix 分支：unmount_drive(target)（not-mounted/EBUSY 均 warn 静默）。仅当 cfg.auto_mount_drive 时参与。
- 红测试：platform 纯函数 `auto_mount_target_respects_flag_and_key`（false→None；true+key→key；true 无 key→$HOME/CyDrive）；`#[cfg(unix)] #[ignore]` 真机 e2e `ignored_unix_automount_roundtrip`（WSL：MockTransport? 不行——run_with_transport 的挂载在 run 流程里；改测 mount_if_configured 等价物？简化：ignored 测试直接调 platform::linux::mount_drive+unmount_drive 往返（davfs2 已验证过）+ auto_mount_target 组合断言——按实现者判断写最有价值形态，#[ignore] 前置说明 WSL+davfs2+8080 服务）。
- Windows 侧：既有 run_e2e 零回归（unix 分支不参与编译）。

---

## 任务
- T1：C1+C2+C3（status 全链）红→绿→commit。
- T2：C4（mount_point 键）红→绿→commit。
- T3：C5（自动挂载）红→绿→commit；WSL 编译+ignored 项验证。
- T4：双平台门禁（win fmt/clippy/test 全量；wsl check+cli/platform test）+ decisions/AGENTS 入档。

## 不做（YAGNI）
- status 不显示 pid/uptime（控制协议最小扩展只有 PING）；不做 JSON 输出；
- 自动挂载不做 sudo/NOPASSWD 自动配置、不做 systemd mount unit 生成；
- Windows 侧 mount 状态只报「盘符→URL」，不校验盘符归属细节（解析 net use 即可）。
