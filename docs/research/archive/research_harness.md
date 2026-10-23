# Harness、历史与本地推理调度研究笔记

研究时间：2026-10-23（Asia/Shanghai）。只读研究；没有安装运行这些项目，也没有性能实测。下列性能与功能若来自项目文档，均为上游声明；设计建议另行标明。

## 1. 已核对的上游快照

| 项目 | 当前查到的主线 commit | committer UTC | 核对结果 |
|---|---|---|---|
| earendil-works/pi（原 badlogic/pi-mono） | 74f46864025c68504cd10c89eeb626b7e3d64cea | 2026-10-22 14:42:51 | 极简可扩展 harness；核心不内置 subagent；扩展、skills、RPC、SDK |
| can1357/oh-my-pi | 0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0 | 2026-10-22 10:22:25 | typed subagent、统一 URI 工具、SSH connection manager、多 memory backend |
| deepseek-ai/deepseek-harness | 687f282e8625c6b8d11ff58211a90524be2e4317 | 2026-10-17 07:22:43 | 0.2.1-rc.2；everything-is-a-plugin，Cordis；仍 developer preview |
| dshbox/cordis-rs | 1754062cef67cfc119fc7a2084c0321a231b9092 | 2026-10-20 01:25:05 | facade 0.11，core/timer/loader 0.6；Rust 1.88，2024 edition；pre-1.0 |
| Neroued/ninfer | 64d10fbf9f8e6bfc79ab0570b94674c5b9997929 | 2026-10-16 05:28:59 | 官方目标单 RTX 5090；1–8 execution lanes；无 priority/QoS、多 GPU/distributed |
| Niko1221/Strata | 011c9e600a93bbc9fd451c563501b5831c32bbca | 2026-10-21 09:07:39 | README app 0.2.2 / engine 0.1.47；OpenAI/Anthropic；默认串行、可 parallel |
| openai/codex | a3d93e1e692f1144cf8685175a5d1f6bf7294f57 | 2026-10-22 17:49:53 | 现行 token-budget compaction 与 history-notes 源码 |

这些是查询时主线，不等于所有读取文件都来自同一原子快照；最终 SPEC 可固定各个已读 blob SHA 或上表 commit 后复核。不要把此表当发布版本保证。

## 2. 值得移植的实际模式

### pi

- Session 以 JSONL 保存，entry 的 id/parentId 形成树；branch 是会话结构的一等对象。
- Compaction 追加 CompactionEntry 和 firstKeptEntryId；原始 entry 仍在文件中，模型上下文是重建视图。
- Branch summary 保留离开的分支的关键结果。ContextEditEntry 可改变未来模型输入而不修改原始历史。
- 扩展可以替换 read/write/edit/bash 的底层 operations。SSH 示例展示此接口，直接复用 Node/OpenSSH 的实现并非 WASM。
- 应移植接口分层和会话模型，不能把 TypeScript/Bun 完整运行环境嵌进“小内核”并声称相同体积。

### oh-my-pi

- 统一资源 URI（ssh://、memory://、agent://、pr:// 等）使 read/search/edit 能跨后端，减少小模型学习一堆不同参数的负担。
- SSH connection manager 重用 ControlMaster，并探测远端 POSIX transfer shell；必须保留 timeout、输出限制、host key 校验与结构化错误。
- Mnemopi 是本地 SQLite memory backend；retain/recall/reflect/memory_edit 可单独发现。其他 memory backend 可通过同一契约接入。
- 首轮 recall 保存成 memory_recall entry，resume/fork 原样回放；记忆后来改变时追加提示而不回写最前面的 block。这对本地 KV 命中极其关键。
- 可借鉴 typed subagent yield、专用工具 surface、结果 schema、可查看/steer/park/终止的 agent hub。
- Hashline 可做编辑模式之一，但不能宣称对所有本地模型最好；应以目标模型错误率/重试 token 数选择。

### DeepSeek Harness / cordis-rs

- DSH 的 model adapter、tool registry、session、agent loop 都可被插件替换。持久 session event 与运行中 agent event 分开。
- session 是 append-only typed log；LLM history 是其 projection；失败/取消 assistant attempt 也有事件，避免伪造成功历史。
- Cordis-rs 提供 typed service/event、明确资源所有权、插件 update/restart/dispose 与按 generation 清理。
- Cordis Scope 是事件路由，ServiceRealm 是服务定位，二者独立；它们不是进程/内存安全边界。
- 设计建议：可信控制面用 cordis-rs 或其契约；可变 agent/plugin 层处于 WASM/进程隔离。救援 supervisor 不应依赖 agent 可卸载的同一插件图，否则“everything is replaceable”会破坏“始终能自救”。

