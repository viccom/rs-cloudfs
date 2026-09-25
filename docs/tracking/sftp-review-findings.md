# ck-sftp 复审 findings（2026-09-25，fix/sftp-review）

> 审查形态：主会话精读生产码（client/driver/transport_face/error/config）+ 子代理 A（测试面+桩面）+ 子代理 B（装配/集成面）。所有 H/M 发现均经主会话 file:line 级复核。
> K67/K74 历史批次已清偿项不重复计入；K67.5 挂账逐项复核。

## High（2）

### H1【生产码】close 硬仗②大小校验失败不恢复现场——`.old` stash 永久遗留
- **位置**：`crates/drivers/ck-sftp/src/driver.rs:654-660`
- **缺陷**：`close` 的五步链（hinted 校验 → flush → awaited close → rename 固化 → 远端大小校验）中，前四步失败都走 `restore_scene`（删 part + stash→final 复位），唯独**硬仗②大小校验失败分支**（`remote_size != self.written` → `Err(Io)`）直接返回，**不恢复现场**。
- **影响**：覆盖写场景下旧版本已被 stash 成 `.old`，此分支失败后：① 旧版本对用户从此不可见（`.old` 被 list 过滤）；② 目标位置是一个**内容错误的新版本**（大小不符）；③ 用户唯一的数据恢复途径是手工登录服务器找 `.cksftp-*-*.old` 残件——**数据丢失级**。
- **修法**：该分支补 `self.restore_scene().await;`（stash→final 复位 + 删 part），与其他失败路径同语义。
- **触发概率**：低（rename 落位 + close OK 后大小突变的场景几乎只剩并发外部改动），但一旦发生就是数据丢失，故定 High。

### H2【桩模型】rename 目录迁移漏搬 symlink 表——symlink×rename 组合零覆盖的桩侧根因
- **位置**：`crates/drivers/ck-sftp/tests/stub/mod.rs:755-786`
- **缺陷**：`rename` 的目录迁移分支只搬 `vfs.dirs` 与 `vfs.files` 两张表，`vfs.symlinks` **零触及**——rename `/old`→`/new` 后，`/old/inner/link` 的 symlink 键仍挂旧前缀，`children_of("/new/inner")` 不再列出它，`remove("/new/inner/link")` 也 remove 不到（键是 `/old/inner/link`）。
- **影响**：K67.5 挂账的「symlink×rename 组合」零测试的**根因**不只是缺测试——桩模型本身缺陷，生产码若在 rename 目录时对 symlink 做处理，桩无法回放真机形态；且若生产码在递归删除时错误处理子树 symlink，桩会掩盖。
- **修法**：`stub/mod.rs:755-786` 的目录迁移分支同步迁移 `vfs.symlinks` 中前缀匹配的键（值不变）；补测试「rename 目录后子树内 symlink 仍可见、lstat 指向原目标」。
- **证据**：`stub/mod.rs:755-786` 只有 `moved_dirs` 与 `moved_files` 两个循环；全文搜索 rename 测试无任何 symlink-in-subtree 场景。

## Medium（4）

### M1【生产码】无 keepalive——长 idle 后首个操作必吃一次失败重试
- **位置**：`crates/drivers/ck-sftp/src/client.rs:320`（`client::Config::default()` 未设 keepalive）
- **缺陷**：`SshConnection.handle` 有 `#[allow(dead_code)]` 注释「保活任务 SF4 真机批议」——SF4 未做。NAT/防火墙 kill idle 连接后，下一个操作先撞死连接 → `Unavailable` → `with_retry` 清槽重连。
- **影响**：每次 idle 超 NAT 超时（典型 60-300s）后的首个操作必多一次失败往返；用户可见表现为「隔几分钟第一次操作慢一下」。
- **修法**：`client::Config { keepalive_interval: Some(Duration::from_secs(30)), ..Default::default() }`（russh 支持）。

### M2【生产码】`with_retry` 并发时序——第二个并发失败会清掉第一个刚重建的连接
- **位置**：`crates/drivers/ck-sftp/src/client.rs:52-67` + `:145-147`
- **缺陷**：两个并发请求同时撞死连接 → 都 `invalidate` → 都重连。若 A 先完成重连并继续执行，B 的 `invalidate` 会把 A 刚建的连接清掉，B 再重建——连接被无谓重建两次。
- **影响**：并发场景下连接被重复重建（性能税），无正确性问题（guard 语义保证在途操作拿到的是有效连接）。D3 单连接纪律下并发度低。
- **修法**：`invalidate` 改为「仅当槽位是死连接才清」（需要连接代际标记）；或接受现状（影响有界）。

