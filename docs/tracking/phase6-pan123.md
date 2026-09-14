# Phase 6：123 网盘存储驱动（pan123）任务跟踪单

> 计划：`docs/plans/2026-09-14-pan123-driver.md` ｜ 需求口径：自用（K59.1）+ 全量公民 + 编译开关 + 零侵入
> 基线：main@c819ee8（workspace 1069/0/12；winfsp 腿 117/0/1）
> 状态：**方案已批准（2026-09-14，K64：D1–D4 全拍板，D1 直裁 web API）——123-0（验证与采样 spike）解锁可开工**
> 前置依赖：与 Phase 4/5 共享的 `compiled_drivers()` 可扩展化重构（谁先到谁做，只做一次）
> worktree：待建（建议 `feat/pan123-driver`）

| 任务 | 内容 | 状态 | 完成情况 | 证据 |
|---|---|---|---|---|
| 123-0 | web API 验证与采样 spike（路线已定 K64） | ⏳ 可开工 | 六项：① web API 形态有效性（pan123-rs ~3 月未动，首项确认）② QR 登录+动态域名 ③ list/info/trash 往返 ④ download_info+Range+**流量限额数字** ⑤ upload 全链（5060/秒传/**duplicate 覆盖行为**/分片/complete）+分片状态保留 ⑥ 密文空 etag；产出 errno/限额/覆盖采样表 | — |
| 123-1 | 认证与驱动骨架 | ⏸ | QR 三端点 + TokenStore + 动态域名发现 + 四件套 + 配置键三处同步 | — |
| 123-2 | 读路径 | ⏸ | list/stat/mkdir/trash/rename + download_info 缓存 + Range 流读 + traffic 预检 + 令牌桶限流 | — |
| 123-3 | 写路径 | ⏸ | upload_request/分片 presign/PUT/complete + duplicate 语义 + MD5 预计算 + 超时自适应 | — |
| 123-4 | conformance + 装配 | ⏸ | 假 123 API + 假 presigned PUT 桩八断言 + 12 装配点 + doctor（token/流量余量）+ web 表单 | — |
| 123-5 | 真机矩阵 | ⏸ | 上传往返/Range 播放/秒传/分片差集/rebuild 限速/加密卷/E2E 三面/流量限额触顶（可选） | — |

## 立项前研究（2026-09-14，已完成）

| 研究 | 结论落点 | 关键收获 |
|---|---|---|
| pan123-rs（MIT，本轮克隆实测） | 计划 §1.1/附录 A.1 | **web 路线 Rust 全套现成**（2862 行：全端点/双头认证/上传三段流/Range 续传/令牌桶）；reqwest 0.12 同栈；签名已简化（随机 key，PCFS CRC32 为备胎）；blocking 形态是移植主项 |
| PCFS（Go） | 计划附录 A.3 | 驱动形态对照 + sign.go 备胎 + 两段下载/IPv4 |
| 开放平台生态（alist 123_open 等） | 计划 §1.2 | 免审秒发但直链空间/VIP 门槛——对挂载+流式播放是硬伤，路线 A 降为备选 |

## 批次日志

- **2026-09-14 拍板收口（K64，方案获批）**：D1 = **web API（直裁，双腿门槛关闭**，路线 A 降为文字参照）；D2/D3 = 沿用 Phase 5 拍板形态（trash 回收站语义不引回收站接口 / 根目录可设置缺省网盘根）；D4 = duplicate 覆盖模式（对齐 writer 契约，rtype=3 先例；覆盖行为 123-0 实测钉表）。**123-0 解锁**，职责收窄为 web API 验证与采样（计划 §6 已改写：首项 = 形态有效性确认——pan123-rs ~3 月未动）。落档四处：decisions K64、计划 §1.3/§6/§8/头部、本表、AGENTS。
- **2026-09-14 立项前置研究完成**（pan123-rs 为专门克隆实测：`E:\GitHub\pan123-rs`，MIT；主仓全程零改动）：
  - 三参照分工定调（计划 §0.1）：pan123-rs = 主路线 Rust 规格书（可移植源）；PCFS = 形态对照 + 签名备胎；alist 123_open = 备选路线参照。
  - **对前轮评估的更新**：web 路线从「直译 PCFS ~ck-baidu 量级」降为「**移植现成 Rust SDK**（主项 = blocking→async 转换）」；新增产品级约束发现——**每日下载流量限额**（非会员；会员 9 元/月解除）与 **MD5 etag 前置**。
  - 计划落库三件：本跟踪单 + `docs/plans/2026-09-14-pan123-driver.md` + decisions K63。
  - D2/D3 建议沿用 Phase 5 已拍板形态（trash 天然回收站语义 / 根目录可设置缺省网盘根），D4 为本阶段新增（duplicate 覆盖语义）。

## 风险与未覆盖（如实记录）

- **未验证项**：全部技术断言来自代码阅读，**零真机验证**——QR 登录、动态域名、Range 206、流量限额数字、upload 全链、分片状态保留（resume 可行性）、5060 行为、密文空 etag 行为，全部属 123-0 spike。
- **未验证项**：pan123-rs 最后提交 2026-06-26（~3 个月前）——web API 形态是否仍有效需 spike 首项确认。
- **产品级约束**：每日下载流量限额对「挂载卷」场景的影响未知（额度数字待实测；负责人会员状态待确认）。
- **待人工决策**：D1 路线（spike 后；初步倾向 B）/ D2 D3（建议沿用 Phase 5 拍板）/ D4 duplicate 语义（计划 §8）。
