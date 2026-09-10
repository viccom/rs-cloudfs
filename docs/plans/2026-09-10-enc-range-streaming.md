# Phase 3.5-a：加密文件 Range 流式读（实时解密窗口）执行计划

> **For Claude:** REQUIRED SUB-SKILL: executing-plans（hub-and-spoke，TDD 红→绿留证；真机批次主会话执行）。

**Goal:** 加密文件（aead_v2 行）获得与 PCFS 对等的**按需 Range 实时解密读**——打开秒回（只拉 34B header + 首 window），随机 seek 每跳一个密文窗口，不再整文件水合；同时保持 per-chunk AEAD 认证（严格强于 PCFS 的裸 CTR）。gcm v1 行、无 range 能力传输、缓存命中路径全部不变。

**裁决 K47（执行期入档 decisions.md）:** 加密读窗口化 = aead_v2 行经**解密传输包装器**（`DecryptingTransport`，cloudkit-core）走既有窗口流式；不采用 PCFS 的 CTR 构造（负责人条件"当前实现复杂且不稳定"经核实不成立：`AeadV2::decrypt_range`/`decrypt_chunk` 原语已实现且测试覆盖，接线范围有界；CTR 需新增 scheme 且丢失认证，AGENTS 明列"纯 CTR 无认证"为 PCFS 反面教材）。

## §0 已核实事实（2026-09-10 主会话侦察）

- `FileRecord.encryption_scheme: String`（`database.rs:80`，"gcm"/"aead_v2"）——分支零网络。
- `open_read` 现状（`vfs.rs:703+`）：密码门 → WF0 缓存探针 → K33 三重门（`is_encrypted || !range_read || size==0 → Hydrate`）。加密行 `RemoteHandle.total_size = u64::MAX` 哨兵（`vfs.rs:676-679`）→ 加密臂必须以 `row.size`（明文权威，K35）作 total_size。
- `cloudkit-crypto` v2：容器 `[34B header(salt/iter/chunk_size)][chunk_i || 16B GCM tag]*`，`decrypt_chunk`/`Header::parse`/`Layout::derive` 均私有（`v2.rs:512/125/186`）；`decrypt_range` 为整片内存 API（`v2.rs:299`）。PBKDF2 100k（`v1.rs:43`）——每 handle 派生一次，K41 宽限 + LRU 摊薄。
- 内层 `open_range` 的 ≤4MiB 有界窗口由各驱动自理（baidu `download.rs` 后台 4MiB 窗口任务）；包装器可整 span 一次内访。
- PCFS 参照（Explore 报告 2026-09-10）：CTR counter 算术 seek + CDN Range + 实时 XOR；首读 2 次串行 HTTP；无认证。其 CryptoWrapper 透明层形态 = 本计划的包装器同款位置。

## §1 架构

```
面（webdav RangeFile / winfsp WindowReader）——零改动，明文坐标窗口
        │ open_range(plain_off, plain_len)  ← ByteStream(明文)
┌───────┴────────────────────────────────────┐
│ DecryptingTransport（core，实现 CloudTransport）│
│  明文窗口 → chunk 跨度 [HEADER+f*stride .. l 尾]  │
│  → inner.open_range(密文坐标) → 逐 chunk 验签解密  │
│  → 切片出请求明文；EOF/越界按明文 total 收口         │
└───────┬────────────────────────────────────┘
        │ inner.open_range(ct_off, ct_len)
   既有传输（baidu/local；telegram 无 range → 仍 Hydrate）
```

- `StreamSource::Stream` 对加密臂携带：`total_size = row.size`（明文）、`transport = Arc<DecryptingTransport>`——面代码不感知加密。
- 懒初始化：header（34B）+ 密钥派生在首个 `open_range` 调用时发生（PCFS 同款：open 不付网络/派生成本；坏 header/错密码表现为首读错误）。
- WF0 缓存优先、K41 宽限、水合回退（gcm/无 range/0 字节）全部原样。

## §2 批次（worktree `feat/enc-range`；串行；每批 TDD 红→绿 + 全门禁）

### E1 crypto 窗口 API（cloudkit-crypto）
- 公开 `AeadV2` 窗口面：`WindowReader`（暂名）——`from_header(password, &[u8;34]) -> Result<_>`（parse+PBKDF2 一次）、`chunk_size()`、`n_chunks_for_plain(plain_len)`（与 `Layout::derive` 互验的公开布局算术，含 exact-multiple 规则）、`decrypt_chunk(index, is_last, ct) -> Result<Vec<u8>>`。
- 测试：随机容器上「任意窗口解密 == 全解密切片」性质测试（含 exact multiple / 1 字节 / 尾 chunk tag-only 边界）。
- Commit: `feat(crypto): public chunk-window decrypt API for aead_v2`

### E2 core：DecryptingTransport + 门改造（cloudkit-core）
- `enc_stream.rs`（新）：`DecryptingTransport` 实现 `CloudTransport`——`open_range` 明文↔密文翻译（首调用懒拉 header）、`open`（全文件）走同路径、其余方法委托 inner、`capabilities()` 透传。`StreamSource` 文档更新。
- `open_read`：三重门第一格改 `is_encrypted && scheme != aead_v2 → Hydrate`（aead_v2 + range + size>0 → Stream，`total_size=row.size`，transport 包包装器）；密码门/缓存探针次序不动。
- 测试：mock 密文容器——①门矩阵（aead_v2+range→Stream；gcm→Hydrate；aead_v2+无 range→Hydrate；无密码→MissingPassword；cached→Hydrate）；②包装器窗口正确性（跨 chunk/EOF 收口/1B）；③内访坐标断言（首读先拉 header；span 不越容器尾）；④既有 `is_encrypted→Hydrate` 钉测试核对（默认列值 "gcm" 应存活，如被改判按 K47 记录）。
- Commit: `feat(core): encrypted aead_v2 rows stream through a decrypting transport`

### E3 面验证 + 真机 + 收口（E3 的 CI 面派代理；真机主会话）
- webdav：Range GET aead_v2 行 → 206 精确切片（mock）；winfsp：open/read_at/宽限重开（mock）。
- 真机（主会话）：加密百度卷（新路径），winfsp 探针——open 时长不再含整文件下载；冷 seek 每跳 ≈ 一个窗口；哈希比对；热读不变。
- 文档：decisions K47、README 增补、AGENTS 计数、tracker。
- Commit: `feat(streaming): encrypted range reads across faces` + docs commit

## §3 风险

| 风险 | 缓解 |
|---|---|
| PBKDF2 100k 每 open（~几十 ms） | K41 宽限表 + LRU 摊薄；挂账：key 派生缓存 |
| 驱动对 `total_size=u64::MAX` 哨兵的 clamp 行为 | baidu clamp 变 no-op（已核）；包装器自己做密文 span 收口 |
| exact-multiple/尾 chunk 布局边界 | E1 性质测试与 `Layout::derive` 互验 |
| 面 pinned 测试改判 | gcm 默认列值应使旧钉存活；改判逐条记 K47 |
