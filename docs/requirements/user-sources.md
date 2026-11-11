# 用户原话与来源


<a id="spec-33"></a>
<!-- migrated-contract: SPEC:33 -->
# 33 用户需求原文与追加澄清

本附录逐字保留最初需求及直接追加的产品要求，便于与 §32 的规范化需求句对照。引用块内出现的技术判断、版本设想与后来澄清都保留原貌；当前有效的规格解释以用户最后澄清和正文裁决为准。第三方评审建议不是用户自动批准的硬要求，其完整讨论的采纳、修正和拒绝见 §30。

## U01 最初完整需求

```text
我们来讨论一种类似 AstrBot 的个人/群聊可用的 Agent Bot。但它使用 2026 最新潮的技术。首先，它有自己持久的运行环境，一个 Linux / POSIX 兼容的 wasm 沙箱。并且可以往里面挂载任意路径。这使得它可以自由记录很多文档啊，记忆啊的东西，而且还能运行工具。同时我们可以用通用的接口为 wasm 沙箱安装基本的 toybox 之类的或者更大的例如 gcc-wasm 等等任意工具，它自己也有这个能力，同时它也能兼容任意本地记忆系统【因为那些github记忆系统方案通常也都是linux/posix 兼容的】。而且沙箱之外的它有的是最小化的 agent software，可以在 wasm 沙箱 down 的情况下自动回滚回最初版本的沙箱等等有类似 git 的版本管理系统【但要求更适合包含可能存在的二进制文件管理，并且也允许接入 git 仓库作为记忆和知识库（要求支持认证的，基于 gh-cli wasm，或者 gitea cli wasm，你也不想自己 agent 的私密记忆被其他人看光光吧？而且我们默认初始化状态下用初始 wasm 沙箱里的一个挂载在主机目录上的路径 git 作为记忆】，而且，它需要的时候，可以给自己 generate ssh private key 并在人类操作界面暴露自己的 ssh pubkey 这样可以 ssh 到人类的各种机器上进行操作(或者是ssh 到后续可能支持的全功能容器，甚至不同的属于它的节点)【ssh wasm 需要支持哦】。它应该用 nix 或者 nix like 的系统管理包。同时由于核心保持最小化，几乎只有操作 wasm 基础 runtime 以及基于 runtime 去访问其他机器/沙箱的能力，因此它的核心很小，不容易损坏，长期运行的负担也很低，甚至能跑在路由器上。另外，它要能像 Codex 的 budget compact 一样能支持从历史 session 读取和回忆起东西，这样才能有足够的真相源。你可以考虑 [https://github.com/Ephemeral-AI-Lab/layerfs](https://github.com/Ephemeral-AI-Lab/layerfs) 作为 fs。但它需要真实操作系统。我们需要的其实是它的 wasm 版本或者裁剪版本。但它也许可以集成到后续的沙箱里。我希望这个 agent 是全自主的，永远运行的。即使人类不在，它也会默默维护自己，执行一些事物，就像真正的人一样。你可以参考 OpenAI Dot 和 Claude Duet 和 Meta Muse，你可以当成竞品详细调研。但可能比那还要更加自主化一点。另外，这是为极致隐私和本地 AI 设计的，我们应该可以详细设置它接入的 API 的最大并发数量、上下文窗口，防止多并发打死本地 API，走全局锁【按请求紧急状态进行排序】。另外，应该用 rust 编写它的核心功能并采用 webserver 的方式对外暴露基于 proto ui 的标准的类似竞品的界面，但界面应当活灵活现，let layout talks，并采用 2026 年 10 月最新最酷的设计规范，并使用 proto-ui 最新 0.3.0 rc 主线 + shadcn【注意，还在早期演进中，记得及时更新版本，而且留下后续替换成 proto-ui compiler 版本的插桩适配空间，在文档里要求每次都 bump 到最新主线】?。deepseek harness 也会是你的参考对象。另外，别忘了通过 iLinkBot 以及官方机器人 API / NapCat，或者我们的 napcat-cli 原生接入 QQ bot、飞书等等的能力，你看看 OpenClaw 吧，可以接入非常多东西，但是不要像她那样限制太多。默认全允许对话【管理员权限则需要手动设置】就可以。我希望它足够轻，开 subagent 递归搜索、研究和 deepresearch 的代价足够小，且能深度搜索但也能在合适的时候收敛，也就是 layerfs 那样能试探多种方案，而且不会撞全局并发上限锁，尤其要考虑能把 kv cache slot 透传的情况，它能更好管理多并发时的上下文哈哈，否则一直cache miss跑整个prefill对本地AI几乎不可用。我们还应该包括和 Ninfer, Strata 搭配使用的中英文教程。为了减少本地 agent 的思考难度，我们应该通过插件或其他手段 提高 ssh 文件访问 ssh 命令执行等等工具、也包括 memory 之类的工具，可以参考 oh-my-pi 的实现，说实话我觉得它非常牛逼需要大量参考研究。包括 pi 和 deepseek harness 的可扩展性也是非常牛逼，而且 cordis 甚至有 rust 版本：[https://github.com/dshbox/cordis-rs](https://github.com/dshbox/cordis-rs) 。对了会话记录应该放到 SQLite FTS5 之类的东西里，大量参考同行最新潮性能最好的设计。而且我们的内核也要方便跑在各种设备上：采用 wasm 作为沙箱，让我们很方便能把这个甚至部署到 Android 设备上！总而言之这很好而且很稳定。你需要把我的需求一字不落地复述一次，然后深度研究，grill me with spec 然后撰写一个完整的产品 SPEC，先不进入开发阶段。
```

