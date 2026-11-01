# 模型 child：有限、多步、可调用真实能力的子 Agent

适用范围：SPEC §5.3（委派与对话）、§5.5（主体—child 双向协作）、§6（child 的独立
system prompt / 模型 / 上下文 / 工具 / 输出预算）、§14.5（递归收敛：父预算账户、去重）、
§41.5（有限子任务）、R106/R107。

实现位置：`crates/shenko-subject/src/model_child.rs`（运行时）、
`crates/shenko-subject/src/child.rs`（共享 native runtime 与工具目录）、
`crates/shenko-store/src/{external,intents,child}.rs`（账本与准入）、
`crates/shenko-types/src/output.rs`（作者可见的 spec 类型）。

本文描述**合同与接口**。本文中的"已实现"只指源码已存在；本次交付**没有**执行任何
构建、测试、native 或真实模型调用（父代理在集成后统一验证）。任何"运行期行为"的断言
都以"待验证"标注。

---

## 1. 一个模型 child 是什么

模型 child 是**一次有界的 agent 运行**，不是"带 `tools=[]` 的单次生成"：

| 事实 | 归属 | 持久位置 |
| --- | --- | --- |
| 冻结的 child 合同（契约 schema 2） | 委派者写、运行器绑定 | `subject_intents` 的 delegation 行 + CAS 对象 |
| 每个模型步（一次真实 generation） | child 自己的 operation/attempt/evidence | `operations` / `attempts` / `evidence_receipts`，operation 名 `model.child.generate.v1` |
| 每次工具调用 | 一个**真实的** native 子任务 | `tasks`（kind = 工具 kind，owner = child 任务） |
| 运行结果（transcript、答案、预算、终止原因） | child 的 run operation | operation 名 `model.child.result.v1` + 其 normalized 记录 |

child 与主体共用同一套 broker、同一 quota gate、同一 native handler 注册表、同一
provider 通路。child 不新增第二个 registry、第二个循环或第二套账本。

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
| `continue_task_id` | 继续先前某次 child 运行 | 可空；见 §6 |
| `owner_answer` | owner 对该 child 提问的回答 | 与 `continue_task_id` 同时出现 |

**输出预算不是主体单步上限的函数。** 主体某一步的 `max_tokens`（S0 payload 的
输出预算）不限制 child 的每步预算；`default_max_tokens` 只是未指定时的默认值，不是
硬上限。child 自己的预算、父任务剩余额度及实际 provider/资源约束分别保留。

child 的每步预算、工具选择、上下文与语义 prompt 全部**独立于主体**：child 不会
机械继承主体的 prompt、性格或模型。

---

## 3. 冻结合同与指纹

`FrozenModelChild`（契约 schema 2）是 child 的冻结身份，内容包括：

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
一起移动）。child 合同 schema 2 必须绑定到同一个 plan schema；用旧 plan 封存的 child
委派会被拒绝，不会被"宽容读取"。

每个模型步另有自己的冻结输入 `FrozenModelChildStep`（step schema 1），它携带
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
子任务 id 由调用身份派生（`tool_task_id(call_id)`），所以**同一个调用永远不会被执行
两次**：重启、恢复或继续讨论时读的是同一条子任务的持久结果。

### 4.1 嵌套的模型 child

`kind = "model.child"` 的工具调用是一个**真实的嵌套 child 运行**：它有自己的冻结合同、
自己的模型步、自己的 transcript，并作为外层 child 的持久子任务存在。

* 嵌套 child 只能花费外层 child **剩余**的预算（`budget.max_total_tokens` 不得超过
  外层剩余额度）。递归**不增加**总预算（SPEC §14.5）。
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

授权的账号绑定：`ModelChildReplyGrant.account` 显式给出账号；缺省时用目标的
audience kind 作为账号（与主体自身回复的账号语义一致）。

发布走的是**与主体自身回复完全相同**的路径：`Store::admit_child_publication` 写入
与主体 S1 发布同构的 `subject_intents`（`kind='publication'`，state=`ready`）与
`subject_publication` 任务行，已安装的渠道插件因此用同一个 service、同一套
prepared/dispatched/confirmed/failed/unknown 状态与回执处理它。发布任务挂在主体的
owner 链上（不是 child 任务上），因此 child 自己的完成**不会**被一次远端发送拖住。

> 备注：`subject_intents` 里 publication 的 contract 字符串目前仍是
> `shenko.napcat/onebot11-send/1`（主体自身发布同样如此）。通用渠道 contract 的迁移
> 属于渠道 owner 的范围，本切片沿用现状以保证 child 与主体走同一条路。

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

---

## 8. 接口清单（父代理集成时需要知道的）

新增/变更的公开面：

* `shenko_types::output`：`ModelChildTool`、`ModelChildContext`、`ModelChildBudget`
  （含 `resolve` / `is_finite`）、`ModelChildReplyGrant`；`ModelChildSpec` 新增
  `tools` / `context` / `budget` / `reply_grant` / `continue_task_id` / `owner_answer`。
* `shenko_store::external`：`MODEL_CHILD_KIND`、`MODEL_CHILD_STEP_OPERATION`
  （`model.child.generate.v1`）、`MODEL_CHILD_RESULT_OPERATION`（`model.child.result.v1`）、
  `Store::external_task_operations(task_id, operation_name)`。
* `shenko_store::Store`：`Store::claim_child_tool_task(...)`（原子创建并 claim 一个
  工具子任务）、`Store::admit_child_publication(...)`（child 发布准入）。
* `shenko_subject`：`ChildToolRuntime` / `ChildToolCall` / `ChildToolOutcome` /
  `NativeChildService::tool_runtime()` / `describe_tools()` / `call_tool()`；
  `NativeTaskInvocation.child_tools`。
* `ModelChildHandler` 的 `descriptor().prompt_section` 现在描述多步 + 工具能力。

共享文件的所有权提醒：本切片改动了 `shenko-store/src/external.rs`（Ledger owner）、
`shenko-store/src/intents.rs`（Subject/Ledger owner）、`shenko-types/src/output.rs`
（Channels owner 共用文件）、`shenko-subject/src/child.rs`、`shenko-subject/src/loop_runner.rs`
（Subject owner）。合并时这些 hunk 需要由父代理与对应 owner 一起落。

---

## 9. 尚未验证的部分（诚实边界）

本次交付**只提交源码**。以下都**没有**执行：

* 任何 `cargo build` / `cargo test` / clippy / rustfmt；
* 任何真实模型调用；
* 任何 native 运行。

因此以下行为是**按合同设计、尚未运行验证**的：多步循环、工具调用与工具结果回灌、
嵌套 child 的预算账户、继续链的 transcript 继承、child 回复经渠道服务外发、
未决步的恢复不重发。

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
