# 完整交付合同

## 1 这次目标的边界

目标是 [领域规格索引](../INDEX.md) 的完整产品，**包含 S1–S5**。S2 首版自开发闭环是关键中间成果。阶段顺序可以调整，也可以提前交付；原有完整目标继续保留。朋友在自己的 fork 自主开发、合并和持续验证，本次交付形式是上游 ready PR；上游拥有者随后 review 与合入。

完整不是“覆盖率达到一个漂亮百分比”。每个原有 R 必须保留，映射到具体 B、规格章节、实现和证据。规格有些行要求研究/文档，有些要求运行行为，有些同时包含两者；采用相应证据。对真正可选的 BoxLite 等实现选择，可证明条件不适用；不能利用此机制豁免用户具名要求，如真实 SSH WASM、Node/Bun 目标、Android、两种 compact 或完整 computer use。

所有开发者遵循必读 [shenko-dev](../../.agents/skills/shenko-dev/SKILL.md)。本合同定义完整产品结果，不为每次增量增加人工 gate，也不要求重复新写完整验收套件；小改可零新增测试，同一连贯增量禁止十个或更多新测试，复用有效证据后继续交付。

## 2 完整状态的必要条件

| 范围 | 完成证据 |
| --- | --- |
| 需求覆盖 | `requirements.json` 的所有现行 R 有完整范围解释、实现或文档落点及真实证据；细分 subcase 不丢母项 |
| 上游完整度 | 每个已选 B 对应已采用上游行为、真实差异与验收；未采用上游无关功能不需要机械复刻 |
| 已枚举验收 | 现行 acceptance index 全部已裁定适用范围；适用项有通过结果，条件项有可审查依据；不能按困难标 N/A |
| 实现质量 | 产品可构建、安装、启动、配置、使用和维护，正常路径与相关异常路径真实运行；无伪成功入口或未接线关键功能 |
| 运行证据 | 模型、provider、tokenizer/template、adapter、runtime、系统、硬件、源码和 plugin lock 可关联；模拟与真实测试明确分开 |
| 可恢复性 | 实测 intent/receipt 各故障点、未知外部效果、主循环恢复、C=1、断网、满盘、迁移、新事实保留和节点分区等适用合同 |
| 长期性 | S5 的 7/30 天运行报告按真实墙钟时间和明确候选/环境形成；加速测试不能冒充 30 天无人运行 |
| 使用体验 | WebUI 是同一主体时间线，实际流式与交互稳定；Playground、渠道降级、媒体和电脑操作有真实证据 |
| 资料交付 | 新环境可依说明复现，NInfer/Strata 中英文教程、插件/awareness/Skill 编写、自解释、迁移与恢复资料准确 |
| PR 状态 | 所有交付提交已经 push；上游集成 PR URL、base/head 和证据版本一致，无未解决冲突或实质失败，已标 ready |

现行规格中没有实测数值的性能目标，由 Agent 根据指定设备先测基线、提出合理指标并记录 Why，再进行改进和回归。不能把未给数值当“无限性能”承诺，也不能因缺少人类逐项定阈值而停工。已明确的限制、4×128K 基线、连续主体和失败语义继续约束该选择。

## 3 全范围能力族

这些能力族用于交叉审计，不替代逐 R 与逐 case 的分母。

