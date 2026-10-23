# 原生推理后端接入研究：vLLM、1Cat-vLLM、NInfer、Strata

研究日期：2026-10-22。范围：公开源码和官方文档核验，供 Personal Agent 产品 SPEC 使用；未部署模型、未作运行时性能实验。本文的“建议”是产品设计，不冒充后端已有功能。

## 1. 结论

四个后端可以共享任务身份、输入输出事件和资源记账框架，但不能共享一个假装所有后端都能 `pin/park/resume(slot)` 的缓存实现。每个原生插件必须同时说明：**具体构建、具体传输、具体模型/连接器配置、实际可调用操作及保证等级**。OpenAI-compatible HTTP 仅是其中一种传输；它不充分表达这些后端的能力。

已有 NInfer、Strata 固定版本判断继续成立：请求内抢占不是单并发可运行的前提；有限 generation 之间的公平轮转就能让主意识与孩子持续推进。新核验最有价值的增量是：

1. **vLLM 的内部能力已经超出普通 Chat HTTP schema，而且不同原生入口并不等价。** 有版本的 KV hints 在 Python AsyncLLM、Rust请求结构与 gRPC 转换中真实存在；所查 Rust HTTP generate 转换却明确写入 `kv_hints: None`。不能从内部字段存在推断所有接口均可透传。[V3][V4][V5][V6][V18]
2. **fork 名称不能代替 capability negotiation。** 1Cat 当前 Chat schema 没有上游同版本主线的 `session_id`；它也不具备上游所查 RL router 的 `/abort_requests` 路由。未知扩展字段可能被接收而忽略，HTTP 200 不是能力确认。[C2][C3][C4][V1][V7]
3. **取消提交成功与执行资源已释放必须分开。** 所查 vLLM abort 链路最终等待把 ABORT 发送给 EngineCore；HTTP 回包里的数量不是 GPU 已完成取消的确认。[V7][V8][V9]
4. **50/50 是对称负载下算法应自然达到的目标结果。** 它不是硬编码配置，也不能在后端没有分区/固定保留能力时假称已经设置了物理 KV 配额。

## 2. 固定版本与名字

| 后端 | 本次核验的仓库与提交 | 最新可见正式发布 | 使用边界 |
|---|---|---|---|
| vLLM | `vllm-project/vllm`，`c722f3a5368a9eed2b90c1a14b8eaf17acba3e93`，提交时间 2026-10-22 18:37:41Z | v0.32.0，2026-10-18 | 下文源码结论默认指该 main SHA，不自动属于 v0.32.0 安装包 |
| 1Cat-vLLM | `1CatAI/1Cat-vLLM`，`1d7321bf2d35ac8970870dc51bc1f7405517c3ca`，提交时间 2026-10-22 08:31:53Z | v1.5.1，2026-10-03 | README 计划改名 1Cat-LLM，当前核验仓库仍为 1Cat-vLLM；不应混淆同名或其他 fork |
| NInfer | `Neroued/ninfer`，`64d10fbf9f8e6bfc79ab0570b94674c5b9997929` | 本次沿用用户指定提交 | 不把主线未来变化推回此版本 |
| Strata | `Niko1221/Strata`，`011c9e600a93bbc9fd451c563501b5831c32bbca` | 本次沿用用户指定提交 | BATCHING 与引擎协议须按对应构建核对 |

版本身份由 GitHub 仓库/commit/release API 核验。[V0][C0] 产品应记录实际运行二进制的版本及构建指纹，无法识别时标记未知；不得以“响应头看起来像 vLLM”认定功能。

## 3. 原生能力矩阵

