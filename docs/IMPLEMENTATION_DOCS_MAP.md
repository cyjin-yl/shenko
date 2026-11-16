# 实现范围与必读文档

每个实现范围对应**按需阅读的完整合同**与**当前源码事实**。状态列只描述仓库里真实存在的接线程度，不描述运行验收——运行证据在 [evidence/](evidence/README.md)。

2026-11-15 按 `054817b0d5729fed56ec5a974d101c31edfd1410` 及本次修复复核装配入口。下面的“已接线”不等于真实账号、模型、硬件或故障旅程已经验收；较早源码观察保留日期，不能当成当前缺失项。

本页“多节点”部分另按 continuation 至 `58b30ba` 的装配源码更新：实际控制日志持久化、Store authority bridge 和真实分支已接线；`60e9963` 的四条整合故障旅程及既有 owner 回归通过，具体命令、输入和边界见 [整合验证记录](evidence/readiness-continuation-20261115.json)。该更新不扩大其他领域原有观察的范围。

状态含义：

- **已接线** — 该能力在 `bin/shenko` 的装配路径上有实际调用点。
- **部分已接线** — 该领域有生产调用路径，但不能代表领域内全部能力已接入。
- **仅源码** — 有实现与类型，但没有进入运行时装配。
- **未实现** — 仓库里没有对应实现。

## 按修改范围查阅

先从文档索引选择相关领域；下面是按用途查阅的入口，不要求每次改动通读全部机器账本和治理正文。涉及的义务、Why、接口与恢复条款仍须完整阅读。

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

- **运行时装配（`bin/shenko/src/lib.rs` 与直接拥有者）** — CLI 的 `main.rs` 调用共享库 `run_cli`；`lib.rs` 保留 `Command`、`run_cli`、`run_owned`。`cli.rs` 拥有命令模式，`cli_dispatch.rs` 派发命令；`runtime_bootstrap.rs` 打开安装与分支，`runtime_lifecycle.rs` 拥有运行及主错误保留的清理，`runtime_start.rs` 明确组装主体与界面。插件、共享会话、推理、消费者和各渠道分别由 `runtime_plugins`、`runtime_sessions`、`runtime_inference`、`runtime_consumers`、`runtime_channels` 及连接器模块直接拥有。工作区成员以根 `Cargo.toml` 为准；声明依赖与实际装配分别核对。
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

