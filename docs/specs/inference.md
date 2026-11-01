# 推理 broker 与物理资源合同

本文是 `shenko-broker` 的功能合同：受控推理如何进入同一入口、provider 如何以开放装配注册、物理资源域与 KV 池如何核算、流式输出如何被真实分批与公开、以及错误与未知结果如何成为可查事实。本文描述**接口与行为**，不替代 [SPEC](../../SPEC.md) 的原文要求；文中标为"已实现"的部分指本仓库当前源码，标为"未验证"的部分明确缺少运行证据。

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
- 两个不同域声明同一 `engine_incarnation` 会被 `ResourceDomainRegistry::resolve` 拒绝（"engine incarnation … is already bound to resource domain …"），并由 `incarnation_conflicts()` 报告。
- 未知结果被隔离而不是释放：`ResourceDomain::quarantine` 记录欠账，`effective_compute_capacity` 从可用预算里扣除被隔离欠账与可强制的外部预留；只有原始终止证据才能通过 `reconcile_recorded` 归还。

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