| 维度 | vLLM 所查主线 | 1Cat-vLLM 所查主线 | NInfer 固定版 | Strata 固定版 |
|---|---|---|---|---|
| 原生接入面 | Chat HTTP 扩展、Python AsyncLLM、Rust原生请求与 gRPC、部分 dev/admin 路由；每一入口能力不同 | fork 的 Chat/Anthropic、SM70 诊断、engine接口与 dev/admin 路由 | Chat/Responses/Anthropic 原生扩展、Prometheus、JSONL调度记录；内部 Engine 不等于全部 HTTP 可控制 | HTTP服务配置 + 原生引擎 stdio 协议；slot控制不能假装是任意 HTTP 字段 |
| 请求/会话身份 | `request_id`、`session_id`；后者不是驻留担保 | 有 `request_id`；所查 Chat schema 未声明 `session_id` | Responses 链内部派生 session key；`prompt_cache_key` 不是此 key，HTTP 不暴露内部 key | 原生槽位编号、独立会话状态；`BDONE` 后槽可空闲而会话仍保留 |
| 缓存/亲和性 | APC、`cache_salt`、`kv_transfer_params`；`data_parallel_rank` 在原生 GenerateRequest，不能当所有入口都有 | APC 与 `cache_salt`、`kv_transfer_params`，按 fork 配置；不能继承上游新能力 | exact-prefix 身份 + KV/递归状态等完整匹配；显式/隐式共享前缀提示 | 每槽会话状态；主机 conversation parking 与 GPU residency 分开 |
| 动态KV提示 | `KvHintsEnvelope` + 连接器解释的有版本 actions；SDK/gRPC支持证据，所查RustHTTP入口不转发 | 未核验到等价的当前上游 envelope 接入，不能宣称存在 | HTTP cache hints 不保证墙钟驻留；内部 StateImage/Host 调度不是外部 pin API | 引擎 slot 命令真实存在，但不能推导出任意 KV 页预算 API |
| 暂停/恢复 | engine-wide pause 的 abort/wait/keep；sleep通常丢KV；连接器可卸载KV块；均不能一概称单会话无损park | 相应pause/sleep API要按fork源码及运行配置；没有所查上游abort路由 | 稳定边界内部 pause，Snapshot 或 Replay；fresh admission不任意抢占resident | `BSTOP` 下一窗口结束；`BYIELD` 适用prompt读取下一chunk，部分进度保留；有配置/执行路径例外 |
| 取消 | SDK abort；dev模式下HTTP `/abort_requests`；提交回执不是物理释放确认 | SDK abort；断开传播/外部网关取消需单独验证 | 断开检测与代理传播；Responses cancel 路由明确不支持background取消 | `BSTOP`，最终 `BDONE ... cancel` 是协议上的终止边界 |
| 观测 | running/waiting、KV比例、prefix token hits/queries、preemptions、可选per-request metrics等 | 增加 `/v1/sm70/acceleration` 配置报告，Anthropic可转译真实cache-read usage | 物理每池KV page groups、device state slots、Host bytes、Snapshot/Replay、请求级调度事件 | BATCHING协议的slot/admission/token/done事件、引擎报告；完整HTTP指标面需按部署补验 |
| 明确不保证 | 稳定会话ID不保证cache命中；接受hint不保证执行；sleep不保证留KV；工程版本间变化 | HTTP200不保证识别字段或成功重置cache；诊断显示已配置不保证某请求真的走加速kernel | `max_context`不等于每请求独占KV；TTL不保证驻留；内部暂停不等于低延迟用户抢占 | 不能按“领域不同”禁止并发；park到Host不等于GPU驻留或并行decode |

矩阵依据：[V1]—[V15]、[C1]—[C8]、[N1][N2][S1]。未知/未核验表示证据不足，不表示后端永远不可能支持。

## 4. vLLM 与 1Cat 的接口细节

### 4.1 必须保留的实际字段及语义

上游 Chat schema 包含 `priority`、`request_id`、`session_id`、`cache_salt`、`kv_transfer_params`、`vllm_xargs`；priority 数字越小越优先，非零值要求相应 priority 调度配置。官方 v0.32.0 文档还声明 `X-Vllm-Priority` header 可覆盖 body 优先级。不得在产品自身使用“数值越大越紧急”时直接原样映射。[V1][V16]

`cache_salt` 参与前缀隔离，不是槽位、保留配额或会话锁。APC 复用相同前缀，主要节省 prefill；命中也依赖 token、LoRA、多模态等实际身份。缓存可以被驱逐。保持同一人设文本但重排tool schema/历史，并不保证同一token前缀。[V2]