### M3【桩】`fail_next_stat` 同时被 stat 与 lstat 消费——注入面比声明更宽
- **位置**：`crates/drivers/ck-sftp/tests/stub/mod.rs:493-495`（stat）与 `:509-511`（lstat）
- **缺陷**：文档声明注入面是「下一次 stat」，但 lstat 处理器同样消费 `fail_next_stat`。conformance 断言⑤调用 `inject_backend_error` 后，harness 触发的 stat 若内部实现为 lstat，注入会被 lstat 消费、断言⑤落空成假绿。
- **影响**：注入语义与文档不符；未来驱动把 stat 实现改为 lstat 时 conformance 断言⑤会无声失效。
- **修法**：拆为 `fail_next_stat`（仅 stat 消费）与 `fail_next_lstat`（仅 lstat 消费），或 lstat 消费时 panic 提示「注入面是 stat 不是 lstat」。

### M4【桩】`rename` 不清空目标已存在时源是 symlink 的形态——`Failure` 判定漏看 symlink 表
- **位置**：`crates/drivers/ck-sftp/tests/stub/mod.rs:747-749`
- **缺陷**：`rename` 的目标存在判定只查 `vfs.files` 与 `vfs.dirs`，不查 `vfs.symlinks`——若目标路径已存在一个 symlink，真实 OpenSSH 上 rename 会被拒（目标存在），桩却放行并写入与 symlink 同键的位置（两表各存一份，状态错乱）。
- **影响**：rename 撞已存在 symlink 目标的生产码行为（应归一为 Exists）无桩覆盖；桩自身进入不一致状态。
- **修法**：`stub/mod.rs:747` 的判定补 `|| vfs.symlinks.contains_key(&newpath)`；补测试「rename 目标已存在 symlink → Exists」。

## Low（5）

### L1【生产码】`list` 全量拉取后内存分页——SFTP 协议限制，非实现缺陷
- **位置**：`crates/drivers/ck-sftp/src/driver.rs:256-314`
- **现状**：SFTP 协议无服务端排序/分页，客户端只能全量拉。与 local 同款「伪分页」。**裁决建议：不修（协议约束）**。

### L2【生产码】reader 的 early-EOF 分支在 close 失败时合并诊断
- **位置**：`crates/drivers/ck-sftp/src/driver.rs:449-459`
- **现状**：诊断信息合并，无行为错误。**不修**。

### L3【桩】`simulate_lost_ack_rename` 的 1s 轮询窗在慢 CI 上可能假阴性
- **位置**：`crates/drivers/ck-sftp/tests/stub/mod.rs:1013-1043`
- **缺陷**：等待在途写落服的预算固定 1s（500 次 × 2ms），CI 高负载下可能超时返回 false、测试 panic。
- **修法**：预算提升到 5s（2500 次）或 panic 信息补「若 CI 高负载请重跑」。

### L4【桩】`hidden` 竞态注入在 rename 到达时 `clear()` 全表——多路径并发注入会被一次性清空
- **位置**：`crates/drivers/ck-sftp/tests/stub/mod.rs:740-742`
- **现状**：单测试串行下无影响，注入面语义粗糙。
- **修法**：改为 `vfs.hidden.remove(&newpath)` + `remove(&oldpath)`，或文档声明「clear 全表是刻意简化」。

### L5【CI】features 矩阵缺 sftp/pan115/pan123/webdav 单驱动腿
- **位置**：`.github/workflows/ci.yml:117-126`
- **现状**：K30 矩阵仅含 `none / telegram / baidu / local` 四条腿，新驱动的单驱动 clippy 腿缺位。
- **修法**：ci.yml features matrix 追加四条腿，或更新 driver-onboarding §1 文字说明新驱动组合由本地命令而非 CI 验收。

## Info（3）

### I1 `staging_names` 的进程级序号在进程重启后从 0 重置
- pid + seq 命名，进程重启后 pid 变了，`(pid, seq)` 对仍唯一——无缺陷，设计自洽。

### I2 `SftpStager::drop` 的 `close_nowait` 兜底不等待确认
- 模块文档已声明「三硬仗①的异常路径边界」——async Rust 固有约束，非实现缺陷。

