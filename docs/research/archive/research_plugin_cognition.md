# 插件发现、正确使用与本地 27B 模型的认知负担

研究日期：2026-10-23（Asia/Shanghai）。本文件是文献核对与产品契约建议，没有开发、安装、训练或跑 benchmark。文献中的数字属于各自实验，不能当成本产品或任意 27B 模型的性能。

## 1. 结论与需要验证的假设

插件生命周期必须覆盖 **安装 → 健康检查 → 可发现 → 正确选择 → 加载正确版本的接口/说明 → 执行 → 验证结果**。仅安装成功，或者模型看过一次说明，不能证明它会在合适时机使用，更不能证明调用正确。

用户提出“27B 推理已经很强，主要欠缺世界知识”，可以作为重要优化假设，但不能作为所有 27B 的既定事实。需要分别测知识可得性、工具候选可得性、工具选择、参数构造、执行/错误恢复和长程目标保持。知识 RAG 补上事实，不会自动修复参数 schema 错误；能力检索补上工具候选，也不会自动保证正确执行。

在当前日期，应把 Qwen3.8-27B 纳入目标，而不是停留在 Qwen3.5。Qwen 官方 model card 描述的是 27B dense、原生视觉语言、混合 Gated DeltaNet/attention 模型，并报告长期 agent 任务与多模态能力。卡片同时提醒：降低 reasoning effort 可能增加失败/重试，未必降低端到端时间。因此不能把“主 loop 用最低思考档”当作未经评估的默认优化。[Qwen 官方卡](https://huggingface.co/Qwen/Qwen3.8-27B)

这里提出的默认方向是：稳定而小的主体工具面、经验证的结构化能力说明、worker 内的 native wrappers、按任务加载少量相关指南、可追溯的外部知识和独立的分层评估。所有发现/检索/说明策略均可替换；Skill 风格只是初始适配形式。

## 2. 五篇一手论文：方法、能支持的结论和边界

### 2.1 SkillsBench — 不是“放上 Skill 就会变聪明”

**来源与版本：** [arXiv:2602.12670v4](https://arxiv.org/html/2602.12670v4)，初稿 2026-02-13，当前 v4 2026-06-14。

当前报告为 87 tasks、18 model-harness configurations；curated Skills 的平均 pass rate 从 33.9% 到 50.5%（+16.6 percentage points）。聚焦、至多三个模块的说明较大而全的 bundle 更有利。作者还发现：高 Skill invocation 不保证高任务完成率，很多失败发生在已经访问 Skill 之后。

v4 的 self-generated diagnostics 在三个 dedicated harness 配置中低于 matched no-Skills baseline，且有覆盖缺口和一个 effort 配置偏差。不能把“自行写教程”直接等同于可靠自改进；也不能把这组负结果扩展成所有验证过的自动生成插件都无效。

**产品含义：** 最小相关指南、可执行 verifier、区分发现/执行瓶颈。早期版本的 84 tasks/+16.2pp 不应与 v4 数据混用。本论文没有证明目标 Qwen3.8-27B 获得同样收益。

### 2.2 ToolRet — 通用语义检索不是现成的工具选择器

**来源：** [Retrieval Models Aren't Tool-Savvy](https://arxiv.org/html/2503.01763v2)，arXiv:2503.01763v2，2025-05-26；ACL 2025。

ToolRet 汇集约 7.6k retrieval tasks 和 43k tools，比较多类检索方法；通用 IR 表现好的模型，在工具检索上仍可能表现不佳，候选缺失会影响后续 agent 完成任务。它还研究针对工具检索的数据/训练。

**产品含义：** 不能默认“接一个 embedding + top-k”就完成插件认知。单独评估 candidate recall/completeness，再评估模型在候选齐全时能否选对和执行。跨工具细微功能差异、前置条件、权限和输入输出都需要进入可检索表示。

**限制：** benchmark 工具与本机 SSH/memory/audio 插件分布可能不同；检索指标改善不等于本地真实任务成功，也没有给本系统规定一个普适 k。

### 2.3 ToolScout — 来源/表述风格失配可造成候选崩塌

**来源：** [When Tool-Backed Skill Retrieval Fails](https://arxiv.org/html/2608.16502v1)，arXiv:2608.16502，2026-08-17。

论文展示：窄来源训练的 dense retriever 在同一大语料的其它来源切片上可能严重失配；把工具重新渲染成 Skill cards 并不能消除问题。ToolScout 用 source-style compatibility 信号决定路由到何种检索器，并保留来源匹配的小样本适配。

**产品含义：** 插件描述要统一但不能只信统一格式；英文 API 文档、中文口语、ASR 转写和本地命令习惯应分来源测试。保留 lexical/泛化检索 fallback；对已知失配的专用 dense 模型，简单融合可能不如直接路由离开。

**限制：** 主要比较使用共享检索/候选暴露/选择管线，覆盖率与 top-1 proxy 不是完整 agent 的复现或执行成功保证。其 TF-IDF guard 可作为待测候选，不能凭论文直接宣称适合所有中英文输入。

### 2.4 FitText — 工具候选可以在执行中修正，但搜索要有预算

**来源：** [FitText: Evolving Agent Tool Ecologies via Memetic Retrieval](https://arxiv.org/html/2605.02411v4)，arXiv:2605.02411v4，2026-08-09。

FitText 用自然语言 pseudo-tool 描述作为检索探针，支持顺序修订、平行探索和带记忆的演化选择，使错误的初始候选能在任务中被替换。当前主实验报告改善，但保留的旧 Qwen3-30B 诊断里更复杂的搜索反而退化，表明生成检索探针本身也需要能力。

**产品含义：** 无候选/执行反馈不吻合时，可以让 discovery worker 做有限次改写和再检索；先测 shallow serial policy，再考虑 population。主 loop 不因此分叉成多个自我续写。

**限制：** 当前主表是其它具体 model-harness 配置，不是本产品 Qwen3.8-27B；论文的 latency 是 concurrency=40 下的负载结果，不能移植为 C=1 本地 GPU 的“搜索几乎免费”。所有探索、embedding、rerank 都要计全局资源和根任务预算。

### 2.5 Tool-DE — 补齐工具说明，但新增文本仍要验证

**来源：** [Tools are under-documented: Simple Document Expansion Boosts Tool Retrieval](https://arxiv.org/html/2510.22670v1)，arXiv:2510.22670，2025-10-26。

论文对工具文档扩充 function intent、when-to-use、limitations、tags 等结构化字段，配合生成、判断、修订和人工验证，研究工具专用 retriever/reranker。它支持“用途/前置条件/限制与名称/schema 同样影响发现”这一设计方向。

**产品含义：** manifest 应有适用/不适用条件、实例和失败说明；可由 worker 草拟，但须依据真实 API 和 conformance fixtures 验证。不能让补全文档凭空扩大插件能力。

**限制：** 主要是工具检索实验，不能据此保证调用、权限或真实副作用正确；文档膨胀也会增加上下文和维护成本。全部长文不应常驻主体 prompt。

## 3. 先定位缺陷，再选择优化

以下是产品评估设计，不是论文声称已证明的通用因果分解。

| 缺陷类型 | 对照实验 | 对应改进 |
|---|---|---|
| 世界/领域知识缺失 | 其它条件相同，提供经过验证的必要事实与来源，观察任务完成变化 | 领域 RAG、官方文档/私有知识库、带出处的 memory；检索新鲜度与冲突处理 |
| 找不到正确插件 | 提供 oracle 工具候选，但不提供解法 | 改 manifest、hybrid retrieval、语言/来源路由；不能只加推理长度 |
| 候选在但选错 | 提供完整最小候选与明确区别，观察选择 | 更好的 use_when/not_when、对比实例、前置条件/环境过滤 |
| 参数/协议错误 | 指定正确工具，测自行构参；再提供 oracle 参数对照 | typed native wrapper、例子、schema/units/enum 校验、结构化错误 |
| 执行与恢复失败 | 正确调用遇到超时、权限、分页、部分成功、版本错 | 幂等 receipt、状态机、可操作错误分类、retry contract、确定性 workflow |
| 长程目标/依赖丢失 | 给同等知识/工具，但改变上下文保持与任务 ledger | memory/context worker、明确依赖、承诺锚点、历史按需读取 |
| 工具结果解释错 | 提供确定性的结构化结果和 oracle 摘要对照 | worker 内程序化聚合/校验、缩减噪音、引用原始结果 |

实验固定 model revision、量化、推理 backend、template/tokenizer、sampling、context/output cap、工具版本与 harness。不同维度有相互作用；不能把所有差值相加，当作已经精确测出每类缺陷比例。

若给足事实后仍选错工具，不应继续堆知识检索；若工具已选对但 shell quoting 错，改进应落到 wrapper。若给足工具和参数仍失败，需检查执行环境和适配器，而不是把错误归因于模型世界知识。

## 4. Plugin 使用契约

插件的稳定接口应是 machine-readable capability contract；Skill Markdown 可作为其中一种人/模型可读视图。

### 4.1 Short manifest

| 字段 | 内容与验收 |
|---|---|
| identity | plugin/capability ID、version、content digest、发布来源、兼容 host/runtime 范围 |
| short purpose | 一句话能做什么，避免泛称“万能助手” |
| use_when | 可观察的触发条件/任务类型，中英文查询别名 |
| not_when | 易混淆场景、已知限制、应改用的能力；不能凭“关键词像”就选中 |
| preconditions | OS/WASM syscall/网络/工具依赖/已配置目标/认证能力；由系统验证，不靠模型猜 |
| input/output | typed schema、单位、默认值、长度边界、稳定错误/结果结构 |
| effects | read/write/send/execute、幂等能力、所需权限与 scope；描述不能授予权限 |
| cost/resource | 是否触发模型/embedding/ASR/TTS、资源域、可取消性、典型调用结构；实测成本与估计分开 |
| examples | 最小成功实例、易错反例、超时/权限/部分成功实例；使用合成数据 |
| guide_refs | 分层说明/完整参考/可执行 workflow 的 content-addressed 引用 |
| conformance | fixture suite/version、最近已验证环境、结果与已知不支持项 |
| lifecycle | installed、healthy、discoverable、deprecated、revoked 等真实状态 |

short manifest 有可配置 token budget，不把全部 README 塞进 root。用途、能力、权限和环境约束都需要能稳定检索；只记录名字与一句含糊 description 不够。也不要把论文的“至多三个模块”机械转换成所有任务最多三个工具的限制。

### 4.2 安装完成的定义

安装 receipt 必须明确：安装的版本/路径/ABI、依赖与 credentials 是否 ready、哪些 capability 已通过 conformance、哪些仅注册未能运行、如何发现、如何卸载与回滚。索引更新应有 registry revision 和可见水位。

主 loop 通过 worker 安装并收到 receipt 后，下一轮必须可发现该能力。不能等重启或要求模型自行猜目录。被撤销/不健康的能力不应继续以普通可调用候选暴露。

## 5. 渐进披露的初始心智

1. **常驻主体小界面。** 保持 handoff、异步 memory/progress、能力查询与 policy 调整的少量稳定接口；主 loop 不因安装一个 SSH 插件就直接获得 exec/file 工具。
2. **先查 capability。** discovery worker 在授权 scope 内按任务、目标环境、健康状态和权限过滤，再进行 lexical/semantic 检索，返回有区别说明的有界 shortlist。
3. **为任务加载。** 主体委派后，执行 worker 只加载相关 native tool schema 和短操作指南；遇到边界再读详细参考。
4. **反馈可修正。** 无合适候选、前置条件不满足或调用反馈不吻合时，有限次重检索/改写问题/更换能力，不无限猜工具名。
5. **保留使用证据。** 记录选择原因、插件 revision、实际调用结果和 verifier 输出，供后续自改进 worker 分析。

小而稳定的插件集合可以直接保留简短 catalog，不必每次都额外调用 embedding/reranker。大集合才使用 retrieval；额外模型消耗是否划算由目标任务测定。中文自然语言、路径/命令 exact match 和口语/ASR 输入应有不同的可测覆盖。

discovery 是有界 worker/service，不是第二个常驻全局主体。即使以插件方式实现，外部消息仍先注入 main；不得由能力检索器直接决定用户最终回应或全局目标。

## 6. Native wrappers：把易错机械工作从模型中移走

这些是建议接口形状，不是已经实现的 API。

| 高频能力 | wrapper 应承担的工作 | 模型仍负责什么 |
|---|---|---|
| SSH exec | target/credential refs、argv/cwd/env/stdin、host key、timeout、输出上限、cancel/receipt；明确区分 shell 字符串 | 决定任务和命令语义，不自行拼接多层 quoting/凭据 |
| SSH file read/write/patch | URI、范围、encoding、expected hash、atomic write、权限和断点 | 选择文件与修改目标，不自己重建 SCP/SFTP 协议 |
| Memory | scope/time/source filters、版本、recall limits、读后写一致性、provenance、conflict | 判断哪些事实相关、是否值得保存，不直接写 SQL |
| Git knowledge | remote/ref/credential refs、fetch/checkout 状态、认证错误、只读/写入能力 | 选择知识版本与提交意图，不暴露 token 到 prompt |
| Search/research | 来源去重、引用 ID、结果截断、缓存、再取全文 | 形成问题与证据判断，不把搜索片段当作已证实全文 |
| Audio | 流/utterance IDs、partial/final revision、时间段、ASR/TTS/声纹资源需求 | 由唯一主体理解和委派后续；音频插件不另起会话人格 |

统一结果 envelope 包含 status、结构化 result、bounded preview、artifact/source refs、side-effect receipt、retry classification、cost/usage 和 continuation handle。说明明确哪些字段能作成功证明；无输出不等于失败，HTTP 200 不等于语义任务完成。

高频机械组合可封装成经过验证的程序化 workflow，例如“读取远端日志→本地过滤→统计→返回证据范围”，只把小结果交给模型。Anthropic 的官方 Programmatic Tool Calling 说明也采用代码内编排与过滤中间结果的方向；那是特定平台能力，本产品在 worker 中复现相应模式，不假设任意本地模型/API 原生支持。[官方说明](https://www.anthropic.com/research/advanced-tool-use)

程序化工具仍通过 capability wrappers；其循环、子进程、并行请求和间接 LLM 调用不得绕过 broker。主 loop 不能借 programmatic tool use 恢复直接 exec 权限。可先使用人工/验证过的 workflow，模型自写 workflow 由 worker 负责测试、版本化与回滚。

## 7. 使用正确性的可执行验收

每个 capability 不仅要有 API schema test，还应有可观察结果的 conformance fixtures：

- 参数正确/错误、单位与 Unicode 路径、空结果/截断/分页、缺 credentials、目标离线、版本不兼容。
- read 与 write、preview 与 commit、accepted 与 finished、cancel-requested 与 stopped 的区分。
- 重复请求的幂等性、部分副作用、未知结果和可允许的 retry。
- 中文/英文/口语正例和易混淆反例；正确动作是“不需要工具”的样本。
- tool description/返回内容中的不可信指令不能改变 host capability、owner 权限或全局 C。
- 插件卸载/重载期间的资源清理与旧 callback fencing。

选用能力的评估必须包含真实目标模型。报告 discovery recall/completeness、选对 capability 的概率、valid-call rate、任务 pass rate、错误恢复成功率、额外模型请求/token、uncached prefill、端到端时延和副作用正确性。不要以高 invocation rate 当成功，也不要只看 schema 合法率。

对于目标 27B，至少比较：无说明、仅短 manifest、相关短指南、长全量指南、native wrapper、有限再检索、程序化 workflow。再按任务类型/语言/量化/上下文长度分组。每项优化必须在预算和 KV 可比条件下评估，不能让某一组多跑几十个请求后只报告更高 pass rate。

## 8. 自主学习与过期管理

可以积极让 self-improvement worker 从失败轨迹提出新指南、examples、wrapper 或检索别名，放在分支中试验。进入默认能力集前，需要现有 conformance + 针对失败的 held-out 用例；同一轨迹写出的规则不能只在同一轨迹上自证有效。

自动生成内容要区分：经过验证的能力事实、经验性建议、尚待验证的假设。memory/Skill 的成功自述不改变事实层。新版若在其它任务上退化，可回滚 manifest/guide/retriever policy，而不回滚真实历史或已完成副作用。

每份索引/说明绑定 plugin version、API/schema digest、runtime capabilities、验证环境和最后核对时间。升级发布后，更新 registry revision，失效相关索引和 stale examples；正在运行的 worker 可使用固定版本完成，安全撤销/关键不兼容则明确停止/迁移。主 loop 的稳定前缀不必每安装一次包就全部重写。

外部工具 schema 动态变化时，不能只刷新嵌入文本而不更新执行绑定。出现 unknown_tool/schema_mismatch 应给可恢复、可观察结果，重新 discover/rebind；不让小模型无限尝试同名旧参数。

## 9. 未来移除 Skill 心智的插桩

建议分开以下服务契约，并全部作为可替换 bootstrap plugins：

- `CapabilityRegistry`：版本化、可验证的能力事实与运行状态。
- `DiscoveryPolicy`：短 catalog、hybrid retrieval、来源路由、有限伪工具探针、未来 learned/compiler policy。
- `GuideRenderer/Loader`：把当前能力转换为 Skill Markdown、短卡片、原生工具说明或其它格式，并实施 token budget。
- `ToolBindingProvider`：把合法 capability 绑定到具体 WASM/native/SSH/MCP/remote agent 实现。
- `ExecutionVerifier`：fixture 与运行结果核对；不依赖模型读某个特定 Markdown 文件。
- `KnowledgeRetriever`：事实/文档/历史检索，与 capability discovery 分开。

未来删除 Skill progressive-disclosure 形式时，只替换 DiscoveryPolicy 和 GuideRenderer；已安装能力、权限、版本、tests、历史和 worker 合同仍有效。不要把“每次必须读取 SKILL.md”写成 kernel 定律，也不要把需要拆除的发现逻辑散落在每个插件。

kernel 仅维护生命周期/恢复/capability 硬门限。上述检索、模型 provider、broker 策略、storage、runtime 和 UI 组件都可以更新和替换；旧资源/attempt/lease 的事实仍需可恢复。学习得到的策略不允许修改这些硬不变量。

## 10. 与持续主体、KV 和实时音频的兼容

main 始终保持唯一顺序思考链。插件安装/检索/技能学习都 handoff，不因为“发现工具”另开一个无界 thought loop。主 loop 的 capability 查询只返回 shortlist/receipt；具体 exec 和文件工作仍在 workers。

工具 schema 与指南的频繁变动会损害 prefix reuse；应尽量在 worker 任务开始时固定最小工具集，按需扩展时记录 revision。主 loop 常驻的小委派面保持稳定。需要动态换工具时接受真实 cache 重建成本，不能为了命中继续使用过期能力。

语音输入的含糊词可能来自 ASR，而不是模型不懂工具。评估要保留 utterance revision、置信信息和用户原音引用，区分识别错误、能力检索错误和执行错误。电话/课堂插件把信息注入 main；完整工具仍经 worker 可用。

ASR/VAD/TTS、embedding/rerank 与主模型共享设备时都声明 resource group 和资源需求。快速本地 lexical lookup 不应不必要地调用大模型；需要模型的 discovery/摘要必须经同一 broker。C=1 下严禁通过高并发检索“隐藏”额外推理，程序化工具亦如此。

## 11. 面向本产品的默认候选，不是未经实测的最优结论

先实现可替换契约和强 conformance：小稳定 main 工具面、native SSH/memory wrappers、短且准确的 capability cards、worker 按需加载的操作指南、lexical+semantic 可选检索、有限顺序再检索。只有在对应目标模型/工作负载实测受益后，再默认开启复杂 reranker、演化搜索或自动生成 Skills。

对已知简单操作可用较小思考预算；对工具选择含糊、上下文冲突或长程任务，应允许增加 reasoning/查证而非机械压低每轮 token。评价对象是完成一项正确任务的总资源与时延，不是单次模型响应看起来更短。

## 12. 父文档可用的 refs 与版本提醒

| 来源 | 已读取 ref | 关键用途 |
|---|---|---|
| Qwen3.8-27B 官方卡 | `turn85search0`、`turn86view4`、`turn89view6`、`turn89view8` | 当前目标身份/架构、官方评估条件、reasoning effort 不必然降低总时延 |
| SkillsBench v4 | `turn85view0`、`turn86view0`、`turn88view2`、`turn87view2`、`turn87view3` | +16.6pp 最新数字、发现与完成分离、自生成诊断；勿混旧版 |
| ToolRet v2 | `turn85view1`、`turn86view1` | 大工具库的候选检索瓶颈 |
| ToolScout v1 | `turn87view0`、`turn88view3`、`turn89view9` | 来源/表述失配，shared-protocol 限制 |
| FitText v4 | `turn85view3`、`turn86view2`、`turn89view2`、`turn89view3` | 动态再检索、legacy Qwen3-30B 能力门槛、40 并发非 C=1 时延证据 |
| Tool-DE v1 | `turn85view4`、`turn86view3`、`turn88view5` | 扩充用途/限制/实例，但必须验证 |
| Anthropic advanced tool use | `turn86view5`、`turn88view6` | 工具按需加载与 worker 内代码编排/结果过滤的官方工程模式 |

父 agent 在最终引用子 agent ref 前须自行 open/find。每来源遵守其 word limit；本文件主体大量内容是标明的产品建议，不能回引为论文已经验证了本产品。该文件未声称复现任何论文结果。

## 13. 定位补充：personal agent 的最小框架

用户追加的定位是架构约束：Pi 是 coding-agent 的最小框架，本产品是 personal-agent 的最小框架。可以继承 Pi 的扩展、会话投影、可替换 operations 与紧凑 harness 思想，不应把“仓库 + 编码会话 + 编码工具”变成个人 Agent 的身份模型。Pi 的上游参考快照为 `earendil-works/pi@74f46864025c68504cd10c89eeb626b7e3d64cea`（2026-10-22）；本段是基于用户定位的产品推论，不是对 Pi 能力上限的断言。

| 需要区分的对象 | personal 框架中的语义 | 对 harness 的具体后果 |
|---|---|---|
| 持续主体与任务 | `SelfIdentity` 持续存在；`TaskContext` 有边界、可完成、可丢弃其临时投影 | 完成代码任务、卸载 coding 插件或换 cwd，不能令主体退出或失去承诺。 |
| 人格与对话窗口 | 人格版本、关系和长期目标跨 session；模型 context 是有限且可重建的投影 | 唯一主体可顺序使用多个 scope/window 的 KV 投影；多投影不等于多条主动主体。 |
| 关系与权限 | 联系人别名、熟悉程度、偏好、互动历史属于关系；管理员/SSH/账号能力属于授权事实 | 自然认识某人不自动给管理权；换聊天平台不创建另一个无关的主人身份。 |
| 持续承诺与临时计划 | 承诺有对象、来源、条件、时限、完成证据与修订历史；任务计划是实现办法 | 任务取消/重试/研究 reset 不静默删除仍有效承诺；承担新的承诺必须有可恢复回执。 |
| 外部事件与编码消息 | IM、声音、课堂、设备、时间变化、worker 结果都进入统一封套 | 任意来源接入不能自行产生第二主循环；原始事件不是限定为 user/assistant/code-tool 的文本对话。 |
| 行动环境与身份家园 | WASM、SSH、桌面、容器、远端 seat 是任务执行位置 | 任意执行环境损坏或卸载，个人身份与原始事实不跟着被重置。 |
| 个人知识与代码索引 | 历史、联系人、承诺、事实来源和私域知识可独立于代码仓库 | Git 是默认文件记忆 provider；不能要求每段关系/承诺必须依赖某个工程 repo。 |
| 框架与默认发行包 | tiny kernel 提供生命周期/恢复/能力边界；personal harness、联系人、记忆、模型、UI 都是可替换 bootstrap 插件 | 默认安装的完整个人体验不等于内核必须硬编码全部行为。coding 是一个可卸载的能力族。 |

建议明确最小个人状态契约，而不把这些全部塞进 kernel：`SelfIdentityRef`、`RelationshipRef`、`CommitmentRef`、`GoalRef`、`TaskRef`、`SourceRef`、`ScopeRef`。存储、推理和提醒策略仍可替换；稳定 ID 与迁移协议负责连续性。关系知识可以是文件/语义记忆，确定性的授权与已承诺回执需要可核对的事实记录，二者不能混成一段自由文本。

能体现定位的验收是：移除 Git code operations、编译器、IDE、code search 和 coding-agent 插件后，主体仍持续思考、认识联系人、接收消息、记住承诺、委派日常任务并能恢复；再装回 coding 插件时不生成一个新的个人身份。相反，单个 coding 插件不要求为了运行一个有限任务而安装所有音频/媒体/桌面依赖。

### 13.1 “任何部分可自改”与行为合同

用户已授权软件层面的任意自我改进范围，不能把此前“最小可信内核”误读为永久不可编辑的代码岛。可替换目标包括主体 harness、人格/提示词、调度策略与 broker 实现、模型/provider/runtime、发现/教学策略、工具与插件管理器、SQLite/记忆/版本服务、UI、Rust kernel、启动器和 rescue。本文件早先提到的硬不变量是**当前用户行为合同**，不是禁止重写相应实现的源码禁区。

主体继续只委派；worker 构建候选、验证、迁移、提升与回滚。无需为每一次已授权自改发明人工审批。可通过替换代码改变实现，但不能从“允许重写代码”推导“允许增加管理员、取消用户指定 C、把私聊外传或同时运行多个主动主体”。用户后续变更行为合同，则合同和验收一起版本化。

更换全部可信实现时，不能数学保证新程序永远正确。可落实的保障是：候选内容闭包；当前有效版本的验收与签发记录；不同时覆盖所有已知可启动闭包；状态迁移可观察；旧 attempt、资源 lease、撤销和出站副作用在新版本继续可见；失败后仍有独立回退路径。验证器也可替换，但候选自己修改评估器后说“全部通过”不应是唯一提升证据。保留当前合同版本和独立的旧版/旁路验证记录，避免自证循环。

## 14. 对主稿的只读快审

**后续裁决覆盖本节旧 half-profile 建议：** 用户后来明确 50/50 应由自动算法在平衡条件下自然得出；固定各半仅可作可选覆盖/对照，不能代替算法要求。下面关于 strict/borrowable 的建议只说明若提供固定模式时应清楚定义，不再主张它是独立强制产品模式。最新自动验收见 `kv_policy_spec.md` KV16–KV18 与 `external_review_inference.md`。

审阅对象：`agent_bot_spec_v0_1.md` 的当前共享快照。未修改主稿；父 agent 可能继续编辑，因此以章节为定位，不把行号或本表作为静态最终状态。本表只补精确度，不建议为了完善规格而无限扩展开发范围。

**已经完整覆盖的方向**：§1 已明确 personal-agent 最小框架；§2.23 与 §9 允许全部软件部分自改；§5 保持无事件也串行继续且主 loop 不直接执行；§13.1–13.2 正确认可 C=1 多上下文和主子半池；§13.4 已明确旧主体请求未确认终止不能在另一 backend 并行续写；§18 将安装、发现、使用与模型验证分开；§20 没把媒体扩展槽误写成永久空留 GPU 槽。不要把这些已纳入内容再次列成遗漏。

| 优先级 / 对应章节 | 建议补精确的点 | 可自动验收的场景 |
|---|---|---|
| P0 / §5.4、A01 | 原子提交应明确包含主体决定、已消费 inbox cursor、worker intents/outbox、唯一后继轮次，而不只“决定与委派”。 | 对这四项之间每个 crash 点注入强杀；恢复后消息不丢、worker 不重复、主体恰有一个可推进后继；无消息仍继续。 |
| P0 / §7、§9.2、§13.4 | bootstrap/harness/broker/kernel 替换不能重置已发请求、取消未知、KV reader/transfer、权限撤销与外部副作用账本。新 epoch 的 fencing 不能单独证明旧服务器已停止。 | 旧 main 请求断流但 backend 保持计算时升级 broker/kernel；新版本仍将其计入 C 和 singleton attempt，不开始并行 main；确认终止或可靠隔离后恢复推进。 |
| P0 / §13.2 | 明确 `strict-half` 与 `borrowable-half`。严格硬保留不出借 entitlement；可借模式应声明借用可回收性、回收边界和不能满足时的结果。 | C=1、主池空闲、子池满：strict 模式拒绝越份额；borrow 模式可借，主体回来时按协议回收，不先腾出账面配额再实际过量。 |
| P0 / §13.2–13.3 | 半池按真实物理状态计量，覆盖共享块去重、recurrent state、在途 source/destination/暂存；“最终能装下”不等于迁移峰值能装下。 | 两上下文各自最终都符合份额，但换入需要旧状态和新 destination 同时存在；admission 保留峰值，不能为加速换入越界或 OOM。 |
| P0 / §6、§13.5 | 主体可有多个 scope 的顺序 context 投影。K/2 是所有 main contexts 的共同池；不能每个群/私聊 context 各自拿 K/2。 | 同一主体顺序处理私聊和两个群；请求始终串行，scope 缓存不串读，所有 main leases 合计受同一主池额度约束。 |
| P1 / §13.4、A03 | 给 C=1 公平策略最少参数：主/子最小份额或轮转权重、允许连续 main 步数、generation/prefill quantum 与 aging。只有有界 quantum 或可终止 backend 下才能声称有界等待。 | main 永远 ready，持续高优先来信，至少两个不同 lineage 的 child 待运行；在配置可行条件下每类都获得服务，cache affinity 不无限压过老化。 |
| P1 / §18.2、18.5 | 短 manifest、检索索引、指南、执行 binding 和 conformance 应绑定同一 plugin/schema/runtime digest；`verified-for-model` 同时绑定模型/量化/template/tool parser 与测试集版本。 | 插件同名升级改变参数而旧向量索引尚在；系统拒绝旧绑定并重新发现/绑定，不把旧模型成绩显示为新版已验证。 |
| P1 / §18.4–18.5 | 补“无须工具/正确拒选”、检索漏召回、已找到但不会用、合法 schema 但动作语义错误的独立指标；不能用 invocation 或 valid JSON 代替完成。 | 中英文任务不点名插件，混合正反例与缺权限样本；oracle evidence/tool/arguments 试验可分别定位知识、发现和执行失败。 |
| P1 / §9.2、A18 | 为“任意部分可自改”加组合验收，而非仅 plugin unload。验证器变更、数据格式迁移、kernel/rescue 交接必须实测，保留当前合同的独立回执。 | 候选改 kernel+history schema，随后故障；恢复到可启动闭包，原始 ID/承诺/权限/未决请求不回退，无重复对外动作，也不产生两条主体。 |
| P1 / §1、§12、§23 | 加“无 coding 插件仍是完整个人 Agent”验收；正式区分跨 session 的关系/承诺与一次任务计划。 | 卸载 coding 能力、换执行机器、切窗口、reset 研究；人格身份、联系人、有效承诺和自然消息路径持续，任务 context 可独立清理。 |

补充边界：自动验收证明的是特定版本/条件下的合同表现，不是为开放式自改、未知 backend 或无限到达负载给出数学保证；没有实际运行这些测试。以上新增场景可并入原有 A01/A03/A04/A05/A13/A18，不必无止境增加产品模块。
