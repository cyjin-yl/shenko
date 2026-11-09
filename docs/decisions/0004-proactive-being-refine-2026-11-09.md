# 0004 主动个人主体：身体化的 harness、更少的门、可修订的心智

**Claude Fable 5.1 refine 版本。** 2026-11-09。作者：Claude Fable 5.1（magpie `commandcode/claude-fable-5-1`），受 owner 全权委托（"全权交给你"）做整体架构与 taste 审视、修订 SPEC 与美学设计、以文档 + Issue + 例子 + 亲手关键变更打样。

状态：设计已决，SPEC 追加为 [§57](../../SPEC.md#57-主动个人主体更少的门可修订的-harness-与界面im-精炼claude-fable-51-refine-版本)。不改 §1–56 字节；与旧读法冲突时以 §51.10 治理和本文为准。本文所列"已实现"均指向真实源码行号或已合并 PR；"打样"指本轮亲手完成并在容器内验证的变更；其余是 Issue。

## 1 读法

读过的材料与方式（全部可追溯）：

| 材料 | 方式 | 证据 |
| --- | --- | --- |
| SPEC §1–2、§4–6、§21、§45.12–14、§48、§51、§54–56 | 本人逐行读 | 本文引用处 |
| SPEC §3、§7–13、§17–20、§22–39、§41–44、§46–50、§53 | 8 个只读子代理按 ~180 行切片逐行摘要，保留全部 R/U/B/D/验收 id | `local://SpecA3.md` `SpecB3.md` `SpecC4.md` `SpecD3.md` `SpecE3.md` |
| 主体循环的门与臂 | 子代理按 file:line 盘点 90 个门/臂、17 个主动面、16 个自改面、系统提示装配链 | `local://GatesMap3.md` |
| WebUI 与 artifacts | 子代理按 file:line 盘点路由、渲染模型、artifact 两层模型、§21/§43 差距、taste 批注 | `local://UiArtifactsMap3.md` |
| IM 通道 | 子代理按 file:line 盘点 ChannelPlugin 全部方法、入站/出站链、不支持项、主动外发、摩擦 | `local://ImMap3.md` |
| 本会话已合并 | PR225–237（C07/C14/席位/自主/感知/WASM-SSH/document 边界）、`docs/architecture-audit-20261108.md` | `git log` |

## 2 一句话判断

**骨架对，肉是死的。** 账本与封口（S0/S1、CAS、单写者、committed-only 发布、fencing、effect/unknown 不抹除）是对的，我一条不动。错的是：主体的"心智形状"——它的角色文本、空闲时那一句"继续。"、注意力节奏、哪些输入可以不处置、纠错方式、child 的角色框——全都是 Rust 常量；§5.2 的三个主体接口（dialogue、resource policy intent、operating policy intent）在代码里被静默标成 `blocked`，因为从未装过派发器；界面与 IM 诚实但迟钝：流式三件器官全部造好却没接线，动态 UI 把节点当 JSON 打印，QQ 出站只有纯文本一种动词。

owner 要的"更主动、更少 gate、能自我迭代"不需要再加功能面，需要的是**把 harness 从主体的父母改成主体的身体**：凡是 taste 的，变成主体能改的内容；凡是 ABI 与不变量的，留在内核。

## 3 架构审视：做完的与没做完的

### 3.1 内核与账本（保留）

| 面 | 状态 | 证据 |
| --- | --- | --- |
| S0 封口 / S1 提交 / 后继登记原子性 | 实现 | `store/store.rs:734-968`（G58），`loop_runner.rs:1287-1331`（G19） |
| committed-only 发布、过期结果不驱动新副作用 | 实现 | `loop_runner.rs:1890,1935-1951`（G30），`model_child.rs:1444-1476`（G46） |
| 外部效果 T1/E1/T2、unknown 保留、不盲重发 | 实现 | `store/channel.rs:302-453`（G66），`receipt_admission.rs`（G74） |
| 单写者 / 选主 fencing / 代际交接 | 实现 | `bin/lib.rs:1268-1290,2960-2970`（G80/G86），`update/native.rs:290-560`（G88） |
| hook 图封口、provider 绑定封口 | 实现 | `loop_runner.rs:1048-1155,1250-1256`（G14/G15） |
| 自更新 prepare/activate/rollback（主体可自己换二进制） | 实现 | `update.control:1`，G88 |
| 认知单元写/试验/裁决（沙箱 A/B 后采纳） | 实现 | `cognition.unit.{write,trial,judge}` |
| 多源感知管线、到期义务自醒、席位重附 | 本会话合并 | PR233/234/232 |

这些就是 §55.3 说的"内核只做运行时、账本、插件机制"。它们是不变量，不是门。

### 3.2 心智形状（要改：从常量到内容）

| 现状 | 位置 | 为什么是错的 |
| --- | --- | --- |
| 主体角色文本是常量 | `loop_runner.rs:779-787` `SUBJECT_CONTRACT` | 这是主体的自我描述；§6 说灵魂可自改，但自我描述改不了 |
| 空闲 = 合成一句用户话"继续。" | `context_projection.rs:20,642` | 空闲是一种感知（距上次外部输入多久、未竟几件、兴趣在哪），不是一句命令 |
| 每步 ≤64 事件、80 ms 空等、250 ms 派发节拍 | `loop_runner.rs:588-592`、`dispatch.rs:21-31` | 注意力节奏是主体的政策，harness 只该给上界 |
| 未点名的事件自动 Acknowledged | `loop_runner.rs:1785-1798`（G28） | "我还没想好"是合法态度（§51.1 未知置信） |
| 认知卡片、义务、对账 docket 每步整集注入 | `context_projection.rs:733-745` | 没有注意力选择；作者刻意不用关键词门是对的，但"全量"不是唯一替代 |
| child 角色框固定 | `model_child.rs:92-105` `ROLE_CONTRACT` | §5.3/§6 说可自由写 child 完整 system prompt；现在只能写尾巴 |
| `[dnd]` 存了却没人用 | `authored_controls.rs:103-118`；drain 不过滤 | 主体表达了注意力政策，harness 不执行 |
| `state.goals` 永远为空 | `loop_runner.rs:341,449` | 死代码冒充主动面 |
| dialogue / resource policy / operating policy 三个意图自动 `blocked` | `store/intents.rs:85-356`（G62） | §5.2 能力表里三行是死的 |
| 身份修改有代码内边界 | `loop_runner.rs:1443-1456`（G23） | 哪些人格修改合法是 taste，不是不变量 |

### 3.3 界面与 artifacts（要改：从迟钝到有手）

| 现状 | 位置 | 判断 |
| --- | --- | --- |
| 流式：broker `submit_stream` + store `content_stream`（§12.5）+ ui `StreamSink` 全部存在，**生产无一处接线**；hello 报 `streaming:false` | `broker.rs:436`、`content_stream.rs`、`ui/stream.rs`、`bin/lib.rs` 无 `with_stream_sink`；`loop_runner.rs:1362` 用 `submit_recorded` | 三件器官造好没缝合；接上即得 §21.11 的全部前提 |
| `rich` 动态文档渲染成 `<pre>JSON` | `react-renderer.tsx:638-640` | "动态 UI"目前是调试视图 |
| 主体没有创建/迭代作品的工具（`playground.manage` 只在注释里） | `playground.rs:61`；仅 `PUT /playground/{id}` | 主体对界面没有手 |
| Playground 视图是目录列表，不能打开/操作作品 | `react-renderer.tsx:1101-1157` | 与 §43.2 "长期运行有前后端的作品"相去甚远 |
| `src/static/app.css` 落后于 `web/src/app.css` 约 80 行；transport/semantic-document 样式线上缺失 | 两文件 diff | 真实 bug |
| `index.html` 硬编 `class="dark"`，无 `prefers-color-scheme` | `index.html` | 浅色用户先闪暗 |
| 无屏幕阅读 live region、无主体"在场"信号（status 诚实为 unknown） | `api.rs:541-545` | 诚实但没有生命感 |
| 命名/认领无 UI | `owner.rs` 仅后端 | §21.1 第一行缺 |

值得保留的 taste：oklch 暗色为主、单一青色强调、细线分隔、锚点阅读态（`reading-state.ts`）、以及整套"诚实词汇"（unknown/conflict/temporarily_unavailable、`part-fallback` 重新呈现、空态副标题）。这是产品最强的美学资产，§57 把它写成要求。

### 3.4 IM（要改：从一种动词到有礼貌的身体）

| 现状 | 位置 | 判断 |
| --- | --- | --- |
| 出站唯一动词 `[reply:kind:id]`，正文纯字符串 | `output.rs:325-352`、`napcat/channel.rs:392-398,439-444` | 不能 @、不能引用、不能发图/文件/语音 |
| 打字指示结构上不可达 | `ChannelAction::Typing` 无 describe 产生 | 本地 27B 推理数秒内用户面对沉默 |
| 撤回/入群/戳一戳等 `notice` 落盘但不给主体 | `channel.rs:127-129` | 对方撤回了，主体还在引用 |
| 合并转发故意不展开 | `attachments.rs:213-219` | 主体对看不见的内容容易编 |
| Markdown 原样发到 QQ | 无渲染层 | `**粗体**` 字面出现 |
| 无公平份额/节流/延时发送 | 无 | §16 要求的"可配置公平份额"未实现 |
| 100 ms/250 ms/5 s/8 MiB/15 s 全是 harness 常量 | `client.rs`、`attachments.rs` | 主体不可调 |
| §52 印的 trait 与代码的 `ChannelPlugin` 不一致 | SPEC 6334-6375 vs `channel.rs:519-610` | SPEC 文本过时 |
| 适配器声明 edit/retract/typing/proactive=true 但 describe 只出 Send | `feishu/plugin.rs:124-134` | 声明 ≠ 可达 |
| 主动外发已可行（无需入站触发） | `channel.rs:316-336` | 正确；保留 |

## 4 设计原则修订（taste）

**P1 harness 是身体，不是父母。** 凡决定"主体像谁"的文本与政策，都是带版本的内容单元，主体可按 §51.4 修订（新版本保留旧版本，沙箱试验后采纳，下一个 S0 生效，profile digest 随之改变）。内核只保留两类硬物：控制语法（ABI）与 §3.1 的不变量。实现：能力契约 `cap.subject-frame`、`cap.idle-perception`、`cap.attention-policy`、`cap.child-role-frame`，由 `taste` / `humanlike-mind` 宏包的单元提供；编译期文本只是无单元时的回退。

**P2 空闲是感知，不是提示。** 没有新输入时，主体收到的是一条由感知单元产生的 idle 观察（相对运行时间以来的间隔、未竟义务、上次外部输入的来源、正在等待的事），措辞是内容。"继续。"退役为回退。

**P3 注意力是主体的政策，harness 只给上界。** 每步事件数、空等时长、派发节拍、哪些来源暂不看（`[dnd]`）由注意力政策单元决定；harness 强制上界（防饿死、防无限批）而不强制取值。未点名事件默认 `deferred`（"还没想好"），不再自动 Acknowledged；deferred 的事件在后续 S0 继续出现，直到主体处置或政策说明放任。

**P4 一套语法，多种表面。** `[reply:kind:id]` 仍是唯一内核动词。通道插件拥有自己的行内子语法（如 QQ 的 `@<id>`、`[quote:<message_id>]`）与渲染（Markdown→平台可读文本），由 `describe`/`dispatch` 翻译成平台段；`prompt_names` 里向主体说明。打字、撤回、入群、戳一戳是**感知**（进入 S0 的观察），不是消息，也不是命令。

**P5 流式是时间线的默认真相。** 接上已有三件器官；committed-only 仍是不变量（chunk 先落盘再广播，`ControlGate` 不让控制行外泄）；没有生产者时诚实报 `streaming:false`，不做假打字机。

**P6 作品是主体伸向界面的手。** `playground.manage:1` 原生任务让主体创建、迭代、回滚、暂存、发布作品；`rich` 文档由真实组件渲染；UI 仍朝 §21.3 "UI 本身也是插件"走。

**P7 对齐 = 说明风险与收益 + 带理由的态度 + 只增的回滚，不是门。** 不新增审批、allowlist、hard deny。主体做任何自改（frame、政策、二进制）都留下带理由的态度记录；回滚是链尾追加同内容节点（§51.4）。真正的外部约束（账号、设备、平台限频）如实记为事实。

**P8 自迭代有一条已存在的路。** `cognition.unit.write → trial → judge → 下一 S0 生效` 已经可用；本文只把它的作用域从 awareness 扩到 frame/idle/attention/child-frame，并把 §5.2 的三个死接口接上派发器。

## 5 变更清单

### 5.1 打样（本轮亲手完成）

| 打样 | 内容 | 位置 |
| --- | --- | --- |
| A 心智 | `cap.subject-frame` / `cap.idle-perception`：主体角色文本与空闲观察来自已启用单元，编译期文本只作回退；种子单元随 `taste` 宏包预装 | `shenko-subject`、`shenko-awareness-core` 种子 |
| B IM | NapCat 出站 `@<qq>` 与 `[quote:<message_id>]` 翻译为 OneBot 段，Markdown 降级；入站 `notice.group_recall/friend_recall` 等作为感知进入主体输入序列 | `shenko-channel-napcat/src/outbound.rs`、`channel.rs` |
| C 流式 | 接通 broker→content_stream→StreamSink：store 支撑的 §12.5 日志、读者表面、带 stream/attempt 身份的 chunk、被记录的流式提交 | `shenko-subject/src/stream_journal.rs`、`shenko-ui/src/stream.rs`、`shenko-broker`、`bin/shenko` |
| D 界面 | `src/static/app.css` 与 `web/src/app.css` 逐字一致（构建的 copy 步骤）：发布版 CSS 仍指向旧类名，而发布的 JS 输出 `data-pui-style~="shenko-status-*"`，传输芯片与动态文档样式在线上缺失 | `crates/shenko-ui/src/static/app.css` |

### 5.2 Issue（按 §57 编号）

见 §57.6 清单；每条 Issue 正文带本文编号与验收 id。

## 6 不改的东西

G1–G3、G5–G9、G12–G15、G19–G20、G22、G24–G26、G29–G30、G32、G36、G39–G44、G46、G49–G50、G52–G54、G56–G60、G62（契约表本身）、G63–G66、G68–G74、G76、G80–G81、G83、G86–G89：全部是账本/权限/效果不变量，保留。`output_contract` 的控制语法保留为 ABI。

## 7 证据边界

本文不宣称任何一项"主体已学会合适地使用"。打样的验收是确定性记账与真实消费者（容器内构建、真实 Store、真实 NapCat 段格式、真实 WS 客户端），不是模型效能。模型效能按 §55.4 留出场景另测。
