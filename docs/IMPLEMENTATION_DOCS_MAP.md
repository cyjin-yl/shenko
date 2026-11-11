# 实现范围与必读文档

每个实现范围对应**必读文档页**与**当前源码事实**。状态列只描述仓库里真实存在的接线程度，不描述运行验收——运行证据在 [evidence/](evidence/README.md)。

状态含义：

- **已接线** — 该能力在 `bin/shenko` 的装配路径上有实际调用点。
- **仅源码** — 有实现与类型，但没有进入运行时装配。
- **未实现** — 仓库里没有对应实现。

## 必读基线（所有实现者）

1. [文档索引](INDEX.md) — 按领域进入现行合同和 Why；旧章节只作追踪标识
2. [docs/specs/architecture.md](specs/architecture.md) — 当前功能边界与各层事实归属
3. [docs/governance/COMPLETION.md](governance/COMPLETION.md) — 完整交付合同
4. [docs/governance/AUTONOMY.md](governance/AUTONOMY.md) — 自主决策边界
5. [docs/requirements/requirements.json](requirements/requirements.json)— 165 项 R 完整分母
6. [docs/requirements/acceptance-cases.json](requirements/acceptance-cases.json)— 236 条验收 case
7. [docs/state/coverage.json](state/coverage.json)— 需求级实现与证据状态
8. [.agents/skills/shenko-dev/SKILL.md](../.agents/skills/shenko-dev/SKILL.md) — 开发原子与约束

## 装配根

