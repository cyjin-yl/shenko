# DSH 工具桥接 v0：2026-11-20 验证记录

本记录只证明选定的 DSH 工具兼容切片，不改变完整 Shenko 或 S1–S5 的验收状态。
实现与操作见 [适配器说明](../../adapters/dsh/README.md)，需求与 Why 见
[生态适配器合同](../specs/ecosystem-adapters.md#dsh-tool-compat-v0)。

## 输入与版本绑定

- 调研时 Shenko 基线：`535e24bc83f5f43b485085b130c17d41a5d22663`；交付分支已整合随后
  主线 `a28c7f4` 的组件证据更新，没有用本次结果替代那些组件的原证据。
- DSH 源码基线：[2e39e70496aa02986ca6a820c5d497e816b69d7e](https://github.com/deepseek-ai/deepseek-harness/tree/2e39e70496aa02986ca6a820c5d497e816b69d7e)。
- 运行时：Linux x64，Node `v24.22.3`；Cordis `4.1.0`，DSH Tools、SystemPrompt、
  NativeToolExecutor 均为 `0.2.2-rc.5`，MCP SDK `1.32.2`。精确传递依赖由适配器锁文件固定。
- [机器可读计算器回执](dsh-bridge-2026-11-20/calculator.json)保留实际协议请求/结果、
  工具 schema、上下游版本与六个交付源文件的 SHA-256。其绑定是实际文件字节，
  不把配置摘要误当作传递依赖的内容摘要。

## 实际第三方插件经 MCP 执行

样本为 [omdsh-dev/dsh-tool-calculator](https://github.com/omdsh-dev/dsh-tool-calculator/tree/b2007a13f06bcf75bf07b9d277ee8d434a316490)，
固定源码 `b2007a13f06bcf75bf07b9d277ee8d434a316490`，插件版本 `0.0.1`。
原 `lib/index.js`、manifest、锁文件均未修改；它自己的 `npm ci --ignore-scripts`
安装了 `dsh-tools 0.1.1-rc.2` 与 `cordis 4.0.1`。没有把新宿主 SDK 塞进插件目录，
也没有删除或改写旧插件的 peer 约束。

实际路径是：原插件入口 → 原插件 SDK 生成的工具定义 → 新宿主真实 Cordis/DSH 执行管线
→ MCP stdio 子进程 → MCP 客户端。使用交付的 `profile` 逻辑生成固定配置摘要的连接资料；
另用 `check` 命令真实装载并清理。`check` 子进程使用空环境，样本不需要凭据或网络。

| 观察 | 实际结果 |
| --- | --- |
| 插件工作树 | Git clean，固定源提交与预期相同 |
| `check` | `ready`，实际装载、目录与 dispose 通过 |
| MCP 初始化与 `tools/list` | 返回原始 `calculator` 工具，输入参数为 string |
| `15 + 27 * sqrt(9)` | 文本 `96`，`structuredContent.value: 96`，`isError: false` |
| 独立原计算函数 | 同一表达式返回 `96` |
| `expression: 42` | 原生参数校验失败，`isError: true` |
| 客户端关闭 | 已实际关闭，stderr 为空 |

结果证明这两个固定版本之间的工具定义可互操作，不代表旧插件的全部 SDK 合同兼容新宿主。

## 定向机制验证

执行 `npm test --prefix adapters/dsh`：**8 个测试通过，0 失败、0 跳过**。
这些测试使用真实上游运行时与 MCP SDK，插件测试夹具负责制造可独立观察的条件。

- 配置生成不加载不存在的插件入口、不复制环境值；生成资料固定配置原字节，修改后拒绝旧资料。
- 原生参数/输出校验、guard、finalizer 仍生效；缺失服务及依赖 Agent admission 的条件工具明确失败。
- 取消传递给实际工具；进入 Unknown 后撤回接纳，已排队调用不再进入工具体。
- 不合作的工具超时后报告 `drained: false`，不伪装成已经完成清理。
- 完整 stdio 路径验证目录、结果、错误参数、日志与协议分离，以及 EOF 后 Cordis disposer 的独立文件记录。
- 工具先写入效果见证再超时：连接关闭，没有普通终态结果；重启只查询目录，效果见证仍只有一次。

独立审查发现并修复两个实际问题：

1. 原实现只限制 `apply`，没有限制模块 `import`。修复后，带活动 interval 和永久 top-level
   await 的临时插件，在 40 ms 模块期限下，CLI 总耗时 211 ms、退出 1；stdout 为空，
   stderr 保留明确模块导入超时。外部 3 秒兜底未触发。
2. 原实现只在入队时检查可用性。取消前一个调用后，排队调用仍可能进入。修复后，派发前
   再检查宿主状态，并在释放串行队列前标记 Unknown、取消所有已接纳等待者；复测只有一次
   工具体进入，取消与 dispose 各一次，最终 `drained: true`。

这里的 Unknown 是桥接进程没有可报告终态的事实；本地取消、进程退出都不能证明远端效果
没有发生。此验证没有进行真实外部写操作，也没有重试任何未知操作。

## 其他插件的源代码评估

| 固定样本 | 可行性判断 | 尚未证明的部分 |
| --- | --- | --- |
| [Roykf/dsh-tool-weather@1320aed](https://github.com/Roykf/dsh-tool-weather/tree/1320aedcfba6c4835f5da2969b86c4d37d2f7ac7) | 必需服务为 tools、systemPrompt；webServer 是可选查询，宿主工具具备单独适配条件 | 没有执行真实天气请求；提示片段不进入 Shenko；浏览器 weather chip 不兼容 |
| [zeroa234/dsh-neutrino@578784c](https://github.com/zeroa234/dsh-neutrino/tree/578784c9f5e649953f3509de83747d27ee132525) | 工具入口只要求 tools；引擎状态、谱面生成和唱歌工具可以分别评估 | 未进行音频合成；其 spawn 路径没有传递 exec.signal，不能宣称合成取消已受验证 |

## 验证层次与剩余范围

| 层次 | 本次状态 |
| --- | --- |
| 上游接口、真实插件宿主与参数/结果合同 | 已验证上述固定版本 |
| 真实 MCP stdio 子进程往返、错误、退出 | 已验证 |
| Shenko Rust `compat.mcp` 的新连接导入、Store、native child、重启接续 | 复用既有代码，**本次未执行该集成旅程** |
| 模型主动发现、选择并正确使用 DSH 工具 | **未执行真实模型评估** |
| 完整 bundle/profile YAML、UI、Agent/Session、Skill、DSH 提示合成 | 不在 v0 实现范围 |
| 唱歌、附件存储、真实外部服务与跨设备环境 | 未验证，不能由计算器结果外推 |

本地首次文档检查缺少声明的 tree-sitter 依赖，且把新安装的 `node_modules` 内第三方说明
误纳入链接扫描；其失败不代表产品执行失败。安装已有 `scripts/requirements-ci.txt`，并使
链接扫描与既有源码扫描一样排除依赖/构建目录后再检查。没有删除规范引用或改弱迁移守恒。
CI 在原有 Node 开发容器的 `web` 必检任务内运行同一锁文件安装与八个测试，并保留测试日志；
CI 尚未实际结束时不据此宣称远端已通过。

## 清理错误与传输关闭失败复测

在 Shenko 开发容器以仓库固定的 Node 24.22.1 安装原锁文件，运行 `npm test`，实际通过 16 个
测试节点（含 6 个清理场景子测试），0 失败、0 跳过。实际 `check` 子进程验证正常清理、同步抛出、
异步拒绝、空错误值、空聚合错误与清理超时。实际 MCP stdio 子进程验证传输关闭失败时，已进入
Unknown 的请求没有收到终态响应，disposer 留下完成见证，CLI 以状态 1 退出。Cordis 4.1.0
不提供结构化清理失败结果，关闭期间观测到的错误日志也会保守地使检查失败。

## 后续原生接续记录

2026-11-21 的 [DSH 原生接续记录](dsh-native-2026-11-21.md)单独记录新的候选、测试精简与
Store/native 集成旅程。本文各次历史运行的测试数量、结果和当时未执行的范围保持原样；
后续记录的当前状态不倒填为本次已经取得的证据。
