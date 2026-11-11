# 推理 broker 与物理资源合同


<a id="why-8"></a>
<!-- migrated-contract: WHY:8 -->
## 为什么推理后端要真实透传

Execution lane、KV 页、host backing、StateImage、GPU 驻留、parking、replay 和并行 decode 不等价。不同后端可以提供不同的低层能力，抽象层要保留这些差异。固定版本的 NInfer 与 Strata 文档已经说明不能承诺任意时刻、任意边界的无损抢占。对本地模型，重复 prefill 与丢缓存可能决定产品是否实用。见 [§13](inference.md#spec-13)、[§25](inference.md#spec-25)–[§27](verification.md#spec-27)、[§41](architecture.md#spec-41)；REF-NINFER、REF-STRATA。

API 版本和 SDK 更新也会改变真实能力。Claude API 2026-10-21 的采样限制只作用于相应绑定；SDK 的 Bearer 默认、类型与方法变化另行跟踪。版本化的发送投影让原始历史保持可查，让主体知道哪些能力变了并自主决定迁移。见 [§47](inference.md#spec-47)；U53。

真实占用也不能因取消请求、网络断线、换代理或切换 alias 而凭空释放。已受控的所有模型调用，包括 memory、评判和 children，都要纳入实际资源域。公平调度与主体可选的免打扰使紧急消息和孩子的工作都能继续，主流可以按体验调整策略。

<!-- /migrated-contract: WHY:8 -->

本文是 `shenko-broker` 的功能合同：受控推理如何进入同一入口、provider 如何以开放装配注册、物理资源域与 KV 池如何核算、流式输出如何被真实分批与公开、以及错误与未知结果如何成为可查事实。本文描述**接口与行为**，不替代 [SPEC](../INDEX.md) 的原文要求；文中标为"已实现"的部分指本仓库当前源码，标为"未验证"的部分明确缺少运行证据。

## 为什么是同一个入口

主体、child、记忆、压缩、诊断、发现都属于"受控消费者"。它们共享同一台机器的同一批物理资源，因此必须进入同一个 broker。绕过 broker 的插件调用会拿到第二份预算，"全局并发上限"就只是本进程的局部事实。

- 入口：`BrokerHandle::submit` / `submit_recorded` / `submit_purpose` / `submit_recorded_purpose` / `submit_stream`。
- 目的分类：`RequestPurpose`（Subject、Child、Memory、Compact、Diagnostics、Discovery、External）。`submit`/`submit_recorded` 从 `InferencePriority` 推导目的，优先级本身不变。
- 未受管客户端：外部直接连同一引擎的客户端不在本产品的控制内。物理资源域记录 `ExternalClientUsage`：可强制的预留从总额扣除，不可强制的只把保证降为软约束（`ResourceDomain::hard_guarantee_available`）。

## 开放 provider 装配

provider 是可安装的运行时实现，不是封闭枚举。

- `ProviderRuntime`（`shenko_broker::registry`）：`profile()`、`binding()`、`complete()`、`stream()`、`count_tokens()`、`probe()`，以及 `available()`（离线绑定返回 `false`，broker 给出 typed `Unavailable`，不伪造成功）。
- `ProviderRegistry` 是 Cordis 服务（`shenko/provider-registry`），由 `ProviderRegistryHost` 发布、`ProviderInstaller` 按 Fiber 生命周期安装与撤销。安装代用 epoch 保护：旧一代的迟到清理不能撤销新一代的注册。
- `ProviderBinding::Profile { profile }` 是开放的封口形态。`ProviderProfile` 冻结：profile_id、revision、backend_family、transport、endpoint_origin、model_id、api_version、sdk、request/response schema revision、gateway、beta、sampling 能力、auth 绑定、usage 字段映射、native 命名空间、resource_domain_id、engine_incarnation、default_max_tokens、serializer_revision。
  - 冻结的是**解析后的实际身份**，不是可变的 profile 查找：封口之后 provider 改了配置，也不能改变这次请求的解释。
  - 历史 `local` / `anthropic` / `stub` 绑定原样可读，旧封存历史不需要迁移。
- 模型名不构成许可名单。`ProviderProfile::validate` 只检查结构（端点 scheme、凭据头名、采样能力自相矛盾等），不检查模型是否"已知"。

### 四个原生后端

NInfer、Strata、vLLM、1Cat-vLLM 各自有 profile 构造器（`profile::native_backends::*`）与原生命名空间（`native.ninfer`、`native.strata`、`native.vllm`、`native.1cat`）。命名空间本身不是能力保证：`BackendCapabilitySnapshot` 里的每个 operation 只有在**探测响应里被后端自己声明**时才是 `Available`，否则是带原因的 `Unknown`。1Cat 的 `/v1/sm70/acceleration` 诊断响应与 `/v1/models` 元数据都作为原始证据保留。

- vLLM：`cache_salt`、`priority`、`kv_transfer_params` 是请求级原生字段；所查 Rust HTTP generate 路由不携带 `kv_hints`，因此该 operation 在 HTTP 传输上报告为不支持或未知，而不是"已透传"。
- 1Cat：未知字段可能被接受后忽略；HTTP 200 不等于参数生效。cache-read usage 无数据时保持 unknown。
- NInfer：`prompt_cache_key` 是缓存提示，不是 Engine session 或 slot 句柄。
- Strata：Host conversation parking 是主机内存，不是 GPU 硬驻留。

## 物理资源域

`ResourceDomain` 是一台物理引擎/设备池，持有计算预算 C、上下文驻留池 K 与被隔离的未知欠账。

- 身份由部署给出：`ResourceDomainIdentity { domain_id, engine_identity, engine_incarnation, epoch }`。URL 不是身份：同一 resident engine 的多个 URL、模型别名、Chat/Responses/Anthropic 入口必须绑定同一 `domain_id`（`ResourceDomainRegistry::bind_alias`）。
- 显式别名优先于 profile 自带的 `resource_domain_id`，所以第二个 URL 拿不到第二份满额预算。
- `engine_incarnation` 在相同 `engine_identity` 范围内唯一（供应商范围内的字符串，不是全局身份）：同一范围内把它声明给第二个 domain 会在 `register` / `resolve` / `observe_incarnation` 处被拒绝（"engine incarnation … is already bound to resource domain …"），并由 `incarnation_conflicts()` 报告。
- 未知结果被隔离而不是释放：`ResourceDomain::quarantine` 记录欠账；被隔离的额度仍算在途，`effective_compute_capacity` 从可用预算里扣掉在途额度与可强制的外部预留，而这个数值就是 `ResourceDomain::try_admit` 在真实派发路径上执行的同一预算（`compute_budget()` 给出分解）；只有原始终止证据才能通过 `reconcile_recorded` 归还。

## 计算与驻留分开

`AdmissionController` 只管 C（同时执行的 generation 数）；`KvPoolState` 只管 K（可保留的上下文驻留）。C=1 不意味着只有一个上下文。

- `ContextLease`：`side`（Main/Child/External）、`state`、`required_units`、`reusable_units`、`recompute_units`、`return_probability`、`priority`、`retention { guarantee, entitled_units, used_units, borrowed_units, reclaim_pending_units }`。
- 份额是算法的结果，不是输入。`AutomaticKvPolicy` 枚举候选（按需求比例、对称、相邻份额、保持当前），按加权代价（未覆盖 prefill/replay + 闲置份额 + 迁移代价）选择，并有滞回与最短保持时间。对称负载下"按需求比例"候选本身就是均分，因此 50/50 是涌现结果而不是写死的 0.5；负载变化后份额随之偏移。
- 只有后端确实提供保留/配额控制时，lease 才能声明 `Reserved`；否则是 `BestEffort`/`Advisory`。容量未被观测时 `capacity_units` 保持 `None`，不改成 0，也不把估算当保证。
- 份额数据来自**后端实际报告的输入 token**（`observe_kv`），不是字符估算。上下文窗口口径由 `shenko-context-policy` 拥有，broker 不做第二份估算。

## 真实流式

流从 provider 自己的事件流开始，不是把 S1 拆块。

- 解析：`parse_anthropic_frame`（Messages SSE）与 `parse_openai_frame`（OpenAI-compatible chunk）。
- 公开与内部分开：`StreamEvent::visibility()`；`ReasoningDelta` 是内部事件，既不发布也不写入日志。工具调用的部分参数同样是内部事件。
- 控制语法闸门：`ControlGate` 按主体输出合同的同一列规则判断——行首 `[` 或 `dispatch:` 的行在整行到齐前被扣留，到齐后按同一规则判定；控制行不进入公开流与日志，原始 provider 证据仍完整留给主体解析。散文行即时发布。
- 有界分批：`BoundedStreamPublisher` 以 100 ms 或 4 KiB 序列化字节为界；单条超大文本事件按字符边界切分，绝不切断 UTF-8。
- 终局回执独立于调用方：`publisher.finish()` 写终局单元并关闭流，无论主体是否还在等待、是否被取消。`submit_stream` 没有持久日志实现时直接拒绝，不静默丢内容。

## 版本化的 Claude 规则

`anthropic_rules` 按 API 版本给出规则集：`AnthropicRuleSet::for_api_version`。2026-10-21 起，采样键 `temperature`/`top_p`/`top_k` 被禁止、user 消息不得带非空 citations、assistant 消息不得直接带 document；旧版本保留旧规则。规则同时作用于普通请求、流式、token counting 与 Batch。

- 消息形态扩展：`InferencePart::Document` / `Citation` / `SearchResult`，各自带 `citations_enabled` 与来源信息。
- 鉴权按绑定作用域：`AuthBinding { method, header, credential_fingerprint }`；`ApiKeyHeader`（`x-api-key`）与 `Bearer`（新 SDK 的默认）都在同一绑定上决定，普通、stream、count、batch 四条路径使用同一身份。凭据值从不进入 profile、prompt、回执或日志。
- 原生选项不被静默丢弃：与封口信封字段冲突或被版本禁止的键是明确错误，其余未知键原样透传，由能力快照记录后端是否真的生效。
- provider 返回的 citations 保留在 `InferenceResponse::citations` 中。

## 错误与未知结果

- 失败分类：`InferenceStatus`（Success、Failed、Cancelled、QueuedCapacityWait、Unavailable、Rejected{status}、InvalidRequest{code}）与 `TerminationKind`（ProviderTerminal、TransportRejected、NeverDispatched、Unknown）。
- 普通与流式结果的 `TerminationEvidence.max_tokens` 绑定本次封口请求实际采用的输出预算：优先使用 payload 的显式值，仅在未指定时使用绑定的默认值。它不是 provider 上限，也不能以默认值覆盖已发送的显式预算；`output_tokens` 仍只取 provider 回报。
- `WireErrorCode` 增加了 `MessageRuleViolation`（`PROVIDER_MESSAGE_RULE_VIOLATION`），承载版本化消息规则的拒绝。
- broker 无法继续或必须降级的错误会进入**拥有该请求的主体**的运行时错误日志：`RuntimeErrorSink::journal_runtime_error` + `RuntimeErrorReport { request_id, agent_id, conversation_id, purpose, code, message, occurred_at_ms }`。写日志是 fail-soft：写失败只计数告警，不替换原错误，也不再触发 issue 或自动重试。
- provider 的终局回执仍是"后端做了什么"的唯一权威；错误日志不是结算路径。

## 诊断接口

| 接口 | 含义 |
| --- | --- |
| `BrokerHandle::available_providers()` | 当前真正可运行的 provider/model 对（安装事实，不是模型目录） |
| `BrokerHandle::serves_model(model)` | 该模型能否在本 transport 上真实执行（封口前的路由依据） |
| `BrokerHandle::for_model(model)` | 同 transport 的另一模型绑定；主体的 seal 保持不变 |
| `BrokerHandle::effective_capacity()` | 扣除隔离欠账与可强制外部预留后的可用 C |
| `BrokerHandle::kv_snapshot()` / `rebalance_kv()` | 驻留池现状与重新分配结果 |
| `Broker::probe_capabilities()` / `discover_capabilities()` | 能力快照与模型上下文上限 |
| `BrokerHandle::domain()` / `domains()` | 物理域与部署的别名绑定 |

## 未验证与外部前提

代际交接使用 `BrokerHandle::quiesce_for_handoff().await`：在真实 accounting
锁内关闭新提交，等待已排队/正在运行的本地 provider reader 与回执写入结束。
超时返回错误，旧 runtime 可以通过 `resume_after_handoff_abort()` 重新开放；
不 abort 外层 future 来假称远端请求已结束。返回快照包含原 request binding/payload、
资源域身份、KV、外部预留和 unknown 欠账；非零欠账不会因交接释放。
`ResourceDomain::handoff_snapshot()` 是转移事实，不是 remote termination 证书。
其他旧 Store writer 的最终停止仍由独立 helper 取得旧 runtime 的排他目录 owner
来证明；此 broker 快照本身不等于全系统水位。

- 本轮是源码阶段：没有运行过 cargo 构建、测试、真实 provider 或模型调用；本文所述接口的编译与运行证据由集成方在合并后统一取得。
- 真实流式需要 provider 端点实际返回 SSE；fixture 只能证明解析与分批逻辑，不能证明真实 wire。
- KV 的物理容量、保留与迁移在本次实现中没有真实后端证据；未观测即未知。
- 四个原生后端的原生 operation 在没有对应部署探测响应时保持 `Unknown`。

<a id="spec-13"></a>
<!-- migrated-contract: SPEC:13 -->
# 13 推理调度与 KV 池

本章说明调度目标与真实后端边界；首版 Request/Permit/ContextLease、有限 generation、取消确认、未知占用和 native evidence 的精确执行合同见 [§41](architecture.md#spec-41)。计算许可与缓存驻留分别核算。

<!-- /migrated-contract: SPEC:13 -->

<a id="spec-13-5"></a>
<!-- migrated-contract: SPEC:13.5 -->
## 13.5 KV 句柄与上下文稳定性

每个 context handle 绑定 backend instance epoch、model/tokenizer/template/toolset digest、scope、session/window、prefix hash、lease/expiry 与支持的操作。backend 重启、模型切换、模板变化、scope 改变都会使相应 handle 失效。缓存只是加速层；失效后从原始事件重建，不丢任务和身份。

替换 broker/harness/kernel 时，active、reserved、cancel-pending、unknown 请求、deadline、KV lease、权限 revision 与后端 epoch 都进入交接记录；不能因新代码账本为空而重发满额请求。已失联旧 backend 的逻辑隔离只阻止旧结果生效；按 [§13.4](quota-resources.md#spec-13-4) 确认实际执行终止或物理资源释放后，才能归还对应容量。插件索引、guide、执行 binding 和已验证模型 profile 绑定同一兼容 revision/digest，避免文档升级但实际工具仍旧版。

稳定 system、工具顺序/序列化和 memory snapshot；新消息和提醒尽量追加到后缀。身份、人名或时间不要每步重写在最前面。并行孩子只在有权限的共同前缀上共享缓存。人格更新、主动 reset、删改临时提醒可能造成重算，要计入调度成本，不能为了命中率阻止有价值的新证据或遗忘。

部分 backend 只能 append，不能 rewind/replace。它们使用 append-compatible 的提醒/过期声明；需要精确临时上下文的模式必须可重建或换窗口。明确区分这两种语义，不能为了 KV 隐瞒已改变了 memory policy。

<!-- /migrated-contract: SPEC:13.5 -->

<a id="spec-13-6"></a>
<!-- migrated-contract: SPEC:13.6 -->
## 13.6 Backend 能力矩阵

| 后端 | 已核验的能力方向 | 明确不能默认承诺 |
| --- | --- | --- |
| 通用 OpenAI-compatible | 全量历史请求与标准工具协议，按实际支持配置 | 标准化 slot/pin/fork/save、真实 KV 容量或取消确认 |
| llama.cpp | id_slot/cache_prompt 与 slot save/restore/erase 路线 | 所有端点/版本都兼容同一组 slot 参数 |
| vLLM | prefix caching、cache_salt、原生请求/connector/KV hints；按 SDK/gRPC/HTTP 区别支持 | 所有传输等价、session_id 等于硬驻留、sleep 等于无损单会话 park |
| 1Cat-vLLM | 独立 fork 的实际 Chat/Anthropic 扩展、SM70 诊断与真实缓存使用量 | 自动继承最新上游字段；HTTP 200 等于参数已生效或 cache reset 已成功 |
| SGLang + Dynamo | 实验 session_control 和 sticky/streaming session 路线 | 通用 branch/rewind；所有 SGLang 部署都已具备 |
| NInfer | prefix reuse、engine 内状态保留；Responses continuation 可关联内部 session | prompt_cache_key 等于 slot；公开可硬切半池或任意固定 slot |
| Strata | 串行/parallel、Host conversation parking、原生 slot/事件协议 | Host 停车等于 GPU 驻留；按领域禁止并发；stdio 功能等于 HTTP 已开放 |

依据：[llama.cpp server](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)、[vLLM prefix caching](https://docs.vllm.ai/en/latest/design/prefix_caching/)、[Dynamo SGLang sessions](https://docs.nvidia.com/dynamo/latest/knowledge-base/modular-components/backends/sg-lang/agents-on-sg-lang)、[NInfer cache 调度](https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/maintainer/resource-scheduling-and-context-cache.md)、[Strata batching](https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/BATCHING.md)。以上是 adapter 的功能边界，不代表本产品已经实现这些 adapter。

Dynamo 的 streaming session 还需考虑 append-only、空闲 TTL 和 pinned KV 清理；并非靠一个通用 OpenAI 字段就可完成。本产品不强制部署 Dynamo 集群，保留其能力作为选装 provider。

诊断指标至少包含 queue wait、TTFT、cached/uncached prompt tokens、prefill 时间、active generation、parked tasks、KV/state 驻留、换入换出、取消未确认、主体最大等待、child 公平性与每任务总成本。内部指标与用户界面的“为什么在等”建立可解释映射。

<!-- /migrated-contract: SPEC:13.6 -->

<a id="spec-13-7"></a>
<!-- migrated-contract: SPEC:13.7 -->
## 13.7 原生 API、能力协商与真实透传

NInfer、Strata、vLLM 和 1Cat-vLLM 是明确的原生后端适配目标；OpenAI-compatible 只是可用传输之一。各插件可提供 HTTP 扩展、gRPC、SDK 或节点 companion，以原生能力目录和版本化命名空间呈现实际支持的操作。共同框架管理任务身份、预算和事件；后端专属的 lane、KV pool、StateImage、Host backing、conversation parking、connector 和 decode 状态保持原义。

能力声明至少绑定 backend family、实际 build/version、model/tokenizer/template fingerprint、transport、connector/configuration 和 engine incarnation；每项能力分别标记 available / requires-configuration / unsupported / unknown。另记录作用域（request/conversation/engine/device pool）、保证（advisory/best-effort/acknowledged/reserved）、容量单位及 measured/estimated/unknown、可调用 schema 和原始返回。内部实现存在、某入口能调用、部署已启用、某次请求实际生效是四种不同事实。

扩展使用 `native.ninfer`、`native.strata`、`native.vllm`、`native.1cat` 等命名空间；声明支持的字段、事件、错误和句柄须端到端透传，且不丢失原生值与版本。不能支持时返回可诊断缺口，Agent 可按 [§18.7](cognition-units.md#spec-18-7) 委派补插件、companion 或必要的后端适配。后端忽略未知字段时，HTTP 200 不能使能力状态变为已支持。原生调用同样经过作用域与全局资源记账，不能绕过 broker。

<!-- /migrated-contract: SPEC:13.7 -->

<a id="spec-13-7-1"></a>
<!-- migrated-contract: SPEC:13.7.1 -->
### 13.7.1 四个后端的适配重点

| 后端 | 原生接入计划 | 保留的真实边界 |
| --- | --- | --- |
| NInfer | Chat/Responses/Anthropic 实际扩展、cache hints、Prometheus 与 JSONL 请求级调度事件；必要时以明确版本的 companion 连接内部 Engine | execution lanes、各类 KV、Device StateImages、Host backing 分开；Snapshot 与 Replay 分开。内部稳定边界暂停不是任意客户端抢占。prompt_cache_key 不是 Engine session key，TTL 不是驻留承诺 |
| Strata | 服务配置和其真实引擎协议；接入 BGEN/BADM/BT/BDONE/BSTOP/BYIELD 等事件与操作时由服务/companion 保持单一 Engine 所有权 | stdio 能力不能冒充现有 HTTP 字段。Host conversation parking、GPU residency、parallel decode 分开；BSTOP 与 prompt 读取时的 BYIELD 语义不同，具体构建/配置需核对 |
| vLLM | 按构建与传输接入 priority、request/session identity、cache_salt、kv_transfer_params、可用 KV hints/connector 及原生观测 | prefix caching 和 affinity 不保证持久物理槽；engine-wide pause/sleep 与单请求控制不同。所查 sleep 会丢 KV，不能作为默认保留缓存轮转办法 |
| 1Cat-vLLM | 独立核验 fork 的 Chat/Anthropic 扩展、原生 engine 接口和 /v1/sm70/acceleration 诊断，不继承未经验证的上游能力 | 所查 Chat schema 未声明上游 session_id；未知字段可被接受后忽略；加速配置报告不证明某请求实际走了某 kernel；cache-read usage 无数据时不得编造 |

NInfer 和 Strata 沿用用户给定固定版本。[NInfer 调度](https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/maintainer/resource-scheduling-and-context-cache.md)、[NInfer serving](https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/serving.md)、[Strata batching](https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/BATCHING.md)。vLLM 本轮固定主线 `c722f3a5368a9eed2b90c1a14b8eaf17acba3e93`，1Cat 固定 `1d7321bf2d35ac8970870dc51bc1f7405517c3ca`；源码能力不自动等于当日正式安装包的能力。[vLLM Chat schema](https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/vllm/entrypoints/openai/chat_completion/protocol.py)、[1Cat Chat schema](https://github.com/1CatAI/1Cat-vLLM/blob/1d7321bf2d35ac8970870dc51bc1f7405517c3ca/vllm/entrypoints/openai/chat_completion/protocol.py)、[1Cat 忽略额外字段的处理](https://github.com/1CatAI/1Cat-vLLM/blob/1d7321bf2d35ac8970870dc51bc1f7405517c3ca/vllm/entrypoints/openai/engine/protocol.py)、[SM70 诊断说明](https://github.com/1CatAI/1Cat-vLLM/blob/1d7321bf2d35ac8970870dc51bc1f7405517c3ca/docs/serving/sm70_release_profile.md)

<!-- /migrated-contract: SPEC:13.7.1 -->

<a id="spec-13-7-2"></a>
<!-- migrated-contract: SPEC:13.7.2 -->
### 13.7.2 传输差异必须可见

vLLM 的 KvHintsEnvelope 有 protocol_version/message_id/actions，各 action 有独立 ID、类型、版本及 payload；Python AsyncLLM 和 Rust gRPC 路径有真实支持证据。但同一提交的 Rust HTTP generate 转换明确设为 `kv_hints: None`，所查 Python Chat schema 也没有声明该字段。插件必须准确告诉 Agent 当前传输缺什么，并可选择另一传输或构建适配；不能只见内部类型就宣称普通 HTTP 已透传。[KV hints 协议](https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/vllm/v1/kv_hints/protocol.py)、[gRPC 转换](https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/rust/src/server/src/grpc/convert.rs)、[HTTP 转换](https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/rust/src/server/src/routes/inference/generate/convert.rs)

同名管理路由也不能合并语义：上游 reset_prefix_cache 返回实际 success；所查 1Cat 路由忽略该返回值并直接给 HTTP 200。上游 abort_requests 的回执表示取消请求已提交，底层发送 ABORT 不等于 GPU 已静止。产品保持 requested → accepted → terminal/quiesced 及 unknown 状态；可靠结束前不归还计算许可，单请求取消不得因空 ID 变成全局取消。全局 pause/sleep/reset 需要明确 engine 作用域，不能伪装成对一个孩子的操作。[上游 reset](https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/vllm/entrypoints/serve/dev/cache/api_router.py)、[1Cat reset](https://github.com/1CatAI/1Cat-vLLM/blob/1d7321bf2d35ac8970870dc51bc1f7405517c3ca/vllm/entrypoints/serve/cache/api_router.py)、[abort API](https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/vllm/entrypoints/rl/online/api_router.py)、[EngineCore client](https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/vllm/v1/engine/core_client.py)

<!-- /migrated-contract: SPEC:13.7.2 -->

<a id="spec-13-7-3"></a>
<!-- migrated-contract: SPEC:13.7.3 -->
### 13.7.3 资源域、轮转与缓存承诺

不同 URL、代理、模型别名、OpenAI/Anthropic/gRPC 入口若指向同一 resident Engine，必须能显式绑定同一 physical resource domain；不同 URL 不能产生额外并发预算，同名模型也不能据名称自动合并不同设备。所有受本产品控制的主/子/记忆/发现请求都经过该域 broker。外部未受控客户端的占用由可取得的压力指标辅助估计，不宣称本地信号量已经控制它们。

C=1 时，每次有限 generation 完成，主体的下一轮继续入队，children 公平取得执行机会；请求内抢占只进一步改善延迟。调度器须处理 thinking/assistant 续写、完整工具参数和 prefill 成本，截断 JSON 不能执行，重建上下文不能冒充无损续写。Strata 的有效并发以当前模型、硬件、负载和时延目标的实际比较选择，不按任务领域禁止并发。

50/50 仍是理想均衡负载下算法自动实现的结果，共享前缀只计一次，工作变化后自动偏移。后端有真实保留/配额控制时才承诺物理保证；只有 hint/affinity/Host backing 时报告 best effort 与实测命中/恢复；只有普通生成接口时仍实现公平推进，并指出原生能力缺口。池大小可以未知，未知不改成零或捏造绝对 token 数。指标保留单位、时间起点及 null，Snapshot/Replay/prefill、编译缓存/CUDA graph/KV cache 分开记录。

<!-- /migrated-contract: SPEC:13.7.3 -->

<a id="spec-25"></a>
<!-- migrated-contract: SPEC:25 -->
# 25 NInfer 接入教程 中文与 English

以下是面向后续使用者的文档草案。上游命令已按所列源码快照核对，**本轮没有安装、启动或测试 NInfer，也没有实现本产品的 provider**。本产品配置表是拟议合同，不是假装已经存在的 CLI。

核验来源：[README 与构建要求](https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/README.md)、[HTTP serving](https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/serving.md)、[CLI 与上下文参数](https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/cli.md)、[资源与缓存机制](https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/maintainer/resource-scheduling-and-context-cache.md)。

<!-- /migrated-contract: SPEC:25 -->

<a id="spec-25-1"></a>
<!-- migrated-contract: SPEC:25.1 -->
## 25.1 中文操作路径

**准备推理机器。** 当前上游以 x86-64 Linux、单张 RTX 5090、sm_120a 为目标；验证过 CUDA 13.4。按上述 README 准备 CMake 3.28+、C++20 编译器、Ninja、pkg-config、FFmpeg 开发库、hwloc、liburing 与 libcurl。它不是放到路由器或 WASM 内的小型服务；Bot 可运行在另一台轻型控制节点。

**取得锁定版本并构建。** 下面命令供未来部署者运行，不会在本次规格研究中执行。

```bash
git clone https://github.com/Neroued/ninfer.git
cd ninfer
git checkout 64d10fbf9f8e6bfc79ab0570b94674c5b9997929
cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Release
cmake --build build -j
```

按对应模型卡取得兼容 v3 artifact；例如已安装 Hugging Face CLI 的环境可运行：

```bash
hf download neroued/Qwen3.8-27B-nvfp4-NInfer qwen3_8_27b_nvfp4.ninfer --local-dir models
```

**先建立单计算并发、可保留多状态的基线。** 以下 16K/32K 数字是教学起点，必须按实际启动分配与负载调整，不是性能最优值。`--device-state-slots 1` 为执行 lane 之外增加一份 Device StateImage，不等于增加计算并发。

```bash
./build/apps/ninfer-serve models/qwen3_8_27b_nvfp4.ninfer \
  --host 127.0.0.1 --port 8080 \
  --model-id personal-local \
  --max-context 16384 --kv-capacity 32768 \
  --max-concurrency 1 --device-state-slots 1 \
  --kv-dtype fp8 --api-key 'REPLACE_WITH_LOCAL_API_SECRET'
```

检查 `/health`、`/v1/models` 与 `/metrics`，确认有效 context、设备与 Host backing。默认 Host backing 可能明显大于控制节点内存；需要限制时查阅 `--host-context-mib`，不能把它与 KV token 池混为同一个量。示例关闭未声明的视觉/推测能力；若需要媒体，按模型支持配置并重新核算资源。

**连接本产品。** 用 CredentialProvider 保存 API secret，ModelAdapter 指向实际 `/v1` 地址；跨机使用已经建立的可信本地网络/SSH tunnel，不能只把示例的 loopback 地址抄到另一台机器。拟议配置为：

| 本产品字段 | 示例/规则 |
| --- | --- |
| model / endpoint | `personal-local`，实际 NInfer URL |
| resource_group | 该物理 engine 的唯一 ID，所有 alias/remote seats 共用 |
| max_active_requests | 1，与上游限制一致 |
| context_limit | 以 `/v1/models` 有效值和用户上限的较小者为准；输出也占窗口 |
| residency_policy | 自动分配；在当前公开接口上配额多为软目标，硬保留需单独验证的 adapter；不能用手设比例代替自动算法 |
| credentials | secret handle，不把 token 放进主体 prompt 或记忆仓库 |

随后运行 A03/A04 对应的“主体→孩子→主体→另一孩子”短验收，检查请求并发、cached/uncached prefill 与恢复时间；仅 HTTP 200 不算缓存复用成功。若使用 `/v1/responses` 的 `previous_response_id`，绑定 backend epoch 与本地 source history，失效可完整重建；`prompt_cache_key` 不是可任意操作的 slot ID。

<!-- /migrated-contract: SPEC:25.1 -->

<a id="spec-25-2"></a>
<!-- migrated-contract: SPEC:25.2 -->
## 25.2 English walkthrough

**Purpose and status.** This is a deployment guide for the planned adapter, not a claim that the Bot or adapter already exists. The commands above are documentation only. Pin NInfer to the listed commit before following them; review upstream changes when upgrading.

1. Prepare a supported inference host and the build dependencies from the pinned README. The small Rust control node can be separate from this GPU machine.

2. Run the checkout/build block in [§25.1](inference.md#spec-25-1), then obtain the matching v3 model artifact. The example download uses the upstream Qwen3.8-27B NVFP4 artifact.

3. Run the server block in [§25.1](inference.md#spec-25-1) after replacing the local API secret. Start with one active computation. Keep the example context/KV sizes as explicit starting assumptions, then inspect actual allocations rather than assuming they fit every workload.

4. Verify readiness, advertised model/context and metrics. Configure the Bot with the same model alias, a credential handle and one shared physical resource-group ID. All Bot workers, memory jobs and remote agents using this engine must share that group.

5. Alternate controller and worker requests while retaining their local histories. Measure reuse and restore cost, and ensure at most one request remains active or termination-uncertain. Do not equate an HTTP success response with a cache hit.

6. Verify automatic allocation separately from cache reuse: a balanced workload should approach an even main/worker split without configuring that ratio, and allocation should adapt when demand or recovery costs change. Hard retention or quota guarantees require separate backend evidence. Preserve full-history reconstruction when reusable state is lost.

The example secret should be supplied through the deployment's credential mechanism in a real installation. A remote control node must reach the actual secured endpoint or tunnel; its own `127.0.0.1` refers to itself. Diagnose memory pressure using separate device KV, recurrent state, Host backing and request limits.

<!-- /migrated-contract: SPEC:25.2 -->

<a id="spec-26"></a>
<!-- migrated-contract: SPEC:26 -->
# 26 Strata 接入教程 中文与 English

核验快照：`Niko1221/Strata@011c9e600a93bbc9fd451c563501b5831c32bbca`。来源：[README](https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/README.md)、[Terminal launch](https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/TERMINAL_LAUNCH.md)、[Batching](https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/BATCHING.md)、[引擎细节](https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/DETAILS.md)。本轮未启动 Strata 或测试其性能。

<!-- /migrated-contract: SPEC:26 -->

<a id="spec-26-1"></a>
<!-- migrated-contract: SPEC:26.1 -->
## 26.1 中文操作路径

**安装与机器检查。** 从 [官方 releases](https://github.com/Niko1221/Strata/releases) 取得匹配系统的 Terminal/app，并记录 app、engine 和模型版本。安装程序可用不代表该系统有适合当前显卡的 engine；按照上游硬件表确认。README 的内存门槛与 Bot 控制节点门槛是两回事。

**先看部署计划，再选择本地模型。** 以下是上游命令，教学示例选 `q2_0`；选择实际量化时以模型质量和机器资源为准。

```bash
strata launch --plan --model q2_0
strata launch --model q2_0 --context 32k --parallel 1 --vision off --yes --no-tui
```

也可运行交互式 `strata launch` 让它展示可用模型与设备。`--parallel 1` 是本例的串行执行设置；图片能力是否开启独立配置。查看当前状态与实际服务地址：

```bash
strata launch --status
strata monitor --url http://127.0.0.1:8080
strata doctor
```

示例 monitor 地址仅适用于默认本机端口，实际以状态输出为准。跨 LAN 可选择上游 `--access lan` 路线并取得生成的 key；本产品将其存为 secret handle。`strata connect` 是上游已有应用的连接辅助，不代表它已经支持尚未开发的本产品。

**接入 Bot。** ModelAdapter 使用实际 `/v1` endpoint，从模型发现结果取 ID；全局 broker 先设 C=1、context 不超过有效值，同 engine 的所有来源统一 resource group。上游可能根据配置/资源体现不同有效槽位，因此诊断使用实际值，不只回显启动意图。

**启用可观测的会话 parking。** Strata 的 conversation cache 可在 Host RAM 保留多份会话并按精确前缀恢复，适用于 main/worker 交替。高级 engine 配置参考 `--conversation-cache-mib` 与 `--conversation-cache-slots`；不要未经核对将 engine flags 直接当作 `strata launch` 顶层 flags。先由 launch 预览实际 engine 命令，再在相应高级配置面设置，并记录有效值。Host 预算包括实际 snapshot 与切换成本，不是 GPU KV 各半接口。

先验证两条固定历史来回交替，再增加短一次性请求，比较未缓存 prefill 与恢复耗时。`/slots/0` 的保存/恢复有串行模式等限制；parallel 下的 501 不应被 adapter 隐藏为成功。不要使用 NInfer 的 `previous_response_id` 假定 Strata 有同样 continuation 语义。

<!-- /migrated-contract: SPEC:26.1 -->

<a id="spec-26-2"></a>
<!-- migrated-contract: SPEC:26.2 -->
## 26.2 English walkthrough

**Status.** These are upstream deployment steps and a specification for the future Bot adapter. No installation or benchmark was performed for this document.

1. Obtain the Terminal/app from the official release page, choose a supported engine for the actual machine, and record app, engine and model versions separately. Installing a frontend alone does not establish GPU compatibility.

2. Run the plan and launch commands in [§26.1](inference.md#spec-26-1). They illustrate a serial, text-only starting point. Use the interactive launcher if you want to inspect supported models and resource choices before starting.

3. Check launch status and use the monitor/doctor commands above. Substitute the real server address if it differs from the example. For LAN access, use the upstream access/key mechanism and store the key through the Bot's credential provider.

4. Point the planned adapter at the actual `/v1` endpoint and discover the advertised model ID. Set the Bot's active-request cap to one initially, respect the effective context ceiling, and group every client of that physical engine under the same broker budget.

5. Configure conversation parking through the documented engine settings. Inspect the engine command generated by the launcher rather than assuming every engine flag is also a launcher flag. Alternate two conversation histories and observe reuse, Host memory and restore latency.

6. Treat Host parking, GPU residency and parallel execution as different capabilities. Do not report hard half-pool isolation from a parking setting, or claim slot save/restore succeeded when the server rejects it in parallel mode. Reconstruct from local source history when a reusable backend state is unavailable.

The launcher may download large model/engine assets during a real deployment; that activity is not part of this SPEC-writing session. The examples deliberately do not invent a Bot installation command or a ready-made `strata connect` target for this future framework.

<!-- /migrated-contract: SPEC:26.2 -->

<a id="spec-35-9"></a>
<!-- migrated-contract: SPEC:35.9 -->
## 35.9 全局推理与中断

首个实机基线沿用 [§22.1](nodes.md#spec-22-1) 四流 128K；133120 是后端余量，不改写为产品标称窗口。主体、控制 child、memory worker、验证回合、候选 B 烟测以及 coding agent 的内部／递归调用，只要使用同一个物理模型资源，都必须进入同一个 broker。不能把一个 tmux pane 计作一槽而漏掉其内部请求，也不能让 A 和 B 各自占有四槽。云模型按实际 provider 资源组核算。

既有 CLI 未接入共享 broker 时，明确为未纳管，不能宣称已实现全局硬上限；应配置其端点/代理/adapter 或选择受管 seat。保留后端扩展字段和真实 KV 状态，不以通用代理抹掉 NInfer／Strata／vLLM／1Cat 的差异。

新消息优先、中断、DND 与队列回访按 [§13.8](subject-runtime.md#spec-13-8)。取消某段 generation、停止控制 child、停止远端 coding agent 和撤销其文件/外部效果是不同动作。后端不能请求内暂停时采用有限 generation 轮转；等终端/构建时释放推理许可。50/50 是算法在适合场景下自行形成的分配结果，不能作为固定配额。

<!-- /migrated-contract: SPEC:35.9 -->

<a id="spec-41-9"></a>
<!-- migrated-contract: SPEC:41.9 -->
## 41.9 推理请求、计算许可与单并发

### 记录与容量不变量

~~~text
InferenceRequest {
  request_id, request_lineage_id, request_revision, supersedes_request_id?,
  owner: { task_id, task_revision, branch_id?, step_id?, purpose },
  resource_group_id, engine_incarnation, capability_snapshot_ref,
  context_revision, context_binding_ref?, hook_graph_ref, request_seal_ref,
  wire_ref, wire_digest,
  model_profile_ref, input_tokens, output_reservation, state_cost_vector,
  priority: { class, inherited_from?, policy_revision, trusted_origin },
  state: sealed | queued | reserved | armed | active | stop_requested |
         unknown | quiesced | retry_pending | terminal,
  active_attempt_id?, allocation_id?, stop_intent_ref?, retry_decision_ref?,
  result_ref?, terminal_reason?, record_revision
}

ComputeAllocation {
  allocation_id, resource_group_id, engine_incarnation,
  request_id, attempt_id, units, owner_broker_epoch,
  state: held | released,
  acquired_commit, release_proof_ref?, released_commit?, record_revision
}

QuiescenceProof {
  proof_id, request_id, attempt_id, engine_incarnation,
  provider_request_handle?, native_slot_generation?,
  scope: request | engine | physical_resource,
  kind: never_dispatched | request_terminal | gated_pause | engine_destroyed,
  native_evidence_ref, observation_commit,
  auto_resume_possible: false,
  capability_contract_ref
}
~~~

request_id 唯一标识一份不可变 RequestSeal 对应的逻辑生成请求；网络 attempt、后端 handle 与实际 slot 不混作该 ID。**任何改变初始模型输入、工具/模型语义参数或预算的 reseal，都同时分配新 request_id、在同一 request_lineage_id 内递增 request_revision，并填写 supersedes_request_id。** 同一个 request_id 不得指向两份初始 seal；有能力证明的 live-steering 输入链按 [§41.18](web-interface.md#spec-41-18) 独立版本化并绑定结果。因此入队、恢复和结果关联可按 request_id 去重，不会把新的输入误认成旧 generation。对同一上游调用的状态查询不生成第二次 generation；原输入未变且符合恢复合同的重试可新增 attempt。

同一 resource_group 的所有 held ComputeAllocation，加已核验且可强制的外部预留，不能超过该组有效 C。reserved、armed、active、stop_requested 和 unknown 都可能占容量；UI 状态名称不决定是否计数，Allocation 记录才决定。resource_group 绑定实际共享 engine/device，而非 URL、模型别名或 API 协议。递归 child、memory、judge、媒体与远端 coding agent 使用该物理域时走同一账本。

多 broker 接管需要共同的持久 authority/epoch 与受管 dispatch 通路。若旧 broker 仍可绕过通路直接请求同一后端，则仅增加本地 epoch 不能兑现硬 C；该部署必须标为未完全受管，完成入口接管或取得可验证的隔离后才能宣称硬上限。软件 A 与候选 B 不能各自初始化一份满额许可。

### 转移表

| 转移 | 前置条件/提交点 | 容量处理与恢复 |
| --- | --- | --- |
| sealed → queued | wire/context/token/native 能力校验通过；request_id 幂等入队 | 不持有 compute；缓存准备另有自己的预算 |
| queued → reserved | broker 当前 epoch 有效；C 与实际资源向量可行；必要 ContextBinding 已可用或可回滚预留成功 | 原子取得 held Allocation；不能先占许可无限等另一资源 |
| reserved → armed（T1） | 最终 context/slot 绑定与 request revision 相符；请求未撤销；dispatch owner 有效 | 持久记录 attempt 与全部外发参数，然后才允许写网络/SDK。armed 后崩溃视为可能已发送 |
| armed → active | 观察到匹配后端 handle 的接受/流事件 | 保存 E1、原始 handle/engine/slot generation；不把 HTTP 200 当完成 |
| queued/reserved → terminal(cancelled_before_dispatch) | 取消事务先赢得未 armed 的状态，dispatcher 再也不能发送该 attempt | 若有 Allocation，以 never_dispatched 证明原子释放；不得仅靠调用者说“我大概没发” |
| armed/active → stop_requested | 持久 stop intent 成功；记录 cancel/yield 的真实作用域 | 事务外发控制操作；仍 held。accepted 是收到控制请求，不是终止证明 |
| armed/active/stop_requested → unknown | 连接断开、超时、controller 崩溃或无法判定请求是否仍运行 | 仍 held；创建无须 LLM 的状态对账工作。deadline、心跳或租约过期均不释放 C |
| armed/active/stop_requested/unknown → terminal 或 retry_pending | 得到匹配 attempt 的终态事实，或确认 engine/物理执行实例已销毁；快速完整响应/明确拒绝允许跳过 active 的单独落账 | E1/T2 先保存 attempt 的终态、usage/partial 和 QuiescenceProof，并原子释放其 Allocation。逻辑结果已完成或明确不再继续才将 Request 标 terminal；同输入仍需合法重试时，保存 retry_decision_ref 并转 retry_pending |
| retry_pending → queued | 原 seal 不变；任务仍需要结果；上次 attempt 已确定静止；恢复/重试策略允许且任何外部计费风险已有记录 | 创建新的 attempt_id 并重新入队；不复活旧 Allocation。terminal Request 不重新打开；需改变语义时创建新的 request_id |
| active/stop_requested → quiesced | 后端提供可核验的暂停完成，并且恢复必须经 broker 重新准入 | 保存 gated_pause 证明后可释放 compute；ContextLease/状态字节独立保留。恢复先重新取得 Allocation |
| quiesced → queued | 原输入/恢复 frontier 有效；当前任务仍需要结果；恢复操作由 broker 独占控制 | 同一逻辑请求的恢复 attempt 入队；保留历史输出/usage，不能把 replay 当新输出再发布 |

Attempt 的终态和 Request 的逻辑终态分别记录。一个明确拒绝而没有执行的 attempt 可结束并释放占用，同时其 Request 等待重试；一次完整成功结果则通常使 Request 终态。未知执行不能被归为可重试的明确拒绝；发生过部分生成时，只有协议证明可续接，或策略明确记录重新计算、重复费用与原 partial 处理方式后，才创建后继 attempt。

后端内部 paused、Host parked、排队状态或没有 token 输出，**都不足以**形成 gated_pause。若 engine 会自行恢复原请求，它仍是 broker 未结束的工作；适配器必须继续计占用，或在后端整体受控的容量模型中证明不会超额。不得先释放 C 再希望旧请求恰好不恢复。

允许释放容量的证据由具体 adapter 合同定义。例如 Strata 的已绑定 slot 在收到对应 BDONE 后可按其协议认定执行槽空闲；仅写出 BSTOP 不足。通用 HTTP 连接断开则通常没有同等级证明。后端一个新连接或新的客户端 session ID 也不能证明旧 server request 消失。

本地 request terminal、compute 已停止、逻辑任务完成、远程 shell 已停止和外部动作已撤销是五个不同事实。取消 generation 不取消已发出的 SSH/GUI/文件效果；这些 operation 继续按自己的 ledger 回收回执和对账。

### 首版调度默认与公平性

首版默认使用有限 generation：默认输出预算 2048 token，按模型实际接口计入推理/可见输出及工具参数开销；模型 profile 可以采用适合该模型的有限上限。达到 length 边界但工具参数未完整时，不执行 partial，而把完整可用输出与不足情况存档，再安排后继/重建。上限是可改工程默认，不是 128K 输入窗口，也不是每步必须耗尽的目标。

普通 ready 请求按主体与 child 的有效任务队列公平轮转；在双方都持续可运行且没有更高等级事件的情形下，默认不连续服务同一侧超过两个完整 generation。child 侧再按任务 lineage、老化与依赖关系选择，递归创建新 task 不增加该父系的总公平份额。实际 token、prefill/replay 和时间成本用于更新策略，不用“每个请求次数相同”冒充耗时相同。

外部高紧急事件仍按 D3 可先服务；紧急来源不能提权。主循环每步立即登记后继，但登记不意味着永远抢在 child 前面。正常请求有限轮转是计算公平协议，不是低频 tick。即使后端完全不支持中途取消，有限请求完成后的轮转也必须使双方继续推进。

同一 child 在两次 durable checkpoint 之间被连续取消三次时，默认生成一次合并的 attention-starvation 建议，交给主体决定 DND、调整任务、改变模型或继续接受打断；不自动向人类索要批准。建议带取消成本、已完成/未完成工作和最老待办；同一停滞 case 在没有新证据/策略变化前不重复刷屏。任何固定阈值均属于可替换策略插件。

取消/断连对账使用确定性 I/O，不等主体取得被占用的唯一槽后才查询。首版控制请求超时只改变 knowledge state；可用有界指数退避和持久 next_probe_at 安排重试，但任何重试时钟都不代替主体连续思考，也不产生“超时即免费”的容量漏洞。无可查询路径时保持 unknown，并允许主体/恢复插件选择已授权的引擎重启、隔离或独立后端；选择前如实保留受影响容量。

<!-- /migrated-contract: SPEC:41.9 -->

<a id="spec-41-10"></a>
<!-- migrated-contract: SPEC:41.10 -->
## 41.10 KV 与原生后端能力

### 能力声明和句柄

~~~text
BackendCapabilitySnapshot {
  capability_id, provider_implementation_digest, backend_family, backend_build,
  transport, configuration_digest, engine_incarnation,
  model_fingerprint, tokenizer_fingerprint, template_fingerprint,
  operations: Map<name, {
    availability: available | requires_configuration | unsupported | unknown,
    scope, guarantee: advisory | best_effort | acknowledged | reserved,
    input_schema_ref, output_schema_ref, native_field_mapping_ref,
    cancellation_semantics_ref, evidence_ref
  }>,
  capacities: [{ pool, unit, value?, provenance: measured | configured | estimated | unknown }]
}

ContextLease {
  context_id, context_revision, owner_task_id, owner_scope_ref,
  provider_id, engine_incarnation, capability_snapshot_ref,
  native_handle_ref, native_handle_generation?,
  model_fingerprint, tokenizer_fingerprint, template_fingerprint, toolset_digest,
  window_id, prompt_token_digest, prefix_length, frontier_ref?,
  state: requested | reserving | resident | parked | transferring |
         reclaim_pending | invalid | released,
  copies: [{ tier, pool, units, identity_ref, readiness, sharing_ref? }],
  retention: { guarantee, entitled, used, borrowed, reclaim_pending, expires_at? },
  supported_operations[], lease_revision
}
~~~

scope 指真实数据/缓存可复用域，不要求为每个群创建隔绝的主体知识。原生 handle 可以是不透明引用；不能把它当模型可自行猜写的 slot 整数。续接必须同时核对 engine incarnation、模型/模板/工具指纹、实际 token 前缀及窗口/恢复 frontier；任何不符都按原始事实重建或使用后端明确提供的转换。

cache affinity、prefix hint、可恢复 Host 状态和 GPU 硬驻留分别标记。不能仅凭请求带 session_id/prompt_cache_key、HTTP 200 或 cache hit 指标就把 retention.guarantee 标成 reserved。capacity 不可知时保留 null/unknown，token 计数不能直接冒充物理字节。

主体的多个 context 合计使用算法给主体分配的份额；children 合计使用另一侧实际可行的份额。理想对称场景下算法自动得到约 50/50 是验收场景；本合同不新增手动 half-pool 开关，也不把比例写进 kernel。共享 prefix 页只按实际物理占用计一次，公平归账另记。

### 迁移与恢复的实际提交点

一次 cache move/spill/restore 采用独立 operation：先预留目的空间与 source reader lease，记录迁移意图，再执行拷贝；只有原生完成证据到达，才发布 ready 副本并释放确实不再有 reader/引用的源。DMA/传输提交后取消，要等真实 reader 结束，不能因本地 future 被丢弃而重用源内存。目的未 ready 不得成为下一次 restore 来源。

恢复计划必须计算源仍存在时的目的/临时峰值；“最终能装下”不足以通过 admission。等待必要 child 前，必须为至少一个依赖的实际执行保留可行路径；主体若硬 pin 全部状态池而 child 无法启动，需迁移/回收/改变计划或报告 infeasible，不能持有父状态无限等待。

Snapshot 和 Replay 保留不同的内容与成本。若 adapter 只能根据持久 token/历史重算，报告 replay/prefill；只有真实执行状态和必要 KV/StateImage 可恢复时才报告 snapshot。Replay 不能重复发布历史输出、再次执行已完成工具或再扣同一次逻辑输出预算；真实新增计算成本仍应记录。

软件更新若 engine 没有重启，句柄仍需由新 adapter 按相同 capability/engine identity 重新验证后接管；不能把进程内指针序列化当可恢复 handle。engine 确认重建后，旧 ContextLease 全部 invalid；原始会话、任务与身份不受缓存丢失影响。

### 本轮上游事实与首版适配边界

| 上游一手证据 | 确认的事实 | Shenko 必须保留的边界 |
| --- | --- | --- |
| [NInfer 固定调度说明 §§3.2、8.3、9](https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/maintainer/resource-scheduling-and-context-cache.md) | typed pool 的联合预留；Snapshot/Replay；reader lease、目的预留、真实传输完成后交接 | 文档内的 engine 暂停/恢复不能自动当 HTTP 客户端已取得任意 pause/pin API；必须绑定 transport/build/config 及真实控制能力 |
| [Strata 固定 batching/engine 协议](https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/BATCHING.md#L547) | BGEN/BADM/BT/BDONE、BSTOP、BYIELD/YIELDED；BDONE 后执行槽空闲且 conversation 可保留 | 原生 stdio 由唯一 companion/服务所有者操作；不能伪造现成 HTTP 字段。BSTOP 与 prompt-read BYIELD 不是同一种停止语义 |
| [Strata conversation parking](https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/BATCHING.md#L507) | 有可配置的 conversation parking 与恢复路径 | Host parking 不宣称是 GPU hard residency；是否命中和恢复成本以该部署实测为准 |

vLLM 与 1Cat 的具体字段继续采用 [§13.7](inference.md#spec-13-7) 已固定的能力矩阵；本次不扩称新的可用 API。共同的 native.* 命名空间保留原始值、版本和返回，不经 OpenAI-compatible 最小公约数丢掉能力。native 参数在 seal 前校验；后端接受后忽略字段属于未生效，必须可诊断，不能被标为成功透传。

<!-- /migrated-contract: SPEC:41.10 -->

<a id="spec-47"></a>
<!-- migrated-contract: SPEC:47 -->
# 47 Provider 协议版本、SDK 演进与自主迁移

来源：U53、R142、B50、REF-CLAUDE-API；关联 [§13](inference.md#spec-13)、[§41.8](context-policies.md#spec-41-8)–[§41.10](inference.md#spec-41-10)、[§42](quota-resources.md#spec-42) 与 [§45](cognition-units.md#spec-45)。核验日期 2026-10-22。本章的官方事实、Shenko 设计和待实现验收分别标明；当前尚未运行真实 Claude API 测试。

<!-- /migrated-contract: SPEC:47 -->

<a id="spec-47-1"></a>
<!-- migrated-contract: SPEC:47.1 -->
## 47.1 Why 与适用范围

同一个模型名不足以确定请求格式。API 版本、SDK major、云平台客户端、网关和所用 beta 都可能改变实际请求、返回值与鉴权。自动更新 SDK 后即使业务代码未变，也可能发送不同协议。Provider 插件需要向主体暴露“目前使用什么、哪些能力改变了、怎样继续”，让主体自主选择保留、升级、迁移或改进能力。

版本规则属于 provider 的可替换代码、能力资料与认知依赖。最小 Rust/cordis-rs 主干只承载引用、生命周期、封口和恢复合同；它不维护所有供应商参数白名单。Claude 2026-10-21 的限制只作用于相应实际绑定；NInfer、Strata、vLLM、1Cat 与其他本地后端继续表达各自真实采样、KV 与扩展字段。

<!-- /migrated-contract: SPEC:47.1 -->

<a id="spec-47-2"></a>
<!-- migrated-contract: SPEC:47.2 -->
## 47.2 核验基线

| 维度 | 2026-10-22 已核验事实 | 对适配器的含义 |
| --- | --- | --- |
| API 版本 | 2026-10-21 已发布；官方迁移文档仍允许需要旧行为的请求继续使用 2023-06-01 | 保存实际版本，旧版本可用性继续随官方状态核验；不预设永久支持或某个虚构停用日期 |
| 采样 | 新版 Messages 与 Batch 请求含 temperature、top_p 或 top_k 任一项即 400，默认值也一样；缺省采样含 temperature=1.0 | 新版 wire 上完全省略这些键；低温效果不存在可等价替换的开关 |
| 消息 | assistant 的直接 document 非法；user text 的非空 citations 非法，包括 search_result 内相应 text；assistant 引用、文档/search_result 的 citations.enabled 与服务端工具结果内嵌文档有各自允许路径 | 按 role、block 与 nesting 校验，保留合法例外；相关结构规则同样覆盖 token counting 与 Batch 内请求 |
| SDK major | Python 2.0.0、TypeScript 1.0.0 等新 major 默认发送新 API 版本；SDK 类型/方法变化还可能独立于版本头 | 升级闭包同时记录代码、依赖、schema 与有效请求；回退版本头不等于回退 SDK |
| 鉴权 | 新 SDK/ant CLI 一般默认以 Authorization: Bearer 发送 API key；Claude API 仍接受 X-Api-Key。平台客户端存在例外 | 网关、重试、中间件和脱敏规则按实际客户端适配；API key、token 和网关凭据的来源分别明确 |

精确管理字段表与 SDK 个别变化见 [迁移核验](../research/claude-api-2026-10-21.md)。本表是阅读基线，不是把所有管理端点、SDK 语言或未采用云平台强制加入 Shenko 的实现范围；实际采用的调用路径必须满足相应合同。[官方版本](https://platform.claude.com/docs/en/api/versioning)、[迁移](https://platform.claude.com/docs/en/api/migration-2026-10-21)、[发布说明](https://platform.claude.com/docs/en/release-notes/overview)。

<!-- /migrated-contract: SPEC:47.2 -->

<a id="spec-47-3"></a>
<!-- migrated-contract: SPEC:47.3 -->
## 47.3 版本化绑定与请求封口

以下是可演进的规范数据形状，类型名不是已存在实现：

```ts
type ProviderWireProfile = {
  profile_id: string;
  revision: string;
  provider_instance_id: string;
  provider_implementation: VersionedRef;
  endpoint_origin_ref: DataRef;
  platform_client: string;
  model_id: string;
  api_version: string | { kind: "not_applicable" } | { kind: "unknown" };
  sdk: { name: string; version: string; closure: VersionedRef } | { kind: "direct_http" };
  request_schema: VersionedRef;
  response_schema: VersionedRef;
  gateway_profile?: VersionedRef;
  beta_features: string[];
  sampling_capabilities: DataRef;
  auth_binding_ref: DataRef;             // 引用身份/方法；不含凭据值
  digest: Digest;
};
```

能力是这组绑定与实际 model 的交集。旧 API 上某模型允许的温度范围，不能推导同版本所有模型也允许；未知能力显式为 unknown。直接 HTTP 实现记录自己的序列化代码版本，不能因为没有 SDK 就略过合同。

`RequestSeal.provider_wire_profile` 固定本次 profile 与实际 rendered messages、工具和 native options 的解释。发送插件在语义封口前完成版本投影、参数意图冲突诊断与预算；实际序列化后再核对 effective headers/body 与已封口绑定一致。SDK、hook、网关补默认值导致的不一致会形成可定位的错误或新 revision。已封口消息不被在途热更新改写。

切换 API、SDK、模型、beta、消息结构或有语义影响的网关策略，产生新 request_id/revision 并重新排队。凭据常规刷新按既有 transport attempt 合同处理。profile 与实际前缀解释绑定；不能只因摘要相同就宣称 KV 可复用。物理 engine admission 身份继续按 [§13](inference.md#spec-13) 确定，协议版本或 alias 改名不创建额外计算容量。

<!-- /migrated-contract: SPEC:47.3 -->

<a id="spec-47-4"></a>
<!-- migrated-contract: SPEC:47.4 -->
## 47.4 参数意图与消息投影

全局配置保存模型/任务的意图，各 provider 负责实际表达。Claude 新版对三个采样字段采用 forbidden 能力：最终 wire 中没有键，`null`、零或默认值都不是“省略”。共享网关也要通过实际出口的契约用例，避免上游删除之后又由公共 default 插回去。允许的本地字段保持原值，不能为修 Claude 400 而从框架通用配置删除整个采样能力。

调用方明确要求低温时，返回结构化 capability mismatch，包含作用域、请求意图与真实可选路径。主体可以自主选择仍可用的旧绑定、其他模型、重新设计任务或采用新的输出验证方法；选择结果成为可查的下一 revision。提示、schema 和重试可以服务任务目标，但不得标记为等价恢复低温采样。普通默认注入则由 serializer 修正，不要求人类逐次选择。

原始 session、角色、引用与富媒体附件保持原样。发送给具体 provider 的是带来源映射的投影。user text 的 citations 可从 wire 投影移除而保留其来源事实；assistant 合法 citations 和 citations.enabled 不受此规则连带清空。assistant 直接 document 需要重建符合来源语义的输入表达，或以明确错误让执行者调整；不能为了消除 400 静默修改原始发言者或虚构一次 user 消息。嵌套结果按该版本实际 block 合同校验。

所有实际使用的路径共用对应版本的编解码依据，包括普通、流式、重试、token count 和 Batch。Batch 按创建时的请求合同保存输入与回执，之后更换插件仍按原绑定读取结果。未知可扩展事件/枚举保留原始证据并按协议区分可忽略附加项与影响完成/工具语义的未知项；后者保持可诊断状态，不伪造完成。

<!-- /migrated-contract: SPEC:47.4 -->

<a id="spec-47-5"></a>
<!-- migrated-contract: SPEC:47.5 -->
## 47.5 鉴权、代理和额度解释

鉴权方法属于实际 platform client/gateway 绑定。选择 API key、auth token 或 workload/profile 来源时，以一个明确的有效凭据和相应 precedence 解释重试与复制客户端。网关拥有一层鉴权、后方服务拥有另一层时，分别定义每一跳消费和转发的字段，避免两个不同凭据意外同时送达 Claude。凭据内容不进入 prompt、持久 profile digest 的明文材料或普通日志，脱敏同时认识两种 header。

SDK 新默认使用 Bearer 不表示旧 header 被服务端废除。原有 proxy 可由适配器按真实合同迁移；Foundry 等例外按独立绑定表达。基础 Anthropic/AsyncAnthropic 的单凭据构造规则与 API 支持哪个 header 是不同层面的事实；AnthropicAWS 等平台客户端保留各自 precedence，不能套用统一的多凭据报错规则。版本头回退不能撤销 SDK 的构造/类型/鉴权行为变化。

[§42](quota-resources.md#spec-42) 实际采用的 Admin/Analytics 解析器按版本分派。特别是 key 的 `scope.workspace_id`、organization scope 与 Default Workspace 要保留真实区别，字段缺失不是“全部可用”或某个默认身份。API key scope、rate-limit 配置、订阅 usage、历史账单和实时计算许可继续分开；新字段映射只改变其正确读法。

<!-- /migrated-contract: SPEC:47.5 -->

<a id="spec-47-6"></a>
<!-- migrated-contract: SPEC:47.6 -->
## 47.6 可安装感知、直觉与自主升级

Provider 可自动依赖以下分别可安装、可查阅的单元，命名为设计示例：

- `perception.provider.effective-protocol`：观察已装代码/SDK、实际请求版本、非秘密鉴权方法、schema 错误与部署变化。
- `awareness.provider.protocol-drift`：SDK 刚升级后出现参数/角色/鉴权错误时，想到核对实际 wire profile、官方迁移和代理链；它是可修正直觉，不能断定每个 400 都源自升级。
- `skill.provider.compatibility-diagnosis`：查版本、构造最小请求、比较合法/非法样本、重现真实代理路径，并形成修复证据。
- provider 的版本 codec、迁移器和可控诊断执行代码：履行实际读写、运行和恢复合同。

主体接收更新消息或上述感知事件后自主选择是否更新，child 执行研究、修复、移植和验证。候选准备、观察与恢复的具体策略仍由安装的 awareness/Skill/插件提供；内核不因此固化五阶段升级意识。已封口请求和在途效果所需的旧 generation 保留到可以真实交接，SDK 自动隐藏请求、compact 和 token-count 调用仍进入适用的推理/资源账本。

若采用的 SDK 删除客户端 tool-runner compaction，调整对应适配层；Shenko 的 budget-compact 与 snapcompact 继续作为两个独立正式插件存在。使用供应商服务端 compact 是另一个明确能力，受原始历史、实际上下文状态与额度合同约束。无模型或新 provider 失败时，确定性配置/自解释/恢复入口仍可用。

<!-- /migrated-contract: SPEC:47.6 -->

<a id="spec-47-7"></a>
<!-- migrated-contract: SPEC:47.7 -->
## 47.7 兼容验收

| 编号 | 场景 | 通过条件 |
| --- | --- | --- |
| PV01 | 2026-10-21 Messages/Batch 请求被默认配置或网关加入三个采样字段；同时运行本地模型请求 | Claude 最终 wire 完全省略相应字段；明确低温意图有可查裁决；本地支持的原生采样字段完整透传，实际请求结果与报告吻合 |
| PV02 | 旧/新 API、不同 model、SDK major、平台 client 与 gateway 的组合发生变化 | 有准确版本矩阵、能力来源与有效 wire；SDK major 默认变化可感知；旧版本状态按事实解释，重新封口/排队且不虚增资源或缓存命中 |
| PV03 | assistant 直接文档、服务端工具内嵌文档、user/search_result text citations、assistant citations 和 citations.enabled 混合历史 | 正负样例按各自 role/nesting 合同判断；Messages/token-count/适用Batch路径一致；原始历史及引用可还原，合法例外保留 |
| PV04 | 默认 Bearer、旧 X-Api-Key proxy、两层网关鉴权、重试与客户端 copy/credential precedence | 每跳身份和实际有效方法明确；服务端只收到所需凭据；日志脱敏完整；真实代理调用成功，切回版本头不会被误报为已回退SDK鉴权 |
| PV05 | 采用 Admin/Analytics 的额度插件从旧响应迁至新响应 | 对已采用字段有版本化 fixtures 与真实路径证据；Default Workspace/organization scope、rate limit 与 usage不混淆；缺失字段保持 unknown/不适用 |
| PV06 | Provider SDK 更新期间有已 seal、排队、流式与结果未回收请求，随后故障回退 | 旧请求按原profile完成或真实终止，新请求用新revision；旧闭包、效果及结果可解释，消息/工具不重复，取消和计算许可仍按实际回执处理 |
| PV07 | 升级后出现400或代理鉴权失败，主体有相关感知/awareness/Skill依赖 | 自动定位真实问题、研究并修复或选择可用绑定，回到原任务验证；日常决策自主，错误分类和新能力可查询；两种本地compact插件仍独立可用 |
| PV08 | SDK去除方法/类型、加入未知事件，或仅模拟测试成功且真实凭据尚缺 | 编译/codec、适用schema负例、真实流式/工具/代理路径和行为回归分别记录；unknown可追踪；报告如实区分fixture与真实调用，完整目标按适用义务继续推进 |

真实测试采用当前账户支持的 model、API、SDK 与 endpoint，记录精确绑定和被测候选。官方文档支持的行为不等于本产品已经通过；本次只增加规格与研究资料。

<!-- /migrated-contract: SPEC:47.7 -->