- **运行时装配（`bin/shenko/src/main.rs`）** — 24 个 workspace crate 中有 20 个在此被装配（`shenko-memory`、`shenko-repl`、`shenko-update`、`shenko-exec-ssh`、`shenko-channel-napcat-link` 除外）。
  - [§44](specs/plugins.md#spec-44)（首次启动与种子发行）
  - docs/specs/architecture.md（装配与事实归属）

## 持久状态与主体

- **Store（SQLite WAL + CAS + ledger）** — 已接线
  - [§41.2](specs/durable-state.md#spec-41-2)、[§36](specs/durable-state.md#spec-36)
  - docs/specs/durable-state.md
  - docs/requirements/requirements.json（R030、R031）

- **协议类型（ID、CommitRef、TaskRecord 等）** — 已接线
  - [§41.3](specs/durable-state.md#spec-41-3)
  - docs/requirements/requirements.json（R029）

- **SubjectLoop（S0/S1 状态机）** — 已接线
  - [§5.1](specs/subject-runtime.md#spec-5-1)、[§41.5](specs/subject-runtime.md#spec-41-5)
  - docs/specs/subject-runtime.md

- **SubjectBranchHead + SubjectGenesis** — 已接线
  - [§41.5](specs/subject-runtime.md#spec-41-5)、[§44.6](specs/plugins.md#spec-44-6)
  - docs/requirements/requirements.json（R051）

- **Node 安装单元（units.rs）** — 已接线（`shenko sync` 导出/导入单位包；`shenko-node` 被 6 个 crate 消费）
  - [§45](specs/cognition-units.md#spec-45)
  - docs/specs/cognition-units.md

- **认知归档与复制（store/cognition.rs、cognition_archive.rs）** — 已接线（`shenko sync export/receive/ack`）
  - [§10.6](specs/durable-state.md#spec-10-6)、[§41](specs/architecture.md#spec-41)
  - docs/specs/durable-state.md

## 推理与资源

- **Broker（provider、admission、request）** — 已接线（`shenko start` 装配 `Broker::new` 与 provider 发现）
  - [§13](specs/inference.md#spec-13)、[§42](specs/quota-resources.md#spec-42)、[§47](specs/inference.md#spec-47)
  - docs/specs/inference.md
  - docs/research/claude-api-2026-10-21.md

- **Anthropic provider** — 已接线（`ANTHROPIC_API_KEY`）
  - [§47](specs/inference.md#spec-47)
  - docs/requirements/requirements.json（R008、R009、R059）

- **本地模型 provider** — 已接线（`SHENKO_USE_LOCAL_MODEL=1` + `SHENKO_LOCAL_MODEL_URL`）
  - [§25](specs/inference.md#spec-25)、[§42](specs/quota-resources.md#spec-42)
  - docs/specs/inference.md

- **额度观测与 admission（shenko-quota）** — 已接线（策略文件经 `SHENKO_QUOTA_POLICY` 读取并持久化；quota crate 被 4 个 crate 消费）
  - [§42](specs/quota-resources.md#spec-42)、[§47.5](specs/inference.md#spec-47-5)
  - docs/specs/quota-resources.md

- **KV 页与物理资源域分配** — 仅源码（`shenko-broker/admission.rs` 有 admission 合同，无运行时容量事实）
  - [§13](specs/inference.md#spec-13)
  - docs/specs/inference.md

## 上下文与压缩

- **Budget compact（shenko-compact-budget）** — 已接线（`BudgetCompactPlugin` 在 `main.rs` 注册）
  - [§12.3](specs/context-policies.md#spec-12-3)、[§35.3](specs/plugins.md#spec-35-3)
  - docs/specs/context-policies.md

- **Snapcompact（shenko-compact-snap）** — 已接线（`SnapCompactPlugin` 在 `main.rs` 注册）
  - [§12.3](specs/context-policies.md#spec-12-3)、[§21.11](specs/web-interface.md#spec-21-11)
  - docs/specs/context-policies.md

- **上下文窗口与策略注册（shenko-context-policy）** — 已接线（`PolicyRegistry::with_ledger`，被 8 个 crate 消费）
  - [§37](specs/context-policies.md#spec-37)、[§41.7](specs/plugins.md#spec-41-7)–[§41.8](specs/context-policies.md#spec-41-8)
  - docs/specs/context-policies.md

## 能力与插件

- **Cordis 内核（shenko-kernel：Plugin/Service 生命周期、capability、hooks、effect contract）** — 已接线（`Supervisor` 是运行时装配根）
  - [§7](specs/plugins.md#spec-7)、[§37](specs/context-policies.md#spec-37)
  - docs/specs/plugins.md

- **Hook API（HookRegistry）** — 已接线（`main.rs` 创建 registry 并注入 subject loop）
  - [§37](specs/context-policies.md#spec-37)
  - docs/specs/plugins.md
  - docs/requirements/requirements.json（R086、R087）

- **Effect Contract** — 仅源码（`shenko-kernel/effect_contract.rs` 有定义，运行时装配无消费点）
  - [§36](specs/durable-state.md#spec-36)
  - docs/specs/plugins.md

- **Awareness 单元（shenko-awareness-core）** — 已接线（`AwarenessRegistry::with_seeds`，安装 `data_dir/awareness` 下的包；被 4 个 crate 消费）
  - [§45](specs/cognition-units.md#spec-45)
  - docs/specs/cognition-units.md

- **自解释（SelfKnowledgeProvider + SelfKnowledgeHandler）** — 已接线
  - [§38](specs/cognition-units.md#spec-38)
  - docs/specs/cognition-units.md
  - docs/requirements/acceptance-cases.json（SELF-01~03）

- **记忆（shenko-memory：MemoryStore、intent 链、search）** — 仅源码（crate 存在于工作区并被 `bin/shenko` 声明为依赖，但运行时无调用点；`spawn_worker` 无生产调用者）
  - [§12](specs/memory.md#spec-12)
  - docs/specs/memory.md

- **研究 / PMA** — 未实现
  - [§14](specs/research-memory.md#spec-14)、[§12.4](specs/memory.md#spec-12-4)
  - docs/specs/research-memory.md

## 执行环境

- **WASM 运行时与家园（shenko-wasm：WasmRuntime、MountSpec、BoundedOutputPipe）** — 已接线（`wasi.exec` 处理器在受管 mount 下执行已安装工具；home manifest 由 seed 准备）
  - [§8](specs/wasm-home.md#spec-8)、[§9](specs/self-update.md#spec-9)、[§10](specs/durable-state.md#spec-10)
  - docs/specs/wasm-home.md

- **WASM 家园 COW（copy_on_write mount）** — 未实现（`MountSpec::validate` 明确以 `MOUNT_MODE_UNSUPPORTED` 拒绝，不降级为直接读写）
  - [§8.3](specs/wasm-home.md#spec-8-3) / A08
  - docs/specs/wasm-home.md

- **具名 WASM 工具（`shenko tool list/install/exec`）** — 已接线
  - [§8.2](specs/wasm-tools.md#spec-8-2)
  - docs/specs/wasm-tools.md

- **REPL（shenko-repl：kernel、journal、state）** — 仅源码（工作区内有 kernel/journal/state 与 R0/R1/R2 合同，运行时装配无调用点）
  - [§19](specs/repl.md#spec-19)、[§19.1](specs/repl.md#spec-19-1)
  - docs/specs/repl.md

- **Computer Use（X11 backend，exec-host）** — 已接线（4 个 handler 注册到同一 backend）
  - [§10.2.1](specs/durable-state.md#spec-10-2-1)、[§24](specs/desktop-runtime.md#spec-24)
  - docs/specs/desktop-runtime.md

- **远程 seat（shenko-exec-ssh：SSH exec、tmux pane、seat 分类）** — 仅源码（工作区内有 `SeatKind::{ClaudeCode,Omp,Codex}` 与 pane 解析，运行时装配无调用点）
  - [§11](specs/remote-seats.md#spec-11)、[§17](specs/remote-seats.md#spec-17)、[§35.5](specs/remote-seats.md#spec-35-5)
  - docs/specs/remote-seats.md

- **自升级交接（shenko-update）** — 仅源码（状态机与一致性备份存在，运行时装配无调用点）
  - [§9](specs/self-update.md#spec-9)、[§35.7](specs/self-update.md#spec-35-7)
  - docs/specs/self-update.md

- **BuildProvider（exec-host/build_provider.rs）** — 仅源码
  - [§35.3](specs/plugins.md#spec-35-3)、[§35.4](specs/self-update.md#spec-35-4)
  - docs/specs/self-update.md

## 渠道与身份

- **NapCat WS 接入与持久 ingress** — 已接线（`NAPCAT_WS_URL` + `NAPCAT_BOT_QQ`；ingress spool 落盘）
  - [§16](specs/channel-adapters.md#spec-16)、[§35.6](specs/channels.md#spec-35-6)
  - docs/specs/channels.md

- **QQ 出站账本（outbound.rs：intent→ack→unknown→reconcile）** — 已接线
  - [§16](specs/channel-adapters.md#spec-16)
  - docs/specs/channels.md

- **NapCat 联系人查询（`NapCatContactsHandler`）** — 已接线（仅在配置了 connector 时安装）
  - [§15.2](specs/contacts.md#spec-15-2)、[§35.6](specs/channels.md#spec-35-6)
  - docs/specs/contacts.md

- **napcat-cli 适配（shenko-channel-napcat-link）** — 仅源码（可选 backend，feature 默认关闭，未被装配）
  - [§16](specs/channel-adapters.md#spec-16)
  - docs/specs/channel-adapters.md

- **官方 QQ / iLinkBot / 飞书适配器** — 未实现
  - [§15.3](specs/channel-adapters.md#spec-15-3)、[§16](specs/channel-adapters.md#spec-16)
  - docs/specs/channel-adapters.md

- **身份与修订（shenko-identity）** — 已接线（`shenko identity`，含 revision CAS 编辑）
  - [§6](specs/subject-runtime.md#spec-6)
  - docs/specs/contacts.md

## 媒体、界面与作品

- **媒体 ingress（shenko-media）** — 已接线（`MediaIngressPlugin` 与 `/api/media` 共用同一 durable revision admission）
  - [§15.2](specs/contacts.md#spec-15-2)、[§20](specs/media.md#spec-20)
  - docs/specs/media.md

- **真实 ASR / TTS / 声纹 provider** — 未实现
  - [§20](specs/media.md#spec-20)
  - docs/specs/media.md

- **Web UI（shenko-ui：时间线、ws、api、playground）** — 已接线
  - [§21](specs/web-interface.md#spec-21)、[§43](specs/configuration-playground.md#spec-43)
  - docs/specs/web-interface.md

- **Playground 后端与 WASI 业务状态** — 已接线（`PlaygroundBackendHandler` 走既有 native Task/Operation/T1/E1/T2 路径）
  - [§43](specs/configuration-playground.md#spec-43)
  - docs/specs/configuration-playground.md
  - docs/development/playground-backend-smoke.md（有限原生 HTTP + 重开 recipe，不是执行回执）

- **真实 provider 流式输出到 UI** — 未实现（UI 现有输出是 S1 结果，不是 provider stream）
  - [§21.12](specs/web-interface.md#spec-21-12)、[§48.4](specs/architecture.md#spec-48-4)
  - docs/specs/web-interface.md

## 多节点

- **Node 同步与 peer 注册（shenko-node/peers.rs、bin node_sync）** — 已接线（`shenko sync status/enroll/export/receive/ack/push/sources/recover-source`）
  - [§22](specs/nodes.md#spec-22)、[§44](specs/plugins.md#spec-44)
  - docs/specs/nodes.md

- **选举、quorum、fencing** — 未实现
  - [§22](specs/nodes.md#spec-22)
  - docs/specs/nodes.md

- **跨节点认知复制（store/cognition.rs）** — 已接线（经 `shenko sync` 导出/接收/确认）
  - [§10.6](specs/durable-state.md#spec-10-6)
  - docs/specs/durable-state.md

## 验证规则

- 任何 Agent 任务的 prompt 必须显式列出"必读文档"清单
- Agent 必须完整读取每份文档，不能跳过或假设"已知"
- 实现后在 docs/state/coverage.json 更新对应 R/B/case 的 implementation_refs 和 evidence
- PR 描述必须列出"已读文档"清单和对应章节号

覆盖率、模拟通过、源码存在与真实运行是不同事实，不互相替代。
