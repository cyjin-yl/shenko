# Shenko 文档盘点与阅读地图

## 权威关系

现行产品规格只有根目录 [SPEC.md](../SPEC.md)，版本 v0.6.2。v0.6.1 从 ChatGPT Page v0.5.1 sequence 8 完整导入，并追加 U50–U53/R140–R142/B49–B50、完整开发交接 §46 与协议演进 §47；v0.6.2 继续追加 U54–U62/R143–R147/B51–B55，展开持续主体、从输入形成 awareness、异构节点容灾与无模型恢复，以及必读开发 Skill。Page 原快照保留在 [history](history/spec-v0.5.1.md)。后续工程变更以 Git 版本为事实入口，Page 作为讨论/历史来源，不并行维护第二份工程主版本。

README 负责入口，WHY 解释意图，AGENTS 和 CLAUDE 负责开发规则，GOAL 定义本次完整委托；[治理目录](governance/AUTONOMY.md)展开操作方法。机器可读索引对应相同 SPEC，生成或编辑后需保持一致；历史草稿不能覆盖现行用户裁决。

## 当前文件组

持续推理的上下文投影保留历史 user/assistant 的真实角色；窗口以已完成的
assistant 回合结束时，另附明确标注「运行时连续思考提示；不是新外部消息」的
user-role 传输回合，避免将已完成回答作为 Anthropic assistant prefill。
新用户输入或工具结果已构成末尾 user-role 回合时不追加该提示。提示只属于
推理投影，不写成新的 inbox/raw 事实；其文本照常计入输入预算，S0 冻结与恢复
仍使用已封存 payload。离线 wire/reopen 回归位于
`crates/shenko-subject/src/recovery_tests.rs`；此变更尚待协调者执行验证，不代表线上事件或所有 provider 均有同一故障。

| 组 | 内容与状态 |
| --- | --- |
| 主 SPEC | 47章，147 R、55 B、59 Ref记录（含历史拼写别名）、216具名case、U01–U62原话 |
| 原主稿快照 | 完整 v0.5.1，139 R / 48 B / 56 Ref / 189 case，不再是现行工程版本 |
| 阅读与治理 | README、WHY、AGENTS、CLAUDE、GOAL、HANDOFF；AUTONOMY、COMPLETION、GITHUB、CLAUDE、CONTINUATION |
| 开发 Skill | [.agents/skills/shenko-dev](../.agents/skills/shenko-dev/SKILL.md) 必读哲学与8个独立原子；`.claude/skills` 指向同一正文，根 `CLAUDE.md` 符号链接到 `AGENTS.md` |
| 结构化追踪 | requirements/behaviors/sources/acceptance-cases JSON；coverage 逐项区分 partial、实际证据与未完成义务，不以 focused 通过冒充全产品 |
| 交接研究与依据 | [8组上游治理](research/agent-development-governance.md)、[Claude/GitHub核验](research/claude-handoff.md)、[编制期完整范围审计（已合入）](research/full-scope-audit.md)、[Claude API迁移](research/claude-api-2026-10-21.md) |
| 历史研究和专题稿 | 下表逐文件保留，共 46 份；均标记历史/已合入，仍有旧建议或早期未决状态 |
| 执行与接续 | [完整计划](plans/active/full-product.md)、[goal状态](state/goal.json)、[coverage](state/coverage.json)、[证据入口](evidence/README.md) |

## 主 SPEC 章节

