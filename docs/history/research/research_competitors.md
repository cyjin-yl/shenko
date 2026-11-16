# 竞品与通信接入研究（中间研究笔记）

核对日期：2026-10-23，Asia/Shanghai。只进行了检索和只读源码阅读，没有开发、修改远端仓库、登录平台或对外发送消息。

## 一、结论与产品定位

三个被点名的商业竞品均已找到官方来源：OpenAI dots 于 2026-09-29 发布，Claude Duet 于 2026-10-21 开启研究预览，Meta Muse 于 2026-09-08 发布。它们不是需要忽略的同名传言。真正差异应写成：**用户控制的持久执行环境、可组合的恢复机制、面向本地推理的资源调度、端侧部署以及开放的工具与消息渠道**。不能把“主动工作”“持久环境”“自建工具”本身当作无人做过的创新。

OpenClaw 已经能本地部署、换模型和 harness、跨多个渠道工作、连接移动/桌面节点、开放私聊；其默认陌生人 pairing 是可配置的，不是不可消除的产品限制。因此本项目应明确比较默认体验、可审计性、常驻资源和本地模型效率，不宜以“它不允许所有人聊天”作为技术壁垒。

## 二、竞品事实矩阵

| 产品 | 可证实产品行为 | 执行/隐私形态 | 可借鉴 | 本项目需要明确的差异 |
|---|---|---|---|---|
| OpenAI dots | 独立云电脑与浏览器，可连接用户电脑；多个项目持续执行；ChatGPT/Slack/Teams 跨渠道连续上下文；主动研究采用只读已连接应用工具 | 托管云执行；应用权限、Custom Rules、auto-review；不能据此宣称本地自主部署 | 单一人格、跨项目任务面板、持续交付和主动更新、原始活动可见 | 自托管、本地模型、可控长期任务预算和用户定义自治范围 |
| Claude Duet | 云电脑；主对话与专题频道共用记忆；Tasks 与 Make 分开；检索历史聊天；晨间简报和间隔检查 | 研究预览，US Pro/Max waitlist；目前 Opus 5.5 固定，不让选模型；不能观看或接管其浏览器 | 主对话保持安静，任务进展/成果分开展示；跨会话引用与可删除来源 | 自选模型、任务可观察执行、SSH 节点、可以由机器判定任务完成而非全部等人标记 |
| Meta Muse | 持续推进长期目标、子 agent 群、构建工具和自改；自己的云 VM；WhatsApp/app | durable state、凭据存储与 runtime cell 分开；Sentinel 控制网络/连接器；Confidential VM 是发布文中的未来计划 | 运行环境损坏不能带走身份/任务/凭据；凭据代理、独立事件审计 | 本地硬件与本地模型；不依赖 Meta 托管；更灵活的预授权自治策略 |
| AstrBot | 多平台、插件/MCP/Skills、上下文压缩；实验性 subagent 和 future_task；本机与第三方沙箱 | Python 3.12+；可用 Ollama/LM Studio/OpenAI-compatible；本机 bwrap/Seatbelt 或 Shipyard Neo/CUA | UMO 统一消息来源；按会话工作区；消息格式归一化和平台能力表 | 与 v4.14 future_task 明确区别：该功能文档称其不是一直运行的自主监控；本项目应原生长期目标与自主维护 |
| OpenClaw | 常驻 Gateway 控制 sessions/tools/events/channels；模型及 harness 插件；本地与远端节点；durable mailbox 和消息队列 | Node 运行时；memory/state/credentials 在用户硬件；配置所用模型/聊天服务决定外发路径；默认 pairing 可改 open | Gateway/节点协议、幂等请求、session mailbox、优先级 aging、节点 capability discovery | Rust 最小可信宿主+WASM、推理资源组的全局 admission、单次请求与 slot 租约分离 |

说明：未做五个产品同机 benchmark，不能声称谁的 RSS/速度“最好”。运行语言和依赖只能用于估计集成风险，不能代替性能测量。

## 三、值得直接研究的实现

### OpenClaw

