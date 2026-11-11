# 设计评审采纳记录


<a id="spec-30"></a>
<!-- migrated-contract: SPEC:30 -->
# 30 DeepSeek 与 Gemini 建议的采纳记录

用户要求选择性吸收这两份建议，且已明确否定永久不可改的 Ring 0、用低频 Tick 替代永续思考，并澄清 50/50 应是自动算法在合适条件下得出的结果。本章保存裁决理由，避免后续实现把第三方建议误当成用户要求。建议来自本次对话粘贴内容及用户所给评审材料；材料中的数字、项目版本和论文解释均独立核验。

| 外部建议 | 裁决 | 本规格中的实际处理 |
| --- | --- | --- |
| 永久不可改 Ring 0；Agent 只能改 Ring 1 | 不采纳，违反已确认目标 | kernel、救援程序与 harness 均可替换；候选构建、状态迁移、健康验证与代际交接提供恢复性。保留上一可启动闭包不等于永久冻结某份代码。 |
| 主体无事时休眠，5 分钟→30 分钟→2 小时 Tick | 不采纳，违反永续思考 | 主体每步提交后继续申请下一轮；公平调度可令其等待，但不能以“省电”把产品默默改成定时 Bot。 |
| Rust 重新编译必然要求路由器本机安装 rustc/LLVM | 修正 | BuildProvider 可在其他自有节点交叉构建；目标机验证架构、闭包与启动结果。离线设备也可消费已验证预构建候选。 |
| Ring 0 带 WASM、SQLite、KV 与 Web 总体必然小于 10 MB | 无依据，不能列作研究事实 | 分别测量 kernel、runtime、默认插件、UI 和工具包；设备目标是待测门槛。可插拔实现与按需加载降低常驻负担。 |
| 用 Cordis effect 逆操作保证任意崩溃恢复 | 部分采纳生命周期思想，纠正保证范围 | effect 处理进程内清理；正式引入 Pi Durable 的任务状态、原子提交、持久取消/补偿和重放思想，见 [§10.6](../specs/durable-state.md#spec-10-6)，存储耐久等级和真实外部逆操作分别核验。 |
| 直接采用 wasi-vfs 作为可写 CAS Overlay | 修正 | 核验到的 wasi-vfs 是嵌入式只读文件系统组件，不能当成完成的可写 overlay；内容树、事务、块存储、宿主挂载需分别设计实现。 |
| LayerFS 仍是 0.1.7、完全依赖 Docker；“零依赖”裁剪天然可用 | 修正版本与组件边界 | 已核验 v1.0.0 source release；最新决定是在合格 Linux VM 中直接运行原版，不以重做/裁剪 LayerFS 为前置；WASM 状态独立设计并兑现自身持久合同。 |
| SQLite Merkle/CAS 自然带来 LWW 与零成本分支 | 不采纳推论 | 内容寻址不决定合并策略。默认报告冲突，明确声明的类型才可用 LWW；根引用 fork 很便宜不等于后续物化、提交、fsync 或 GC 没有成本。 |
| Git 存文本、CAS 指针存富媒体 | 采纳并补齐事务语义 | 区分 local committed 与 remote replicated，先保证对象闭包可用再发表 ref；GC 保留所有可达、迁移和恢复根。自定义指针与 Git LFS 规范分别标识。 |
| PMA 两阶段与选择性沉默 | 采纳结构，独立配置成本 | 先更新结构化状态，再决定提醒或不提醒；无提醒不等于没有模型成本，全部受统一 broker 约束。 |
| PMA 的 8 是触发间隔，selective 在所有指标远胜，代码天然 transient | 纠正 | 8 是观察窗口，主实验触发间隔为 1；消融指标不支持全面优势。公开 user_turn 路径会进入历史，本产品严格 transient 是额外合同。 |
| transient 用完即焚、历史也不记录 | 不采纳删除证据的解释 | 后续上下文默认不再注入，但实际注入、来源和请求 manifest 保存在原始 ledger；append-only backend 的重建代价如实计入。 |
| 每次 research reset 必须带 <200 tokens 失败墓碑 | 改为可选消融策略 | 论文的 best-only 与带失败笔记是不同实验；默认不把旧失败叙事自动带回。当前权限、已发生外部效果和已验证现实约束另行保留。 |
| NInfer 可在句子边界任意抢占、秒回且零 prefill | 纠正 | 真实暂停边界、admission、Snapshot/Replay 与容量不足路径各有条件；完成一个有限 generation 后轮转可提供基础活性，请求内抢占是额外能力。 |
| Strata 遇不同领域任务必须禁止 parallel | 不采纳绝对规则 | 专家驻留、batch 权重共享和额外 session 状态共同决定收益；按部署测量选择有效并发，不按主题标签拍定。 |
| 主体固定 15–20%，取消 50/50 | 不采纳固定比例，也不把 50/50 手动 profile 当达标 | 自动规划驻留与计算；均衡理想负载自行接近各半，负载改变则调整。所有主体 scope 合计、所有孩子合计分别归账。 |
| 所有角色强制完全一致 system prefix | 修正 | 仅共享真正相同的前缀；主体可为 child 任意编写 system prompt 并选不同模型，不能为了 cache 强迫完全同词。D1 统一个人知识不等于所有 child 必须加载所有资料。 |
| 27B 推理强世界知识弱已被 PMA 证明 | 不采纳泛化 | 将其视为待测模型画像；分别诊断知识、发现工具、参数、长程状态与量化/template 的影响。 |
| 生僻命令一律先拼接并执行 --help | 采纳知识前置目标，修改执行方式 | 先用准确版本的 docs/schema/recipe；确需执行 help，由 worker 使用受控参数调用。未知命令执行不能自动视为无副作用。 |
| 两级插件发现、正反例、成本字段、Use Auditor | 采纳并加版本与评测合同 | 教学/索引/binding 同版本；有限审计 worker 识别漏用误用。N 次成功或合法 JSON 不等于学会，使用自然任务和独立反例验收。 |
| VT100 画面提纯 | 采纳但不替代协议 | 首选 SDK/RPC；终端状态机还原画面、提供语义 diff 与来源。执行前核对当前进程、seat epoch、问题和 revision，防止回答已消失的旧问题。 |
| 按用户活跃渠道求助、固定 3 分钟后电话 | 采纳动态求助；不采纳固定升级规则 | 联系策略结合人格、任务期限、可达性、受众和已有授权；无已读回执不能推断未读。相同问题解决后取消其他升级。 |
| 声纹/写作习惯自动把两人合并，固定置信度 0.96 | 不采纳作为通用身份规则 | 稳定平台身份、候选关联与生物特征证据分离；可撤销、可纠错。联系人识别不自动产生管理员权限。 |
| 音频热流与后台摘要 | 采纳双路径，保持主体连续输入 | 原始流与带来源的语义片段、呼叫/状态事件分别处理；背景笔记不阻止主体持续思考和工具委派，不固定三分钟才让它知道现场。 |
| 竞争产品都要求用户在场 | 纠正 | 官方资料已描述 unattended/background；本产品差异需落实在本地所有权、任意层可改、WASM、资源治理与具体自主行为上。 |
| Proto-UI 用 caret 或每 release 更新即可 | 不采纳替代方案 | 每个开发任务核验并 bump 最新主线，锁定确切 commit；当前真实 adapter 版本为 alpha，compiler seam 保留。 |
| 按受众限制主体可见私聊内容 | 已由 D1 否定 | 主体统一认识全部经历，保密由人格/灵魂和自主判断决定；不硬编码亲密度，不把社会承诺变成永久 ACL。 |
| 所有节点任何时刻全局单主，分区必须停止一侧 | 按 D11 改为在线优先 | 正常选主，故障时允许显式串行分支；重连投票、停旧主体并合并真实记忆任务，物理并发和副作用继续如实记账。 |
| 把 snapcompact 叫进程/KV 快照或无损档案 | 不采纳 | 实际为本地有界文字转 PNG 的视觉上下文；原文与执行 ledger 独立保存，图像预算、准确率和 KV 代价须实测。 |

材料提出的产品名、额外“宪法模块”、内核逐次人工审批表和先裁掉媒体/REPL等阶段性范围，不自动成为本产品要求。当前无正式产品命名；“任何代码可改”与“当前有效用户合同仍需满足”同时成立。

本章新增事实的主要核验入口：[wasi-vfs README](https://github.com/kateinoigakukun/wasi-vfs/blob/0f4db4b64882db50ef8c04018e7e98bf59a63f92/README.md)、[LayerFS v1.0.0](https://github.com/Ephemeral-AI-Lab/layerfs/releases/tag/v1.0.0)、[Nix 远程构建](https://nix.dev/manual/nix/2.35/advanced-topics/distributed-builds)、[SQLite 原子提交](https://www.sqlite.org/atomiccommit.html)、[PMA 论文](https://arxiv.org/html/2607.08716v1)、[PMA 固定代码](https://github.com/yifannnwu/proactive-memory-agent/tree/89e5c0d6aadfe531a1aee42fd290d48be89973dd)、[NInfer 调度原文](https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/maintainer/resource-scheduling-and-context-cache.md)、[Strata batching 原文](https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/BATCHING.md)。其他对应依据见 [§3](product-positioning.md#spec-3)、[§14](../specs/research-memory.md#spec-14)、[§17](../specs/remote-seats.md#spec-17)–[§21](../specs/web-interface.md#spec-21)。

<!-- /migrated-contract: SPEC:30 -->
