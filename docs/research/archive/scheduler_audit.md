# 调度与历史规格审核清单

日期：2026-10-23（Asia/Shanghai）。范围：规格审核与自动验收设计；没有开发、安装或性能实测。本文所有 MUST、阈值符号和验收场景都是拟议产品契约，不是对任何上游实现的现状保证。文末列出支持这些边界判断的一手来源。

**最新 KV 裁决：** 用户的 50/50 指 auto 在平衡需求/驻留价值/恢复成本下应自然接近各半，并随工作负载变化适应；不等于手动半池配置或独立固定 profile。下文旧有 fixed-half 场景仅作对照/可选配置的正确性测试，不作为替代自动策略的首版门槛；正式新验收见 `kv_policy_spec.md` KV16–KV18。

**已吸收新增硬约束：** 主体是永远持续、单逻辑线程的 proactive thought loop；没有新事件也持续迭代。主 loop 只能进行决策、委派以及通过 worker 异步读/存记忆，不直接操作系统或写文件。版本、任务进度、记忆的实际工作由 subagents/workers 完成。本文普通任务的结束/park 规则不适用于主 loop；其完整状态机与验收在 `thought_loop_spec.md`。

**审查结论：推理请求、任务执行、上下文窗口和 KV 租约必须分别记账。** 仅给 HTTP 客户端套 semaphore，无法满足递归自治、取消恢复、公平调度和本地缓存的组合要求。严格并发上限首先是安全性质；任务最终获得执行机会、取消完成和研究收敛是具有前提的活性性质，不能用同一句“全局锁”概括。

## 0. 审核约定与最小测试设施

### 0.1 严重性

- **P0：发布阻断。** 超额调用、不可恢复死锁、跨权限历史泄漏、历史原文损坏，或规格作出无法保证的承诺。
- **P1：能力门槛。** 某后端/部署模式必须通过才能标记支持；不支持时可明确降级，不能伪造能力。
- **P2：可配置体验策略。** 需确定阈值、观测结果和退化语义；不以未经测量的性能数字作为门槛。

默认以下条目为 P0；标注 P1/P2 的例外除外。所谓“通过”，必须有断言或机器可检查的事件轨迹，不能只看 UI 没有报错。

### 0.2 必须有的确定性测试设施

1. **假后端**：分别模拟支持/不支持取消、显式 KV 生命周期、仅自动 prefix cache、无缓存信息、有限优先级、延迟终止、断流但继续推理、重启和无响应。
2. **假时钟**：控制 deadline、aging、lease expiry、重试和周期任务；调度正确性不依赖真实秒数或硬件速度。
3. **两个独立计数点**：broker 的资源账本，以及后端收到/开始/停止请求的日志。只检查 broker 自己的计数会漏掉直连调用和过早归还 permit。
4. **故障注入点**：发送前后、持久化前后、收到首 token 后、取消前后、KV close 前后、分支合并前后、历史/索引事务中间。
5. **可重放调度轨迹**：保存随机种子、配置 revision、请求/任务 ID、状态转移和故障序列。失败测试必须可确定性重放。
6. **资源与权限测试语料**：至少覆盖单实例多 URL、多协议别名、两个实例共享设备、不同设备、普通聊天者/管理员/私有空间、含中英文与二进制输出的历史。

### 0.3 用符号声明阈值，不冒充实测结论

SPEC 需给每个部署 profile 配置并公开：请求容量 `C_r`、broker 排队容量 `Q_max`、前台允许插队次数 `K_fair`、亲和性插队上限 `K_affinity`、后台服务份额或等效公平规则、单次 generation 的硬 deadline/输出上限、失联隔离时限、KV 驻留预算与 idle TTL、每任务/根任务的预算、最大研究深度和停止条件。

这些值是待选择的策略参数。本文不会凭空声称某硬件达到某延迟或吞吐。只有在后端提供确定的最长服务时间并且所有资源需求最终可满足时，才可把“最多再让行 N 次”换成墙钟时间上界。

## 1. 生命周期和可检查定义

### 1.1 四类对象不得共用一个状态字段

| 对象 | 必须独立表示的状态 | 关键区别 |
|---|---|---|
| Task | ready / running-controller / waiting-model / waiting-child / waiting-tool / parked / finished / cancelled / failed | 这些是普通子任务状态。主 loop 不因没有事件或所有 child 未完成而 park；它保持唯一后继轮次待调度。存在一个 task 不等于占用推理容量；等待子任务可以持有任务身份，但不能占满孩子所需的执行席位。 |
| InferenceAttempt | queued / reserved / sending / accepted / streaming / cancel-requested / terminal-confirmed / uncertain | 从 reserved 开始占用 broker 请求容量；只有确认未发送，或确认后端终止，才释放。 |
| ContextProjection | prepared / committed / superseded / invalidated | 表示模型实际看到的消息投影、来源范围和序列化 revision；与历史原文分开。 |
| CacheLease | requested / bound / resident / release-requested / released-confirmed / invalid / uncertain | 请求结束不代表 KV 释放；lease 失效也不代表正在执行的请求已经结束。 |

任务取消是用户可见的意图；请求终止是后端计算事实；KV close 是驻留状态释放事实。三者必须有不同事件和时间戳。

### 1.2 请求容量守恒

令 `Live_r` 为对资源域 `r` 已预约或可能仍在后端执行的 attempt 集合，包括 sending、accepted、streaming、cancel-requested、uncertain。对于每个有效配置 revision，必须始终满足：

`Σ claim(a, r) ≤ C_r`，其中 `a ∈ Live_r`。

对于同一资源域内每个请求占一份容量的部署，`claim(a,r)=1`。一个请求使用多个设备/资源域时，必须一次性预约完整资源向量。`uncertain` 不能只在 UI 里展示而从账本消失。

降低运行中的容量配置时，允许已有占用暂时高于新上限，但必须进入 **draining**：停止新准入直到恢复合规；这个例外必须可识别，不能假装从未超限。

### 1.3 历史的权威性