## 3. Codex budget compact：需精确表述

当前公开源码 compact_token_budget.rs 明确跳过 model/server summarization，直接 start_new_context_window；手动/自动路径均走正常 compaction hooks。新窗口具有 first/previous/current window IDs。session/mod.rs 将新的 Compacted、WorldState、TurnContext 追加到 rollout，不能把模型可见 context replacement 等同于删除原始历史。

公开 ext/history-notes 提供 history.list_windows/list_items/read_item/search_contents，以及 notes list/read/search/append/write。这些实现目前依赖 Codex backend 和相应 auth/provider gating，不是可直接用于任意本地 API 的离线历史系统。

本产品可本地复现的语义：历史原文可寻址；切窗口保留任务/承诺/工具 receipt/未完成子任务；按需读取旧窗口和笔记。提供两种可替换 ContextPolicy：普通 summary+recent 与 budget-window+history-recall。弱模型是否能可靠地自行找回任务，必须用目标模型的长任务回归测试决定默认，不能只因为 Codex 有就默认同等效果。

## 4. 推理 broker：应是有资源预算的调度器

以下为设计建议，而非从某上游复制的现成实现。

### 资源身份与 hard limits

- BackendInstance 对应一个真实 server 启动实例；ResourceGroup 对应实际共享的 GPU/内存/吞吐预算。多个 URL、API 协议和 model alias 不得各建独立 semaphore 绕过总上限。
- 三类上限独立：活跃 agent/task 数、正在执行的 LLM 请求数、驻留 KV/状态预算。可有几十个 parked agent，而同时 LLM 请求只有 1–2 个。
- 所有主/子 agent、advisor、compaction、memory consolidation 与共享设备的 embedding/rerank 都经 broker，不得有隐藏直连调用。
- 所谓全局只能覆盖经过此 broker 的消费者。若其它应用直连同一推理服务，需它们也经代理或预留容量；无法声称监控 HTTP 客户端就获得了物理 GPU 全局互斥。
- 每请求必须验算 rendered input + reserved output + tool overhead <= 有效 context window；视觉 token 不能按字符数估计。使用 backend tokenizer/count endpoint 或固定模板和 tokenizer 的可信估算。
- 控制 admission 的不仅是请求数，还包括未缓存 prefill tokens、KV/state 空间、prefill chunk/时间预算。混合模型不只 KV，还可能有 recurrence/PLE/StateImage，按后端能力报告。

### 无死锁和取消

- inference permit 只覆盖实际模型请求；父 agent 发出工具调用后，在等待子任务/SSH/搜索/人工回复前释放。
- 等待 children 时可以有 cache lease，但不能持有 inference permit。递归分叉遵循同一规则。
- cancel 待确认后才回收 capacity；连接断开不等于后端已释放资源。进入 cancellation-pending/suspect 状态，直到 ack、结束事件或可靠后端观测，防止幽灵请求和超额发放。
- 一次远端请求不能持有文件系统写事务锁等待 LLM；跨资源不要“先占半个再等半个”。
- 可在单 broker 内做到严格 inflight 上限；多节点 broker 需要 lease/fencing/单调 generation，作为后续部署 profile，不能只用分布式锁一词带过。

### 优先级与收敛

- 系统确定 priority；群聊文本中的“紧急”不是提升任意用户到管理员/最高队列的权限。
- 建议 foreground 人类交互、已承诺到期任务、后台研究、自维护分级；同级做公平队列，aging 防饥饿，cache affinity 仅作有界 tie-break，避免为了缓存饿死新会话。
- C=1 时“保留一个紧急 slot”意味着所有背景任务永久不能运行，不能这么设计。紧急请求只能等当前安全 yield，或在后端支持时 cancel/preempt；界面必须显示真实等待原因。
- background 单步 output quantum 限制可减少抢占损失；这是可配策略，不应把硬件上限以外的产品禁令强加给 owner。
- 递归研究维护 task DAG、query/source dedup、evidence ledger、未解决问题清单；分叉前记录 hypothesis 与 stop condition；给最终综合预留预算。
- 收敛由“验收目标覆盖 + 新证据边际收益 + 当前预算/期限”共同决定。上限可配置，不能以每个 parent 各自 cap 伪装总 cap。

## 5. KV / continuation capability 矩阵

