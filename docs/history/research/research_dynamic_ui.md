# Dynamic Response / Intelligent UI：官方核验与个人 Agent 动态界面规格补充

核验日期：2026-10-22 UTC / 2026-10-23 Asia/Shanghai。只做公开资料研究和产品规格，不进入开发，不修改主稿。以下第 1–3 节区分可证实的竞品事实；第 4 节以后是本产品建议，不冒充 OpenAI 已公开协议。

## 1. 先核对名称与可复用边界

用户提出的 **Dynamic Response** 需求必须保留：Agent 根据任务和目标界面的实际能力，自主组织布局、图形、表单与可操作组件，并能降级到该渠道可用的表现形式。

本轮对官方文档、发布及帮助中心进行精确名称和语义检索，**没有确认 OpenAI 将这项功能正式命名为 “Dynamic Response”**。与用户描述对应的公开产品方向，官方名称是 **Intelligent UI**，发布于 **2026-10-07**。不能把未找到这一名称写成“该能力不存在”，也不应将用户的需求删除或偷换成普通 Responses API。

OpenAI 发布页说明：模型组合文字、视觉和交互；底层使用可流式呈现的原生组件库及随生成处理界面的编译器。官方也描述了模型对内容、布局和交互选择的训练。这证明“模型自主组织界面 + 渐进呈现”的产品方向；**没有在所查官方来源中找到可直接嵌入我们项目的原生组件 DSL、独立编译器 SDK、开源运行时或名为 Dynamic Response 的公开 API 合同**。