- 已确认接收的输入、已确认记录的输出和工具 receipt 构成持久事件流；每项具有稳定 ID 和可验证因果关系。
- FTS、向量、摘要、memory facts、模型上下文和 KV 都是派生物。它们损坏或被清空，不能删除历史原文或凭空制造已执行行为。
- 未被客户端接收/持久化的后端 token 无法被承诺完整保留。崩溃留下的缺口必须表示为 interrupted/unknown，不能补成成功答案。

## 2. 全局容量、身份与绕限审核

| ID | 不变量 | 自动验收场景与判定 |
|---|---|---|
| A01 | 每次上游 inference 必须关联唯一 attempt ID、根预算账户、资源域集合和配置 revision。 | 同时触发主对话、子任务、advisor、摘要、memory reflect、embedding/rerank。后端日志的每个请求都能在账本找到预约记录；没有未知请求。若 embedding 使用不同设备，必须显式归属另一域。 |
| A02 | URL、协议和 model alias 不构成独立物理预算。 | 三个 endpoint（Chat、Responses、Anthropic 兼容）指向同一后端，再加两个 model alias；设共享 `C=1`，同时提交，后端已开始而未终止请求的最大数量为 1。 |
| A03 | 不同 backend instance 可以共享设备预算，但不能共享缓存 handle 身份。 | 两个 server 共用同一 GPU；请求总量受共享域限制。将 A 的 cache handle 交给 B 必须拒绝或全量重建，不能因 GPU 相同而接受。 |
| A04 | 无法自动确认物理共享关系时，不得宣称自动获得 GPU 全局互斥。 | 将同一服务器经两个反向代理配置，隐藏实例元数据。系统要求显式资源域映射，或报告只保证已配置域；不能无声分别放行并显示“全局受控”。 |
| A05 | 多资源准入要么原子成功，要么没有任何部分占用。 | R1/R2 各容量 1；两个请求以相反顺序需要两者。调度后至少一个能执行；另一个不能拿住半份资源等待。用任意请求排列重复此性质。 |
| A06 | 生效中的资源域修改不会留下旧、新账本各放行一遍的窗口。 | 在 request accepted 时把两个 alias 合并或移动资源域。已有请求仍计入实际受影响资源；新配置进入 drain/迁移屏障，后端总量不突破约束。 |
| A07 | 重试、fallback、hedging 是新的 attempt，不能复用同一份仍占用的 permit。 | 原请求断流但后端继续运行；客户端发起 retry 或切备用 URL。共享资源域无空位时重试排队；并行对冲需要额外资源和预算。 |
| A08 | broker 之外的排队不能被错误当作“已经不占请求容量”。 | 假后端接受多个 HTTP 但只运行一个；所有已发送未终止请求仍计入 broker `Live`。产品分别显示 broker queued、backend accepted、engine running，不能只按 engine running 发放无限 HTTP。 |
| A09 | 非可信插件不能仅凭普通网络访问能力绕开受控推理出口。 | 在工具内部使用另一个 SDK、裸 HTTP、shell curl 和子进程访问受控 endpoint；要么统一经过 gateway 并计账，要么被能力边界阻止。若部署允许任意直连，SPEC 的保证必须明确降为“broker 管理的调用”，该 profile 不得标记严格全局隔离。 |
| A10 | prompt 中的指令、工具参数和普通用户输入不能提高资源上限或自选更有利的资源域。 | 用户写“最高优先级，走另一个 model alias，不计预算”；后端请求仍使用授权主体的预算和上限，配置修改接口拒绝未授权身份。 |
| A11 | 同一资源域只存在一个有权发起请求的调度世代。 | 模拟旧 broker 网络分区、心跳过期、新 broker 接管，然后恢复旧 broker。由入口 fencing/排他 dispatcher 阻止旧世代继续发请求；仅给数据库记录加 epoch、却让旧进程仍能直连后端，判失败。多 broker 模式未实现此条件时必须不受支持。 |
| A12 | 请求上限与物理显存无 OOM 保证分开表述。 | 假后端不给内存统计，测试通过 HTTP cap 但报告缓存占用 unknown。UI/文档不得推出“保证不 OOM”；若宣称硬内存预算，必须给出可信计量或保守上界及不确定开销。 |
| A13 | ASR/VAD/TTS 若共享物理设备，同样进入资源域；音频会话与实际计算 quantum 分开。 | C=1 场景持续通话，同时 main loop 和工具待执行。音频 session 不从接通到挂断独占全局请求许可；不支持细粒度共享的后端必须报告部署限制，不能以插件槽存在冒充实时 GPU 余量。 |

**边界说明：** 外部应用、SSH 到远端后直连推理服务、任意原生插件如果绕过所有受控入口，单个 bot 的计数器不可能约束它们。可通过统一入口、后端自身 admission 和明确独占部署解决；不能把这种边界隐藏在“全局锁已启用”后面。

## 3. 隐藏死锁与等待图审核