## U02 单线程主体、委派与人格

```text
对了，为了让他足够像人，它真正的持续的主动 thought loop 永远是单线程的。它会一直思考接下来该做什么，永不停止，但它可以也会被积极鼓励 handoff 任务给子 agent 进行探索和自我改进。而我们说的版本任务进度和记忆机制发生在 subagent。主动 thought loop 很少能做指派思考、保存、读取记忆和指派 subagent 外的事情，它不被允许直接操作和写入文件。另外还允许给这个 agent 人格，但追加在我们的一大串 system prompt 后面，不要影响工具调用啊什么的。
```

## U03 两篇论文、外部消息与 KV 驻留

```text
另外看看 Preprint: letting research agents forget (restart from the best result) beat letting them remember everything. Do you reset agents on purpose?

[Discussion](https://www.reddit.com/r/AI_Agents/?f=flair_name%3A%22Discussion%22)

A preprint from UC Berkeley, Stanford, and Bespoke Labs (arXiv, Oct 14) tested teams of coding agents doing automated research on four tasks: heuristic programming, kernel optimization, and model training.

What happened: more time, more agents, and more communication all plateaued as history piled up. The authors say each agent anchored to its own past attempts and made fewer, smaller edits.

The fix was periodic forgetting: discard every agent's session and history, then restart from the best result so far. They report up to 15% better results than parallel agents without it, and up to 39% better than a single agent.

Caveats: it's a preprint, the tasks were bounded research problems, and I wouldn't assume it transfers to everyday agent work. Drew Breunig calls a related pattern "context inertia" (an agent holding to an early hypothesis after the evidence changes). That's one writer's term, not an established diagnosis.

For people running long agents: do you reset on purpose, and what do you carry over so you don't lose the work? 和 [License: CC BY 4.0](https://info.arxiv.org/help/license/index.html#licenses-available)

arXiv:2607.08716v1 [cs.AI] 09 Jul 2026

# Remember When It Matters: Proactive Memory Agent for Long-Horizon Agents

Yifan WuMeta AILizhu ZhangMeta AIYuhang ZhouMeta AIMingyi WangMeta AIBo PengMeta AISerena LiMeta AIXiangjun FanMeta AIZhuokai ZhaoMeta AI

###### Abstract

In long-horizon tasks, decision-relevant state is often scattered across an expanding trajectory, while the action agent must surface it and act. As trajectories grow, task requirements, environment facts, prior attempts, diagnoses, and open subgoals can be buried in the context window or pushed beyond it, failing to influence decisions when needed. We call this failure mode *behavioral state decay*. We study memory as an active intervention mechanism rather than passive retrieval. A separate memory agent runs alongside an unmodified action agent, updating a structured memory bank from the recent trajectory and deciding whether to inject a memory-grounded reminder or remain silent. The module is plug-and-play with frontier action agents and existing agent harnesses. Across Terminal-Bench 2.0 and `\tau^{2}`-Bench, it improves pass@1 for both weaker and stronger action agents, with gains of +8.3 pp on Terminal-Bench and +6.8 pp on `\tau^{2}`-Bench. Ablations show that selective intervention outperforms passive bank exposure, always-on injection, advisor-only guidance, and general retrieval. As an early step toward open-weight memory policies, we train Qwen3.5-27B on SETA using SFT and GRPO, improving validation reward and achieving partial transfer to Terminal-Bench.



另外别忘了，如果有外部消息进来，几乎总是这样，会直接注入到思考循环中，也就是 agent 会收到：从 XXX 渠道的 XXX（别名？ID？Agent 也许应该给它认识到的人编号的，希望能自然演进出这种行为或者作为插件有这种行为，甚至允许它对声纹识别，只要有算力可以做识别）给你发了消息，内容是什么什么，如果有富文本内容甚至文件应该去哪里下载怎样获取等等等等。包括消息来源 provider 也应该是插件、也包括 API provider 等等。



以及**内核轻量，不等于持续思考不消耗算力。** 如果主体永远占住唯一的推理槽，子 Agent 就无法工作。这是不对的。我们应该支持基于 KV 池的单并发轮转模式：也就是以 KV 池的一半进行主进程另一半给子 subagent，这样就不会老 cache miss。但这只是应该满足的一个 case，理想形态下，我们会设计一种算法允许主 agent 自由调控，或者纯粹靠算法分配一个基于并发数量和 kv 池大小（也许可以不填？）的较优解？
```

