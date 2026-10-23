# Claude 启动与恢复

根目录 `CLAUDE.md` 是指向 `AGENTS.md` 的符号链接，两者共用必读入口。进入仓库先完整读 [shenko-dev](../../.agents/skills/shenko-dev/SKILL.md)；开发原子 Skills 按需使用。`.claude/skills` 的每个目录链接到 `.agents/skills` 的同一规范源，保留实际 Git symlink，不能用两份独立正文替代。

用户选择 Opus 5.5、ultracode、大量并行子 Agent 与 bypass 模式，在朋友自己的 fork 中自主完成整个产品。Claude Code 官方实际参数是 `--dangerously-skip-permissions`，等价于 `--permission-mode bypassPermissions`。

## 启动

在自己的 Shenko fork 根目录：

```bash
claude --model claude-opus-5-5 --effort ultracode --dangerously-skip-permissions
```

进入后粘贴 [GOAL.md](../../GOAL.md) 中的完整短入口。一次 headless 启动也支持：

```bash
claude --model claude-opus-5-5 --effort ultracode --dangerously-skip-permissions \
  -p '/goal 按本仓库 GOAL.md、AGENTS.md 和 WHY.md，在自己的 fork 自主并行开发完整 SPEC 的全部正式范围，持续更新 upstream PR；以 COMPLETION 合同要求的真实验收和完整证据结束，每个重要轮次展示候选、覆盖、验证和 PR 状态。' \
  --output-format stream-json --verbose
```

接手者自行检查 `claude --version`、`claude auth status --text` 与实际模型。没有登录时使用 `claude auth login` 的官方流程。研究截点下 Opus 5.5 要求 Claude Code ≥2.1.280，最新 ultracode 会话 toggle 语义要求 ≥2.1.284；以实际安装帮助和官方更新为准。

## 恢复

以下命令用于原进程已结束、从 transcript 重建的会话。用记录下来的真实 session ID：

```bash
claude --resume '<session-id>' --model claude-opus-5-5 \
  --effort ultracode --dangerously-skip-permissions
```

headless恢复：

```bash
claude --resume '<session-id>' --model claude-opus-5-5 \
  --effort ultracode --dangerously-skip-permissions \
  -p '读取 GOAL.md 与持久检查点，核对实际 fork、PR、任务和证据后继续完整开发；确认原 goal 当前状态，必要时恢复同一目标。' \
  --output-format stream-json --verbose
```

普通 terminal resume不会自动恢复旧bypass模式；`-p`恢复的模式也取决于本次启动参数，因此上述入口均显式带flag。仍活跃的goal可以恢复；已经清除的goal需要依据原因处理并重设。session、worker、进程和外部效果分别对账。

仍在运行的官方 background session 使用 `claude attach '<id>'` 接回，保留原进程及其模式。当前版本对运行中的 session 执行 resume 时，若带 `--model`、`--permission-mode`、`--settings`、`--output-format` 等配置/输出参数或管道重定向，会以状态 1 退出；参数不会重配原进程。需要改参数重建时先停止目标 session，再按上面的完整参数恢复。具体版本与行为见 [CONTINUATION.md](CONTINUATION.md)。

## 三个实际机制

- `/goal` 是原生命令，条件最多4,000字符。它的evaluator只看会话中出现的证据，因此要实际读文件、执行检查并呈现结果。
- `--effort ultracode` 开启工作流并设xhigh；当前会话内 `/effort ultracode` 的toggle保留当前effort。它不是模型名，也不是全局并发上限；ultracode/teams与普通subagent限制分别按实际机制管理。
- 鉴权失败、信用余额耗尽、无法compact的上下文溢出或模型不可用可能清除goal；临时rate/usage limit通常保留并暂停。按实际状态继续，而不是把所有“额度问题”当同一种终止。

[CLI reference](https://code.claude.com/docs/en/cli-reference)、[model config](https://code.claude.com/docs/en/model-config)、[goal](https://code.claude.com/docs/en/goal)、[sessions](https://code.claude.com/docs/en/sessions)、[subagents](https://code.claude.com/docs/en/sub-agents)。读取日期：2026-10-22。