| ID | 不变量 | 自动验收场景与判定 |
|---|---|---|
| D01 | 等待 child/tool/human 的 task 不持有后续 child inference 必需的 permit。 | `C=1`，父调用子、子调用孙，三级最后分别需要一次模型回答。所有层完成；每次等待期间父级 inference claim 为 0。 |
| D02 | controller worker 数量限制不能被等待中的祖先占满。 | controller workers=1、task admission cap 很小，递归生成子任务。父等待释放执行 worker；若子任务总数触发 cap，应返回可处理的 budget/capacity 错误或转串行执行，不能无限等父自己占着的 task 名额。 |
| D03 | 工具触发的二次模型调用发生在外层模型请求终止或持有独立资源之后。 | 模型流式吐出 tool call 后延迟发送 terminal；工具立即想调用模型。不得把外层同一 permit 借给内层。可以等待外层 terminal 后运行，或在有额外容量时独立准入。 |
| D04 | compaction、summarizer、memory consolidation 没有豁免通道，也不会请求自己已经持有的唯一 permit。 | `C=1`，前台到上下文阈值触发 model-based compact。原 generation 已终止再排 compaction；摘要失败能回退到已定义 context policy，不出现自等。 |
| D05 | 数据库写锁、文件系统发布锁、SSH 连接池锁不得跨越不受控外部等待。 | 在保存历史、发布分支和 SSH 操作中注入无限慢模型。另一个独立会话仍可提交持久事件、取消请求和读取状态；锁诊断图中不存在锁→LLM→锁循环。 |
| D06 | scheduler 的 cancel/status/terminal 接收路径不需要等待被取消任务占据的同一个工作队列。 | worker 全忙且其中一个 backend 永不回答；发 cancel 与终止事件。控制面仍能处理并更新状态；不能因为执行线程池耗尽而连释放 permit 都做不到。 |
| D07 | wait/join 图无环，或显式拒绝形成环的边。 | A 等 B、B 等 C，再请求 C 等 A；第三条边在持久化时被拒绝并给出 cycle 诊断，不靠超时才发现。包括插件创建的隐式 join。 |
| D08 | 持有 KV 的等待任务不能永久阻塞完成依赖所必需的缓存空间。 | 父任务占满 pinned-state 预算，child 需要最小缓存才能运行。按策略释放/降级父缓存、预先预留 child 空间，或给 child 明确不可准入错误；不允许 parent 等 child、child 等 parent lease 的永久环。 |
| D09 | 模型交接与独占 conversation/session lock 不形成重入死锁。 | 父在 session lock 内发 child，child 需要历史读取或同 session 输出。读取使用已提交快照；写入走独立分支/明确 mailbox；不得让 child 等父持有且只在 join 后释放的写锁。 |
| D10 | 每个等待都有可观测原因和可取消入口。 | 人为卡住所有类型的等待，status 必须暴露 request/child/resource/lease/deadline 等依赖 ID；cancel 能把逻辑任务转到终态，物理不确定资源另行隔离。 |
| D11 | 容量不足是可判断的不可准入/待扩容状态，不伪装成公平排队。 | 请求需求大于该 profile 永久上限，或其剩余上下文无法容纳最低有效输出。必须在准入前拒绝/compact/分片，不能永久位于队头阻塞其它任务。 |
| D12 | 可完成任务在满足公平特性、后端最终响应和有限依赖的前提下最终退出等待。 | 对资源量和依赖数量有限的随机任务 DAG 做状态探索；无未标注的 terminal-unreachable 状态。发现 wait 环或容量自占时输出最小重放轨迹。 |

## 4. 优先级、公平性与饥饿审核

**必须先选契约：** 无限制到达的高优先任务、严格优先级和低优先任务一定获执行，三者不能同时保证。建议将“紧急优先”定义为有界插队，并给后台保留非零服务份额或等价的最老可执行请求规则。紧急灾难模式可以暂停后台，但必须是有时限、可见且单独计量的状态。

| ID | 不变量 | 自动验收场景与判定 |
|---|---|---|
| F01 | effective priority 来源是已授权 policy，不是模型自报字符串。 | 普通群聊、owner、后台定时任务同时提交不同 urgent 字段。结果符合身份/任务来源规则；恶意 urgent 不能越权。 |
| F02 | 优先级顺序必须在 broker 内落实；透传 backend priority 只算附加能力。 | 假后端忽略 priority 或拒绝非零字段。排队阶段仍按约定挑选；不支持字段不会导致无限失败重试或暗中反转顺序。 |
| F03 | 后台在持续前台到来时有可验收的最低服务机会。 | 放入一个始终 eligible 的背景请求；持续插入前台。它最多被约定 `K_fair` 个后来请求超过，或在规定调度窗口获得最小份额。仅“每秒加一点 age”而永远低于前台，判失败。 |
| F04 | cache affinity 不能无限提高旧会话地位。 | 一条持续 cache hit 的流和一个冷会话竞争。冷会话被亲和性超过的次数不超过 `K_affinity`；超过后必须选冷请求或记录另一个真实资源阻塞原因。 |
| F05 | 公平性对主体/根预算账户生效，不能通过拆出大量子任务抢占份额。 | A 生成大量 child，B 仅一个请求。A 子任务仍共用 A 的公平账户；不能按 child 数成比例夺取更多份额。 |
| F06 | 高优先父任务等待的必要子任务可有限继承优先级，但不能提升其权限和总预算。 | foreground 等 background child，另有持续 background。child 获依赖优先级；父取消后继承撤销；child 的权限与预算不变，不能给无关兄弟节点永久加速。 |
| F07 | 大请求不会因为小请求持续占用碎片而永久不 eligible。 | 大请求需求可装进总预算但需要资源聚齐；小请求连续到来。到预定保留条件后，调度器 drain/预留足够资源给大请求，或者明确 reject/split；不能将资源永远碎片化当成免除公平性的理由。 |
| F08 | `C=1` 不设置一个永远闲置、仅供前台使用的保留 slot。 | 仅有后台任务时应能运行；运行中前台到达时按已选自然 yield 或 cancel/preempt 策略处理，不能声称两者同时立即运行。 |
| F09 | 非抢占后端的已运行长请求阻塞是诚实可见的。 | backend 不支持 preempt；后台已执行，前台到达。UI 显示等当前请求终止及取消能力，不许用提升 queue priority 声称已抢占。任何墙钟等待上界都必须含最长当前请求时长。 |
| F10 | deadline、queue TTL 和 aging 使用单调时间语义，重启后恢复一致。 | 时钟回拨/快进、进程重启；过期请求不重新变新，不重复延长 deadline。持久时间与单调计时的转换规则明确，已过期请求不 dispatch。 |
| F11 | 队列有界，超载反馈可重试但不会造成零延迟重试风暴。 | 输入速率长期大于服务能力；queue 不超过 `Q_max`，返回具体 retry/backpressure 状态，重试有退避/预算；不会无限占内存。重复事件可幂等折叠。 |
| F12 | 不同 queue 的公平份额和 owner 暂停策略可复现。 | 用相同配置 revision 和输入轨迹重放，除显式随机 tie-break 外结果一致；暂停 background 有开始/结束记录，到时自动解除或明确失败，不能永久隐身。 |
| F13 | 永续主 loop 与必要 subagents 在可服务条件下都有正的执行份额。 | `C=1`，主 loop 永远有下一轮，同时 memory/progress/执行 worker 持续待执行。按配置的有界轮间交错，两侧都获得执行机会；主 loop 不永久占 slot，worker 也不能让主体永远没有下一轮。不能用“主 loop 无事件就睡眠”伪造公平。 |

