# Shenko 开发交接

给朋友的 Agent 使用。完整委托在 [GOAL.md](GOAL.md)，工程入口在 [AGENTS.md](AGENTS.md)。目标 upstream 为 [cyjin-yl/shenko](https://github.com/cyjin-yl/shenko)，由拥有者创建，公开仓库，默认分支 `main`；实际接续状态见 [goal.json](docs/state/goal.json)。

所有接手者先完整读 [shenko-dev](.agents/skills/shenko-dev/SKILL.md)，包括写入子 Agent；八个开发原子按需调用。根 `CLAUDE.md` 是 `AGENTS.md` 的符号链接。自主开发不设人工 gate，验证按实际问题收敛，小改不堆测试。

## 当前开发状态（2026-10-23）

**开发已开始。** 接手者先读本节再续接，不要重新初始化。

### 已完成
- Cargo workspace 骨架（edition=2024，16 crates）
- `shenko-types`：完整协议类型（SPEC §41.3）— ids, commit, object, error, task, operation, evidence, store, agent, subject
- `docs/state/goal.json` 和 `docs/state/coverage.json` 确认产品尚未实现，规格完整
- 工具链：rustc nightly，wasm32-wasip2 + wasm32-unknown-unknown targets，wasm-tools，wasmtime-cli 45
- 文档完整性检查通过（147 R / 55 B / 216 case）

### 进行中（Workflow wf_b7e1a935-e44）
并行实现所有 crate：
- shenko-store（SQLite WAL），shenko-kernel，shenko-broker，shenko-subject
- shenko-memory，shenko-channel-napcat，shenko-exec-host/ssh，shenko-wasm
- shenko-compact-budget（独立于 snapcompact），shenko-compact-snap（独立于 budget）
- shenko-awareness-core，shenko-ui（axum），shenko-seed，bin/shenko
- 编译错误修复，初始提交，PR 创建

### 待做（workflow 完成后）
1. cargo check 清零 — 修复所有 error
2. S1 验证：SQLite WAL round-trip，subject loop liveness，store CAS
3. NapCat WebSocket 真实连接（S2 关键路径，SPEC §16）
4. Claude API binding（§47 wire contract PV01–PV08）
5. tmux seat 控制（§17/§35 自开发闭环）
6. Proto-UI 集成（§21，shadcn + compiler seam）
7. 两个独立 compact 插件真实实现
8. 持续主体语义等待验证（max 600s per §5.1/U55）

### 恢复命令
```bash
cd /tank/shared/projects/shenko/shenko
git status
# 切换到实现分支（workflow 完成后创建）
git branch -a | grep impl
claude --model claude-opus-5-5 --effort ultracode --dangerously-skip-permissions
```

---

## 启动

在开发者机器的工作目录运行当前账户可用的 Claude Code：

```bash
claude --model claude-opus-5-5 --effort ultracode --dangerously-skip-permissions
```

## 一个目标，从身份准备到完整交付

```text
/goal 完整开发并交付 Shenko，接收 PR 的 upstream 为 cyjin-yl/shenko。先检查本机 gh 与你自己的 GitHub 身份，按官方方式安装/登录；已有登录就复用，没有账号就准备注册并只把平台要求的本人步骤交给账户持有人。fork upstream，clone 自己的 fork，核对 origin/upstream；已有工作就直接续接。在仓库中读取 AGENTS.md、WHY.md、SPEC.md、GOAL.md 和 docs/governance/COMPLETION.md，建立完整依赖图、需求覆盖与证据记录。你在自己的 fork 内自主开发、合并、重构、装工具、配置 CI、调配并行 subagents/ultracode、研究、实现、调试和验证，持续向 upstream 提交并更新 PR，等待人类 review 时照常推进。完整实现 S1–S5、所有 R/B/正式义务与真实验收；首版自开发闭环后继续全部能力。每个重要轮次展示候选 SHA、覆盖计数、关键证据、PR head 与下一任务；压缩、重启或换 worker 后依持久记录恢复同一目标。只有独立完整性审计、真实集成与长期验收均满足，所有交付提交已推送、PR ready 且没有实质缺口时完成。外部等待如实记录，继续其余可做工作。
```

## 最后的交付

交付完整实现和有效证据对应的 ready PR，给出构建/安装/运行方法、全部范围的实现与验收索引、实际长期观察结果、已知限制及上游 head。PR 在开发过程中尽早建立并持续更新；人类随后 review 与合入。完整目标的具体范围、证据有效性和长期观察条件以 COMPLETION 为准。
