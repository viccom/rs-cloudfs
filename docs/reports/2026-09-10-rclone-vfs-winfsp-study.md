# rclone VFS/缓存/WinFsp 深研档案（Phase 3 前置研究）

- **日期**：2026-09-10 ｜ **方法**：三只读代理并行深研 rclone master@ef69687（clone 于 `E:\Rs_Codes\_refs\rclone`，长期参照目录，与 PrivateCloudFS 同等地位）
- **背景**：Phase 3 立项 WinFsp 类本地盘挂载前的巨人肩膀研究；本仓已有流式读基座（open_range 4MiB 窗口直通 + staged 写提交，K33–K37）

## 一、VFS 核心层（vfs/）

- **三层架构**：Node(os.FileInfo+Open) / Handle(完整 os.File 方法集) / VFS 持根——挂载层只做三件事：持 VFS、fh 句柄表（槽位复用）、flag/errno 翻译（cmd/cmount/fs.go:48-97,579-638）。策略全部收敛在 core 层 `File.Open` 的「打开意图 × 缓存模式 × 缓存命中」28 行分派表（vfs/file.go:885-912）。
- **读句柄**：懒打开顺序流；seek 三级——①预读缓冲内 SkipBytes 免网络（read.go:122-127）②RangeSeek 记新偏移下次读时重发 Range 请求（chunkedreader/sequential.go:125-160）③错误后整流重建；Seek() 本身零网络。乱序读 20ms 容忍窗（--vfs-read-wait）。chunk 128MiB 起指数翻倍。
- **写句柄**：off 模式=io.Pipe 流式上传（不可 seek、必须 O_TRUNC）；writes/full 模式=RWFileHandle 走磁盘缓存稀疏文件，close 后 5s 延迟写回。
- **Flush 零副作用原则**（read_write.go:189-198 "Flush can be called multiple times"）：Flush 只 updateSize，持久化全挂 Release——Windows/播放器高频 Flush 的生命线。
- **句柄宽限期** --vfs-handle-caching 5s（vfscache/item.go:703-752）：close 后不真关，重开免重建——专治播放器/杀毒"关了马上再开"。
- **上传中打开**：waitForValidObject 最多等 5s（file.go:575-590）。
- **并发**：File.muRW 串行化 RW 打开/关闭/Remove；锁顺序成文（file.go:23-39）；延迟 rename（写者打开时挂起）。

## 二、磁盘缓存子系统（vfs/vfscache/）

- **模型**：每文件 Item=稀疏文件（Windows FSCTL_SET_SPARSE）+ JSON sidecar 元数据（ModTime/Size/Fingerprint/Dirty + **Rs 区间表**——排序自动合并的已缓存区间列表，lib/ranges）。随机读判定=Rs.Present(r)。
- **miss 协同（对最关键问题的答案）**：**无独立"直通旁路"**——miss 时启动顺序流式下载（chunkedreader 窗口=max(buffer-size,1MiB)+read-ahead），**每落盘一段就 kickWaiters 唤醒区间已齐的读请求**（downloaders.go:387-408,466-473）——首字节≈TTFB 且数据进缓存，复看零网络；请求落在在途流窗口 [start, offset+window) 内则复用流不开新 GET（downloaders.go:342-356）。
- **写回**：expiry 最小堆队列 + 单飞；上传期间再写=取消在途+重排队（writeback.go:267-272,393-412）；整文件 Copy 重传（比我们的分块 staged 弱——保留我们的）。
- **空间**：双配额（max-size + min-free-space）；三级清理（超龄 purge→超配额按 ATime 逐删→在用干净文件 Reset）；脏元数据崩溃恢复（启动 reload 续传）。
- **解耦**：vfscache 零 FUSE/WinFsp 依赖，同一缓存层服务全部挂载/serve 后端——cloudkit-winfsp 复用同一缓存层的活证据。
- **自认坑**：锁序地狱三条全序规则；dirty 文件 Close 阻塞补齐全文件；Windows 同尺寸 truncate 跳过防 Defender 重扫；exFAT 无稀疏支持性能极差。

## 三、WinFsp 集成层（cmd/cmount + mountlib）

- **路线**：Windows 上唯一路径=cgofuse→WinFsp **FUSE 兼容层**（为 6 平台复用回调代码）；官方 release CGO_ENABLED=0 运行时动态加载 winfsp-x.dll。**为复用付出的兼容层税**：atomic_o_trunc 失效、`\?\` 全不支持、1601 纪元时间过滤、Init 早于挂载可见、POSIX→ACL 失真（靠 WinFsp 1.9 SDDL FileSecurity 补丁）——单平台项目直绑 native API 可避开大半。
- **Windows 特化**：--network-mode（SMB 仿真盘，**关掉 Explorer 缩略图扫描**）经 VolumePrefix 实现；volname 32 字符截断；盘符自动选择跳 A/B/C；inode=进程内自增；ReaddirPlus（readdir 一次带全 stat，消灭 getattr 风暴——Explorer 最大单点优化）；Utimens 过滤 1601；Statfs 结果缓存防右键属性打爆。
- **生命周期**：mount 返回前后各 10s 轮询挂载点出现/消失（就绪竞态）；Unmount 无 retry/force（句柄占用=用户责任，宽限期解决大半）；--daemon Windows 不支持（前台+服务托管）；重复挂载靠 RC liveMounts + Windows 兜底。
- **错误面**：translateError 单点（未知→EIO+日志）；Mknod/Link/xattr 显式 ENOSYS；**无回调超时包装**——策略是"减少回调+缓存一切"（attr 1s/目录 5m/Statfs 缓存）。
- **血泪清单**：DLL 缺失运行时 panic→recover+安装提示；Destroy 不保证送达→atomic 标志；提权会话盘符普通会话不可见；多挂载 VolumePrefix 必须唯一；缓存目录两进程共享会损坏。

## 四、对 rs-cloudfs 的汇合裁决输入

1. **直绑 native（winfsp-rs）优于 FUSE 兼容层**（单平台；rclone 的兼容层税清单=反面教材）
2. **"窗口直通"与"整文件水合"应统一为区间账本块缓存**（Rs 模型：水合=填满 [0,size) 特例；直通=账本未覆盖的流式路径）——Phase 3.5 候选
3. **Flush 零副作用 + Release 提交 + 句柄宽限期** = WinFsp 存活三件套
4. **薄挂载层**：适配器只做句柄表+翻译，策略在 core
5. **Explorer 防打爆**：readdir 带 stat + 元数据走本地 db + network-mode 逃生舱
6. 许可：winfsp-rs 纯 GPL-3.0 无 FLOSS 例外 vs 本仓 deny.toml MIT-only——feature 门控隔离（默认关）