- Gateway 的 typed WebSocket request/response/event 协议，静态 Web UI 与 CLI 共享接口。
- 节点通过显式身份、capabilities 与 commands 宣告能力；同一 Gateway 管理桌面/移动/headless 节点。它证明多节点无需复制完整主 agent。
- side-effecting `send`、`agent` 请求使用幂等键。对于本项目，需要把短期 dedupe cache 升级为 durable outbox/operation ledger，才能跨崩溃恢复。
- 当前队列文档已有 session mailbox、转发时保留提交者 authority、consistent transcript prefix，值得借鉴。
- 当前 main lane 默认并发按 CPU 算：`max(8, available CPU parallelism * 4)`，subagent 又按 spawning session 分预算，不能照搬成共享 GPU 的全局并发限制。
- 前台请求有有限插队时间，避免后台永久饥饿；正在运行的 turn 不抢占。本项目是否取消进行中的推理应独立由 backend 能力与 cache 损耗决定。

### AstrBot

- 目前沙箱支持 local（bwrap/Seatbelt）和 Shipyard Neo/Shipyard/CUA。
- Shipyard Neo 区分稳定 Sandbox、可重建 Session 和持久 Cargo；会话失效时重建运行环境、重新挂载工作数据。
- 普通成员/管理员权限分开。这些是成熟边界，不等于一定要给普通成员设置聊天白名单。
- Subagent 文档明确：子代理不会默认收到主代理全部历史，必须通过委派内容提供任务背景；后台完成可以唤醒主 agent。
- `future_task` 的一次性失败删除语义不宜照搬长期自治系统。这里的产品建议是记录 attempt、retry、terminal outcome 和投递状态，避免“跑过就丢任务”。

### Muse

- 官方技术设计隔开 runtime cell、`authd`、durable application state 和外部操作权限。
- 对本产品最有价值的是**状态和故障域分离**，而非把 Sentinel 的全部云产品审批流程搬过来。
- 支持一次、会话、任务、时限、持续授权，给出“充分自治且减少询问”的参考。最终策略应由本产品 owner 定义。
- 不要混淆“私有云 VM”与“提供商无法读取”；官方技术文明确当前运维政策限制访问，而非技术上阻止 Meta 访问。Confidential VM 当时属于未来计划。

## 四、原生聊天接入能力与规格

### 微信 iLink

一手来源是 Tencent/openclaw-weixin，MIT 许可、公开协议文，比第三方自述“官方 API”的仓库更可靠。

- HTTPS JSON、二维码授权、本地 token、long polling、文本/图片/语音/文件/视频、typing。
- 当前官方 `src/channel.ts` capability 明确是 `chatTypes: ["direct"]`。**不可在 SPEC 中承诺 iLink 微信群聊已经可用。** 平台能力应由 adapter 显式报告。
- 游标 `get_updates_buf` 应原样持久化和回传；收消息先入 durable inbox 再推进游标/确认，重启不能靠模型回忆 offset。
- `context_token` 回复时透传；官方协议文明确“客户端无 token 仍发送”不代表服务端会接受。不能把这种行为当可任意主动触达保证。
- 官方协议文明确客户端类型/样例不等于完整服务端契约，建议将 adapter 契约测试和兼容版本写进维护要求。
- 账号与 peer 共同参与 session key；多账号不能共享同一个对话 ID 命名空间。

### QQ 官方机器人 API

当前官方文档支持 QQ 单聊、群聊、频道。现有 `GROUP_MESSAGE_CREATE` 全量模式在开启“接收所有消息”后提供不限于 @ 的群消息，所以不能沿用“官方 QQ bot 只能收到 @”的旧判断。