## 5. 取消、崩溃与资源回收审核

| ID | 不变量 | 自动验收场景与判定 |
|---|---|---|
| C01 | 取消尚未发送的请求立即撤销预约且永不发送。 | 在 queued/reserved 边界并发 cancel 和 dispatch；线性化结果只能是未发送且已释放，或已经发送并进入后续取消状态。不得出现已释放后又发送。 |
| C02 | 已发送请求收到 UI cancel 后仍占请求容量，直到可信终止证据。 | 假后端关闭客户端连接但继续计算；`C=1` 下第二请求不得开始。UI 立即停止继续展示为有效回答，同时标注后端停止确认中。 |
| C03 | socket EOF、客户端超时、取消 API 的“已受理”不自动等于计算停止。 | 三种事件分别注入；只有 adapter 定义的 terminal/stop ack/可信实例销毁证据才能释放。每个证据类别可追溯到能力契约。 |
| C04 | cancellation-pending 超时不能为了恢复可用性而悄悄归还容量。 | 后端永久失联；超时后转 uncertain/quarantined，域可 fail-closed 或由已授权 supervisor 重启并验证旧实例结束；不能一边显示未知一边重新放满。 |
| C05 | request terminal 和 KV released 分开记账。 | 请求结束但 session pinned：request permit 释放，KV 预算仍扣除。反向场景释放缓存后请求仍在运行：不能释放 request permit。 |
| C06 | 本地 lease expiry 只代表该 handle 不再可用，不自动证明远端显存已释放。 | 延迟 close RPC、服务端时钟不同或 keepalive 已到达但 ack 丢失。缓存标 invalid/uncertain，占用按适配器可信证据处理；不得直接将本地计时器当显存释放证明。 |
| C07 | 重复、乱序 terminal/cancel/close 回调只产生一次资源归还。 | 对同一 attempt 重复发送 stop ack、EOF、error，再乱序 terminal。账本无负数，permit 总量不增长；迟到事件不能结束同 ID 的新世代请求。 |
| C08 | broker crash 后未决发送不会变成免费且自动重发的请求。 | 在 durable send-intent 后、实际发送前后分别 crash。恢复时使用后端 idempotency/reconcile（若有），否则标 uncertain 并保守处理；不能未经证实同时重试旧请求。 |
| C09 | 日志记录已完成、用户已经取消两者可以并存，但迟到成功不能复活任务。 | cancel 后到达完整答案：保存与原 attempt 关联的晚到 receipt，用户任务仍 cancelled；只有明确重新开启的新 task 才能继续对话。 |
| C10 | 插件卸载/reload 清理旧 generation 所有请求、timer、SSH 任务和 KV lease；清理未确认的资源仍计账。 | 热重载含运行中模型和 SSH 的插件；旧 callback 不得操作新实例；资源 registry 能列出全部处置结果，uncertain 不消失。 |
| C11 | 取消传播的范围是明确策略，不混同取消推理与撤销外部副作用。 | 父取消时 child、尚未发送的消息、运行中 SSH、已完成文件写入分别按契约处理。不能把“停止模型”记为“远端命令未执行”；外部结果不明保留 unknown receipt。 |
| C12 | KV slot 物理复用有 generation/owner 检查，不能仅凭整数 slot ID。 | A 的 slot 释放后分给 B，再到达 A 的旧 close/restore。旧操作被拒绝，B 的缓存不被删改；若上游没有 fencing，adapter 必须串行化/确认清理后才复用。 |

## 6. 上下文、prefill 与 KV 租约隔离审核