1Cat 的 Chat schema 明确有 `priority/request_id/cache_salt/kv_transfer_params/vllm_xargs`，但没有所查上游的 `session_id`。基础模型使用允许额外字段的兼容策略，并记录被忽略字段；因此不能用“请求没报错”作为扩展字段探测成功。[C2][C3]

### 4.2 KV hints：是原生扩展的实用范例，不是万能缓存API

上游定义：

- envelope：`protocol_version, message_id, actions`；
- action：`action_id, action_type, action_version, payload`；
- 由实际实现组件验证 action-specific payload。[V3]

Python `AsyncLLM.add_request` 接受 `kv_hints`；Rust `GenerateRequest` 保存并转发；Rust gRPC转换确实把 protobuf hint 转入生成请求。KVCR tier manager 会接收请求hint，并检查router能力。[V4][V5][V10][V18]

但同一提交里的 Rust HTTP generate 转换写着 `kv_hints: None`，Python Chat schema也没有声明该字段。产品必须把“此构建具备能力”与“此传输可以调用它”分开记录。连接器内部的 transfer pin 也不能被改名成“任意 Agent GPU KV 永久保留”。[V1][V6][V10]

推荐允许 provider 插件提供 HTTP、gRPC、本地SDK或companion等多个传输，并按实际选择生成可发现的schema。需要修改后端或编写插件才能取得的能力，应以明确能力缺口交给 Agent 自行研究实现；不能静默伪造一个已成功的调用。

### 4.3 Pause、sleep、reset 和 abort 的区别

上游 `/pause` 是engine级控制：abort取消所有inflight；wait排空；keep冻结并等待resume。abort/wait暂停期间拒绝新请求，keep可接受但排队。它不是“暂停指定孩子，让主体继续”的默认操作。[V7]

sleep level1将权重卸至CPU并丢KV；level2丢权重和KV；pause后释放KV也可能要求醒来重算。相关dev路由需要特定配置，sleep还要求启用对应功能。把sleep用作每轮主/子轮换会违背保留缓存的目标。[V11][V12]

上游 `/reset_prefix_cache` 返回 `{success:bool}`，活动请求或offload引用可以让它失败。1Cat同名路由忽略实际布尔结果并返回HTTP200。产品必须解释真实结果，不得把不同后端的200统一记成“cache已清空”。[V13][C5]

上游 `/abort_requests` 若不提供request IDs会针对所有跟踪的请求；插件必须在“单请求取消”操作中验证非空ID，防止意外变成全局取消。API返回的 `aborted` 数量是提交的数量，底层最终只是发送ABORT消息给engine，不能证明GPU已执行完取消。[V7][V8][V9]

建议取消状态至少为 `requested → accepted → terminal/quiesced`，另有 `unknown`。网络断开、API200、取消消息入队都不能自动归还物理推理许可。若现有服务未暴露可核实的结束边界，使用其可信companion/终止事件，或保守维持未知占用并进行健康检查；不要靠短sleep伪装确认。

### 4.4 Offload和指标必须保留单位、缺失与实验性

上游OffloadingConnector支持把完成KV块放至CPU及可选后级存储。请求级 `kv_transfer_params.max_load_tokens` 可限制外部加载token，0禁止外部加载但仍允许GPU命中；官方明确它是experimental。它不是整条请求的CPU快照，也不自动涵盖所有混合模型的递归状态。[V14]

可选per-request metrics包含queue、TTFT、generation、ITL等；字段不可用时为null。TTFT起点是后端定义的调度起点，不是客户端消息到达时间；stream/n>1等还影响是否可取。Prometheus `kv_cache_usage_perc` 的1表示100%，prefix查询与命中是token数量而非请求数量。只拿到比例时，不得反推出绝对池大小。[V15][V17]

1Cat的 `GET /v1/sm70/acceleration` 返回配置层的加速报告。官方说明该报告不能证明某次请求确实走了指定kernel；路由日志/计数才是进一步证据，而Python dispatch计数又不随每次CUDA graph replay递增。compilation cache、CUDA graph与KV cache必须分别记账。[C6][C7]

