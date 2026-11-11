# 产品场景与竞品定位


<a id="spec-3"></a>
<!-- migrated-contract: SPEC:3 -->
# 3 研究结论与竞品定位

<!-- /migrated-contract: SPEC:3 -->

<a id="spec-3-1"></a>
<!-- migrated-contract: SPEC:3.1 -->
## 3.1 具名竞品

| 对象 | 已核验的产品事实 | 本产品采用的启发 | 本产品的差异目标 |
| --- | --- | --- | --- |
| OpenAI dots | 2026-09-29 发布；独立云电脑，持续任务与跨渠道工作；空闲主动研究使用连接应用的只读能力 | 持续交付、活动可见、主对话与项目成果分离 | 用户自托管、可选本地模型、授权范围内可行动的自主循环 |
| Claude Duet | 2026-10-21 研究预览；云电脑，主对话和专题频道共用记忆，Tasks/Make、历史搜索与简报 | 安静的主对话、成果工作区、连续人格 | 运行环境/模型可选、执行可观察、SSH 与本地节点 |
| Meta Muse | 长期目标、子 Agent、自建工具、自改及云执行；运行 cell 与身份/持久状态分离 | 可损坏环境不能带走身份与任务；持续授权 | 本地所有权、小型 Rust 控制面、WASM 家园、本地资源调度 |
| AstrBot | 多平台与插件；有沙箱、实验性 subagent/future_task；主动任务文档区分定时执行与持续自主监控 | 消息归一化、会话工作区、渠道兼容 | 原生永续主体循环及可恢复的委派体系 |
| OpenClaw | 自托管 Gateway、多渠道/节点、可换模型与 harness；默认 pairing 可以配置为开放对话 | 节点能力发现、mailbox、渠道/运行协议 | 默认开放聊天、最小可信宿主、全局推理与 KV 管理 |

