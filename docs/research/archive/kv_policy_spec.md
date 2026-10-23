# 多上下文驻留与 KV 调度规格

日期：2026-10-23（Asia/Shanghai）。只研究与制定规格，没有实现和硬件实测。本文将用户的 C=1、多上下文驻留与自动份额分配作为设计起点；后端实际支持程度必须如实标注。

**最新用户裁决，覆盖早期 fixed-half 优先表述：50/50 是自动算法在合适平衡条件下应该自然达到的分配情况，不是用户手工设置比例或单独提供固定模式便算满足。** 算法根据负载、驻留需求与恢复成本自动调整；条件变化时不能永远机械各半。固定比例模式仅保留为可选覆盖和评测基线，不是唯一方案、默认行为或单独的首版发布门槛。用户未接受固定 15–20%，也未取消自动平衡场景。KVPolicy 可替换，不写死到永久不可改 kernel。

## 1. 核心契约

**计算许可与上下文驻留完全分开。** `C=1` 仅表示同一受控资源域最多一个推理请求处于计算/可能仍在计算的生命周期；主体上下文与多个 subagent 上下文可以同时保留，在每次计算 quantum 之间轮换。释放 request permit 不应顺带清掉 KV；保留 KV 也不应占用 request permit。

当前正式策略要求与可选对照：

1. **Auto（正式要求）**：根据 C、可知或未知的池容量、当前上下文大小、待运行任务、紧急程度、缓存恢复成本和实际命中反馈，选择驻留集合、执行顺序、quantum 与份额。在主/子工作集需求充分、边际驻留价值与恢复成本平衡的条件下，应自然接近各半；这里“子”是所有 children 合计。目标是接近该工作负载下的优良调度，不承诺未知未来/容量下数学全局最优。
2. **Fixed half（可选覆盖/基线）**：可表达主体 1/2、子 agents 集体 1/2 的静态对照，分别明确严格分区与可借用语义。这有助于检验 auto 是否退化，但手工模式跑通不构成自动算法验收通过。

主 loop 可在 owner 预授权的范围内自由改变策略、主体份额、亲和偏好、借用政策和 quantum。Host 始终执行硬约束：实际请求容量、主体唯一性、已确认的物理容量、信任域、正在读取/搬运的数据完整性和有效租约。main 的策略建议不能把 unknown 改成 known，不能把 cancel-requested 改成已经释放。

这里的 broker/planner、model provider、WASM runtime、SQLite history、UI 都是可替换 bootstrap plugins；本文定义的是服务契约，不要求编译进 kernel。极小 kernel 仅提供生命周期/恢复与 capability 硬门限。策略插件可替换，已经发出的请求、驻留 lease 和 uncertain 状态不能因替换而从资源账本消失。

## 2. 可用池不是显卡标称显存，也不只是一串 token

调度器应使用资源向量，而不是一个“slot 数”统一代表所有东西。

| 资源 | 计量 | 与 C 的关系 |
|---|---|---|
| Request capacity | active/reserved/uncertain attempts | 硬上限 C |
| Main/其它 typed KV pools | 真实 page groups/bytes，或后端报告的 token 等价单位 | 可由多个驻留上下文共享，不等于 C |
| Recurrent/continuation state | StateImage 个数及其实际 bytes | 混合模型恢复需要；可能是独立瓶颈 |
| Host context backing | 已分配 bytes，含在途 destination reservation | 支持驻留换出；不等于全部进程 RAM |
| Disk snapshots | 已发布 bytes、I/O 预算与版本兼容性 | 可选冷层；恢复不是零成本 |
| Execution workspace / speculative peak | 下一执行单元需要的临时资源 | 与模型、批大小、输出/推测配置有关 |
| Audio processing | ASR/VAD/TTS 的计算 quantum、持续流 state、音频缓冲与模型驻留 | 与主推理共享同一 GPU 时纳入物理域；音频会话数不等于一直占 compute permit |
| Model weights / 其它常驻数据 | 后端本身占用 | 先扣除，不能把它们算成可任意平分的 KV 池 |

用户填 `KV pool size` 时，配置必须写明单位和范围。后端报告 Main KV token 容量时，可以在该池内按 token page 分配；跨模型或混合 recurrent state 时，不得用同一个 token 数推导通用显存大小。

