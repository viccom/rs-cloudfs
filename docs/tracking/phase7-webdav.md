# Phase 7：WebDAV 存储驱动任务跟踪单

> 计划：`docs/plans/2026-09-17-webdav-driver.md` ｜ 需求口径：负责人 2026-09-17「按推荐方案执行；与既有 phase 同策略（计划+跟踪单+TDD）；编译可选；严格遵循项目规范与约束」；**批准：2026-09-21 负责人实施指令（全程自主执行，批次 WD0–WD5 顺延执行）**
> 基线：main@eda60a7（workspace **1394/0/42**，ignored 42 = 真机/平台/真网类；编号漂移修正——立项裁决用 **K80/K81**，webdav 为**第 7 驱动**，见计划头部留痕）
> 状态：**WD1 完成（2026-09-21）——WD2 待开工**
> worktree：`feat/webdav-driver`，独立 target（共享 CARGO_TARGET_DIR 双指纹既有教训）

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| WD0 | 立项落档 + 真机怪癖 spike（无生产代码） | ✅ 2026-09-21 | decisions K80/K81 入档（main `b2d8b67`）；计划落库+批准状态（main `c4a042d`）+ AGENTS 联动（main `da2c4ed`）；`examples/webdav_spike/`（5 模块 + digest/xml 可移植资产）双服务器 11 项怪癖矩阵钉死（附录 C 回填）；fixture 文档落盘（凭据只经 env）；**D2 修订**（generic 只读 mtime，预设降级路径触发）+ D5 维持留痕 | 本批日志 |
| WD1 | 驱动骨架 + 配置接入（无真实网络） | ✅ 2026-09-21 | WD1a：`ck-webdav` 十文件 crate（config/urls/mtime/auth/xml 纯函数层 + client/driver/stager/transport_face 骨架；能力位 R4 逐位依据注码）；45 测试红（42 失败）→绿。WD1b：十三处接入 + **编译器揭出 3 处清单外**（CyDriveConfig 六字段/穷举 round-trip 测试/run_sync_command namespace 臂）全补；feature 三件套 + `compiled_drivers()` 第 7 行（六驱动输出逐字不变，pinned 测试零漂移）+ K31 文案 + twin 双臂 + doctor 骨架臂 + B-M1 env 路由 + web 前端四文件；16 文件 +1048/−25（−25 限 compiled_drivers 测试 cfg 守卫机械重写，pan123 先例） | 本批日志 |
| WD2 | 手搓注入桩 + 客户端核心 + 读路径 | ⬜ | axum 内存 VFS 桩（RFC 严格建模 + 注入旋钮：401 Digest/stale/畸形 multistatus/200-全量回 Range/412/405/连接杀/lost-ACK）；auth 双形态（nc 单调/uri 转义/引号逗号解析/stale 再协商恰一次）；xml 解析（三格式 mtime/href 解码/命名空间宽容）；读面 list（K67 过滤）/stat/reader 8 MiB 窗口流（206 校验 + 200 截断 + 416 EOF 语义）；负面清单 §4.5 逐条测试钉死 | — |
| WD3 | 写路径 stager + conformance | ⬜ | commit-on-close（spool→PUT .part→MOVE 固化→size 复核→mtime best-effort）；断言①覆盖写形态实测裁决（.ckwd- stash 预案在案，K67 H2 重放窗防线 + lost-ACK 桩注入）；错误映射表 §4.4 全行双桩回放；conformance 八断言（**dav-server 参照桩**注入，RESUME 门控跳过） | — |
| WD4 | 装配接线 + 裁剪组合 | ⬜ | 十三处生效路径 + doctor probe（OPTIONS+auth 五态）+ web 表单/volumes.js/i18n/app.js+system.js 标签表（B-M4 别漏行）+ sync 门控 + `CYDRIVE_WEBDAV_PASSWORD` 入 with_env_overrides（B-M1）；裁剪组合构建（含 `not(baidu)+webdav` twin dispatch 测试，pan115_combo_dispatch 模式）+ K31 文案测试 | — |
| WD5 | WSL2 真机矩阵 | ⬜ | 双服务器（rclone 明文 Basic 全动词 + Apache Digest/PROPPATCH 腿）十腿矩阵：①上传回读逐字 ②Range 跨窗口 ③吞吐 128 MiB ④会话复用 ⑤覆盖写+staging 不可见+abort 恢复 ⑥外部文件可见 ⑦错误腿分类 ⑧digest 全链路 nc 真实递增 ⑨断线白名单自愈 ⑩拒绝腿可行动文案；可选自举冒烟腿（不作判据）；fixture 文档完备 | — |

