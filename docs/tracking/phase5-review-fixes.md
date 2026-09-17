# Phase 5 合入后深度审查修复批（2026-09-17，fix/phase5-review）

> 审查：2026-09-17 三路深度审查（主会话精读驱动源码 + 子代理测试/桩面 + 子代理集成/装配面），
> 报告 2 High + 12 Medium + 若干 Low。本表 = High/Medium 逐项销账（TDD：代码缺陷先红后绿）。
> 分支：`fix/phase5-review`（自主模式裁决：工作区在制改动是多项修复的载体，先收口在制批再叠修复；
> 未动 main、未 push——`git branch -D fix/phase5-review` 或不合并即整批回退）。
> 决策档：K73（decisions.md 2026-09-17 条目）。

## 销账表（High/Medium 全量）

| 项 | 内容 | 状态 | commit | 红→绿证据 |
|---|---|---|---|---|
| H-T1 | upload_path tmpdir 撞名（pid+纳秒，Windows ~1ms 粒度并行初始化互删）→ flaky 根因 | ✅ | d35769f | 时间粒度竞态无法确定性红测：压测前后对比——修复前 2 失败/26 轮（审查期实测）+ 首轮审查运行 2 失败；修复后 **0 失败/30 轮**（--test-threads=8）；tempfile OS 级唯一命名 + `keep()` 保持手动清理语义 |
| H-T2 | conformance probe 测试双重假覆盖（NeedsReauth 从未触发 + 430004 注入在 probe 不经过的面上空转） | ✅ | 9161217 | 桩加持续 user_info 注入 + refreshToken 恒败路由；四变体（Alive/NeedsReauth/RateLimited/Unreachable）全部真实触发（补覆盖型，无产品缺陷） |
| M-S1 | delete 句柄 parent 段恒空：invalidate("") no-op（ghost 行）+ API parent_id 空串真机未验 | ✅ | 3636445 | 红：`mkdir-after-delete → Exists`（ghost 缓存行复现）；绿：句柄三段带真实父 cid（list/stat/上传三产点）。桩新增 last_delete_parent 断言 API 收到真实父 cid |
| M-S2 | STS 每分片重取 + close 死代码（3 片上传 6 次 get_token，全走 1rps 限流 API） | ✅ | ae44c14 | 红：`got 6`；绿：`get_token == 1`（TransferState 缓存链首 STS，两 ctx 构造器合一，死代码删除） |
| M-S3 | 直链 401/410 不自愈（文档声明 vs 实现只有 403 走恢复）——长流硬死 | ✅ | 328da54 | 红：`Unavailable("CDN GET: HTTP 410")` 杀流；绿：410 后重取直链续传完整载荷 + downurl_calls ≥ 2（桩 cdn_gone_budget 注入） |
| M-S4 | rename 目标存在预检 cache-only（冷目标目录穿透到 move/update，Exists 契约丢失） | ✅ | 2b15762 | 红：跨目录 rename 到未列目录返回 Ok（穿透）；绿：Exists + 源文件原位未动（预检 miss 现列，mkdir 同款） |
| M-S5 | pathcache 无 TTL/无容量（外部删改永久陈旧 + rebuild 内存无界） | ✅ | d9dc3bd | 单测三钉：TTL 过期 miss（30ms 注入窗）/ 1024 目录上限驱逐最旧 / invalidate 即时（`with_ttl` 测试缝，LimiterConfig::fast 先例） |
| M-I1 | `not(baidu)+pan115` twin 丢 TokenStore（一次一换下刷新不回写 = 重启失授权） | ✅ | b4dc100 | 行为级红测需端点注入缝（dispatch 路径无此缝——**如实记录**）；验证 = twin 与主 twin 逐参对齐 + pan115-only 组合编译/测试过；回写机制本身由 oauth_state_machine 驱动级钉住 |
| M-I2 | 同组合多卷 dispatch 把任何非 telegram 卷按 pan115 装配（误导性报错） | ✅ | b4dc100 | 组合构建（--no-default-features --features pan115）红：`misrouted into the pan115 assembly`；绿：baidu 卷拿到 BAIDU_DRIVER_REQUIRED（dispatch_unified_backend_volume 移入 lib + 单一 match 全组合通用） |
| M-T1 | sign_val 区间 SHA1 数值正确性零验证（桩只记录不校验） | ✅ | 9161217 | 预计算常量对账（dev-dep sha1 触发 E0464 双 rlib，改常量钉——同性质零依赖面）+ parse_sign_check 直接单测（闭区间/倒置/半开拒）；语义与 spike upload.rs:54 对照一致 |
| M-T2 | 在制 debug_rename_landing 探针（零断言/不清理/--ignored 连带执行） | ✅ | 15195ce | 随在制批收口删除；调查目的已由 live ⑤ 断言化用例覆盖（K72.3） |
| M-T3 | live ④ 固定名+固定内容（SHA1 全局去重短路 multipart 路径且不可见） | ✅ | 15195ce | ④ 对齐 ⑤⑥ 纪律：stamp 唯一名 + stamp_content 按轮随机（共享助手，⑥ 同步换用）；真机重跑待真机窗口 |
| M-T4 | normalize_endpoint（真机缺陷③修复函数）生产分支无正面钉 | ✅ | 9161217 | 生产向量（https/http 剥 scheme + 尾斜杠 + 裸形态直通）+ loopback 缝向量 |
| M-T5 | K72 悬空引用（live_matrix 两处引用查无此档） | ✅ | 15195ce | decisions.md K72 补档（SHA1 去重短路教训 + K70.7 批关联） |

