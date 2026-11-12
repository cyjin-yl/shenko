# 可选生态适配器：MCP 与 Agent Skills

**内核定位。** 可选的参考插件，建立在[内核合同](kernel.md)之上。第三方材料不携带权限（[内核 6](kernel.md#kernel-6)），经[内核 7](kernel.md#kernel-7)登记，以单元的形式安装和启停（[内核 8](kernel.md#kernel-8)）。

本文是第三方生态兼容的功能与接口合同，对应 [§49](ecosystem-adapters.md#spec-49)，并复用 [§7](plugins.md#spec-7)、[§18](plugins.md#spec-18)、[§36](durable-state.md#spec-36)、[§45](cognition-units.md#spec-45) 的既有机制。
它描述**目标行为与实际接口**；真实协议调用、模型使用与长期观察的证据由
`docs/specs/verification.md` 的规则单独记录，源码与本文件都不是运行期证据。

## 产品边界

产品**不内置**任何第三方实现：没有厂商 MCP server、没有厂商 Skill、没有厂商账号流程、没有
自动安装脚本。产品提供的是两个**可选适配器**，让操作者自己选定的外部实现可以在既有安装、
任务效果与认知单元机制下被使用；第三方实现保持独立、可移除，并由操作者自己的安装与账号
状态负责。

两个适配器彼此独立：

| 单元 | crate | 作用 | 不做什么 |
| --- | --- | --- | --- |
| `compat.mcp` | `shenko-compat-mcp` | 与显式配置的外部 MCP 端点通信 | 不实现 server、不捆绑目录、不读账号 |
| `compat.skill` | `shenko-compat-skill` | 读取原样保留的 Agent Skill 材料 | 不执行脚本、不安装依赖、不授予工具 |

`crates/shenko-compat` 只放二者共用的安装、回执与安装观察机制（`installation`、`recorded`、
`runtime`），本身既不是生态插件，也不维护另一套安装或服务注册表。

CLI 的 Cargo feature 也独立：默认编译 `compat-mcp` 与 `compat-skill`，但不代表安装或启用。
`cargo build -p shenko --no-default-features --features compat-skill` 只编译 Skill 适配器，不引入
MCP crate 或 `rmcp` SDK；将 feature 换为 `compat-mcp` 则只编译 MCP 适配器。只给
`--no-default-features` 时两者都不编译，对应子命令也不存在，`compat status` 仍可检查安装。

```rust
// 安装：普通单元导入，先禁用地记录
node.handle_unit_request(UnitRequest::Import { export, package_root, enable: Some(false) })?;
// 启用：唯一的“向主体提供”动作
node.handle_unit_request(UnitRequest::SetEnabled { unit_id, enabled: true })?;
// 观察安装事实；仅当前启用且实现身份一致时发布实际 native definition
let host = AdapterHost::new(&supervisor, store, node);
supervisor.spawn(McpPlugin, host).await?;
```

## CLI

```
shenko compat status                      # 已装/已启用的插件与已记录的外部单元；只读
shenko compat enable  compat.mcp          # 显式提供
shenko compat disable compat.mcp          # 记录撤回；运行中观察者清理旧代际
shenko compat mcp import conn.json --unit-id mcp.example-search [--enabled]
shenko compat skill import ./some-skill   --unit-id skill.example [--enabled]
shenko compat skill read  skill.example --revision <rev> [--artifact-digest blake3:...]
                                          [--path SKILL.md] [--offset N] [--limit 131072]
```

`import` 只记录字节：不连接端点、不启动进程、不读取任何凭据值，也不安装声明的依赖。外部
单元的 title、描述与来源引用作为认知 metadata 进入安装记录，因此主体可以用既有的
`cognition.unit.list/read` 发现自己装了什么，而不需要适配器自己再做一套发现接口。

`status` 和 `skill read` 使用 Node 的只读打开方式，不初始化或迁移安装；损坏安装直接报错，
不伪装成可用性为 false 的成功命令。`skill read` 使用既有安装 CAS 的只读打开方式，也不初始化
CAS 目录。导入结果中的 enabled 是实际安装状态；重导入保留状态时不会仅回显命令参数。旧版本
资源仍可读取，同一 revision 有多个摘要时必须明确指定摘要。状态列表只读取安装 metadata，
不为展示标题而导出全部资源闭包。

## MCP：连接描述与凭据

```json
{
  "description": "Operator-configured search server",
  "provenance_refs": ["https://example.invalid/how-this-was-chosen"],
  "connection": {
    "transport": "stdio",
    "program": "/usr/local/bin/example-mcp",
    "args": ["--profile", "read-only"],
    "env_refs": { "EXAMPLE_MCP_TOKEN": "EXAMPLE_MCP_TOKEN" }
  }
}
```

`streamable_http` 变体接受 `url` 与 `header_env_refs`，要求 HTTP(S)、无 URL 内嵌凭据、无
fragment。两种变体里的凭据都只写**环境变量名**：连接时才解析，stdio 子进程 `env_clear()` 后
只注入被引用项，HTTP 头部值标记为 sensitive。适配器不把这些解析出的值复制回安装资料，
第三方 stderr 也不继承 Shenko 的终端。此边界不保证用户提供的配置字面值或服务返回内容
不含敏感信息；原始请求与观察会按既有证据机制保留，不能把环境引用机制当作通用脱敏器。

请求是显式的一次协议交换：

```rust
// 冻结输入 → 武装 → 连接/派发；恢复期从不重发
let response = session.request(client_request, deadline, &cancellation).await?;
```

请求 deadline 包含 SDK 队列接纳、发送与等待响应，也包含 SDK 自己的超时取消通知等待。
超过 deadline 后结果仍为 unknown，不证明远端停止。stdio 清理并行等待 SDK 关闭与自有
进程退出；进程在 3 秒内未退出则终止并 join，避免阻塞写锁让 SDK 关闭与进程退出互相等待。
此 3 秒是进程清理窗口，不是远端业务的完成保证。

传输读取有界：`streamable_http` 的 POST JSON 响应体上限 128 KiB，SSE 单事件数据上限
min(请求方配置, 16 MiB)；超限的一次请求结算为 unknown，不重发。stdio 单帧沿用 SDK 默认
16 MiB 行上限。超过上限本身不证明远端没有成功，只证明这次响应不能被本适配器有界观察。

结果语义：

| 观察 | 记录 | 是否等于远端成功 |
| --- | --- | --- |
| 收到 result，`isError:false` | `received`，`RemoteCompleted` | 否，只证明这次交换被观察到 |
| 收到 result，`isError:true` | `rejected/failed` | 否 |
| 超时 / 取消 / 传输不确定 | `unknown`，`EffectStatus::Unknown` | 否，且不声称远端已停止 |

分页、`input_required` 续接与后续调用都是调用方显式的下一次请求，适配器不猜、不循环、不
自动重发。过期 session 不重连重发（SDK 的 `reinit_on_expired_session` 关闭）；HTTP 重定向与
自动重试关闭。连接缓存以精确单元版本为键，属于运行期缓存，不是第二套安装状态；单元被禁用
或替换后，空闲与在用连接一并取消、关闭并等待退出。

## Agent Skills：原样保留

- `SKILL.md` 与所有成员按原字节保留（BOM、CRLF、可执行位都在内），frontmatter 用 YAML 解析
  只为了取 `name`/`description` 与保留未识别字段，正文从不重写。
- 未识别 metadata、`metadata.requires.*`、`allowed-tools` 都保留为**声明**：不安装、不授权、
  不执行。读取返回分页成员清单或单个成员的精确字节范围，由主体自己决定下一步。
- 安装单元是三个普通包加一个独立插件包：内联 `SKILL.md` 根单元 → 内联 JSON 索引单元 →
  外置原始字节单元。索引含成员名、长度、可执行位、按字节序拼接的原始归档位置、每成员
  摘要以及按全局 1 MiB 边界切的每块摘要；外置原始字节通过既有 Node
  `external_artifact` 绑定到本安装 CAS。索引和外置字节都不建立另一套注册表；索引不携带
  本安装 locator，单元摘要与完整归档摘要仍是跨节点身份。本地读取忽略导出时的 locator，
  在本机 CAS 按 digest 读取。
- 原始字节单元最多 4 GiB（既有 `MAX_EXTERNAL_ARTIFACT_BYTES`）；`SKILL.md` 和最终索引仍分别
  受 Node 内联包上限约束。索引先计算文件名和记录元数据的内存上限，成员名、总长度和
  `SKILL.md` 大小先由文件描述符 admission，再在写入安装前逐个按描述符读取；任何成员长度
  或模式在导入过程中变化都会拒绝本次导入。失败可能留下未引用的 CAS 对象，不会把该对象
  注册为一个可用的源单元。
- 导入会先把选中的文件名清单冻结，再写入本机 CAS 或 Node。因此安装数据目录嵌在源目录内
  时也不会把导入自己创建的对象加入源包。这不是整个源目录的原子快照，并发编辑仍可能让
  导入拒绝；硬链接也只是同一文件的普通入口，不建立额外 provenance。
- 成员路径限制为可移植相对路径；符号链接一律拒绝而不是跟随；成员不能同时是另一个成员的
  目录。导入根及其祖先也逐层无链接打开，成员清单冻结与实际文件读取都相对于打开的目录
  句柄，不再检查路径后按完整路径重新打开；类型、大小和可执行位取自实际打开的文件。
  普通文件被并发换成 FIFO 不得阻塞导入；文件在描述符大小检查后增长，在读到超过声明
  长度时拒绝。句柄约束保证不因祖先替换而跳到外部路径。
- 整个索引决定修订；`SKILL.md` 摘要绑定根单元，读取时两者必须一致，因此“同一版本号
  不同资源或资源模式”是不同版本。读取单个成员不调用 `unit_content` 导出整包。
- 有界读路径按索引读取原始归档：CLI `--limit` 限制一次 JSON/base64 读取（1..131072），
  缺省按 1 MiB 块对齐流式写出完整成员；`--offset` 在指定 `--path` 时是文件字节偏移，
  不带 `--path` 时是成员序号。每个触及的 1 MiB 原始块分别校验；损坏的未读块不改变其他
  已认证范围的读结果，也不是对归档其余字节的完整性证明。
- Native `compat.skill.source` 定义版本为 2。输入为
  `{unit:{unit_id,revision,artifact_digest},query:{path?,offset?,limit?}}`；path 存在时
  offset 是字节偏移，否则是成员序号，limit 同为 1..131072。返回值含 `next_offset`；
  主体需要后续范围时用上一页游标继续显式调用。

## 磨合学习：观察，而不是规则

一次真实调用产生一条可引用的原始观察：

```json
{ "observation": { "...": "..." },
  "source_receipt": { "digest": "blake3:...", "bytes": 1234 },
  "learning_source": { "kind": "cas_object", "reference": "operation:<id>", "digest": "blake3:..." } }
```

主体可以把这条观察作为 `cognition.unit.write` 的 `derived_from`，写下自己的 awareness/Skill
单元（例如“这个端点的分页有上限”“这个 Skill 的示例脚本依赖它自己的目录结构”）。安装侧只在
摘要确实存在于本安装 CAS 时把来源标为 verified；其余保持 claimed。学习单元是独立版本：

- 上游升级不会静默改写已学内容；纠正以新版本、`supersedes` 或 retire 表达。
- 观察（发生了什么）与判断（该怎么用）分别保留；一次调用不自动产生 adopted 单元。
- 磨合结论默认是候选，不用关键词过滤、强制提示或永久授权替代主体自己的判断。

### 大观察的外置与有界取回

序列化后不超过 256 KiB 的观察仍按上述内联归一化；更大的观察只写一次：原始 JSON 字节经
`seal_reader`（64 KiB 暂存）进入 CAS，另写一份小清单
`{format:"compat-indexed-observation",schema_version:1,artifact_digest,bytes,
chunk_bytes:1048576,chunks:[每 1 MiB 块的 blake3]}`。清单对象同时是 E1 `raw_ref` 与
`learning_source.digest`——学习来源保持小对象且可按块取回，归一化结果里的
`observation` 变成 `{"status":"observation_outlined","artifact":{digest,bytes},
"read":{handler:"compat.artifact.read",...}}`，不再内嵌原始字节。
读取前先对根对象做完整 blake3 认证：同长度损坏不能再借用原始摘要返回字节。清单字段
（`artifact_digest` 规范性、`chunk_bytes` 必须为 1 MiB、每块摘要规范性、覆盖数一致）在读
取前校验；不支持的 `chunk_bytes`（含 0）结算 failed 而非 panic。逐块认证保持不变：单个
块损坏只让覆盖该块的读取 failed，其他范围仍可读。

MCP 代际同时注册 native 定义 `compat.artifact.read`（定义版本 1，同一 T1/E1 机制）：

- 输入 `{digest,offset?,limit?}`，`limit` ≤ 131072（缺省按整对象的有界读取）；digest 必须
  是本安装 CAS 可解析的规范 blake3 摘要。
- 普通 ≤1 MiB 对象按 `format:"raw"` 返回 base64 范围；清单对象按其 `chunks` 索引逐块认证
  （`format:"compat-indexed-observation"`），返回 `{status:"received",total_bytes,offset,
  next_offset,content_base64}`，`next_offset` 为 null 即到末尾。
- 单个块损坏只让**覆盖该块的读取**结算 failed 且不返回字节，其他范围仍可读；无效摘要、
  对象缺失、超限 limit、越界 offset、根摘要不匹配、不支持的分块大小同样结算 failed，不
  panic、不变 unknown。读取只发生在
  本安装 CAS：digest 不授予远端读取权限，handler 没有外部效果。

### 使用摘要：聚合观察，不代为采纳

同一 MCP 代际再注册 `compat.usage.digest`（定义版本 1）。它对本安装账本里三类适配器操作
（`compat.mcp.request`、`compat.skill.source`、`compat.artifact.read`）的最新 E1 证据做**有界
确定性聚合**：输入 `{unit?,window?,grouping,cursor?}`，`window.max_records` ≤ 200 且缺省 200，
`since_rfc3339` 过滤更早观察，`grouping` 为 `method` 或 `method+status`（方法取归一化观察的
`protocol.method`，否则取冻结输入的 `request.method`；状态是已结算操作结果：succeeded、
failed、cancelled、interrupted 或 unknown）。

Store 在三类操作之间统一选择各操作最后追加的 E1，按 UTC 观察时间降序排列，保留纳秒
精度；时间相同时按 receipt_id 降序，无时间戳的观察排在末尾。每次处理至多 3072 条候选，
包括随后不匹配 unit 的候选；另取一条 SQL 元数据判断是否还有候选，不读取其 CAS 内容。
这限制返回元数据和 CAS 扫描量，不承诺 SQLite 查询的常数时间成本。`since_rfc3339` 和
`cursor` 在候选选择时生效；游标严格续读更旧的 `(observed_at,receipt_id)`。

输出窗口装不下本次匹配记录时，`next_cursor` 指向最后一条已返回记录，保留被窗口排除的
匹配记录；本次匹配全部装下而候选扫描未结束时，游标推进到最后一条已扫描候选。因此即使
本页匹配为零，也可能带有非空游标；调用方须继续到游标为 null，才能确认扫描结束。稀有
Skill 观察可在大量 MCP 观察之后的后续页到达。`scanned_operations` 与 `matched_records`
都是本次扫描的计数，不是全库总量。`max_records:0` 不扫描，返回空分组和 null 游标。

每个分组返回 `{method,status,count,first_seen_utc,latest_occurrence_utc,exemplar_digests}`：
至多 200 组、每组至多 3 个归一化结果摘要。**每个摘要先在本安装 CAS 重新认证才进入结果**——
读不出来或字节对不上的摘要一律省略而不是断言。输出是观察：不判断、不采纳、不用关键词
替代学习；主体要形成“使用中学习”结论时，把某个 exemplar 摘要作为 `cognition.unit.write`
的 `derived_from{kind:"cas_object"}`，写出的单元仍是候选。

### 原始未知观察进入主体接续

收到失败 T2 与收到原始 unknown E1 是不同的事实。兼容沙箱可以消费真实失败 T2；操作仍为
Reconciling、没有终态结果且效果为 Unknown 时，则把已认证的原始 E1 作为普通
`native-observation` 系统事件提供给主体，不伪造 T2、不结算任务，也不改写远端效果状态。
事件身份沿用原 receipt_id，载荷绑定原 task、operation、attempt、raw/normalized 引用及
提交坐标。相同原始回执重接纳是幂等操作；事件接纳失败向上传播并关闭原 writer，重开后只
恢复该原始证据，不重发外部请求。

原生观察的消费资格依据当前 head 的已提交 S1 和实际 disposition：Deferred 观察会继续被
选入后续 S0，非 Deferred 消费才移出。历史的 staged/consumed 标记不能替代当前提交事实。
默认 `cognition.unit.trial` 的严格成功读取合同仍适用；兼容运行时显式认证失败/未知观察。

### 模型学习与独立效果测量

`shenko-compat-mcp` 的 `learning_efficacy` 消费者使用共享 `PreparedSandbox` 生命周期：
真实 Store/CAS、主体、原生派发器和同一计量 broker。开发阶段分别让模型接触 MCP 业务
成功与失败，以及 Skill 的完整入口、方法和开发数据；只提供业务任务与原始资料，不提供
待学习正文或留出场景。随后冻结第一条已被 S1 消费的成功新候选写入，认证实际
`cognition.unit.write.v1` 输入、原始 T1/attempt/E1/T2、来源摘要和先消费后写入的顺序。
Skill 读取还须覆盖三个完整成员并通过各成员摘要。没有合格候选时保留失败，不挑下一条写入。

冻结后规划七种留出场景：MCP 已应用、业务拒绝、效果后丢失响应、接受但尚未应用；Skill
新数据、同一入口的方法变化、方法缺失。每种场景六次重复，候选/全部版本缺席对照各一次，
共 84 个槽位；槽位在启动前全部持久化。两侧各有相同的 900 秒执行和 120 秒评审窗口，
交替运行顺序；配置、原始 baseline 与实际安装结果在打开 Node trial 前封存。

首个合格 S1 的原始请求、线缆观察、SQLite 提交与余额或实际 Skill 资料分别保留，并在
清理前采集独立事实。评审只看去掉候选身份、条件和物理路径的事实投影；MCP 协议完成不
等于业务成功。尚待应用的提交只能在运行时 join 后另行结算为晚到证据，不倒填评审事实。
原始模型请求、失败、未启动槽位和晚到结果全部留存，不重跑、不补分、不自动启用或采纳。
一侧清理未建立时，后续共享绑定停止使用。

## 生命周期与恢复

- 两个插件各自持有安装观察者，目标轮询间隔为 250 ms；初次装配等待第一次对账完成。
  Node 是唯一安装事实来源，其他 CLI 进程的启用、禁用或精确实现替换也会被观察到。
  未启用时不发布 native definition、不连接外部端点。启用后可直接使用，禁用后撤回定义、
  取消并 join 旧代际；重新启用建立新代际，不重放旧操作。观察任务由 Cordis 持有并等待退出，
  错误记入主体运行时日志并保留到 fallible cleanup，不通过只写日志伪装清理成功。
- `compat disable` 的成功表示安装事实已写入，不是所有在用会话已经退出的同步回执。
  连接取消与进程 join 可能晚于写入；底层 I/O 阻塞时，轮询间隔也不是实际清理的墙钟上限。
- 每次调用都是既有 native handler：T1 武装 → 真实派发 → E1 回执。调用者取消只放弃等待，
  已武装的外部尝试仍由该代际的 worker 收尾，不会被静默改成成功。
- 握手期间的取消：`Session::connect` 可能耗时长于武装前的取消检查。获取会话读锁后、发送请求
  前再次检查 durable abort；若已取消，结算 Cancelled + EffectStatus::None，不发送请求。
- reconcile 只查询原始回执，绝不重新派发；进程替换后未结束的尝试保持 unknown。
- 卸载/代际清理：先关闭接纳，再 join 已接纳 worker，最后取消并关闭所有自有会话；清理失败
  不得被报告成成功卸载。
- 传输读取上限：HTTP JSON 响应体 128 KiB、SSE 单事件 min(请求方配置, 16 MiB)、stdio 单帧
  16 MiB（SDK 默认）；超限的响应结算为 unknown 且不证明远端停止，不是远端业务结论。

## 验证状态

结构、接口与失败语义以源码和本文为准；真实协议会话、真实第三方 Skill 使用、模型侧磨合
学习与长期观察各自单独取证。安装成功、`enable` 成功或回执存在，都不替代“这次调用真的做到
了什么”的证据。

`build/ecosystem-compat-20261104/compat-learning-foundation-after-r5/` 的完整原生运行通过
16 个用例，包含失败 T2 接续、无 T2 的原始未知 E1 接续、Deferred 重选与事件接纳失败后
writer 关闭/重开恢复；真实学习消费者同时编译通过。HTTP fixture 由实际服务循环报告就绪，
请求诊断在内存保留、关闭时落盘并 join；连接与请求 deadline 没有放宽。先前 r3/r4 的连接
期限失败及独立 startup observer 结果保留，后续通过不改写旧证据，也不证明早先超时的唯一
原因。这些结果建立执行与留证机制，不建立真实模型的学习效果或任何采纳结论。

2026-11-05 的 `compat-learning-producer-sdk-20261105-r2/` 通过两项真实
Store/NativeChildService/rmcp 消费者：唯一 armed producer 绑定保留 SDK 序列化与原始
metadata 的区别；两个相同的并发 producer 均失败，独立业务账本保持零写入。
`compat-native-selector-after-20261105-r1/` 的三项 sandbox 接续用例进一步从实际
provider 请求恢复注册表生成的调用名与版本，经主体解析、原生执行和原始回执消费完成接续。
修复前，同一消费者因能力说明缺少调用 selector 而失败；没有通过手写别名或固定业务步骤
掩盖缺失的说明。上述证据验证调用与留证机制，真实模型学习效果仍须由独立测量判定。

<a id="spec-49"></a>
<!-- migrated-contract: SPEC:49 -->
# 49 第三方生态兼容：可选的 MCP 与 Agent Skills 适配器

2026-11-04 用户明确：第三方生态只在**外层兼容插件**里使用，**不把别人的 MCP 服务集成进系统**；产品不内置厂商 server、厂商 Skill、厂商账号或自动安装流程。因此本节把“能使用外部能力”与“自带外部实现”分开：前者是可安装的适配器，后者不属于产品。

<!-- /migrated-contract: SPEC:49 -->

<a id="spec-49-1"></a>
<!-- migrated-contract: SPEC:49.1 -->
## 49.1 两个独立插件，一个安装权威

MCP 与 Agent Skills 是两个**独立**的可选单元与独立 crate：`compat.mcp`、`compat.skill`。任一方的安装、启用、升级与卸载都不要求另一方存在；启用 Skill 不引入 MCP SDK，启用 MCP 不读取任何 Skill 正文。二者都复用既有机制，不新增第二套注册表：

| 事实 | 所有者 |
| --- | --- |
| 适配器与外部连接装了什么、是否启用、哪个版本 | `shenko-node` 的安装单元（唯一安装权威） |
| 一次调用实际发生了什么、原始响应与未知 | Store 的 operation/evidence |
| 本进程当前提供哪些 provider/capability/help | Cordis 运行时注册表（当前视图，代际清理） |

适配器是普通 `UnitKind::Code`/`UnitKind::Skill` 单元：外部 MCP 连接是带 `compat.mcp` 依赖的普通单元，Skill 是带一个精确 source-bundle 依赖的普通 `Skill` 单元。导入只是把操作者选定的字节记进安装记录，**不连接、不启动、不读取凭据、不安装依赖**；只有显式 `enable` 才向主体提供。

<!-- /migrated-contract: SPEC:49.1 -->

<a id="spec-49-2"></a>
<!-- migrated-contract: SPEC:49.2 -->
## 49.2 MCP 适配器的真实边界

- 协议实现使用固定版本的上游 SDK，遵循 MCP 稳定版规范与 legacy 协商；Shenko 不实现自己的 server，也不内置任何厂商服务或目录。
- 连接描述只含 endpoint、stdio 程序与参数、以及**环境变量名**的引用；凭据值只在该进程运行期解析，不写入安装单元、回执、日志或项目挂载。stdio 子进程使用清空环境后仅注入被引用变量，stderr 不继承 Shenko 的终端或日志。
- 每次调用先持久武装（T1）再连接或派发；**恢复期间从不重发**已武装请求。一个协议响应只是被观察到的交换：`isError` 是失败，`input_required`/task/分页是仍需显式继续的状态，都不等于远端业务成功。超时、取消与传输不确定保留为 unknown，不伪造成完成或停止。
- 连接缓存以精确单元版本为键，属于运行期缓存而非安装权威。单元被禁用或替换后，空闲与在用连接都在该代际被取消、关闭并等待退出；退休不声称远端副作用已停止。列出能力不自动连接。
- 工具注解与 `allowed-tools` 都是**声明**，不构成授权；适配器不因声明而扩权。

<!-- /migrated-contract: SPEC:49.2 -->

<a id="spec-49-3"></a>
<!-- migrated-contract: SPEC:49.3 -->
## 49.3 Skill 适配器的真实边界

- 上游 `SKILL.md` 与全部成员按原字节保留（含 BOM/CRLF 与可执行位），不重写 frontmatter、不把 OpenClaw/ClawHub 或任何厂商格式当成自己的格式，也不建立第二份内容副本作为“规范化”结果。
- 未识别的嵌套 metadata 与依赖声明原样保留为来源事实；它们不触发安装、不授予权限、不自动执行脚本。
- source 记录由普通单元闭包表达：根单元内联 `SKILL.md`，一个 Code 单元内联有界 JSON 索引，另一个 Code 单元把按成员字节序拼接的原始归档绑定为本机 CAS `external_artifact`。索引保留成员名、长度、可执行位、归档偏移、每成员摘要与每 1 MiB 归档块摘要；跨节点身份是索引与外置字节摘要，本机 locator 不出现在索引中，也不是对端读取权限。
- source 索引的修订决定整个资源闭包的版本，因此资源或模式变化就是新版本；`SKILL.md` 摘要仍绑定根单元，读取时与原始归档中的入口字节一致。
- 成员路径限定为可移植的相对路径；符号链接一律拒绝而不是跟随。文件名清单先冻结再写 CAS/Node，读取时相对于打开的目录句柄逐个成员 admission；长度或模式变化会拒绝导入，不把一个并发源目录的读写边界包装成原子快照。
- 成员名与最终索引仍受 Node 内联包上限约束，整个原始归档最多使用既有外置 artifact 上限；读取单个成员按 1 MiB 归档块认证触及范围，不通过导出完整内联包换取大资源可用性。CLI 可流式返回完整原文，主体接口返回 1..131072 字节的有界分页并给出下一游标。未读块损坏不影响已认证范围的读取，但该接口也不替代完整归档校验。

<!-- /migrated-contract: SPEC:49.3 -->

<a id="spec-49-4"></a>
<!-- migrated-contract: SPEC:49.4 -->
## 49.4 使用中的磨合学习

实际调用产生两种可追溯事实，且都不改写上游来源：

1. **原始观察**：适配器回执的归一化对象包含 `learning_source{kind: cas_object, reference, digest}`，指向实际存储的原始内容。
2. **独立学习单元**：主体可用既有 `cognition.unit.write` 写入自己的 awareness/Skill 单元，`derived_from` 引用上述摘要；安装侧只在该摘要确实存在于本安装 CAS 时标为 verified，其余保持 claimed。

学习单元独立于第三方版本：来源升级不静默改写已学内容，纠正以新版本或 retire 表达，观察与判断保留各自证据。禁止把一次调用结论变成全局强制规则、关键词过滤或永久授权；磨合结论默认是**候选**，需要主体自己的判断与证据才被采纳。

<!-- /migrated-contract: SPEC:49.4 -->

<a id="spec-49-5"></a>
<!-- migrated-contract: SPEC:49.5 -->
## 49.5 不降低既有要求

本节新增能力，不修改 [§1](architecture.md#spec-1)–[§48](architecture.md#spec-48)：安装单元、任务效果、证据级别、代际清理、取消所有权与恢复对账的要求不变。适配器不新增人类审批门禁，也不以“已安装/已启用”冒充真实调用成功；协议级观察、真实模型使用、长期观察分别取证。上游是否回应、是否接受贡献同样不阻塞本条实现。

<!-- /migrated-contract: SPEC:49.5 -->

<a id="spec-50-1"></a>
<!-- migrated-contract: SPEC:50.1 -->
## 50.1 适配器传输有界与大观察的有界取回

本节记录 2026-11-04 已实现的两个边界，作为 [§49](ecosystem-adapters.md#spec-49) 的补充事实，不修改 [§1](architecture.md#spec-1)–[§49](ecosystem-adapters.md#spec-49) 原文：

- `streamable_http` 传输读取有界：POST JSON 响应体上限 128 KiB，SSE 单事件数据上限 min(请求方配置, 16 MiB)；超限的一次请求结算为 unknown 且不重发。stdio 单帧沿用 SDK 默认 16 MiB 行上限。超过上限本身不证明远端停止或失败。
- 归一化观察超过 256 KiB 时不内联：原始 JSON 字节经流式 `seal_reader`（64 KiB 暂存）写一次 CAS，另写 `{format:"compat-indexed-observation",schema_version:1,artifact_digest,bytes,chunk_bytes:1048576,chunks:[每 1 MiB 块 blake3]}` 清单；清单 digest 同时是 E1 `raw_ref` 与 `learning_source.digest`，归一化 `observation` 为 `{"status":"observation_outlined",...}`。
- 同一 MCP 代际注册 native 定义 `compat.artifact.read`（版本 1）：输入 `{digest,offset?,limit?}`，`limit` ≤ 131072；普通 ≤1 MiB 对象按 `format:"raw"` 返回范围，清单对象逐 1 MiB 块认证，`next_offset` 游标到 null 为读完。覆盖损坏块的读取结算 failed 且不返回字节，其他范围仍可读；无效摘要、对象缺失、超限或越界输入同样结算 failed。读取只发生在本安装 CAS，handler 无外部效果，reconcile 只读原始回执。

<!-- /migrated-contract: SPEC:50.1 -->

<a id="spec-50-2"></a>
<!-- migrated-contract: SPEC:50.2 -->
## 50.2 使用中学习的确定性使用摘要

本节记录 2026-11-04 已实现的 `compat.usage.digest`（native 定义版本 1），作为 [§49.4](ecosystem-adapters.md#spec-49-4) 的补充事实，不修改 [§1](architecture.md#spec-1)–[§49](ecosystem-adapters.md#spec-49) 原文：

- 同一 MCP 代际在 `compat.artifact.read` 之后注册该定义；扫描本安装账本中 `compat.mcp.request`、`compat.skill.source`、`compat.artifact.read` 三类操作的已提交 E1 证据，不新增平行 SQL。
- 输入 `{unit?,window?,grouping}`：`unit` 按冻结输入的 `unit.unit_id/revision/artifact_digest` 过滤；`window.max_records` ≤ 200（缺省 200）取按 `observed_at` 与 `receipt_id` 排序后的最新记录，`since_rfc3339` 过滤更早观察；`grouping` 为 `method` 或 `method+status`。方法取归一化观察 `protocol.method`，否则取冻结输入 `request.method`，最后回退操作名；状态是已结算 `OperationOutcome`（无结果记 unknown）。
- 输出至多 200 组，每组 `{method,status,count,first_seen_utc,latest_occurrence_utc,exemplar_digests(≤3)}`；exemplar 是归一化结果对象的 CAS 摘要，先经 `read_object` + blake3 认证才进入结果，认证失败的省略而不是断言。扫描总量有界并如实报告 `scan_truncated`；同一输入重复调用返回同一观察，不新增外部效果（`EffectStatus::None`），T1/E1 与 reconcile 沿用既有 `RecordedCall` 机制。
- 结果声明 `observations_only`：摘要不代替主体判断。采纳路径不变——主体以某个 exemplar 摘要作 `cognition.unit.write` 的 `derived_from{kind:"cas_object"}`，写出的单元仍为候选。

<!-- /migrated-contract: SPEC:50.2 -->
