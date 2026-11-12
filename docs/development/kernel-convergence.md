# 内核收敛计划

本文说明代码怎样一步步向[内核合同](../specs/kernel.md)收敛。它是开发方法文档，不是产品合同，也不是实现完成的声明。来源见[决策 0007](../decisions/0007-kernel-and-reference-plugins-2026-11-11.md)。

本计划依据对提交 `dd28003` 源码的阅读写成，所引用的符号与依赖边界在 `db3970e` 上复核过。起草它的会话无法取得 Rust 依赖，没有编译或运行产品，所以下面每一步的工作量和可行性都是阅读源码后的估计。动手的人以实际构建和运行结果为准，发现估计不对就修改本文。

## 现状

内核合同九条里，[内核 1](../specs/kernel.md#kernel-1) 到[内核 5](../specs/kernel.md#kernel-5) 的机制已经在代码里；[内核 6](../specs/kernel.md#kernel-6) 的原则写在合同和注释里，尚未逐个入口核对；[内核 7](../specs/kernel.md#kernel-7) 到[内核 9](../specs/kernel.md#kernel-9) 只做了一部分。

扩展机制的现状是一个门、两个架子，外加三处绕路。

| 机制 | 现状 |
| --- | --- |
| 生命周期（`shenko-kernel` 的 `PluginRegistry`，Cordis） | 在用 |
| 能力架（`PluginRuntime`：提供者、能力、效果合同、说明） | 在用；原生任务处理器经 `NativeTaskHandlerPlugin` 登记 |
| 钩子架（`HookRegistry`：七个钩子点、五种模式） | 主体每步在四处派发，但生产装配没有登记任何处理器 |
| 绕路一：主体直接读认知目录 | `shenko-subject` 持有 `shenko-awareness-core` 的 `CognitionCatalog`，按 `cap.subject-frame`、`cap.attention-policy`、`cap.idle-perception`、`cap.child-role-frame` 四个固定名字读取 |
| 绕路二：独立的贡献注册表 | `shenko-context-policy` 的 `ContributionRegistry` 负责收集；`context.collect` 钩子点只有定义，主体没有派发它 |
| 绕路三：装配写死 | `bin/shenko/src/lib.rs` 里有三十多处手写的处理器登记，启停不由安装单元的状态决定 |

依赖方向的现状记录在[依赖边界基线](../state/kernel-boundary.json)：25 条“插件伸进内核内部”，7 条“内核依赖具体插件”。

承重代码的分布：名为 `shenko-kernel` 的 crate 约四千行源码，只含生命周期、能力目录与钩子图；决策线、封存与账本在 `shenko-store`（约四万八千行）和 `shenko-subject`（约两万三千行）里。`shenko-store` 的三十多个公开模块中，认知档案、研究、媒体、座位、联系人、额度、记忆、配置等是领域表，不是账本本身。

## 原则

- 每一步是一个可以独立构建和验证的小改动，在同一个改动里完成三件事：改代码，改写与之冲突的现行条款，更新依赖边界基线。
- 收拢，不重写。不新建平行框架；已有的门和架子就是目标形状。
- 被替代的旧路径在同一步里删除，不保留双路径和兼容别名。原始数据、经历与回执通过明确的迁移保留。
- 不为了让数字好看而拆分或合并 crate。
- 不删减需求。某一步让一条需求的实现位置变了，就更新它的实现引用。

## 建议的顺序

顺序按“风险低、收益大的先做”排列。各步之间的依赖在每步里写明；没有依赖的步骤可以并行。

### 第 0 步：先让最小单机闭环有真实证据

[决策 0006](../decisions/0006-meta-perception-continuation-2026-11-11.md)已经规定了最小单机闭环：真实输入进入耐久事实，主思考流接续，有限 child 取得真实结果，主体给出可观察输出，进程重启后从已有事实继续。在这条闭环取得真实运行证据之前，建议不开新的领域。收敛工作改变的是结构，它需要一条已知能跑通的轨迹来判断自己有没有破坏行为。

### 第 1 步：任务处理器合同下沉到内核接口

对应[内核 7](../specs/kernel.md#kernel-7)。

11 个插件 crate 依赖 `shenko-subject`。除 `shenko-exec-host` 之外的 10 个，从中使用的只有三类东西：任务处理器合同（`NativeTaskHandler`、`NativeTaskDescriptor`、`NativeTaskInvocation`、`NativeTaskHandlerPlugin`、`ChildToolCall` 等）、它的登记服务（`NativeChildService`、`NativeChildPlugin`），以及收件箱和发件箱的消息类型（`InboxEvent`、`InboxEventKind`、`OutboxMessage`）。

做法：

1. 把任务处理器合同迁入 `shenko-kernel` 的能力架。它本来就是“能力架上放什么”的定义。
2. `NativeTaskInvocation` 引用了 `shenko_store::TaskInvocation`。把这个类型和合同需要的其他纯数据类型下沉到 `shenko-types`。
3. 收件箱与发件箱的消息类型同样下沉到 `shenko-types`。
4. 各插件改为从接口 crate 引入，去掉对 `shenko-subject` 的依赖。

预期效果：依赖边界基线里“插件伸进内核内部”减少 10 条。这一步只移动类型和 trait，不改变行为，适合最先做。

### 第 2 步：沙箱装配移出执行插件

对应[内核 7](../specs/kernel.md#kernel-7)。依赖第 1 步。

`shenko-exec-host` 的认知试用沙箱（`cognition_trial.rs`）直接运行主体循环，引用了 `loop_runner` 和 `dispatch`。运行一个完整的主体是装配行为。把沙箱的装配部分移到装配层，执行插件只保留处理器。

预期效果：最后一条“插件依赖主体”的边消失。

### 第 3 步：角色、空闲、注意力经钩子架到达主体

对应[内核 7](../specs/kernel.md#kernel-7)与[内核 9](../specs/kernel.md#kernel-9)。

1. 新增决定型钩子点，分别回答四个问题：主体的角色框是什么，空闲时主体感知到什么，注意力预算是多少，child 的角色框是什么。名字由实现者按现有钩子点的命名习惯确定。
2. `shenko-awareness-core` 在自己的生命周期里登记这些钩子点的处理器，从安装单元里取当前版本的正文。歧义（两个不同单元提供同一能力）仍然报告给主体，不由处理器替主体选择。
3. 主体改为派发钩子，不再持有 `CognitionCatalog`，去掉对 `shenko-awareness-core` 的依赖。

需要保住的既有行为：角色框改变时 `profile_digest` 随之改变，上下文窗口按既有合同重建；钩子图在封存与发送之间被替换是类型化冲突；注意力数值仍被夹在[内核 2](../specs/kernel.md#kernel-2)的界限之内。

这一步改变了提示文本到达模型的路径，按[本地模型行为验收](local-model-behavior.md)复测。

### 第 4 步：只留一个收集点

对应[内核 7](../specs/kernel.md#kernel-7)。可与第 3 步并行。

1. 主体在上下文投影阶段派发 `context.collect` 钩子点。合并规则已经在内核里（`merge_contributions`），只有一条。
2. 记忆的干预贡献改为 `context.collect` 的收集处理器。
3. 删除 `ContributionRegistry` 的收集路径。使用记录（哪次请求用了哪条贡献）保留为封存输入的一部分。
4. 同步改写[上下文策略合同](../specs/context-policies.md)里关于贡献注册表的条款。

### 第 5 步：删除编译进去的人格文本

对应[内核 9](../specs/kernel.md#kernel-9)。依赖第 3 步。

`SUBJECT_CONTRACT`、`IDLE_CONTINUATION_PROMPT` 和 `ROLE_CONTRACT` 的内容已经以种子单元的形式存在（`subject-frame`、`idle-perception`、`subject-child-frame`）。删除这些常量；没有提供者时，运行时给出类型化的缺失事实，做法与“缺少钩子运行时是类型化拒绝，而不是封存一个空图”一致。

输出协议的说明（控制行语法）保留，它是内核 ABI 的说明。

同步改写 [57.1](../specs/subject-runtime.md#spec-57-1) 与 [57.2](../specs/subject-runtime.md#spec-57-2) 里“没有提供者时用编译回退文本”的条款。按[本地模型行为验收](local-model-behavior.md)复测。

### 第 6 步：主体不再引用具体插件

对应[内核 7](../specs/kernel.md#kernel-7)。这是几步独立的小改动，每条依赖边一步。

| 依赖边 | 方向 |
| --- | --- |
| `shenko-subject` → `shenko-awareness-core` | 第 3 步完成后消失 |
| `shenko-subject` → `shenko-memory` | 第 4 步完成后消失 |
| `shenko-subject` → `shenko-context-policy` | 上下文策略是每步都需要的，接口定义下沉到内核接口，策略实现留在插件 |
| `shenko-subject` → `shenko-broker` | 内核接口定义发送边界与回执接收者（[内核 4](../specs/kernel.md#kernel-4)、[内核 5](../specs/kernel.md#kernel-5)），broker 实现它 |
| `shenko-subject` → `shenko-quota`、`shenko-store` → `shenko-quota` | 额度是准入策略，经钩子架或 broker 的策略进入；额度表随第 8 步迁走 |
| `shenko-subject` → `shenko-identity` | 人格是可修订内容，经钩子架到达主体 |

### 第 7 步：处理器的启停由安装单元决定

对应[内核 8](../specs/kernel.md#kernel-8)。

1. 每个内置的原生处理器对应一个 `code` 单元，工件是内置的。
2. 装配代码遍历已启用的单元来登记处理器，替换现在的手写清单。停用一个单元，就撤回它那一代的登记。
3. 默认发行组合补上元感知与元 awareness 单元。它们是普通单元，按[awareness 成效规则](awareness-first-plugins.md)在沙箱里与停用基线对比；没有跑过的单元只能算暂定，不因为默认安装就算有效。

### 第 8 步：账本瘦身

对应[内核合同的判据](../specs/kernel.md#判据)。这是工作量最大的一组，放在最后，每个领域一步。

1. 在内核接口里定义插件可以使用的窄账本接口：事务、内容存储、意图与回执、对原始事件的引用。
2. 领域表连同它们的迁移搬回各自的插件：认知档案、研究、媒体、座位、联系人、额度、记忆、配置等。`shenko-store` 只保留决策线、步、请求回执、效果台账、原始会话和内容存储。
3. 每次搬迁都通过明确的格式迁移保留原有数据（[内核 3](../specs/kernel.md#kernel-3)）。同步改写[耐久状态合同](../specs/durable-state.md)里相应的归属说明。

预期效果：“插件伸进内核内部”里指向 `shenko-store` 的 14 条逐步消失。

### 随时可做：让“一步”读得出来

`execute_step` 是一个将近一千行的函数，装配入口 `run` 约一千八百行。不改变行为，把 `execute_step` 按步的阶段拆成具名的函数：暂存输入、投影上下文、S0 封存、发送、记录结果、S1 提交。阅读的人应当能从函数名看出[主体运行时合同](../specs/subject-runtime.md)里的那张阶段表。这一步与并行改动冲突的可能性大，挑一个主体循环没有其他人在改的时间做。

### 核对：内容不携带权限

对应[内核 6](../specs/kernel.md#kernel-6)。这是一次核对，产出是证据记录，不一定产生代码。

逐个入口确认内容不会带来权限：渠道入站与管理员身份的指派；经 `compat.mcp`、`compat.skill` 导入的第三方材料；主体自己编写单元的路径；child 工具调用的授权引用；Web 界面输入。发现某个入口会让文字提升权限，就把它记为缺陷修复。

## 依赖边界检查

```bash
python3 scripts/check_kernel_boundary.py
```

这个检查只读工作区的清单文件，是结构检查。它不构建、不运行产品，也不说明任何 crate 的代码是否正确。

角色写在[基线文件](../state/kernel-boundary.json)里，是数据，不是代码：

- 接口：插件可以依赖的 crate。目前是 `shenko-types` 与 `shenko-kernel`。
- 内核内部：今天持有决策线、封存的步与账本的 crate。目前是 `shenko-store` 与 `shenko-subject`。
- 装配：把产品接起来的 crate，可以依赖一切。目前是 `shenko` 与 `shenko-seed`。
- 其余工作区成员都是插件。

它检查三个方向：插件依赖内核内部；内核内部依赖插件；接口依赖接口之外的 crate。测试依赖不计入，因为一个测试可以装配整个产品而不让 crate 本身依赖它。

基线是棘轮。出现基线里没有的违例，检查失败。基线里的某条违例已经消失时，检查同样失败，直到用 `--update` 把它从基线里删掉；这样改进一旦发生就被锁住。`--update` 只删不增。

随着收敛推进，角色会变化。例如第 1 步之后，任务处理器合同属于接口；第 8 步之后，`shenko-store` 里留下的部分才是真正的内核。调整角色时在同一个改动里说明理由。

## 每一步怎样验证

沿用[适量验证](../../.agents/skills/shenko-dev-verify/SKILL.md)：用最少的有效证据回答这一步的具体风险。

- 移动类型和依赖的步骤（第 1、2、6 步）：构建通过，相关的既有测试集通过，依赖边界检查的基线减少。
- 改变提示文本或事件投影路径的步骤（第 3、5、7 步）：除上述之外，用真实模型按[本地模型行为验收](local-model-behavior.md)采样，与改动前的成功率对比。
- 搬迁数据的步骤（第 8 步）：迁移前后原始事实可读，重启后步序列接续。
- 每一步都运行 `python3 scripts/check_docs.py`，它只检查文档完整性。
