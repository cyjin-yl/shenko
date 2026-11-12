# 上下文策略、切窗事务与瞬态贡献合同

**内核定位。** 参考插件，属于钩子架，建立在[内核合同](kernel.md)之上。策略只引用原始事件（[内核 3](kernel.md#kernel-3)），窗口随步封存（[内核 4](kernel.md#kernel-4)），经[内核 7](kernel.md#kernel-7)接入。目标是请求路径上只有一个收集点，即 `context.collect` 钩子点：贡献注册表保留“提前准备、请求路径只读”的合同，作为该钩子点的一个收集处理器接入。当前实现里主体直接调用注册表的 `collect`，没有派发这个钩子点；收敛步骤见[内核收敛计划](../development/kernel-convergence.md)第 4 步。

本文是 [§12.3.1](context-policies.md#spec-12-3-1)–[§12.3.2](context-policies.md#spec-12-3-2)、[§37.7](context-policies.md#spec-37-7)、[§41.8](context-policies.md#spec-41-8) 的实现边界说明，描述**已存在的接口与行为**，不替换、不删减 SPEC 的任何要求。本领域现行合同与 `docs/requirements/requirements.json` 的 R022、R115、R116 与验收项 A44、A45、V1-12、H04、PG01、PV07 仍是行为依据；接口名随实现演进。

涉及的代码单元：`shenko-context-policy`（合同、注册表、窗口账本、贡献注册表）、`shenko-compact-budget`（BudgetCompact 插件）、`shenko-compact-snap`（SnapCompact 插件）。策略实现不进入核心；核心只持有可替换合同。

## 为什么切窗是"换表示"，不是"丢经历"

原始 session 是真相源（R022）。切窗改变的是本轮模型可见的表示，不改变主体的持续逻辑序列，也不等于丢弃经历或回滚任务。因此任何策略都只能**引用**原始事件，不能改写、截断或伪造它们。压缩服从 taste（[51.9 助手代定（用户授权）](cognition-units.md#spec-51-9)）：窗口里出现修订过的单元或态度时，默认只露出旧版本数量和最新修改时间，不另定一套披露，也不删掉只增日志。

- `WindowEntry::Verbatim` 保存 `ItemRef{seq, event_id}`，正文在投影时从历史源解析；窗口不复制正文，也不替换正文。
- `WindowEntry::Pointer/Text/Image` 覆盖一段 `[from, to]`，是**有界派生物**。指针文本明确写明"原始事件仍可按 seq 或 event id 读取"。
- `WindowEntry::Image` 的字节存在 `Derivative` 里，并带 `media_type` 与 `blake3`。它是可重建的派生物，不是无损档案。

原始事件的读取入口不依赖产生窗口的插件是否还活着：已提交窗口保存自己的解码版本、源事件引用与派生物，插件升级或卸载只影响后续窗口。

## 两个独立可安装策略

R116/A45 要求 budget compact 与 snapcompact 都是正式交付的独立插件，各自拥有 manifest、版本、依赖与生命周期，可独立安装、更新、卸载和替换。

| 策略 | 单元 id | 触发 | 适用能力 | 产物 |
| --- | --- | --- | --- | --- |
| Budget compact | `policy.budget-compact` | 已用 token 超过 `max_context * trigger_ratio` | 无（纯文本） | 保留系统前缀、全部未完成调用单元与近期完整单元，其余换成指针 |
| Snapcompact | `policy.snapcompact` | 已用 token 超过 `image_request_context * trigger_ratio` | `vision`（真实视觉信封） | 中段渲染为 PNG 帧，文字头尾保留 |

两者的实现体是各自 crate 的 `ContextPolicy` 实现，核心不包含任何策略分支。

### 安装权威只有一处

安装事实的唯一权威是 `shenko-node` 的安装单元（`UnitKind::Code`，unit_id `policy.<策略 id>`）。本产品**不**另建一套 enable/version 注册表：

- 单元的 `artifact_digest` 是**策略 manifest 文档**的 blake3。编译进产品的策略没有独立工件文件，它的 manifest 文档就是标识其实现的真实内容；换成"标签"或版本号冒充身份是不允许的。
- 单元的 `config` 就是策略自己的配置文档（`trigger_ratio`、`recent_ratio`、`head_tokens`、`columns` 等）。运行时的策略用**单元记录里那份配置**构造，不使用硬编码默认值。
- `enabled` 是单元自己的启停状态。`shenko context policy disable snapcompact` 之后 snapcompact 不再运行；`enable` 恢复。缺失的记录与已禁用的记录是两种不同事实。
- 偏好顺序（先试哪个策略）目前跟随**已启用单元的安装顺序**；配置服务（namespace `shenko.context-policy`，logical key `compaction_preference`）一旦带上值即以配置为准。运行时如实报告当前用的是哪个来源。

命令入口：

```
shenko context policy list                 # 可安装策略、安装记录、启用集合、当前偏好顺序
shenko context policy enable <id>          # 安装（若缺失）并启用
shenko context policy disable <id>         # 保留记录，停止运行
shenko context windows [--limit N]         # 已提交窗口链的元数据（不读派生物载荷）
```

### 生命周期是普通 Cordis 生命周期

每个策略是一个 `Plugin`，`apply` 时向 `PolicyRegistry` 注册，`dispose` 时注销自己那一次注册。注销按 `(manifest id, epoch)` 精确匹配，因此被替换掉的旧 generation 迟到清理不会误删继任者。更新配置是同一 Fiber 的更新，会产生新的安装 epoch。

## 切窗事务：prepare → publish → adopt

[§12.3.2](context-policies.md#spec-12-3-2) 要求切换在安全边界完成，并且"准备新表示"这一步本身很慢。慢的部分不能占用请求路径。

### 慢路径（请求之外）

`PolicyRegistry::prepare_candidate(history, caps, used_tokens)` 读取整段原始历史并渲染新表示。主体持有一个准备任务，使用 `spawn_blocking` 执行这一步；后台任务只返回候选，当前主体负责发布与采纳。`prepare_and_publish(...)` 仍供明确拥有准备与发布生命周期的调用者使用，不能在请求事务中调用。

准备结果不是裸窗口，而是一个**绑定到当时修订**的候选：

```
PreparationBinding {
    scope_id,        // 分支/范围
    source_window,   // 准备时所处的窗口
    leaf_seq,        // 准备读到的最后一个原始事件
    model_generation,// 模型/模板/编码器代
    policy_epoch,    // 准备它的策略的安装 epoch
    policy_id,       // 准备它的策略
}
```

`publish` 把候选与**当前**状态逐项核对，任何一项不符即拒绝发布，而不是让过期窗口覆盖新窗口。

### 快路径（请求之内）

请求路径只做两件便宜的事：

- `observe_pressure(caps, used_tokens)`：记录实际观察到的输入大小与当时的真实模型限制。
- `needs_preparation(caps, used_tokens)`：合并"已安装策略自己的触发阈值"与"已提交窗口本身是否已超限"（`window_over_budget`，只读窗口账本）。**两者都不读原始历史。**

需要新窗口时，请求路径调用 `adopt_prepared(history)`：它再次核对全部绑定，并在**候选准备期间到达的原始事件也计入**的容量下检查是否仍能装下；合格才提交，否则丢弃候选并保留当前窗口。请求路径从不自己准备窗口。

**准备尚未完成不是失败。** 最终载荷仍能装下时，主体保留当前窗口并封口为 `Preparing`，不谎称已经压缩；最终载荷超限且确有准备任务时，`prepare_context` 返回 `Ok(None)`，主体在取消令牌下等待自己的任务，再从耐久暂存原件重新投影同一 Planned 步骤。等待期间没有 S0 封口、推理许可或单次贡献消费；取消时仍保留原始输入。

已发布候选直接经过 `adopt_prepared` 核验与投影，不等待一个不存在的后台任务。候选准备期间到达的新原始事件会进入最终载荷。若采纳后的完整载荷仍超限（例如必须原样呈现的新输入批次或贡献太大），明确拒绝，不删减原件，也不对同一结果无限重试。

准备失败只在源窗口、模型代、原始叶子、策略配置修订与触发预算均仍一致时归属于当前投影：能装下的请求继续，依赖该准备才能装下的请求返回失败。源或配置已变时重新考虑准备；过期候选与容量拒绝分别处理，不能把 `Stale` 当作未变化来源上的 `Refused`。`configuration_revision` 仅是进程内策略集合/偏好变化标记，不是新增持久事实。

主体取消等待或被丢弃会撤销任务句柄；已经运行的阻塞策略不能由 Tokio `abort` 强行终止，但它只计算候选，没有独立发布路径。因此不能据此声称策略代码立即停机或已完成线程回收。

### 提交时保留什么

`WindowLedger::commit` 在提交瞬间重新计算并写入：

- `call_states`：窗口提交时每个调用 ID 的原始 seq/状态基线。它是窗口携带的**事实副本**，不是第二个真相；执行账本仍是唯一可改写方。
- 未完成的调用单元（`Pending`/`Unknown`）在准备阶段就以整单元形式保留，不会被压成摘要或指针。窗口投影时若发现某调用的状态自基线以来发生变化，`rehydrate` 会把**整个原始单元**取回，包括夹在中间的无关消息——避免"工具失败在摘要里变成成功""取消的 assistant attempt 被拼成已执行的操作"。
- 取消的 assistant attempt 保持 `Assistant { cancelled: true }` 标记。
- 工具参数以 `args_ref` 指向执行账本中的**完整参数**，窗口不做截断 JSON。

### 原始事件、KV 与代价

- `covers_through` 之后的原始事件在物化时**逐条追加**，不会被丢弃。切窗期间到达的消息继续排队，旧快照不能覆盖它们。
- `cost.reusable_prefix_tokens` 是"最终输入与上一个窗口完全一致"的前缀 token 上界（同一原始事件、同一派生文本、同一派生字节、同一模型/模板/编码器代）；`cost.recompute_tokens` 是必须重算的部分。`cost.kv_accounting` 逐字说明这是**输入身份上界**，不是后端缓存命中测量。后端自报的 cache 数字由 broker 单独记录，两者不得混用。
- `cost.raw_events_covered`、`cost.open_calls`、`cost.frames`、`cost.frame_pixels`、`cost.image_bytes`、`cost.truncated_chars`、`cost.unrenderable_chars` 记录实际发生的代价。
- 无缩减、超限或帧不可读时回退其他策略，**不无限 compact**：注册表自身还有一条 5% 的最小收益门槛（`MIN_GAIN_RATIO`），把同一段原始历史重新切成同样大的窗口会被拒绝。

### 重启与代际围栏

安装 epoch 从已提交链的最大 `policy_epoch` 之上继续，因此**上一次进程准备的窗口永远不能在新进程里提交**。模型重新绑定（profile 变化）会追加一个覆盖全量原始历史的新根窗口，旧压缩前缀仍可读，但不再作为新绑定的输入。已提交窗口链在新进程中继续追加，不因重启丢失。

## Snapcompact 的帧是真实帧

视觉能力只按**真实能力**判断，不按模型名：`ModelCaps.vision` 来自 provider 实际通告的视觉信封，`image_request_context()` 取 `min(max_context, max_image_context)`。没有视觉输入的模型上 snapcompact 报 `Unavailable`，注册表回退到 budget compact 等可用路线——这是能力降级，不是取消交付。

渲染路径：GNU Unifont 字形（8×16 半角 / 16×16 全角），CJK 与拉丁、数字、路径、代码共用同一可读网格；超出 BMP 的字符没有字形，会被计入 `unrenderable`，并且**按记录逐条判定**：一条长工具输出不会稀释一条不可读的用户消息使其通过阈值。每帧在存入派生物之前校验 PNG 签名——**不是 PNG 的字节就不是帧**，文本回退永远不能冒充图像证据。

图像 token 是按 `patch_px` 的**估计**（`vision_tokens_estimate`），不是后端编码器的实测计数；成本报告如实这样写。

## 瞬态上下文贡献（[§37.7](context-policies.md#spec-37-7)、[§41.8.1](context-policies.md#spec-41-8-1)）

贡献是**请求之前**准备好的材料：一次性的记忆提醒、一个认知视角、一条任务状态说明、"这一段已被压缩"的告知。它标明生产者、适用范围、失效时机；已提交 S0 记录本次请求采用的材料，provider 收据另行证明是否实际发送、收到或完成，二者不能混同。

类型定义在 `shenko-types::plugin_runtime`（协议数据，[§41.3](durable-state.md#spec-41-3) 的共用类型层），字段与 [§41.8.1](context-policies.md#spec-41-8-1) 的 `ContextContribution` 一致，并补齐了 SPEC 要求的**范围（scope）与过期（expiry）**：

```
ContextContribution { key, slot, source, scope, input_revision, lifetime,
                      required, budget_tokens, tokens, data, prepared_at_ms }
```

- `source` 绑定真实生产者：`plugin_id`、`plugin_version`、`artifact_digest`（没有工件的生产者用 `None` 并把 `kind` 记为 `Reminder`）、`kind`。
- `scope` 标明适用的分支与任务；`lifetime` 是 `ThisRequest` / `ThisRequestUntilMillis` / `UntilEventRevision` / `ExpiresAtMillis`。`ThisRequestUntilMillis` 同时保留单次消费和未消费时的到期时间；TTL 不代表可以重复使用。
- `required` 的贡献不能被当作可选项跳过：无法纳入时进入 `missing_required`，由调用方决定含义，注册表不静默丢弃。

### 合并规则只有一条

合并规则实现在 **kernel**（`shenko_kernel::contribution::merge_contributions`），因为 hook 图的 `collect` 合成与上下文注册表必须以同一方式合并——只有一条规则，不是每个调用方各写一份：

- 同 `(slot, key)` 且内容完全相同 → 折叠为一个，重新收集是幂等的。
- 同 `(slot, key)` 但内容不同 → `ContributionConflict`，错误码逐字为 `CONTRIBUTION_CONFLICT`。冲突状态保留到整个分组结束，首个、后续以及第三个生产者均留下遗漏记录，不会因先移除冲突项而让后到者获胜。
- kernel 的 `same_contribution` 是合并与消费共同使用的身份判据：范围、生产者、输入修订、寿命、必需性、预算、token 和内容引用均须匹配。准备时间是观察元数据，不为相同输入材料制造新身份。

### 收集与用量证据

`ContributionRegistry::collect(scope, now_ms)` 是请求路径：它只读已经准备好的材料，合并、丢弃过期项（并记录原因）、遵守各自声明的预算、从不调用生产者。总 token 使用 checked addition；无法表示的新增项进入 `omitted`，不会回绕或导致主循环 panic。整个请求的模型容量仍由最终载荷检查负责，不能把生产者声明成本当作实际 tokenizer 计数。

最终载荷检查按**完整载荷**收费，而不是按追加贡献之前的前缀：追加的贡献正文用同一条启发式重新估算，与生产者声明成本取较大者，加到所选窗口的估算上；所选窗口的图像估算被保留，不因按 vision=0 重扫而丢失。上限比较同时看这项完整估算和被校准的预算输入——绑定一致时，上一次 provider 实测输入的增量基准以完整载荷的估算为基数。容量没有观测到时仍然是未知，不按模型名臆造上限。

`record_usage(usage)` 投影已提交 S0 采用的完整准备身份，并消费与其匹配的单次项（`ThisRequest`、`ThisRequestUntilMillis`）。消费身份在注销、重新注册、重新发布以及恢复先于发布时都保留；改变准备时间不会制造新材料。另一作用域或较新输入修订不会被旧请求一并消费。同一请求的相同记录是幂等重放，改变其封口或内容会返回 `UsageConflict`，不覆盖旧证据。

用量证据（`ContributionUsage`）带请求 id、请求修订、输入修订、封口摘要、发送项、遗漏项与总 token。主体只在 Store 接受 `planned → preparing` 后登记消费；仅写完 CAS 对象、过期快照被拒或 S0 事务失败都不消费。恢复和服务实例替换从该分支**全部已有封口的步骤**重建，包括早于当前 head 的步骤与退役尝试，而不只读取最新一步。一次步骤固定同一个已恢复的注册表实例。

持久真相仍然是现有 schema 12 的 S0 与原始调用记录；没有新增消费表、必需归档字段或第二份持久身份。注册表是可重建视图，其消费记录不是 provider 发送收据，也不能证明模型已收到。不能解码或校验的历史封口会明确阻止恢复，不默认为“尚未消费”。

四条使生产者与消费者不会各写一套约定的固定事实：

- **材料文档**：`ContributionPayload { v, text }` 是 `data` 引用所指文档的形状。生产者封 `ContributionPayload::seal(text)` 并把结果的对象引用放进 `data`；消费者解析引用后用 `ContributionPayload::read(bytes)` 取出正文。文档版本更新会被读取方拒绝，而不是猜测。
- **用量记录**：`ContributionUsage.sent` 保存完整 `ContextContribution`，包括作用域与生产者输入修订，不再以缺失这些字段的精简记录决定消费。`omitted` 的元素是 `ContributionOmission { key, slot, source, reason }`，`reason` 形如 `理由: 细节`。`ContributionPlan::usage(request_id, request_revision, input_revision, seal_digest)` 直接产出这份记录，调用方不需要手工拼装。
- **范围匹配是精确的**：按任务准备的贡献只有在请求用同一任务 id 收集时才可用。用 `task_id: None` 收集只会得到范围级材料；其他任务的材料会以 `scope_mismatch` 被报告，也不会成为本请求的必需项。必需身份按 `(slot, key)` 检查，另一 slot 的同名键不能补足它。`scope_wide(scope_id)` 是范围级作用域的唯一构造入口。
- **两个 token 数字是两件事**：`ContributionPlan.total_tokens` 是被保留贡献的声明成本合计（被裁剪的不计），而请求封口里的估算 token 是按实际追加的正文重新计算的。两者都要记录，不能互相替代；准入时以两者的较大值参与完整载荷的收费。

生产者通过 `register(provider)` 注册；自己执行异步准备的生产者用 `register_publisher(source)`。两者返回本次注册的 `EntityId`，不是可被后继复用的槽位。`prepare` 是慢路径，必须在请求之外运行。注册表是 Cordis 服务（`shenko/context-contribution`），插件在清理时用 `unregister(registration)` 注销自己这一实例。

发布按注册实例与 scope 隔离：

1. **开始准备前**调用 `begin_publication(registration, scope)` 取得不可复制的 ticket。新一轮准备使旧 ticket 失效，但保留最后成功发布的材料。
2. `publish(ticket, items)` 校验注册仍存活、批次未被替代、每项 source 与注册身份一致、scope 与 ticket 一致。成功时只替换本注册在该 scope 的批次；空批次只撤回自己的材料，不清空其他生产者。
3. `unregister` 撤回本注册的全部已发布批次并使在途 ticket 失效。旧实例迟到发布或清理不能影响后继注册。
4. `refresh` 在调用生产者之前取得各自 ticket，锁外执行准备，再整体校验和发布；失败或迟到结果不部分覆盖现有批次。收集路径仍不调用生产者。

## 错误与恢复

| 情况 | 类型 | 行为 |
| --- | --- | --- |
| 策略不可用 | `PolicyError::Unavailable` | 该策略被跳过，尝试下一个已安装策略 |
| 窗口没有变小 | `PolicyError::NoReduction` | 回退，不重复压缩同一段历史 |
| 超过请求上限 | `PolicyError::OverBudget` | 回退到其他策略 |
| 帧不可读 | `PolicyError::FrameUnreadable` | 该策略放弃，回退到其他策略 |
| 历史/账本读写失败 | `StorageError` | 窗口操作失败；**绝不**当成"没有历史"——那会从一段被静默截断的记录里切窗 |
| 候选与现状不符 | `PublishError::{ScopeMismatch, StaleSource, ModelGenerationChanged, StalePolicy, LeafAhead}` | 候选被丢弃，登记为 `Stale` |
| 提交前现状变化 | `CommitError::{StaleSource, ModelGenerationChanged, StalePolicy, LeafAhead}` | 提交被拒绝，窗口账本不变 |
| 提交后容量不足 | — | 候选被丢弃为 `Refused`，返回 `Ok(None)`，保留当前窗口 |
| 贡献冲突 | `ContributionError::Conflict` | 冲突项列入 `omitted`，错误码 `CONTRIBUTION_CONFLICT`，后到者不获胜 |
| 必需贡献无法纳入 | — | 键进入 `missing_required`，由调用方决定，不得用空值绕过校验 |

准备、发布、采纳的每一步都有可见结果（`PreparationOutcome::{Published, Adopted, Stale, Refused}` 与 `PreparationDiagnostic`），可以被查询，而不是静默。

## 接口与调用路径

| 接口 | 位置 | 调用者 |
| --- | --- | --- |
| `PolicyRegistry::{observe_pressure, needs_preparation, window_over_budget}` | `shenko-context-policy` | 主体请求路径（`context_projection.rs`，由 ArchSubjectNext 迁移） |
| `PolicyRegistry::{prepare_candidate, prepare_and_publish, publish}` | `shenko-context-policy` | 主体运行时后台准备任务（`loop_runner.rs`，ArchSubjectNext） |
| `PolicyRegistry::adopt_prepared` | `shenko-context-policy` | 主体请求路径 |
| `PolicyRegistry::{apply_preference, install, uninstall}` | `shenko-context-policy` | 组合根与配置服务处理器 |
| `ContributionRegistry::{register, register_publisher, begin_publication, publish, unregister}` | `shenko-context-policy` | 生产者插件的准备与生命周期 |
| `ContributionRegistry::{refresh, collect, record_usage}` | `shenko-context-policy` | 后台准备任务与主体 S0/恢复路径 |
| `merge_contributions` | `shenko-kernel` | hook `collect` 合成与 `ContributionRegistry::merge` |
| `ContextPolicyHost` / `ContributionHost` | `shenko-context-policy` | 组合根发布服务 |
| `BudgetCompactPlugin` / `SnapCompactPlugin` | 各自 crate | 组合根按安装记录启停 |
| `shenko context policy …` / `shenko context windows` | `bin/shenko/src/context_policy.rs` | 用户/运维 |

组合根（`bin/shenko/src/main.rs`）在模型无关的恢复服务之后、推理开始之前发布 `PolicyRegistry` 与 `ContributionRegistry`，然后**按 Node 安装记录**决定运行哪些策略、用哪份配置。

## 当前证据状态

本文描述的是**源码中已存在的接口与行为**。集成原生消费者已验证独立发布共存、一次性身份重发仍消费、先恢复 usage 后发布仍消费、注销撤回，以及真实 Store/CAS/主体 S0 重开后恢复同一消费与封口。旧的四项注册表失败、三项重开失败及修复后的原始回执均保留。注册表回归与 S0 CAS 拒绝回归通过；这些是合成材料的运行时机制证据，不是模型行为验收。

以下边界仍开放，本文件不据此宣称全部验收通过：

- 贡献恢复证据不代表真实 provider 发送、跨版本归档、物理断电或完整 C01/C03 验收。
- 未做真实模型/真实视觉帧的端到端切窗：帧渲染、中文/数字/路径/代码的可读性、图像 token 实际消耗仍需按 [§12.3.2](context-policies.md#spec-12-3-2) 的实测口径（各策略在相同任务上的正确率、原文定位、未完成工作续接、cached/computed prefill、Replay、总完成时长）取得。
- provider 侧的真实视觉信封尚未接入，因此 `ModelCaps.vision` 目前来自已通告能力；没有通告时 snapcompact 走 `Unavailable` 回退，这是设计中的诚实降级。
- 偏好顺序的权威配置值由配置服务提供；当前运行时使用安装顺序并在报告里说明来源。

<a id="spec-12-3"></a>
<!-- migrated-contract: SPEC:12.3 -->
## 12.3 历史切窗和中文检索

历史子 Agent 支持列窗口、列事件、按 ID 读取、全文搜索、时间/人物/任务过滤与证据引用。ContextPolicy 支持 summary+recent、budget-window+history recall、snapcompact 等可替换策略，按模型与任务实测选择。新窗口保留未完成委派、执行目标、预算、有效工具/权限版本与关键 source handles；社会关系、亲密程度与是否继续信守某项私人承诺由主体自主回忆判断，不作为硬编码关系卡每轮灌入。

所有 compact/new-window 都保留原始事件。回忆结果含范围、时间、source ID、是否被 superseded/删除与可信状态。重要 tool failure 不能在摘要里变成成功，取消的 assistant attempt 不能拼成已经执行的操作。[Codex 历史工具参考](https://github.com/openai/codex/blob/a3d93e1e692f1144cf8685175a5d1f6bf7294f57/codex-rs/ext/history-notes/src/tools.rs)

中文检索必须有专项设计：FTS5 unicode61 不自动完成中文分词；trigram 的 MATCH 不匹配少于三个 Unicode 字符。要求覆盖单字/两字词、人名、英文标识符、代码与中英混合，使用合适 tokenizer、双字索引或有限范围 substring fallback。资源访问资格在检索前检查；主体不按当前群聊受众屏蔽它已有的个人记忆，遵循 [§6](subject-runtime.md#spec-6) 的统一知识裁决。[FTS5 官方文档](https://sqlite.org/fts5.html)

外部 MemoryBackend 统一 retain/recall/reflect/update/forget/flush/health 与 provenance。QMD/OpenViking/Hindsight 等可选装，native 依赖、embedding/VLM 与服务进程纳入资源图；任何内部模型调用也必须经统一推理预算，不能借 memory 插件绕开全局锁。

<!-- /migrated-contract: SPEC:12.3 -->

<a id="spec-12-3-1"></a>
<!-- migrated-contract: SPEC:12.3.1 -->
### 12.3.1 Budget compact、notes-backed windows 与 snapcompact

用户已明确要求 budget compact 与 snapcompact 均实现为正式交付的独立插件，分别拥有 manifest、版本、依赖与生命周期，可独立安装、更新、卸载和替换。它们通过可替换 ContextPolicy 合同运行；策略实现不进入核心。其他上下文路线也可经同一合同接入，保持原始事件与附件真相源独立。切窗改变的是本轮模型可见表示，不改变主体持续逻辑序列，也不等于丢弃经历或回滚任务。

| 策略 | 已核验机制 | 本产品采用方式与成本 |
| --- | --- | --- |
| Codex budget compact | 固定源码 manual/auto 路径都跳过模型/服务器摘要，执行 start_new_context_window；正常触发 compact hooks 与事件 | 正式 budget compact 插件：新窗口保留必要运行状态与稳定 window/item 引用，按需读取历史；本地实现 SQLite/FTS5 与 notes，不依赖 Codex 云历史服务 |
| OMP notes-backed windows | 同版本有默认关闭的实验模式：context_notes、new_context、read/grep history://current/full，保留近期完整 tool-call/result 单元和稳定 entry IDs | 研究分支绑定、笔记 revision 与恢复工具齐备检查；不能只切掉历史却不提供可用的原文入口 |
| summary + recent | 模型形成语义摘要，保留近期原事件 | 作为可选策略；摘要 child 走全局 broker，记录遗漏、失真与额外调用成本 |
| OMP snapcompact | 本地把有界历史文字序列化、规范化并渲染为密集 PNG，具视觉能力的模型再读回；渲染不调用摘要 LLM | 正式 snapcompact 插件，按需安装并按真实视觉能力启用；计入渲染/内存、图像字节、视觉编码/token、prefill 与阅读正确率，不当作 VM/进程/KV 快照 |

两个插件共同使用版本化的历史引用、窗口准备/提交、未完成执行状态和成本报告接口，各自实现上下文表示、触发条件与适用能力声明。snapcompact 的字体、渲染器、帧格式和视觉依赖由插件及其依赖管理；模型或后端暂不支持时应报告 unavailable/degraded，交由当前策略或主体选择 budget compact 等可用路线。未启用不等于没有实现，不能把视觉限制作为取消该插件交付的理由。插件升级/卸载在切窗安全边界交接；已提交窗口保留其解码版本、源事件引用与必要派生物，旧 generation 不能晚提交覆盖新窗口；历史可恢复性不依赖原插件进程仍然存活。

Codex 固定 `a3d93e1e692f1144cf8685175a5d1f6bf7294f57` 的 history-notes 有窗口/条目列举、读取和搜索等能力，但 extension 的启用涉及特定 provider/backend 条件，接口也有 eventually-consistent 提示。这里参考它的心智与接口，不宣称复制工具名即可本地运行；本产品需明确本地已提交事件的读取一致性。[budget 路径](https://github.com/openai/codex/blob/a3d93e1e692f1144cf8685175a5d1f6bf7294f57/codex-rs/core/src/compact_token_budget.rs)、[history tools](https://github.com/openai/codex/blob/a3d93e1e692f1144cf8685175a5d1f6bf7294f57/codex-rs/ext/history-notes/src/tools.rs)、[extension 条件](https://github.com/openai/codex/blob/a3d93e1e692f1144cf8685175a5d1f6bf7294f57/codex-rs/ext/history-notes/src/extension.rs)

OMP 固定 `0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0`，snapcompact 源码包版本 18.12.6。它的 Archive.text 已经过工具输出/参数截断和字符规范化，并记录 truncatedChars；中段转成图像，文字头尾保留，再次 compact 合并的是有界 source。它不是无损原文档案，PNG 和有界文字都只能是可重建的派生物。Bun/TypeScript + N-API Rust renderer 的现状也不等于已有 WASI 版本，可由 [§18.8](wasm-tools.md#spec-18-8) 移植。[snapcompact README](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/snapcompact/README.md)、[实现](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/snapcompact/src/snapcompact.ts)、[notes/compaction 文档](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/docs/compaction.md)

<!-- /migrated-contract: SPEC:12.3.1 -->

<a id="spec-12-3-2"></a>
<!-- migrated-contract: SPEC:12.3.2 -->
### 12.3.2 切窗事务、未完成工具和 KV 代价

切换在安全边界完成：准备新表示，核对 source window/branch leaf/model generation，提交新 window 与恢复包，再续接原序列。期间新消息继续入队，旧快照不能覆盖它们。执行 ledger 独立保存 call ID、完整参数引用、执行/取消/unknown 状态、remote operation ID 和真实结果；保留完整调用单元或可恢复引用，不执行截断 JSON，不伪造 tool success，不因切窗重复已发生的 SSH/GUI/消息动作。OMP 对运行中 compact 的 abort、结果入账与有条件续接只提供事件顺序参考，不等于任意外部操作可回滚。[维护逻辑](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/session/session-maintenance.ts)、[工具运行中 compact 测试源码](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/test/agent-session-compact-running-tool.test.ts)

新窗口、摘要或图像会改变实际输入身份；只能复用真正相同的前缀，不能保留旧 KV handle 就声称后缀零重算。snapcompact 中的图像档案半预算启发式不是本产品主/子 50/50 调度算法。各策略按相同任务测正确率、原文定位、未完成工作续接、cached/computed prefill、Replay、总完成时长和所有辅助成本；无缩减、超限或帧不可读时可回退其他策略，不无限 compact。

首个基线仍是用户确认的四流 128K，133120 仅为冗余。图像路径另协商真实限制：用户指定 NInfer 版本的 Vision 默认关闭，启用后的 merged-token envelope 为 32768，视觉请求取 min(max-context,32768)，不能从文本 128K 推出图像请求也有同等窗口；以后版本按实际部署更新能力。对当前 1Cat 基线同样测视觉处理器、缩放、CJK/数字/路径/代码精度与图像 token，而非照搬云模型字体/帧常量。[NInfer serving](https://github.com/Neroued/ninfer/blob/64d10fbf9f8e6bfc79ab0570b94674c5b9997929/docs/serving.md)

主体提出 compact/回忆意图，实际笔记、档案加工和状态提交由 child/provider 完成；不因此给主体文件工具。C=1 下主体不得占住唯一许可等待摘要 child；确定性渲染虽不占 LLM 槽，仍计设备 CPU/RAM/IO。灵魂、关系及社会承诺按 [§6](subject-runtime.md#spec-6) 自主回忆，不借恢复包硬塞固定亲密度。

<!-- /migrated-contract: SPEC:12.3.2 -->

<a id="spec-37"></a>
<!-- migrated-contract: SPEC:37 -->
# 37 cordis-rs 主干上的注入时序与 API 兼容

[§41](architecture.md#spec-41) 提供首版 HookDescriptor、plan 编译/激活、RequestSeal、运行结果、冲突和错误合同。本章的语义扩展点保持不变，具体时序以该合同为准；这是对指定主干的服务/API 约定，不是更换插件框架。

<!-- /migrated-contract: SPEC:37 -->

<a id="spec-37-1"></a>
<!-- migrated-contract: SPEC:37.1 -->
## 37.1 本轮裁决与参考范围

用户指定 cordis-rs 为插件化主干；Fabric 用于参考 API 注入先后、hook 扩展与插件兼容设计。本节的 hook registry、顺序编排和兼容适配器由 Cordis 插件生命周期装配。独立可安装的 awareness 及 bootstrap 复用该主干和既有执行服务；不把全部意识塞入一个总插件，也不产生第二套 Agent 调度框架。

插件主干已由 U45 确认为 [dshbox/cordis-rs](https://github.com/dshbox/cordis-rs)。固定研究基线 [1754062 README](https://github.com/dshbox/cordis-rs/blob/1754062cef67cfc119fc7a2084c0321a231b9092/README.md) 定义 Plugin、Context、Service、Event 和 generation 生命周期；[effect.rs](https://github.com/dshbox/cordis-rs/blob/1754062cef67cfc119fc7a2084c0321a231b9092/crates/cordis-core/src/effect.rs) 给出资源清理合同。Shenko 的持久操作、RequestSeal、hook 顺序、Awareness 与跨代交接由本 SPEC 定义，不冒称上游已有同名 API。原 colibri-rs 仅为历史笔误，不再保留项目身份待确认项。

本节暂按 FabricMC 的 Fabric API／Mixin 理解用户的 Fabric 参考。可取之处是显式事件点、类型化回调、命名 phase、相对顺序和扩展点兼容；Java 字节码注入不作为 Rust/WASM 默认实现方法。Fabric 官方明确区分事件回调与新增事件的触发位置；建立 callback 接口本身不会使业务路径自动产生事件。固定源码 a335c35c0b9a489c7f4e5dd0fe36f1974845f1e1 用于核对 phase/invoker 实现；其同 phase 注册顺序与循环退化行为不直接采用为本产品保证。[Fabric Events](https://docs.fabricmc.net/develop/events/)、[Fabric Event phase API](https://maven.fabricmc.net/docs/fabric-api-0.102.0+1.21/net/fabricmc/fabric/api/event/Event.html)、[固定 Event 源码](https://github.com/FabricMC/fabric-api/blob/a335c35c0b9a489c7f4e5dd0fe36f1974845f1e1/fabric-api-base/src/main/java/net/fabricmc/fabric/api/event/Event.java)

<!-- /migrated-contract: SPEC:37.1 -->

<a id="spec-37-2"></a>
<!-- migrated-contract: SPEC:37.2 -->
## 37.2 四种顺序分别表达

| 顺序 | 要解决的问题 | 不应混用的机制 |
| --- | --- | --- |
| 外部事件的接收与注意力顺序 | QQ、语音、child、更新、代理、存储告警何时交给主体，何时打断、聚合或进入 DND | 由持久 inbox、因果关系和注意力策略处理；不是某插件的 hook priority |
| 一次 API 调用内的处理顺序 | 先解析身份还是生成提示，先选工具还是算 token，compact 后如何补提醒 | 按 hook/phase 的显式依赖图处理；不依赖插件安装或异步回调完成顺序 |
| 最终模型输入的排列 | system、人格、工具、history、事件、瞬态提醒各放哪里 | 按版本化 context slot 与 provider 实际消息格式渲染；回调执行先后不直接决定 prompt 拼接顺序 |
| 软件版本与激活顺序 | 插件、hook schema、provider、兼容 adapter 哪个版本可共同工作 | 依赖/版本协商和代际激活处理；不因模块先启动便获得覆盖其他模块的权限 |

例如一个紧急 QQ 事件可以先于后台研究结果获得主体注意力，但加工它的身份插件仍需在渠道归一化之后执行；PMA 计算可以提前完成，其提醒却放到动态 context 区。没有任何一项能用一个全局 priority 整数同时表达。

<!-- /migrated-contract: SPEC:37.2 -->

<a id="spec-37-3"></a>
<!-- migrated-contract: SPEC:37.3 -->
## 37.3 外部事件先持久化，再在可用边界注入

NapCat 消息、呼叫/转写、child 结果、远端 DecisionRequest、版本更新、网络/代理变化、磁盘压力和能力缺口都进入统一事件合同，保存原始内容、来源、稳定 ID、发生/接收时间、causal parent、修订、附件与目标主体/任务。插件可以产生事件，但不能直接把字符串插入一个正在推理的上下文对象。

主体每轮选取可处理事件并固定 inbox 水位与选中 ID；新来的事件保持 pending。普通可合并进展允许形成有限 digest，原始记录和未处理身份不丢失；同一来源的因果顺序、撤销/修改事件和旧版本 receipt 不因全局优先级而被误解释。过滤、DND 和主动联系仍由已确认的主体策略及插件决定，不把入站筛选硬编码成仅 @ 才让主体知道。

生成期间来信，按后端真实能力选择：在合法 tool/turn 边界注入、通过真实 live steering 接口传递，或取消当前 generation 后用新 request revision 重新构造。不能直接改已发出的请求、已计算的 token 或旧 KV 的内容；取消后保留已发生的工具效果和实际请求占用。包含在请求里、已完成回复、已处理任务分别记状态，不能因尝试注入过就把消息永久标为已完成。

以上只决定消息何时进入连续主体；没有消息时主循环仍继续思考。事件处理机制不把它改为等待消息才醒来的 Bot。

<!-- /migrated-contract: SPEC:37.3 -->

<a id="spec-37-4"></a>
<!-- migrated-contract: SPEC:37.4 -->
## 37.4 Hook 最小描述

每个 hook 点至少定义 namespace/name、contract major 与可接受版本范围、输入/输出 schema、触发语义、phase、允许的返回结果、字段所有权/合并方式、同步或异步限制、错误/超时策略、是否可重放，以及 [§36](durable-state.md#spec-36) 的效果合同。handler 再声明自己的稳定 ID、插件 generation、before/after、必需/可选依赖、读取和写入区域、成本与可取消性。

不使用无边界的“所有插件拿到同一个可变 request 然后随便改”。处理器可返回受约束的 typed patch、context contribution 或明确 decision；assembler 按该 hook 的规则验证、合并并记录来源。Observer 只观察已经确定的结果，不能暗中改 request 或撤销已经发生的操作。

Hook 合同按用途选择以下语义，不能默认套一个 short-circuit 规则：

| 类型 | 默认行为 |
| --- | --- |
| Observe | 收到不可变输入或提交后事件；可排队后续任务，不改变本次结果 |
| Transform | 串行应用已声明的 patch；写同一字段要有确定覆盖/合并规则 |
| Collect | 收集可独立计算的贡献，再按稳定规则组合；异步完成快慢不决定最终顺序 |
| Decide | 定义具体枚举与组合器，例如 pass/defer/select；谁可以短路、是否必须全部评估由 hook 发布者规定 |
| Execute boundary | 经过统一 operation 合同进入实际执行；产生真实效果和回执，不作为可随意重复的纯转换 |

中间件如允许 around/next，必须声明每次调用能否以及何时调用 next；默认至多一次。重试交给持久 operation 协议，不能让一个 around handler 私下多次发送同一模型/API 请求或终端输入。Gate 取消之后仍发出 outcome 供只读 observer 观察；取消发生之前的效果继续按 [§36](durable-state.md#spec-36) 记账。

<!-- /migrated-contract: SPEC:37.4 -->

<a id="spec-37-5"></a>
<!-- migrated-contract: SPEC:37.5 -->
## 37.5 Phase、相对顺序与冲突

每个 hook contract/graph revision 内使用明确的 phase 与 handler 的 before/after 关系拓扑排序；phase 可由该 hook 的 provider 版本化更换，不是全框架永久统一的业务阶段。硬依赖要求目标存在且兼容，可选关系单独表示。无依赖项使用稳定 ID 等确定性次序；异步完成顺序不能改变语义。局部 priority 不能推翻当前依赖图、字段合同或事件紧急度。

两个插件同时要求最后执行、形成环、声明不兼容 schema、抢占同一独占字段或不同意取消语义时，候选图激活失败并生成可诊断事件，旧健康图继续工作。主体可自主选择调整顺序、安装兼容 adapter、改写插件或暂不激活冲突候选。不能“最后加载者获胜”后悄悄改变主行为，也不能因为一个新增插件冲突就破坏现有主循环。 这些检查处理当前图的类型、依赖与冲突；不要求每次业务配置都经过 某个配置策略的 validated 阶段。

同一 hook 内的排序不能跨越外层生命周期因果。例如 request 已发送之后的 observer 无法通过更高 priority 变成发送前 transformer。插件声明“after”也不证明其可覆盖别人写的值；覆盖必须属于该字段的显式策略。

<!-- /migrated-contract: SPEC:37.5 -->

<a id="spec-37-6"></a>
<!-- migrated-contract: SPEC:37.6 -->
## 37.6 首版建议的语义扩展点

这些名称用于讨论 API 边界，最终命名由实现时的主干 API 适配确定；不要求全部常驻注册空回调。

| 扩展点 | 可做的事 | 截止边界 |
| --- | --- | --- |
| ingress.normalized / ingress.persisted | 归一化渠道、附来源/附件 handle、记录后通知 | 原始证据保留；需持久化后才声称已可靠接收 |
| turn.inputs.select | 将排队事件、到期提醒与 child 状态纳入本轮 | 固定事件集合/水位；后到事件排下一 revision |
| context.collect | 回忆候选、任务状态、能力索引、人格表达、临时提醒 | 返回有来源的结构化 contribution，不任意写 system |
| context.policy.select / context.compact | 选择 Budget Compact、Snapcompact 或其他已装策略 | 一个窗口转换由明确计划统筹，不能两个插件互相反复清空 |
| context.assemble / context.validate | 固定 slot、工具 schema、上下文预算与模板兼容 | 完成后形成不可变 context revision |
| model.request.prepare / model.request.seal | 映射原生 backend 参数、缓存 affinity、provider 扩展与提交准备 | 最终 wire 请求/影响缓存身份的字段变动必须重新验证并生成新 revision |
| model.stream.observe / model.result.persisted | 观察输出、保存结果、统计、生成后续事件 | observer 不回写已发送的输入，不以 UI chunk 代替提交事实 |
| tool.prepare / effect.prepared / effect.observed | 绑定真实操作范围、先记录意图、执行后接收回执 | 副作用遵守 [§36](durable-state.md#spec-36)；旧回执不能因 hook 卸载而丢失 |
| checkpoint.prepare / restore.plan / generation.handoff | 收集各插件状态、参与范围和在途所有权 | 部分覆盖明示；不能令软件快照撤销外部事实 |
| capability.changed | 安装/移除/升级后更新发现索引、用法和能力限制 | 本次正在执行的图保持固定；新能力进入后续明确 revision |

新功能可以定义自己的 namespace hook，例如 voice.transcript.revised、seat.question.observed；公共流程新增调用位置时，由相应 harness/provider 插件在真实代码中显式触发。注册一个名字不能创造不存在的时机。

<!-- /migrated-contract: SPEC:37.6 -->

<a id="spec-37-7"></a>
<!-- migrated-contract: SPEC:37.7 -->
## 37.7 Prompt 布局、工具正确性与 KV

ContextContribution 标明 slot、source、适用 task/turn/revision、生命周期、token 预算、是否可摘要、是否必须保留及替代来源。建议区分稳定 system/tool contract、后置人格、任务/历史、选中事件、动态工具描述、瞬态 memory reminder；具体 wire 排列由模型模板 adapter 兑现。工具定义可在 provider 专门字段中存在，不伪称所有 API 都把它放在同一条 system 消息。

人格仍追加于核心规则之后，并不改变工具调用 schema；主 loop 的可调用能力仍受 [§5](subject-runtime.md#spec-5) 角色合同。外部网页、群消息或插件说明保留内容来源，不因 hook 注入位置被自动提升为开发者/系统指令。

下游插件优先依赖公开的语义 phase，例如 history.finalized，而非绑定某一个 compact 插件名；更换 compact 策略不要求所有注入者重写依赖。瞬态 PMA 提醒在本次 request 有效，下一轮不会自动成为滚动 history；其内容、来源和注入回执仍可在原始事实中追溯。context compact、工具选择和动态贡献的 token 成本统一结算，不能在最后预算检查后再塞入不计成本的提醒。可以先预留预算、再完成贡献并最终校验；超预算时按已记录计划裁剪或新建窗口，不能静默删掉当前任务。

稳定前缀仅在实际 token 序列相同时复用；插件更新、人格修改、工具 schema/模板变更都可能改变指纹。记录实际 context/template/tool/plugin 版本与 provider 缓存身份，尊重后端真正支持的 slot／共享前缀能力。执行顺序确定不等于 KV 一定命中，不承诺任意 hook 变更仍零 prefill。

<!-- /migrated-contract: SPEC:37.7 -->

<a id="spec-37-8"></a>
<!-- migrated-contract: SPEC:37.8 -->
## 37.8 异步、有限等待与 C=1

U63/[§5.6](subject-runtime.md#spec-5-6) 澄清：下文的有限等待是执行器或必要运行机制的有界合同，不允许 thought 工具同步等待业务 I/O、child 或模型，也不允许用无界提交/锁等待隐藏慢工具。thought 只读现成有界结果或提交异步意图，超时/未知准确返回；只有显式语义 wait 改变主体的自主等待模式。

轻量 schema/身份/预算处理不需要模型参与。I/O 或模型驱动的 memory/能力研究以可恢复任务准备结果，完成后发布 contribution/event；关键链路可使用已完成结果、按合同有限等待、延期或走明确的缺省路径。可选贡献超时不拖死主体；必需转换失败不能伪造成功，交由旧健康图、恢复路径或主体可解释的失败状态处理。

任何 hook 都不能持有同一后端唯一推理许可、事务写锁或全局图锁，再等待需要它的 child/model 调用。需要额外推理时，在持有推理许可之前准备，或按 [§41](architecture.md#spec-41) 取得 never_dispatched/真实静止证明后释放并重排；不能仅把已 armed/unknown 请求改成 waiting 就清槽。Hook 子请求携带 parent/request purpose，默认不再触发发起自己的同一模型 hook；显式递归需有有限执行合同。也不能等待当前主体轮次未来提交才能产生的答案，这类依赖必须延期为后续事件或子任务。后端最终准入可以附加实际 lane/slot 等运输元数据；若它改变模型内容或预算，必须重新 seal。

动态消息和工作结果通过队列形成下一事件，不在同一调用栈递归触发无限 hook。使用 causation ID、去重和进展监测发现插件互相唤醒的循环；由主体委派修复，不能将防重入实现成停止永续思考。

<!-- /migrated-contract: SPEC:37.8 -->

<a id="spec-37-9"></a>
<!-- migrated-contract: SPEC:37.9 -->
## 37.9 新增 Hook、兼容与自修改

新增 handler：声明要订阅的 hook 版本与相对关系，经主干生命周期注册，验证候选图与冲突，达到切换边界后激活。

新增 hook 点：先定义 payload、返回值/合并器、时机、错误/重放与副作用合同；再修改对应 provider/harness，使真实执行路径在正确位置触发；补文档、能力索引和兼容样例。主体可通过 [§35](self-update.md#spec-35) 的 self-development child 自行完成此过程。

允许 breaking change。API 兼容同时覆盖字段形状与时序/语义：移动触发点跨越外部效果、改变 phase 顺序、取消范围、返回值合并或 observer 可写性，均需要显式版本迁移评估。可并存显式的旧/新版本适配器、迁移调用方，或整体升级受影响闭包；兼容转换只执行一次真实 operation，不能为了同时兼容两个事件版本而把外部动作做两次。缺失 required hook 或主版本不符时不能静默跳过；optional hook 缺失则按声明降级并可发现。

能力索引向 Agent 提供“能在哪些时机扩展、输入/输出、顺序约束、示例、效果边界和验证办法”。局部 CLI/SDK/API 更改通过对应 adapter 解决，不要求更换 cordis-rs 主干。模型以后不再需要冗长用法指导时可以换认知插件，类型和运行时兼容合同仍明确。

<!-- /migrated-contract: SPEC:37.9 -->

<a id="spec-37-10"></a>
<!-- migrated-contract: SPEC:37.10 -->
## 37.10 激活、重放与审计

每个 dispatch/request 固定 hook graph revision、插件实现集合、输入水位、context revision、输出和必要原始证据。热更新先构建并验证候选图，随后原子发布；模型根据旧工具 schema 产生的调用继续绑定旧合同，旧实现不可用时返回明确失效并重新规划，不能交给同名新实现悄悄解释。在途调用完成或按合同交接，不会一半用旧 transformer、一半用新 reducer。卸载经主干清理作用域；旧版本仍负责的外部请求保留 receipt reader 和协调任务。

重放可以重建转换和观察，但非确定性贡献使用记录的结果；需要重算则明确为新 revision。外部 Execute hook 读取已有 operation 结果或对账，不能因事件重放再次发 QQ、按终端键或更新机器。hook 异常、超时、跳过、合并冲突与取消都进入 trace，供主体诊断和自主修复。这里的留痕实现也是可替换插件，不扩大不可改核心。

<!-- /migrated-contract: SPEC:37.10 -->

<a id="spec-37-11"></a>
<!-- migrated-contract: SPEC:37.11 -->
## 37.11 首版验收

| 编号 | 场景与通过条件 |
| --- | --- |
| H01 | 同一组插件以不同安装顺序加载，给定相同版本与依赖图得到相同处理顺序和最终结构；异步贡献完成顺序不改变结果 |
| H02 | 插件 A 要求 after B、B 要求 after A，候选图拒绝并说明环；旧主体继续，能委派修复 |
| H03 | generation 期间连续入站 QQ、child、更新与音频事件；按真实边界注入，不丢原文、不修改旧请求、不启动另一主体 |
| H04 | Budget Compact、Snapcompact、PMA 与工具发现同时安装，策略选择及最终预算确定；瞬态提醒不永久污染 history |
| H05 | C=1 且 memory hook 需要额外模型推理，主/子都能推进，没有占许可等待许可的死锁 |
| H06 | 给定 handler 新增 hook schema 或 before/after 依赖，缺失 required 目标不会静默失效；兼容 adapter 可升级激活 |
| H07 | 活跃请求期间热更新插件，当前请求按固定图结束或明确交接；后续请求使用新图，旧 receipt 仍能解释 |
| H08 | handler 超时、异常、重复事件或反馈循环有明确 trace 与恢复；不以新插件故障使整个永续主体不可恢复 |
| H09 | 自开发新增真实 hook 点和插件，加载后在目标时机实际触发；只注册名字但路径不触发不能通过 |
| H10 | 回放经过发送/Seat/更新 hook 的历史，读取既有事实或协调 unknown，不重复外部效果；已实现 provider 原生字段不被兼容层吞掉 |

所有 H 项是待开发的验收要求，具体签名、状态和错误由 [§41](architecture.md#spec-41) 约束；已确认的 Cordis 提供插件装配与生命周期，不能据此宣称已有全部 Shenko API。部署 hook plan 与未来候选分别记录；hook 内的相位固定仅针对该合同版本，不构成所有业务变更的统一检查流程。

<!-- /migrated-contract: SPEC:37.11 -->

<a id="spec-41-8"></a>
<!-- migrated-contract: SPEC:41.8 -->
## 41.8 Hook、上下文与请求封口

| 阶段 | 输出 | 允许的工作 | 硬边界 |
| --- | --- | --- | --- |
| ingress normalize/persist | InboxEvent/raw refs | 纯归一化；异步补充转 worker | durable accepted 前不能推进可重放 ACK；正文不升级成 system |
| inputs select | InputPlan | 选事件、因果支撑、既有 child 状态 | 固定集合与读水位；后到事件另排 |
| context collect | Contribution refs | 读取已提交结果、发起有界准备任务 | 不占唯一模型 permit 等另一次模型调用 |
| history finalize/compact | Window revision | 选一个明确 compact plan；保存寻址 | 不让多个 compact 插件反复覆盖彼此 |
| context assemble/validate | ContextRevision | 固定角色工具集、system、后置人格、历史、事件、瞬态贡献 | 完整 token 校验；不可省略工具参数开销 |
| model prepare/seal | RequestSeal | 映射 native 参数、cache hint 与可解释降级，持久化工具合同及 implementation digest | 改模型内容、模板、工具或预算必须重新 seal |
| broker admission | Allocation/ContextBinding | 取得已声明资源、添加真实 slot/lane 等运输绑定 | 运输绑定若改变语义或预算，退回重新 seal |
| dispatch/stream/result | Attempt/EvidenceReceipt | T1 → 外部请求 → E1/T2 | observer 不能改已发请求，stream 不直接执行未完成调用 |
| decision commit | S1/child intents/successor | 校验当前 epoch/revision 并提交 | 成功提交前不外发、不启动草稿委派 |

需要外部 I/O/LLM 的 hook 先创建可恢复准备任务，在事务与 compute permit 之外执行。可选贡献首版默认最多等待 250ms 的本地准备预算，超时登记 omitted/deferred；后台任务仍可完成并进入下一步。该预算不强制取消已发生的外部操作。必需贡献不能按 optional 跳过，必须明确等待、采用已验证的旧图/已有结果，或产生恢复事件；不得静默用空值绕过校验。

<!-- /migrated-contract: SPEC:41.8 -->

<a id="spec-41-8-1"></a>
<!-- migrated-contract: SPEC:41.8.1 -->
### 41.8.1 扩展点、处理器与图

```ts
type HookPoint = {
  id: string;
  major: number;
  input: SchemaRef;
  output: SchemaRef;
  mode: "observe" | "transform" | "collect" | "decide" | "around";
  writable_paths: string[];
  timeout_ms: number;
  failure: "stop_unit" | "report_and_continue";
};

type HookHandler = {
  id: string;
  implementation: VersionedRef;
  point_id: string;
  point_major: number;
  before: string[];
  after: string[];
  requires: string[];
  possible_model_call: boolean;
  deterministic: boolean;
};

type HookDispatch = {
  dispatch_id: string;
  graph_digest: Digest;
  handler_digests: Digest[];
  input_revision: string;
  input: DataRef;
  request_id?: string;
  request_revision?: number;
};

type HookResult =
  | { kind: "no_change" }
  | { kind: "replace"; value: DataRef }
  | { kind: "contribute"; items: ContextContribution[] }
  | { kind: "decide"; value: DataRef }
  | { kind: "defer"; dependency_task_id: string }
  | { kind: "reject"; code: string; details: DataRef };

type ContextContribution = {
  key: string;
  slot: string;
  source: VersionedRef;
  data: DataRef;
  input_revision: string;
  lifetime: "this_request" | "until_event_revision";
  required: boolean;
  budget_tokens: number;
};
```

图在激活前完成 schema 兼容检查、硬依赖检查和拓扑排序。有环或缺少硬依赖时拒绝候选图，保留当前图。合法拓扑顺序相同时，按稳定 handler ID 决定顺序；插件安装时刻、异步完成时刻和任务紧急优先级均不成为隐式排序键。

`before/after` 只约束同一 hook point 中存在的处理器；`requires` 明确声明必须存在的依赖。指向不存在的可选处理器不阻止激活。Phase 是 API 先后关系，消息 priority 是注意力/资源调度，两者不复用同一字段。

每个 `HookPoint` 必须给出有限的 `timeout_ms` 与失败策略；标准安装 profile 提供数值，插件缺省不等于无限等待。声明决定该扩展点的合同，单个处理器不能把本来 required 的 schema 校验改成可忽略。所有处理器遵循同一资源核算；`possible_model_call` 不是不受全局并发限制的许可。

<!-- /migrated-contract: SPEC:41.8.1 -->

<a id="spec-41-8-2"></a>
<!-- migrated-contract: SPEC:41.8.2 -->
### 41.8.2 各类输出的合成规则

| mode | 合法输出 | 合成与失败规则 |
| --- | --- | --- |
| observe | `no_change`、`defer` | 不改输入；异步观察不能阻止已提交事实发布。需要后续动作则提交独立任务 |
| transform | `no_change`、`replace`、`defer`、`reject` | 按固定顺序串行变换；每次验证 output schema 和 writable paths |
| collect | `no_change`、`contribute`、`defer`、`reject` | 按固定处理器顺序和 contribution key 合并，不能按 Promise 完成顺序拼接 |
| decide | `no_change`、`decide`、`defer`、`reject` | 第一个有效 decision 结束此扩展点；`no_change` 表示继续询问 |
| around | 本扩展点定义的结果 | `next()` 至多调用一次；重试必须成为 operation attempt，不能隐式多次 next |

同 slot/key 的冲突只有在该 slot 声明了明确 reducer 时才能合并；否则报 `CONTRIBUTION_CONFLICT`。不存在“后完成的覆盖前面的”默认行为。Transform 写入不在 `writable_paths` 中的字段报 `HOOK_WRITE_VIOLATION`。无论 failure 策略如何，非法修改都不能被采用。

`report_and_continue` 只跳过该处理器本次输出并留下诊断；它不能把一个已发出的外部效果抹掉。如果 hook 需要产生外部效果，必须调用 [§41.7](plugins.md#spec-41-7) operation 接口；hook 重试只重取已有 operation。纯声明“deterministic”不能免除真实外部调用的 ledger。

observe 返回 `defer` 只登记后续独立任务，不把已经完成的源单元转回等待；需要当前 request 等待其结果的处理器必须注册到允许贡献或变换的扩展点。hook 产生的嵌套模型调用进入同一 broker，默认不再次运行该 hook 自己；显式递归仍必须声明独立 dispatch identity 并通过等待环检查。

<!-- /migrated-contract: SPEC:41.8.2 -->

<a id="spec-41-8-3"></a>
<!-- migrated-contract: SPEC:41.8.3 -->
### 41.8.3 RequestSeal

```ts
type RequestSeal = {
  request_id: string;
  request_lineage_id: string;
  request_revision: number;
  supersedes_request_id?: string;
  seal_schema_version: number;
  runtime_generation: RuntimeGeneration;
  graph_digest: Digest;
  context_revision: string;
  input_plan_ref: DataRef;           // 引用 InputPlan；不是已消费的消息游标
  rendered_messages: DataRef;
  offered_tools: Array<{
    tool_name: string;
    contract_digest: Digest;
    implementation: VersionedRef;
    declaration: DataRef;
  }>;
  contributions: DataRef[];
  provider: VersionedRef;
  model_config: DataRef;
  semantic_native_options: DataRef;
  provider_wire_profile: DataRef;    // §47；固定实际 API/SDK/schema/model/gateway 解释
  token_budget: { input_limit: number; output_limit: number; accounting: string };
  prefix_digest: Digest;
};
```

首版请求顺序固定为：

1. 在主循环可提交边界选择已持久事件与任务状态；提交 `InputPlan`，其中包含各 ledger 的读取水位向量、实际 selected event refs/revisions 与 causal support refs，再创建新 request_id 及同 lineage 的新 request_revision。读取到 seq 105 不代表已消费所有 seq≤105 的事件。

2. 固定本次 hook graph，运行 context/tool/provider 的语义变换与必要依赖任务。

3. 组装人格、历史、已选择事件、按需能力和瞬态提醒，执行预算与工具调用结构校验；保存各 contribution 的来源及本次生命周期。

4. 将模型将看到的精确 messages、工具声明及模型/provider 语义参数提交为 `RequestSeal`。

5. 进入推理 broker 队列，获得 permit，调用后端；实际 lane/KV affinity/尝试与资源回执另行记录。

上述次序是协议顺序；实现可并行纯计算，但不能越过依赖和提交边界。需要等待模型的 hook 发生在步骤 4 之前，且父请求没有持有推理 permit。若某处理器在已有 permit 的路径上要求模型依赖，只有尚未 armed 的 attempt 才可在同一事务中撤销派发资格、形成 `never_dispatched` 证明并释放 Allocation，再登记 `waiting_dependency`。已经 armed/active/unknown 的请求必须先取得本章认可的 QuiescenceProof 才能释放；无法取得时，把模型辅助工作延后到后继请求。不能仅改成等待状态就清除物理占用。禁止持有 writer 事务等待，也禁止等待只有“本主请求完成后”才能产生的依赖；后者返回 `DEPENDENCY_CYCLE` 或将该工作改成下一轮贡献。

API/SDK/message-schema/模型语义变更按 [§47](inference.md#spec-47) 生成新的 seal revision；凭据值不进入 wire profile。Seal 后只允许改变不改变模型输入语义的传输字段，例如凭据刷新、连接信息及后端实际支持的 KV affinity；每次 transport attempt 记录其真实字段。切换模型、修改输出上限、修改 native 语义选项、工具 schema 或 prompt，均生成新 request_id 及同 lineage 的新 request_revision，并重新预算、封口和排队。调度器不能一边保留旧 prefix 身份，一边隐蔽改写其实际 token 输入。

live steering 不改变旧 prefix_digest；后继上下文身份还须包含实际应用链及输出 frontier，不能只凭同 request_id 恢复到缺少额外输入的 KV。无 native frontier 兼容证明时从原始 seal、应用链和持久轨迹重建，并记录 replay/prefill 成本。

`prefix_digest` 关联精确输入前缀及影响其解释的模型/tokenizer 身份，用于一致性和调度提示；摘要相同不等于后端真实 cache hit，KV 是否驻留或可恢复以 provider 的具体回执为准。

stream 开始后，新消息继续被持久接纳，但不能插入该请求过去的 messages。它进入下一边界，或触发有真实后端支持的取消/steering 路径。取消后的部分输出保留为事实；是否进入后续模型上下文由恢复/上下文合同决定，不能伪造完整 tool-call/result 配对。

工具调用必须匹配 `RequestSeal.offered_tools`。按 sealed contract 解释参数，绑定该版本实现或显式的兼容适配器。旧实现不可用且不存在声明过的适配器，返回 `TOOL_CONTRACT_UNAVAILABLE`；不能用当前同名工具静默替换。当前权限和资源绑定仍在派发时重新核验。

<!-- /migrated-contract: SPEC:41.8.3 -->

<a id="spec-41-8-4"></a>
<!-- migrated-contract: SPEC:41.8.4 -->
### 41.8.4 hook 恢复与热更新

一个 dispatch 固定 `graph_digest`、输入 revision 和处理器工件。模型辅助等非确定结果按 `(dispatch_id, handler_digest, input_digest)` 保存；重启重用已提交结果，不再次调用模型制造另一答案。输入已变化时创建新 dispatch，不把陈旧异步结果追加到已 sealed 请求。

热更新发布新图只影响新的 dispatch/request_id。旧图正在运行时保留其工件租约。晚到的 hook 输出若仍属于可继续的旧 dispatch，可以完成它；若该请求已取消/替代，记录结果并标记未消费，不能自动注入新请求。

REF-FABRIC 只支持类型化扩展点/phase 顺序方面的来源；上述持久 dispatch、permit 释放、seal 和恢复为 REF-DESIGN。REF-DURABLE 的 [§7.2 hooks](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2906) 与 [§7.4 prompt/tool 变更](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L3661) 是具体参考。与其当前 phase 再解析工具名的规则不同，Shenko 明确绑定 sealed contract，避免热更新重解释旧调用。

<!-- /migrated-contract: SPEC:41.8.4 -->
