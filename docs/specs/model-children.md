# 模型 child：有限、多步、可调用真实能力的子 Agent

**内核定位。** 参考插件：能力架上的委派执行者，建立在[内核合同](kernel.md)之上。委派是带回执的效果（[内核 5](kernel.md#kernel-5)）；child 与主体共用推理容量而互不饿死（[内核 2](kernel.md#kernel-2)）；经[内核 7](kernel.md#kernel-7)登记。child 的角色框是可修订的单元内容（[内核 9](kernel.md#kernel-9)）。


<a id="why-2"></a>
<!-- migrated-contract: WHY:2 -->
## 为什么只有一条主思考流，却鼓励很多孩子

主流承载持续身份和决定顺序；children 承载需要文件、进程、版本分支、研究试探和实际记忆维护的工作。这样可以同时保有连续的人格与廉价、多路线的行动能力。主子可以交谈、共享可发现的能力、使用不同模型和 prompt；角色不同不意味着孩子必须是功能残缺的模型。见 [§5](subject-runtime.md#spec-5)–[§6](subject-runtime.md#spec-6)、[§17](remote-seats.md#spec-17)–[§18](plugins.md#spec-18)。

主流不直接操作任意文件，并非为了让它无能。它通过语义记忆、委派与自解释理解自己，通过 children 完成可跟踪的效果。实际操作属于任务，就能被版本化、核验、恢复和继续。开发 Shenko 的外部 Claude 是开发者角色，不能把产品主流的窄工具约束误套成“开发 Agent 不准改仓库”。

<!-- /migrated-contract: WHY:2 -->

适用范围：[§5.3](model-children.md#spec-5-3)（委派与对话）、[§5.5](model-children.md#spec-5-5)（主体—child 双向协作）、[§6](subject-runtime.md#spec-6)（child 的独立
system prompt / 模型 / 上下文 / 工具 / 输出预算）、[§14.5](model-children.md#spec-14-5)（递归收敛：父预算账户、去重）、
[§41.5](subject-runtime.md#spec-41-5)（有限子任务）、R106/R107。

实现位置：`crates/shenko-subject/src/model_child.rs`（运行时）、
`crates/shenko-subject/src/child.rs`（共享 native runtime）、
`crates/shenko-kernel/src/task.rs`（任务处理器与工具合同）、
`crates/shenko-store/src/{external,intents,child}.rs`（账本与准入）、
`crates/shenko-types/src/output.rs`（作者可见的 spec 类型）。

本文描述合同与接口；已执行的机制验证及未通过的边界见 [§9](self-update.md#spec-9)。脚本 provider 配合真实
CLI、Store、broker 的验证不等于真实模型行为验收，也不覆盖未执行的恢复场景。

---

## 1. 一个模型 child 是什么

模型 child 是**一次有界的 agent 运行**，不是"带 `tools=[]` 的单次生成"：

| 事实 | 归属 | 持久位置 |
| --- | --- | --- |
| 冻结的 child 合同（契约 schema 3；历史 schema 2 的读取见 [§6](subject-runtime.md#spec-6)） | 委派者写、运行器绑定 | `subject_intents` 的 delegation 行 + CAS 对象 |
| 每个模型步（一次真实 generation） | child 自己的 operation/attempt/evidence | `operations` / `attempts` / `evidence_receipts`，operation 名 `model.child.generate.v1` |
| 每次工具调用 | 一个**真实的** native 子任务 | `tasks`（kind = 工具 kind，owner = child 任务） |
| 运行结果（transcript、答案、预算、终止原因） | child 的 run operation | operation 名 `model.child.result.v1` + 其 normalized 记录 |

child 与主体共用同一套 broker、同一 quota gate、同一 native handler 注册表、同一
provider 通路。child 不新增第二个 registry、第二个循环或第二套账本。

主体的 native 能力说明由 `NativeChildService::installed_prompt_sections()` 生成：每条可见
说明先给出描述符中真实的 `[delegate:<kind>:<definition_version>]`，再给出该 handler 的
用法。安装单元 ID 不能代替调用定义；例如 `compat.mcp` 单元提供的调用是
`compat.mcp.request:1`，`compat.skill` 单元提供的是 `compat.skill.source:2`。
主体的 S0 随请求封存这份实际说明；说明不替模型选择工具，也不改写已经创建的任务。

### 1.1 有限性

child 运行永远有界，三条预算同时生效（`ModelChildBudget`）：

* `max_steps`：本运行可取的模型步数；
* `max_tool_calls`：本运行可发起的工具调用数；
* `max_total_tokens`：本运行可预留的输出 token 总量。

未显式给出预算时，默认由 child 自己的每步输出预算推导（`max_steps = 8`）。
显式预算采用作者给出的有限整数，不额外限制为 32 步或 32 次工具调用；嵌套运行仍受
父任务剩余额度和实际资源准入约束。child 不会变成第二条无限主动循环。

---

## 2. 作者可见的选择（`ModelChildSpec`）

主体用 `[delegate] {json}` 写一个 child。字段：

| 字段 | 含义 | 约束 |
| --- | --- | --- |
| `goal` | 任务目标 | 非空，≤64 KiB |
| `system_prompt` | **委派者独立撰写的完整语义 prompt** | 非空，≤64 KiB |
| `model` | 本次运行选择的模型 | 非空，≤512，无控制字符 |
| `max_tokens` | **本 child 每步的输出预算** | >0，不超过本 child 的总预算；实际发送仍须通过资源准入 |
| `tools` | 本 child 被授予的工具（`{kind, definition_version}`） | 可空；空 = 纯文本 child |
| `context` | 交给 child 的上下文块（`{label, text}`） | 可空 |
| `budget` | 本运行的有限预算 | 可空；缺省 = 由 `max_tokens` 推导 |
| `reply_grant` | 外发授权（目标、消息类型、有效期） | 可空；缺省 = 该 child 完全不能外发 |
| `continue_task_id` | 继续先前某次 child 运行 | 可空；见 [§6](subject-runtime.md#spec-6) |
| `owner_answer` | owner 对该 child 提问的回答 | 与 `continue_task_id` 同时出现 |

**输出预算不是主体单步上限的函数。** 主体某一步的 `max_tokens`（S0 payload 的
输出预算）不限制 child 的每步预算；`default_max_tokens` 只是未指定时的默认值，不是
硬上限。child 自己的预算、父任务剩余额度及实际 provider/资源约束分别保留。

child 的每步预算、工具选择、上下文与语义 prompt 全部**独立于主体**：child 不会
机械继承主体的 prompt、性格或模型。

---

## 3. 冻结合同与指纹

`FrozenModelChild`（当前契约 schema 3）是 child 的冻结身份，内容包括：

* `task_id`、`created_by_step_id`、`software_generation`、`role_revision`；
* `spec`（作者写的全部选择）、`provider_binding`（transport + 选定 model 的确切绑定）；
* `budget`、`max_tokens`；
* `reply_grant`（已绑定到本 child 的 `task_id`）；
* `continued_from`（继续链上的前一个 child 任务）；
* `prompt_fingerprint`（语义 prompt 的 blake3）、`toolset_fingerprint`（工具选择的 blake3）。

`Store::validate_model_child_intent` 在 S1 提交时逐项核对这份合同与主体当次 S0 的
provider 绑定、决定内容、预算提示一致；不一致即拒绝该委派（`ShenkoError::conflict`）。
运行器在执行前再次用 `FrozenModelChild::validate` 重建并全等比较。

该准入同时读取主体的 **FrozenStepInput plan schema**（当前 = 12，与 Subject 的统一切换
一起移动）。child 合同必须绑定到同一个 plan schema；用旧 plan 封存的 child
委派会被拒绝，不会被"宽容读取"。

每个模型步另有自己的冻结输入 `FrozenModelChildStep`（当前 step schema 2；历史读取见 [§7.1](plugins.md#spec-7)），它携带
`contract_digest`（= 冻结合同的 digest）、`request_id`、该步的 `provider_binding`、
`max_tokens` 和**实际发出的 payload** 与其 `payload_fingerprint`。账本上每个 step
operation 的 `final_input_ref` 指向它，`input_digest == final_input_ref.digest`。

---

## 4. 工具调用协议

共享 provider wire 目前**不承载** provider 原生 tool block，因此 child 的工具协议是
产品自己的控制行：

```
[tool:<kind>:<version>] {json}
```

一次输出可以有多行工具调用，按出现顺序执行。解析失败（版本不是正整数、JSON 不合法、
kind 为空）的行**不**被当作工具调用，而是留在答案正文里。

* 只有 `spec.tools` 里列出的 `(kind, definition_version)` 才被允许；
* 只有 **registry 里真实注册**的 handler 才会出现在给 child 的工具目录里
  （`NativeChildService::describe_tools`：作者写下的工具名不等于能力）；
* 未授权的工具 → 结果 `not_allowed`；未注册的工具 → 结果 `unavailable`；
  handler 失败 → 结果 `failed`；结果尚未落地 → `unknown`。
  这些都以事实形式回灌给模型，不伪装成成功。

工具调用执行方式：child 通过共享 native runtime（`ChildToolRuntime`）请求一次
**真实的** native 子任务。该子任务有自己的 task 行（owner = child 任务）、operation、
attempt、evidence 与 T2，走的是主体委派时完全相同的 handler、authority 与账本。
子任务 id 由调用身份派生（`tool_task_id(call_id)`）。不存在的子任务先插入，再在
同一事务中认领；认领失败时创建也回滚，不留下可被另一 dispatcher 抢走的中间任务。
已有调用身份不再新建任务：重启、恢复或继续讨论时读取、协调原任务的持久结果，
未知效果不能被当作一次新调用。

### 4.1 嵌套的模型 child

`kind = "model.child"` 的工具调用是一个**真实的嵌套 child 运行**：它有自己的冻结合同、
自己的模型步、自己的 transcript，并作为外层 child 的持久子任务存在。

* 嵌套 child 只能花费外层 child **剩余**的预算（`budget.max_total_tokens` 不得超过
  外层剩余额度）。递归**不增加**总预算（[§14.5](model-children.md#spec-14-5)）。
* 嵌套 child **没有**自己的外发授权：外层 child 持有被授予的 `reply_grant`，
  一个逻辑回复只有一个负责发布者。
* 嵌套 child 的每步输出预算不因 transport 默认值而缩小，仍受自己的总预算和实际资源约束。

---

## 5. 双向对话

* **child → owner（问题）**：child 的输出可以带 `[ask] <问题>`。该步结束时运行以
  `termination = needs_decision` 收束（这是一个**结论性**的生成边界，运行本身成功），
  问题、答案正文与 transcript 一起写进 child 的持久结果。owner 在下一步的 context
  里看到该 child 的 T2 结果。
* **owner → child（回答）**：owner 用一个新的 `[delegate]`，带上
  `continue_task_id`（先前那个已完成的 child 任务）与 `owner_answer`。
  这次运行**继承**前一次运行的 transcript（读的是前一次运行的持久结果对象），
  从前一次的下一个步号继续，**不重新发起孩子，也不重复执行已完成的工具调用**
  （继承的记录里带着原调用身份，工具结果按原身份读取）。
* 继续链的预算约束：继续运行的 `budget.max_total_tokens` 不得超过被继续运行
  **剩余**的额度；`budget.max_tool_calls` 不得超过其剩余的工具调用额度。
  超出即在 S1 准入阶段被拒绝。

---

## 6. 外发：回复授权走通用渠道服务

child 的 `[reply] 正文` 只有在它**确实被授予** `reply_grant` 时才会外发：

* 授权未过期；
* 正文非空。

**目标由授权固定**：child 不能选择、收窄或改投目标 —— 冻结合同里存的是授权解析出的
那一个 `PublicationTarget`（`reply_target`），发布时原样使用。child 的回复行里没有
目标字段，因此 child 与渠道侧的 `PublicationTarget` 结构演进解耦。

没有授权就不发布（不降级、不改投其它目标）。

授权的账号绑定：账号命名空间属于**目标**。`PublicationTarget.account` 是拥有该会话
的连接器账号；`ModelChildReplyGrant.account` 可以显式给出同一个账号，缺省（或空串）
时用目标自己的账号。audience kind（`group`/`private`/`feishu-chat` …）只是"这个平台
id 属于哪一类受众"，**永远不是账号**；把它当账号会让授权记录与实际投递账号不一致。
显式账号与目标账号同时存在且不相等时，授权在冻结阶段被拒绝，不靠猜测定夺。

授权里的目标是**版本化**的：`schema_version` 固定 2，`id` 是平台不透明字符串，`account`
是账号命名空间（未知时留空字符串）。主体看到的委派语法给出的就是这个形状；未版本化
或数字 id 的目标在通道侧根本无法绑定，因此不再作为可用写法出现在语法里。

冻结合同版本随之升级：`FrozenModelChild` 的 schema 2 → 3。schema 2 的历史合同保留
它自己的旧账号推导（audience kind），校验时不会被新语义改写；schema 3 的合同记录
账号命名空间。Store 的准入同时认识两个版本，并按封存时的版本核对授权。
这不是所有历史运行都能在新二进制接续的证明：恢复还依赖被冻结的 handler artifact
可用，重新构造 schema 2 再校验的测试不能替代真实旧记录的恢复观察。

发布复用主体的 native publication 任务链：`Store::admit_child_publication` 写入
与主体 S1 发布同构的 `subject_intents`（`kind='publication'`，state=`ready`）与
`subject_publication` 任务行，已安装的渠道插件因此用同一个 service、同一套
prepared/dispatched/confirmed/failed/unknown 状态与回执处理它。发布任务挂在主体的
owner 链上（不是 child 任务上），因此 child 自己的完成**不会**被一次远端发送拖住。

child 回复与主体回复共用同一条通用渠道发布链（同一 `subject_intents`
/`subject_publication` 行、`CHANNEL_PUBLISH_CONTRACT`、同一组 prepared/dispatched/
confirmed/failed/unknown 状态），child 侧不再残留 `shenko.napcat/onebot11-send/1`
这样的写死合同字符串。

### 原生证据（2026-11-03）

默认 CLI 的真实进程 + 真实 NapCat/OneBot 插件 + 本地 OneBot WebSocket 双工 +
脚本 provider：主体委派一个带 `reply_grant` 的 child（目标
`{schema_version:2, channel:qq, kind:group, id:"123456789", account:"qq:10001"}`），
child 回 `[reply] …`，CLI 自己的发布循环经插件 `describe`/`dispatch` 把消息发出。

观察到的事实：

* 平台双工收到**一次** `send_group_msg`，`group_id=123456789`，正文就是 child 的
  `[reply]` 原文；
* 持久层里的发布行 `contract=shenko.channel/publish/2`，目标 `schema_version=2`，
  `account=qq:10001`（**不是** audience kind `group`），并带 `reply_grant_id`；
* 该发布的 E1 回执 `observation_level=provider_accepted`；
* 输出中没有任何凭据样式，provider 无错误。

边界：平台侧是**本地 OneBot 双工**，不是真实 QQ 平台；provider 是脚本，不是真实
模型；不含崩溃/掉电耐久性；也**没有**证明真实模型会照着委派语法写出可绑定的
`reply_grant`。因此本条只证明"授权→发布→插件→回执"这条链路本身，不等于通用渠道
外发的完整验收。

证据根：`build/spec-architecture-20261101/child-channel-r{3,5,6,7}/`、
`child-channel-publication-assessment.json`。

早期实测曾暴露正常关机被误报为 `channel closed` 的缺陷；把所有 closed send 都
吞掉虽通过当时的关机对照，却掩盖了运行中的接收者丢失。公开的 `Inbox::close()`
在持有 `Arc` 时也能关闭接收端，持久结果存在不等于投递成功。

当前生命周期区分**主体停止接收**与**持久消费者停止**。CLI 先发出主体停止信号，
再关闭 inbox，等主体到达可恢复结算边界后才停止消费者。dispatcher 只在主体或
自身已开始停止时正常结束投递；其他 closed send 返回原始通道错误并写入现有
`native_dispatcher` journal，结果及 E1 仍留在 Store。NapCat 入站同样区分预期
停止与意外关闭，后者走现有 `napcat_admission_failed` journal，spool 不因此 ACK。
直接使用 `run_committed_dispatcher` 的调用者须分别传入主体停止与消费者停止信号；
两者确实同生命周期时才复用同一个 token。

有限验证（2026-11-04）：扩充既有真实 `printf` 子进程/重开消费者回归，修复前
关闭仍被持有的 inbox 后没有返回错误、没有 journal；修复后返回类型化的 send
错误并留下恰一条 journal。仅停止主体接收时，另一个真实 `printf` 仍能由消费者
执行和结算，不新增错误；新 inbox 收到两个持久结果，ACK 后重开保持原 E1 和
journal。`dispatch-lifetime-verification-after-r1` 还通过主体库 63 项及 CLI 构建；
数量变化来自删除旧错误预期和纯转发的两个小测试，不是过滤失败用例。

同一保留 CLI 的 `dispatch-lifetime-channel-native-r1` 收到一次获授目标的
`send_group_msg`、一条带 child grant 的发布及 `provider_accepted` 回执，正常
退出且没有 runtime error；拒绝与截断两个默认 CLI 场景也正常退出、无重复 child
请求或失败输出的发布效果。回执根为 `build/spec-architecture-20261101/`，独立
只读账本投影为 `dispatch-lifetime-native-before-after-facts.json`。平台仍是本地
OneBot 双工，provider 仍是脚本；不是实际 QQ、真实模型或掉电验收，也不代表已
穷尽 NapCat 入站关闭的所有交错。

---

## 7. 错误、取消与恢复

| 情况 | 行为 |
| --- | --- |
| 模型步结果未知（provider 未给终态证据） | 该步 operation 停在 `reconciling`；child **不再发第二次**，整体以"未决 claim"返回，dispatcher 把 child 停在 `reconciling`，绝不伪造结果 |
| 恢复（进程重启） | `ModelChildHandler::new` 在 broker 启动前扫描所有 `model.child.generate.v1` operation，为每个未决请求注册原始读取器（`register_recovered`），债务按账本重建，不重发 |
| 取消 | 取消 child 会 fence 它的工具子任务（owner 链）与其下一次发送（`may_dispatch` 的 durable fence）；**晚到的 E1 仍然被记录**（E1 没有 T2 权限），但不会推进已取消的任务 |
| 配额 | 每一步都走共享 quota gate；step 的配额预留只属于那一步 |
| 重复身份 | 同一 `(task_id, step_index)` 的 operation 与同一 `call_id` 的工具子任务都是幂等读取，不是第二次执行 |

**成功的含义**：child 的成功只证明"该定义的 generation 到达了终态边界"，不证明自然
语言目标已达成。截断、拒绝、Unknown 分开记录（`ModelChildStepRecord.complete` 与
`stop_reason`）。

**失败传播**：沿用 generation 已有的接受条件：状态为成功、存在 provider 响应、
终止类型为 `provider_terminal` 且 stop reason 为 `stop`/`end_turn`/`stop_sequence`。
不满足时 `ModelChildStepRecord.complete = false`；正文空字符串本身不是新增拒绝条件。
运行循环不执行该步输出的工具行或 `[reply]`，也不把它当空答案：run 以
`termination = generation_failed` 收束，run-result operation 记 `Failed`，原 step 的
outcome 与其原始 E1 保持不变。修复前 run 可能会被误记为 `Completed`/成功；
真实试验 `child-channel-live-r3` 中主体使用 Coder390，但 child 请求了未服务的模型，
原始 provider 回执为 HTTP 404，
因此留下 `model.child.generate.v1 = failed` 而 `model.child.result.v1 = succeeded`
的配对，见 `child-provider-terminal-facts-20261104.json`。旧记录不改写，新代码按
原样读取。E1 已落盘但 run-result 尚未生成时，恢复按已有失败步收束，不发送下一步。
完整 SPEC 验收、真实模型行为与所有恢复组合仍需分别验证。

有限验证（2026-11-04）：`subject-controls-verification-r6` 的 65 项主体库回归通过，
包括真实 broker 已落 E1、handler 不再被 poll、run-result 尚不存在时替换服务与
重开 Store 的恢复用例；同一原始 step/receipt/E1 保留，没有第二次 provider 请求。
同一保留 CLI 二进制分别运行脚本 HTTP 404 和 HTTP 200/`length` 场景，两者都是
一次 child 请求、step/run 失败、`generation_failed`、零发布意图和零工具 operation。
截断正文仍保留其解析出的工具行，但没有执行；解析计数不是执行回执。
对应回执位于 `build/spec-architecture-20261101/` 下
`child-generation-rejected-after-r1/receipt.json` 与
`child-generation-truncated-after-r1/receipt.json`，均正常退出且进程组消失。
旧二进制截断对照留下八个失败 generation 和一条带 child grant 的发布意图，最终
run 因预算耗尽失败；这与上述 HTTP 404 的错误成功是两份不同证据，不代表消息已
送到真实平台。这里没有真实模型、真实 IM 平台或物理掉电验收。

同一个 invocation 的 T1 与发送记录会推进任务的 `record_revision`。下一模型步、工具
调用或最终 run-result 操作必须读取新的记录版本，不能继续用最初 claim 的旧版本。
刷新只更新记录版本，不采用新任务或新权限：任务版本、输入、实现、invocation id、epoch
必须保持一致，已取消或已替换的运行拒绝继续。Store 在下一次 T1 内仍原子检查 CAS、
owner、controller generation 与可执行状态；晚到的模型 E1 不因此获得 T2 权限。

发送边界按 **operation/attempt** 而不是整个 task 记账。
`Store::commit_native_dispatch(ctx, operation_id, attempt_id)` 在同一事务中核对持久
operation 的任务归属、attempt 的 operation/invocation/epoch 和当前执行权限。一个
attempt 只有一个调用者能获得发送权；同一 invocation 的下一模型步使用自己的
operation/attempt，不会被前一步的发送记录误判为重发。

task memo 中 schema 2 的发送记录保留每个 attempt 的绑定和提交位置，不删除前一步
记录，也不伪造 provider 已接收的事实。已经提交的 attempt 即使随后被取消，重复
回调仍按“可能已发送”处理；尚未提交的 attempt 被取消后不得发送。旧的任务级
发送标记不能归属到某个 attempt 时保留未知状态，不能以字段缺失推断“从未发送”。

---

### 7.1 准备材料的作用域、单次消费与恢复

child 与主体共用同一个准备材料注册表，但**收集作用域不同**：child 只收集自己的
任务作用域 `{ scope_id: task_id, task_id: Some(task_id) }`，主体只收集自己的分支级
作用域 `{ scope_id: branch_id, task_id: None }`。主体的范围级收集不会取走 child 的
任务材料，child 也不会取走主体的材料。

准备材料在**请求封装里**随 step 一起封存（`model-child-step-input` schema 2），
不进入由工具结果重建的滚动历史；模型看到的是一条 `[prepared contributions]` 的
用户消息。schema 1 的历史 step 没有这份计划，仍然可读；schema 2 缺失或为 null
的计划一律拒绝，不能把一次请求降级成“这次没有使用任何材料”。

单次消费以 **T1 被接受** 为准：只有请求真正被接受之后，child 才把这次封口的用量
登记到注册表；仅写完 CAS 对象不算消费。注册表实例被替换（插件重启）时，child 从
live 服务重新解析注册表，并遍历当前 Store 中全部已接受的 model-child step
operation，从各自封口输入重放用量，包括已完成或取消任务的旧步骤，而不是只读
当前 child 或最新一步。

Store 在 T1 准入时复核这份计划：计划里的每一项都必须属于该 child 的任务作用域、
声明 token 不得超过自身预算、合计 token 与计划一致，且每个材料正文的 CAS 闭包
可读。范围、token 或闭包不符即拒绝准入，不做静默裁剪。

---

## 8. 接口清单

新增/变更的公开面：

* `shenko_types::output`：`ModelChildTool`、`ModelChildContext`、`ModelChildBudget`
  （含 `resolve` / `is_finite`）、`ModelChildReplyGrant`；`ModelChildSpec` 新增
  `tools` / `context` / `budget` / `reply_grant` / `continue_task_id` / `owner_answer`。
* `shenko_store::external`：`MODEL_CHILD_KIND`、`MODEL_CHILD_STEP_OPERATION`
  （`model.child.generate.v1`）、`MODEL_CHILD_RESULT_OPERATION`（`model.child.result.v1`）、
  `Store::external_task_operations(task_id, operation_name)`。
* `shenko_store::Store`：`Store::claim_child_tool_task(...)`（原子创建并 claim 一个
  工具子任务）、`Store::commit_native_dispatch(ctx, operation_id, attempt_id)`（单次
  attempt 的发送边界）、`Store::admit_child_publication(...)`（child 发布准入）。
* `shenko_kernel::task`：`ChildToolRuntime` / `ChildToolCall` / `ChildToolOutcome` /
  `NativeTaskInvocation.child_tools`；
* `shenko_subject`：`NativeChildService::tool_runtime()` / `describe_tools()` /
  `call_tool()`。
* `ModelChildHandler` 的 `descriptor().prompt_section` 现在描述多步 + 工具能力。

---

## 9. 验证范围与未通过的边界

2026-11-02 的集成记录已覆盖 Linux 工作区构建，以及真实 Store/parser/broker 配合
脚本 loopback 响应的限定机制验证。它们不是实际模型行为验收，不证明下列全部行为。

2026-11-03 的同一有界原生场景依次暴露并保留了两次失败：首次工具任务使用更新而非
插入，导致 revision 下溢；修复插入后，整个 task 共用的发送标记又阻止了第二模型步。
修复两处后，真实默认 CLI 完成了五个 child 模型步，执行研究工具与一次 PMA 调用，
最终结果回到主体，CLI 正常退出。网络响应由脚本提供，不是真实 Coder390 行为证明。

证据位于 `build/spec-architecture-20261101/` 的 `pma-epoch-before-r1/`、
`pma-epoch-admission-fixed-r1/` 和 `pma-epoch-dispatch-fixed-r2/`。
最后一次观察的 CLI SHA-256 为
`a79deb1e5972f3f74fb1573f23a161f86837b01b574e1f68aec45bd67e345e5b`；
原始 provider 请求与独立 Store/CAS 回读分别保存在后者的
`provider-observation.json` 和 `store-observation.json`。

该观察当时还证明 **epoch 定向瞬态提醒未通过**：五个 child 请求都没有收到 prepared
contribution，而提醒正文作为 PMA 工具结果出现在最后三个请求的滚动历史中。PMA
决策成功落账不等于提醒投递和单次消费正确。

2026-11-03 修复 epoch 定向投递与请求恢复边界后，同一有界原生场景重跑并通过：提醒
正文只出现在**拥有该 epoch 的 model child 自己的 prepared contribution** 中一次
（`step_index = 2`，即第三次 child 请求），此后不再出现；它没有进入任何 child 请求的滚动 tool
历史，也没有进入主体的请求。持久化的投递作用域是该 model child 的任务作用域
`{scope_id: child_task, task_id: Some(child_task)}`，而不是 epoch id——epoch id
从来不是收集作用域。PMA 决策结果只保留身份摘要与封口的决策引用，提醒正文不再成为
永久的滚动上下文。

该次证据位于 `build/spec-architecture-20261101/child-epoch-r2/`（provider 观察
`provider-observation.json`、独立 Store/CAS 回读 `store-observation.json`、
`receipt.json`），构建与回归证据位于同目录 `child-epoch-build-1/`。CLI SHA-256 为
`456d1157eef27232debec08c03bdaeb5e7779a8baa61ffb1b4cdc8104bff855c`。provider 响应
仍由脚本给出，不是真实 Coder390 行为证明。

**来源绑定更正**：上述二进制上的机制观察保留，但旧构建脚本在构建后才取源码摘要，
没有构建前后比较；不能把 `sources_unchanged: true` 解释成完整构建来源证明。
合并树补跑 `child-epoch-merge-r1/` 的七项机制检查也为真，但其绑定脚本先追加
`Cargo.lock` 再判断空列表，导致完整源码分支不可达，回执只绑定了锁文件。
该补跑的 Store observer 仍从分支工作树编译，不能称为独立的合并树消费者。
共享 Cargo target 的跨工作树产物风险也未被这些退出码排除。

`agent-heavy` 当时有四个槽，并非单槽互斥。保留的
`child-epoch-build-1/merge-process-snapshot-20261103.json` 显示两个 merge supervisor
及其 Cargo 进程同时存在；不声称两作业串行，也不据此推断两个编译器或测试同时执行。
这些限定另存于 `child-epoch-build-1/evidence-correction-20261103.json`，不覆盖原回执。

上述限定场景之外，嵌套 child 的预算账户、继续链的 transcript 继承、child 经通用
渠道外发以及全部中断/恢复组合仍需各自的原生和真实模型证据。

本次 Store roundtrip 21 项、Subject library 55 项、PMA contribution 3 项、PMA 决策
结算 3 项测试通过；新增断言覆盖工具任务创建/认领的原子性与回滚、不同模型步的独立
发送、同 attempt 竞争、取消前后及重开后的防重复、旧任务级发送记录的保守恢复，
以及 child 一次性材料在注册表换代与重开之后不被复活。初次测试因 fixture 缺少
native input 绑定而失败，修正的是测试初始化，未放宽产品准入。

回归期间还暴露并修复了一处投影缺陷：按需发布的作用域被并入周期作用域集合后，
会被当作“不再 pending”而清空，抹掉另一个归属仍在读取的批次。现在按需发布只发布
该作用域，不改动周期集合。

`child-epoch-build-1/rerun.log` 中 Subject library 的 420 秒阶段先报告两个用例
`FAILED`，随后在完整失败摘要产生前被终止。后续四线程局部运行与默认线程完整运行
分别通过，只证明各次运行的结果，不能据此归因于宿主负载或排除产品竞态；原因未定。

`child-tool-verification-r3/receipt.json` 的两条测试命令均退出 0，但外层检查因
进程组仍存在而退出 1；原始失败保持不变。后续进程快照发现一个已退出、由 PID 1
持有的 `kill` zombie，其组内没有执行中进程。由于外层未保留原 Cargo PID，不能把
该组归属写成确定事实，也不能声称本次所有进程组均已消失；这与原生 CLI 观察中
已确认的正常退出、进程组消失是不同的证据边界。

测试超时仅说明观察未在指定窗口内完成，不能单独归因于宿主负载，也不能由后续通过
排除产品缺陷。完整用例耗时不能代替内部等待阶段的耗时；逐次启动并等待结束的重复
运行不构成并发证据。必须保留原始失败、实际命令与可执行文件身份，不以放宽观察
时限代替原因调查。

2026-11-02 的探针记录了一次约 6.92 秒后被观察到的超时：完成了三轮派发检查，
其中已完成的 `run_once` 调用最长约 164.7ms；到期后的快照有一个 ready 结果、
一个 `Running` 任务和一个 broker 占用。`progress_events` 实际计数的是循环迭代，
不是持久化进展；来自多个用例的逐轮日志也未绑定唯一用例身份。这些读数没有直接
测量 SQLite/fsync 阻塞、任务调度间隔或 provider 的发送阶段，因而不能据此证明
"产品正常推进"或将原因确定为磁盘阻塞。此前相应的因果结论撤回。

现有测试辅助函数使用 30 秒的逐轮 `tokio::time::timeout`，并在轮次开始时检查
120 秒的累计墙钟时间。这是比原 5 秒等待更大的观察窗口，不是产品修复证明，
也不是脱离墙钟的轮次预算。对照仅覆盖可挂起的 pending future 与可协作调度的
不收敛循环；Tokio 超时不能抢占同步阻塞代码，累计时间检查也不构成严格的函数
返回时限。原始超时及其原因仍应单独保留，不能由放宽后通过的套件代替结论。

父代理集成后的验证建议（同一套 native consumer / loopback 装置）：

1. 编译 + `cargo test -p shenko-subject`（含已迁移的 `model_child_tests.rs`）；
2. loopback provider 依次返回：一句含 `[tool:<kind>:<v>] {...}` 的输出 → 一句结果 →
   一句最终答案，断言产生了 **两个** step operation、**一个** 工具子任务、工具结果
   出现在第二个 step 的 payload 里；
3. 断言有限的作者预算可超过主体单步预算、transport 默认输出值及 32 步/工具次数；
   保留并另行验证父任务剩余额度和实际 provider/资源准入；
4. 断言 `[reply:...]` 在无授权时不产生 publication 行、有授权时产生与主体自身
   publication 同构的行；
5. 断言未决步在重启后**没有**第二次 wire 请求，且 operation 停在 `reconciling`；
6. 真实模型（owner 选定的 Coder390 通路）跑一次多步 + 一次工具调用，记录真实
   transcript 与结果。

未验证项不得被当作"已通过"。

<a id="spec-5-3"></a>
<!-- migrated-contract: SPEC:5.3 -->
## 5.3 委派与对话

外部消息默认进入主体结构化输入序列，由它决定理解、回复或委派；不能默认跳过主体直接交给另一个长期聊天人格。普通聊天可由有限对话 child 生成并在有效委派内发送。主体选择其角色、完整 system prompt、所用模型与必要的人格上下文；child 可以有不同工作角色，不必机械复制整份主体 system。免打扰/垃圾过滤按 [§13.8](subject-runtime.md#spec-13-8) 进入可追溯的延期或隔离状态，仍可由主体回看。

D2 已裁决：允许有限直接发布。主体在 handoff 时可一次授予 ReplyGrant，绑定 task/generation、目标 provider/account/conversation/thread、所回复事件、可发消息类型、完成/取消条件和必要的有效期。child 在有效委派内可把文本、附件或动态 UI 交给渠道插件直接发送，无需主体逐条再写一次，也无需每条询问人类；研究任务没有该委派时按通常结果路径回传。

一个逻辑回复只登记一个发布责任人。进展、澄清、阶段成果、最终答案和失败分别记录；“收到，正在查”不吞掉最终答案或随后失败。直接发送后，主体消费内容摘要、来源和投递回执，不默认再复述同一答案。peer 广播/自动回传不产生新的外部发言委派，也不形成主子互相自动回复的无限链。

发送前校验当前任务版本与有效委派；取消/改目标后的晚结果不抢发。发件箱持久记录 prepared/dispatched/confirmed/failed/unknown、逻辑 reply ID、结果 revision 和平台消息 ID。内部 Agent 收到、平台接受、人类已读、任务完成是不同状态；发送后断线先核查，平台缺少幂等/查询时如实保留不确定性。消息已经发出后，撤回/编辑是独立操作，不能把 task 取消当消息从未存在。已完成结果的交付不应额外占用推理 slot。

参考 OMP 固定版本的 owner-routed async-result、父消息/有效 steer 驱动的有限跟进、canonical result 接受条件、delivery epoch 与防重复 relay。其 IRC 为 Agent 间 mailbox，主 UI relay 为展示；这不是现成的 QQ/飞书外发能力。本产品在相应思想上增加 durable 渠道发布合同。[OMP 结果接受/回传](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/task/executor.ts)、[async delivery](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/session/async-job-delivery.ts)、[IRC bus](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/irc/bus.ts)。每次完整指定 child system prompt 和不同模型是本产品一级能力，不误称 OMP 普通 task wire 已普遍提供这两个参数。

子 Agent 进度应包含：当前阶段、完成证据、下一阻碍、预算使用、工作分支、待处理外部副作用。主体获得有限结果包，不把每个孩子的全部思考串回自己的 context。记忆读写的确定性步骤可以在 memory worker 内不调用模型；只有语义反思/综合需要推理，避免每次读取强制额外 LLM 回合。

<!-- /migrated-contract: SPEC:5.3 -->

<a id="spec-5-5"></a>
<!-- migrated-contract: SPEC:5.5 -->
## 5.5 共享能力全集与主体—child 双向协作

主体和 child 使用同一套 Agent 定义、能力目录、插件实现与版本体系；“主/子”是当前任务与 role 的区别，不是两套互不相通、孩子天然缺能力的框架。任何一方都可认识已安装能力、发现缺口、提出安装或开发意图，并按任务需要请求另一方协助。一个 child 产出的插件经验证与提升后进入共享 registry，主体和其他孩子在后续安全边界即可发现，不要求重新启动或复制整套安装。

当前有效 role 决定本轮直接执行入口：主体继续保留 [§5.2](subject-runtime.md#spec-5-2) 的意图、语义记忆和委派职责；文件、SSH、REPL、安装与版本操作由执行角色的 child 完成。共享能力全集不把主体变成第二个文件操作者，也不要求所有 child 接收全部工具 schema 或所有私密上下文。角色、任务 scope、数据可见性、执行 lease 与资源预算分别表达；切换 role 或提升插件有版本回执，不能借修改角色名字绕过当前有效合同。框架本身和这些角色实现仍可按 [§9](self-update.md#spec-9) 自改。

协作不是只返回一次最终摘要。提供持久的双向 dialogue：主体可追问假设、解释目标、讨论设计、提供来源、请求验证；child 可反问需求、报告观察不足、提出能力方案、请求同伴或反馈冲突。每条消息带 task/conversation、sender/recipient instance、role revision、in-reply-to、来源与附件引用；状态至少区分已收、已投递、已处理、已取代。恢复后续接同一讨论，不重复发起孩子或执行已完成动作。

对话可以异步进行。等待回答、等待工具和等待孩子时释放模型计算许可；每个接收者的对话更新按自己的顺序处理。主体仍只有一条有效决策序列，child 仍有目标与终止条件。主子双方引用同一证据/插件产物，而非在各自 context 中反复复制全部历史。自然任务验收应包含 child 主动指出“现有工具看不到选中项”并与主体共同收敛出修复方案。

<!-- /migrated-contract: SPEC:5.5 -->

<a id="spec-14-5"></a>
<!-- migrated-contract: SPEC:14.5 -->
## 14.5 递归收敛

子任务形成 DAG，有父预算账户和唯一语义指纹；相同查询/来源/实验去重。spawn 只分配已有预算，不凭递归增加总预算；为最终比较、复核和交付预留份额。工具 IO 可并行，推理 admission 与 KV 驻留独立调度。

研究阶段先扩展假设，再围绕证据缺口收敛；达到阶段验收、边际收益下降或本阶段资源需要调整时，保存当前结论、证据、未知点和继续成本。D9 允许兴趣长期续开，不能把阶段结束、暂时没有用户用途或一次预算耗尽当作必须永久停止的理由。主体可自主分配下一阶段，也可选择改变/放弃兴趣；阶段产物便于连续性，不是必须定期向人交作业。

验证采用相同总资源预算比较：保留全部上下文、summary、reset、selective reminders、reset+reminders；计入主体/子 Agent/记忆/judge 的全部推理与冷 prefill。任务正确率、证据覆盖、重复错误、延迟、资源消耗共同评估，避免只追 cache hit 或多开孩子数量。

<!-- /migrated-contract: SPEC:14.5 -->

<a id="spec-53"></a>
<!-- migrated-contract: SPEC:53 -->
# 53 子代理命名 / Child Agent Naming

来源：用户指示 2026-11-06；与 [§1](architecture.md#spec-1)「框架／产品名与每个 Agent 的姓名分开，实例初始化时由用户命名或让 Agent 自己命名」一致。

Shenko 的子代理（child agents, `ChildIdentity`）由主体自主命名，而非使用随机 UUID 或数字序号。命名规则：

- **有意义**（meaningful）：名字反映任务性质、角色或个性特征，使人和主体在日志、对话和 journal 中能直觉识别。
- **可记忆**（memorable）：名字简短、可发音，适合在对话和消息中引用。不使用 `child-3f8a-b2c1` 这类标识。
- **反映任务或个性**（task- or personality-reflective）：执行编码任务的子代理、执行研究的子代理、执行通讯的子代理各有不同名字风格，主体可按自己的 awareness 和 taste 决定命名方案。

```rust
/// 子代理身份。名字由主体选择，不随机生成。
/// Child agent identity. Name chosen by the subject, never randomly generated.
pub struct ChildIdentity {
    /// 人类可读名字，例如 "Kael"、"笔砚"、"Scout"。
    pub name: String,
    /// 可选的角色或任务描述。
    pub role: Option<String>,
    /// 内部唯一标识，仍由 CAS 或序列生成，不暴露给用户。
    pub internal_id: ChildId,
}
```

命名不是装饰。`ChildIdentity.name` 出现在 journal 条目、outbox 消息署名、日志输出和主体对话引用中。主体可以修改已有子代理的名字（作为该 identity 的一次 revision），旧名字保留在 journal 历史中。

当用户在实例初始化时选择让 Shenko 命名自己的子代理时，主体按自身 awareness 和 personality 选名，不请求人类逐个批准。如果用户希望自行命名某个子代理，这是一条普通指令，主体执行即可。

<!-- /migrated-contract: SPEC:53 -->
