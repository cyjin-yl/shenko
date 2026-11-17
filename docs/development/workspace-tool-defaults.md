# 工作区工具默认组合：有限证据设计

当前[工作区工具合同](../specs/workspace-tools.md)按用户允许的推理方式暂选 replace。本页保留可运行入口与后续比较设计，并记录首次小模型诊断的明确失败边界；尚未形成编辑器成效排名。真实模型验收继续遵守 [shenko-dev](../../.agents/skills/shenko-dev/SKILL.md) 和[本地模型行为验收](local-model-behavior.md)。

## 可运行的最小诊断

[`workspace_tools_acceptance`](../../bin/shenko/examples/workspace_tools_acceptance.rs) 复用 `PreparedSandbox` / `run_prepared_sandbox`，通过真实 CLI 配置与启动安装入口读取 `tools.workspace`，再由产品主体、有限模型 child 和共享 native runtime 执行。它只展开下面第一个任务模板：同名配送配置中调整在用普通配送门槛，同时保持加急配送行为以及 README、示例文件的原始字节。oracle 读取实际配置，检查五个边界金额的运费；没有向模型提供预期工具调用或输出。快照范围是这三个给定文件，不宣称审核了整个工作区。

```sh
cargo run -p shenko --example workspace_tools_acceptance -- \
  --base-url "$SHENKO_LOCAL_MODEL_URL" --model "$SHENKO_LOCAL_MODEL" \
  --context-tokens "$SHENKO_DEPLOYMENT_CONTEXT_TOKENS" \
  --output /path/to/new-diagnostic-directory --trials 1 --deadline-secs 180
```

`--context-tokens` 来自部署事实；`--max-output-tokens` 设置主体每步上限及消息中请求的 child 每步输出预算。child 合同仍由真实主体编写，结果同时记录 requested / actual budget 与可比性；没有产生 child 时不可比。`--fixture-seed` 只固定初始文件数据，provider seed 保留 `null`，采样沿用真实 provider 默认。每次重复切换两组先后顺序。首次默认每组一次，只诊断接线；增加 `--trials` 复用同一模板，不新增独立场景或变更评分。

每组保留独立 `raw/<editor>-<trial>-<id>/shenko.db`、`cas/`、实际 profile、原始 S0 / S1、child 冻结合同、逐步 provider 结果及 native T1 / E1 / T2；`evidence.json` 汇集这些原始记录及引用。`raw/run.json` 绑定真实 endpoint / model、源码与可执行文件摘要，原始失败位于各组 `outcome.json`。根目录 `report.json` 是不含 endpoint、密钥和模型原文的摘要，以 binding digest 关联原始资料。raw 数据不提交仓库。

成本按原始 `request_id` 去重，包含主体和 child 的失败请求；provider 未报告的 token 字段为 `null`，不当成零。工具指标区分产品 parser 识别出的调用与实际 native operation；无法识别的工具语法仍保留原文，不用关键词猜测次数。每组结束后先排空原始 provider reader；若仍有未知债务或 runtime 清理失败，后续组明确标记未开始，不释放未知债务后继续对比。

`subject_step_records` 是已封存 S0 的记录数，包含 retired 重试和 preparing 记录；`committed_subject_steps` 只统计处于 committed、同时保有 S1 decision 与 commit 引用的步骤。它们均不能替代原始 provider 请求数。早期运行器的 `subject_steps` 指前一种记录数，读取旧报告时应按其绑定源码解释，原始报告不回写。

当前入口比较 replace / hashline；禁用专门编辑器的 execution-only 对照、后两个任务模板与 27B 参考验收仍未覆盖。小模型即使完成这个任务也只形成链路诊断，失败则保留失败；该运行器不改变暂定默认。后续实测报告应另行绑定具体运行目录和模型部署证据。

## 2026-11-17 首次真实小模型诊断

[脱敏证据摘要](../evidence/workspace-tools-diagnostic-20261117.json)绑定运行源码、二进制、模型权重、runtime 和原始记录包的摘要。实际模型为 Qwen3-0.6B Q8_0，llama.cpp build 12359，32K context，reasoning off；它不替代仓库约定的 27B 参考验收。两组各运行一次，初始文件一致，每组期限 180 秒、主体每步输出上限 2,048 token，保留实际默认采样。