## U04 远端 Agent 遥控、选项决策与联系用户

```text
而且它应该有远程 Agent 遥控能力的。例如远程的在某个 tmux pane 或者 agent seat 里的 claude code 或者 omp 之类的，可以允许其他地方来的消息源让它做某某任务，然后它自己找记忆里任务在哪个 runtime 直接 ssh 或者其他远程方式过去调控这个任务。反过来，远程的 agent 需要选择【比如调用 askuserquestion/permission 的时候，它自己主动决定选哪个选项【自主性非常强！】或者觉得拿不准，主动知道要用所有能联系上用户的方式去联系用户（不是系统的硬报错或者固定的联系方式！依然是基于 agent 的个性和行为的！）这种行为和能力希望也是作为插件安装。另外我们还要支持传统的 Skill 和 MCP 市场倒是，但不推荐使用太多，还是推荐包装成原生的插件。
```

## U05 可恢复 WASM REPL

```text
而且它最好也有个可恢复的 python / bun / node repl in wasm 之类的而且能动态装上包，这样能做不少其他情况做不到的事情（）这些也做成插件吧
```

## U06 插件认知与 27B 模型研究

```text
插件不仅要能装上去还要确保它会用且会在正确的情况下使用。这一点你可以参考 Skill 的心智设计，但也要须知我们在面向未来设计，因此模型以后可能并不需要这些。27B 模型的特点是推理能力很强但世界知识不足。要搜索相关论文做这方面的优化。
```

## U07 电话、旁听与媒体能力

```text
最好原生留一个槽位和插件能力允许后期和它打电话～ 可以和我一起旁听一节课甚至更多功能。这种状态遵循外部插入信息的状态依然有完整的工具调用能力，而且允许它录音允许它辨认声纹啊什么的，都由它自己发展。
```

## U08 最小核心

```text
别忘了核心要非常小，一切都是插件
```

## U09 任意部分自我重写

```text
而且就像 deepseek harness 一样，agent 可以选择给自己重新编写代码替换自己的任何部分
```

## U10 Computer Use

```text
千万别忘了 computer use 能力哈。我们只是最小核心属于 wasm。后期是要可以接入给它的 Linux / Windows / MacOS 的任何容器并给它操作能力的。这也是插件要写到计划里。
```

## U11 Personal Agent 定位

```text
是这样的：Pi 实际上是 for coding agent 的最小框架。我们是 for personal agent 的最小框架。
```

## U12 全部派子 Agent 调研

```text
以上内容你别忘记了。全部都要派 subagent 仔细调查。
```

## U13 动态生成式 UI

```text
别忘了 OpenAI 最新推出的 Dynamic Response 功能。能针对不同的介面给出能力降级的动态生成式 UI 也是我们给 Agent 赋予的能力之一。【可以告诉它可以给那边的 DSL 生成什么 UI 哪些元素和语法可用，也可以是暴露的工具】
```

## U14 对外部评审的阅读要求

以下是该条消息开头的用户指示；其后粘贴的第三方长篇建议不作为已确认需求重复，已完整进入 §30 的研究裁决。

```text
也请你读一下 DeepSeek 和 Gemini 给你的建议【不要全盘采纳，而是采纳对你而言有启发的点，因为他俩幻觉比较多】。
```

## U15 否定永久 Ring 0 与低频 Tick

```text
永久不可改的 Ring 0、把永续思考改成低频 Tick这些的确违反了需求，不要听他们的！
不过，50/50 KV 场景这一块，也许有调度器，或者插件，确实是会比较好一点。
```

## U16 50/50 的最终澄清

```text
50/50是理想情况下这个调度算法应该能做到的自动分配的情况。
```

## U17 自主更新与联网代理选择

```text
就连是否自动更新，联网是否走代理它也可以收到消息。它可以自己选择要不要更新自己而不必问用户。
```

## U18 BoxLite 研究线索

```text
https://github.com/boxlite-ai/boxlite 这也许很有用。你也可看看。
```

## U19 自主补能力、原生接口、Pi Durable、LayerFS VM、Life 与 UI