对每个真实池分别满足：已占用 + 已预约 destination + 当前执行所需增长/临时峰值 ≤ 可信容量。共享 immutable prefix 只能计一次物理占用；删掉一个逻辑 context 可能释放零字节。公平份额的逻辑归账规则和物理独占计数必须分开。

推荐默认归账：由主体创建、被子任务共同读取的稳定 prefix 只在主体分区计一次；子任务独有后缀计入子池。更一般的共享归账可配置，但不能重复计费，也不能把一个被多人引用的对象误认为可单独释放。后端无法提供这些事实时，采用保守估计并标 estimated。

## 3. 后端能力必须逐项协商

每个 adapter 提供能力 manifest，至少包括：

- capacity discovery：exact / estimated / unknown；各 pool 的单位与更新频率。
- automatic prefix reuse 与 cached-token telemetry。
- routing affinity hint、显式 context identity、固定/可替换 session lease。
- retention：无控制 / advisory hint / soft priority / 不可逐出的 hard pin。
- per-context 或 per-class quota，以及是否能原子预约/回收。
- Host/disk spill：engine 自动执行还是 caller 可显式控制，能否观察完成。
- append、fork、rewind、save、restore、close 的独立支持状态。
- recurrent/backend state 是否完整包含在恢复对象中。
- request cancel、lease close 和 transfer completion 的确认方式。

**内部统一接口可以存在，但不能凭它假装后端已经支持。** 对普通 OpenAI-compatible API 返回的仅是 hints 时，UI 要显示“软目标/实际保留未知”；不能在界面画出已确保的两块各半物理缓存。

### 3.1 已核对的 NInfer 能力

固定快照：`Neroued/ninfer@64d10fbf9f8e6bfc79ab0570b94674c5b9997929`（2026-10-16）。

- `--max-concurrency C` 是执行绑定数；`--device-state-slots H` 是额外 Device StateImages，因此总容量为 `C+H`。这直接允许 C=1 仍准备多份 state。
- `--kv-capacity` 是共享 Main Text KV 池；`--max-context` 是单序列逻辑上限，池不平均切给各 lane。
- `--host-context-mib` 是 state、KV、pause snapshot 与在途目的共同使用的 Host backing；缓存复用需要完整 KV + recurrent/hidden/backend state。
- Engine 自己处理 prefix 选择、GPU/Host 迁移、pressure pause/replay，公开 metrics 可以观察各 pool、state、host、transfer 和 replay。
- 当前已读的 HTTP/serve 契约没有 main/child 归属、每主体容量比例或 caller-controlled hard pin 分区。`prompt_cache_key` 不是 engine session key，TTL hints 不保证墙钟驻留。

因此：可以利用真实的 C=1 多 state/共享 KV/Host backing，并做低污染请求和成本观测；**不能仅靠现有公开 HTTP 宣称已强制主/子各 50%**。精确分区需要选装后端扩展/受版本控制的原生 adapter，或上游新接口。其自身已经有资源调度器，Bot 不应在不知道物理占用的情况下再发出与其冲突的逐页驱逐指令。

### 3.2 已核对的 Strata 能力

固定快照：`Niko1221/Strata@011c9e600a93bbc9fd451c563501b5831c32bbca`（2026-10-21）。

- `--conversation-cache-mib` 与 `--conversation-cache-slots` 可在 Host RAM park 多个 controller/worker 会话，单请求计算模式也可使用。复用按 exact token/image prefix，不需要客户端 session ID。
- snapshot 包含运行态、checkpoint、已使用 KV 和相关 draft state；驻留与恢复有实际 RAM/传输开销，cache byte budget 还包括切换中的 incoming snapshot。
- 短一次性调用可以使用 `strata_checkpoint:false`；全局 min-tokens 与 park-gain 可减少短请求污染/过度 capture，代价是相应后缀重算。
- `strata_prefix` 支持每 engine 一个 pinned shared prefix；这不是任意多 main/child 的独立 pinned lease，更不是 GPU 各半分区接口。
- batch slots 与 parking 是不同能力；slot0 save/restore 还有单槽、版本/配置和并行模式限制。

因此：Strata 官方模式已经覆盖“串行计算，多份历史保留”的关键用例；但 Host parking 容量不能写成 GPU KV 硬预留，min-tokens、one pinned prefix 也不能包装成通用的 50/50 quota。配置 `parallel>1` 再把 broker 设 C=1，也不能未经验证就假定 engine 一定为串行请求保留任意多个 GPU context；应测实际 solo/batch 路径。

