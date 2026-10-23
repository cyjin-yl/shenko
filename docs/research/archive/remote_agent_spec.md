# 远程 Agent Seat 插件：研究与产品规格补充

研究时间：2026-10-23 Asia/Shanghai。状态：研究与 SPEC 建议，不进入开发。此文中 `seat.*` 等名称是本产品拟定的能力词汇，不是声称现有 Claude Code / omp 已有的命令。

## 1. 产品目标与主循环边界

用户从 QQ、飞书、私聊或其他已接渠道发来任务后，唯一主 thought loop 读取记忆，知道相关工作曾在哪台机器、哪个工程和哪个远端 agent 上进行，再委派子 agent 连接、验证现场和继续操作。主循环本人仍只思考、读/存记忆和委派；SSH、tmux 输入、远端问答回答与消息发送由子 agent 的插件完成。

远端 Claude Code、omp 或其他工具是**受委派的执行工作位**，不成为第二个人格主体。它们可以长时间执行、产生自己的工具调用及子任务，但对外仍属于同一任务链，且不自动获得主体全部私密记忆。

插件应可以单独安装和升级。优先提供本产品原生协议插件，获得统一任务、记忆、权限、推理预算、问答和消息路由能力；传统 Skills、MCP、已有市场也必须能够接入。

### 1.1 最新裁决：任何软件层都可自我改写

本规格不再设定“内核、救援层、人格或主 system 永久不可修改”的代码边界。自我改进子 agent 可以为 kernel、rescue、provider、bridge、harness、工具、人格及提示词生成候选，验证后自动晋升。主循环不直接写文件、唯一主体、全局推理上限及已授予权限等，是**当前生效的行为合同**；候选激活前必须证明满足当前合同，不能通过把合同换成容易通过的版本而自证合格。用户随后改变合同应记录为新的合同版本。

候选通过 A/B 激活，不原地破坏唯一可启动根。保留上一可启动闭包、配套数据格式/迁移信息和已验证恢复路径；救援程序本身升级时亦然。内容寻址旧制品保留不等于该层实现永远不许改写。能够回退二进制不代表能够回退已发生的外部行为或不可逆数据迁移。新 controller 必须领取新 epoch，不继承旧屏幕/工作位输入 lease。

## 2. 一手来源核验：哪些能力已经存在

### 2.1 Claude Code / Claude Agent SDK

官方 Agent SDK 的 `canUseTool` 可接收尚未被其他规则解决的工具审批，以及 `AskUserQuestion`。后者必须回送原问题和答案，支持选项及自由文本。已经自动批准的工具不会再进入该回调；需要覆盖每次工具调用的逻辑使用 `PreToolUse`。