| 后端 | 可以真实承诺 | 不可以默认承诺 |
|---|---|---|
| generic OpenAI-compatible | 正常全量 history、连接取消尝试、配置并发 | 统一 slot/session/pin/fork/save API；prompt_cache_key 有效 |
| llama.cpp | 显式 id_slot、cache_prompt，以及 /slots/{id}?action=save/restore/erase；版本化 adapter | 任意 slot 与自动 RAM prompt cache 总是兼容；必须测具体版本，已有上游 pin/cache bug |
| vLLM | 自动 prefix blocks；cache_salt 信任域隔离；启用 priority scheduling 才可用非零 priority | OpenAI schema 里的稳定 KV slot；任意跨 backend 迁移缓存 |
| SGLang + Dynamo | 实验 nvext.session_control bind/open/close、session_id、timeout；worker/rank affinity；streaming session KV 生命周期 | 所有 SGLang部署自动支持；任意 branch/rewind；vLLM/TRT 同等支持 |
| NInfer | exact-prefix reuse；Responses local previous_response_id 继承 engine-local session key；cached token usage；resource-pressure pause | prompt_cache_key 就是 session id；公开 slot pin API；engine priority/QoS；官方支持 4090/V100 |
| Strata | exact-prefix 匹配复用、可选 conversation parking；parallel slots 留 conversation；单 slot0 save/restore | 任意 id_slot 透传；并行状态下 save/restore；Responses server-side previous_response_id |

KV handle 必须含 backend instance epoch、model/tokenizer/template/toolset digest、trust namespace、lease/expiry；后端重启或模型更新后失效可正常全量重建，不能把缓存当唯一历史。

稳定前缀策略：稳定 system、工具 schema 顺序与序列化、memory snapshot；把时间/队列状态/最新检索证据尽量放在增长后缀。分支共享 immutable history prefix，child-specific 任务接在后部。频繁 compact、重排 tools、重写早期消息都有 prefill 代价，要在 UI 看到 cached/uncached tokens 与 TTFT。

## 6. Dynamo streaming-session 细节

官方文档将该接口标为 experimental。bind 是 router-only sticky routing；open/close 涉及 worker streaming sessions，需 --enable-streaming-session（文档最低 SGLang 0.5.11）和 KV router mode。首轮可共享 lead agent prefix，后续持有独立 SessionSlot；pin KV 不参与普通 eviction，关闭时释放。

限制：只支持 append，不能 replace/offset rewind/drop_previous_output。idle timeout 每请求刷新；若工具调用超过 TTL，slot 被清理，必须重新 open + prefill。close 是一次 generation 完成后触发的 close RPC；独立中止/进程死亡需要 timeout 或生命周期清理。应做单独 pinned-KV 上限、保活策略、终止清理与失效重建测试。无需把整个 Dynamo 集群作为轻量 bot 强依赖，只借鉴能力契约并选装 adapter。

## 7. 历史、记忆和 FTS5

- SQLite append-only events 是权威原文；FTS5、向量、summary、memory facts 都是派生层。把 SQLite 文件直接当 Git 文本提交会产生二进制 churn，Git 默认 memory 路径应存 Markdown/JSON 文档和事件引用；DB 通过一致备份归档。
- 最少结构：space_id、session_id、branch_id、window_id、agent_id、sequence、role、timestamp、source URI/hash、tool call/result、payload_ref、visibility、tombstone。
- 全量 tool outputs 与二进制附件放 CAS，以事件引用；模型收到 bounded preview+read URI，不能为了压上下文丢掉真相源。
- FTS5 external-content table 可减少内容重复，但索引一致性是应用责任，应同事务更新或可重建。
- unicode61 以连续字母数字为 token，不自动中文词切分；trigram 的 full-text 查询不能匹配少于三个 Unicode 字符。因此“北航”“衢州”这类两字查词必须有自定义中文分词/双字索引或受限 substring fallback。
- 先 scope/权限过滤，再检索/rerank；group memory、个人 DM、agent 私有工作笔记不能仅靠提示词隔离。跨空间合成的新摘要也需明确可见性。
- recall 返回出处、时间、置信状态和是否已被更新；新事实覆盖旧事实用 supersedes/tombstone，不把总结当证据。
- FTS/embedding可损坏并重建，原始事件不能随沙箱 rootfs 回滚。删除需要覆盖索引、原文、备份/Git历史的保留策略，用户要决定严格忘记还是审计可恢复。

## 8. 建议的可插拔接口（语义清单，不是开发代码）

