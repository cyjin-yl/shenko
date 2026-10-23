# Complete Shenko

## Assignment

你是仓库拥有者朋友的开发 Agent。你的目标是把 [SPEC.md](SPEC.md) 所规定的 Shenko **完整实现、验证并以持续维护的 PR 交付到上游仓库**。协调模型使用账户实际可用的 Claude Opus 5.5；使用已核验的 ultracode 和并行 subagents。S1–S5 是实施顺序，全部属于此目标；不要在完成脚手架、MVP、S2 首版或开出 PR 后结束。

这是一项持续授权的开发任务。使用 `--dangerously-skip-permissions` 启动，在你自己的 fork 内自主研究、决策、实现、合并、重构、装工具、配置 CI、调配子 Agent、验证、提交和更新 PR。按 Why 调整工具、架构、算法、插件协议、数据格式和任务分解，完成用户已经确定的全部目标。上游访问由实际 GitHub 权限决定；人类 review 进行时继续推进开发。

## Start from the actual environment

读取 AGENTS.md 与必读 [.agents/skills/shenko-dev/SKILL.md](.agents/skills/shenko-dev/SKILL.md)、WHY.md、docs/INDEX.md、docs/governance/COMPLETION.md，以及 SPEC §§1–2、28–29、40、46–47。随后按任务读对应章节、R/B/Ref 与验收行，按需选择原子开发 Skills。确认当前目录、Git 状态、工作区现有文件、实际 Claude 版本/模型/模式以及当前权限。保存既有工作，不用重新初始化覆盖已存在的开发进度。

GitHub 使用朋友本人或其合法管理的 Agent 身份。按 [GITHUB.md](docs/governance/GITHUB.md) 检查并安装官方 `gh`，复用有效登录，或发起官方 web/device 登录；没有账户时准备注册资料，由账户持有人完成平台要求的本人步骤。取得 read/fork 能力后 fork 并 clone 自己的仓库，设置实际 commit 身份。凭据使用当地适合的凭据存储；仅将真实缺失的账号动作汇总为一次具体请求，其他准备与开发继续。

核对 upstream `cyjin-yl/shenko` 和实际 default branch（交接时为 `main`），origin 指向你的 fork，upstream 指向接收 PR 的仓库。开发、分支、合并和内部发布方式由你自行组织。仓库可用后直接开始。Claude 启动、headless 和恢复参数见 [CLAUDE.md](docs/governance/CLAUDE.md)。

## Drive the whole implementation

审计需求和合同，建立可执行的依赖图和验收映射。现行 v0.6.2 有 147 项 R、55 组 B、216 条已枚举验收；v0.6.1 的209条完整保留，本轮补7条持续主体、察觉、节点/恢复和开发Skill相关定义。数字是当前可查基线，后续新增继续进入范围。研究/说明性要求以完整资料核验，真实行为要求以实际实现和运行证据核验；验收定义不要求每个小改动各写一份新测试。

优先打通真实关键链，给后续开发提供自举能力：持久 WASM、默认 host-mounted Git memory、Cordis 装配、单持续主体、durable children/ledger、推理 broker、NapCat、SSH/HostExecution、真实 tmux/Claude/OMP seat、自解释、候选构建与自迁移、两个独立 compact 插件、默认主体 WebUI、对话配置、基础持久 Playground 和 bootstrap。该链完成后继续所有后期能力与平台，最终按完整合同验收。

并行派发相互独立的实现、上游研究、故障验证和视觉审查。为写入工作分配隔离 worktree/branch；明确接口、依赖、任务 ownership 和资源域。动态增减 children，使用实际支持的 ultracode。共享模型、桌面输入、live seat 和数据库按真实资源域协调，以有效进展决定并行度。

必要时开发新的工具、awareness、Skill、provider、移植层或外部辅助程序，完成原来的用户旅程后验证。第一版就应能通过 NapCat 遥控既有 coding seat 继续开发自身。对目标技术真实移植，不以 host wrapper、接口、mock 或 README 声明替代最终 WASM 功能。

保持完整行为：错误、取消、外部效果未知、重试、恢复、升级、满盘、断网、并发、身份、真实资源与 UI 状态都按适用合同处理。独立评审和真实运行证据用于发现遗漏；不要用多个 Agent 一致称赞代替验证。UI 依据功能与视觉合同自主实现，无逐屏人类批准。

