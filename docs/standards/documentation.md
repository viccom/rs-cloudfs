# 文档规范（Documentation Standards）

> 状态：v1.0（2026-09-07）｜ 适用：仓库全部 docs/ 与 AGENTS/README
> 来源：rs-CyDrive 文档体系（plans/decisions/reports 三轨 + AGENTS 状态机式维护）实际运转验证

## 1. 目录结构与职责

```
README.md            定位、状态、快速上手（永远反映当前真态）
AGENTS.md            Agent 工作契约（见 §4）
docs/
  decisions.md       裁决记录：冲突/风险/取舍，只追加不改写
  plans/             设计+实施计划（含 Kickoff 指令）
  reports/           证据类产物：spike 报告、验收报告、基准结果
  standards/         本规范集（architecture/code-style/interfaces/logging/documentation）
```

- **一次工作一份文档**：计划进 plans/、证据进 reports/、裁决进 decisions.md——不建孤立散文。

## 2. 计划文档（plans/）格式

文件名 `YYYY-MM-DD-<主题>.md`；两档：
- **设计裁决稿**（架构/方向性文档，如 fusion-foundation）：定位/决策/阶段计划/风险/裁决状态——不要求 Kickoff 与逐批验收；
- **实施计划**（可执行批）：必备下列七节；
- 稿件被取代时头部加 **SUPERSEDED banner**（指向取代文档 + 仍有效部分清单）。

实施计划必备节（另：**每个实施计划必须配套 `docs/tracking/<phase>.md` 任务单与跟踪表**——任务×状态×完成情况×证据，每批收口更新并随 commit 提交）：

1. **定位**（负责人指令原文/日期/北极星关联）；
2. **Goal / 架构 / 技术栈**（钉死的决策 + 依据）；
3. **任务批次**（TDD 批划分 + 每批验收判据）；
4. **不做**（明确出界的项，防范围蔓延）；
5. **规模与风险**（含止损点/中止条件）；
6. **Kickoff 指令**（新会话粘贴即执行的自包含段：worktree/分支/前置/门禁/中止条件）；
7. 附录可固化情报（带 文件:行号 引用）。

## 3. decisions.md 条目格式

`## YYYY-MM-DD <主题>`：**冲突双方/动议** → **采纳裁决** → **风险/代价** → （回滚方式）；冲突/风险必须记（全局约定）；新条目追加文件末尾。

## 4. AGENTS.md 维护纪律

- AGENTS.md 是**会话开工的第一读物**：项目信息、当前阶段（批次级状态）、常用命令、硬性规则、已知陷阱、待人工清单；
- 每批收口必须同步：状态条目、测试计数行（`cargo test --workspace` 总数与分 crate）、待人工清单增删；
- 判断标准：删掉某行后 Agent 是否更容易犯错——不影响行为的内容不进 AGENTS。

## 5. 诚实性要求（硬规则）

- 汇报验证必须贴**真实命令与真实输出**（尾部即可）；「应该可以/理论上」不算验证；
- 跳过项显式声明（静默跳过 = 违规）；未做的不写成完成；
- 报告里的数字可复算（计数来源注明）；
- 文档声明与代码行为不一致时，以代码为准并**当批修正文档**（如 sync-server --help 曾误述 secret 范围——文案属契约，改文案或改代码必须同步）。

## 6. 变更联动

- 行为变更（config 键/CLI/协议/默认值）→ 同批更新：README、AGENTS 计数行、相关 plans/reports、用户文档（deployment/usage）；
- 验收报告至少包含：改动摘要 / 验证证据（命令+输出）/ 未询问的决定与回滚 / 遗留清单。

## 7. 语言与署名

- 文档中文；代码/提交信息英文按 rs-CyDrive 惯例；提交信息不加 Co-Authored-By 尾注。