### I3 任务描述键名漂移
- 任务描述的 `sftp_user/sftp_key_path/sftp_key_passphrase/sftp_fingerprint` 与实际键名 `sftp_username/sftp_private_key_path/sftp_private_key_passphrase/sftp_host_fingerprint` 不符——代码自身一致，描述过时，不影响装配面。

## K67.5 挂账复核

| 挂账项 | 现状 |
|---|---|
| early-EOF 桩不可达 | **仍存在**——`readdir` 的 EOF 只在「取尽后下一轮」返回（`stub/mod.rs:662-664`），没有「首轮回 EOF 但句柄内还有 entries」的 early-EOF 注入面；真实服务器可能在任何一轮返回 EOF（协议允许） |
| quota 真值零覆盖 | **仍存在**——桩 `init` 不声明任何扩展（`stub/mod.rs:467-475`），statvfs 声明后 total/used 的真值映射零覆盖 |
| symlink×rename 组合 | **根因坐实为桩模型缺陷**（H2）——不只是缺测试，是桩的 rename 漏搬 symlink 表 |
| 真机断连腿 | **名不副实**——`live_matrix.rs:304-340` 腿④宣称「断线重连」，实际只测会话复用（函数体注释自承「断开方式不现实」）；真机传输层死（TCP RST/NAT 超时）的 with_retry 行为零真机覆盖 |

## 修复批销账（2026-09-25，负责人批准范围 H1+H2+M1+M3+M4；M2 与 L 级挂账留裁决）

| 项 | 状态 | 修法与证据 |
|---|---|---|
| H1 | **已修**（`fix/sftp-review`） | `driver.rs` close：大小校验失败分支 + rename-否臂改走新 `restore_scene_after_commit`（先清出被占 final 再常规恢复）；`restore_scene` 删除「复位失败即删 stash」兜底（违反模块自身「恢复失败只留不可见残件」契约的数据丢失面）。红：`close_size_mismatch_restores_the_previous_version`（short-write 注入，修复前 final=嫌疑版本 + .old 遗留）→ 绿。 |
| H2 | **已修** | 桩 rename 目录迁移补 `moved_links`（symlinks 表随子树前缀重写）+ 链接本体可作 rename 源。红证（临时回退修复重放）：`rename_directory_subtree_carries_symlinks` → `the symlink must be visible under the new prefix: NotFound` → 恢复修复后绿。 |
| M1 | **已修** | `client.rs` 新 `ssh_transport_config()`：`keepalive_interval = 30s`（russh 0.63.3 `client::Config` 字段核实于 registry 源码）。接线钉测 `ssh_transport_config_pins_keepalive`。 |
| M3 | **已修（并在执行期揭出更深真相）** | 拆分为 `fail_next_stat` / `fail_next_lstat` 双旋钮。**执行期发现：conformance ⑤ 此前就是「假绿」现行犯**——K67 已把 driver.stat 非根实现为 symlink_metadata（SSH_FXP_LSTAT），旧桩靠 lstat 处理器偷吃 stat 槽位让 ⑤ 通过；拆分后 ⑤ 立即翻红，正确收口 = harness 注入改打 `fail_next_lstat`（driver.stat 的真实动词）。新钉测 `stat_and_lstat_injection_knobs_are_independent`（双旋钮互不越界）。 |
| M4 | **已修**（含生产一行） | 桩 rename 目标存在判定补 symlinks 表 + 链接本体 rename 支持；**生产侧** driver.rename 的目标预检与竞态臂从跟随 stat 改 **lstat**（悬空链也是既有目录项——修复前悬空链目标在桩修复后 surfaced 为 Io，契约要求 Exists）。红：`rename_onto_dangling_symlink_reports_exists`（桩修复前假成功→桩修复后 Io→生产修复后 Exists）+ `rename_onto_live_symlink_reports_exists`（契约钉恒绿）。 |

**验证**：ck-sftp 全套 **74 passed / 0 failed / 15 ignored**（write_path 25→30、conformance 3、connect_auth 9、read_path 15、lib 17）+ clippy `-D warnings` + fmt 绿 + workspace 全量 **1842/0/63**（190 套件；1836 基线 + 6 新测试）。修复 commit = `74c2625`。

