# Phase 7：WebDAV 存储驱动任务跟踪单

> 计划：`docs/plans/2026-09-17-webdav-driver.md` ｜ 需求口径：负责人 2026-09-17「按推荐方案执行；与既有 phase 同策略（计划+跟踪单+TDD）；编译可选；严格遵循项目规范与约束」；**批准：2026-09-21 负责人实施指令（全程自主执行，批次 WD0–WD5 顺延执行）**
> 基线：main@eda60a7（workspace **1394/0/42**，ignored 42 = 真机/平台/真网类；编号漂移修正——立项裁决用 **K80/K81**，webdav 为**第 7 驱动**，见计划头部留痕）
> 状态：**已批准（2026-09-21）——WD0 进行中**
> worktree：`feat/webdav-driver`，独立 target（共享 CARGO_TARGET_DIR 双指纹既有教训）

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| WD0 | 立项落档 + 真机怪癖 spike（无生产代码） | ⬜ | decisions K80（选型）/K81（D1–D6 拍板）入档；计划状态→已批准 + AGENTS 联动；`examples/webdav_spike/` 双服务器（rclone serve webdav + Apache mod_dav 含 Digest）怪癖矩阵钉死（计划附录 C 回填）；fixture 文档 `phase7-webdav-fixture.md` 落盘（凭据只经 env）；D2/D5 证据复核（反证则修订计划留痕） | — |
| WD1 | 驱动骨架 + 配置接入（无真实网络） | ⬜ | `ck-webdav` 七模块 crate（纯函数层先行：config 六键/URL 构造/mtime 三格式/challenge 解析）；十三处接入点 + feature 三件套（`webdav` 入 default，第 7 驱动）；`compiled_drivers()` 第 7 行——**六驱动既有输出逐字不变**；TDD 纯函数层红→绿 | — |
| WD2 | 手搓注入桩 + 客户端核心 + 读路径 | ⬜ | axum 内存 VFS 桩（RFC 严格建模 + 注入旋钮：401 Digest/stale/畸形 multistatus/200-全量回 Range/412/405/连接杀/lost-ACK）；auth 双形态（nc 单调/uri 转义/引号逗号解析/stale 再协商恰一次）；xml 解析（三格式 mtime/href 解码/命名空间宽容）；读面 list（K67 过滤）/stat/reader 8 MiB 窗口流（206 校验 + 200 截断 + 416 EOF 语义）；负面清单 §4.5 逐条测试钉死 | — |
| WD3 | 写路径 stager + conformance | ⬜ | commit-on-close（spool→PUT .part→MOVE 固化→size 复核→mtime best-effort）；断言①覆盖写形态实测裁决（.ckwd- stash 预案在案，K67 H2 重放窗防线 + lost-ACK 桩注入）；错误映射表 §4.4 全行双桩回放；conformance 八断言（**dav-server 参照桩**注入，RESUME 门控跳过） | — |
| WD4 | 装配接线 + 裁剪组合 | ⬜ | 十三处生效路径 + doctor probe（OPTIONS+auth 五态）+ web 表单/volumes.js/i18n/app.js+system.js 标签表（B-M4 别漏行）+ sync 门控 + `CYDRIVE_WEBDAV_PASSWORD` 入 with_env_overrides（B-M1）；裁剪组合构建（含 `not(baidu)+webdav` twin dispatch 测试，pan115_combo_dispatch 模式）+ K31 文案测试 | — |
| WD5 | WSL2 真机矩阵 | ⬜ | 双服务器（rclone 明文 Basic 全动词 + Apache Digest/PROPPATCH 腿）十腿矩阵：①上传回读逐字 ②Range 跨窗口 ③吞吐 128 MiB ④会话复用 ⑤覆盖写+staging 不可见+abort 恢复 ⑥外部文件可见 ⑦错误腿分类 ⑧digest 全链路 nc 真实递增 ⑨断线白名单自愈 ⑩拒绝腿可行动文案；可选自举冒烟腿（不作判据）；fixture 文档完备 | — |

## 批次日志

（每批收口追加：完成情况 / 红→绿留证 / 未询问的自主裁决及回滚方式 / 执行期发现）

## 风险与未覆盖（随批更新）

- 待 WD0：真实服务器怪癖矩阵未钉（D2/D5 形态以 spike 证据为准）
- 待 WD5：真实广域网链路形态与吞吐（WSL2 回环数字口径，sftp SF5 判例）