- 群消息接口不支持流式参数；需要增量缓冲、合并分段或最终结果投递。
- 被动回复期限：单聊 60 分钟/每条最多 4 次；群聊 5 分钟/最多 5 次（文档核对日版本）。
- 群主动消息目前 bot 维度企业/个人认证 60/min、未认证 30/min；单群 20/min、1000/day。SPEC 应要求 adapter 从配置/元数据管理限制，不将当日值永久写死成核心逻辑。
- `msg_id + msg_seq` 是重复发送控制键，adapter 要持久记录。
- 用户可以关闭主动消息。深度研究耗时超过回复窗口时，任务仍应完成，结果进入网页 Inbox，平台可投递时再投递；不能声称“永远能通知”。
- QQ member_role 与本 Agent 的管理员权限不同；平台群主管理员不自动变成主机 SSH/设置/所有者记忆管理员。

### NapCat / 用户 napcat-cli

NapCat 是基于 NTQQ 的协议端，提供 OneBot 类 API；本项目作为客户端对接它即可，不需要把 NTQQ/NapCat 本体编入 WASM。它可以运行在另一台节点或现有容器。

用户仓库已确认：`cyjin-yl/napcat-cli`。README 描述 CLI + WebSocket daemon + SQLite events + skills-fs HTTP provider + agent wake。现有 wake 有 debounce、cooldown、同 session 串行 worker，HTTP 是推荐通道，CLI one-shot 被标为 legacy（进程启动、quoting、会话漂移、幂等问题）。

建议两条接入路径：标准 Rust/WASM adapter 直接连接 OneBot HTTP/WS；napcat-cli 兼容插件连接其 HTTP wake 和事件接口。后者用于平滑复用用户现有部署，不把 Python/FUSE 变成核心必选依赖。

用户 README 明确提醒 Docker 端口交叉映射；教程应探测实际 HTTP/WS，而不是写死 18800/18801。FUSE 可选，CLI + wake 在缺失 skills-fs 时仍可工作。

NapCat 仓库存在非标准混合许可/附加声明；API 互操作和复制/分发其代码是不同决定。规格应登记依赖许可来源，在决定捆绑源码前做具体核对，不把 README 上的“开源”当 SPDX 已确认。

### 飞书 / Lark

- 企业自建应用可以用官方 SDK 的出站长连接，适合路由器/家庭网络没有公网回调地址的部署。官方事件/回调文仍要求快速响应，不能把长 LLM 推理放在确认回调前。
- 私聊、群聊、卡片、文件及事件按应用权限与可用范围生效；读取群全部消息与只读 @ 消息需要不同权限。
- 官方文指出可能重复推送，消息幂等应使用 `message_id`，不能只依赖 `event_id`。
- 每应用最多 50 条长连接，推送为集群模式而非广播；多节点部署必须有 channel owner lease，避免以为每个副本都能收到全部事件。
- 飞书与国际 Lark 的 host、tenant、App ID、身份空间分别建模，不用昵称或邮箱自动合并身份。

## 五、最重要的产品规格追问

1. **同一个人格的隐私边界是什么？** 一个 owner 的所有私聊、每个外部访客私聊、每个群聊是否独立 memory/workspace？建议 personality/skills 可共享，原始会话与私密记忆按 principal/scope 隔离；经明确分享才跨域引用。
2. **默认开放对话到底开放哪些能力？** 建议陌生人可聊天、搜索公开资料、处理自己上传文件；管理配置、owner 私有 Git、SSH、安装高权限插件仅手动授予。否则“极致隐私”和“任何人可要求使用所有主机凭据”直接冲突。
3. **群里的主动参与规则？** 默认允许每个人发起，但自动回应所有消息/仅被 @/模型挑选参与频率是另一轴。默认 conversational attention + burst 合并，可配置每群安静时间、提醒聚合。
4. **长期自治的正当任务源？** 除用户显式任务，是否允许 agent 从已有目标自行创建维护、学习、整理、探索任务？建议 standing goals/intent registry，允许自主提议与执行在授权 scope 内的子目标，活动可撤回/暂停。
5. **无人值守允许哪种外部副作用？** 写 memory/装包/提交私库/执行 SSH、给群发消息、替人购买分别是否可以持续授权？不要只用单个“全自主”开关掩盖其区别。
6. **所有者在哪个渠道收决策和故障通知？** 网页 Inbox 必须有；额外渠道可选。有的平台不能主动触达时不能丢掉 pending decision。
7. **语音/视觉第一版是否必需？** 手机 IM 上图片/语音非常常见；本地 ASR/VLM 本身也会占本地推理预算。先保留原始媒体+派生转写的可追溯关系，不能只有模型看过的摘要。
8. **单实例还是共享组织服务？** “很多群可聊天”不等于多 owner SaaS。推荐第一版单 owner、多 audience scope、多 node；不把多租户账单/组织治理塞进最小内核。
9. **第一版渠道优先序？** 用户已有 NapCat+napcat-cli，建议它与网页为最早验收链路；QQ 官方、飞书、iLink 均列正式 adapter，但微信群聊必须注明不可证实/不支持。
10. **保留/忘记语义？** 用户撤回消息、删除记忆、退出群之后，原始日志、FTS、embedding、摘要、Git 与 snapshot 分别怎么处理？“检索不见”与“物理/备份删除”需明确区分。

