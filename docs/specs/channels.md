# Channels — functional contract

**内核定位。** 参考插件。入站是带来源的原始事实（[内核 3](kernel.md#kernel-3)），出站是外部效果（[内核 5](kernel.md#kernel-5)），经[内核 7](kernel.md#kernel-7)登记；总体边界见[内核合同](kernel.md)。增加一个渠道不需要改动内核。

[§15](channels.md#spec-15)–[§16](channel-adapters.md#spec-16)、[§35.6](channels.md#spec-35-6)、[§41.6](channels.md#spec-41-6) · C13. This is the contract the channel slice
implements; it names the exact types and runtime paths so a second channel
can be added without touching the core.

## 1. Plugin boundary (`shenko_types::channel`)

Everything platform-specific lives behind `ChannelPlugin`:

| method | owns |
| --- | --- |
| `channel_id` / `sources` / `kinds` / `prompt_names` | the advertised reply grammar and inbox labels |
| `classify(source, raw) -> Option<ChannelEnvelope>` | inbound normalization; `None` = not a message this plugin owns |
| `staged_text(envelope) -> String` | the model's view: `[who · audience] body` |
| `resolve_reply(kind, id) -> Option<PublicationTarget>` | the opaque-id round trip from staged header to sealed target |
| `describe(target) -> Result<ChannelDispatch, ChannelError>` | the platform action, parameters and acceptance predicate |
| `dispatch(dispatch, content) -> BoxFuture<Result<ChannelReceipt, ChannelError>>` | the external call only; the durable record exists before this runs |

The core never parses platform ids, never re-derives acceptance from a status
word, and never owns a second operation ledger. `ChannelRegistry` rejects
duplicate channel ids, source labels and reply tags at registration.

## 2. Publication target (sealed record, `shenko_types::output`)

`PublicationTarget` v2: `{schema_version: 2, channel, kind, id: String,
account: String, tenant: Option<String>, conversation: Option<String>}`.

- `id` is an **opaque string** — QQ decimal ids, QQ-Official OpenIDs, Feishu
  chat ids all fit; the core never parses it.
- `account` is the owning connector scope (`qq:<bot_qq>` for the shipped QQ
  plugin). An **empty** account appears only on records produced by the
  migration of a pre-v2 target; a single-account plugin may serve it and
  records the account it actually used.
- Deserialization is strict (v2 only, `deny_unknown_fields`). Old
  `{"kind":"qq_group","id":N}` records are readable **only** through
  `PublicationTarget::read_legacy`, which is used by the Store migration —
  there is no unversioned alias.
- Sealed-record consequences: `ParsedOutput` schema 4→5, `StepDecision`
  schema 7→8. `Store::open` runs `migrate_publication_targets`, which re-seals
  legacy decision objects (rewriting their targets), updates
  `subject_steps.decision_ref` and `tasks.input_ref`, and records provenance
  in `channel_record_migrations`. It never touches `subject_steps.input_plan_ref`.

## 3. Outbound lifecycle (`shenko_store::channel`)

The bin runtime (`bin/shenko/src/main.rs`, publication consumer) drives every
ready publication through one pipeline, for every registered channel:

1. `Store::ready_publications(limit)` reads the ready index.
2. No plugin serves `target.channel`, or `describe()` errs
   (`UnsupportedTarget`/`UnresolvedReply`): `Store::refuse_publication` —
   durable `blocked` state, effect `none`, the runtime **keeps running**.
   The old "non-QQ target cancels the runtime" bail is gone.
3. `Store::arm_publication(intent_id, &dispatch)` — T1: claims the intent and
   arms the operation+attempt before the network is touched. `Ok(None)` means
   another dispatcher already claimed it: never a second send.
4. The runtime sets `dispatch.echo = operation_id` (durable correlation).
5. `plugin.dispatch(...)`:
   - `Ok(receipt)` → `record_publication_receipt` (E1). `accepted` is the
     plugin's own verdict (`onebot_retcode_zero` for QQ).
   - `Err(Transport | Unknown)` → `record_publication_unknown` — the request
     may have reached the platform; reconciliation settles it, never
     blind-resends.
   - `Err(UnsupportedTarget | UnresolvedReply | Rejected)` after arming →
     deterministic refusal recorded as a failed receipt (effect `none`,
     resolved).
6. Every outcome is mirrored as a subject-readable `channel_runtime_facts`
   row (`kind = publication_outcome`, states `accepted | rejected | unknown
   | refused`).

`Store::reconcile_subject_publications` (moved from `intents.rs`, unchanged
semantics) settles `claimed`/`unknown` publications after a restart: an armed
send whose owner died is local loss-of-control evidence, recorded `unknown`
(`provider_stop_proven: false`), never assumed failed.

## 4. Inbound lifecycle

- WS connector (`NapCatClient::run_connection_loop`) and the optional
  napcat-cli poller both ingest into the **same** `IngressSpool`; the source
  identity `["napcat", account, "onebot_v11", conversation, message_id]`
  dedups a message seen by both transports. napcat-cli is a second **source**
  of the same QQ plugin — one registry entry, one classification, one ledger.
- Admission (bin): `plugin.classify` runs on the raw payload; the envelope
  (not the raw wire frame) becomes the inbox payload, so the model projection
  is identical at admission and replay (an envelope classifies to itself).
  Payloads the plugin does not classify (notices, meta) keep the raw frame
  and stage with a plain `[source]` header.
- Two structural facts the plugin states at classification, and nobody
  re-derives later:
  - `reply_target.shape` (`one_to_one` | `group`) says how many counterparts
    the audience reaches. It is a property of this audience, not of the reply
    kind: a Feishu chat id is either a p2p chat or a group.
  - `actor.own_account` says the platform reported the line as sent by the
    account this channel speaks as. For NapCat that is a `message_sent`
    frame, for example sent from another client of the same account. The
    shipped bot APIs (QQ official, Feishu, iLink) drop the bot's own messages,
    so their lines are never own-account.

  The built-in IM depends on both facts ([web §5.2](web-interface.md#5-2-external)).
- The durable record of a delivered channel message is the staged input of
  the step that took it (`session_events`, `{"v":1,"inbox_event":…}`). It is
  not an `inbox_events` row: that table holds web, playground, wake and media
  inputs.
- Attachments: `parse_message` produces `ChannelAttachment` values whose
  `locator` is confined to the configured NapCat-host `file_root`
  (`NapCatConfig::file_root`, host name `file_host`). At admission the runtime
  resolves each locator **on the owning host** through the plugin's file
  capability and seals the bytes into the store (`ChannelAttachment.object`,
  content-addressed); the remote path never becomes a local file. A locator
  that cannot be resolved stays a locator and is recorded as a
  `attachment_locator` fact (state `pending`) — the message is never dropped.
- No S1 receipt means no ACK, even if delivered: the ack mirror only acks
  spool rows whose disposition commit exists in the control ledger.

## 5. Administration is owner-set

`NapCatConfig` keeps four facts apart: `ws_url` (where), `access_token`
(transport credential — Authorization header only, never in URLs, logs,
receipts or bindings), `bot_qq` (the served account), `admin_qq` (owner-set
administrators, recorded as `admin_grant` channel facts at startup).
Administration is never derived from a group role, the transport token or
familiarity; the envelope's `role_grant_ref` (`qq-admin:<qq>`) is set only
from `admin_qq`. Reachability stays open: anyone who can reach the account
may talk (R036).

## 6. Runtime errors are facts ([§45.14](cognition-units.md#spec-45-14))

At each unable-to-continue boundary — ingress connection loop, admission,
ack mirror, publication consumer, napcat-cli source — the runtime journals
`Store::journal_runtime_error` with the actual `agent_id`, a stable `kind`,
the module path and bounded extra, before cancelling. The journal entry is
not a delivery receipt: it never retries the operation and never opens an
issue by itself. Independent operation unknown/conflict receipts stay on the
operation ledger.

## 7. napcat-cli wire contract (verification prerequisite)

`NapCatCliSource` polls `GET {base}/events?after=<seq>&limit=256` expecting
`{"events": [{"seq": <n>, "event": {<OneBot v11 message>}}]}` with
monotonically increasing `seq`. This shape is documented, not yet verified
against the pinned upstream napcat-cli ([相关领域合同](channels.md#spec-35-6)); a mismatch is a typed
`CliSourceError`, never a guessed parse. **Enabling this source against a
real daemon requires parent verification of the wire shape.**

<a id="spec-15"></a>
<!-- migrated-contract: SPEC:15 -->
# 15 消息注入与认识人的能力

<!-- /migrated-contract: SPEC:15 -->

<a id="spec-15-1"></a>
<!-- migrated-contract: SPEC:15.1 -->
## 15.1 消息封套

外部消息一旦被 provider 接受，先形成 durable inbox event，再进入唯一主体 mailbox。输入至少含 provider/plugin ID、provider account/tenant、conversation/thread ID、message ID、发送者平台稳定 ID、显示名/别名、可选 person ID、发送/接收时间、正文与结构化内容、reply/mention 关系、scope、附件引用及获取方式。 配置选择、更新通知和网络/代理状态也可由插件作为结构化消息投递到同一主体；Agent 自主决定并委派处理，不默认把这些消息转交用户作答，见 [§9.4](network-observation.md#spec-9-4)。

主体看到自然语言可读的封套，例如“来自飞书某群，联系人 P0042（本群显示名 A、已验证平台 ID）发来消息……；附一个文件，可通过受控 attachment handle 获取”。显示名、别名、Agent 取的编号与平台账号不可混作身份主键。消息正文是消息内容，不能冒充 system 指令或由正文自称管理员。

富文本保留文本、链接、提及、图片/语音/视频/文件的结构。附件元数据含名称、MIME、大小、hash、有效期、下载/读取 provider 与权限；临时 URL/token 不直接塞进不必要的公开日志。主体需要内容时委派读取或媒体 worker，不能因为有附件就获得任意文件工具。

所有消息立即持久入队，但正在进行的 generation 能否中途加入新内容取决于后端。通常在下一安全步/工具边界注入；真正紧急时可按策略取消并重新构造请求。不能声称普通 HTTP streaming 会自动把新 token 加进已经开始的 prompt。消息在队列中可合并展示，但每个原始 ID 保留，不能静默丢弃。

<!-- /migrated-contract: SPEC:15.1 -->

<a id="spec-35-6"></a>
<!-- migrated-contract: SPEC:35.6 -->
## 35.6 NapCat 接入和升级期间的消息连续性

默认连接现有 NapCat 的 OneBot WebSocket 服务；原生插件和 napcat-cli 桥接均将事件交给同一 inbox，不自行启动另一个人格或回复 loop。HTTP/反向 WS 可作为后续或选装运输方式。账号认证、OneBot token、WebUI token 与本产品管理员身份分别表达；聊天继续默认开放，管理身份仍由人类明确设置。[官方网络配置](https://doc.napneko.icu/config/basic)、[OneBot WebSocket](https://github.com/botuniverse/onebot-11/blob/master/communication/ws.md)

固定核验 NapCatQQ 3420f0b76d7dc0ac92cde1663f21ea27f61e13bb 的 WS 事件发送路径只向当前 OPEN 连接发送，不能据此宣称断线事件会持久补投。升级主控时优先让独立 ingress spool 或已有 napcat-cli daemon 继续接收；已持久接收的事件必须接续处理。上游断线区间记录 gap，历史补齐是尽力核对；不能显示“重连成功”就认定所有消息都已恢复。[固定 WS 实现](https://github.com/NapNeko/NapCatQQ/blob/3420f0b76d7dc0ac92cde1663f21ea27f61e13bb/packages/napcat-onebot/network/websocket-server.ts)

复用 napcat-cli 3da08c03ff491cb8b86896102b0eab06c9b0c145 时，不能把秒级 since + 倒序 limit 当可靠消费游标。桥接层提供可持久推进的 cursor、消息去重与原始行身份；现有 wake 的触发节奏不能限制主体接收普通群消息。接收去重键包含 provider、account、conversation 和 message identity，不能只用一个跨账号可能冲突的数字。[固定事件存储](https://github.com/cyjin-yl/napcat-cli/blob/3da08c03ff491cb8b86896102b0eab06c9b0c145/napcat_cli/lib/events_sqlite.py)

出站先持久化 operation，再发出请求并保存实际回执。echo 只关联调用，不视为端到端去重保证；请求提交后超时进入 unknown，先核对再决定是否重发。附件位置按 NapCat 所在机器／容器解析，由文件插件复制或读取，不能把对方本地路径直接交给 WASM 当本地文件。

<!-- /migrated-contract: SPEC:35.6 -->

<a id="spec-41-6"></a>
<!-- migrated-contract: SPEC:41.6 -->
## 41.6 外部事件、注意力与输入放置

### 事件身份与三个独立进度

~~~text
InboxEvent {
  event_id, event_revision, event_kind,
  source: { provider_id, account_id, transport_identity_domain,
            conversation_id?, thread_id?, external_event_id?,
            source_event_revision?, source_cursor? },
  actor: { authenticated_platform_id?, person_link_ref?, role_grant_ref? },
  occurred_at?, received_at, admitted_at: CommitRef,
  causal_parents[], supersedes_ref?, raw_ref, normalized_ref?, attachment_refs[],
  normalization_state: ready | pending | failed,
  attention: { disposition: ready | deferred | quarantined,
               policy_revision, reason_ref?, review_condition? },
  processing_mirror?: { state: unresolved | interpreted | delegated | resolved,
                        disposition_refs[], mirrored_through: Map<ledger_id, seq> },
  record_revision
}

EventDisposition {
  disposition_id, event_id, event_revision, branch_id, step_id,
  disposition: interpreted | delegated | resolved | deferred,
  decision_ref, delegated_task_refs[], reason_ref?,
  committed_at: CommitRef
}

PresentationReceipt {
  presentation_id, event_id, event_revision,
  step_id, request_id, context_revision,
  representation: full | excerpt | aggregate_member | handle_only,
  source_span_ref?, committed_at: CommitRef
}
~~~

**权威存储位置。** 原始 InboxEvent 可以留在独立 ingress ledger I；EventDisposition、PresentationReceipt、输入撤回/选择控制记录与主体决定的权威位于控制 ledger L。S0 在 L 保存对 I 中精确 event revision 与原始对象的引用；S1 在 L 的同一事务写 EventDisposition、决定和后继。I 的 processing_mirror 仅为异步派生视图，不参与提交真相。Selection 读入 I 的候选后，必须用 L 的当前处置与撤回记录覆盖陈旧 ready 镜像；L 已提交而 I 尚未更新时崩溃，也不能重复委派该事件。对不同 availability branch 的独立处置保留 branch_id，合并时不伪造原先存在一个全局 writer。此合同不要求跨两个 SQLite 数据库原子写入。

入站至少区分：①本地可靠接收；②被放进某次模型输入；③主体已提交何种处置。另有下游任务完成和人类已读状态，不能混在第三项里。模型 request 被取消后，其 PresentationReceipt 仍可用于诊断，但消息仍 unresolved，直到 S1 明确解释、委派、解决或决定无需回复。

默认强去重键为 provider_id、account_id、transport_identity_domain、conversation identity、事件种类、external_event_id 和平台修订标识。platform message ID 相同但内容发生真实编辑时保存新 revision；来源声称同一 revision 却给出不同内容时保存冲突证据，不覆盖原文。无法取得稳定平台身份时，标记 weak_dedup，并使用接收 spool 的稳定记录 ID；不能宣称已经实现跨重连强去重。

接收事务先保存原始对象及可用的归一化记录，再确认本地 accepted；支持 ACK/可重放游标的 provider 仅在提交成功后推进其 ACK/cursor。NapCat WS 没有因此自动获得上游持久 replay：本地 spool 已接收的事件必须续接，连接缺口须独立登记 gap。若归一化需要异步工作，先保留 raw_ref 和 pending 状态，再由 worker 完成，不能因插件解析错误把消息丢在内存里。

### 优先级空洞不能用一个消费游标覆盖

~~~text
InputPlan {
  input_plan_id, step_id, plan_revision,
  snapshot_watermarks: Map<ledger_id, seq>,
  selected_event_refs[], deferred_event_refs[],
  causal_support_refs[], ordering_policy_ref,
  required_context_refs[], optional_contribution_refs[],
  budget: { max_events, max_input_tokens, actual_tokens?, count_method },
  projection_digest, created_commit: CommitRef
}
~~~

snapshot_watermarks 只是本次读视图的边界，不代表之前所有事件均已处理。主体先处理序号 105 的来电、尚未处理序号 101 的研究结果时，必须保留 101 的状态；不能把 last_consumed_seq 更新成 105 而丢掉它。实现可用事件状态索引和显式集合，不能仅用一条单调消费游标表达注意力进度。

首版工程默认每步最多直接选取 64 条事件、为事件投影预留 8192 个实际输入 token，取先达到的限额；这些是可改默认，不覆盖总上下文窗口校验。超过额度时先使用确定性的计数/来源分组及可分页原文引用，已有合格摘要也可复用；需要语义摘要时，创建通过同一 broker 的有限 summary child，不隐式增加模型并发。摘要中涉及 100 条消息不等于这 100 条均 resolved；S1 必须记录它实际处理的集合和所采取的处置，未处理记录仍可继续选取。若事件 token 数暂不可可靠计算，采用保守字节/文本预算并在最终 tokenizer 阶段重新校验。

选择顺序默认先满足因果约束，再按有效注意力等级、等待老化与稳定 tie-break 排列；相同来源的修订、撤销、问题关闭不能被孤立解释。跨来源没有已知因果时，用 admitted CommitRef 的 ledger_id/seq 与 event_id 作确定性 tie-break，只保证可复现，不伪称真实发生顺序。平台时间只作信息，不能倒置已知因果。

外部消息依 D3 通常最高紧急，但正文不能自行把 trusted priority、管理员身份或工具权限改高。DND/过滤仅改变可追溯的 attention disposition；主体仍可回看原始事件。归一化的权限撤销、seat 已被接管、问题关闭及任务取消等控制事实，在实际 effect arm 前必须核对最新 revision，即使模型当前仍使用较早的输入快照。

### generation 期间的注入

普通路径为：事件持久接收 → 发出合并的 attention notification → 下一 S0 选取。已发送请求的 context_revision 和 wire_digest 不可变。runtime 不得直接追加字符串到正在 decode 的历史对象；[§41.18](web-interface.md#spec-41-18) 的 live-steering 应用链是显式额外协议，不改变初始历史对象。

若 adapter 有已验证的 live steering：保存 steering operation、适用 request/attempt/engine incarnation 及后端确认的生效边界，按 [§41.18](web-interface.md#spec-41-18) 保存 SteeringApplication 与 ResultAppliedSteering；只有控制 ACK 不能登记为当前轮 live steering。收到后端接受不等于该事件已被主体处理。若只有 cancel：先持久 stop intent，等待 broker 确认旧请求停止，再为同一步构建新的 InputPlan/request revision；原模型 partial 保留但不执行不完整工具调用。若没有请求内停止能力：等有限 generation 结束，在下一边界注入。

一个当前请求只保留一个合并的 stop/escalation 意图；同一轮连续来十条消息不发送十次无意义 cancel。必要事件始终入 inbox，通知丢失可由 ready 索引恢复，不能依赖“收到通知”才知道有消息。

<!-- /migrated-contract: SPEC:41.6 -->

<a id="spec-52"></a>
<!-- migrated-contract: SPEC:52 -->
# 52 通道协议优先级 / Channel Protocol Priority

来源：用户指示 2026-11-06；与 [§4](subject-runtime.md#spec-4)、[§45.10](cognition-units.md#spec-45-10) DEP01/DEP02 的渠道绑定一致。

首批支持协议（first-supported protocols）：

1. **NapCat**（OneBot v11 + NapCat 扩展）— QQ 群聊与私聊的主连接。
2. **SnowLuma** — 轻量 QQ 协议适配。
3. **QQ Bot 官方 API** — 腾讯开放平台机器人接口。
4. **Discord** — Discord Bot Gateway + REST。

WebUI 也作为一条 IM 通道，走相同的 `PublicationTarget` 接口和消息流水线，不另造独立交互栈。所有渠道共享统一的 `ChannelPlugin` trait：

```rust
/// 所有通道协议实现此 trait。
/// All channel protocols implement this trait.
pub trait ChannelPlugin: Send + Sync + 'static {
    /// 协议标识，例如 "napcat", "snowluma", "qqbot-official", "discord", "webui"。
    fn protocol_id(&self) -> &str;

    /// 连接生命周期：启动、重连、优雅关闭。
    /// Connection lifecycle: start, reconnect, graceful shutdown.
    fn start(&mut self, ctx: ChannelContext) -> Result<(), ChannelError>;
    fn shutdown(&mut self) -> Result<(), ChannelError>;

    /// 发送一条已格式化的出站消息。
    /// Send a formatted outbound message.
    fn send(&self, target: &PublicationTarget, message: &OutboundMessage) -> Result<SendReceipt, ChannelError>;

    /// 注册入站消息回调。
    /// Register an inbound message callback.
    fn on_message(&mut self, handler: Box<dyn Fn(InboundMessage) + Send + 'static>);
}
```

`ChannelPlugin` 是安装边界：每条协议实现一个插件，通过 [§45.10](cognition-units.md#spec-45-10) 的依赖装配注册，运行时按 `protocol_id` 发现和绑定。协议之间不互相依赖。一条协议断开或卸载不影响其余协议正常工作（与 DEP02 一致）。

WebUI 通道的 `PublicationTarget` 指向浏览器会话而非外部 IM 平台。它复用相同的消息格式化与 receipt 机制，但不依赖外部网络连接。

首批协议的优先级不是路由权重或 fallback 顺序，而是开发与验收的优先顺序。产品运行时同时连接多条协议，由主体决定在哪条通道回复。排序反映实际用户环境与渠道覆盖面：NapCat 作为日常 QQ 主连接排第一；Discord 覆盖非 QQ 场景。后续协议（Telegram、Matrix、微信等）按相同 `ChannelPlugin` trait 接入，不改动已有协议代码。

<!-- /migrated-contract: SPEC:52 -->

<a id="spec-57-4"></a>
<!-- migrated-contract: SPEC:57.4 -->
## 57.4 通道拥有自己的表面语法

**一套语法，多种表面。** 内核回复动词仍是 `[reply:kind:id]`，不变。通道插件拥有自己的行内子语法与平台渲染：`@<id>`、`[quote:<message_id>]`、Markdown 到平台可读文本的降级，由插件的 `describe`/`dispatch` 翻译成平台段（`napcat/channel.rs:392-398,439-444`）。插件在 `prompt_names` 里向主体说明自己接受什么。

**平台事件是感知。** 撤回、入群、离开、戳一戳等 `notice` 不是消息，也不是命令，而是可感知的观察：进入主体输入序列，保留来源与被撤回消息的原 id（`channel.rs:127-129`）。主体因此不会继续引用一条已经不存在的消息。

**能力声明必须可达。** 只有 `describe` 能实际产出的动作才算该通道的能力（`feishu/plugin.rs:124-134`、`qq_official/plugin.rs:131-141` 目前声明了 edit/retract/typing 却只产 Send）。声明与可达不一致时，能力表按可达事实修正。

**保留。** 主动外发无需入站触发（`channel.rs:316-336`），这是对的，不加门。

<!-- /migrated-contract: SPEC:57.4 -->

<a id="spec-57-15"></a>
<!-- migrated-contract: SPEC:57.15 -->
## 57.15 通道优先级的现行读法（2026-11-09）

来源：owner 2026-11-09 的指示（QQ 群里的公告与讨论）：**SnowLuma 因腾讯方律师函自今日起转为 private，不再 public；取消 SnowLuma 的适配计划，改成 QQ 官方 Bot 接口。** 原文不在 [§33](../requirements/user-sources.md#spec-33)（[§33](../requirements/user-sources.md#spec-33) 是锁定的历史字节），此处记录现行读法。

- [§52](channels.md#spec-52) 的首批协议排序是 **2026-11-06 的读法**，其历史句子保留原文；现行顺序为：**NapCat（OneBot v11）仍是日常 QQ 主连接 → QQ 官方 Bot API（腾讯开放平台）升到第二位并成为下一条要真正做通的通道 → Discord → 其后 Telegram／Matrix／微信等按同一 `ChannelPlugin` trait 接入**。SnowLuma 从首批里去掉。
- 这是**开发与验收的优先顺序**，不是路由权重或 fallback 顺序（[§52](channels.md#spec-52) 原文如此规定，仍然有效）。产品运行时仍可同时连接多条协议，由主体决定在哪条通道回复。
- 仓库里没有 SnowLuma 适配器代码，因此"取消适配计划"不需要删除任何实现；已有代码不动。
- **[§52](channels.md#spec-52) 印的 `ChannelPlugin` trait 文本已经过时**（SPEC 6334–6375 印的是 `protocol_id`/`start`/`shutdown`/`send`/`on_message`）。现行实现是 `crates/shenko-types/src/channel.rs` 的 `classify`/`staged_text`/`resolve_reply`/`describe`/`dispatch`/`resolve_attachment`，加上 `sources`/`kinds`/`prompt_names`。以代码为准；[§52](channels.md#spec-52) 的 trait 段落按本条理解，不据此另造第二套接口。
- QQ 官方 Bot 的真实约束（被动回复窗口、群支持范围、卡片与主动触达额度）是平台事实，按实际可达能力记录，不写成产品权限门；能力表只列 `describe` 实际能产出的动作（[§57.4](channels.md#spec-57-4)、验收 RT19）。
- [§56](../requirements/traceability.md#spec-56) 追踪矩阵里"SnowLuma/官方 QQ/Discord 适配未全部落地"那一行按本条理解：SnowLuma 一项不再是待做项，官方 QQ 一项升为下一条要真正做通的通道。
<!-- /migrated-contract: SPEC:57.15 -->

## Implementation appendix: built-in conversation reads (2026-11-13)

`Store::channel_conversation_messages(agent_id, limit)` in
`crates/shenko-store/src/trajectory.rs` projects existing facts without writing
or acknowledging them. Staged input ownership follows `session_events.session_id`
to `raw_sessions.agent_id`; publications use `subject_intents.agent_id`, and
their evidence lookup remains correlated to that owned intent's operation id.
The publication window remains the most recent requested number of that
subject's publication rows. For staged inputs, it streams `session_events`
newest first until the requested number of unique inbox event identities has
passed envelope, reply-audience and echo
admission, or the actual history is exhausted. Re-staged copies, unclassified
payloads, envelopes without a reply audience and publication echoes do not spend
that input window. It retains only the admitted identities and messages, without
a raw-row multiplier or materializing the complete history.

A receipt's provider message id identifies a possible echo only within that
recent publication window and the exact canonical target's channel, account,
audience kind and opaque audience id. The inbound plugin must also have stated
`actor.own_account`; equal ids in other accounts, audiences or providers remain
independent messages. The projection preserves the original inbox event id,
payload text and platform/receive time, and the publication intent id, state and
receipt observation time (else recorded task creation, then step start, else unknown). Timestamps
use the canonical UTC representation, unknown times sort last, and malformed
stored publication/evidence records or known timestamps return decoding errors.
Only evidence declared as `channel_receipt` is decoded as a provider answer;
generic recovery observations retain their own time and unknown effect facts.
No writer, frozen schema, delivery verdict or input disposition changes.
The existing Store roundtrip conversation
regression covers these scopes, two real subjects sharing the Store and a small
window behind re-staged and inapplicable inputs; runtime verification is separate.

## Implementation appendix: canonical originals across upgrades

`ChannelActor::own_account` preserves the presence of the platform fact:
`Some(true)` records the served account's line, `Some(false)` records an
observed counterpart, and `None` records missing attribution. Historical
envelopes that omitted the field remain absent when serialized again. New
NapCat message classification records the `message_sent` / `message` fact;
notice events do not assert the origin of a sent line. iLink records a
counterpart only for its explicit user-message type. A projector does not
reconstruct missing ownership from the current account configuration.

The native NapCat admission path (`bin/shenko/src/lib.rs`,
`napcat_inbox_event`) first reads the event's original account-bound spool
row and the control ledger's disposition. An existing S1 disposition alone
permits the spool acknowledgement. For an unresolved event,
`Store::first_subject_input(agent_id, source_event_id)` reads the earliest
staged original from that agent's current branch, including a sealed or
deferred step. This is a pure read of `subject_raw_inputs` and
`session_events`, supported by an index on source identity and session; it
does not stage, enrich, acknowledge or change a sealed input. Admission
reuses that complete inbox original, including its payload and timestamps,
before running any newer classifier or attachment resolver. A fresh event
uses the spool's first wire original and reception time. Repeat/conflict
wire observations remain in the ingress ledger.

`context_projection::stage_input` reuses the first staged channel projection
when the complete stable source fields still match. This also preserves the
old text when a deferred event enters a later step. Without retained text,
NapCat renders missing attribution explicitly as unknown. A recorded
ownership fact controls new projections even when the actor id differs from
the current configured account. The existing full-fact staging conflict
fence remains in force; no input identity, S0 schema, sealed request or
external-effect outcome is rewritten to make a replay fit.

The bounded regression sources cover literal old envelopes through native
spool delivery, staged/sealed reopen, the unchanged staging conflict, and
S1-based acknowledgement; the subject regression separately uses real S0
sealing and deferred re-staging to retain the original text and request
identity. These source changes have not been executed by the coding worker.
Parent verification must bind the combined candidate and distinguish native
durability mechanisms from actual model behavior.
