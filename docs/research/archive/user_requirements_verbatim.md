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