以下全部属于同一主 SPEC，不是47份相互竞争的规格。行号只用于本次导入定位，变更后按标题检索；路径和章节名是阅读入口。

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
| [25 NInfer 接入教程 中文与 English](../SPEC.md#25-ninfer-接入教程-中文与-english) | 1407 |
| [26 Strata 接入教程 中文与 English](../SPEC.md#26-strata-接入教程-中文与-english) | 1477 |
| [27 共同排障与接入验收 Common troubleshooting](../SPEC.md#27-共同排障与接入验收-common-troubleshooting) | 1526 |
| [28 规格裁决记录与仍未决的问题](../SPEC.md#28-规格裁决记录与仍未决的问题) | 1543 |
| [29 交付路线与阶段验收](../SPEC.md#29-交付路线与阶段验收) | 1566 |
| [30 DeepSeek 与 Gemini 建议的采纳记录](../SPEC.md#30-deepseek-与-gemini-建议的采纳记录) | 1600 |
| [31 研究版本清单与证据使用规则](../SPEC.md#31-研究版本清单与证据使用规则) | 1640 |
| [32 需求追踪矩阵](../SPEC.md#32-需求追踪矩阵) | 1678 |
| [33 用户需求原文与追加澄清](../SPEC.md#33-用户需求原文与追加澄清) | 1860 |
| [34 BoxLite：可选完整 Linux Runtime 插件评估](../SPEC.md#34-boxlite可选完整-linux-runtime-插件评估) | 2338 |
| [35 首版开发者发行：可继续开发自身的完整闭环](../SPEC.md#35-首版开发者发行可继续开发自身的完整闭环) | 2432 |
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

## 历版与专题稿盘点

v0.1、v0.2.0、v0.3.0 是实际存在的旧整稿；spec_v020_review 是旧审阅副本。v0.4 的本地文件是增量/专题稿，不能把它们声称为另一份完整最新SPEC。当前bootstrap、awareness、hook和provider草稿已由现行主稿吸收或修订。

| 文件 | 原始主题 | 地位 |
| --- | --- | --- |
| [agent_bot_spec_v0_1.md](research/archive/agent_bot_spec_v0_1.md) | 1 文档状态与产品定义 | 历史证据；以现行SPEC为准 |
| [external_review_channels.md](research/archive/external_review_channels.md) | 外部建议复审：竞品、Proto、远程 Agent 与媒体 | 历史证据；以现行SPEC为准 |
| [external_review_inference.md](research/archive/external_review_inference.md) | 外部建议审查：主体 Harness、Memory、KV 与 27B 插件认知 | 历史证据；以现行SPEC为准 |
| [external_review_wasm.md](research/archive/external_review_wasm.md) | 外部建议审查：WASM、文件系统、恢复与核心自改 | 历史证据；以现行SPEC为准 |
| [kv_policy_spec.md](research/archive/kv_policy_spec.md) | 多上下文驻留与 KV 调度规格 | 历史证据；以现行SPEC为准 |
| [personal_agent_scope_audit.md](research/archive/personal_agent_scope_audit.md) | 个人 Agent 最小框架定位与需求覆盖审查 | 历史证据；以现行SPEC为准 |
| [personal_agent_spec_v0_2_0.md](research/archive/personal_agent_spec_v0_2_0.md) | Personal Agent 最小框架 · 产品 SPEC v0.2.0（研究与裁决稿） | 历史证据；以现行SPEC为准 |
| [personal_agent_spec_v0_3_0.md](research/archive/personal_agent_spec_v0_3_0.md) | Personal Agent 最小框架 · 产品 SPEC v0.3.0（研究与裁决稿） | 历史证据；以现行SPEC为准 |
| [remote_agent_spec.md](research/archive/remote_agent_spec.md) | 远程 Agent Seat 插件：研究与产品规格补充 | 历史证据；以现行SPEC为准 |
| [requirements_audit.md](research/archive/requirements_audit.md) | 产品 SPEC 需求覆盖审查 | 历史证据；以现行SPEC为准 |
| [requirements_traceability.md](research/archive/requirements_traceability.md) | 需求追踪附录 | 历史证据；以现行SPEC为准 |
| [research_boxlite_integration.md](research/archive/research_boxlite_integration.md) | BoxLite 插件集成核验 | 历史证据；以现行SPEC为准 |
| [research_boxlite_runtime.md](research/archive/research_boxlite_runtime.md) | BoxLite：运行时、持久化与接入边界核验 | 历史证据；以现行SPEC为准 |
| [research_compaction_codex_omp.md](research/archive/research_compaction_codex_omp.md) | Codex budget compact 与 OMP snapcompact：源码核验增量 | 历史证据；以现行SPEC为准 |
| [research_competitors.md](research/archive/research_competitors.md) | 竞品与通信接入研究（中间研究笔记） | 历史证据；以现行SPEC为准 |
| [research_dynamic_ui.md](research/archive/research_dynamic_ui.md) | Dynamic Response / Intelligent UI：官方核验与个人 Agent 动态界面规格补充 | 历史证据；以现行SPEC为准 |
| [research_forgetting.md](research/archive/research_forgetting.md) | 研究补充：Scaling Discovery Requires Forgetting 与研究子 agent 的可控重启 | 历史证据；以现行SPEC为准 |
| [research_harness.md](research/archive/research_harness.md) | Harness、历史与本地推理调度研究笔记 | 历史证据；以现行SPEC为准 |
| [research_interrupt_dnd_d3.md](research/archive/research_interrupt_dnd_d3.md) | D3 增量：外部打断、主体决定免打扰、误过滤自主修复 | 历史证据；以现行SPEC为准 |
| [research_layerfs_vm_self_port.md](research/archive/research_layerfs_vm_self_port.md) | LayerFS 原版进入 Linux VM；WASM 独立状态层与自主移植闭环 | 历史证据；以现行SPEC为准 |
| [research_migration_retention_election.md](research/archive/research_migration_retention_election.md) | D7 / D8 / D9 / D11 增量协议建议 | 历史证据；以现行SPEC为准 |
| [research_native_inference_vllm.md](research/archive/research_native_inference_vllm.md) | 原生推理后端接入研究：vLLM、1Cat-vLLM、NInfer、Strata | 历史证据；以现行SPEC为准 |
| [research_omp_delivery_d2.md](research/archive/research_omp_delivery_d2.md) | OMP D2：子 Agent 的有限直接发布核验 | 历史证据；以现行SPEC为准 |
| [research_pi_durable_capability.md](research/archive/research_pi_durable_capability.md) | Pi Durable 与工具能力自改进核验 | 历史证据；以现行SPEC为准 |
| [research_plugin_cognition.md](research/archive/research_plugin_cognition.md) | 插件发现、正确使用与本地 27B 模型的认知负担 | 历史证据；以现行SPEC为准 |
| [research_proactive_memory.md](research/archive/research_proactive_memory.md) | Proactive Memory Agent：论文、代码核验与产品规格建议 | 历史证据；以现行SPEC为准 |
| [research_repl.md](research/archive/research_repl.md) | Python / Bun / Node：WASM 内可恢复 REPL 规格研究 | 历史证据；以现行SPEC为准 |
| [research_wasm.md](research/archive/research_wasm.md) | WASM、持久化与可恢复 Agent：研究记录 | 历史证据；以现行SPEC为准 |
| [scheduler_audit.md](research/archive/scheduler_audit.md) | 调度与历史规格审核清单 | 历史证据；以现行SPEC为准 |
| [shenko_awareness_dependencies_draft.md](research/archive/shenko_awareness_dependencies_draft.md) | §45 增量：可安装单元与依赖装配 | 历史证据；以现行SPEC为准 |
| [shenko_awareness_draft.md](research/archive/shenko_awareness_draft.md) | §45 可安装的 Awareness：给 Agent 补上该想到的事 | 历史证据；以现行SPEC为准 |
| [shenko_bootstrap_draft.md](research/archive/shenko_bootstrap_draft.md) | Shenko 首次启动与种子发行规范草案 | 历史证据；以现行SPEC为准 |
| [shenko_runtime_contracts_v0_4.md](research/archive/shenko_runtime_contracts_v0_4.md) | Shenko 运行合同增量：主体、事件、推理、Seat 与升级交接 | 历史证据；以现行SPEC为准 |
| [shenko_section41_merged.md](research/archive/shenko_section41_merged.md) | 41 首版规范执行合同 | 历史证据；以现行SPEC为准 |
| [shenko_section41_plugin_protocols_draft.md](research/archive/shenko_section41_plugin_protocols_draft.md) | §41 规范接口补充：插件操作、注入、自解释与版本交接 | 历史证据；以现行SPEC为准 |
| [shenko_section42_provider_quota_draft.md](research/archive/shenko_section42_provider_quota_draft.md) | §42 后期增量：Provider 周期额度与自适应模型预算 | 历史证据；以现行SPEC为准 |
| [shenko_spec_v0_4_0_additions.md](research/archive/shenko_spec_v0_4_0_additions.md) | 21.10 Magpie 视觉参照与比较方法 | 历史证据；以现行SPEC为准 |
| [shenko_streaming_ui_contract.md](research/archive/shenko_streaming_ui_contract.md) | 21.11 流式输出的阅读稳定性：Magpie 与 Alma 参照 | 历史证据；以现行SPEC为准 |
| [shenko_u42_u43_ui_config_playground.md](research/archive/shenko_u42_u43_ui_config_playground.md) | U42 / U43 规范增量：主体 Web 输入、对话配置与持久 Playground | 历史证据；以现行SPEC为准 |
| [shenko_v1_durable_contract_draft.md](research/archive/shenko_v1_durable_contract_draft.md) | Shenko V1：持久执行与自升级合同草案 | 历史证据；以现行SPEC为准 |
| [shenko_v1_protocol_preface.md](research/archive/shenko_v1_protocol_preface.md) | 41 首版规范执行合同 | 历史证据；以现行SPEC为准 |
| [spec_questions.md](research/archive/spec_questions.md) | 第二轮产品规格追问 | 历史证据；以现行SPEC为准 |
| [spec_v020_review.md](research/archive/spec_v020_review.md) | Personal Agent 最小框架 · 产品 SPEC v0.2.0（研究与裁决稿） | 历史证据；以现行SPEC为准 |
| [thought_loop_spec.md](research/archive/thought_loop_spec.md) | 唯一主体的持续 Thought Loop 规格补充 | 历史证据；以现行SPEC为准 |
| [thought_ui_notes.md](research/archive/thought_ui_notes.md) | 单一持续 Thought Loop：产品语义、界面与新增规格矛盾 | 历史证据；以现行SPEC为准 |
| [user_requirements_verbatim.md](research/archive/user_requirements_verbatim.md) | 33 用户需求原文与追加澄清 | 历史证据；以现行SPEC为准 |

## 验证记录

[history/manifest.json](history/manifest.json)记录原稿大小和SHA-256；原始U01–U49逐字保留，并追加U50–U62。`python3 scripts/check_docs.py`检查覆盖ID、原话、原稿hash、本地链接、必读Skill及符号链接；它不是产品测试。现有文档没有声称本次已经实现或完成设备/性能验收。

当前跨包接线入口：[统一 Node 与恢复合同](development/managed-node.md)、
[#71 源码/工件/原生运行证据](evidence/node-sync-cognition-20261026.json)。
映射 [C17](tasks/C17.md)、[C10](tasks/C10.md)、[C09](tasks/C09.md)、
[C01](tasks/C01.md)、[C03](tasks/C03.md)；完整包与 S1–S5 仍按各自剩余验收继续。

模型驱动行为的验收方法：[本地模型行为验收](development/local-model-behavior.md)。
提示词、控制协议或事件投影改变后，用真实本地模型采样成功率；确定性测试不能代替
“这个本地模型会不会做对”。

能力编写原则：[内核零业务，能力优先写成 perception + awareness](development/awareness-first-plugins.md)。
内核只做机制，判断交给可安装的觉察与视角；不用关键词表强制动作。

## 社区任务栈

- [朋友可复制PROMPT](../PROMPT.md)
- [当前工作包与已发布issues](plans/active/community-stack.md)
- [R/B/case归属与依赖JSON](plans/active/community-stack.json)