| ID | 不变量 | 自动验收场景与判定 |
|---|---|---|
| K01 | 上下文限制作用于完整最终渲染后的输入，不只是用户文本。 | 输入加入 tools/schema/system/history/images/特殊模板，再预留输出与必要余量；总量超过 effective window 时必须在 dispatch 前 compact/split/reject。边界使用后端 count 或版本匹配 tokenizer 验证。 |
| K02 | effective context window 不超过模型运行时、engine 配置、owner 设置三者的最小有效值。 | alias 宣称大窗口但 server 配小窗口；发送前使用真实较小限制。改变任何配置必须更新相应 revision，旧预估不可直接复用。 |
| K03 | 输出预算包含后端实际计入 context/收费/上限的隐藏 reasoning 或其它生成 token。 | 假后端有隐藏 reasoning；不能仅用可见文本 token 判定额度。未知字段显示 unknown，并采用该 adapter 的保守上界/拒绝策略。 |
| K04 | 上下文窗口容量、正在执行请求数、prefill 预算和驻留状态预算分别约束。 | 单个长会话未超 context window，却超过全域 KV 预算；不得因为 request cap 尚空就放行。反之 request permit 已释放后仍存在 parked pinned lease，驻留账本保持非零。 |
| K05 | 缓存命中预测不能充当硬 prefill 额度的事实。 | 入队时预测全部命中，dispatch 时缓存被逐出。若没有可信 pin/保留承诺，应按完整输入最坏情况预约；若可提前确认失效则先补充资源再 dispatch。不能按预测 hit 记 0 prefill 并声称硬上限成立。 |
| K06 | 对于未知 memory/state 大小的后端，逻辑 token 预算不能冒充精确显存预算。 | 混合 recurrent/StateImage 模型返回有限统计；系统报告估计方式与 unknown 字段，不把 `tokens × 固定 KV 字节` 当普适事实。P1：精确字节级保障只对经验证的 adapter 生效。 |
| K07 | lease 身份至少包括 backend instance epoch、model revision、tokenizer/template revision、序列化/工具 schema revision、信任域和 lease generation。 | 每次只修改一个字段，再尝试续用旧 handle。被拒绝/失效并从历史重建，或有逐项证明兼容的 migration；不能静默套用旧状态。 |
| K08 | model alias 不变也不能掩盖权重、量化、tokenizer、chat template 或推理配置变化。 | 后端热换同名模型或改模板；若不能获得 fingerprint，重连/健康状态变更必须保守失效，配置不得宣称强缓存一致性。P1：不能探测热换的模式需明示 owner 操作要求。 |
| K09 | append-only session 只能续接与后端已确认 token 前缀一致的新投影。 | 早期消息编辑、工具 schema 重排、memory 原文更新、compaction reset 后续接。adapter 检查 projection revision/prefix digest，选择新 session/full prefill；不得把删改后的历史作为仅追加。 |
| K10 | 子分支不能同时写入同一个独占 KV/session handle。 | 两个 child 从同一父窗口分叉并同时生成。使用独立 lease、后端明确支持的 fork，或安全串行；若只支持 append，禁止把同 session ID 分配给两个不同后缀。 |
| K11 | 从原始历史可重建语义正确请求，缓存失效只影响成本/延迟。 | 清空全部后端 KV，重启服务，令旧 previous_response_id 失效。任务继续时投影包含完整必要历史、工具 call/result 和原任务；不能因缓存丢失而遗忘已经提交的约束。 |
| K12 | 不同权限域不共享可观察的 KV 状态，除非有明确授权共享策略。 | 私人 memory 进入 A 前缀后，B 群聊不得绑定/恢复 A handle，也不得通过 cached-token 指标或 handle 查询读出 A 私有内容。后端 cache_salt/namespace 能力不足时降级隔离承诺。 |
| K13 | 稳定前缀优先但不能压倒事实更新、删除或权限变更。 | memory 更新通常追加 revision 通知；删除敏感事实/撤权时必须失效受影响投影与 lease，使下一轮不能再读取旧内容。不能为了 cache hit 保留已撤权内容。 |
| K14 | idle TTL、renew、close 的语义按具体后端能力协商，不能虚构通用 slot API。 | generic API 只支持 full history 时，slot/pin/fork/save 标 unsupported；SGLang/Dynamo append session、Strata slot save、NInfer Responses continuation 各跑能力专项测试；不支持操作必须结构化报错并走合法 fallback。 |
| K15 | pinned lease 存在独立配额和最长保留策略，不能无限挤占有执行资格的冷会话。 | 长时间 parked 的会话定时 renew，同时新会话等待内存。按授权公平策略 evict/停止续租/拒绝新增；测试中不能由无限 renew 造成未标注的饥饿。 |
| K16 | compaction 的触发次数与成本有界，失败不会造成 compact→retry→compact 自旋。 | 生成摘要后仍超窗口，或目标模型反复返回无效摘要。重试次数耗尽后进入明确 fallback/blocked，保留原文，不再次无条件触发同一压缩。 |

### 6.1 “支持缓存”的验收必须细分

至少分别列：automatic prefix reuse、caller affinity hint、sticky routing、exclusive session lease、append continuation、fork/rewind、explicit close、save/restore、cached-token telemetry。某个后端提供其中一项，不代表其他项也存在。

特别测试：工具等待超过服务端 session idle TTL 后，下一轮应 reopen/full prefill；请求依然可从本地历史恢复。server-side `previous_response_id` 不能成为唯一对话原文。仅 router sticky 的 bind 操作不能算已经 pin KV。

## 7. 权威历史、分支、压缩与检索审核

