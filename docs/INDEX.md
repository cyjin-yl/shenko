# Shenko 文档索引

## 权威关系

最新明确用户决定控制目标；[领域合同](specs/architecture.md)定义行为及其 Why，当前源码和真实运行证据说明实现状态。根 `SPEC.md`、`WHY.md` 已退役为导航，不能作为内容来源。实现 Agent 从本索引进入具体文档，引用实际标题/锚点，不再写“完整合同见 SPEC §…”。

[迁移决定](decisions/0005-domain-contract-migration-2026-11-11.md)说明保全与边界；[机器映射](contract-migration.json)连接旧章节和现行目标。原编号仅为稳定追踪标识，不要求读者往返单体规格。历史归档不是当前指令。

## 当前入口

- [AGENTS](../AGENTS.md) 与 [shenko-dev](../.agents/skills/shenko-dev/SKILL.md)：开发哲学与协作。
- [实现地图](IMPLEMENTATION_DOCS_MAP.md)：调用点与实现缺口，不冒充运行验收。
- [完整交付合同](governance/COMPLETION.md)：正式范围及证据等级。
- [需求索引](requirements/README.md)：165 项需求、63 组行为、59 条来源、236 条具名验收。
- [用户原话](requirements/user-sources.md)、[裁决记录](decisions/product-rulings.md)、[追踪矩阵](requirements/traceability.md)、[设计来源](requirements/design-provenance.md)。
- [覆盖账本](state/coverage.json)与[运行证据](evidence/)：候选及验证状态。
- [历史归档](history/)：不可用旧方法覆盖新的明确决定。

## 领域合同

30 份现有领域合同保留原所有权；其中的规范和理由由该领域共同维护。

| 领域 | 拥有者 | 内容入口 |
| --- | --- | --- |
| [architecture](specs/architecture.md) | parent | 分域规范、接口、Why、错误与恢复 |
| [browser](specs/browser.md) | ArchBrowser | 分域规范、接口、Why、错误与恢复 |
| [channel-adapters](specs/channel-adapters.md) | ArchOtherChannels | 分域规范、接口、Why、错误与恢复 |
| [channels](specs/channels.md) | ArchChannels | 分域规范、接口、Why、错误与恢复 |
| [cognition-units](specs/cognition-units.md) | ArchCognitionUnits | 分域规范、接口、Why、错误与恢复 |
| [configuration-playground](specs/configuration-playground.md) | ArchPlayground | 分域规范、接口、Why、错误与恢复 |
| [contacts](specs/contacts.md) | ArchContacts | 分域规范、接口、Why、错误与恢复 |
| [context-policies](specs/context-policies.md) | ArchContextPolicies | 分域规范、接口、Why、错误与恢复 |
| [desktop-runtime](specs/desktop-runtime.md) | ArchDesktop | 分域规范、接口、Why、错误与恢复 |
| [documentation](specs/documentation.md) | ArchDocs | 分域规范、接口、Why、错误与恢复 |
| [durable-state](specs/durable-state.md) | ArchLedger | 分域规范、接口、Why、错误与恢复 |
| [ecosystem-adapters](specs/ecosystem-adapters.md) | ArchEcosystem | 分域规范、接口、Why、错误与恢复 |
| [inference](specs/inference.md) | ArchBroker | 分域规范、接口、Why、错误与恢复 |
| [media](specs/media.md) | ArchMedia | 分域规范、接口、Why、错误与恢复 |
| [memory](specs/memory.md) | ArchMemory | 分域规范、接口、Why、错误与恢复 |
| [mobile-runtime](specs/mobile-runtime.md) | ArchMobile | 分域规范、接口、Why、错误与恢复 |
| [model-children](specs/model-children.md) | ArchModelChildren | 分域规范、接口、Why、错误与恢复 |
| [network-observation](specs/network-observation.md) | ArchNetwork | 分域规范、接口、Why、错误与恢复 |
| [nodes](specs/nodes.md) | ArchNodes | 分域规范、接口、Why、错误与恢复 |
| [plugins](specs/plugins.md) | ArchPluginRuntime | 分域规范、接口、Why、错误与恢复 |
| [quota-resources](specs/quota-resources.md) | ArchQuota | 分域规范、接口、Why、错误与恢复 |
| [remote-seats](specs/remote-seats.md) | ArchRemoteSeats | 分域规范、接口、Why、错误与恢复 |
| [repl](specs/repl.md) | ArchRepl | 分域规范、接口、Why、错误与恢复 |
| [research-memory](specs/research-memory.md) | ArchResearch | 分域规范、接口、Why、错误与恢复 |
| [self-update](specs/self-update.md) | ArchUpgrade | 分域规范、接口、Why、错误与恢复 |
| [subject-runtime](specs/subject-runtime.md) | ArchSubject | 分域规范、接口、Why、错误与恢复 |
| [verification](specs/verification.md) | ArchEvidence | 分域规范、接口、Why、错误与恢复 |
| [wasm-home](specs/wasm-home.md) | ArchWasmHome | 分域规范、接口、Why、错误与恢复 |
| [wasm-tools](specs/wasm-tools.md) | ArchWasmTools | 分域规范、接口、Why、错误与恢复 |
| [web-interface](specs/web-interface.md) | ArchWeb | 分域规范、接口、Why、错误与恢复 |

## Why 按领域阅读