主要来源：[GPT-6 and Intelligent UI for everyone](https://openai.com/index/gpt-6-for-everyone/)，2026-10-07，ref `turn98view1`。命名核验另见 [Intelligent UI in ChatGPT](https://help.openai.com/en/articles/20001598-intelligent-ui-in-chatgpt)，ref `turn98view0`。

**产品决策建议：**正式 SPEC 可将本功能称为“动态生成式响应 / Dynamic UI”；竞品来源写 Intelligent UI。我们自行定义可替换的语义、DSL、动作和 renderer 协议；不将 ChatGPT 私有呈现能力当作本地模型天然具备的 API，也不依赖 OpenAI 才能实现。

## 2. 已核验的 Intelligent UI 能力与限制

| 项目 | 当前官方资料确认的范围 | 对本产品的含义 |
|---|---|---|
| 响应形态 | 文本、图形、可点击按钮、表单、图表及会话内交互体验；简单问题仍可用文本 | 动态 UI 是按任务选择，不要求每条消息变成卡片 |
| 生成过程 | 原生可流式组件 + compiler，界面可随模型生成逐步出现 | 需要渐进发布和稳定布局；不等于必须输出任意网页源码 |
| 用户偏好 | 可用个人指令/本轮要求引导样式；布局未必固定 | 本产品应该有明确的文本/简洁/自动/丰富偏好 |
| 状态 | 部分组件刷新当前 thread 可保留状态；不跨 thread 保留 | 本产品不能直接继承这种限制；持久任务/跨渠道状态需自己的账本 |
| 产品范围 | 帮助页明确 Chat 可用，Work 与 Voice 不可用 | 本产品语音降级及跨渠道交互是自己的目标，不引用它当已被竞品实现 |
| 推理范围 | Instant 至 Extra High；Pro reasoning 不支持 | 这是该产品当前范围，不应变成本产品模型限制 |

前两行见发布页 `turn98view1`；后四行见 [Intelligent UI 帮助](https://help.openai.com/en/articles/20001598-intelligent-ui-in-chatgpt)，`turn98view0`。公开帮助页本轮显示 Updated yesterday，应按查阅日期理解。

另一个 [当前模型帮助页](https://help.openai.com/en/articles/20001354-gpt-6-and-other-models-in-chatgpt)（`turn98view2`）说明支持 web 与更新过的受支持客户端、旧桌面 app 不支持、无独立 Intelligent UI 配额。官方各页对 Free/Go 的模型描述和“Layout and visuals”设置文案存在不一致，本轮不将具体免费档模型/开关路径写入本产品结论。确定的是：公开设置只能减少视觉元素，不能据此承诺全体验纯文本。

## 3. 四个容易混淆的层次

| 名称 | 公开可核验的职责 | 不能据此推出 |
|---|---|---|
| ChatGPT Intelligent UI | ChatGPT 自己生成/组合的原生响应体验 | 任意 API 输出已含可移植原生 UI；独立 compiler 已开放 |
| Apps SDK / 当前 Plugins UI | 开发者通过 MCP 工具关联界面资源，在支持的 host 中呈现自定义组件 | 与 Intelligent UI 的内部原生编译协议相同 |
| ChatKit widgets/actions | 开发者在自己的聊天产品使用声明式 widget 及动作事件 | 自动获得 ChatGPT 的整套界面决策训练与组件能力 |
| Responses / Structured Outputs / tool calling | 模型通信、结构化输出和调用工具的机制 | 单凭模型 API 就会自动为 QQ、飞书或本地 Web 渲染适当界面 |

### 3.1 Apps SDK 当前公开路径的变化

本轮直接打开 `https://developers.openai.com/apps-sdk/`，重定向到 `https://developers.openai.com/plugins`，ref `turn102view1`。文档重组不表示同名 SDK 包全部消失；当前 `@openai/apps-sdk-ui` 仍被列为可选组件库。

当前官方 [Add UI to your MCP server](https://developers.openai.com/plugins/build/chatgpt-ui)（`turn94view0`、`turn96view3`）推荐 MCP Apps 标准：工具通过 `_meta.ui.resourceUri` 关联资源，组件以 iframe 承载，使用 `ui/*` JSON-RPC / postMessage 桥。文档要求工具在无 UI 的客户端仍有用，按需要的具体能力探测扩展并提供 fallback，而非按 host 名称硬编码。`window.openai` 保留为兼容/扩展层。

这正适合借鉴“能力协商 + 可选 UI”，但它没有证明 ChatGPT 原生生成式 UI 的内部 DSL 可移植。

### 3.2 状态、模型可见信息和权限是不同概念

官方 [Plugin reference](https://developers.openai.com/plugins/reference)（`turn101view1`、`turn102view2`）区分 `structuredContent/content` 与只交给组件的 `_meta`，另有 widget 状态快照及组件调用工具/发送 follow-up 的接口。工具 annotations 是提示，服务端仍须执行授权。对本产品可借鉴这一分离，但“模型不可见”不等于“用户看不到”或“已加密”。

### 3.3 ChatKit 提供另一组公开可借鉴原语

[ChatKit widgets](https://developers.openai.com/api/docs/guides/chatkit-widgets)（`turn101view2`）提供结构化组件、JSON、按钮和表单；[ChatKit actions](https://developers.openai.com/api/docs/guides/chatkit-actions)（`turn101view3`）允许 UI 动作触发服务器处理、更新 widget 或模型响应，并明确客户端 action/payload 应视为不可信数据。它可以作为我们 typed UI/action 协议的参考，不能替代唯一主体和 durable inbox 的架构。

[Structured Outputs](https://developers.openai.com/api/docs/guides/structured-outputs)（`turn102view0`）支持按 JSON Schema 约束输出；这是生成 typed DSL 的一种模型适配路径。结构合规不证明权限正确、业务事实真实或目标渠道能渲染，服务端语义校验仍然必要。

## 4. 本产品应拥有的完整能力

目标不是让 Agent 每次套一个固定卡片模板，也不是强迫它先开发网页再回答。它应能理解“用户此刻需要比较、选择、理解因果、查看进度、填写信息、操作一件事”，在当前 surface 能力和偏好内动态组合界面；普通解释与简单问答仍自然使用文字。

动态 UI 包括临时响应、可持续交互的任务面板和可保存的小工具。临时展示与持久业务状态分开；保存为长期应用是显式能力升级，不将每个一次性计算器都变成永远运行的服务。

### 4.1 可插拔职责

| 角色 | 职责 | 应避免的耦合 |
|---|---|---|
| SurfaceCapabilityProvider | 报告某渠道/客户端/会话实际可显示与可交互的能力 | 不根据“QQ”“Web”标签猜能力 |
| UI composition worker / policy | 根据任务语义、数据、偏好和能力组合 UI | 不成为独立常驻人格，不直接越过主体委派 |
| UiProjection / typed UI document | 保留语义节点、数据引用、布局约束、动作引用和必要文字 | 不把 React props 当永久领域模型 |
| Renderer / compiler adapter | 将同一语义投影到 Proto-UI、聊天卡片、文本、语音等 | 不把当前 compiler 内部 AST 写入所有业务插件 |
| Action dispatcher + ledger | 验证主体身份、上下文版本、授权、提交与结果 | 不把任意按钮 payload 当可信工具命令 |
| Channel delivery adapter | 持久投递、更新、回执、失败与替代形式 | 不承诺平台没有提供的编辑/幂等能力 |

这些是可替换插件契约。核心保留最低限度的协议注册、能力句柄与事件承载；不常驻完整组件库、浏览器或 JS 工具链。

## 5. 能力协商的最小完整合同

能力属于**某个具体投递目标**：provider/account + conversation/thread + client/session + audience scope，带 capability revision 和有效期。群与私聊、不同账户认证档、桌面与手机可能不同；重新连接或客户端切换需重新确认。

应协商的维度：

- **表达**：plain text、Markdown 子集、表格、图片/SVG、图表、音视频、文件、原生卡片、按钮、表单、可执行 sandbox component。
- **交互**：选择/输入/多选/拖拽、callback 身份验证、表单提交、可取消、返回上下文、键盘与触屏。
- **更新**：不可修改的消息、编辑、patch、追加、流式、回执、删除/失效、最大大小/条数与实际频率限制。
- **状态**：只在当前挂载保留、服务端持久、跨设备恢复、跨渠道共同业务状态、离线输入能力。
- **呈现环境**：viewport、DPI、语言、方向、主题、字体放大、屏幕阅读器、减少动画；声道则给出语音/文字回退能力。
- **执行/联网**：允许的 component runtime、可用计算能力、资源访问范围、资源大小及网络策略。能展示按钮不自动意味着有权限执行按钮所指动作。
- **偏好**：本轮的纯文本要求、长期简洁/自动/丰富设置、低流量/低功耗、是否接受可选的外链丰富视图。

模型只接收短小能力摘要和版本化组件索引。需要某组件时再按需加载 schema、例子及约束，避免把每个平台的完整 UI DSL 长期塞进主 system。稳定描述可以缓存，动态 capability revision 和目标受众放在请求后缀。

未知能力按可确认的基础表达处理；不能把猜测呈现为已支持。平台临时限流、附件不可达、卡片 callback 不可用是运行时能力变化，应进入新的投影/投递决策。

## 6. 两条生成入口，共用一个语义文档

### 6.1 路径 A：给模型 DSL / 元素语法

UI worker 获得当前可用节点、schema/语法、小样例与限制，生成 typed document。可使用支持的模型结构化输出、grammar decoding 或普通文本 DSL + validator。不能要求所有本地模型都具备同一供应商的严格 JSON 能力。

推荐语义层至少区分：

- `purpose`：比较、解释、选择、编辑、观察进度、模拟等意图。
- `content`：正文、证据引用、结构化数据、图形/媒体引用。
- `layout`：分组、顺序、强调、可响应约束；可自由组合受支持节点，不局限一组固定业务模板。
- `interaction`：已注册 action 的引用和输入 schema。
- `fallback`：最低限度可读信息和可完成当前意图的替代交互。
- `scope/version`：受众、任务、schema/组件版本、状态/动作版本。

这是一种产品数据模型建议，名称尚可调整。节点类型与组件库允许插件扩展、自我改进及版本升级，不是永久冻结的白名单。

### 6.2 路径 B：暴露 UI 组合工具

可以为 UI worker 提供拟定的 `describe_surface / compose / validate / publish / patch / retire` 结构化工具；工具返回 projection ID、验证错误、有效版本和投递状态。它们最终产生与 DSL 路径相同的语义文档和动作账本，避免两套不一致的状态系统。

对很小的本地模型，可先给高层语义工具，如“比较这三个候选”“收集这几个字段”，由 renderer policy 生成首版。复杂时再展开节点级 DSL。可读语法/工具由每个能力插件提供；主循环通常只需知道能委派何种交互任务。

### 6.3 主循环职责不因动态 UI 被扩大

唯一主体决定要表达什么、委派谁，以及需要的行动范围。UI/对话子 Agent 生成、校验和发布实际响应。主体不通过一个 `ui.run_script` 的伪装接口重新获得直接文件写入、shell 或浏览器控制。

生成 UI 所需的模型推理与修复调用走统一 broker，并计入原任务预算。没有必要让每个布局 patch 都回主循环再申请一次推理；已授权的 worker 可完成有限组合任务。

## 7. 状态、动作与同一主体的输入闭环

### 7.1 三种状态分开

1. **展示状态**：展开哪组内容、图表缩放、焦点、正在编辑但未提交的字段。可以在客户端保留；无需每次触发 LLM。
2. **交互对象状态**：表单草稿、共享清单、小工具输入、当前提案。需要时持久化并明确作用于某人/某群/某任务。
3. **业务事实与副作用**：任务是否已取消、消息是否发送、某候选是否启用。以服务端任务/效果账本为准，不能只看前端按钮颜色。

持久 UI 不等于不可删除历史。继承主规格的保留、删除、派生缓存清除和受众权限规则；重放历史可以重建当时展示，但默认不重新开放已经过期的动作。

### 7.2 持久 action 身份与版本

每个可执行 action 绑定 interaction/task ID、action ID、来源 surface/message、允许的 actor/scope、输入 schema、有效期/nonce、当前业务或提案 revision、语义 payload hash、效果类型与所需授权。

**render revision 与 action revision 应分开。** 改间距/颜色不必让所有按钮失效；改收件人、价格、目标节点、任务范围或候选版本，必须产生新的动作绑定。服务端依据当前有效状态校验，而不是相信客户端发来的旧 label 或“已批准”字段。

按钮、文本回复“选 2”、语音选择和其他渠道卡片可指向同一 decision/action identity。重复投递只得出一次有效决定；另一次若是改变主意则作为新事件，不能再次执行旧副作用。具体 external effect 仍按主规格的 idempotent/unknown 分类，不伪造 exactly-once。

### 7.3 交互回到唯一主 loop

用户交互先持久写入标准 inbox：稳定来源、已验证 actor、view/action identity、revision、输入值、时间和关联任务。相关人类意图进入唯一主 thought loop 的序列，不新建“UI 主脑”。

纯展示操作可以在 renderer 本地处理；服务端 action 也可以在主体已签发的有限委派范围内，由 worker 执行并回报。新的长期承诺、目标变化和超出委派范围的要求回到主体决策。这样既保留唯一人格，又不使每次展开列表都消耗推理。

发送同一交互到多个渠道时，共享业务状态，但分别生成符合各受众的展示。一个私聊的自由文本回答不因共享 decision ID 被原文同步到公开群。

## 8. 渐进生成不能破坏可用性

流式 UI 以验证过的节点或 patch 边界提交，保留稳定 node ID、状态、焦点和布局约束；不能把未闭合 DSL、临时默认值或未知目标按钮直接变成可点击动作。文本和只读部分可逐步出现，有副作用的 action 仅在其当前语义绑定完整后启用。

布局不应在用户即将点击时把按钮换位或改变含义。已有交互对象发生实质修改要产生可识别版本；更新失败保留最后一个有效视图和可读解释。模型完成、UI 校验完成、投递完成、用户查看、动作完成是不同状态。

生成小工具可以有本地确定性计算和动画，不必每次拖动 slider 调 LLM。确需新的模型分析时采用显式提交或适当合并，并受同一 broker 调度；渲染/重连失败也不能无限循环生成新 UI。

## 9. 跨渠道降级：保留意图，不强制浏览器

下面是产品应支持的投影策略；是否支持某一原生卡片/按钮，必须来自具体 adapter 的能力声明，不凭平台名称承诺。

| 交互意图 | 丰富 Web / Proto UI | 有原生卡片与 callback 的渠道 | 文本/媒体为主的渠道 | 语音或终端 |
|---|---|---|---|---|
| 比较多个方案 | 并排比较、可筛选表格、细节展开 | 可用的卡片/字段组合 | 简洁表格或逐项差异，可附静态图 | 先讲关键差异，可逐项追问 |
| 作一个选择 | 绑定当前提案的按钮/选项 | 原生按钮或选择卡片 | 编号选项 + task/decision 对应回复 | 自然语言或键盘选择，复用同一决策身份 |
| 填写资料 | typed form、即时校验 | 平台支持的字段/表单 | 按语义分步收集，保留草稿和错误提示 | 少量字段逐步问，避免一次读长表单 |
| 查看研究/工作进度 | 可展开任务和证据图 | 状态卡片或更新消息 | 有意义的简短进度 + 可请求细节 | 简报与按需展开，不读全部日志 |
| 探索数值/因果 | slider、chart、小型模拟 | 支持的有限参数选择 | 静态图 + 数据 + “改为 X 再算”的文本交互 | 口头/命令调整参数后给结果 |
| 远程 Agent 待决问题 | 原始问题与建议、版本化行动 | 同义选项卡 | 题干/推荐/编号，回复可回灌原 seat | 语音/终端选项，不泄露私密远端内容 |

当某渠道能力不足，优先在当前渠道完成语义上可完成的任务。丰富 Web 页面、认证链接或文件可作为补充，而非让所有 QQ/微信/飞书消息都变成“请打开浏览器”。没有图片能力时，静态图也必须有文字信息；无 callback 时不能画一个看起来能点、实际无法操作的假按钮。

降级不能丢失用户需要作决定的关键条件、单位、来源或动作后果。复杂任务确实无法在当前媒介完成时，说明需要的能力并由主体选择已有可达界面；不能静默把私密交互发布到公网。

## 10. Proto-UI、shadcn 与未来 compiler 的连接

当前目标继续采用用户指定的 Proto-UI 主线 + shadcn，实际版本沿用主规格的核验记录，不能把未发布的 RC 当现有版本。本研究不另行捏造版本号。

动态 UI 的稳定边界应是 **语义文档 + 行为合同 + 设计 token + action/state 协议**。当前 Proto runtime/React adapter 和未来 Proto compiler adapter 分别消费这一边界；编译器的内部结构不进入记忆、TaskSeat、消息协议或业务插件。

每轮开发按用户要求核验并 bump 最新主线，锁确切 commit/package/schema 版本，记录兼容变化与通过的验收。升级 renderer/compiler 不应使历史的持久 UI 无法读、动作错绑或权限扩大；若旧文档需要迁移，迁移应可解释并保留只读回看/文本回退。

预留 compiler 插桩：节点/事件来源映射、编译错误到模型可修复诊断、首段呈现时间、完整呈现时间、输出大小、节点数、交互延迟、状态迁移与降级原因。性能是否更好以实测决定，不预先承诺 compiler 必然更小更快。

“let layout talks”在这里是：用布局表达信息的关系、当前注意与可选行动；动态响应仍使用可识别的主体语气、控件含义、主题和无障碍方式。不要把整套应用导航/危险操作位置随每轮模型输出任意改换。

## 11. 本地隐私、可扩展性与低成本

生成式 UI 默认可采用 typed nodes，复杂自定义体验允许通过可安装/自生成的受限 component plugin 扩展。不能把表达灵活性等同于对任意远程 JS、凭据或主机文件的隐式授权；组件也不能将正文里的字符串自动执行为工具命令。

借鉴公开 MCP UI 的资源/网络范围声明，但我们应保持本地可离线资源、受控 artifact handle 和实际 audience ACL。完整表单数据、截图、私密联系人和密钥不应因图表资源托管、远程字体或公开调试服务被上传。需要网络/数据连接的小工具可以使用已授权的 provider，目的地与作用范围清楚。

UI 状态要能选择哪些内容供模型理解、哪些只是展示资料；给 renderer 的数据也须符合受众范围。可被模型读取的 UI 内容是用户/工具数据，不升级为主 system。动态组件自改沿用任何层自改的候选验收与版本激活机制。

资源应有设备 profile 预算：组件/节点数量、数据行数、图像/资源大小、后台更新率、执行时间/内存、缓存及持久历史规模。可共享确定性模板/编译产物，避免反复让小模型从零写 CSS。UI 层的简单校验/布局不需要 LLM；模型参与的组合、内容变更和修复才进入 broker，全部计入原任务预算。

## 12. 必须加入的验收场景

1. **同一回答跨三种界面。** Web 呈现交互比较，具备能力的 IM 呈现选择卡片，纯文本渠道呈现编号选项；数据、含义及 decision ID 一致。
2. **不强制跳浏览器。** 文本渠道可以完成选择、补充字段和取得结果；丰富页面只是补充。
3. **旧按钮失效。** 用户打开昨天的候选部署卡，目标版本今天已改变；旧动作被识别，给出当前状态，不部署错误候选。
4. **只有外观变化。** 调整颜色/布局保持同一有效 action identity，未提交输入不丢失；实质 payload 变化产生新的 action revision。
5. **跨渠道重复回答。** QQ 与 Web 同时提交同一决定，只有一个有效提交；后续修改作为新意图进入主体，不重复外部副作用。
6. **纯文本/无障碍偏好。** 明确要求纯文本时不强塞卡片；图表有可读数据/解释，键盘、屏幕阅读器、字号和减少动画可用。
7. **流式中断。** 半个表单生成时断开，已显示内容仍可读；未完成的动作不可执行；恢复不重置有效业务状态。
8. **唯一主体。** 用户表单提交、语音选择、远端 AskUserQuestion 都进入同一 inbox/主体序列；UI 插件没有隐藏常驻人格。
9. **C=1 与本地成本。** 调 slider 的纯计算不触发模型；需要推理的 UI worker 与主 loop/视觉 worker 共用 broker，不超发。
10. **renderer/compiler 升级。** 同一语义对象经新 Proto adapter 输出，历史视图能读取，动作状态与权限不漂移，必要时降级到文本。
11. **目标能力改变。** 移动端或 IM 不再支持某组件，自动给出有效替代，不让用户停在不可操作空白容器。
12. **隐私隔离。** 私聊中生成的表单/关系卡被群消息引用时，只投递该群可见摘要；共享 interaction ID 不是跨域读权限。
13. **自扩展。** Agent 委派孩子安装/生成新组件插件，通过 schema、资源与当前行为合同验收后可用；无需把插件语法永久编进核心。

## 13. 建议写入主稿的简短结论

动态生成式 UI 是 Personal Agent 的正式表达与交互能力。主体根据目标界面能力、受众与偏好委派生成语义 UI，经可替换 renderer/compiler 投影到 Web、原生卡片、文本、媒体、语音或终端。模型可获得版本化 DSL/组件说明或结构化 UI 工具；交互进入同一主体事件流，动作由持久版本与权限合同约束。没有丰富 UI 的渠道仍可完成核心语义任务，不强制跳转浏览器。

竞品参考应称 OpenAI **Intelligent UI**。已证实其原生流式组件和编译方向；公开 Apps SDK/Plugins/MCP Apps 与 ChatKit 提供不同的开发界面，不能混写成已经公开的同一原生动态响应协议。

## 14. 一手来源索引与不确定性

| 来源 | 日期/核验状态 | refs |
|---|---|---|
| https://openai.com/index/gpt-6-for-everyone/ | 2026-10-07 发布，已开全文 | `turn98view1` |
| https://help.openai.com/en/articles/20001598-intelligent-ui-in-chatgpt | 查阅时 Updated yesterday，已开全文 | `turn98view0` |
| https://help.openai.com/en/articles/20001354-gpt-6-and-other-models-in-chatgpt | 查阅时 Updated 13 hours ago；部分档位文案与前页不一致 | `turn98view2` |
| https://help.openai.com/en/articles/6825453-chatgpt-release-notes | 2026-10-07 Intelligent UI 条目 | `turn97view0` |
| https://developers.openai.com/apps-sdk/ | 本轮 redirect 到 /plugins | `turn102view1` |
| https://developers.openai.com/plugins/build/chatgpt-ui | 当前 MCP Apps/iframe/能力探测/无 UI 回退官方说明 | `turn94view0`、`turn96view3`、`turn101view5` |
| https://developers.openai.com/plugins/reference | 工具结果、UI 状态、桥及权限提示说明 | `turn101view1`、`turn102view2` |
| https://developers.openai.com/plugins/concepts/ui-guidelines | 可选组件库、display modes、无障碍 | `turn99view1`、`turn101view4`、`turn102view4` |
| https://developers.openai.com/api/docs/guides/chatkit-widgets | typed widget 与 JSON 路径 | `turn101view2` |
| https://developers.openai.com/api/docs/guides/chatkit-actions | action 事件、服务端处理、payload 不可信 | `turn101view3` |
| https://developers.openai.com/api/docs/guides/structured-outputs | JSON Schema 输出机制；不代表完整 UI runtime | `turn102view0` |

没有使用论坛普通用户帖子作为产品事实依据。官方搜索暂未确认的内容：Dynamic Response 精确产品名、原生 Intelligent UI DSL/独立 compiler 公共发布、跨第三方 IM 运行相同原生响应、跨 thread 原生组件状态。对这些应写“本轮未确认/未找到公开合同”，不能推断绝对不存在。主 Agent 对外引用前需自行打开同一来源。
