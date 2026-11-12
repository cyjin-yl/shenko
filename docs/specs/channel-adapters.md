# 频道适配器功能合同（channel-adapters.md）

**内核定位。** 参考插件，登记在能力架上，建立在[内核合同](kernel.md)的[内核 5](kernel.md#kernel-5)、[内核 6](kernel.md#kernel-6)与[内核 7](kernel.md#kernel-7)之上。每个渠道适配器独立安装和卸载；目标标识对内核是不透明的字符串。

对应 [§15.3](channel-adapters.md#spec-15-3)、[§16](channel-adapters.md#spec-16)、[§35.6](channels.md#spec-35-6)；需求 R035、R057。本文只描述本切片
（ArchOtherChannels：官方 QQ / 微信 iLink / 飞书 Lark 三个新适配器及真实
运行时注册）的功能、接口、错误与恢复合同；不替代 SPEC，不把"已写源码"
冒充为"已运行验证"。运行证据由父控统一补齐后单独登记。

## 功能边界

本切片提供官方 QQ 机器人、微信 iLink 机器人、飞书/Lark 开放平台的三个
`ChannelPlugin` 适配器，以及把它们接入运行时的一条组合根。每个适配器拥有
该平台的一切：授权续期、入站分类、真实的 send/edit/retract/typing/
附件调用、能力声明、健康与回执。Shenko 核心只看见规范化
`ChannelEnvelope` 与不透明 `PublicationTarget`，平台自有字符串不外泄。

诚实的平台差异（是平台事实，不是偏好）：

- **官方 QQ**：OpenAPI v2 + WebSocket 网关（`v2/events`，OPCODE 13 intents）。
  有 edit、retract、频道内 typing。服务群（`group_openid`）、单聊
  （`user_openid`）、频道（`channel_id`）三类受众，全部使用按应用下发的
  OpenID——数值 QQ 号对官方机器人不可得，适配器绝不假装可得。
- **微信 iLink**：`getupdates` 长轮询入站 + 每会话 `context_token`；媒体走
  AES-128-ECB 加密 CDN。**无 edit、无 retract、无 typing；只服务单聊**，
  协议没有群发，故不声明群能力。
- **飞书/Lark**：事件分发 WebSocket（`/callback/ws`）入站 + open API 出站，
  app ticket（`app_id`+`app_secret`）换 `tenant_access_token`。消息是
  `post` 类型块文档。有 edit（PATCH）、retract（DELETE）、typing（独立端点）。
  受众是会话（`chat_id`）与会话内话题（话题根消息 `thread_id`）。

napcat（QQ 私有协议，OneBot）由 ArchChannels 负责，不在本切片。

## 授权与续期（[§16](channel-adapters.md#spec-16)）

- **官方 QQ**：`app_id`+`client_secret` 换 `access_token`，按平台
  `expires_in` 缓存并提前 300s 续期；令牌只进 `Authorization` 头，不进
  URL/日志/工件。WebSocket 网关用同一令牌 identify。
- **iLink**：无 app ticket。人扫二维码（`get_bot_qrcode?bot_type=3`），
  轮询 `get_qrcode_status`（wait/scaned/confirmed/need_verifycode/
  verify_code_blocked/expired/scaned_but_redirect），confirmed 后得到
  `bot_token`+`ilink_bot_id`+`ilink_user_id`(+可能换 `baseurl`)。
  令牌写本地文件（0600，仅属主可读），可过期、可换绑，绝不进日志/回执/URL。
  二维码端点只带应用头（`iLink-App-Id`、`iLink-App-ClientVersion`），不带
  授权——那时还没有令牌。
- **飞书**：`POST /auth/v3/tenant_access_token/internal` 用 `app_id`+
  `app_secret` 换 `tenant_access_token`（开放 API）与 `app_access_token`
  （事件分发 WS），各自缓存、提前 300s 续期；secret 只进令牌端点。

三个平台的凭证都**本地保存**；账号 scope 分别是 `qq-official:<app_id>`、
`ilink:<ilink_bot_id>`、`feishu:<app_id>`。管理员身份是 owner 显式设置
的本地事实（`admin_openids`/`admin_user_ids`/`admin_open_ids`），不从群
角色、app ticket 或亲密度推导（[§16](channel-adapters.md#spec-16)，R035）。

## 入站：分类、游标与回执语义（[§15.1](channels.md#spec-15-1)、[§15.3](channel-adapters.md#spec-15-3)）

- 每个适配器的 `classify(config, raw) -> Option<ChannelEnvelope>` 把平台
  帧分类为规范化信封；不归本适配器的事件返回 `None`，原始帧留在入站
  账本里。已规范化的信封重放时分类为自身（准入与回放投影一致）。
- **官方 QQ**：游标是网关 `s` 序号 + `session_id`，逐帧持久化，断线按
  opcode 7/9 决定 resume 还是重新 identify。
- **iLink**：游标是 provider 的 `get_updates_buf`，每次轮询后原样持久化；
  平台建议的 `longpolling_timeout_ms` 决定下一次长轮询持有；`ret==-14`
  账号停一小时。
- **飞书**：WS 长连接无客户端游标，断线指数退避重连，平台补发缺口；
  应用层按事件去重归账本负责。

入站附件**字节不随行**：定位器（`AttachmentLocator`）命名平台宿主
（qq-platform 临时 URL / ilink-cdn 加密参数 / feishu-drive file_key），
由受控附件句柄取回（[§15.1](channels.md#spec-15-1)）。`ChannelAttachment` 使用共享落地形状
（`size`/`locator:Option`/`object`，无 bytes/hash/expires_at）。

## 出站：真实 send/edit/retract/typing/附件

`describe(target) -> ChannelDispatch` 把不透明目标翻译成平台自有参数，
被动应答必须携带平台给出的上下文（QQ 的 `msg_id`、iLink 的
`context_token`、飞书话题的根消息）——上下文来自 monitor 每会话记忆，
绝不自造。`dispatch(dispatch, content) -> Receipt` 做真实调用：

- **发送**：QQ `v2/users|groups|channels/…/messages`（`err_code==0` 受理）；
  iLink `sendmessage`（`ret==0`）；飞书 `im/v1/messages`（`code==0`）。
- **编辑**：QQ `PATCH messages/{id}`；飞书 `PATCH messages/{id}`；**iLink 无**。
- **撤回**：QQ `DELETE messages/{id}`；飞书 `DELETE messages/{id}`；**iLink 无**。
- **typing**：QQ 频道端点；飞书 `typing` 端点；**iLink 无**。
- **附件**：QQ 先传媒体得 `file_info` 再随消息发送；iLink 先 AES-128-ECB
  加密上传 CDN（MD5+`getUploadUrl`+`x-encrypted-param`）再把加密媒体引用
  放进 `image/video/file/voice_item`；飞书先 `drive/v1/medias/upload_all`
  得 `file_key` 再放进 `post` 块（`img`/`media`/`file`）。

## 能力、健康与回执

- `capabilities()` 按平台事实声明（iLink 的 edit/retract/typing/群全是
  false），差异写进 `notes`，不夸大。
- `HealthState` 在 connecting/ready/degraded/stopped 间迁移，记录
  `last_event_at`；健康经 `record_channel_fact` 上报。
- 回执 `ChannelReceipt` 的受理判据是平台自有错误码；**应答丢失
  （`Unknown`/传输超时）与明确拒绝（`Rejected`）严格区分**——拒绝=远端
  未发生，丢失=可能已执行，按 no-blind-replay 对账而非重发（R057）。
  契约摘要（`contract_digest`+`acceptance_predicate`）把这条边界钉死。

## 注册与运行时接线

`crates/shenko-channel-adapters/src/runtime.rs` 是组合根：
`register_adapters(registry, AdapterDeps) -> Adapters`，
`AdapterDeps{data_dir, qq/ilink/feishu: Option<Arc<Config>>}`（纯配置，无
Store）。`Adapters::build` 每平台只建一份 `HealthState` 与
`ReplyContextCache`，出站插件与入站 monitor 共享，故应答上下文与 monitor
记录严格一致；`register` 把三插件注册进 `ChannelRegistry`；
`run_ingress(sender, cancel)` 产出 `InboundEvent{envelope, raw, source}`。
本 crate 不含 shenko-store：intent/attempt/receipt 归 Store 账本
（ArchChannels 的 `arm_publication`/`record_publication_receipt`/
`record_publication_unknown`/`refuse_publication` 契约）。

channel id：`qq-official` / `ilink` / `feishu`；source 标签：`qqofficial` /
`ilink` / `feishu`；reply tag（全局唯一，前缀化）：`qqo-group`/`qqo-private`/
`qqo-channel`、`ilink-direct`、`feishu-chat`/`feishu-thread`。

## 错误与恢复

- 明确拒绝（平台应答且拒绝）→ `ChannelError::Rejected`，可安全重试或拒投。
- 应答丢失 → `ChannelError::Unknown`，按可能已执行对账，绝不盲重放。
- 平台应答但缺承诺字段（如 `file_key`/`message_id`）→ `Incomplete`（归类
  为传输侧，保守按未知处理）。
- 传输错误经 `redact_transport_error` 去凭证（reqwest 会把带 token 的 URL
  嵌进错误串，只留 scheme+host）。

## 验证状态（诚实登记）

- **源码完整**：三适配器全协议路径、组合根、注册与入站接线均已实现，
  非桩非类型壳。
- **参考推导**：协议字段来自主版本文档（QQ 开放平台、微信 ilink bot
  文档/openclaw-weixin、飞书开放平台/lark-oapi）。iLink 媒体 AES-128-ECB
  向量以 FIPS-197/SP800-38A 已知答案在编译期自校验。
- **未执行（父控统一验证）**：本切片源码阶段未跑 build/clippy/测试/
  原生/真实模型。共享类型（`shenko_types::channel` 信封/dispatch/trait、
  `output::PublicationTarget` v2）由 ArchChannelsNext 落树后本 crate 方可
  编译；`PublicationTarget` 的 `id:String` 加宽是 sealed-wire 变更，归其
  owner。真实自有账号的 wire 路径（扫码授权、发/改/撤/typing、加密媒体
  上传下载）属父控后续按 eligible 账号验证的外部前提。

<a id="spec-15-3"></a>
<!-- migrated-contract: SPEC:15.3 -->
## 15.3 消息插件合同

MessageProvider/ChannelAdapter 支持 capability discovery、认证/续期、receive/ack、cursor、附件获取、send/edit/retract/typing、投递回执和健康状态。渠道插件故障不使主 loop/记忆服务一起退出。插件暴露真实平台能力，例如 direct/group、富文本、流式回复、主动投递和回复窗口。

入站按 account+provider message identity 去重；先持久写入再推进可重放游标。出站经 durable outbox，区分已生成、待发送、已送达、明确失败与结果未知。平台没有可靠幂等/回执时，不能承诺 exactly-once；不确定操作由 Agent/worker 观察核对再决定重试。

<!-- /migrated-contract: SPEC:15.3 -->

<a id="spec-16"></a>
<!-- migrated-contract: SPEC:16 -->
# 16 中国平台与开放对话

| 路线 | 正式接入范围与要求 | 已知边界 |
| --- | --- | --- |
| QQ 官方机器人 | 单聊、群聊、频道；认证、事件、媒体、回复/主动投递 | 可接全量群消息取决于平台设置；回复窗口与限频可变，不能写死成无限主动触达 |
| NapCat / OneBot | 原生客户端 adapter，连接已有本地/远端协议端 | NapCat/NTQQ 本体不要求塞进 WASM；账户命名空间与官方 OpenID 分开 |
| napcat-cli | 兼容用户已有 daemon/事件/HTTP wake 部署 | 保留其现有使用路径，不让 Python/FUSE 成为全产品必选 |
| 微信 iLinkBot 路线 | 基于明确版本的 iLink provider，处理二维码授权、游标、context token 与附件 | 当前 Tencent 官方插件声明 direct；微信群能力不能假称已具备；具体 iLinkBot wrapper 可后续指定 |
| 飞书 / Lark | 私聊、群、线程、卡片、文件；长连接或回调 | App/tenant/权限与国际 Lark 区别明确；回调快速确认，推理异步执行 |
| 其他平台 | 同一 provider 插件合同与市场 | 平台不可达或拒绝发送时，成果仍在网页 Inbox |

依据：[QQ 消息总览](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/overview.html)、[Tencent iLink 协议](https://github.com/Tencent/openclaw-weixin/blob/main/docs/ilink-bot-protocol.md)、[iLink 渠道能力](https://github.com/Tencent/openclaw-weixin/blob/main/src/channel.ts)、[napcat-cli](https://github.com/cyjin-yl/napcat-cli)、[飞书消息事件](https://open.feishu.cn/document/server-docs/im-v1/message/events/receive?lang=zh-CN)。以实现时平台最新文档和契约测试为准；本文不把本次限频数值当长期核心常量。

默认所有能到达的用户可对话，不设默认 DM pairing/群 allowlist。群主身份不自动获得本 Agent 管理权；管理员由 owner 明确设置。服务保护使用可配置的公平份额/节流与可解释排队，不暗改成只允许少数人聊天。

“开放对话”不等于每条群消息都必须回答。群参与模式可设被叫到才答、自然参与、积极参与；消息仍按所配置的收取/留存能力进入主体，主体自行决定有无值得说的话。bot 互相唤醒、碎片消息合并、群休息时间和连续追问由行为插件处理。

任务完成与通知送达是两个状态。超过平台被动回复窗口、联系人关闭主动消息、账号掉线时，Agent 可以选择其他已授权渠道联系相关人；网页 Inbox 是可取结果的位置，不能被当成唯一固定的联络策略。

<!-- /migrated-contract: SPEC:16 -->
