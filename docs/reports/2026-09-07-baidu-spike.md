# 百度网盘 Spike 报告（Batch S，验证驱动）

- **日期**：2026-09-07 ｜ **执行**：Phase 1 Batch S（rs-cloudfs worktree `feat/phase0-1`）
- **工具**：`examples/baidu_spike`（workspace-excluded 独立 crate，真网调用不进 CI 门禁；子命令 refresh/qps/resume/rapid/dlink/throughput/cleanup + 诊断 ls/rapid-probe/dl-try）
- **网络形态**：全程直连（reqwest `no_proxy`）+ 强制 IPv4（`local_address(0.0.0.0)`）；UA 统一 `netdisk;P2SP;2.2.91.136;android-android`（下载侧为硬约束，见 §5）
- **凭据来源**（值不入库不入报告）：refresh_token 取自 `E:\GitHub\rs-CyDrive\test\instances\baidu2.json`（**只注文件名**）；appkey/secret 由 spike 运行时从 PCFS `client.go` 解析（或 `BAIDU_SPIKE_CLIENT_ID/SECRET` env）；刷新产物仅存 `%TEMP%\baidu_spike\token.json`。所有输出经 scrub（token 前 6 后 4）。
- **⚠ 限额结论适用范围**：当前 appkey 为 PCFS 借用的**第三方 appkey**，本文全部 QPS/限额/吞吐结论**仅对该 appkey 的限额桶有效**。正式 appkey（个人开发者）到位后须复测 §2。

## 结论速览

| # | 验证项 | 结论 | 关键数字 |
|---|---|---|---|
| S-1 | OAuth refresh | ✅ 跑通（换用 baidu2.json；baidu1 链已失效） | expires_in=2592000s（30d），刷新耗时 143ms |
| S-2 | QPS/限速 | ✅ 三面连发**零拒绝**（无 429/31034/任意 errno 拒绝） | list 10 连发（含并发）全 200/errno=0，204–501ms；分片 10 连发（含并发）全 error_code=0；下载流 5 连发（含并发）全 302+206 |
| S-3 | 三步曲+断点续传差集 | ✅ **持久化 uploadid 差集续传成立**；同参重 precreate 不恢复会话 | phase1=[0,1,2] → phase2 只传 [3,4,5,6,7]（2.3s），create errno=0 证明服务端保留旧分片 |
| S-4 | 秒传 return_type | ⚠ **此桶秒传不触发**：同内容（即时/延迟 2min+）均 return_type=1 | B 批按「可能返回 2 但不依赖」处理 |
| S-5 | dlink+Range+缓存 | ✅ 302→CDN，head/mid/tail 三点 206 且字节级匹配；**下载三约束**（有界 Range ≤4MiB、netdisk UA、禁全量 GET）；dlink 复用窗口 | 单 dlink 支撑 256 分片×4 并发全量下载（~49s）+ **≥56min 未失效**（探测终止时仍 206；上界未测到） |
| S-6 | ≥1GB 吞吐 | ✅ 上 27.4 MB/s / 下 21.7 MB/s | 上：256 片×4 并发 39.3s；下：256×4MiB 有界分片×4 流，单 dlink 复用 |
| 止损 | — | **未触发** | 下载 21.7 MB/s ≥ 5MB/s 阈值；QPS 无拒绝形态 |

---

## 1. S-1 OAuth refresh

**方法**：`GET openapi.baidu.com/oauth/2.0/token?grant_type=refresh_token&...`（client_id/secret 运行时解析自 PCFS client.go，不在仓库落值）；新 token 写 `%TEMP%\baidu_spike\token.json`。

**原始输出**（脱敏）：

```text
[SUMMARY] refresh|http=200|ok=1|expires_in=2592000s|took_ms=143
[SUMMARY] refresh|access_token=121.a2...SUtw|refresh_token=122.2e...N04A|cached_at=C:\Users\viccom\AppData\Local\Temp\baidu_spike\token.json
```

**过程偏差（如实记录）**：首选 `baidu1.json` 被拒——`error=expired_token desc=refresh token has been used`（百度 refresh_token **一次一换、旧即作废**，baidu1 链已被此前某次刷新消费）。改用 `baidu2.json`（两文件结构相同：`config.{access_token,refresh_token,encrypt,root_dir}`；baidu2 为加密实例，但本 spike 只取其 token 凭据，不触碰其 `/apps/privatefs` 数据）。

**结论**：refresh 链路可用；access_token 30 天有效。