```text
另外。它要能意识到自己需要一个插件辅助它。比如，连接 tmux 发现 capture pane 无法满足它操作 coding agent 的需求：看不到 TUI 程序的高亮在哪一行！它就要能自动意识到自己需要编写一个插件、外部应用等等。必要的时候它要能和 child 对话而且他俩实际上要能共享能力超集，只是平时执行的任务和 role 不同。



### 后端能力必须真实透传

研究没有支持把 NInfer 和 Strata 包装成完全一样的缓存接口。

- **NInfer** 的内部调度区分 execution lanes、KV、StateImages 和 Host backing。暂停发生在其稳定边界，fresh admission 也不是任意抢占 resident；某些恢复需要 Replay。不能据此承诺“来消息就句子边界抢占，秒回且零 prefill”。依据是它的[固定版本调度文档](https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/maintainer/resource-scheduling-and-context-cache.md)。
- **Strata** 的并发有权重共享收益，也有额外会话状态挤占专家缓存的成本。是否串行应由真实部署测量决定，不能按“两个任务领域不同”一律禁止并发。Host conversation parking、GPU 驻留和并行解码也要分别表达。依据是其[固定版本 batching 文档](https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/BATCHING.md)。

**请求内抢占并非单并发能够工作的必要条件。** 在没有它的后端上，有限 generation 之间公平轮转，已经能使主体和孩子持续推进；请求内抢占则进一步改善紧急响应延迟。因此我们需要支持 strata vllm(包括1cat-vllm) ninfer等等 api 透传而不只是 openai compatible 就满足了。



这里吸收了 Cordis 的生命周期思想，但没有把它夸大：**进程内清理回调不能替代断电后的持久恢复，也不能撤销已经发生的外部操作。但是 pi durable 的架构就可以。你可以进行参考和设计。**



### WASM 是明确兼容目标，不能直接等同 Linux

WASI 0.3 已发布，但这不意味着 Linux ELF、动态链接、原生扩展、完整进程语义会自动具备。所谓“Linux/POSIX 兼容的 memory 项目”，仍可能依赖需要移植的部分。好极了，正好可以让模型意识到自己可能需要自己给自己在有限空间内移植需要的 wasm 能力并安装插件。



### LayerFS 值得参考，但不能直接承诺其未提供的持久性

已核验到 LayerFS 的 v1.0.0 source release；其发布资料仍区分平台和持久性范围，Disposable profile 不提供崩溃、断电持久性保证。我们不应该重新实现一次，而是在 vmm 之类的真实 litevm 里直接用它。而基于 wasm 的状态则用我们自己基于最新论文和设计所需要的实现。



联系人编号、声纹关联可以演进，但候选身份关联与管理员权限是两回事；一次匹配不能自动产生管理身份。你看 life 吧我们已经在做了。



详细 KV、分支树和 Agent seat 状态可以展开查看，不要求把整个首页做成监控大屏。我感觉你做的界面审美有点太刻板。这里就把功能和要求都描述了让 Claude Opus 来实现吧！OK我来看看你的一堆D的问题
```

## U20 D1/D2/D3/D7/D8/D9/D11/D12 首轮裁决