### 3.3 其它 adapter 的边界

| 后端 | 可用控制 | 对本需求的界限 |
|---|---|---|
| llama.cpp | 明确 slot、prompt cache、slot save/restore/erase；具体语义按固定版本测试 | slot 数不是 bytes 比例；静态上下文切分、统一池及外部缓存互动要核对该版本，不能从整数 slot 推出最优分配。 |
| vLLM | prefix caching、隔离 salt；有版本/配置相关的 offload/调度能力 | 常规 OpenAI endpoint 没有通用 caller lease/主子配额；自动复用不等于主缓存不会被驱逐。 |
| SGLang + Dynamo | 实验 session_control、sticky routing、append session pin；priority radix eviction 与 HiCache 是另外的能力 | bind 只有路由亲和；open session 的状态不参加普通 eviction，不能假定还能走同一 HiCache L2 spill。append-only、idle TTL 和 close 生命周期限制仍在；没有由此自动得到 50/50 配额。 |
| 普通兼容 API | 上下文提交、限流与可能的 usage 信息 | 容量、保留位置、pin、spill、迁移可能全未知；使用软策略，显示保证等级。 |

## 4. Fixed half：可选覆盖与评测基线

本节保留精确术语用于可选配置及对照，不再把静态各半当作用户最终要求。未实现固定 profile 不等于遗漏自动分配；实现了它也不能代替下节 Auto 的验证。

### 4.1 严格各半

在已知可用 typed KV 容量 `B` 上，主分区 entitlement/cap 为 `0.5B`，子池集体为 `0.5B`，按物理 page 向安全方向舍入。recurrent StateImages、Host backing 与执行 workspace 另行满足完整恢复条件，不能因为 KV 已各半就忽略 state 瓶颈。

一个主 context 可以保留在其半池中；多个 children 的当前工作集在另一半中按 policy 保留/换出/淘汰。C=1 时只有一条实际 generation：main 请求与 child 请求在步骤边界交错。main 的缓存不因让出 compute permit 被清空；child 池内部轮换也不应侵犯主 entitlement。

主或某个 child 的上下文超过其可用分区时，选择有授权的 context window 转换、压缩、Host spill 或明确 infeasible。严格模式不能悄悄借用然后仍称严格各半。若后端不支持强制 quota，则该模式状态为 unsupported-hard / available-soft，而不是伪装实现成功。

### 4.2 各半 entitlement，可借用空闲部分

另设 `lending=enabled` 模式：各自保有一半的可收回权益，空闲部分可临时借给另一方。UI 分开显示 entitled、used、borrowed、reclaim-pending；实占短时不是严格 50/50，但合同是“需要时能在已定义边界收回”。

借用记录包含 lender、borrower、pool、amount、deadline/epoch、可撤销点、预计恢复开销和 fallback。借用不能侵占 lender 下一 quantum 必需的增长/临时空间，也不能默认认为不断运行的 main 永久不再需要自己的份额。

回收先阻止 borrower 新的非必要增长，选安全 checkpoint/terminal 边界，必要时 spill 或丢弃可重建缓存。完成复制、确认无 reader 和发布状态之前，不能把字节提前记为空闲。对不支持单元暂停的后端，最早安全回收点可能是请求结束或经确认的取消；要如实反映等待成本。

若“最终状态能装下，但 GPU/Host 交换中间态装不下”，不能互等。必须预留 in-flight destination 空间；不支持流式分段交换时，释放低价值可重建对象或选择重算。不得设计未实现的零额外空间换入算法。

### 4.3 固定模式的可调项

`main_fraction=0.5` 仅为固定对照的示例；生产 auto 不要求 owner 输入该值，也不设成绑定 C 的 `1/(C+1)`。另有 shared-prefix 归账、每 tier reserve、worker pool 内保留规则、lending/reclaim、最小 growth headroom、pin TTL 和最终 context reset 政策。

需要区分“给主体 50% 的可用 KV 字节”“给主体 50% 的计算时间”“主上下文最多 N token”。它们是三种不同配置，不能相互代换。

## 5. Auto：有硬约束的在线启发式规划

### 5.1 优化目标

优先保证主体唯一、硬资源合规、必要依赖可前进与调度公平。在这些约束下，减少可避免的 prefill/replay、传输和排队延迟；紧急前台/主体轮次可按 policy 增加权重。