## 六、建议写入验收条件（设计建议，非同行事实）

- 新人首次私聊，无需白名单即可得到答复；同时无法访问 owner 的 SSH key、私有知识库或修改管理员列表。
- 两个群里同名用户、跨两个 QQ bot 的相同数值 ID、iLink 多账号同 peer 等不产生 session collision。
- 群消息在短时间爆发时合并 context，但每个原始 message ID 可检索；每个会话始终只有一个 active turn 写 transcript。
- 收到事件后 agent 沙箱宕机，事件已落库；重启恢复且不会重复发送已成功的回复。
- 运行超过 QQ 被动回复窗口的研究，仍可从网页取结果，并明确显示平台投递状态。
- 接口或凭据失效时 channel degraded，与其他渠道/SQLite/恢复 UI 隔离；agent 自己不能假报平台已送达。
- 长连接多副本断线重连不会把同一消息触发两次研究；基于 message ID+adapter account 的 inbox dedupe。
- 从群提问检索出来的任何 owner 私有记忆均是隔离测试失败，不以模型“应该知道不要泄露”替代过滤。

## 七、主要一手来源与检索引用

父 Agent 在正式引用子 Agent 检索 refs 前，需要自行 open 或 search+find 相应页面。以下 URL 可直接使用。

1. OpenAI dots 发布：https://openai.com/index/introducing-dots/ — 2026-09-29；refs `turn4search0`, `turn9view0`。重点云电脑、跨渠道、只读 proactive research、Custom Rules。
2. OpenAI dots 文档：https://developers.openai.com/codex/dots?nav=features — `turn4search3`。
3. Claude Duet 帮助：https://support.claude.com/en/articles/17631495-get-started-with-claude-duet — `turn14view0`。主线程、Tasks/Make、历史检索、固定模型和浏览器限制。
4. Claude Duet 发布：https://claude.com/resources/articles/claude-duet — `turn20view5`, `turn33view3`。2026-10-21；来源经官方 release notes 的链接确认。
5. Claude release notes：https://support.claude.com/en/articles/12138966-release-notes — `turn14view1`；10-21 preview。
6. Meta Muse 发布：https://about.fb.com/news/2026/09/introducing-muse-personal-ai-agent/ — 2026-09-08；`turn9view1`。
7. Muse 技术设计：https://research.meta.ai/blog/security-and-safety-for-ai-agents-our-approach-with-muse — 2026-09-08；`turn14view2`, `turn22view6`, `turn27view4`。
8. AstrBot README：https://github.com/AstrBotDevs/AstrBot/blob/master/README.md — GitHub fetch blob `f6d5d01eca06778161d7d9c57d863ec6236de318`。
9. AstrBot 沙箱：https://docs.astrbot.app/use/astrbot-agent-sandbox.html — `turn20view4`, `turn22view3`。
10. AstrBot 主动能力：https://docs.astrbot.app/use/proactive-agent.html — `turn27view0`。
11. AstrBot subagent：https://docs.astrbot.app/use/subagent.html — `turn27view1`。
12. OpenClaw README：https://github.com/openclaw/openclaw/blob/main/README.md — fetch blob `98cefb7193ef59c963bdacd24a642d195adf8103`。
13. OpenClaw architecture：https://github.com/openclaw/openclaw/blob/main/docs/concepts/architecture.md — fetch blob `bb4d0fcae83035605f882cfd88c05fda5551d798`。
14. OpenClaw queue：https://docs.openclaw.ai/concepts/queue — `turn27view3`。
15. OpenClaw policies：https://docs.openclaw.ai/gateway/security — `turn11search3`。
16. Tencent iLink README：https://github.com/Tencent/openclaw-weixin/blob/main/README.md — fetch blob `a23105015a5594071b7d868ce0d074345088a6c3`。
17. Tencent iLink protocol：https://github.com/Tencent/openclaw-weixin/blob/main/docs/protocol_zh_CN.md — `turn16search12`，fetch blob `59e5001dcced54052355580223362bfe94922000`。
18. Tencent current direct-only capability：https://github.com/Tencent/openclaw-weixin/blob/874637a0dbc24050f72d1385add4fc6170a2d726/src/channel.ts — GitHub search `chatTypes` 得到 `chatTypes: ["direct"]`。网页抓取失败，使用 connector 读取。
19. NapCat：https://github.com/NapNeko/NapCatQQ — README blob `67c0bcf7519363ee00b49d364f3b5ac20ec78692`。
20. 用户 napcat-cli：https://github.com/cyjin-yl/napcat-cli — `turn33view1`，README blob `c2ecc58ae504dc8a8809187815934891617364ce`。
21. QQ 消息概述：https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/overview.html — `turn27view2`。
22. QQ 群发送：https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_messages.post.html — `turn29view0`，页面更新 2026-09-03。
23. QQ 全量群消息：https://bot.q.qq.com/wiki/develop/api-v2/autogen/event/group_message_create.html — `turn33view2`。
24. QQ 单聊：https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_users_user_openid_messages.post.html — `turn29view3`。
25. 飞书接收：https://open.feishu.cn/document/server-docs/im-v1/message/events/receive?lang=zh-CN — `turn36search1`；2026-08-05，message_id dedupe；web open 为空，官方搜索摘录可见。
26. 飞书长连接：https://open.feishu.cn/document/server-docs/event-subscription-guide/event-subscription-configure-/request-url-configuration-case — `turn36search0`；50 连接与 cluster 非 broadcast。
27. 飞书 callback：https://open.feishu.cn/document/event-subscription-guide/callback-subscription/step-1-choose-a-subscription-mode/configure-callback-request-address?lang=zh-CN — `turn36search2`；3 秒回调处理要求。
28. 飞书官方集成例权限：https://open.feishu.cn/solutions/detail/ticket?lang=zh-CN — `turn29search0`。