其Anthropic端可从真实的 `prompt_tokens_details.cached_tokens` 映射 `cache_read_input_tokens`，需要 `--enable-prompt-tokens-details`；没有底层数据时不编造数值，也不杜撰cache creation。该行为值得直接借鉴。[C8]

## 5. NInfer 与 Strata：保留已核验合同，只增加可调用面澄清

### NInfer

`max_concurrency`是启动固定的1—8条执行lane；`kv_capacity`是共享Main Text KV池；额外Device StateImages和Host backing另有配额。不能把C、KV token总量、StateImage数量写成一个“slot数”。内部压力暂停发生在稳定提交边界；Snapshot不足时可Replay，恢复不重复发布已提交token但仍消耗计算。[N1][N2]

HTTP缓存方面，Chat/Responses支持implicit/explicit共享前缀write candidates；`prompt_cache_key`明确不是Engine session key或前缀身份，TTL仅是提示，不能保证墙钟驻留。Responses使用 `previous_response_id` 重建本地历史，并内部传递有界session key；HTTP schema不暴露这个内部key。Responses store为进程内LRU，重启会失去，不能当Agent持久session真相源。[N2]

可用指标比“OpenAI usage.cached_tokens”丰富：每个KV池page groups、Device StateImage slots、Host used/reserved/capacity bytes、preemption/snapshot/replay，以及JSONL请求级pause/restoration/recovery事件。Host reserved已包含在used，不可相加。恢复到旧frontier与首次新的执行进度分别可观测，适合衡量恢复实际收益。[N2]

已有HTTP断开取消契约依赖代理也关闭上游连接；TCP仍ack的客户端不能凭应用不读判断失联。Responses的cancel子路由明确不支持background execution；不能因为路径与云API类似就当它实现同样的取消功能。[N2]

### Strata

`parallel:N` / `--batch N` / `--slots N`增加会话状态，同时可能共享权重读取成本，也可能挤占专家缓存。文档自身有不同硬件/并行配置的测量差异。因此串行/并行选项应由当前模型、硬件、prompt负载和延迟目标的实测选择，不能根据任务话题不同直接断言expert thrashing。[S1]

引擎stdio有真实的slot协议：`BGEN/BGENI`、`BADM`、`BT`、`BDONE`、`BSTOP`、`BYIELD`。后两者不是相同操作：BSTOP在下一窗口结束该slot；BYIELD针对prompt读取在下一chunk让出，部分进度留存，并有执行路径/配置例外。BDONE表示slot可再用但可保留会话。Host conversation cache通过容量和条目配置停车/恢复，与GPU当前resident及parallel decode是三件事。[S1]

若HTTP服务已拥有引擎stdin/stdout，Agent插件不能抢占这条通道另做一个不知情调度器。应集成在服务/companion层，或通过服务真正提供的扩展进行透传；用独立进程协调并保持单一engine所有者。

## 6. 建议写入产品SPEC的合同

### 6.1 能力声明与透传

Provider插件应暴露可版本化的能力目录，每项至少包括：

| 字段组 | 要表达的事实 |
|---|---|
| 身份 | backend family、build/version、model/tokenizer/template fingerprint、transport、connector配置、engine incarnation |
| 能力状态 | available、requires_configuration、unsupported、unknown；unknown不能显示成false或成功 |
| 作用域 | request、conversation、engine、device pool、connector；全局操作不得伪装为单任务操作 |
| 保证 | advisory、best_effort、acknowledged_effect、reserved等；已提交/已接受/已完成分别记录 |
| 资源 | 计算lane预算C；KV/StateImage/Host等类型化容量与占用K；measurement/estimate/unknown及单位 |
| 扩展 | 命名空间、schema版本、适用传输、原生字段与原始返回、配置前提、变更成本 |

例如 `native.vllm`、`native.1cat`、`native.ninfer`、`native.strata` 可以各自保留字段和操作；共享部分只管理请求身份、预算、事件和引用，不把所有模型状态压成“一个cache slot”。这不是建议实现一个现在不存在的后端接口，而是要求插件如实描述现有接口。

