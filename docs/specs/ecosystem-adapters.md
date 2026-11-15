# 可选生态适配器：MCP、Agent Skills 与 Poppy

**内核定位。** 可选的参考插件，建立在[内核合同](kernel.md)之上。第三方材料不携带权限（[内核 6](kernel.md#kernel-6)），经[内核 7](kernel.md#kernel-7)登记，以单元的形式安装和启停（[内核 8](kernel.md#kernel-8)）。

本文是第三方生态兼容的功能与接口合同，对应 [§49](ecosystem-adapters.md#spec-49)，并复用 [§7](plugins.md#spec-7)、[§18](plugins.md#spec-18)、[§36](durable-state.md#spec-36)、[§45](cognition-units.md#spec-45) 的既有机制。
它描述**目标行为与实际接口**；真实协议调用、模型使用与长期观察的证据由
`docs/specs/verification.md` 的规则单独记录，源码与本文件都不是运行期证据。

## 产品边界

产品**不内置**任何第三方实现：没有厂商 MCP server、没有厂商 Skill、没有厂商账号流程、没有
自动安装脚本。产品提供**可选适配器**，让操作者自己选定的外部实现可以在既有安装、
任务效果与认知单元机制下被使用；第三方实现保持独立、可移除，并由操作者自己的安装与账号
状态负责。

适配器彼此独立：

| 单元 | crate | 作用 | 不做什么 |
| --- | --- | --- | --- |
| `compat.mcp` | `shenko-compat-mcp` | 与显式配置的外部 MCP 端点通信 | 不实现 server、不捆绑目录、不读账号 |
| `compat.skill` | `shenko-compat-skill` | 读取原样保留的 Agent Skill 材料 | 不执行脚本、不安装依赖、不授予工具 |
| `compat.poppy` | `shenko-compat-poppy`；宿主接线在 `bin/shenko/src/poppy.rs` | 发现、签名访问、OAuth 账号、浏览器加入与 UCP 商务请求 | 不捆绑商家、账号或支付提供方；能力声明与传输成功不等于购买完成 |

`crates/shenko-compat` 只放共用的安装、回执与安装观察机制（`installation`、`recorded`、
`runtime`），本身既不是生态插件，也不维护另一套安装或服务注册表。

CLI 的 Cargo feature 也独立：默认编译 `compat-mcp` 与 `compat-skill`，但不代表安装或启用。
`cargo build -p shenko --no-default-features --features compat-skill` 只编译 Skill 适配器，不引入
MCP crate 或 `rmcp` SDK；将 feature 换为 `compat-mcp` 则只编译 MCP 适配器。只给
`--no-default-features` 时适配器都不编译，对应子命令也不存在，`compat status` 仍可检查安装。
Poppy 默认不编译，显式加 `--features compat-poppy` 才提供；安装与接口见
[Poppy：身份、账号与商务接入](#poppy-foundation)。

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

<a id="poppy-foundation"></a>
## Poppy：身份、账号与商务接入

**Why。** 个人 agent 访问外部服务时需要说明身份、保留账号边界，并能重新观察不确定结果。
Poppy 身份、OAuth 授权和 UCP 请求通过既有可选插件、原生任务与效果账本接入；商家声明
不自动变成主体权限或购买决定。[Issue #297](https://github.com/cyjin-yl/shenko/issues/297)
跟踪这一方向。本节说明当前接口，运行结果另按[验证合同](verification.md)记录。

协议库位于 [`shenko-compat-poppy`](../../crates/shenko-compat-poppy/src/lib.rs)，不依赖 Store、
Subject 或新的服务注册表。节点身份、凭据与任务接线由
[`bin/shenko/src/poppy.rs`](../../bin/shenko/src/poppy.rs) 及其子模块拥有。

### 可选安装与公开身份

`compat-poppy` 默认不编译；它复用 MCP 传输，因此启用这个 feature 会编译 MCP 依赖，
但不会自动启用 `compat.mcp`。`compat.poppy` 与公司连接均使用普通 Node 安装事实，
只有当前启用的精确安装版本才向主体提供能力。CLI 子命令见
[`CompatPoppyCommand`](../../bin/shenko/src/lib.rs)，以下 `shenko` 指带该 feature 的二进制。
示例域名须替换成操作者控制的公开 HTTPS 地址和实际商家 origin。

```sh
cargo build -p shenko --features compat-poppy
shenko compat poppy init \
  --client-id https://agent.example/poppy/client.json \
  --jwks-uri https://agent.example/poppy/jwks.json \
  --profile-uri https://agent.example/.well-known/ucp \
  --redirect-uri https://agent.example/poppy/callback
shenko compat poppy identity --public-dir ./poppy-public
shenko compat enable compat.poppy
shenko compat poppy connect https://merchant.example --unit-id poppy.merchant --enable
shenko compat poppy connections
```

`init` 生成本节点的长期签名密钥，保存公开配置，输出公开文档；`identity` 可重新导出文档。
这两个命令不发布 HTTPS 服务。操作者须把导出的文件按配置托管，提供真实的回调接收能力，
并保持公开内容与当前密钥、重定向 URI、支付处理器声明一致：

| 导出文件 | 示例公开地址 | 内容 |
| --- | --- | --- |
| `client.json` | `https://agent.example/poppy/client.json` | CIMD 客户端 metadata；该 URL 就是 `client_id` |
| `jwks.json` | `https://agent.example/poppy/jwks.json` | 公开签名密钥；不包含私钥 |
| `ucp.json` | `https://agent.example/.well-known/ucp` | 操作者级 UCP 平台 profile 与公开验证密钥 |

平台 profile 不包含公司专用用户 ID、账号 token 或私钥。默认支付处理器为空；只有实际
接入了能提供不透明支付凭据的提供方，才用 `init --payment-handlers-file <file>` 或
`configure-payments --file <file>` 配置其公开声明，再重新托管 profile。声明文件不是支付凭据。
Shenko 不提供示例商家账号、支付卡或由商家广告自动启用的付款提供方。

`connect` 执行公开发现并保存连接，输出精确的 `connection:{unit_id,revision,artifact_digest}`。
它不登录账号。`probe poppy.merchant` 会重新读取并核对上述三个公开文档，然后尝试所选
UCP MCP 服务的签名 `initialize` / `tools/list`；它要求商家实际声明相应服务。返回的
`published_identity_verified`、工具目录与本地能力交集不证明账号访问或购买成功。

```sh
shenko compat poppy discover https://merchant.example
shenko compat poppy probe poppy.merchant
shenko compat poppy new-session poppy.merchant
shenko compat disable compat.poppy
```

`discover` 是匿名诊断，输出 JSON，不初始化主体或创建任务。`new-session` 保留公司与用户
身份、节点 refresh token，换用新的连接 Session；旧购物车或浏览器 cookie 不被假定随之迁移。
连接更新后应重新取得 `connections` 的 revision/digest。停用插件撤回原生能力并取消、join
自有请求；它不等同于商家侧退出账号。账号撤销走下述 `revoke` 动作。

### 发现、签名与凭据边界

发现从 `/.well-known/poppy.json` 开始，核验最终 domain、版本、渠道和 OAuth 资源/发行方
的相互声明。公司标识使用核验后的 issuer；只有明确的无 AS 声明或相应 metadata 404
才使用 HTTPS origin。网络故障、错误 issuer、无效 JSON 不伪装成无账号服务。选择的
1.x 文档对应请求版本 `1.0`，旧版兼容 `0.4`；版本文档须同 origin、不可重定向且身份一致。
公司声明 UCP 时读取同 origin 的 `/.well-known/ucp`，保留来源、时间、摘要和原始声明。

发现请求匿名，不带 Session、token、cookie，不使用环境代理或自动重试。每次连接前检查
并固定 DNS，CLI/native 默认拒绝私网和非标准端口。发现单请求上限 12 秒、总时限 60 秒、
单文档 512 KiB，主发现至多跟随五次重定向；选定版本、OAuth 与 UCP metadata 不跟随重定向。
带凭据的传输在已验证的实际目标上生成签名，不把旧签名转发到重定向地址。

连接保留每公司稳定的 opaque pairwise user ID 和 Session 身份。请求在最终 method、URL、
Authorization、DPoP、UCP metadata 与幂等键确定后签名：短期委托密钥生成每请求新鲜的
`Poppy-Session`，有 token 时绑定 `ath`，操作者密钥补齐 HTTP Message Signature。
普通 Session 不授予账号访问。非浏览器网页 GET 始终不带账号 token；浏览器使用自己的
商家 cookie。OAuth 端点使用 `private_key_jwt`，其 audience 是精确 issuer，不带 Poppy
Session/version/extensions 头。

实际操作冻结公司、pairwise user、操作者、连接版本、Session 和账号 grant binding。
每次成功的新登录生成新的 grant binding；refresh 保持原 binding 和原 DPoP 密钥，返回
Bearer 访问 token 也不丢弃绑定 refresh token 的 DPoP 密钥。未登录请求使用连接的基础
Session ID；账号请求根据基础 Session ID 与持久 grant binding 派生稳定的独立 Session ID，
刷新保持不变，每次成功的新登录分开处理。首次登录也不隐式继承匿名购物车或浏览器 cookie。
操作冻结的是实际发送的 Session ID。协议版本或 `Poppy-Extensions` 声明变化须开始新
Session，远端错误不会触发自动切换和重发。账号不根据 opaque token 内容猜测。
服务资源按 protected-resource metadata 与 issuer 核对，MCP 的 resource
必须精确匹配实际端点；资源 token 不得用于其他 API 或浏览器加入。

节点凭据保存在数据目录下的 `poppy-private`，与可共享的 Node 连接、任务输入和 CAS 分开。
Unix 实现使用属主校验、0700 目录、0600 且不跟随符号链接的文件、持有期间的 OS 锁，以及
AES-256-GCM 密文的原子替换和 fsync；未实现对应 ACL 的非 Unix 平台明确拒绝此 vault。
长期密钥、OAuth state/verifier、callback、refresh/access token、DPoP 私钥和支付 instrument
留在节点上；
公开观察只含引用、实际返回的 scope/expiry 等 metadata 和未知结果记录。
同步连接或迁移任务不能凭空获得另一节点的账号授权。

DPoP nonce 按 AS/RS 角色及精确 endpoint 分开持久化，成功响应中的新 nonce 也会更新。
429 的 `Retry-After` 在后续发送前检查；无有效时间时记录本地保守等待。OAuth 会在这个
本地检查通过后才写入发送标记。这里没有自动重试刷新、兑换授权码、撤销或商务写入的循环。

### 原生任务接口

插件通过既有 `watch_installed` 发布四个 version 1 定义：

| 原生定义 | 输入与用途 |
| --- | --- |
| `compat.poppy.discover` | `{origin,timeout_secs?}`，公开发现；超时为 1–60 秒 |
| `compat.poppy.account` | 共用连接封套加 `account:{action,...}`，账号流程与浏览器加入 |
| `compat.poppy.request` | 共用连接封套加 `request:{action,...}`，签名访问声明的服务 |
| `compat.poppy.commerce` | 共用连接封套加 `commerce:{action,...}`，UCP 协商、请求与恢复查询 |

后三者的封套为 `{connection,authentication?,timeout_secs?,account|request|commerce}`，只允许
对应的一种动作。`connection` 必须原样使用当前 `connect` / `connections` 返回的精确引用。
`authentication` 默认为 `{"mode":"signed_out"}`；账号访问使用
`{"mode":"account","resource":"https://实际资源地址"}`，公司的默认 token 使用 `resource:null`。
超时缺省 60 秒，范围 1–120 秒。选择账号访问而凭据缺失、过期、资源不匹配或隔离时会报错，
不会默默降级成匿名写入。`account` 中除 `join_browser` 外的控制动作自行加载 grant，
所以过期访问 token 不会阻止 status、refresh 或 revoke。

以下是传给 `delegate:compat.poppy.account:1` 的完整输入示例；尖括号内容均须替换，scope
须来自实际商家和当前任务需求：

```json
{
  "connection": {
    "unit_id": "poppy.merchant",
    "revision": "<connections 返回的 revision>",
    "artifact_digest": "<connections 返回的完整 artifact_digest>"
  },
  "account": {
    "action": "begin_direct",
    "redirect_uri": "https://agent.example/poppy/callback",
    "scopes": ["<实际需要且商家支持的 scope>"]
  }
}
```

后续示例的动作对象放入同一个封套的对应字段。任务在外部动作前通过 `RecordedCall` 冻结
输入并建立 T1、取得本次 dispatch 归属；每次发送前检查当前任务和插件代际。停止与取消
可以阻止后续发送，但不证明已发出的远端动作停止。原始回执使用既有 E1/CAS 路径；共用
`compat.artifact.read` 在 MCP/Poppy 任一精确安装启用时保持可用。

### 账号授权与私有浏览器流程

账号动作由 [`account.rs`](../../bin/shenko/src/poppy/account.rs) 实现：

| `account` 对象 | 行为 |
| --- | --- |
| `{"action":"status"}` | 读取当前 grant、被替换 grant、待授权流程和隔离标记；不远程验证登录 |
| `{"action":"begin_direct","redirect_uri":"<已登记 URI>","scopes":["<scope>"]}` | 保存 PKCE/state，返回授权 URL；商家声明 PAR 时先执行 PAR |
| `{"action":"complete_direct","callback_ref":"oauth-callback-<UUID>"}` | 读取节点私有回调引用，核验 issuer/state/路由后兑换 code |
| `{"action":"begin_device","scopes":["<scope>"]}` | 返回经核验的 verification URI、user code、公司说明和期限；device code 保持私有 |
| `{"action":"poll_device"}` | 按保存的 interval 查询一次，处理 pending/slow_down，不自动循环 |
| `{"action":"refresh","resource":"<服务资源 URL>","scopes":["<已获授权 scope>"]}` | 用原 grant/DPoP key 获取此资源的访问 token；省略 scopes 保留原 grant 范围 |
| `{"action":"revoke"}` | 逐个撤销当前和已被替换的已知 refresh grant，保留未确认结果 |
| `{"action":"join_browser","session_id":"<浏览器 Session UUID>","tab_id":"<已有 tab>","lease_token":"<有效 lease UUID>","controller_epoch":1}` | 在既有受控浏览器及当前租约中完成私有加入；这里的 session_id 是浏览器 ID |

Direct/device 都要求用户在商家实际授权页面完成登录与同意。`begin_*` 只证明流程启动，
不证明已登录。Direct 使用登记的 HTTPS 回调或受支持的 loopback URI；当前宿主不声称已
验证“回调来自发起设备”，因此始终请求 `prompt=consent`。CLI 不启动回调 HTTP listener，
也不代用户批准商家页面。真实接收器把完整回调 URL 保存为本属主的 0600 文件后，可导入：

```sh
shenko compat poppy callback --unit-id poppy.merchant --file /private/path/oauth-callback.txt
```

输出只有 `callback_ref`。把它用于 `complete_direct`，不要把 callback URL、code、verifier
或 token 放进任务 JSON、命令行参数或聊天记录。Device 展示给用户的短码与验证 URL 是
该流程有意公开的交互信息，设备轮询凭据仍留在 vault。待授权、失败的新登录不覆盖已有
grant；成功替换后旧 refresh token 保留在待撤销队列，HTTP 200 才确认对应撤销请求。

当前账号记录保存一组最新访问 token；切换服务时显式 refresh 到该 resource，需恢复默认
token 时传 `resource:null`。实际 scopes 与 expiry 作为发行方观察返回，不扩写成更高权限。
发送授权码兑换、device token、refresh 或 revoke 前保存原 operation ID 和 flight。失去
响应、进程中断或无法解读成功响应时隔离该 flight；重新打开不会盲用可能已旋转的 token。
未知的旧授权兑换也不会被后来的登录抹掉，因此 `revoke` 可以报告 incomplete，不能据此
宣称商家账号已经全局退出。

浏览器加入要求连接声明 `browser_session_endpoint`。适配器只在自己的浏览器中导航到该
精确端点，私下读取同一文档的隐藏 `code` 与表单；随后由 agent HTTP 请求 `/authorize`。
只有观察到 204 才允许原浏览器提交原表单，提交前再次检查 URL、文档、字段、caller、租约
和 controller epoch。签入加入还要求实际 `poppy:browser-sign-in` scope 与公司默认 token。
`web.unscoped` 是网站声明的访问范围，不能作为主体授权。code、脚本结果和 cookie 不进入
模型观察；Poppy/Authorization 不注入浏览器全局流量。浏览器提交 ACK 或导航只构成回执，
尚不证明正确账号 cookie 已生效，结果保留 `application_effect:unverified`。

### 签名 API、MCP 与网页

`request` 支持以下形状，具体路由须来自连接的声明和读取到的 schema：

```json
{"action":"mcp","endpoint":"https://merchant.example/mcp","request":{"method":"tools/list","params":{}}}
```

```json
{"action":"api","description_url":"https://merchant.example/openapi.json","method":"GET","url":"https://merchant.example/api/catalog","headers":{}}
```

```json
{"action":"page","url":"https://merchant.example/products"}
```

例如将第一段作为共用封套的 `request`，委派给 `compat.poppy.request:1`；省略 authentication
就是带 Poppy 身份的未登录访问。OpenAPI 请求的可选 `body` 是 JSON，`headers` 仅用于公开
业务头，凭据与签名由宿主生成。OAuth 端点和 schema 文档不接受 Session 注入；`page`
要求 `signed_out`，显式选择账号认证会被拒绝。通用 API/MCP 可使用商家签发的 Bearer 或
DPoP token，访问目标和 scope 仍由服务验证。收到 MCP 结果或 HTTP 响应只记录协议/资源观察，不能
据此判定某项业务目标已完成。

### UCP 请求、续办与未知结果恢复

[`commerce.rs`](../../bin/shenko/src/poppy/commerce.rs) 使用本机公开平台 profile 与商家
profile 的兼容交集，当前实现版本为 `2026-08-25`。`{"action":"negotiate"}` 会读取实际
MCP `tools/list` 或商家声明的 REST schema，返回相应协商证据。每次商务请求按实际
工具/schema 校验操作与 payload；`transport` 为 `mcp`（缺省）或 `rest`，商家必须声明
相应服务。HTTP `UCP-Agent` 与 MCP 请求 metadata 使用同一平台 profile，Bearer token
须由所选公司的 issuer 为实际服务签发；UCP 接入不会把其他服务的 DPoP token 当作可用
Bearer token。

当前操作名为 `search_catalog`、`lookup_catalog`、`get_product`、`create_cart`、`get_cart`、
`update_cart`、`cancel_cart`、`create_checkout`、`get_checkout`、`update_checkout`、
`complete_checkout`、`cancel_checkout`、`get_order`。有无 `id` 由操作决定；schema 和商家
协商结果决定该操作当前是否可用。写入必须携带此原始动作的 UUID 幂等键；重启、超时或
恢复不生成新键后重发同一写入。

下面的 `commerce` 对象用于读取已经观察到的 checkout，委派给 `compat.poppy.commerce:1`：

```json
{"action":"request","operation":"get_checkout","transport":"mcp","id":"<原始 checkout ID>"}
```

购买完成必须引用先前已记录的 checkout operation。节点会重新读取 checkout，核对公司、
用户、grant、资源 ID、购买条款摘要、可完成状态和有效期；金额、币种、商品、配送等条款
变化不会被新 payload 偷换。`complete_checkout` 只接受空或缺省 payload，以及绑定到这次
观察的节点支付凭据引用。具备实际提供方 instrument 后，先从属主私有 JSON 文件导入：

```sh
shenko compat poppy credential \
  --reference instrument-7a3efde6-b038-4e3b-bec8-07d817fa811b \
  --file /private/path/instrument.json \
  --unit-id poppy.merchant \
  --checkout-observation "<实际 checkout 观察的 operation UUID>" \
  --resource "<实际 UCP 服务 resource URL>"
```

instrument 必须由实际提供方取得，包含其 `id`、`handler_id`、`type` 等真实资料；不能用示例
字符串伪造。导入将它与原观察的公司、用户、操作者、grant、checkout 和条款摘要一起封存。
没有资源专用 token 时省略 `--resource`，只有与真实服务 metadata 相符时才可使用默认 token。
随后动作形状为：

```json
{
  "action": "request",
  "operation": "complete_checkout",
  "id": "<同一 checkout ID>",
  "idempotency_key": "93173b3d-6e8f-4cd2-9b24-4256f468e480",
  "checkout_observation": "<实际 checkout 观察的 operation UUID>",
  "payment_credential_ref": "instrument-7a3efde6-b038-4e3b-bec8-07d817fa811b"
}
```

以上 UUID 仅说明格式；实际新动作使用自己的原始幂等键和已导入引用。任务输入不接受内联
`payment` 凭据，完成请求的凭据只在节点内解析到最终传输。`buyer_ip` 仅可使用用户侧宿主
提供的实际地址，不能拿 agent 的出口 IP 猜测。

商家需要网页续办时，观察中返回 `continuation:{kind:"user_browser",url,headers:"none",...}`。
把该 URL 交给用户浏览器完成商家要求的步骤，再读取原 checkout；这不是 browser join，
不附加 Poppy/Authorization 头。`incomplete`、`requires_escalation`、`ready_for_complete`、
`complete_in_progress` 和 `completed` 分开保存。`complete_in_progress` 只表示处理被接受；
只有实际观察到 completed 及 order ID 才记录订单，仍不据此声称支付结算已被独立核实。

未知结果通过单独的读取任务恢复，动作是：

```json
{"action":"reconcile","operation_id":"<原始商务操作 UUID>"}
```

该动作读取冻结输入和原回执，在同一公司、用户、操作者、平台与 grant 绑定下查询原资源，
不重发原修改。原 create 的响应丢失且从未取得资源 ID 时，结果继续 unknown。后来看到
购物车变化不能证明由哪一次 update/cancel 引起；完成订单的恢复还必须匹配原 checkout
和购买条款，才可形成原操作的后置条件证据。更换账号 grant 后不允许拿新 grant 的结果
结算旧 grant 下的未知订单。

本节不声称已完成真实商家登录、真实浏览器账号绑定、付款或购买。用户授权需要实际商家
流程，协议接受程度由实际响应决定；构建、fixture、工具目录和本地能力交集不能替代这些
证据。首个发现/签名增量的历史结果见
[Poppy 首个增量验证记录](../evidence/poppy-foundation-20261115.md)，后续实测由独立证据记录维护。

协议依据：[Poppy 1.0](https://personalagentprotocol.org/docs/1.0/spec)、
[0.4 兼容文档](https://personalagentprotocol.org/docs/0.4/spec)、
[UCP extension](https://personalagentprotocol.org/docs/1.0/extensions/ucp)、
[UCP 2026-08-25](https://ucp.dev/2026-08-25/specification/overview/)、
[DPoP / RFC 9449](https://www.rfc-editor.org/rfc/rfc9449)。
