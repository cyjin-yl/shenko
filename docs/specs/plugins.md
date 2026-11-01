# 插件运行时、Hook 图与能力目录

本文是插件运行时的功能与接口合同，对应 SPEC §7、§18、§36、§37、§41.2、§41.7、§41.8、§45。
它描述**目标行为与实际接口**，不声称任何运行期证据已经取得；真实能力、模型行为与长周期观察
由 `docs/specs/verification.md` 的证据规则单独记录。

## 三个所有者，三种事实

插件运行时只做机制：谁装了什么、装的东西有什么边界、扩展点在什么时机被调用。业务判断
留给认知单元和模型。

| 事实 | 唯一所有者 | 运行时的角色 |
| --- | --- | --- |
| 某个 Node 上安装了什么 | `shenko-node` 的安装单元 | 投影：运行时只报告本进程当前提供的实现 |
| 一次调用实际发生了什么 | Store 的 operation / evidence | 不复制；回执由调用路径写入 |
| 本进程现在提供哪些 provider / capability / help | 运行时注册表 | 权威：注册表是这一项的唯一真相 |

运行时注册表因此不是第二个安装数据库，也不是第二个单元库。它是一次进程生命周期内的
**当前视图**：插件在自己的 `apply` 中注册，在自己的 teardown 中注销。

## 活的服务，不是启动快照

`shenko-kernel::runtime::PluginRuntime` 是 Cordis 服务 `shenko.runtime.plugin/v1`，
由 `RuntimeServicesPlugin` 与 hook 图一起发布在运行时根 context 上：

```rust
let supervisor = shenko_kernel::Supervisor::new(generation);
supervisor.publish_services().await?;                 // 发布两个服务
let runtime = supervisor.context().try_service::<shenko_kernel::runtime::PluginRuntime>()?;
let hooks   = supervisor.context().try_service::<shenko_kernel::hooks::HookRegistry>()?;
```

插件在 `apply` 中解析服务并注册，在 `ctx.effect_sync` 中注销。注册与注销共享同一代际：
Cordis 的 generation drain 先跑 teardown，再释放服务，所以卸载后不会留下仍可被发现的
provider 或 capability。

注册项：

- `ProviderRegistration { provider_id, kind, version, plugin_id, implementation, description,
  operations, capabilities, health }` — 一个 provider 的实现身份与它每个 operation 的
  §36 effect contract。
- `CapabilityRegistration { capability_id, version, description, plugin_id, provider_id, enabled }`
- `HelpRegistration { capability_id, version, purpose, key_methods, applies_when,
  does_not_apply_when, schema_help, limitations, recovery, implementation }` — §18.2 的
  可使用性包。

每次注册、注销与健康变化推进 `PluginRuntime::revision()`；能力目录本身的变化另外推进
`PluginRuntime::capability_revision()`。安装回执、封口请求和 UI 视图因此都能指名它们
观察到的确切注册表状态。

`ProviderHealth` 区分 `Installed` / `Healthy` / `Degraded` / `Unavailable`：加载成功不等于
完成，被模型验证过是另一个事实。

用户可见的发现入口是 `shenko capabilities`：它执行与运行中的 agent **同一条**安装路径
（`bin/shenko/src/main.rs::install_native_definitions`），然后打印实时注册表视图、hook
合同与 hook 图摘要。它不是静态清单。

## 原生 handler 的 EffectContract

`NativeTaskHandler` 增加了两个默认方法（SPEC §36.3）：

```rust
fn effect_contracts(&self) -> Vec<shenko_types::effect::EffectContract> { Vec::new() }
fn capabilities(&self) -> Vec<shenko_types::plugin_runtime::NativeCapabilityDeclaration> { Vec::new() }
```

默认实现不破坏任何已有 handler，但声明了边界�� handler 就在实时注册表里出现，并且只
在它被安装期间出现。`EffectContract` 的规范定义在 `shenko-types::effect`，它表达：

- 身份与版本：实现 digest、operation 与其版本；
- 资源选择：可达的资源族与实际绑定规则；
- checkpoint 能力：可捕获什么、明确排除什么、一致性等级；
- effects：可能发生的外部效果（可同时多种）；
- retry：幂等保证来自何处；
- cancel：取消前后可能已经完成的阶段、停止证据、不能取消的部分；
- compensation：补偿方法、前置条件、可能失败、是否留痕；
- lifecycle：升级、卸载、崩溃时在途调用与租约如何处理；
- 保证依据：仅声明 / 运行时受限 / 已测 / 未知。

五种动作（reset / restore / replay / cancel / compensate）不共用一个 rollback 标签；
读写性质、外部可见性、幂等性、可取消性和可补偿性分别表达。

声明用 `shenko-types::effect::EffectContractBuilder` 以 Fluent 方式构造：每个字段都显式
给出；`contract_id` 由 `EntityId::derived` 从插件、操作名、操作版本与实现 digest 确定性地
派生，同一实现同一操作永远是同一份合同，不同实现是不同合同。`NativeTaskDescriptor` 提供
`implementation()`，实时 provider 视图、操作合同和 ledger 中的工件摘要因此指名同一个实现。

