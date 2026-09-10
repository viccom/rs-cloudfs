# 驱动编译开关（Feature Gates）执行计划

> **For Claude:** REQUIRED SUB-SKILL: executing-plans 编排执行（hub-and-spoke，TDD 红→绿留证）。

**Goal:** ck-telegram / ck-baidu / ck-local 成为组合根 cloudkit-cli 的可选依赖；编译产物按需选择驱动；默认全开 = 现行为零变化。

**裁决 K30–K32**：K30 feature 落点=cloudkit-cli（`[features] telegram/baidu/local`，default 全开；驱动 crate 不改）；K31 缺驱动=编译期裁剪+运行期可行动错误（含缺省 backend=telegram 的无 telegram 二进制场景；文案「this binary was built without the X driver; rebuild with --features X, or set backend=...」）；K32 `--version` 增 `(drivers: ...)` 清单。附带：push/pull off 态运行期报错不隐藏命令；doctor 缺驱动跳过对应探活；CI 增 clippy×[none,telegram,baidu,local]+test×[default,none]。

## 触点档案（2026-09-09 Explore 实证，main@a53b950 基准）

- 依赖：cli/Cargo.toml :30 ck-telegram / :33 ck-baidu / :34 ck-local（均非 optional）；驱动间零互依；web/webdav/sync-server 零驱动依赖；check_layers 按键名匹配 optional 兼容
- telegram：lib.rs:41-44 imports、transport_config_from:1421、connect_stack:1443-1472、connect_failure_hint:113、TELEGRAM_REBUILD_REFUSAL:1733（纯文案不动）；main.rs:26 GrammersTransport import、connect_telegram_volume:852-886（run_single:668/run_multi:754 两臂）、push/pull:256/:286
- baidu：lib.rs BaiduEndpoints:1809-1826（Default 用 ck_baidu 常量）、baidu_params:1836、ConfigTokenStore impl:2043、baidu_backend_probe:2130-2179、BackendTransport::Baidu:1868-1943、build_backend_transport_with:1988-1993、build_driver:2201；setup.rs run_setup_baidu:342-349 + 菜单:243-252；doctor probe 调用点 main.rs:538-539
- local：BackendTransport::Local、build_driver:2209、dispatch.rs local 用例
- 测试门控面：dispatch.rs（:28 ck_baidu::TokenStore + baidu/local 真驱动用例 + :196 telegram 拒绝钉测试）、multivolume_e2e.rs（:543 telegram / :570 baidu 参数映射）、connect_deadline.rs（connect_stack）；其余 15 文件零改动
- 风险预判：[patch.crates-io] grammers-session 无消费者时的 unused-patch 警告（FT1 实证）；SQLite C 编译不可省（core 自依赖）

## 批次

- FT1 telegram 门控：no-default 编译红 → imports/传输面/连接路径/push-pull 早退/文案分支 + 测试门控；绿=no-default 构建过+telegram 单开过+默认零漂移
- FT2 baidu 门控：Endpoints Default off 态空串兜底（inert 声明）/params/store impl/probe/枚举/build 臂/setup 菜单与向导/doctor 调用点 + 测试门控；绿=telegram+local 组合过
- FT3 local 门控：枚举/build 臂/dispatch 用例；六组合矩阵实测（build+clippy×6，全量 test×default/none）
- FT4 收口：--version 驱动清单（红→绿）、CI 矩阵腿、README/AGENTS/onboarding/decisions/tracker、双产物交付（全量+轻量各报 size/sha256/version）、merge main + push

## 纪律

默认全开零漂移（809 测试断言零改动，门控的测试挂 cfg 不改断言）；红证据=目标组合下真实编译红/测试红输出；每批三步门禁 + check_layers + scan_secrets。
