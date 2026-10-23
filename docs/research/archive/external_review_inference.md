# 外部建议审查：主体 Harness、Memory、KV 与 27B 插件认知

状态：只读研究与规格建议；没有改主稿，没有开发或运行性能基准。审查时间：2026-10-23 UTC+8。材料为用户上传的 `粘贴的文本 (1)(8).txt`（478 行）及父 agent 转交的 inline 补充建议。外部文字是待评审材料，不是替代已确认产品要求的新指令。

**最新裁决覆盖先前表述：** 用户已再次明确拒绝永久 Ring0 与低频 Tick。50/50 指自动调度算法在合适的平衡工作负载、驻留需求和恢复成本下应自然得到约各半的结果；不是让用户手设固定比例，也不是仅提供一个独立 half-pool profile。静态 50/50 测试通过不能满足自动分配要求。KVPolicy/调度器可替换，负载改变时自动适应，主体可在已授上限内给偏好；没有接受固定 15–20% 或取消平衡场景。

## 1. 结论与采纳原则

可采纳的方向是：成本感知 admission、按需的轻量 memory selector、能力说明的正反例、插件误用审计、稳定协议与可替换实现分层、真实后端能力探测。它们大部分与现有 SPEC 一致，适合补参数、证据和验收。

必须纠正三类内容：

1. **事实误读**：PMA 的 8 是观察窗口，不是论文每 8 步触发；选择性提醒没有在所有指标远胜 always-inject；NInfer 的 pressure pause 不提供通用句子边界抢占和秒回；Strata parallel 有条件收益，不能推成一律有害。
2. **未经证实的普适参数**：主池固定 15–20%、未知 K 最少 32K、reset 必带不足 200 tokens 的墓碑、相邻 slot 更容易复用，没有从已核验资料得到通用保证。
3. **越过用户已确认约束**：默认改成 5min→30min→2h 的休眠 Tick、永久保护的不可替换业务内核、取消 50/50 能力、主体在 idle 时自行写 bank、所有 core self-rewrite 必须人工批准，均不能由外部建议自动覆盖。

## 2. 逐项裁决