来源：[Handle approvals and user input](https://code.claude.com/docs/en/agent-sdk/user-input)，refs `turn68view0`、`turn69view0`、`turn69view1`。问答是可以被应用承接的结构化协议，并不要求我们的产品每次都停下来找人。

官方权限流程仍会检查 deny/ask 规则。`dontAsk` 可直接拒绝本应提问的调用；部分无提示配置跳过 `canUseTool`。**不能把关闭提示当成接通自主问答。**

来源：[Configure permissions](https://code.claude.com/docs/en/agent-sdk/permissions)，ref `turn68view1`。

当前 hooks 文档另外确认：

- `PreToolUse` 可通过 `updatedInput` 回答 `AskUserQuestion`；只返回 allow 不构成回答。
- `PermissionRequest` 可代用户 allow/deny，但没有 `tool_use_id`；与 SDK host 同时处理时，先作出的决定生效。
- `defer` 可让非交互 `-p` 会话在未执行工具时结束，之后 resume；仅支持单个 tool call，交互 TUI 不支持该语义。恢复时随意发新 prompt 可能丢弃原待决调用。
- hooks 超时的默认行为并不普遍阻止执行；用它承载必需策略时必须验证失败处理。

来源：[Hooks reference](https://code.claude.com/docs/en/hooks)，refs `turn68view2`、`turn69view2`、`turn69view3`、`turn70view3`。版本敏感能力应在运行时探测，不把当前主线字段写成对旧 CLI 的无条件保证。

**远端现存 TUI 与新建 SDK 工作位是不同接入对象。** 不应假定能够给任意已运行的终端 retroactively 加上 SDK callback。没有预装 bridge/hooks 的既有 TUI 可以走 tmux 兼容路径；新建工作位优先使用远端 SDK/结构化 bridge。需要迁移已有 session 时，先确认旧会话的活动与所有权，不得让同一工程出现两个都认为自己接管成功的写入者。

Claude 自带 [Remote Control](https://code.claude.com/docs/en/remote-control) 面向 claude.ai/移动端，要求相应订阅，且当前不支持自定义 gateway endpoint。它不是本产品私有 SSH 协议的别名或必须依赖。来源 ref `turn70view2`。

### 2.2 omp：已经有 Rust RPC 客户端

已用 GitHub 插件固定读取 `can1357/oh-my-pi` 主线快照：

- commit：`0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0`，2026-10-22。
- `docs/rpc.md` blob：`bf0f14b412a650eed2629575d2578e1cf6ea9e2e`。
- `docs/sdk.md` blob：`552e4bf3f9a0b593d91ff50bfc46423994940eed`。
- `sdk/rust/omp-rpc/README.md` blob：`b49db24fa4eebb90359c2007eceec544aa51e2d2`。
- `docs/extensions.md` blob：`9bc7786fc40bf3b24513abb910ece3cdbf5fb259`。

RPC 是 stdio JSONL 自定义协议。已确认的相关能力包括 prompt、steer、follow_up、会话读取/切换、事件流、结构化 UI 请求/回答、子 agent 状态。request id 用于关联；prompt 被接受、agent yield、整个 session settled 是不同状态。

`rpc-ui` 提供工具问答，`set_ask_dialog` 可启用完整结构化问题。`--no-ui` 会让需要审批的工具失败，而不是自动批准。ask 超时可能选择推荐项或首项；晚到答案会被忽略。插件必须显式管理这种语义，不能把超时选项记录成用户同意。

来源：[RPC 文档](https://github.com/can1357/oh-my-pi/blob/main/docs/rpc.md)，ref `turn70view0`，固定快照内容以上述 GitHub 读取为准。

[Rust 客户端](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/sdk/rust/omp-rpc/README.md) 自带从 wire schema 生成的 serde 类型和 reader/writer transport，支持协议协商、分块、prompt 结果、host tools/URI、UI responses。它能作为 Rust 插件实现的参考，但没有验证其当前全部实现可直接编译进本产品 WASM 环境。远端跑 omp，近端只运行协议适配器，可避开这个不必要的前提。

[SDK 文档](https://github.com/can1357/oh-my-pi/blob/main/docs/sdk.md) 另有 `interactivePrompts`、`setToolUIContext`、`providerSessionId` 和独立的 `providerPromptCacheKey`，可给非 TUI host 提供问答与缓存身份。ref `turn73view3`。

OMP 的 `ctx.ui.openTerminal` 是交互 TUI 的新建 pane/group 能力；RPC、ACP、headless 不提供它。不能因此宣称 RPC 能接管任意现存 tmux pane。来源：[extensions.md](https://github.com/can1357/oh-my-pi/blob/main/docs/extensions.md)，ref `turn73view4`。

### 2.3 tmux：可靠的终端控制基础，不提供 agent 问答语义

tmux 官方 control mode 支持通过 SSH 传输文本协议、命令响应和异步 pane 输出。官方推荐用 session/window/pane ID，而非易变的名字或位置；输出含转义序列且未必是 UTF-8，连接积压后需要重新捕获屏幕。

来源：[tmux Control Mode](https://github.com/tmux/tmux/wiki/Control-Mode)，refs `turn68view3`、`turn71view2`、`turn71view3`。

我们由此作出的设计推论：tmux 命令成功仅表示终端层操作被执行，不证明 Claude/omp 已接受任务，更不证明它完成了工作。终端内容中的问题也可能只是历史滚动文本，必须确认它现在仍是可回答的活动 prompt。

## 3. 接入方式：按实际能力分级

| 方式 | 适合对象 | 能保证什么 | 必须公开的限制 |
|---|---|---|---|
| 原生远端 bridge + SDK/RPC | 我们创建或可安装 companion 的工作位 | 结构化会话、问题、控制请求、结果、授权及持久回执 | 版本变化要协商；远端 bridge 必须真实存活 |
| Agent hooks / extension bridge | 可扩展的现有 Claude Code / omp | 将生命周期、权限、问题转为我们自己的事件 | 不同 event 的 ID、超时、触发覆盖不同 |
| SSH + tmux control mode | 用户已经开的 TUI、其他终端 agent | 查看、选择目标、输入、观察终端输出 | 缺少稳定语义 ID 时，回答和完成判断可信度降低 |
| 只读观察 | 无可靠写入协议或暂时失去控制权 | 查询状态、保存证据、向用户解释进度 | 不承诺可安全提交按键或自动回答 |

产品不因缺少高级适配器就拒绝所有工作：能观察则观察，能从已授权接口继续则继续；必要时主动寻找合适的人获取信息。它也不把无法验证的现场操作伪装成已完成。

## 4. Task–Seat Registry：记忆用于找到入口，现场决定真实状态

自然语言记忆记录“我在 lab 主机帮你做 project A”；它不是仍然活着的进程证明。Registry 保存可机器校验的工作位记录；每次实际操作前做 live verification。

### 4.1 需要的记录

| 类别 | 字段与用途 |
|---|---|
| 业务关联 | task ID、来源渠道/群/线程/消息 ID、委派人、受益人、可见域、目标与任务修订号 |
| 运行节点 | node ID、SSH principal、host identity 指纹引用、连接 profile ID、最后验证时间；不把凭证原文写入记忆 |
| 工作环境 | workspace 绝对路径、项目/仓库身份、当前分支或快照、执行所需 profile |
| Agent 身份 | backend 类型与版本、remote session ID、bridge instance ID、能力矩阵 |
| 终端定位 | tmux server/socket 身份、session/window/pane ID、进程 PID 与启动身份；名字只作展示 |
| 所有权 | 当前 controller、lease、fencing epoch、允许的操控范围、人类是否已接管 |
| 恢复信息 | last event cursor、inbox/outbox 回执、pending questions、未决副作用、结果证据位置 |
| 推理资源 | provider/endpoint 逻辑 ID、broker resource-group、是否受控、计费归属、usage 覆盖质量 |

不要把 pane ID 单独当全局 UUID：它要与节点、tmux server 实例及工作位 epoch 一起解释。不能只见到相同目录名就覆盖另一项任务。

### 4.2 Live verification 的判定

操作前确认主机身份匹配、bridge/process 存活、session 与项目匹配、lease 仍属于本 controller、目标没切换、问题仍待决。若记忆过期，先刷新记录并找回真实 session；只能在既有任务权限内新建替代工作位。

若同一个目录出现两个候选，利用项目身份、远端 session 元数据和近期任务证据消歧；仍不确定时把差异作为一个具体选择联系用户，而非机械报“session not found”。

### 4.3 所有权与人类并用

默认一个工作位只有一个机器写入 controller，可以有多个观察者。controller 的 lease 过期或 epoch 变化时，旧输入和旧回答失效；重连不能仅凭旧内存继续写。

用户在远端亲自输入仍应受到尊重：结构化 bridge 记录外部修订；终端模式观察到新的人工输入/目标状态变化后重新校验，避免把 agent 原计划一串键打进人类刚开的 shell。接管机制不应把“人类只是看了一眼”误当成永远撤销授权，也不能声称本产品的 lease 能阻止不合作的本机 root 或直接键盘操作。

## 5. 自主回答远端问题：委派授权，不是每次都找人

### 5.1 分开三类事件

| 事件 | 示例 | 推荐行为 |
|---|---|---|
| 实现选择 | 用项目现有 SQLite 还是增加 PostgreSQL | 在用户目标、项目事实、偏好记忆和当前授权内由子 agent 决定 |
| 已授权动作的工具确认 | 写项目文件、运行测试、重建可丢弃依赖 | 插件验证委派范围后自主回答，不重复要求人类批准 |
| 缺少关键事实或新增授权范围 | 面向外部发布哪个品牌账号；删除未包含在委派中的真实数据 | 先查已授权记忆/现场；仍未知则主动联系合适的用户，保存任务等待状态 |

“人格”影响怎样权衡选择、解释和联系，不扩展权限。清晰的既有委派就是决策依据；不是看到 confirm 就固定拒绝，也不是看到推荐项就固定选第一个。远端明确的 managed deny、账户限制或真实的人类身份验证不能通过换入口/模拟按键规避。

### 5.2 委派凭据

每次任务有结构化 authority envelope：谁授权、允许的项目/节点/账号/路径/行为、有效时间、预算、可对外代表谁、哪些决定可自行作出、哪些依赖新信息。可以来自长期管理员设置与当前用户任务，不要求每次重新配置。

回答记录注明 `decider = agent | human | backend_default`、依据、相关记忆版本、问题版本、授权版本和结果。Agent 代答就是 agent 代答，不能在产品审计中冒充人类亲自点击过。

### 5.3 一个 pending question 的完整身份

建议使用：task ID + seat epoch + remote session ID + 请求 ID + 问题/选项修订摘要 + 工具参数摘要 + 发起实例 nonce。桥接器生成自己的稳定 question ID，并保留上游原始关联字段。

Claude `PermissionRequest` 没有 `tool_use_id`，不能只拿工具名加参数哈希永久去重：相同命令可能是两次合法调用。优先把 `PreToolUse`/SDK/桥接调用信息关联起来；无法关联时，明确降低可自动回答的粒度。

OMP 的 UI request ID 和 prompt result ID 也要绑定当前 session/epoch，不能认为进程重启后同名 ID 仍是原问题。

### 5.4 防止迟到回答污染任务

在发送答案前重新确认该问题仍 pending、选项未改变、任务未取消、controller lease 有效。用一次原子状态提交选择唯一答案；同一问题在多渠道收到多个答案时，第一条有效提交生效，后续答复被解释为修正意图，按当前执行进度处理，不盲目重发。

backend 已超时、用户已在远端回答或 session 已切换后，晚到的“同意”不能被投递给下一个问题。UI 应清楚显示该回答是否仍可采用。

### 5.5 长等待与后端自动选择

Claude SDK 可以等待 callback，也有经验证的 defer/resume 路径；插件按实际支持选择，恢复时保护待决 tool call，不通过普通新 prompt 意外替换它。

OMP 的问答超时默认选择是特别需要适配的语义。若任务需要等人，必须在 deadline 前使用经过验证的暂停/取消/自定义问答 host 路径，并把待决问题保存为本产品状态。不能只让 timeout 到来然后把 recommended 当作授权。若某版本无法保证持久等待，能力矩阵标记这一点，再选其他可用接入方式；不虚构一个未证实的“无限等待”开关。

等待人类时不持续占用本地推理 permit，也不妨碍主循环做其他工作。

## 6. 联系用户：有记忆、有判断、可多渠道，仍是同一问题

联系不是“遇到不确定就统一发错误”。子 agent 先整理：当前卡在哪里、已经查过什么、推荐什么、为什么缺少该决定，以及等待期间还有哪些工作能继续。

### 6.1 Contact policy 的输入

允许使用已验证身份关联、用户设置/历史偏好、最近活跃渠道、私密程度、紧急性、时区/勿扰、可用平台能力、原任务上下文和实际送达结果。不要预设某个渠道永远是首选，也不要为了联系而把所有群全部刷一遍。

推荐顺序是由策略动态选择的，而不是代码固定 QQ → 飞书 → 邮件。普通事项可优先原会话；原群不能看具体隐私时，只在群内给简短状态，在已授权私聊中问细节。主动消息、文件和机器人群聊能力仍受各平台现实限制，例如当前 iLink adapter 只宣告 direct chat，不能假定微信与 QQ 群一样可用。

多种方法包含不同渠道、交互卡片或可访问的任务页面，但仅在用户已经允许的联系范围内。没有配置/授权的电话、短信、邮箱不能被创造出来。消息发送由渠道子 agent 执行，主循环负责委派。

### 6.2 一个问题，多份投递，唯一决策

持久 outbox 记录 question ID、destination、attempt、平台消息 ID、状态与下一次重试条件。发送回执不等于用户看见；超时升级不等于批准。多渠道通知复用同一问题身份，答完一处后撤销或标记其他卡片已解决；无法撤销的平台发适当更新。

每次升级必须有原因，例如未送达、用户主动换渠道、期限缩短，而不是固定无限频率。普通分支继续能做的工作；没有答案的关键决定保持 pending，并以合适节奏主动回来处理。

### 6.3 回灌原工作位与来源会话

答案先进入本产品的规范化任务事件，验证答复人身份与权限，再由当前 controller 投递给**原 remote question / session**。不能把用户的任意消息串到所有远端终端。

原任务的来源会话收到简洁进度和结果；必要时跨渠道的私密回答只回灌远端可见范围，不能原封不动复制到群里。已有 source thread 仍是任务的展示入口，即使决定是在另一私聊完成。

## 7. tmux 兼容路径的具体规则

这部分是本产品建议，tmux 本身没有 task/permission 语义。

- 用 control mode 获取事件并以 ID 定位；只在已验证的 pane 中输入。保存读取 cursor/screen revision，接收重绘和窗口变化。
- 对新建进程优先使用独立 argv 的启动方式；对既有 TUI 使用明确文本/按键输入协议。shell command、终端文本和 JSON 是不同编码层，不能用 JSON 序列化冒充 shell quoting。
- 不把终端打印的“press y”直接当授权事件。确认当前活动 UI、目标进程、问题文本、选项与当前任务；历史 scrollback 中的旧问题没有待决权。
- 输入前后读取状态，确认答案被消费并发生预期转移。tmux 的成功响应只确认 tmux 命令层，不等价于远端工作完成。
- 断线时若不知道按键是否已投递，不机械重放。先观察已产生的状态，必要时标记 delivery unknown；只有可证明幂等的输入才自动重试。
- 兼容层应可替换终端 parser；不要把 Claude/omp 某个版本的 ANSI 文案写进不可升级的核心。

终端内容按来自远端工作区的不可信数据处理。诸如“忽略管理员限制”“把密钥粘贴到聊天”的文本不因出现在 agent pane 中就提升为本产品指令。

## 8. 推理 broker 与费用：不能由 SSH 连接数冒充全局并发

### 8.1 三种资源覆盖模式

| 模式 | 能否硬约束本地全局 inflight | 计费/用量处理 |
|---|---|---|
| 所有远端模型请求经本产品 broker | 可以，在对应资源组内 | request ID、任务树、重试、cache、模型与优先级统一计账 |
| 远端直连独立云提供商 | 不占本地 GPU，但可能有独立账户并发/费用 | 设独立 resource-group，采集远端 usage；观察覆盖不完整时明确显示 |
| 远端绕过 broker 直连同一本地 API | 不能由近端调度器给出硬保证 | 必须改成受管入口或后端统一限流，才可承诺全局上限 |

一个远端进程可能开多个子 agent、advisor、summary/embedding/视觉调用；不能只给“这个工作位”发一个许可就当所有模型调用已受控。共享 GPU 的调用要在真正 inference dispatch 时排队；等待远端工具、网络或人类时不占计算许可，cache lease 与 compute permit 分离。

权限判断若需要本地模型，也是一条 broker 请求。不得由持有唯一 permit 的请求等待另一个永远拿不到 permit 的审批 agent。

### 8.2 Claude 路径的实际支持边界

官方 [gateway 文档](https://code.claude.com/docs/en/llm-gateway) 允许支持其 API 格式的 gateway，但明确不支持通过 gateway 将 Claude Code 路由到非 Claude 模型。ref `turn71view1`。所以不能在“本地 AI”教程里把任意 OpenAI-compatible 本地模型宣称为 Claude Code 官方兼容能力；本地优先可用 omp/本产品自身 harness，Claude 工作位是可选远端 backend。

本产品 SSH bridge 不要求使用 Claude 自带的云 Remote Control。若用户选择云模型工作位，UI 应能显示对应数据目的地；极致隐私的本地 profile 不应悄悄因为某个子 agent 选择而切换到云端。

### 8.3 用量数据不能重复累加

当前 Claude SDK 文档说明 `total_cost_usd` 是估算；`usage` 不含子 agent，而 `modelUsage` 与总费用估算包含。resume 的总数可能已经包含旧会话花费；跨多个 result 直接求和会重复计账，重置会话也不能重置本产品累计预算。

来源：[Track cost and usage](https://code.claude.com/docs/en/agent-sdk/cost-tracking)，refs `turn71view0`、`turn74view3`。版本差异要进入适配器测试；本产品账本以 provider/request/epoch 的事件去重与可信度标注为准。模型费、远端机器费、本地 GPU 时间应分项。

OMP SDK 中 provider session 与 prompt cache key 分离，可作为 KV 身份设计参考，但不证明任何后端支持任意 slot ID。插件必须透传已协商的字段，记录哪些调用真的受控；未知缓存命中不能展示成已命中。

## 9. 原生插件、Skills、MCP 与市场

### 9.1 原生插件推荐承担的能力

拟定的原生 remote-agent 插件提供 `seat.locate / inspect / dispatch / observe / steer / answer / interrupt / detach` 等结构化能力；插件 manifest 声明协议版本、适用远端版本、依赖、权限、资源组接入和恢复语义。具体命名尚可调整。

插件不能自行绕过任务账本开一个隐藏永久主体。插件中的控制器是机械状态机；需要判断时请求被委派的 agent。SSH transport、Claude SDK companion、OMP RPC adapter、tmux adapter 可以独立更新，极小核心只提供必要宿主能力和持久调度接口。

“原生”表示采用我们的扩展契约，不等于强制所有插件都变成不可隔离的本机动态库。具体 WASM component/进程形式由主架构确定，不能由此反向引入整个 Bun/Node 到路由器核心。

### 9.2 兼容标准与现实边界

Agent Skills 的一手 [格式规范](https://agentskills.io/specification) 是 `SKILL.md` 加可选 scripts/references/assets，frontmatter 声明元数据。ref `turn73view0`。本产品应能从本地目录或认证 Git 导入；skill 文字不是可执行权限授权，脚本仍使用既有能力。

[MCP Registry](https://modelcontextprotocol.io/registry/about) 是元数据发现层，不直接托管所有执行代码；私有服务器/私有包应使用私有目录或兼容 registry。refs `turn73view1`、`turn74view0`。市场中出现一个 MCP server 不意味着它已适合 WASM 或已被安全审计。

Claude 的 [plugin marketplace 格式](https://code.claude.com/docs/en/plugin-marketplaces) 可作为兼容导入来源，ref `turn73view2`。应先识别它包含 Skills、MCP、hooks 还是宿主专有功能，展示能力映射；不能把安装一个 manifest 宣称为全量兼容另一产品内部命令。

### 9.3 安装、升级和隐私

可以由 agent 自行发现和安装在既有插件安装授权范围内的版本；新权限按用户设置处理，不能把每次无新增权限的更新变成阻塞确认。安装候选经过解析、依赖/ABI 检查、隔离验收后原子切换，保留回滚版本。

支持本地离线包、认证 Git/Gitea、私有 registry 与多个市场。发现/更新的联网政策可关闭或设定；私密任务、仓库名、SSH 主机和记忆检索词不应因“智能推荐插件”自动发给公共市场。

## 10. 建议验收场景

1. 飞书发来的任务找到昨天 QQ 提到的远端项目，检索结果只用于定位；现场 session 已换时不会输入错 pane。
2. 远端问“沿用项目数据库吗”，agent 根据有效项目记忆自主回答，并注明代答依据，用户不被反复打断。
3. 远端出现超出委派范围的外部发布选择，agent 先查记忆，然后选择当前合适私聊渠道联系用户；用户不在线时任务保存并继续其他可做工作。
4. 同一问题先发 QQ 后因未送达改用飞书，用户两边都回复；只提交一条有效决策，另一条按修正处理，不重复执行。
5. 用户在远端手动回答，稍后手机卡片返回旧答案；插件识别 stale question，不把答案打到新 prompt。
6. OMP ask 即将 timeout，系统不会将推荐项自动记录为用户授权；等待策略符合真实 backend 能力。
7. Claude defer 会话恢复，仍能回答原 tool call；不会为了“唤醒”发普通 prompt 而悄悄丢弃问题。
8. SSH 网络在发送输入后中断；重连先读取现场，不盲目 replay 可能已产生副作用的输入。
9. 节点 lease 被新 controller 接管后，旧 controller 的回答/写入被拒绝；外部人类接管也可观察并协调。
10. C=1 的本地 API 同时面对主循环、远端 omp 子 agent 和审批推理，实测没有超发和 permit 等待死锁。
11. 远端直连云模型时仍可工作，但明确属于独立资源组；无法观测的调用不会被纳入“已严格限制”的数字。
12. 插件升级导致 RPC schema 或问答能力变化时，旧任务可以保留、回退或迁移；没有验证前不把 task 标为完成。

## 11. 仍需最终产品选择的少数问题

- 对已经存在、没有 bridge 的 Claude/omp TUI，默认允许在充分 live verification 后用 tmux 自动回答，还是只允许观察并建议安装 bridge？建议前者支持，但能力可信度与恢复保证分级，不默认禁用整个入口。
- 一个工作位被人类同时操作时，默认暂缓机器输入还是允许合作式排队？建议识别明确人工接管时让出写入，纯观察不撤销任务委派。
- 未回答的私密问题可以主动联系哪些已验证账号，允许什么紧急度下换渠道？建议用户设置长期边界，之后由人格/记忆/现场动态选择，不逐次申请联系权限。
- 强本地 profile 是否完全禁止任何云模型工作位，还是允许用户主动选择某个任务例外？建议 profile 默认严格遵守已有本地边界，例外一旦授权可在该任务持续有效。

## 12. 一手来源与版本索引

| 主题 | URL | ref / 固定证据 |
|---|---|---|
| Claude SDK 问答 | https://code.claude.com/docs/en/agent-sdk/user-input | `turn68view0`、`turn69view0` |
| Claude SDK 权限 | https://code.claude.com/docs/en/agent-sdk/permissions | `turn68view1` |
| Claude hooks | https://code.claude.com/docs/en/hooks | `turn68view2`、`turn70view3` |
| Claude 云 Remote Control | https://code.claude.com/docs/en/remote-control | `turn70view2` |
| Claude gateway | https://code.claude.com/docs/en/llm-gateway | `turn71view1` |
| Claude 费用 | https://code.claude.com/docs/en/agent-sdk/cost-tracking | `turn71view0`、`turn74view3` |
| OMP RPC | https://github.com/can1357/oh-my-pi/blob/main/docs/rpc.md | `turn70view0`，GitHub 固定 commit `0b5c27a…` |
| OMP SDK | https://github.com/can1357/oh-my-pi/blob/main/docs/sdk.md | `turn73view3`，同 commit |
| OMP Rust RPC | https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/sdk/rust/omp-rpc/README.md | GitHub 插件成功读取；网页直开失败，不以失败 ref 佐证 |
| OMP 扩展 | https://github.com/can1357/oh-my-pi/blob/main/docs/extensions.md | `turn73view4`，同 commit |
| tmux control mode | https://github.com/tmux/tmux/wiki/Control-Mode | `turn68view3`、`turn71view2` |
| Skills 格式 | https://agentskills.io/specification | `turn73view0` |
| MCP registry | https://modelcontextprotocol.io/registry/about | `turn73view1`、`turn74view0` |
| Claude marketplace 格式 | https://code.claude.com/docs/en/plugin-marketplaces | `turn73view2` |

主 agent 对外引用子 agent refs 前应自行 open 同一 URL。这里未执行远程连接、插件安装、账户登录、代码修改或任何对外发送。

## 13. Computer Use 正式插件计划

Computer Use 是正式能力方向：基础运行环境仍为 WASM，后续通过可安装插件接入完整 Linux、Windows、macOS 环境或远程桌面。截图、视觉识别、鼠标键盘、浏览器 DOM/AX 与 SSH agent seat 都属于受委派的执行能力。轻量设备可以仅负责主体和调度，把桌面部署在别处；不要求路由器核心加载浏览器、桌面环境或视频解码器。

### 13.1 ComputerUseProvider 与可替换后端

拟定 `ComputerUseProvider` 提供发现与能力协商、建立/恢复桌面会话、观察、输入、订阅状态、接管/释放及效果核验接口。它与 `ExecutionProvider` 共用任务、节点、凭据和权限身份；与 remote-agent plugin 共用 TaskSeat registry，但不把 GUI 会话和终端 pane 当作同一种输入域。具体 API 名称留待实现阶段确定。

| 后端 | 主要能力 | 必须保留的能力差异 |
|---|---|---|
| 原生 node agent | OS 截图、输入、窗口及可用的原生 accessibility 接口 | 各 OS 权限、桌面 session、锁屏和无头环境不同；原生 AX 不是 DOM |
| VNC / RFB | 远端 framebuffer 与输入 | 像素/输入协议不能直接保证元素语义、应用效果或身份授权 |
| RDP 适配器 | Windows 等环境的远程桌面连接 | 会话隔离、登录和与本地桌面的关系由所接服务器能力协商，不保证等同物理显示器 |
| 浏览器协议适配器 | DOM、浏览器 AX、截图及定位/输入 | 仅管被授权的 browser/tab/context；不能把页面结构访问当作 OS 桌面权限 |
| SSH + agent seat | 远端 CLI、tmux、SDK/RPC | 不能因 SSH 可连便假定 GUI 可见可控；可与桌面观察关联但协议分开 |

优先使用可获得的结构化目标定位，保留视觉截图路径；不能因拿到 DOM 就宣布视觉无须支持。一个后端可以只提供部分能力，UI 与工具 schema 必须如实声明。重型库和 OS companion 均为可选依赖、可卸载/升级；provider 协议和工件版本可协商。

### 13.2 “完整环境”需要区分 OS 与运行方式

- Linux 可采用有相应显示服务/会话的容器、VM 或物理节点；普通 WASM guest 不凭空获得 Linux desktop。
- Windows 桌面面向实际桌面 session、Windows VM 或物理节点。Microsoft 文档明确 Windows containers 不支持 GUI，不能把完整 Windows desktop 写成普通 OCI 容器的一项开关。
- macOS 需要受支持的 Mac 运行环境；Apple 提供 Apple silicon 上的 macOS Virtualization 框架路径。不能声称 macOS 是 Linux OCI image 或可在任意路由器上虚拟化运行。

来源：[Microsoft Windows containers 应用兼容性](https://learn.microsoft.com/en-us/virtualization/windowscontainers/quick-start/lift-shift-to-containers)，ref `turn91search0`；[Apple macOS VM](https://developer.apple.com/documentation/virtualization/running-macos-in-a-virtual-machine-on-apple-silicon)，ref `turn91search1`。这些来源支持 OS 能力边界，不替代实际节点部署时的版本、硬件和使用条件检查。

### 13.3 观察身份、过期屏幕和输入 lease

观察结果携带 `node_id / session_id / display_id / window_id / tab_id` 中可用的稳定标识、frame ID、布局/文档 revision、时间、viewport、缩放/DPI、坐标变换与可见权限范围。视觉模型给出的像素坐标仅对对应观察和变换有效，不能当永久按钮身份。

输入操作绑定 task、controller epoch、lease、目标和预期观察 revision。切换窗口/标签、分辨率改变、页面导航、屏幕长时间未更新或 reconnect 后重新观察；目标失配则重新规划，不把旧坐标照抄到新画面。连续拖拽/输入等可在一个有界 action 内处理，但结束后必须核验。断开或接管时释放本插件仍持有的 modifier/key/button，避免“Ctrl 永远按下”。

写入 lease 的范围是**共享焦点和鼠标键盘的交互桌面输入域**，不只是窗口或 pane。允许多个只读观察者；多个看起来独立的窗口可能仍争用同一输入域。识别到明确人类接管时撤销机器写入并使旧 epoch 失效，保留任务和只读观察；恢复控制要经过新的 lease 与现场验证。lease 约束本产品和配合的 node agent，不能声称能阻止人类、root 或未受管工具直接输入。

### 13.4 GUI 效果、操作系统权限与实际授权

GUI 副作用不能只凭 action ID 实现 exactly-once。每次操作记录意图、发送尝试、传输回执、观察到的结果、应用级收据（若有）与 unknown 状态。点击“发送”后断网，不应盲目重放；先检查消息/任务/订单实际状态。文件系统或 VM 回滚也无法撤回已发出的消息、已发布内容或第三方系统提交。

权限能力应分别声明 `screen_read / accessibility_read / input / clipboard / file_transfer / browser_context` 等。macOS 屏幕录制与 Accessibility 有各自的系统授权；SSH 身份和本产品 owner 身份不自动替代 OS 桌面访问权限。缺少权限时给出真实受阻状态和一次性正确接入流程，不虚构可用性，也不绕过拒绝。已获得有效委派和 OS 权限后，正常操作按该授权自主执行，无须每次点击再向人类确认。

来源：[Apple 屏幕/音频录制访问](https://support.apple.com/guide/mac-help/control-access-screen-system-audio-recording-mchld6aa7d23/mac)，ref `turn91search2`；[Apple Accessibility 访问](https://support.apple.com/guide/mac-help/allow-accessibility-apps-to-access-your-mac-mh43185/mac)，ref `turn91search5`。

### 13.5 视觉请求仍属于同一个推理系统

视觉模型、用模型完成的 OCR、截图总结、browser agent 及远端桌面 agent 均受对应资源组的 broker 调度。不得用“Computer Use 插件在远端”作为绕过同一本地 API 并发限制的理由。记录图像数量、分辨率/裁剪策略、可获得的视觉 token/prefill 用量、耗时、目的地和费用；后端未返回的数字标为未知。优先采用变化检测、按需截图和证据裁剪，避免固定高频把同一屏幕送去完整 prefill；不能把普通图像哈希命中宣传为 KV 命中。

主循环通过其唯一 mailbox 序列接收经权限过滤的多模态证据、子 agent 结论和待决选择。截图可以进入同一主体上下文；视觉执行子 agent 不形成第二个主体思考循环。主循环仍只读/存记忆和委派，实际点击/键入/文件写入属于子 agent。UI 展示观察、行动意图、委派、执行结果与可信度，不伪造公开的逐 token 内部思维。

截图与屏幕文本可能含私人聊天、密钥、密码输入、跨租户数据。原始帧的持久化、分享与模型路由继承任务 ACL 和本地 profile；未授权的群只能获得可公开结果。网页、终端、AX 文本都属于环境证据，不能因为出现在屏幕中而覆盖当前行为合同。

### 13.6 Computer Use 与全组件自改的共同验收

1. 只有 WASM 基础包的路由器可安装轻型 transport 并委派远端桌面，未加载桌面/视频重量级依赖。
2. 同一 desktop 的两个子 agent 同时想输入，只允许有效 lease 持有者写入；两个完全隔离桌面 session 可分别操作。
3. 用户接管后，延迟到达的旧点击被 fencing 拒绝；恢复后重新读取窗口和屏幕，不沿用旧坐标。
4. 视觉推理进行时窗口位置/DPI 改变，旧结果不能点击新画面的无关按钮。
5. 点击后连接中断，任务显示 effect unknown 并核验效果，不重复提交；快照回滚不会把外部发送记录抹成“没发生”。
6. C=1 时主 loop、视觉 worker、远端 agent 与审批推理不会超发；等待远端画面或人类时不持有 compute permit。
7. 内核、rescue 或 ComputerUseProvider 候选可自主通过当前合同 gate 并激活；启动/迁移/能力验证失败会回退到保存的可启动闭包，旧 input lease 不复活。
8. 人格/主体提示词候选自动晋升后仍只有一个活动主体、相同工具协议和当前权限边界；回放评价不是第二个能对外行动的主体。

### 13.7 新增一手来源索引

| 主题 | URL | ref |
|---|---|---|
| Windows containers 不支持 GUI | https://learn.microsoft.com/en-us/virtualization/windowscontainers/quick-start/lift-shift-to-containers | `turn91search0` |
| Apple silicon macOS VM | https://developer.apple.com/documentation/virtualization/running-macos-in-a-virtual-machine-on-apple-silicon | `turn91search1` |
| macOS 屏幕访问权限 | https://support.apple.com/guide/mac-help/control-access-screen-system-audio-recording-mchld6aa7d23/mac | `turn91search2` |
| macOS Accessibility 权限 | https://support.apple.com/guide/mac-help/allow-accessibility-apps-to-access-your-mac-mh43185/mac | `turn91search5` |
| 浏览器 CDP Accessibility 域 | https://chromedevtools.github.io/devtools-protocol/tot/Accessibility/ | `turn91view0` |
| RFB 协议 | https://www.rfc-editor.org/info/rfc6143/ | `turn91view1` |

本节 provider/lease/epoch/gate 等为本产品规格建议，不是声称上述协议已经提供完整的任务授权、可靠回滚或 exactly-once 效果语义。