声明支持的原生字段必须端到端转发并留可审计轨迹；无法支持时返回明确能力缺口。对后端本身“可接受但忽略”的兼容行为，不能据此标记实现成功。Agent可委派子Agent补插件或移植后端能力，主/子都能发现同一能力超集，工具暴露按角色和当前任务渐进提供。

### 6.2 同一个物理后端，共用资源域

URL、模型别名、OpenAI入口、Anthropic入口、代理地址乃至不同插件实例可能指向同一resident Engine。Provider应允许显式绑定同一resource domain，并通过可取得的server instance/device pool信息协助识别。凭相同模型名不能自动合并不同机器；凭不同URL也不能自动分开同一GPU。

所有受本产品控制的主loop、children、memory sidecar、摘要、模型驱动的工具发现都经过该域预算。外部未受控客户端仍可能占用资源，应从后端压力指标保守调整；不能宣称本地信号量已经控制整个外部世界。

### 6.3 C=1时持续推进与50/50自然分配

一个主意识逻辑序列只保留一个正在执行的主generation。每次有限generation结束，保存足够的协议状态并把下一步放回队列；该序列持续有可执行后继，调度器让children公平取得计算片。这里没有将持续思考替换成低频Tick。

generation quantum必须考虑token上限、模型的thinking/assistant续写、已有工具调用完整性和prefill成本。不能把截断的JSON工具参数当完成调用，也不能丢掉被截断的模型状态后声称无损续写。优先选择后端真实支持的续写/原生token状态；否则明确重建及cache-miss成本。

KV分配算法以任务持续价值、等待年龄、下一次执行需求、可恢复成本、真实资源压力作输入。对主/子需求、优先级、收益和状态大小都对称且可容纳的场景，算法应收敛到约50/50的可用独立状态预算；共享prefix只计一次。工作不对称时自动偏移，不设主体永远15%或50%的教条。

应区分三种实现等级：

1. **后端真有配额/保留控制**：在协商的语义内实现可验证保留。
2. **有缓存hint、亲和性或Host backing但无硬保留**：算法可优化目标，展示实际命中/恢复/驱逐，明确best effort。
3. **仅能取得普通生成API**：C=1有限generation公平轮转仍工作，但不承诺物理KV50/50，也不伪造slot；需要通过原生插件升级能力。

资源池未知时自动运行并以实测调整，不强迫用户填写KV大小。若达到50/50验收必须由原生能力支持，则由插件明确报告哪个功能缺口阻止该保证，并允许Agent自行补齐。策略插件可演进，内核不内置某家后端的长期固定配额。

## 7. 必要验收场景

| 场景 | 必须观察到的结果 |
|---|---|
| 同一GPU的两个URL/协议入口 | 同时提交不能绕过同一C预算；任务数、active generation、resident/parked状态分开显示 |
| 对称C=1主/子负载 | 两者持续有进度，无饥饿；支持保留控制的后端达到约50/50独立状态预算；不支持时显示best effort与实际miss，绝不伪造达标 |
| KV/Host压力与恢复 | 分别记录驱逐、reload、Snapshot、Replay、computed prefill；不把重放计入新输出，不承诺zero-prefill |
| 未知或被忽略字段 | 给1Cat发送上游独有字段、给不支持hint的传输发送hint，能力探测不能仅以HTTP200通过；调度状态不能因忽略字段而错误变成pinned |
| 扩展语义差异 | priority正负方向正确且配置可用；prefix reset依据真实success，1Cat200仅为请求回执；全局pause不用于取消单个child |
| 取消、断网、代理未转发断开 | cancel请求发出不立即释放C；可靠终止后释放；无法确认保持unknown并恢复协调；空ID不触发全局abort |
| Engine重启/模型模板变化 | 旧request/session/native handle标记过期；持久任务与session仍可重建；缓存身份失配计为真实miss |
| Strata并发选择 | 对当前硬件测串行与多个并发值的主响应延迟、总吞吐、prefill和状态成本，再选择策略；不能仅依任务领域禁止并发 |
| 原生观测完整性 | null保留null，指标单位和时间起点保持；配置报告不当执行证明，编译缓存/CUDA图/KV三者分开 |
| 端到端精确续写 | tool schema/JSON顺序、thinking策略、tokenizer/template、media/LoRA身份保持或明确失效；截断工具调用不执行；续写后无重复外部动作 |