## 八、可复现版本记录的限度

GitHub 插件取到的 README/content SHA 是 blob SHA，**不是 commit SHA**。最近可检索提交也不必然等于主分支 HEAD，因此以下只能作检索时间锚，不应误标已完整审计版本：

- AstrBot：`0cd42bd4dd216d8810da4e3f5fb2e381d3c6817e`，2026-10-22T08:45:35Z，fix dashboard log SSE。
- OpenClaw：`062bbe5f94746bb92f29fe7c039238e7762ff2ab`，2026-10-22T17:57:46Z，Postgres durability。
- 用户 napcat-cli：`3da08c03ff491cb8b86896102b0eab06c9b0c145`，2026-07-27T20:55:34Z，fix auth qr subparser。
- Tencent iLink：代码搜索 snapshot `874637a0dbc24050f72d1385add4fc6170a2d726`；未在此子任务核对其 commit 时间。

其他未证实：没有 benchmark 证明路由器能承担这些竞品完整默认部署；没有发现 iLink 当前官方客户端群聊支持；不保证官方 QQ 权限在每个账户自动开通；不保证所有飞书 tenant 均可用全部 scopes；未做用户真实账号链路测试。

## 九、iLinkBot 命名消歧与渠道能力矩阵补充

用户没有提供 iLinkBot 精确 URL，名称本身无法唯一确定仓库。本轮 GitHub 名称搜索找到 `XTmai/WeChat-iLinkBot`（第三方收集文档与 Python 示例）、若干同名 fork、`isozone/ilinkbot-plugins` 等，**不能把其中任意一个认定为用户点名的唯一 SDK**。可确定的协议上游为腾讯 `Tencent/openclaw-weixin`，官方插件包名为 `@tencent-weixin/openclaw-weixin`。SPEC 可以先命名 `WeChat iLink adapter`，实现规范对齐腾讯客户端与协议文，若用户有指定 iLinkBot wrapper，再作为兼容层接入。