**执行期新发现（未修，留裁决）**：
1. **[M] writer 中途 Drop 不复位 stash**——`SftpStager::drop` 只 `file.take()`；覆盖写场景下消费者不调 close/abort 直接 Drop 会让 final 停在「被 stash 丢空」状态（旧版本困在不可见 .old）。ck-local 的 Drop 有同步恢复面，async 面做不了——需要消费侧契约（upload queue 必调 abort）或会话级清扫（ck-local sweep 同源）。挂账待裁决。
2. **[L] mkdir 撞悬空 symlink 目标**——`mkdir` 的预检/竞态臂仍走跟随 stat，悬空链目标 surfacing 为 Io 而非 Exists（与 M4 同族，rename 已修 mkdir 未动）。
3. **[L] rename 源为悬空 symlink**——源预检走跟随 stat，悬空链源报 NotFound（POSIX 下 rename 悬空链合法）。
4. **[L] 桩 `add_symlink` 对文件目标会向 dirs 表插入文件路径**（目标自动建目录逻辑不分文件/目录）——现无测试触发（既有用例全为目录目标），潜在桩状态污染。

## 二轮修复批销账（2026-09-25，负责人指令「①修；②③④零破坏低代价则修」——四项全修）

| 项 | 状态 | 修法与证据 |
|---|---|---|
| ① 裸 Drop 困旧版 | **已修（生产可达已核实）** | `transport_face.rs`：`store_bytes` 与 `upload_stream` 的错误路径显式 `stager.abort().await` 再上抛（帧泵拆 `write_frames` 自由函数使借用成立）。**可达性核实**：两函数的 `?` 早退 = 裸 Drop 点（覆盖写失败重试耗尽 → 文件对卷消失，旧版本困 `.old`）。红：`transport_stream_error_restores_the_stashed_old_version`（流中帧错误注入，红 = final 空 + 残件遗留）→ 绿。`transport_upload_write_error_restores_the_stashed_old_version` 转为契约钉（russh-sftp 写缓冲到 flush 才落桩——write 注入失败走 close 内部恢复面，非裸 Drop 路径；两腿断言同形）。桩新增 `fail_next_write` 一次性旋钮。 |
| ② mkdir 撞悬空链 | **已修** | `driver.rs` mkdir 预检与竞态臂从跟随 stat 改 **lstat**（与 M4 rename 同面）。红：`mkdir_onto_dangling_symlink_reports_exists`（修复前假成功+桩污染）→ 绿（Exists + 链原位 + 无目录覆写）。 |
| ③ 悬空链源 rename | **已修** | `driver.rs` rename 源预检改 **lstat**（POSIX：rename 搬链接本体不解析目标；delete/recursive_remove 的 K67 lstat 契约同源）。红：`rename_dangling_symlink_source_succeeds`（修复前 NotFound）→ 绿（链接本体随迁、目标串不变）。 |
| ④ 桩 add_symlink 污染 | **已修** | 目标自动建目录跳过已被文件/链接占据的组件。红证经回退重放捕获（`the file target must not be registered as a dir`）；**执行期陷阱记录**：红批后 `mv .bak` 恢复保留旧 mtime，cargo 判定文件未变跳过重编跑了回退版旧二进制——假红一例；`touch` 强制重编后绿。 |

**验证**：ck-sftp **79/0/15**（write_path 30→35）+ clippy `-D warnings` + fmt 绿 + workspace 全量 **1847/0/63**（1842 基线 + 5 新测试）。

## 装配/集成面（子代理 B）——全绿

K28 env 纪律、K30 三件套、D2 指纹显式接受、conformance 八断言接入、read-through 探针（as_driver + authoritative_index）、web 表单回填（K67 port 修复仍在）、R3 凭据红线（SftpParams 不派生 Debug、日志零凭据值）七个维度全部合规。唯一 Low = CI features 矩阵缺新驱动腿（L5）。

## 总评

**生产码质量高**：错误映射分层清晰（三函数 + 单测逐条钉死）、三硬仗纪律全部落地且有测试钉、commit-on-close 与 ck-local 同构且重放窗已修（K67）、symlink 语义 lstat/stat 分工明确且根例外已修。**H1 是唯一的数据丢失级缺陷**——五行修复。

**测试面置信度 7.5/10**：桩的 OpenSSH 语义对齐声明写得诚实且具体，但 H2 暴露了「桩照实现抄」的实质案例——桩的 rename 只搬了实现里有的两张表（dirs/files），漏了第三张（symlinks）。open 句柄快照隔离（比真机更强）未在差异声明中记录。

**建议优先级**：H1（五行修）→ H2（桩修 + 测试）→ M1（keepalive 一行配）→ M2（裁决：修或接受）→ M3/M4（桩注入面收窄）→ L3/L4/L5（顺手或挂账）。
