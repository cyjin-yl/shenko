# S2 官方 IM API 阻断表（2026-11-22）

**范围。** 本文回答一个具体问题：S2 首版开发者发行里的 IM 验收（V1-01、V1-10）还缺哪些平台能力，缺的是哪一条 API/权限，哪些能在本地补，哪些必须由外部账号或平台解除，以及关闭每一项需要什么证据。本文不新增合同；[`docs/specs/channel-adapters.md`](../specs/channel-adapters.md)、[`docs/specs/channels.md`](../specs/channels.md)、[`docs/specs/self-update.md`](../specs/self-update.md) 与 [COMPLETION](../governance/COMPLETION.md) 仍是权威。

## 0. 结论（先读这一段）

1. **S2 的 IM 分母是 V1-01 与 V1-10**（[§35.10](../specs/self-update.md#spec-35-10)）。S2 sprint checklist 把这两项写成“local channel recovery + external account evidence”，并注明 **“Live IM awaits the official API”**（工作区 `build/s2-sprint-20261118/sprint-tracker-body.md`，非仓库文件）。V1-14 还需要一条已配置的开放聊天通道来观察“无 QQ 新消息时”的空闲，但它不需要额外的官方 API 权限。
2. **仓库至今没有任何真实平台往返。** [`channels.md` 的证据附录](../specs/channels.md#spec-channel-evidence-level) 明确写了 NapCat、QQ 官方、iLink、飞书都没有在真实平台完成过一次入站或出站；`docs/evidence/qq-official-capability-audit-20261110.md` 只是官方文档与插件源码的对照，不是账号探测。`build/s2-sprint-20261118/evidence/im-local-protocol-r1` 的 13/13 是 `local_protocol`，不是 `verified_real`。
3. **阻断不全是“缺账号”。** 逐条对照官方文档与基线源码后，QQ 官方与飞书各有本地实现缺口：QQ 的媒体发送把消息体发到上传端点、频道发送路径多了 `/v2`；飞书的 `app_access_token` 用错端点，长连接协议（`/callback/ws/endpoint` + protobuf + ACK）没有实现。拿到凭据也不能直接跑通。NapCat 路径本地已接线，主要缺真实 QQ 客户端账号。
4. **S2 阶段合同只要求默认 NapCat。** [§29 的 S2 行](../specs/verification.md#spec-29) 写的是“默认 NapCat、HostExecution/SSH、真实 Agent seat…”，QQ 官方/iLink/飞书在 S3 行。S2 sprint checklist 选择等官方 API 是排期选择，不是 SPEC 的 S2 义务。若已有 NapCat 账号，V1-01/V1-10 可按 S2 合同先关闭；QQ 官方作为 S3 推进。
5. **不能把 `local_protocol` 通过写成 V1-01/V1-10 完成。** [COMPLETION §5](../governance/COMPLETION.md) 区分 `simulated` 与 `verified_real`；`blocked_external` 不是完成。真实平台证据缺失时，这两项必须保持未完成。

## 1. 判定口径

- **合同。** 入站分类/游标/去重、出站 send/edit/retract/typing、回执与 unknown 见 [§15.3](../specs/channel-adapters.md#spec-15-3)；中国平台与平台边界见 [§16](../specs/channel-adapters.md#spec-16)；通道优先级与“官方 QQ 升为下一条要真正做通的通道”见 [§52](../specs/channels.md#spec-52) 与 [§57.15](../specs/channels.md#spec-57-15)；能力表只列 `describe` 实际能产出的动作见 [§57.4](../specs/channels.md#spec-57-4) 与 RT19（[§29 验收表](../specs/verification.md#spec-29)）。
- **证据等级。** `specified`、`implemented`、`buildable`、`simulated`、`verified_real`、`verified_long_run` 是不同事实；`blocked_external` 不是完成（[COMPLETION §5](../governance/COMPLETION.md)）。本文只做源码/文档对照，**没有** build、test、原生运行、模型调用或平台请求。
- **平台事实不是产品门。** 被动回复窗口、频控、群主开关、用户主动消息开关按 [§16](../specs/channel-adapters.md#spec-16) 记为平台事实，不写成产品审批或 allowlist。
- **安全边界。** 本文不记录任何真实 `app_secret`、`bot_token`、`access_token`、`client_secret` 或可复用的完整附件 URL。真实值只进入本地凭据设施；证据只保留脱敏账号 scope 与摘要。

## 2. 阻断总表

列含义：**归属** `本地` = 可在本仓库补/改；`外部` = 必须由账号、租户、群主或平台提供；`本地+外部` = 两边都缺。**S2** 表示是否直接阻断 V1-01/V1-10。

### 2.1 QQ 官方 Bot（S2 checklist 里的 “official API”）

| ID | 需要的平台能力 | 缺失/待补的 API 或权限（精确） | 基线状态（锚点） | 归属 | S2 | 关闭证据 |
|---|---|---|---|---|---|---|
| QQ-1 | Bot 身份与访问令牌 | 腾讯开放平台主体认证 + 创建机器人 → `AppID`/`AppSecret`；个人主体只能把自己是群主/管理员的群拉入、在“开发设置”添加开发体验用户；企业主体才可公开添加。`POST https://bots.qq.com/app/getAppAccessToken`，body `{appId, clientSecret}`，返回 `code == 0` 与 `access_token`。 | 配置类型与 token 客户端已实现：`crates/shenko-channel-adapters/src/qq_official/config.rs:44-47`、`api.rs:8`；无真实凭据。`build/s2-sprint-20261118/e2e-suite/tests/seats_channels/test_blocked_external_im.py:85-110`（`BE-V1-01-03`）明确 `BLOCKED_EXTERNAL`。 | 外部（账号/主体/机器人）；本地已有 `--channels-config`（`bin/shenko/src/lib.rs:106-113`） | 是 | 脱敏 `app_id`、token endpoint `code == 0` 与 `access_token` 存在性（不记录 token）、`qq-official` health=`ready`、账号 scope `qq:<app_id>` |
| QQ-2 | 单聊/群事件入站 | WebSocket 网关 + intents：`GROUP_AND_C2C_EVENT (1<<25)`（默认包含）；频道另需 `GUILDS (1<<0)` + `PUBLIC_GUILD_MESSAGES (1<<30)`；`GET /gateway` 或 `/gateway/bot`；identify/resume。`GUILD_MESSAGES (1<<9)` 仅私域机器人可设，默认集合**没有**包含它（正确）。 | 网关、识别、心跳、resume 已实现：`crates/shenko-channel-adapters/src/qq_official/gateway.rs`；默认 intents `config.rs:28-47`；无真实 READY。 | 外部：平台按机器人类型授予 intents；标准群/单聊事件不需要单独申请 | 是 | `READY` 的 `session_id`/`seq`、原始事件 `t`、游标文件 `qq-official-gateway-*.json`、ingress sqlite 行 |
| QQ-3 | 群全量消息 / 主动发言 | 群主在手机 QQ 的机器人资料页或入群“设置权限”里开启“接收全量消息”和“主动推送消息”；用户在机器人资料页开启/关闭“主动消息”。主动消息频控：企业/个人认证 60 qpm、未认证 30 qpm；单群 20 qpm、1000 条/日；单聊 10 qps（认证）/5 qps（未认证）、20 qpm、1000 条/日。 | 能力表 notes 记录平台事实（`qq_official/plugin.rs:299`）；无真实开关或频控测量。 | 外部（群主/用户操作）；本地只记录事实，不写成产品门 | 条件：需要看非 @ 群消息或超出被动窗口主动发言时 | 群主设置截图/说明、被动 vs 主动消息的 provider receipt、频控/窗口测量 |
| QQ-4 | 入站截图（附件） | 事件 `attachments[].url`（`multimedia.nt.qq.com.cn`，带 `rkey`）；分类、locator、CAS 封印；`message_scene.ext` 可能带 `auth_token`。 | `events.rs:227+` 解析 locator；`plugin.rs:790+` 有界下载并封印 CAS；真实 URL 未验证。 | 外部（真实图片）；本地：验证 401/403 时是否需 `auth_token`/`rkey` 处理 | 是 | 原始事件 id、附件 URL 的脱敏 provenance、CAS `blake3`、MIME/size、`channel_runtime_facts` 的 attachment/object |
| QQ-5 | 被动/主动文本回复与回执 | `POST /v2/users/{user_openid}/messages`、`POST /v2/groups/{group_openid}/messages`；被动回复带 `msg_id` + `msg_seq`；单聊被动 60 分钟/4 次（页面 `msg_id` 字段另写 5 分钟，页面自身不一致）；群被动 60 分钟/5 次（开发者是群主/管理员时 50 次）；频道被动 5 分钟。`msg_seq` 防同 `msg_id` 重复。 | 发送/回执已实现；能力表把群窗口写成“5 分钟/5 次”（`qq_official/plugin.rs:299`，与当前官方群发送页的 60 分钟/5 次不符）；默认 `reply_window_seconds=300` 窄于 60 分钟。 | 本地（更正 notes/窗口默认值）；外部（真实会话） | 是 | publication intent/attempt/receipt、provider `message_id`、`msg_seq`、`accepted=true`、无 `304023/304024` 审核/失败码 |
| QQ-6 | 出站媒体 | 先 `POST /v2/{users\|groups}/{openid}/files` 得 `file_info`，再向 `/v2/{users\|groups}/{openid}/messages` 发 `msg_type:7, media.file_info`。 | **本地缺陷**：`qq_official/api.rs:230-244` 的 `send_media` 把消息体 POST 回 `files_url`（上传端点），不是 messages 端点；`plugin.rs` 已先 `upload_media`。 | 本地（改 `send_media` 目标 URL）；外部（真实上传/发送） | 条件：结果包含图片/文件时；RT19 媒体能力需要 | `file_info` + `message_id`、CAS digest、MIME/size；RT19 媒体能力 |
| QQ-7 | 频道（guild）发送/typing | `POST /channels/{channel_id}/messages`（官方频道发送页路径，无 `/v2`）；频道发送要求机器人保持 WebSocket 在线；频道 Markdown 需内邀；typing 路径需按官方页面再次核对。 | **本地缺陷**：`qq_official/api.rs:222` 用 `/v2/channels/{channel_id}/messages`；`api.rs:307` 用 `/v2/channels/{channel_id}/typing`；能力表却声明 `guild_channel: true`（`plugin.rs:278`）。 | 本地（改路径/确认 typing）；外部（频道权限/内邀） | 否（S3）；但 RT19 的 `guild_channel` 声明需要 | 频道 `message_id`、在线 WS、RT19 频道能力；不可达则把 `guild_channel` 降为 false |
| QQ-8 | 引用回复 | 非机器人消息的引用取 `message_scene.ext` 的 `msg_idx`（不是 `message_id`）；机器人自己的消息取发送响应 `ext_info.ref_idx`。 | 分类器未把 `msg_idx` 暴露给主体；`[quote:<message_id>]` 直接写 `message_reference.message_id`（`qq_official/outbound.rs`）。 | 本地（映射/拒绝）；外部（真实引用场景） | 否（S3） | 成功引用的 `message_reference` 与平台回执；否则不宣称引用能力 |

### 2.2 NapCat / OneBot（[§29](../specs/verification.md#spec-29) 的 S2 默认通道）

| ID | 需要的平台能力 | 缺失/待补的 API 或权限（精确） | 基线状态（锚点） | 归属 | S2 | 关闭证据 |
|---|---|---|---|---|---|---|
| NAP-1 | 真实 QQ 收/发 | QQ 账号登录 NTQQ + NapCat 的 OneBot v11 WebSocket URL/token；`--napcat`、`--napcat-account`、`--napcat-access-token`；bot 是账号本人。 | 适配器、持久 spool、出站已接线：`bin/shenko/src/runtime_napcat.rs`；本地协议 13/13（`im-local-protocol-r1`）；无真实账号。 | 外部（QQ 账号/NTQQ/NapCat）；本地已接线 | 是（若选 NapCat 路径） | OneBot lifecycle/connect、原始事件、SNIG 行、`send_group_msg`/`send_private_msg` 的 echo/action、provider receipt |
| NAP-2 | 入站附件 | `--napcat-file-root` 指向 NapCat 主机上的媒体目录；`--napcat-file-host` 命名远端主机；真实图片文件。 | 已实现 locator、边界、pending 事实；无真实文件。 | 外部（账号/文件）+ 本地配置 | 是（截图） | CAS digest、MIME/size、`attachment_locator` pending/success 事实 |
| NAP-3 | napcat-cli 第二来源（可选） | 已部署的 napcat-cli `/events`；`--napcat-cli-url`。 | 已实现 `napcat_cli.rs`；无真实 daemon。 | 外部 | 条件（若用 cross-source） | 跨来源同 `message_id` 去重、SNIG 行 |
| NAP-4 | 断线/缺口 | 真实 NTQQ/NapCat 断开或网络阻断；OneBot 重连；`IngressGap`/`gap_count`。 | 本地协议覆盖 `ws_disconnect_gap_honesty`；无真实断线。 | 外部（账号）+ 本地故障注入 | 是（V1-10） | 断线前后 cursor、gap 事实、重放事件、无丢失 |
| NAP-5 | 被动窗口/频控测量 | 平台事实；当前无测量值（[channels.md 附录](../specs/channels.md#spec-channel-evidence-level)）。 | 能力表 `proactive: true` 但未核实。 | 外部 | 否（能力诚实性） | 实际窗口/频控测量；否则保持“未核实” |

### 2.3 iLink / 飞书（S3；不直接阻塞 S2）

| ID | 需要的平台能力 | 缺失/待补的 API 或权限（精确） | 基线状态（锚点） | 归属 | S2 | 关闭证据 |
|---|---|---|---|---|---|---|
| IL-1 | QR 授权入口 | `QrLogin` 已实现但没有 CLI/运行入口；操作者必须在别处取得 `bot_token`。 | `crates/shenko-channel-adapters/src/ilink/auth.rs:88,114`；全仓库只有 `ilink/mod.rs` 引用；`runtime_adapters` 只从配置读 token。 | 本地（加 `shenko channels ilink-login` 或启动流程） | 否 | QR 请求/轮询/confirmed、`0600` 凭据文件、重启读取 |
| IL-2 | 真实 iLink 账号 | provider 可达；`iLink-App-Id`/`client_version`；`bot_type=3`；人工扫码；`bot_token`/`ilink_bot_id`/`ilink_user_id`。 | 配置、长轮询、发送、媒体已实现；无真实 provider。 | 外部 | 否 | confirmed 凭据（脱敏）、direct 往返、`context_token`、游标 |
| FS-1 | 飞书应用/权限/发布 | 企业自建应用、开启机器人、事件订阅 `im.message.receive_v1`；scope：`im:message` 或 `im:message:send_as_bot`；收单聊 `im:message.p2p_msg:readonly`；收群 @ `im:message.group_at_msg:readonly`（或敏感 `im:message.group_msg`）；媒体 `im:resource`/`im:resource:upload`；创建版本 → 发布 → 管理员审核 → 可用范围；机器人入群且有发言权。 | 配置、发送、事件分类已实现；无真实应用。 | 外部 | 否 | `tenant_access_token`（脱敏）、`message_id`、事件 `event_id`、scope 列表、发布/审核记录 |
| FS-2 | `app_access_token` | `POST /open-apis/auth/v3/app_access_token/internal`，body `{app_id, app_secret}`，返回 `app_access_token`。 | **本地缺陷**：`feishu/auth.rs:214-215` 用 `tenant_access_token/internal`，`app_token()` 却取 `app_access_token`（`auth.rs:127-146`）→ 必然 `Incomplete`。 | 本地 | 否（S3） | 端点修复后的 `code == 0` + token 存在性；单测 UNRUN |
| FS-3 | 长连接入站 | `POST https://open.feishu.cn/callback/ws/endpoint`，body `{AppID, AppSecret}` → `URL` + `ClientConfig`；WSS 上 `pbbp2.Frame` protobuf Binary；CONTROL ping/pong；每 DATA 帧 3 秒内 ACK（`biz_rt`）；分片重组；`ClientConfig.PingInterval`。 | **本地缺陷**：`feishu/monitor.rs:106-158` 直接连 `{base}/callback/ws`、`Authorization: Bearer {app_access_token}`、只处理 `Message::Text` JSON；没有 endpoint 发现、protobuf、ACK、分片。 | 本地（较大；可评估 `feishu-ws` crate 或仓内 `prost`；依赖决策归父控） | 否（S3） | 连接成功日志/健康、真实 `im.message.receive_v1` 事件、ACK 帧、重连 |
| FS-4 | 出站媒体 | 官方发送文档要求 `POST /open-apis/im/v1/images`（`image_type=message`）或 `/im/v1/files`（`file_type`），scope `im:resource`/`im:resource:upload`；发送页明确不能用 `drive/v1/medias/upload_all` 的 `file_token`。 | **本地风险**：`feishu/api.rs:226` 用 `drive/v1/medias/upload_all`（`parent_type`），`media.rs` 把 `file_key` 放进 `post` 块；未验证平台是否接受。 | 本地（改为 `im/v1/images\|files` 或证明 drive key 可用） | 否（S3） | `image_key`/`file_key` + `message_id`；真实图片/文件往返 |
| FS-5 | 卡片 | [§16](../specs/channel-adapters.md#spec-16) 列了“卡片”；当前只有 `post` 块，没有 `interactive`/card。 | 本地 S3 缺口 | 本地 | 否 | 真实卡片消息 + `message_id`；否则能力表不宣称 |
| FS-6 | 长连接仅企业自建 | 飞书长连接只支持企业自建应用，不支持商店应用；保存订阅方式时本地客户端必须已连接。 | 文档事实；当前无 live | 外部（应用类型/管理员） | 否 | 应用类型、订阅方式保存成功、首次事件 |

### 2.4 横切阻断

| ID | 需要的平台能力 | 缺失/待补的 API 或权限（精确） | 基线状态（锚点） | 归属 | S2 | 关闭证据 |
|---|---|---|---|---|---|---|
| X-1 | 出网 | 到达 `bots.qq.com`、`api.bot.qq.com`（WSS）、`multimedia.nt.qq.com.cn`、`ilinkai.weixin.qq.com`、`open.feishu.cn`、飞书 WSS/CDN；代理/Tor/企业网关可能阻断 WSS、长轮询或大附件。 | `im-local-protocol` 不需要真实出网；`im-local-protocol-r1/report.json` 记录 27B 不可达，说明环境存在网络限制，不能假定平台可达。 | 本地探测 + 外部网络 | 是 | 脱敏 endpoint provenance、health、连接/超时分类 |
| X-2 | 只读 doctor | 当前没有“只验证凭据/端点/网关、不发消息”的命令；启动整个 subject 才能发现凭据错误，live 窗口过长。 | 本地缺失 | 本地 | 条件 | `shenko channels doctor --config ...` 的 token/gateway/WSS 结果；不产生外发 |
| X-3 | 故障注入/证据 | V1-10 需要可控断线、重复、丢回执；没有 live 证据 harness。 | 本地缺失 | 本地 + 外部账号 | 是 | 代理丢响应、断线重连、重复帧/平台重复、脱敏 receipt、DB/CAS 摘要 |
| X-4 | unknown 不盲重放 | [§36](../specs/durable-state.md#spec-36) / E02：已发出但响应丢失保留 unknown，先核对，不盲重发。 | 本地代码/mock 已覆盖；无真实外部效果。 | 外部（真实发送）+ 本地故障注入 | 是 | operation 状态、无自动重发、对账证据、`acknowledged_unknown`（如适用） |
| X-5 | 证据等级 | `local_protocol` ≠ `verified_real`；`blocked_external` 不是完成。 | [COMPLETION §5](../governance/COMPLETION.md) | 流程 | 是 | 每项的 evidence level + candidate commit + environment |

## 3. 本地可做的工作（全部 UNRUN，未应用）

这一节是“拿到外部凭据之前就能做”的具体改动。它们不是 2.x 表里的外部阻断；不完成它们，外部凭据也未必能跑通。所有条目都**没有**在本包执行 build/test。

| 编号 | 改动 | 精确锚点 | 为什么阻断 live | 验证（父控执行，UNRUN） |
|---|---|---|---|---|
| L1 | `send_media` 改发到 messages 端点 | `crates/shenko-channel-adapters/src/qq_official/api.rs:230-244`；可新增 `MediaScope::messages_url`，`send_media` 用它而不是 `files_url` | 现在把 `msg_type:7` 消息体 POST 到 `/files` 上传端点，真实平台会拒绝/误解 | 适配器单测（mock HTTP 断言 URL）+ 一次真实图片发送；记录 `file_info` 与 `message_id` |
| L2 | 频道发送/typing 路径去掉 `/v2` | `qq_official/api.rs:214-226`、`api.rs:301-311`；对照官方 `POST /channels/{channel_id}/messages` | 频道发送声明 `guild_channel: true`（`plugin.rs:278`），但路径与官方页不一致；不可达即违反 RT19 | mock 断言路径；真实频道消息；不可达则降能力表 |
| L3 | 群被动窗口 notes/默认值 | `qq_official/plugin.rs:299`；`config.rs` 的 `reply_window_seconds` 默认 300 | 官方群发送页写 60 分钟/5 次（群主/管理员 50 次）；当前 notes 写 5 分钟/5，默认缓存 5 分钟后丢失 `msg_id`，回复会变成主动消息（受群主开关限制） | 断言 notes 文本；用假时钟验证 10 分钟后仍能被动回复；真实被动回复 |
| L4 | 入站附件 `auth_token` 兜底 | `qq_official/events.rs:227+` 未读 `message_scene.ext`；`plugin.rs:790+` 只带 URL | 真实附件 URL 可能要求 `auth_token`/`rkey`；若 401/403，当前会记 pending 并丢字节（消息保留） | 用真实事件 URL 测试；401/403 时加 `auth_token` 查询/头部；CAS digest |
| L5 | 飞书 `app_token` 端点 | `feishu/auth.rs:214-215` 目前是 `tenant_access_token/internal`；`app_token()` 取 `app_access_token`（`auth.rs:127-146`） | 租户端点不返回 `app_access_token`，`app_token()` 必然 `Incomplete`，长连接无法开始 | 拆 `tenant_url()`/`app_url()`；mock 断言 `/open-apis/auth/v3/app_access_token/internal`；`code==0` |
| L6 | 飞书长连接协议 | `feishu/monitor.rs:106-158`；需要 `POST /callback/ws/endpoint` + `pbbp2.Frame` protobuf + CONTROL ping/pong + 每 DATA 帧 3 秒 ACK + 分片 | 当前连 `{base}/callback/ws` 并只读 JSON Text；飞书长连接是 Binary protobuf，且必须 ACK；真实入站不可能工作 | 连接真实应用，记录 connected/事件/ACK；或用官方 SDK 互操作测试；父控决定是否引入 `feishu-ws`/`prost` |
| L7 | 飞书出站媒体端点 | `feishu/api.rs:226` 用 `drive/v1/medias/upload_all`；`feishu/media.rs` 把 `file_key` 放进 `post` 块 | 官方发送页要求 `im/v1/images` / `im/v1/files` 的 key，并明确 drive `file_token` 不能用于消息；当前路径很可能被拒 | 真实图片/文件发送；记录 `image_key`/`file_key` + `message_id`；不可用则改端点 |
| L8 | iLink QR 登录入口 | `ilink/auth.rs:88,114,207`；`bin/shenko/src/lib.rs` 增加子命令；`runtime_adapters` 仍读配置 | `QrLogin` 是死代码；操作者必须在别处取得 `bot_token`，否则 iLink 无法从零安装 | 请求 QR → 轮询 → confirmed → `0600` 文件 → 重启读取；不打印 token |
| L9 | 只读 `channels doctor` | 新增 `bin/shenko` 子命令/模块；复用 `QqOfficialApi::access_token/gateway_url`、`TenantAuth::app_token`、`ILinkApi::get_updates`（只读） | 缩短 live 窗口；避免启动 subject 后才发现 token/网关错；便于外部账号交接 | 对 mock 与真实账号各跑一次；失败分类；**不得**发送消息 |
| L10 | live 证据 harness | 测试脚本/工具（不属于产品 runtime）：可控丢响应代理、断线、重复帧、脱敏 receipt | V1-10 需要故障注入；现有 `local_protocol` 只到 mock | 父控在 CT102 开发容器执行；记录 candidate、环境、时间、DB/CAS 摘要；UNRUN |
| L11 | QQ 引用 `msg_idx` | `qq_official/events.rs` 分类与 `outbound.rs` 的 `[quote:]` | 非机器人消息引用需要 `msg_idx`；当前只传 `message_id`，S3 引用能力不可靠 | 真实群引用回复；不可达则能力表不宣称 |
| L12 | 飞书卡片 | `feishu/media.rs` 只有 `post` 块；无 `interactive` | [§16](../specs/channel-adapters.md#spec-16) 列了卡片；S3 才需要 | 真实卡片消息；不可达则能力表不宣称 |

**本地改动的边界：** L1–L12 都只是候选；没有一条在本包执行过。L6 是实质性协议实现，不能用一个“看起来能连”的测试代替真实 ACK/事件；L9 是只读探测，不能发送真实消息，也不能把 token 写进日志。任何改动都必须保留既有 `local_protocol` 回归，并新增不超过治理上限的定向测试（[COMPLETION §1](../governance/COMPLETION.md)）。

## 4. 外部解除步骤（按平台）

### 4.1 QQ 官方 Bot（S2 checklist 的 “official API”）

1. 用真实账号登录 `q.qq.com`，完成主体认证（个人或企业）。个人主体只能把机器人加入自己是群主/管理员的群，并通过“开发设置”添加开发体验用户；企业主体才可公开添加。
2. 创建机器人，取得 `AppID`/`AppSecret`（secret 只在本地凭据设施；不写进证据、日志、URL）。
3. 把机器人加入一个测试群（或让开发体验用户私聊）。群主在手机 QQ 的机器人资料页/入群“设置权限”中按需开启“接收全量消息”“主动推送消息”；用户按需开启“主动消息”。
4. 在 `channels.json` 的 `qq_official` 写全字段（`app_id`、`app_secret`、`api_base`、`token_base`、`intents`、`shard`、`reply_window_seconds`、`reply_context_capacity`、`admin_openids`）；启动 `shenko start --channels-config <path>`，确认 health/capability 事实。
5. 人工发送一条消息 + 一张截图；记录入站、CAS、出站进度/结果。V1-10 再按 4.4 做断线/重复/丢回执。

### 4.2 NapCat / OneBot（S2 默认路径）

1. 用专用 QQ 账号登录 NTQQ，安装并启动 NapCat，开启 OneBot v11 WebSocket 与 token；确认账号风险与平台条款。
2. 启动 `shenko start --napcat ws://… --napcat-account <qq> --napcat-access-token <token> --napcat-admin <owner> --napcat-file-root <dir> --napcat-file-host <host>`；确认 `napcat-ingress.sqlite` 与 SNIG。
3. 人工发送文本 + 截图；记录入站、CAS、出站 `send_group_msg`/`send_private_msg` 与 echo。
4. 断开 NTQQ/NapCat 或网络，期间发送消息，恢复后确认 cursor/gap/replay。可选 `--napcat-cli-url` 做第二来源。

### 4.3 飞书 / iLink（S3，不阻塞 S2）

- **飞书：** 企业自建应用 → 开启机器人 → 添加 `im.message.receive_v1` 事件与 §2.3 的 scope → 创建版本并发布 → 管理员审核 → 设置可用范围 → 机器人入群并有发言权；长连接只支持企业自建应用，保存订阅方式时本地客户端必须已连接。先完成 L5/L6/L7。
- **iLink：** 准备可用的 iLink provider 与 `iLink-App-Id`/`client_version`，人工扫码（`bot_type=3`），取得 `bot_token`/`ilink_bot_id`/`ilink_user_id`；先完成 L8。只服务单聊，无 edit/retract/typing/群。

## 5. V1-01 / V1-10 关闭证据清单

### 5.1 V1-01（QQ 发起自身改进并附截图）

**合同：** [§35.10](../specs/self-update.md#spec-35-10)；[§35.7](../specs/self-update.md#spec-35-7)、[§35.8](../specs/self-update.md#spec-35-8) 的运行中交接/能力缺口；[§15.3](../specs/channel-adapters.md#spec-15-3)、[§16](../specs/channel-adapters.md#spec-16)。

**必须观察到的真实事实：**

1. 一个真实 QQ 用户（同一 `account_scope`）向 bot 发送一条改进请求 + 截图；记录原始事件 `id`、平台消息 id、会话 openid（脱敏）、时间、`t`/`seq`。
2. 截图被解析为附件 locator，取回字节并封印为 CAS；记录 `blake3`、MIME、size、原始 locator 的脱敏 provenance。
3. 主体只接纳一次（入站去重），找到既有 tmux 工作位；child 完成修改与验证；记录 seat/worktree/commit、验证结果。
4. 同一 QQ 会话收到准确进展与最终结果；记录 publication intent/attempt/receipt、平台 `message_id`、`msg_seq`、`accepted`；文本与 commit 可对照。
5. 不出现重复外发、伪造成功或把 `BlockedExternalError` 写成通过。

**证据文件建议：** `receipt.json`（case、candidate commit、平台、account scope、脱敏身份、时间、入站/CAS、outbound receipt、DB sha256、环境、结果）；截图与日志作为附件；secret 不进入。

### 5.2 V1-10（重复、断线缺口、丢回执）

**合同：** [§35.10](../specs/self-update.md#spec-35-10)；[§15.3](../specs/channel-adapters.md#spec-15-3)；[§36](../specs/durable-state.md#spec-36) 与 E02（[§29](../specs/verification.md#spec-29) 表内）；[§57.15](../specs/channels.md#spec-57-15) 不把平台窗口写成产品门。

1. **入站重复：** 同一平台 `msg_id` 被推送两次（平台重复或从真实原始帧重放）；证明只产生一个 inbox event / 一个 disposition，SNIG/ingress 不重复消费。
2. **断线缺口：** 断开 WebSocket/NapCat 或网络；期间人工发送消息；恢复后用同一 `session_id`/`seq` resume（QQ）或 provider cursor（iLink/NapCat）；证明 gap 事实、重放、无丢失、无重复。
3. **发送成功但回执丢失：** 通过本地受控代理转发一次真实发送，平台受理后丢弃 HTTP 响应；证明 operation 进入 `unknown`、**不自动重发**、随后按合同对账（查询/证据/acknowledged_unknown）；E02 的 unknown 不被改名为失败。
4. 记录每一步的 operation/intent/attempt/receipt、游标、DB/CAS 摘要、注入点与时间；把“未知”保留为未知。

### 5.3 RT19（能力表真实可达）

- 对每个声明 `true` 的能力（QQ：send/retract/media/markdown/proactive；NapCat：send/attachments/proactive；iLink：send/attachments/proactive；飞书：send/edit/retract/attachments/proactive）至少做一次真实调用或明确拒绝；不可达的从能力表删除/降级。
- QQ `edit:false`、`typing:false`、iLink 无 edit/retract/typing 是平台事实，保持 false 不算缺口；但 `guild_channel`、`attachments`、媒体发送路径必须真实可达，否则违反 [§57.4](../specs/channels.md#spec-57-4)/RT19。

## 6. 不能由本包证明的事

- 没有执行任何 build、test、clippy、fmt、原生运行、模型调用或平台请求；2.x 表中的“已实现”只指源码存在，不是运行通过。
- 没有真实 QQ/飞书/iLink/NapCat 账号；没有验证任何 token、gateway、WSS、附件 URL、频控或窗口。
- L1–L12 的候选改动没有编译/测试；不保证它们是完整修复，也不保证没有更多 live 阻断。
- 外部官方文档在本轮只作为平台事实来源读取（2026-11-22）；平台政策/页面会变化，实际以账号后台与真实回执为准。
- 本文不证明模型行为、不证明 S2 整体完成，也不把 `local_protocol` 提升为 `verified_real`。

## 7. 来源

**仓库合同：**

- [`docs/specs/channel-adapters.md`](../specs/channel-adapters.md)：§15.3、§16；§15.3 的消息插件合同、§16 的中国平台表与平台边界。
- [`docs/specs/channels.md`](../specs/channels.md)：§15.1、§52、§57.4、§57.15；[证据附录](../specs/channels.md#spec-channel-evidence-level)（无真实平台往返）。
- [`docs/specs/self-update.md`](../specs/self-update.md)：§35.7、§35.8、§35.10（V1-01、V1-10）。
- [`docs/specs/verification.md`](../specs/verification.md)：§29 阶段表（S1/S2/S3）、RT19。
- [`docs/specs/durable-state.md`](../specs/durable-state.md)：§36 与 E02。
- [`docs/governance/COMPLETION.md`](../governance/COMPLETION.md)：§1、§5（证据等级、blocked_external 不等于完成）。
- [`docs/evidence/qq-official-capability-audit-20261110.md`](../evidence/qq-official-capability-audit-20261110.md)：官方文档 vs 插件的可达动作对照；明确没有真实账号探测。

**基线源码：**

- `crates/shenko-channel-adapters/src/qq_official/{config,api,gateway,events,outbound,plugin}.rs`：QQ 官方 token/gateway/intents/分类/发送/能力表。
- `crates/shenko-channel-adapters/src/feishu/{auth,monitor,api,media}.rs`：飞书 token、长连接、发送、媒体。
- `crates/shenko-channel-adapters/src/ilink/{auth,monitor,api}.rs`：iLink QR、长轮询、发送。
- `bin/shenko/src/{lib,runtime_adapters,runtime_napcat,runtime_channels}.rs`：`--channels-config`、适配器注册、ingress 排空、NapCat 接线。
- `build/s2-sprint-20261118/e2e-suite/tests/seats_channels/test_blocked_external_im.py`：`BE-V1-01-01..04`、`BE-V1-10-01` 的显式 `BLOCKED_EXTERNAL`。
- `build/s2-sprint-20261118/evidence/im-local-protocol-r1/report.json`：13/13 `local_protocol`（不是真实平台）。
- `build/s2-sprint-20261118/sprint-tracker-body.md`：S2 checklist 原文（“Live IM awaits the official API”）。

**外部平台文档（2026-11-22 读取；只作平台事实，不是本仓库证明）：**

- QQ 事件订阅/Intents：<https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/interface-framework/event-emit.html>
- QQ 群消息（全量模式）：<https://bot.q.qq.com/wiki/develop/api-v2/autogen/event/group_message_create.html>
- QQ 发送群聊消息：<https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_messages.post.html>
- QQ 发送单聊消息：<https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_users_user_openid_messages.post.html>
- QQ 发送子频道消息：<https://bot.q.qq.com/wiki/develop/api-v2/server-inter/channel/message/send.html>
- QQ Bot FAQ（主体、公开添加、群主设置权限）：<https://bot.qq.com/bot_FAQ>
- 飞书接收消息事件与 scope：<https://open.feishu.cn/document/server-docs/im-v1/message/events/receive?lang=zh-CN>
- 飞书发送消息：<https://open.feishu.cn/document/server-docs/im-v1/message/create?lang=zh-CN>
- 飞书 `app_access_token`：<https://open.feishu.cn/document/server-docs/authentication-management/access-token/app_access_token_internal?lang=zh-CN>
- 飞书上传图片/文件：<https://open.feishu.cn/document/server-docs/im-v1/image/create?lang=zh-CN>、<https://open.feishu.cn/document/server-docs/im-v1/file/create?lang=zh-CN>
- 飞书长连接（企业自建、3 秒处理、集群单连接）：<https://open.feishu.cn/document/event-subscription-guide/callback-subscription/step-1-choose-a-subscription-mode/configure-callback-request-address?lang=zh-CN>
- 飞书 SDK 长连接实现（`/callback/ws/endpoint`、protobuf、ACK）：<https://github.com/larksuite/node-sdk/blob/main/ws-client/index.ts>；Rust 参考：<https://docs.rs/feishu-ws/latest/feishu_ws>
- iLink 协议来源（SPEC 引用；本轮抓取 404，未独立复核）：<https://github.com/Tencent/openclaw-weixin/blob/main/docs/ilink-bot-protocol.md>
