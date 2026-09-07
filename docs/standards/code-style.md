# 代码规范（Code Style）

> 状态：v1.0（2026-09-07）｜ 强制级别：门禁拦截
> 来源：rs-CyDrive 质量门禁（M0 起生产验证）；PCFS 正反两面（测试离线化先例 / 注释掉的调试日志残留反例）

## 1. 质量门禁（每批必过，全绿才算完成）

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
```

- 三步门禁**始终 workspace 级**（上述命令原样）；WSL 侧 `check + 相关 crate test` 在跨平台改动（cfg 面/平台 crate/构建系统）时加跑；
- 触碰 core/热路径的批次门禁必须 workspace 级（tier-1 批教训：单 crate 门禁漏检 webdav 断链）；跨 crate enum/trait 演进同理。

## 2. 错误处理

- 库 crate 一律 `thiserror`；bin 顶层 `anyhow`；`Result` 传播，**禁止生产路径 `unwrap`/`expect`/`panic!`**（测试代码不受限）；
- 跨层错误必须分类（见 architecture R2 + interfaces §3）；错误信息**可行动**（告诉用户下一步做什么，例：403 文案指明 config 键名）；
- **bin 入口必须显式消费/校验命令行参数**——未知参数拒绝并退出非零，绝不静默忽略后启动服务（rs-CyDrive 0.5.3 sync-server 教训：`--help` 被当启动跑出残留进程）；
- Mutex 中毒恢复统一模式：`unwrap_or_else(|p| p.into_inner())` + 注释（rs-CyDrive 先例）；
- best-effort 操作（缓存清理等）显式 `let _ =` + 注释声明吞错理由，禁止无声 `ignore`。

## 3. TDD 纪律

- **红→绿留证**：先写失败测试、跑出真实红输出（断言红/超时红优先于编译红），再实现转绿；红绿分 commit；
- **断言零漂移**：已红测试的断言绝不修改；发现断言本身错误 → 停下报告，不改了之；
- 契约变更（默认值/语义演进）属例外，须 commit message 注明裁决依据（decisions.md 条目）；
- 编译必需的机械修（新增字段补全等）允许，但 commit 内逐条列出；
- **测试样本必须来自真机抓取**，不得手写想象格式（rs-CyDrive 两例教训：mount.davfs stderr、net use UNC）；
- 外部命令输出解析类代码必配真机来源的 fixture 测试；
- 真机/真网测试 `#[ignore]` + 命名 `ignored_*` + 注明前置（env/凭据/管理员），CI 不跑但文档列出；
- **离线优先**：mock transport/HTTP 层承载绝大多数测试；驱动实现的协议面用字节级断言（表单格式/头/帧）钉死（PCFS httptest 先例）。

## 4. 平台与并发

- `#[cfg(windows)]`/`#[cfg(unix)]` 隔离；对方平台必须可编译（R5）；
- async 上下文禁止阻塞调用（同步 IO/长 CPU 段落 `spawn_blocking` 或说明）；
- 长命等待必须可取消（select 停机门/超时死线）；SSE/流读取必须有活性检测（0.7.1 High-3 教训：90s 空闲死线）；
- 唤醒/变更通知一律走 MetadataEvent 总线（D8），**禁止新增手工埋点**。

## 5. 提交与分支

- Conventional Commits（feat/fix/docs/test/refactor/chore）；**不加 Co-Authored-By 等署名尾注**；
- 提交信息正文写「为什么」与裁决依据；红绿对的信息注明红证据形态；
- 跨 ≥3 commit 或 ≥5 文件的批用 git worktree 隔离；日常小修直 main；
- 一次会话 ~150 消息或上下文过半 → 收口检查点（commit 干净 + 交接更新）。

## 6. 代码卫生

- 注释密度对齐周边；注释只写「代码自身表达不了的约束与理由」（为何存在/边界/裁决指针），不写「下一行干什么」；
- 调试代码（println/注释掉的日志/手写临时代码）绝不入库（PCFS 反例：通篇注释掉的 log.Printf）；
- `TODO` 必须带锚（`TODO(batch-x)` / decisions 条目），无锚 TODO 视为未完成工作；
- 死代码/被替代的实现随批删除（PCFS 反例：driverfs/memory/multifs 三代并存）；
- 公共 API 变更与文档更新同批完成（见 documentation.md）。

## 7. 性能纪律

- 热路径（VFS 读/db 查询/分块）改动附带量级评估；列表/目录操作禁止 O(n²) 嵌套扫描（0.5.2 push_diff 教训）；
- 大文件路径禁整文件缓冲（内存/磁盘双面）——流式优先，常量内存为目标（加密 v2 设计前提）；
- 基准类测试 `#[ignore]` 按需跑，结果入 docs/reports。
