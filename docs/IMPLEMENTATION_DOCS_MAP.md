# 实现范围与必读文档

每个实现范围对应**必读文档页**与**当前源码事实**。状态列只描述仓库里真实存在的接线程度，不描述运行验收——运行证据在 [evidence/](evidence/README.md)。

2026-11-15 按 `054817b0d5729fed56ec5a974d101c31edfd1410` 及本次修复复核装配入口。下面的“已接线”不等于真实账号、模型、硬件或故障旅程已经验收；较早源码观察保留日期，不能当成当前缺失项。

本页“多节点”部分另按 continuation 至 `58b30ba` 的装配源码更新：实际控制日志持久化、Store authority bridge 和真实分支已接线；`60e9963` 的四条整合故障旅程及既有 owner 回归通过，具体命令、输入和边界见 [整合验证记录](evidence/readiness-continuation-20261115.json)。该更新不扩大其他领域原有观察的范围。

状态含义：

- **已接线** — 该能力在 `bin/shenko` 的装配路径上有实际调用点。
- **部分已接线** — 该领域有生产调用路径，但不能代表领域内全部能力已接入。
- **仅源码** — 有实现与类型，但没有进入运行时装配。
- **未实现** — 仓库里没有对应实现。

## 必读基线（所有实现者）

1. [文档索引](INDEX.md) — 按领域进入现行合同和 Why；旧章节只作追踪标识
2. [docs/specs/kernel.md](specs/kernel.md) — 内核合同：九条不变量、每条的 Why 与当前实现状态
3. [docs/specs/architecture.md](specs/architecture.md) — 当前功能边界与各层事实归属
4. [docs/governance/COMPLETION.md](governance/COMPLETION.md) — 完整交付合同
5. [docs/governance/AUTONOMY.md](governance/AUTONOMY.md) — 自主决策边界
6. [docs/requirements/requirements.json](requirements/requirements.json)— 165 项 R 完整分母
7. [docs/requirements/acceptance-cases.json](requirements/acceptance-cases.json)— 236 条验收 case
8. [docs/state/coverage.json](state/coverage.json)— 需求级实现与证据状态
9. [.agents/skills/shenko-dev/SKILL.md](../.agents/skills/shenko-dev/SKILL.md) — 开发原子与约束

## 装配根