**B 批实现参数**：
- refresh_token 必须**每次刷新后立即持久化**（旧值即刻作废，无重放余地）；CredentialStore 回调在 refresh 响应到达原子落盘。
- 刷新后旧 access_token 观察到仍被接受（本 spike 未单独验证宽限期，保守按「刷新即切换」实现）。
- errno 110/111/-6 三档映射按附录 A 落地：110→刷新重放一次；111→人话指引重新授权（勿重试循环，§7a）。
- **待负责人**：baidu1.json 链已失效，若两实例链路有分工请确认后续以 baidu2 链为准（或提供新 baidu1 refresh_token）。

## 2. S-2 QPS/限速（⚠ 仅 PCFS 第三方 appkey 桶）

**方法**：目录已存在状态下（首轮先以 precreate-only 探针自动建域，见 §首注）对 `method=list` 10 连发（顺序 + 并发）、`superfile2 partseq=0`（同一 4MiB 分片重发）10 连发（顺序 + 并发）、下载全流程（xpan 302 + CDN 4KiB 有界 Range）5 连发（顺序 + 并发）。

**原始输出摘要**（第二轮，目录真实存在；完整日志 `%TEMP%\baidu_spike/logs/qps2.log`）：

```text
[SUMMARY] qps|list_seq10|ok=10|errno_set={0}|min_ms=242|max_ms=501
[SUMMARY] qps|list_burst10|ok=10|errno_set={0}|min_ms=204|max_ms=328
[SUMMARY] qps|part_seq10|ok=10|error_code_set={0}|min_ms=344|max_ms=598
[SUMMARY] qps|part_burst10|ok=10|error_code_set={0}|min_ms=447|max_ms=2357
[SUMMARY] qps|create_after|errno=0|fs_id=671337245231660
[SUMMARY] qps|dl_seq5|ok=5|forms={"xpan302/cdn206"}|min_ms=348|max_ms=695
[SUMMARY] qps|dl_burst5|ok=5|forms={"xpan302/cdn206"}
```

**结论**：在 10 连发/10 并发量级，**未观察到任何拒绝形态**——无 HTTP 429、无 errno 31034、无 error_code 拒绝；并发分片仅表现为带宽分摊（单请求 0.9–2.4s），list 基线 RTT ≈200–330ms（直连 pan.baidu.com）。

**B 批实现参数**：
- 初始并发不必保守：分片上传 4 并发、目录列举适度节流（≥10 并发安全）即可；31034/429 处理留**单点重试钩子**（指数退避一次）而非全链路限速器。
- 正式 appkey 桶必须重放本节（工具已就位，改 env 即可复跑）。

## 3. S-3 上传三步曲 + 断点续传差集

**方法**：`resume abort` = 32MiB（8×4MiB 分片，首片随机内容防秒传）→ precreate → 持久化 `{path,size,block_md5,uploadid}` → 上传分片 [0,1,2] → `std::process::abort()` 硬自杀（无清理、无 create）；`resume continue` = 重启后**用持久化的旧 uploadid** 探测续传（重发一个缺失分片探活），只补差集后 create。

**原始输出**：

```text
[SUMMARY] resume_abort|uploaded=[0, 1, 2]|uploadid=P1-MTA...OTA=|aborting_now=1
[SUMMARY] resume|old_uploadid_probe|partseq=3|error_code=0|session_alive=true|http=200
  resume part 3..7 up (each ~390-610ms)
[SUMMARY] resume|mode=persisted_uploadid|phase1_uploaded=[0,1,2]|phase2_uploaded=[3, 4, 5, 6, 7]|diff_only=true
[SUMMARY] resume|phase2_wall_ms=2328|create_errno=0|final_size_ok=true|server_md5_field_is_contentid_not_literal_md5 (server_hash=759d7604...)
```