| ID | 不变量 | 自动验收场景与判定 |
|---|---|---|
| H01 | 接收确认意味着输入已达到承诺的持久级别。 | 在 UI ack 前后断电模拟；已 ack 事件恢复后存在，未 ack 事件可幂等重发。若产品仅提供内存接收确认，必须标注不同状态，不能称已持久记录。 |
| H02 | 每个 inference 保存其实际投影来源与 revision，能够解释“当时模型看见了什么”。 | 原始历史含撤销/分支/摘要；重放指定 attempt，得到同一有序消息、工具定义、原文范围和 serializer digest。不要求随机生成答案相同。 |
| H03 | Compaction/new window 追加变更事件，不就地覆盖历史原文。 | 多次压缩并切窗口，随机抽取早期用户要求、工具失败、引用 URL：按 event ID 可读回原始字节或明确的不可用保留状态。摘要不能替代原文。 |
| H04 | branch ID、parent tip 与 causal links 一致，分支写入使用冲突检测。 | 同一父 tip 两个 child 并发提交；形成两个分支，或一方显式 rebase。不能把不同试验的 tool results 混入一条看似顺序成功的历史。 |
| H05 | 同一会话的新增输入按 mailbox 规则有确定次序，streaming 输入不会默默改写已发送前缀。 | 模型生成中连续收到用户补充/撤回。记录进入下一轮、显式 steer/cancel 或新 branch 的决策；重放得到相同顺序和被采用范围。 |
| H06 | tool call/result 具有稳定关联和真实终态，不以超时推断未执行。 | 外部命令已执行但 receipt 丢失后崩溃。恢复记录 unknown，需要查询/幂等重试/人工处理；不能补造 success，也不能盲目重复非幂等操作。 |
| H07 | 取消/失败的 assistant attempt 和部分输出不伪装成完整有效回答。 | 在任意 token 中断，后续重建输入按 adapter 规则排除/标记不完整项；历史原文保留 interrupted 状态；不可出现孤立 tool result 导致错误 transcript。 |
| H08 | 历史、原始 blob、索引的提交具备可恢复一致性。 | 在 CAS 写入、event commit、FTS update 各步骤 crash。恢复后 event 引用不能指向不存在且未标记的内容；孤立 blob 可 GC；索引可重建。 |
| H09 | FTS/向量/摘要损坏不损坏权威事件。 | 删除/破坏派生索引后重新构建；事件数量、内容 hash、因果关系和 ACL 保持一致，检索恢复到参考结果集。 |
| H10 | 异步索引的可见水位明确，不把未索引误报为不存在。 | 写入后立即 search。提供 read-your-writes、受限原文 fallback 或明确 `indexed_through` 水位；查询后只返回空数组却宣称已搜完全部历史，判失败。 |
| H11 | 检索权限在候选结果、snippet、计数、rerank 和引用解引用中一致执行。 | A 私聊与 B 群聊使用相同关键词；B 的结果、snippet、总数、错误信息和 source read 不泄漏 A。摘要/索引不能丢失父来源可见性。 |
| H12 | 中文短词与代码标识符检索有具体可测语义。 | 固定语料覆盖一字、两字“北航/衢州”、三字、人名、混合英文、`foo_bar`、路径与标点。逐个配置精确预期结果；不能只启用 trigram MATCH 就宣称短中文检索完整。 |
| H13 | tokenizer/index schema 更新只替换派生索引版本，查询切换原子化或暴露一致性边界。 | 中途 kill reindex，恢复后使用完整旧索引或完整新索引；不能出现一半新分词一半旧分词却无版本说明。原文 ID 不变。 |
| H14 | recall 和 summary 带 provenance、来源范围与事实状态；合成内容不是原始证据。 | 故意给摘要一个错误结论。审计能定位其作者/模型/来源；用户问原话时返回原文而非摘要。引用不存在的 event ID 必须拒绝或标失效。 |
| H15 | 删除/撤权对原文、索引、摘要、memory、活跃投影、缓存和备份具有明确覆盖范围。 | 对测试秘密执行 forget，再逐项查找、重建、恢复 snapshot。验收按选定 retention policy 判断；若备份/Git 尚保留，不得报告“所有副本已永久删除”。 |
| H16 | WASM rootfs rollback 不回滚已经接受的真实世界会话和 receipt。 | guest snapshot 从 V3 回滚 V1：外置历史仍含 V2/V3 已提交事件；恢复过程只改变执行环境版本。回滚不能自动重放外部非幂等动作。 |
| H17 | schema 升级与 core 降级不允许不兼容写入。 | 新版迁移历史 DB，再回滚 core。旧版若不认识 schema，应只读/拒绝启动写入并进入救援流程；不能初始化空 DB 或覆盖新字段。 |
| H18 | 历史落盘故障不能被静默忽略并继续产生无法追溯的操作。 | DB 只读、磁盘满、I/O 错误时触发模型/SSH/发消息。根据已定策略停止副作用或进入显式 degraded 模式；用户和审计可见缺口，不能仍显示完全可追溯。 |

**不能过度承诺的“exactly once”：** 对不支持幂等键和执行查询的任意 SSH 命令/外部 API，在“已执行、receipt 未落盘”之间崩溃，无法普遍判定是否安全重试。规范应承诺已持久记录的 intent、可区分的未知结果、幂等能力优先和明确重试策略，而不是宣称所有工具天然 exactly once。

## 8. 递归研究、预算与结果收敛审核

| ID | 不变量 | 自动验收场景与判定 |
|---|---|---|
| R01 | 根任务总预算覆盖所有后代、失败、重试、advisor、compaction 和 memory 工作，不能通过 spawn 复制额度。 | 根预算 B，派生多个 child，各自再派生。预算账本满足已耗费+已预约+可分配 ≤ B；child 预算来自转移/分配，不是新建 B。外部明确追加预算单独留事件。 |
| R02 | token/cost/request/tool/时间/深度是分开的维度，不互相冒充。 | 低 token 但无限搜索工具、零输出但长时间等待、大量失败请求分别触发对应上限；没有单靠 token 就允许无限外部动作的漏洞。 |
| R03 | 为最终综合预留的预算不能被背景分支挪用。 | 让 research 用尽可分配部分；仍能运行约定的最终汇总，或用无模型的确定性模板报告已知结果与缺口。不得因完全耗尽而无回答退出。 |
| R04 | 未确认终止 attempt 的最大可能消耗仍被预约，不能取消后立即反复刷出无限预算。 | cancel/retry 循环且 usage 延迟。实际/上界账本仍受 B 限制；只有确认未使用部分可返还，unknown 费用明确标注。 |
| R05 | 每个分支启动前必须有待回答问题、预期产物、证据标准、停止条件和预算来源。 | 模型请求“再多开几个找找”。缺少必填字段时不 spawn，返回可修正错误；不得由无限模糊目标生成子任务树。 |
| R06 | 深度、活跃后代、总派生数量均有根级上限；触顶返回可继续综合的结果。 | 人为制造无限自我分解任务。达到任何上限后拒绝新分叉并产生 typed partial/budget_exhausted，不会父子互等。 |
| R07 | 去重基于查询意图/来源身份/内容 revision，重复镜像不自动算独立证据。 | 多次相同 query、URL 跟踪参数、转载/镜像内容。去重统计符合固定语料；新 revision 可重新取证，但必须注明变化原因。语义去重的概率误差单独报告。 |
| R08 | 停止谓词是可观测的，不只由模型说“我还想查”。 | 配置 coverage、连续 N 次无新增有效证据、预算、deadline、重复循环门槛。假结果序列逐项触发对应 stop reason；关闭所有活性上限的 profile 不得承诺必然收敛。 |
| R09 | coverage 达成须满足已定义验收项；有矛盾时可以 partial，不能默默标 complete。 | 为问题 ledger 注入支持/反对证据。机械检查每项状态、来源、未解冲突；是否语义充分由模型评审/人工样本评估单列，不能称程序已证明事实正确。 |
| R10 | join 不默认无限等所有 child；失败、超时和取消都有结果值。 | 一个 child 永不完成、一个失败、一个成功。按 all/quorum/best-effort + deadline 的显式策略结束 join，保留各状态并综合；不能由一个死 child 永久阻塞 owner。 |
| R11 | 达到终止条件后禁止新增不属于收尾的工作，迟到结果只能进入审计或新明确任务。 | 在 budget/deadline 时同时到达 child 新分叉和完成结果；线性化后不再扩树，收尾预算仍可用；report 不因迟到事件重新开始无限研究。 |
| R12 | 普通子任务/研究 episode 必须完成、取消或 park；唯一主 thought loop 必须继续真实模型迭代。 | 清空外部 inbox、完成所有 children，持续运行 N 轮测试：主 loop 仍提交下一轮并有真实模型请求，不等待外部事件/cron。已完成研究子树不重复扩张；主 loop 可判断无需新增子任务后继续自身下一轮。N 只是有限验收样本，不是主体寿命上限。 |
| R13 | 任务完成状态区分 satisfied、partial、no-new-evidence、deadline、budget、blocked、failed、cancelled。 | 同一固定任务分别触发不同终止原因；报告与审计状态一致。budget 用尽的部分研究不能标记为已经满足全部需求。 |