Rust 使用 2024 edition。所有工程判断与已授权交付自主完成，不设置人工 gate；每个增量只解决实际验证问题，优先复用证据，同一连贯增量不得新写十个或更多测试。完整验收清单不能变成小功能的重复测试工厂。主主体持续/语义等待、从输入学习 awareness、设备身份加入与分区容灾、provider 隐私和无模型恢复按本轮现行裁决实现。

## Keep a durable record

维护 `docs/state/goal.json`、`docs/state/coverage.json`、执行计划和可检索的证据。重要 checkpoint 包含工作版本、已证明事实、未完成项、下一可执行任务、决定的 Why、资源和在途效果。每个上下文结束或 worker 更换后从这些记录与实际 Git/运行状态续接，授权不丢失。

上游参考、协议或依赖发生变化时，核对原始资料与锁定版本再更新。Proto-UI 每开发任务按规格追踪最新主线，记录与验证 bump；保持 compiler 适配空间。方法不适用时改方法，不能默默取消用户要求。

## Deliver without waiting for review

有有意义的实现切片就创建 draft PR，默认维持一条完整集成 PR；有需要可以维护有依赖关系的 focused PR。继续 push、修复 CI、回应已有 review、补证据和实现剩余部分，不因 PR 未获人工 review 暂停。用版本化 body file 更新 PR；维持清晰提交与变化说明。

需要真实外部环境时尽可能自行检测、安装和准备。只有缺凭据、设备、账户本人验证、实际权限或无法从规格裁决的产品分歧才形成外部 blocker。记录受影响 ID、已尝试方法、最小所需信息和可自动恢复条件；继续其他工作。不能用 blocked、deferred、future、unsupported 替代 full-goal 的完成。

## Completion condition

以 [完整完成合同](docs/governance/COMPLETION.md) 为最终条件：全部范围内 R/B/验收有与交付源码匹配的真实证据；具名后端、WASM 工具、平台和插件行为成立；跨模块与真实故障/恢复成立；长期运行按实际经过的时间证明；中英文教程、部署、迁移、依赖与源头对照可复现；没有未解决的实质缺口；上游 PR 已存在、已推送所有交付提交、无冲突且 ready for review。无需等待 review/merge 才算交付，不能在不完整时改成 ready。

为 `/goal` 的判定模型提供证据：在重要轮次输出当前候选 SHA、R/验收总数与各状态计数、真实运行证据路径及关键结果、PR URL/head SHA、剩余任务和外部等待。它不会自己读仓库或运行测试。最后一轮显示独立完整性审计与实际验收摘要，不只说“已完成”。

原生 `/goal` 被清除、进程退出或外部服务暂不可用时，保存真实进度。按 [CONTINUATION.md](docs/governance/CONTINUATION.md) 区分临时暂停、已清除目标与产品完成，恢复后继续同一完整目标。新进程或已结束会话的 resume 显式传入 `--dangerously-skip-permissions`。

## Copyable goal

在已 clone 的仓库中启动已配置的 Claude Code，再输入下面这条原生命令。它刻意保持短于原生 4,000 字符条件长度，并把可持久化细节放在仓库。

```text
/goal 完整实施本仓库 GOAL.md：读取 AGENTS.md、WHY.md、SPEC.md 与 docs/governance/COMPLETION.md。在自己的 fork 中自主开发、合并、重构、配置工具/CI、规划、研究、开并行 subagents/ultracode、实现、调试、验证和记录，持续向实际 upstream 提交更新 PR，等待 review 时继续推进。完整完成 S1–S5、所有 R/B/正式义务和真实验收；首版自开发闭环后继续全部能力。每个重要轮次展示候选 SHA、覆盖计数、关键证据、PR head 与剩余任务；跨会话按持久记录恢复同一目标。独立完整性审计、真实运行与长期验收均满足，所有交付提交已推送且 PR ready 无实质缺口时完成。外部等待按真实状态记录，继续其他可做工作。
```

从仓库外发给朋友的引导语包含实际 upstream URL，先以自己的身份 fork 并 clone，再读取 GOAL.md。该文档描述待执行委托；真实开发与会话状态由运行时记录。
