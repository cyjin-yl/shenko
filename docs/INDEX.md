# Shenko 文档索引

本索引是文档导航，不是进度板。每条入口说明**它是什么、谁拥有它、当前状态如何**；状态判定只依据仓库实际内容与已保存证据，不依据计划或意图。

## 权威关系

现行产品规格只有根目录 [SPEC.md](../SPEC.md)。§1–47 与所有 R/B/Ref/验收条目保持不变；§48 是 2026-11-01 追加的自主主体与文档分层章节。

- [SPEC.md](../SPEC.md) — 产品行为依据：147 项需求、56 组行为、59 条来源、216 条具名验收，保留 U01–U63 用户原话。行号只用于本次导入定位，变更后按标题检索。
- [docs/specs/architecture.md](../docs/specs/architecture.md) — 当前功能边界与各层事实归属（父级拥有）。
- [docs/specs/](specs/) — 各领域的功能合同，由该领域代码拥有者撰写。
- [WHY.md](../WHY.md) — 设计意图与已确定取舍；AGENTS.md — 开发与协作规则；CONTRIBUTING.md — 贡献约定。
- [docs/governance/](governance/) — 自主边界、完整交付合同、许可与接续规则。
- [docs/requirements/](requirements/) — 同一 SPEC 的机器可读索引。
- [docs/evidence/](evidence/) — 已保存的真实运行证据与其边界。
- [docs/state/coverage.json](state/coverage.json) — 每项需求的实现与证据状态。
- [docs/history/](history/) — 历史来源，不是现行指令。

历史草稿不能覆盖现行用户裁决；机器可读索引与 SPEC 必须保持一致。

## 现行文档

| 文档 | 内容 | 所有者 | 状态 |
| --- | --- | --- | --- |
| [SPEC.md](../SPEC.md) | v0.6.2 + §48 完整规格 | 父级（§1–47 字节不变） | 现行行为依据 |
| [docs/specs/architecture.md](../docs/specs/architecture.md) | 自主主体与可替换能力架构 | 父级 | 现行 |
| [docs/specs/](specs/) | 27 份领域功能合同 | 各领域代码拥有者 | 逐份撰写中 |
| [WHY.md](../WHY.md) | 设计为什么成立 | 父级 | 现行 |
| [AGENTS.md](../AGENTS.md) | 自主开发、并行协作、验证与接续规则 | 父级 | 现行 |
| [CLAUDE.md](../CLAUDE.md) | 指向 AGENTS.md 的符号链接 | 父级 | 现行，保持不变 |
| [CONTRIBUTING.md](../CONTRIBUTING.md) | 贡献与认领约定 | 父级 | 现行 |
| [CONTRIBUTORS.md](../CONTRIBUTORS.md) | 贡献来源与署名归属 | 父级 | 现行 |
| [docs/governance/AUTONOMY.md](governance/AUTONOMY.md) | 自主决定边界与何时才需要人裁决 | 父级 | 现行 |
| [docs/governance/COMPLETION.md](governance/COMPLETION.md) | 完整交付合同：什么才算完成、如何证明 | 父级 | 现行 |
| [docs/governance/GITHUB.md](governance/GITHUB.md) | 身份、fork 与持续 PR | 父级 | 现行 |
| [docs/governance/CLAUDE.md](governance/CLAUDE.md) | Claude 启动与恢复的实际参数 | 父级 | 现行 |
| [docs/governance/CONTINUATION.md](governance/CONTINUATION.md) | 跨 session 与 worker 接续 | 父级 | 现行 |
| [docs/governance/DCO.md](governance/DCO.md) | 贡献来源与提交权声明 | 父级 | 现行 |
| [docs/governance/CONTRIBUTOR_LICENSE.md](governance/CONTRIBUTOR_LICENSE.md) | 贡献者再许可（保留署名，非转让） | 父级 | 现行 |
| [docs/governance/LICENSING.md](governance/LICENSING.md) | 项目许可：AGPL-3.0-or-later + owner 双许可 | 父级 | 现行，已采用 |
| [docs/governance/LICENSE-INVENTORY.md](governance/LICENSE-INVENTORY.md) | 依赖与许可实测清单 | 父级 | 实测记录 |
| [docs/requirements/](requirements/) | R/B/Ref/case 机器索引与说明 | 父级 | 与 SPEC 同步 |
| [docs/evidence/](evidence/) | 真实运行证据 | 各切片 | 逐项保存 |
| [docs/state/coverage.json](state/coverage.json) | 需求级实现与证据状态 | 父级 | 逐项推进 |
| [docs/development/](development/) | 局部开发合同与验收 recipe | 各切片作者 | 现行 |
| [docs/decisions/](decisions/) | 已采用的决定记录 | 父级 | 现行 |
| [scripts/check_docs.py](../scripts/check_docs.py) | 文档与归档完整性检查 | 父级 | 现行 |

