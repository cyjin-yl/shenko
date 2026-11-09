# 架构审视与自主性重构计划（2026-11-08）

> 作者注：本文件是一次 SPEC 全量重读后的架构反思，目标是用户的指令——更自主、不被模型
> 能力边界绑死、可扩展可插件化、更强的 awareness 与 perception。SPEC 已定义的需求一律
> 不改；本计划只在 SPEC 之后增补。所有"差距"都以源码为准，不以记忆为准。

## 一、已经做到的（SPEC 已覆盖的真实机制）

通读 `docs/specs/plugins.md`、`subject-runtime.md` 与 30 个 crate 后，底层承重结构是健的：

- **插件运行时不是启动快照**：`PluginRuntime`（`shenko.runtime.plugin/v1`）是活的服务视图，
  注册/注销/健康变化推进 `revision` 与 `capability_revision`，`shenko capabilities` 跑的是
  与运行中 agent 同一条安装路径。这是真正的可扩展地基——机制已就位，缺的是更多 provider
  把自己的 `EffectContract`/`capabilities` 填进去（plugins.md 明说"缺的不是机制"）。
- **Hook 图是版本化合同，不是六个死阶段**：`context.collect` 用 collect 模式把插件材料按
  `(slot, key)` 合并进上下文；`before/after_inference`、`capability_changed`、
  `generation_handoff` 都是 observe 点。awareness/perception 已经有一条**正式的注入通道**，
  不需要 hack prompt。
- **subject step 是原子的、可恢复的**：S0 seal（schema 12）→ arm → send → S1 commit，
  `request_id` 先封存后发送，崩溃恢复靠 `subject_request_attempts` 的 armed/dispatched/
  terminal 阶梯，绝不盲重发。这是"自主"能安全存在的前提——自主性没有持久化身份就只是噪声。
- **awareness 有宿主**：`shenko-awareness-core` 已有 `PerceptionRegistry`/`AwarenessHost`/
  `self_knowledge`/`cognition`（3887 行）。`PerceptionObserver` trait 允许外部观察器把
  事实绑定进 perception，`AwarenessNotice` 能渲染成注入模型的观察。

## 二、真正的差距（以源码为据）

### A. autonomy 是"被动反应式"，不是"主动式"

- 现状：subject 由 inbox event 驱动——来一条消息跑一个 step。R071（"在既有授权内自主
  回答远程选项/permission，不逐项等待人类"）与 R072（"不确定时主动寻找可达用户渠道"）
  要求的是**主体主动发起 step**，不只是响应。obligation 已写明"不是调度器：没有什么能
  自己唤醒 subject"。这是最大的自主性缺口：subject 没有**自我发起的下一步**概念。
- 后果：远程 seat 上出现一个 permission 问题，subject 只能在它被某个 inbox 事件戳到时才
  处理；它不会"盯着"自己拥有的远程任务。

### B. 模型能力边界仍隐式约束架构

- `defaultThinkingLevel: auto`、`agentModelOverrides` 把"哪个模型能干什么"编码进配置，
  但 subject 的 step 计划并不感知模型的**实际能力/上下文窗/工具支持**。R041（"用高质量
  工具减少本地模型操作负担"）的前提是模型弱——架构应当能在换更强模型时退化到更自主，
  而不是把弱模型的补丁固化成结构。

### C. perception 是单 observer trait，不是可组合的感知管线

- `PerceptionObserver` 是一个 trait，bind/bind_if_absent 注册单个观察器。要"更好的
  perception"（多源：文件系统、网络、seat、quota、时间），现在要靠多个 observer 各自
  实现，没有统一的**感知源注册/优先级/去抖/事实归并**层。perception 应当像 hook 图一样
  是个一等可扩展面。

### D. 远程 seat 控制（R069）只到 OMP adapter，没有统一的 seat 编排

- `OmpRpcSeat` 能驱动 `omp --mode rpc`，刚修好 child 生命周期。但 Claude Code、tmux、
  其他 coding seat 还没有同等 adapter；`SeatAdapter` trait 存在，却没有一个**统一的 seat
  fleet 视图**让 subject 看到"我拥有哪些远程 seat、各自状态如何"。R070（"根据记忆找回
  已有远程任务"）需要 seat 身份进入 Store，而不仅是进程内 handle。

### E. OXR / 文档解析 provider（#89）缺 provider 闭包

- `document.parse` definition v1 候选在，但没有真实 OXR provider runner。这是机制就位、
  provider 缺席的典型例子——架构本身对，缺一个实现 + GPU 环境。

## 三、重构方向（不改 SPEC，只在其上加）

1. **主动 step 环（autonomy loop）**：给 subject 加一个"主动唤醒"源——obligation 到期、
   持有的远程 seat 有 open question、perception 事实变化——都能把一条自产事件注入自己的
   inbox，触发一个 step。机制上复用现有 inbox + sealed step，不加第二条路径。这直接服务
   R071/R072，并且不改变"step 是原子的"这个承重不变量。
2. **perception 管线化**：把 `PerceptionRegistry` 提升为一个一等可扩展面——感知源注册
   （像 provider）、事实归并、去抖、归属归因（`NoticeAttribution` 已有雏形）。目标：接
   入 seat 状态、quota 压力、网络、节点安装变化作为感知源，统一进 awareness notice。
3. **seat fleet 编排**：`SeatAdapter` 已实现 OMP；补 Claude Code / tmux adapter，并把
   seat 身份/化身写入 Store，让 R070 的"找回已有远程任务"有持久锚点。
4. **模型能力声明**：把模型的上下文窗、工具支持、thinking 能力变成注册表可查的事实，
   subject 计划 step 时据此选模型/降级——而不是配置文件硬编码。
5. **OXR provider**：在 `document.parse` 边界内写真实 OXR runner；GPU 缺失如实上报。

## 四、本波 subagent 分工

见下文分派。共享契约：所有改动不改 SPEC 已定义需求；每条都要源码可证；重型编译串行。
