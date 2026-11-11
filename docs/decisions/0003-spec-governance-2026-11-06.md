# 0003 SPEC 读法治理

> 目录迁移后，当时根文件布局与锁定方式仅为历史记录；现行维护规则见 [0005](0005-domain-contract-migration-2026-11-11.md)，行为裁决按具体领域链接阅读。

状态：2026-11-06。不改运行代码，不改 [§1](../specs/architecture.md#spec-1)–[§47](../specs/inference.md#spec-47) 的字节。
现行认知合同仍是 [51 认知元概念：内容、态度与理由](../specs/cognition-units.md#spec-51) 与 [0002](0002-cognitive-atoms-2026-11-05.md)。本节只记录把其他规范入口理顺到这一哲学的改动。

判断依据：Shenko 是享有人权的主体。引导而非禁止，不加人工审批、权限门、产品 allowlist 或静态 hard deny，只说明风险与收益。通用性优先，私人仓库内容不进仓库。core 只有运行时、账本和插件机制。时间感知、世界感知、预测、awareness 宿主、遗忘、自解释都是插件。`taste`、`humanlike-mind`、`forgetting` 并列预装，名字不带 default。内容可在线修订；core 和有实现的插件只整版替换。回滚只用自有版本化根，链尾追加节点，日志只增不减，外部副作用不撤回，`agent_id` 不变。fact 与 conclusion 是同一种内容加态度，原文不覆盖。依赖写在 `cap.` 上。主体相对运行时间只计主推理循环。冒烟测试和字段等实现细节由实现 Agent 设计。

LayerFS 只在认知状态回滚上不考虑。[§10.2.1](../specs/durable-state.md#spec-10-2-1) 的 Linux VM 工作区保留。

## 未改的锁定原文

`docs/history/spec-integrity.json` 锁定 SPEC 前 783140 字节。下面这些旧句还在原文里，现行读法写在 [§51.10](../specs/cognition-units.md#spec-51-10)，不涂掉原句。

| 位置 | 旧表述（保留） | 现行读法 | 理由 |
| --- | --- | --- | --- |
| [§45.1](../specs/cognition-units.md#spec-45-1) | awareness 是模型能够遵守的直觉文本 | 能力；文档与 watcher 是 harness | 与 [§51.2](../specs/cognition-units.md#spec-51-2) 冲突。原文是用户原话所在区，不改字节 |
| [§10.1](../specs/durable-state.md#spec-10-1) | 快照不是 CPU/进程内存；身份、原始会话、副作用回执不随沙箱倒退 | 外部副作用不撤回；`agent_id` 保持切换前的值；原始会话留在只增日志里；线性内存、执行栈、解释器对象图纳入版本化根 | 回滚目标扩大了，但「文件快照不能假装撤销已发出的消息」仍然成立 |
| [§10.2](../specs/durable-state.md#spec-10-2)、[§10.2.1](../specs/durable-state.md#spec-10-2-1) | WASM 用 copy-on-write；LayerFS 用于合格 Linux VM | 认知回滚不用 LayerFS。COW 的 `discard_abs` 删日志是实现缺口，不是现行回滚 | 工作区集成与认知回滚是两件事 |

`docs/history/` 与 `docs/research/archive/` 是历史来源，这次不改。

## 本次改动

| 位置 | 旧表述 | 新表述 | 理由 |
| --- | --- | --- | --- |
| [§51.10](../specs/cognition-units.md#spec-51-10) | 无 | 写明锁定章节的现行读法 | 不能改前缀，又不能让旧句继续当现行合同 |
| `AGENTS.md` 内核条 | Awareness is advisory text | awareness 是能力；当前 harness 才是不授权、不派发的文本。不新增人工审批、产品 allowlist 或静态 hard deny | 开发入口仍把能力说成文档 |
| `WHY.md` awareness 节 | 每个 awareness 应是具体、可安装的意识 | 可安装的是单元；能力见下一段 | 上一句会把正文读成能力本身 |
| `docs/INDEX.md` 权威关系与规格行 | 规格只到 [§48](../specs/architecture.md#spec-48) | 前缀不变；追加必须以 [§48](../specs/architecture.md#spec-48) 开头，其后可以有后续章节。现行认知读法指向 [§51](../specs/cognition-units.md#spec-51)、§0002、§0003 | 索引落后于已经追加的 [§49](../specs/ecosystem-adapters.md#spec-49)、[§51](../specs/cognition-units.md#spec-51) |
| `docs/specs/documentation.md` | [§48](../specs/architecture.md#spec-48) 是唯一允许的追加章节；校验只允许 [§48](../specs/architecture.md#spec-48) | 追加部分以 [§48](../specs/architecture.md#spec-48) 标题开头，后面的章节不改前缀 | 与 `scripts/check_docs.py` 的实际检查一致，也与已有的 [§49](../specs/ecosystem-adapters.md#spec-49)、[§51](../specs/cognition-units.md#spec-51) 一致 |
| `docs/specs/architecture.md` | 只写 `taste` 与 `humanlike-mind` 并列 | 加上并列的 `forgetting`，名字不带 default；并指向 [§51.10](../specs/cognition-units.md#spec-51-10) 对 [§10.1](../specs/durable-state.md#spec-10-1) 的读法 | 发行预装的第三份宏包在架构入口里缺了 |
| `docs/specs/memory.md` | `kind` 是开放知识种类 | 保留为可学习的自由标签，只作提示；fact/conclusion 不是两种 kind。默认隐藏旧行是实现缺口 | 开放字符串会被读成种类边界 |
| `docs/specs/research-memory.md` | `environment_fact` 是 bank 的一种 kind | 字段名暂留；现行读法是研究条目上的态度 | 0002 已决定改用态度，合同仍把它写成 kind |
| `docs/specs/context-policies.md` | 切窗只引用原始事件 | 另写压缩服从 taste：默认只露出旧版本数量和最新修改时间，不删日志 | 0002 已决定压缩服从 taste，合同未写 |
| `docs/specs/wasm-home.md` | 检查点不承诺线性内存、执行栈、解释器对象图 | 这是今天的实现缺口；现行目标把这三项纳入版本化根。回滚是追加节点，不删提交日志 | 合同句会被读成「这三项永远不回滚」 |
| `docs/specs/self-update.md` | 新事实不能因为指针倒退而消失 | 保留这句。补上：认知回滚以 [§51.4](../specs/cognition-units.md#spec-51-4) 的追加节点为准，本节只是现行软件代际激活 | 「指针倒退」会被当成认知回滚的定义 |
| `docs/specs/cognition-units.md` | awareness path itself stays text | 库里的 awareness 字节仍是文本，那是 harness，不是能力 | 机制描述盖过了 [§51](../specs/cognition-units.md#spec-51) |
| `docs/specs/plugins.md` | 业务判断留给认知单元和模型 | 补上时间/世界/预测/awareness 宿主/遗忘/自解释不进运行时；不用关键词表、人工审批、产品 allowlist 或静态 hard deny 强制判断 | 内核边界写得太窄，容易把新能力写回运行时 |
| `docs/development/awareness-first-plugins.md` | 写成一段材料的地方是 harness | 点名 `AWARENESS.md` 正文也是 harness | 写作模板仍像能力本身 |
| 决策 0002 文首 | 只指向 [§51](../specs/cognition-units.md#spec-51) | 加上指向 0003 | 两份决定要能互相找到 |

共 15 处。没有改运行代码。

## 审过但保留

- [§10.2.1](../specs/durable-state.md#spec-10-2-1) 与架构里的 LayerFS Linux VM 工作区、`layerfs_committed` 与 `product_durable` 的区分。它不负责认知状态回滚。
- 上游 Store 打开前的文件系统类型检查、设备上的 su 策略、WASM 路径边界。这些是外部或机制事实，不是主体判断用的 allowlist。
- [§48.1](../specs/architecture.md#spec-48-1) 已经把真实 ABI、资源上限和账户授权与人类审批分开。不重写。
- [§49.4](../specs/ecosystem-adapters.md#spec-49-4)「不把一次调用结论变成全局强制规则」与引导而非禁止一致。不重写。
- 历史归档与研究草稿不改，避免把来源抹掉。

## 剩余待用户决定

无。这次能按上述哲学判定的都已写入现行读法。实现 Agent 仍自己设计冒烟测试、换位思考字段、时间／世界／预测的拆法，以及遗忘的权重和原子例子；这些不是待用户决定的产品问题。