当前以真实声明装机的定义：`host.printf.v1`（只支持取消；无 reset/restore/replay/compensate；
外部效果是启动宿主进程、stdout 成为证据）、`history.query.v1`（只读，五种动作都不适用）、
`wasi.exec.v2`（取消在 guest epoch 生效、reset 重置沙箱、managed_home 挂载下的写是真实外部
效果、checkpoint 是 guest epoch 级）。其余定义（computer_use、self.knowledge、errors.read、
issue.submit、playground 等）的声明由各自所有者补全——契约类型、注册路径和
`EffectContractBuilder` 已就位，缺的不是机制。

## 版本化的扩展点

Hook 点是有名字、有版本的合同，不是六个固定的全局阶段。

```rust
let point = shenko_kernel::hooks::HookPointId::new("context.collect")?;   // 必须是 namespace.name
let mut contract = shenko_kernel::hooks::HookPointContract::standard(point.clone());
contract.mode = shenko_kernel::hooks::HookMode::Collect;
contract.writable_slots = vec![ContributionSlot::TransientReminder];
registry.declare_point(contract)?;
```

合同随图一起被准入：换一份合同就重新校验整张图；主版本提升会 fence 仍声明旧主版本的
handler（在册者以 `PointMajorMismatch` 拒绝，必须先撤销），而已经取得快照的调用继续按
它租到的合同派发。每张图带一个确定性 `graph_digest`（对合同与有序成员 spec 的 blake3），
所以保留的回执在热更新和重启之后仍然可解释。

标准安装 profile 声明这些点：

| 点 | 模式 | 写权限 |
| --- | --- | --- |
| `subject.before_inference` | observe | 无 |
| `subject.after_inference` | observe | 无 |
| `subject.before_operation` | observe | 无 |
| `subject.after_operation` | observe | 无 |
| `runtime.capability_changed` | observe | 无 |
| `runtime.generation_handoff` | observe | 无 |
| `context.collect` | collect | 全部瞬态 slot（`SystemContract` 除外，内核自己拥有） |

`context.collect` 对应 SPEC §41.8 阶段表中的 "context collect"：插件把准备好的材料按
`(slot, key)` 贡献进来，合并后的集合由上下文策略装配并封口。插件也可以用
`HookPointId::new("namespace.name")` 声明一个自有命名空间的新点，不用改动框架。

派发回执是 SPEC §41.8.1 的 `HookDispatch`：`dispatch_id`、`point`、`generation`、
`graph_digest`、有序的 `handler_digests`、`results`、`stopped_early`。调用者用
`dispatch_at(&payload, DispatchContext { input_revision, input, request_id, request_revision })`
把输入修订、封口输入和请求身份交给回执；内核不虚构这些事实。

## 运行时自己的变化也是 hook 事件

注册表在每次注册、注销或健康变化时：推进 `revision` 与 `capability_revision`、把一条
`RuntimeChange` 记入变更日志、并把能力变化排入 hook 图（`runtime.capability_changed` 点，
`HookPayload::CapabilityChanged { capability_id, enabled }`）。注册表自己不开异步运行时，
事件在下一个明确边界派发：

- 运行中的 agent 由 `bin/shenko/src/main.rs::spawn_runtime_hook_pump` 驱动：它订阅
  `PluginRuntime::changed()`（一个 `watch` 通道，晚订阅者也看得到当前修订号），有变化就
  `HookRegistry::drain_pending()`。这是事件驱动，不是轮询。
- `shenko capabilities` 发现命令在安装完成后排干一次并把回执打进报告。
- 队列有界（1024）：排干速度跟不上时保留最新事件并计数丢弃数，不假装每一次变化都被观测。

## 五种合成模式

| mode | 合法输出 | 合成规则 | 违规 |
| --- | --- | --- | --- |
| observe | `no_change`、`defer` | 不改输入 | 其它输出丢弃并留诊断 |
| transform | `no_change`、`replace`、`defer`、`reject` | 按固定顺序串行应用；`PatchFields` 覆盖声明字段，`ReplaceWhole` 整体替换 | 写 `writable_paths` 之外的字段 → `HOOK_WRITE_VIOLATION`，不采用 |
| collect | `no_change`、`contribute`、`defer`、`reject` | 按 `(slot, key)` 合并 | 同键不同物 → `CONTRIBUTION_CONFLICT`，列出生产者，后到者不获胜 |
| decide | `no_change`、`decide`、`defer`、`reject` | 第一个有效 decision 结束该扩展点 | 非法输出丢弃 |
| around | 该扩展点自己的输出 | `next()` 至多调用一次 | 类型系统保证：continuation 被消费 |

每个 handler 有该点的有限 `timeout_ms`；超时、错误与 panic 都被收容并留下诊断。失败策略
`report_and_continue` 跳过该 handler 的输出并继续，`stop_unit` 在记录已有结果后结束本次
派发。非法输出**永远**不被采用，与失败策略无关。

## 确定性顺序与冲突