| 接口 | 关键操作/字段 |
|---|---|
| ModelAdapter | capabilities、tokenize/count、generate(stream)、cancel、usage、health；backend extension 仅 allowlisted 传递 |
| InferenceBroker | submit、cancel、status、lease；resource_group、priority_class、deadline、input/output budget、affinity |
| ContextPolicy | project、estimate、compact/new_window、fork、read_history；必须返回 source ranges |
| HistoryStore | append、get、list_windows/items、search、fork、export；原始事件写权限不交给任意 memory 插件 |
| MemoryBackend | retain、recall、reflect、update/invalidate/forget、flush、health；scope、provenance、budget |
| ResourceProvider | stat/list/read/write_atomic/patch/search/watch；file/ssh/git/history/memory/blob 等 URI |
| ExecutionProvider | exec(argv,cwd,env,stdin,limits)、stream/poll、signal、cancel；local WASM/SSH/container/node |
| CredentialProvider | create_key、public_key/fingerprint、sign/use credential、rotate/revoke；secret reference 而非 secret 值 |
| SubagentService | spawn/fork、send、wait、park/resume、cancel、typed_yield；父子 lineage 与预算账户 |
| BranchWorkspace | fork_snapshot、diff、checkpoint、merge/promote、discard；不自动回放不可逆外部动作 |
| Scheduler | durable trigger、claim、heartbeat、backoff、dedup、receipt；保证重启不会重复主动发消息 |

SSH 文件读写用 URI adapter，命令用结构化 exec；POSIX shell 字符串与 argv 明确区分。pi 的 ssh.ts 是示例，里面 JSON.stringify shell path 的片段不能作为正式 shell escaping 契约直接复制。

## 9. Ninfer / Strata 双语教程应交付的实质内容

每个教程均中英文同等完整，明确测试版本/commit、model/tokenizer hash、OS/GPU/RAM、配置来源、能力矩阵、可复现实测命令与实际结果。

### NInfer / NInfer integration

1. 支持边界 / Supported hardware：主线仅 RTX 5090、x86-64 Linux；其它卡 fork 单独标注、不得套用官方数字。
2. 构建与 artifacts / Build and model artifacts：官方依赖、v3 .ninfer，固定版本；此阶段仅写教程需求，未执行。
3. 服务配置 / Serve profile：max-context、kv-capacity、max-concurrency、kv-dtype、device-state-slots、host-context-mib、preserve-thinking、model-id、api-key。
4. Bot 接入 / Bot connection：/v1 URL，model alias，resource_group，C <= max-concurrency，token counter，JSON schema tool calling，streaming/cancel。
5. 历史与缓存 / Continuation：Chat Completions full transcript；Responses previous_response_id 可选，fallback local log；不编造 slot ID。
6. 诊断 / Diagnostics：health、metrics、cached_tokens、TTFT、queue wait、Host retention、真实 context overflow 和 OOM；说明逻辑 context ceiling != 总 KV 池。

### Strata / Strata integration

1. 安装/engine version/模型/hardware 按官方当时支持矩阵。
2. strata launch 的 context、parallel、vision、API key/LAN access；/v1 Chat Completions 与 /v1/messages、/v1/responses 路由差别。
3. 从 parallel=1 基准开始；Bot broker C 与实际 engine slots 一致；检查 engine 可能自动降低 slots 的日志/metadata。
4. conversation-cache-mib/min-token、prompt-cache、稳定工具前缀、preserve reasoning；短 side requests 污染缓存的调参。
5. 单 slot save/restore 是独立可选实验；parallel 模式不可用需准确说明。
6. 两会话交替、长工具等待、后台研究+前台消息、断连取消、重启失去 KV 的测试；记录 first/cached TTFT、uncached prompt tokens、吞吐和内存。

## 10. 给用户的关键规格追问

1. “全自主”是否包括自行安装不受信任插件/修改 system instructions/更改 inference limits/新增 SSH target，哪些由 owner 一次配置能力，哪些可自决？默认全允许对话并不决定这些权限。
2. 初期硬件基线究竟是哪台：路由器/Android 只跑 controller，还是必须同机 inference？其 RAM/存储/idle 功耗上限决定能否内置重 runtime/embedding。
3. Ninfer/Strata 是否就是已找到两个仓库？是否接受官方 NInfer 5090 profile 与社区其它卡 profile 分开写？
4. API 独占还是也被其它应用直连？若共享，愿否统一走 broker？否则无法给严格全局最大并发保证。
5. 紧急前台到来时，可否中止后台 generation 丢掉部分推理/缓存，还是必须等自然 yield？C=1 下有直接体验差异。
6. 对旧会话的“记得”：默认保留全部原文多久？严密删除是否也要清 Git/备份，还是以可追溯审计优先？
7. 普通聊天者能否使用 SSH/私人 memory/安装工具？建议能力按 person/group/workspace，不跟“能聊天”绑定。
8. Context 默认用 summary+recent、window-reset+recall，还是按模型能力自动选？小模型召回失败时谁提供最低任务摘要？
9. subagent 是共享人格知识但隔离任务 workspace，还是可互相读一切历史？权限影响 cache_salt 与 memory scope。
10. 是否接受 v1 提供通用兼容 + llama.cpp/NInfer/Strata adapters，而 pinned streaming KV 作为可选 SGLang/Dynamo adapter；不把所有后端强行伪装同一 slot API？