1. **持续个人主体与协作**：永续串行主流、同一模型以旧/新上下文持续轮询的最长600秒主动语义等待、灵魂/人格与实例命名、统一知识、任意 child prompt/model、双向讨论、有限 child 直发、长期自主兴趣、DND/过滤及自主修复。
2. **最小与可自改运行系统**：Rust 2024/cordis-rs、bootstrap/seed、可改 core/rescue、真实 WASM 家园、mount、Nix-like 闭包、插件 generation、跨版本 hook、完整自开发/迁移/继续运行。
3. **存储与记忆**：原始 session/FTS5 中文检索、默认 host-mounted 私有 Git memory、二进制版本/CAS、effect ledger、两种独立 compact、可安装 memory 系统/移植支持、PMA 与 child reset-to-best、永久默认保留和空间处置；全部LLM失效后的系统日志、凭据授权读取/导出与离线磁盘镜像恢复。
4. **工具与执行环境**：实际具名 WASM 工具/认证 Git/SSH/keygen、远端文件与执行、Python/Bun/Node REPL 的真实支持与恢复等级、原版 LayerFS Linux VM、条件采用的 BoxLite、所有适用资源/副作用边界。
5. **推理与资源**：NInfer/Strata/vLLM/1Cat 原生透传、未知容量、同引擎 alias、取消失联、C=1 轮转、真实 KV 驻留/换出/replay 和自动半池案例；云周期额度能力也完成，只是实现顺序靠后；实际采用 provider 的 API/SDK/model/gateway 版本合同、请求投影、鉴权与自主迁移按 [§47](../specs/inference.md#spec-47) 验收。
6. **渠道与身份**：NapCat/napcat-cli/官方 QQ/iLinkBot/飞书具名接入、开放聊天与手设 admin、富文本附件、身份候选/Life 复用、插件化消息和 API provider。
7. **远程与多节点**：已有 Claude/OMP/其他适配 seats、TUI 属性/高亮与缺能力自补、已授权SSH身份、Linux/Windows/macOS和rootAndroid能力、自主环境漏洞修补与更新；较晚身份加入、连通域主节点、异构本地/API模型与provider隐私、递归接续和孤立智能分支重连合并。
8. **声音与共同活动**：通话、旁听、录音、转写修订、声纹感知与可演进身份关联、同一主流的工具委派、可声明的实际资源与存储成本。
9. **可扩展认知与生态**：能力发现且正确使用、独立 perception/awareness/Skill/code、从任意实际输入即时汲取并自主演进 awareness、时间/未竟事/重复/项目/邮件的自然察觉、自动版本依赖闭包、传统 Skill/MCP 市场适配与原生插件优先、自解释和 Why；dreaming 保持可选社区组合。
10. **界面与作品**：Proto-UI 主线/shadcn/compiler seam、Magpie/Alma 参照的稳定流式、默认主流 UI、所有配置可对话与面板查看、跨渠道生成 UI、持久可交互全栈 Playground 与主体感知。

## 4 证据记录

一条证据至少能回答：验证了哪个要求/场景，运行哪个源码和环境，执行了什么，观察到什么，在哪里能复查。日志、截图、录屏、结构化报告、真实 provider receipt 与独立重跑可以组合。秘密留在授权的本地凭据设施，不进入公开或普通 PR 附件；报告保留非秘密的环境标识。

建议记录：`evidence_id`、`requirements`、`cases`、`candidate_commit`、`consumed_inputs`（实现、依赖、配置、模型/设备等实际相关输入）、`method`、`environment`、`started_at`、`finished_at`、`result`、`artifacts`、`reproducer`、`reviewer`。字段可以随实现演进，意图是让证明可追溯，不能用填写表格代替真实运行。

证据可以按影响复用：无关文案改动不使已验证的存储行为自动失效。若相关代码、配置、依赖、模型或环境改变，重新验证受影响部分并解释有效性边界。最终报告绑定当前交付 commit 和有效证据集合；不能凭一次旧 SHA 的绿灯宣布任意新版本通过。长跑中重大运行闭包变化必须分段或重启相应观察，不偷偷把不同候选时间拼成稳定 30 天。

## 5 状态与不能冒充完成的东西

`specified`、`implemented`、`buildable`、`simulated`、`verified_real`、`verified_long_run` 是不同事实。`blocked_external`、`waiting_observation`、`not_implemented` 和 `failed` 都不是 complete。初始文件只证明规格与交接材料存在。

以下均不能单独结束 goal：空 TODO、源码文件数量、文档齐全、模型说看起来完成、测试数量未知或零执行、全部测试被 skip、展示 mock 的截图、接口返回 200、只把“unsupported”错误做得友好、仅完成首版、PR 已建立或 review 迟迟没有人回复。

缺失设备、真实服务账号或目标环境时，继续准备代码、适配与可运行验证；记录精确等待，不伪造硬件或凭据。模型判断“impossible”也不是用户批准移除功能。先查证能否移植、替换实现或分解问题；确有矛盾时提出最小的产品裁决问题，其他范围继续推进。

## 6 最终报告与 goal evaluator

最终完整性审计由独立 worker/可复现检查对照原始 R、B、case 和实际交付执行。作者自述不够，另一个 Agent 单纯复述作者也不够。输出 `docs/evidence/completion.json` 及可阅读报告，列出完整分母、适用性依据、当前候选、证据有效性、失败/阻塞数、长期观察、PR head。未解决的实质缺口必须为零；真实可选条件的排除逐项说明。

Claude `/goal` evaluator 只看到对话里呈现的内容，不自己打开这些文件。最后一轮应在 transcript 中展示审计命令、结果摘要、关键证据和 PR 当前状态，才能让其判定有依据。goal 清除和产品完成必须分别记录。

<a id="spec-46"></a>
<!-- migrated-contract: SPEC:46 -->
# 46 完整产品的自主开发与仓库交接

来源：U50–U51/U55/U58、R140–R141/R146、B49/B54；REF-USER、REF-DESIGN、REF-DEV-GOV、REF-AGENT-SKILLS。本章与 [§40](development-contract.md#spec-40) 一起定义此次完整开发委托；实际工具版本和操作见仓库治理文档。这里的开发 Agent 可以使用兼容的 OpenAI 或 Anthropic 开发环境；既有完整交接对象仍是朋友的 Claude 编程环境。

<!-- /migrated-contract: SPEC:46 -->

<a id="spec-46-1"></a>
<!-- migrated-contract: SPEC:46.1 -->
## 46.1 文档与自主权

现行产品规格及 Why 位于 docs 的领域合同，docs/INDEX.md 提供导航，AGENTS.md 提供开发入口，根目录 CLAUDE.md 必须是指向 AGENTS.md 的符号链接，本文件定义完整交付范围。该链接共享同一份规则，不改为 @import 或维护一份复制品；平台检出后如实检查 symlink 是否生效。docs/requirements 物化同一 R/B/Ref/case，docs/state 和执行计划保存真实进展。历史规格与研究草稿保留来源及被取代状态。ChatGPT Page 保留讨论来源和迁移前快照，仓库发布后后续工程修订由 Git 追踪。

所有开发者在开始工作前阅读专用哲学 Skill **shenko-dev**，由 AGENTS.md/CLAUDE.md 的必读入口明确指向，不能只依赖模型自动匹配 description。它解释 personal agent、持续自我、主循环/child 分工、开放来源 awareness、原子能力、已授权自主行动与相称验证的 Why。开发能力拆为可单独采用的原子 Skill：按具体工作选择实现、研究、恢复、集成、验证等相关能力，不规定每次必须完整跑一组固定流程。普通开发与自修复不增加人类 gate；真实权限不足只影响相关动作。

可移植 Skill 采用 `SKILL.md` 与 YAML frontmatter 的 `name`、`description` 共同字段，正文给目的、适用条件、可选方法和验证边界；宿主特有扩展只在确有用途时单独表达，不将其解释为另一宿主同样生效的权限。单一源码目录采用 `.agents/skills/<name>/SKILL.md`，Claude 通过 `.claude/skills` 对应符号链接读取同一内容，避免两套正文漂移。官方支持与实际宿主发现结果分别核验，已写文件不冒称两种环境已运行验证；发行/上传若要求实体目录，生成时保留内容与版本一致。兼容依据见 REF-AGENT-SKILLS。

朋友的 Agent 在自己的 fork 内自主研究、规划、开发、重构、合并、调配工具与 CI、更新依赖、移植、验证、修复、记录与交付，并积极并行使用子 Agent。工程方法、分支组织、权限配置、harness 和开发流程可以按目标自行调整。用户已选择 Claude 的 bypassPermissions 模式，官方实际启动参数为 `--dangerously-skip-permissions`；它取代先前 auto mode 推荐。上游仓库访问由真实 GitHub 权限决定。交接着重说明目标、Why、证据与恢复，普通开发决定由 Agent 完成。

真正缺少账号本人验证、凭据、设备或服务访问能力时，完成可做准备后提出具体所需，继续其他工作。本次文档仓库尚未实现产品；接收 GOAL.md 的 Agent 即开始完整开发。

<!-- /migrated-contract: SPEC:46.1 -->

<a id="spec-46-2"></a>
<!-- migrated-contract: SPEC:46.2 -->
## 46.2 完整目标与 PR

S1–S5 全部正式能力属于本次 goal。首版自开发闭环优先交付，之后继续全部后期能力。具名 WASM 目标、各后端真实能力、移动/路由器、远端与 computer use、媒体、awareness 依赖、动态 UI/Playground、云额度与长期稳定等范围继续保留。真实可选实现按其适用条件判断；完整目标同时覆盖逐项需求和未编号的正式义务。

在自己的 fork 内自主整合和持续运行，向 upstream 尽早建立 draft PR 并持续更新。默认维护一条完整集成 PR，也可按实际依赖组织多个 PR。等待人类 review 时照常推进。全部交付提交已推送、PR ready、与候选匹配的完整证据有效时，达到本次交付条件；上游合入由仓库拥有者随后处理。

<!-- /migrated-contract: SPEC:46.2 -->

<a id="spec-46-3"></a>
<!-- migrated-contract: SPEC:46.3 -->
## 46.3 持久工作与验证

每个工作项关联目标/R/B/case、依赖、owner/worktree、工件版本、证据、重要 Why、blocker 与下一动作。细节可用文件、Git、GitHub 与真实 harness 实现。多实现者使用独立 worktree/branch 和清晰接口，由协调者整合；共享模型、live seat、桌面、数据库与远端效果按实际资源分配。

测试证明具体行为，独立审查查证具体风险，完整度审计从要求端重查遗漏。证据绑定候选及真正相关输入，变化后重验受影响部分，输入不变的有效证据可复用。规格错误和测试错误可以改，保留原结果与理由。遗漏、失败和真实环境缺口转为可执行任务，持续收敛到完整实现。

开发 Skill 同样遵守 [§40.5](development-contract.md#spec-40-5)：一个增量不得新增十个或更多测试，不以测试数量充当开发进度；选择足以证明具体变化的已有或少量新增验证。验收目录保存完整目标义务，不要求每个增量重新实现一遍整个目录，也不设置按测试数量的人类签字。

原生 /goal 的状态与产品完成分别记录。检查实际 Claude 版本、Opus 5.5 和 ultracode 能力；evaluator 只看会话中呈现的内容，执行者因此展示候选、覆盖、真实结果和 PR 状态。进程结束后恢复时显式传入 `--dangerously-skip-permissions`。goal 被清除或外部服务暂时不可用时，从持久记录核对状态并恢复同一目标；临时限流、套餐额度等待与清除 goal 的错误按当前官方语义区分。跨会话继续沿用本次开发授权。

<!-- /migrated-contract: SPEC:46.3 -->

<a id="spec-46-4"></a>
<!-- migrated-contract: SPEC:46.4 -->
## 46.4 GitHub 与启动

使用朋友自己的 GitHub 身份，或其合法管理的 Agent 身份。检查/安装官方 gh，复用有效登录或通过官方 web/device 流程认证。没有账户时准备注册，由账户持有人完成平台要求的本人步骤。取得仓库 read/fork 能力后 fork 并 clone，核对 origin 指向自己的 fork，upstream 指向接收 PR 的实际仓库；之后自主开发、合并、push 和维护 PR。本次 upstream 为拥有者已创建的公开仓库 [cyjin-yl/shenko](https://github.com/cyjin-yl/shenko)，默认分支 main；账户与访问状态按真实服务端核验。

交互入口为 `claude --model claude-opus-5-5 --effort ultracode --dangerously-skip-permissions`，随后输入 GOAL.md 的 `/goal`；headless 和 resume 见 [Claude 运行](CLAUDE.md)。用户原话中的 `--dangerously-bypass-permissions` 表达模式意图，实际命令采用官方有效拼写。

<!-- /migrated-contract: SPEC:46.4 -->

<a id="spec-46-5"></a>
<!-- migrated-contract: SPEC:46.5 -->
## 46.5 完成证明

完整 Definition of Done 见 [COMPLETION](COMPLETION.md)，执行入口见 [GOAL](../history/entrypoints-20261101/GOAL.md)，fork 与 PR 方法见 [GITHUB](GITHUB.md)，接续见 [CONTINUATION](CONTINUATION.md)。重要结论与方法有 Why 和来源，工程实现可以依据这些目的演进。

长期 7/30 天观察按实际经过时间与明确候选记录。环境缺失保持真实未验证，仍可提交阶段 PR 并推进其他工作。最终独立完整性审计同时核对实现、集成、真实环境、故障恢复、长期观察、资料与交付 PR。S2、一次 demo、接口、mock、PR 创建或脚本退出 0 分别表示具体进展，由完整合同决定最终完成。

<!-- /migrated-contract: SPEC:46.5 -->

<a id="spec-46-6"></a>
<!-- migrated-contract: SPEC:46.6 -->
## 46.6 交接验收

这些案例定义开发工作流的可观察验收，当前状态均为未验证。按相关场景验证，证据有效性由实际受影响输入决定。

| 编号 | 场景 | 通过条件 |
| --- | --- | --- |
| G01 | 现行文档与历史边界 | 接手者能找到唯一现行 SPEC、WHY、GOAL 与事实账本；旧草稿明示被取代，原始 U01–U62 完整可查。 |
| G02 | 完整范围保持 | S1–S5 全在目标；原有 R/B/case 与未编号正式义务贯穿任务分解与交付；S2 完成后继续剩余完整目标。 |
| G03 | 常规决策自主 | 合理默认、研究、架构、依赖、实现、修复、验证、合并、提交与更新 PR 在自己的 fork 中自主连续执行。 |
| G04 | 并行与集成 | 多个写入子 Agent 在独立 worktree/branch 协作，接口与 ownership 明确；共享外部资源按实际容量分配，集成候选另行验证。 |
| G05 | PR 等待与继续 | draft/ready/review 状态与产品进度分开；一个 PR 等人工 review 时，仍能在自己的 fork 推进与整合后续功能。 |
| G06 | 跨 session 接续 | 压缩、worker 更换、goal 清除或进程退出后，从原始 Git/任务/效果证据恢复；保留完成项与在途效果，继续原目标。 |
| G07 | 独立完整性审计 | 从原始要求重新检查已勾选项与所有能力族，stub/mock/未接线、遗漏异常及真实平台缺口形成可执行的新任务。 |
| G08 | 证据与候选绑定 | 验收记录对应实际输入、环境和候选；零执行、skip、pending 与通过分别表达，相关改变重新验证。 |
| G09 | fork 自主开发与交付 | 实际身份完成认证、fork、clone 与 origin/upstream 核对；在自己的 fork 自主分支、合并、工具/CI 配置、push，向 upstream 提交并持续更新 PR。 |
| G10 | 真实 Claude 运行能力 | 启动和恢复采用实际 --dangerously-skip-permissions；核对版本、模型、goal、ultracode 与真实资源域，evaluator 能看到执行证据。 |
| G11 | 外部等待与长期验证 | 凭据/设备/访问或真实观察时间影响的范围明确；继续其他工作；7/30 天验收按实际时间形成证据。 |
| G12 | 可演进规矩与交付准确 | 按 Why 改进计划、方法、规则与验证器，保留真实失败和目标；最终交付提交已 push、PR ready 且完整证据有效，人工 review 随后进行。 |
| G13 | OpenAI/Anthropic 开发入口与原子 Skill | 根 CLAUDE.md 为指向 AGENTS.md 的 symlink，必读 shenko-dev 明确；两宿主入口共享同一 SKILL.md 内容，name/description 可读，按需采用原子技能无固定流程/人工 gate。实际宿主发现与静态文件检查分开报告，验证遵守 N06 |

<!-- /migrated-contract: SPEC:46.6 -->