实际不一致例子：第三方 XTmai 文档写取 QR code 用 GET、状态 `scanned`，当前腾讯协议文写 POST、状态 `scaned`。这正说明不能直接照搬社区教程推导 2026-10 的服务契约。官方源码默认分支 `src/channel.ts` 第 229-230 行仍直接声明 `chatTypes: ["direct"]`，已通过 GitHub fetch 全文片段复核。因此“当前官方插件只宣告私聊”是源码事实；“微信服务器永远不会开放群聊”则不是结论。

| Adapter | 接收/发送 | 群/频道 | 引用与线程 | 文件/媒体 | 幂等与延迟限制 | 本项目权限默认（建议） |
|---|---|---|---|---|---|---|
| WeChat iLink | long poll 收、HTTP 发；多账号 | 当前官方 capability 仅 direct，群能力不承诺 | 引用消息+本地 quote cache；未证实原生线程 | 文本/图片/语音/视频/文件 | 保存不透明 cursor；透传 context_token；client_id 是客户端标识，不能据此承诺服务端严格 exactly-once | QR 配置账户后允许可达用户私聊；不因此授管理权 |
| QQ official | 事件收、HTTP 发；单聊有流式接口 | 群与频道可用；全量群事件要开启相关能力 | 普通群引用回复；不要映射成原生 thread | 富媒体先上传再带 file_info，有 TTL | msg_id+msg_seq；群被动窗口5分钟/5次，主动受频率、用户设置约束 | 可到达会话允许聊天；QQ 群主不是系统管理员 |
| NapCat / OneBot11 | HTTP+正/反向WS，连接方式可选 | QQ 私聊/群；不套用 QQ official 的平台 OpenID | reply segment 与消息链；无已证实普通群原生 thread | 由 NapCat 暴露的 upload/download/media API | echo 是请求响应关联，不是 exactly-once 承诺；丢 ack 后发送状态 uncertain，需 ledger/查询核对 | 所有接入会话默认可聊天；主机操作需手动身份授权 |
| napcat-cli bridge | 复用 NapCat 接口+SQLite event/wake HTTP | 同下层 NapCat | 继承下层；wake 携带上下文 | CLI file 子命令 | debounce/cooldown/串行 wake 不替代 durable idempotency；避免 CLI one-shot fallback 成功假象 | 复用主体映射但不从普通 HTTP 字段直接授管理员 |
| Feishu / Lark | 事件订阅/长连接收、API 发 | 私聊/群，权限与应用可见范围决定 | reply_in_thread 与 root/thread IDs 可用 | 图片/文件/卡片由相应 API | inbound 按 message_id；发送 uuid 可在1小时去重；长连接快速 ack；集群非广播 | 可访问用户/群默认聊天；额外平台 scopes 与本产品 admin 独立 |

新增一手证据：飞书 reply：https://open.feishu.cn/document/server-docs/im-v1/message/reply?lang=zh-CN (`turn43search3`)；thread：https://open.feishu.cn/document/im-v1/message/thread-introduction (`turn43search5`)；发送 uuid：https://open.feishu.cn/document/server-docs/im-v1/message/create (`turn43search4` 搜索返回带 appId 的同页 URL)；NapCat OneBot：https://napneko.github.io/onebot/network (`turn42search0`)。飞书发送 uuid 只保证文档所述时间窗口，不解决无限期崩溃恢复，宿主 outbox 仍然必要。