同一扩展点内按 `before`/`after` 拓扑排序，无依赖项按稳定 handler id 决定次序。安装时刻、
异步完成时刻与消息紧急度都不是排序键。`before`/`after` 只约束同一扩展点；`requires` 是硬
依赖，缺失即拒绝候选图（可跨扩展点）。有环、缺硬依赖、schema 不符或主版本不符都拒绝候选，
**旧图继续工作**。

## 在途图租约

`snapshot()` 取得一张不可变图并登记一个租约；租约计数按图修订号统计，
`HookRegistry::await_quiescence(revision)` 等到该修订没有在途租约为止。卸载与换代等待的是
这个真实屏障，而不是猜测调用已经结束。请求封口时封的是 `HookSnapshot::binding()`：
`graph_digest`、图修订号、扩展点列表、handler 实现摘要与租约纪元。

## 上下文贡献的出处必须到达原始调用证据

一次 collect 派发的结果是 `Vec<ContextContribution>`，每条都带：

- `slot`（渲染位置）、`source`（plugin_id / plugin_version / artifact_digest / kind）、
- `scope`、`input_revision`、`lifetime`、`required`、`budget_tokens`、`tokens`、
- `data`（准备好的字节的对象引用）。

合并规则只有一份，在 `shenko_kernel::contribution::merge_contributions`：同一 `(slot, key)`
且生产者、数据引用、tokens、lifetime、required 完全相同 → 折叠为一条；任何不同 →
`CONTRIBUTION_CONFLICT`，诊断列出生产者。上下文策略的注册表调用同一个函数，所以 hook 路径
和政策路径不可能给出不同的合并结果。

贡献的出处由封口请求原样携带（`hook_graph`、`context_contributions`、`runtime_revision`、
`capability_revision`），因此原始调用记录可以指名每一个瞬时提醒的生产者、版本与工件摘要。
没有运行时服务时，封口明确失败，而不是封一份声称"跑过某个图"却从未跑过的请求。

## 生命周期：真实的更新与卸载

- `PluginRegistry::uninstall(&FiberHandle)` → Cordis 的 `FiberHandle::dispose()`：插件自己的
  generation cleanup 先跑（服务被撤回、注册被注销、worker 被 join），之后才返回。
- `PluginRegistry::update::<P>(&FiberHandle, P::Input)` → Cordis 的 `FiberHandle::update`：
  同一个 Fiber 身份、依赖与在途工作不变，新的 input 成为权威代际；被 veto 的更新保留旧 input。
- 两者都是上游合同的薄封装，没有第二套生命周期。

## 错误与恢复

| 情况 | 行为 |
| --- | --- |
| 候选图有环 / 缺硬依赖 / schema 或主版本不符 | 拒绝候选，旧图继续；诊断指名冲突的 handler |
| handler 超时、报错、panic | 收容为诊断结果；按点的失败策略继续或结束 |
| handler 返回该 mode 不允许的输出 | 丢弃并留诊断，永不采用 |
| collect 冲突 | `CONTRIBUTION_CONFLICT`；不采用该 handler 的输出，诊断列出生产者 |
| transform 写越权字段 | `HOOK_WRITE_VIOLATION`；不采用该 handler 的输出 |
| 卸载时有在途派发 | 等待租约屏障后再释放资源；不谎报完成 |
| 注册表服务不存在 | 明确错误；不用假的 0 或空视图代替 |

## 内核边界失败进入同一个 Store 日志（§45.14）

插件运行时有两类"无法继续/降级"边界不在调用者的错误路径里留第二条记录，它们直接观察进
subject 的同一份 runtime-error 日志：

- **Cordis 生命周期事故**：`Diagnostics` 导出器在 unknown 闩锁 false→true 跃迁时观察一次
  （每个事故一条日志）；之后的 `check_outcomes` 拒绝准入是同一事故的后果，不再重复记录。
- **Hook 图边界**：`publish` 的修订冲突（运行时自身的并发缺陷）与候选图准入拒绝
  （`declare_point` / `swap_generation` 校验失败——旧图继续生效，正是降级点）。

观察走 `shenko_kernel::errors::ErrorObservation`：assembly 在 genesis 之后挂接唯一的
store-backed `ErrorSink`（`bin/shenko/src/main.rs::StoreErrorSink`），写入
`Store::journal_runtime_error`——与 `subject_loop`、native child 等边界同一份日志、同一个
store；没有第二个 DB，也不触发任何硬编码自治动作。sink 挂接之前（启动早期、尚无 branch）
失败只留在 stderr + 计数（`pre_sink_failures` 如实 surfaced），不虚构日志条目；
`journal_runtime_error` 本身是 fail-soft（写失败计数、不传播），所以日志失败不会递归回观察
路径。这不是 blanket tracing subscriber：观察发生在具体内核边界，且非 `cordis:` 前缀的
tracing 记录不会触发观察。

## 尚未取得的运行期证据

本文件描述接口与目标行为。真实模型是否在这些扩展点上做出更好的判断、真实 provider 的
effect contract 是否与实际行为一致、以及长周期运行中的稳定性，都需要按
`docs/specs/verification.md` 的规则单独取证；源码与本文件都不是运行期证据。
