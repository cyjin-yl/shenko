# Pi Durable 与工具能力自改进核验

范围：准确定位 Pi Durable 官方项目、核验持久执行/重放/补偿的真实保证，并把 tmux TUI 高亮丢失作为 Agent 自主补齐工具能力的具体设计案例。只研究和提出 SPEC 合同，没有安装、运行工具演示或开发。

## 1. 准确项目与固定版本

- **官方实现是 `earendil-works/pi` 主线的 `packages/durable`**，npm 包名 `@earendil-works/pi-durable`，不是必须另找的“durable 分支”。本轮固定 [commit c60de42e98428dc1ef7440d78c23f5915966cdb3](https://github.com/earendil-works/pi/commit/c60de42e98428dc1ef7440d78c23f5915966cdb3)，时间 **2026-10-22 19:14:23 UTC**，该 package version **1.4.0**。
- [README](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/README.md) L3 仍标 Experimental，API 可能跨 release 变化；[package.json](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/package.json) 依赖同版本 pi-ai/chord/pi-codemode/pi-telemetry，Node engine >=22.19.0。Rust 产品可以借鉴合同或做适配，不能把 TypeScript 库直接称为已具备 Rust/WASM 实现。
- `earendil-works/pi-durable-cloudflare` 是官方示例；`disler/durable-pi` 是另一个应用项目，不能混作核心上游。本轮不以这些周边项目的功能代替 durable 包的源码证据。
- tmux 固定 [commit 1c418f0b62d131b57d2e113362c2911867102652](https://github.com/tmux/tmux/commit/1c418f0b62d131b57d2e113362c2911867102652)，**2026-10-22 15:10:50 UTC**。具体远端安装仍需探测自己的版本，不能默认已带主线全部新 flags。

## 2. 对用户判断的准确吸收

用户指出 Pi Durable 架构比进程内清理回调强，这一点有明确依据，应该采纳为正式参考：它把**工作本身、阶段、父子所有权、取消意图与结果**持久化，重启可以继续运行阶段或 abort handler，而非期待已死亡进程执行 finally/cleanup。

仍需区分三层保证：

| 层次 | Pi Durable 的实际设计 | 不应扩大成的承诺 |
| --- | --- | --- |
| 进程崩溃后的工作恢复 | committed task/checkpoint、inbox、entries、documents 保留；reopen 对账，resume 调度 | 活进程栈/任意内存自动复活 |
| 持久取消与业务补偿 | durable abort mark、子任务自下而上结束，应用提供的 abort handler 执行补偿；未完成处理可在重启后继续 | 框架知道每个外部系统怎样撤销，或者任意效果都可逆 |
| 断电下不丢已承诺状态 | 取决于 storage durability。默认 Node SQLite WAL + NORMAL 明说最新 commit 可能因断电/host failure 丢失；JSONL 可设 fsync | 只要叫 durable，就已经满足“断电后绝不丢最新已 ACK 操作” |
| exactly-once | 同 requestId 可复用 submission；已记账 nested tool result 可重放；有外部幂等支持时可安全重试 | 所有 shell、GUI、消息、付款自动且无条件 exactly-once |

### 最值得借鉴的架构

**统一原子提交线。** Session 的一次 commit 可以同时写 immutable entries、完整 task record 与 Chord document changes；先存储成功，才发布进展。外部副作用明确不在 Session mutation transaction 内。[normative spec L41–78](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L41)

**不是只有 transcript。** task 是有版本的持久 state machine；可 pending/running/waiting/sleeping/completing/terminal，checkpoint 与创建 child tasks 可同事务提交。reopen 将原 running 工作恢复为 pending，保留 checkpoint 与 abort mark；没有当前定义或无法迁移的任务保持 blocked，等待正确代码。主线还会检测 crash loops，避免确定性 OOM 任务无限复活。[spec L1993–2043](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L1993)、[L2181–2229](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2181)

**effect sandwich。** 先 commit intent，再执行 effect，最后 commit receipt/outcome。重启看到 intent 时，效果可能已发生；handler 必须选择幂等重试、用外部 handle 查询或记录 interruption。终态 task 是持久 result receipt，但本地 receipt 不替代外部系统的真实确认。[spec L2045–2084](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2045)

**取消是持久的工作。** abort mark 提交后才向 active invocation 发取消信号；重启再次推导所属子任务的取消。父任务要等待普通 owned work 完成才成为 terminal；background 是明确的所有权边界。abort 自下而上，当前代码支持应用在 abort handler 内补偿或创建另外的持久补偿工作。正常 close 不等同 abort：close 不宣告业务取消，未完成工作在 reopen 后可继续。[spec L2137–2179](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2137)、[L2390–2407](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md#L2390)

**工具重放不是一律重跑。** 实际 tool task 在 execute 前保存最终参数和 replay policy；默认 unsafe。重启时，只有持久记录和当前实现都声明 safe 才重跑，否则提交 interrupted 结果及已持久 partial output。已有专项测试覆盖 unsafe 中断、两侧 policy、安全重跑环境改变、真实 bash 中断等。[tool.ts L168–197](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/src/harness/tool.ts#L168)、[recovery tests L108 起](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/test/harness-tools-recovery.test.ts#L108)

**nested-call 与 Code Mode journal。** nested calls 有稳定位置或显式 key，重跑调用者可复用已完成或仍在运行的子调用。Code Mode 限制时间/随机等不确定性、先 journal 返回值再交给脚本，重启重放这些值；检测路径分叉、sandbox fingerprint/version 变化或超时。这个机制避免重跑已记账的内层工具；未记账且已经发生的外部效果仍要自己的幂等。[README L307–368](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/README.md#L307)

### 两个不能照抄的示例推论

1. 官方 `12-recovery.ts` 在 console.log 前写 `printed-n=true` memo，用来演示避免已打印后再次打印。**推论：**若在 memo 落盘后、打印前崩溃，重跑会跳过尚未发生的打印。因此“先记完成标志再做任意外部操作”只能避免一类重复窗口，不能同时保证完成；生产 outbox 不能把这种示例直接当 exactly-once。[example L26–34](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/test/examples/12-recovery.ts#L26)
2. `11-tasks.ts` 支付示例明确假设服务按 key 幂等；`23-child-tasks.ts` 的“退款”是 fake bank 的内存 Set 删除，展示任务所有权和补偿顺序，不是跨银行事务实现。真实退款仍需要可持久核验的 receipt、幂等 key、失败处理；取消邮件/群消息/已执行命令通常没有普遍逆操作。[payment example L10–14](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/test/examples/11-tasks.ts#L10)、[child tasks example L24–58](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/test/examples/23-child-tasks.ts#L24)

### 断电、单写者与唤醒的边界

- SQLite README 明示 WAL + synchronous=NORMAL；实际 Chord Node adapter 也在 L189–190 设为此值，options 只有 auto-checkpoint 和 busy timeout，没有可直接传入的 synchronous 选项。要满足更强断电合同，需在我们的 storage adapter 中选择并验证更强持久设置、实际 fsync/目录同步与存储硬件语义，不能写一句“换 FULL”就当所有设备通过了测试。[README L752–783](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/README.md#L752)、[Chord SQLite node L178–192](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/chord/src/sqlite/node.ts#L178)
- JSONL `fsync` 默认为 false；源码在启用后调用 filesystem 的 flushFile；并且完整历史保留在内存，README 建议长期 agent 用 SQLite。本轮没有对 crash/powercut 真机测试，不能借源码接口宣称所有物理设备断电保证成立。[JSONL storage L257、L295](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/src/storage/jsonl/storage.ts#L257)
- 上游要求一个 process owns storage，没有跨进程 locking。我们的多节点唯一主体、epoch/fencing 仍必须自己实现；SQLite 并发写锁不等于应用层唯一调度者。[README L783](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/README.md#L783)
- scheduler 在内存中，durable state 不会自行产生一个活进程。Cloudflare 官方 PiHarness 通过 Lifecycle job/alarm 保证 reopen；本地需要 supervisor/boot recovery，Android 需要实际 OS 生命周期适配。该唤醒机制用于失效恢复和有限任务等待，不能替换用户已确认的永续主体为低频 Tick。[Cloudflare 官方 PiHarness 文档](https://developers.cloudflare.com/agents/harnesses/pi/)
- 每 conversation 的持久 provider sessionId 有助于 affinity，fork/child 会获得新 ID；这不是 KV hard residency、slot pin 或全局推理并发保证。[README L127](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/README.md#L127)

## 3. tmux capture-pane 高亮丢失的准确原因与可用能力

### 原生接口已能解决的一层

默认 capture-pane 从 tmux 当前屏幕网格取字符并序列化；不启用序列输出时，颜色/背景/反显等属性不会作为字符内容传回。原生 **`capture-pane -e`** 专门加入文本和背景属性的 escape sequences。源码只有在有 `-e` 时才设置 `GRID_STRING_WITH_SEQUENCES`，再调用 `grid_string_cells`，所以“capture-pane 必然拿不到高亮”不成立。[官方 manual L2896–2933](https://github.com/tmux/tmux/blob/1c418f0b62d131b57d2e113362c2911867102652/tmux.1#L2896)、[cmd-capture-pane.c L336–357](https://github.com/tmux/tmux/blob/1c418f0b62d131b57d2e113362c2911867102652/cmd-capture-pane.c#L336)

文档示意命令（本轮未执行）：

```sh
tmux capture-pane -p -e -t %42
```

这里 `%42` 仅是示例；实际必须使用已 live-verify 的稳定 pane identity，不能复制为真实目标。

| 选项/接口 | 精确用途 | 不能误解成什么 |
| --- | --- | --- |
| capture-pane `-p` | 输出到 stdout，非 paste buffer | 不会自行保留样式 |
| capture-pane `-e` | 输出文本/背景属性 escape sequences | 不提供 AskUserQuestion 的可靠语义，更不等于原始字节流录像 |
| capture-pane `-C` | 将非打印字符转义为 octal | 转义后仍需按该合同解码；不是自动渲染高亮 |
| `-N` / `-J` | 保留行尾空格；J 还连接 wrapped lines | 视觉坐标场景不能随意 J 后再当原行列使用 |
| `-S`/`-E` | 指定历史/可见行范围；默认仅可见内容 | 历史里出现选项不能证明当前仍等该答案 |
| `-a` / `-M` | 切换 alternate buffer 或 pane mode screen | 不应对所有 TUI 无条件追加 a；源码 a 取 saved_grid，普通路径取当前 base.grid，选错缓冲区反而看旧内容 |
| tmux control mode | 持续接收 `%output pane-id value` 和 layout/mode 事件 | value 的控制字符与反斜杠为 octal，需要解码并送终端状态机；它本身不替 Agent 判断问题语义 |

来源：[screen 选择代码 L271–294](https://github.com/tmux/tmux/blob/1c418f0b62d131b57d2e113362c2911867102652/cmd-capture-pane.c#L271)、[control mode manual L9160 起、%output L9264](https://github.com/tmux/tmux/blob/1c418f0b62d131b57d2e113362c2911867102652/tmux.1#L9160)。

### 根因不能只归咎于 tmux

即便已经带 -e，高亮仍可能在后续链路丢失：SSH wrapper 去 ANSI、JSON/text renderer 过滤控制字节、模型只收到纯文本摘要、错误的 screen buffer、只看历史或旧帧、未建模 cursor/selection，或应用使用不在普通 pane 内容里的 overlay。需要在每一层记录可控的原始证据引用和编码声明，比较原始 capture、传输 payload、parser grid、模型收到的结构化观察，定位信息在哪一步消失。

适合给较小模型的是结构化屏幕观察：pane/process/seat epoch、尺寸、cursor、带 attributes 的 text runs、候选 selected row、观察时间/revision、confidence 与原始捕获 handle。这比每次将全部 ANSI 灌进 context 更易用。保留 ANSI 属性可以让 worker识别选择状态，但选择状态不自动授予操作权限；先有 Claude/OMP SDK/RPC 问题事件时优先使用。

### Agent 自发补工具的具体案例

1. 主体接到“远端选项任务停滞”的事件与旧观察，委派诊断孩子；主体本身不运行 tmux 或改文件。
2. 孩子发现多个选项文本齐全但 selected 状态缺失，提出 **ObservationGap**，记录当前工具版本、tmux 版本、seat epoch、需要的证据，不把“不知道哪项选中”猜成某一项。
3. 孩子先查已有 schema/官方 manual，再进行授权内只读探测；若原工具忘了 -e，优先修 wrapper 参数与编码处理。若 attrs 已保留但模型看不懂，再增加终端状态解析与可解释 selected-row 投影；持续 TUI 可评估 control mode。没有必要一开始重造 PTY 全栈。
4. 在候选插件版本上用受控终端样例验证颜色/反显、宽字符、resize、alternate screen、重复文本、spinner、已消失的问题。需要新代码时由自改进孩子实现并按既有候选 gate 激活，不把每次提升变成人工批准。
5. 在真实 seat 上先重新观察；只对仍有效的问题和 lease 输入，核验按键后的实际状态。记录新能力、适用条件、反例及回滚版本，后续同类任务从 registry/discovery 得到更强工具。
6. 诊断与提升也采用持久任务：崩溃后知道是在“仅发现问题”“候选已构建”“已激活但未验证”还是“真实操作已发出待核验”。工具改好了不等于远端任务已执行，二者分别提交 receipt。

## 4. 建议写入 SPEC 的九条落地合同

| ID | 可落地合同 | 关键验收 |
| --- | --- | --- |
| PD-01 | **统一持久提交**：主体进展、inbox admission、任务/checkpoint、必要文档与结果引用形成原子批次；只发布已成功持久化的版本。明确每种 storage 的 crash/power-loss 保证及 ACK 点。 | 在 commit 前后、发布前后与断电模拟点故障；不显示未提交成功，不丢已承诺且属于所宣告耐久等级的 intent。 |
| PD-02 | **可恢复任务树与唯一主体**：保存 owner、task kind/version、checkpoint、runtime/seat、scope、epoch；reopen 先对账并恢复未完成任务。多节点 fencing 另行实现，不能拿 Pi 的单进程提交线当分布式锁。 | 重启、双节点争抢、旧 lease 迟到、代码定义暂缺；主体仍只有一条，任务不因客户端断线而丢失。 |
| PD-03 | **效果分级与持久回执**：副作用先记 intent 与稳定 operation/idempotency key，再执行，再记外部 receipt/outcome；可幂等重试、可查询、可补偿、不可逆/未知要区别。 | 在“外部成功、回执未提交”窗口断开；重启按 key 查询或重试，不盲目再发消息/提交/键盘输入。 |
| PD-04 | **持久取消与补偿**：取消先提交 intent，再中止/对账执行；普通 owned 子任务自下而上处理。补偿是可恢复的任务，由 provider 定义真实逆操作；无法撤销的效果保留已发生事实。 | 补偿期间再崩溃、部分成功、退款/撤回失败；状态可继续，不能以 abort flag 或 finally 已调用冒充撤销完成。 |
| PD-05 | **受约束重放**：工具的已存与现行 replay 合同都满足才自动重放；nested call 稳定 key；Code Mode 记录结果与 fingerprint；版本迁移显式。 | pending nested call、已完成子调用、版本升级、分支改变、随机/时间输入；已完成调用不重复，无法证明的重放停在可诊断状态，由主体继续决策。 |
| PD-06 | **宿主恢复责任**：本地 supervisor/boot recovery、Android 生命周期、多节点接管分别兑现；持久 deadline/等待不占推理 permit。正常可运行时主体持续推进，恢复机制不变成低频 Tick。 | kill、宿主重启、休眠、离线、恢复后延迟任务；真实可用时继续唯一主体，不自称 SQLite 会自动拉起进程。 |
| PD-07 | **等待、取消、计费与容量分开**：客户端停止等待不取消真实工作；终态要等相关执行确认完成或标 unknown；重放使用原结果与费用账本，不重复计入/抹去失败费用。所有需要模型的恢复/诊断同 broker。 | C=1 下 child/tool 等待不占额外 permit；请求取消未获后端确认时不虚增可用容量；恢复不绕并发限制。 |
| PD-08 | **能力缺口可由 Agent 自主改进**：ObservationGap/ExecutionGap 是一等任务输入；孩子可查文档、探测已有 flags、编写候选 adapter、验证并提升。主 loop 仅委派、记忆和决定下一步。 | tmux 高亮丢失案例由 Agent 自行定位并补齐；先试已有原生能力，失败可再开发插件；不让主体临时获得 shell/file 工具。 |
| PD-09 | **有来源的终端语义观察**：保留可用 attrs/几何/cursor 和编码合同，控制流状态与候选选择绑定 pane/process/question revision；SDK/RPC 优先，终端启发式只产候选。 | 反显/颜色/wide chars/resize/alternate screen/历史 yN/问题过期/人类抢先回答；旧观察不驱动输入，丢样式不默默退化成猜选项。 |

这些合同借鉴 Pi Durable 的有效架构，同时保留用户已确认的永续唯一主体、孩子执行、所有层可自改和动态自主联系。无需把项目改为 Node/Pi 应用，也不需要在 SPEC 阶段进入实现。

## 5. 一手网页引用与复核线索

- Pi 官方 README（对应 GitHub primary）：https://github.com/earendil-works/pi/blob/main/packages/durable/README.md ，web ref `turn131view0`（源文件与行号以上文 fixed commit 为准）。
- Pi 官方根仓库检索：`turn128search0`；主包 README 检索：`turn128search3`。
- Cloudflare 官方 PiHarness，页面标 Last updated Oct 22, 2026：https://developers.cloudflare.com/agents/harnesses/pi/ ，`turn131view1`，仅用于宿主 lifecycle 和当前集成状态，不把 beta 的托管部署当作本地产品必选依赖。
- tmux 一手证据为上述官方 repo 的固定 manual 与 C 源码。man7 的当期手册镜像 `turn131view2` 可辅助查看，不替代固定源码。