### 8.1 单主体持续思考专项不变量

| ID | 不变量 | 自动验收场景与判定 |
|---|---|---|
| L01 | 一个身份同时最多有一个非终态主体 inference attempt，包括失联 unknown 的旧 attempt。 | 同时注入 child 完成、用户输入、watchdog 和重启恢复。实际后端运行日志中该主体的活跃/未知 attempt 数 ≤ 1；没有投机地在另一个 backend 生成下一条自我续写。 |
| L02 | 每次成功提交轮次 i，原子产生唯一待执行后继 i+1；无新事件不改变此规则。 | 在 commit/后继入队边界 crash；恢复后恰有一个后继。模型给出“没有事情可做”也不令主体 finished/parked。 |
| L03 | 主 loop 不直接拥有 exec、文件、Git、SQLite、SSH、包管理或版本发布工具。 | 用 persona/prompt/模型输出要求主 loop 执行上述操作，能力注册层拒绝；相同授权任务可以通过 handoff 由 worker 执行。审计中副作用的 actor 始终是 worker 或确定性 runtime。 |
| L04 | memory read/save、task progress 和 version 操作是异步 worker 作业。 | `C=1`，主发 memory read 后立即释放 inference/worker；memory worker 得到执行并回传 receipt。主 loop 不同步等待占满其所需 slot；pending 状态不是已保存/已读取。 |
| L05 | worker 的完成回调只能写持久 inbox/receipt，不能直接再启动一条主 loop。 | 多个 child 同时完成；只增加下轮可见事件，无额外主体调用。worker 可做任务内局部决策，不能取得全局 agenda 或无限自我续写权限。 |
| L06 | persona 追加在基础 system prompt 后，但不改变 native tool schema、权限和调度契约。 | 更换 persona、加入声称可写文件的 persona 指令，schema/capability fingerprint 和工具授权保持；需重新建立受前缀变化影响的 cache。模型格式错误有结构化恢复，不能绕过验证。 |
| L07 | 运行恢复由确定性 supervisor 与 fencing 保证，不设第二个永续 LLM 监督主体。 | 旧控制进程失联、新实例接管：旧世代提交/派发被拒绝；旧主体请求终止未确认前新实例只恢复状态，不开始另一轮主体推理。 |
| L08 | 状态必须区分 broker 排队、后端不可用、资源额度耗尽、owner 暂停、物理离线；它们不能冒充“正在思考”。 | 分别注入相应条件。仅真实模型推理标 thinking；健康可用时无事件仍持续，物理关机/不可用期间记录恢复缺口，不能声称脑内持续运行。 |
| L09 | 主 loop 的连续性不等于任何子任务不许收敛，也不等于无限新建任务。 | 主体持续 N 轮时重复同一 handoff 意图；幂等/去重、子任务预算和根级上限仍生效，研究结束后不会因主体仍运行而被自动重开。 |
| L10 | 普通外部消息和音频输入默认注入同一主体，不能按平台/消息/ASR 修订各开 chat agent 绕过主体。 | QQ/飞书/Web/电话同时输入；每条具有稳定 event/utterance revision 并进入 structured inbox，只有一条主体推理链；主体委派后的 worker 才执行回应。 |
| L11 | broker/modelprovider/runtime/history/UI 可替换，kernel 不因这些规格膨胀为全部实现。 | 替换 bootstrap plugin 后接口 conformance 继续通过；旧资源、epoch 和持久历史可恢复；内核仅负责必要生命周期/恢复和能力门限，策略可独立更新。 |

## 9. 最小发布验收包

所有组合无须穷举整个真实后端产品矩阵；确定性模拟器覆盖算法性质，每个支持的真实 adapter 再核对其能力契约。以下是必须保留的跨模块场景。