## 领域功能合同（docs/specs/）

每份文档只由该领域的代码拥有者撰写，写的是**稳定的功能、接口、理由、错误与恢复合同**，不是任务列表或进度。

| 领域 | 文档 | 主要 SPEC 章节 |
| --- | --- | --- |
| 主体运行时 | [subject-runtime.md](specs/subject-runtime.md) | §5–6、13.8、41.5–6、44 |
| 模型子 Agent | [model-children.md](specs/model-children.md) | §5.3、5.5、6、14.5、41.5 |
| 插件与内核装配 | [plugins.md](specs/plugins.md) | §7、18、36、37、41.2、41.8、45 |
| 认知与安装单元 | [cognition-units.md](specs/cognition-units.md) | §18、38、45 |
| 上下文与压缩策略 | [context-policies.md](specs/context-policies.md) | §12.3、13、37、41.7–8 |
| 记忆 | [memory.md](specs/memory.md) | §10.3、10.5、12、45.12 |
| 研究与保留 | [research-memory.md](specs/research-memory.md) | §14、12.4、10.5 |
| 消息通道 | [channels.md](specs/channels.md) | §15–16、35.6、41.6 |
| 其他渠道适配器 | [channel-adapters.md](specs/channel-adapters.md) | §15.3、16、35.6 |
| 联系人与身份 | [contacts.md](specs/contacts.md) | §6、15.2、15.2.1、16、20 |
| 媒体 | [media.md](specs/media.md) | §20、35.3、41.6 |
| WASM 家园 | [wasm-home.md](specs/wasm-home.md) | §8–10、41.3、44 |
| WASM 工具 | [wasm-tools.md](specs/wasm-tools.md) | §8.2、18.8、35、44 |
| 远程 seat | [remote-seats.md](specs/remote-seats.md) | §17、35.5、41.11 |
| REPL | [repl.md](specs/repl.md) | §19、19.1、41.3 |
| 节点与选举 | [nodes.md](specs/nodes.md) | §5.4、22、38、41、44 |
| 自升级 | [self-update.md](specs/self-update.md) | §9、10、35.8、41.12、44 |
| 桌面执行 | [desktop-runtime.md](specs/desktop-runtime.md) | §10.2.1、22、24、34 |
| 移动端 | [mobile-runtime.md](specs/mobile-runtime.md) | §24.5、24.5.1、5.4 |
| 浏览器 | [browser.md](specs/browser.md) | §24.6、19.1、41 |
| Web 界面 | [web-interface.md](specs/web-interface.md) | §21、41.6、43 |
| 配置与 Playground | [configuration-playground.md](specs/configuration-playground.md) | §21、23、41.9、41.10、43、44 |
| 额度与资源 | [quota-resources.md](specs/quota-resources.md) | §13、42、47.5 |
| 持久状态 | [durable-state.md](specs/durable-state.md) | §10、12、36、41.1、41.4 |
| 文档体系 | [documentation.md](specs/documentation.md) | 全部 SPEC 与用户文档重构要求 |
| 验证与证据 | [verification.md](specs/verification.md) | §23、27、28–29、40、46 及全部验收表 |
| 推理与物理资源 | [inference.md](specs/inference.md) | §13、25–26、42、47 |

## 主 SPEC 章节

以下全部属于同一主 SPEC。路径和章节名是阅读入口。

