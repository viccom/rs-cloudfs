# 日志规范（Logging Standards）

> 状态：v1.0（2026-09-07）｜ 强制级别：review 拦截
> 来源：rs-CyDrive tracing 体系 + 0.6.0/0.7.x 日志批实证；PCFS 反例（注释掉的 log.Printf 残留）；sync-server「无日志不可诊断」用户反馈（0.6.0 动机）

## 1. 框架与形态

- 一律 `tracing`（库 crate）；结构化字段一行式：`tracing::info!(key = value, "消息")`；
- **库 crate 禁 println!**；CLI 一次性命令（setup/stats/sync 等）沿用 println 补偿惯例（不初始化 subscriber 的场合）；
- 服务进程缺省 `RUST_LOG=info`（EnvFilter 回退值，sync-server 0.6.0 教训：缺省 ERROR 让服务「像没启动」）；显式设置优先。

## 2. 级别语义（严格执行）

| 级别 | 用途 | 例 |
|---|---|---|
| error | 数据/服务受损或操作失败需人介入 | 500、持久化失败 |
| warn | 降级但自愈/可诊断的异常；**所有周期任务失败**（sync pass 失败、SSE 重连） | 403 重试、驱动退避 |
| info | 生命周期与一次性运维事件 | listening on / mounted / pass complete / 订阅建立 |
| debug | 细节（字段值、决策依据、origin id） | 帧解析、唤醒来源 |
| trace | 仅协议字节级调试 | — |

- 周期性事件的 info 必须节流（每 pass 一条完成行，不逐行打）；高频路径只 debug。

## 3. 脱敏（红线）

- **凭据类绝不入日志**：token/secret/key/Authorization/cookie/口令——含错误消息与 URL 查询串（PCFS `client.go:217-228` 脱敏先例）；
- 长 ID（namespace/client_id/fs_id）截前 **8 字符**（rs-CyDrive 服务端惯例）；完整 ID 仅 debug 且仍非凭据；
- serde 错误回显警惕：错误消息可能内嵌请求字段值——400 路径只回显位置信息（line/col）不回显值（sync-server 审查 Low 项的规约化）；
- panic 消息同受此约束（args 解析错误不含参数原值除非无害）。

## 4. 标准行格式

- **请求/操作完成行**：`<动作> <结果>, endpoint/name = X, 关键计数…, elapsed_ms = N`（sync-server push/pull 行先例）；
- **拒绝行（warn）**：动作 + 拒绝原因 + 可行动指引；绝不回显被拒凭据；
- **连接生命周期**：建立（info，含对端标识截断）/ 断开原因（warn）/ 重连成功（info + 补偿动作声明）；
- 启动横幅：版本、工作目录、监听地址（**实际绑定地址**，非配置值——0.5.2 教训）。

## 5. 可观测性扩展点

- 指标（Prometheus 形态）为 Phase 3 项（PCFS 先例参考），落地时字段命名与本规范计数字段对齐；
- 日志不承担审计职责（家庭级）；但凭据加载/刷新事件必须留 info 级足迹（不含值）。
