# Phase 2.5 任务跟踪单（多卷启用 · Volume Registry）

> 计划：docs/plans/2026-09-08-phase2-5-multivolume.md ｜ 裁决：K19–K29
> 纪律：每批 TDD 红→绿留证；门禁=三步+check_layers+scan_secrets；单卷回归零漂移为每批验收项；每批收口更新本表并随 commit 提交。
> worktree：`feat/phase2-5`（批次提交隔离，MV5 收口 merge 回 main）。

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| MV0 | 配置与发现：volumes_dir 键、卷文件解析（键分类/卷名/互斥/冲突校验）、cli discover 接线 | ✅ 完成 | commit 02ae147：键二分（PROCESS 6/VOLUME 29）+VolumeConfig/discover_volumes/load_volumes/混用检测+DiscoveredConfig{Single,Multi}（Multi 进 run 得 MV1 明确错误）；新测试 25（core 17+cli 8），22 真红→绿+3 声明性 | 两 crate 454 passed/0 failed（基线 429 零漂移）；clippy 0 警告；fmt clean；check_layers OK 11 manifests；scan_secrets OK |
| MV1 | Registry 装配核心：VolumeRegistry、run 多卷循环、RunHandle 聚合、失败可见降级、session/baidu 基准目录 | ✅ 完成 | commit b9c7516：VolumeRegistry/VolumeRuntime/VolumeStatus（K22 降级+全败 Err）；run_multi_with_transports 测试注入面（K21 卷主目录/K25 单 stop gate+进程控制面/K26 逐卷 sync）；telegram session 与 baidu sessions_dir 跟随注入基准；单卷流程 run_single_volume/connect_telegram_volume 原样抽出；盘符冲突 presence 化（未声明不参战，K27）；MV0 占位门随测试退役。执行注记：子代理额度切断后主会话接手收尾（编译错 2+借用错 1+契约差 2：shutdown→Result、Debug impl；presence 语义为接手期裁决入批注） | workspace 770 passed/0 failed（741+25+6−2 账目吻合）；multivolume_e2e 6/6；clippy -D warnings 零警告；fmt clean |
| MV2 | WebDAV 单端口 /vol/<name> 前缀路由 + 逐卷挂载 + MiniRedir 子路径真机探针（K20B 回退门） | ⬜ 未开始 | — | — |
| MV3 | 仪表盘多卷：/api/volumes、卷参数 API、前端切换 tabs+汇总、16 键冻结回归 | ⬜ 未开始 | — | — |
| MV4 | CLI 运维面（volumes/doctor/status/setup）+ 文档联动 + K 裁决入档 | ⬜ 未开始 | — | — |
| MV5 | E2E 硬验收（单进程三卷真机 V/Y/Z）+ 清理 + merge/push 收口 | ⬜ 未开始 | — | — |

## 批次日志

- 2026-09-08：Phase 2.5 立项。研究两轮（PCFS 实例模型 + 本仓装配面盘点）完成，计划与跟踪单落盘。
- 2026-09-09：MV0 收口（02ae147）。取舍：validate 多卷分支落 discover/load 组合链而非 CyDriveConfig::validate()（IO/presence 语义）；卷语义校验（凭据链后）留 MV1；VolumeConfig.base_dir=volumes 目录，K21 卷主目录由 MV1 计算。
- 2026-09-09：MV1 收口（b9c7516）。子代理被额度上限切断（改动未提交、不编译），主会话按 Phase 2 先例接手收尾。接手期裁决（均可逆）：① shutdown 返回 Result（stop task join 失败冒泡，sync task abort 后非取消错误仍 warn——与单卷语义一致）；② drive_letter 冲突检测 presence 化——未显式写盘符的卷不参与冲突（默认 "Y:" 是占位非挂载声明），挂载决策归 MV2；③ MV0 的 ensure_single_volume 占位门及其 2 测试随真装配落地退役（doc 注释说明）。
