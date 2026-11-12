# Shenko 记忆 — 功能合同（memory.md）

**内核定位。** 参考插件，建立在[内核合同](kernel.md)之上。记忆必须能回到原始事实（[内核 3](kernel.md#kernel-3)）；它向上下文提供材料的路径属于钩子架（[内核 7](kernel.md#kernel-7)）。


<a id="why-5"></a>
<!-- migrated-contract: WHY:5 -->
## 为什么记忆必须能回到原始事实

摘要、向量索引、主动提醒和 compact 都是为了更容易想起关键材料；它们不是唯一真相。原始 session、文件版本、消息、结果与证据需要能按 ID 找回。选择性提醒可以在该想到时帮忙，研究 child 可以从最好结果重新开始，主体的个人经历不因此被清空。见 [§12](memory.md#spec-12)、[§14](research-memory.md#spec-14)。

默认长期保留文字、音频、帧和产物，是因为丢掉的真实经历可能无法再造。空间不足时由主体自主决定归档、迁移或遗忘；先证实新副本存在，再处理唯一副本。升级允许 breaking change，并优先付磁盘成本保留旧副本。见 [§9](self-update.md#spec-9)–[§10](durable-state.md#spec-10)、[§28](../decisions/product-rulings.md#spec-28)、[§41](architecture.md#spec-41)；U20。

<!-- /migrated-contract: WHY:5 -->

适用条款：[§10.3](memory.md#spec-10-3)、[§10.4](durable-state.md#spec-10-4)、[§10.5](memory.md#spec-10-5)、[§12.1](memory.md#spec-12-1)–[§12.4](memory.md#spec-12-4)、[§45.12](memory.md#spec-45-12)；需求 R006、R010、R014、R015、R041、R043、R050、R065、R066、R104、R111、R113。本文是稳定的功能/接口/错误/恢复合同，不是任务板。

## 1. 职责边界

记忆层负责：请求的持久接纳（intent）、提交为可检索记忆条目（entry）、检索（中文子串/FTS + 以匹配为中心的预览）、语义遗忘（tombstone）、真实取代（supersession）、Git 文本知识库投影（[§10.3](memory.md#spec-10-3)）与二进制对象指针（[§10.5](memory.md#spec-10-5)）。SQLite 台账 + CAS 是真相；Git 是可读投影，可重建、可替换，永不做真相裁决。

不负责：原始会话存储（raw session，Ledger/ArchLedger）、受众过滤（[§6](subject-runtime.md#spec-6)/R104：无 audience ACL）、研究 worker 的 bank/intervention（ArchResearch 在同一合同接口上扩展）。

## 2. 数据模型（store/src/memory.rs + migrations）

`memory_entries`：`entry_id, agent_id, kind(开放知识种类), content, source, source_ref, applicability, confidence, scope, record_revision, created_at_ms, updated_commit_seq, deleted(bool tombstone), superseded_by, supersedes, git_commit, git_path`。

- `kind` 是开放知识种类（"note"、"commitment"、"insight"、"reflection"…），**不是**操作。操作种类（save/correct/forget/recall/reflect）只存在于 intent 行（`memory_intents.kind`，常量 `MEMORY_OP_*`）。2026-11-06 起，这个开放字符串保留为可学习的自由标签，只作提示。fact 与 conclusion 不是两种 kind；差别是理由停在何处（[51 认知元概念：内容、态度与理由](cognition-units.md#spec-51)）。默认检索仍直接隐藏旧行，还没有「旧版本数量 + 最新修改时间」的投影，这是实现缺口，不是现行展示规则。
- tombstone 单调：`deleted` 只增不改回为假（语义遗忘是叠加标记，不是删除行）。被取代条目保留原文。
- `record_revision` 在每次写入 +1；`base_revision` 不匹配当前 revision 时是持久冲突，不是静默覆盖。

`memory_intents`：完整请求信封（`MemoryIntentRequest`）——operation、knowledge_kind、scope、content、content_ref(CAS)、entry_id、supersedes_entry_id、applicability、confidence、base_revision、idempotency_key、sources、state(accepted|committed|failed|conflicted)、reason。`idempotency_key` 按 (agent_id, key) 幂等：同 key 重放返回既有行；信封字段不一致是持久冲突。

CAS 定位符跨节点比较使用内容寻址尾段（`<ab>/<cd>/<64hex>`，`portable_locator`）；节点本地路径差异不构成冲突（R113 合并）。

## 3. 操作语义

- **retain/save**：接纳（持久 `accepted` 行 + 内容封入 CAS）→ worker 提交（写入 entry、intent 置 `committed`）。accepted ≠ remembered（[§12.2](memory.md#spec-12-2)）。
- **reflect**：knowledge_kind 默认 "reflection"，必带来源（sources）。
- **correct**：新 entry 的 `supersedes` 指向旧 entry；旧 entry 保留原文、`superseded_by` 回填、`record_revision`+1。拒绝取代已删除/已取代/不存在的条目。`base_revision` 不匹配 → `conflicted` 持久行 + commit_log，两侧证据都保留。
- **forget**：绑定目标 entry；提交时 tombstone 标记。原文通过 `read` 永远可读；`search/list/recall` 默认不返回（`*_including_retired` 返回并带状态）。
- **recall**：无 audience ACL（[§6](subject-runtime.md#spec-6)/R104）。预览以匹配为中心（`MatchExcerpt`：preview、start_char(Unicode 标量偏移)、truncated_before/after、match_visible）；FTS 归一化导致匹配无法字面定位时如实报告不可见，绝不编造区间。

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
- 受众/亲密度过滤：合同层面不存在（[§6](subject-runtime.md#spec-6)/R104）。

<a id="spec-10-3"></a>
<!-- migrated-contract: SPEC:10.3 -->
## 10.3 默认 Git 记忆和私有仓库

首次启动创建宿主 memory 目录，初始化本地 Git，并挂载为 guest 的 /memory。目录的实际位置可配置。factory 恢复重新挂载该目录，不清空它。文件化记忆与知识由 worker 维护，保留出处引用和 schema；SQLite 原始事件独立存放，不将持续变化的数据库二进制每轮提交进 Git。

GitHub/Gitea remote 支持私有认证、多个仓库、read/write scope、pull 冲突和 token 到期。默认不创建公开仓库、不自动推任何远端。配置私库同步后，在授权范围内自动提交与推送；远端可见性变更、认证失败、同步落后要明确可见。SSH key、token 和会话凭据不进入普通 memory Git/snapshot。

二进制附件进入对象存储，Git 保存引用；用户选择 Git LFS 时记录其独立认证、配额与可用性。Git 与对象存储共享版本引用的完整性需要验证，不能成功 push Markdown 却遗漏实际附件。

<!-- /migrated-contract: SPEC:10.3 -->

<a id="spec-10-5"></a>
<!-- migrated-contract: SPEC:10.5 -->
## 10.5 二进制引用、提交和回收协议

采用“Git 文本 + 对象引用”作为建议默认：文本、Markdown、配置直接版本化；大音频、屏幕帧、向量和其他二进制由对象 provider 存储。自定义 pointer 记录格式版本、object/manifest ID、大小、backend namespace 与不含秘密的 key handle；标准 Git LFS 则使用其自己的协议，自定义 BLAKE3 pointer 不自动与 LFS 兼容。具体 hash/分块算法可换，不能把选 BLAKE3 当成已实现持久性或加密。[Git LFS spec](https://github.com/git-lfs/git-lfs/blob/main/docs/spec.md)

提交顺序是持久登记 intent、写完整对象闭包、确认本地持久化，再以预期旧 revision 发布 branch/head 和 receipt。对象在外部文件而 metadata 在 SQLite 时要处理两者间崩溃间隙。发布冲突显式保留候选；LWW 仅适用于明确选择该规则的路径，不能称 Merkle/CAS 天然自动合并。

远端同步先确认对象可取，再发布 Git ref/共享 root。区分 accepted、committed-local、replicated；Git push 成功不代表大文件已同步。凭据过期、payload 缺失、key handle 失效和远端不支持条件更新分别报告。默认本地 Git 仍能离线完整工作，不要求远端仓库作为唯一 durability 来源。

GC 保护活跃分支、保留的 Git 历史、原始会话附件、读者/transfer lease、pending 提交/上传、候选与上一代恢复闭包和 owner pin。基于一致 roots/epoch 标记，删除前处理并发新根；不能只扫当前 main 然后删掉正在提交的对象。失败实验分支可清理，但 research reset 不删原始证据。GC 有界运行，记录回执；满盘时不能先删唯一恢复闭包来腾空间。

SQLite 同步模式、底层 flush 行为和故障类型共同决定实际 durability；WAL、内容寻址和“对象不可变”都不能单独推出断电保证。提交中断、head 更新前后、receipt 发送前后、满盘与并发 GC 属于未来专项验收。[SQLite 原子提交](https://www.sqlite.org/atomiccommit.html)

<!-- /migrated-contract: SPEC:10.5 -->

<a id="spec-12"></a>
<!-- migrated-contract: SPEC:12 -->
# 12 会话真相源和记忆

<!-- /migrated-contract: SPEC:12 -->

<a id="spec-12-1"></a>
<!-- migrated-contract: SPEC:12.1 -->
## 12.1 数据模型与来源

建议宿主 SQLite 保存 append-only 原始事件及必要索引，主要实体为 agent/epoch、session、scope、window、task/attempt、event、tool receipt、operation/outbox、memory intent/revision、object ref、generation。每条事件有稳定 ID、顺序、时间、身份、可见范围和来源；完整大输出/附件在 CAS，事件保存 hash/ref，模型获得有限预览。

SQLite 原始事件是“发生了什么”的证据；Git 文件是明确整理过的知识与记忆，两者互相引用而不混同。summary、FTS、embedding 和 UI 投影可重建。模型猜测、用户陈述、工具观测、已验证事实与过时事实必须能区分；摘要不能将未证实陈述升级为事实。

采用短事务、单写入协调、合理 checkpoint，不让 worker 持有数据库写锁等待推理或网络。SQLite WAL 不放到不满足其共享内存/锁语义的网络文件系统；备份使用一致备份机制，不能只复制主 db 忽略 WAL。依赖要包含已知 WAL-reset 修复，并在开发时核对当前稳定版本。[SQLite WAL](https://sqlite.org/wal.html)、[备份 API](https://sqlite.org/backup.html)、[SQLite 变更记录](https://sqlite.org/changes.html)

<!-- /migrated-contract: SPEC:12.1 -->

<a id="spec-12-2"></a>
<!-- migrated-contract: SPEC:12.2 -->
## 12.2 记忆请求与提交

memory save 接收语义内容、source refs、scope、期望 revision 和幂等键，不接收任意文件路径、脚本或裸 SQL。运行时先持久登记 intent，返回 accepted/pending。memory worker 检索相关项、处理冲突、写文件/选定 backend、提交 Git 或 revision，再返回 committed/failed/conflicted 与出处。

**accepted 不等于 remembered。** 主体和 UI 只有收到 committed 回执才称“已记住”。需要 read-your-writes 时可以返回清楚标记的 pending overlay；任务承诺/权限等关键状态需要等待正式提交，普通补充知识可最终一致。SQLite intent、Git commit 与搜索索引不是一个天然跨系统事务，采用可重放的提交 ledger；崩溃后按 intent ID 找回 commit，补齐 receipt/索引，不重复写一条新记忆。

同一事实被多个 worker 更新时使用 base revision 与来源冲突检查，保留矛盾证据，不以最后写入时间无声覆盖。主体理解冲突并委派核实，不直接修改数据库解决。

<!-- /migrated-contract: SPEC:12.2 -->

<a id="spec-12-4"></a>
<!-- migrated-contract: SPEC:12.4 -->
## 12.4 遗忘、保留和隐私

区分研究 worker 的 context/workspace reset、语义记忆 forget 和真实 erase：reset 改变新尝试看见什么，semantic forget 改变默认回忆，erase 才删除内容。真实删除可来自 owner 指示，也可来自 D8 已授权的主体空间处置决定；前两者不自动删除原始会话、实验档案或外部效果记录。研究 reset 后新 worker 默认不被旧失败叙事重新填满，原始档案仍可按策略独立回查。

D8 已裁决：原始文字、录音和实际采集的屏幕帧默认永久保留，没有隐含 TTL；默认永久并非无限存储承诺。RetentionPolicy/ArchiveProvider 将空间、增长、可重建缓存、候选存储和预计迁移成本作为事件交给主体。主体可自主决定去重/压缩、迁往用户自己的网盘/对象存储/其他节点，或真正遗忘某些内容，不需要逐次询问用户。这些操作由 child/provider 执行，策略本身可继续改写。

| 意图 | 要求 |
| --- | --- |
| semantic_forget | 改变默认 recall/关系判断或标记过时，保留原文；与物理删档分开 |
| archive_move | 认证用户自有目标，传完整对象与 manifest，核对 hash、必要密钥引用并读回验证；可恢复后才释放本地唯一完整副本 |
| erase | 主体选择具体范围与原因，worker 删除原对象、索引、派生摘要和可控历史/备份，记录实际删除与尚不可达副本 |
| restore / merge | 先应用有效删除标记再开放旧快照；不复活已删正文，不用摘要再造后冒充原文 |
| GC | 保留活跃分支、迁移、恢复闭包、未决动作与归档 manifest 的根；不能按对象年龄直接清掉仍有用途的资料 |

真实删除的墓碑只保留必要身份、范围和效果记录，不保留本来要删除的秘密正文。它在断电恢复和分支同步中继续生效；明确重新创建的内容使用新事件。空间压力下可优先清理可重建缓存、再迁移原文，这只是可替换的建议策略，不能成为永远禁止主体选择遗忘的固定规则。Git 历史、备份和远端副本的删除能力分别报告，不能声称删除本地 key 就使所有副本消失。

预警应在耗尽前进入主体，预留必要控制/journal 空间；主体可提前委派无模型时也可执行的压力处置方案。若突发满盘且无可执行方案，记录真实捕获缺口，不静默删掉未被选中的唯一原始资料。长期兴趣与长期资料分开管理，研究 reset/compact 本身不触发 erase。

原始数据保存期限已裁决为默认永久，不再等待固定天数。Agent 的自主真实删除能力不把 compact、研究 reset、回滚或投票落选变成隐式删档；每种操作保留准确语义与可核对结果。

<!-- /migrated-contract: SPEC:12.4 -->

<a id="spec-45-12"></a>
<!-- migrated-contract: SPEC:45.12 -->
## 45.12 个性化适配、反思与自我推翻的插件合同

2026-10-26 用户明确要求个性化与反思插件化，并强调“它要能自我推翻”。本节扩展 [§45.7](cognition-units.md#spec-45-7) 的自主学习，不把特定认知策略、固定反思阶段或自动泛化规则写死在统一 Node 的最小核心里；尚未宣称产品实现或验收完成。相关跨包工作沿用 [#71](https://github.com/cyjin-yl/shenko/issues/71)、C10/C09/C17，不另造重复工作包。

个性化适配、反馈解释、反思与修正方法可以由独立 awareness、Skill 和代码插件组合，实现可替换、可停用、可演进。主体可即时理解和自我质疑，不要求每次先安装一个反思单元。底座提供版本、来源、事实与恢复机制；何时反思、采用何种假设和怎样调整行为由主体及其插件决定。

可复用认知保留情境、适用范围、原始来源、推断与实际证据的区别、版本、反例、置信与未知之处，以及被撤回或由哪个新认知替代的关系（supersession）。一次反馈可能只针对当前任务、某个受众或一个时段；不能自动升级为用户的全局永久偏好。新事实可以促成收窄、分情境、修订或推翻，修订理由可追溯，不靠删除反例把旧判断包装成一直正确。

跨节点同步既传当前认知，也传其证据历史、撤回状态和替代关系。迟到的旧 revision、离线节点回流或从旧备份恢复，不能让已被撤回的结论无声复活为当前规则；真正重新采用应形成带当前依据的新判断。重连时保留差异与不确定性，主体自主解释冲突，不按最新墙钟时间或复制副本数量认定真伪。适用的删除、遗忘与保留合同继续有效；保存修订谱系不创设“用户不能删除”的新原则。

同一观察在多个可信本地模型间传播只能算同一来源；独立环境复现、另取观察与模型复述必须区分。可以共享学到的解决方法，但每节点能力仍按 [§5.4.4](nodes.md#spec-5-4-4) 和 [§24.5](mobile-runtime.md#spec-24-5) 核实；认知传播不复制实际授权或成功运行事实。

<!-- /migrated-contract: SPEC:45.12 -->

<a id="spec-45-12-1"></a>
<!-- migrated-contract: SPEC:45.12.1 -->
### 45.12.1 可判定结果与剩余工作

后续实现应证明：针对局部反馈形成有限适用认知；遇到反例能够自主撤回或替代；离线重连/旧版本回流后仍能识别撤回与 supersession；多个节点转述同一证据不被误计为独立验证；同一插件在不同节点的能力差异如实显示，bundle 后未授权或服务未运行不假报 ready。复用现有任务、记忆、包和同步机制，不为每条认知新建数据库或审批流程。

当前只完成设计表达。认知同步、节点能力发现、各移动 OS/应用后端、root/Shizuku/Dhizuku 集成及 SQLite 实际读取仍需对应源码、环境和结果证据；设计、静态资料核查、构建、真实运行与容灾验收分别记录。

<!-- /migrated-contract: SPEC:45.12.1 -->