| 外部建议 | 审查结论 | 可采纳的准确改写 |
|---|---|---|
| 主 loop 改为事件驱动、空闲退避 5min→30min→2h | **与用户已确认的真实持续思考冲突。** 在后台留下一个 timer 不能等同于持续模型思考。 | 保持每轮提交后登记唯一后继；可调整每步推理深度、上下文与输出 quantum，按公平调度与 children 交替。事件触发/合并适用于辅助维护 worker，不替代主体。owner 日后主动选择节能模式时另列清楚语义。 |
| 不引入 preemption 就不能同时持续主体与低成本子 Agent | **结论过强。** 有限 generation 完成后释放许可、轮次间协作调度已经能让 C=1 双方推进。 | 请求内抢占是可选低时延优化；基础正确性依靠有限单步、无持锁等待、公平队列及有界重试。无法中止的长请求确实限制交互时延，需要如实展示。 |
| observe 阶段只查队列，仍称永不停止思考 | **语义偷换。** 确定性读取和低成本模型判断是不同状态。 | UI 区分实际推理、等待资源、恢复、确定性后台操作；可用小模型做部分判断，但需要符合主体连续推理的明确配置，不能把不调用模型的等待悄悄当默认主体。 |
| 主体 reflect 时更新 memory bank | **与主体不直接操作文件/记忆机制由 worker 执行冲突。** | 主体提交 semantic save/revise intent；memory worker 读写 bank、Git 与索引。确定性写入可不调用 LLM，但仍不赋予主体裸文件/SQL 工具。 |
| KV-aware scheduler、session store、thought loop 属于不可替换 ConstitutionalModule | **与极小 kernel、任何部分可自改冲突。** | 保留不可被普通输入绕过的当前行为合同；其实现依然可替换。broker、harness、history 是 bootstrap provider，交接时保留 epoch、未决请求、lease 和数据迁移回执。 |
| 内核/调度参数 self-rewrite 默认 ask | **增加用户没有要求的逐次审批。** | 既有权限内允许 worker 自主候选构建、验证、提升和回滚；新增管理员或外部权限另属授权事实。调参在 owner envelope 内由主体自由调整。 |
| 一半 KV 给主体会让所有 children cache miss，应取消 | **并不成立，且误解最新裁决。** child 约半池可以容纳多份工作状态；命中取决于工作集与缓存能力。 | 以自动分配为产品要求：平衡条件下自然约各半，需求变化后适应。固定比例仅可作为可选覆盖/对照，不能代替算法验收。 |
| 主池必须 pin 15–20% | **没有普适依据。** 小窗口主体可能只需少量；多 scope 投影、混合模型 state 或长主上下文也可能超过此数。 | 15–20% 可以是某次负载下算法算出的结果，记录依据；不能设成普适常数。按 footprint、返回概率、恢复成本和公平性适应，平衡条件下应能自行约各半。 |
| NInfer Host 默认 8 GiB | **近似但不完整。** 固定 serving 文档写的是 8 GiB 加八份模型 StateImage，显式 `--host-context-mib` 覆盖总额。[N1] | 教程展示实际默认组成与设备可用内存；8 GiB 是推理节点配置，不能当路由器的必选内存需求。 |
| NInfer 可句子边界无损抢占、秒回、零 prefill | **没有该通用保证；部分与正式调度规则相反。** 稳定提交边界不等于自然语言句子；新到达请求不主动抢占 resident；Snapshot 不足时可 Replay。[N2] | adapter 分开声明 engine pressure pause、调用方可请求的暂停、取消、快照恢复、replay 与 latency 观测。没有正式 API 不承诺 caller 控制；零历史 prefill 只在确实复用的范围报告。 |
| Strata 跨域并发 decode 必然 expert thrash，因此禁止 parallel | **错误泛化。** 官方文档同时列出 batch 共享权重收益、slot 挤占 expert cache 的成本及不同部署的测量。[S1] | 默认按 backend 与模型 profile 选择并发，保留 owner override。实际比较串行、多驻留轮转、parallel 的 TTFT、总完成时间、expert miss/传输与单流速度。 |
| main/memory/children 必须完全相同 system prefix，才能零拷贝 | **不成立，也会破坏角色/权限分离。** 只能共享相同 token、同模型/模板/可见域且 backend 支持的不可变前缀；其余 state 与新后缀有实际成本。[N1,N2,S2] | 可复用公共协议前缀，但按角色保留不同工具/任务边界。缓存仅优化正确的输入，不能为了复用把 memory writer、SSH 或 raw exec 工具泄漏给主体。 |
| 未知 K 默认 `max(4K × sessions, 32K)` | **维度和安全性都不足。** K 是实际物理容量；会话增多不能让物理内存随公式变大。token 与 bytes/state 也不通用等价。 | 报告 capacity unknown；读取可信 metadata/metrics、模型 geometry、KV dtype 和 state 预算；采用保守的请求/context 工作集与压力回退。配置 token ceiling 可以独立存在，但不得伪装实测物理 K。 |
| 把共享 prefix 放到同 slot 或相邻 slot 增加复用 | **同 context 的真实 affinity 可以有效；相邻数字 ID 没有通用语义。** cache 可能为共享 page/radix/Host snapshot，slot 编号不等于邻接内存。[N2,S1,S2] | 使用 backend 明示的 context handle、实例 affinity、exact-prefix cache identity；不根据 `slot_id + 1` 推断复用或位置。 |
| PMA 每 8 steps 执行两 phase | **前半句误读。** 主实验每 step；8 是近期观察窗口。代码配置为 `trigger_interval: 1`、`sliding_window_size: 8`。[P1,P2] | 两 phase 可作为研究对照；产品触发频率独立可配，可做事件合并或每 N 步候选，但注明是新策略，重新测质量与成本。 |
| PMA 严格 transient，消费后无任何历史痕迹 | **论文语义与公开路径不能混同。** user_turn 注入被底层 Chat 追加进 `_messages`；清空 pending 不能证明下一次 prompt 不含它。[P3,P4] | 区分 raw audit 与 model context projection。严格 transient 只指后续默认模型请求不再携带该 overlay；原始注入事实、source 和当次 call manifest 仍持久。 |
| selective 远胜 always-inject | **不符合消融全部指标。** macro 为 64.3/63.5，而 micro 为 61.2/61.5（selective/always）；没有“全面远胜”的依据。[P1] | 采用 grounded reminder/silent 的可插拔机制；本地默认由等预算质量、干扰率和额外调用成本评估决定。 |
| reset 必附小于 200 tokens 的失败墓碑 | **是可试验的新策略，不是论文原法。** 论文 fresh 轮次排除旧失败叙事；强制墓碑可能把锚定重新带回。[F1] | 必带现行任务约束、权限、预算、已发生外部副作用和已验证环境限制；失败假设默认不带。可将有限墓碑作为独立 policy/ablation，注明来源和置信，不将未证假设固化成禁路。 |
| PMA 已证明 27B 推理强、世界知识弱 | **论文没有测这个命题。** 其训练对象是 Qwen3.5-27B memory policy，action 模型是冻结的 122B；未训练 memory 反而降低该验证 reward。[P1] | 把知识/推理/选择/执行/长程状态拆开评估；不可将一种模型的一项 policy 训练结果推成整个参数级别的认知结论。 |
| Skill 心智对 27B 必需且不是过渡品 | **无普遍证据，且违背可替换 discovery/teaching 的未来需求。** | 短 manifest、反例、相关示例与静态 docs 作为候选；schema-only、短指南、native wrapper、检索与程序化 workflow 在目标版本/量化/任务上比较。 |
| 插件触发正反例与 cost_profile | **值得吸收。** 但 manifest 自报不是可信性能实测，更不能授予权限。 | 绑定 plugin/schema digest，区分 estimate/measured/unknown；cost_profile 声明模型调用、resource group、context/prefill、工具 IO、取消与副作用。 |
| Plugin Use Auditor；N 次成功后 stable | **前者有价值，后者证据不足。** 简单使用计数会被重复容易样本误导。 | 有界审计 worker 从成功/失败/未选用轨迹抽样，检查漏用与误用，产出候选改进；提升靠独立自然任务/反例/版本/权限/恢复 conformance，不强制 user approval 替代验证。 |

