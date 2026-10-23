# 持续开发与恢复

完整任务包含 [GOAL.md](../../GOAL.md) 与 [完成合同](COMPLETION.md) 的全部正式 S1–S5。朋友在自己的 fork 中自主开发、合并分支、重构、装工具、配置可管理的权限与 CI，并持续更新 upstream PR。授权和产品目标随着 session、模型和 worker 更换继续存在；review 等待不暂停开发。

## 产品目标与 native goal

`/goal` 在每轮结束后判断条件并决定下一轮；项目的进展则由需求覆盖、产物、验证和运行检查点承载。每个 session 只有一个 active goal，条件最多 4,000 字符，新的条件替换旧条件。evaluator **只评估 transcript 中出现的证据**，不会自行读 GOAL、打开仓库或执行测试。执行者实际读取规范、运行检查，并在重要轮次呈现候选、覆盖、关键结果、PR 与剩余任务。[官方 Goals](https://code.claude.com/docs/en/goal)

持久记录分别保存 `product_status` 与 `harness_status`。下面的状态名是项目对实际运行的描述，不假设 Claude API 提供同名字段：

| 实际发生的情况 | native goal 的行为 | 项目如何继续 |
| --- | --- | --- |
| evaluator 判 `met` | goal 清除并记录 achieved。 | 按完整验收证据确认产品状态；若发现缺口，继续补齐。 |
| evaluator 判 `impossible` | goal 清除并记录失败原因。 | 核对真实障碍，研究其他方法或修复条件，继续同一完整范围。 |
| 自管凭据认证失败、信用余额耗尽、无法 compact 的溢出或模型不可用 | 这些错误可能清除 goal。 | 保存进度，恢复可用条件后重设原目标。 |
| 临时 API rate limit、Claude usage limit | goal 保留；支持的交互运行进入 paused。 | 记录恢复条件与可观察 reset，推进可独立完成的工作。 |
| 暂时断网、服务过载 | goal 保留；支持的交互版本自动重试，达到次数后暂停。 | 根据真实进度重连或接续。 |
| 连续数轮没有工具进展 | 可能暂停并保留 goal，等待下一 prompt。 | 排查停滞，提交可执行的续接任务。 |

交互重试/暂停提示从 Claude Code 2.1.269 起支持，headless 的 idle check-in 行为不同；是否自动续行依据当前实际运行。resume 可恢复仍 active 的 goal，已清除或已 achieved 的不自动恢复；原生 turn count、timer 和 token-spend baseline 会重置。项目累计进度和长期运行时间保存在自己的记录中。[官方 failures、check-ins 与 resume](https://code.claude.com/docs/en/goal)

## 恢复执行模式与会话

U51 采用官方实际 `--dangerously-skip-permissions`。以下命令用于原进程已结束、从 transcript 重建的会话。普通 terminal 恢复不会自动保留旧 bypass 模式，`-p` 恢复通常按新运行的模式启动，因此重建时显式传入：

```bash
claude --resume '<真实-session-id>' --model claude-opus-5-5 \
  --effort ultracode --dangerously-skip-permissions
```

headless 继续：

```bash
claude --resume '<真实-session-id>' --model claude-opus-5-5 \
  --effort ultracode --dangerously-skip-permissions \
  -p '读取 GOAL.md 与最新检查点，核对实际 fork、PR、任务和证据后继续全部 S1–S5；确认 native goal 当前状态，按原因恢复同一目标。' \
  --output-format stream-json --verbose
```

使用精确 session ID；多会话环境中的“最近会话”不一定是目标会话。若原会话已经丢失，可在新的 session 读取持久检查点与真实工作状态，重新设置同一完整 goal。原启动依赖的 `--settings`、`--mcp-config`、`--plugin-dir`、`--fallback-model`、`--add-dir` 等按实际需要重传。[官方 Sessions](https://code.claude.com/docs/en/sessions#permission-mode-on-resume)

仍运行的官方 background session 保留其当前模式。v2.1.285 起，terminal 中的普通 `claude --resume '<session-id>'` 通常 attach 原进程；更早版本会提示显式 attach 或先停止。带 `--model`、`--settings`、`--permission-mode`、`--output-format` 等参数，或使用管道/重定向时，当前版本会不发送 prompt、以状态 1 退出，并不会重新配置原进程。上面的完整参数示例因此只用于已结束的会话；接回正在执行的会话用 `claude attach '<id>'`，需要改参数重建时先 `claude stop '<id>'`，再恢复。terminal 中仅加 `-p` 也可能走 attach，并不保证非交互。[官方 running background resume](https://code.claude.com/docs/en/sessions#resume-a-running-background-session)

后台进程、子 Agent 与外部效果分别核对。正常运行保持 session persistence，项目规则与能力按正常模式加载；详细首次启动和版本说明见 [CLAUDE.md](CLAUDE.md)。

## Checkpoint 内容

在有意义的工作成果、重大决定、上下文接近结束、worker handoff 或实际错误时更新。普通工具调用无需逐个写完整表格。

- `goal_id`、GOAL/SPEC revision、全部正式范围与当前需求/验收覆盖。
- 真实 fork/upstream、worktree、交付分支、HEAD/dirty work、PR URL/base/head。
- `claude_session_id`、所在机器与 transcript 位置、CLI 版本、实际模型/effort、恢复所需启动参数和非秘密配置引用。
- 已证明事实、当前产物、重要 Why、采用的上游 pin、未完事项及下一可执行工作。
- worker/worktree ownership、运行进程/任务、共享模型/seat/桌面等资源、未决外部效果和可查询标识。
- 验证命令、环境、被测候选、真实开始/结束时间、结果与证据路径；当前外部等待的受影响 ID 和恢复条件。

shared 状态由协调者整合，worker 在独立路径返回产物与证据。可自主替换成更适合的数据库或日志；新的执行者仍能找到入口并复原当前事实。凭据由相应凭据设施管理，检查点记录可定位的配置引用。

## 从真实状态继续

恢复时读取当前 Git 与 runtime，确认旧执行者是否仍在运行，再接续任务 ownership。查询远端 HEAD、既有 PR、实际进程和资源占用后补做缺失操作。回执丢失保持 unknown，直到证据能够说明结果；进程重启和会话恢复不会撤销已发送的消息、tmux 输入或其他外部效果。

开发者可自行配置监督器、tmux、任务队列或其他接续工具，恢复暂时网络/服务故障并重新派发失去执行者的工作。认证、信用余额和临时用量窗口按各自真实状态处理。项目范围保持完整；最新用户对目标的修改或取消即时更新持久记录。

外部等待只影响依赖它的任务。长期观察记录真实墙钟区间、候选和环境，等待期间可以继续独立开发。代码或运行闭包变化时，按影响判断已有证据是否仍适用，并记录分段或重新验证的理由。

## 并行协作与报告

按依赖关系调配 ultracode 与子 Agent，给写入 worker 独立 worktree、branch 和明确的集成归属。开发者自主在 fork 中整合各分支，并验证组合后的实际候选。worktree 隔离源码；模型、SSH seat、桌面、数据库、端口和外部账户仍按真实共享容量协调。

ultracode session 豁免 `CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS`；workflow agents、teams 和普通 Agent tool 有不同限制。teams 的 in-process teammates 不随 resume 自动重建，恢复时依据实际产物重新派发剩余工作。[官方 Subagents](https://code.claude.com/docs/en/sub-agents#concurrent-subagent-limit)、[Agent teams](https://code.claude.com/docs/en/agent-teams#limitations)

报告已经完成的行为、关键证据、剩余工作和真正缺少的外部输入。进程退出、goal 清除、配额等待与产品完整交付是不同事实。只要仍有可执行工作就继续推进；全部剩余工作确实在等待外部条件时，明确最小所需因素与自动恢复条件，保留未完成状态。