## 在制批收口（批 0，15195ce）

setup 扫码向导 + live_matrix ④⑤⑥ + pan115_e2e 补腿（K70.7 内容）在审查后修毕自身问题（M-T2/M-T3/M-T5/clippy/fmt/L2 poll 上限 10 次）入库。**注**：该批为上一会话在制工作，自主模式裁决先行 commit 以隔离后续修复——不认可可 `git revert 15195ce`（冲突面：批 1+ 的 live_matrix 改动需手解）。

## Low 挂账（未修，如实记录）

- rename 跨父错误映射 `Io/Unavailable/Invalid → Exists` 过宽（lib.rs rename 的 move 臂）——传输失败会误报目标占用
- `open_writer` 不 `create_dir_all` spool 父目录（生产卷家目录存在故不触发；H-T1 修复后测试面也不再触发）
- OSS 传输类错误（status=0）不在 `retryable()` 集——最瞬态的失败不标可重试（当前无自动重试消费面，影响=分类语义）
- `setup_http_client()` 缺 IPv4 `local_address` 绑定（K18 纪律；spike 直连形态可达故无实害，对齐性缺口）
- complete 的 callback 响应体被丢弃（诊断信息丢；可见性由 resolve_new_row 兜底）
- doctor 文案 22/18 连续空格（doctor.rs:919/926 + lib.rs:6836 非 pan115 批）；conformance/read_path 遗留 debris（_cfg_marker 等）
- 桩面缺口：envelope 数字形态无 API 面回放、downurl 端点自身 UA 绑定无桩、OSS V1 签名整体不校验（string_to_sign 纯函数钉）
- 多卷同账号 = 每卷独立 limiter（合计超 1rps；K69.3 实测 4rps 可持续——可接受注记）
- conformance.rs:223 的 pid-only 临时目录名（单测试使用不撞；与 H-T1 同类，改 tempfile 更稳）
- live ⑥ 的「杀进程」实为 drop 形态（真 kill 未测；孤儿 uploadId 无 AbortMultipartUpload 清理）
- https-loopback 端点在 normalize_endpoint 会被剥 scheme（现无此形态消费方）

## 真机待验项（2026-09-17 真机验证批销账——K74，live_matrix 6/6 + pan115_e2e 4/4）

- ~~M-S1 修复后 delete 带真实 parent_id 的真机形态~~ **✅ 验证过**：live 全程三段句柄（SUMMARY `fid:pc:parent`，parent 段已填充），cleanup 经新句柄 delete 成功（D2 回收站）。
- ~~live ④ 随机化后的重跑~~ **✅ 12MiB multipart 5.1s 过**（stamp 名 + 按轮随机内容——K72 纪律生效）。
- ~~live ⑥ 的 drop-形态断言~~ **✅**：真 OSS ListParts 报 3 片 + 第二轮**同 uploadId 复用** + 12MiB 逐字节回读。
- **真机新揭缺陷（审查漏网）**：`ufile/move` 目标参数官方形态是 `to_cid`（SDK/桌面版双参照），驱动误发 `to_pid` → 静默错置（文件落账号根、目标列表不可见）。修复 3c5ab51（桩同步改严格建模，离线红→绿）；旧形态碎片 21 件已清扫核空。e2e rebuild 腿两个缺陷（root="0" driver 全账号 25 分钟 vs scoped 2.0s 契约 + 固定名种子）修复 fe4844a。探针方法论教训见 K74.3。
- M-S3 的 401/410 自愈在真机 CDN 的实际形态：**未遇**（本轮真机无直链过期实发）——桩按 K69.4 注记建模，挂账维持。
- downurl 直链 TTL 真值：未测（DLINK_TTL 保守 30min 维持）。

## 验证总账

- 每批独立 commit + 常规提交信息；红→绿证据见上表与各 commit message
- 终验（2026-09-17）：workspace 全量 `cargo test --workspace --no-fail-fast`（数字见 AGENTS 常用命令行）、`cargo clippy --workspace --all-targets -- -D warnings` 0 错、`cargo fmt --all -- --check` 绿、`scripts/check_layers` 绿、`scripts/scan_secrets` 绿
- 裁剪组合腿：`--no-default-features --features pan115`（含新组合测试）/ `local,baidu` / `sftp,pan115` / `local` 编译全过
