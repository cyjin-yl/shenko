# Shenko：Claude Code 完整开发交接核验

研究日期：2026-10-22；按 U51 更新执行方式。本文核验官方公开命令、身份与仓库协作、持续任务恢复。目标 upstream 为 `cyjin-yl/shenko`，拥有者现已创建公开仓库，默认分支 `main`；实际接续状态见 `docs/state/goal.json`。本报告研究官方机制，朋友的开发会话尚未启动。

## 1. U51 确定的接手方式

朋友使用自己的 Claude 与 GitHub 身份，在**自己的 Shenko fork** 中使用 `--dangerously-skip-permissions` 自主开发。Agent 可以创建和合并分支、重构任何组件、调整开发方法、安装工具、运行实验、配置自己可管理的权限与 CI、调配子 Agent，并将完整结果通过持续更新的 upstream PR 交付。分支布局、内部合并方式和 PR 拆分由开发者按实际工作决定；review 等待不暂停开发。

完整任务包含 SPEC 的全部正式 S1–S5。自开发首版、某个可运行切片和第一个 PR 都是中间成果。`GOAL.md` 定义完整交付条件，`AGENTS.md` 与 `WHY.md` 提供执行原则，当前 SPEC 与需求/上游行为对照表提供产品真相。接手者依据最新用户决定自主推进，并维护真实证据与剩余任务。

这是 U51 的工作方式；此前推荐的 auto mode、上游 collaborator 分支路线以及额外的逐操作审批约束均已由本修订替换。GitHub 的实际仓库权限决定可执行的远端操作，项目文档不虚构权限。

## 2. Claude Code 的官方支持

| 能力 | 核验事实 | 使用方式 |
| --- | --- | --- |
| bypass | 实际 flag 为 `--dangerously-skip-permissions`，等价于 `--permission-mode bypassPermissions`。 | 首次启动和从 transcript 重建时显式传入。 |
| `/goal <condition>` | 每个 session 一个 active goal；条件最多 4,000 字符；独立 evaluator 评估 transcript。 | 短入口指向完整 GOAL，执行者实际读取规范并呈现证据。 |
| headless goal | 支持 `claude -p '/goal …'`。 | 使用 `--output-format stream-json --verbose` 观察过程。 |
| ultracode | 是工作流设置；CLI `--effort ultracode` 同时开启它并设 `xhigh`。 | 与模型和资源容量分别记录。 |
| Opus 5.5 | 模型 ID `claude-opus-5-5`，要求 Claude Code ≥ 2.1.280。 | 核验朋友账号/provider 的实际可用性与运行模型。 |
| subagents | 当前默认允许嵌套到主会话下三层，可配置深度。 | 用于独立实现、研究、验证。 |
| agent teams | 实验功能，默认关闭；通过 `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1` 启用。 | 按任务选择，不把 team 面板当持久项目账本。 |
| continuation | `--resume <session-id>` 恢复指定会话；`--continue` 选择最近会话。 | 并行工作时保存并使用明确 session ID。 |

