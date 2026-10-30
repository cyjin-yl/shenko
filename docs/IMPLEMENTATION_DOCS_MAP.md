# Shenko Implementation Documentation Map

每个实现范围对应的**必读文档页**。任何 Agent 开始实现前必须完整读取对应文档。

## 格式
- `实现范围`: 必读文档列表（按优先级）

## S1 Core (已合并 main)
- **Store (SQLite WAL + CAS + ledger)**: 
  - SPEC.md §41.2 (Store & Ledger)
  - docs/requirements/requirements.json (R030, R031)
  - docs/requirements/acceptance-cases.json (STORE-01~04)
  
- **Protocol types (IDs, CommitRef, TaskRecord, etc.)**:
  - SPEC.md §41.3 (协议类型)
  - docs/requirements/requirements.json (R029)

- **Broker (Anthropic API)**:
  - SPEC.md §13 (Inference admission)
  - SPEC.md §47 (Provider wire contract PV01-PV08)
  - docs/research/claude-api-2026-10-21.md
  - docs/requirements/requirements.json (R008, R009, R059)

## S2 Critical Path (已合并 main)
- **SubjectLoop (S0/S1 state machine)**:
  - SPEC.md §5.1 (持续主体)
  - SPEC.md §41.5 (SubjectStep state transitions)
  - docs/requirements/requirements.json (R003, R013)
  - docs/requirements/acceptance-cases.json (LOOP-01~05)

- **SubjectBranchHead + SubjectGenesis**:
  - SPEC.md §41.5 (SubjectBranchHead)
  - SPEC.md §44.6 (Genesis bootstrap)
  - docs/requirements/requirements.json (R051)

- **NapCat WS client**:
  - SPEC.md §16 (QQ channel)
  - docs/requirements/requirements.json (R001, R014)

- **NapCat Link (可选 backend)** — shenko-channel-napcat-link 提供可选 feature backend（`napcat-link` feature，默认关闭，非 bin 依赖）:
  - SPEC.md §16 (QQ channel)
  - crates/shenko-channel-napcat-link/README.md

- **WebUI (timeline, websocket)**:
  - SPEC.md §21 (Web UI)
  - docs/requirements/requirements.json (R018, R019)
  - docs/requirements/acceptance-cases.json (UI-01~08)

## S2 Self-Dev Loop (已合并 main)
- **TmuxSeat + SSH exec** — 部分实现：TmuxSeat/SeatObservation 在 shenko-exec-ssh，未接 runtime（main.rs TODO §35.4）:
  - SPEC.md §35.5 (三条 seat 路径)
  - SPEC.md §11, §17 (Host execution)
  - docs/requirements/requirements.json (R065, R066)

- **BuildProvider**:
  - SPEC.md §35.3 (独立构建工作区)
  - SPEC.md §35.4 (自开发任务流程)
  - docs/requirements/requirements.json (R051, R070)

- **NapCat IngressSpool**:
  - SPEC.md §35.6 (消息连续性)
  - docs/requirements/requirements.json (R014)

- **NapCat contacts/identity read (napcat.contacts)** — shenko-channel-napcat `ContactQuery` 只读 allowlist + `QueryReceipt`（raw 帧与连接 provenance）；shenko-exec-host `NapCatContactsHandler` 走既有 durable task/evidence 路径，仅 connector 配置时安装:
  - SPEC.md §15.2, §16, §35.6
  - docs/requirements/requirements.json (R001, R059)

- **SelfKnowledgeProvider**:
  - SPEC.md §38 (自解释插件)
  - docs/requirements/requirements.json (R092, R093)
  - docs/requirements/acceptance-cases.json (SELF-01~03)

- **Hook API**:
  - SPEC.md §37 (Hook 兼容与注入顺序)
  - docs/requirements/requirements.json (R086, R087)

- **Effect Contracts**:
  - SPEC.md §36 (插件副作用、重置与恢复边界)
  - docs/requirements/requirements.json (R082, R083)

## S3 (部分实现 / 未开始)
- **WASM runtime (wasmtime 45)** — 部分实现：engine + WASI p1 + home preopen 已接通；VFS/MountSpec 与真实家园未接，checkpoint 仅标记文件:
  - SPEC.md §9, §10 (WASM provider)
  - docs/research/archive/external_review_wasm.md
  - docs/requirements/requirements.json (R028, R088)