- **运行时装配（`bin/shenko/src/lib.rs`）** — CLI 的 `main.rs` 调用共享库 `run_cli`；共享库装配主体、原生处理器、记忆 worker、REPL、SSH seat、升级交接与按配置安装的渠道。工作区成员以根 `Cargo.toml` 为准；声明依赖与实际装配分别核对。
  - [§44](specs/plugins.md#spec-44)（首次启动与种子发行）
  - docs/specs/architecture.md（装配与事实归属）

## 持久状态与主体

- **外部 Harness 历史研究（`history.obelisk`）** — 已接线（可选 Node 安装单元；`bin/shenko/src/obelisk.rs` 在主体和 capability discovery 两条装配路径观察启停）
  - [外部历史合同](specs/external-history.md)与 [TODO / 验证记录](development/obelisk-history.md)
  - `shenko-exec-host::external_history` 保存完整原件、来源版本、字节引用与有界全文投影；`obelisk_runtime` 拥有独立 CLI 查询进程。
  - 现有 Store raw session/CAS 及 cognition archive 复制证据；不替换 `history.query`，不新增记忆真相源。此状态不代表真实模型或长期运行验收已通过。

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

- **Node 安装单元（units.rs）** — 已接线（`shenko sync` 导出/导入单位包）
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

- **Budget compact（shenko-compact-budget）** — 已接线（`BudgetCompactPlugin` 在共享运行库注册）
  - [§12.3](specs/context-policies.md#spec-12-3)、[§35.3](specs/plugins.md#spec-35-3)
  - docs/specs/context-policies.md

- **Snapcompact（shenko-compact-snap）** — 已接线（`SnapCompactPlugin` 在共享运行库注册）
  - [§12.3](specs/context-policies.md#spec-12-3)、[§21.11](specs/web-interface.md#spec-21-11)
  - docs/specs/context-policies.md

- **上下文窗口与策略注册（shenko-context-policy）** — 已接线（`PolicyRegistry::with_ledger`，被 8 个 crate 消费）
  - [§37](specs/context-policies.md#spec-37)、[§41.7](specs/plugins.md#spec-41-7)–[§41.8](specs/context-policies.md#spec-41-8)
  - docs/specs/context-policies.md

## 能力与插件

- **Poppy 公开发现（shenko-compat-poppy）** — 已接线，但必须显式编译 `compat-poppy` 并启用 `compat.poppy` 才向主体提供 `compat.poppy.discover:1`。装配根 `bin/shenko/src/poppy.rs` 复用安装观察、native handler 和持久回执；协议 crate 不依赖 Store 或 Subject。
  - [Poppy：发现与签名基础](specs/ecosystem-adapters.md#poppy-foundation)
  - `shenko compat poppy discover <https-origin>` 是独立的公开诊断命令，不创建主体任务。
  - 委托 ES256 Session 签名和公钥 metadata/JWKS 生成**仅源码**；商家请求尚不携带这些签名。平台托管、持久凭据、OAuth/DPoP、浏览器 join、UCP 协商、购物与订单接续**未实现**。跟踪 [Issue #297](https://github.com/cyjin-yl/shenko/issues/297)。

- **Cordis 内核（shenko-kernel：Plugin/Service 生命周期、capability、hooks、effect contract）** — 已接线（`Supervisor` 是运行时装配根）
  - [§7](specs/plugins.md#spec-7)、[§37](specs/context-policies.md#spec-37)
  - docs/specs/plugins.md

- **Hook API（HookRegistry）** — 已接线（`Supervisor` 持有 registry，共享运行库将同一实例注入 subject loop）
  - [§37](specs/context-policies.md#spec-37)
  - docs/specs/plugins.md
  - 2026-11-11 读 `dd28003` 的补充（在 `db3970e` 上复核）：主体每步在四处派发钩子，但生产装配没有登记任何处理器；`context.collect` 钩子点只有定义，主体没有派发它。见[内核收敛计划](development/kernel-convergence.md)。
  - docs/requirements/requirements.json（R086、R087）

- **Effect Contract** — 已接线（`NativeTaskHandlerPlugin` 通过 `register_native_definition` 将处理器的 `effect_contracts()` 发布到运行时 provider/capability 目录；卸载时撤回登记。各处理器的执行与恢复语义仍须单独核对）
  - [§36](specs/durable-state.md#spec-36)
  - docs/specs/plugins.md

- **Awareness 单元（shenko-awareness-core）** — 已接线（`AwarenessRegistry::with_seeds`，安装 `data_dir/awareness` 下的包；被 4 个 crate 消费）
  - [§45](specs/cognition-units.md#spec-45)
  - docs/specs/cognition-units.md

- **自解释（SelfKnowledgeProvider + SelfKnowledgeHandler）** — 已接线
  - [§38](specs/cognition-units.md#spec-38)
  - docs/specs/cognition-units.md
  - docs/requirements/acceptance-cases.json（SELF-01~03）

- **记忆（shenko-memory：MemoryStore、intent 链、search）** — 已接线（`MemoryHandler` 与 `MemoryWorkerPlugin` 安装在共享运行库；worker 使用 `GitKnowledgeStore` 消费已提交 intent，接受意图与完成记忆分别记账）
  - [§12](specs/memory.md#spec-12)
  - docs/specs/memory.md

- **研究 / PMA** — 部分已接线（`research_handlers` 安装研究 epoch、candidate、bank 与 retention 原生操作；`MemoryInterventionPlugin` 装配模型驱动的记忆干预。完整研究效果与真实模型成效仍看各自证据）
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

- **REPL（shenko-repl：kernel、journal、state）** — 已接线（`ReplSessionHandler` 与 `ReplCellHandler` 共用会话 owner；会话参加升级交接。具体语言、WASM 兼容性与 R0/R1/R2 恢复证据分别核对）
  - [§19](specs/repl.md#spec-19)、[§19.1](specs/repl.md#spec-19-1)
  - docs/specs/repl.md

- **Computer Use（X11 backend，exec-host）** — 已接线（会话、观察、lease、输入等处理器共用 backend；真实桌面与输入设备可用性是独立条件）
  - [§10.2.1](specs/durable-state.md#spec-10-2-1)、[§24](specs/desktop-runtime.md#spec-24)
  - docs/specs/desktop-runtime.md

- **远程 seat（shenko-exec-ssh：SSH exec、PTY、seat）** — 已接线（SSH、OMP/Claude seat、观察、回答与 fleet 处理器共用 `SeatRuntime`，启动时尝试重新绑定已有持久 seat；真实远端工具与完整问答/编码旅程单独验收）
  - [§11](specs/remote-seats.md#spec-11)、[§17](specs/remote-seats.md#spec-17)、[§35.5](specs/remote-seats.md#spec-35-5)
  - docs/specs/remote-seats.md

- **自升级交接（shenko-update）** — 已接线（`UpdateControlHandler`、`update_control` 与 `update_runtime` 连接候选准备、安装控制通道、原生 worker/REPL/Playground 的 quiesce，以及有限 helper 的交接恢复）
  - [§9](specs/self-update.md#spec-9)、[§35.7](specs/self-update.md#spec-35-7)
  - docs/specs/self-update.md

- **候选构建 provider** — 已接线（`bin/shenko/src/update_control.rs` 使用 `shenko-update::build::CargoBuildProvider`；`exec-host/build_provider.rs` 的独立类型不能代替这一实际调用路径）
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

- **napcat-cli 事件源** — 已接线（`NAPCAT_CLI_URL` 配置 `NapCatCliSource`，作为同一 QQ 渠道的附加 ingress 来源，共用持久去重与主体确认；独立 `shenko-channel-napcat-link` crate 的状态不能代表此生产路径）
  - [§16](specs/channel-adapters.md#spec-16)
  - docs/specs/channel-adapters.md

- **官方 QQ / iLinkBot / 飞书适配器** — 已接线（`SHENKO_CHANNELS_CONFIG` 经 `shenko-channel-adapters` 安装所配置平台，记录健康事实、持久 ingress、主体 disposition 与出站对账；未配置的平台不被宣称为可用）
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

- **OXR 文档解析（`document.parse/v1`）** — 已配置时接线（`bin/shenko/src/document_runtime.rs` 在绑定实际 endpoint、model、account 与凭据后注册 native handler；使用现有 Task/Operation/Attempt/CAS/E1 记录一次原始上传、异步任务句柄、原始等待期限与页结果，恢复只对账原尝试。未配置时不注册；本地协议与恢复测试不等于真实 OXR 引擎验收）
  - [§20](specs/media.md#spec-20)
  - docs/evidence/document-parse-recovery-20261115.md

- **Web UI（shenko-ui：时间线、ws、api、playground）** — 已接线
  - [§21](specs/web-interface.md#spec-21)、[§43](specs/configuration-playground.md#spec-43)
  - docs/specs/web-interface.md

- **Playground 后端与 WASI 业务状态** — 已接线（`PlaygroundBackendHandler` 走既有 native Task/Operation/T1/E1/T2 路径）
  - [§43](specs/configuration-playground.md#spec-43)
  - docs/specs/configuration-playground.md
  - docs/development/playground-backend-smoke.md（有限原生 HTTP + 重开 recipe，不是执行回执）

- **provider 流式输出到 UI** — 已接线（`SubjectStreamJournal` 与 `UiPublicStreamSink` 注入主体的 `with_stream_sinks`，UI 使用同一 `StreamSink`；持续流式体验、重连和真实 provider 行为仍需对应运行证据）
  - [§21.12](specs/web-interface.md#spec-21-12)、[§48.4](specs/architecture.md#spec-48-4)
  - docs/specs/web-interface.md

## 多节点

- **Node 同步与 peer 注册（shenko-node/peers.rs、bin node_sync）** — 已接线（`shenko sync status/enroll/export/receive/ack/push/sources/recover-source`）
  - [§22](specs/nodes.md#spec-22)、[§44](specs/plugins.md#spec-44)
  - docs/specs/nodes.md

- **控制日志、原始计划 quorum 与 fencing** — 已接线（`shenko-node/control.rs`、`control_transport.rs` 通过持久化 prepare/accept/ACK/commit 保留真实控制值、最高已接受原值和完整 CAS 闭包；Joint/Final 检查 old/new quorum，正常传输使用有界后缀与对象块。`node_sync::establish_primary` 获取新 quorum，缓存主节点证明不能直接跳过确认。此处选择的是原始计划，不是跨 ledger 的确定性 Store 结果）
  - [§5.4](specs/nodes.md#spec-5-4)、[§22](specs/nodes.md#spec-22)
  - docs/specs/nodes.md
  - docs/evidence/control-quorum-20261115.md

- **Node–Store 提交边界与运行时重建** — 已接线（`authority_guard.rs`、Store admission 与 `runtime_authority.rs` 在网络 admission 后持有 Node reservation，核验精确 grant/read fence，直到 Store 自身 head/task/epoch/cancellation CAS 提交；本地 materialization 标记仅在成功提交后成立。运行时接到权限恢复信号会清理并重建消费者；无法确认 materialization 的原计划保留 CAS 证据和 reconciliation debt，不重新取得物理发送许可。整合 reservation/cancellation/crash-rebuild 旅程已通过，本地锁不等于网络租约）
  - docs/specs/nodes.md
  - docs/evidence/control-quorum-20261115.md

- **真实 availability branch 与输入继承** — 已接线（`subject_branches.rs`、`runtime_branches.rs` 保留父 head、冻结历史和未提交输入选择，创建真实新 branch/root task/first step，再由本地 Store receipt 投影 Node identity；原 request/attempt 不挪动。opening authority 绑定 holder、term、configuration 和 fence，conditional open 同时比较完整 branch 事实，Store fork 前再次验证，运行时拒绝已过期 origin。历史 receipt 修复不伪造当前权限）
  - docs/specs/nodes.md
  - docs/evidence/branch-continuity-20261115.md

- **选举失败后的分区接管** — 部分已接线（自动隔离 fallback 仅适用于本轮选举恢复没有收到任何其他 configured voter 的 authenticated route response，且失败的 authority snapshot 仍有效。连通拒票、不可用协议响应或少数组件返回 `PrimaryNotSelected` 并停止本地 Start；有限退让和一次 catch-up 不是常驻 follower。连通少数组件唯一统筹者与常驻 follower 生命周期尚未实现，完整合同不变）
  - [§5.4.1](specs/nodes.md#spec-5-4-1)
  - docs/specs/nodes.md

- **跨 ledger 的 canonical S1 状态机复制** — 未实现（quorum 已保存的原始计划与本地 Store 实际 materialization 分开记录；仍需固定 read/write/result 身份的 typed transition，以及独立于各 ledger 物理 receipt 的逻辑决定身份）
  - docs/specs/nodes.md
  - docs/evidence/control-quorum-20261115.md

- **跨节点认知复制（store/cognition.rs）** — 已接线（经 `shenko sync` 导出/接收/确认）
  - [§10.6](specs/durable-state.md#spec-10-6)
  - docs/specs/durable-state.md

## 内核九条与代码

[内核合同](specs/kernel.md)的九条在源码里的位置与状态。这里只记录阅读 `dd28003` 源码看到的事实（所引用的符号在 `db3970e` 上复核过），不是运行验收；差距怎样收敛见[内核收敛计划](development/kernel-convergence.md)。

| 内核条目 | 主要位置 | 状态 |
| --- | --- | --- |
| [内核 1](specs/kernel.md#kernel-1) 一条决策线 | `shenko-store`：`commit_step_and_advance_head`、`runtime_boundary.rs` | 机制已有 |
| [内核 2](specs/kernel.md#kernel-2) 循环不停 | `shenko-subject`：`run_subject_loop`、`attention_budget`；`shenko-types`：`MAX_WAIT_SECS` | 机制已有 |
| [内核 3](specs/kernel.md#kernel-3) 经历不改写 | `shenko-store`：`raw_session.rs`、内容存储；`shenko-node`：单元版本历史与移除事实 | 机制已有 |
| [内核 4](specs/kernel.md#kernel-4) 先封存，再行动 | `shenko-subject`：`FrozenStepInput`；`shenko-store`：`subject_request.rs` | 机制已有 |
| [内核 5](specs/kernel.md#kernel-5) 效果有账 | `shenko-kernel`：`NativeTaskHandler` 的 `execute`、`reconcile`、`effect_contracts`（登记与派发在 `shenko-subject`）；`shenko-broker`；渠道出站台账 | 机制已有 |
| [内核 6](specs/kernel.md#kernel-6) 权限来自事实 | 能力声明与认知材料的合同、注释 | 原则已写，入口尚未逐一核对 |
| [内核 7](specs/kernel.md#kernel-7) 一个门，两个架子 | `shenko-kernel`：`PluginRegistry`、`PluginRuntime`、`HookRegistry` | 部分：钩子架没有处理器；主体直接读认知目录；贡献注册表与钩子点并存；[依赖边界](state/kernel-boundary.json)有 26 条违例 |
| [内核 8](specs/kernel.md#kernel-8) 一个安装权威 | `shenko-node`：`units.rs`；`shenko-update` | 部分：原生处理器的启停不由单元状态决定；种子里没有元感知与元 awareness 单元 |
| [内核 9](specs/kernel.md#kernel-9) 内核不做判断 | `shenko-awareness-core` 的种子单元；`shenko-subject` 的 `subject_frame`、`idle_perception` | 部分：编译回退文本仍在（`SUBJECT_CONTRACT`、`IDLE_CONTINUATION_PROMPT`、`ROLE_CONTRACT`） |

## 验证规则

- 任何 Agent 任务的 prompt 必须显式列出"必读文档"清单
- Agent 必须完整读取每份文档，不能跳过或假设"已知"
- 实现后在 docs/state/coverage.json 更新对应 R/B/case 的 implementation_refs 和 evidence
- PR 描述必须列出"已读文档"清单和对应章节号

覆盖率、模拟通过、源码存在与真实运行是不同事实，不互相替代。