来源：[CLI reference](https://code.claude.com/docs/en/cli-reference)、[Goals](https://code.claude.com/docs/en/goal)、[Model configuration](https://code.claude.com/docs/en/model-config)、[Subagents](https://code.claude.com/docs/en/sub-agents)、[Agent teams](https://code.claude.com/docs/en/agent-teams)、[Sessions](https://code.claude.com/docs/en/sessions)。

### 2.1 flag、版本与并行

官方资料没有提供 `--dangerously-bypass-permissions` 这一拼写；可执行文档使用已核验的 `--dangerously-skip-permissions`。`--allow-dangerously-skip-permissions` 只是允许以后切换到 bypass，并不立即开启它。[CLI reference](https://code.claude.com/docs/en/cli-reference)

当前会话内 `/effort ultracode` 切换 ultracode 并保留 effort；该新语义及关闭 toggle 要求 ≥ 2.1.284。CLI `--effort ultracode` 自 2.1.203 起支持。持久设置使用 `"ultracode": true`；`effortLevel` 与 `CLAUDE_CODE_EFFORT_LEVEL` 的值不接受 `ultracode`。workflow 关闭或模型不支持 xhigh 时，该编排模式不可用。[Model configuration — Adjust effort level](https://code.claude.com/docs/en/model-config#adjust-effort-level)

子 Agent 请求计入相同账号用量。`CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS` 约束特定 Agent tool 的准入，**ultracode session 豁免该上限**；workflow agents 和 teams 另有各自限制。开发者依据真实容量调配并行与共享资源。teams 的 in-process teammates 不随 `/resume`、`/rewind` 自动恢复，恢复时核对产物并重新派发尚未完成的工作。[Subagents — Concurrent subagent limit](https://code.claude.com/docs/en/sub-agents#concurrent-subagent-limit)、[Agent teams — Limitations](https://code.claude.com/docs/en/agent-teams#limitations)

## 3. 启动与恢复命令

以下命令供朋友在其机器执行；本报告没有实际启动这些会话。先核验已有环境和身份：

```bash
claude --version
claude auth status --text
gh --version
gh auth status
gh api user --jq '.login'
gh repo view cyjin-yl/shenko --json nameWithOwner,isPrivate,viewerPermission,defaultBranchRef
```

缺少登录时使用官方流程，已有可用身份直接复用：

```bash
claude auth login
gh auth login --hostname github.com --git-protocol https --web
gh auth setup-git --hostname github.com
```

`claude auth login --console` 选择 Console/API 计费，与普通 Claude 订阅登录不同。凭据由凭据设施管理，检查输出保留 login、host 和权限等非秘密信息。[Claude CLI](https://code.claude.com/docs/en/cli-reference)、[gh auth login](https://cli.github.com/manual/gh_auth_login)、[setup-git](https://cli.github.com/manual/gh_auth_setup-git)、[repo view](https://cli.github.com/manual/gh_repo_view)

### 3.1 在自己的 fork 启动

```bash
claude --model claude-opus-5-5 --effort ultracode --dangerously-skip-permissions
```

进入后使用 [GOAL.md](../../GOAL.md) 的完整短入口。对应 headless 示例：

```bash
claude --model claude-opus-5-5 --effort ultracode --dangerously-skip-permissions \
  -p '/goal 读取 GOAL.md、AGENTS.md、WHY.md 和引用的完整规范，在自己的 fork 自主并行实现 Shenko 全部正式 S1–S5。自主合并、重构、装工具、配置开发权限与 CI，持续更新 upstream PR。以完整交付合同的真实证据完成，每个重要轮次展示候选、覆盖、验证、PR 与剩余任务。' \
  --output-format stream-json --verbose
```

`/goal` 与 permission mode 是独立机制。evaluator 只能评估会话里已经展示的结果，不自行读取 `GOAL.md`、测试日志或 PR；执行者需要读取、运行并把关键证据带回 transcript。[Goals — Run non-interactively / How evaluation works](https://code.claude.com/docs/en/goal)

### 3.2 恢复指定会话

以下命令用于原进程已结束、从 transcript 重建的会话。交互恢复：

```bash
claude --resume '<真实-session-id>' --model claude-opus-5-5 \
  --effort ultracode --dangerously-skip-permissions
```

headless 恢复：

```bash
claude --resume '<真实-session-id>' --model claude-opus-5-5 \
  --effort ultracode --dangerously-skip-permissions \
  -p '读取 GOAL.md 与最新检查点，核对实际 fork、PR、任务和证据，继续全部正式范围；确认 native goal 当前状态，按原因恢复同一产品目标。' \
  --output-format stream-json --verbose
```

从 transcript 恢复普通 terminal 会话时，旧 bypass 模式不会自动恢复；`-p` 恢复通常按新 `-p` 运行的模式启动，因此命令再次显式传入 flag。原启动若依赖 `--settings`、`--mcp-config`、`--plugin-dir`、`--fallback-model` 或 `--add-dir`，重建时按实际需要再次提供。[Sessions — Permission mode on resume](https://code.claude.com/docs/en/sessions#permission-mode-on-resume)

仍在运行的官方 background session 保留原进程与模式。v2.1.285 起，terminal 中的普通 `claude --resume '<session-id>'` 通常会 attach；更早版本会提示使用 `claude attach '<id>'` 或先停止。带 `--model`、`--settings`、`--permission-mode`、`--output-format` 等配置/输出参数，或使用管道、重定向时，当前运行中的会话不会被重新配置：命令不发送 prompt，并以状态 1 退出。因此，上述含这些参数的示例仅用于已经结束的会话。接回正在工作的执行者时用 `claude attach '<id>'`；确实要以新参数重建同一会话时，先 `claude stop '<id>'`，再执行恢复命令。terminal 中单独加入 `-p` 也不保证这一 attach 路径保持非交互。[Sessions — Resume a running background session](https://code.claude.com/docs/en/sessions#resume-a-running-background-session)

正常启动会加载项目规则、skills、hooks 等，并保留 session。`--bare` 会跳过这些自动加载且不使用普通订阅/keychain 认证；`--no-session-persistence` 会关闭可恢复会话记录。`--bg` 与 `-p` 是不同运行方式，不能组合。独立后台会话可通过 `claude attach`、`claude logs`、`claude respawn` 管理；任意已有 tmux TUI 则仍需对应的 seat/终端适配。[Headless](https://code.claude.com/docs/en/headless)、[CLI reference](https://code.claude.com/docs/en/cli-reference)

第一次交互启动 bypass 有 Claude Code 自身的一次性提示，headless 没有该对话框。实际 managed 配置、平台认证与主机执行条件仍按软件的真实行为生效；这里不增加项目审批步骤。[Permission modes — bypassPermissions](https://code.claude.com/docs/en/permission-modes#skip-all-checks-with-bypasspermissions-mode)

## 4. 自己的身份、自己的 fork

GitHub 要求人注册账号并负责其使用，机器账号也由人建立；需要的邮箱验证、MFA/SSO 等由真实账号流程完成。身份和源库访问可用后，Agent 自行完成 clone/fork、工具配置和后续开发。长期独立 bot 可以采用 GitHub App：installation token 一小时过期，可在 app 已授予的权限内自动续发。[GitHub Terms §B.3](https://docs.github.com/en/site-policy/github-terms/github-terms-of-service#3-account-requirements)、[Installation token](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-an-installation-access-token-for-a-github-app)

私库 fork 要求有源库访问权且 owner 允许 fork；fork 保持 private，目的账号/组织还受相应计划与策略约束。如果 upstream 是个人账号的私库，collaborator 只有 read/write 档位，没有只读 collaborator。因此“朋友没有 upstream push 权限”应以实际服务端配置核验，不能由一份邀请说明推导。U51 的开发地点是朋友自己的 fork；源库身份和权限事实不会变成逐项开发审批。[Forks](https://docs.github.com/en/pull-requests/reference/forks)、[Personal repository permissions](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/repository-access-and-collaboration/permission-levels-for-a-personal-account-repository)

默认复用朋友自己的 `gh` 登录。fine-grained PAT 当前仍有 outside/repository collaborator 等兼容缺口，且限定 resource owner；classic PAT 通过 `gh auth login --with-token` 使用时，CLI 文档要求的通用最低 scopes 为 `repo/read:org/gist`。实际凭据方案依据当前身份和所需操作选择，不依赖 owner 的登录。[PAT limitations](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens#fine-grained-personal-access-tokens-limitations)、[gh auth login](https://cli.github.com/manual/gh_auth_login)

### 4.1 Fork、开发与 PR

确认实际 upstream 后，在适当的父目录执行：

```bash
gh repo fork cyjin-yl/shenko --clone
cd shenko
git status --short --branch
```

核对 fork owner、origin/upstream 与实际默认分支；已有 fork 或工作成果时接续它们。Agent 自主选择分支组织、内部合并、重构、工具和 CI；默认分支也可作为自己 fork 的集成候选。仓库级 commit 身份采用朋友本人或其真实 bot 配置。[gh repo fork](https://cli.github.com/manual/gh_repo_fork)

有真实实现切片后，推送选定的交付分支并创建 upstream PR：

```bash
git push --set-upstream origin '<实际交付分支>'
gh pr create --repo cyjin-yl/shenko \
  --base '<实际upstream目标分支>' --head '<朋友login>:<实际交付分支>' \
  --draft --title 'Implement Shenko' --body-file '<实际PR正文文件>'
```

后续开发继续合并到 fork 中的候选并更新 PR：

```bash
git push origin '<实际交付分支>'
gh pr edit '<真实PR编号>' --repo cyjin-yl/shenko --body-file '<最新PR正文文件>'
gh pr checks '<真实PR编号>' --repo cyjin-yl/shenko --json name,state,bucket,link
```

PR 正文保持已实现范围、R/B/case 映射、被测 commit、证据、剩余任务与下一工作准确。完整合同达成后自行转 ready；完成的验收结果及交付提交就是交付依据，不需要等待 review 才推进下一任务。`--head user:branch` 显式选择 fork，当前 gh 对该 user 位置不支持 organization owner；正文使用 `--body-file` 保留换行。[pr create](https://cli.github.com/manual/gh_pr_create)、[pr edit](https://cli.github.com/manual/gh_pr_edit)、[pr checks](https://cli.github.com/manual/gh_pr_checks)、[pr ready](https://cli.github.com/manual/gh_pr_ready)

### 4.2 CI 按真实权限工作

Agent 自主配置自己 fork 的 workflow、runner、可管理的权限与所需凭据。upstream 的 fork PR workflow、secrets、`GITHUB_TOKEN` 权限和运行批准由其实际设置决定；本地 `gh` 登录与 Actions token 是不同身份面。基础 fork PR workflow 权限是只读 token、无 secrets，额外能力来自相应设置。报告分别呈现 fork CI、upstream CI 与本地测试的证据；pending/skipped 保持其真实状态。尚缺的外部因素只影响依赖它的检查，其余开发继续。[Actions settings](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/enabling-features-for-your-repository/managing-github-actions-settings-for-a-repository#enabling-workflows-for-forks-of-private-repositories)

## 5. 产品目标与 native goal 分开恢复

完整 S1–S5 目标持久存在于 GOAL、需求覆盖和检查点。native `/goal` 是当前 session 的推进机制。以下状态不等同于项目是否完成：

| native 观察 | 官方行为 | Shenko 的接续依据 |
| --- | --- | --- |
| `met` | 清除 goal，并记录 achieved。 | 对照交付合同和真实证据确认产品状态。 |
| `impossible` | 清除 goal，并记录失败原因。 | 核对障碍，研究替代方法或修复条件，再继续同一完整目标。 |
| 自管凭据认证失败、信用余额耗尽、无法 compact 的溢出、模型不可用 | 相应错误可清除 goal。 | 保存未完事项，恢复真实运行条件后重设原条件。 |
| 临时 API rate limit、Claude usage limit | goal 保留；交互运行可显示 paused，按实际恢复条件续行。 | 记录等待原因/reset 信息，继续可做的独立工作。 |
| 暂时断网、服务过载 | goal 保留；支持的交互版本重试，达到次数后暂停。 | 观察实际进度，选择恢复机制。 |
| 连续数轮无工具进展 | 可能停下并保留 goal，等下一 prompt。 | 排查停滞，给出可执行续接。 |

交互自动重试/暂停提示要求相应版本；headless 不具有全部相同的 idle check-in 行为。active goal 可随 session resume 恢复，但 turn count、timer、token-spend baseline 会重置。原始长跑时间和累计项目进度由检查点保留。[Goals — Resume / Evaluation / Failures / Check-ins](https://code.claude.com/docs/en/goal)

项目状态可以是 `in_progress`、`waiting_external` 等，harness 状态可以是 active、paused、cleared；这些是项目的状态投影，具体字段以当前状态文件为准。evaluator 的判词、执行器退出码和真实产品验收分别入账，避免把一个会话结束误解为完整交付。

## 6. 持久交接记录与故障恢复

执行合同见 [CONTINUATION.md](../governance/CONTINUATION.md)。最少保留以下可核对事实，存储形式可随开发演进：

| 记录 | 内容 |
| --- | --- |
| 交付基线 | GOAL/SPEC revision、全部 S1–S5 需求和上游行为映射、当前有效裁决。 |
| 实际工作位置 | fork/upstream、工作目录、交付分支、HEAD/dirty work、PR URL/base/head。 |
| 会话与配置 | 精确 session ID、所在机器/可恢复 transcript、CLI 版本、真实模型、启动参数及必要配置引用。 |
| 工作与资源 | worker/worktree ownership、产物、运行任务、共享资源、在途效果与 unknown 回执。 |
| 证据 | 对应要求、源码/环境、实际命令、时间、结果、日志与复现方式。 |
| 下一步 | 最有价值的未完任务、可解决的条件与当前确实缺少的外部输入。 |

在有意义的工作单元、重要决定或交接前更新即可，无需每个工具调用填表。恢复时先核对已有进程、实际文件和远端状态：后台子任务可能仍运行，也可能已经退出；teams 面板或 transcript 不代替产物核验。push/PR 创建回执丢失时查询服务端 HEAD 和既有 PR 后再推进。tmux 输入、消息发送等外部效果按已经发生、未发生或未知处理，进程恢复不会撤销它们。

开发者可自行选择监督器、tmux、队列和恢复工具。目标是从真实状态持续工作，而不是无限重放启动命令。等到设备、账号或实测观察条件就绪后继续原目标；长期验证使用真实墙钟区间与对应候选。重要轮次将候选 SHA、覆盖/失败/等待计数、关键运行结果、PR head 和剩余任务呈现在 transcript，支持 goal evaluator 作有依据的判断。

## 7. 核验范围

本次核验针对官方文档公开支持的机制，未测试朋友机器或证明其账号已获模型、源库与运行环境访问。所有示例中的路径、分支、PR 编号和 session ID 都从真实执行状态取得。当前权威执行入口为 [Claude 启动与恢复](../governance/CLAUDE.md)，完整目标为 [GOAL.md](../../GOAL.md)；本文提供原始官方事实与采用理由。