## 3. NInfer 抢占与恢复：应保留的真实能力

本次重新读取固定 commit `64d10fbf9f8e6bfc79ab0570b94674c5b9997929`。正式文档区分 execution lanes、额外 Device StateImages、shared typed KV 与 Host backing；一个请求可以暂时使用大部分 KV，压力时暂停/恢复。不能把这种内部资源机制包装成公开支持“任意 bot role 硬 pin、比例分区或按紧急来信随时抢占”的 HTTP 协议。[N1,N2]

以下差异直接影响规格：

- **暂停时机**是 Native 稳定提交边界；正在 materializing、已终结、持完整恢复许可的请求不是任意 victim。不是看到句号就安全暂停所有模型状态。[N2]
- **到达优先级**不能由上层假造：正式规则写明 fresh admission 不主动抢占 resident；缩短 prefill chunk 不会让一个没有 lane 的新请求凭空入场。[N2]
- **可恢复语义与恢复成本分开**：Snapshot 保留完整恢复状态，Replay 重新计算到既有 frontier。Replay 不重发历史输出、不重新抽样旧 token，仍然消耗计算；未使用的 speculative drafts 也可能被丢弃/重建。[N2]
- **cancel 不是 pause**：取消可能使生成不完整；切换新请求能复用多少取决于精确 token/history 与剩余缓存。正式 serving 文档提醒规范化 JSON 或重新编码生成文本可能改变 exact-prefix 可复用位置。[N1]

因此可以支持“恢复时避免重复外部效果与重复输出”，但不能承诺“所有抢占完全无损、秒回、零 prefill”。交互等待由 queue、lane admission、正在执行单元、snapshot/transfer、replay、新后缀 prefill、decode 共同决定。零 prefill 必须定义是零历史重算还是连新输入都没有；有新来信时通常仍需处理新 token。