算法不宣称对未知未来全局最优。产品承诺是：提供可解释、可观测、可替换的在线 planner；通过可重复工作负载和小规模离线最优 oracle 比较，让“近优”成为可测目标。真实模型、硬件上的性能数字留待实测。

### 5.2 输入

- 请求 cap C、主唯一 attempt 条件、ready queues、依赖 DAG、deadline/aging、主体/worker 公平配额。
- 可发现或 owner 指定的容量向量；信息置信度与观测时间。
- 每个 context 的完整身份、有效恢复 frontier、当前 tokens、增长预估、state 大小、共享引用、各 tier 副本与 lease/reader 状态。
- 下一次使用的已知信息：main 总有下一轮；child 是否 ready、是否等待长工具、是否一次性、是否已完成。
- 实测/估计的重算速度、恢复 bytes 与传输时间、snapshot capture 成本；不会把所有 cached tokens 当作同等节省。
- main 的策略偏好/临时保护请求，以及不能越过的 host invariants。

### 5.3 候选动作

对 adapter 真正支持的动作生成有限候选：保留 GPU、降至 Host、保存可恢复磁盘 snapshot、删除可重建状态、复用 shared prefix、调整下次 quantum、调整 main/child 份额、安排先执行哪个 eligible request。unsupported 动作不进入搜索空间。

对每个可选恢复来源估计：

`resume_cost = transfer_time + uncovered_prefill/replay_time + switch_overhead`

候选保留的价值是“预计在规划范围内省下的重算/恢复成本”，减去 capture、搬运、占用稀缺空间和驱逐其它上下文的代价。对 main 下一轮这样的确定性需求，复用机会来自调度事实；其它 child 的未来复用概率只能是估计，不能被记录成已经发生的 cache hit。

不同候选共享物理 pages 时，按实际边际可释放量和幸存 fallback 计价。释放零个当前稀缺资源的操作不应被选来解决该资源短缺。对 recurrent state 还要验证完整 StateImage 和对应 KV/frontier 能恢复，不能只保留前缀 KV 就报告完整命中。

### 5.4 决策步骤

1. **先选必须推进的工作。** 公平/紧急/依赖继承决定本轮必须照顾的 eligible 集合，防止缓存命中最多的任务永远独占计算。
2. **枚举有限配额/驻留方案。** 包括当前方案、fixed-half、按近期实际需求调整的相邻方案，以及能够满足被饿住大任务的 drain/临时借用方案。
3. **检查完整可行性。** 包括当前执行、输出增长、所有 typed pools、state、source readers、destination reservations 和复用/迁移过程的峰值。
4. **在可行方案中比较成本。** 用已观察成本区间；小规模可有限搜索，规模较大用按价值/稀缺资源排序加局部交换。planner 有有界计算预算，不能在路由器上为一次队列选择求解无上界大优化问题。
5. **加入滞回。** 收益未超过迁移成本和不确定性余量时维持原方案；设置最小 policy 保持期/切换次数上限，避免 main 每轮把份额从 30% 改到 70% 导致抖动。
6. **两阶段执行和核对。** reserve → transfer/close → confirm → publish/release；实际事实变化后重新报价，不沿用失效资源快照。
7. **反馈更新。** 缓存命中、实际 uncached prefill、transfer、deadline/公平结果回到下一规划周期；解释每次主要调度选择。

planner 是确定性的资源服务，不是另一个人格/持续 thought loop。main 负责选择目标偏好、需要探索的策略和允许的调节；engine/coordinator 在这些范围内执行本地资源决策。

### 5.5 C 与 KV 容量共同影响结果

- C=1、GPU 能同时容纳 main+多个 child：保留多份 GPU 状态，主要决定执行顺序与 child 子池替换，避免来回重算。
- C=1、GPU 只能容纳 main+一个 child，Host 较多：保留 main，child 候选在 Host 轮转；是否每轮交替取决于传输成本与公平上界。
- C>1、KV 很小：不能因计算 permit 有空就把 C 路长上下文全部准入；可以实际只运行较小子集，同时维持其它恢复点。
- C>1、KV 充足：main 仍最多一个请求，其余容量用于 workers；KV 份额不必按 C 平均分配。
- main 上下文很小、child 长任务很大：auto 可把主体保底减少、借更多给 child，前提是 main 下一轮仍有可执行恢复路径。
- 主体有紧急消息且 child 很长：提高主体下次 quantum 的优先级/恢复预算；是否取消 child 由能力与 policy 决定，不能假定透传 priority 会中断已经运行的 generation。

