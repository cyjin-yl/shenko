# DSH 工具插件兼容层 v0

把部分 [DeepSeek Harness（DSH）](https://github.com/deepseek-ai/deepseek-harness) 工具插件放到
独立 Node 进程中运行，通过 Shenko 现有的 MCP 适配器使用。桥接程序使用真实上游 Cordis
和 DSH 工具运行时；Shenko 的主体、安装权威、任务回执与恢复路径保持既有合同。

设计理由与支持边界见 [DSH v0 SPEC](../../docs/specs/ecosystem-adapters.md#dsh-tool-compat-v0)。
上游 API 调研基线为
[`2e39e70496aa02986ca6a820c5d497e816b69d7e`](https://github.com/deepseek-ai/deepseek-harness/tree/2e39e70496aa02986ca6a820c5d497e816b69d7e)；
实际运行依赖以本目录的 `package.json`、`package-lock.json` 为准。

## 能接什么

适合能够独立注册并执行 DSH tool 的插件，以及其已经明确配置的普通 Cordis 服务依赖。
宿主挂载 `SystemPrompt`、`ToolsService` 和 `NativeToolExecutor`，再按配置装载插件。
`SystemPrompt` 用于满足 DSH 工具运行时依赖，其中的文字不会自动进入 Shenko 的 prompt。

v0 不承诺完整 DSH preset/profile、agent/session、LLM、UI、交互审批、Skill 或动态上下文
兼容。缺少实际服务时应保留错误，不能把待注入插件当作已可用工具。插件函数内部自行访问
网络、文件或外部模型仍属于该插件行为；这不自动获得 Shenko broker 的推理额度管理。

### 一个真实插件样例

已用 [omdsh-dev/dsh-tool-calculator](https://github.com/omdsh-dev/dsh-tool-calculator/tree/b2007a13f06bcf75bf07b9d277ee8d434a316490)
的原始 `lib/index.js` 验证宿主 API 和完整 MCP stdio 子进程：插件为 `0.0.1`，保留它自己的锁文件及
`dsh-tools 0.1.1-rc.2` / `cordis 4.0.1` 依赖；桥接宿主使用 `dsh-tools 0.2.2-rc.5`
与 `cordis 4.1.0`。`15 + 27 * sqrt(9)` 返回 canonical value `96` 与对应文本，错误参数
保持失败，关闭时实际完成清理。可把 `entry` 指向该源码快照构建后的 `lib/index.js` 复现。

这证明了一个具体工具定义跨这组版本的可用性；宿主版本超出该插件声明的 DSH peer 范围，
不构成整个旧版 API 的兼容承诺。MCP stdio、Shenko 原生 Store 和模型使用分别看
[验证记录](../../docs/evidence/dsh-bridge-2026-11-20.md)的对应项；不能由计算器结果推断
依赖 agent/session、浏览器 UI 或外部引擎的其他插件已通过。

## 安装桥接程序

使用 Node 24.9.0 或更高版本；Shenko 仓库的 `.node-version` 指定 24.22.1。从仓库根目录运行：

```sh
npm ci --ignore-scripts --prefix adapters/dsh
```

这只安装锁文件中的桥接程序依赖。准备好要加载的插件及其自己的依赖，并使用可由 Node
加载的模块入口；桥接命令不会自动搜索插件市场或安装配置缺少的模块。

## 选择插件

保存一个配置文件，例如 `/opt/shenko/dsh-tools.json`，把示例路径换成当前节点的真实路径：

```json
{
  "schemaVersion": 1,
  "plugins": [
    {
      "id": "my-tools",
      "entry": "/absolute/path/to/shenko/adapters/dsh/examples/echo.mjs",
      "config": {}
    }
  ],
  "envRefs": {
    "PATH": "PATH",
    "HOME": "HOME"
  },
  "callTimeoutMs": 30000,
  "shutdownGraceMs": 1000
}
```

| 字段 | 用途 |
| --- | --- |
| `schemaVersion` | 当前为 `1` |
| `plugins[].id` | 这一配置中的唯一插件身份，供错误诊断使用；不是改写上游工具名称的前缀 |
| `plugins[].entry` | 本地编译后 `.js` / `.mjs` / `.cjs` 入口；相对路径按配置文件目录解析；不是 npm 搜索词或待安装包名 |
| `plugins[].config` | 原样交给该插件的 JSON 配置 |
| `envRefs` | 子进程环境变量名到当前节点环境变量名的映射 |
| `callTimeoutMs` | 宿主调用期限，整数 1–2147483647 ms；默认 30000 ms，工具自身期限更短时采用较小值 |
| `shutdownGraceMs` | 取消后的本地清理窗口，整数 1–2000 ms，默认 1000 ms；不代表远端副作用已经停止 |

示例使用仓库附带的 `examples/echo.mjs`；换成当前 checkout 的真实路径即可先检查最小
插件。接入自己的插件时替换 `entry` 与 `config`，需要的服务插件按依赖顺序显式列入
`plugins`。依赖 DSH Agent 状态的条件 `when` 工具不在 v0 范围中，不能按无条件工具导出。

Shenko 的 stdio MCP 客户端会先清空子进程环境，再解析 `envRefs`。因此需要子进程运行外部
程序的插件通常要显式保留 `PATH`，使用用户目录的插件可能需要 `HOME`。凭据也只填引用名，
例如 `"WEATHER_TOKEN": "MY_WEATHER_TOKEN"`；值留在执行节点的环境中。插件的 `config`
本身不是自动脱敏存储，不要把凭据字面值放入可分享的配置。

## 检查、生成资料、导入

从仓库根目录执行，使用同一个配置文件：

```sh
node adapters/dsh/src/cli.mjs check /opt/shenko/dsh-tools.json
node adapters/dsh/src/cli.mjs profile /opt/shenko/dsh-tools.json > /opt/shenko/dsh-mcp.json
shenko --data-dir /opt/shenko/state compat mcp import /opt/shenko/dsh-mcp.json --unit-id dsh.my-tools --enabled
shenko --data-dir /opt/shenko/state compat enable compat.mcp
shenko --data-dir /opt/shenko/state compat status
```

| 命令 | 实际行为 |
| --- | --- |
| `check CONFIG` | 加载选定插件，检查实际依赖/工具，然后 dispose；会执行插件初始化代码 |
| `profile CONFIG` | 只解析静态配置，向 stdout 输出现有格式的 `McpConnection`；不执行插件 |
| `serve CONFIG` | 启动 Node 宿主，在 stdin/stdout 提供 MCP；通常由 Shenko 按生成资料启动 |

`check` 是真实装载探针，应只用于你选定并信任的代码；它不提供隔离沙箱。插件初始化也可能
产生副作用。`profile` 不加载插件，也不验证工具运行成功。Shenko 的导入同样不会启动进程。
最后的两个 enable 状态各有含义：`--enabled` 提供这条连接，`compat enable compat.mcp`
提供 MCP 适配器；只有连接启用仍不足以执行。

生成的连接资料使用绝对 Node/CLI/配置路径，并将配置原始字节的 SHA-256 作为 `--sha256`
启动参数。修改配置后，旧资料启动会报 `Bridge config changed since profile generation`；
重新生成、导入资料再启用。此检查不冻结插件文件或其传递依赖。在另一台设备使用前重新
检查这些路径、依赖与环境引用。stdio 的 stdout 留给 MCP 协议；不要通过包装脚本或插件
的 `process.stdout.write` 输出日志，普通 `console` 输出由 CLI 转到 stderr。
Shenko 不继承第三方 stderr，启动失败时先在目标节点运行 `check` 查看结构化诊断。

## 在 Shenko 中使用

从 `compat status` 或导入回执取出实际 `unit_id`、`revision` 和 `artifact_digest`，让
原生子任务 `compat.mcp.request:1` 对这一精确连接调用 `tools/list`，再按返回的 schema
调用 `tools/call`。例中的版本与摘要必须替换为实际值：

```json
{
  "unit": {
    "unit_id": "dsh.my-tools",
    "revision": "<实际 revision>",
    "artifact_digest": "blake3:<实际 digest>"
  },
  "request": { "method": "tools/list", "params": {} },
  "timeout_secs": 60
}
```

调用工具时，将 `request` 换成：

```json
{
  "method": "tools/call",
  "params": {
    "name": "<tools/list 返回的名称>",
    "arguments": {}
  }
}
```

参数必须符合实际工具 schema。工具名不由配置的 `id` 推测，也不要把 `tools/list` 成功
解释为工具业务已经执行。MCP `outputSchema` 使用对象封套 `{value: <原生结果 schema>}`，
成功结果的 `structuredContent.value` 保留 DSH canonical value，文本内容使用上游 renderer。
不支持的附件引用明确失败，桥接程序不会为补齐内容任意读取插件给出的本机文件。

调用沿用 Shenko 原生任务与 Store 回执。工具异常保持失败语义；取消或超时会关闭桥接
连接而不返回伪造的成功结果，使 Shenko 保留 Unknown。MCP 请求还有自己的
`timeout_secs`，可能比工具期限更早到期；任何一层期限都不证明远端业务效果未发生。

## 升级、退出与故障

只撤回本连接：

```sh
shenko --data-dir /opt/shenko/state compat disable dsh.my-tools
```

这会写入安装事实，运行中的 MCP 观察者随后退休相应会话和进程；命令返回不代表进程已完成
清理。`compat disable compat.mcp` 会影响这个适配器下的全部连接，选择操作前确认对象。

连接资料绑定配置文件字节，但不会冻结入口路径背后的所有代码和依赖。升级时先禁用并确认
旧进程已退出，保存原源码/版本和锁文件，更新插件后重新 `check`、生成和导入资料，再启用。
不能用同一个路径的新文件重放旧未知操作。超时或取消仅说明本地执行请求已被中止或失去
观察，远端 HTTP 请求等效果可能已发生，需按原业务状态对账。

常见失败优先检查：目标 Node 版本；入口文件与该插件依赖；插件需要的服务是否存在；
`envRefs` 在执行节点是否有值；生成资料的绝对路径是否仍有效。需要 DSH agent/session
等未桥接能力的插件，应视为超出当前兼容范围，不要靠删掉依赖声明强行通过检查。

固定的 Cordis 4.1.0 会把 disposer 的抛出或拒绝转为 error 日志，dispose promise 本身仍可能
成功；空 AggregateError 还可能在日志展开时完全消失。桥接宿主在实际 disposal 期间观察
框架传给 logger.error 的原始原因，并保留独立的日志观察器；这一窗口内出现 error 时，
`check` 失败，不输出 ready 或 plugin-disposal 成功回执。日志没有专门的 disposer 失败类型，
因此同一窗口内插件主动记录的 error 也会使检查失败。清理超过窗口仍报告未完成；观察器
持续到实际清理结束才移除，超时不会替插件完成清理或对账。已有启动错误或 Unknown 保留
为主错误，额外清理错误只写入 stderr 诊断。

Unknown 调用不会返回普通 MCP 结果或错误，即使 stdio 关闭本身失败。对应请求保持未结算，
宿主仍完成有界清理，由实际 CLI 的错误退出路径终止进程；普通工具错误仍正常返回。

## 可重复验证与证据范围

```sh
npm test --prefix adapters/dsh
```

原生集成旅程在项目 Rust 开发容器中执行，要求 Node ≥24.9 和上述锁文件依赖已经安装：

```sh
export SHENKO_DSH_NODE="$(command -v node)"
export SHENKO_COMPAT_EVIDENCE_ROOT="$PWD/target/dsh-native-evidence"
export SHENKO_DSH_EVIDENCE="$SHENKO_COMPAT_EVIDENCE_ROOT/native-journey.json"
cargo test -p shenko-compat-mcp --test dsh_native --locked -j2 -- --nocapture
```

这一条旅程使用真实 Node 安装、Store、native child 与 DSH stdio 子进程，检查目录和结果
入账、已发生效果后的 Unknown、关闭并重开后的原回执对账，以及单独撤回连接。
它也随 Linux 的 `cargo test --workspace` 执行，缺少 Node 或适配器依赖会明确失败。
CI 的 `rust` 必检任务在运行 workspace 测试前安装仓库固定的 Node 与适配器依赖，
保留 workspace 测试日志、JSON 摘要和原始 Store/CAS。`web` 必检任务运行八个独立的
宿主与协议检查。

测试验证固定依赖下的宿主与协议行为；真实插件、Shenko 原生 Store 接续及真实模型选用
效果分别记录。没有某一层的实际运行记录时，该层保持未验证。`check` 结果仅针对本次配置、
源版本和节点；不能推广为所有 DSH 插件、跨节点环境或完整 DSH agent 的兼容声明。