- **Memory (proactive, search, decay)** — 部分实现：FTS5（含 LIKE 回退）与 intents 存储已具备；thought loop 已在 S1 后消费标记内容并提交 intent（093b417）；`spawn_worker` 仍无调用者:
  - SPEC.md §12 (会话真相源和记忆)
  - docs/research/archive/research_proactive_memory.md
  - docs/requirements/requirements.json (R039, R040, R041)

- **Compact (Budget + Snap)** — 部分实现：独立 crate（shenko-compact-budget/snap），未注册为插件:
  - SPEC.md §12.3 (历史切窗)
  - SPEC.md §35.3 (首版默认插件闭包)
  - docs/requirements/requirements.json (R050, R055, R056, R057)

- **Self-upgrade handoff** — 部分实现：shenko-update 状态机与一致性备份已具备，未接 runtime:
  - SPEC.md §35.7 (自升级交接)
  - docs/requirements/requirements.json (R071)
  - docs/requirements/acceptance-cases.json (V1-05, V1-06, V1-13)

- **Playground UI (shadcn + config)** — 局部候选源码：不可变版本/精确回滚、同 Store durable Inbox、现有 native Task/Operation/E1/T2 承载 WASI 后端（无第二 action DB）；二进制业务状态与输出保留 CAS 引用。此次尚未执行构建/原生验证，完整 manifest、认证和多 runtime 仍开放:
  - SPEC.md §43 (Conversation config + Playground)
  - docs/research/archive/shenko_u42_u43_ui_config_playground.md
  - docs/development/playground-backend-smoke.md (finite native HTTP + reopen recipe; not an execution receipt)
  - docs/requirements/requirements.json (R096, R097)

- **Transcript revision ingress (continuous media)** — 部分实现：durable
  admission/类型合同/effective 观察与同一主体投影已具备；无真实音频采集、
  ASR/声纹处理器或通话 provider 接线:
  - SPEC.md §15.2, §20 (联系人与声纹、实时通话与课堂旁听)
  - docs/development/media-transcript-ingress.md
  - docs/requirements/requirements.json (R084, R085)

- **Computer Use (full scope)**:
  - SPEC.md §24 (Computer Use 与完整操作系统节点)
  - docs/requirements/requirements.json (R032, R033, R034)

- **REPL (Node/Bun)**:
  - SPEC.md §18 (REPL execution)
  - docs/research/archive/research_repl.md
  - docs/requirements/requirements.json (R035, R036, R037)

- **Android/iOS channel**:
  - SPEC.md §16.2 (Android channel)
  - docs/requirements/requirements.json (R017)

- **Forgetting + Migration**:
  - SPEC.md §27, §28 (遗忘与迁移)
  - docs/research/archive/research_forgetting.md
  - docs/requirements/requirements.json (R058, R076, R077)

## S4/S5 Advanced (未开始)
- **OMP托管 (JSONL RPC)**:
  - SPEC.md §35.5 (seat path 2)
  - docs/research/archive/research_omp_delivery_d2.md
  
- **Claude Code SDK托管**:
  - SPEC.md §35.5 (seat path 3)
  - docs/research/claude-handoff.md

- **Multi-node + remote agent**:
  - SPEC.md §29.4 (S4: 多节点)
  - docs/research/archive/remote_agent_spec.md
  - docs/requirements/requirements.json (R079, R080)

- **Voice (input/output)**:
  - SPEC.md §29.5 (S5: 语音)
  - docs/requirements/requirements.json (R020, R021)

- **Native inference (vLLM/TensorRT)**:
  - SPEC.md §47.2 (PV09 local inference)
  - docs/research/archive/research_native_inference_vllm.md
  - docs/requirements/requirements.json (R062)

## 必读基线（所有实现者）
1. **SPEC.md** (5842 lines) — 权威规格
2. **docs/governance/COMPLETION.md** (63 lines) — 完整交付合同
3. **docs/governance/AUTONOMY.md** (34 lines) — 自主决策边界
4. **docs/requirements/requirements.json** (1609 lines) — 147 项 R 完整分母
5. **docs/requirements/acceptance-cases.json** (3798 lines) — 216 条验收 case
6. **docs/state/coverage.json** (2366 lines) — 当前覆盖状态
7. **.agents/skills/shenko-dev/SKILL.md** — 开发原子与约束

## 验证规则
- 任何 Agent 任务的 prompt 必须显式列出"必读文档"清单
- Agent 必须用 Read 工具完整读取每份文档（不能跳过或假设"已知"）
- 实现后必须在 docs/state/coverage.json 更新对应 R/B/case 的 implementation_refs 和 evidence
- PR 描述必须列出"已读文档"清单和对应章节号

---
Generated: 2025-01-23
Spec version: 0.6.2
