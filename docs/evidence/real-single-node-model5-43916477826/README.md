# 第五次真实模型运行：32K 窗口下仍未形成单机闭环

[原 run 43916477826](https://github.com/cyjin-yl/shenko/actions/runs/43916477826) 的结论仍为 **failure**。这一目录保存完整原始 ZIP、完整解码 job 日志、运行 API 原件，以及两条独立读取原件的分析路径；没有重新运行模型或改写验收。

## 源码、环境与原件

公开 head 为 `be46be34593452f47b824d3ae62f625032e2d54c`；实际 checkout 为 `5e7a3d5ade83d21fbc6e86b7eb8c3f19fb3fbe3c`，tree 为 `58e5e59b230a5020ee56ccf800deea1466c3151b`。模型仍是 Qwen3-0.6B-Q4_K_M、llama b12359、32,768 context、原完整 prompt、采样、4,096 输出 reserve 和原请求／阶段期限，不能替代 Qwen3.8-27B NVFP4 参考模型。

- [original.zip](original.zip)：artifact 14255345735，8,232,052 bytes，独立 SHA-256 `ab362c507c24dae49f11429c73b58ecc81cf21c90ebf4d9cc99672323759e084`。
- [完整 job 日志](model-job-133471871117.log.gz)：解压后 229,057 UTF-8 bytes／226,987 字符；与 connector 原文逐字读回比对。解压后 SHA-256 `da48c5ffab41785a157a348e1cd51a46961cd455e60d47a27936bf66c4ed4c58`。
- [原总结](selected-originals/real-single-node/summary.json)、[实际模型调用环境](selected-originals/model-runtime-loaded.json)、[原 runtime 日志](selected-originals/real-single-node/first.runtime.log)、[run](run-43916477826.json)、[jobs](jobs-43916477826.json)、[artifacts](artifacts-43916477826.json)。

ZIP 原字节未改。选取的可直接阅读原件只是便利副本，其余原始文件仍在完整 ZIP。所有归档文件及压缩后的实际字节由 archive-index.json 列出。

## 实际发生了什么

| 范围 | 原始事实 |
| --- | --- |
| 输入与启动 | startup pause、实际浏览器 composer 输入、持久 receipt、首个 frozen request 的原输入均通过 |
| 流与界面 | 801 个实际 public stream parts；无 page error；不是完成输出或流质量指标；未使用真实屏幕阅读器 |
| 模型请求 | 41 条耐久 subject request 终态；40 success/provider_terminal/stop，最后 1 failed/Unknown；不等同 41 次已证明的物理 POST |
| 主体步骤 | 40 retired、1 awaiting_model；没有 accepted decision、child、TaskResult、operation、attempt 或 E1 |
| 首个输出 | native delegation 控制行的 JSON 后追加了正文，实际拒绝理由为 one JSON value 要求 |
| 后 39 个输出 | 相同的 233 字节 wait:end／wait:replace 文本，但对应 frozen semantic_wait 均为 null，实际均被拒绝 |
| 输出、退出和恢复 | 原 child／真实结果／E1／提交输出四项均 false；last_exit=1；restart 未到达 |

独立复核逐一重建 40 份原 SSE：每份都有 DONE 与 stop，拼接的 delta.content 同时等于原 response.content 和 model-result CAS，且有 40 个不同 provider completion ID。原纠错链保留了上一输出、精确拒绝反馈、原输入和 worker 基线。原件没有单独的完整出站 HTTP request-wire 抓包，因此不能把 frozen payload 核对写成独立线上请求字节审计。

40 次成功终态的 input usage 为 12,720–18,502 tokens，实际输出为 196、69 tokens，length 均 false；没有这 40 次由输出截断或 context-limit 拒绝造成失败的证据。这只说明本配置没有完成本任务，不能外推正式参考模型的能力。

## 三类问题分开处理

直接失败是已记录的控制协议拒绝。忽略非法控制、允许不存在的 semantic wait，或把模型正文声称的结果当成真实 child 结果，都会改坏验收。

另外确认了一个独立实现缺口：实际安装的 host.printf descriptor 没有 prompt_section，因此没有进入模型可见的注册表用法说明。所有原 frozen system prompt 都遗漏该能力；原 handler 的输入实际为 text，而模型第一条输出发明了 input 字段。这值得补齐 handler 自有说明，但尚未证明它是全部失败的唯一原因，也没有新的行为改善、disabled 对照或参考模型证据。详见[独立复核](independent-review/README.md)。

最后的 Unknown 与停止是另一条事实链：1,800 秒首阶段未达标后，harness finally 发送 SIGINT；17:51:32.390510Z 记录 shutdown signal，随后最后请求记录 Unknown，并保留原 awaiting_model，运行时退出 1。时间相近及缺少终态流不能证明 provider 未收到请求。本次没有确定该 Unknown 的具体产生分支，不把它改成 NeverDispatched、自动重发或成功退出。

## 可复查方法与分析自身的失败

[独立脚本](independent-review/review_model5.py) 直接读原 ZIP 和八份精确执行源码；其原执行、stdout/stderr、来源与逐 request 结果全部保留。它不调用 Rust parser、不发 provider 请求、不打开活 Store，也不声称重新计算 BLAKE3。ZIP SHA-256、成员及引用长度用于绑定原字节。

另保留精确诊断源码 [inspect_model5_artifact.py](inspect_model5_artifact.py)、[离线入口](run_offline_inspection.py) 与 offline-analysis 的完整派生文件。第一次本地传输多加末尾换行，源码 blob 校验按预期拒绝；原失败及当时字节保留在 offline-initial-source-transport-failure。改为精确原源码字节后分析完成，原模型结论保持 failure。

隔离诊断工作流的首次 YAML 验证失败、后续只读成功和源码恢复见[环境恢复原件](../environment-recovery-20261116/README.md)。诊断脚本退出零只表示原件分析完成，不新增任何产品验收通过，也不证明 S2、多节点全语义、27B、设备或长期运行。