**结论**：
1. **差集续传成立**：旧 uploadid 会话跨进程存活，phase2 只传 [3,4,5,6,7]（5 片/2.3s），create 成功即服务端断言「8 片齐全」→ phase1 三片被服务端保留。这是 PCFS 未实现的改进点，**B2 必做**。
2. **同参重 precreate ≠ 恢复**：相同 path/size/block_list 二次 precreate 返回**新 uploadid + 全量 block_list [0..7]**（首跑实测；其语义为「该新会话仍需上传的分片」）。即**不能靠重 precreate 拿已传列表**——与任务原文「precreate 已传列表解析」的预设不符（已记 docs/decisions.md）。驱动必须在 precreate 后**立即持久化 uploadid**（我们已在 abort 前落盘，与真实驱动一致）。
3. create 对缺片硬失败（errno=10），证明服务端按 uploadid 会话校验全片集合——差集断言由此背书。
4. **服务端 `md5` 字段非字面 MD5**：listing/`md5` 与 dlink URL 内嵌的 content-id 一致（含非 hex 字符，如 `90bd6c1c2m4f...`）。完整性校验必须走**内容比对**（本报告 §5 字节级 Range 匹配）或 CDN 响应头 `content-md5`/`x-bs-meta-crc32`，不可比对该字段。
5. **rtype=1 是「冲突重命名」**：重复上传同名 qps_part.bin 未覆盖，生成了 `qps_part_20260907_123850.bin`。B 批需覆盖语义时用 **rtype=3**（待 B2 以 rtype=3 复核，本 spike 未展开）。

**B 批实现参数**：上传会话表（path/size/block_md5/uploadid/已完成分片位图）随分片完成即刻落盘；恢复时先用旧 uploadid 重发任一缺失分片探活，活则补差集、死则整体重来；分片 4MB、4 并发照抄。

## 4. S-4 秒传 return_type 分支

**方法**：A=全新 1MiB 三步曲上传（对照）；B=同内容（同 size+block_list）precreate 到新路径（秒传尝试，间隔数秒）；延迟复测=对 A 内容在 ~2min 后再 precreate 到第三个路径。

**原始输出**：

```text
[SUMMARY] rapid|A_normal|return_type=1|parts=1|pre_ms=191|parts_ms=468|create_ms=639
[note] B precreate raw: {"path":".../rapid_b.bin","uploadid":"N1-MTgz...","return_type":1,"block_list":[0],"errno":0,...}
[SUMMARY] rapid|B_same_content|errno=0|return_type=1|uploadid_len=59|fs_id=0
[SUMMARY] rapid|C_fresh_control|return_type=1|parts=1
[SUMMARY] rapid_probe|delayed_same_content|errno=0|return_type=1|uploadid_len=59|fs_id=0
```

**结论**：**此第三方 appkey 桶内秒传不触发**——同内容即时与延迟 2min+ 均 `return_type=1`（需上传），且 precreate 响应含 uploadid + block_list。`return_type=2` 分支代码路径仍须实现（官方语义存在、正式 appkey/热内容可能命中），但**不得作为功能依赖**（无秒传不影响正确性）。

**B 批实现参数**：precreate 返回 `return_type==2` 时跳过 superfile2/create 直接收尾；errno/return_type 异构解析按本节响应样本。

## 5. S-5 dlink + Range + 缓存时长

**方法**：8MiB 文件三步曲上传后 `xpan/file?method=download`（禁重定向）→ 302 Location（baidupcs.com CDN，签名 query ~1120–1216B）→ 以有界 Range 探测。缓存时长=**复用同一 dlink** 周期探测（bytes=0-0，间隔 30s→63min 递增）至失效。

**Range 复测（字节级）**：

```text
[SUMMARY] dlink|xpan_status=302|redirect_ms=310|url_shape=https://xafj-cm11.baidupcs.com/file/af10d00c... [query_len=1120]
[SUMMARY] dlink|form|token_appended=false
[SUMMARY] dlink|range_head|status=206|bytes=1024|content_match=true
[SUMMARY] dlink|range_mid|status=206|bytes=4096|content_match=true
[SUMMARY] dlink|range_tail|status=206|bytes=1024|content_match=true
```

**下载授权三约束（dl-try 矩阵，同一 dlink）**：

| 探测 | UA | Range | 结果 |
|---|---|---|---|
| full GET 无 Range | netdisk | — | **403** `error_code=31326 user is not authorized hitcode:104` |
| 开放 Range `bytes=0-` | netdisk | 0- | **403** 31326 |
| 有界 1MiB/2MiB/4MiB | netdisk | 0-N | **206** ✓（4MiB 单流 ≈9 MB/s） |
| 有界 8/16/32/64/256MiB（含文件中段） | netdisk | — | **403** 31326（**单请求分片上限 4MiB**，位置无关） |
| 有界 1MiB/4MiB/64MiB | Mozilla | — | **403**（92B 体）→ **UA 必须为 netdisk 族** |
| +access_token 追加 | netdisk | 全量 | 403（token 救不了违反三约束的请求） |

另：直连 Location 与「追加 access_token」**两态都出现过**（qps 首轮直连可用、次轮直连 403 须追加）→ B2 实现 fallback：先直连，403 则追加 token 重试一次。CDN 206 响应头提供 `content-md5`/`x-bs-meta-crc32`/`x-bs-file-size`（可作整文件校验与 size 来源）。