```text
### D1：同一个主体，在群聊时应“知道”多少私聊内容？

它可以顺序切换不同受众的上下文，也可以把全部私聊都带进当前上下文，再靠模型判断什么不能说。

**我的建议是同一主体、按受众切换注意力投影。** 代价是它并非每一轮都同时记着所有秘密。把所有私聊都给模型，再只靠文字规则保密，无法提供同等级的数据隔离。

你更希望哪一种体验？



作为主体，它应该什么都知道。只是选择性地说与不说。如，某个用户对它说：这是个秘密，请替我保守。理想情况下它会保密的。但是也可能有这种情况：它与那个人闹翻了，它不想替他保守了！那也可以不保守。这都取决于它的“人格、灵魂【允许自己修改自己的灵魂！基本上是能描述它为人处事哲学和自我认知的一句、数句话。而 subagent 的 system prompt 则由它指派任务时可以任意书写甚至指派不同模型】”如何写。而不是写死的“亲密度”机制，模型可以自己翻过往的回忆来取得自己和他人的关系，而不是系统强制注入，这不人道。



### D2：主体委派回复后，对话子 Agent 能否直接发出结果？

有限情况下可以。具体参考 omp。他们做得不错。



### D3：唯一推理槽被孩子占用时，紧急消息值得损失多少已算工作？

如果后端不能暂停，只能等待或取消当前 generation，这个取舍必须明确。

我的建议是普通消息等下一有限轮次；真正紧急的输入允许取消尚未提交的输出。但“多快必须回应”与“能接受多少重算”需要你的偏好，不能假装一个 priority 字段已经解决。

基本上是最紧急的。允许打断 child 正在做的事情。但如果频繁打断导致 child 完全没进展了，可以询问主体 loop 是否需要暂时打开免打扰一段时间，免打扰结束再提醒它去统一处理免打扰时队列里的消息【像不像人类使用手机？！对，就是这样的。】。甚至也允许它设置垃圾信息屏蔽规则【囧】。但是如果不小心设置了通配符导致一点消息也收不到，它最好也能自己搞明白，发现这件事情是个问题哈哈哈。



### D7：自改存储格式时，更愿意付磁盘成本，还是付兼容成本？

磁盘成本！我们可以随时 break change。而升级时由于我们已经提醒主体思考 loop 我们要不要升级，而且它能决定要不要升级。我们完全可以假设它知道要升级这件事情。所以理想情况下，它应该先跑旧副本，迁移数据、然后烟测新版本是否能工作，最后再把自己迁移过去。这个能力也应该是插件。



### D8：原始文字、录音、屏幕帧分别保留多久？

最好的情况下是永久保留。空间不够时交给它自己决定要不要忘掉什么或者传到用户自己的网盘啊其他存储空间啊什么的。这个行为也写成插件。



### D9：它无人陪伴时，怎样判断自己的兴趣值得继续研究？

主体永续已经确定。需要定义的是子任务的资源与交付约束。

例如它突然对一个与你无关的数学问题感兴趣，能否持续投入？应限制总 token、设备功耗、并行探索数量，还是要求定期形成可查看成果？你希望它拥有多大的自发兴趣空间？

是的，能持续投入。这正是它应该做的。



### D11：Android 被挂起时，应不应该由另一节点接过主体？

可以恢复原设备后继续，也可以由在线节点接管。

后者需要明确仲裁，网络分区时不能让两个设备同时成为“自己”。你希望优先保证随时在线，还是优先保持主体在指定设备上的归属？



后者可以的。优先保证永远在线。如果多个节点均有主体，则让他们动态投票仲裁谁成为新主体，然后只留下新主体把另一个主体的记忆、任务合并过来。



### D12：首个真实评测基线是哪套模型与硬件？

目前是我本地的 Qwen3.8-27B。你可以从 life 看到相关信息。通过 Tailnet 调用。
```

## U21 budget compact、snapcompact 与四流 128K 更正

```text
budget compact 和 snapcompact by omp 其实也都是值得你参考的对象了。然后我们其实是四流 128K 的配置，其中 133 是冗余。
```

## U22 两种 compact 均作为插件

```text
这两种 compact 都要实现为插件哦
```

## U23 讨论名字

```text
对了 我们应该讨论一下名字？
```

## U24 框架名与实例名分离

```text
它是一个朴素、好用的框架名，也可以是一个有“持续存在、自主生活”意味的产品名。但不能是用户直接称呼它的名字。每个人都可以给它名字，在初始化时。或者，就让它自己起一个。
```

## U25 中文意象

```text
这个有点没有中文意象啊？太西式了。
```

## U26 亲切感

```text
又太禅了～ 不够亲切。
```

## U27 Jotai 参照

```text
就像是状态（Jotai）这样的感觉
```

## U28 产品定位与名字

```text
这些又和自主的智能体没关系了？但是我们做的其实不是智能体，而是超过一点的东西。
```

## U29 Sonzai 偏好

```text
`主体 / Shutai` 让人联想到主体思想会爆笑的。`自己 / Ziji` 听起来像指代用户自己。

我比较青睐存在 Sonzai。
```

## U30 不要 slogan

```text
no slogan plz。一旦解释了这是什么就没有逼格了。let layout and design tells, not words and slogan
```

## U31 竞品命名灵感

```text
你可不可以从 dot duet muse 这种地方拿到灵感
```

## U32 首版自我开发

```text
我们可以让第一版产品就能实现自我开发自我进化吗？部署在开发者的机器上，能连接 napcat，从而直接就能遥控 tmux 里的 claude code 或 omp 或其他 agent 【注意我们的 SPEC】继续开发自身？
```

## U33 插件副作用与恢复边界

```text
那么插件也许需要标明沙箱可以重置回退恢复的边界？会产生副作用的地方需要标清楚。
```

## U34 注入时序与 Fabric

```text
你会发现我们可能会在非常多的时候注入这些消息吧。插件如何安排注入的时序？如何新增这些注入钩子啊什么的？也许可以参考 Fabric 的设计。
```

## U35 保留 colibri-rs 主干

```text
当然。我们的 colibri-rs 依然是插件化的主干。刚刚说的仅仅是 API 先后注入的兼容问题
```

## U36 内置自解释插件

