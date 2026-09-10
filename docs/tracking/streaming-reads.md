# 流式读 任务跟踪单

> 计划：docs/plans/2026-09-10-streaming-reads.md ｜ 裁决 K33–K37
> worktree：`feat/streaming-reads`（收口 merge 回 main）。

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| SR0 | MockTransport 观测面 + RemoteHandle helper + Vfs::open_read 接缝 | ✅ 完成 | commit 72879d5：open_calls/open_range_calls 观测面（connect 门拒调也记录）；HandlePolicy{Read,Delete} 集中 hydrate/delete_remote_gated 四处差异去重（语义逐字保留）；StreamSource::{Stream{handle,total_size,transport},Hydrate} + open_read 三重门（加密/能力关/0 字节→Hydrate；缺密码→MissingPassword 与 hydrate 一致）；mock capabilities 旋钮复用现有 builder 无新面 | 新测试 11（vfs_open_read 8+transport 3）；workspace 821 passed/0 failed（810 基线零漂移）；clippy/fmt clean |
| SR1 | WebDAV RangeFile + 窗口聚合 + 三重门分派 | ✅ 完成 | commit 8a71dd1：RangeFile（Window{start,Bytes} 缓冲+pos 单事实源；seek 惰性同位 no-op；read_bytes 按需 open_range(pos,min(4MiB,total-pos)) 整窗聚合零拷贝切片；EOF 空读零网络；write Forbidden）；open 读态 K33 分派（Stream→RangeFile，Hydrate/Err→原 hydrate 路径）；storage_err 映射表（NotFound→404 其余→500）+with_stream_window 测试旋钮 | 新测试 12（fs_adapter 6/smoke 4/multivolume 1+单测 1）；workspace 833 passed/0 failed（821+12）；R-5 回退钉测试原样绿；clippy/fmt/check_layers/scan_secrets clean |
| SR2 | /api/download 流式 Body + 头语义 | ✅ 完成 | commit cfeedc1：api_download 走 open_read 分派（Stream→流式：200/206 显式 Content-Length+Accept-Ranges+Body::from_stream 三态 RangeBody；416 基于 total_size 零远端调用；Hydrate→hydrate_download 原路径 verbatim 搬移）；回退路径 download_response 补 Accept-Ranges；MockTransport 增 OpenRangeAction 脚本面（Ok/Fail/FailAfterBytes） | 新测试 8（web_e2e 6/multivolume 1/transport_traits 1）；workspace 841 passed/0 failed（833+8）；既有 download_range_serves_206_slice 零改动兼容；clippy/fmt/check_layers/scan_secrets clean |
| SR3 | 真机验收（PotPlayer/仪表盘/前端）+ 收口 | ✅ 完成 | 764MiB 真视频三面验收：API 流式 206 首字节 <1ms/1MiB 全程 159ms/中部 seek（600MB 偏移）163ms；WebDAV 端口 206 326ms 字节与 API 一致；mp4 ftyp 合法。客户端发现：Windows WebClient FileSizeLimitInBytes 默认 50MB 是 Z: 盘大文件客户端限制（0.4MB 成功/68.8MB 失败实证）——URL 路径不受限，Z: 盘需机器级注册表调整（decisions 记录修复命令）。K33–K37 入档 decisions；README/AGENTS 联动 | 本表+decisions；merge main 收口（主会话） |

## 批次日志

- 2026-09-10：立项。三轮研究完成（PCFS/本仓接缝/互联网同类），K33–K37 裁决定稿，计划与跟踪单落盘。

- 2026-09-10：SR3 收口。流式读全批完成（72879d5→8a71dd1→cfeedc1），真机验收过，merge main。