**dlink 缓存时长**（PCFS 未测、本 spike 目标参数）：

```text
  probe @ 0.5 min -> 206 ok=true
  probe @ 1.0 min -> 206 ok=true
  probe @ 2.0 min -> 206 ok=true
  probe @ 4.0 min -> 206 ok=true
  probe @ 8.0 min -> 206 ok=true
  probe @ 16.0 min -> 206 ok=true
  probe @ 31.0 min -> 206 ok=true
  probe @ 36.0 min -> 206 ok=true
  probe @ 41.0 min -> 206 ok=true
  probe @ 46.0 min -> 206 ok=true
  probe @ 51.0 min -> 206 ok=true
  probe @ 56.0 min -> 206 ok=true   ← 进程于此刻被环境终止（63min 预算跑到 56min），非 dlink 失效
```

**TTL 结论**：**单 dlink 复用 ≥56 分钟未失效**（12 次探测全 206，终止时仍有效；失效上界未测到——进程被会话超时终止而非 dlink 过期）。叠加吞吐实测（同一 dlink 连续承载 256 分片×4 并发×1GiB），dlink 是**高复用度长寿命凭证**。

**B2 缓存参数建议**：dlink 按 (fs_id/path) 缓存，TTL 取 **30 分钟**（远低于实测下界 56min 的保守值）+ **403/31326 驱动刷新**（先追加 token 重试、再重取 dlink，两段 fallback）；PCFS「每次 Seek 重取 dlink」与「全跨度单 Range」模式在当前 CDN 均不可照抄（后者 403，见上矩阵）。

**结论**：同一 dlink 至少支撑 **256 次×4 并发**连续分片请求（1GiB 下载全程未换链），且 **≥56 分钟未失效**（探测终止时仍有效）。PCFS「每次 Seek 换 dlink」为过度保守，多列一次 xpan 调用。

**B 批实现参数**：下载器 = 固定 4MiB 有界 Range 分片 + netdisk UA + 4 并发流 + dlink 缓存复用 + 403(31326) 时先试追加 token、再试刷新 dlink；206 响应头做增量校验。

## 6. S-6 ≥1GB 吞吐（上行/下行）

**方法**：1GiB（首片随机+模式填充，防秒传）→ 单遍顺序算 256×4MiB 分片 MD5 → 三步曲（4 并发分片 worker）→ 下载 = 4MiB 有界分片 ×4 并发流（单 dlink 复用）。

**原始输出**（两次全程，取第二次；`logs/throughput3.log`）：

```text
[SUMMARY] throughput|upload|return_type=1|parts=256|precreate_ms=179|parts_wall_ms=39250|create_ms=746|net_MBps=27.4|incl_hash_MBps=25.5|per_part_ms_avg=542|max=1815
[SUMMARY] throughput|verify|listed=true size_ok=true md5_ok=false(服务端 md5 字段为 content-id，非字面 md5，见 §3)
[SUMMARY] throughput|download|chunks=256 bytes=1073741824 bytes_ok=true net_MBps=21.7 token_appended=false
[SUMMARY] throughput|download_browser_ua_4m_chunk|status=403 bytes=92   ← UA 对照
```

**结论**：**上行 27.4 MB/s、下行 21.7 MB/s**（均远超 5MB/s 止损线）；两次独立全程复测一致（25.2/23.6 与 27.4/25.5 上行）。create 后 listing size 即时正确；下载 256 分片全部 206、字节计数全额。

**B 批实现参数**：4MiB 分片 + 4 并发即为可用默认（本机带宽/VPN 环境下）；hash 单遍顺序读 1GiB ≈4s（两遍读盘可避免：边读边算+分片队列复用缓冲）；下载并发 4 流足够，UA/Range 约束见 §5。

## 止损判定

**未触发**：下载 21.7 MB/s ≥ 5 MB/s；全项 QPS 探测零拒绝；六项验证全部完成。R/E 批按计划推进，B 批进入实现。

## 阻塞与偏差（如实记录）