```text
对了，其实我们还可以参考 omp 的自解释机制。agent 可以读取到自身的代码和说明书以及设计哲学。因此问到自身相关的问题也可以回答，而且还可以在编写自身插件代码的时候起到作用。这个也写成插件，只是内置。
```

## U37 来源、完整度、开发规范与 Magpie

```text
我们的所有想法来源都要标一下 ref，不仅是 credit，也给这个行为找到用于做完整程度比较的上游。并确定一下开发规范。目前应该尽量宽松不留下人类 gate。也就是开发 Agent 需要高度自主化。另外，界面的风格和美观程度要参考 magpie。
```

## U38 询问名字

```text
对了。名字有了吗？
```

## U39 正式命名 Shenko

```text
那么就叫Shenko(深刻)吧，出自 Always Online 这首歌的 More and more 尽是深刻。威妥玛拼音比较酷哈哈。shen1-kʻo4 ㄕㄣ ㄎㄜˋ。
```

## U40 规格具体程度与 Pi Durable 参照

```text
对了 人家写的 spec.md 为啥那么好啊。你的好像不够细化和具体？[https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2045](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2045) 如果你还有问题尽管说。
```

## U41 周期额度与稳定流式体验

```text
对了，对于 Claude 这类能检测到周期额度的 provider。要能够提供用量估测从而选择用聪明还是笨模型以及影响思考循环运行的平均次数哦。理想情况下通过自适应调优可以完美刚好跑完一周的额度之类的。也要让 Agent 自身能感受到自己依赖的供应商还有多少额度。当然，我们是为了本地 AI 设计的，这个排后面去。以及 magpie 和 alma 的流式输出界面几乎不抖动是我们 WebUI 的榜样【爆笑】。
```

## U42 默认主体时间线与 append/steer

```text
WebUI 默认只展示一个和深刻脑内思考循环对话的界面哦。【其实也不是对话，而是在它这一轮思考结束后 append 消息 / steer 打断插入消息罢了。】
```

## U43 对话配置与持久 Playground

```text
理论上所有配置和设置通过和它对话都能更改而且不会给自己改坏。但是还是要提供一些配置面板方便人类查看的。顺便给它一个 playground 可以长期挂载它因为兴趣或者和人类的交流创建的 artifacts 或者能互动能有后端和界面且模型也能感知到它的变化的 Intelligent UI
```

## U44 首次启动与可安装意识

~~~text
思考一下如何 bootstrap？用户第一次打开 agent 时我们必须配置什么呢？

以及，**准备候选 → 验证 → 应用 → 观察真实运行 → 确认生效，这个不是写死的吧？应该也作为一种可安装的意识，一种 awareness 能力或者插件？**
~~~

## U45 Cordis 名称更正

~~~text
对不起啊是 cordis-rs 我打错了
~~~

## U46 Awareness 与全包含插件的区别

~~~text
awareness 是能力而不是全包含的插件哈
~~~

## U47 独立可安装的直觉与意识

~~~text
awareness 是一种可以安装的直觉和意识。能弥补世界知识缺少带来的意识不足。类似于 Skill 但 Skill 偏执行，awareness 更偏向于直觉。也是一种类似插件的东西。但不是 awareness 插件，然后所有我提到的意识都塞这一个插件里。而是不同意识分到不同可安装的 awareness 里。
~~~

## U48 感知、直觉及自动版本依赖

~~~text
比如 QQ 连不上了？感知到连不上这个状态是一种感知能力，连不上了应该找 NapCat 而不是什么 QQ.exe 是一种直觉。它们在我们这里都应该是可原子化安装、查阅的。而他们被 napcat 外部连接 provider 所依赖，在安装的时候会自动安装这些直觉、Skill、插件依赖的代码。插件之间也会存在相互的包含版本的依赖。直觉的依赖则可以笼统或精确。
~~~

## U49 Why 与自主判断

~~~text
日常赞美 Gemini 4！哈基米！

今天它对一个 Skill 的运行评价：「现有 Agent 设计于 2026 年初，包含大量零散、硬性的流程规定（“必须执行某脚本”、“禁止修改某参数”），但缺少顶层的 “为什么（Why）”。当底层工具报错或遇到特殊情况时，强推理模型会因为流程规定与科学直觉冲突而产生内耗」

这才叫前沿智能嘛！

Why 很重要哦。
~~~

## U50 文档仓库与完整开发交接

