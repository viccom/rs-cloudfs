# Phase 8 read-through 合入前深度审查 findings（K78 形态）

> 审查基线：`feat/readthrough-index`@720e49c；三路并行（A 驱动核心 / B 测试桩面 / C 消费面接线），全部只读精读 + 现场实跑（core 16+9+3 绿、webdav/web/local 面测试绿）。
> 裁决列：**修复批**（本批 TDD 清偿）/ **挂账**（跟踪单风险节）/ **裁决不修**（查证支撑）。

## High

### H1（A-1=C-1 同根）D10 加密+宽面卷读面拒收 × 计划 §7「加密卷零变化」冲突
- 位置：readthrough.rs:175-177/:345-347（加密检查在回源前返回 `Err(EncryptedInstance)`）→ webdav 403 / winfsp ACCESS_DENIED / web 500。
- 事实：加密+宽面卷（baidu/pan115/pan123/webdav 任一开 `enable_encryption`）此前读面完全可用（索引视图+解密读，pan115/pan123 e2e 加密腿为证）；合入后整个读面（PROPFIND/GET/枚举/仪表盘）被拒，Explorer 无可行动文案。D10 的危害论证只针对**物化密文行**（size=密文），不针对读既有索引。
- **裁决（主会话执行期，K83 入档）**：D10 语义收窄为「**拒物化、不拒读**」——加密+宽面卷在门后直接退化到 db 读（`read_dir_fresh → Ok(db.list_dir)`、`stat_fresh → degrade()`），永不回源物化；读行为与 RT3 前逐字一致（§7 兑现）。与加密+窄面的既有退化臂同形，机制面统一。依据：计划 §0 风险表与 §7 合规表两处明文「加密卷零变化」优先于 §0 功能清单里「加密卷显式拒收」的一句话表述；不物化即 D10 底线（AEAD 预算数学/size 契约）完整保持。可逆性：一行门序；若负责人裁定要恢复字面拒收，revert 该门臂+重跑 e2e 加密腿即可。→ **2026-09-23 被 K84 升级**：H1（拒物化不拒读）保留为过渡态，终态 = B 方案（加密 read-through 放开，K84.2）；原回滚路径由 K84.4 取代。
- 修复批：改门臂 + 重写 RT2 测试 12（`encrypted_instances_refuse…` → `encrypted_instances_degrade…`：播种 db 行 + 断言零网络返回行）+ 测试 9 邻面回归。

## Medium（修复批）

| # | 来源 | 内容 | 修复方向 |
|---|---|---|---|
| M1 | A-2 | read_dir_fresh 的 NotFound 臂无 in-flight 过滤——在途上传行可被双确认删除（D7「两侧永不触碰」第三面破口） | 该臂行循环补 `row_in_flight` 过滤（同单点判据），TDD：NotFound 臂 + pending 行 → 行幸存 |
| M2 | A-3 | `list_all_pages` 无页数/条目上限——故障驱动自指游标 = 读路径无限循环，winfsp FSD 线程挂死 | 页数+累计条目双上限（定值实现期合理），超限归 `StorageError` 带上下文；TDD：假驱动无限 cursor → 超限 Err |
| M3 | A-4 | 完成趟 sweep 无体量地板——驱动静默空列表类故障一趟清空全索引并弃缓存标记（本仓 K74 静默错置先例支持加闸） | sweep 前地板：pruned 候选超既有 uploaded 行 50% 且基数 >100 → 放弃 sweep+warn+留检查点？否——留行即可（趟已完成清检查点，行保留待下次）；TDD：大库+全空列表 → 行全保留 |
| M4 | C-2 | NotFound 臂 prune 无候选上限（对照 reconcile 的 32 cap）——千级行目录首次探测 = N 次串行 stat，winfsp 面放大为 FSD 挂起 | 沿用 `PRUNE_CANDIDATE_CAP`：超限跳过删除直接 NotFound；与 M1 同臂同批 |
| M5 | C-3 | api_list 深跳「远端存在但空」目录假 404（三面唯 web 缺 stat_fresh 预检） | api_list 前置 `stat_fresh(&rel)`（与网关 read_dir 同形）；TDD：深跳空目录 → 200 空列表 |
| M6 | B-1 | stat_fresh 兜底 Ok 臂（父层半残→driver.stat 物化）零测试 | 补 fail_next_list + stat 命中用例（CountingDriver 缝现成） |
| M7 | B-2 | rebuild DeadlineStop 臂（页间时间中断→目录回队→rerun 重列）零测试 | 补多页目录+微预算续跑用例（list 计数 +1 断言） |
| M8 | B-7 | D2 门第二臂（宽面在场但 authoritative_index=false）无测试 | WideTransport caps=false 变体零网络退化用例 |

## Low（处置：顺手修 6 + 挂账 9 + 裁决不修 3）

**顺手修（随修复批）**：L1(B-4) materialize updated_at 断言 `>=`→`>`；L2(B-8) root_record 补一行测试；L3(C-4) open_handle 注释漂移更正；L4(C-7) vfs.rs:1081(+rename_entry :1497/:1536) in-flight 判据收编 helper；L5(B-6) rebuild.rs 模块文档 ghost 术语辨析（sweep-ghost vs pending）；L6(B-10) 腿 05 注释 200→192。

**挂账**：L7(A-5) DirCache flights/generations 无驱逐（真机矩阵后评估 LRU）；L8(A-6) 跨进程 rebuild 共享检查点（文档明示同卷单写者）；L9(A-7) NTP 回步窗 sweep 误删（罕见运维事件，read-through 可自愈）；L10(A-8 后半) NotFound 负缓存缺失（重复视图重复付费）；L11(A-9) 网关适配器自写 db 路径不失效（D6 5s 容忍面，rmcollection/MOVE）；L12(B-3) 真机删除腿缺确认计数（离线 5a 已钉）；L13(B-5) 页界 512 双处硬编码；L14(B-11) AList 腿失败路径清理缺 Drop guard（腿 07 兜底+人工清法：删 `/_e2e_readthrough_*` 前缀）；L15(B-12) TTL 缝测试 50ms 窗提宽；L16(C-5) webdav 冷路径双 list（已挂账维持）；L17(C-9) web Refresh 不呈现 interrupted（非回归）；L18(C 路建议) winfsp FSD 缓存窗的 `dir_info_timeout` 真机探针（成本最低的真修候选，挂账下批）。

**裁决不修（查证支撑）**：①后端最终一致性窗（persist_success 后 list/stat 视图滞后→双 NotFound→行删，读你的写模型下低概率+persist 自愈）——接受残余；②webdav 根 open 语义微变（NotFound→Forbidden，Explorer 不触碰 GET 集合的边角）；③web 面 EncryptedInstance 500 映射——H1 修复后读路径无发射点，match 扩展保留为防御臂。

## 三路查证无问题项（摘）
并发归并无旧快照窗（invalidate 只撤 marked 不触 generations）；删除主路径无绕路（谎报 Ok/任何 Err 一律保留）；scan_started_at 全仓单写点、损坏降级三臂全部 fresh-scan 重锚；materialize 平移逐列保真；六真驱动 authoritative_index 全部 conformance 背书；resolve_row 纯 db 守卫面完整（delete/rename 零网络）；sync.rs 布尔等价零漂移；多卷重装配 DirCache 无残留共享；api_files/bot /ls 零改动实证；「桩照实现抄」未复现（mock 按契约建模、真机腿用外部真值）。
