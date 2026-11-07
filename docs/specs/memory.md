# Shenko 记忆 — 功能合同（memory.md）

适用条款：SPEC §10.3/§10.4/§10.5/§12.1–12.4/§45.12；需求 R006、R010、R014、R015、R041、R043、R050、R065、R066、R104、R111、R113。本文是稳定的功能/接口/错误/恢复合同，不是任务板。

## 1. 职责边界

记忆层负责：请求的持久接纳（intent）、提交为可检索记忆条目（entry）、检索（中文子串/FTS + 以匹配为中心的预览）、语义遗忘（tombstone）、真实取代（supersession）、Git 文本知识库投影（§10.3）与二进制对象指针（§10.5）。SQLite 台账 + CAS 是真相；Git 是可读投影，可重建、可替换，永不做真相裁决。

不负责：原始会话存储（raw session，Ledger/ArchLedger）、受众过滤（§6/R104：无 audience ACL）、研究 worker 的 bank/intervention（ArchResearch 在同一合同接口上扩展）。

## 2. 数据模型（store/src/memory.rs + migrations）

`memory_entries`：`entry_id, agent_id, kind(开放知识种类), content, source, source_ref, applicability, confidence, scope, record_revision, created_at_ms, updated_commit_seq, deleted(bool tombstone), superseded_by, supersedes, git_commit, git_path`。

- `kind` 是开放知识种类（"note"、"commitment"、"insight"、"reflection"…），**不是**操作。操作种类（save/correct/forget/recall/reflect）只存在于 intent 行（`memory_intents.kind`，常量 `MEMORY_OP_*`）。2026-11-06 起，这个开放字符串保留为可学习的自由标签，只作提示。fact 与 conclusion 不是两种 kind；差别是理由停在何处（[SPEC §51](../../SPEC.md#51-认知元概念内容态度与理由)）。默认检索仍直接隐藏旧行，还没有「旧版本数量 + 最新修改时间」的投影，这是实现缺口，不是现行展示规则。
- tombstone 单调：`deleted` 只增不改回为假（语义遗忘是叠加标记，不是删除行）。被取代条目保留原文。
- `record_revision` 在每次写入 +1；`base_revision` 不匹配当前 revision 时是持久冲突，不是静默覆盖。

`memory_intents`：完整请求信封（`MemoryIntentRequest`）——operation、knowledge_kind、scope、content、content_ref(CAS)、entry_id、supersedes_entry_id、applicability、confidence、base_revision、idempotency_key、sources、state(accepted|committed|failed|conflicted)、reason。`idempotency_key` 按 (agent_id, key) 幂等：同 key 重放返回既有行；信封字段不一致是持久冲突。

CAS 定位符跨节点比较使用内容寻址尾段（`<ab>/<cd>/<64hex>`，`portable_locator`）；节点本地路径差异不构成冲突（R113 合并）。

## 3. 操作语义

- **retain/save**：接纳（持久 `accepted` 行 + 内容封入 CAS）→ worker 提交（写入 entry、intent 置 `committed`）。accepted ≠ remembered（§12.2）。
- **reflect**：knowledge_kind 默认 "reflection"，必带来源（sources）。
- **correct**：新 entry 的 `supersedes` 指向旧 entry；旧 entry 保留原文、`superseded_by` 回填、`record_revision`+1。拒绝取代已删除/已取代/不存在的条目。`base_revision` 不匹配 → `conflicted` 持久行 + commit_log，两侧证据都保留。
- **forget**：绑定目标 entry；提交时 tombstone 标记。原文通过 `read` 永远可读；`search/list/recall` 默认不返回（`*_including_retired` 返回并带状态）。
- **recall**：无 audience ACL（§6/R104）。预览以匹配为中心（`MatchExcerpt`：preview、start_char(Unicode 标量偏移)、truncated_before/after、match_visible）；FTS 归一化导致匹配无法字面定位时如实报告不可见，绝不编造区间。

主体自存路径（`settle_subject_memory`）是刻意的不带 sidecar 表示，不算损坏；认知对账（cognition.rs）将其排除在绑定缺口之外。

## 4. 接口

Rust（`shenko-memory`）：
- `MemoryBackend` trait：`retain/reflect/correct/forget/recall/read/lineage/flush/health`。可替换后端（R010：QMD/OpenViking/Hindsight 等是同一合同的其它实现）。
- `StoreMemoryBackend`：台账真相实现；`with_knowledge(git)` 附加 Git 投影。
- `MemoryRequest`（完整信封）、`RecallQuery`（无受众）、`MemoryHit`/`MatchExcerpt`、`RetainOutcome`/`ForgetOutcome`/`MemoryHealth`/`FlushOutcome`。
- `MemoryWorker` + `MemoryWorkerPlugin`（Cordis）：常驻 drain，间隔 5s，token 关闭即停；不阻塞主体循环，不持台账写锁。
- `GitKnowledgeStore`：`open(data_dir/memory)`（缺 `.git` 时初始化，恢复复制来的数据目录同样成立）、`sync_entry`、`sync_pending`、`record_pointer`、`health`。

原生 child（shenko-exec-host/src/memory_task.rs）：
- `memory.query/v1`，只读、仅本主体（task.agent_id 必须是活体主体分支）、有界。op：
  - `search`：`{"op":"search","query":"…","knowledge_kinds":[],"scopes":[],"include_retired":false,"limit":20}` → hits（含 excerpt + supersession_status）。
  - `read`：`{"op":"read","entry_id":"…"}` → 全文（含已遗忘原文）。
  - `lineage`：`{"op":"lineage","entry_id":"…"}` → 取代链。
- 观测经 `record_external_evidence` 密封原始结果；规范化超 480KiB 时以 `normalized_result_too_large` 显式失败，不截断冒充。

## 5. 运行时接线（bin/shenko）

启动时：注册 `MemoryHandler`（NativeTaskHandlerPlugin，prompt section 自动呈现给模型）；以 `data_dir/memory` 打开 `GitKnowledgeStore`；spawn `MemoryWorkerPlugin`。模型经 `[delegate:memory.query:1]` 使用；worker 处理图书馆侧（`pending_library_memory_intents`，排除主体自有 intent）的异步提交。

## 6. 错误与恢复

- 冲突（base_revision/信封不一致/idempotency 重放字段不一致）：持久 `conflicted` 行 + commit_log；无写入发生；双方证据保留供主体裁决。
- 处理失败：intent 行 `failed` + reason；重放返回 `AlreadyProcessed`，不重复副作用。
- Git 同步失败：只记日志，永不回滚台账提交、永不伪造 Git ref；下次 `sync_pending` 重试。
- 崩溃恢复：`process_intent` 以 `memory_intents` 持久状态为准重建 in-process 队列（`process_all` 同时 drain 队列与 durable pending）。
- 跨节点归档导入：定位符比较按内容尾段，相同的 bytes 不产生冲突。

## 7. 显式未覆盖

- 研究 worker 的 MemoryInterventionPolicy/bank（ArchResearch，在同一 `MemoryBackend` 合同上实现）。
- 原始会话历史检索（`history.query`，既有）。
- 受众/亲密度过滤：合同层面不存在（§6/R104）。