附件“4 个并发 150K session”没有给出可复查的具体 trace 与设备配置。本次检查 TTFT 工具 README 未看到对应无条件承诺；即便找到某次通过的 benchmark，也只能作为所记录模型/typed pool/Host/硬件/请求轨迹的能力证据。不能据此保证任意四个 150K 请求的延迟或无重算。[N3]

## 4. Strata：专家缓存与会话缓存不能混为同一个 K

固定 commit `011c9e600a93bbc9fd451c563501b5831c32bbca` 的 BATCHING 文档明确支持 opt-in parallel。它说明 batch 可共同读取 dense/shared/routed expert 权重，也说明额外 session state 会挤占 expert cache。官方 setup 按可驻留专家比例和 slot 成本推荐；用户显式并发设置仍允许保留。[S1]

这支持**按配置选串行或并行**，不支持“跨域请求必然 thrash”的绝对论。主题/任务不同可能改变路由分布，但不能只凭“一个 coding、一个 chatting”预测每层每 token 的专家集合，也不能将 token/state pool 与模型 expert cache 直接加成一个通用 tokens 数。

可采纳的候选策略：把总请求完成时间、交互 TTFT、单流 decode、expert hit/miss、CPU/PCIe 传输、KV/recurrence 状态、Host parking 放在同一实验记录中；在 owner cap 内，auto 选择适合该部署的有效并发。串行多上下文保留已有官方 Host conversation cache 路线，不能把它写成并发解码，也不能把 Host RAM 误称 GPU 硬保留。[S1,S2]

## 5. PMA、reset 与本产品的正确组合

PMA 的有用结构是：观察有限轨迹、维护带来源的执行状态、选择提醒或保持沉默。两 phase 与触发频率是两个参数。silent 仅表示不注入提醒；既然两 phase 已执行，成本不会消失。本地实现可以先用确定性增量记录、lexical/embedding 检索和有界模型 selector，但是否更便宜且不损质量必须评估，不能预设“轻量 scorer 必定等价”。[P1,P2,P5]

**严格 transient 的合同建议**：每个 hint 绑定目标 task/scope/epoch、source、适用条件与有效期；调用投影将其只放在指定的未启动请求；后续默认投影排除它，但原始 ledger 保留注入事实及当次请求 manifest。若后端仅 append，不能删除先前 overlay 后仍谎报原会话零重算；可选择新窗口/重建，或明确使用持久回执+过期标记模式。两者需独立评测。

**reset 的合同建议**：研究 reset 只刷新有限 worker 的可见探索状态。主体切窗则恢复身份、现行承诺、未完成任务、预算、权限和原始 source refs；个人 Agent 的每一轮不一定存在“best commit”，不能把论文的代码优化 seed 结构硬套为唯一主体恢复包。

失败墓碑可分两类：

- **必须保留的现实约束/回执**：某凭据已撤销、某外部变更已发生、用户否决了某动作、当前环境确证没有某接口。这些不是任意可忘的旧猜想。
- **可排除的探索叙事**：某旧 agent 认为路线无望、未充分验证的失败解释、长篇尝试经过。为了 fresh exploration 可以默认不注入；必要时显式 recall。

若实验选用 200-token 墓碑，应标为成本预算而不是事实正确性的神奇阈值；不能因装不下就丢掉关键权限/外部副作用。比较 best-only、verified-constraints-only、bounded failure notes、完整历史等条件，并保持总模型/评估预算一致。[F1；本段策略为产品提议]

## 6. 插件认知：保留实际帮助，去除永久理论

建议保留 `use_when`、`not_when`、examples、prerequisites、effects、errors 与成本字段；不用断言这是所有 27B 的永久心理机制。公共执行合同和可替换发现策略分离，未来模型可以不读 Skill Markdown，但权限、schema、版本、回执与 conformance 仍然有效。