## 批次日志

### WD1（2026-09-21，a+b 两子批，worktree feat/webdav-driver）

**完成**：WD1a 子代理（crate 骨架+纯函数层）+ WD1b 子代理（组合根接入面），主会话 diff 审查后合批提交。

- **WD1a**：`crates/drivers/ck-webdav`（Cargo.toml workspace 继承 + 十源文件 2528 行）——config（六键纯函数解析 + K31 文案）/urls（穿越防御：raw+解码双检 `..`/`.`/`\0`、双斜杠折叠、字面 `%`→`%25`、collection_url 尾斜杠、Destination 恒绝对 URI）/mtime（三格式 + RFC 9110 两位年边界 + 月长校验）/auth（spike digest 移植 + AuthState D1 骨架）/xml（spike 移植 + href 反转义/百分号解码 + 非 2xx propstat 不进投影 + `is_addressable_name` K67 过滤）/client（reqwest 构建 + 超时分层三常量 + 九动词 `TODO(wd2)` 占位）/driver（九方法骨架 + 能力位 R4 逐位依据 + quota None）/stager/transport_face。**TDD**：红 `3 passed; 42 failed` → 绿 `45 passed; 0 failed`（红相位 3 个天然绿负向断言，pan123 先例同型）。
- **WD1b**：16 文件 +1048/−25。锚点复核（附录 B 漂移表入批报告——web 表单实际在 templates/ 非 static/）；六处 config 清单 + validate 块（query/fragment 拒收为计划外超集，K31 精神）；sync 门控 `!matches!(Local|Sftp)` 形态下 Webdav 天然 true（零改写 + 文档段 + 测试钉——零漂移优先）；feature 三件套 + WEBDAV_DRIVER_REQUIRED + DRIVER_ROWS 第 7 行 + BackendTransport::Webdav 五臂 + dispatch_unified_backend_volume 单 match 臂 + twin 双臂 + build_driver 双臂 + doctor 骨架臂（`TODO(wd4)`）+ with_env_overrides `CYDRIVE_WEBDAV_PASSWORD`（B-M1）+ resolve_volume_settings 零改动复核 + web 四文件。
- **编译器揭出 3 处清单外接入点**（穷尽性机械证明）：CyDriveConfig 六 serde 字段 / tests/config.rs 穷举 round-trip / run_sync_command namespace 臂（baidu 式）。

**验证**：`cargo test --workspace -j 4` = **1455/0/42**（1394 基线 + 45 + 16；既有断言零漂移）；clippy/fmt/check_layers（16 manifests 7 drivers）/scan_secrets 绿；裁剪两腿 `local,baidu` / `webdav` 构建过 + off-feature K31 测试实跑绿。

**自主裁决（未询问）**：①factory 签名按仓库先例 `async fn factory(&WebdavParams) -> Result<Arc<WebdavDriver>, StorageError>`（指令模板与仓库形态冲突时从仓库）；②rand 0.10（workspace 无 [workspace.dependencies] 节，pan115 运行时先例）；③tempfile 为运行时依赖（stager 字段需要）；④`webdav_accept_invalid_certs` core 侧 `Option<bool>`（web 表单 bool 链 serde 兼容），宽容解析留驱动侧 map 面；⑤sync 门控零改写（文档+测试钉替代显式臂）；⑥`https:///dav/` 被 url crate 解析为 host="dav" —— 空 host 测试改用 `https://:5006/dav/` 真错误形态。回滚 = revert 本批 commit。

**挂账（WD4/审查批裁决）**：①webdav_url 理论可含 userinfo（`https://u:p@h/`）——SHOW 回显与 sync namespace 泄漏面（SECRET 清单只含 webdav_password）；②WD1a 骨架 `#[allow(dead_code)] // WD1a 骨架` 锚 18 处（WD2 接线后移除）。