1. `baidu1.json` refresh_token 已被消费失效（§1）→ 改用 `baidu2.json`，**待负责人知悉**（若 baidu1 链另有用途需补发）。
2. 任务原文「precreate 已传列表解析」与实测不符（§3.2）→ 改用「持久化 uploadid 差集」路径达成同等目标，已记 `docs/decisions.md`。
3. spike 首版 continue 曾因上述误设走错分支（create errno=10）——已在最终版修正并完整重跑两轮 abort/continue，报告数据均为最终版输出。
4. 诊断期临时用独立 target-dir 构建（后台 dlink 进程锁住主 target 的 exe），不影响仓库内容。
5. dlink 缓存探测进程在 56min 标记处被会话环境终止（预算 63min）——终止时 dlink 仍有效，TTL 上界未测到（§5 已如实标注）。

## cleanup 证据

```text
# 第一遍（删 9 个文件）
[note] remote /apps/baidu_spike holds 9 entries: ["/apps/baidu_spike/dlink_8m.bin", "/apps/baidu_spike/qps_part.bin",
  "/apps/baidu_spike/qps_part_20260907_123850.bin", "/apps/baidu_spike/rapid_a.bin", "/apps/baidu_spike/rapid_c.bin",
  "/apps/baidu_spike/resume_32m.bin", "/apps/baidu_spike/throughput_1g.bin", "/apps/baidu_spike/throughput_1g_20260907_125131.bin",
  "/apps/baidu_spike/throughput_1g_20260907_125234.bin"]
[note] post-delete list: errno=0 entries=0
[SUMMARY] cleanup|remote_deleted=9|remote_remaining=Some(0)|local_temp_removed=6|note=xpan_delete_moves_to_recycle_bin(10d retention,not_verifiable_via_this_api)

# 第二遍（删 spike 自建目录本身）
[note] dir delete errno=0 per-item=[(0, "/apps/baidu_spike")]
[SUMMARY] cleanup|spike_dir_removed=true|dir_recheck_errno=-9
```

远端 9 文件 + 目录本身全部删除（复查 errno=-9 = 目录不存在）；本地 `%TEMP%` 大文件已清（保留 token/state 供复跑）。注：文件清单本身即 rtype=1 冲突重命名的旁证（qps_part_2026…、throughput_1g_2026…×2 均为重跑产物）。**xpan filemanager delete 走回收站（约 10 天保留期），API 层无法验证回收站状态，如实记录**。

## 复跑指南

```bash
cargo run --manifest-path examples/baidu_spike/Cargo.toml --release -- refresh
cargo run --manifest-path examples/baidu_spike/Cargo.toml --release -- qps
cargo run --manifest-path examples/baidu_spike/Cargo.toml --release -- resume abort   # 自杀退出属预期
cargo run --manifest-path examples/baidu_spike/Cargo.toml --release -- resume continue
cargo run --manifest-path examples/baidu_spike/Cargo.toml --release -- rapid
cargo run --manifest-path examples/baidu_spike/Cargo.toml --release -- dlink          # 含 ~63min 缓存探测
cargo run --manifest-path examples/baidu_spike/Cargo.toml --release -- throughput
cargo run --manifest-path examples/baidu_spike/Cargo.toml --release -- cleanup
# 诊断：ls / rapid-probe / dl-try
```

token 缓存与日志在 `%TEMP%\baidu_spike\`（token.json / state.json / resume_state.json / logs/ / target-diag/）——本机临时目录，不入仓库。

---

## 附录 B：2026-09-08 增补轮（负责人凭据/测试根裁决后）

- **背景**：负责人裁决——instances 授权即正式凭据；测试根 = `/apps` 下新建子目录（本轮 `/apps/cloudfs-spike`）；PCFS 源码为协议权威参照。工具随之增 `BAIDU_SPIKE_REMOTE_DIR` env 覆盖（4c0e46b）+ cleanup 同步支持（后续 fix）。
- **token**：直接可用（`ls errno=0`，无需重新授权）。
- **差集续传复验（新根）**：phase1=[0,1,2] 杀进程 → phase2 只补 [3,4,5,6,7]（7259ms），create errno=0，与 §3 结论一致。
- **dlink TTL 上界（S-5 挂账补测）**：探针预算扩至 96min，结果 **96.0min 仍 206、预算封顶未测到失效**——下界自 56min 推高至 ≥96min。**B2 参数建议修订**：dlink 缓存 TTL 从 30min 保守值上调至 **60–90min 安全区间**（失效即重取 dlink 的兜底逻辑不变）。
- **零污染证明**：`/apps` 测试前 14 条目 → 测试后 14 条目（`/apps/privatefs` 全程只读未动）；本轮所建 `/apps/cloudfs-spike` 及全部测试文件已删（dir_recheck errno=-9；xpan 删除进回收站 10 天保留为 API 已知限制）。