Plugin Use Auditor 作为有限 worker，消费经 scope 过滤的原始轨迹和候选 capability catalog。输出漏用/误用/指南不足/实现错误/证据不足的区别，提出解释或候选修改，不直接把模型推断写成已验证 memory。held-out 用例不能被同一 self-improvement 过程公开修改后作为唯一成功证明。审计器本身也可替换；当前有效合同和独立回执保留。

27B 优化按失败位置选择：

1. 提供 oracle 证据后变好，才支持知识/检索缺口的解释。
2. 提供 oracle 工具名后变好，说明候选发现/选择可能是瓶颈。
3. 提供正确参数草案后才变好，检查 schema、单位、路径、协议和工具调用训练。
4. 单步都正确、长程失败，检查任务状态、语义提醒和 context management。

这些是诊断性实验，不把混合影响强行当单因果结论。模型版本、量化、chat template、reasoning effort、工具 parser、语言和上下文长度均进入 profile。PMA 训练改善的是其 memory policy 校准，不能代替以上测量。[P1；完整工具认知一手来源见 `research_plugin_cognition.md`]

与另一路审查的一致补充：JIT 指导优先读准确版本的静态 docs/schema 或已注册 recipe；确需运行 help 时由 worker 在能力范围内执行，按 binary digest/version/subcommand 缓存。`--help` 是惯例而非无副作用证明；不能对任意 shell pipeline 或 SSH 字符串盲目拼接 `--help`。

## 7. 建议并入 SPEC 的短条款

以下均是对现有要求的补强，不修改用户已确认方向：

1. 主体每轮完成即提交唯一后继，主子在有限 quantum 间公平调度；后端请求内 preemption 是可选能力。
2. 每个请求记录 compute permit、context residency、取消未知三类独立状态；cache lease 不等于正在计算。
3. 自动 KV 分配是要求；平衡条件下算法应自行约各半，变化时适应。静态比例只是可选覆盖/评测对照，不代替算法。主池覆盖所有 main scope projections 的合计。
4. 不能依据 slot 数字邻接、URL、模型 alias 或输出文本相似度推断物理共享/容量。
5. backend 自报能力和可观察指标有版本；unsupported/unknown 保留为正式状态，不以拍定的 tokens 公式补空。
6. memory frequency、selector 模型、transient/persistent 投影、research reset carry-over 均可独立配置与替换。
7. 不以 N 次工具调用、N 次容易成功或合法 JSON 宣称插件“已学会”；用自然任务、反例、版本与恢复测试。
8. 所有模型消费者都经 broker；文档中的“Remote API 走 GlobalConcurrencyGate”必须扩展到 main/child/memory/rerank/ASR 等所有受控共享资源请求。
9. 文件 checkpoint、模型 Snapshot、context ledger 和外部副作用 receipt 是四种不同状态。LayerFS commit 本身不使活跃模型 generation 可无损抢占。
10. 测量总完成成本和用户等待；不能只最大化命中率、只比较单流 tokens/s，或把某次快恢复推广为全局 SLA。

## 8. 最少新增验收

| 场景 | 通过条件 |
|---|---|
| C=1，无外部消息，main 与两个 children 永远 ready | main 每步继续且不休眠至 5/30/120 分钟；children 按配置公平得到计算；无请求内抢占能力也可推进。 |
| C=1，未填写比例，主/子两类需求充足、驻留收益与恢复成本平衡 | auto 自行获得接近各半的可用驻留分配；离散 page/state 的容差预先定义；检查真实驻留与复用，不能只在 UI 输出 50/50。 |
| 上一平衡负载随后变为主体小且稳定、children 工作集扩大，再反向变化 | auto 根据可观察边际复用收益与成本调整份额，并能回到平衡；不是永远 50/50，也不是固定 15–20%。公平活性和物理 cap 始终满足。 |
| 关闭 NInfer Host backing 或造 snapshot 空间不足 | 如实出现 replay/等待/拒绝，实际重算计入成本；不显示无损零 prefill。 |
| Strata 两个不同任务，分别在低/高专家驻留配置比较 | 选择依据实测完成时间、TTFT、miss/传输和公平性；不根据“跨域”标签一律禁并发。 |
| K unknown，agent 数从 1 增到 50 | UI 仍 unknown；物理 budget 不随会话数伪增；上下文和 compute admission 保持保守可观测。 |
| memory hint 只允许用一次 | 下一实际请求有 hint，其后 projection 无 hint；raw audit 仍可读；append-only backend 的重建/模式差异真实显示。 |
| reset 后注入一条未经验证的失败墓碑 | strict best-only 模式拒绝自动注入；可选 bounded-note 模式有来源/置信标签，且不得删除仍有效约束。 |
| 插件升级参数 schema，旧指南/向量索引尚在 | 旧 binding 失效，重新发现/绑定；不能靠历史 N 次成功继续显示新版 verified。 |
| core/broker 自升级期间旧请求 cancel unknown | 新版本继承占用与 singleton attempt 状态；不因换插件释放幽灵请求的容量。 |