| 章节 | 导入行号 |
| --- | ---: |
| [1 文档状态与产品定义](../SPEC.md#1-文档状态与产品定义) | 3 |
| [2 已确认的产品约束](../SPEC.md#2-已确认的产品约束) | 21 |
| [3 研究结论与竞品定位](../SPEC.md#3-研究结论与竞品定位) | 137 |
| [4 核心产品场景](../SPEC.md#4-核心产品场景) | 176 |
| [5 单线程永续主体与子 Agent](../SPEC.md#5-单线程永续主体与子-agent) | 193 |
| [6 人格、可自改灵魂与统一知识](../SPEC.md#6-人格可自改灵魂与统一知识) | 275 |
| [7 系统架构与故障边界](../SPEC.md#7-系统架构与故障边界) | 291 |
| [8 WASM 家园与工具兼容](../SPEC.md#8-wasm-家园与工具兼容) | 326 |
| [9 Nix-like 包与自我改进](../SPEC.md#9-nix-like-包与自我改进) | 368 |
| [10 文件版本与恢复](../SPEC.md#10-文件版本与恢复) | 432 |
| [11 SSH 身份与远端工具](../SPEC.md#11-ssh-身份与远端工具) | 518 |
| [12 会话真相源和记忆](../SPEC.md#12-会话真相源和记忆) | 532 |
| [13 推理调度与 KV 池](../SPEC.md#13-推理调度与-kv-池) | 607 |
| [14 递归研究与选择性记忆](../SPEC.md#14-递归研究与选择性记忆) | 768 |
| [15 消息注入与认识人的能力](../SPEC.md#15-消息注入与认识人的能力) | 826 |
| [16 中国平台与开放对话](../SPEC.md#16-中国平台与开放对话) | 862 |
| [17 远程 Agent 遥控插件](../SPEC.md#17-远程-agent-遥控插件) | 881 |
| [18 插件能被发现并正确使用](../SPEC.md#18-插件能被发现并正确使用) | 927 |
| [19 有状态 REPL 插件](../SPEC.md#19-有状态-repl-插件) | 1011 |
| [20 实时通话与课堂旁听扩展](../SPEC.md#20-实时通话与课堂旁听扩展) | 1040 |
| [21 Web UI 与设计系统](../SPEC.md#21-web-ui-与设计系统) | 1058 |
| [22 部署和资源目标](../SPEC.md#22-部署和资源目标) | 1269 |
| [23 验收与发布阻断项](../SPEC.md#23-验收与发布阻断项) | 1306 |
| [24 Computer Use 与完整操作系统节点](../SPEC.md#24-computer-use-与完整操作系统节点) | 1365 |
| [25 NInfer 接入教程](../SPEC.md#25-ninfer-接入教程-中文与-english) | 1407 |
| [26 Strata 接入教程](../SPEC.md#26-strata-接入教程-中文与-english) | 1477 |
| [27 共同排障与接入验收](../SPEC.md#27-共同排障与接入验收-common-troubleshooting) | 1526 |
| [28 规格裁决记录与仍未决的问题](../SPEC.md#28-规格裁决记录与仍未决的问题) | 1543 |
| [29 交付路线与阶段验收](../SPEC.md#29-交付路线与阶段验收) | 1566 |
| [30 DeepSeek 与 Gemini 建议的采纳记录](../SPEC.md#30-deepseek-与-gemini-建议的采纳记录) | 1600 |
| [31 研究版本清单与证据使用规则](../SPEC.md#31-研究版本清单与证据使用规则) | 1640 |
| [32 需求追踪矩阵](../SPEC.md#32-需求追踪矩阵) | 1678 |
| [33 用户需求原文与追加澄清](../SPEC.md#33-用户需求原文与追加澄清) | 1860 |
| [34 BoxLite：可选完整 Linux Runtime 插件评估](../SPEC.md#34-boxlite可选完整-linux-runtime-插件评估) | 2338 |
| [35 首版开发者发行](../SPEC.md#35-首版开发者发行可继续开发自身的完整闭环) | 2432 |
| [36 插件的副作用、重置与恢复边界](../SPEC.md#36-插件的副作用重置与恢复边界) | 2562 |
| [37 cordis-rs 主干上的注入时序与 API 兼容](../SPEC.md#37-cordis-rs-主干上的注入时序与-api-兼容) | 2661 |
| [38 内置、可替换的自解释插件](../SPEC.md#38-内置可替换的自解释插件) | 2790 |
| [39 设计来源与上游行为完整度](../SPEC.md#39-设计来源与上游行为完整度) | 2865 |
| [40 开发 Agent 自主工作规范](../SPEC.md#40-开发-agent-自主工作规范) | 3030 |
| [41 首版规范执行合同](../SPEC.md#41-首版规范执行合同) | 3130 |
| [42 Provider 周期额度、模型选择与持续思考的自适应调优](../SPEC.md#42-provider-周期额度模型选择与持续思考的自适应调优) | 4632 |
| [43 对话配置与持久 Playground](../SPEC.md#43-对话配置与持久-playground) | 4839 |
| [44 首次启动与种子发行](../SPEC.md#44-首次启动与种子发行) | 5022 |
| [45 可独立安装的 Awareness、感知与依赖装配](../SPEC.md#45-可独立安装的-awareness感知与依赖装配) | 5255 |
| [46 完整产品的自主开发与仓库交接](../SPEC.md#46-完整产品的自主开发与仓库交接) | 5545 |
| [47 Provider 协议版本、SDK 演进与自主迁移](../SPEC.md#47-provider-协议版本sdk-演进与自主迁移) | 5602 |
| [48 自主主体、运行时能力闭环与正式文档分层](../SPEC.md#48-自主主体运行时能力闭环与正式文档分层) | 6070 |

## 历史与归档

[docs/history/](history/) 保留来源，不保留现行指令。归档清单与字节校验见 [archives-20261101.json](history/archives-20261101.json)。

| 归档 | 内容 |
| --- | --- |
| [history/spec-v0.5.1.md](history/spec-v0.5.1.md) | 导入自 ChatGPT Page 的 v0.5.1 完整快照（原规格哈希见 manifest） |
| [history/manifest.json](history/manifest.json) | 46 份旧规格/研究稿的 SHA-256 清单 |
| [history/entrypoints-20261101/](history/entrypoints-20261101/) | 原 `GOAL.md`、`PROMPT.md`、`HANDOFF.md` 原件 |
| [history/progress-20261101/](history/progress-20261101/) | 旧进度板、交接稿、goal 状态快照、旧计划与编制期治理草案 |
| [history/task-cards-20261101/](history/task-cards-20261101/) | 22 张社区任务卡（C01–C22） |
| [history/acceptance-source-v0.5.1.json](history/acceptance-source-v0.5.1.json) | v0.5.1 的 189 条具名验收原始定义 |
| [docs/research/archive/](research/archive/) | 46 份历史研究与专题稿 |

2026-11-01 起的现行规则：根目录不再有 `GOAL.md`、`PROMPT.md`、`HANDOFF.md`，也不保留兼容符号链接；旧入口只在归档中可查。SPEC §1–47 内部指向旧路径的链接属于旧文档布局，按 §48.5 解释，不为此改动原文字节。

**并行实施规模**：旧任务卡与交接稿中的"最多 1 条 / 最多 5 条 workflow"是当时的环境上限，已被取代。现行指令是同时使用大量实现 Agent，范围由真实机器容量、共享重型工作锁与集成责任划分决定——见 [AGENTS.md](../AGENTS.md) 的「Parallelize and integrate」与 [docs/governance/AUTONOMY.md](governance/AUTONOMY.md)。

## 现行实现地图

[docs/IMPLEMENTATION_DOCS_MAP.md](IMPLEMENTATION_DOCS_MAP.md) 按实现范围列出必读文档，并标注每项在**源码**中的真实状态（接线、未接线或仅有类型）。它描述源码事实，不构成运行验收。

## 验证记录

[docs/evidence/](evidence/) 保存各切片的源码/工件绑定证据，每条区分候选、实际输入、环境、方法、结果与尚未覆盖的范围。已保存的证据包括：文档完整性报告、[统一 Node 与认知恢复](evidence/node-sync-cognition-20261026.json)、[C02 有限 child 运行时](evidence/c02-finite-child-runtime-20261025.json)、[C07 原生 SSH](evidence/c07-native-ssh-20261025.json)、[C17 首条独立节点旅程](evidence/c17-native-first-journey-20261025.json)、[C19 长跑观察工具](evidence/c19-longrun-observation-tooling-20261029.json) 等。

`python3 scripts/check_docs.py` 校验需求/行为/来源/验收编号与 SPEC 一致、原稿逐字保留、归档字节未变、本地链接有效、必读 Skill 与符号链接完好。它不检查产品行为是否通过；产品验收由实际实现提供。
