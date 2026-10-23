# Shenko：持续自主开发的仓库治理研究

研究截点：2026-10-22 UTC。范围：文档仓库与后续开发交接；未安装这些项目，未运行其测试，未修改 Shenko 主 Page 或 GitHub 仓库。仅使用项目官方仓库、官方规范与厂商文章。

## 结论与采用方式

最有用的组合是：**短入口文档与可导航的产品依据、跨会话保留的工作记录、隔离且可整合的并行工作、绑定实际候选的验证证据、持续发现未完成需求，以及 PR 交付之后继续推进未阻塞工作。** 不必引入某一个项目的完整流程，更不能把其人类批准点、语言限制或固定阶段原样移植。

这是对 8 组代表性上游的核验，不是热度排行榜。厂商项目和社区项目分开看；最近提交证明仍在演进，不证明市场占有率或优于其他项目。下面的“上游事实”来自列明版本；“Shenko 建议”是本项目的设计推导，不是上游已经替我们实现的功能。

**U51 当前采用政策。** 朋友的开发 Agent 使用官方 `--dangerously-skip-permissions`，在朋友自己的 fork 内自主规划、开发、重构、安装和改进工具、修改 CI、组织子 Agent、提交及合并工作；向 upstream 通过 PR 持续交付和更新。上述操作已获授权，Agent 持续自行决策并推进。文中上游的审批、收尾与并行规则保留为研究事实；Shenko 的采用方式以本段及以下项目建议为准。

## 1. 固定版本登记