## 9. 一手来源与本次核验位置

| ID | 固定 URL / 版本 | 本次读取与用途 |
|---|---|---|
| N1 | https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/serving.md | GitHub connector 重读；blob `8d3401199c91741909d62b578d855663bc645757`；Host 默认、请求/state/KV、exact-prefix 与缓存 hints。 |
| N2 | https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/maintainer/resource-scheduling-and-context-cache.md | 重读；blob `3112c6094aab610edba24436434f647f5b254131`；§8.2–8.3 稳定边界、fresh 不主动抢占、Snapshot/Replay。 |
| N3 | https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/tools/bench/ttft/README.md | 重读；blob `562bb295b0799435a6aedd5a2c0d9719bf0a0864`；需区分 evidence/未观测/性能，固定 trace 不等于延迟保证。 |
| S1 | https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/BATCHING.md | 重读；blob `6e7b58d392598465eb2bb80da4924d939d45ada5`；batch 权重共享、slot 成本、推荐与 override、solo/batch 路线。 |
| S2 | https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/DETAILS.md | 读取此前保存的同 SHA 正文；blob `2b8e96591af1902d0a062c72cfa941329a14544a`；Host conversation parking、exact prefix、失败回退。 |
| P1 | https://arxiv.org/html/2607.08716v1 | 本次 web refs `turn105view0`、`turn107view0`、`turn107view1`；§3.4、§4.1、Table 2、§4.5。 |
| P2 | https://github.com/yifannnwu/proactive-memory-agent/blob/89e5c0d6aadfe531a1aee42fd290d48be89973dd/configs/memory_terminalbench.yaml | 本次重读；blob `f06855cb7d8ccf13268fcb1748bbf23482eb1f1f`；interval=1、window=8、user_turn。 |
| P3 | https://github.com/yifannnwu/proactive-memory-agent/blob/89e5c0d6aadfe531a1aee42fd290d48be89973dd/src/memory_agent/memory_enabled_agent.py | 重读 145–325；blob `9aadfa1ffbb8825434d9f4e2cbbbee242b20a0f0`；pending 清空与 prompt 拼接。 |
| P4 | https://github.com/yifannnwu/proactive-memory-agent/blob/89e5c0d6aadfe531a1aee42fd290d48be89973dd/external/harbor/src/harbor/llms/chat.py | 重读；blob `49b88ed06605a4649e83079c31893ea52daf47e4`；Chat.chat 将完整 prompt 加入 `_messages`。 |
| P5 | https://github.com/yifannnwu/proactive-memory-agent/blob/89e5c0d6aadfe531a1aee42fd290d48be89973dd/src/memory_agent/memory/memory_agent.py | 重读 246–330；blob `216cedf07837677c198cf9b7bafdea04858e6435`；两个 phase 不同 system/tool 面；结合此前 process 路径确认串行两调用。 |
| F1 | https://arxiv.org/pdf/2610.20563 | 本次 `turn110view0`；§3.1 新 session 与 best candidate，不保留旧失败叙事；详细完整审查见 `research_forgetting.md`。 |

父 agent 对外引用子 agent web ref 前需自行 open/find。此文件的策略建议和验收设计没有声称被论文或上游实现直接验证；引用原文时遵守各来源 word limit，不将内部笔记当成新的独立证据。