| 观察 | replace | hashline |
| --- | ---: | ---: |
| 原始模型请求数 | 7 | 4 |
| 输入 token | 26,504 | 15,074 |
| 输出 token | 642 | 894 |
| 实际提交的 S1 | 0 | 0 |
| 有限 child / 工作区操作 | 0 / 0 | 0 / 0 |
| 含清理的耗时 | 192.832 秒 | 194.512 秒 |

11 次请求均正常返回 `success / stop`，随后被真实主体解析器以 `CONTROL_PROTOCOL_INVALID` 拒绝，原因均为 `reply id is not resolvable on this channel`。两组都在主体控制协议阶段耗尽期限，没有产生 child，也没有修改三个初始文件。因此输入用量和请求数的差异不能用来判断编辑器成本；两种方言都没有得到实际执行。每成功任务成本保持 `null`，缺失的 cache-write 用量也保持 `null`，cache-read 不能再次加到总输入。

原始报告中的 `subject_steps` 分别为 8 / 5，含 7 / 4 条 retired 重试及各一条 preparing 记录；独立回查证实提交数均为零。后续源码已将字段拆为记录数与真实提交数，未回写这次原始输出。原始 Store / CAS、失败全文、实际 profile、服务配置和清理记录保留在私有复查包，仓库摘要记录其 SHA256。当前结论仍是 **replace 为基于推理的暂定默认，真实模型比较未到达编辑器阶段**。

## 比较对象

固定同一实际模型、初始源码、任务、其他上下文及总预算，对比 replace、hashline，以及禁用两种专门编辑器的对照。对照只使用当时真实已安装且已授予 child 的执行能力；记录到底用了 `wasi.exec`、REPL 还是 SSH，不假设不存在的 bash 或 tools callback。读取、搜索和其他条件尽可能一致，避免同时改变提示词、模型与编辑方言。

必须经过真实 Store、主体 step、provider wire、产品 parser、有限 child 和 native 工具执行链。直接调用纯编辑引擎只能证明匹配机制；脚本代替模型输出只能证明接线，不能拿来计算模型成功率。端点与模型从实际环境取得，不因缺少服务把跳过记成通过。

## 三个有区分力的场景

1. **重复文本中的正确修改。** 同一工作区存在相似函数或同名文件，要求修改正确目标并保留另一处行为；观察是否读到相关内容、能否从歧义错误恢复。
2. **状态变化后的继续修改。** 在有效读取后确定地插入另一项合法修改，要求保留该修改并继续原任务；观察过期依据、重新读取、失败与恢复的真实结果，不靠随机 sleep 制造竞态。
3. **跨文件行为修复。** 实现与调用方之间有约定，单独修改某处仍能编译；由独立行为验收判断修复，观察检查与再次修改的总成本。

每个场景重复采样，保留失败原文；重复次数与阈值按现有行为 harness 配置，不为每次重复增写测试。先固定方言说明和源码版本，出现有解释力的差异再增加样本，不铺开模型、语法、策略的全组合。本轮全部新增独立检查仍受“一个连贯增量少于十项”的共同限制，不能按 worker 或参数表拆分绕开。

## 选择规则与最小记录

首先比较固定预算内最终通过独立验收的比例，再比较所有运行的总成本除以成功任务数；失败运行、重试、检查及修复均计入分子。成功为零时不报告有限的成功任务成本。输入／缓存／输出 token、耗时与工具轮次分别记录，轮次仅作解释指标。邻近日志中没有二次修改，不足以证明没有错误；观察结束仍未解决的结果单独保留。

每次运行至少绑定源码与 profile、模型与端点身份、实际产品 prompt／parser 版本、所授工具与安装版本、任务与预算、原始 transcript 和回执、独立验收结果及成本。公开摘要不包含凭据，原始失败仍可追溯。模型换版或协议变化后，旧结果不能直接证明新组合有效。

小样本无稳定差异时保留较简单的暂定默认，不宣布普遍最优。若 hashline 在同等成功能力下稳定降低完成成本，或提高固定预算成功率，才据证据改变相应默认并保留旧配置和理由。更强的源码或格式检查本身不是模型行为改善的证据。
