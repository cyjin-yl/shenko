# 在朋友的 fork 中开发并交付 PR

## 自己的账户

朋友的 Agent 使用朋友本人或其管理的 Agent 账号。先检查当前 `gh --version`、`gh auth status`，已有可用登录直接复用。缺 CLI 时按 [官方安装说明](https://github.com/cli/cli#installation) 为实际操作系统安装。

```bash
gh auth login --hostname github.com --git-protocol https --web
gh auth setup-git --hostname github.com
gh api user --jq '.login'
```

Agent 准备并推进官方登录流程。已有账号可以直接用；需要注册时按 [GitHub 注册说明](https://docs.github.com/en/account-and-profile/how-tos/account-management/creating-an-account-on-github) 创建，由账户持有人完成平台要求本人执行的注册、邮箱或设备验证。登录凭据通过 CLI/系统凭据存储保存，commit 身份使用实际账户配置。这是一次性接入，不是日常开发的审批流程。

## Fork 后开始

本次 upstream 为 [cyjin-yl/shenko](https://github.com/cyjin-yl/shenko)，公开仓库，默认分支 `main`。读取仓库并 fork 到自己的账号：

```bash
gh repo view cyjin-yl/shenko --json nameWithOwner,isPrivate,defaultBranchRef
gh repo fork cyjin-yl/shenko --clone
cd shenko
git remote -v
git status --short --branch
```

核对 `origin` 指向自己的 fork，`upstream` 指向原仓库；CLI 已配置的正确 remote 直接使用。若本机已有 clone/fork，则读实际状态后续用。GitHub 的 [fork 规则](https://docs.github.com/en/pull-requests/reference/forks)决定仓库是否可读/可fork；私有仓库需要相应读取权限和允许fork的设置。接入问题只影响这一步，文档不会另外要求拥有上游 push 权限。

**自己的 fork 是完整开发空间。** Agent 自主开分支、合并、重构、调整 CI、组织大量 worktree/subagents、选择发布候选和验证方法。分支名与内部 PR 组织由它决定，原仓库通过外部 PR 接收成果。

## 持续交付

产生有意义的实现后，在自己的 fork push，并向 upstream 开 draft PR。下面的值使用查到的实际 default branch、login 和交付分支：

```bash
git push --set-upstream origin '<交付分支>'
gh pr create --repo cyjin-yl/shenko \
  --base '<upstream默认分支>' --head '<朋友login>:<交付分支>' \
  --draft --title 'Implement Shenko' --body-file '<正文文件>'
```

默认可以维护一条集成 PR；更适合分批交付时自行组织 focused/stacked PR。正文说明当前行为、关联要求、真实验证、剩余工作和重要决定的 Why。后续持续 push 并更新：

```bash
git push origin '<交付分支>'
gh pr edit '<PR编号>' --repo cyjin-yl/shenko --body-file '<最新正文文件>'
gh pr checks '<PR编号>' --repo cyjin-yl/shenko --json name,state,bucket,link
```

PR 等待 review 时，继续在 fork 实现和集成其余目标。已有反馈由 Agent 自主判断和处理。完整验收达到后自行标 ready，交付后的 review/merge 与本次完整开发分开。

GitHub回执丢失时查实际 branch/PR/head再继续。CI运行和工件结果也按实际状态记录；fork上的自主验证、独立环境与其他开发工作可以继续推进。

## 命令来源

[gh auth login](https://cli.github.com/manual/gh_auth_login)、[setup-git](https://cli.github.com/manual/gh_auth_setup-git)、[repo fork](https://cli.github.com/manual/gh_repo_fork)、[pr create](https://cli.github.com/manual/gh_pr_create)、[pr edit](https://cli.github.com/manual/gh_pr_edit)、[pr checks](https://cli.github.com/manual/gh_pr_checks)、[pr ready](https://cli.github.com/manual/gh_pr_ready)。核验截点 2026-10-22；使用时以实际版本帮助为准。