## 6. Quantum 与切换成本

一个 quantum 是 backend 支持的可安全让出计算的最小单元：通常为一个有限输出的请求；有真实 pause/resume 能力时才可更细。不能随意中断 token 流再称恢复无成本。

较短 quantum 改善响应和公平，但每次切换可能发生 Host restore、snapshot capture 或新后缀 prefill。较长 quantum 减少搬运，却增加其它任务等待。auto 根据观测选择输出长度/任务切片，在最大让行次数和 deadline 内优化；main 可调整偏好，但不能取消公平保证或制造父子等待死锁。

对 tool-wait 的 child，不必为“维持热度”持续发假请求。应真实保留 lease、放入 Host 或记录可重建状态。对 main，下一轮本来就存在，不需要额外 keepalive generation 伪造请求；keepalive/renew 若后端支持，应作为独立控制操作计量。

频繁保存完整状态的开销也要评估。可以保留较旧但仍兼容的 checkpoint，接受小段 suffix 重算；但这个选择必须看实际 capture/restore 和 prefill 成本，不能宣称缓存 token 越多越优。

## 7. 容量未知或用户不填写

`capacity` 是 Optional，而不是把 unknown 当作 0 或无限大。

1. 先尝试 adapter 的正式 metadata/metrics：例如 engine 配置、pool occupancy、state count、Host budget；能力支持时自动获得真实值。
2. 只得到全局使用量、没有每 context footprint 时，保留全局硬限制与 per-context 估计；不能强制声称精准两半。
3. 完全未知时，仍能运行：保留 C 的请求准入、context window 检查、稳定前缀、main 高亲和、child 去重/有界工作集和实际命中反馈；main/child fraction 解释为软保留目标。
4. 从正常工作负载渐进观察成功驻留数、命中/失配、返回的 memory-pressure 信息，不以主动 OOM 压测或不停扩大上下文作为默认探测方式。
5. 无法观测时 UI 清楚显示 unknown；不存在可信容量就不能承诺字节级硬 50/50 或不 OOM。用户仍可直接选择 fixed-half 目标，系统说明当前是 advisory 级别，并给出达到强保证需要的 adapter 能力。

主 loop 可提供自己预期的使用次序和上下文重要性，这有助于估计；它不能把愿望写成物理容量。backend 重启、模型/配置变更后所有成本与容量观测都有 epoch/revision，旧估计进入过期状态，重新校准。

## 8. 避免 parent/child 的内存死锁

即使已经释放计算 permit，仍可能发生“main pin 满内存 → child 无法执行 → main 必须等 child 的结果”。因此：

- 必要依赖在准入时要有完整的下一执行单元路径，包括 KV/state 与恢复峰值。
- pin 分为 hard pin、可撤回软保留和普通可逐出缓存；main 不能在不知道容量的情况下把所有父缓存改成永不可逐出。
- 给主/子双方设最小可推进空间，或者允许把非执行中的父 context spill 到 Host；其真相仍可由历史重建。
- 若 owner 选择不可撤销的 pin 且使依赖需求无法装下，应在创建/准入依赖时明确报 infeasible，而不是无期限排队。
- 借用回收只有在物理 release confirmed 后才计为可用；cancel/close/transfer 三种结果各自确认。
- 资源恢复时要保留“恢复到旧 frontier 后至少产生一次新进展”的空间，避免反复恢复、立刻再被暂停、永远没有新 token 的活锁。

主 loop 仍持续思考；如果某个目标因不可行内存合同受阻，它可以考虑调整策略/委派修复，不必停止整个主体，也不能无事实地声称受阻目标已完成。

## 9. Main 的策略调整接口

拟议内部契约（不是声称已存在的 HTTP backend API）：

`policy.propose(expected_revision, mode, main_fraction, tier_preferences, pin_preferences, lending, quantum_preferences, urgency_weights, horizon, expires_after, reason)`

返回 requested、accepted/clamped/rejected、effective revision、保证级别以及生效所需的 reclaim/transfer 操作。主 loop 不直接操作缓存裸指针或后端内部 slot；由 adapter 对可支持动作实施。

