# 架构约束（Architecture Constraints）

> 状态：v1.0（2026-09-07 建仓「规范先行」批）｜ 强制级别：**违反即返工**
> 来源：`docs/plans/2026-09-07-cloudfusion-foundation.md` §2/§4；rs-CyDrive crate 边界纪律；PCFS 反例（`lazy_multifs.go:4` 抽象泄漏）

## 1. 六层架构与依赖方向

```
L5 应用层  cli / webdav / web / bot(telegram) / (未来: desktop, 其他网关)
L4 服务层  upload-queue / sync-engine / cache / crypto / inbound / notify
L3 领域层  vfs / metadata-db / MetadataEvent 总线
L2 存储抽象 storage: StorageDriver trait + Capabilities + StorageError + Volume/Registry + conformance kit
L1 驱动层  drivers: telegram / baidu / local / (未来: 115/123/s3…)
L0 基础    http(代理/IPv4/连接池) / keyring / logging / config
```

**依赖方向规则：高层 → 低层**；同层 crate 间禁止相互依赖，**唯一豁免 = 组合根**（cloudkit-cli 作为装配层可依赖同层全部 crate）。

> **生效时点与过渡豁免（§1.5）**：本条对 crate 依赖图的完全生效 = Phase 1 R 批（StorageDriver 落地）后；cloudkit-core 为 L3+L4 合体 crate，其内部跨层引用（如 hydrate 消费 crypto）属声明的结构性豁免——crypto 定位为 L2 侧基础能力，被 core 消费不算违规。

## 1.5 过渡豁免清单（随批次消除）

| 现存偏差 | 消除批次 |
|---|---|
| ck-telegram（现 cydrive-telegram）→ core 反向依赖（trait 在 core） | Phase 1 R（trait 迁 L2） |
| crate 名仍为 cydrive-*，R1 的 crate 边界机械保障未就位 | Phase 0（改名 + `scripts/check_layers` + CI） |
| Capabilities 声明暂无法满足 R4 的 conformance 前置 | Phase 2（套件随 ck-local 建立；此前能力位以驱动单测+真机为准并注明） |
| cloudkit-core 跨层合体（L3+L4） | 长期接受（拆分成本 > 收益，北极星裁决） |

## 2. 硬性红线（违反 = CI/review 拒绝）

| # | 红线 | 理由/来源 |
|---|---|---|
| R1 | **L2 及以上禁止 import 任何 L1（drivers/*）符号** | PCFS 教训：FS 层 import baidu driver 做错误断言，抽象永久泄漏 |
| R2 | **驱动层错误禁止跨层裸传**——所有后端错误必须在驱动内映射为 `StorageError` 分类学 | 同上；未知错误 → `StorageError::Io` 但必须带足上下文（后端错误码+消息） |
| R3 | **禁止硬编码任何密钥/凭据/魔法默认密钥** | PCFS 教训：硬编码 appkey + 默认 token 加密密钥；凭据一律 env > config > keyring 链 |
| R4 | **能力位必须诚实**——`Capabilities` 只声明经过 conformance kit + 真机验证的能力；**local 类驱动豁免真机项**（离线套件即足——本地文件系统无可测远端语义） | PCFS 先例（115/123 只声明已验证项） |
| R5 | 平台代码必须 `#[cfg(windows)]`/`#[cfg(unix)]` 隔离且**保证对方平台可编译** | rs-CyDrive 纪律（Python 版 feaac0b 教训） |
| R6 | Python 兼容契约（DB schema/分块命名/caption/端口）不经负责人明示同意不得改动 | rs-CyDrive 兼容红线（telegram 驱动延续） |
| R7 | 运行期产物绝不提交（config/session/db/cache/control 文件） | rs-CyDrive 纪律；`test/` 全目录已 gitignore（真实凭据） |

## 3. 关键架构机制（对应设计文档 D1–D10）

- **VolumeId**（D6）：`EntryId = (VolumeId, BackendHandle)`；v1 单卷运行，ID 格式从第一天带卷——多卷挂载时代 L3+ 零返工；
- **影子/权威索引**（D4）：驱动声明 `AUTHORITATIVE_INDEX` 能力；VFS 解析顺序 = 本地 db → (权威)后端 → (影子)sync；bootstrap 双路径；
- **MetadataEvent 总线**（D8）：db update_hook 单点门铃，含 volume 维度；sync/未来订阅者从这里拿变更，**禁止任何「手工埋点」回归**（rs-CyDrive 0.7.2 教训）；
- **加密装饰器**（D7）：v1 GCM（兼容冻结）+ v2 分块 AEAD（流式+随机访问）；scheme 是 Entry 元数据，读路径按 scheme 分发；
- **conformance kit**（D9）：驱动过套件 = 上层能力自动可用；新驱动 PR 必附套件通过证据。

## 4. crate 结构与执行形态

```
crates/
  cloudkit-storage   (L2)  ← 新建
  cloudkit-core      (L3+L4) ← cydrive-core 改造
  cloudkit-crypto    (L4)  ← 新建（方案 trait + gcm-v1 + aead-v2）
  drivers/ck-telegram  drivers/ck-baidu  drivers/ck-local   (L1)
  cloudkit-sync-server (L4 服务, 独立部署)
  cloudkit-webdav / cloudkit-web / cloudkit-platform / cloudkit-cli (L5)
```

- **二进制名保持 `cydrive`**（部署位零感知）+ 新增 `cydrive-sync-server` 不变；
- R1 的机械保障（**Phase 0 交付**）：`scripts/check_layers`（校验 L3+ crate 的 Cargo.toml 无 drivers/* 依赖）+ 接入 CI；
- vendor 目录（grammers-session）延续 rs-CyDrive 的 exclude + [patch] 模式。

## 5. 实例与配置模型（D5）

v1：实例 = 后端（config 顶层 `backend = "telegram"|"baidu"|"local"`，缺省 telegram 完全兼容）；多卷/进程内多云 = v1.5+ 议。`proxy_url` 语义按后端声明（telegram 走代理 / baidu、local 直连），doctor 相应提示。

## 6. 修订程序

本约束的任何修改须走 `docs/decisions.md` 记录（日期/动议/裁决/影响），架构级变更（层级/依赖方向/红线增删）需负责人明示批准。