- [为什么主体需要持续存在](specs/subject-runtime.md#why-1)
- [为什么只有一条主思考流，却鼓励很多孩子](specs/model-children.md#why-2)
- [为什么核心小，而且任何部分都能改](specs/self-update.md#why-3)
- [为什么坚持 WASM 家园，又接入真实操作系统](specs/wasm-home.md#why-4)
- [为什么记忆必须能回到原始事实](specs/memory.md#why-5)
- [为什么 awareness、感知、Skill 与代码要分别安装](specs/cognition-units.md#why-6)
- [为什么多节点仍然是同一自我](specs/nodes.md#why-7)
- [为什么推理后端要真实透传](specs/inference.md#why-8)
- [为什么第一版就要能继续开发自己](specs/self-update.md#why-9)
- [为什么界面从一个主体开始](specs/web-interface.md#why-10)
- [为什么开发规范采用高自主、强证据](specs/documentation.md#why-11)
- [为什么这些规矩也能演进](specs/documentation.md#why-12)

## 章节迁移导航

以下每个入口直接指向当前领域正文；子节分属不同领域时分别列出。完整源行区间与哈希见机器映射。

| 原章节标识 | 现行具体文档 |
| --- | --- |
| §1 | [1 文档状态与产品定义](specs/architecture.md#spec-1) |
| §2 | [2 已确认的产品约束](specs/architecture.md#spec-2) |
| §3 | [3 研究结论与竞品定位](research/product-positioning.md#spec-3) |
| §3.1 | [3.1 具名竞品](research/product-positioning.md#spec-3-1) |
| §3.2 | [3.2 Harness 与记忆系统](research/product-positioning.md#spec-3-2) |
| §3.3 | [3.3 四个会影响方案的事实纠正](research/product-positioning.md#spec-3-3) |
| §4 | [4 核心产品场景](specs/subject-runtime.md#spec-4) |
| §5 | [5 单线程永续主体与子 Agent](specs/subject-runtime.md#spec-5) |
| §5.1 | [5.1 唯一主体的正式语义](specs/subject-runtime.md#spec-5-1) |
| §5.2 | [5.2 主体能力表](specs/subject-runtime.md#spec-5-2) |
| §5.3 | [5.3 委派与对话](specs/model-children.md#spec-5-3) |
| §5.4 | [5.4 崩溃恢复与唯一性](specs/nodes.md#spec-5-4) |
| §5.4.1 | [5.4.1 选举控制与真实经历分别保存](specs/nodes.md#spec-5-4-1) |
| §5.4.2 | [5.4.2 重连后只留下一个主体](specs/nodes.md#spec-5-4-2) |
| §5.4.3 | [5.4.3 资源与副作用不随选举清零](specs/nodes.md#spec-5-4-3) |
| §5.4.4 | [5.4.4 统一 Node、共享认知与节点能力](specs/nodes.md#spec-5-4-4) |
| §5.5 | [5.5 共享能力全集与主体—child 双向协作](specs/model-children.md#spec-5-5) |
| §5.6 | [5.6 非阻塞 thought 接口与事实投影](specs/subject-runtime.md#spec-5-6) |
| §6 | [6 人格、可自改灵魂与统一知识](specs/subject-runtime.md#spec-6) |
| §7 | [7 系统架构与故障边界](specs/plugins.md#spec-7) |
| §8 | [8 WASM 家园与工具兼容](specs/wasm-home.md#spec-8) |
| §8.1 | [8.1 Runtime 选择与兼容等级](specs/wasm-home.md#spec-8-1) |
| §8.2 | [8.2 工具清单与真实差距](specs/wasm-tools.md#spec-8-2) |
| §8.3 | [8.3 执行和挂载合同](specs/wasm-home.md#spec-8-3) |
| §9 | [9 Nix-like 包与自我改进](specs/self-update.md#spec-9) |
| §9.1 | [9.1 任何部分都可自主重写](specs/self-update.md#spec-9-1) |
| §9.2 | [9.2 版本交接与行为合同](specs/self-update.md#spec-9-2) |
| §9.3 | [9.3 构建位置与生命周期清理](specs/self-update.md#spec-9-3) |
| §9.4 | [9.4 自主更新策略与联网配置](specs/network-observation.md#spec-9-4) |
| §9.5 | [9.5 磁盘优先的 breaking-change 迁移插件](specs/self-update.md#spec-9-5) |
| §10 | [10 文件版本与恢复](specs/durable-state.md#spec-10) |
| §10.1 | [10.1 三个状态域](specs/durable-state.md#spec-10-1) |
| §10.2 | [10.2 二进制友好的版本层](specs/durable-state.md#spec-10-2) |
| §10.2.1 | [10.2.1 LayerFS 原版运行于真实 Linux VM](specs/durable-state.md#spec-10-2-1) |
| §10.3 | [10.3 默认 Git 记忆和私有仓库](specs/memory.md#spec-10-3) |
| §10.4 | [10.4 恢复流程](specs/durable-state.md#spec-10-4) |
| §10.5 | [10.5 二进制引用、提交和回收协议](specs/memory.md#spec-10-5) |
| §10.6 | [10.6 参考 Pi Durable 的持久执行协议](specs/durable-state.md#spec-10-6) |
| §11 | [11 SSH 身份与远端工具](specs/remote-seats.md#spec-11) |
| §12 | [12 会话真相源和记忆](specs/memory.md#spec-12) |
| §12.1 | [12.1 数据模型与来源](specs/memory.md#spec-12-1) |
| §12.2 | [12.2 记忆请求与提交](specs/memory.md#spec-12-2) |
| §12.3 | [12.3 历史切窗和中文检索](specs/context-policies.md#spec-12-3) |
| §12.3.1 | [12.3.1 Budget compact、notes-backed windows 与 snapcompact](specs/context-policies.md#spec-12-3-1) |
| §12.3.2 | [12.3.2 切窗事务、未完成工具和 KV 代价](specs/context-policies.md#spec-12-3-2) |
| §12.4 | [12.4 遗忘、保留和隐私](specs/memory.md#spec-12-4) |
| §13 | [13 推理调度与 KV 池](specs/inference.md#spec-13) |
| §13.1 | [13.1 三种资源必须分开](specs/quota-resources.md#spec-13-1) |
| §13.2 | [13.2 自动分配中的半池情形](specs/quota-resources.md#spec-13-2) |
| §13.3 | [13.3 自动分配与主体调控](specs/quota-resources.md#spec-13-3) |
| §13.4 | [13.4 计算顺序与无死锁规则](specs/quota-resources.md#spec-13-4) |
| §13.5 | [13.5 KV 句柄与上下文稳定性](specs/inference.md#spec-13-5) |
| §13.6 | [13.6 Backend 能力矩阵](specs/inference.md#spec-13-6) |
| §13.7 | [13.7 原生 API、能力协商与真实透传](specs/inference.md#spec-13-7) |
| §13.7.1 | [13.7.1 四个后端的适配重点](specs/inference.md#spec-13-7-1) |
| §13.7.2 | [13.7.2 传输差异必须可见](specs/inference.md#spec-13-7-2) |
| §13.7.3 | [13.7.3 资源域、轮转与缓存承诺](specs/inference.md#spec-13-7-3) |
| §13.8 | [13.8 外部打断、主体决定免打扰与过滤修复](specs/subject-runtime.md#spec-13-8) |
| §14 | [14 递归研究与选择性记忆](specs/research-memory.md#spec-14) |
| §14.1 | [14.1 两篇新增研究的含义](specs/research-memory.md#spec-14-1) |
| §14.2 | [14.2 四种状态不同处理](specs/research-memory.md#spec-14-2) |
| §14.3 | [14.3 Research reset 合同](specs/research-memory.md#spec-14-3) |
| §14.4 | [14.4 主动记忆 worker](specs/research-memory.md#spec-14-4) |
| §14.5 | [14.5 递归收敛](specs/model-children.md#spec-14-5) |
| §14.6 | [14.6 可无限延续的自主兴趣](specs/research-memory.md#spec-14-6) |
| §15 | [15 消息注入与认识人的能力](specs/channels.md#spec-15) |
| §15.1 | [15.1 消息封套](specs/channels.md#spec-15-1) |
| §15.2 | [15.2 联系人与声纹插件](specs/contacts.md#spec-15-2) |
| §15.2.1 | [15.2.1 复用 Life 的已有身份材料](specs/contacts.md#spec-15-2-1) |
| §15.3 | [15.3 消息插件合同](specs/channel-adapters.md#spec-15-3) |
| §16 | [16 中国平台与开放对话](specs/channel-adapters.md#spec-16) |
| §17 | [17 远程 Agent 遥控插件](specs/remote-seats.md#spec-17) |
| §17.1 | [17.1 任务与 seat 的连续性](specs/remote-seats.md#spec-17-1) |
| §17.2 | [17.2 自主处理问题与许可](specs/remote-seats.md#spec-17-2) |
| §17.3 | [17.3 主动寻找人类](specs/remote-seats.md#spec-17-3) |
| §17.4 | [17.4 远端推理资源](specs/remote-seats.md#spec-17-4) |
| §17.5 | [17.5 终端观察与语义整理](specs/remote-seats.md#spec-17-5) |
| §18 | [18 插件能被发现并正确使用](specs/plugins.md#spec-18) |
| §18.1 | [18.1 原生插件优先和生态兼容](specs/plugins.md#spec-18-1) |
| §18.2 | [18.2 可使用性包](specs/plugins.md#spec-18-2) |
| §18.3 | [18.3 相关研究与设计判断](specs/plugins.md#spec-18-3) |
| §18.4 | [18.4 27B 本地模型的优化](specs/cognition-units.md#spec-18-4) |
| §18.5 | [18.5 评估与持续改善](specs/cognition-units.md#spec-18-5) |
| §18.6 | [18.6 即时工具知识与使用审计](specs/plugins.md#spec-18-6) |
| §18.7 | [18.7 能力缺口发现与自主补全](specs/cognition-units.md#spec-18-7) |
| §18.8 | [18.8 WASM 能力自主移植](specs/wasm-tools.md#spec-18-8) |
| §19 | [19 有状态 REPL 插件](specs/repl.md#spec-19) |
| §19.1 | [19.1 多 kernel 与统一会话生命周期](specs/repl.md#spec-19-1) |
| §20 | [20 实时通话与课堂旁听扩展](specs/media.md#spec-20) |
| §21 | [21 Web UI 与设计系统](specs/web-interface.md#spec-21) |
| §21.1 | [21.1 体验目标与功能要求：交由 Claude Opus 设计实现](specs/web-interface.md#spec-21-1) |
| §21.2 | [21.2 Proto-UI 主线和 compiler seam](specs/web-interface.md#spec-21-2) |
| §21.3 | [21.3 UI 本身也是插件](specs/web-interface.md#spec-21-3) |
| §21.4 | [21.4 Dynamic Response 与官方参考边界](specs/web-interface.md#spec-21-4) |
| §21.5 | [21.5 目标能力协商](specs/web-interface.md#spec-21-5) |
| §21.6 | [21.6 DSL 与 UI 工具两条入口](specs/web-interface.md#spec-21-6) |
| §21.7 | [21.7 跨界面降级](specs/web-interface.md#spec-21-7) |
| §21.8 | [21.8 状态与动作闭环](specs/web-interface.md#spec-21-8) |
| §21.9 | [21.9 流式呈现与 compiler 迁移](specs/web-interface.md#spec-21-9) |
| §21.10 | [21.10 Magpie 视觉参照与比较方法](specs/web-interface.md#spec-21-10) |
| §21.11 | [21.11 流式输出的阅读稳定性：Magpie 与 Alma 参照](specs/web-interface.md#spec-21-11) |
| §21.12 | [21.12 默认 Web 界面：与主体直接交互的一条时间线](specs/web-interface.md#spec-21-12) |
| §22 | [22 部署和资源目标](specs/nodes.md#spec-22) |
| §22.1 | [22.1 首个真实评测基线：用户的 Qwen3.8-27B，经 Tailnet 调用](specs/nodes.md#spec-22-1) |
| §22.2 | [22.2 无模型时的日志、外部提取与离线复活](specs/nodes.md#spec-22-2) |
| §23 | [23 验收与发布阻断项](specs/verification.md#spec-23) |
| §24 | [24 Computer Use 与完整操作系统节点](specs/desktop-runtime.md#spec-24) |
| §24.1 | [24.1 正式能力及插件边界](specs/desktop-runtime.md#spec-24-1) |
| §24.2 | [24.2 操作系统边界](specs/desktop-runtime.md#spec-24-2) |
| §24.3 | [24.3 观察版本和桌面输入所有权](specs/desktop-runtime.md#spec-24-3) |
| §24.4 | [24.4 GUI 副作用和资源](specs/desktop-runtime.md#spec-24-4) |
| §24.5 | [24.5 移动 OS 感知与执行均为插件，APK 是发行组合](specs/mobile-runtime.md#spec-24-5) |
| §24.5.1 | [24.5.1 自有应用 SQLite 观察与自主风险判断](specs/mobile-runtime.md#spec-24-5-1) |
| §24.6 | [24.6 原生浏览器能力、持久身份与人类接管](specs/browser.md#spec-24-6) |
| §24.6.1 | [24.6.1 身份、运行会话和真实持久化范围](specs/browser.md#spec-24-6-1) |
| §24.6.2 | [24.6.2 virtual-headed 默认与同会话接管](specs/browser.md#spec-24-6-2) |
| §24.6.3 | [24.6.3 崩溃恢复与待验收行为](specs/browser.md#spec-24-6-3) |
| §25 | [25 NInfer 接入教程 中文与 English](specs/inference.md#spec-25) |
| §25.1 | [25.1 中文操作路径](specs/inference.md#spec-25-1) |
| §25.2 | [25.2 English walkthrough](specs/inference.md#spec-25-2) |
| §26 | [26 Strata 接入教程 中文与 English](specs/inference.md#spec-26) |
| §26.1 | [26.1 中文操作路径](specs/inference.md#spec-26-1) |
| §26.2 | [26.2 English walkthrough](specs/inference.md#spec-26-2) |
| §27 | [27 共同排障与接入验收 Common troubleshooting](specs/verification.md#spec-27) |
| §28 | [28 规格裁决记录与仍未决的问题](decisions/product-rulings.md#spec-28) |
| §29 | [29 交付路线与阶段验收](specs/verification.md#spec-29) |
| §29.1 | [29.1 文档与工程维护规则](specs/verification.md#spec-29-1) |
| §29.2 | [29.2 本轮研究组织与完成状态](specs/verification.md#spec-29-2) |
| §30 | [30 DeepSeek 与 Gemini 建议的采纳记录](research/design-review-adoption.md#spec-30) |
| §31 | [31 研究版本清单与证据使用规则](requirements/research-sources.md#spec-31) |
| §32 | [32 需求追踪矩阵](requirements/traceability.md#spec-32) |
| §33 | [33 用户需求原文与追加澄清](requirements/user-sources.md#spec-33) |
| §34 | [34 BoxLite：可选完整 Linux Runtime 插件评估](specs/desktop-runtime.md#spec-34) |
| §34.1 | [34.1 结论与研究基线](specs/desktop-runtime.md#spec-34-1) |
| §34.2 | [34.2 架构位置与插件划分](specs/desktop-runtime.md#spec-34-2) |
| §34.3 | [34.3 持久化、快照与任意挂载的真实边界](specs/desktop-runtime.md#spec-34-3) |
| §34.4 | [34.4 Computer Use、远端 Agent 与 REPL](specs/desktop-runtime.md#spec-34-4) |
| §34.5 | [34.5 自主联网、代理与升级](specs/desktop-runtime.md#spec-34-5) |
| §34.6 | [34.6 设备、体积与资源核算](specs/desktop-runtime.md#spec-34-6) |
| §34.7 | [34.7 条件验收与采用决定](specs/desktop-runtime.md#spec-34-7) |
| §35 | [35 首版开发者发行：可继续开发自身的完整闭环](specs/self-update.md#spec-35) |
| §35.1 | [35.1 首版目标与既有约束](specs/self-update.md#spec-35-1) |
| §35.2 | [35.2 开发者机器与 WASM 家园的关系](specs/wasm-home.md#spec-35-2) |
| §35.3 | [35.3 首版默认插件闭包](specs/plugins.md#spec-35-3) |
| §35.4 | [35.4 一次真实自开发任务](specs/self-update.md#spec-35-4) |
| §35.5 | [35.5 三条 Seat 路径，各自兑现能力](specs/remote-seats.md#spec-35-5) |
| §35.6 | [35.6 NapCat 接入和升级期间的消息连续性](specs/channels.md#spec-35-6) |
| §35.7 | [35.7 自升级必须完成运行中的交接](specs/self-update.md#spec-35-7) |
| §35.8 | [35.8 能力缺口必须能驱动实际开发](specs/self-update.md#spec-35-8) |
| §35.9 | [35.9 全局推理与中断](specs/inference.md#spec-35-9) |
| §35.10 | [35.10 首版自动验收](specs/self-update.md#spec-35-10) |
| §36 | [36 插件的副作用、重置与恢复边界](specs/durable-state.md#spec-36) |
| §36.1 | [36.1 正式合同与适用范围](specs/durable-state.md#spec-36-1) |
| §36.2 | [36.2 五种动作不能共用一个 rollback 标签](specs/durable-state.md#spec-36-2) |
| §36.3 | [36.3 静态声明按 operation 细分](specs/durable-state.md#spec-36-3) |
| §36.4 | [36.4 调用时解析实际恢复边界](specs/durable-state.md#spec-36-4) |
| §36.5 | [36.5 一次操作的持久记录](specs/durable-state.md#spec-36-5) |
| §36.6 | [36.6 回退计划与跨版本恢复](specs/durable-state.md#spec-36-6) |
| §36.7 | [36.7 模型、工具与界面如何看到边界](specs/durable-state.md#spec-36-7) |
| §36.8 | [36.8 首版恢复合同验收](specs/durable-state.md#spec-36-8) |
| §37 | [37 cordis-rs 主干上的注入时序与 API 兼容](specs/context-policies.md#spec-37) |
| §37.1 | [37.1 本轮裁决与参考范围](specs/context-policies.md#spec-37-1) |
| §37.2 | [37.2 四种顺序分别表达](specs/context-policies.md#spec-37-2) |
| §37.3 | [37.3 外部事件先持久化，再在可用边界注入](specs/context-policies.md#spec-37-3) |
| §37.4 | [37.4 Hook 最小描述](specs/context-policies.md#spec-37-4) |
| §37.5 | [37.5 Phase、相对顺序与冲突](specs/context-policies.md#spec-37-5) |
| §37.6 | [37.6 首版建议的语义扩展点](specs/context-policies.md#spec-37-6) |
| §37.7 | [37.7 Prompt 布局、工具正确性与 KV](specs/context-policies.md#spec-37-7) |
| §37.8 | [37.8 异步、有限等待与 C=1](specs/context-policies.md#spec-37-8) |
| §37.9 | [37.9 新增 Hook、兼容与自修改](specs/context-policies.md#spec-37-9) |
| §37.10 | [37.10 激活、重放与审计](specs/context-policies.md#spec-37-10) |
| §37.11 | [37.11 首版验收](specs/context-policies.md#spec-37-11) |
| §38 | [38 内置、可替换的自解释插件](specs/cognition-units.md#spec-38) |
| §38.1 | [38.1 产品能力与角色边界](specs/cognition-units.md#spec-38-1) |
| §38.2 | [38.2 OMP 参考与本产品增加的能力](specs/cognition-units.md#spec-38-2) |
| §38.3 | [38.3 四类自身资料与版本身份](specs/cognition-units.md#spec-38-3) |
| §38.4 | [38.4 索引与部署体积](specs/cognition-units.md#spec-38-4) |
| §38.5 | [38.5 何时主动使用](specs/cognition-units.md#spec-38-5) |
| §38.6 | [38.6 回答、开发与更新的闭环](specs/cognition-units.md#spec-38-6) |
| §38.7 | [38.7 首版验收](specs/cognition-units.md#spec-38-7) |
| §39 | [39 设计来源与上游行为完整度](requirements/design-provenance.md#spec-39) |
| §39.1 | [39.1 所有设计均有来源和比较范围](requirements/design-provenance.md#spec-39-1) |
| §39.2 | [39.2 来源登记](requirements/design-provenance.md#spec-39-2) |
| §39.3 | [39.3 已枚举的行为对照](requirements/design-provenance.md#spec-39-3) |
| §39.4 | [39.4 较细的 memory 对照种子](requirements/design-provenance.md#spec-39-4) |
| §39.5 | [39.5 完整度怎样比较](requirements/design-provenance.md#spec-39-5) |
| §39.6 | [39.6 维护与自解释](requirements/design-provenance.md#spec-39-6) |
| §40 | [40 开发 Agent 自主工作规范](governance/development-contract.md#spec-40) |
| §40.1 | [40.1 默认连续完成已授权目标](governance/development-contract.md#spec-40-1) |
| §40.2 | [40.2 实现选择与未决问题](governance/development-contract.md#spec-40-2) |
| §40.3 | [40.3 交付完整行为](governance/development-contract.md#spec-40-3) |
| §40.4 | [40.4 工程与插件约定](governance/development-contract.md#spec-40-4) |
| §40.5 | [40.5 自动验证与相称成本](governance/development-contract.md#spec-40-5) |
| §40.6 | [40.6 自主 UI 实现与视觉验收](governance/development-contract.md#spec-40-6) |
| §40.7 | [40.7 文档与任务接续](governance/development-contract.md#spec-40-7) |
| §40.8 | [40.8 规范验收](governance/development-contract.md#spec-40-8) |
| §40.9 | [40.9 先表达目的与理由](governance/development-contract.md#spec-40-9) |
| §41 | [41 首版规范执行合同](specs/architecture.md#spec-41) |
| §41.1 | [41.1 本章的地位、范围与阅读方法](specs/verification.md#spec-41-1) |
| §41.1.1 | [41.1.1 定义写完的判据](specs/verification.md#spec-41-1-1) |
| §41.2 | [41.2 默认持久存储与观察边界](specs/durable-state.md#spec-41-2) |
| §41.2.1 | [41.2.1 本地写入域与 SQLite profile](specs/durable-state.md#spec-41-2-1) |
| §41.2.2 | [41.2.2 对象与元数据的提交顺序](specs/durable-state.md#spec-41-2-2) |
| §41.2.3 | [41.2.3 提交失败、发布与重开](specs/durable-state.md#spec-41-2-3) |
| §41.3 | [41.3 共用身份、类型与调用上下文](specs/durable-state.md#spec-41-3) |
| §41.4 | [41.4 任务、效果与恢复的规范记录](specs/durable-state.md#spec-41-4) |
| §41.5 | [41.5 永续主体的一步与有限子任务](specs/subject-runtime.md#spec-41-5) |
| §41.6 | [41.6 外部事件、注意力与输入放置](specs/channels.md#spec-41-6) |
| §41.7 | [41.7 插件操作接口与实际效果绑定](specs/plugins.md#spec-41-7) |
| §41.7.1 | [41.7.1 静态合同与本次实际绑定](specs/plugins.md#spec-41-7-1) |
| §41.7.2 | [41.7.2 调用接口与幂等身份](specs/plugins.md#spec-41-7-2) |
| §41.7.3 | [41.7.3 状态、回执与先后顺序](specs/plugins.md#spec-41-7-3) |
| §41.8 | [41.8 Hook、上下文与请求封口](specs/context-policies.md#spec-41-8) |
| §41.8.1 | [41.8.1 扩展点、处理器与图](specs/context-policies.md#spec-41-8-1) |
| §41.8.2 | [41.8.2 各类输出的合成规则](specs/context-policies.md#spec-41-8-2) |
| §41.8.3 | [41.8.3 RequestSeal](specs/context-policies.md#spec-41-8-3) |
| §41.8.4 | [41.8.4 hook 恢复与热更新](specs/context-policies.md#spec-41-8-4) |
| §41.9 | [41.9 推理请求、计算许可与单并发](specs/inference.md#spec-41-9) |
| §41.10 | [41.10 KV 与原生后端能力](specs/inference.md#spec-41-10) |
| §41.11 | [41.11 远端 Seat 问题、决定与答复](specs/remote-seats.md#spec-41-11) |
| §41.12 | [41.12 自主升级与跨代交接](specs/self-update.md#spec-41-12) |
| §41.13 | [41.13 自解释的版本锚点与读取接口](specs/cognition-units.md#spec-41-13) |
| §41.14 | [41.14 结构化错误与恢复动作](specs/durable-state.md#spec-41-14) |
| §41.15 | [41.15 QQ 到远端开发再到自身升级的完整轨迹](specs/self-update.md#spec-41-15) |
| §41.16 | [41.16 规范故障与一致性验收](specs/verification.md#spec-41-16) |
| §41.16.1 | [41.16.1 提交与代际故障](specs/verification.md#spec-41-16-1) |
| §41.16.2 | [41.16.2 主体、队列、后端与 Seat](specs/verification.md#spec-41-16-2) |
| §41.16.3 | [41.16.3 插件与版本交错](specs/verification.md#spec-41-16-3) |
| §41.17 | [41.17 精确上游锚点与本产品差异](requirements/design-provenance.md#spec-41-17) |
| §41.18 | [41.18 WebInputSubmission 规范接口](specs/web-interface.md#spec-41-18) |
| §42 | [42 Provider 周期额度、模型选择与持续思考的自适应调优](specs/quota-resources.md#spec-42) |
| §42.1 | [42.1 产品目标与边界](specs/quota-resources.md#spec-42-1) |
| §42.2 | [42.2 已核验的 Anthropic 观测面](specs/quota-resources.md#spec-42-2) |
| §42.3 | [42.3 规范类型](specs/quota-resources.md#spec-42-3) |
| §42.4 | [42.4 观测、归因与账本边界](specs/quota-resources.md#spec-42-4) |
| §42.5 | [42.5 自适应算法与准入节奏的最低合同](specs/quota-resources.md#spec-42-5) |
| §42.6 | [42.6 与事件、RequestSeal、KV 和执行调度的衔接](specs/quota-resources.md#spec-42-6) |
| §42.7 | [42.7 验收场景](specs/quota-resources.md#spec-42-7) |
| §42.8 | [42.8 来源、采用范围与证据边界](specs/quota-resources.md#spec-42-8) |
| §43 | [43 对话配置与持久 Playground](specs/configuration-playground.md#spec-43) |
| §43.1 | [43.1 所有配置可通过对话管理](specs/configuration-playground.md#spec-43-1) |
| §43.2 | [43.2 Playground 是长期存在的全栈作品空间](specs/configuration-playground.md#spec-43-2) |
| §43.3 | [43.3 本轮验收增量](specs/configuration-playground.md#spec-43-3) |
| §44 | [44 首次启动与种子发行](specs/plugins.md#spec-44) |
| §44.1 | [44.1 第一次打开会发生什么](specs/plugins.md#spec-44-1) |
| §44.2 | [44.2 无 LLM 时种子发行必须已经具备的闭包](specs/plugins.md#spec-44-2) |
| §44.3 | [44.3 真正必填、自动推导与后续发展](specs/plugins.md#spec-44-3) |
| §44.4 | [44.4 首位 owner/admin 与普通聊天](specs/plugins.md#spec-44-4) |
| §44.5 | [44.5 Bootstrap 记录与接口](specs/plugins.md#spec-44-5) |
| §44.6 | [44.6 首个 Genesis、根和唯一第一步](specs/plugins.md#spec-44-6) |
| §44.7 | [44.7 可独立安装的 awareness 与执行基元](specs/plugins.md#spec-44-7) |
| §44.8 | [44.8 NapCat 与 tmux 首版可选接入](specs/plugins.md#spec-44-8) |
| §44.9 | [44.9 错误合同与必要验收](specs/plugins.md#spec-44-9) |
| §45 | [45 可独立安装的 Awareness、感知与依赖装配](specs/cognition-units.md#spec-45) |
| §45.1 | [45.1 定义与独立交付单元](specs/cognition-units.md#spec-45-1) |
| §45.2 | [45.2 单元内容与最小包形式](specs/cognition-units.md#spec-45-2) |
| §45.3 | [45.3 具体单元示例](specs/cognition-units.md#spec-45-3) |
| §45.4 | [45.4 独立安装、更新与共享目录](specs/cognition-units.md#spec-45-4) |
| §45.5 | [45.5 渐进披露与正确时机](specs/cognition-units.md#spec-45-5) |
| §45.6 | [45.6 与 Skill、执行插件和策略的组合](specs/cognition-units.md#spec-45-6) |
| §45.7 | [45.7 持续经历与实现生命周期](specs/cognition-units.md#spec-45-7) |
| §45.8 | [45.8 配置直觉与固定 phase 的删除](specs/cognition-units.md#spec-45-8) |
| §45.9 | [45.9 最低验收](specs/cognition-units.md#spec-45-9) |
| §45.10 | [45.10 可安装单元与依赖装配](specs/cognition-units.md#spec-45-10) |
| §45.10.1 | [45.10.1 独立单元及其职责](specs/cognition-units.md#spec-45-10-1) |
| §45.10.2 | [45.10.2 一份依赖图，两条独立的依赖属性](specs/cognition-units.md#spec-45-10-2) |
| §45.10.3 | [45.10.3 Provider 自动安装依赖闭包](specs/cognition-units.md#spec-45-10-3) |
| §45.10.4 | [45.10.4 QQ 断连的实际绑定](specs/cognition-units.md#spec-45-10-4) |
| §45.10.5 | [45.10.5 更新、卸载与反向依赖](specs/cognition-units.md#spec-45-10-5) |
| §45.10.6 | [45.10.6 验收增量](specs/cognition-units.md#spec-45-10-6) |
| §45.11 | [45.11 来源与研究边界](specs/cognition-units.md#spec-45-11) |
| §45.12 | [45.12 个性化适配、反思与自我推翻的插件合同](specs/memory.md#spec-45-12) |
| §45.12.1 | [45.12.1 可判定结果与剩余工作](specs/memory.md#spec-45-12-1) |
| §45.13 | [45.13 可替换的时间与环境感知](specs/cognition-units.md#spec-45-13) |
| §45.14 | [45.14 运行时错误事实与主体自主处置](specs/cognition-units.md#spec-45-14) |
| §46 | [46 完整产品的自主开发与仓库交接](governance/COMPLETION.md#spec-46) |
| §46.1 | [46.1 文档与自主权](governance/COMPLETION.md#spec-46-1) |
| §46.2 | [46.2 完整目标与 PR](governance/COMPLETION.md#spec-46-2) |
| §46.3 | [46.3 持久工作与验证](governance/COMPLETION.md#spec-46-3) |
| §46.4 | [46.4 GitHub 与启动](governance/COMPLETION.md#spec-46-4) |
| §46.5 | [46.5 完成证明](governance/COMPLETION.md#spec-46-5) |
| §46.6 | [46.6 交接验收](governance/COMPLETION.md#spec-46-6) |
| §47 | [47 Provider 协议版本、SDK 演进与自主迁移](specs/inference.md#spec-47) |
| §47.1 | [47.1 Why 与适用范围](specs/inference.md#spec-47-1) |
| §47.2 | [47.2 核验基线](specs/inference.md#spec-47-2) |
| §47.3 | [47.3 版本化绑定与请求封口](specs/inference.md#spec-47-3) |
| §47.4 | [47.4 参数意图与消息投影](specs/inference.md#spec-47-4) |
| §47.5 | [47.5 鉴权、代理和额度解释](specs/inference.md#spec-47-5) |
| §47.6 | [47.6 可安装感知、直觉与自主升级](specs/inference.md#spec-47-6) |
| §47.7 | [47.7 兼容验收](specs/inference.md#spec-47-7) |
| §48 | [48 自主主体、运行时能力闭环与正式文档分层](specs/architecture.md#spec-48) |
| §48.1 | [48.1 自主性与真实约束分开](specs/architecture.md#spec-48-1) |
| §48.2 | [48.2 单元、服务与运行事实闭合](specs/architecture.md#spec-48-2) |
| §48.3 | [48.3 感知、渐进披露与学习](specs/architecture.md#spec-48-3) |
| §48.4 | [48.4 协议所有权与产品表面](specs/architecture.md#spec-48-4) |
| §48.5 | [48.5 正式规范与历史入口](specs/architecture.md#spec-48-5) |
| §48.6 | [48.6 上游协作不得阻塞产品开发](specs/architecture.md#spec-48-6) |
| §49 | [49 第三方生态兼容：可选的 MCP 与 Agent Skills 适配器](specs/ecosystem-adapters.md#spec-49) |
| §49.1 | [49.1 两个独立插件，一个安装权威](specs/ecosystem-adapters.md#spec-49-1) |
| §49.2 | [49.2 MCP 适配器的真实边界](specs/ecosystem-adapters.md#spec-49-2) |
| §49.3 | [49.3 Skill 适配器的真实边界](specs/ecosystem-adapters.md#spec-49-3) |
| §49.4 | [49.4 使用中的磨合学习](specs/ecosystem-adapters.md#spec-49-4) |
| §49.5 | [49.5 不降低既有要求](specs/ecosystem-adapters.md#spec-49-5) |
| §50 | [50 追加事实](specs/architecture.md#spec-50) |
| §50.1 | [50.1 适配器传输有界与大观察的有界取回](specs/ecosystem-adapters.md#spec-50-1) |
| §50.2 | [50.2 使用中学习的确定性使用摘要](specs/ecosystem-adapters.md#spec-50-2) |
| §50.3 | [50.3 通路观察的测量入口](specs/network-observation.md#spec-50-3) |
| §50.4 | [50.4 未竟义务的显式结清与复制闭包](specs/durable-state.md#spec-50-4) |
| §51 | [51 认知元概念：内容、态度与理由](specs/cognition-units.md#spec-51) |
| §51.1 | [51.1 元概念](specs/cognition-units.md#spec-51-1) |
| §51.2 | [51.2 awareness 是能力，不是文档](specs/cognition-units.md#spec-51-2) |
| §51.3 | [51.3 内核、宏包与发行](specs/cognition-units.md#spec-51-3) |
| §51.4 | [51.4 可学习](specs/cognition-units.md#spec-51-4) |
| §51.5 | [51.5 主体地位](specs/cognition-units.md#spec-51-5) |
| §51.6 | [51.6 尚未实现](specs/cognition-units.md#spec-51-6) |
| §51.7 | [51.7 提议：遗忘与归档](specs/cognition-units.md#spec-51-7) |
| §51.8 | [51.8 提议：按能力依赖，不钉死具体包](specs/cognition-units.md#spec-51-8) |
| §51.9 | [51.9 助手代定（用户授权）](specs/cognition-units.md#spec-51-9) |
| §51.10 | [51.10 读法治理](specs/cognition-units.md#spec-51-10) |
| §52 | [52 通道协议优先级 / Channel Protocol Priority](specs/channels.md#spec-52) |
| §53 | [53 子代理命名 / Child Agent Naming](specs/model-children.md#spec-53) |
| §54 | [54 自主设计原则 / Autonomous Design Principles](specs/architecture.md#spec-54) |
| §54.1 | [54.1 从人类心智和认知学角度设计](specs/architecture.md#spec-54-1) |
| §54.2 | [54.2 不受模型限制约束的设计目标](specs/architecture.md#spec-54-2) |
| §54.3 | [54.3 可扩展与插件化](specs/architecture.md#spec-54-3) |
| §54.4 | [54.4 更好的 Awareness 与 Perception](specs/architecture.md#spec-54-4) |
| §54.5 | [54.5 同行参考 / Peer Reference](specs/architecture.md#spec-54-5) |
| §55 | [55 自主能力验收合同 / Autonomous Capability Acceptance](specs/verification.md#spec-55) |
| §55.1 | [55.1 自主性的可判定化](specs/verification.md#spec-55-1) |
| §55.2 | [55.2 模型限制不作为设计边界](specs/verification.md#spec-55-2) |
| §55.3 | [55.3 插件化验收](specs/verification.md#spec-55-3) |
| §55.4 | [55.4 Awareness 与 Perception 的持续演进](specs/verification.md#spec-55-4) |
| §55.5 | [55.5 同行参考的落地边界](specs/verification.md#spec-55-5) |
| §56 | [56 验收追踪矩阵 / Acceptance Traceability Matrix](requirements/traceability.md#spec-56) |
| §57 | [57 主动个人主体：更少的门、可修订的 harness、界面与 IM 精炼（Claude Fable 5.1 refine 版本）](specs/architecture.md#spec-57) |
| §57.0 | [57.0 为什么要这一章：骨架是对的，肉是死的](specs/architecture.md#spec-57-0) |
| §57.1 | [57.1 角色框架是可修订内容单元](specs/subject-runtime.md#spec-57-1) |
| §57.2 | [57.2 空闲是感知，不是提示](specs/subject-runtime.md#spec-57-2) |
| §57.3 | [57.3 注意力是主体的政策](specs/subject-runtime.md#spec-57-3) |
| §57.4 | [57.4 通道拥有自己的表面语法](specs/channels.md#spec-57-4) |
| §57.5 | [57.5 流式是时间线的默认真相](specs/web-interface.md#spec-57-5) |
| §57.6 | [57.6 作品是主体伸向界面的手](specs/configuration-playground.md#spec-57-6) |
| §57.7 | [57.7 界面的诚实词汇与生命感](specs/web-interface.md#spec-57-7) |
| §57.8 | [57.8 三个死接口复活](specs/subject-runtime.md#spec-57-8) |
| §57.9 | [57.9 对齐即说明风险与收益](specs/cognition-units.md#spec-57-9) |
| §57.10 | [57.10 自迭代闭环](specs/self-update.md#spec-57-10) |
| §57.11 | [57.11 需求](requirements/traceability.md#spec-57-11) |
| §57.12 | [57.12 行为](requirements/design-provenance.md#spec-57-12) |
| §57.13 | [57.13 验收](specs/verification.md#spec-57-13) |
| §57.14 | [57.14 实现状态与证据边界](specs/verification.md#spec-57-14) |
| §57.15 | [57.15 通道优先级的现行读法（2026-11-09）](specs/channels.md#spec-57-15) |
