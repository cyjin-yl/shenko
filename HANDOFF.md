# Shenko 开发交接

给朋友的 Agent 使用。完整委托在 [GOAL.md](GOAL.md)，工程入口在 [AGENTS.md](AGENTS.md)。目标 upstream 为 [cyjin-yl/shenko](https://github.com/cyjin-yl/shenko)，由拥有者创建，公开仓库，默认分支 `main`；实际接续状态见 [goal.json](docs/state/goal.json)。

所有接手者先完整读 [shenko-dev](.agents/skills/shenko-dev/SKILL.md)，包括写入子 Agent；八个开发原子按需调用。根 `CLAUDE.md` 是 `AGENTS.md` 的符号链接。自主开发不设人工 gate，验证按实际问题收敛，小改不堆测试。

## 当前开发状态（2026-10-23，S2 进行中）

**S1 已合并 main，S2 关键路径已合并。** 当前在 `impl/s2-self-dev` 分支继续 S2 自开发闭环。

### 已完成并合并到 main
- Cargo workspace（edition=2024，16 crates），`cargo check --workspace` 零 error
- shenko-types：完整 §41.3 协议类型（IDs, CommitRef, TaskRecord X02, OperationRecord X03, EvidenceReceipt X04, AgentProfile, SubjectStep, SubjectBranchHead, SubjectGenesis, StepDecision, TaskWait 等全部 §41.5/§44.6 类型）
- shenko-store：SQLite WAL + FTS5 检测 + CAS（blake3）+ ledger 单 mutation line + 5 张新表（subject_branch_heads, subject_steps, subject_genesis, bootstrap_records, inbox_events）+ initialize_genesis()/upsert_step()/get_existing_agent_id()
- shenko-broker：真实 Anthropic Messages API 2023-06-01（claude-opus-5-5, rustls-tls, usage accounting）+ 4-flow admission + stub fallback
- shenko-subject：真实 S0/S1 状态机（Preparing→Committed）+ 语义等待 600s + tokio::sync::Mutex inbox（Send 安全）
- shenko-channel-napcat：真实 OneBot v11 WebSocket（tokio-tungstenite, 重连, send_private/group_message）
- shenko-ui：完整单主体时间线 WebUI（OKLCH dark/light tokens, system font + PingFang SC, native-feel cursor/selection/hover 约定, 稳定 scroll anchor, aria-live, WCAG-AA, reduced-motion, mobile 44px, WsState broadcast）
- bin/shenko：start 命令完整接线（store → bootstrap_genesis → broker → subject loop → 可选 NapCat → UI）
- S1 store round-trip 测试 4/4 通过

### PR 历史
- PR #1: `impl/s1-core-workspace` → 已合并 main（`a50267f`）
- PR #2: `impl/s2-critical-path` → 已合并 main（`f536d7c`）

### 进行中（impl/s2-self-dev 分支）
按 SPEC §35 首版开发者发行自开发闭环：

1. **§35.4 自开发任务流程** — 需要：SelfDevelopment/PluginManager child, BuildProvider/VerificationProvider
2. **§35.5 三条 Seat 路径** — 需要：tmux 前台 TUI 观察（capture-pane -e 保留高亮）、托管 OMP JSONL RPC、托管 Claude Code SDK
3. **§35.6 NapCat 消息连续性** — 需要：持久 ingress spool, 消息去重 cursor, 断线 gap 记录
4. **§35.7 自升级交接** — 需要：prepare/copy → catch_up → smoke → handoff → observe/retire, A/B 副本
5. **§38 自解释插件** — 需要：SelfKnowledgeProvider, self:// runtime/docs/source/design 资源入口, build manifest 绑定
6. **§36 effect 合同** — 需要：插件操作声明（reset/restore/replay/cancel/compensate 五种区分）, 调用时实际绑定
7. **§47 provider wire contract** — 需要：PV01–PV08 conformance 测试

### 待做优先级
- tmux seat adapter（§35.5 第一条路径：已有 tmux TUI 观察 + 交互）— 这是 V1-01/V1-02 验收的前置
- SelfKnowledgeProvider（§38）— 让主体能解释自身并指导插件开发
- BuildProvider/VerificationProvider（§35.3）— 自开发闭环的构建+验证
- NapCat 消息连续性（§35.6）— ingress spool + cursor
- 自升级交接（§35.7）— A/B 副本 + 事实对账

### 恢复命令
```bash
cd /tank/shared/projects/shenko/shenko
git checkout main && git pull origin main
# 当前工作分支
git checkout impl/s2-self-dev  # 或创建新分支
cargo check --workspace
cargo test -p shenko-store  # S1 验证
# 运行
ANTHROPIC_API_KEY=sk-... cargo run --bin shenko -- start --port 7860
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