~~~text
整理一下你现在到底有哪些 SPEC 文档？其他项目【最新最热的项目】如何治理 Agent 驱动的开发？我们接下来可能要 HANDOFF 给 Claude Opus 5.5 持续循环开发了。在这边提前定一下规矩。让它可以 /goal 执行开发并尽可能少地问人类相关的决策。另外 Claude 会是我朋友的 Agent 最后以提 PR 的形式交到我们仓库。因此我们要先新建一个仓库把所有 SPEC、WHY、README、AGENTS.md 放进去然后用一个 /goal 告诉它怎么配置 gh 和注册/登陆 GitHub 账号，如何开始开发，如何交付。什么情况才算完结【注意我们这次的 goal 目标是完整状态。】而且不需要等待我们 review 就可以提 PR 并持续更新而且开一大堆 subagent ultracode 去开发。
~~~

## U51 fork 内自主开发与权限模式澄清

~~~text
1. **auto mode 不是“仅允许 PR”的技术权限。** 官方默认允许当前仓库 push 与创建 PR，甚至包括默认分支。交接约束应持久明确为只推自己开发分支、不合并、不扩权；不要新增逐 push/PR 人类 gate，也不要假称项目里的 `autoMode` 配置能覆盖信任策略。

搞毛线啊给这么多 gate。我的要求是 --dangerously-bypass-permissions 绕过所有限制。虽然我没给他 push 权限但他 fork 以后在自己的仓库里爱怎么开发怎么开发反正我收 PR。不要这么多：“不XX 也不XX”
~~~

本条前段为用户引用并否定的旧交接建议，后段是有效裁决。官方实际 CLI 拼写核验为 `--dangerously-skip-permissions`，等价于 `--permission-mode bypassPermissions`；保留原话与可运行命令的区别。该模式用于朋友开发环境，GitHub 服务端的真实仓库权限照常决定 upstream 访问结果。

## U52 拥有者创建交接仓库