| 上游 | 本轮固定提交／文章 | 日期 UTC | 核验重点 |
| --- | --- | --- | --- |
| Pi／Pi Durable | [936e5f40aede9327b9142b63506ed2b68bc6b4ac](https://github.com/earendil-works/pi/tree/936e5f40aede9327b9142b63506ed2b68bc6b4ac) | 2026-10-22 21:32:38 | 持久任务、提交后可见、恢复与仓库开发规约 |
| oh-my-pi | [0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0](https://github.com/can1357/oh-my-pi/tree/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0) | 2026-10-22 10:22:25 | 子任务、独立产物、隔离、结果状态、GitHub 操作规范 |
| OpenAI Symphony | [be10a1b79df723d6d7612b5651c8522704dafb2e](https://github.com/openai/symphony/tree/be10a1b79df723d6d7612b5651c8522704dafb2e) | 2026-09-15 22:12:07 | issue 工作单元、WORKFLOW、workpad、PR 与真实批准点 |
| OpenAI Codex／harness engineering | [ca87ef86e101c4e0b18847116fadcff078abaeaa](https://github.com/openai/codex/tree/ca87ef86e101c4e0b18847116fadcff078abaeaa)；官方工程文章 | 2026-10-22 21:56:16；文章 2026-02-11 | 文档地图、agent review、可执行架构约束、审查证据 |
| Anthropic 长时开发 harness／Claude Code | [claude-code 5ed799e5cc56bea9efff57398ca43e31d82784ff](https://github.com/anthropics/claude-code/tree/5ed799e5cc56bea9efff57398ca43e31d82784ff)；两篇官方 harness 文章 | 仓库 2026-10-22 04:34:32；文章 2025-11-26、2026-03-24 | 特性清单、交接、独立评价、移除已经无益的流程 |
| Superpowers | [2ce8aa15d05bd57213e6f746f953aafb7ded7a8b](https://github.com/obra/superpowers/tree/2ce8aa15d05bd57213e6f746f953aafb7ded7a8b) | 2026-10-21 21:00:49，提交为 v7.0.1 | 连续执行、裁决日志、任务与整分支审查、完成证据 |
| gstack | [a59b5e4097ef8459c97d60bd3bcf7a20687de885](https://github.com/garrytan/gstack/tree/a59b5e4097ef8459c97d60bd3bcf7a20687de885) | 2026-10-22 20:10:23 | 候选内容冻结、验证收据、文档同步、无人值守与 gate 的区别 |
| GitHub Spec Kit | [27d14e20c187c1f2916540ea201ecbdc9f8ed588](https://github.com/github/spec-kit/tree/27d14e20c187c1f2916540ea201ecbdc9f8ed588) | 2026-10-22 14:12:35 | spec→plan→tasks 追踪、converge、可组合流程及默认人工 gate |

注：此前 Shenko 已固定的 Pi Durable c60de42 仍是原规格引用，不应静默替换。本轮读取较新的主线 README 与开发规约用于治理比较，不将两个提交拼成一个虚构版本。Claude Code 仓库提交只定位该产品当时公开仓库；下述长时 harness 实验来自有日期的官方文章，不声称它们就是该仓库提交内的实现。

## 2. 上游事实与可采用部分

### G01 — Pi Durable：进度要有提交事实，重新打开不能靠模型记得

**上游事实。** 当前 README 将 transcript、document state 和 task 区分；一次 commit 可以原子保存 entries、文档变更和任务。任务保存 checkpoint；存储成功后才显示已提交内容。它也区分 task fault、park、迁移错误和宿主错误，而非统一报“agent 失败”。这些是持久执行能力，不是 PR 的测试成功证明。存储默认仍声明单进程所有权，没有跨进程锁。见 [README 概念 L92–104](https://github.com/earendil-works/pi/blob/936e5f40aede9327b9142b63506ed2b68bc6b4ac/packages/durable/README.md#L92-L104)、[恢复 L214–220](https://github.com/earendil-works/pi/blob/936e5f40aede9327b9142b63506ed2b68bc6b4ac/packages/durable/README.md#L214-L220)、[存储边界 L783](https://github.com/earendil-works/pi/blob/936e5f40aede9327b9142b63506ed2b68bc6b4ac/packages/durable/README.md#L783)。

Pi 仓库自己的 [AGENTS.md L32–49](https://github.com/earendil-works/pi/blob/936e5f40aede9327b9142b63506ed2b68bc6b4ac/AGENTS.md#L32-L49) 还规定其特有检查命令、默认不运行某些全套测试及依赖锁审查。那是维护该仓库的操作规范，不是任何项目都应照抄的普适法则。

**Shenko 建议。** 开发控制者恢复时读取任务记录、commit、PR 和验证产物，核对现在的实际状态；口头说过完成不算完成。第一天可用版本化文档、Git 和 GitHub 工作项保存交接，不必先实现 Shenko 自己的 DurableEngine 才能开发它。也不把 Markdown 文件宣称为具备数据库级断电原子性。

### G02 — OMP：委派要给完整任务，结果、补丁与状态分别读取

**上游事实。** Task 文档明确保存子 Agent 身份、退出／取消／错误状态、模型、结构化结果有效性，以及 patch、branch、base SHA 等产物信息。异步结果可以投递，完整输出通过 agent URI 查阅。独立 checkout 必须实际启用并由任务选择；省略 isolated 会共享父 checkout。隔离可捕获 patch 或用分支集成。子 Agent 不自动继承完整对话；可向已存在的 Agent 跟进。见 [docs/tools/task.md L66–95](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/docs/tools/task.md#L66-L95)、[隔离 L130–143](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/docs/tools/task.md#L130-L143)、[协调 L194–200](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/docs/tools/task.md#L194-L200)。

其 [AGENTS.md L26–40](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/AGENTS.md#L26-L40) 对外部贡献有逐次 GitHub 发言确认、贡献者亲写说明等要求；这些不能作为 Shenko 已获授权开发的默认阻碍。RoboOMP 的 `pr-review-handoff.md` 是实现交接文档，本轮不把它全部当成已经完成的功能。

**Shenko 建议。** 子任务 brief 必须自足，包含目标、关联需求、当前写入分工、依赖和验收依据。成果接纳同时看真实 diff、失败状态和验证证据，不能仅看结构化 JSON 合法或报告文本写了 DONE。已有子 Agent 仍掌握相关上下文时可跟进；独立审查则可使用新的上下文。并行实现优先独立 worktree／分支，明确集成人。

### G03 — Symphony：把工作从会话中解耦，但默认 Human Review 不符合本次委托

**上游事实。** SPEC 定义按 issue 建隔离 workspace、从仓库 WORKFLOW 加载行为及有界并发；自身是 scheduler/runner，不内置所有 ticket 和 PR 业务。重启可由 tracker 与文件恢复，但不承诺原内存调度状态精确恢复。SPEC 本身不强制一种审批姿态。[SPEC L16–69](https://github.com/openai/symphony/blob/be10a1b79df723d6d7612b5651c8522704dafb2e/SPEC.md#L16-L69)

参考 Elixir WORKFLOW 要求无人值守、用一个持久 workpad comment 保存计划和验收、处理 PR review；同时却明确 Human Review 等人批准，Merging 表示已经获人类批准。不能只读“unattended”便称其默认全自主。[WORKFLOW L69–115](https://github.com/openai/symphony/blob/be10a1b79df723d6d7612b5651c8522704dafb2e/elixir/WORKFLOW.md#L69-L115)、[反馈 L173–184](https://github.com/openai/symphony/blob/be10a1b79df723d6d7612b5651c8522704dafb2e/elixir/WORKFLOW.md#L173-L184)

其实现开发规则要求用真实可观察行为证明健康，而非仅看到 PID；非平凡改动应及早进行对抗性审查，可靠复现的失败不会被其他干净 review 抵消。[elixir/AGENTS.md L30–39](https://github.com/openai/symphony/blob/be10a1b79df723d6d7612b5651c8522704dafb2e/elixir/AGENTS.md#L30-L39)

**Shenko 建议。** 采用工作项、依赖、持久 workpad 和验证包，按项目需要选择工具和调度器。fork 内自主整合；upstream PR 持续跟踪、修复，审查期间继续推进完整产品的后续工作。

### G04 — Codex 与 OpenAI harness engineering：入口是地图，质量约束可执行

**上游事实。** 官方工程文章描述短 AGENTS.md 指向结构化 docs，设计原则、执行计划、技术债和质量状态纳入仓库；使用隔离 worktree、真实应用和可查询观测信息验证。该团队采用本地自审及其他 Agent 审查，人类 review 可以有，但并非必需。这是特定团队的经验，不是任何仓库天然具备的保证。[OpenAI，2026-02-11](https://openai.com/index/harness-engineering/)

当前 Codex review rubric 要求发现具体、可操作、具有影响证据的缺陷；引用项目规则时核对适用的指令文件，不凭空创造规则。它也区分阻塞缺陷与非阻塞风格问题。[固定 rubric L9–23](https://github.com/openai/codex/blob/ca87ef86e101c4e0b18847116fadcff078abaeaa/codex-rs/prompts/templates/review/rubric.md#L9-L23)、[规则依据与优先级 L46–62](https://github.com/openai/codex/blob/ca87ef86e101c4e0b18847116fadcff078abaeaa/codex-rs/prompts/templates/review/rubric.md#L46-L62)

**Shenko 建议。** AGENTS.md 不塞入整个产品 SPEC；它说明目标、阅读导航、开发授权、真实命令和交付要求。实现约束应能由合适测试／静态检查反馈，避免只靠反复提示。独立 reviewer 必须给触发场景、影响和位置；禁止为了显得严格而持续制造新风格 gate。整产品验收另查遗漏，不能只用“本 PR 新引入的 bug”这一种 review 范围。

### G05 — Anthropic：完成清单需要实际使用验证，流程也应随模型进化

**上游事实。** 2025 年长时 harness 文章记录了两类失败：一次做太多导致交接混乱，以及看到部分实现就过早宣布完成。其示例使用初始特性清单、进度文件、Git 和启动脚本，并要求端到端验证。[官方文章，2025-11-26](https://www.anthropic.com/research/effective-harnesses-for-long-running-agents)

2026 年后续文章研究 planner/generator/evaluator，指出生成者倾向高估自身产物；也披露新模型下移除了每 sprint 的框架，将部分评价改到末尾，按实际价值调整复杂度。文中 QA 仍发现漂亮界面里核心交互只是 stub，同时评价者也会漏检。[官方文章，2026-03-24](https://www.anthropic.com/research/harness-design-long-running-apps)

**Shenko 建议。** 把每项产品要求对应到可观察行为与验证证据；有按钮、接口、trait 或文档不等于功能成立。复杂边界采用独立审查和故障场景；普通修改不必每次重跑庞大多角色仪式。Claude Opus 5.5 的具体 /goal、ultracode 行为需由专门运行能力核验决定，本报告不拿旧实验证明新命令行为。持续开发仍需可重开的工作记录，不能把命令名当作持久恢复保证。

### G06 — Superpowers v7：明确连续执行和裁决记录，但并行、收尾规则需要改配

**上游事实。** SDD 明确执行中不逐任务向人问是否继续；歧义优先按 spec、plan 和判断裁决，并记录理由。它用每计划 ledger 抵抗 compact 后重复派发已完成工作；每任务审查后还作整分支审查。它要求 reviewer 看真实 BASE…HEAD 范围，不能用 HEAD~1 漏掉多提交任务。[SDD L17–31](https://github.com/obra/superpowers/blob/2ce8aa15d05bd57213e6f746f953aafb7ded7a8b/skills/subagent-driven-development/SKILL.md#L17-L31)、[ledger L108–133](https://github.com/obra/superpowers/blob/2ce8aa15d05bd57213e6f746f953aafb7ded7a8b/skills/subagent-driven-development/SKILL.md#L108-L133)、[review L251–270](https://github.com/obra/superpowers/blob/2ce8aa15d05bd57213e6f746f953aafb7ded7a8b/skills/subagent-driven-development/SKILL.md#L251-L270)

但它也明令不要并行派多个实现者，且 finishing skill 默认向人展示如何收尾的选项。验证 skill 对“完成”强调新鲜运行证据；不能把 agent 自报成功视为验收。[并行限制 L233](https://github.com/obra/superpowers/blob/2ce8aa15d05bd57213e6f746f953aafb7ded7a8b/skills/subagent-driven-development/SKILL.md#L233)、[finishing L53–80](https://github.com/obra/superpowers/blob/2ce8aa15d05bd57213e6f746f953aafb7ded7a8b/skills/finishing-a-development-branch/SKILL.md#L53-L80)、[verification L14–48](https://github.com/obra/superpowers/blob/2ce8aa15d05bd57213e6f746f953aafb7ded7a8b/skills/verification-before-completion/SKILL.md#L14-L48)

**Shenko 建议。** 采用 ledger、裁决、自足 brief 与独立审查；按实际依赖动态协调并行任务和写入责任。用户已授权 fork 内自主合并、向 upstream 以 PR 交付，Agent 按此自行完成收尾并继续。证据有效性绑定候选内容与环境，不要求每说一句状态都重复全测。其 git-ignored ledger 可以参考，但跨机器交接的关键状态还须有可共享持久副本。

### G07 — gstack：检查必须绑定被检查的内容，零测试不等于通过

**上游事实。** 最新 ship 将 invocation record 保存为可恢复记录，含版本、review handles、检查命令、输入、日志和剩余工作。其最终验证会确认 writer 已结束，冻结候选；可在实际输入、依赖和参数相同的条件下复用检查。测试命令 exit 0 但运行零项不算通过，计数不可获取则如实报告，而非编造数量。[ship L434–452](https://github.com/garrytan/gstack/blob/a59b5e4097ef8459c97d60bd3bcf7a20687de885/ship/SKILL.md#L434-L452)、[writers L903–921](https://github.com/garrytan/gstack/blob/a59b5e4097ef8459c97d60bd3bcf7a20687de885/ship/SKILL.md#L903-L921)、[收据 L970–1029](https://github.com/garrytan/gstack/blob/a59b5e4097ef8459c97d60bd3bcf7a20687de885/ship/SKILL.md#L970-L1029)

它区分 spawned/unattended 与 headless，对部分问题自动选择推荐项或记录 pending gate；并未取消所有人工决定。完整 ship 还有固定阶段和必须执行的 review，不宜将其宣称为通用最小流程。[分支规则 L62–83](https://github.com/garrytan/gstack/blob/a59b5e4097ef8459c97d60bd3bcf7a20687de885/ship/SKILL.md#L62-L83)、[ship 总则 L418–432](https://github.com/garrytan/gstack/blob/a59b5e4097ef8459c97d60bd3bcf7a20687de885/ship/SKILL.md#L418-L432)

**Shenko 建议。** 借鉴证据收据和内容绑定，不照搬完整流程。冻结当前被审查的工作树／commit，不要求停掉其他独立 worktree。修改后只重跑被影响或依赖不明的检查。测试／验证工具本身也可改，但 PR 要解释变化及其对证明力的影响，不能通过删除断言、放宽标准或隐藏失败使完成率变好看。

### G08 — Spec Kit：用 convergence 重新找未完成项，而不是相信勾选

**上游事实。** 当前 README 将 SDD、修复、评估和自定义流程表述为独立入口；允许以扩展、preset、workflow 和 overlay 组合，不是一切任务的必经阶段。[README L25–60](https://github.com/github/spec-kit/blob/27d14e20c187c1f2916540ea201ecbdc9f8ed588/README.md#L25-L60)

converge 要按 spec/plan/tasks 的全部意图检查实现，包含已勾选项；缺失、部分实现、矛盾或越界形成可追溯的新增任务，不改写旧 task identity。[converge L135–156](https://github.com/github/spec-kit/blob/27d14e20c187c1f2916540ea201ecbdc9f8ed588/templates/commands/converge.md#L135-L156)、[新增任务 L202–230](https://github.com/github/spec-kit/blob/27d14e20c187c1f2916540ea201ecbdc9f8ed588/templates/commands/converge.md#L202-L230)

默认 implement 模板仍要求发现未勾选质量清单时停下问人；质量清单的勾选本来也不等于实现完成。[implement L56–87](https://github.com/github/spec-kit/blob/27d14e20c187c1f2916540ea201ecbdc9f8ed588/templates/commands/implement.md#L56-L87)

**Shenko 建议。** 将现有 R/验收编号连接到实际实现、PR 和证据，定期从需求端重新核对，追加遗漏任务。保留真实差距，不把“converge 没找到缺陷”升级为数学证明。未完成清单触发自主补查／实现／重新规划；可修复的工具与 CI 由 Agent 继续处理，真实外部阻塞记在受影响工作项上，同时推进其余工作。

## 3. 建议的文档仓库分工

以下文件名是 Shenko 的建议，不声称所有上游采用同名文件；尤其 WHY.md 不是上述项目共同规定的标准。

| 文档／记录 | 作用 | 不承担的责任 |
| --- | --- | --- |
| README.md | 项目入口、当前真实状态、如何开始阅读 | 不把文档仓库说成已能运行的产品 |
| WHY.md | 产品为何存在、个人 Agent 的完整目标、已拒绝的妥协与典型场景 | 不重复全部协议，也不成为永久不可修改的宪法 |
| AGENTS.md | 简洁导航、现行工作授权、必要行为约束、真实验证和 PR 规则 | 不包含整个 SPEC，不复制其他仓库的审批惯例 |
| Claude 入口文件／启动 brief | 让朋友使用的实际 harness 找到同一 AGENTS、目标与恢复记录 | 不维护第二份相互冲突的产品规范 |
| SPEC 与拆分章节索引 | 完整产品需求、现行裁决、跨模块合同与证据来源 | 不因首版优先级而删除后续完整目标 |
| 需求追踪／验收矩阵 | requirement → work item → implementation／PR → verification；明确 unknown、blocked、partial | 不以 checkbox 数量代替产品完成 |
| active plans 与 decision records | 当前方法、依赖、并行拆分、假设、已作裁决及改变原因 | 计划可改，不凌驾于用户要求和有效 SPEC |
| 工作记录／交接索引 | 正在做什么、在哪个 worktree／branch、做到何处、下一可执行步骤 | 不把模型对话记忆当唯一真相源 |
| 验证产物索引 | 测试命令、环境、候选内容、结果、日志／视频／截图引用 | 不向仓库提交密钥或无限量原始日志 |

短文档可以先承担上述几项职责，不必第一天制造空目录树或模板海。约束一旦有真实反复失败，应考虑提供可执行检查和可理解错误；没有证据的假想流程不应自动增加到每次开发的必经路径。

## 4. 持续开发与 PR 的最小事实合同

下面是治理记录的字段建议，不是要求先实现一套任务平台，也不是固定开发阶段：

```text
WorkItem {
  work_id, goal_or_spec_revision, requirement_refs[],
  objective, acceptance_refs[], dependencies[],
  owner_agent_ref?, branch_or_worktree_ref?, base_revision?,
  current_facts_ref, decision_refs[], implementation_commit_refs[],
  verification_refs[], review_refs[], pr_refs[],
  blocker_refs[], next_executable_action?, updated_at
}

VerificationEvidence {
  evidence_id, claim_or_acceptance_ref,
  candidate_tree_or_commit, dependency_and_environment_ref,
  command_or_procedure, exit_or_observed_outcome,
  tests_ran_or_unknown, evidence_artifact_refs[],
  observed_at, limitations[], superseded_by?
}
```

模型可以自行拆分、重排、合并任务或改变实现方案，必须保留当前目标与需求引用，不能靠改任务文字把未实现要求消失。blocked、implemented、verified、PR open、merged 是不同事实；不要强迫它们压成一个 done 布尔值。PR 开着不会自动证明验收通过，CI 通过也不会证明未被测试的用户需求已实现。

**提交 PR 后继续。** 向 upstream 交付 PR 时，记录 URL、head/base、验证和剩余事项，选择下一个可执行工作。后续开发可使用 fork 内自主整合的成果；需要表达交付依赖时，可采用 stacked branch／PR 并固定其 base，上游改变后重新集成和验证。不得把同一共享工作树中的未提交改动当作别人随时可用的稳定 API。

**在 fork 内自主整合，向 upstream 以 PR 交付。** Agent 自主提交、合并、重构，并修复工具及 CI；这些操作按 U51 持续执行。向 upstream 通过 PR 持续交付和更新，其审查与合并状态单独跟踪。fork 中的整合和后续开发由 Agent 自行推进，真实执行失败由 Agent 记录并继续处理。

**恢复先对账。** 新会话读取 SPEC 当前修订、任务／交接索引、Git status/log、活动分支及 PR 真实状态，确认哪些 child 还活着，哪些检查仍适用于当前输入，再续做。不要重派已完成任务，不要把失联 child 当已终止；不能确认的范围保留未知。这里使用朋友开发环境的 harness 事实，不冒称尚未开发的 Shenko 已经替其提供可靠性。

## 5. 并行协作、自审与防止假完成

1. **按依赖与写入责任并行。** 可独立研究、实现、测试和审查的工作分派给多个子 Agent；共享接口有明确当前合同和整合责任。分工由 Agent 随实际依赖自主调整，跨模块重构和工具／CI 改进纳入同一协作安排。独立 worktree 隔离文件改动，不能隔离同一个外部测试账号或数据库的效果，相关环境也须分配或协调。
2. **brief 足够自足，目标不缩小。** 子 Agent 获得当前任务相关资料与全局约束，不必复制整段聊天。控制者始终保留完整产品需求账目；局部 PR 的成功不成为停止整个 /goal 的理由。
3. **实现者自审与独立审查分工。** 自审检查疏漏和验证范围；独立审查直接读指定 BASE…HEAD 和相关合同。对单线程主体、全局推理容量、unknown effect、回退事实保全等边界，需要能推翻实现的具体时序和故障场景，不满足于“代码看着合理”。
4. **证据来自行为。** 编译通过、单元测试通过、端到端路径可用、故障恢复成立是不同命题。UI 用实际交互、状态和必要视觉证据；QQ／tmux 用真实目标或明确隔离 fixture；无法运行的外部条件写 blocked／unverified，不能用 mock 结果冒充真机通过。
5. **检查与判断都可改，但不能伪造历史。** 可以修正错误测试、更新过时 rubric、替换验证器；解释为什么原检查不再对应需求，并保留实际失败和新证据。不得把用户未授权的范围缩减伪装成技术重构，也不设置永久不可改的测试或规则区域。
6. **审查要收敛。** 优先修复可复现的实质缺陷；对误报写依据，对无关旧问题建后续任务。修复后检查受影响范围；没有具体剩余风险时，不无限叠加新 reviewer 或反复全测。复用证据时确认其输入仍一致；真正改变输入或无法判断依赖则重验。
7. **从需求端定期复核。** 重新核查已勾选项和关键用户场景，发现缺口形成新任务。核心能力仅有 stub、被静默降级或只剩文档时，不能报完整产品完成。里程碑帮助交付顺序，不成为把完整要求截断成 MVP 的理由。

## 6. 哪些上游规则不能直接移植

| 上游模式 | 对本次委托的问题 | Shenko 替代 |
| --- | --- | --- |
| Symphony 默认停在 Human Review | 将一个交付状态变成持续开发总暂停 | fork 内自主整合；upstream PR 独立跟踪、修复，审查期间继续开发 |
| Superpowers 不并行派实现者 | 将共享目录冲突规避扩大为全局串行 | 独立 worktree、依赖图、写入归属、候选集成验证 |
| 每个分支结束再问 merge／PR／keep | 用户已授权 fork 内自主合并及 upstream PR 交付 | Agent 自行整合 fork、交付 upstream PR，记录后继续 |
| OMP 贡献流程逐次要求 GitHub 发言确认和亲写句子 | 外部项目维护规则会妨碍已获授权的自主交付 | 在 Shenko 仓库采用本次用户授权范围，不复制他仓的投稿政策 |
| Spec Kit 未勾清单就停下问 yes/no | 已知缺口被转成人类调度负担 | 自主补查／修复／记录真实阻塞，并选择其他可做工作 |
| 所有任务统一走完整 gstack sprint／ship 仪式 | 不同改动付出相同成本，形成大量无效阶段 | 按真实风险、输入变化与验收要求选择所需能力 |
| 每次状态汇报都重新全测／永远增派 reviewer | 持续消耗但没有增加有效证据 | 绑定输入的证据复用，针对具体剩余问题重验 |
| 将任一上游 prompt／流程提升成永久不可变规则 | 违反 Shenko 全部实现可演进、能力可替换的方向 | 规则与策略版本化；现行用户合同和真实事实在改动中继续可追溯 |

常规工程决策和持续推进由已受委派的开发 Agent 负责；真实环境状态、执行失败及 upstream PR 状态分别记录，保留可追溯的事实与证据。

## 7. 交接给开发者前最值得写清的五件事

- **完整目标及停止条件**：完成完整 SPEC，阶段成果可独立 PR；没有把剩余要求自动归为永不实现的“future work”。
- **现有授权**：按 U51 在朋友的 fork 内自主规划、并行委派、修改实现与文档、重构、安装和改进工具、修改 CI、验证、提交及合并；向 upstream 通过 PR 持续交付和更新，并持续推进后续工作。
- **事实与裁决入口**：WHY／SPEC／原始需求／已裁决问题如何导航；计划可以调整，未授权改变用户目标不可以伪装成计划优化。
- **恢复入口**：下一次 /goal 或恢复会话先读哪些文件和远端状态；所有活动 worktree、PR、验证和未完成承诺可找到。
- **验证与交付证据**：真实可用命令由实现仓库补齐；现在没有命令就明确尚未建立，不提前在文档中声称通过。PR 描述解释问题、改动、实际验证、未解决差距和后续依赖。

## 网页复核入口

供主 Agent 独立打开后引用；正文的能力细节以固定源码链接为准。

- OpenAI harness engineering：https://openai.com/index/harness-engineering/
- OpenAI Symphony 官方介绍：https://openai.com/index/open-source-codex-orchestration-symphony/
- Anthropic 2025 长时 harness：https://www.anthropic.com/research/effective-harnesses-for-long-running-agents
- Anthropic 2026 应用开发 harness：https://www.anthropic.com/research/harness-design-long-running-apps
- Superpowers 官方仓库：https://github.com/obra/superpowers
- gstack 官方仓库：https://github.com/garrytan/gstack
- Spec Kit 官方仓库：https://github.com/github/spec-kit
- Symphony 官方仓库：https://github.com/openai/symphony

所有版本表与固定文件通过 GitHub 只读 API 核验。没有把动态首页缓存、社区传闻或示例输出当成实际测试结果；本报告也未核验或比较这些工具在 Shenko 上的实测成功率。