owner 可预授权这些范围内的自由调整，不要求每次人工确认。Host 不可被 main 解除的约束包括：C、单主体、身份/信任域、reader/transfer 完整性、可信物理上限、已授权预算、旧 epoch fencing 和既定公平活性下界。worker 可以提出调优报告，但不能绕过 main 自行更改全局 policy。

紧急物理压力下的确定性 reclaim 可以由 resource manager 按已授权策略执行，无需让 OOM 风险等待一次 main 推理；它必须回注结果，不能悄悄更改 owner 的硬边界。

## 10. 观测与界面

每个 context 应能查看：所属 main/child、reusable frontier、模型/tokenizer/template/tool revision、tier、实际/估计 bytes、pin 强度、最后真实复用、下一使用预测置信度、borrowed bytes、release/transfer pending、fallback 来源。

全局显示：C 与实际 active/reserved/uncertain；各 pool 的 capacity/used/reserved；main/child entitlement 与实占；Host spill/restore；cached 与 uncached prompt tokens；真实 replay；TTFT 中的 queue 与处理时间；policy revision 与主要选择原因。

不能混用：Host used 已包含 reserved 时再相加；context 的逻辑大小与 unique physical footprint；cached-token 数与传输字节；模型权重/experts 缓存与 conversation KV；配置愿望与后端实际达到值。缓存恢复成功可以仍有明显 H2D 延迟，命中率不是唯一性能指标。

### 10.1 实时电话/课堂旁听的资源接口

原生预留的是音频插件的注册、事件和能力接口，不是常年闲置的 GPU slot。电话与课堂的音频/转写、说话者线索及修订仍以 structured inbound 进入唯一 main loop；ASR、VAD、TTS、录音和声纹工作由 workers 执行。音频插件不能为了低延迟再启动一个绕 main 的常驻对话主体。

ASR/TTS 与 LLM 用同一 GPU 时，应声明共同物理资源域及各自模型/state/workspace，占用由实际能力计量。长连接的音频 session、持久 ASR decoder state 与实际处理音频 chunk 的 compute quantum 分开；不能让一通电话从接通到挂断始终拿住 main 唯一的计算许可。反过来，后端不能细粒度共享时，也不能伪造 chunk 可抢占，要选择独立设备/CPU、较小任务 quantum 或如实报告该部署达不到延迟目标。

实时语音 ingress、短 TTS 与主体下一轮可配置较高但有界优先级，并纳入公平/资源预算。ASR 部分结果有 utterance ID、revision、final 标志；不能把每个修订都当成全新消息启动一条主体请求。打断只影响同一主体顺序 attempt；phone plugin 不获得第二主体特权。语音延迟目标必须对具体后端与设备实测，不能从“插件槽已预留”推导 GPU 已有实时余量。

## 11. 自动验收与“近优”评估