~~~text
我为你创建了 [https://github.com/cyjin-yl/shenko](https://github.com/cyjin-yl/shenko)
~~~

## U53 Claude API 2026-10-21 迁移提醒

~~~text
对了这个注意一下：\
刚刚，Anthropic发布了新版Claude API规格，把temperature、top_p、top_k三个采样参数一起删了。  版本号是2026-10-21，这是Claude API自2023年以来首次推出新的正式版本号。 这次更新涉及采样控制、消息格式、SDK和鉴权方式，做Agent、Harness和模型网关的，升级前需要检查一下自己的格式。  最直接的影响：用新版Messages API时，只要请求里还带着这三个参数中的任何一个，就会返回400错误。 连temperature=1这样的默认值也不能传。  新版统一使用默认采样配置，其中temperature为1.0。 原本依赖低温度减少输出波动的应用，迁移后会失去这项调节能力，官方文档明确写了，没有其他设置可以替代。  此外，新版收紧了消息格式校验：assistant消息里直接放document块、user文本块里携带非空citations数组，都会报错。部分管理和企业分析接口的旧字段也被替换。  旧版2023-06-01目前仍未弃用，可以继续使用。 不过，升级SDK也可能把请求一起切到新版本，例如Python SDK 2.0默认就会使用新接口。  做Agent和Harness的，升级前得检查一下请求头封装：如果还在自动塞temperature之类的参数，这次就需要一起改了。
引用
**ClaudeDevs**
@ClaudeDevs
10月22日
API version 2026-10-21 is now available.  The Claude API has evolved a lot, but this is our first official version release since 2023. We plan to release versions more often so you can handle breaking changes before a new model requires them. &#x20;
~~~

该提醒是核验入口。§47 区分 API 版本变化、模型能力与 SDK 自身变化，以官方迁移文档和实际部署为准；不将社交帖的表述直接作为运行证明。

## U54 时间、未竟事项与可选 dreaming 的原子察觉

~~~text
其实不需要，社区想要的话做成插件就可以了。我只是假设社区如果需要。那么我还问你，定时任务呢？有没有？长程任务怎么办呢？
~~~

~~~text
定时任务也是一种关于时间的察觉能力。其实被放到了察觉分支里。包括 dreaming 其实也是察觉到似乎夜晚到来于是意识到需要整理记忆。应该被写成原子化的东西。长程任务呢？应该察觉到还有哪些事情没有做完，主动去做完。如果任务列表空空如也又整理了太久记忆？是否应该察觉到无聊，主动去看看别的事情比如自己作为 maintainer 维护的项目是否更新？是否需要加新功能？大家在 SNS、IM 上都在讨论什么？察觉到是否需要翻看邮件？当然收到新邮件的时候也会有所察觉。对吧？你现在给我列出的这些具体结构反而让我担心，担心它们没有被自解释，没有被列成原子化的察觉、意识的能力。
~~~

## U55 持续主循环、语义等待与原子开发 Skill

~~~text
为什么会等待呢？thought loop 大部分时间应该一直跑着【除非它主动要求基于语义条件的等待【不是基于时间的等待】而且超时最多 10 min。等待时也会有它自己的模型根据前文 + 新上下文判断是否停止等待一直轮询请求。】。它下辖的其他 agent 才可能 sleep。

OK。那我们可以展开一点。把那些东西写清楚。然后写成 $shenko-dev 专用的哲学上的 Skill ，要求所有开发者均读这一条。并把开发能力拆成原子化的能力【原子 Skill 组】而且不允许设置任何人类 gate，要求必须完全自主化开发，也不允许一个开发进度写十个或更多测试，导致没写多少功能光写测试了，稍微加个功能就要写一堆一堆测试。Skill 怎么写？要求兼容 OpenAI & Anthropic 规范。
~~~

## U56 从任意外部输入汲取 awareness

~~~text
其实，也应该要能从用户 prompt，任何外部输入【群友的聊天、网上刷到的帖子和视频】中汲取 awareness... 正是 thought loop 应该做的。
~~~

## U57 多设备自我同步、设备能力与自主环境维护

~~~text
对了。Shenko 被安排到多个设备上并通过某种方式，如内网直连【直接嗅探感知】或 Tailnet 或 Zerotier 连接起来后，自我可以被多设备同步，且可以得到任意设备上的任意操作能力。比如 Shenko 到了 root 过的 Android 手机上后，就可以完全控制屏幕执行任意 Shell 之类。Linux Mac 和 Windows 也是如此。当然。目前的主要连接方式还是授权 Shenko 所使用的 SSH Key 连接。另外 Shenko 也要自己能发现自己的一些环境有漏洞自动修复和更新哈。
~~~

## U58 统一开发入口的符号链接

~~~text
另外一般给 CLAUDE.md link 到 AGENTS.md 就可以了。符号连接哈。
~~~

## U59 Rust 2024

~~~text
另外 Shenko 用 Rust 2024 哈。
~~~

## U60 分布式容灾与主节点

~~~text
Shenko 的多节点和自我同步使得它拥有了分布式容灾能力呢。但是总会有一个是主节点的。
~~~

## U61 身份加入、异构智能与分区演进

~~~text
Shenko 的多节点身份同步通过其他节点发现先于它创建的节点，然后设定为毁掉当前 Shenko 的身份替换为与主节点的身份同步为开始。另外注意 provider 可能不能同步。也许这些设备每个都跑了一个本地 LLM。Shenko 主节点会知道这个，给不同从节点派发不同的 subagent。而主节点断线时，从节点也会知道主节点的模型不能用了，然后在网络里推选一个最聪明的备用，如此递归下去呢。最坏情况如果全部都断了，那么每个节点【有智能能力的】都会进化成独立节点等待某一天的合并哦。另外 Android 机加 Kimi-Code API，不一定要跑着 Qwen 3.8 27B 也可以算有智能能力。我的意思是能 access LLM 就可以。但 Shenko 要有能力 aware 哪些节点的供应商有隐私风险。
~~~

## U62 智能失效后的日志、记忆提取与离线复活

~~~text
Shenko 的智能能力就算完全怀掉了，依然会打系统日志，也能从外部【有 credentials 的情况下】很方便提取出记忆、经历过的事情等等哦。这样最坏的情况下可以 dump 磁盘镜像，复活、提取记忆、合并。在我们有超智能节点的情况下。
~~~

## U63 非阻塞 thought、时间环境感知与插件化

下列文字由协作席 `Agent: dot` 作为 2026-10-27 架构讨论中的用户原话提交（[PR84](https://github.com/cyjin-yl/shenko/pull/84)，原 head `2fa360b42444abbc715c81592be8a18f12e61cf3`）。集成席尚未能独立回读其原始会话，故逐字来源仅为协作席的报告：

> 不过 thought 层就不允许存在慢工具或者阻塞工具，所有工具都默认 bg 或者立刻返回拿结果，这是不是比较好可以绕过问题。需要主动调用语义 wait 才会等待。
>
> 可以加入。不过要考虑我们独有架构如何加入怎么加入。
>
> 我们的恢复其实是借鉴的 pi durable
>
> 主体如何感知外界时间 天气等等呢
>
> 还有相对时间感知 即使系统时间错乱 他也要能知道自己思考了多久
>
> 记得插件化
>
> OK 可以写入仓库设计了 顺便维护一下其他设计和 issue pr

对应 §5.6/§45.13；协作席将原始目标记录为用户裁决，控制面有界返回、计时域/未知间隔、插件职责与验收细分是据此形成的工程合同，不伪称用户指定了具体 Rust 类型或所有字段。恢复沿用 §10.6/§41 的 Pi Durable 参照。外部架构截图仅作为讨论启发，不据转述证明第三方源码实现；不复制或公开截图中的私人身份材料。

<!-- /migrated-contract: SPEC:33 -->
