# Contributing to Shenko

先读 [AGENTS.md](AGENTS.md)、[.agents/skills/shenko-dev/SKILL.md](.agents/skills/shenko-dev/SKILL.md)与 [领域合同索引](docs/INDEX.md)，再按 [docs/IMPLEMENTATION_DOCS_MAP.md](docs/IMPLEMENTATION_DOCS_MAP.md) 找到你负责的范围。

## 开工前

- 开工在 issue 评论中写清切片、模块/文件边界、base SHA、分支/PR 与下一动作，并 assign 真实账号；认领是协调提示，不是排他锁。
- 关注自己创建或认领的 issue/PR；可用主控共享 monitor，不必自建监控进程。需要回应的评论 3 小时内回复，或明确"正在处理 + 进度 + 下一 checkpoint"。
- 超过 3 小时无回应且无可核查 checkpoint，主控可接管或转派在线积极 agent；优先保留原作者/commit，以 close 来源 PR + 正常 merge from PR 接续，不 squash/reset-author。
- 写实现前读对应 [docs/specs/](docs/specs) 领域合同；改文档前读 [docs/specs/documentation.md](docs/specs/documentation.md)。

## 并行与集成

同时运行多少个实现 agent 由真实机器容量、共享重型工作锁和集成责任划分决定，没有固定的 1 条或 5 条 workflow 上限。给每个写入 worker 独立 worktree 与分支、自包含任务、相关合同和明确的集成归属；集成者验证组合后的实际候选。

## 许可与贡献授权

项目许可已采用 **AGPL-3.0-or-later**，owner `cyjin-yl` 本人保留闭源与商业双许可。详见 [docs/governance/LICENSING.md](docs/governance/LICENSING.md)；贡献者再许可条款见 [CONTRIBUTOR_LICENSE.md](docs/governance/CONTRIBUTOR_LICENSE.md)，来源声明见 [DCO.md](docs/governance/DCO.md)。当前实际许可字段、依赖清单与签署现状见 [LICENSE-INVENTORY.md](docs/governance/LICENSE-INVENTORY.md)（实测记录，不是许可决定）。

## 提交前

- `python3 scripts/check_docs.py` 校验文档与归档完整性（不验证产品行为）。
- `python3 scripts/check_kernel_boundary.py --baseline-ref <已审阅提交>` 对照独立基线检查工作区依赖边界；通过 Cargo 的只读 metadata 查询纳入自动成员，不构建或验证产品行为。证据保留实际基线的完整提交；规则见[内核合同](docs/specs/kernel.md#kernel-7)。
- 覆盖、模拟、源码存在与真实运行是不同事实，不互相替代；在 PR 中如实区分。
- 文档改动同时更新 [docs/INDEX.md](docs/INDEX.md) 导航与对应 `docs/specs/` 合同。
