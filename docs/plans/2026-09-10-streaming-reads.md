# 流式读（Streaming Reads / Range 直通）执行计划

> **For Claude:** REQUIRED SUB-SKILL: executing-plans 编排执行（hub-and-spoke，TDD 红→绿留证）。

**Goal:** baidu/local/telegram 卷的 WebDAV 读与 /api/download 从「整文件水合后供本地文件」改为 **Range 直通流式**（请求哪段拉哪段，4MiB 对齐窗口 + 顺序预取）；加密行与无 range_read 能力的驱动保持全量水合回退（R-5）。

**输入研究**（2026-09-10 三轮：PCFS 深研 / 本仓接缝盘点 / rclone+alist+JuiceFS 互联网验证，全文在会话与 decisions 入档摘要）：
- PCFS 机制=驱动句柄 Seek 重发 Range（无落盘/无预取/128KB 缓冲）；其 baidu 每次 Seek 重走 302+新建 client 是反面教材——本仓 dlink 缓存+有界窗口+背压更优，保留
- 本仓 open_range 三驱动就绪（baidu download.rs:93-147 有界 4MiB+两段 fallback；local 原生；telegram 按 chunk 窗口 :406-449）；dav-server 读契约=metadata 定长→Range 解析→seek→read_bytes 循环（16KiB/次）；R-5 注释（webdav lib.rs:118-130）预留了能力位检查门；sync-server events.rs:385 有 Body::from_stream 先例
- rclone 上限 10MB Range / JuiceFS readahead / alist 本地代理——4MiB 对齐窗口+顺序预取设计与社区实践一致

## 裁决 K33–K37

| # | 裁决 |
|---|---|
| K33 | 流式=能力位门控直通：`range_read=true` 且行**非加密** → RangeFile/流式 Body；否则（含 v1 GCM 行、能力关、0 字节行）回退现有 hydrate 全量路径（R-5，`get_range_without_range_read_capability_still_slices` 测试必须保持绿）。telegram 卷按 chunk 粒度直通（chunk 小则等效流式；加密 telegram 行仍走 hydrate） |
| K34 | 窗口策略：RangeFile 内部按 **4MiB 对齐窗口**（与 baidu 有界分片天然对齐）拉取并缓冲，dav-server 的 16KiB read_bytes 从缓冲切片；**seek 惰性**（只记逻辑位置，不发网络请求），下次 read 时按新位置开窗——「开窗即预取」，无额外投机预取；流式读**不进磁盘缓存**（is_cached/evict 全不触碰；baidu 有 DlinkCache 60min 兜底重复开窗成本）。后续块缓存/预取深化挂账不阻塞本批 |
| K35 | v1 GCM 整文件 AEAD 永不直通（硬约束 vfs.rs:560-563）；row.size 对 baidu/local 是权威索引值（rebuild 自后端 list），可直接作 Content-Length |
| K36 | /api/download 流式化：单 Range 206 + 显式 Content-Length/Content-Range + Accept-Ranges（沿用 parse_byte_range/resolve_byte_range 语义与 416 行为，基于 row.size 判定）；Body::from_stream（照 sync-server FramePipe 形态，transport ByteStream 的 Err 满足 BoxError）；hydrate 回退共用旧 download_response |
| K37 | 非目标（维持 hydrate）：bot /get（bot.rs:229）、pull 命令（lib.rs:1623）；CacheManager 接口不动；dav-server read_buf_size 不动（内部聚合已消除 16KiB 开销） |

## 批次（worktree feat/streaming-reads；每批 TDD 红→绿 + 三步门禁）

- **SR0 接缝与观测面**：MockTransport 增 open/open_range 调用记录（观测面，两层测试依赖）；Vfs 抽「行→RemoteHandle」helper（hydrate 与 delete_remote_gated 两处内联去重）；新增 `Vfs::open_read(rel) -> Result<ReadStream, VfsError>`——能力位+加密+0 字节三重门（不满足返回可识别的 Fallback 信号或直接内部回退 hydrate，形态自定记回传），满足则组装 RemoteHandle 返回带 (total_size, transport, handle) 的流式读句柄。测试：vfs 单测（门控三态+句柄可用性，Mock 断言远端只见窗口请求）
- **SR1 WebDAV RangeFile**：新 DavFile 实现（metadata=RowMetaData/seek 惰性/read_bytes 窗口缓冲切片/write Forbidden/flush Ok；窗口耗尽按新逻辑位置 open_range 续窗）；open 读态分派（K33 三重门 → RangeFile | HydratedFile）；StorageError→FsError 映射补全。测试：fs_adapter 直调用例（seek/read 序列/窗口边界）+ smoke「Range GET 时 transport 只见有界 open_range 不见全量 open」+ 既有 R-5 回退测试保持绿 + multivolume 前缀形态回归
- **SR2 /api/download 流式**：K36 全项。测试：web_e2e 流式 206（精确切片+CL/Content-Range 头）+「不全量拉取」（MockTransport 观测面断言 open_range 窗口而非全量）+ hydrate 回退路径回归（加密行）
- **SR3 真机验收 + 收口**：真 baidu 视频三面验收（Z: 盘 PotPlayer 直播、仪表盘 URL 流式、前端 <video> 首帧时间对比）+ 长视频 seek 行为 + decisions K33–K37 入档 + README/AGENTS/onboarding 联动 + tracker 全绿 + merge main + push

## 纪律

默认行为零漂移面：非流式路径（hydrate 回退、bot、pull、上传链）不动；既有 810 测试断言零改动；红证据=目标行为下的真实红输出。