- **Node 安装单元（`units.rs` 与 `units/`）** — 已接线（`shenko sync` 导出/导入单位包；根模块保留请求与派发，codec、能力、模式迁移、依赖解析、安装、传输、物化、进程试用、证据、采纳、卸载、环境代际及成员关系各有直接拥有者，仍使用同一 `Node::conn`）
  - [§45](specs/cognition-units.md#spec-45)
  - docs/specs/cognition-units.md

- **认知归档与复制（`store/cognition.rs`、`cognition/` 与 `cognition_archive.rs`）** — 已接线（`shenko sync export/receive/ack`；根模块保留公共恢复类型和模式，snapshot、closure、admission、import、files、peers、rows、reconcile、memory、tasks 分别拥有导出、闭包、准入、整笔导入、文件/Git、回执、行模式及语义合并。required-fact 拒绝仍在文件发布之前，导入顺序与提交同事务推进）
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

- **官方 QQ / iLinkBot / 飞书适配器** — 已接线（`SHENKO_CHANNELS_CONFIG` 经 `shenko-channel-adapters` 安装所配置平台，记录健康事实、持久 ingress、主体 disposition 与出站对账；未配置的平台不被宣称为可用）；**真实平台验收仍缺失**：接线只证明装配路径存在，尚未在真实账号上观察过任何一条渠道的入站/出站往返（[证据等级](specs/channels.md#spec-channel-evidence-level)）
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
  - `crates/shenko-ui/web/src/projection.ts#UI_PROJECTION_SCHEMA` 为 `shenko.ui-projection/v2`，应用语义投影允许 `LocalizedText`；`UI_INTENT_SCHEMA` 仍为 v1，服务器 WS 帧与服务 DTO 保持各自协议。结构任务角色、canonical public text、历史缺失来源和本地化显示分别保留类型，界面不增加模型控制行解析。
  - `crates/shenko-ui/web/src/messages.ts` 的 `MessageId`、`MessageArguments`、`UiMessage`、`message` 与 `resolveMessage` 拥有带参数的中文/英文消息目录。语言变体先经 `resolveLocale`，未知语言及缺译项确定地回到 `zh-CN`；服务器和模型原文继续按原文显示，`live-read-models.ts#HistoryReadError` 保留本地诊断的消息 ID 与参数。
  - `LiveController` 只协调连接和状态；`SocketTransport` 拥有 WS 与重连，`LiveReadModels` 拥有读取和历史页，input/timeline reducer、preferences、pending-input store 各守其状态边界。渲染由 timeline surface/entries、composer、delegation branches、submission history、work frame 直接组成；IM 的内容 `ResizeObserver` 阅读锚点与 Inspect 的选择恢复随其组件保留。
  - `main.tsx` 仅在 `?demo=1` 动态载入 `demo-controller`，后者才引入 `demo-fixtures`。`build.mjs` 通过 esbuild 的实际 outputs 记录分块模块及摘要；Rust 的 `build.rs` 按 manifest 嵌入具名 chunks，HTTP 只服务该生成集合。这里记录源码接线；实际浏览器请求、语言、重连及阅读稳定性仍由各自运行回执说明。

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

## 领域合同

[内核合同](specs/kernel.md)之外已登记的领域合同保留原所有权；其中的规范和理由由该领域共同维护。每份领域合同相对内核的定位见[内核合同的对照表](specs/kernel.md#不属于内核的部分)。

| 领域 | 拥有者 | 内容入口 |
| --- | --- | --- |
| [architecture](specs/architecture.md) | parent | 分域规范、接口、Why、错误与恢复 |
| [browser](specs/browser.md) | ArchBrowser | 分域规范、接口、Why、错误与恢复 |
| [channel-adapters](specs/channel-adapters.md) | ArchOtherChannels | 分域规范、接口、Why、错误与恢复 |
| [channels](specs/channels.md) | ArchChannels | 分域规范、接口、Why、错误与恢复 |
| [cognition-units](specs/cognition-units.md) | ArchCognitionUnits | 分域规范、接口、Why、错误与恢复 |
| [configuration-playground](specs/configuration-playground.md) | ArchPlayground | 分域规范、接口、Why、错误与恢复 |
| [contacts](specs/contacts.md) | ArchContacts | 分域规范、接口、Why、错误与恢复 |
| [context-policies](specs/context-policies.md) | ArchContextPolicies | 分域规范、接口、Why、错误与恢复 |
| [desktop-runtime](specs/desktop-runtime.md) | ArchDesktop | 分域规范、接口、Why、错误与恢复 |
| [documentation](specs/documentation.md) | ArchDocs | 分域规范、接口、Why、错误与恢复 |
| [durable-state](specs/durable-state.md) | ArchLedger | 分域规范、接口、Why、错误与恢复 |
| [ecosystem-adapters](specs/ecosystem-adapters.md) | ArchEcosystem | 分域规范、接口、Why、错误与恢复 |
| [external-history](specs/external-history.md) | ArchMemory | 可选 Obelisk 查询、完整原文与稳定引用、跨节点归档、执行及恢复边界 |
| [inference](specs/inference.md) | ArchBroker | 分域规范、接口、Why、错误与恢复 |
| [kernel](specs/kernel.md) | parent | 九条内核不变量、六栏 Why、参考插件的边界 |
| [media](specs/media.md) | ArchMedia | 分域规范、接口、Why、错误与恢复 |
| [memory](specs/memory.md) | ArchMemory | 分域规范、接口、Why、错误与恢复 |
| [mobile-runtime](specs/mobile-runtime.md) | ArchMobile | 分域规范、接口、Why、错误与恢复 |
| [model-children](specs/model-children.md) | ArchModelChildren | 分域规范、接口、Why、错误与恢复 |
| [network-observation](specs/network-observation.md) | ArchNetwork | 分域规范、接口、Why、错误与恢复 |
| [nodes](specs/nodes.md) | ArchNodes | 分域规范、接口、Why、错误与恢复 |
| [plugins](specs/plugins.md) | ArchPluginRuntime | 分域规范、接口、Why、错误与恢复 |
| [quota-resources](specs/quota-resources.md) | ArchQuota | 分域规范、接口、Why、错误与恢复 |
| [remote-seats](specs/remote-seats.md) | ArchRemoteSeats | 分域规范、接口、Why、错误与恢复 |
| [repl](specs/repl.md) | ArchRepl | 分域规范、接口、Why、错误与恢复 |
| [research-memory](specs/research-memory.md) | ArchResearch | 分域规范、接口、Why、错误与恢复 |
| [self-update](specs/self-update.md) | ArchUpgrade | 分域规范、接口、Why、错误与恢复 |
| [subject-runtime](specs/subject-runtime.md) | ArchSubject | 分域规范、接口、Why、错误与恢复 |
| [verification](specs/verification.md) | ArchEvidence | 分域规范、接口、Why、错误与恢复 |
| [wasm-home](specs/wasm-home.md) | ArchWasmHome | 分域规范、接口、Why、错误与恢复 |
| [wasm-tools](specs/wasm-tools.md) | ArchWasmTools | 分域规范、接口、Why、错误与恢复 |
| [web-interface](specs/web-interface.md) | ArchWeb | 分域规范、接口、Why、错误与恢复 |

## Why 按领域阅读

- [为什么要有一份很小的内核合同](decisions/0007-kernel-and-reference-plugins-2026-11-11.md#why)
- [为什么主体需要持续存在](specs/subject-runtime.md#why-1)
- [为什么只有一条主思考流，却鼓励很多孩子](specs/model-children.md#why-2)
- [为什么核心小，而且任何部分都能改](specs/self-update.md#why-3)
- [为什么坚持 WASM 家园，又接入真实操作系统](specs/wasm-home.md#why-4)
- [为什么记忆必须能回到原始事实](specs/memory.md#why-5)
- [为什么 awareness、感知、Skill 与代码要分别安装](specs/cognition-units.md#why-6)
- [为什么多节点仍然是同一自我](specs/nodes.md#why-7)
- [为什么推理后端要真实透传](specs/inference.md#why-8)
- [为什么第一版就要能继续开发自己](specs/self-update.md#why-9)
- [为什么界面从一个主体开始](specs/web-interface.md#why-10)
- [为什么开发规范采用高自主、强证据](specs/documentation.md#why-11)
- [为什么这些规矩也能演进](specs/documentation.md#why-12)

## 章节迁移导航

以下每个入口直接指向当前领域正文；子节分属不同领域时分别列出。完整源行区间与哈希见机器映射。

| 原章节标识 | 现行具体文档 |
| --- | --- |
| §1 | [1 文档状态与产品定义](specs/architecture.md#spec-1) |
| §2 | [2 已确认的产品约束](specs/architecture.md#spec-2) |
| §3 | [3 研究结论与竞品定位](research/product-positioning.md#spec-3) |
| §3.1 | [3.1 具名竞品](research/product-positioning.md#spec-3-1) |
| §3.2 | [3.2 Harness 与记忆系统](research/product-positioning.md#spec-3-2) |
| §3.3 | [3.3 四个会影响方案的事实纠正](research/product-positioning.md#spec-3-3) |
| §4 | [4 核心产品场景](specs/subject-runtime.md#spec-4) |
| §5 | [5 单线程永续主体与子 Agent](specs/subject-runtime.md#spec-5) |
| §5.1 | [5.1 唯一主体的正式语义](specs/subject-runtime.md#spec-5-1) |
| §5.2 | [5.2 主体能力表](specs/subject-runtime.md#spec-5-2) |
| §5.3 | [5.3 委派与对话](specs/model-children.md#spec-5-3) |
| §5.4 | [5.4 崩溃恢复与唯一性](specs/nodes.md#spec-5-4) |
| §5.4.1 | [5.4.1 选举控制与真实经历分别保存](specs/nodes.md#spec-5-4-1) |
| §5.4.2 | [5.4.2 重连后只留下一个主体](specs/nodes.md#spec-5-4-2) |
| §5.4.3 | [5.4.3 资源与副作用不随选举清零](specs/nodes.md#spec-5-4-3) |
| §5.4.4 | [5.4.4 统一 Node、共享认知与节点能力](specs/nodes.md#spec-5-4-4) |
| §5.5 | [5.5 共享能力全集与主体—child 双向协作](specs/model-children.md#spec-5-5) |
| §5.6 | [5.6 非阻塞 thought 接口与事实投影](specs/subject-runtime.md#spec-5-6) |
| §6 | [6 人格、可自改灵魂与统一知识](specs/subject-runtime.md#spec-6) |
| §7 | [7 系统架构与故障边界](specs/plugins.md#spec-7) |
| §8 | [8 WASM 家园与工具兼容](specs/wasm-home.md#spec-8) |
| §8.1 | [8.1 Runtime 选择与兼容等级](specs/wasm-home.md#spec-8-1) |
| §8.2 | [8.2 工具清单与真实差距](specs/wasm-tools.md#spec-8-2) |
| §8.3 | [8.3 执行和挂载合同](specs/wasm-home.md#spec-8-3) |
| §9 | [9 Nix-like 包与自我改进](specs/self-update.md#spec-9) |
| §9.1 | [9.1 任何部分都可自主重写](specs/self-update.md#spec-9-1) |
| §9.2 | [9.2 版本交接与行为合同](specs/self-update.md#spec-9-2) |
| §9.3 | [9.3 构建位置与生命周期清理](specs/self-update.md#spec-9-3) |
| §9.4 | [9.4 自主更新策略与联网配置](specs/network-observation.md#spec-9-4) |
| §9.5 | [9.5 磁盘优先的 breaking-change 迁移插件](specs/self-update.md#spec-9-5) |
| §10 | [10 文件版本与恢复](specs/durable-state.md#spec-10) |
| §10.1 | [10.1 三个状态域](specs/durable-state.md#spec-10-1) |
| §10.2 | [10.2 二进制友好的版本层](specs/durable-state.md#spec-10-2) |
| §10.2.1 | [10.2.1 LayerFS 原版运行于真实 Linux VM](specs/durable-state.md#spec-10-2-1) |
| §10.3 | [10.3 默认 Git 记忆和私有仓库](specs/memory.md#spec-10-3) |
| §10.4 | [10.4 恢复流程](specs/durable-state.md#spec-10-4) |
| §10.5 | [10.5 二进制引用、提交和回收协议](specs/memory.md#spec-10-5) |
| §10.6 | [10.6 参考 Pi Durable 的持久执行协议](specs/durable-state.md#spec-10-6) |
| §11 | [11 SSH 身份与远端工具](specs/remote-seats.md#spec-11) |
| §12 | [12 会话真相源和记忆](specs/memory.md#spec-12) |
| §12.1 | [12.1 数据模型与来源](specs/memory.md#spec-12-1) |
| §12.2 | [12.2 记忆请求与提交](specs/memory.md#spec-12-2) |
| §12.3 | [12.3 历史切窗和中文检索](specs/context-policies.md#spec-12-3) |
| §12.3.1 | [12.3.1 Budget compact、notes-backed windows 与 snapcompact](specs/context-policies.md#spec-12-3-1) |
| §12.3.2 | [12.3.2 切窗事务、未完成工具和 KV 代价](specs/context-policies.md#spec-12-3-2) |
| §12.4 | [12.4 遗忘、保留和隐私](specs/memory.md#spec-12-4) |
| §13 | [13 推理调度与 KV 池](specs/inference.md#spec-13) |
| §13.1 | [13.1 三种资源必须分开](specs/quota-resources.md#spec-13-1) |
| §13.2 | [13.2 自动分配中的半池情形](specs/quota-resources.md#spec-13-2) |
| §13.3 | [13.3 自动分配与主体调控](specs/quota-resources.md#spec-13-3) |
| §13.4 | [13.4 计算顺序与无死锁规则](specs/quota-resources.md#spec-13-4) |
| §13.5 | [13.5 KV 句柄与上下文稳定性](specs/inference.md#spec-13-5) |
| §13.6 | [13.6 Backend 能力矩阵](specs/inference.md#spec-13-6) |
| §13.7 | [13.7 原生 API、能力协商与真实透传](specs/inference.md#spec-13-7) |
| §13.7.1 | [13.7.1 四个后端的适配重点](specs/inference.md#spec-13-7-1) |
| §13.7.2 | [13.7.2 传输差异必须可见](specs/inference.md#spec-13-7-2) |
| §13.7.3 | [13.7.3 资源域、轮转与缓存承诺](specs/inference.md#spec-13-7-3) |
| §13.8 | [13.8 外部打断、主体决定免打扰与过滤修复](specs/subject-runtime.md#spec-13-8) |
| §14 | [14 递归研究与选择性记忆](specs/research-memory.md#spec-14) |
| §14.1 | [14.1 两篇新增研究的含义](specs/research-memory.md#spec-14-1) |
| §14.2 | [14.2 四种状态不同处理](specs/research-memory.md#spec-14-2) |
| §14.3 | [14.3 Research reset 合同](specs/research-memory.md#spec-14-3) |
| §14.4 | [14.4 主动记忆 worker](specs/research-memory.md#spec-14-4) |
| §14.5 | [14.5 递归收敛](specs/model-children.md#spec-14-5) |
| §14.6 | [14.6 可无限延续的自主兴趣](specs/research-memory.md#spec-14-6) |
| §15 | [15 消息注入与认识人的能力](specs/channels.md#spec-15) |
| §15.1 | [15.1 消息封套](specs/channels.md#spec-15-1) |
| §15.2 | [15.2 联系人与声纹插件](specs/contacts.md#spec-15-2) |
| §15.2.1 | [15.2.1 复用 Life 的已有身份材料](specs/contacts.md#spec-15-2-1) |
| §15.3 | [15.3 消息插件合同](specs/channel-adapters.md#spec-15-3) |
| §16 | [16 中国平台与开放对话](specs/channel-adapters.md#spec-16) |
| §17 | [17 远程 Agent 遥控插件](specs/remote-seats.md#spec-17) |
| §17.1 | [17.1 任务与 seat 的连续性](specs/remote-seats.md#spec-17-1) |
| §17.2 | [17.2 自主处理问题与许可](specs/remote-seats.md#spec-17-2) |
| §17.3 | [17.3 主动寻找人类](specs/remote-seats.md#spec-17-3) |
| §17.4 | [17.4 远端推理资源](specs/remote-seats.md#spec-17-4) |
| §17.5 | [17.5 终端观察与语义整理](specs/remote-seats.md#spec-17-5) |
| §18 | [18 插件能被发现并正确使用](specs/plugins.md#spec-18) |
| §18.1 | [18.1 原生插件优先和生态兼容](specs/plugins.md#spec-18-1) |
| §18.2 | [18.2 可使用性包](specs/plugins.md#spec-18-2) |
| §18.3 | [18.3 相关研究与设计判断](specs/plugins.md#spec-18-3) |
| §18.4 | [18.4 27B 本地模型的优化](specs/cognition-units.md#spec-18-4) |
| §18.5 | [18.5 评估与持续改善](specs/cognition-units.md#spec-18-5) |
| §18.6 | [18.6 即时工具知识与使用审计](specs/plugins.md#spec-18-6) |
| §18.7 | [18.7 能力缺口发现与自主补全](specs/cognition-units.md#spec-18-7) |
| §18.8 | [18.8 WASM 能力自主移植](specs/wasm-tools.md#spec-18-8) |
| §19 | [19 有状态 REPL 插件](specs/repl.md#spec-19) |
| §19.1 | [19.1 多 kernel 与统一会话生命周期](specs/repl.md#spec-19-1) |
| §20 | [20 实时通话与课堂旁听扩展](specs/media.md#spec-20) |
| §21 | [21 Web UI 与设计系统](specs/web-interface.md#spec-21) |
| §21.1 | [21.1 体验目标与功能要求：交由 Claude Opus 设计实现](specs/web-interface.md#spec-21-1) |
| §21.2 | [21.2 Proto-UI 主线和 compiler seam](specs/web-interface.md#spec-21-2) |
| §21.3 | [21.3 UI 本身也是插件](specs/web-interface.md#spec-21-3) |
| §21.4 | [21.4 Dynamic Response 与官方参考边界](specs/web-interface.md#spec-21-4) |
| §21.5 | [21.5 目标能力协商](specs/web-interface.md#spec-21-5) |
| §21.6 | [21.6 DSL 与 UI 工具两条入口](specs/web-interface.md#spec-21-6) |
| §21.7 | [21.7 跨界面降级](specs/web-interface.md#spec-21-7) |
| §21.8 | [21.8 状态与动作闭环](specs/web-interface.md#spec-21-8) |
| §21.9 | [21.9 流式呈现与 compiler 迁移](specs/web-interface.md#spec-21-9) |
| §21.10 | [21.10 Magpie 视觉参照与比较方法](specs/web-interface.md#spec-21-10) |
| §21.11 | [21.11 流式输出的阅读稳定性：Magpie 与 Alma 参照](specs/web-interface.md#spec-21-11) |
| §21.12 | [21.12 默认 Web 界面：与主体直接交互的一条时间线](specs/web-interface.md#spec-21-12) |
| §22 | [22 部署和资源目标](specs/nodes.md#spec-22) |
| §22.1 | [22.1 首个真实评测基线：用户的 Qwen3.8-27B，经 Tailnet 调用](specs/nodes.md#spec-22-1) |
| §22.2 | [22.2 无模型时的日志、外部提取与离线复活](specs/nodes.md#spec-22-2) |
| §23 | [23 验收与发布阻断项](specs/verification.md#spec-23) |
| §24 | [24 Computer Use 与完整操作系统节点](specs/desktop-runtime.md#spec-24) |
| §24.1 | [24.1 正式能力及插件边界](specs/desktop-runtime.md#spec-24-1) |
| §24.2 | [24.2 操作系统边界](specs/desktop-runtime.md#spec-24-2) |
| §24.3 | [24.3 观察版本和桌面输入所有权](specs/desktop-runtime.md#spec-24-3) |
| §24.4 | [24.4 GUI 副作用和资源](specs/desktop-runtime.md#spec-24-4) |
| §24.5 | [24.5 移动 OS 感知与执行均为插件，APK 是发行组合](specs/mobile-runtime.md#spec-24-5) |
| §24.5.1 | [24.5.1 自有应用 SQLite 观察与自主风险判断](specs/mobile-runtime.md#spec-24-5-1) |
| §24.6 | [24.6 原生浏览器能力、持久身份与人类接管](specs/browser.md#spec-24-6) |
| §24.6.1 | [24.6.1 身份、运行会话和真实持久化范围](specs/browser.md#spec-24-6-1) |
| §24.6.2 | [24.6.2 virtual-headed 默认与同会话接管](specs/browser.md#spec-24-6-2) |
| §24.6.3 | [24.6.3 崩溃恢复与待验收行为](specs/browser.md#spec-24-6-3) |
| §25 | [25 NInfer 接入教程 中文与 English](specs/inference.md#spec-25) |
| §25.1 | [25.1 中文操作路径](specs/inference.md#spec-25-1) |
| §25.2 | [25.2 English walkthrough](specs/inference.md#spec-25-2) |
| §26 | [26 Strata 接入教程 中文与 English](specs/inference.md#spec-26) |
| §26.1 | [26.1 中文操作路径](specs/inference.md#spec-26-1) |
| §26.2 | [26.2 English walkthrough](specs/inference.md#spec-26-2) |
| §27 | [27 共同排障与接入验收 Common troubleshooting](specs/verification.md#spec-27) |
| §28 | [28 规格裁决记录与仍未决的问题](decisions/product-rulings.md#spec-28) |
| §29 | [29 交付路线与阶段验收](specs/verification.md#spec-29) |
| §29.1 | [29.1 文档与工程维护规则](specs/verification.md#spec-29-1) |
| §29.2 | [29.2 本轮研究组织与完成状态](specs/verification.md#spec-29-2) |
| §30 | [30 DeepSeek 与 Gemini 建议的采纳记录](research/design-review-adoption.md#spec-30) |
| §31 | [31 研究版本清单与证据使用规则](requirements/research-sources.md#spec-31) |
| §32 | [32 需求追踪矩阵](requirements/traceability.md#spec-32) |
| §33 | [33 用户需求原文与追加澄清](requirements/user-sources.md#spec-33) |
| §34 | [34 BoxLite：可选完整 Linux Runtime 插件评估](specs/desktop-runtime.md#spec-34) |
| §34.1 | [34.1 结论与研究基线](specs/desktop-runtime.md#spec-34-1) |
| §34.2 | [34.2 架构位置与插件划分](specs/desktop-runtime.md#spec-34-2) |
| §34.3 | [34.3 持久化、快照与任意挂载的真实边界](specs/desktop-runtime.md#spec-34-3) |
| §34.4 | [34.4 Computer Use、远端 Agent 与 REPL](specs/desktop-runtime.md#spec-34-4) |
| §34.5 | [34.5 自主联网、代理与升级](specs/desktop-runtime.md#spec-34-5) |
| §34.6 | [34.6 设备、体积与资源核算](specs/desktop-runtime.md#spec-34-6) |
| §34.7 | [34.7 条件验收与采用决定](specs/desktop-runtime.md#spec-34-7) |
| §35 | [35 首版开发者发行：可继续开发自身的完整闭环](specs/self-update.md#spec-35) |
| §35.1 | [35.1 首版目标与既有约束](specs/self-update.md#spec-35-1) |
| §35.2 | [35.2 开发者机器与 WASM 家园的关系](specs/wasm-home.md#spec-35-2) |
| §35.3 | [35.3 首版默认插件闭包](specs/plugins.md#spec-35-3) |
| §35.4 | [35.4 一次真实自开发任务](specs/self-update.md#spec-35-4) |
| §35.5 | [35.5 三条 Seat 路径，各自兑现能力](specs/remote-seats.md#spec-35-5) |
| §35.6 | [35.6 NapCat 接入和升级期间的消息连续性](specs/channels.md#spec-35-6) |
| §35.7 | [35.7 自升级必须完成运行中的交接](specs/self-update.md#spec-35-7) |
| §35.8 | [35.8 能力缺口必须能驱动实际开发](specs/self-update.md#spec-35-8) |
| §35.9 | [35.9 全局推理与中断](specs/inference.md#spec-35-9) |
| §35.10 | [35.10 首版自动验收](specs/self-update.md#spec-35-10) |
| §36 | [36 插件的副作用、重置与恢复边界](specs/durable-state.md#spec-36) |
| §36.1 | [36.1 正式合同与适用范围](specs/durable-state.md#spec-36-1) |
| §36.2 | [36.2 五种动作不能共用一个 rollback 标签](specs/durable-state.md#spec-36-2) |
| §36.3 | [36.3 静态声明按 operation 细分](specs/durable-state.md#spec-36-3) |
| §36.4 | [36.4 调用时解析实际恢复边界](specs/durable-state.md#spec-36-4) |
| §36.5 | [36.5 一次操作的持久记录](specs/durable-state.md#spec-36-5) |
| §36.6 | [36.6 回退计划与跨版本恢复](specs/durable-state.md#spec-36-6) |
| §36.7 | [36.7 模型、工具与界面如何看到边界](specs/durable-state.md#spec-36-7) |
| §36.8 | [36.8 首版恢复合同验收](specs/durable-state.md#spec-36-8) |
| §37 | [37 cordis-rs 主干上的注入时序与 API 兼容](specs/context-policies.md#spec-37) |
| §37.1 | [37.1 本轮裁决与参考范围](specs/context-policies.md#spec-37-1) |
| §37.2 | [37.2 四种顺序分别表达](specs/context-policies.md#spec-37-2) |
| §37.3 | [37.3 外部事件先持久化，再在可用边界注入](specs/context-policies.md#spec-37-3) |
| §37.4 | [37.4 Hook 最小描述](specs/context-policies.md#spec-37-4) |
| §37.5 | [37.5 Phase、相对顺序与冲突](specs/context-policies.md#spec-37-5) |
| §37.6 | [37.6 首版建议的语义扩展点](specs/context-policies.md#spec-37-6) |
| §37.7 | [37.7 Prompt 布局、工具正确性与 KV](specs/context-policies.md#spec-37-7) |
| §37.8 | [37.8 异步、有限等待与 C=1](specs/context-policies.md#spec-37-8) |
| §37.9 | [37.9 新增 Hook、兼容与自修改](specs/context-policies.md#spec-37-9) |
| §37.10 | [37.10 激活、重放与审计](specs/context-policies.md#spec-37-10) |
| §37.11 | [37.11 首版验收](specs/context-policies.md#spec-37-11) |
| §38 | [38 内置、可替换的自解释插件](specs/cognition-units.md#spec-38) |
| §38.1 | [38.1 产品能力与角色边界](specs/cognition-units.md#spec-38-1) |
| §38.2 | [38.2 OMP 参考与本产品增加的能力](specs/cognition-units.md#spec-38-2) |
| §38.3 | [38.3 四类自身资料与版本身份](specs/cognition-units.md#spec-38-3) |
| §38.4 | [38.4 索引与部署体积](specs/cognition-units.md#spec-38-4) |
| §38.5 | [38.5 何时主动使用](specs/cognition-units.md#spec-38-5) |
| §38.6 | [38.6 回答、开发与更新的闭环](specs/cognition-units.md#spec-38-6) |
| §38.7 | [38.7 首版验收](specs/cognition-units.md#spec-38-7) |
| §39 | [39 设计来源与上游行为完整度](requirements/design-provenance.md#spec-39) |
| §39.1 | [39.1 所有设计均有来源和比较范围](requirements/design-provenance.md#spec-39-1) |
| §39.2 | [39.2 来源登记](requirements/design-provenance.md#spec-39-2) |
| §39.3 | [39.3 已枚举的行为对照](requirements/design-provenance.md#spec-39-3) |
| §39.4 | [39.4 较细的 memory 对照种子](requirements/design-provenance.md#spec-39-4) |
| §39.5 | [39.5 完整度怎样比较](requirements/design-provenance.md#spec-39-5) |
| §39.6 | [39.6 维护与自解释](requirements/design-provenance.md#spec-39-6) |
| §40 | [40 开发 Agent 自主工作规范](governance/development-contract.md#spec-40) |
| §40.1 | [40.1 默认连续完成已授权目标](governance/development-contract.md#spec-40-1) |
| §40.2 | [40.2 实现选择与未决问题](governance/development-contract.md#spec-40-2) |
| §40.3 | [40.3 交付完整行为](governance/development-contract.md#spec-40-3) |
| §40.4 | [40.4 工程与插件约定](governance/development-contract.md#spec-40-4) |
| §40.5 | [40.5 自动验证与相称成本](governance/development-contract.md#spec-40-5) |
| §40.6 | [40.6 自主 UI 实现与视觉验收](governance/development-contract.md#spec-40-6) |
| §40.7 | [40.7 文档与任务接续](governance/development-contract.md#spec-40-7) |
| §40.8 | [40.8 规范验收](governance/development-contract.md#spec-40-8) |
| §40.9 | [40.9 先表达目的与理由](governance/development-contract.md#spec-40-9) |
| §41 | [41 首版规范执行合同](specs/architecture.md#spec-41) |
| §41.1 | [41.1 本章的地位、范围与阅读方法](specs/verification.md#spec-41-1) |
| §41.1.1 | [41.1.1 定义写完的判据](specs/verification.md#spec-41-1-1) |
| §41.2 | [41.2 默认持久存储与观察边界](specs/durable-state.md#spec-41-2) |
| §41.2.1 | [41.2.1 本地写入域与 SQLite profile](specs/durable-state.md#spec-41-2-1) |
| §41.2.2 | [41.2.2 对象与元数据的提交顺序](specs/durable-state.md#spec-41-2-2) |
| §41.2.3 | [41.2.3 提交失败、发布与重开](specs/durable-state.md#spec-41-2-3) |
| §41.3 | [41.3 共用身份、类型与调用上下文](specs/durable-state.md#spec-41-3) |
| §41.4 | [41.4 任务、效果与恢复的规范记录](specs/durable-state.md#spec-41-4) |
| §41.5 | [41.5 永续主体的一步与有限子任务](specs/subject-runtime.md#spec-41-5) |
| §41.6 | [41.6 外部事件、注意力与输入放置](specs/channels.md#spec-41-6) |
| §41.7 | [41.7 插件操作接口与实际效果绑定](specs/plugins.md#spec-41-7) |
| §41.7.1 | [41.7.1 静态合同与本次实际绑定](specs/plugins.md#spec-41-7-1) |
| §41.7.2 | [41.7.2 调用接口与幂等身份](specs/plugins.md#spec-41-7-2) |
| §41.7.3 | [41.7.3 状态、回执与先后顺序](specs/plugins.md#spec-41-7-3) |
| §41.8 | [41.8 Hook、上下文与请求封口](specs/context-policies.md#spec-41-8) |
| §41.8.1 | [41.8.1 扩展点、处理器与图](specs/context-policies.md#spec-41-8-1) |
| §41.8.2 | [41.8.2 各类输出的合成规则](specs/context-policies.md#spec-41-8-2) |
| §41.8.3 | [41.8.3 RequestSeal](specs/context-policies.md#spec-41-8-3) |
| §41.8.4 | [41.8.4 hook 恢复与热更新](specs/context-policies.md#spec-41-8-4) |
| §41.9 | [41.9 推理请求、计算许可与单并发](specs/inference.md#spec-41-9) |
| §41.10 | [41.10 KV 与原生后端能力](specs/inference.md#spec-41-10) |
| §41.11 | [41.11 远端 Seat 问题、决定与答复](specs/remote-seats.md#spec-41-11) |
| §41.12 | [41.12 自主升级与跨代交接](specs/self-update.md#spec-41-12) |
| §41.13 | [41.13 自解释的版本锚点与读取接口](specs/cognition-units.md#spec-41-13) |
| §41.14 | [41.14 结构化错误与恢复动作](specs/durable-state.md#spec-41-14) |
| §41.15 | [41.15 QQ 到远端开发再到自身升级的完整轨迹](specs/self-update.md#spec-41-15) |
| §41.16 | [41.16 规范故障与一致性验收](specs/verification.md#spec-41-16) |
| §41.16.1 | [41.16.1 提交与代际故障](specs/verification.md#spec-41-16-1) |
| §41.16.2 | [41.16.2 主体、队列、后端与 Seat](specs/verification.md#spec-41-16-2) |
| §41.16.3 | [41.16.3 插件与版本交错](specs/verification.md#spec-41-16-3) |
| §41.17 | [41.17 精确上游锚点与本产品差异](requirements/design-provenance.md#spec-41-17) |
| §41.18 | [41.18 WebInputSubmission 规范接口](specs/web-interface.md#spec-41-18) |
| §42 | [42 Provider 周期额度、模型选择与持续思考的自适应调优](specs/quota-resources.md#spec-42) |
| §42.1 | [42.1 产品目标与边界](specs/quota-resources.md#spec-42-1) |
| §42.2 | [42.2 已核验的 Anthropic 观测面](specs/quota-resources.md#spec-42-2) |
| §42.3 | [42.3 规范类型](specs/quota-resources.md#spec-42-3) |
| §42.4 | [42.4 观测、归因与账本边界](specs/quota-resources.md#spec-42-4) |
| §42.5 | [42.5 自适应算法与准入节奏的最低合同](specs/quota-resources.md#spec-42-5) |
| §42.6 | [42.6 与事件、RequestSeal、KV 和执行调度的衔接](specs/quota-resources.md#spec-42-6) |
| §42.7 | [42.7 验收场景](specs/quota-resources.md#spec-42-7) |
| §42.8 | [42.8 来源、采用范围与证据边界](specs/quota-resources.md#spec-42-8) |
| §43 | [43 对话配置与持久 Playground](specs/configuration-playground.md#spec-43) |
| §43.1 | [43.1 所有配置可通过对话管理](specs/configuration-playground.md#spec-43-1) |
| §43.2 | [43.2 Playground 是长期存在的全栈作品空间](specs/configuration-playground.md#spec-43-2) |
| §43.3 | [43.3 本轮验收增量](specs/configuration-playground.md#spec-43-3) |
| §44 | [44 首次启动与种子发行](specs/plugins.md#spec-44) |
| §44.1 | [44.1 第一次打开会发生什么](specs/plugins.md#spec-44-1) |
| §44.2 | [44.2 无 LLM 时种子发行必须已经具备的闭包](specs/plugins.md#spec-44-2) |
| §44.3 | [44.3 真正必填、自动推导与后续发展](specs/plugins.md#spec-44-3) |
| §44.4 | [44.4 首位 owner/admin 与普通聊天](specs/plugins.md#spec-44-4) |
| §44.5 | [44.5 Bootstrap 记录与接口](specs/plugins.md#spec-44-5) |
| §44.6 | [44.6 首个 Genesis、根和唯一第一步](specs/plugins.md#spec-44-6) |
| §44.7 | [44.7 可独立安装的 awareness 与执行基元](specs/plugins.md#spec-44-7) |
| §44.8 | [44.8 NapCat 与 tmux 首版可选接入](specs/plugins.md#spec-44-8) |
| §44.9 | [44.9 错误合同与必要验收](specs/plugins.md#spec-44-9) |
| §45 | [45 可独立安装的 Awareness、感知与依赖装配](specs/cognition-units.md#spec-45) |
| §45.1 | [45.1 定义与独立交付单元](specs/cognition-units.md#spec-45-1) |
| §45.2 | [45.2 单元内容与最小包形式](specs/cognition-units.md#spec-45-2) |
| §45.3 | [45.3 具体单元示例](specs/cognition-units.md#spec-45-3) |
| §45.4 | [45.4 独立安装、更新与共享目录](specs/cognition-units.md#spec-45-4) |
| §45.5 | [45.5 渐进披露与正确时机](specs/cognition-units.md#spec-45-5) |
| §45.6 | [45.6 与 Skill、执行插件和策略的组合](specs/cognition-units.md#spec-45-6) |
| §45.7 | [45.7 持续经历与实现生命周期](specs/cognition-units.md#spec-45-7) |
| §45.8 | [45.8 配置直觉与固定 phase 的删除](specs/cognition-units.md#spec-45-8) |
| §45.9 | [45.9 最低验收](specs/cognition-units.md#spec-45-9) |
| §45.10 | [45.10 可安装单元与依赖装配](specs/cognition-units.md#spec-45-10) |
| §45.10.1 | [45.10.1 独立单元及其职责](specs/cognition-units.md#spec-45-10-1) |
| §45.10.2 | [45.10.2 一份依赖图，两条独立的依赖属性](specs/cognition-units.md#spec-45-10-2) |
| §45.10.3 | [45.10.3 Provider 自动安装依赖闭包](specs/cognition-units.md#spec-45-10-3) |
| §45.10.4 | [45.10.4 QQ 断连的实际绑定](specs/cognition-units.md#spec-45-10-4) |
| §45.10.5 | [45.10.5 更新、卸载与反向依赖](specs/cognition-units.md#spec-45-10-5) |
| §45.10.6 | [45.10.6 验收增量](specs/cognition-units.md#spec-45-10-6) |
| §45.11 | [45.11 来源与研究边界](specs/cognition-units.md#spec-45-11) |
| §45.12 | [45.12 个性化适配、反思与自我推翻的插件合同](specs/memory.md#spec-45-12) |
| §45.12.1 | [45.12.1 可判定结果与剩余工作](specs/memory.md#spec-45-12-1) |
| §45.13 | [45.13 可替换的时间与环境感知](specs/cognition-units.md#spec-45-13) |
| §45.14 | [45.14 运行时错误事实与主体自主处置](specs/cognition-units.md#spec-45-14) |
| §46 | [46 完整产品的自主开发与仓库交接](governance/COMPLETION.md#spec-46) |
| §46.1 | [46.1 文档与自主权](governance/COMPLETION.md#spec-46-1) |
| §46.2 | [46.2 完整目标与 PR](governance/COMPLETION.md#spec-46-2) |
| §46.3 | [46.3 持久工作与验证](governance/COMPLETION.md#spec-46-3) |
| §46.4 | [46.4 GitHub 与启动](governance/COMPLETION.md#spec-46-4) |
| §46.5 | [46.5 完成证明](governance/COMPLETION.md#spec-46-5) |
| §46.6 | [46.6 交接验收](governance/COMPLETION.md#spec-46-6) |
| §47 | [47 Provider 协议版本、SDK 演进与自主迁移](specs/inference.md#spec-47) |
| §47.1 | [47.1 Why 与适用范围](specs/inference.md#spec-47-1) |
| §47.2 | [47.2 核验基线](specs/inference.md#spec-47-2) |
| §47.3 | [47.3 版本化绑定与请求封口](specs/inference.md#spec-47-3) |
| §47.4 | [47.4 参数意图与消息投影](specs/inference.md#spec-47-4) |
| §47.5 | [47.5 鉴权、代理和额度解释](specs/inference.md#spec-47-5) |
| §47.6 | [47.6 可安装感知、直觉与自主升级](specs/inference.md#spec-47-6) |
| §47.7 | [47.7 兼容验收](specs/inference.md#spec-47-7) |
| §48 | [48 自主主体、运行时能力闭环与正式文档分层](specs/architecture.md#spec-48) |
| §48.1 | [48.1 自主性与真实约束分开](specs/architecture.md#spec-48-1) |
| §48.2 | [48.2 单元、服务与运行事实闭合](specs/architecture.md#spec-48-2) |
| §48.3 | [48.3 感知、渐进披露与学习](specs/architecture.md#spec-48-3) |
| §48.4 | [48.4 协议所有权与产品表面](specs/architecture.md#spec-48-4) |
| §48.5 | [48.5 正式规范与历史入口](specs/architecture.md#spec-48-5) |
| §48.6 | [48.6 上游协作不得阻塞产品开发](specs/architecture.md#spec-48-6) |
| §49 | [49 第三方生态兼容：可选的 MCP 与 Agent Skills 适配器](specs/ecosystem-adapters.md#spec-49) |
| §49.1 | [49.1 两个独立插件，一个安装权威](specs/ecosystem-adapters.md#spec-49-1) |
| §49.2 | [49.2 MCP 适配器的真实边界](specs/ecosystem-adapters.md#spec-49-2) |
| §49.3 | [49.3 Skill 适配器的真实边界](specs/ecosystem-adapters.md#spec-49-3) |
| §49.4 | [49.4 使用中的磨合学习](specs/ecosystem-adapters.md#spec-49-4) |
| §49.5 | [49.5 不降低既有要求](specs/ecosystem-adapters.md#spec-49-5) |
| §50 | [50 追加事实](specs/architecture.md#spec-50) |
| §50.1 | [50.1 适配器传输有界与大观察的有界取回](specs/ecosystem-adapters.md#spec-50-1) |
| §50.2 | [50.2 使用中学习的确定性使用摘要](specs/ecosystem-adapters.md#spec-50-2) |
| §50.3 | [50.3 通路观察的测量入口](specs/network-observation.md#spec-50-3) |
| §50.4 | [50.4 未竟义务的显式结清与复制闭包](specs/durable-state.md#spec-50-4) |
| §51 | [51 认知元概念：内容、态度与理由](specs/cognition-units.md#spec-51) |
| §51.1 | [51.1 元概念](specs/cognition-units.md#spec-51-1) |
| §51.2 | [51.2 awareness 是能力，不是文档](specs/cognition-units.md#spec-51-2) |
| §51.3 | [51.3 内核、宏包与发行](specs/cognition-units.md#spec-51-3) |
| §51.4 | [51.4 可学习](specs/cognition-units.md#spec-51-4) |
| §51.5 | [51.5 主体地位](specs/cognition-units.md#spec-51-5) |
| §51.6 | [51.6 尚未实现](specs/cognition-units.md#spec-51-6) |
| §51.7 | [51.7 提议：遗忘与归档](specs/cognition-units.md#spec-51-7) |
| §51.8 | [51.8 提议：按能力依赖，不钉死具体包](specs/cognition-units.md#spec-51-8) |
| §51.9 | [51.9 助手代定（用户授权）](specs/cognition-units.md#spec-51-9) |
| §51.10 | [51.10 读法治理](specs/cognition-units.md#spec-51-10) |
| §52 | [52 通道协议优先级 / Channel Protocol Priority](specs/channels.md#spec-52) |
| §53 | [53 子代理命名 / Child Agent Naming](specs/model-children.md#spec-53) |
| §54 | [54 自主设计原则 / Autonomous Design Principles](specs/architecture.md#spec-54) |
| §54.1 | [54.1 从人类心智和认知学角度设计](specs/architecture.md#spec-54-1) |
| §54.2 | [54.2 不受模型限制约束的设计目标](specs/architecture.md#spec-54-2) |
| §54.3 | [54.3 可扩展与插件化](specs/architecture.md#spec-54-3) |
| §54.4 | [54.4 更好的 Awareness 与 Perception](specs/architecture.md#spec-54-4) |
| §54.5 | [54.5 同行参考 / Peer Reference](specs/architecture.md#spec-54-5) |
| §55 | [55 自主能力验收合同 / Autonomous Capability Acceptance](specs/verification.md#spec-55) |
| §55.1 | [55.1 自主性的可判定化](specs/verification.md#spec-55-1) |
| §55.2 | [55.2 模型限制不作为设计边界](specs/verification.md#spec-55-2) |
| §55.3 | [55.3 插件化验收](specs/verification.md#spec-55-3) |
| §55.4 | [55.4 Awareness 与 Perception 的持续演进](specs/verification.md#spec-55-4) |
| §55.5 | [55.5 同行参考的落地边界](specs/verification.md#spec-55-5) |
| §56 | [56 验收追踪矩阵 / Acceptance Traceability Matrix](requirements/traceability.md#spec-56) |
| §57 | [57 主动个人主体：更少的门、可修订的 harness、界面与 IM 精炼（Claude Fable 5.1 refine 版本）](specs/architecture.md#spec-57) |
| §57.0 | [57.0 为什么要这一章：骨架是对的，肉是死的](specs/architecture.md#spec-57-0) |
| §57.1 | [57.1 角色框架是可修订内容单元](specs/subject-runtime.md#spec-57-1) |
| §57.2 | [57.2 空闲是感知，不是提示](specs/subject-runtime.md#spec-57-2) |
| §57.3 | [57.3 注意力是主体的政策](specs/subject-runtime.md#spec-57-3) |
| §57.4 | [57.4 通道拥有自己的表面语法](specs/channels.md#spec-57-4) |
| §57.5 | [57.5 流式是时间线的默认真相](specs/web-interface.md#spec-57-5) |
| §57.6 | [57.6 作品是主体伸向界面的手](specs/configuration-playground.md#spec-57-6) |
| §57.7 | [57.7 界面的诚实词汇与生命感](specs/web-interface.md#spec-57-7) |
| §57.8 | [57.8 三个死接口复活](specs/subject-runtime.md#spec-57-8) |
| §57.9 | [57.9 对齐即说明风险与收益](specs/cognition-units.md#spec-57-9) |
| §57.10 | [57.10 自迭代闭环](specs/self-update.md#spec-57-10) |
| §57.11 | [57.11 需求](requirements/traceability.md#spec-57-11) |
| §57.12 | [57.12 行为](requirements/design-provenance.md#spec-57-12) |
| §57.13 | [57.13 验收](specs/verification.md#spec-57-13) |
| §57.14 | [57.14 实现状态与证据边界](specs/verification.md#spec-57-14) |
| §57.15 | [57.15 通道优先级的现行读法（2026-11-09）](specs/channels.md#spec-57-15) |

## 现行合同清单

[contracts.json](contracts.json) 枚举现行合同文档，消费者读取每份文档全文，包括迁移标记外的新条目。新增合同文档时同步此清单与机器索引。历史迁移映射只证明原件保全和去向，不规定未来合同边界，也不要求新条目虚构旧章节。
