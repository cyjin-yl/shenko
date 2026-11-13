# Web 界面契约 — [§21](web-interface.md#spec-21)、[§41.18](web-interface.md#spec-41-18)、[§43](configuration-playground.md#spec-43)（C11 + architecture-web 切片）

**内核定位。** 参考插件，建立在[内核合同](kernel.md)之上。界面读取的是账本里的事实（[内核 3](kernel.md#kernel-3)），输入与输出经[内核 7](kernel.md#kernel-7)接入；它是主体时间线的一个读者和输入来源，可以替换。


<a id="why-10"></a>
<!-- migrated-contract: WHY:10 -->
## 为什么界面从一个主体开始

默认 WebUI 是主体的时间线和输入；用户是在它的持续注意力中 append 或 steer。独立的页面聊天代理会破坏这项语义。能力、水位、分支树与 seats 可以按需展开，不必把首页设计成运维监控屏。Magpie 与 Alma 提供的是布局、交互与稳定流式阅读的参照；品牌不需要 slogan 来解释。见 [§21](web-interface.md#spec-21)、[§43](configuration-playground.md#spec-43)。

Playground 让它长期保留兴趣和交流产生的作品。作品可以有前后端与数据，用户操作也能被主体感知。动态 UI 要把意图投影到渠道真实支持的元素上，并保留动作版本、状态与去重，不能只是一张看起来能点的卡片。见 [§21](web-interface.md#spec-21)、[§41](architecture.md#spec-41)、[§43](configuration-playground.md#spec-43)。

<!-- /migrated-contract: WHY:10 -->

本文件是 `crates/shenko-ui` 的对外契约：WS 协议、持久输入、撤回、富呈现、
流式 part、能力协商，以及浏览器客户端的诚实边界。原始 SPEC 前缀保持不变；
实现以 ledger 事实为准，传输成功永不冒充主体接纳。

原生默认入口为 `http://127.0.0.1:<port>`，不自动监听外网。远程部署应提供
明确的认证访问路径。无模型首启与有模型运行装配同一 owner claim 服务；
安装控制凭据不通过页面访问先后、启动日志或模型上下文交付，见
[安装认领与配置契约](configuration-playground.md#ownership--444)。

## 1. WS 协议（`/ws`，全部 JSON，`schema_version` 逐消息校验）

### 1.1 服务器 → 客户端

| type | 语义 |
|---|---|
| `hello` (v2) | `server_version` + `capabilities`（append/steer/withdraw/actions/documents/attachments/durable_input_receipts/committed_replay/streaming）+ `surface`（基线能力事实，revision 0） |
| `capabilities` (v2) | `client_hello` 的应答：`surface` 重算（revision 递增，含 `valid_until`） |
| `timeline_part` (v2) | 已提交 part：全量正文，`chunk_seq=0`，`terminal=true`，`committed_only=true`；`stream/request/attempt` 缺省（当前 OutboxMessage 无此绑定，不推断）。携带流式前缀的 part 会额外发出修正 part（`part_id="text/0@<commit>"`），客户端按替换处理 |
| `stream_part` (v2) | 流式 public-text chunk（仅当真实生产者接入；`streaming:false` 时永不发送）。`step` 可为 0（尚无已提交步；客户端不得虚构）；其余 identity 与 `timeline_part` 同构 |
| `document` (v1) | 已校验并按本连接能力降级的 SemanticDocument（见 [§3](../research/product-positioning.md#spec-3)） |
| `document_error` (v1) | 文档块无法成为界面；原文仍可见，富层丢弃，原因可读 |
| `identity` | 主体身份 revision 变化通知（既有） |
| `receipt` (v2) / `submission_view` (v2) | 见 [§2](architecture.md#spec-2) |

### 1.2 客户端 → 服务器

| type | 语义 |
|---|---|
| `client_hello` (v2) | 显示事实 attestation：`viewport`、`language`、`theme`、`font_scale?`、`reduced_motion?`、`low_bandwidth?`。仅影响降级，不产生权限 |
| `submit` (v2) | `submission_id`/`message_id`/`subject_id`/`mode: append\|steer`/`content`/`attachment_refs`/`observed_step_id?`。拒绝未知字段 |
| `query_submission` (v2) | 重连对账：`submission_id`+`subject_id`+`send_epoch?`（客户端计数，仅回显） |
| `withdraw` (v2) | `submission_id`+`subject_id`+`cancel_id`；持久控制记录，不删除已接纳事实 |
| `action` (v2) | `document_id`/`action_id`/`revision`/`nonce`/`payload`（取自 DocumentOption 的 canonical payload）；服务端重算 blake3，不信任客户端摘要 |

## 2. 持久输入与撤回（[§41.18](web-interface.md#spec-41-18)）

- 接纳顺序：先 `Store::admit_web_submission`（InboxEvent + submission 绑定同事务），
  再返回 `receipt`。同一 `(subject, submission_id)` 同 payload 重放返回原记录
  （`replayed:true`）；不同 payload 为 `conflict`，原始事实永不改写。
- 传输断开≠丢失：客户端以原 `submission_id` 查询/重发；服务端从 ledger 对账。
- **placement 诚实**：当前 adapter 无已验证的请求内控制能力，append 与 steer
  一律 `actual=next_boundary`（`target_request_id` 在已封口 steer 时记录）。
  `live_steer`/`cancel_reseal` 仅存在于线协议，本代不产生。
- **撤回状态机**（store 侧归 ArchSubject）：未呈现（未 disposition、未封口选入、
  未 stage 进 Planned 步）→ `withdrawn_before_presentation`（S0 消费时排除）；
  否则 `withdrawal_queued`，由 S-折入注解。行永不删除；`commit_seq` 提供全序；
  重启后按 ledger 重放。原始事件与接纳事实保留。
- `submission_view.result`：`found`/`not_admitted`（持久证据）/
  `temporarily_unavailable`（永不冒充 not_admitted）/`invalid`。

## 3. 富呈现（[§21.5](web-interface.md#spec-21-5)、[§21.9](web-interface.md#spec-21-9)）

- 已提交正文中的 ```` ```shenko-ui ```` 围栏块为 `SemanticDocument`（schema v1）。
  校验：已知节点 kind、action 绑定（payload blake3 = canonical JSON 摘要）、
  fallback.summary 非空。校验失败 → 文本降级，绝不出半可执行界面。
- 降级阶梯：`rich → card → text`；每档保留编号 ActionOption，option N 恒定对应
  同一 `(action_id, nonce)`，“选 2”与按钮点击是同一决定。
- 能力协商：`SurfaceCapabilities{revision, valid_until, streaming(服务端事实),
  native_cards, buttons_forms, …}`；客户端事实仅 attestation，`streaming` 只来自
  真实接线。未知能力降级为已确认基础形态。
- 动作执行权：点击只接纳一条持久输入（dedup key = submission 键，`kind="ui_action"`），
  主体像处置任何输入一样处置它；`actor_scope` 非 owner 拒绝。

## 4. 流式 part（[§21.11](web-interface.md#spec-21-11)）

- 表 `subject_output_parts`（PK `(agent_id, stream_id, message_id, part_id, chunk_seq)`，
  索引 `(agent_id, commit_seq)`）。Append-only：同键同内容=幂等重放（返回原 commit），
  同键异内容=conflict（不改写）。无 update/delete 路径。
- `content_kind` 恒为 `public_text`：脱敏是生产者责任，本表不再过滤。
- **生产者契约（依赖 ArchBroker/ArchSubject）**：`message_id`=arm 时预分配的
  step_id；`part_id="text/0"`；`part_revision`=attempt 级 revision；chunk_seq 从 0
  连续；producer 在写入前保证 public-text 脱敏。`StreamSink::publish` 先持久后广播；
  `parts_after(cursor)` 供重连断点续传；chunk_seq 空洞由读取方 surfaced，绝不补。
- UI 诚实：无生产者时 `streaming:false`，无 chunk 路径，无打字机动画；
  已提交 replay 不受影响的。

## 5. 客户端（web/src）

- `projection.ts`：UiState 增 `capabilities`/`surface`/`streaming`/`documents`/
  `document_errors`；Submission 增 `placement`/`withdrawal`；新增
  `ServerCapabilities`/`SubmissionPlacement`/`WithdrawalView`/`DegradedDocument`/
  `ActionOption` 类型；composer 增 steer 动作；submission 节点增 withdraw 动作
  （按 props 传 action id，不靠 refs 下标）。
- `live-controller.ts`：hello 解析全部能力；onopen 发 `client_hello`；新处理
  `capabilities`/`document`/`document_error`/`stream_part`；`applyChunk` 统一
  committed/stream 合并——committed 全量正文 seq 0 被吸收为 S1 绑定（两种到达
  顺序幂等），流 chunk 落在 committed 基 part 只补 identity 不补文本；空洞进
  pending buffer，绝不拼接；`sendWithMode`/`sendWithdraw`/`sendAction`。
- 视觉层（2026-11-12 重做）：`ui-kit.tsx` 适配全部上游 family 并给出统一按钮/
  toggle；`react-renderer.tsx` 是外壳、时间线与输入区；`surfaces.tsx` 是按需
  页面与主体详情 drawer；`markdown.tsx` 按块渲染已提交正文（未闭合代码围栏从
  第一行起就是代码块并标明未闭合）；`icons.tsx` 是本地线性图标。视觉原则：
  中性底色、单一强调色（浏览器暴露 `AccentColor` 时跟随系统）、层级来自字号与
  留白、只有真实状态才有颜色（扁平状态点 + 文字，无光晕/脉冲）、外壳不可选中
  且保持箭头光标、动作结果显示为安静的状态行而非浮动 toast。
- `react-renderer.tsx`：SemanticDocumentView（summary + 编号选项按钮，Rich 显示
  节点树 JSON）、ReceiptView 增 placement/撤回按钮/撤回标记、Composer 增
  Steer 按钮与流式事实文案、day 分组兼容 document 节点。详情页按投影的
  `kind` 读取 `tasks_surface`、`memory_surface`、`config_surface` 和
  `playground_surface`；导航后呈现接口的实际数据、空状态或错误，不能只改变
  当前导航项而留下空白页面。
- 已提交消息与流消息共享同一条消息校验和类型边界；只有校验后的消息可进入
  拼接与待补读缓冲区。流消息允许 `step=0`，已提交消息仍要求正数步骤与
  `step_id`；类型检查不能靠转换、占位步骤或放宽消息校验通过。
- `demo-controller.ts`：补齐新 UiState 字段（demo 全 false/空）。

## 5.1 界面扩展：主体流与镜头（2026-11-12）

界面有两个视图（见 [5.2](#5-2)）：**对话** 是默认的主要交互界面；**Inspect** 是主体
连续思考的原始流（时间线 + 输入）。任务、记忆、Playground、配置以及作品都是
**镜头（lens）**：在当前视图旁打开，不离开视野；窄屏时镜头覆盖视图，关闭即回到原
阅读位置。外壳没有常驻侧栏导航；视图切换在顶部居中，镜头栏在顶部右侧（手机在底部），
Ctrl/⌘ 1–9 打开第 N 个镜头，Ctrl/⌘ 0 关闭。Inspect 左侧的竖线是主体连续的 thought
loop：每个已提交轮次是线上的一个实心点，未提交的流式内容是空心点，作品与文档从线上
分出，用户输入落在线旁。顶部的注意力行只陈述事实：进行中的任务（不计结构性的
`subject_root`）、最近已提交轮次和上报的主体状态。

镜头由 `web/src/slots.ts` 的贡献注册表提供：外壳自己的页面与外部贡献走同一个
`registerLens()`，卸载即移除，不存在“内置导航 + 二等扩展”。

**默认信任（所有者决定，2026-11-12）。** 作品与插件的界面是受信任的扩展：作品页与
外壳同源、不加 sandbox；`/playground/{id}` 只固定声明的类型（`nosniff`）并给出
`x-artifact-revision`。

| 来源 | 呈现 | 权限 |
| --- | --- | --- |
| `shell` | 本 bundle 的组件 | 外壳自身 |
| `work` | Playground 作品**已采纳**的页面（`GET /playground/{id}/adopted`：活动 manifest 的前端所指的那个版本），同源 iframe；作者写下的较新 head 是草稿，只能由读者明确选择“草稿预览”打开；没有已采纳版本时镜头说明这一点 | 与外壳相同；安全绳是命名修订与回退，以及安全模式 |

Why：主体会在没有人盯着的情况下自己编写、在沙箱里自测并改进这些扩展，任何改变都是
可指认、可回退的新修订（[内核 8](kernel.md#kernel-8)）；不加人类审批门、产品白名单
或静态拒绝（[内核 9](kernel.md#kernel-9)、开发规则“不以硬拒绝替主体判断”）。所以
默认信任，失败由回退和救援处理，而不是事前隔离。救援入口：地址加 `?safe=1` 时外壳
不打开任何作品页（[§21.3](#spec-21-3) 的独立救援要求）。已知代价：受信任的作品页能
以所有者会话调用 `/api`；如果一页由外部内容诱导写成，它在所有者打开时以所有者身份运行。
这由主体的来源意识、自测与回退承担，记录在此以便日后按证据修订。

这里的默认信任授予已安装扩展使用其运行环境与能力声明的权限，不改变
[§21.3](#spec-21-3) 的服务协议、能力检查和数据可见范围，或
[§21.6](#spec-21-6) 对任意代码组件声明运行环境与能力的要求。作品页面经所有者的
Web 会话调用服务；页面和第三方 slot 贡献都不直接读写 SQLite，业务副作用仍由任务与
operation ledger 证明。能力目录中的插件贡献接线属于下方明确列出的后续实现。

DeepSeek Harness 的 Web 客户端以 Cordis 插件向类型化 slot（`ctx.slots.register/inject`，
list/keyed/chain/hoc 与 priority 覆盖）注册同源 React 组件，安装即信任。Shenko 采用
同样的“外壳也是贡献者”结构；插件的界面贡献按 [内核 7](kernel.md#kernel-7) 只经能力
目录登记，外壳从能力目录发现，不另开扩展路径。

尚未实现，按顺序推进：插件经能力目录声明镜头、会话来源与记录视图（见 5.2 的 slot）；
插件界面以 Proto-UI Prototype 交付，由外壳经 adapter 渲染；仅主题 token 的皮肤贡献。

HTML 作品在时间线中同时提供页面入口与“详情与操作”；动作绑定正文响应的 `x-artifact-revision`，不混用记录修订。
作品列表尚未加载或暂时失败时保留已知镜头，仅依成功取得的列表移除已不存在的作品。
浏览器拒绝写入偏好时，固定与取下仍在当前页面生效；镜头快捷键不抢占输入框与可编辑内容中的按键。

<a id="5-2"></a>
## 5.2 对话与 Inspect：群聊、委派与 Proto-UI slot（2026-11-12）

**两个视图。** 对话（IM）是给所有者的主要交互界面，采用 Messages 的视觉惯例：左侧
会话列表，右侧当前会话；查看者一侧为系统蓝气泡，其他参与者为灰气泡，连续同一发言者
合并、末条带尾巴，群聊在首条上方标发言者、末条旁显示其字母头像，间隔超过 15 分钟显示
时间，回执在末条下方（已发送 / 已送达 / 已选入 / 已处理，分别对应 sent、accepted、
selected、committed 四个独立事实，不是进度条）。Inspect 保留 5.1 的原始思考流。

**一切对话都是带参与者的会话。** `web/src/threads.ts` 用同一模型表示三类会话，差别只在
参与者与查看者坐在哪一侧：

| 会话 | 参与者 | 查看者一侧 | 每条消息的事实来源 |
| --- | --- | --- | --- |
| 直接对话 | 你、主体 | 你 | 你的提交与回执；主体的消息是**选入了你那条消息的已提交步骤**（回执的 `selected_step_id`），不猜测；主体自己思考的轮次合并为一行“独自思考了 N 轮”，可跳到 Inspect；只有真实持久流式内容显示为“正在输出” |
| 委派（群聊） | 主体、子代理、工具 | 主体（旁观） | `GET /api/tasks/{id}/trajectory`：任务的封存输入（委派内容）、按账本顺序的操作及其最终证据封存的规范化记录（每步正文、工具调用、提问、所有者消息、token 用量），以及它拥有的子任务（工具 child） |
| 渠道群聊/私聊 | 主体、对方或群成员 | 主体（旁观） | `GET /api/conversations`，见下文“主体全部对外沟通” |

<a id="5-2-external"></a>
**主体全部对外沟通（owner 决定，2026-11-13）。** 对话界面展示主体对外的全部沟通，不只是
它和你的对话：

- **任何渠道插件都一样。** IM 平台、内网里接触到的其他节点、具身机器人的通道，都只是又
  一个渠道插件，不是特例。
- **对方是谁都一样。** 对方可以是人、机器人，也可以是不同主体的另一个 Shenko。
- **私聊。** 渠道支持私聊时，私聊是一对一会话：没有发言者名字，只显示一个头像。
- **委派。** 委派群聊仍然列在其中，那是主体的内部协作。
- **同一主体的其他节点。** 它们是同一个自我（见 [nodes](nodes.md)），不是会话对象。

Why：主动主体的对外行为是你最需要看见、也最难从思考流里拼出来的东西。一个会话列表能
回答“它在和谁说话、说了什么、送到了没有”。

会话的事实来源：

- **入站。** 每条入站消息读自它真正的持久位置：取走它的那一步的暂存输入
  （`session_events` 中 `{"v":1,"inbox_event":…}`，负载为 `ChannelEnvelope`）。渠道消息
  不经 `inbox_events` 表。被延后的消息可能由后续步骤再次暂存，按入站事件 id 去重。
- **出站。** 已提交的 publication 按最近读取。时间优先取平台回执的 `observed_at`，没有
  回执时取做出决定的那一步的开始时间。状态取 publication 状态。
- **归组。** 两个方向按回复对象 `account|kind|id` 归为同一会话。
- **一对一还是群。** 由插件在 `ChannelReplyTarget.shape` 里声明（`one_to_one` /
  `group`）。它是受众的性质而不是回复类型的性质：同一个飞书 chat id 可能是单聊，也可能是
  群。没有声明的会话按群显示，界面不按 `private` 之类的词表猜。
- **本账号的消息。** 平台回报“本账号从其他客户端发出”的消息，由插件标为
  `ChannelActor.own_account`，显示在主体一侧并注明来源。若它的平台消息 id 与某条
  publication 回执的 `provider_message_id` 相同，它就是那条 publication 的回声，不再列出
  第二次。
- **对方是谁。** 只按事实认定：持有所有者设定的管理授权（`role_grant_ref`）的发言者标为
  管理员；联系人目录（[contacts](contacts.md)）中的身份按其证实状态显示。“对方是另一个
  Shenko 主体”同样必须是一条有来源的事实，例如经验证的对等身份或目录中的已证实关系，
  绝不按昵称、头像或消息正文的自称判断。

尚未具备，依次推进：

1. **其他 Shenko 主体的身份事实。** 目录还没有“对方是 Shenko 主体”这一类关系，对等身份
   也还没有验证协议。在那之前，另一个 Shenko 显示为它在该渠道的名字。
2. **内网和具身机器人的渠道插件。** 目前还没有。它们接入后无需改界面。
3. **以主体名义在渠道里发言。** 网页还不能这样做：你在会话里写的话，应当作为给主体的消息，
   由它决定是否转达，而不是代它发言。

旁观的会话不显示输入框：网页还不能代主体在渠道里发言或给运行中的子代理发消息，界面
如实说明。Inspect 中每个委派从发起它的已提交轮次分出（任务的 `created_by_step_id`），
可就地展开同一条轨迹，或“在对话中打开”成为群聊。`subject_root` 自己拥有自己，不算
父任务也不算子任务。

**slot：外壳与插件走同一组扩展点。** 三个注册表，外壳自己的实现也经它们登记：

| slot | 形状 | 用途 |
| --- | --- | --- |
| `registerLens` | list，按 `order` | 视图旁的镜头 |
| `registerThreadSource` | list，按 `order` | 会话来源（直接对话、委派、渠道由外壳登记） |
| `registerRecordView` | keyed，按 `operation_name` | 轨迹中一种封存记录的呈现；未登记的记录显示为名称与状态，不丢失 |

**仍用 Proto-UI。** 会话列表、视图切换与镜头栏是上游 toggle-group（列表为 vertical
方向，单选、方向键与 roving focus 由上游提供）；委派展开用上游 collapsible；气泡是
Shenko 自己写的 `prototypes/message-bubble.proto.ts`，构建期由 Proto-UI compiler 编译
为 React DOM 源码，`side/tail/tone` 由 Prototype 规则决定外观，消息内容走匿名 slot，
因此正文更新是普通 React 内容。注意 Proto-UI 规格 C-PROPS-0014：props 变化本身不触发重绘；模板直接读取 prop 的 Prototype 要 `def.props.watch([...], (run) => run.update())`（transport-badge 即如此），运行时 React adapter 默认 `autoUpdateOnPropsChange` 会掩盖这一点，编译产物不会。
Proto-UI 的 slot 优势在于：Prototype 与宿主无关（React/Vue/Web Component/编译产物），
anatomy 以 family/role 声明结构与顺序，context 承载数据；插件界面交付 Prototype，外壳
用 adapter 放进上表的 slot，同一份界面可被不同宿主渲染。

## 6. Proto-UI pin

`web/upstream-pin.json`：`35195a6e6f1ba6a7d800da0029f2683013abd18b`
（2026-11-12 主线，migrated_from `b25229a…`）。上游 `pnpm-lock.yaml` sha256 不变
（`3ac437fd…`）；adapter-react 签名与 shadcn 导出经比对一致。界面采用的 shadcn
family：button、textarea、scroll-area、collapsible、drawer（主体详情）、
toggle-group（追加/引导）、toggle（代码软换行）、input（命名、认领、配置）、
separator（日期分隔）。tooltip 未采用：其 trigger 自身可聚焦，包住按钮会产生
双重 Tab 停靠。

构建时由该提交的 `scripts/build/public-packages.mjs` 解析 pin 中
`adapter-react`、`prototypes-shadcn`、`cli` 三个入口的依赖闭包，而不是调用
另一份源码或使用旧提交的 `dist`。本 pin 的闭包为 42 个包；这不代表全部
上游适配器均已构建或运行。`PROTO_UI_SOURCE` 必须通过真实 Git HEAD 和 lock
摘要校验，`PROTO_UI_BUILD_RECEIPT` 绑定这次上游构建。使用 Node 24 与锁定的
依赖运行类型检查和 `web/build.mjs`，由真实 adapter、esbuild 和官方 CLI
生成资源、样式、许可及带资源摘要的 manifest。Rust 将这些资源嵌入二进制，
因此资源重建之后还必须重新编译并启动 UI；修改 manifest 或只打开旧二进制
不能证明升级生效。

### compiler：构建期编译 Shenko 自己的 Prototype

该 pin 的 `@proto.ui/compiler` 是私有源码包：`exports` 只有
`./src/index.ts`，没有 `dist`，也不在 42 个公开包的构建选择里。所以它既不能
当包产物导入，也不能假装存在运行期编译器。现行路径是**构建期编译**
（`web/compiler.mjs`）：

- 用同一份审查过的源码，审计 `packages/compiler/package.json` 的包身份、私有标记
  与 `exports` 入口，再对其 `src/` 计算确定性目录摘要；
- 用工作区锁定的 esbuild 把该 TypeScript 模块图打成一份 Node ESM
  （`generated/compiler/`），`typescript` 保持外部依赖（compiler 声明 `^5.0.0`，
  由 web 锁定的 devDependency 满足）；`@floating-ui/dom` 由声明它的
  `packages/modules/positioning` 安装副本解析并记录版本与摘要；
- 用 `compileFile` 编译 `web/prototypes/*.proto.ts`（Shenko 自己写的
  Prototype），profile 为 `react-dom-source-v1`，输出 create-only 写入
  `web/generated/prototypes/<name>/`，含 compiler 自己的 `provenance.json`；
- 浏览器 bundle 只导入生成的 React DOM 源码，因此 manifest 记
  `runtime_imported: false`；compiler 源码摘要、bundle 摘要、每个 Prototype
  的源字节摘要与生成摘要都写进 `build-manifest.json` 的 `compiler` 事实里。

`web/generated/` 是构建产物、不入库；`tsc` 需要先有这次编译，因此
`pnpm check` = `node build.mjs --prototypes-only && tsc --noEmit`。

界面上的 transport 状态徽标就是这个编译产物：Prototype 只负责呈现（props
`label`/`tone` 与 token 样式），连接事实、文案与推理仍留在
`react-renderer.tsx`，没有搬进 compiler 或 Prototype。

<a id="6-compiler-mode"></a>
### compiler 模式是目标（owner 决定，2026-11-13）

owner 决定：Shenko 的界面以 compiler 模式交付。运行时 adapter 是过渡桥，只用在
pin 的 compiler 还编译不了的上游 family 上，不是终点。

**Why（实测，候选 `c0497fb`，Proto-UI `35195a6`，React 19.3.1 production，
headless Chromium）：**

- **体积。** `app.js` 压缩后 1092.5 KiB（gzip 312 KiB）。其中 Proto-UI 的
  runtime、adapter、modules、prototypes 及其依赖约 680 KiB；react-dom 是 205 KiB，
  Shenko 自己的源码是 152 KiB。
- **同一个 Prototype，两种宿主。** `message-bubble` 挂载 150 个：
  - 编译产物和纯 DOM 在误差之内；
  - 经 adapter 多 83 ms。
- **上游 shadcn family 经 adapter 的开销。**
  - button：每个实例挂载约 2 ms。
  - toggle-group：挂载是超线性的（150 项要 6.0 s）。根因在上游，见下文。
- **真实界面（demo 数据）。** 对话与 Inspect 来回切换，每次 50–138 ms。十次切换里，
  Proto-UI 的 JS 时间是 693 ms，React 是 130 ms，Shenko 是 71 ms。

**为什么还不能全部切换。** pin 的 compiler 能编译的 shadcn 文件是 0/216。界面用到的
每个 family 都在 compiler 里有具体阻塞，见下表（`react-dom-source-v1`，第一处诊断）。
shadcn 层先全部卡在 `PUI1003 Unsupported source package @proto.ui/prototypes-base/*`；
解决之后，base 层还有：

| family | base 层阻塞 |
| --- | --- |
| button | 无，base 可编译；shadcn 层只差包解析 |
| toggle | `PUI1004` 子节点里的三元表达式 |
| toggle-group | `PUI1004` 模块级运行时绑定；context 嵌套崩溃（#295 F1） |
| collapsible | `PUI1003 defineHook`；context 嵌套崩溃 |
| scroll-area | `PUI1006` context 值类型、非原始 prop、语义句柄进入数组 |
| input、textarea | `PUI1003 asControlLabel` |
| separator | `PUI1006` 只准入已声明的原始 prop |
| drawer | `defineHook`、`asAxisInput`、数据类型、模板插值 |

**禁止的捷径。** 在 Shenko 里用 compiler 可编译的子集重写这些 family，或者构建前把
shadcn/base「拍平」成本地文件再编译，都不允许。那样焦点、无障碍和 collection 行为就和
上游分叉了，也正是 #295 审计列出的禁止做法。

**迁移规则。**

- **切换条件。** 某个 family 同时满足下面两点时，才在 `ui-kit.tsx` 这一处边界换成
  编译产物：
  - pin 的 compiler 能编译上游的 shadcn 版本；
  - 差分一致性注册表覆盖了它的行为，包括焦点与 disabled（F7）、props 更新（F5）、
    context（F1、F4）。
- **记录。** 切换时记录前后的实测数字。
- **顺序。** 按实测成本排：
  1. toggle-group（会话列表、视图切换、镜头栏）；
  2. button；
  3. collapsible；
  4. scroll-area；
  5. input、textarea；
  6. toggle、separator、drawer。
- **体积。** 编译产物现在每个组件各带一份辅助运行时：两个小组件就是 40 KiB（#295 F6）。
  所以体积要等 F6 解决后再比较，不能假设 compiler 一定更小（§21.2）。
- **插件界面。** 插件界面以 Prototype 交付。目标是在安装时编译；在此之前，插件界面
  经 adapter 渲染。

**过渡期的上游修复。** toggle-group 超线性的根因在上游：
- anatomy 每次查询都按 `compareDocumentPosition` 重排整个域；
- collection item 的 meta 通道又会算自己的位置；
- 一次结构变化时，域里每个监听实例都各自重排一遍。

修复让一轮通知内每个域只排一次序，并且只返回 meta。150 项从 6.0 s 降到 0.42 s，
300 项是 1.34 s。修复已在本地提交为 Proto-UI 会话分支上的 `5db86b49`，但这次会话没有向上游推送的
权限，所以还没有上游 PR。合入主线后随 pin 升级采用；在那之前 Shenko 仍按本节的 pin 构建。

界面验收需实际切换页面并对照当前接口与嵌入 manifest；壳层可见、HTTP 200
或构建成功均不足以证明页面可用。独立 UiServer 的只读页面验证不替代真实
主体、provider、持久输入和流式接线验收。

## 7. 依赖与集成注记

- `Store::submit_web_withdrawal`/`web_input_withdrawal` 与 raw_session.rs 撤回排除
  归 **ArchSubject**（其树已落地；本树调用点已就位）。
- `InferenceStreamEvent` 生产者与 part 写入器归 **ArchBroker/ArchSubject**；
  未接线时 `streaming:false`，全部测试不依赖流。
- server.rs 中的 `media_runtime` 字段/`with_media_runtime()` 归 **ArchMedia**。
- 父代执行：cargo check/test（shenko-store + shenko-ui）、
  `node build.mjs --prototypes-only` + `tsc --noEmit`、`node build.mjs`、
  原生浏览器验证、发布前 pin 新鲜度。构建产物（`src/static/*`）与 Rust 嵌入
  由父代重建；未重建时不得声称界面已升级。

<a id="spec-21"></a>
<!-- migrated-contract: SPEC:21 -->
# 21 Web UI 与设计系统

<!-- /migrated-contract: SPEC:21 -->

<a id="spec-21-1"></a>
<!-- migrated-contract: SPEC:21.1 -->
## 21.1 体验目标与功能要求：交由 Claude Opus 设计实现

本节给 Claude Opus 提供产品能力、信息关系、状态语义和体验验收；具体视觉语言、色彩、组件与动效由其设计实现。**默认 WebUI 只展示主体的一条时间线和输入区**，输入以 append/steer 接入同一个持续 thought loop，不另开聊天 Agent。时间线就地呈现主体愿意表达的内容、当前交流所需进展和结果；不要求展示原始内部思维。任务、记忆、插件、设置、Playground、KV、分支树和 Agent seat 等能力均有按需入口，不能重新堆成默认监控首页。产品应活灵活现，由布局、状态与交互表达；正式品牌 Shenko 无 slogan。操作标签、错误、真实回执和恢复结果仍清楚可读。

| 用户要完成的事情 | 必须具备的内容与交互 |
| --- | --- |
| 初始化与命名 | 区分框架品牌和实例姓名；可由用户取名，或让 Agent 自主取名；恢复已有实例时保留其身份与姓名 |
| 默认主体时间线 | 用户输入、主体表达、相关进展与结果；append/steer 的 accepted/selected/committed 回执可区分，不开启独立聊天主体 |
| 理解或调整一项工作 | 目标、阶段、证据、成果与可用 steering；可以进入 child 协作和原始现场 |
| 处理需要注意的事项 | 未决问题、完成结果、投递状态与可继续的动作，不要求逐个查看工具日志 |
| 回忆人与经历 | 联系人、来源、共同经历、承诺、历史修订与可解释的回忆结果 |
| 接入和管理环境 | 节点、渠道、插件、包、SSH 公钥、更新与恢复入口；呈现操作的真实效果 |
| 深入了解运行情况 | 按需展开 KV、队列、预算、分支、Agent seat 和健康状态；技术细节有完整入口 |
| 通话、旁听并继续办事 | 收听、说话、录音、近期语境和正在进行的任务状态；工具协作持续可用 |
| 使用动态生成式界面 | 在目标渠道完成选择、输入、比较、查看证据；弱界面保留相同语义和可完成操作 |
| 查看和接续自身开发 | 候选变更、验证证据、当前/候选运行版本、迁移进度、真实恢复范围及仍在运行的 seat；可从 QQ 或 Web 继续 steering |
| 问它如何工作 | 从自然对话进入有出处的自身说明，区分设计、当前实现、实际启用和候选；需要时展开源码/文档与 hook 详情 |
| 对话调整与面板查看配置 | 共用同一配置服务，显示 effective/候选/验证/恢复；主体能自主委派更改，坏端点/代理有非 LLM 恢复路径 |
| 长期作品与 Intelligent UI | Playground 按需打开，持久运行有前后端/数据的作品；人的动作与后台变化形成可追溯事件，进入主体同一 inbox |

上表描述产品整体能力，不要求全部常驻首页。唯一主体时间线与输入区的默认结构已由用户确定；其余导航和展开形式由设计 Agent 自主完成。视觉验收关注生命感、辨识度、节奏、中文可读性和状态变化，流式输出遵守 [§21.11](web-interface.md#spec-21-11) 的锚点、选区、交互保持与响应门槛。动效对应真实状态，不伪造进度；小屏、键盘/焦点、屏幕阅读与 reduced motion 为正式要求。设计不能改变主体串行、故障收敛、真实数据/动作/恢复语义；当前只形成 SPEC，未实现界面。

使用 shadcn 当期组件与 token 体系，参考 2026 年 10 月新增的 AI 工作流表达，但不把某个流行外观当作唯一“2026 标准”。第三方 Trace/Reasoning 组件只展示真实可公开的摘要或事件，不自动输出完整内部思维。[shadcn changelog](https://ui.shadcn.com/docs/changelog)

<!-- /migrated-contract: SPEC:21.1 -->

<a id="spec-21-2"></a>
<!-- migrated-contract: SPEC:21.2 -->
## 21.2 Proto-UI 主线和 compiler seam

每个开发任务开始核验 Proto-UI 主线；将相关包整体 bump 并 pin 到同一已验证 commit，记录旧/新 SHA、迁移、兼容结果与 upstream 变化。合并/发布前再次检查主线是否变化。不能长期固定旧 RC，也不在设备启动时无验证拉取漂移的 main。

若最新主线导致必要功能失败，保留上一健康运行版本，明确升级阻断与修复计划，不能伪写“已升级”。“每次追主线”是开发与更新管理要求，运行中的自主升级仍走候选 generation、验证和原子激活。

应用的领域模型、交互状态、无障碍语义和设计 token 与 renderer 分开；UiProjection 提供语义组件、事件、布局约束和资源引用。当前 React/runtime adapter 实现投影，未来 compiler adapter 接收同一应用语义并返回编译产物。只允许边界层依赖不稳定 compiler API；不在各页面散布其内部实现细节。

compiler 迁移门槛包括状态一致、事件/焦点、SSR/离线静态部署需求、插件 UI、CSP、无障碍、国际化与主题回归。不能假设 compiler 必然更快更小，届时比较实际构建和运行成本。

<!-- /migrated-contract: SPEC:21.2 -->

2026-11-13 owner 决定以 compiler 模式为目标，adapter 只作过渡。实测成本、逐 family 的阻塞、迁移规则和过渡期的上游修复见 [§6 compiler 模式](web-interface.md#6-compiler-mode)。

<a id="spec-21-3"></a>
<!-- migrated-contract: SPEC:21.3 -->
## 21.3 UI 本身也是插件

Webserver 与 Web UI 可替换，默认提供标准管理/交互体验；headless 节点可只运行协议端。UI 插件通过版本化服务协议读状态，不能直接读写 SQLite 或绕过能力检查。第三方插件的界面扩展受同一权限和数据可见范围约束。

字体、图标和静态资源默认本地提供，不依赖外部 CDN 或默认遥测。插件卸载后清理路由/资源；升级失败时仍能通过独立救援入口恢复健康 UI。完整 rescue 不依赖失效前端。

<!-- /migrated-contract: SPEC:21.3 -->

<a id="spec-21-4"></a>
<!-- migrated-contract: SPEC:21.4 -->
## 21.4 Dynamic Response 与官方参考边界

用户所说的 Dynamic Response 功能方向完整保留。本轮对应核验到的 OpenAI 正式产品名是 **Intelligent UI**，于 2026-10-07 发布：模型组织文字、视觉和交互，使用可流式呈现的原生组件库及生成时处理界面的 compiler。没有在所查公开合同中找到可直接嵌入本项目的原生 DSL、独立 compiler 或同名 Dynamic Response API，不能把其产品展示当成已开放 SDK。[官方发布](https://openai.com/index/gpt-6-for-everyone/)、[官方帮助](https://help.openai.com/en/articles/20001598-intelligent-ui-in-chatgpt)

公开 Plugins/MCP Apps 的 iframe/bridge 与无 UI 回退、ChatKit 的 widgets/actions，分别提供可借鉴的开发原语；它们与 ChatGPT 内部原生生成式 UI 不等同。Structured Outputs 可以协助生成 typed DSL，但本产品需自己的 renderer、状态和动作合同，不能要求所有本地模型支持某供应商私有接口。[Plugins UI](https://developers.openai.com/plugins/build/chatgpt-ui)、[ChatKit widgets](https://developers.openai.com/api/docs/guides/chatkit-widgets)、[ChatKit actions](https://developers.openai.com/api/docs/guides/chatkit-actions)

<!-- /migrated-contract: SPEC:21.4 -->

<a id="spec-21-5"></a>
<!-- migrated-contract: SPEC:21.5 -->
## 21.5 目标能力协商

SurfaceCapabilityProvider 按 provider/account、conversation/thread、client/session 和 audience scope 报告能力，并带 revision 与有效期。不是只凭平台名假定 Web 一定能执行脚本、QQ 一定不能卡片，或所有飞书账户都具有同一 callback 权限。

能力描述至少含：文本/Markdown 子集、表格/图表/媒体、原生卡片、按钮/表单、可执行组件；输入方式与 callback 身份；消息编辑/patch/流式/大小上限；状态持久化与跨设备能力；viewport、语言、主题、字号、无障碍与减少动画；可用 component runtime、资源访问范围和本轮纯文本/低流量偏好。

模型获得简短能力索引，选择后再展开相关 schema/语法与样例。稳定索引可缓存，具体目标和能力 revision 放在后缀。未知能力以已确认的基础形式处理，重新连接/客户端变化/限流会触发重新协商；“支持显示按钮”不代表按钮自动获得执行权限。

<!-- /migrated-contract: SPEC:21.5 -->

<a id="spec-21-6"></a>
<!-- migrated-contract: SPEC:21.6 -->
## 21.6 DSL 与 UI 工具两条入口

UI/对话 worker 可以获得版本化 DSL，直接生成受约束的语义文档；也可以使用 describe_surface、compose、validate、publish、patch、retire 等拟议结构化工具。两条入口共享同一个语义对象、验证器和动作账本，避免工具版与 DSL 版状态不一致。节点和组件可由插件扩展，未来模型更强时可替换生成策略。

语义文档至少包含 purpose、content/source refs、layout、interaction/action refs、fallback、audience/task、schema/component versions。布局允许自由组合已支持组件，不把所有回答限制成固定业务模板。对较小模型先提供高层操作，例如“比较三个方案”“收集这五个字段”，必要时展开节点级语法；生成器自身的效果也纳入插件认知评测。

主体决定表达和交互意图并委派；不因为动态 UI 获得 ui.run_script 或文件写入工具。生成、修复和需要模型的交互都经同一 broker。新组件可以由自改进子 Agent 编写和安装，但任意代码组件必须声明其运行环境与能力，不能让一段展示文档直接获得宿主权限。

<!-- /migrated-contract: SPEC:21.6 -->

<a id="spec-21-7"></a>
<!-- migrated-contract: SPEC:21.7 -->
## 21.7 跨界面降级

| 用户意图 | 丰富 Web / Proto UI | 平台支持的原生卡片 | 文本/媒体渠道 | 语音/终端 |
| --- | --- | --- | --- | --- |
| 比较方案 | 并排布局、筛选表格、证据展开 | 字段与详情卡片 | 简洁表格或逐项差异，必要时静态图 | 关键差异简报、按需逐项展开 |
| 作出选择 | 绑定提案 revision 的按钮/选项 | 原生 callback 选项 | 编号选项与关联回复 | 自然语言/按键选择同一 decision |
| 填写信息 | typed form、即时校验、草稿 | 可用字段/表单 | 分步收集并保留草稿 | 少量字段逐次询问 |
| 探索因果/数值 | slider、图表、小型确定性模拟 | 可用的参数选择 | 图表/数据及“改参数再算”的对话 | 口述参数，得到解释或附属图形 |
| 查看任务/远端问题 | 证据、现场与动作面板 | 状态卡片/选项 | 有意义的进展和问题编号 | 简报与有效选择，不朗读全部日志 |

降级保持事实、意图与可完成操作，不只是截一张无法点击的卡片。优先在当前渠道完成可完成的任务；受认证 Web 视图或文件是可选增强，不强迫所有用户跳浏览器。语音和终端 fallback 是本产品目标，不据此宣称 OpenAI 当前产品已经支持同样范围。

<!-- /migrated-contract: SPEC:21.7 -->

<a id="spec-21-8"></a>
<!-- migrated-contract: SPEC:21.8 -->
## 21.8 状态与动作闭环

区分展示状态（展开、焦点、图表缩放）、交互对象（表单草稿、共享清单、模拟输入）和业务事实（任务/发送/候选是否实际生效）。前两类可按需持久化，最终副作用由任务和 operation ledger 证明，不能以按钮变绿当成功。

action 绑定 interaction/task ID、action ID、来源 surface/message、允许的 actor/scope、schema、有效期/nonce、业务 revision、语义 payload hash 和所需能力。render revision 与 action revision 分开：调间距无需使按钮全部失效；改变收件人、目标节点、候选版本或含义必须产生新的动作绑定。服务端不相信客户端传来的“已批准”字段。

按钮、“选 2”、语音答案和另一渠道的卡片可归属于同一 decision ID，去重并处理改主意。人类提出的新意图先持久入 inbox，再进入唯一主体序列；已有效委派的有限动作由 worker 完成并回报。展开面板、缩放图表和确定性 slider 计算无需触发模型；不存在独立的 UI 主脑。

共享交互身份不等于共享所有内容：私聊答案不能因同一 decision ID 自动原文广播到群。历史 UI 可重现当时的信息，默认不重新启用过期动作。重放 UI 事件不重做已完成的外部副作用。

<!-- /migrated-contract: SPEC:21.8 -->

<a id="spec-21-9"></a>
<!-- migrated-contract: SPEC:21.9 -->
## 21.9 流式呈现与 compiler 迁移

只发布已校验的节点/patch，保留稳定 node ID、焦点、草稿和布局约束。文本与只读信息可先显示；动作语义绑定完整后才启用。不能把未闭合 DSL 或模型尚在修改的目标变成可点击按钮，也不能在用户操作时无声交换按钮位置和含义。

中断时保留最后有效视图与真实状态；生成完成、验证完成、投递完成、用户查看、动作完成分别记录。纯客户端计算避免每个 slider 更新都请求 LLM；确需新分析时显式提交或合并请求，仍受预算限制。

当前 Proto runtime adapter、未来 compiler adapter、卡片/文本/语音 renderer 消费同一语义对象。迁移要覆盖历史文档读取、动作 revision、状态恢复、焦点与无障碍；不能只测试“新版本能画出来”。界面组件库、字体和执行环境可插件化更新，不扩大小内核职责。

<!-- /migrated-contract: SPEC:21.9 -->

<a id="spec-21-10"></a>
<!-- migrated-contract: SPEC:21.10 -->
## 21.10 Magpie 视觉参照与比较方法

U37 指定 Magpie 的风格与美观程度为参考。本规格先以高相关公开项目 [yetone/magpie](https://github.com/yetone/magpie/tree/398803e3f575dcafc20573f674a98d71105c9694) 作为工作参照，固定提交 398803e3f575dcafc20573f674a98d71105c9694，2026-10-22。已查看该版本的 [Agents 完整页](https://github.com/yetone/magpie/blob/398803e3f575dcafc20573f674a98d71105c9694/site/public/img/agents-light.png)、[紧凑面板](https://github.com/yetone/magpie/blob/398803e3f575dcafc20573f674a98d71105c9694/site/public/img/panel-light.png) 和 [模型选择器](https://github.com/yetone/magpie/blob/398803e3f575dcafc20573f674a98d71105c9694/site/public/img/picker-light.png)。原始指代尚未由链接唯一确认；若后续用户所指不同，替换视觉来源即可，不阻断其余规格。REF-MAGPIE 与 B35 保存比较范围。

| 参照中可观察的处理 | Shenko 的设计要求 |
| --- | --- |
| 连续列表、对齐的对象/状态/控制，轻分隔与克制容器 | 用空间、文字层级和稳定位置让人看懂当前关注、对话、委派与成果；不要求所有信息都装进大卡片 |
| 操作就在对象旁边，紧凑视图仍保留常用控制 | 任务、会话、seat、模型等常用动作就地可达；详细参数按需展开 |
| 搜索、来源分组、当前选中标记与焦点各有表达 | 长列表能区分相似工具/联系人/节点；焦点与已选状态不混淆，键盘与屏幕阅读可用 |
| 完整页与紧凑面板保持同样对象与术语 | 宽屏、窄屏和渠道降级保留对象身份与动作语义，不要求复制相同布局 |
| 中性背景、清楚文字、少量颜色用于状态或选择 | 建立语义 token 和主题；颜色表达真实状态，不能靠满屏状态灯或持续动画制造生命感 |

Shenko 默认首页固定为主体时间线与输入区，其他能力按需展开。Magpie 的模型网关列表适合参考信息组织和操作细节，不直接规定个人主体首页。布局变化应来自实际关注、委派、结果和可继续的工作；不会把静态参照截图当作已观察到的运行时动效。品牌使用 Shenko，无 slogan；实例名称独立，初始化与恢复见 A46。

设计 Agent 对每个采用点保存 source view、对应的产品场景、采用/改变理由与真实实现画面或交互证据。至少覆盖本次实现影响的主题、中文长文、窄屏、焦点、空/加载/错误和动态状态。生命感、辨识度、节奏与可读性由实际交互共同评价，遵守单主体首页边界，其余不用像素抄图或人类逐屏批准代替设计判断。Proto-UI/shadcn 与 compiler 接缝继续遵循 [§21.2](web-interface.md#spec-21-2)；视觉参考不强制引入 Magpie 的框架或宿主依赖。

<!-- /migrated-contract: SPEC:21.10 -->

<a id="spec-21-11"></a>
<!-- migrated-contract: SPEC:21.11 -->
## 21.11 流式输出的阅读稳定性：Magpie 与 Alma 参照

### 目标与证据边界

用户补充 Magpie、Alma 的流式输出几乎不抖动，并将其作为 Web UI 的体验榜样。Shenko 采用这一可感知目标：**新内容持续出现时，用户正在阅读、选择或操作的内容保持稳定；内容增长不反复移动整页、抢滚动或重置交互。** 这不是要求页面停止增长，也不是用慢速打字动画遮住性能问题。

本轮定位的 Alma 是 [alma.now](https://alma.now/) 与 [yetone/alma-releases](https://github.com/yetone/alma-releases)，官网作者链接和发布仓库相互对应；与 yetone/magpie 的上下文一致。其他同名学习、旅游或图书馆产品不作为本节参照。官方资料明确支持实时 streaming、Markdown、代码高亮等；本轮未运行它，也没有对其掉帧、布局位移或不同设备性能实测。

| 来源 | 本轮确实核验的内容 | 不由这份证据推出 |
| --- | --- | --- |
| [Magpie 固定 Agents 图](https://github.com/yetone/magpie/blob/398803e3f575dcafc20573f674a98d71105c9694/site/public/img/agents-light.png)、[紧凑面板图](https://github.com/yetone/magpie/blob/398803e3f575dcafc20573f674a98d71105c9694/site/public/img/panel-light.png)、[picker 图](https://github.com/yetone/magpie/blob/398803e3f575dcafc20573f674a98d71105c9694/site/public/img/picker-light.png) | 已实际查看三个官方静态截图：排版、信息密度、就地操作、细线分隔和状态表达 | 这些截图不能证明流式滚动、动画、运行时帧率或“零抖动” |
| [Alma 官网](https://alma.now/) | 官方将 Markdown、代码高亮和 seamless streaming 列为聊天界面能力 | 营销描述不是可量化性能报告 |
| [Alma 官方 Chat Interface](https://alma.now/docs/guide/chat) | 默认实时流式输出，可关闭；消息支持 Markdown/公式/代码，代码块有复制、换行等选项，产物可在侧栏预览 | 未说明其具体渲染算法、scroll anchoring 实现或某套组件库；不能据此声称它用 ProtoUI/某渲染器 |
| [Alma v0.4.211](https://github.com/yetone/alma-releases/releases/tag/v0.4.211)，2026-10-22T16:29:11Z | 官方发布说明修复：正在 retry 时再点 Regenerate 导致并行 turn，现在替代进行中的 retry | 该修复不能证明所有 retry/流式交互已经无竞态 |
| [Alma v0.4.202](https://github.com/yetone/alma-releases/releases/tag/v0.4.202)，2026-10-20T05:59:24Z | 官方发布说明修复：单条聊天消息渲染失败可能使整个窗口空白 | 可以作为局部渲染故障隔离的反例；不是“当前所有渲染失败必然已解决”的证明 |

Alma 发布归档主线固定为 [e22723b7c6ca8bb74c3bd1ed32e76df790c9d5ba](https://github.com/yetone/alma-releases/tree/e22723b7c6ca8bb74c3bd1ed32e76df790c9d5ba)（2026-09-24）；它是发布/归档仓库。本轮没有读到 Alma 主聊天渲染器的公开源码。以下均为 **Shenko 自己的首版规范与待执行验收**，不把拟采用方案归功为已核验的上游实现。

### 流数据与显示状态

所有正文、代码、工具结果、引用和动态 UI 内容遵守 [§41](architecture.md#spec-41) 的 committed-only 发布：chunk 先持久化，再进入可恢复显示流。显示层可把多个已提交 chunk 合并到一次绘制，但不能把尚未提交的模型 token 提前展示成正文。cursor、速率等临时非内容状态与持久内容分开。

流封套必须关联 stream_id、request_id、attempt_id、message_id、part_id、part_revision、chunk_seq、commit_ref 和内容类型。稳定的 message/part 身份贯穿 streaming、完成、失败、取消和重连；不能按当前文本长度、数组序号、token 数或每次更新时间生成新的 UI key。

同一 part 的 chunk_seq 有序应用，重复序号幂等忽略；遇到空洞先缓冲/补取，不先拼接后到内容再把整段改回去。恢复可以返回已提交快照和后继 cursor。最终正文必须与对应有效 attempt 的持久内容一致；不能为了平滑补造文字、漏掉标点或把旧 attempt 的尾部接到新 attempt。

生成结束、任务结束、工具结束及发送完成各用自己的事件驱动。界面“停止中”不等于 broker 已释放 slot；取消/重试经同一任务协议执行，重复点击不能绕过主体与全局 broker 产生另一条无归属 generation。真实状态即使暂时 unknown 也要准确显示。

### 滚动与阅读锚点

阅读状态至少区分 following_latest、reading_history 和 selecting_or_interacting。following_latest 才自动跟随当前输出尾部；用户向上滚动、拖动滚动条、选择正文或操作已展开内容后进入保留阅读位置的状态。新 token、工具完成或图片加载不能自动将其拉回底部。用户明确点“回到最新”或自行滚到底部后才恢复跟随。

锚点使用稳定 message_id/part_id 加块内位置，例如可定位的文本范围或逻辑块与偏移；不只保存 scrollTop。前面的历史分页、虚拟列表高度修正、图片解析、工具卡片更新后，应保住同一阅读对象及其视口偏移。若原对象被用户主动删除或明确替换，则按可解释的最近存活位置恢复，不跳到任意列表末尾。

在跟随状态下按合并后的内容增长更新位置，不为每个 token 重新启动一段长 smooth-scroll 动画。滚动动画中用户操作应立即接管；键盘导航、搜索定位、文本选择和输入框焦点都不能被自动跟随夺走。移动软键盘/visual viewport 改变时，保持正在编辑的输入位置和当前阅读锚点。

### Markdown 与代码块的稳定呈现

渲染器明确区分已稳定块和仍可能被后继字符补全的尾部块。增量只修改受影响的块；新 token 不卸载重建整个消息、整个历史列表或所有代码块。格式变化确实影响当前尾部时，允许正确的语义转换，但必须保持无关已稳定内容的身份、选区与阅读锚点。

未闭合代码围栏、链接、数学表达式、列表、表格和多字节/组合字符都有明确的临时呈现规则。不得在每个 chunk 到来时让整个回答在纯文本和富文本之间来回切换。最终格式与该版本声明的 Markdown/扩展规则相符；“尾部稳定”不能成为永久显示错误最终结构的理由。

代码块在确认块类型后保留稳定容器、字体、行高、复制入口与用户选择的折行/展开状态。语法高亮可以异步完成，但着色不应改变无关文本的几何布局；不能高亮一次就换整棵代码 DOM，导致选区、横向滚动或复制按钮位置重置。代码继续增长时只增长必要区域，自动折叠/展开不随每个 token 反复触发。

复制操作返回用户当前可见 revision 的完整有效内容；未完成代码明确保留其 partial 状态。点击“运行”是独立 action，绑定完整输入与 revision；不得因流式解析刚凑出按钮外观就让未完整工具/代码片段执行。

### 字体、媒体与容器尺寸

正文与代码的字体、字号、行高和布局 token 在一次内容流中保持一致。首屏使用可及时获得的确定字体方案；后续字体载入不能无补偿地造成整篇换行重排。中文、英文、emoji、组合音标和代码中的宽字符纳入样例；不能仅用英文短句证明稳定。

图片、视频、图表与附件预览在已有可靠元数据时预留匹配空间；尺寸未知时使用明确的占位容器，加载完成后只调整必要区域并恢复阅读锚点。不得虚构图片真实尺寸或把不断增长的空白高度当稳定性优化。引用预览、徽章、token/时间数字及工具状态更新不应不断挤压正文列宽。

窗口、侧栏、字体缩放和移动方向变化时允许必要的响应式重排；重排后保住原阅读对象与用户输入，避免聊天面板因旁侧产物的每次更新反复改变宽度。产物侧栏的打开、关闭、调整宽度是显式交互或明确的产品状态转换，不能由流式 token 连续触发。

### 工具卡片与生成式 UI

同一工具调用以稳定 operation_id/part_id 更新状态、摘要和结果，不把“运行中卡片”“结果卡片”“最终卡片”当三个相互替换的无关节点。用户展开的日志、横向滚动、选区和表单值保留；工具结束不强制折叠用户正在看的详情，也不把所有长结果自动展开到首页。

动态生成式 UI 在 schema 校验通过的完整子树/修订边界呈现。表单、选择器与动作绑定稳定 action_id 和 revision；后续流式内容不重置用户已输入的内容、悄悄改已选项或把过期动作指向另一任务。提交仍走主体事件/授权和效果协议。并非所有渠道具备这种布局能力：QQ 文本/文件等降级保持同一语义、结果与动作身份，不强迫打开浏览器才能理解回复。

渲染某一条消息、图表或插件卡片失败时，错误限制在该内容区域，保留原始内容的可读降级与“重新呈现”入口。会话历史、输入框、导航、其他任务不能随之空白。失败的动态 UI 不自动重新执行产生它的外部操作。

### 更新节奏与可访问性

将显示更新与网络 chunk 到达解耦：在浏览器绘制机会合并已提交的增量，不为每个 token 做整篇语法分析/布局。可用增量解析、缓存已稳定块、工作线程或其他方案；技术选择留给实现，不能为迁移 ProtoUI compiler 固定一套不可替换渲染器。

不添加固定慢速打字队列来伪造流畅，也不让输出已经结束但 UI 仍逐字演完数秒。用户启用 reduced motion 时关闭不必要的进入、光标/光晕或滚动动画。屏幕阅读器避免每个 token 重读整个回答，采用有界语义更新；停止、重试、复制、展开与回到最新可用键盘操作。这里要求的是可验证的行为，不声称仅有 CSS focus 样式就完成无障碍验收。

### 首版测试语料与可判定门槛

至少建立一个可复用的已提交流回放 fixture：长中文/英文混排，嵌套列表、表格、引用、公式、代码围栏、长行、emoji/组合字符、延迟图片、先工具后正文、工具错误、动态表单和取消/重试。测试不同 chunk 划分（单字符/随机小片段/突发批次）、短暂停顿、重复事件、缺段与重连。回放由本地数据驱动，不为每轮 UI 测试增加模型调用。

记录实际浏览器/WebView、设备、viewport、字号/缩放、主题、网络回放模式与数据量。首版基线包括桌面宽屏、约 390 CSS px 窄屏、100%/200% 缩放及 reduced motion；不能把一台桌面机器的结果外推为所有 Android/路由器端都通过。路由器承载核心的成本与用户浏览器渲染成本分别核算。

| ID | 场景 | 通过条件 |
| --- | --- | --- |
| UI-S01 | 用户停在中段阅读，尾部继续输出 10 秒，并在上方补入历史/加载图片 | 同一阅读锚点持续可见；在静态 viewport 且对象未被主动替换时，默认容忍偏移不超过 2 CSS px；不被拉回底部 |
| UI-S02 | following_latest 连续流入、用户中途上滚或拖滚动条 | 尾部增长平顺；用户操作立即停止跟随；只有明确恢复操作/回到底部才重新跟随 |
| UI-S03 | 逐段到达代码围栏、表格、数学式、链接，包含中文/emoji 跨 chunk | 无整篇纯文本/富文本反复切换；无无关稳定块的卸载/重置；最终文本与格式符合持久内容和当前 parser 合同 |
| UI-S04 | 用户在持续增长的代码块中选中文字、横向滚动、改折行 | 后续 chunk/高亮不清掉选择、横向位置和折行偏好；复制结果匹配可见 revision |
| UI-S05 | 工具卡片从准备到运行再到完成；用户已展开日志或填写动态表单 | 同一对象稳定更新；不自动收起、重建、清空输入或重复提交；旧 action revision 被正确拒绝/更新 |
| UI-S06 | 字体/媒体延迟到达，侧栏开合、窗口 resize、200% 字号缩放、软键盘弹出 | 不产生无补偿整页跳动；响应式变化后原阅读对象仍定位可见，合法可滚动范围内恢复至原偏移附近（默认一行高以内）；输入焦点与内容保留 |
| UI-S07 | 消息 parser/代码高亮/某插件组件抛错 | 仅对应区域降级；导航、输入框、其余历史继续可用；修复呈现不重放外部工具 |
| UI-S08 | 已提交 chunk 在发送前/发送后断线，重连重复/补发 | 正文不丢、不重复、不串 attempt；未持久内容没有提前显示；可按 chunk_seq 续接 |
| UI-S09 | stream 中连续点击两次停止/重试；旧结果迟到 | UI 绑定正确的 request/attempt 与替代关系；不制造第二条无限主体或绕 broker 的并发；旧内容只作为原 attempt 事实保存 |
| UI-S10 | 参考设备正常负载下回放文本流 | 从客户端收到已提交事件到对应可见更新，首版目标 p95 ≤100ms；记录长任务和掉帧，不能用延迟打字满足视觉目标。该阈值不包含 LLM/网络/存储等待，也不是未测设备保证 |

锚点偏移、内容完整性、交互状态保留和响应时延分别测量。页面整体布局分数或平均 FPS 不能单独替代上述用户场景；必要的新增内容高度也不能简单算成失败。若基线未达标，保留实测结果和具体差距，不能以“使用了 shadcn/ProtoUI/流式组件”作为通过证据。

### 引用与实现状态

来源：REF-MAGPIE、REF-ALMA、REF-USER（U41/R131）、REF-DESIGN。用户提供的体验描述是产品目标；官方文档与发布归档是已核对参照，尚未对这些产品或 Shenko 做流式性能实测。所有门槛为待实施的本产品验收；默认首页另遵循 [§21.12](web-interface.md#spec-21-12) 的唯一主体时间线。

<!-- /migrated-contract: SPEC:21.11 -->

<a id="spec-21-12"></a>
<!-- migrated-contract: SPEC:21.12 -->
## 21.12 默认 Web 界面：与主体直接交互的一条时间线

WebUI 初始只呈现主体的一条时间线和输入框。输入接入同一 subject 的 thought loop，不启动另一条 chat agent，不以另一个聊天人格替主体作答。时间线呈现用户输入、主体选择表达的内容、简短进展和结果；不要求展示原始逐 token 内部思维。任务、记忆、插件、配置、额度、KV、Seat 和 Playground 通过按需入口展开，不能重新挤占成默认监控首页。

发送提供 `append` 与 `steer` 两种放置方式，首版默认 append。append 等待当前已封口的主体 generation 到达可接纳边界；steer 请求尽早介入。界面按真实回执区分“已接收”“已选入某轮”“主体已作出处置”，不能把传输成功显示成主体已经读懂或任务已经完成。后端只能下一边界处理 steer 时，状态如实表达这个实际位置。

放置方式与 D3 的紧急程度分开。append 不是后台低优先级，不因主体永远思考而排不到；它不因自身到达打断当前主体 generation。对正在占槽的 child 是否抢占、何时处理积压及是否进入 DND，继续使用同一调度与主体政策。steer 也不因此获得管理员身份或新增工具权限。

<!-- /migrated-contract: SPEC:21.12 -->

<a id="spec-41-18"></a>
<!-- migrated-contract: SPEC:41.18 -->
## 41.18 WebInputSubmission 规范接口

```ts
type WebInputSubmission = {
  submission_id: string;               // 客户端持久保存，重试保持不变
  subject_id: string;
  mode: "append" | "steer";
  text: string;
  attachment_refs: ObjectRef[];
  rich_content_ref?: ObjectRef;
  observed_step_id?: string;            // 观察信息，不是控制权
  observed_request_id?: string;
};

type WebInputView = {
  submission_id: string;
  event_id: string;
  accepted_commit: CommitRef;
  presentations: PresentationReceipt[];
  dispositions: Array<{ step_id: string; commit_ref: CommitRef; disposition_ref: ObjectRef }>;
  placement: {
    requested: "append" | "steer";
    actual: "pending" | "next_boundary" | "live_steer" | "cancel_reseal";
    target_request_id?: string;
    operation_id?: string;
    evidence_refs: ObjectRef[];
  };
  withdrawal?: { cancel_id: string; commit_ref: CommitRef;
    result: "withdrawn_before_presentation" | "withdrawal_queued" };
};

submit_web_input(input: WebInputSubmission): Promise<WebInputView>;
get_web_input(subject_id: string, submission_id: string): Promise<
  { kind: "found"; value: WebInputView } |
  { kind: "not_admitted" } | { kind: "temporarily_unavailable" }
>;
withdraw_web_input(subject_id: string, submission_id: string, cancel_id: string): Promise<WebInputView>;
```

认证主体来自当前会话，不能由正文中的 actor/admin 字段自报。服务以 `(authenticated_actor, subject_id, submission_id)` 查重：相同内容与 mode 返回原事件和回执；同 key 不同 payload 返回 `SUBMISSION_CONFLICT`，不修改原消息。查重命中先于旧观察版本的 stale 检查。修改已发送内容或把 append 改为 steer，提交关联原事件的新意图，不重用旧 key 改写事实。

### 接纳、呈现和处置

| 回执/阶段 | 唯一依据 | 不代表什么 |
| --- | --- | --- |
| accepted | 原始内容、附件引用和 InboxEvent 已提交；source 为 Web provider，external_event_id 为 submission_id | 不代表模型已看到，不代表任何外部任务已启动 |
| selected | S0 的 InputPlan/PresentationReceipt，或有独立记录的 live-steering 呈现 | 不代表已处理；所在请求取消后原消息仍可能 unresolved |
| committed | S1 对该 event revision 的持久处置；可为解释、委派、解决或明确无需回复 | 不代表 child 完成、外部效果完成或用户已读 |

三者是不同事实回执，不是可省略中间证据的单轴进度条。界面可显示当前待处理状态，同时保留此前请求被取消的 presentation。低于本地持久等级的浏览器乐观气泡标为待发送；提交前不得冒充 accepted。

append 在接受后的首个适用 S0 成为 ready 候选。有已 seal 的主体请求时，它不触发该请求的 stop/steer；无已 seal 请求时可纳入即将建立的 S0。预算放不下时保存显式 deferred/未处理集合并按等待老化继续选择；选择策略不能以“主循环尚未结束”为永久等待条件。当前 generation 被其他原因中断，不要求 append 再等一个人为构造的完整旧轮次。

steer 在接纳后绑定当时实际可作用的 request/engine incarnation。adapter 有已核验 live steering 时，通过 operation 发送并保存真实生效边界；只有 cancel 时，持久 stop intent、确认旧请求停止，再构建同一 subject step 的替代输入计划。无请求内控制能力则转 next_boundary。目标请求已经结束时，不将旧 stop 操作自动改绑下一请求。一次请求的 stop intent 合并去重，不因十条输入重复发十次取消。

任何语义 reseal 都产生新 request_id，在同一 request_lineage_id 中递增 request_revision，并记录 supersedes_request_id；旧 RequestSeal、partial 输出和历史不改写。真实 live steering 使用以下不可变记录，不改写原 seal：

```ts
type SteeringApplication = {
  application_id: string;
  request_id: string;
  attempt_id: string;
  engine_incarnation: string;
  sequence: number;
  previous_application_digest: Digest | null;
  exact_input_ref: ObjectRef;
  event_revisions: Array<{ event_id: string; event_revision: string }>;
  effective_output_frontier: {
    native_frontier_ref: ObjectRef;
    persisted_output_frontier_ref: ObjectRef;
  };
  adapter_contract_ref: ObjectRef;
  evidence_refs: ObjectRef[];
  application_digest: Digest;
  recorded_commit: CommitRef;
};

// 并入现有模型结果 manifest，不另建第二份结果记录。
type ResultAppliedSteering = {
  request_id: string;
  attempt_id: string;
  engine_incarnation: string;
  through_sequence: number;             // 0 表示没有已应用的 steering
  application_chain_digest: Digest;      // 0 时绑定该 request 的初始 seal
  result_output_frontier_ref: ObjectRef;
  evidence_refs: ObjectRef[];
};
```

`sequence` 在同一 request/attempt/engine 的应用链中递增，前序 digest 连到上一条已记录应用；`application_digest` 覆盖精确输入、事件版本、身份和生效边界等内容，提交后不可改写。effective output frontier 表示适配器合同能证明新输入已生效的输出边界；本地收到控制 ACK 的时刻不能代替它。模型结果携带 `ResultAppliedSteering`，S1 校验身份、完整链、结果 frontier 与后端证据，只能消费原 InputPlan 集合以及该结果 applied frontier 覆盖的事件版本。较晚应用、其他 attempt 或无证据的输入保持未处理，不能靠“已经提交过 steer”算作本轮使用。

适配器若只能证明控制请求已收到，不能给出可验证生效边界及结果对应关系，则本协议不把它登记为可用 live steering，实际路径降为 cancel_reseal 或 next_boundary。

### 撤回、未知与恢复

withdraw 的取消目标是这条输入的后续处理，不是所有已产生效果。原始 InboxEvent 位于 I；PresentationReceipt、EventDisposition、withdraw 控制和 S0/S1 位于 L。撤回事务在 **L** 检查呈现/处置并登记 withdraw 控制，与 S0 在同一数据库中排序：若它先于任何 sealed presentation/S1 消费，保存撤回事实并使原事件不再参与普通选择，返回 `withdrawn_before_presentation`；若已被选入或已作出处置，则在 L 追加有因果关联的撤回控制供后继步骤处理，返回 `withdrawal_queued`。不能声称模型没见过，不能删除已提交决定、自动撤销 SSH/发信或改写过去。I 中原始输入仍保留；不依赖 I/L 的虚构跨库事务裁定这场竞争。

浏览器断连不取消输入或主体。发送前保存稳定 submission_id 与 payload；响应丢失后查询或用同 key 重试。`temporarily_unavailable`、超时和空白页面不是 `not_admitted`，不能据此生成新 key 重发。writer 重开后从 Inbox、PresentationReceipt 和 S1 恢复视图；steer operation 状态不明时按原 operation 对账，不重复取消或重新派生主体。时间线订阅从持久 cursor 续接；需要重新快照时返回显式 resnapshot，按 item identity 去重。

<!-- /migrated-contract: SPEC:41.18 -->

<a id="spec-57-5"></a>
<!-- migrated-contract: SPEC:57.5 -->
## 57.5 流式是时间线的默认真相

**合同。** 接上已有三件器官：broker 的 `submit_stream`、store 的 [相关领域合同](durable-state.md#content-stream-journal) 内容流日志、UI 的 `StreamSink`。chunk 先落盘再广播；控制行不外泄（broker 的 `ControlGate`）；committed-only 发布不变；流式是额外的有序表面，不替代 S1 的提交。没有生产者时 UI 继续诚实报告 `streaming:false`，绝不假打字机。

**为什么现在做。** [§21.9](web-interface.md#spec-21-9)、[§21.11](web-interface.md#spec-21-11) 全部前提已写好，缺的只是接线；主体在本地 27B 上推理数秒，期间用户面对的应当是正在发生的输出，而不是沉默。

<!-- /migrated-contract: SPEC:57.5 -->

<a id="spec-57-7"></a>
<!-- migrated-contract: SPEC:57.7 -->
## 57.7 界面的诚实词汇与生命感

**保留（不得退化）。** 暗色为主的 oklch 语义色板、单一青色强调、细线分隔、锚点阅读状态（`reading-state.ts`）、`unknown`/`conflict`/`temporarily_unavailable` 这套诚实词汇、空态副标题、单点失败降级为原文 + 重新呈现。它们是本产品最强的美学资产，比任何新控件都重要。

**修。** 浅色偏好不闪暗（`index.html` 现在硬编 `class="dark"`）；样式只有单一来源（`src/static/app.css` 与 `web/src/app.css` 的偏差必须消失）；屏幕阅读器对新提交内容给出有界宣布；命名与认领有入口；主体在场信号按诚实事实呈现（不知道就说不知道，不假装在思考）。

<!-- /migrated-contract: SPEC:57.7 -->

## Context policy input budget

`/api/context` retains the bound provider's advertised limits as raw facts.
Strategy availability uses the subject's shared `input_model_caps` projection
and the exact output reservation supplied by the same bootstrap. Both text
and image context budgets subtract that reservation; image patch geometry
does not change. Unknown advertised context remains unknown. An exhausted
input budget or an output reservation outside the token type is reported as
not evaluated with its reason, rather than advertising usable strategies.

## Playground root response adapter

`/api/playground` returns roots as `[artifact_id, root_path, root_identity]`
tuples. The live controller admits complete string triples into the semantic
root view; the renderer displays that path and identity rather than reading
object fields from the wire tuple. The HTML work consumer case uses the real
wire shape alongside the content revision header and immutable version list.

Content writes update the authored head without implicitly adopting its
manifest or erasing staged candidates. A newly authored work has no active
manifest until explicit publication; later PUT, rollback and backend binding
retain the existing adoption and candidate facts. Stage can therefore propose
the current head and the UI can explicitly adopt that candidate.
Content, stage and publish transitions share the Playground record write lock;
a reader's revision is checked inside that transition. Publication resolves
the candidate CAS object before looking up its typed manifest version digest,
and the UI's adoption action selects the newest retained candidate.

## Conversation publication facts

Channel conversations combine classified inbox envelopes with committed publication intents. Each direction selects its newest bounded history. A publication's displayed time comes from the task created in the same S1 transaction, which also covers publications created by a delegated child. Imported task metadata keeps its recorded time; an older publication without that metadata retains an unknown time. Messages use normalized UTC timestamps and stable message identities for ordering. Existing inbox rows do not carry a cross-table commit sequence, so the projection does not infer a causal order from unrelated row numbers.

Publication labels follow the actual `ready`, `claimed`, `acknowledged`, `settled`, `blocked` and `unknown` ledger states. `settled` is displayed as an ended operation: cancellation can settle a publication that was never dispatched, so this state alone does not prove that a message was sent or read.

## Owner input history and inspection reads

`GET /api/submissions?limit=100&before_seq=<accepted_commit_seq>&before_id=<submission_id>` reads the current installation's owner messages from `web_input_submissions`, including their canonical payload, persisted event time and durable receipt view. Action submissions retain their separate presentation lane. Pages are newest first, with `total`, `has_older` and `next_before: {seq, submission_id}`; the tuple cursor retains every input when an archive import records several inputs in one receiving-ledger commit. The limit uses the same 1–200 bounds as the context metadata pages. Both conversation and Inspect offer earlier pages, and retain the reader's message anchor while those pages are inserted. A bounded page never proves that an omitted pending submission was not admitted.

The shared receipt reader recovers `selected_step_id` from the sealed input plan even after S1 committed a disposition. The browser hydrates existing submission IDs and original payload/time, merges durable facts without sending an input, and keeps locally pending or conflicting attempts. It does not save recovered historical messages as new local submissions. Owner messages precede their actual selected step; replay arrival time does not substitute for historical order. Until all owner history is available, unpaired steps are shown without claiming that they were independent thought. `selected` means selected into sealed input; `committed` means durably handled, and neither receipt alone proves that the model read the message or produced a reply.

Timeline and stream DTOs carry the receiving-ledger `commit_seq` separately from the immutable source `commit_ref`. The browser orders durable inputs and replies in that local sequence space, including imported source commits, while unadmitted browser attempts retain an unknown position after durable history. Historical receipt polling updates the displayed facts without writing a browser outbox record; only this browser's own send or explicit retry creates that record. The IM viewport captures the visible message before DOM mutation and holds that anchor across the intermediate history-loading control, without combining manual restoration with native scroll anchoring.

The reading anchor prefers a visible concrete message over a temporary unpaired-round summary, because history hydration can replace that summary while retaining the message. Transport `observed_at` remains available as an observation fact and is not printed as a reply's historical creation time; an unknown reply time stays unknown. The conversation list retains the latest known message time instead of inventing a timestamp for the replayed reply.

Compiled message hosts can mount after the parent React commit. The IM viewport observes its message content and reapplies the reading anchor when that content changes size, including native host publication. Scrolling selects a new reading anchor, and following the tail continues through those later mounts. Unmounting the thread disconnects its observer.

The completeness of cached owner history is also bound to the committed parts already rendered. A new committed identity keeps an unpaired round neutral until a newest-page request started after that identity has been applied. A newer refresh requested during an existing load is retained once; failure keeps the error surface without retrying itself. Duplicate committed replay does not invalidate the snapshot. Responsive icon-only navigation clips its descriptive labels visually while retaining them in the accessibility tree.

Task lists, trajectories and channel conversations require an explicit subject in their Store readers. The API resolves the installation's existing local subject for each read, and fails on missing or ambiguous identity rather than falling back to all rows. Foreign task IDs are absent from that subject's trajectory surface, including child lookups. Inspection times are normalized RFC3339 UTC; a failed conversation read preserves previously known messages and reports the unavailable state.

Committed replay also requires an explicit subject through the Store and outbox readers. A socket without a resolved local subject does not replay parked replicas or advertise committed replay; other subjects' imported steps remain outside the local subject's timeline.

Rendered subject parts and their copy actions use the original `(message_id, part_id)` pair, so different rounds' `text/0` parts retain independent identities. Work lenses observe the durable active manifest digest as well as the authored revision; adoption can therefore refresh a page without another content write. The adopted revision label comes from the actual adopted response header, and an unchanged page retains its frame.

## Pending-mount compiler pin

The current build pin advances from the section 6 build record's `35195a6e6f1ba6a7d800da0029f2683013abd18b` to the published commit `41282cb0ddc39283826e52e9932be123fc83eb47` ([Proto-UI PR #1459](https://github.com/Proto-UI/Proto-UI/pull/1459)). The upstream lock digest and package versions remain unchanged; the three public entry points have a freshly built 42-package dependency closure at this commit.

The compiler preserves an explicit `run.update()` issued after the first mount frame is rendered and before that frame is acknowledged. Mount acknowledgement credits the pending dirty intent so the next frame can present the current value. Shenko retains its authored Props watcher and regenerates both Prototypes through the pinned compiler. This update does not settle the wider Adapter defaults, update-ownership and diagnostic questions in [Proto-UI issue #1453](https://github.com/Proto-UI/Proto-UI/issues/1453).

## Task roles and canonical display implementation

`TaskSummary.role` is a typed read-model field shared by `/api/tasks` and
`/api/tasks/{id}/trajectory`: `subject_root`, `delegation`, `tool` or `unknown`.
The root requires the subject head's recorded root identity and self-ownership.
A delegation requires that task's subject-owned `subject_intents` delegation
fact. A tool requires the existing `child_tool_parent` admission fact to agree
with `TaskOwner`. Definition names do not establish any of these roles; a new
plugin delegated through the same task contract is projected immediately.
Missing or unrecognized historical provenance stays `unknown` and the row
remains visible. The task record, frozen inputs and archive schemas gain no
presentation-only field: the existing head, task and intent facts already
travel in the recovery archive and supply the same roles after import/reopen.

The recognized model-child step record is schema 1. Its existing `text` is
the recorded child parser's public display text. New records retain a UTF-8
prefix of at most 64 KiB; `text_truncated` reports whether that display text
was shortened. The receipt sink classifies and decodes the complete original
response before applying this bound: a reference definition after 64 KiB
can make an earlier apparent control literal, and genuine controls beyond
the display bound still reach the owning decoder. `tool_calls`, `reply` and
`question` hold separate parsed controls, while `output_ref` and E1 preserve
the complete original evidence. The thread renders retained text verbatim,
including its whitespace, JSON strings and control-like literal examples.
Missing or unknown
record schemas report unavailable public text and retain the original record
for inspection; they are never reinterpreted with a newer parser. The root
timeline similarly consumes the already-parsed `ParsedOutput.text` from S1.

`shenko_types::output::OutputSource` owns the shared CommonMark source
classifier used by subject/child decoders and the stream gate. It preserves
quoted/list continuations, fenced/inline code, block and inline HTML
(including multiline comments), and resolved reference links. It classifies
the structural control tag without constraining the opaque platform reply
identity; the owning channel resolver retains target validation.
Its byte-based `stable_prefix_end(false)` allows ordinary partial
prose to stream, but retains the tail from the first possible control while
future Markdown/reference input could still change its classification.
Only genuine EOF fixes that tail; interrupted or unknown responses do not
acquire a false EOF. Markdown ranges are computed lazily on first complete
classification, rather than on each incremental prefix query.

These are source changes and consumer test definitions, not new runtime or
model evidence. The integration owner rebuilds the frontend and exercises
the parser, archive/reopen and structured thread consumers on the combined
candidate; previously retained proofs are unchanged.

## Conversation observation projection — implementation note

`web/src/threads.ts` keeps channel provenance on each message. `own_account`
is an optional observation: `Some(true)` identifies account origin, without
identifying a client; `Some(false)` identifies a counterpart only when the
provider established it; missing historical observations remain `unknown`.
Unknown account origin is labeled on the message and is not assigned to the
subject or used to name a proven counterpart. `im.tsx` renders each message's
source annotation even inside a run of messages from the same displayed sender.

The participant list contains observed senders, not a membership roster. Missing
audience shape uses the group layout with an explicit unknown-type subtitle;
group membership counts remain unknown. An envelope's administrative-grant
observation stays on that message with its recorded time (or an explicit missing
time), including in group conversations. It never changes the participant into
the owner or claims current privileges, and display labels grant no authority.

Feishu states audience shape only for recognized `p2p` and `group` values;
missing or unfamiliar values remain absent. Its explicitly identified user
sender establishes counterpart origin, while unfamiliar or missing sender types
leave account ownership unknown. These are source changes; browser, provider,
model and combined-candidate behavior still require the parent's verification.
