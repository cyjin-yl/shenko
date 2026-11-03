# 模型 child：有限、多步、可调用真实能力的子 Agent

适用范围：SPEC §5.3（委派与对话）、§5.5（主体—child 双向协作）、§6（child 的独立
system prompt / 模型 / 上下文 / 工具 / 输出预算）、§14.5（递归收敛：父预算账户、去重）、
§41.5（有限子任务）、R106/R107。

实现位置：`crates/shenko-subject/src/model_child.rs`（运行时）、
`crates/shenko-subject/src/child.rs`（共享 native runtime 与工具目录）、
`crates/shenko-store/src/{external,intents,child}.rs`（账本与准入）、
`crates/shenko-types/src/output.rs`（作者可见的 spec 类型）。

本文描述合同与接口；已执行的机制验证及未通过的边界见 §9。脚本 provider 配合真实
CLI、Store、broker 的验证不等于真实模型行为验收，也不覆盖未执行的恢复场景。

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
子任务 id 由调用身份派生（`tool_task_id(call_id)`）。不存在的子任务先插入，再在
同一事务中认领；认领失败时创建也回滚，不留下可被另一 dispatcher 抢走的中间任务。
已有调用身份不再新建任务：重启、恢复或继续讨论时读取、协调原任务的持久结果，
未知效果不能被当作一次新调用。

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

发布复用主体的 native publication 任务链：`Store::admit_child_publication` 写入
与主体 S1 发布同构的 `subject_intents`（`kind='publication'`，state=`ready`）与
`subject_publication` 任务行，已安装的渠道插件因此用同一个 service、同一套
prepared/dispatched/confirmed/failed/unknown 状态与回执处理它。发布任务挂在主体的
owner 链上（不是 child 任务上），因此 child 自己的完成**不会**被一次远端发送拖住。

> 迁移边界：主体发布已使用 `CHANNEL_PUBLISH_CONTRACT`，child 的
> `PUBLICATION_CONTRACT` 仍残留 `shenko.napcat/onebot11-send/1`。因此不能据共享
> `PublicationTarget` 和任务表声称 child 的通用渠道发布已完成验收。

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
* `shenko_subject`：`ChildToolRuntime` / `ChildToolCall` / `ChildToolOutcome` /
  `NativeChildService::tool_runtime()` / `describe_tools()` / `call_tool()`；
  `NativeTaskInvocation.child_tools`。
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

该观察同时证明 **epoch 定向瞬态提醒仍未通过**：五个 child 请求都没有收到 prepared
contribution，而提醒正文作为 PMA 工具结果出现在最后三个请求的滚动历史中。PMA
决策成功落账不等于提醒投递和单次消费正确，不能将本次多步执行成功泛化为此项通过。

上述限定场景之外，嵌套 child 的预算账户、继续链的 transcript 继承、child 经通用
渠道外发以及全部中断/恢复组合仍需各自的原生和真实模型证据。

本次 Store roundtrip 21 项、Subject library 54 项测试通过；新增断言覆盖工具任务
创建/认领的原子性与回滚、不同模型步的独立发送、同 attempt 竞争、取消前后及重开后
的防重复，还有旧任务级发送记录的保守恢复。初次测试因 fixture 缺少 native input
绑定而失败，修正的是测试初始化，未放宽产品准入。

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