依据：[dots 官方发布](https://openai.com/index/introducing-dots/)、[Duet 官方介绍](https://claude.com/resources/articles/claude-duet)、[Duet 帮助](https://support.claude.com/en/articles/17631495-get-started-with-claude-duet)、[Muse 发布](https://about.fb.com/news/2026/09/introducing-muse-personal-ai-agent/)、[Muse 架构](https://research.meta.ai/blog/security-and-safety-for-ai-agents-our-approach-with-muse)、[AstrBot 主动任务](https://docs.astrbot.app/use/proactive-agent.html)、[OpenClaw 队列](https://docs.openclaw.ai/concepts/queue)、[OpenClaw 安全与开放策略](https://docs.openclaw.ai/gateway/security)。没有进行同机性能比较；不能据编程语言或宣传数字宣称本产品更快、更省内存。

<!-- /migrated-contract: SPEC:3.1 -->

<a id="spec-3-2"></a>
<!-- migrated-contract: SPEC:3.2 -->
## 3.2 Harness 与记忆系统

| 对象 | 研究发现 | 规格中的采用方式 |
| --- | --- | --- |
| pi | 树形 session、追加 compaction、可替换 operations 与扩展 | 原始事件和上下文投影分离；同一资源操作可跨本地/SSH 后端 |
| oh-my-pi | 统一资源 URI、结构化子 Agent、SSH 管理、多 memory backend；memory recall 可固定首轮结果再追加变化 | 作为工具与子 Agent 体验的主要参考；减少小模型拼命令、重复读长上下文的负担 |
| DeepSeek Harness | model/tools/session/agent loop 插件化；持久 session 事件与实时运行事件分开 | harness 可替换；事件、工具、服务生命周期有正式合同 |
| cordis-rs | Rust typed services/events、资源生命周期与 generation 清理 | 可作为可信模块装配候选；Scope/ServiceRealm 不被当作安全沙箱 |
| Codex budget compact | 当前公开路径直接开启新 context window，跳过模型/服务器摘要；历史和 notes 工具另行提供 | 本地实现窗口、原文寻址与任务连续性；不把 summary 当唯一记录 |
| QMD / OpenViking | 分别提供混合检索与分层上下文组织的参考；各有额外运行/模型依赖 | 选装 backend 与设计参考，不塞进路由器必选内核 |
| Pi Durable | task/checkpoint、提交后发布、持久取消/补偿、nested-call journal | [§10.6](../specs/durable-state.md#spec-10-6) 正式恢复协议，不以 cleanup 或 transcript 代替 |
| OMP snapcompact / notes windows | 文字 PNG 视觉表示与独立笔记/原文切窗模式，各有预算与限制 | [§12.3](../specs/context-policies.md#spec-12-3) 多种 ContextPolicy 按本地模型实测，不误当 RAM/KV 快照 |

依据：[pi](https://github.com/earendil-works/pi)、[pi 会话格式](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/session-format.md)、[oh-my-pi](https://github.com/can1357/oh-my-pi)、[DSH 架构](https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/architecture.md)、[cordis-rs](https://github.com/dshbox/cordis-rs)、[Codex 切窗实现](https://github.com/openai/codex/blob/a3d93e1e692f1144cf8685175a5d1f6bf7294f57/codex-rs/core/src/compact_token_budget.rs)、[Codex 历史工具](https://github.com/openai/codex/blob/a3d93e1e692f1144cf8685175a5d1f6bf7294f57/codex-rs/ext/history-notes/src/tools.rs)、[QMD](https://github.com/tobi/qmd)、[OpenViking](https://github.com/volcengine/OpenViking)。这些是参考实现，不意味着将其全部依赖原样移植。

<!-- /migrated-contract: SPEC:3.2 -->

<a id="spec-3-3"></a>
<!-- migrated-contract: SPEC:3.3 -->
## 3.3 四个会影响方案的事实纠正

**WASM 不自动等于 Linux ABI。** WASI/WASIX 提供的系统接口与 Linux ELF、动态链接、fork、native 扩展、GPU 驱动并不等价。Linux/POSIX 兼容的 memory 项目可能仍需要移植或远端服务。WASI 0.3 已于 2026-06-11 发布，不能再按尚未发布的 Preview3 规划；但每个 runtime/工具链的具体接口兼容仍需核验。[WASI 0.3 官方说明](https://bytecodealliance.org/articles/WASI-0.3)

**LayerFS 采用真实 Linux VM 内直接集成路线。** v1.0.0 source release 的 Store 和完整 workspace 各有平台条件；Disposable profile 不提供崩溃/断电持久性承诺。用户已决定不重做 LayerFS：原版用于合格 Linux VM，WASM 状态单独实现；上游提交与本产品持久发表分别记账，具体拓扑与保证见 [§10.2.1](../specs/durable-state.md#spec-10-2-1)。主线 qualification 不能冒充发布版保证。[固定 SDK 合同](https://github.com/Ephemeral-AI-Lab/layerfs/blob/2aaf2552ce21154928b9f9c83582e41a637f5910/sdk/js/README.md)、[平台 guard](https://github.com/Ephemeral-AI-Lab/layerfs/blob/2aaf2552ce21154928b9f9c83582e41a637f5910/core/crates/layerfs-store-service/src/kernel_guard.rs)

**Proto-UI 当前主线不是已发布的 0.3.0 RC。** 核验到 React adapter 为 0.3.0-alpha.1；compiler 包仍 private，不能假称稳定可替换。采用用户指定的主线路线，同时在文档和产物中记录真实版本。[已核验 manifest](https://github.com/Proto-UI/Proto-UI/blob/b928a56c6909835771fc2131503b4a7139db738c/packages/adapters/react/package.json)、[compiler manifest](https://github.com/Proto-UI/Proto-UI/blob/b928a56c6909835771fc2131503b4a7139db738c/packages/compiler/package.json)

**KV 功能不能统一伪装成 OpenAI-compatible。** slot、prefix reuse、Responses continuation、backend sticky routing 是不同能力；缺少某项时需报告“不支持/未知”，并回退到本地原文重建。后文给出专门矩阵。

<!-- /migrated-contract: SPEC:3.3 -->
