# ck-webdav 复审 findings（2026-09-25，fix/webdav-review）

> 审查形态：主会话精读生产码全量（lib/client/driver/stager/transport_face）+ 主会话直做测试面/桩面（子代理 A 两次均被 API 限流终止——`no healthy account available`；write_path/stub/davref/conformance 全文精读，read_path/connect_auth 覆盖面核对）+ 子代理 B 重派在飞（装配面）。
> 既往批次复核：K80-K82 + K86 审查批（H1+M1-M15）+ 反向复核批——抽查确认修复钉仍在（K75-1 412 复核臂 / WD5 probe 认证复核腿 / M1 nc 保留 / M3 读取封顶 / M6 并置 challenge / M7 成员失败映射 / M4 0 字节全链）。

## High（1）

### H1【生产码】transport 面缺 `upload_stream` + `store_bytes` 裸 Drop（sftp ① 判例同型）
- **位置**：`crates/drivers/ck-webdav/src/transport_face.rs:196-214`（store_bytes）；`upload_stream` 方法**不存在**
- **缺陷**：① `store_bytes` 的 `stager.write(data).await?` 错误早退 → stager 裸 Drop——webdav 的 stash 在 writer 打开时已执行（driver.rs:763-781），裸 Drop 后旧版本困在 `.ckwd-*.old`、final 空缺（与 sftp ① 同源后果：覆盖写失败重试耗尽 → 文件对卷消失）。② `upload_stream` 未实现——L3 上传队列的流式路径对 webdav 不可达（集成面缺口；上层若只走 `upload` 整文件路径则无运行时影响）。
- **修法**：sftp 判例同款——`store_bytes` 错误路径显式 `stager.abort()`；补 `upload_stream`（`write_frames` 帧泵 + abort-on-error，sftp transport_face 逐字同型）。
- **触发概率**：写错误/流错误在传输中断时真实可达（与 sftp ① 同判）。

## Medium（2）

### M1【生产码】rename 缺父重试臂把「重试再撞 412」压成 `NotFound`
- **位置**：`crates/drivers/ck-webdav/src/driver.rs:646-649`
- **缺陷**：`ParentSuspect` → 建父 → 重试恰一次的重试臂 match 只分 `Done` 与「其余一律 `NotFound`」——重试若撞 412（Overwrite:F 撞既有目标：建父窗内目标被并发创建）会被压成 `NotFound`（父），而首发 412 走「重 stat 复核后 Exists」（driver.rs:613-626）。同一条 rename 路径上 412 的首发与重试后分类不一致。
- **影响**：并发 rename 竞态窗的错误分类误导（报「父缺」实为「目标被占」）。
- **修法**：重试臂的 `PreconditionFailed` 单独走首发同款复核臂。

### M2【测试基建】lost-ACK 旋钮「头先写、body 即断」对写动词恒隐形——两条 lost-ACK 测试的覆盖是时序彩票（flake 2 结构性根因）
- **位置**：`crates/drivers/ck-webdav/tests/stub/mod.rs:1894-1912`（`killed_response` + `KillStream`）；消费面 `client.rs:700-706`（put）/ `:830-846`（move_）
- **缺陷**：`killed_response()` 返回**完整 HTTP 200 响应头** + 首 poll 即错的 body。对 PUT/MOVE（ACK = 状态行本身），客户端 `send()` 在头到达即成功 → `status.is_success()` → `drain_bounded` 吞掉 body 错误 → **按成功返回（`Ok(())` / `MoveOutcome::Done`）**。即：常见时序下「连接杀」对写动词完全不可见——`stager_lost_ack_on_move_resumes_as_committed` 与 `stager_lost_ack_on_put_recovers_via_part_recheck` 声称钉死的恢复腿**只在「RST 抢在客户端读头之前」的少数时序才被执行**（TCP RST 会丢弃客户端内核缓冲里未读的头字节 → send 层传输错误 → 恢复路径才运行）。两条测试的覆盖是 TCP 时序彩票；K86 压测观察到的偶发失败与此同源（两条代码路径随时序随机切换；恢复腿运行时的额外传输毛刺——死连接池重用 → 重试退避——在高负载下放大波动）。
- **附带**：头到达时序下「效果已落 + 200 头确认」被驱动按成功处理**语义上是对的**（头即 ACK）——缺陷在桩的建模（要模拟 lost-ACK 必须在头写出前杀连接），不在驱动。
- **修法方向**：lost-ACK 旋钮改「效果已落 → 连接在**头写出前**终止」——axum fallback 面表达不了「头前杀」，需照 `davref/mod.rs` 的裸 hyper accept-loop 形态给桩加连接级控制（效果执行后直接 drop TcpStream，不写任何响应字节）；或把 lost-ACK 注入改到驱动可注入缝。**修前先用压测循环实证 K86 观察到的致命路径**（修后恢复腿变确定性，flake 应随之消失）。
- **另一条 flake（`put_insufficient_storage...` 507 重传腿）**：主会话静态读未能钉死致命机制——507 零效果 + 恢复 DELETE 404 + 重传全链逐帧核对均确定性；不排除与 M2 同类（桩内状态 × hyper 池行为的负载敏感交互）。**建议与 M2 同批做压测复现**（`cargo test -p ck-webdav --test write_path -- --test-threads=1` 循环 N 次带 RUST_BACKTRACE）。

## Low（2）

### L1【生产码】list 的 `dir.join` 防御与 `is_addressable_name` 过滤冗余
- **位置**：`crates/drivers/ck-webdav/src/driver.rs:458-471`
- **现状**：注释自述「防御——Depth 1 的子名已无 `/`」。零行为缺陷，不修。

