# 上下文策略、切窗事务与瞬态贡献合同

本文是 SPEC §12.3.1–12.3.2、§37.7、§41.8 的实现边界说明，描述**已存在的接口与行为**，不替换、不删减 SPEC 的任何要求。`SPEC.md` 原文与 `docs/requirements/requirements.json` 的 R022、R115、R116 与验收项 A44、A45、V1-12、H04、PG01、PV07 仍是行为依据；接口名随实现演进。

涉及的代码单元：`shenko-context-policy`（合同、注册表、窗口账本、贡献注册表）、`shenko-compact-budget`（BudgetCompact 插件）、`shenko-compact-snap`（SnapCompact 插件）。策略实现不进入核心；核心只持有可替换合同。

## 为什么切窗是"换表示"，不是"丢经历"

原始 session 是真相源（R022）。切窗改变的是本轮模型可见的表示，不改变主体的持续逻辑序列，也不等于丢弃经历或回滚任务。因此任何策略都只能**引用**原始事件，不能改写、截断或伪造它们：

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

SPEC §12.3.2 要求切换在安全边界完成，并且"准备新表示"这一步本身很慢。慢的部分不能占用请求路径。

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

## 瞬态上下文贡献（§37.7 / §41.8.1）

贡献是**请求之前**准备好的材料：一次性的记忆提醒、一个认知视角、一条任务状态说明、"这一段已被压缩"的告知。它标明生产者、适用范围、失效时机；已提交 S0 记录本次请求采用的材料，provider 收据另行证明是否实际发送、收到或完成，二者不能混同。

类型定义在 `shenko-types::plugin_runtime`（协议数据，§41.3 的共用类型层），字段与 SPEC §41.8.1 的 `ContextContribution` 一致，并补齐了 SPEC 要求的**范围（scope）与过期（expiry）**：

```
ContextContribution { key, slot, source, scope, input_revision, lifetime,
                      required, budget_tokens, tokens, data, prepared_at_ms }
```

- `source` 绑定真实生产者：`plugin_id`、`plugin_version`、`artifact_digest`（没有工件的生产者用 `None` 并把 `kind` 记为 `Reminder`）、`kind`。
- `scope` 标明适用的分支与任务；`lifetime` 是 `ThisRequest` / `UntilEventRevision` / `ExpiresAtMillis`。
- `required` 的贡献不能被当作可选项跳过：无法纳入时进入 `missing_required`，由调用方决定含义，注册表不静默丢弃。

### 合并规则只有一条

合并规则实现在 **kernel**（`shenko_kernel::contribution::merge_contributions`），因为 hook 图的 `collect` 合成与上下文注册表必须以同一方式合并——只有一条规则，不是每个调用方各写一份：

- 同 `(slot, key)` 且内容完全相同 → 折叠为一个，重新收集是幂等的。
- 同 `(slot, key)` 但内容不同 → `ContributionConflict`，错误码逐字为 `CONTRIBUTION_CONFLICT`。冲突状态保留到整个分组结束，首个、后续以及第三个生产者均留下遗漏记录，不会因先移除冲突项而让后到者获胜。
- kernel 的 `same_contribution` 是合并与消费共同使用的身份判据：范围、生产者、输入修订、寿命、必需性、预算、token 和内容引用均须匹配。准备时间是观察元数据，不为相同输入材料制造新身份。

### 收集与用量证据

`ContributionRegistry::collect(scope, now_ms)` 是请求路径：它只读已经准备好的材料，合并、丢弃过期项（并记录原因）、遵守各自声明的预算、从不调用生产者。总 token 使用 checked addition；无法表示的新增项进入 `omitted`，不会回绕或导致主循环 panic。整个请求的模型容量仍由最终载荷检查负责，不能把生产者声明成本当作实际 tokenizer 计数。

最终载荷检查按**完整载荷**收费，而不是按追加贡献之前的前缀：追加的贡献正文用同一条启发式重新估算，与生产者声明成本取较大者，加到所选窗口的估算上；所选窗口的图像估算被保留，不因按 vision=0 重扫而丢失。上限比较同时看这项完整估算和被校准的预算输入——绑定一致时，上一次 provider 实测输入的增量基准以完整载荷的估算为基数。容量没有观测到时仍然是未知，不按模型名臆造上限。

`record_usage(usage)` 投影已提交 S0 采用的完整准备身份，并消费与其匹配的单次项（`ThisRequest`）。消费身份在注销、重新注册、重新发布以及恢复先于发布时都保留；改变准备时间不会制造新材料。另一作用域或较新输入修订不会被旧请求一并消费。同一请求的相同记录是幂等重放，改变其封口或内容会返回 `UsageConflict`，不覆盖旧证据。

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
- 未做真实模型/真实视觉帧的端到端切窗：帧渲染、中文/数字/路径/代码的可读性、图像 token 实际消耗仍需按 SPEC §12.3.2 的实测口径（各策略在相同任务上的正确率、原文定位、未完成工作续接、cached/computed prefill、Replay、总完成时长）取得。
- provider 侧的真实视觉信封尚未接入，因此 `ModelCaps.vision` 目前来自已通告能力；没有通告时 snapcompact 走 `Unavailable` 回退，这是设计中的诚实降级。
- 偏好顺序的权威配置值由配置服务提供；当前运行时使用安装顺序并在报告里说明来源。