### WD0（2026-09-21，worktree feat/webdav-driver）

**完成**：立项三 commit 落 main（计划+跟踪单 `c4a042d` / decisions K80+K81 `b2d8b67` / AGENTS 联动 `da2c4ed`）→ WSL2 双服务器 fixture（rclone serve webdav v1.60.1 :8080 Basic + Apache 2.4.58 mod_dav :8081 Digest + /dav-stale 2s nonce 腿）→ curl 双轮探针 + `examples/webdav_spike` Rust 化矩阵（十腿+quota+cleanup，reqwest+quick-xml+md-5+httpdate 依赖面同驱动计划）→ 附录 C 回填 11 项 + fixture 文档 → D2 修订/D5 维持。

**怪癖矩阵要点**（全文 = fixture 文档）：
- **rclone MKCOL 已存在→201**（幂等成功陷阱）→ mkdir 必须 stat 预检（baidu 同型先例）
- **apache 集合 URL 无尾斜杠→301 不执行**（PROPFIND/MKCOL/DELETE/MOVE 全动词）→ 集合操作恒带尾斜杠；rclone 尾斜杠全不敏感
- **rclone 目录 MOVE 后 VFS 缓存不可见窗 ≈5min**（子项 404/500、窗内 DELETE 204 撒谎留残；数据已落盘；`--vfs-cache-mode writes` dir-cache-time 缺省）——spike Rust 腿新揭，curl 探针未见
- **apache 倒序 Range→200 全量**（忽略 Range）→ 200 截断回退实证必需
- **digest**：challenge 引号感知解析必要性实证（stale=true 位置不定）；**apache 不查 nc 重放**；过期→401+stale→新 nonce 重算恰一次恢复（D1 全链路真机验证 ✓）
- rclone 缺 Overwrite 头+目标存在→412（偏离 RFC 缺省 T）；apache 相对 Destination→400 / rclone 接受
- quota RFC4331 双 404 → None 降级实证；chunked PUT 双接受（D5 维持，样本仅二）
- 副发现：rclone 解析器容忍畸形 XML 声明、apache expat 400——请求体构造必须严格良构

**自主裁决（未询问）**：
1. **D2 降级**（generic 只读 mtime）——计划 §8 预设修订路径，触发条件实证成立（两家均不能真写 mtime）；nextcloud X-OC-Mtime 搭车保留。回滚 = 恢复 D2 原文。
2. **连接形态 = VM-IP 直连**（Windows→WSL2 localhostForwarding 本机当前失效，实测 000；VM IP 经 `hostname -I` 取）——fixture 文档记录；若后续 forwarding 恢复可换 127.0.0.1，无代码影响。
3. **D5 维持**（不因双样本升格 chunked 为默认）——保守可逆。
4. spike 增补 futures-core 声明（trait-only，reqwest stream 特性本就拉入，零新增编译产物）。

**执行期坑（沉淀）**：①`--noproxy *` 在 shell 变量展开被 glob（两轮探针假信号根源——unset 代理 env 或数组传参）；②Git Bash→wsl.exe 内联复杂命令引号被吞（AGENTS 已有明训，A/B 验证时再踩——.sh 路线不可省）；③htdigest 640 root:root → apache worker 读不到 → 500（chown root:www-data）；④WSL bashrc 的 curl 包装函数对探针产生双跑噪音（代码后缀为真值，已交叉验证）。

## 风险与未覆盖（随批更新）

- ~~待 WD0：真实服务器怪癖矩阵未钉~~ **已钉（附录 C）**；新增挂账：**rclone 目录 MOVE 不可见窗**（WD5 矩阵断言设计须避开 rename 后立即 list 的断言形态；驱动层不做补偿——服务器缓存行为非协议语义）
- chunked PUT 普遍性：仅双样本（挂账维持，D5）
- nextcloud vendor 路径（X-OC-Mtime）：fixture 无 Nextcloud 实例，未实证（零成本搭车，挂账）
- 待 WD5：真实广域网链路形态与吞吐（WSL2 回环数字口径，sftp SF5 判例）
