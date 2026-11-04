# 可选生态适配器：MCP 与 Agent Skills

本文是第三方生态兼容的功能与接口合同，对应 SPEC §49，并复用 §7/§18/§36/§45 的既有机制。
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
确定性聚合**：输入 `{unit?,window?,grouping}`，`window.max_records` ≤ 200 且缺省 200，
`since_rfc3339` 过滤更早观察，`grouping` 为 `method` 或 `method+status`（方法取归一化观察的
`protocol.method`，否则取冻结输入的 `request.method`；状态是已结算操作结果：succeeded、
failed、cancelled、interrupted 或 unknown）。

扫描上限 3072 只计**匹配**操作（通过 unit/since 过滤的），不把不匹配的操作算进预算，
所以 3073 条 MCP 观察不会耗尽扫描上限而遗漏后续 Skill 观察。按观察时间（RFC 3339 纳秒）
降序排列，时间戳相同时按 receipt_id 确定顺序。`next_cursor` 在一页满且可能还有匹配记录时
返回；调用方用它继续读取被当前页窗口排除的记录，不需要重新扫描已返回的记录。`cursor` 输
入字段省略时从最新记录开始；提供时跳过游标处及之前的记录。`max_records:0` 返回空分组。

每个分组返回 `{method,status,count,first_seen_utc,latest_occurrence_utc,exemplar_digests}`：
至多 200 组、每组至多 3 个归一化结果摘要。**每个摘要先在本安装 CAS 重新认证才进入结果**——
读不出来或字节对不上的摘要一律省略而不是断言。输出是观察：不判断、不采纳、不用关键词
替代学习；主体要形成“使用中学习”结论时，把某个 exemplar 摘要作为 `cognition.unit.write`
的 `derived_from{kind:"cas_object"}`，写出的单元仍是候选。

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
