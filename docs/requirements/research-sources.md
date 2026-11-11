# 研究版本与证据范围


<a id="spec-31"></a>
<!-- migrated-contract: SPEC:31 -->
# 31 研究版本清单与证据使用规则

本轮截至 2026-10-22 UTC。下表保留既有固定研究基线；完整的来源登记、采用差异和行为比较以 [§39](design-provenance.md#spec-39) 为索引，首版协议见 [§41](../specs/architecture.md#spec-41)。主线可能继续变化，研究 pin 不代表永久冻结依赖；开发时按 [§21.2](../specs/web-interface.md#spec-21-2)、[§29.1](../specs/verification.md#spec-29-1)、[§40](../governance/development-contract.md#spec-40) 更新与验证。日期、版本和 README 不替代实际运行，本规格没有把源码阅读写成已安装或已验证性能。

| 对象 | 本轮核验版本或固定入口 | 对决策的作用 |
| --- | --- | --- |
| WASI | [0.3 发布说明](https://bytecodealliance.org/articles/WASI-0.3)，2026-06-11 | Component/async 路线已发布；不等于 Linux ABI 完整兼容。 |
| Wasmtime / Wasmer / WAMR | [Wasmtime 50.0.0](https://github.com/bytecodealliance/wasmtime/releases/tag/v50.0.0)；[Wasmer 7.5.0](https://github.com/wasmerio/wasmer/releases/tag/v7.5.0)；[WAMR 2.4.5](https://github.com/bytecodealliance/wasm-micro-runtime/releases/tag/WAMR-2.4.5) | 候选 runtime 的平台、接口、AOT/JIT 与体积需目标设备验证。 |
| LayerFS | [v1.0.0 source release](https://github.com/Ephemeral-AI-Lab/layerfs/releases/tag/v1.0.0)，tag `2aaf2552ce21154928b9f9c83582e41a637f5910`；阅读主线 `b5814bf3a8d886ef5e32298118dd1661c352bd2a` | release Store/Disposable 与主线 persistence qualification 分开。 |
| wasi-vfs | [README](https://github.com/kateinoigakukun/wasi-vfs/blob/0f4db4b64882db50ef8c04018e7e98bf59a63f92/README.md)，0.6.3 | 只读嵌入文件系统的实际边界。 |
| pi | [固定树](https://github.com/earendil-works/pi/tree/74f46864025c68504cd10c89eeb626b7e3d64cea) | 最小 coding harness、树形 session、扩展与 operation 抽象。 |
| oh-my-pi | [固定树](https://github.com/can1357/oh-my-pi/tree/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0) | SSH、memory、资源 URI、子 Agent 和工具体验。 |
| DeepSeek Harness | [固定树](https://github.com/deepseek-ai/deepseek-harness/tree/687f282e8625c6b8d11ff58211a90524be2e4317)，0.2.1-rc.2 | 插件生命周期与可替换 Agent loop。 |
| cordis-rs | [固定树](https://github.com/dshbox/cordis-rs/tree/1754062cef67cfc119fc7a2084c0321a231b9092)，facade 0.11 / core 0.6 | typed services/events 与资源清理，不当作沙箱或持久事务。 |
| Codex | [固定树](https://github.com/openai/codex/tree/a3d93e1e692f1144cf8685175a5d1f6bf7294f57) | budget compact 切窗、历史工具可用性与原始记录。 |
| NInfer | [固定树](https://github.com/Neroued/ninfer/tree/64d10fbf9f8e6bfc79ab0570b94674c5b9997929)，2026-10-16 | lanes、KV/StateImages、Host backing、Responses 与恢复边界。 |
| Strata | [固定树](https://github.com/Niko1221/Strata/tree/011c9e600a93bbc9fd451c563501b5831c32bbca)，2026-10-21；app 0.2.2 / engine 0.1.47 | launch/engine 参数分层、conversation parking、parallel 的条件收益。 |
| Proto-UI | [固定树](https://github.com/Proto-UI/Proto-UI/tree/b928a56c6909835771fc2131503b4a7139db738c)，2026-10-21；React adapter 0.3.0-alpha.1 | 用户主线路线；compiler private 状态与可替换接口。 |
| 主动遗忘论文 | [arXiv:2610.20563v1](https://arxiv.org/abs/2610.20563v1) | 有指标的探索 worker 从 best seed 重启；不泛化为个人主体全面失忆。 |
| Proactive Memory Agent | [arXiv:2607.08716v1](https://arxiv.org/abs/2607.08716v1)；[代码](https://github.com/yifannnwu/proactive-memory-agent/tree/89e5c0d6aadfe531a1aee42fd290d48be89973dd) | 更新/选择提醒、消融、实际注入生命周期与训练对象。 |
| SkillsBench | [2602.12670v4](https://arxiv.org/abs/2602.12670v4) | 任务级使用效果与技能自生成不必然有效；不以安装即学会。 |
| ToolRet / ToolScout | [2503.01763v2](https://arxiv.org/abs/2503.01763v2)；[2608.16502v1](https://arxiv.org/abs/2608.16502v1) | 检索候选、发现偏差与独立工具选择测试。 |
| FitText / Tool-DE | [2605.02411v4](https://arxiv.org/abs/2605.02411v4)；[2510.22670v1](https://arxiv.org/abs/2510.22670v1) | 有预算的工具描述与运行时文档 grounding；不能照搬其他并发设置的性能。 |
| OpenAI 动态界面 | [Intelligent UI 发布](https://openai.com/index/gpt-6-for-everyone/)；[Plugins UI](https://developers.openai.com/plugins/build/chatgpt-ui)；[ChatKit](https://platform.openai.com/docs/guides/chatkit) | 官方原生方向与公开 iframe/widgets 能力分开；未核验到通用公开原生 DSL/compiler API。 |
| BoxLite | [固定树](https://github.com/boxlite-ai/boxlite/tree/b59b8c1f39e45e49adede56a65749a431d1380e7), 2026-10-22；[正式发布 v0.10.7](https://github.com/boxlite-ai/boxlite/releases/tag/v0.10.7)，2026-10-18 | 可选完整 Linux microVM；磁盘级 snapshot、GUI、local/REST 与代理差异见 [§34](../specs/desktop-runtime.md#spec-34)；主线能力不自动等于发行包能力。 |
| Pi Durable | [固定包树](https://github.com/earendil-works/pi/tree/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable)，1.4.0，Experimental | 持久任务、提交后发布、补偿与调用重放；默认 SQLite NORMAL 的断电边界独立处理。 |
| tmux | [固定 manual](https://github.com/tmux/tmux/blob/1c418f0b62d131b57d2e113362c2911867102652/tmux.1)，2026-10-22 | capture-pane -e、属性保留/control mode 与自主补工具案例。 |
| vLLM | [主线固定树](https://github.com/vllm-project/vllm/tree/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93)，2026-10-22；正式 v0.32.0 | 原生 KV hints、SDK/gRPC/HTTP 差异、取消与缓存回执，不把主线当发行包。 |
| 1Cat-vLLM | [固定树](https://github.com/1CatAI/1Cat-vLLM/tree/1d7321bf2d35ac8970870dc51bc1f7405517c3ca)，2026-10-22；正式 v1.5.1 | fork 独立能力与忽略字段行为；与用户 Life 生产 c39+patches 分开。 |
| OMP snapcompact / notes windows | [固定实现](https://github.com/can1357/oh-my-pi/tree/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/snapcompact)，源码包 18.12.6 | 文字 PNG 派生档案、独立 notes/history 路线；未声称 npm 发布或 WASI 已实现。 |
| Life 身份材料 | [people](https://github.com/cyjin-yl/life/blob/618f0dcb13eea858b36c7a3f9a802208b56378e6/catalog/people/README.md)、[voiceprints](https://github.com/cyjin-yl/life/blob/618f0dcb13eea858b36c7a3f9a802208b56378e6/catalog/voiceprints/README.md) | 复用 ID、候选和修订，录音内 rank 不冒充全局身份；不复制真实人物/声纹为示例。 |
| Life 推理基线 | [固定读取版本](https://github.com/cyjin-yl/life/blob/aea9568c0f5ef9d099abf0b423ca02ec730da69e/areas/technology/home-lab/2026-10-09-doescompute-1cat-dflash2-candidate-trial.md)，最近明确生产记录 2026-10-11 | 双 V100 TP2/1Cat 资料；用户最新确认四流 128K、133120 冗余优先；本轮未在线重测。 |
| 选举/资源排他 | [Raft 原论文](https://raft.github.io/raft.pdf)、[fencing 分析](https://martin.kleppmann.com/2016/02/08/how-to-do-distributed-locking.html) | 真实多数和目标端排他；分区事实合并是本产品新增协议，不伪称理论已实现产品。 |

竞品、渠道、REPL、Computer Use、memory 与视觉资料就近引用，并纳入 [§39](design-provenance.md#spec-39) 的稳定 Ref 和行为比较。用户原创、上游已具备、本产品扩展及有意不同分别记录；后续修订更新受影响需求和实际证据，保留旧裁决及比较基线。

**证据等级：**源码/规范说明某能力存在的设计或实现路径；官方教程说明支持部署方法；真正可用需记录目标配置下的运行结果；性能/稳定性结论需相同任务与资源预算的可复查测量。推断、建议默认、公开事实和用户明确要求不能互相替代。

<!-- /migrated-contract: SPEC:31 -->