这些属于后续开发阶段的集成验收计划。当前未执行测试，不能据此给出具体毫秒响应或吞吐保证。

## 8. 固定版本一手资料

[V0]: https://api.github.com/repos/vllm-project/vllm/commits/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93
[C0]: https://api.github.com/repos/1CatAI/1Cat-vLLM/commits/1d7321bf2d35ac8970870dc51bc1f7405517c3ca
[V1]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/vllm/entrypoints/openai/chat_completion/protocol.py
[V2]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/docs/design/prefix_caching.md
[V3]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/vllm/v1/kv_hints/protocol.py
[V4]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/vllm/v1/engine/async_llm.py
[V5]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/rust/src/llm/src/request.rs
[V6]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/rust/src/server/src/routes/inference/generate/convert.rs
[V7]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/vllm/entrypoints/rl/online/api_router.py
[V8]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/vllm/v1/engine/async_llm.py
[V9]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/vllm/v1/engine/core_client.py
[V10]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/vllm/v1/kv_offload/tiering/kvcr/manager.py
[V11]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/docs/features/sleep_mode.md
[V12]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/vllm/entrypoints/launchers/api_server/routers.py
[V13]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/vllm/entrypoints/serve/dev/cache/api_router.py
[V14]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/docs/features/kv_offloading_usage.md
[V15]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/docs/features/per_request_metrics.md
[V16]: https://docs.vllm.ai/en/v0.32.0/serving/online_serving/openai_compatible_server/
[V17]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/vllm/v1/metrics/loggers.py
[V18]: https://github.com/vllm-project/vllm/blob/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93/rust/src/server/src/grpc/convert.rs
[C1]: https://github.com/1CatAI/1Cat-vLLM/blob/1d7321bf2d35ac8970870dc51bc1f7405517c3ca/README.md
[C2]: https://github.com/1CatAI/1Cat-vLLM/blob/1d7321bf2d35ac8970870dc51bc1f7405517c3ca/vllm/entrypoints/openai/chat_completion/protocol.py
[C3]: https://github.com/1CatAI/1Cat-vLLM/blob/1d7321bf2d35ac8970870dc51bc1f7405517c3ca/vllm/entrypoints/openai/engine/protocol.py
[C4]: https://github.com/1CatAI/1Cat-vLLM/blob/1d7321bf2d35ac8970870dc51bc1f7405517c3ca/vllm/entrypoints/serve/rlhf/api_router.py
[C5]: https://github.com/1CatAI/1Cat-vLLM/blob/1d7321bf2d35ac8970870dc51bc1f7405517c3ca/vllm/entrypoints/serve/cache/api_router.py
[C6]: https://github.com/1CatAI/1Cat-vLLM/blob/1d7321bf2d35ac8970870dc51bc1f7405517c3ca/vllm/entrypoints/serve/sm70/api_router.py
[C7]: https://github.com/1CatAI/1Cat-vLLM/blob/1d7321bf2d35ac8970870dc51bc1f7405517c3ca/docs/serving/sm70_release_profile.md
[C8]: https://github.com/1CatAI/1Cat-vLLM/blob/1d7321bf2d35ac8970870dc51bc1f7405517c3ca/docs/serving/online_serving/anthropic_cache_usage.md
[N1]: https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/maintainer/resource-scheduling-and-context-cache.md
[N2]: https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/serving.md
[S1]: https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/BATCHING.md

另：正式发布记录分别见 [vLLM v0.32.0](https://github.com/vllm-project/vllm/releases/tag/v0.32.0) 与 [1Cat v1.5.1](https://github.com/1CatAI/1Cat-vLLM/releases/tag/v1.5.1)。gRPC实际透传证据见 [V18]；它与Rust HTTP不透传证据[V6]应一起引用。