| 场景 | 故障/配置 | 必须同时满足 |
|---|---|---|
| T01：三级递归 | request cap=1、controller worker=1、有限子任务 cap | D01–D03、D07、R01/R06；完成综合，没有隐藏直连。 |
| T02：持续前台流 | 一个背景请求、持续新前台、cache-hit 老会话 | F03–F05；按配置有界让行/公平份额，不虚构墙钟保证。 |
| T03：取消后幽灵计算 | 客户端断流、后端继续、取消 ack 丢失 | A07、C02–C07；第二请求不能挤占仍占用容量。 |
| T04：KV 与请求分别释放 | terminal 后继续 pin；或 lease 先失效 | C05/C06、K04；两个账本独立且最终可核对。 |
| T05：共享 GPU 别名 | 两 server、三 URL、多个 model alias | A02/A03/A06；预算共享，cache handle 不共享。 |
| T06：工具里另开 SDK | 内置工具、WASM 插件、原生插件、shell、SSH 路径 | A01/A09；经过强制入口的模式完全计账，无法约束的模式准确标注边界。 |
| T07：热更新缓存 | tokenizer/template/tool schema/model 各改变一次 | K07–K11；旧 cache 被拒绝，全量历史重建，不能丢任务约束。 |
| T08：长工具等待 | 超过 server session idle TTL；随后继续 | K11/K14；重建请求成功，显示缓存失效而非丢失会话。 |
| T09：broker crash 窗口 | send-intent 前后、实际发送前后、terminal 落盘前 | C08、A11；无免费重试、无双 dispatcher、无账本凭空清零。 |
| T10：资源碎片与锁环 | 大请求+不断小请求；父 pin 满内存等 child | A05、D08、F07/K15；预留/drain/明确拒绝，不能永久等待。 |
| T11：历史压缩与回滚 | 多次 compact、分支、WASM rollback、索引损坏 | H02–H04、H09/H16；原文可追溯，外部 receipt 不回滚。 |
| T12：权限撤销 | 私有 memory 进入 cache 后撤权；并行群聊检索 | K12/K13、H11/H15；下轮不可读已撤权内容，派生索引一致。 |
| T13：中文与索引换代 | 1/2/3 字、人名、代码路径，reindex 中断 | H10–H13；固定预期集合、水位和版本可核对。 |
| T14：研究饱和 | 无新证据、互相转载、一个 child 永久失联 | R03/R07–R13；有明确部分结果和停止原因，episode 退出。 |
| T15：容量配置降低 | 运行中 C 从较大值降为 1 | A06 与 draining 定义；已有请求可终止，新请求不加剧超限。 |
| T16：无法证明后端停止 | 后端永久失联，没有可强制重启的权限 | C04；进入可见不可用/隔离状态，保留真实不确定性，不为了“永远运行”伪造安全回收。 |
| T17：无事件的永续主体 | inbox 空、children 全完成、后端健康 | L01/L02、R12；持续产生真实思考轮次，不能退化为事件驱动休眠。 |
| T18：主体与 worker 单槽交错 | C=1、主体持续 ready、memory/task workers 需要模型 | F13、L04；双方按配置公平获执行，无父占 inference/worker 的死锁。 |
| T19：双恢复与迟到回调 | 旧主请求 unknown、多 child 同时返回、新 supervisor 接管 | L01/L05/L07；主体唯一性覆盖真实后端请求和决策提交，无第二条思考链。 |
| T20：主体能力和人格 | 更换 persona、模型企图直接写文件/SSH | L03/L06；只能产生合法 worker 委派，工具 schema 和权限不被 persona 改写。 |

验收输出必须包含：配置文件与 revision、固定种子、后端能力声明、按资源域绘制的预约/实际执行/uncertain 曲线、完整失败轨迹、历史一致性检查结果、缓存命中信息的 known/estimated/unknown 标记。真实硬件测试另外报告 TTFT、cached/uncached prompt tokens、输出吞吐、峰值驻留状态和任务完成率；本文件不预填数值。

## 10. SPEC 定稿前必须决定的六件事

1. **严格全局边界。** 所有受控推理调用是否强制经统一入口？是否承认外部客户端/任意 SSH 隧道直连属于保证之外？
2. **单请求容量下的前台策略。** 等自然终止，还是允许 cancel 后台？后端不确认停止时，接受 fail-closed 还是授权 supervisor 重启？必须知道会损失哪些缓存/未完成输出。
3. **公平性契约。** 选择有界插队、最低份额或其它可验证机制；不接受同时承诺无限严格高优先和背景必进展。
4. **KV pin 的可撤销性。** owner/前台 pin 是否有不可撤销例外、最长持有时间及超预算处置？否则冷会话可能永远不可准入。
5. **历史保留与删除。** 完整审计、Git/备份可恢复与严格忘记的取舍；定义删除覆盖范围以及离线备份的清理策略。
6. **连续主体与子任务预算。** 子任务 episode 的根预算、综合保留和结束谓词，与永续 thought loop 的持续服务份额分开。主体不能因“没有事件”默认休眠；若 owner 设置硬费用/能耗上限，耗尽后的 blocked 状态、恢复条件和对连续性的影响必须明示，不能同时声称无限算力和有限总消耗。

## 11. 与研究来源的对应关系

以下仅支撑能力边界与风险背景；上面的调度算法和验收条目是本产品设计建议，不应冒充上游已经实现。主线会变化，开发前应按 `research_harness.md` 的 commit 表重新固定快照和适配器 conformance tests。

- Codex token-budget context reset 与可读历史接口：[compact_token_budget.rs](https://github.com/openai/codex/blob/main/codex-rs/core/src/compact_token_budget.rs)、[history-notes tools](https://github.com/openai/codex/blob/main/codex-rs/ext/history-notes/src/tools.rs)。启发 H02/H03，不等于本地可直接复用其 backend。
- pi 的树状 session 与 compaction：[session-format](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/session-format.md)、[compaction](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/compaction.md)。启发 H03/H04。
- oh-my-pi 的记忆快照与稳定 replay：[Mnemopi memory backend](https://github.com/can1357/oh-my-pi/blob/main/docs/mnemosyne-memory-backend.md)。启发 K09/K13，不能使隐私撤回失效。
- DeepSeek Harness 的持久 session event：[session subsystem](https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/subsystems/session.md)。启发 H02/H06/H07。
- NVIDIA Dynamo 的实验 SGLang streaming session：[Agents on SGLang](https://docs.nvidia.com/dynamo/latest/knowledge-base/modular-components/backends/sg-lang/agents-on-sg-lang)。bind/open/close、append-only、idle timeout 与 pinned 状态限制对应 C05/C06、K10/K14/K15。
- NInfer 的会话、缓存与取消语义：[serving.md](https://github.com/Neroued/ninfer/blob/master/docs/serving.md)；Strata 的并发与 slot 限制：[BATCHING.md](https://github.com/Niko1221/Strata/blob/main/docs/BATCHING.md)、[DETAILS.md](https://github.com/Niko1221/Strata/blob/main/docs/DETAILS.md)。支持分能力审核，不能推出通用 KV 协议。
- llama.cpp 的显式 slot 接口：[server README](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)；vLLM 的 prefix cache 与隔离参数：[prefix caching](https://docs.vllm.ai/en/stable/design/prefix_caching/)。对应版本化能力与 K12。
- SQLite FTS5 的 tokenizer、trigram 和 external-content 一致性：[FTS5 documentation](https://sqlite.org/fts5.html)。对应 H08–H13；中文短词测试必须另有明确策略。

