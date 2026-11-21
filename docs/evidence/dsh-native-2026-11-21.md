# DSH 原生接续：2026-11-21 验证记录

**首次实测结果：该次 CI 六项必检成功；8 个 JavaScript 用例与 1 条真实 Store/native 旅程通过。原生回执已原字节归档并逐项核对。**

本记录接续 [2026-11-20 桥接验证](dsh-bridge-2026-11-20.md)，回答 DSH 工具经现有
`compat.mcp` 和 native child 执行后，Store 中的成功、Unknown、关闭重开与撤回是否保持
原合同。需求与边界见 [DSH v0 合同](../specs/ecosystem-adapters.md#dsh-tool-compat-v0)，
操作见 [适配器说明](../../adapters/dsh/README.md)。不改变完整 Shenko 或 S1–S5 的验收状态。

## 候选与证据绑定

| 项目 | 本次绑定 |
| --- | --- |
| 首次实测 PR head | `08d3a0dd3aa67338421a31379a03ee5a57636690` |
| CI | [run 44821797841](https://github.com/cyjin-yl/shenko/actions/runs/44821797841)，六项必检任务均 `success` |
| 该次 CI merge checkout | `476fa82d1965463d08167c6179547f933c565f13`，与原生回执的 `source_revision` 相同；PR head 与实际执行 checkout 分别保留 |
| 环境 | Linux；`rust:1.98.1-bookworm` 开发容器；仓库固定 Node `24.22.1`；适配器依赖由 `npm ci --ignore-scripts --no-audit --no-fund` 安装 |
| JavaScript 检查 | CI `web` 日志已确认 **8 通过、0 失败、0 跳过** |
| Rust workspace 步骤 | `success`；实际执行区间 `2026-11-20T21:40:59Z` → `2026-11-20T21:58:12Z` |
| 原生目标实际运行 | [Rust job 136584286004](https://github.com/cyjin-yl/shenko/actions/runs/44821797841/job/136584286004) 的[原始日志片段](dsh-native-2026-11-21/workspace-test-excerpt.txt)：**1 通过、0 失败、0 忽略、0 过滤，4.71 秒**；开始 `2026-11-20T21:55:14.5321684Z`，结果 `2026-11-20T21:55:19.2447561Z` |
| 机器回执 | [native-journey.json](dsh-native-2026-11-21/native-journey.json)，`85101` 字节，原字节复制；[提取来源](dsh-native-2026-11-21/extraction.json) |
| 原始证据位置 | 完整 CI `rust-evidence` 工件中的 `workspace-test.log`、`dsh/native-journey.json` 与 `compat/dsh-*/` 下的 Store/CAS、Node 安装、配置和文件见证 |

回执实际记录 Node `v24.22.1`，Cordis `4.1.0`，DSH Tools、SystemPrompt、NativeToolExecutor
均为 `0.2.2-rc.5`，MCP SDK `1.32.2`。归档时核对了回执内七个适配器/锁文件摘要和一个
Rust 测试源码摘要，均与上述 PR head 的相关文件相同；依赖版本也与 manifest 一致。
原 native 可执行产物摘要为
`blake3:3d7a08b28194d051d861a466302dffb33ba65dfe4c597c9ac179bdd541acd463`，
成功与 Unknown 操作的冻结合同都绑定这个产物。此记录仍绑定原 CI checkout，不倒填为
后续文档提交曾执行过测试。

PR 后续继续整合主线；最新候选的运行结果由其对应的必检 CI 单独确认。本页保留首次
实测候选、原回执和原始运行事实，不把旧 `source_revision` 改成当前分支 head。

## 回执提取与归档

归档 JSON 的实际 SHA-256 为
`a071bd5fea56693ce7b4cb8cb0b9f0995b8a423e0a73dda7115aa44fbba4c07f`，与提取记录相同。

完整 [rust-evidence 工件 14674006742](https://github.com/cyjin-yl/shenko/actions/runs/44821797841/artifacts/14674006742)
的 CI 元数据记录大小为 `999815759` 字节、工件摘要为
`sha256:2f6bef9b7ae5e2aa1bddd6240e1e6f279f888b0c62fa670f34257b63b218d356`。
完整归档超过连接器的单次下载上限。随后一次[只读导出运行 44827300134](https://github.com/cyjin-yl/shenko/actions/runs/44827300134)
从该原 ZIP 通过 Range 共读取 `1129601` 字节，校验 ZIP 成员 CRC 后复制指定的
`dsh/native-journey.json`；这次导出没有重新执行 DSH。生成的
[小工件 14674267072](https://github.com/cyjin-yl/shenko/actions/runs/44827300134/artifacts/14674267072)
为 `9208` 字节，完整 ZIP 的 SHA-256 已核对为
`67daed3bb657daca246040d4ed85ca77be72c8be461923fa4e8fce3464fae59f`。

仓库保存提取出的原 JSON、提取元数据与原 job 日志片段；完整 Store/CAS 继续位于原工件。
本次归档复核读取 JSON 中的实际记录和相关源码字节，没有声称重新扫描完整 Store/CAS。
交付同时增加独立的 `dsh-native-evidence` 小工件上传，并继续保留完整 `rust-evidence`，
使后续回执可以直接取回；这一取回粒度改进不改变本次运行的来源或结果。

原生旅程源码为
[`dsh_native.rs`](../../crates/shenko-compat-mcp/tests/dsh_native.rs)。它使用仓库已有
`protocol-plugin.mjs` 的 `add`、`uncertain` 工具；仅在外层加进程 PID 见证，不替换上游
Cordis、DSH 执行管线或 MCP 传输。第三方计算器的原有结果仍属于前一份历史记录，本次
fixture 旅程不扩大该插件的验证范围。

回执还保留实际 Node 可执行路径、PID 包装模块与配置摘要、生成的连接资料及精确安装身份。
连接参数里的配置 SHA-256 与 `config_sha256` 相同。摘要标识原件，不代替对应原件。

## 原生旅程的检查与观察

以下值直接读取归档 JSON；两代 `success_durable`、`unknown_durable` 及 `add`、`unknown`
调用结果分别逐字段相等。第二代 durable 记录由实际停止屏障之后的 Store 读回产生。

| 检查目标 | 实际观察 |
| --- | --- |
| 目录与原生结果 | 首次 `tools/list` 为 `succeeded`，返回 `add` 和 `uncertain`，`add` 的 `a`、`b` 均为 integer；成功调用的文本为 `42`，`structuredContent.value` 为 `42`，`isError: false` |
| 成功操作 | `state: resolved`、`outcome: succeeded`、`effect_status: complete`；1 个 attempt、1 份 E1，`task_result` 存在；重开及停止后的完整记录相同 |
| 效果后的 Unknown | 原始协议观察为 `unknown`，原因为 `protocol_or_transport_completion_unknown`；操作保持 `state: reconciling`、`effect_status: unknown`、`outcome: null`；1 个 attempt、1 份 E1，`task_result: null`；重开及停止后未改变 |
| 原任务继续可对账 | 最终 `recovery_task_after_shutdown` 为 `pending` / `parked`，`native_park: reconciling`、`native_recovery: true`；诊断为 `original armed operation has no conclusive outcome; no replay` |
| 显式新请求与连接撤回 | 第二代 `explicit_list` 为 `succeeded`；`connection_fact.enabled: false` 后，连接调用为 `failed`，原因是外部单元不是当前启用版本，返回 `operation_id` 为 nil、`evidence_ref: null` |
| 整个适配器撤回 | `adapter_disabled.outcome: unavailable`，原因为 `no registered native definition for this tool` |
| 进程与独立效果见证 | `owned_processes_joined` 为 `[21243, 21256]`；见证中 `add` 1 次、`effect-before-timeout` 1 次、`disposed` 2 次，没有第二次原效果 |

见证原文为：

```text
add
effect-before-timeout
disposed
disposed
```

## 执行方式与检查数量

该原生旅程没有 `ignore`，随 Linux 的必检 workspace suite 正常执行。CI 在测试前安装
Node 和适配器依赖，命令为：

```sh
cargo test --workspace --features shenko/compat-poppy --locked --no-fail-fast -j2
```

完整运行日志是 `workspace-test.log`，其中包含这条 DSH 原生旅程。单独复现命令见适配器
说明。运行时或依赖缺失会失败，不当作跳过后通过。

本增量保留 **8 个 JavaScript 用例和 1 条原生连续旅程**。已删除简单配置分支用例、普通
SDK timeout/restart 重复路径，以及六模式 disposer 矩阵；保留空聚合清理错误和传输
关闭失败的关键路径。前一份记录中的原始 8 项、后续 16 个测试节点是当时发生的事实，
不会因当前精简而改写。

## 证明边界

上述结果只证明这个固定候选、依赖和 fixture 的实际路径：同一个 Rust
进程中的运行时代际停止、Store/安装关闭重开、原始 Unknown 不重放，以及连接/适配器
撤回。它不证明 OS 进程崩溃或掉电恢复、不证明真实模型会主动发现和选择工具，不证明
任意第三方插件、外部服务、跨节点部署或完整 DSH agent/session 兼容。Unknown 的本地
清理与进程退出仍不能证明远端效果已停止；原效果必须按原证据对账。