### L2【生产码】stager close 的 stat 复核失败臂不恢复现场——「数据不可破坏优先」裁决在案但与 sftp 相反
- **位置**：`crates/drivers/ck-webdav/src/stager.rs:311-318`（注释自述 H1 裁决）
- **现状**：提交固化后 stat 复核失败 → 只上抛、stash 留 `.ckwd-` 残件（restore 会以 Overwrite:T 把旧对象盖回已提交新版 = 数据破坏）。sftp 本轮 H1 的裁决相反（恢复现场优先——先清被占 final 再复位 stash）。两者各自自洽：webdav 保「已提交的新版不可破坏」，sftp 保「旧版本不丢」；语义差异的根源是 Overwrite:T 语义下 webdav 无法「清 final 再复位」而不破坏新版。
- **建议**：不修代码；在 decisions.md 记一笔双驱动恢复哲学对照（已在 sftp-review-findings 交叉引用）。

## Info（2）

### I1 transport capabilities 镜像无显式 remote_delete 覆盖
- `driver.capabilities()` 已声明 `remote_delete: true`（driver.rs:431），行为等价；与 sftp 的显式覆盖写法差异而已。

### I2 K82 挂账维持
- rclone 目录 MOVE 缓存窗（真机形态）/ TLS 自签腿（需真机窗口）/ Nextcloud X-OC-Mtime（无实例未实证）——挂账不动。

## 装配/集成面（子代理 B，主会话已复核修正）

### M3【core】webdav_url 校验**四臂全部**回显原文——userinfo 内嵌凭据形态下密码进错误链
- **位置**：`crates/cloudkit-core/src/config.rs:2192`（scheme 臂）/`:2199`（query/fragment 臂）/`:2211`（userinfo 臂）/`:2218`（host 臂）——四臂文案均带 `got {url:?}`
- **缺陷**：`webdav_url = "https://user:pass@host/dav?x=1"`（凭据嵌 URL 的误用形态）命中任一臂都把**含密码的原文**写进 `ConfigError::Invalid`——该错误不经 `redact_credential_values` 漏斗（只挂 Parse 构造点），`webdav_url` 不在 SECRET_VALUED_KEYS（合理）→ 凭据随 boot 错误 / 控制通道 ADD 回复 / tracing 落日志（R3 面）。driver 侧同函数已有 M4 裁决「拒收文案不回显原文」+ 测试钉（ck-webdav lib.rs:352-377）——**core 侧是同纪律的完整漏网半边**。
- **复核修正（主会话）**：子代理 B 原报「userinfo 臂是唯一不回显的臂」**有误**—— userinfo 臂（2211）同样回显；缺陷比原报更宽（不存在「调顺序即修」的捷径，B 建议的修法②不完整）。
- **修法**：四臂统一去 `{url:?}`（driver M4 同款文案形态），core validate 面补「不回显」断言（driver lib.rs:352-377 同款正反钉）。**同族顺手项**：`sync_url` 两臂同款回显（config.rs:1997/2009）——修复批顺手核对 sync_url 的 userinfo 漏斗并同治。

### L3【装配面】core validate 与 driver parse 的「纯空白值」语义分叉
- **位置**：core config.rs:2222-2236（`!v.trim().is_empty()`）vs driver config.rs:155-156（`!v.is_empty()` 不 trim）——主会话已核行号
- **现状**：`webdav_username = "  "` core 判 anonymous 放行、driver 报 pair 错误——方向安全（driver 恒更严），一致性瑕疵。修法：driver `non_empty` 改 trim 语义（与「空白=未设置」文档语义对齐）。

### L4【装配面】doctor 把「配置不完整」渲染成「不可达」
- **位置**：cli lib.rs:6349-6359（`webdav_backend_probe` 配置错误腿 → `WebdavProbe::Unreachable`）——主会话已核行号；渲染 doctor.rs:1145-1152
- **现状**：配置错误混入网络故障态（baidu 有专门 NeedsReauth 态先例）；detail 文案可行动故仅 Low。修法：新增 `Misconfigured` 变体或独立渲染。

### I3 web 表单跨凭据组残留键（既有 P3 形态，非 webdav 特有）——备查
### I4 `webdav_host`/`webdav_port`（进程级 dav-server 监听键）与六卷键前缀并存——无缺陷，排障提示

### 装配面合规结论（B 十维度逐项）
K28 env 纪律 / dispatch 三臂 + K31 文案 / conformance 双桩接入 / as_driver 探针 / doctor 五态（WD5 修复在位）/ web 表单 i18n / feature 裁剪组合测试 / R3 零泄漏（WebdavParams 不派生 Debug 保持）/ sync namespace 离线推导——全部合规；driver-onboarding §8 验收清单逐项全过。历史教训（M-I1/M-I2/B-M1/K28）均有 webdav 等价面测试钉。

## 总评

**测试面置信度 8.5/10**（上限被 M2 的时序彩票压住）。强项：手搓桩的 RFC 建模严格（Digest 服务端真实验证/response-uri 逐字节比对/nc 高水位/apache 多前缀 multistatus/SlashStrict 全动词 301）；双桩制的 davref 真实现腿 + 断言①裁决史在案；read_path 20 腿 + connect_auth 20 腿覆盖面完整（含 M1/M3/M6/M7/M9-M13 全部修复钉）。最大盲区 = M2（写动词连接杀的建模缺陷让 lost-ACK 覆盖时序化）。
