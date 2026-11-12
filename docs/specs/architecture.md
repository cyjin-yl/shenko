# 自主主体与可替换能力架构

**内核定位。** 本文是总览。哪些规则属于内核、哪些是可以替换的参考插件，由[内核合同](kernel.md)规定；本文既有义务保持有效，不能由内核文档的优先级减损或改写，结构差距列在[内核合同的差异一节](kernel.md#与现行合同的差异)。


<a id="why-intro"></a>
<!-- migrated-contract: WHY:intro -->
# Why Shenko

Shenko（深刻）是面向 personal agent 的最小框架。编码、聊天、研究、远程控制和电脑操作是它可安装的能力。产品围绕一个持续存在的主体组织：它有自己的经历、关系、承诺、兴趣与可继续行动的环境。这里说明设计为什么成立；精确状态、接口与故障交错在对应领域合同中定义，可从[索引](../INDEX.md)进入。

<!-- /migrated-contract: WHY:intro -->

本文是现行架构合同；各领域的具体行为、Why、需求与验收条件由[文档索引](../INDEX.md)导航。迁移保留原有义务与原话，不以目录变化删减范围；接口名称随实现演进，不把设计名称当作已经运行的能力。

## 为什么重整

自主性不等于无限制地发送请求，也不等于替模型编排更多固定流程。真正的自主性需要主体能认识当前事实、取得工具知识、选择行动、知道行动是否发生，并修改自己采用的策略。有限模型同样应获得这套完整机制；换用更强模型时，不应先拆除一套围绕旧模型写死的分类器。

从 `78f5f63cc6b51cffb68a0619179e765afdb73913` 的源码可以追踪到单节点 S0/S1、持久 intent、有限原生 child、节点安装单元、上下文插件和若干真实协议实现。但类型、注册、接线、实际运行和模型有效性是不同证据层。源码审查不能证明当前候选已通过运行验收。尤其不能用空 provider、未调用的 trait、拒绝某项能力或仅存在的测试定义代替交付。

重整采用三个决定：保留现有事实账本和 Cordis 生命周期；打通实际运行链而不是再建平行框架；把注意、判断和改进放在可替换的模型策略与认知单元中。

## 一个自我，不是多个互不相通的代理框架

主体在一条分支上维持唯一有效决策序列。children 是有限任务执行者，可以拥有独立完整 system prompt、模型、上下文、工具和预算，也可以继续委派与双向讨论；它们不是固定的一次文本生成器。主子共用安装单元、能力知识、provider、推理 admission、任务/效果账本和原始经历。

角色限制来自 SPEC：主体使用 thought、语义记忆、自解释与委派，不直接通过 REPL 或帮助查询取得文件执行权。child 的输出预算不得机械继承主体单轮的较小上限；模型名称、量化形式、平台标签或参数字面值不构成额外许可名单。真正的能力不匹配、实际上下文容量、owner 硬资源上限、账户额度和授权事实仍须准确表达。

正常运行、语义等待、推理基础设施失效和任务等待分别保存。主体自愿等待仍使用自身模型持续思考，沿用 SPEC 的有限等待合同；不能偷偷改成睡眠、轮询定时器或另一个轻量模型。基础设施失败也不能伪装成主体主动等待。

## 事实、认知与效果分别拥有写入者

| 层 | 持有的事实 | 不替其他层决定的事 |
| --- | --- | --- |
| Store / CAS / 原始会话 | 身份、分支、任务、输入、已封口请求、结果、决定、效果、版本引用 | 不从文本猜行动，不用检索词强制业务选择 |
| Node 安装单元 | 单元版本、内容身份、依赖边、启停、实际试用与本节点 readiness | 不把复制成功、平台标签或别的节点成功当成本地可运行 |
| Cordis 运行时 | 服务发布、运行代、有效注册、取消和 teardown | 不成为另一套持久事实库，不把 startup 快照当永久目录 |
| Perception | 带来源、时间、revision、范围、置信和未知的观察 | 不把诊断假设、候选身份或终端选项文本变成权威 |
| Awareness / Skill / policy | 可查阅的直觉、方法、注意与执行策略 | 不自授权限，不暗中派发工具，不与某模型永久绑定 |
| Provider / child | 已绑定任务中的实际执行、传输和回执 | 不以本地超时证明远端取消，不盲重放未知副作用 |

每项能力都有可追踪的闭合链：单元内容 → 依赖与本地绑定 → 生命周期注册 → 当前能力目录 → 主体/child 的选择 → durable intent → 执行与回执 → 原始事实与后续认知。缺失任何一段均不能称为完整插件化。

CAS 的文件名不是内容证明。封口遇到已有对象时必须核对大小和摘要，损坏时拒绝而不是覆盖；并发发布采用不替换目标的原子操作，先同步对象、再同步分片目录到已接纳的根。打开 Store 时不可忽略 CAS 根目录创建或同步失败。大对象的核验使用固定大小的缓冲区，不因核验而把整个对象读入内存。这些机制不自动构成断电、介质损坏或恶意本地写入者下的验收证据。

运行时错误先机械脱敏，再成为原始事实：带转义的引号键值、凭据赋值、Bearer token 和结构化附加字段沿同一规则处理。消息的省略号计入 4096 字符上限；附加字段超过 4096 字节时整体替换为明确的丢弃标记。`error_digest` 保持 [§45.14](cognition-units.md#spec-45-14) 指定的未脱敏原文 blake3 判重语义，不把它误作脱敏内容摘要或保密机制。脱敏不是任意秘密检测器；调用方仍不得把无标签的秘密当诊断文字。主体读取错误后自主决定如何处理，错误写入本身不触发 issue、重试或固定修复流程。`issue.submit:1` 仍是本地草案合同，不将本地 outbox 回执冒充网络送达；对外发布走主体选择的已授权渠道。

草案的 digest 文件名也不是内容证明。新的提交或 armed 恢复观察须有界读取完整 v1 文档，核对冻结提交的 digest、标题、摘要、错误引用、来源与受众，并验证时间格式；异物或损坏保留原样，不写入成功回执。发布使用独占临时文件、文件同步、不替换目标的原子改名及目录同步；并发竞争者只接纳经核验的同一提交。已结算的历史回执不因外部文件后来变化而改写。正常执行、损坏拒绝、并发与断电持久性是不同的验证范围。

未竟义务由原始创建决定持有身份与正文，并以同一完整向量进入后续 S0 和实际模型请求。历史压缩不能消除义务；完整正文仍占用正常上下文预算。主体可用 `[obligation:terminal]` 的 `obligation_id`、`state`（`completed` 或 `cancelled`）和非空 `reason`，经 typed `ObligationTerminalChange` / `PolicyKind::ObligationTerminal` 明确结清本步 S0 中的目标。普通输出或 child 完成不自动改状态。S1 原子保存变更、原始 S0/创建/控制/policy 绑定、`ObligationTerminalReceipt` 与 head；外来、未封口、同一步新建或重复目标拒绝。终态保持单调，迟到旧请求保留拒绝结算而不覆盖原终态；恢复不能从请求或孤立执行标记补造成功。

认知归档 schema 6 要求接收方完整理解义务终态闭包；旧 schema-5 reader 拒绝新归档，缺失或冲突的必需事实在文件、数据库、基线与复制水位发布前拒绝。现代结算严格回放与 schema-3 历史创建投影分别处理。受控证据 `build/obligation-lifecycle-20261105/actual-readers-20261105-112512/receipt.json` 使用分别编译的真实新旧 Store：旧 reader 拒绝实际 schema 6，新 reader 关闭重开、relay 及第二接收方保持一个完全相同的已完成结算。旧归档到新 reader 的来源为空 Store，只证明版本接纳；该证据不证明有内容的旧归档恢复、物理断电、分布式恢复或模型效能。完整合同见 [50.4 未竟义务的显式结清与复制闭包](durable-state.md#spec-50-4)。

## 安装事实只有一个权威

继续使用 `shenko-node` 的安装单元存储，覆盖 perception、awareness、Skill 和 code。不得为了认知单元新建另一套相互漂移的安装 registry。运行时目录是这些安装事实与当前服务发布的投影，不是第二个数据库。

同一单元的版本、artifact digest、依赖选择、配置引用、来源、适用条件与当前启停状态可追查。依赖可精确绑定，也可按实际提供的能力/主题解析；required 与 recommended 不混为一谈。绑定必须解释失败和未知，不因抽象标签不匹配就禁止调查与真实试用。凭据、provider grant 和节点本地路径不会随认知包自动复制。

纯文本单元不启动 daemon。需要执行的观察器或 provider 使用 Cordis 拥有的资源与 shutdown barrier。安全边界切换目录 revision；在途请求保留原图、旧代码和恢复所需对象，后续请求看到新图。启停、升级和卸载不能留下孤立任务、定时器、服务或失去解释依据的回执。

## 上游协作与实现选择不阻塞产品

[48.6 上游协作不得阻塞产品开发](architecture.md#spec-48-6) 记录用户于 2026-11-01 明确的选择：不等待 cordis-rs 上游回应或合并才继续开发。必要的原生能力可以由精确固定、可追踪的 fork 补丁提供；上游跟进与产品开发独立推进。

对外贡献以通用生命周期问题、可复现证据和可独立验证的机制为中心，不要求上游接受 Shenko 的业务架构。可以整理为更合适的 issue/PR，并在依赖与证据接续明确时替代原 PR；不隐去消费端来源，不为拆分而破坏必须共同成立的原子保证。

用户也明确允许必要时直接适配 [TypeScript 原上游](https://github.com/cordiverse/cordis)。这是一条允许评估和实现的后备路线，不是已完成迁移的声明，也不是现在必须换底层的命令。Rust 补丁、Rust 上游和原 TypeScript 实现按实际能力与成本选择；原生生命周期语义、typed service/event、代际清理及持久外部效果的独立恢复合同不随实现选择消失。不得把上游合并状态转成产品审批或模型能力门禁。

## 渐进披露，而不是关键词触发或全部灌入

主体可见当前能力/认知单元的简明卡片与状态，按需要读取精确版本的正文、schema、Why、限制和来源。模型、已安装的选择策略或执行 child 决定相关性；核心不按关键词决定是否允许知道某条直觉。卡片数量和正文长度是表示与资源问题，不是把知识排除在主体之外的许可边界。

2026-11-05 起，认知的现行概念以 [51 认知元概念：内容、态度与理由](cognition-units.md#spec-51) 为准，不再把 awareness 只解释成模型能够自行遵守的一段直觉文本。awareness 是一种能力：现在由可安装的宿主或 watcher 主动 notice，用文档模拟模型尚未内化的注意；文档本身不是能力。perception 已确定为教主体如何观察的无状态单元；某次观察事件仍是原始经历，其结果是理由停在该观察上的 fact。用他人的 perception 解释同一段经验，就是换位思考。现有代码里的感知材料仍只是报告带来源、时间、revision、范围、置信与未知的观察，尚未区分「这则 perception 属于或模拟哪个主体」。该字段留给实现 Agent，不再是用户待决。时间感知、世界感知、预测、awareness 宿主、perception、换位思考和维护习惯都是可安装插件，不进内核。发行由名为 `humanlike-mind` 的宏包等预装其中一些，可以替换或扩展。宏包是一起安装的一组单元，可同时包含实现插件与 awareness、perception、fact、conclusion 等内容。`humanlike-mind` 依赖「能遗忘」这一能力，不钉死名为 `forgetting` 的包。契约前缀是 `cap.`，这条边是 `recommended`。多个提供者在所用原子能力不冲突时可以并存，不在包级声明互斥。见 [51.8 提议：按能力依赖，不钉死具体包](cognition-units.md#spec-51-8)。`taste`、`humanlike-mind` 与 `forgetting` 是并列预装宏包，互不包含，名字不带 default。账本可以记录经历的写入时间，那不是时间感知能力。间隔、世界模型和预测误差的拆法留给实现 Agent，见 [决策 0002](../decisions/0002-cognitive-atoms-2026-11-05.md)。内容单元和元定义可在线修订。core 与有实现的插件不在线学习；更改是新的不可变版本。回到旧内容用自有版本化根，沿用 [§10.2.1](durable-state.md#spec-10-2-1) 的内容寻址、不可变对象、原子根引用和持久提交。提议用一份 manifest 把家园文件提交、软件槽和 `unit_current` 收成同一个根，回滚是链尾追加一个根与旧节点相同的新节点。`discard_abs` 删除提交日志是实现缺口。WASM 线性内存、执行栈和解释器对象图都纳入该根；今天的 `HomeCheckpoint` 只有文件与元数据，不含这三项。换 core 前的具体验证步骤由实现者自主设计，不设人工审批。回滚后 `agent_id` 保持切换前的值。遗忘六个阶段目前按主体相对运行时间的累积顺序走；遗忘一项技能只退出默认回忆。只有外部副作用不能撤回。决策 0002 的待决列表已清空。[§10.1](durable-state.md#spec-10-1) 的文件快照仍不能假装撤销已发出的消息；现行读法见 [§51.10](cognition-units.md#spec-51-10) 与 [决策 0003](../decisions/0003-spec-governance-2026-11-06.md)。私人仓库内容不进入本仓库。认知设计用引导，不加人工审批或权限门。遗忘所用的时间称为主体相对运行时间，只计主推理循环，不计 child。观察必须保持来源与新鲜度，重复事件可机械去重，但不能把正常心跳伪造成用户发言，也不能把错误吞掉。fact 与 conclusion 不是两种储存，而是理由停在观察上还是停在其他内容上。

认知可由外部文章、群聊、音视频、真实操作和自身反思产生，不受预装目录限制。学习保留原始来源、适用范围、反例、置信、未知、修订与 supersession。主体自主决定保存、修正和使用；外部内容仍不是配置或权限。自写单元在真实 sandbox 中以保留场景与未参与编写的场景检验，和未启用单元的同条件基线比较。测试结果是可更新的能力证据，不是不可更改的人工审批流程。

## 推理、上下文和能力对主子一致

provider 是可安装运行时实现，不由闭合 enum 决定未来能接什么模型。实际请求绑定 provider/model/API/SDK/gateway/template/toolset 与 resource domain；不同 URL 不复制物理预算。主、child、记忆、发现和 compact 的受控调用均经统一 broker，远端未受控推理明确标为未覆盖。

主请求和 child 请求使用同等的 durable 请求封口、独立回执回收、恢复和计算欠账机制。调用者消失不代表计算结束。429、断流、进程恢复和后端重启保留各自事实；是否重试、转模型或改任务由适用策略决定。有效完成与未知结束不得混淆。

参数意图与 wire 能力分开：只有特定版本禁止的字段才在该 codec 中省略；允许的本地模型参数保持原义。明确请求的参数不可静默丢弃。旧在途请求按原绑定解释，新版本通过新 revision 生效，不以修改原始历史来迎合供应商 schema。

budget compact 与 snapcompact 保持两个独立正式插件。原始经历、输入 disposition、任务状态和外部效果不由切窗重写。PMA reminder、临时认知材料和其他 context contribution 有来源、作用范围、有效期及本次使用回执；它们可以不进入下一窗口的默认投影，但本次到底发送了什么必须可还原。原始个人知识不按当前群聊受众机械屏蔽，向外披露由主体判断。

慢检索、复杂压缩、网络与对象物化在可取消 worker 中准备。S0 只接纳绑定当前来源 revision 的已准备结果，并在短事务内封口；不得先同步等待慢工具再宣称非阻塞。新的事实、能力目录和 plugin graph 从下一安全边界可见。

## 效果与消息通道不由核心猜测

通道插件同时拥有入站分类、封套投影和出站描述/执行。目标标识为不透明字符串，provider/account/tenant/conversation 的命名空间显式保存；QQ 号码、官方 OpenID、飞书 chat_id 或 iLink context token 不强行塞进整数。

Store 只保存通用 publication/task/operation 事实。平台 action、参数、认证、有效回复窗口、能力与接受谓词属于实际插件。普通来信默认开放；管理员是人类明确设置的独立事实，不从群主、token、声纹或认识的人推导。

收到、排队、理解、延期、忽略、委派和发送分别记录。消息 gap、未知发送和附件位置可由主体读取。任务完成不等于通知已送达，provider 接受不等于人类已读；回执未知时先保留未知并允许查询/新决定，不自动重发。channel 或可选插件失效不能连带关闭主体、记忆或其他通道。

工具的 EffectContract 描述真实 reset、replay、cancel、reconcile 与 compensation 边界，并绑定具体实现版本。只读操作、可查幂等操作和不可判定外部写入不能共用一个假定成功的恢复办法。

## 记忆与研究不是同一种状态

记忆 intent 的操作种类与知识的开放 kind 分开。save、correct、forget、recall 进入同一可恢复提交链，带来源、scope、base revision 和幂等身份。纠正留下关系与旧证据；语义遗忘保留单调 tombstone；真实 erase 和 archive_move 各自遵守 SPEC 的授权、闭包及回读合同。检索结果保持 source、时间、范围、supersession、删除和可信状态，并能读取实际命中位置。

Git 记忆由有限 worker 维护可解释的文本和对象引用，SQLite 原始日志不每轮塞进 Git。可替换 backend 统一 retain/recall/reflect/update/forget/flush/health；存储机制不决定什么人生经历值得记住。

research reset 重建的是某个研究 worker 的可见 session/候选工作区，保留可复现的最佳 artifact、评分、约束、资源和效果事实，不重置主体身份、不清除失败证据，也不重新投影所有失败叙事。PMA 提供带出处的 silent/reminder 决定，而不是接管主体的第二条无限循环。

## 节点、执行与自升级保持真实边界

统一 Node 保存同一自我与各节点不同的实际能力。bound subject、当前 Primary 角色、近期模型可达性和仍然活着是不同事实。成员配置、quorum、选主和 fencing 由真实 ElectionProvider 实现，不以可见节点数量或单个文件锁冒充分布式一致性。分区经历、额度、未知效果和 tombstone 不因选主或版本回退清零。

WASM 家园必须真实实现 COW、对象/branch 提交和恢复。目录准入绑定实际打开的 handle；校验 pathname 后再重新打开，不构成防 ancestor-swap 的证明。命名 WASM 工具、Python/Node/Bun 等 kernel 的目标不因提供 native 或 SSH 替代路线而完成。大 artifact 使用有界流式对象路径，不将全部包无限放大成内存 JSON。

现有 coding seat、管理的 RPC/SDK seat 和终端观察是不同 adapter。候选问题可来自 terminal_inference，但保留置信、未知和原始证据；回答绑定 question revision、seat incarnation、task revision 和 writer lease，并在实际输入前核对当前现场。保留真实高亮/选择状态，不从选项顺序猜答案。

候选构建、迁移和试用不覆盖唯一运行副本。运行中交接包含真实 quiesce、catch-up、selector CAS、epoch 和 writer/route handoff；单独的原子文件发布不等于整套交接。恢复可不依赖 LLM，保留已验证旧闭包、原始经历与离线导出入口。Linux、Windows、macOS、Android、浏览器与桌面执行按各自能力/授权提供实现，不以 typed-unavailable 充当交付。

## 产品表面显示实际发生的事

默认 WebUI 是主体的一条时间线与输入框。append、steer、withdraw、任务对话和按需详情进入同一持久运行时；没有另一个 UI 聊天人格。流式内容从真实 provider stream 开始，有序保存后发布，重连按 durable 游标补齐；把完整 S1 结果拆成几个块不叫真实流式。

公开内容、内部控制语法和未提交动作分开。尚未完成的输出可作为明确的临时展示；实际 action 仍由完整解析与 S1/T1 合同接纳。不能把原始内部思维全部暴露到界面，也不能让不完整 DSL 成为可执行按钮。阅读、选区、焦点、草稿和布局在持续输出时保持稳定。

配置与 Playground 使用相同身份、服务、任务和效果协议。真实业务状态、构建/运行 artifact 根、外部效果与展示状态分开保存；恢复闭包必须能枚举它们。owner claim 与实际 actor 存在，既不是任意 HTTP 请求都能冒充 owner，也不是给日常自主操作新添人工审批。

## 从同行吸收什么

以下只引用公开说明，不冒称审计过闭源运行时或已测得 Shenko 的效果：

- [DeepSeek SuperAgent 研究原型](https://github.com/deepseek-ai/dsh-superagent-prototype/tree/6337fc823ce20251c8de8085948165c81aaa701e)：小型端口、明确状态写入者、事实/命令区分和按需卡片知识。2026-11-01 查询的 main 为 `6337fc823ce20251c8de8085948165c81aaa701e`；原型不等同于正式产品或内部系统，不复制其中的业务 allowlist 与固定工作步骤。
- [Claude Duet](https://claude.com/resources/articles/claude-duet)：持续对话、后台执行、共享记忆/Skill/app 和注意力摘要。其固定模型、定期检查和审批策略不是 Shenko 的合同。
- [Muse 官方介绍](https://about.fb.com/news/2026/09/introducing-muse-personal-ai-agent/)与[产品说明](https://introducing.muse.ai/)：执行途中继续交谈、对话之外的 artifact、可修改的记忆/人格、按需查看活动。其安全/审批产品策略不自动移植。
- [OpenAI dots](https://learn.chatgpt.com/docs/dots)及[任务与记忆](https://learn.chatgpt.com/docs/dots/tasks-and-memory)：持续责任、多个后台任务、同一主体跨渠道接续、主动研究与可见活动。其通道对话、持久记忆和任务上下文的区分有价值；其模型绑定、共享审批和只读研究限制不是 Shenko 的限制。

共同启发是长期主体应当拥有连续性、可见的后台工作和可修改的知识，而不是把一个会话机器人包成调度器。Shenko 进一步要求自我重写、可替换实现、跨节点真实经历和完整恢复合同；这些目标必须由实际运行与模型行为证明。

## 规范、证据和文档入口

正式功能合同在 `docs/specs/`；安装/使用/扩展说明与真实运行证据分别维护。README 是产品入口，不是任务板。旧 GOAL、PROMPT、HANDOFF 和历史任务卡保留在显式历史归档，迁移当前引用，不保留兼容入口或符号链接。`CLAUDE.md → AGENTS.md` 是已有的单一开发规则入口，保持不变。

完成状态逐项区分 specification、source、build、controlled runtime、real provider/model、故障恢复与长周期观察。原有要求不因架构调整、缺少机器、提供可选 adapter 或交付一次 PR 而消失。未达到的验收保留原义并继续完成。

<a id="spec-0"></a>
<!-- migrated-contract: SPEC:0 -->
# Shenko 产品 SPEC v0.6.2

<!-- /migrated-contract: SPEC:0 -->

<a id="spec-1"></a>
<!-- migrated-contract: SPEC:1 -->
# 1 文档状态与产品定义

版本：v0.6.2，产品规格、规范执行合同与完整开发交接。正式产品名 Shenko（深刻）；保留全部原始要求和后续裁决，追踪 147 项需求、55 组行为与 59 组来源记录（含一项历史拼写别名，不等于独立上游数），共 216 个验收定义。本版追加 U54–U63、R143–R147、B51–B55，扩展既有章节：主体持续接续及最多 600 秒的自主语义等待；从任何输入产生 awareness，并将有用经验自主沉淀为原子单元；时间、未竟目标、重复感、项目、社交和邮件的独立感知/察觉组合；多设备同步同一自我、真实已授权能力与环境自修复；每连通域单一主节点、异构模型/隐私感知与无模型提取恢复；必读 shenko-dev 与兼容 OpenAI/Anthropic 的原子开发 Skill；Rust 2024。dreaming 仅是社区可选插件方向。[§46](../governance/COMPLETION.md#spec-46)–[§47](inference.md#spec-47) 的完整自主开发交接及 Provider 版本合同继续有效。S1–S5 均在完整目标内；首版不是终点，PR 等待 review 不暂停工作，真实账号、资源和验证边界保持准确。历史规格与原话完整保留；新增 7 个验收定义是产品义务，并非每个增量要重写整套测试。原始研究截点：2026-10-22 UTC。编制期仓库只含规格与交接资料，当时尚未开始产品实现、部署或目标机器测试；该历史边界不代表当前工程状态。

2026-10-27 设计澄清：追加协作席记录的 U63、[§5.6](subject-runtime.md#spec-5-6) 与 [§45.13](cognition-units.md#spec-45-13)，保留原研究截点，不声称全部上游研究重新核验。U63 的原始会话尚未由集成席独立回读，来源边界见该节。当前仓库已有实现并持续开发；本次变更仅写入非阻塞 thought、事实投影及插件化时间/环境感知合同，不新增产品实现或运行证据。具体已完成切片和剩余义务以任务卡、真实源码与绑定版本的证据为准。

正式命名：**Shenko（深刻）**。名称来源按 U39 记录为 Always Online 的“More and more 尽是深刻”，并保留用户提供的 `shen1-kʻo4`、`ㄕㄣ ㄎㄜˋ`。品牌展示使用 Shenko，不附 slogan 或解释性副标题；产品由布局、设计和交互表达。框架／产品名与每个 Agent 的姓名分开，实例初始化时由用户命名或让 Agent 自己命名。Sonzai 等此前候选仅留作命名历史，不再是当前待定项。

**定位：Pi 是面向 coding agent 的最小框架；本产品是面向 personal agent 的最小框架。** 编码是它能安装的一类能力。产品组织的中心是一个人的 Agent 的持续身份、关系、承诺、记忆、主动注意力和跨环境行动，而不是某个代码仓库或一次编码任务。

**产品定义：一个由用户拥有、在本地持续生活和工作的 Agent。正常连通运行只有一个当选主体，每个主体分支只有一条永续串行的思考循环；故障分叉与重连收敛按 [§5.4](nodes.md#spec-5-4)。主体主动决定下一步，将实际行动、研究、版本操作和记忆维护交给子 Agent；通过可恢复的 WASM 家园、原始会话证据和统一推理调度，在个人与群聊中保持连续人格。**

框架应把“个人连续状态”与“任务临时状态”分开：关系、承诺和长期目标跨 session 存续；编码、检索、课堂旁听、远程操作都是有边界的任务插件。安装编码插件不改变产品身份，卸载编码插件后仍应是完整可用的 personal agent。主体 harness 自身也可替换，不把个人 Agent 的行为体系全部硬编码进最小 kernel。

“持续思考”“持久执行环境”“子 Agent”“本地隐私”“小内核”是同时成立的目标。主体持续推理会消耗模型算力，小内核只说明控制软件的负担；本规格不承诺无限思考、零推理成本、手机永不被系统挂起可以同时实现。

标记规则：**已确认**来自用户原始需求及追加裁决；**研究事实**有相应一手来源与版本；**首版默认／建议默认**是开发 Agent 可以自主采用、验证和替换的工程方案，不表示必须等待人类逐项批准；**规范合同**给出必须保持的可观察行为与失败语义；**技术验收**需由实现证据证明，不等于已实现，也不自动产生人类 gate。当前只做 SPEC 是本次任务范围，后续开发任务的自主推进规则见 [§40](../governance/development-contract.md#spec-40)。

首版开发者 profile 明确允许宿主原生 Execution／Build／Seat 插件，连接已有 NapCat、tmux 与 coding agent，同时保留真正可恢复的 WASM 家园与默认宿主挂载 Git 记忆；不等待所有 CLI 都移植 WASM 才完成首版闭环。所有权模型与最低控制硬件仍有待定部分，本文暂以“单主人、多受众、主体统一知识；64 位、至少 512 MB 控制节点；模型可在另一自有节点”展开，最低硬件属于待测建议。D1 已取消按受众屏蔽主体记忆的默认方案；D12 首个模型基线为用户 Qwen3.8-27B 四流 128K，经 Tailnet 调用。真实 SSH WASM、Android 本地运行和各设备 profile 继续是正式目标，未实测兼容性不冒称已成立。

<!-- /migrated-contract: SPEC:1 -->

<a id="spec-2"></a>
<!-- migrated-contract: SPEC:2 -->
# 2 已确认的产品约束

1. 面向个人聊天和群聊，借鉴 AstrBot 与 OpenClaw 的渠道广度。默认任何平台可达用户均可对话；管理员必须由人类明确登记。

2. Rust 2024（edition 2024）编写核心控制功能，提供 Webserver 与正式 Web UI；内核小、可恢复、适合长时间运行和低资源设备。实际工具链和依赖版本由构建锁记录，edition 不冒充固定编译器版本。

3. Agent 有自己的持久 WASM 执行环境，目标是实用 Linux/POSIX 兼容工具环境，可映射任意宿主进程有权访问的路径，真实运行和安装工具。

4. Nix 或 Nix-like 包管理；Agent 可以自主装包、试验、升级和回滚。toybox、gcc-wasm、Git、认证 GitHub/Gitea CLI 和 SSH WASM 保留为明确兼容目标。

5. 出厂记忆是挂载到初始 WASM 沙箱的宿主目录，其中初始化 Git 仓库。私有远端可选，默认没有远端也能完整工作。

6. 软件和工作区有二进制友好的版本/分支/恢复机制；沙箱损坏时宿主自动恢复，最终能回到初始可启动版本。按最新决定，在合格真实 Linux VM 中直接使用原版 LayerFS；WASM 状态独立实现，不再以重做或移植 LayerFS 为默认前置路线。

7. Agent 按需创建自己的 SSH 身份；UI 展示公钥。支持人类机器、后续完整容器和多个自有节点。

8. 原始 session 是真相源，可在预算切窗或压缩后检索和重读；会话存储采用 SQLite FTS5 类设计，保留外部 memory backend 接口。

9. 本地 AI 与极致隐私优先。推理并发、上下文、优先级和共享物理资源必须统一管理；支持真实 backend KV slot/affinity 能力透传。

10. 子 Agent 支持低开销递归探索、多方案分支、深度研究和适时收敛；不能产生父子任务互相等待的资源死锁。

11. UI 使用 Proto-UI 当前主线与 shadcn；每轮开发核验并 bump 最新主线，锁定确切版本；留出 Proto-UI compiler 迁移接口。布局承担状态表达，不做普通后台表格的简单拼装。

12. 接入 iLinkBot 路线、QQ 官方机器人、NapCat、已有 napcat-cli、飞书等；提供 Ninfer 与 Strata 的中英文教程。

13. **追加约束：每个连通域只有一个当前主体，完整连通后只有一个当选主体；每个分支只有一条 proactive thought loop，分叉与重连收敛按 [§5.4](nodes.md#spec-5-4)。默认持续思考，主体自愿语义等待仍由同一主模型轮询，最长 600 秒，见 [§5.1](subject-runtime.md#spec-5-1)。主体没有直接文件操作、shell、SSH、装包或版本操作工具；保存/读取记忆走专用委派接口。版本、任务进度与实际记忆机制在子 Agent 侧。**

14. **追加约束：人格追加在稳定核心 system prompt 之后；人格不能改变工具调用合同、权限、恢复或资源上限。**

15. **追加约束：外部消息几乎总是直接注入主体思考序列，附来源渠道/provider、发送者别名与稳定身份、内容、富文本/附件的受控获取引用。消息源和模型 API provider 均插件化，联系人编号和可选声纹识别可以自然演进或由插件提供。**

16. **追加约束及最新澄清：C=1 单并发轮转必须能够保留多份上下文。KV 分配通过可替换调度器/策略插件自动完成，允许主体表达偏好；50/50 是理想条件下算法应能自行得到的分配结果，不是要求用户手设比例，也不要求所有负载始终各半。仅手动设成 50/50 能运行不算满足。容量可以自动发现，未知能力不能伪报。**

17. **追加研究：纳入主动遗忘重启研究子 Agent，以及主动记忆提醒两条机制；研究结论与产品推论分开，不因上下文 reset 删除原始证据。**

18. **追加约束：可安装远程 Agent 操作插件，定位并控制 SSH/其他 runtime 上的 Claude Code、omp、tmux/agent seat；自主处理选项与已授权 permission，不确定时按人格与情境自主寻找可联系用户的渠道。**

19. **追加约束：支持传统 Skill 和 MCP 市场，常用能力推荐原生插件；插件不仅可安装，还必须可发现、学会用、在适当情境使用，并面向未来模型保留更简洁替代路径。**

20. **追加目标：可恢复 Python/Bun/Node WASM REPL 与动态依赖安装作为插件，三个具名目标分别注明真实 runtime 与兼容程度。**

21. **追加约束：原生预留实时通话、旁听课堂、录音、声纹等插件接口；媒体信息进入同一主体循环，保留完整工具委派能力，允许通过子 Agent 自主发展这些能力。**

22. **再次确认：核心必须非常小，一切业务能力均插件化；不能把可选媒体、REPL、模型、渠道或大型工具变成必选常驻依赖。**

23. **追加约束：Agent 可通过子 Agent 重写、替换自己的任何软件部分，包括主体 harness、提示词与人格、调度器、插件系统、记忆、UI、Rust kernel 和救援程序。采用候选版本、迁移、验证、交接与恢复，不设置永久不可改的代码区域，也不默认要求逐次人工批准。当前用户行为合同仍须满足；允许改代码不自动授予新的管理员身份或外部权限。**

24. **追加约束：Computer Use 是正式插件路线。WASM 家园之外可连接被授予的 root Android、Linux、Windows、macOS 机器、容器、VM、远程桌面和 agent seat，使用相应屏幕、shell、浏览器、键鼠、应用与文件能力。实际操作按已授权设备能力执行；不能把可操作环境限制为 WASM，也不能把所有 OS 桌面等同于 Linux 容器。**

25. **定位再次确认：这是 personal agent 的最小框架，Pi 的 coding-agent 最小框架思想是参考；必须逐项研究和保留全部需求，不把个人关系、主动生活、音频或远程行动裁掉为编码附属功能。**

26. **追加约束：动态生成式 UI 是 Agent 的正式能力。参考用户称为 OpenAI Dynamic Response 的方向，按目标界面支持的 DSL、元素、语法或 UI 工具自主生成响应，并可跨 Web、原生卡片、文本、媒体、语音等界面降级；不能只做一个固定聊天皮肤。**

27. **追加约束：是否启用自动更新、何时及更新哪些部分、联网是否走代理等配置问题，都可作为消息进入唯一主体。Agent 自主决定并委派子 Agent 执行，也可自主调整更新与联网策略；不把这些决定默认转交用户审批。**

28. **主体统一个人知识、可自改灵魂；保密与关系由其自主判断，不硬编码亲密度。每次委派可自由指定 child 的完整 system prompt 和不同模型。**

29. **主子共享能力全集并可双向讨论；Agent 能发现自身工具不足，自主安装、编写插件/外部应用或移植 WASM 能力。**

30. **外部消息通常最高紧急可打断 child；停滞时由主体决定临时免打扰，结束后处理队列；垃圾过滤由主体决定并能自主发现误屏蔽。**

31. **自更新优先磁盘成本，允许 breaking change；保留运行旧副本，迁移/烟测新版本后最后交接自身，能力插件化。**

32. **原始文字、录音和屏幕帧默认永久；空间不足由主体决定迁往用户自有存储或遗忘，能力插件化。**

33. **自主兴趣可长期持续投入，不要求与用户当前任务有关，也不设统一的兴趣寿命。**

34. **多节点同步同一自我并在线优先：每连通域单主，主节点按真实异构模型能力、可达性与隐私安排 child 和递归接管；可 access LLM 的孤岛可独立演进，重连选主并合并经历任务。新设备身份加入与旧分支汇合分别处理；无 LLM 时日志、授权记忆提取与离线恢复仍可工作。**

35. **首个真实基线是用户自有 Qwen3.8-27B，经 Tailnet，四流 128K；133120 是部署冗余。**

36. **UI 给 Claude Opus 完整功能/体验 brief，保留布局与视觉自由，诊断按需展开；继续 Proto-UI 最新主线、shadcn 与 compiler seam。**

37. **budget compact 与 snapcompact 均为正式交付的独立插件，各自可安装、更新、卸载和替换，不写入核心；OMP notes-backed windows 同时作为历史访问参考。后端视觉能力决定 snapcompact 的实际启用，不取消插件交付要求；原始历史、执行进度和 KV 保证不与派生上下文混同。**

38. **框架／产品名称为 Shenko（深刻），与实例姓名分离；初始化可由用户命名、稍后命名或让 Agent 自主取名，姓名不影响稳定身份连续性。**

39. **品牌与首页不使用 slogan 或解释性副标题；以布局、交互和设计表达产品，操作信息仍须清楚。**

40. **首版开发者发行就完成 NapCat／QQ → 现有 tmux coding agent → 自身候选开发／验证 → 自主迁移激活的闭环；不能把自开发与运行接续推迟到后期。**

41. **插件按操作声明副作用、实际资源、checkpoint 覆盖、重试／取消／补偿与恢复边界；沙箱回退不能抹去已发生的外部事实。**

42. **cordis-rs（dshbox/cordis-rs）为用户指定的插件主干；U45 已纠正 U35 的拼写。借鉴 Fabric 的 API 注入时序与兼容思想，明确 hook 的版本、phase、相对顺序与热替换，不另换主干。**

43. **默认内置但可替换的自解释插件，让主体与 child 按需查询自身代码、说明书、设计哲学和当前运行事实，能够回答自身问题并指导自开发。**

44. **每项有意义的设计均连接用户来源或固定上游 Ref，并记录采用范围、差异、行为验收与实现证据；不是只列致谢。**

45. **开发 Agent 在已授权目标内自主研究、实现、验证、修复和集成，不默认留下逐 commit、逐 PR、逐阶段或逐升级的人类 gate。**

46. **视觉风格和美观以具体 Magpie 画面与交互为参照，保持生活化的个人主体体验，由后续设计 Agent 自主完成。**

47. **正式框架／产品名为 Shenko（深刻），不加 slogan；实例名称由用户或 Agent 自己选择，身份不因改名而变化。**

48. **核心行为必须写成可实施合同：具体记录、状态、版本、原子提交、取消、失败恢复与可判定用例；不能靠更多“应当支持”代替。**

49. **供应商周期额度由后期插件观测和预测，可自主调整模型与主体平均准入次数；本地优先，不把规划目标冒称精确余额或必然刚好用完。**

50. **流式界面参考 Magpie 与 Alma，保持阅读锚点、选区和交互状态，用可回放指标验收。**

51. **默认 WebUI 只有主体时间线和输入；append/steer 进入同一 loop，任务、设置、Playground 等按需展开。**

52. **所有设置可以经对话修改，面板读取同一实际配置；配置执行、观测与恢复能力可以组合使用，不把候选、验证、应用、观察、确认写成通用硬流程，不设置永久不可改配置或常规人工 gate。**

53. **Playground 长期保存并运行有界面、有后端、有数据的作品，其业务变化经持久事件进入主体同一 inbox。**

54. **首次打开只补实际缺失的推理连接与明确管理员初始化；发行自带无需 LLM 的可恢复 seed，名字、人格和渠道可后续发展，首个主体只创建一次。**

55. **thought loop 可从任意外部输入即时汲取 awareness；有复用价值的直觉再各自沉淀为可安装、查阅、修改和移除的单元。时间、未竟目标、重复/无聊、项目、社交与邮件察觉可独立组合，dreaming 仅社区可选；不设总意识插件或固定流程，来源与实际权限不因沉淀升格。**

56. **感知、awareness、Skill 和插件代码均可细粒度安装与查阅；provider 自动解析安装所需依赖，代码依赖带版本，直觉依赖允许笼统或精确并保存实际解析结果。**

57. **Why 是能力说明与开发规范的一部分：交代目标、关键理由、适用前提和可观察效果；让主体与孩子在工具或条件变化时自主调整方法，不增加逐操作说明或人工 gate。**

<!-- /migrated-contract: SPEC:2 -->

<a id="spec-41"></a>
<!-- migrated-contract: SPEC:41 -->
# 41 首版规范执行合同

<!-- /migrated-contract: SPEC:41 -->

<a id="spec-48"></a>
<!-- migrated-contract: SPEC:48 -->
# 48 自主主体、运行时能力闭环与正式文档分层

2026-11-01 用户要求在完整阅读本 SPEC 后重新审视已完成与未完成的所有架构，加强自主性、可扩展性、插件化、awareness 与 perception，并完成全部既有要求。另要求撤下 GOAL 等任务交接入口，把真实功能要求整理为正式功能规范，README 回归产品介绍、安装、使用、扩展与导航。本章只追加实现边界与文档解释；[§1](architecture.md#spec-1)–[§47](inference.md#spec-47)、所有既有 R/B/Ref/验收条目及其原文保持不变，也不将新设计记作已经实现。

<!-- /migrated-contract: SPEC:48 -->

<a id="spec-48-1"></a>
<!-- migrated-contract: SPEC:48.1 -->
## 48.1 自主性与真实约束分开

主体与 child 共用可发现的能力、版本、provider 和恢复协议，按角色执行不同任务。不得因现有模型较小而永久限制模型名称、child 的工具使用、知识种类、认知来源或合理的独立输出预算，也不得用关键词表替主体判断问题与动作。真实 ABI、上下文容量、资源域、owner 硬上限、账户/设备授权和外部效果仍按既有合同保留；不把这些事实改写成日常规划、更新或开发需要人类审批的流程。

<!-- /migrated-contract: SPEC:48.1 -->

<a id="spec-48-2"></a>
<!-- migrated-contract: SPEC:48.2 -->
## 48.2 单元、服务与运行事实闭合

perception、awareness、Skill 和 code 延用同一安装单元权威。版本、artifact、依赖、启停、试用和本地 readiness 是持久事实，Cordis 服务和能力目录是当前运行代的投影；不再建立平行认知 registry。纯文本单元不需要 daemon，执行单元由生命周期拥有实际资源。能力从安装、装配、发现、选择、持久委派、实际执行、回执到主体可读事实形成闭合链，只有类型或注册不构成实现。

目录变化在安全边界进入后续请求，在途请求保留原来代码/图/profile 的解释和恢复闭包。主请求与 child 请求采用一致的 durable 请求与独立结果回收边界；调用者退出、本地超时、连接断开不自动证明远端计算或副作用结束。

<!-- /migrated-contract: SPEC:48.2 -->

<a id="spec-48-3"></a>
<!-- migrated-contract: SPEC:48.3 -->
## 48.3 感知、渐进披露与学习

perception 产生带来源、时间、revision、作用范围、置信与未知的观察；awareness 提供可修改的直觉，Skill 提供可查阅的方法。能力卡片与精确版本正文支持按需读取，不要求每轮灌入所有正文，也不由核心词表决定主体能否想到某项知识。

来自原始经历、外部内容和自身反思的学习保留出处、适用范围、反例、修订与 supersession。外部内容不会自动成为权限。自写单元沿用 [§45](cognition-units.md#spec-45) 的真实 sandbox、保留场景与对照行为验收，未验证事实不伪装成已接受能力；测试与策略本身仍可自主改进，不新增人工门禁。暂时 context contribution 有本次使用依据，而原始经历、任务状态、个人知识与外部效果不会随投影或研究 reset 清零。

2026-11-05 起，把 awareness 读成「模型读过文档就会遵守的直觉」被 [§51](cognition-units.md#spec-51) 取代。awareness 是能力；文档与外部 notice 是当前 harness。fact 与 conclusion 是同一种内容上的态度，不是两种储存。

<!-- /migrated-contract: SPEC:48.3 -->

<a id="spec-48-4"></a>
<!-- migrated-contract: SPEC:48.4 -->
## 48.4 协议所有权与产品表面

消息插件同时拥有入站分类、来源/账户命名空间、出站 operation 描述、执行与回执。核心不硬编码某个渠道的 target 类型或发送合同；字符串平台 ID 保持不透明。gap、未知发送、附件位置和插件失败可被主体读取，不把心跳/供应商元事件伪造成用户发言。任务完成、provider 接受和用户已读保持区别。

Web 时间线的持续内容来自真实 provider stream，经有序持久记录后发布；事后拆分完整结果不是流式。公开内容、控制语法和未提交动作分开，动作仍按完整解析与 S1/T1 接纳。配置、Playground、seat、媒体与设备插件复用同一任务/效果和实际 actor 机制，不另起一个 UI 主体，也不把状态按钮当作效果证据。

<!-- /migrated-contract: SPEC:48.4 -->

<a id="spec-48-5"></a>
<!-- migrated-contract: SPEC:48.5 -->
## 48.5 正式规范与历史入口

当前功能边界由 [架构规范](architecture.md) 与 `docs/specs/` 下的领域规范展开；README 提供产品、安装使用、扩展和文档导航。旧任务入口从当前根目录移出并保留原件：`GOAL.md`、`PROMPT.md`、`HANDOFF.md` 对应 `docs/history/entrypoints-20261101/` 下的同名文件；其余历史交接和任务卡由文档索引指向各自归档。原文中指向旧 GOAL 等路径的链接属于旧文档布局，应按本段解释，不为此修改 [§1](architecture.md#spec-1)–[§47](inference.md#spec-47)，也不保留根目录兼容入口或新增符号链接。既有 `CLAUDE.md → AGENTS.md` 单一开发规则入口保持不变。

文档分层不缩小工作范围。源码存在、构建、受控运行、真实 provider/model、故障恢复与长期观察分别有证据；typed-unavailable、一次 PR、旧候选的回执、仅模拟成功或尚缺真实环境都不替代既有验收。具名 WASM、REPL、OS/Android、选举、在线迁移和所有长期观察目标继续按原合同实现。

<!-- /migrated-contract: SPEC:48.5 -->

<a id="spec-48-6"></a>
<!-- migrated-contract: SPEC:48.6 -->
## 48.6 上游协作不得阻塞产品开发

2026-11-01 用户明确：cordis-rs 上游是否回应、接受需求或合并 PR，不得成为 Shenko 开发与交付的前置条件。已有补丁可以在可追踪、精确固定的 fork 中继续维护；产品实现、验证和交付与上游协作分别推进，不用等待上游决定代替可做的开发。

面向上游的贡献按通用、可复用的问题与机制组织为功能改进或上游跟进的 issue/PR，而不是要求上游为 Shenko 定制产品架构。拆分依据是各项改动能否独立说明、实现和验证；必须共同成立的原子边界不为缩小 PR 而拆坏。保留实际消费端、复现和既有提交的来源，不以换标题掩盖相同的实现或未满足的合同。原 PR 可以在后续通用方案中关闭或被替代，依赖与证据的接续须明确；这不是停止产品开发的条件。

用户进一步明确允许必要时直接适配 TypeScript 原上游。cordis-rs 当前说明将自身定位为 [cordiverse/cordis](https://github.com/cordiverse/cordis) 的 Rust 移植与重设计；本次核验的原上游为 TypeScript/JavaScript，不把先前口述的“C 库”误写为已存在的 C ABI。实现语言和具体维护者不是新的永久绑定：持续维护必要的 Rust 补丁、跟进 Rust 上游或适配原 TypeScript 实现均可按真实成本与行为证据选择；本条允许后备路线，不宣称已经完成迁移。

本条补充既有 Cordis 实现选择，不降低生命周期、typed service/event、代际清理和恢复要求。终止接纳与并发创建之间的原子边界、所有已接纳插件的真实清理、调用者取消后的完成所有权，以及 Shenko 持久外部效果的独立对账仍须成立。更换实现不得以空适配器、仅签名兼容或丢弃旧行为换取表面可用，也不新增日常开发或自主操作的人类审批门禁。

<!-- /migrated-contract: SPEC:48.6 -->

<a id="spec-50"></a>
<!-- migrated-contract: SPEC:50 -->
# 50 追加事实

<!-- /migrated-contract: SPEC:50 -->

<a id="spec-54"></a>
<!-- migrated-contract: SPEC:54 -->
# 54 自主设计原则 / Autonomous Design Principles

来源：用户指示 2026-11-06；与 [§1](architecture.md#spec-1) 产品定义、[§48](architecture.md#spec-48) 主体自主性、[§51](cognition-units.md#spec-51) 认知元概念一致。

<!-- /migrated-contract: SPEC:54 -->

<a id="spec-54-1"></a>
<!-- migrated-contract: SPEC:54.1 -->
## 54.1 从人类心智和认知学角度设计

Shenko 的认知架构从人类心智和认知科学出发设计，不从模型限制或当前技术栈的瓶颈出发。[§51](cognition-units.md#spec-51) 的认知原子——内容、态度与理由——是从人类认知结构提取的，不是为了绕过上下文窗口或 token 限制。遗忘（[§51.7](cognition-units.md#spec-51-7)）参照 Ebbinghaus 和间隔重复研究；态度参照人类对知识的信心、熟悉度和价值评估。

这不意味着忽视工程约束。模型的上下文窗口、推理速度和准确率是真实的运行环境事实（与 [§51.10](cognition-units.md#spec-51-10)「上游或操作系统自己的检查是外部事实」同理）。设计从认知需求出发，实现在真实约束中落地。区别在于：约束驱动适配，而非约束驱动设计目标。

<!-- /migrated-contract: SPEC:54.1 -->

<a id="spec-54-2"></a>
<!-- migrated-contract: SPEC:54.2 -->
## 54.2 不受模型限制约束的设计目标

产品设计不把当前模型的具体限制当作永久边界。上下文窗口会增长、推理能力会提升、多模态会普及——设计应预留这些方向的扩展空间，而不是把「当前模型做不到」当作「不需要设计」的理由。

具体而言：
- 认知结构（awareness、perception、态度、理由链）的设计不以特定 token 预算为前提。
- 通道和环境感知不以特定模型的工具调用格式为前提。
- 子代理委派不以特定模型的并发能力为前提。

当前实现可以在当前模型能力范围内做最优适配（例如用 taste 控制投影长度），但架构本身不因此收窄。

<!-- /migrated-contract: SPEC:54.2 -->

<a id="spec-54-3"></a>
<!-- migrated-contract: SPEC:54.3 -->
## 54.3 可扩展与插件化

已有的插件化架构——[§45.10](cognition-units.md#spec-45-10) 的依赖装配、[§51.8](cognition-units.md#spec-51-8) 的能力契约、[§52](channels.md#spec-52) 的 `ChannelPlugin`——是产品级承诺，不是开发阶段的权宜之计。核心原则：

- **能力可安装、可替换、可学习**（[§51.4](cognition-units.md#spec-51-4)）。awareness 和 perception 是原子插件，不是硬编码列表。
- **主体可自修改**（[§51.4](cognition-units.md#spec-51-4) 版本化根）。core runtime 整体可替换，不需要人工审批。
- **协议可插拔**（[§52](channels.md#spec-52) `ChannelPlugin`）。新通道不改动已有通道代码。
- **宿主能力可插拔**（[§46.1](../governance/COMPLETION.md#spec-46-1) 原生执行插件）。编码、部署、远程操作都是可安装的 Skill，不是产品本体。

扩展不等于泛化。每个扩展点有明确的 trait 或能力契约边界。不因「可能有人需要」而预留未使用的抽象。

<!-- /migrated-contract: SPEC:54.3 -->

<a id="spec-54-4"></a>
<!-- migrated-contract: SPEC:54.4 -->
## 54.4 更好的 Awareness 与 Perception

[§51.1](cognition-units.md#spec-51-1)–[§51.3](cognition-units.md#spec-51-3) 把 awareness 从文档降级为能力、把态度从 metadata 升级为一等记录。这是起点，不是终点。目标是让 Shenko 的 awareness 和 perception 持续演进：

- **Awareness 自主习得**（[§51.4](cognition-units.md#spec-51-4)）：主体可以从经验中学到新的 awareness 原子，不只是安装预写的单元。
- **Perception 多源融合**：环境感知（[§44](plugins.md#spec-44)）、时间感知（[§43](configuration-playground.md#spec-43)）、社交感知（[§42](quota-resources.md#spec-42)）、通道状态（[§52](channels.md#spec-52)）、资源压力（[§51.7](cognition-units.md#spec-51-7) 存储压力）都作为感知输入，主体自主综合判断。
- **态度驱动行为**：态度不只是标签。态度影响主体的注意力分配、任务优先级、通道选择和遗忘决策。这些影响由主体的推理循环自然产生，不由硬编码规则强制。

<!-- /migrated-contract: SPEC:54.4 -->

<a id="spec-54-5"></a>
<!-- migrated-contract: SPEC:54.5 -->
## 54.5 同行参考 / Peer Reference

设计参考同行产品的公开方向，但不复制其具体实现或受其限制约束：

- **DeepSeek SuperAgent**：自修改（self-modification）和可逆执行。Shenko 的版本化根（[§51.4](cognition-units.md#spec-51-4)）和 COW 工作区已覆盖可逆性。不采用其中要人批准浏览器半边的做法（[§51.4](cognition-units.md#spec-51-4) 已明确）。
- **Muse**：持续个性和情感状态。Shenko 的持续身份（[§5](subject-runtime.md#spec-5)）、态度（[§51.2](cognition-units.md#spec-51-2)）和 personality awareness 覆盖此方向。不采用情感模拟或角色扮演路线。
- **Claude Duet**：协作与多代理编排。Shenko 的主体-子代理架构（[§6](subject-runtime.md#spec-6)、[§53](model-children.md#spec-53)）和 ChildIdentity 覆盖此方向。保留单一主体的设计，不做对等多主体。
- **OpenAI Dot**：个人助手与记忆持续。Shenko 的 Git 记忆（[§7](plugins.md#spec-7)）、原始会话证据（[§8.3](wasm-home.md#spec-8-3)）和跨设备同步（[§5.4](nodes.md#spec-5-4)）覆盖此方向。不采用云端依赖或厂商锁定。

参考方向是公开可得的设计理念，不引入这些产品的具体 API、数据格式或协议依赖。Shenko 的设计前提是本地拥有、主体自主（[§51.5](cognition-units.md#spec-51-5)、[§48](architecture.md#spec-48)），这与部分同行的云端优先方向不同。

<!-- /migrated-contract: SPEC:54.5 -->

<a id="spec-57"></a>
<!-- migrated-contract: SPEC:57 -->
# 57 主动个人主体：更少的门、可修订的 harness、界面与 IM 精炼（Claude Fable 5.1 refine 版本）

来源：2026-11-09 owner 全权委托的整体架构与 taste 再审（"全权交给你"）。执行：Claude Fable 5.1（magpie `commandcode/claude-fable-5-1`）。设计全文、现状证据与打样记录见 [决策 0004](../decisions/0004-proactive-being-refine-2026-11-09.md)。本章不改 [§1](architecture.md#spec-1)–[§56](../requirements/traceability.md#spec-56) 字节；与旧读法冲突时以本章和 [§51.10](cognition-units.md#spec-51-10) 治理为准。它把 [§51](cognition-units.md#spec-51)、[§54](architecture.md#spec-54)、[§55](verification.md#spec-55) 已经写下的方向落成可判定的合同，不新增审批、不新增权限门、不新增产品 allowlist。

本章把 owner 2026-11-09 的指示记为 **U64**：

> 「请你重新基于 SPEC 审视我们的所有架构。做完的和没有做完的。确保自己全部读取过以后进行自主性的深刻反思和创新。我们需要更自主，我们不需要模型限制，我们需要可扩展和插件化，我们需要更好的 awareness 和 perception。」

> 「我完全信任你的 taste 和架构设计……我希望这是一个 proactive personal being/agent……我需要更少的 gate 和写死的 harness arm。让模型能更好的 proactive 工作自我更新迭代是最重要的。」

> 「记得把你写的写上这是 claude fable 5.1 refine 版本。」

<!-- /migrated-contract: SPEC:57 -->

<a id="spec-57-0"></a>
<!-- migrated-contract: SPEC:57.0 -->
## 57.0 为什么要这一章：骨架是对的，肉是死的

读完全部 SPEC 与实现后的判断：**账本与封口是对的，一条不动；错的是主体的心智形状被写成了 Rust 常量。**

已成立的内核不变量（本��只列举，不改）：S0 封口 / S1 提交与后继登记原子性（`store/store.rs:734-968`）、committed-only 发布（`loop_runner.rs:1890,1935-1951`）、外部效果 T1/E1/T2 与 unknown 不盲重发（`store/channel.rs:302-453`）、单写者与选主 fencing（`bin/shenko/src/lib.rs:1268-1290,2960-2970`）、hook 图与 provider 绑定封口（`loop_runner.rs:1048-1155,1250-1256`）、主体自更新 prepare/activate/rollback（`update/native.rs:290-560`）、认知单元 write/trial/judge 闭环。

确实还没做完的，集中在四处：

1. **心智形状是常量。** 角色文本（`loop_runner.rs:779-787`）、空闲时那句"继续。"（`context_projection.rs:20,642`）、每步 64 事件 / 80 ms 空等（`loop_runner.rs:588-592`）、未点名事件自动 Acknowledged（`loop_runner.rs:1785-1798`）、child 角色框（`model_child.rs:92-105`）——都是编译期文本。
2. **三个主体接口是死的。** [§5.2](subject-runtime.md#spec-5-2) 能力表里的 dialogue、resource policy intent、operating policy intent 在 `store/intents.rs:85-356` 被静默标成 `blocked`，因为从未装过派发器。`[dnd]` 存下来了但没人执行（`authored_controls.rs:103-118`）。`state.goals` 永远为空（`loop_runner.rs:341,449`）。
3. **界面迟钝。** 流式三件器官全部造好却没接线：`broker.submit_stream`（`broker.rs:436`）、store 的 [相关领域合同](durable-state.md#content-stream-journal) 内容流日志（`content_stream.rs`）、UI 的 `StreamSink`（`ui/stream.rs`）——`bin/shenko` 从不调用 `with_stream_sink`，UI 诚实报 `streaming:false`；`rich` 动态文档渲染成 `<pre>JSON`（`react-renderer.tsx:638-640`）；主体没有创建作品的工具（`playground.rs:61`）；`src/static/app.css` 落后于 `web/src/app.css`。
4. **IM 只有一种动词。** 出站只有 `[reply:kind:id]` + 纯文本（`output.rs:325-352`、`napcat/channel.rs:392-398,439-444`），不能 @、不能引用、不能发媒体；打字指示结构上不可达；撤回等 `notice` 落盘但不给主体（`channel.rs:127-129`）。

本章的写法：**凡是决定"主体像谁"的，都变成主体能修订的内容；凡是 ABI 与不变量的，都留在内核。**

<!-- /migrated-contract: SPEC:57.0 -->