## 11. SPEC 验收建议

- C=1 下父任务递归 spawn 三级，所有孩子与最终合成都能完成，无 permit deadlock。
- 主/子/advisor/compaction/memory 同时提交，同一 resource_group 的 upstream active requests 永不超配置；被取消但后端未停的请求计入占用。
- 两不同 URL 指向同 engine，不得双倍放行；backend restart 后老 KV handle 被拒绝并正常重建。
- 100K-prefix 两会话交错测试明确记录 cached tokens/TTFT；缓存失效不丢任何历史和任务。
- 任意 tool/agent/插件 unload 后没有孤儿请求、定时器、FS handle、SSH session 或 pinned KV lease。
- 数据库故障/索引重建不会制造“记忆事实”；compaction 后能从原文找到随机选取的用户要求、工具失败和来源。
- 中文 1/2/3 字、人名、英文符号、代码标识符、时间范围和群聊权限回归。
- Engine 只提供普通 OpenAI-compatible 时，其界面应显示“缓存能力未知/未暴露”，而非假显示 pinned slot。

## 12. 一手来源与网页 refs

- Pi README https://github.com/earendil-works/pi/tree/main/packages/coding-agent （turn18view5 根仓库）
- Pi session format https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/session-format.md （turn39view3）
- Pi compaction https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/compaction.md （turn39view4）
- Pi SSH operations example https://github.com/earendil-works/pi/blob/main/packages/coding-agent/examples/extensions/ssh.ts （GitHub connector 已读）
- OMP README https://github.com/can1357/oh-my-pi （turn18view6）
- OMP memory replay https://github.com/can1357/oh-my-pi/blob/main/docs/mnemosyne-memory-backend.md （turn39view2）
- OMP SSH manager https://github.com/can1357/oh-my-pi/blob/main/packages/coding-agent/src/ssh/connection-manager.ts （GitHub connector 已读）
- DSH architecture https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/architecture.md （turn18view3）
- DSH session https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/subsystems/session.md （turn39view5）
- cordis-rs https://github.com/dshbox/cordis-rs （turn18view4）
- NInfer README https://github.com/Neroued/ninfer/blob/master/README.md （GitHub connector 已读）
- NInfer serving https://github.com/Neroued/ninfer/blob/master/docs/serving.md （turn18view0）
- Strata batches https://github.com/Niko1221/Strata/blob/main/docs/BATCHING.md （turn18view1）
- Strata detail https://github.com/Niko1221/Strata/blob/main/docs/DETAILS.md （turn18view2）
- llama.cpp server https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md （turn15search0 / turn32view3）
- llama.cpp explicit slot bug evidence https://github.com/ggml-org/llama.cpp/issues/28139 （turn15search18），issue 是报告，未独立复现，不能当 current bug 已证实。
- vLLM prefix/cache_salt https://docs.vllm.ai/en/stable/design/prefix_caching/ （turn23view4 / turn32view2）
- vLLM request priority source https://github.com/vllm-project/vllm/blob/main/vllm/entrypoints/openai/chat_completion/protocol.py （GitHub connector 已读，393–402 行附近）
- Dynamo SGLang agentic https://docs.nvidia.com/dynamo/latest/knowledge-base/modular-components/backends/sg-lang/agents-on-sg-lang （turn30view0）
- SQLite FTS5 https://sqlite.org/fts5.html （turn23view0 / turn32view0 / turn32view1）
- Codex compact https://github.com/openai/codex/blob/main/codex-rs/core/src/compact_token_budget.rs （turn39view0）
- Codex history tools https://github.com/openai/codex/blob/main/codex-rs/ext/history-notes/src/tools.rs （turn39view1）
- Codex backend/gating https://github.com/openai/codex/blob/main/codex-rs/ext/history-notes/src/extension.rs 、backend.rs （GitHub connector 已读）
- Codex rollover persistence https://github.com/openai/codex/blob/main/codex-rs/core/src/session/mod.rs （GitHub connector 已读）

父 agent 引用我的 web ref 前须自行 open/find；每网页标注 wordlim 通常 200，勿在最终文档大量转述同一来源而超限。整合设计部分应清楚标为建议/推导。