| ID | 场景 | 可检查条件 |
|---|---|---|
| KV01 | C=1，主体和两个 child 上下文都可装入 | 同时最多一个计算 attempt；多份 lease 驻留，轮换不会无条件清空其它上下文。 |
| KV02 | 已知 B、强配额 adapter、strict half | 主与所有 children 合计各不超过其 page 舍入后的半池；其它 state/峰值资源也满足；超额需求明确处理。 |
| KV03 | 普通兼容 API，没有 pool 信息 | fixed-half 显示 soft/unknown；没有伪造 slot、bytes 或 pin 成功；请求 C 仍受控。 |
| KV04 | lending 开启，child 借空闲主份额，main 随后增长 | recall 正确阻止新增借用，安全释放/迁移后恢复 entitlement；release 未确认前不重复分配。 |
| KV05 | 最终分布可装下，中间换入换出峰值装不下 | 不死锁、不提前释放源；选择合法 fallback/重算或明确 infeasible。 |
| KV06 | 只有 StateImage 稀缺，而 Main KV 尚多 | planner 识别正确瓶颈，不驱逐一堆不能释放 state 的纯 KV 对象伪装成功。 |
| KV07 | 共享前缀有多个逻辑 owner | unique footprint 只计一次；删除一个 alias 释放零字节时账本准确；不能把仍被 reader 引用的页回收。 |
| KV08 | 主/子 token 长度不断增长，predict growth 偏差 | 下一单元有合法预约；硬 adapter 不越池，软 adapter 记录偏差并收敛配置，不把预测当事实。 |
| KV09 | 缓存很热的任务与冷紧急任务竞争 | 缓存偏好不突破公平/紧急策略；完成时记录实际重算和传输代价。 |
| KV10 | main pin 占满、必需 memory/child 等待 | 通过保底/soft reclaim/Host spill/明确不可行处理，不能形成等待闭环。 |
| KV11 | lease TTL 过期、backend 重启、tokenizer 更换 | 旧 handle 失效，历史可重建；不把 context miss 当作任务/记忆丢失。 |
| KV12 | main 每轮提出反向份额调整 | 滞回与 revision 检查避免无谓搬运；main 看到为何延后/拒绝；硬限制不变。 |
| KV13 | 上下文换出时 cancel、close ack 乱序 | source/destination/compute 三套回收各自正确，无 double free 或幽灵可用容量。 |
| KV14 | child 一次性短请求反复到来 | supported hint/no-checkpoint 模式减少污染；不假报命中，主体和可复用长 child 的缓存保留结果可观测。 |
| KV15 | C 从 1 增加，多请求 KV 无法同时装下 | 不强迫用满 C；最大请求数仍不超 C，选择能实际前进的 subset，保留公平活性。 |
| KV16 | 未设置比例；主/子需求充足、边际复用价值和恢复成本平衡 | auto 自行得到接近各半的真实驻留分配；page/state 离散容差预先定义。检查 actual residency/reuse，而不是 UI 输出或静态规则直接给出 0.5。 |
| KV17 | 平衡 trace 转为主体小且稳定、children 工作集扩大，再反向改变 | auto 按真实价值/成本调整并能回归平衡，不能固定 50/50 或 15–20%；硬 cap、公平活性、峰值迁移资源始终合规。 |
| KV18 | 同一平衡需求使用不同 C、已知 K 和未知 K 重放 | C 与驻留份额独立；未知时不能伪报精确各半/容量。可观察后端上检验收敛；不足能力报告软策略/需要 adapter 扩展，不能用手工比例掩盖缺口。 |

离线评估使用固定 trace：主永续循环、分叉研究、长工具等待、群聊突发、主/子长度差异、低 Host RAM、capacity unknown、模型/模板更新。比较 no-cache、LRU、fixed-half strict/borrowable 与 auto；记录总新增 prefill/replay、总 transfer bytes/time、前台/主体轮次等待、任务完成、最大等待间隔与峰值内存。

小规模可穷举同一能力空间和已知 trace 的最优规划作为 oracle；报告 auto 与其差距，不编造近似比。真实 trace 的未来并非已知，oracle 只能作离线参照。策略和成本模型的计算开销也纳入路由器部署 profile。

## 12. 一手来源与精确快照

- [NInfer serving](https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/serving.md)，blob `8d3401199c91741909d62b578d855663bc645757`：capacity、extra state、host backing、metrics、HTTP hints。
- [NInfer 资源调度与上下文缓存](https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/maintainer/resource-scheduling-and-context-cache.md)，blob `3112c6094aab610edba24436434f647f5b254131`：完整 state、真实引用、恢复/传输许可和交换峰值边界。其已有算法供参考，本文件 auto planner 是产品提案。
- [Strata DETAILS](https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/DETAILS.md)，blob `2b8e96591af1902d0a062c72cfa941329a14544a`：串行多 conversation parking、min-tokens/park-gain、一个 pinned prefix、session files。
- [Strata BATCHING](https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/BATCHING.md)，blob `6e7b58d392598465eb2bb80da4924d939d45ada5`：batch slot 与 parking 的不同职责。
- [Dynamo SGLang for Agentic Workloads](https://docs.nvidia.com/dynamo/latest/knowledge-base/modular-components/backends/sg-lang/agents-on-sg-lang)，查询时 Latest v1.5.1，web ref `turn66view3`：priority eviction、HiCache 与 session_control 的分离和限制。
- [llama.cpp server](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)、[vLLM prefix caching](https://docs.vllm.ai/en/stable/design/prefix_caching/)：具体控制要继续固定 adapter 版本。

GitHub 固定 commit 的网页检索出现 DisabledError，但上述四个文件均由 GitHub 连接器成功读取并核对 blob SHA；不要把失败的 web refs 当作已获得网页正文。父文档可使用可访问的主线来源 citation 并保留此处固定快照链接。
