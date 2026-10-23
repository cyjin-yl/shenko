## 21.10 Magpie 视觉参照与比较方法

U37 指定 Magpie 的风格与美观程度为参考。本规格先以高相关公开项目 [yetone/magpie](https://github.com/yetone/magpie/tree/398803e3f575dcafc20573f674a98d71105c9694) 作为工作参照，固定提交 398803e3f575dcafc20573f674a98d71105c9694，2026-10-22。已查看该版本的 [Agents 完整页](https://github.com/yetone/magpie/blob/398803e3f575dcafc20573f674a98d71105c9694/site/public/img/agents-light.png)、[紧凑面板](https://github.com/yetone/magpie/blob/398803e3f575dcafc20573f674a98d71105c9694/site/public/img/panel-light.png) 和 [模型选择器](https://github.com/yetone/magpie/blob/398803e3f575dcafc20573f674a98d71105c9694/site/public/img/picker-light.png)。原始指代尚未由链接唯一确认；若后续用户所指不同，替换视觉来源即可，不阻断其余规格。REF-MAGPIE 与 B35 保存比较范围。

| 参照中可观察的处理 | Shenko 的设计要求 |
| --- | --- |
| 连续列表、对齐的对象/状态/控制，轻分隔与克制容器 | 用空间、文字层级和稳定位置让人看懂当前关注、对话、委派与成果；不要求所有信息都装进大卡片 |
| 操作就在对象旁边，紧凑视图仍保留常用控制 | 任务、会话、seat、模型等常用动作就地可达；详细参数按需展开 |
| 搜索、来源分组、当前选中标记与焦点各有表达 | 长列表能区分相似工具/联系人/节点；焦点与已选状态不混淆，键盘与屏幕阅读可用 |
| 完整页与紧凑面板保持同样对象与术语 | 宽屏、窄屏和渠道降级保留对象身份与动作语义，不要求复制相同布局 |
| 中性背景、清楚文字、少量颜色用于状态或选择 | 建立语义 token 和主题；颜色表达真实状态，不能靠满屏状态灯或持续动画制造生命感 |

Shenko 首页的内容仍是人与主体正在参与的事情。Magpie 的模型网关列表适合参考信息组织和操作细节，不直接规定个人主体首页。布局变化应来自实际关注、委派、结果和可继续的工作；不会把静态参照截图当作已观察到的运行时动效。品牌使用 Shenko，无 slogan；实例名称独立，初始化与恢复见 A46。

设计 Agent 对每个采用点保存 source view、对应的产品场景、采用/改变理由与真实实现画面或交互证据。至少覆盖本次实现影响的主题、中文长文、窄屏、焦点、空/加载/错误和动态状态。生命感、辨识度、节奏与可读性由实际交互共同评价，不用固定面板数量、像素抄图或人类逐屏批准代替设计判断。Proto-UI/shadcn 与 compiler 接缝继续遵循 §21.2；视觉参考不强制引入 Magpie 的框架或宿主依赖。


# 39 设计来源与上游行为完整度

## 39.1 所有设计均有来源和比较范围

本章落实 U37。每个有意义的产品要求、架构选择、插件行为、算法策略和视觉决定均连接到来源；§32 的 R001–R128 逐项指向原话 U 编号及本章 B 行为记录。B 记录再指向具体 Ref、采用范围、差异和验收。引用粒度是一个可以理解、实现和验证的行为，不要求每行普通代码堆一串链接。

来源分为 USER、UPSTREAM、PAPER、DERIVED。USER 是用户原话与后续裁决；UPSTREAM 是项目实现、规范或官方产品材料；PAPER 是论文及作者研究；DERIVED 是本产品基于要求和证据作出的组合设计。来源性质与证据等级分开：官方介绍不能当源码实装，论文方法不能当本产品效果，本产品扩展不能归为上游已提供。U14 转贴的 DeepSeek/Gemini 建议按 §30 的采纳/修正/否定记录引用，不自动成为用户要求。

每条来源保留稳定 ref_id、项目/作者、固定 commit/release/文档或论文版本、精确文件/符号/段落、实际阅读日期、已检查范围和已知限制。可变官网记录阅读日期与关键合同；不能把 main/master/latest 伪称固定版本。固定研究基线与最新跟踪入口并存，上游更新形成新记录，不覆盖旧版本比较的依据。以下新增读取与核对日期为 2026-10-22 UTC；正文既有固定版本继续有效。

每条行为保留 behavior_id、requirement_ids、refs、upstream_contract、relationship、delta、conformance_cases、implementation_refs、evidence 和 gap。relationship 可为直接复用、移植、适配、扩展、有意不同、暂缓或待核验；“暂缓”不删除用户要求。实现证据至少标明实际工件版本、环境/模型配置、执行方法与结果。源码阅读、规范说明、模拟、回放、目标环境实测及长期运行分别记录。

## 39.2 来源登记

本表是 54 组稳定来源入口；同组内的具体能力必须使用行为行或就近正文中的精确位置。Pi 基础 harness 与 Pi Durable 保留不同提交，不将两个时间点拼成一个不存在的版本。colibri-rs 准确项目尚未匹配；Magpie 暂以 yetone/magpie 作为工作参照；二者状态不会被登记表自动变成用户确认。

| Ref | 来源性质与证据 | 固定位置或阅读基线 | 采用范围与边界 |
| --- | --- | --- | --- |
| REF-USER | USER；用户原话 | §33 U01–U39；§28 裁决记录 | 原始需求、澄清与优先级。用户原创不伪配外部上游；新裁决覆盖旧建议，旧原文保留。 |
| REF-DESIGN | DERIVED；规格推导 | 本 SPEC v0.4.0 的相应章节与 B01–B41 | 本产品的组合协议、算法候选和工程推导；逐行写明上游与差异，尚未实现不能记为已验证。 |
| REF-PI | UPSTREAM；固定源码＋相应文档 | [固定树 74f4686](https://github.com/earendil-works/pi/tree/74f46864025c68504cd10c89eeb626b7e3d64cea)；[session-format.md](https://github.com/earendil-works/pi/blob/74f46864025c68504cd10c89eeb626b7e3d64cea/packages/coding-agent/docs/session-format.md) | 最小 coding harness、树形 session、扩展与 operations。作为个人框架的结构启发，不要求复制 coding 产品全部功能。 |
| REF-OMP | UPSTREAM；固定源码＋相应文档 | [固定树 0b5c27a](https://github.com/can1357/oh-my-pi/tree/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0)；README、docs/extensions.md、docs/rpc.md，具体自解释与 compact 路径见 §12.3、§38 | 工具、资源 URI、SSH、子 Agent、memory、RPC、自解释与 compact 分别对照；只引用根仓库不能证明某一行为。 |
| REF-DSH | UPSTREAM；固定源码＋相应文档 | [0.2.1-rc.2 / 687f282](https://github.com/deepseek-ai/deepseek-harness/blob/687f282e8625c6b8d11ff58211a90524be2e4317/docs/architecture.md) | model/tools/session/agent loop 可替换与生命周期。Rust/WASM 的可恢复自替换是本产品扩展。 |
| REF-CORDIS | UPSTREAM；固定源码＋相应文档 | [1754062 / effect.rs](https://github.com/dshbox/cordis-rs/blob/1754062cef67cfc119fc7a2084c0321a231b9092/crates/cordis-core/src/effect.rs)；同提交 README，facade 0.11 / core 0.6 | typed services/events、scope/generation 清理；不提供任意外部效果撤销或断电事务保证。 |
| REF-COLIBRI | USER；用户原话 | U35；§37.1 | 指定插件主干的名字已确定，准确仓库/API 尚待关联。同名 MoE 项目不是已核验的插件主干，不用它补空白。 |
| REF-FABRIC | UPSTREAM；固定源码＋相应文档 | [Events 文档](https://docs.fabricmc.net/develop/events/)；[固定 Event.java a335c35](https://github.com/FabricMC/fabric-api/blob/a335c35c0b9a489c7f4e5dd0fe36f1974845f1e1/fabric-api-base/src/main/java/net/fabricmc/fabric/api/event/Event.java)；[phase API](https://maven.fabricmc.net/docs/fabric-api-0.102.0+1.21/net/fabricmc/fabric/api/event/Event.html) | 类型化扩展点、phase 与相对顺序；产品拒绝有环候选、版本固定与恢复机制不归为 Fabric 已有保证。 |
| REF-DURABLE | UPSTREAM；固定源码＋相应文档 | [Pi Durable 1.4.0 / c60de42 规范](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md)；同包 README、src/harness/tool.ts | task/checkpoint、提交后发布、intent/effect/outcome、持久取消/补偿和调用重放；Experimental 与存储同步等级保留。 |
| REF-CODEX | UPSTREAM；固定源码＋相应文档 | [budget compact a3d93e1](https://github.com/openai/codex/blob/a3d93e1e692f1144cf8685175a5d1f6bf7294f57/codex-rs/core/src/compact_token_budget.rs)；[history-notes tools](https://github.com/openai/codex/blob/a3d93e1e692f1144cf8685175a5d1f6bf7294f57/codex-rs/ext/history-notes/src/tools.rs) | 预算切窗与原始历史/notes 分离；provider 条件、读取一致性与本地实现分别记录。 |
| REF-WASI | UPSTREAM；官方规范/文档/产品说明 | [WASI 0.3 发布说明，2026-06-11](https://bytecodealliance.org/articles/WASI-0.3)；[WASIX 功能](https://www.wasix.org/docs/explanation/features/) | 接口/异步/进程兼容边界；Linux ELF、fork、动态链接、native 扩展不能由名字推导。 |
| REF-RUNTIME | UPSTREAM；官方规范/文档/产品说明 | [Wasmtime 50.0.0](https://github.com/bytecodealliance/wasmtime/releases/tag/v50.0.0)；[Wasmer 7.5.0](https://github.com/wasmerio/wasmer/releases/tag/v7.5.0)；[WAMR 2.4.5](https://github.com/bytecodealliance/wasm-micro-runtime/releases/tag/WAMR-2.4.5) | runtime、目标架构、AOT/JIT 与设备 profile 的候选基线，实际体积/驻留/Android 与路由器覆盖待测。 |
| REF-PORTS | UPSTREAM；候选文档/局部实现 | [wasi-gh](https://github.com/ai-ecoverse/homescoop/blob/main/packages/wasi-gh/package/README.md)；[OpenSSH WASIX port](https://github.com/ai-ecoverse/homescoop/blob/main/packages/wasix-openssh/package/README.md)；[slicc-kernel](https://github.com/ai-ecoverse/slicc-kernel)，2026-10-22 读取的动态入口 | 工具移植线索及特定 host 依赖；未锁实现 commit，不能宣称已有通用 daemon 或所有 gh/tea/toybox/gcc 目标已成立。 |
| REF-NIX | UPSTREAM；官方规范/文档/产品说明 | [Nix 2.35 profiles](https://nix.dev/manual/nix/2.35/package-management/profiles)；[远程构建](https://nix.dev/manual/nix/2.35/advanced-topics/distributed-builds)；[Cargo target](https://doc.rust-lang.org/cargo/commands/cargo-build.html) | 声明式闭包、generation、回滚与异机构建；WASM 内完整 Nix 不由此得到证明。 |
| REF-LAYERFS | UPSTREAM；固定源码＋相应文档 | [v1.0.0 / 2aaf255 SDK](https://github.com/Ephemeral-AI-Lab/layerfs/blob/2aaf2552ce21154928b9f9c83582e41a637f5910/sdk/js/README.md)；同提交 Store guard、Docker backend、persistence README；主线 b5814bf 见 §31 | 真实 Linux VM 内复用原版；Store、workspace、Disposable/persistence 各按自身条件比较。 |
| REF-BOXLITE | UPSTREAM；固定源码＋相应文档 | [v0.10.7](https://github.com/boxlite-ai/boxlite/releases/tag/v0.10.7)；[固定树 b59b8c1](https://github.com/boxlite-ai/boxlite/tree/b59b8c1f39e45e49adede56a65749a431d1380e7)；§34 列出的 rootfs、snapshot、GUI、REST 路径 | 可选完整 Linux microVM 与 Computer Use；磁盘快照不等于进程/内存快照，宿主挂载另算。 |
| REF-STORAGE | UPSTREAM；固定源码＋相应文档 | [SQLite 原子提交](https://www.sqlite.org/atomiccommit.html)、[WAL](https://sqlite.org/wal.html)、[备份](https://sqlite.org/backup.html)；[Git LFS spec](https://github.com/git-lfs/git-lfs/blob/main/docs/spec.md)；[wasi-vfs 0.6.3](https://github.com/kateinoigakukun/wasi-vfs/blob/0f4db4b64882db50ef8c04018e7e98bf59a63f92/README.md) | 持久边界、一致备份、二进制指针格式、只读 VFS 限制；本产品可写 CAS/树/恢复协议属 DERIVED。动态文档读取日期 2026-10-22。 |
| REF-FTS | UPSTREAM；官方规范/文档/产品说明 | [SQLite FTS5](https://sqlite.org/fts5.html)，2026-10-22；§12.1–12.3 | 原始 session 与可重建全文索引；中文短词覆盖和本地读取一致性是产品额外验收。 |
| REF-SSH | UPSTREAM；官方规范/文档/产品说明 | [OpenSSH ssh-keygen](https://man.openbsd.org/ssh-keygen)，2026-10-22；REF-OMP 的 SSH 工具；REF-PORTS 的 WASIX port | keygen/公钥/指纹、认证与结构化远端操作；真实 WASM、host bridge 和 remote service 分开标示。 |
| REF-QMD | UPSTREAM；固定源码＋相应文档 | [README / 86d2365](https://github.com/tobi/qmd/blob/86d2365d17fc682e491c2c53d1e4a884aee8682a/README.md)；[查询语法](https://github.com/tobi/qmd/blob/86d2365d17fc682e491c2c53d1e4a884aee8682a/docs/SYNTAX.md)，2026-10-22 | BM25/向量/typed expansion/RRF/rerank、命中后按path/docid取原文、索引与embedding维护的可选参考；Node/Bun+native模型依赖另算。 |
| REF-OPENVIKING | UPSTREAM；固定源码＋相应文档 | [检索 / 824e9c7](https://github.com/volcengine/OpenViking/blob/824e9c7ff5e7cebbc36f55257c39f8bbe75d5a24/docs/en/concepts/07-retrieval.md)；[层次内容](https://github.com/volcengine/OpenViking/blob/824e9c7ff5e7cebbc36f55257c39f8bbe75d5a24/docs/en/concepts/03-context-layers.md)；[session](https://github.com/volcengine/OpenViking/blob/824e9c7ff5e7cebbc36f55257c39f8bbe75d5a24/docs/en/concepts/08-session.md) | L0/L1目录sidecar、来源/陈旧状态、session先归档后异步提取及memory_diff；每query全局向量检索可选rerank；已移除旧递归目录检索。为选装后端，不引入第二主体。 |
| REF-PMA | PAPER；论文＋代码 | [2607.08716v1](https://arxiv.org/abs/2607.08716v1)；[代码 89e5c0d](https://github.com/yifannnwu/proactive-memory-agent/tree/89e5c0d6aadfe531a1aee42fd290d48be89973dd) | 更新 bank、选择提醒/沉默与消融；严格 transient 投影是本产品差异，原始注入证据仍保存。 |
| REF-RESET | PAPER；论文/原作者研究 | [Scaling Discovery Requires Forgetting 2610.20563v1](https://arxiv.org/abs/2610.20563v1) | 有评测器的 research worker 从 best seed 重启；额外失败笔记单独消融，不清空个人主体身份/真实历史。 |
| REF-TOOL-RESEARCH | PAPER；论文/原作者研究 | [SkillsBench v4](https://arxiv.org/abs/2602.12670v4)；[ToolRet v2](https://arxiv.org/abs/2503.01763v2)；[ToolScout v1](https://arxiv.org/abs/2608.16502v1)；[FitText v4](https://arxiv.org/abs/2605.02411v4)；[Tool-DE v1](https://arxiv.org/abs/2510.22670v1) | 分别支持过程知识、工具召回、来源偏差、预算内检索和描述增强的研究问题；§18 分别验收，不把论文收益照搬到 27B/C=1。 |
| REF-SKILLS-MCP | UPSTREAM；官方规范/文档/产品说明 | [Agent Skills 格式与渐进披露](https://agentskills.io/specification)；[Anthropic 工具工程](https://www.anthropic.com/engineering/advanced-tool-use)；[MCP 2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28) | 兼容入口、按需加载与工具协议；MCP 的可选扩展逐项协商，市场安装不等于安全性或任务有效性证明。 |
| REF-NINFER | UPSTREAM；固定源码＋相应文档 | [64d10fb 调度与 context cache](https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/maintainer/resource-scheduling-and-context-cache.md)；同提交 docs/serving.md | lanes、KV、StateImages、Host backing、稳定暂停边界、Replay 与视觉上限分开透传。 |
| REF-STRATA | UPSTREAM；固定源码＋相应文档 | [011c9e6 BATCHING.md](https://github.com/Niko1221/Strata/blob/011c9e600a93bbc9fd451c563501b5831c32bbca/docs/BATCHING.md)，app 0.2.2 / engine 0.1.47 | Host conversation parking、GPU 驻留和并行 decode 不合并为单一缓存开关；并行收益以部署测量为准。 |
| REF-VLLM | UPSTREAM；固定源码＋相应文档 | [c722f3a 固定树](https://github.com/vllm-project/vllm/tree/c722f3a5368a9eed2b90c1a14b8eaf17acba3e93)，正式 v0.32.0；§13.6–13.7 的具体接口 | KV hints/原生传输/取消/回执与服务配置按实际支持程度记录，不发明统一 slot。 |
| REF-1CAT | UPSTREAM；固定源码＋相应文档 | [1d7321b 固定树](https://github.com/1CatAI/1Cat-vLLM/tree/1d7321bf2d35ac8970870dc51bc1f7405517c3ca)，正式 v1.5.1；REF-LIFE 的生产版本另列 | fork 能力、忽略字段和用户生产 patch 的差异；不把最新源码当作用户已部署版本。 |
| REF-LIFE | UPSTREAM；固定源码＋相应文档 | [people / 618f0dc](https://github.com/cyjin-yl/life/blob/618f0dcb13eea858b36c7a3f9a802208b56378e6/catalog/people/README.md)；[voiceprints](https://github.com/cyjin-yl/life/blob/618f0dcb13eea858b36c7a3f9a802208b56378e6/catalog/voiceprints/README.md)；§22.1 固定推理记录 | 用户私有来源：稳定身份、录音内 rank、候选/确认/修订；推理资料按 U21 四流 128K 更正。不向公开文档复制私密素材。 |
| REF-NAPCAT | UPSTREAM；固定源码＋相应文档 | [NapCat 3420f0b WebSocket](https://github.com/NapNeko/NapCatQQ/blob/3420f0b76d7dc0ac92cde1663f21ea27f61e13bb/packages/napcat-onebot/network/websocket-server.ts)；[OneBot WS](https://github.com/botuniverse/onebot-11/blob/master/communication/ws.md)；[napcat-cli 3da08c0](https://github.com/cyjin-yl/napcat-cli/blob/3da08c03ff491cb8b86896102b0eab06c9b0c145/napcat_cli/lib/events_sqlite.py) | 收发/账号/附件/重连与事件缺口；echo 不是幂等键，since 时间戳不是完整游标，按 §35 对照。 |
| REF-QQ | UPSTREAM；官方规范/文档/产品说明 | [QQ 消息总览](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/overview.html)，2026-10-22 | 官方事件、媒体、回复/主动投递及平台权限；与 NapCat 账号命名空间分离。 |
| REF-ILINK | UPSTREAM；固定源码＋相应文档 | [Tencent iLink 协议](https://github.com/Tencent/openclaw-weixin/blob/main/docs/ilink-bot-protocol.md)；[channel.ts](https://github.com/Tencent/openclaw-weixin/blob/main/src/channel.ts)，2026-10-22 动态入口 | direct、游标、context token、附件与认证；不能据名称承诺微信群，具体 iLinkBot wrapper 仍按版本适配。 |
| REF-FEISHU | UPSTREAM；官方规范/文档/产品说明 | [飞书消息事件](https://open.feishu.cn/document/server-docs/im-v1/message/events/receive?lang=zh-CN)，2026-10-22 | 私聊/群/线程/卡片/文件、长连接或回调；tenant/App 权限和国际 Lark 差异。 |
| REF-ASTRBOT | UPSTREAM；官方规范/文档/产品说明 | [AstrBot 主动任务](https://docs.astrbot.app/use/proactive-agent.html)，2026-10-22 | 多渠道与主动任务的产品比较入口；定时任务与永续主体差异明确，不继承无关默认限制。 |
| REF-OPENCLAW | UPSTREAM；官方规范/文档/产品说明 | [队列](https://docs.openclaw.ai/concepts/queue)、[Gateway 策略](https://docs.openclaw.ai/gateway/security)，2026-10-22 | 节点/消息/渠道启发与开放策略对照；本产品默认开放聊天，管理员另行登记。 |
| REF-CLAUDE | UPSTREAM；官方规范/文档/产品说明 | [Agent SDK user-input](https://code.claude.com/docs/en/agent-sdk/user-input)、[CLI](https://code.claude.com/docs/en/cli-reference)、[hooks](https://code.claude.com/docs/en/hooks)，2026-10-22 | 结构化 question/permission、CLI 运行/恢复、hook 边界；不把已有交互 TUI 冒充任意 SDK 可 attach 会话。 |
| REF-TMUX | UPSTREAM；固定源码＋相应文档 | [manual 1c418f0](https://github.com/tmux/tmux/blob/1c418f0b62d131b57d2e113362c2911867102652/tmux.1)；[Control Mode](https://github.com/tmux/tmux/wiki/Control-Mode)；[vt100 crate](https://docs.rs/vt100/latest/vt100/) | 属性/屏幕/cursor/控制流与语义观察；终端 parser 不自动识别所有应用问题，send-keys 无条件重放不安全。 |
| REF-REPL | UPSTREAM；候选文档/局部实现 | [CPython WASI](https://github.com/python/cpython/tree/main/Platforms/WASI)、[Pyodide packages](https://pyodide.org/en/stable/usage/loading-packages.html)、[Javy](https://github.com/bytecodealliance/javy)、[ComponentizeJS](https://github.com/bytecodealliance/ComponentizeJS)、[Node targets](https://github.com/nodejs/node/blob/main/BUILDING.md)、[Bun](https://github.com/oven-sh/bun)，2026-10-22 动态入口 | 分别核验解释器、包与恢复等级；JS engine、宿主 Node/Bun 不冒充 Node/Bun in WASM。 |
| REF-OS | UPSTREAM；官方规范/文档/产品说明 | [Windows containers](https://learn.microsoft.com/en-us/virtualization/windowscontainers/quick-start/lift-shift-to-containers)、[macOS VM](https://developer.apple.com/documentation/virtualization/running-macos-in-a-virtual-machine-on-apple-silicon)、[屏幕录制](https://support.apple.com/guide/mac-help/control-access-screen-system-audio-recording-mchld6aa7d23/mac)、[Accessibility](https://support.apple.com/guide/mac-help/allow-accessibility-apps-to-access-your-mac-mh43185/mac)，2026-10-22 | 桌面会话/VM/授权的真实平台边界；不将 Linux 容器等同所有 OS 的桌面。 |
| REF-RAFT | PAPER；论文/原作者研究 | [Raft 原论文](https://raft.github.io/raft.pdf)；[fencing 原作者分析](https://martin.kleppmann.com/2016/02/08/how-to-do-distributed-locking.html) | 多数与目标端 fencing 的依据；在线优先分支、重连合并与个人连续性属本产品协议。 |
| REF-PROTO | UPSTREAM；固定源码＋相应文档 | [b928a56 React manifest](https://github.com/Proto-UI/Proto-UI/blob/b928a56c6909835771fc2131503b4a7139db738c/packages/adapters/react/package.json)；[compiler manifest](https://github.com/Proto-UI/Proto-UI/blob/b928a56c6909835771fc2131503b4a7139db738c/packages/compiler/package.json) | 本轮真实 React adapter 0.3.0-alpha.1、compiler private；每开发任务追最新主线并 pin；compiler seam 是产品合同。 |
| REF-SHADCN | UPSTREAM；官方规范/文档/产品说明 | [shadcn changelog](https://ui.shadcn.com/docs/changelog)，2026-10-22 | 组件/token 与当期 AI 交互参考；不虚构唯一的 2026 视觉标准。 |
| REF-DYNAMIC-UI | UPSTREAM；官方规范/文档/产品说明 | [Intelligent UI 发布](https://openai.com/index/gpt-6-for-everyone/)、[官方帮助](https://help.openai.com/en/articles/20001598-intelligent-ui-in-chatgpt)、[Plugins UI](https://developers.openai.com/plugins/build/chatgpt-ui)、[ChatKit widgets](https://developers.openai.com/api/docs/guides/chatkit-widgets) 与 [actions](https://developers.openai.com/api/docs/guides/chatkit-actions) | 生成式界面产品方向与公开开发原语分开；本产品跨渠道 DSL/动作账本/降级为 DERIVED。 |
| REF-MAGPIE | UPSTREAM；官方截图＋源码入口 | [yetone/magpie 398803e](https://github.com/yetone/magpie/tree/398803e3f575dcafc20573f674a98d71105c9694)；site/public/img/agents-light.png、panel-light.png、picker-light.png；§21.10 精确链接 | 工作视觉参照；三个官方截图已观察。用户未给确切链接，身份作为高相关候选；运行交互/动画未在本轮实测。 |
| REF-DOTS | UPSTREAM；官方规范/文档/产品说明 | [OpenAI dots 发布，2026-09-29](https://openai.com/index/introducing-dots/) | 产品方向：持续任务、活动与成果。官方介绍层级，不作为本地资源或实现性能证据。 |
| REF-DUET | UPSTREAM；官方规范/文档/产品说明 | [Claude Duet，2026-10-21](https://claude.com/resources/articles/claude-duet)；[帮助](https://support.claude.com/en/articles/17631495-get-started-with-claude-duet) | 产品方向：主对话、工作区、共同记忆与历史。UI/命名灵感不等于要求全面克隆。 |
| REF-MUSE | UPSTREAM；官方规范/文档/产品说明 | [Muse 发布](https://about.fb.com/news/2026/09/introducing-muse-personal-ai-agent/)；[运行架构](https://research.meta.ai/blog/security-and-safety-for-ai-agents-our-approach-with-muse) | 持续自主、子 Agent、自建工具、运行环境与身份分离的对照；本产品本地可改协议另行验证。 |
| REF-NAME | UPSTREAM；官方规范/文档/产品说明 | [Sonzai / Mind Layer 官网](https://sonz.ai/)，2026-10-22 | 历史候选 Sonzai 的同领域命名排查；用户已于 U39 正式选择 Shenko。仅保留历史出处，不是 Shenko 的命名来源或可用性判断。 |
| REF-HINDSIGHT | UPSTREAM；候选文档/局部实现 | [vectorize-io/hindsight](https://github.com/vectorize-io/hindsight)，2026-10-22；未锁源码行为基线 | 已识别候选项目，尚未建立与本产品 retain/recall/reflect 全合同的逐行为对照；不宣称已适配。 |
| REF-LLAMACPP | UPSTREAM；候选文档/局部实现 | [server README](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)，2026-10-22 动态入口 | §13.6 的 slot/cache 附加接口参考；实际启用与缓存回执需部署验收，未锁 commit 不作严格版本等价结论。 |
| REF-DYNAMO | UPSTREAM；官方规范/文档/产品说明 | [SGLang for Agentic Workloads](https://docs.nvidia.com/dynamo/latest/knowledge-base/modular-components/backends/sg-lang/agents-on-sg-lang)，读取时标 v1.5.1，2026-10-22 | priority hints 与实验性 session control 参考；精确 backend 配置/限制保留，不自动成为首版依赖。 |
| REF-GIT-WORKTREE | UPSTREAM；官方规范/文档/产品说明 | [Git worktree 手册](https://git-scm.com/docs/git-worktree)，读取时最后修改版 2.56.0，2026-10-22 | 并行候选工作树与清理/恢复的接口参考；共享仓库对象与宿主权限不构成安全隔离。 |
| REF-QWEN | UPSTREAM；官方规范/文档/产品说明 | [Qwen3.8-27B 模型卡](https://huggingface.co/Qwen/Qwen3.8-27B)；U20/U21 的用户实际配置 | 模型识别与待测画像；实际量化、模板、四流上下文以部署manifest为准，不能从参数量推出固定能力结论。 |

## 39.3 已枚举的行为对照

以下 41 个范围是当前采用范围的比较清单，全部处于“规格已规定，产品待实现与目标环境验证”。它们不表示已经实现 41 项，也不声称已完整审查或复制每个上游的所有功能。§32 给出每一需求到 B 行的反向索引；一个 B 范围进入实施时，按本表列出的动作、失败与恢复行为继续拆成可执行用例，保持原 B ID 可寻址。

| 行为 ID 与范围 | 来源 | 采用、差异与完整度比较 | 正文和验收入口 |
| --- | --- | --- | --- |
| B01 个人框架与持续生活 | U01/U11/U20；REF-PI、REF-DOTS、REF-DUET、REF-MUSE | 采用最小 harness、持续任务/成果/共享经历的启发；本地所有权、统一个人知识、真正永续主体是用户目标与本产品组合，不以竞品宣传作性能证据。 | §1、3–4；A24；卸载 coding 插件后仍完成个人对话、承诺与主动任务。 |
| B02 小核心与可替换主干 | U01/U08/U09/U15/U35；REF-DSH、REF-CORDIS、REF-COLIBRI | 逐项比较 model/tools/session/loop 装配、服务生命周期、事件订阅与清理；采用 colibri-rs 指定主干，准确仓库待关联。core/rescue 可改，不永久冻结 Rust 核心；生命周期不冒充持久事务。 | §7、9；A18/A21；坏插件、卸载、强杀和核心迁移分别验收；记录 kernel+默认插件总成本。 |
| B03 唯一主体与主子协作 | U02/U15/U19/U20；REF-OMP、REF-PI、REF-DESIGN | 永续串行、主体不直接 I/O、任意 child prompt/model 来自用户。OMP executor.ts、async-job-delivery.ts、irc/bus.ts 提供结果归属/epoch/有限跟进参考；扩展为 durable 双向讨论与有限直接渠道回复。 | §5–6；A01/A02/A03/A31/A36/A37；主子等待不占彼此必需推理许可，进展不吞最终结果，取消/迟到不重复外发。 |
| B04 持久任务和调用恢复 | U19/U33；REF-DURABLE、REF-STORAGE | 采用 task/checkpoint、调用日志、提交后发布、intent/effect/outcome、durable cancel/compensate；新增 Rust/provider 跨进程与跨版本恢复、QQ/SSH/GUI 外部事实对账。 | §10.6、35–36；A33、E01–E10；在 intent、外部完成、receipt 前后故障，不把 unknown 当失败重试或成功。 |
| B05 持久 WASM 家园与挂载 | U01/U19；REF-WASI、REF-RUNTIME、REF-STORAGE | 逐项比较 ABI/imports、网络、进程、文件锁、异步、AOT/JIT、目标设备；自有状态树、任意可访问挂载、RO/direct RW/COW 与恢复是本产品实现。 | §8、10、22；A07/A08/A17；同一包逐 runtime/profile 测试，重启与 factory 恢复保留身份/原始事实。 |
| B06 WASM 工具安装和自主移植 | U01/U19；REF-PORTS、REF-WASI、REF-NIX | toybox/gcc-wasm/gh/tea/SSH 均是具名目标；上游 port 是线索，记录 hosted/target、imports、host shim、包功能子集。主体能发现缺口并委派移植，不用 host wrapper 勾选 WASM 完成。 | §8.2、18.8；A09 与逐包 conformance；安装、运行、认证、取消、卸载/恢复及原阻塞任务实际通过。 |
| B07 包 generation 与自主更新 | U01/U09/U15/U17/U20；REF-NIX、REF-DSH、REF-CORDIS、REF-DURABLE | 采用闭包、锁定、异机构建和 generation；Agent 自选更新/代理/延后；A 继续服务、复制迁移 B、烟测、交接与携带新事实的恢复属于本产品合同。允许 breaking change 与核心/救援器替换。 | §9、35；A21/A27/A28/A39；无需用户在线；更新失败可继续工作，回退不丢 B 的新事实。 |
| B08 WASM 二进制状态与快照 | U01/U14/U19/U33；REF-STORAGE、REF-DESIGN | CAS/目录树、分支、对象可达性/GC、原子 ref 与一致 checkpoint 为本产品设计；Git LFS、SQLite durability 仅提供各自原语。只读 wasi-vfs 路线明确排除，LWW 仅对声明的类型成立。 | §10.1–10.5、36；A07/A08/A11、E01/E05/E06；大对象、满盘、提交/GC 并发、direct 挂载及旧格式恢复分别验证。 |
| B09 原版 LayerFS 与真实 VM | U18/U19；REF-LAYERFS、REF-BOXLITE | 直接复用原版 LayerFS，分别比较 Store/fs/branch 与 workspace mount/exec/commit；同 Linux kernel、本地 FS、Docker/FUSE 条件保留。BoxLite 可选；product_durable 是额外发表协议。 | §10.2.1、34；A29/A34；发行版与主线、本地与 REST、磁盘/进程快照分开；guest 组合和断电承诺实测。 |
| B10 私有 Git 记忆与二进制引用 | U01；REF-NIX、REF-STORAGE、REF-GIT-WORKTREE、REF-PORTS | 默认 host-mounted Git；文本版本与二进制对象引用分开。采用 Git/LFS 的已定义部分，自定义指针另有格式；worker 工作树隔离不冒充权限隔离，认证/密钥不进入普通记忆。 | §10.3–10.5、35；A09/A11；GitHub/Gitea 私库认证，先有对象闭包再发表引用，冲突不静默覆盖。 |
| B11 SSH 身份和小模型远端工具 | U01/U04；REF-SSH、REF-OMP、REF-PORTS | 采用结构化资源/exec 思路，比较 keygen、公钥展示、host identity、读/搜索/patch/原子写/传输、流式/取消/连接复用。内容 hash 防陈旧修改是产品工具合同；WASM 签名桥、多跳等需分别实证。 | §11、17；A09/A14；路径含空格/中文、变更冲突、远端退出/断线/unknown、身份轮换和恢复均保持真实。 |
| B12 原始会话与全文回忆 | U01/U21；REF-PI、REF-FTS、REF-STORAGE、REF-CODEX | 原始事件与检索/摘要投影分离；FTS 可重建、返回出处与修订。中文单/双字、混合代码、已删除/旧事实处理是额外产品要求；不将摘要作为唯一真相。 | §12.1–12.3；A10/A11/A44；切窗后按 ID 重读、检索失败原文、索引损坏重建与一致备份。 |
| B13 budget compact 独立插件 | U21/U22；REF-CODEX、REF-OMP、REF-DURABLE | 对照 compact_token_budget.rs 的无模型摘要切窗与 history-notes 工具；OMP notes 路线另作比较。扩展本地窗口/任务恢复包、原文寻址与插件生命周期；保留不同 backend 的一致性差异。 | §12.3.1–12.3.2；A44/A45、H04；多次切窗/卸载/切换时不丢未完成调用、原文与消息，不重复执行。 |
| B14 snapcompact 独立插件 | U21/U22；REF-OMP | 精确对照 packages/snapcompact 的有界 Archive.text、截断/规范化、文字头尾+中段 PNG、再次 compact，以及 session-maintenance 的运行中处理。WASM 移植和本地视觉适配是增量；PNG 不是无损档案或 KV snapshot。 | §12.3；A44/A45；CJK/数字/路径/代码、视觉支持、图像 token/准确率/预填充成本及真实插件可替换性。 |
| B15 可替换 memory backend | U01/U06；REF-OMP、REF-QMD、REF-OPENVIKING、REF-HINDSIGHT | 以 retain/recall/reflect/update/forget/flush/health 与 provenance 适配；QMD 的 typed混合检索/取证/索引分离，以及OpenViking目录层次/当前全局检索/异步记忆分别对照。各backend额外模型可选；Hindsight尚为候选，不要求全搬上游。 | §3.2、12.3；A10/A11/A13；来源可追、索引变更/删除/重建、混合中文、模型费用、降级与冷启动。 |
| B16 主动记忆与选择性提醒 | U03；REF-PMA、REF-DURABLE | 采用 bank update 与选择提醒/No-op 两阶段。k=8 观察窗、触发频率、user_turn 历史行为分别记录；严格 transient 后续投影、本地节奏、统一 broker 属扩展，真实注入仍入档。 | §14.4、37；A12/A20、H04/H06；比较 silent/passive/always/selective，计全部成本和漏/误提醒。 |
| B17 探索遗忘与从 best 重启 | U03；REF-RESET、REF-LAYERFS、REF-DESIGN | 仅对探索 worker 重建可见 session/search state，从验证 best seed 继续；失败笔记为单独可选策略。原始档案、有效现实约束、外部效果保留；个人主体不整体失忆。 | §14.1–14.3；A12/A20；同预算多次比较、独立 judge、旧叙事确实不可见、最佳产物可复现。 |
| B18 递归研究和长期兴趣 | U01/U20；REF-PI、REF-RESET、REF-LAYERFS、REF-DESIGN | 低成本子任务 DAG、去重、有限工作单元与比较/交付份额属本产品策略；不是无限 spawn。主体可无限续配自发兴趣，不设置必须与用户有关或统一寿命上限。 | §5、13、14.5–14.6；A03/A20/A41；递归不新增物理预算，阶段交付和长期目标在重启/接管后延续。 |
| B19 原生模型后端与部署基线 | U01/U19/U21；REF-NINFER、REF-STRATA、REF-VLLM、REF-1CAT、REF-LIFE、REF-LLAMACPP、REF-DYNAMO | 以各版本/传输/配置透传原生能力；NInfer Snapshot/Replay、Strata Host/GPU/decode、vLLM hints、1Cat fork 分别对照。llama.cpp/Dynamo 为附加参考；用户四流128K、133120冗余优先。 | §13.6–13.7、22.1、25–27；A05/A32/A43；未知/忽略字段和失败回执可见，中英文教程由同一实际能力资料生成。 |
| B20 全局推理和自动 KV 分配 | U01/U03/U16/U19；REF-NINFER、REF-STRATA、REF-VLLM、REF-DESIGN | 物理模型域统一 admission；计算/驻留/Host backing 分开。C=1 有限 generation 轮转可工作，抢占为可选优化；50/50 是适合负载下自动算法结果，非固定比例或后端已提供保证。 | §13.1–13.7；A03–A05/A32/A43；主/child/memory/judge/远端 seat 均计账，alias不超限，缺 K 保守估计，负载变化可调整。 |
| B21 急消息、免打扰与过滤自修复 | U20；REF-OPENCLAW、REF-DURABLE、REF-DESIGN | 用户来信默认最高紧急，可打断 child；饥饿证据触达主体，由主体决定临时 DND，结束处理积压。垃圾规则可自设，独立 ingress/健康证据发现误屏蔽；不将未知后端取消当已释放槽。 | §13.8、15；A06/A38；高频打断、队列恢复、通配符误屏蔽、DND 到期与取消失联。 |
| B22 消息 provider 与开放渠道 | U01/U03/U04；REF-ASTRBOT、REF-OPENCLAW、REF-NAPCAT、REF-QQ、REF-ILINK、REF-FEISHU | 采用统一封套、渠道插件与异步入口。分别枚举接收/去重/附件/发送/编辑/回执/重连/缺口/账号；开放对话与手动 admin 为用户裁决。上游没有的 replay/幂等不自行承诺。 | §15–16、35.6；A06/A14；QQ/飞书/iLink 各自适配，消息到durable到主体到回复完整走通；unknown与平台限制保留。 |
| B23 联系人、声纹与 Life | U03/U07/U19/U20；REF-LIFE、REF-DESIGN | 复用稳定 person ID、录音内 rank、候选关联/确认/修订；跨渠道关系由经历与模型判断演进，不写死亲密度。声纹提供线索，不自动授予 admin。 | §6、15.2、20；A10/A35/A36；误识别拆分/更正、未知人、来源可回看；不复制真实私密样本作公开测试。 |
| B24 远端 Agent Seat 与终端语义 | U04/U19/U32；REF-OMP、REF-CLAUDE、REF-TMUX | OMP structured RPC、Claude SDK/CLI、现有 TUI 三路线分别枚举 submit/accept/result/settled/question/respond/interrupt/resume。保留属性、cursor、buffer、revision；capture文字不等于选中态。有限自主选择与多渠道求助为产品扩展。 | §17、35.5；A14/A30、V1 对应项、E04；相同文字不同高亮、resize/重连、旧问题、人工竞争输入、后台任务未settled、问答超时。 |
| B25 自主发现与补足能力 | U06/U09/U19/U32；REF-OMP、REF-DSH、REF-SKILLS-MCP、REF-DESIGN | 运行失败/观察缺口触发定位、查文档、配置/安装/编写插件或外部程序、验证后回到原任务。不是只生成代码；主子共享 registry、按 role 使用，所有模型子调用受broker。 | §18.7–18.8、35.8、38；A13/A30/A31；tmux高亮原任务实际完成，新插件可发现、会用、卸载/恢复正确。 |
| B26 插件认知和 27B 知识补偿 | U06；REF-TOOL-RESEARCH、REF-SKILLS-MCP、REF-QWEN、REF-OMP | 分别处理事实知识、候选召回、选择、构参、执行和长程状态；渐进披露、版本帮助、正反例与审计可替换。27B画像是用户假设，不能冒称五篇工具论文证明所有27B。 | §18.2–18.6；A13/A20；中英自然任务/干扰插件/旧版本/近似反例，记录真实完成、prefill/时延与全模型成本。 |
| B27 原生插件与 Skill MCP 生态 | U01/U04/U08；REF-CORDIS、REF-DSH、REF-SKILLS-MCP | 默认原生语义插件，兼容传统 Skill/MCP/市场；包格式、协议版本、能力/认证/取消/更新逐项声明。安装、健康、索引、教学、执行与结果核验构成闭环；市场声明不自动变权限。 | §7、18.1、36–38；A13/A18；安装后下一轮可发现，更新旧schema失效、卸载释放资源；不要求常用任务先安装大量MCP。 |
| B28 可恢复 Python Node Bun REPL | U05；REF-REPL、REF-NIX、REF-DURABLE | Python/WASI与Pyodide分开；Node/Bun in WASM仍是各自目标，不能以普通JS替代。代码/对象/VM恢复等级、包锁/ABI与外部副作用重放为本产品合同。 | §19；A15；重启/动态装包/依赖变化、不能序列化对象、已发生外部效果分别验收；逐实现标真实恢复等级。 |
| B29 电话、课堂和持续媒体 | U07；REF-LIFE、REF-DESIGN | 原始流、转写revision、热事件/语义片段、录音与声纹插件槽位由用户要求推导；具体媒体栈尚未选定，不虚构已有完整上游。完整工具委派和单主体继续成立。 | §20；A16；旁听中提问/工具调用/插话、丢帧/转写修订、录音和长期资料可追；选定媒体实现后登记协议来源。 |
| B30 多 OS Computer Use | U10/U18；REF-BOXLITE、REF-OS、REF-DESIGN | 真实桌面 provider、结构化观察+screen、输入lease和结果核验；Linux/Windows/macOS的平台条件分别表达，不以Windows container冒充桌面。后续选定RDP/RFB/CDP/AX实现再锁协议。 | §24、34；A22/A23/A29；过期坐标/DPI/焦点、竞争输入/人工接管、截图前后与实际效果、断线unknown。 |
| B31 在线优先与多节点汇合 | U20；REF-RAFT、REF-DURABLE、REF-DESIGN | 用户允许故障分支；采用真实成员投票/目标端fencing的限制，新增BranchJournal、停止落选主体与记忆/任务/效果汇合。不伪造分区下全球绝对单主。 | §5.4、22；A42；单/双/三节点、重新分区、旧节点恢复、重复外部效果、同Engine共享并发分别验证。 |
| B32 永久默认与自主归档遗忘 | U20；REF-STORAGE、REF-DURABLE、REF-DESIGN | 原始文字/录音/帧默认永久；主体自主选择归档、自有网盘迁移或真正erase。研究reset、语义forget、对象移动与删除分开；墓碑/副本/回滚后的删除一致性为产品合同。 | §12.4；A40；半上传、配额不足、索引/缓存/派生物、备份恢复与分支汇合不复活被删对象。 |
| B33 动态生成 UI 与能力降级 | U13；REF-DYNAMIC-UI、REF-SKILLS-MCP、REF-DESIGN | 采用生成式交互方向、widgets/actions/bridge原语；自有DSL/工具入口、surface能力、流式验证、跨渠道动作身份与降级是扩展。官方产品UI不能当公开compiler/DSL API。 | §21.4–21.9；A25/A26；Web/卡片/文本/语音均可完成意图，半成品动作不启用，过期/重复回答不重复效果。 |
| B34 Proto UI shadcn 与 compiler 接缝 | U01；REF-PROTO、REF-SHADCN、REF-DESIGN | 采用真实主线adapter与组件/token；每开发任务核验bump并pin。领域/状态/事件与renderer分离，compiler adapter可替换，不假设private compiler已稳定或必然更快。 | §21.2–21.3；A19/A26；升级/回退、焦点/草稿/无障碍/离线资源/插件UI与布局保持真实语义。 |
| B35 Magpie 视觉与生活首页 | U30/U37；REF-MAGPIE、REF-DUET、REF-DOTS、REF-DESIGN | 工作参照为已看三张截图：连续列表、就地控制、清楚焦点/选中、紧凑/完整视图连贯。主体首页按对话/关注/委派/成果设计；动态生命感是本产品目标，非静态截图证明。 | §21.1/21.10；A19、N07；参考图与真实实现状态并列，检查中文长文、小屏/键盘/主题/减少动画；无slogan，无强制监控大屏。 |
| B36 首版自开发自迁移闭环 | U32；REF-NAPCAT、REF-OMP、REF-CLAUDE、REF-TMUX、REF-NIX、REF-DURABLE、REF-GIT-WORKTREE、REF-DESIGN | 组合现有NapCat→找回现有seat→开发自己→候选验证→迁移/接管→继续任务；源/工件/运行身份绑定、A/B与外部控制独立是增量。允许host构建/执行但保留实际WASM家园。 | §35；V1-01–V1-15；普通任务、自主补插件、核心替换、失败恢复、消息与模型资源约束组合验收；产出patch不算完成。 |
| B37 操作级副作用与恢复合同 | U33；REF-CORDIS、REF-DURABLE、REF-STORAGE、REF-BOXLITE、REF-DESIGN | 静态operation声明→运行时实际mount/target/epoch绑定→intent/receipt→恢复计划。reset/restore/resume/cancel/compensate独立；不能把插件整体标rollback=true就覆盖外部副作用。 | §36；E01–E10；边界变化、迟到事实、新格式receipt、原生shell未知范围、补偿再次失败均可解释。 |
| B38 注入时序与 hook 兼容 | U34/U35；REF-FABRIC、REF-CORDIS、REF-COLIBRI、REF-DSH、REF-DURABLE、REF-DESIGN | 借鉴类型化事件、phase和相对顺序，新增阶段内DAG、候选环拒绝、event/context/hook版本分离、每次请求封口、在途旧schema绑定旧实现。保持主干，不新增另一套框架。 | §37；H01–H10；同phase/缺依赖/环、compact后装配、C=1 hook模型调用、热替换/卸载、迟到receipt与旧窗口。 |
| B39 内置自解释与设计追溯 | U36/U37；REF-OMP、REF-DESIGN | 对照 omp://入口、omp-protocol.ts、docs-index.ts与Philosophy；扩展运行build→源码→手册→哲学→refs/parity→当前能力。允许能力缺口/自开发自主触发，非永久提示塞满源码。 | §38、39；K01–K09；回答自身问题有版本出处、离线最小索引、候选/实装/启用分开、不泄密，主循环仍不直接文件I/O。 |
| B40 人格、灵魂与产品命名 | U02/U20/U23–U31/U38/U39；REF-USER、REF-DOTS、REF-DUET、REF-MUSE、REF-NAME | 人格/可自改灵魂与统一知识来自用户；产品/实例名分开，无slogan。正式框架/产品名为 Shenko（深刻）；Always Online 的命名来源、shen1-kʻo4 与 ㄕㄣ ㄎㄜˋ 按 U39 记录。旧候选和重名研究仅留历史，不产生当前命名待批项。 | §1、6、21；A36/A46；改名/升级/接管不换identity；人格不损坏工具schema或替代当前用户产品合同。 |
| B41 自主开发规范与完整度证据 | U01/U12/U37；REF-USER、REF-DESIGN | 开发任务获授权后自主分解、实现、验证、修复、提交和在目标范围内集成/迁移；无逐PR/阶段人类gate。每项idea保存来源、采用差异、行为用例与证据，未实现如实标明。 | §29、39–40；N01–N08；用例映射全、证据可复查、不中途缩成demo，真实权限不足仅阻断相关动作。 |

## 39.4 较细的 memory 对照种子

QMD 与 OpenViking 的参考不能只剩“混合检索”和“分层记忆”。本次固定基线进一步拆成以下子行为；其他范围按同样粒度增加，不要求每次普通修复重做全网研究。

| 子行为 | 固定上游合同 | 本产品比较与增量 |
| --- | --- | --- |
| B15.Q1 检索计划 | QMD 86d2365：原查询进入词法与向量；lex 扩展走词法，vec/hyde 走向量，再融合和可选重排；README 与 docs/SYNTAX.md | 分别测试精确标识符、模糊意图和中英混合；记录实际执行阶段及成本，不硬抄融合权重或模型 |
| B15.Q2 索引更新 | 同版 README 的 Indexing：update 报告 indexed/updated/unchanged/removed/needsEmbedding；embed 独立 | 识别“源已更新、向量未更新”，模型或索引版本变化可重建；不把扫描完成当所有记忆就绪 |
| B15.Q3 命中后取证 | 同版 get/multi-get 按 path/docid 和行范围取内容，collection context 返回背景 | 增加原始事件 ID、source revision/hash 和准确片段；索引或短 docid 不承担永久真相源 |
| B15.O1 分层披露 | OpenViking 824e9c7 的 03-context-layers.md：L0/L1 是目录 sidecar，可以只存在一层；有来源和 freshness；L2 可能是解析内容 | 处理缺层、陈旧摘要和按需读原文；原始录音/附件永久留存另由本产品保证 |
| B15.O2 当前检索 | 同版 07-retrieval.md：每 query 全局向量检索，URI/type/permissions/metadata/level 限定范围；find 与带意图规划的 search 分开 | 不实现已经移除的递归目录队列来冒充当前等价；跨 query 去重、总 limit 和本地预算显式处理 |
| B15.O3 异步派生记忆 | 同版 08-session.md：先归档/入队返回 accepted 与 task_id，后台摘要/提取，完成后有 memory_diff；no-op 可不产生 task | 比较 accepted/running/completed/failed/no-op 与实际变更，原轨迹先存；异步模型请求仍进入统一资源治理 |

这些借鉴不要求安装 QMD/OpenViking，也不要求移植其全部实现。若选装，上游内部 embedding、rerank、规划、摘要和提取产生的模型请求与资源占用仍须可计账；尚不能接入全局 broker 的路径明确报告，而不是悄悄增加一套不受控推理。恢复、授权或数据保留不能仅凭上游项目名推断。

## 39.5 完整度怎样比较

采用范围先明确可观察行为的分母，包括适用的成功、失败、取消、重试、恢复、升级、版本差异与组合流程。之后分别报告“来源已核对”“规格已覆盖”“代码已实现”“目标环境已验证”；不混成一个漂亮的完成百分比。某行为有意不同，保留原因、代价与本产品验收，可以完成比较，却不能算作已实现上游原行为。

目前 128/128 需求已有来源和行为落点，这是规格追踪覆盖。当前不存在产品实现或目标机器测试证据，因此不填写实现通过率。只列接口、返回 mock、按钮能点击、合法 JSON 或源码存在，都不足以证明相关用户旅程完成。性能与质量比较固定任务、模型/量化、模板、总预算和版本，计入辅助模型、冷 prefill、取消损失及重建成本；论文最高增益不写成产品承诺。

上游单测可以复用为种子，但仅在语义与平台条件相符时采用；不能复制上游自己的盲区。行为差异用独立回放、可重复任务或目标设备检查验证。视觉以具体来源画面、对应真实状态、交互与适应性比较，不以像素相同作为设计完成标准。参考的是所选体验与质量，页面内容仍服从本产品。

## 39.6 维护与自解释

进入开发后，可将同一记录物化为 requirements、references、behavior-parity 等机器可读文件，并生成文档、测试索引和 §38 自解释索引。格式由开发 Agent 自主选择，不要求先引入一套管理服务。代码的非显然行为可用稳定 B/Ref ID 连接设计，PR/变更记录仅列受影响的需求、差异和证据，普通纠错不写长篇新 ADR。

相关上游变化时，Agent 自主比较受影响的行为、更新固定基线、调整实现和验证；无需每次请人批准引用或升级。Proto-UI 继续执行用户指定的每开发任务核验/bump 主线规则；不将该特殊要求扩张为所有依赖每次都必须升级。复用代码或素材时保存来源与相应版权/许可说明；借鉴思想和复制实现分别登记。

缺少来源、过期定位、未闭合差异与未验证效果是可处理的工作项。Agent 能补证据就自主补；只有真实缺失的项目身份、用户信息或外部访问阻止相关步骤时才询问，其他工作继续。来源登记不会产生新的固定人类 gate。


# 40 开发 Agent 自主工作规范

## 40.1 默认连续完成已授权目标

本章适用于开发 Shenko 的 Agent，也适用于 Shenko 委派的自开发 worker；依据 U37、U09、U15、U17、U20、U32 与 B41。当前任务仍为 SPEC；用户以后要求开始实现某个目标时，该目标的授权在会话压缩、worker 更换、失败恢复、分支切换和版本升级后持续有效，不能再次退回“先问是否继续”的流程。

进入已授权开发后，Agent 自主完成必要研究、任务分解、设计、实现、调试、验证、文档、提交和集成；目标范围包括部署/迁移时，继续完成相应环境的部署、迁移和运行核验。默认不设置逐任务、逐 commit、逐 PR、逐依赖更新、逐界面或逐发布阶段的人类 gate。PR 可以用于记录和协作，不自动等于等待人类审稿。目标完成前有可解决问题就继续修复；并行工作能提高质量或速度时自主委派。

真实账户权限、用户明确范围和仓库已经强制执行的机制按实际状态处理；不能伪造授权或绕过访问控制。既有长期授权足以覆盖的动作不重问。确实缺少用户才能提供的信息、凭据或新增范围时，只暂停受影响的动作，说明具体缺什么、从何处产生约束，完成其他工作后提出最小问题。工具/平台拒绝不是抽象的“需要安全审批”；如实报告拒绝的动作和原因，优先采取授权范围内可行的替代路径。

## 40.2 实现选择与未决问题

依赖、内部类型、算法候选、插件边界、构建位置、分支/worktree、测试方法、迁移步骤与重构路径由开发 Agent 自主决定。默认方案可以采用、验证和替换，不因尚未收到用户逐项“批准”而停工。D 项属于有待明确的产品问题，能够通过现有合同、源码、运行证据或可恢复假设继续的工作自主推进；未决项不成为全项目暂停条件。

保持用户已经确定的产品目标：真正永续串行主体、主循环不直接文件操作、共享能力生态与不同 role、任何代码可自改、真实 WASM 家园、原生推理能力、自动 KV 分配、完整首版自开发闭环、两种独立 compact 插件、默认开放对话、手动管理员与无 slogan。内部实现可以大改；不能用“优化”偷偷替换这些要求，或以研究难度为理由删除后期正式目标。用户新裁决优先，随之更新规格和验收。

允许 schema、插件 API、hook 顺序、存储格式、harness、UI renderer 和开发工具发生 breaking change。按 §9、§36–37 维护可运行候选、迁移/接续方法和所需恢复闭包，不要求永久支持旧接口。磁盘成本优先的升级方案可由 Agent 自行选择；升级后的新增事实不能靠恢复旧软件而抹去。

## 40.3 交付完整行为

每项任务先明确目标旅程、适用 profile 与已承诺范围，再从 §32/§39 找到用户要求、上游行为、差异和验收。Agent 可自主拆分实现顺序，但不能把当前承诺的完整能力降成演示、静态页面、空接口或“之后再接”。真实 WASM、实际认证私库、真实已有 TUI 控制、正确高亮观察、可靠接续等具名目标，以目标本身验收。

完整程度比较只涵盖所选择的上游行为，不要求复制其整套产品。对已经采用的能力，适用的错误、取消、恢复、升级和组合行为不能无记录地省略。未采用的上游功能可以说明理由；用户已经要求的功能仍保留在需求和阶段路线中，不能擅自标为不适用来提升通过率。

完成状态区分设计已定、代码存在、可构建、模拟通过、真实环境通过与长期运行通过。任务交付带实际工件/源码版本、运行环境、相关证据与剩余差异；文档不得把计划写成已实现。源码阅读、README 声明和模型自评不替代执行证据。

## 40.4 工程与插件约定

Rust 核心只承担必要的装配、运行与最小恢复支撑。新业务行为先落到可替换插件/provider；若确需进入核心，记录为何现有扩展边界无法表达、常驻成本和后续可替换路径，不要求人类逐次批准结构选择。插件语言与构建环境按目标设备/ABI 自主选择；不得为了移植一项工具无声增加所有设备必选的浏览器、Node 或完整容器守护进程。

版本化边界包含 schema、hook、能力、runtime/imports、模型/template、数据格式与恢复器。调用绑定实际执行版本；能力声明与运行效果分开。结构化错误至少保留来源、重试条件、取消/超时/unknown 与真实 receipt；不要把这些全部变成一段无法解释的字符串。状态提交遵循既有事务/ledger 合同；等待网络或模型时不占住孩子必需的推理许可或数据库写锁。

工具既要能用，也要容易正确使用。新增插件同时更新 capability card、准确版本的 schema/帮助、适用与不适用例、资源/副作用范围和自解释索引。发现失败时，优先判断是能力缺失、观察不足、知识不足、检索漏召回还是实现错误；修复后回到原始任务证明有效。用户已经设置的 role 边界继续成立，主体通过意图和委派使用能力。

依赖与构建可复现，发布记录真实锁定版本。Proto-UI 依 §21.2 每开发任务核验并 bump 最新主线，合并/发布前再次核验；升级失败由 Agent 诊断、修复或保留健康运行版本，不静默写“已升级”。其他依赖根据任务需要与相关变化自主更新，不要求每次全部追新。

## 40.5 自动验证与相称成本

技术验收可以自动阻止一个失败候选被宣称成功，但通过后的下一步由 Agent 自主推进，不默认插入人类签字。验证范围由受影响的合同和具体剩余风险决定：低影响文案修正无需重新跑全部模型评测，某个 UI 细节修复无需启动所有平台，普通实现选择无需另外开一轮 Deep Research。

代码检查、构建、类型/格式检查、已有回归、协议 conformance、回放、故障注入、真实场景和人工可查看的证据按需组合。独立证据指结论不能只来自作者自述；确定性工具、另一个 worker 的复现或独立样本均可承担相应验证，不强制每次小改动再启动 LLM reviewer。

改变插件发现/工具选择并宣称改善、修改研究/记忆策略并宣称收益时，用相关独立任务与等预算对照；不得把失败例子背进 prompt 后当泛化证明。恢复性或性能声明必须对应真实存储、设备、模型和后端条件。扩展验证只为解决具体风险或满足适用的既有合同，充分后继续交付。

检查若揭示实现错误，修复实现；若检查或原合同有误，记录原失败、理由和修订后证据。不得删失败用例、缩小分母、伪造输出、隐藏真实剩余差异来得到“通过”。自动验证本身可以修正和演进，仍须保持诚实可复查。

## 40.6 自主 UI 实现与视觉验收

Claude Opus 或其他后续被指定的设计 Agent 依据 §21 的功能/状态合同及 Magpie 参照，自主完成布局、组件、色彩、排版与动效，不要求用户逐屏批准。参考画面与实现画面应能找到对应的状态和比较理由；采用清楚层级、就地操作和连贯视图，不照搬与个人主体无关的配置页内容。

实际运行相关界面并检查中文长文/长名称、窄屏、明暗主题、键盘焦点、选择/悬浮/禁用、加载/空态/错误、流式变化与 reduced-motion 中适用的状态。页面不能只在静态截图中成立；动态 UI、任务/消息/升级结果和真实服务状态一致。达到可观察的质量要求后继续集成，留下可查看证据，不把“等人看看美不美”设成默认必经步骤。

## 40.7 文档与任务接续

仓库的开发说明、AGENTS.md、插件模板、PR/任务模板与自解释索引应引用本章和同一需求/来源记录，避免多份矛盾的审批制度。开发 Agent 可自主更新这些文件以反映已授权的实现方式；不得写出禁止用户所要求的自主性的永久条款。

工作 checkpoint 保存目标、已完成事实、当前工件、重要决定与来源、未完成项、验证命令/结果、资源和在途副作用。后继 worker 可从原始证据恢复，无需依赖前一 worker 的全部对话。报告说明实际完成和必要限制，不在仍有授权内工作可做时用“是否继续”结束任务。NInfer、Strata 和关键插件的中英文教程继续由真实接口、配置和复现步骤维护。

## 40.8 规范验收

这些检查针对任务或相关变更的真实范围，可由 Agent 自动完成；不是每个小改动一律跑八套全量测试。

| 编号 | 检查 | 成立条件 |
| --- | --- | --- |
| N01 | 授权接续 | 压缩/重启/换 worker/换分支后仍能继续已授权目标，不重复请求逐项许可 |
| N02 | 自动推进 | 合理默认、实现、验证、修复和范围内集成/迁移可连续完成；不因每个 PR/阶段自动等人 |
| N03 | 需求和来源覆盖 | 新增/变化行为有 R/U/B/Ref 落点，用户原创与推导不伪归上游，实际采用范围明确 |
| N04 | 完整行为 | 当前阶段承诺的旅程及适用异常/恢复完成；未完成项真实列出，不以 mock 或接口代替 |
| N05 | 证据可靠 | 工件/环境/方法/结果可对应，未执行不写通过；失败不通过删用例或缩分母掩盖 |
| N06 | 成本相称 | 只扩展足以解决具体风险的验证；不同模型/后端、辅助调用与并发总资源真实计账 |
| N07 | 视觉与实际使用 | 有具体 Magpie 参照和真实界面状态证据，中文/窄屏/焦点/适用动态状态成立，无固定人工逐屏审批 |
| N08 | 自主变更与恢复 | Agent 可改任意代码/接口/格式，候选、接续与恢复符合真实效果边界；无永久不可改核心或默认逐升级审批 |

