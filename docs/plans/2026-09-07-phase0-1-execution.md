# Phase 0 + Phase 1 执行计划（含 Kickoff 指令）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.
> 性质：实施计划（documentation.md 两档制中的可执行档）；权威设计 = `2026-09-07-cloudfusion-foundation.md` v1.2（D1–D10），冲突处以 v1.2 与 standards/ 为准。

**任务范围（负责人 2026-09-07 指令）**：完成第一阶段全部任务 = **Phase 0（搬迁改名）+ Phase 1（Batch S 百度 spike / Batch R trait 瘦身 / Batch E 加密 v2 流式）**，全程自主、不中途确认，疑问记录进 decisions.md 并在收尾报告集中列出。

---

## Batch 顺序与验收

### Batch P0：搬迁改名（纯机械，每步全绿）
1. worktree 隔离（见 Kickoff）；按 foundation §3 crate 映射逐一 `git mv` + Cargo.toml/lib.rs/引用改名（cydrive-* → cloudkit-*/ck-*），**二进制名保持 `cydrive`/`cydrive-sync-server`**；
2. 交付 `scripts/check_layers`（校验 L3+ crate 无 drivers/* 依赖；当前应过——现状无此依赖）并接入 `.github/workflows/ci.yml` + CI 增秘密扫描步骤（对新增内容 grep token/appkey 模式即可，最小实现）；
3. 验收：workspace 测试全绿（527，计数随迁移命名同步）+ fmt/clippy 干净 + WSL 侧 check/test（克隆 `~/rs-cloudfs`，建立法照 rs-CyDrive 惯例：clone 自 /mnt/e 路径后 Windows→WSL 单向同步或直接双检出不强制）+ `cydrive.exe --version` 正常。

### Batch S：百度 spike（验证驱动，非 TDD；照 examples/history_spike.rs 先例）
- 新 `examples/baidu_spike/`（或 examples 二进制），六项验证按 foundation Phase 1 行 + multicloud 附录 A 情报表执行：
  ① OAuth device code 或 refresh 流程跑通（凭据见 Kickoff 前置）；② QPS/限速（列目录 10 连发/分片 10 连发/下载 5 连发，记录 429/31034/拒绝形态——**注：当前 appkey 为 PCFS 第三方，限额结论仅对该桶有效，自有 appkey 结论留待补测**）；③ 上传三步曲 + **断点续传差集**（precreate 已传列表解析、中途杀进程重传只补差集）；④ 秒传 return_type 分支行为；⑤ dlink+Range 复测 + **dlink 有效缓存时长**（PCFS 未做，我们要的优化参数）；⑥ ≥1GB 吞吐实测（上/下行）。
- 产出 `docs/reports/2026-09-XX-baidu-spike.md`（每项：方法/原始输出摘要/结论/对 B 批的实现参数建议）。
- **止损点**：下载吞吐 <5MB/s 或限额使基本操作不可用 → 停止后续批，报告留档（百度后端降级 v2 待议，R/E 批不受影响继续）。

### Batch R：trait 瘦身（纯重构，TDD 迁移测试护航）
- 按 D1–D4 建 `cloudkit-storage`：StorageDriver/Capabilities/StorageError 分类学（百度 errno 三档映射先以单测形态预埋）/VolumeId(EntryId 从第一天带卷)/conformance_suite 框架（断言集八条见 interfaces.md §6，本批先落 mock 实现跑通套件本体）；
- CloudTransport 演进：核心面收敛 + `InboundCap`/`ChatCap` 拆分（provided/可选 trait 优先，breaking 波及面清单先列）；grammers transport 适配；**解除 ck-telegram→core 反向依赖**（architecture §1.5 过渡豁免表销账）；bot worker/webdav 消费方能力探测降级；
- 验收：workspace 全绿 + **真机 Telegram 冒烟一次**（用 D:\Tools 生产实例：向 `/_e2e_smoke/` put 一个小文件经 Explorer Y:、PROPFIND 读回、删除清理——注意 PROPPATCH 陷阱条款；生产 db 不做破坏性操作）+ decisions 记录关键取舍。

### Batch E：加密 v2 流式（TDD）
- `cloudkit-crypto`：CryptoScheme trait + v1 GCM（现有逻辑迁入，行为/字节不变，Python 互操作测试迁移护航）+ **v2 分块 AEAD（STREAM 构造）**——块大小 1MB 默认（可配）、流式加密上传（零 .enc.tmp、常量内存）、`decrypt_range`（块级随机访问）、口令+PBKDF2 延续；
- 接线：`encryption_scheme` 配置键（默认 gcm；legacy json 拒收）+ Entry 元数据 scheme + sync payload 可选字段（旧实例忽略）+ hydrate 按 scheme 分发（v2 流式水合）；
- 验收：workspace 全绿；v1 文件 hydrate/upload 字节不变（既有测试零改动）；v2 新增 roundtrip/tamper 拒绝/跨块 Range/大文件流式（内存峰值断言）测试；真机冒烟（telegram + v2 加密小文件上传/下载往返）。

### 收口
版本递增 0.8.0（trait 破坏性演进）；decisions/AGENTS（阶段状态+计数）入档；`cargo build --release` 产物验证；**不部署生产位**（新仓产物首次部署留到 Phase 2 验收后统一裁决，本批只构建验证）。

---

## Kickoff 指令（新会话粘贴即执行；负责人 2026-09-07 预授权：全程自主，不中途确认）

> **执行 `E:\Rs_Codes\rs-cloudfs` 的 Phase 0 + Phase 1 全量**（docs/plans/2026-09-07-phase0-1-execution.md，Batch P0→S→R→E→收口）。模式：全程自主，**不向用户请求任何确认**；可逆实现细节自行裁决并在 decisions.md 记录理由；真实疑问/范围变更/止损点触发 → 记录到 decisions.md「待负责人」清单并继续可继续的部分，收尾报告集中列出；破坏性操作（动生产 db/部署位/删除既有数据）一律禁止。
> 1. 开工先读（按序）：新仓 `AGENTS.md` → `docs/standards/`（五份全）→ `docs/plans/2026-09-07-cloudfusion-foundation.md`（v1.2，D1–D10 权威）→ 本计划 → `docs/plans/2026-09-06-multicloud.md` 附录 A（百度情报）。工作目录 `E:\Rs_Codes\rs-cloudfs`，长批 worktree 隔离（分支 `feat/phase0-1` 自 main 切出，P0 可直 main——机械批酌情）。
> 2. 凭据（只读，绝不入库/入日志/入提交）：百度 refresh_token 在 `E:\GitHub\rs-CyDrive\test\instances\baidu*.json`（access_token 已过期属预期，spike 首步用 PCFS appkey——`E:\Go_codes\PrivateCloudFS\drivers\baidu\client.go:69` 处只读参照——刷新获取新 token，存 OS 临时目录）；telegram 真机冒烟用 `D:\Tools\rs-CyDrive` 生产实例（谨慎协议见 Batch R 验收）。
> 3. 纪律：TDD（S 批除外，验证驱动）；断言零漂移；workspace 级三步门禁每批必过；跨平台改动加 WSL（~/rs-cloudfs 克隆自建）；conformance 断言集八条不得缩减；规范冲突以 standards/ 为准并记 decisions。
> 4. 中止条件：spike 止损点（继续 R/E）；重派两次仍不绿；契约矛盾；需动 Python 兼容红线——停、留现场、写清结论。
> 5. 收尾汇报：改动摘要/每批验证证据（真实输出）/未询问的决定与回滚/疑问与待负责人清单。
