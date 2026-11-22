# d16 原始三轮浏览器旅程回执

这份记录保留固定产品候选 `d16adbf89c9f79dfce8ef90d77843412bcda85bf` 的原始三轮观察，结果为 **0/3 完整旅程通过**。三轮均实际开始并完成收尾，没有未尝试的轮次；后续观察器修正和产品修复不能改写这次分母。原始 [batch-summary.json](batch-summary.json)、各轮 `summary.json` 和失败日志保持原字节。

## 固定身份与实际结束

产品二进制 SHA-256 为 `d44703d84ff02a4235005e54c935ddeb2b6dc78a4dbe4184e981c293f1daccce`，原 release manifest 记录 90,351,680 字节。构建和模型观察时使用的是 d16；导出时工作树随后移动到别的测试候选，不改变这次源身份。这里保留 [release-manifest.json](release-manifest.json)，不复制或重新哈希二进制。

原 wrapper 于 2026-11-21 23:51:18 UTC 开始三轮旅程，driver 于 2026-11-22 00:05:11 UTC 写完批次结果，wrapper 的结束文件为 00:05:14 UTC。原 host 进程 2576290 自然返回 1，工具记录耗时 837.36 秒；这与秒精度文件的时间跨度分开保留。driver、sampler、wrapper、模型后置身份检查的原退出码分别为 **1、0、1、0**。

[model-pre.json](model-pre.json) 与 [model-post.json](model-post.json) 的模型进程 PID、启动 ticks、boot ID、命令行哈希及三个活动配置文件逐项相同。此检查证明被选中进程及配置文件的身份边界，不是内存中权重张量的完整证明。[runner/host-real.sh](runner/host-real.sh) 和 [runner/real.sh](runner/real.sh) 保留实际 cargo/model 两把宿主锁、容器锁、前后身份检查及实际运行参数。

## 三轮结果须分别理解

| 轮次 | 原始旅程失败 | 父进程退出 | 完成时刻（UTC） |
| --- | --- | --- | --- |
| 001 | Inspect 文本定位同时匹配按钮和内部 span，触发严格模式错误 | 0 | 2026-11-21 23:56:00 |
| 002 | 同一 Inspect 严格模式错误；原中间回答门不能证明结果消费因果 | 0 | 2026-11-22 00:00:20 |
| 003 | WebSocket 异常断开；原生结算报告 predecessor 对象闭包缺失，原错误最终保留 | 1 | 2026-11-22 00:05:10 |

三轮的进程组都完整回收，父进程均已 reap，最终没有存活 PID、线程或僵尸进程，也未强制杀死后代。第三轮父进程退出 1 仍明确是不干净的产品停止；进程组清理完成不把原错误变成成功。

第三轮原日志在 00:03:16.690497 UTC 记录一次 `MODEL_RESULT_INCOMPLETE` 拒绝与替换边界。在 00:05:08.596498 UTC，task `e7d5b19f-c4ca-400f-b768-b5b1b0d67ed0` 的 native settlement 报告 `invalid direct native predecessor: snapshot object closure is incomplete`，随后出现原请求的本地观察取消、managed park 被 fence 和 runtime reconstruction。host SIGINT 观察到的时刻是随后 00:05:09.630014 UTC；最终 `control:15:blake3:0d44e544583e848669a5f0830af4094c058a061ad3e1c0ed8b272c576d12f595` 缺少本地 applied marker 的原错误保持到退出 1。[error-context.json](attempt-003/error-context.json) 只是带原行号的派生摘录；完整原日志见 [first.runtime.log.gz](attempt-003/first.runtime.log.gz)。这些日志建立事件次序，不单独证明缺失对象的唯一根因。

第一轮在后续独立因果审查中确认了真实首任务结果回答，但仍没有完成暂停、重启和下一条消息的整条旅程。第二轮的旧自动门存在假阳性：提交顺序和正文中出现标记不足以证明该回答的请求已经消费原结果；独立审查发现它来自未包含结果的已封存请求，正文仍在等待。这里保留原 `checks`，不把它们重新解释成语义通过。细粒度 sealed input、provider result、E1/T2 和 DOM 绑定由独立的选取 CAS 因果审计提供，本保全步骤不代替该审查，也不把 raw WebSocket 收帧等同于浏览器显示。

第一、三轮停止过程中出现的 `INFERENCE_OBSERVATION_STOPPED` 仍表示本地观察结束、远端终止未经确认；不能由本地退出 0 或进程组为空推断远端效果不存在。

## 保全范围与复查

原导出器 [export-real-receipts.py](collection/export-real-receipts.py) 的 SHA-256 为 `1682d24087422fb309aae2dc57365b7da1e92ad1960814f24dbc2af57d7dfb58`，调用前已经确认三轮 finished、summary 哈希一致、父进程回收、进程组为空以及外层 wrapper 和模型后置检查结束。导出器及其筛选判据没有修改。

- [index.json](index.json) 是原导出索引，覆盖 **91 份原件**，包括账本、runtime/WebSocket、浏览器截图与 DOM/ARIA、模型身份、环境采样和批次回执。原公共 tar 为 1,152,328 字节，SHA-256 `4ec8fe1c64358d5b59d56d0531f60b2ef7352243348bb97159fa0a3924f55cbb`。
- [support-index.json](support-index.json) 另记 **8 份原件**：四个原观察器/采样源码、两个原 wrapper、原 sampler 日志及原导出器源码。这里的四个文件是 journey、browser witness、timing summarizer 和 environment sampler，全部与原 driver manifest 的哈希一致。补充 tar 为 74,705 字节，SHA-256 `b6ee1d651391f66c2a3e4e6402ec2699821bc5816f7270f69e56de29cadec0c3`。
- **99 份原件**逐一核对存储长度与 SHA-256。压缩文件使用 `gzip mtime=0`，并核对解压后的原长度与 SHA-256；`index.json`、`support-index.json` 本身与解析汇总是索引或派生记录，不额外计成一次运行。
- [timing-execution.json](timing-execution.json) 记录原 timing 工具对三份已关闭 runtime 日志的一次实际解析；[timing.json.gz](timing.json.gz) 包含 **1,920 个匹配 scope 对、0 个未配对、0 invalid、0 drop**。scope 结束包含错误返回，不是操作成功、实际发送或旅程成功回执。父子耗时不可重复相加。
- [compact.json](compact.json) 汇总原退出、分母、身份和保全范围；[file-manifest.json](file-manifest.json) 校验本目录全部交付文件。`collection/*.tool.json` 保留导出、补取和原 host 完成的工具回执。

没有复制安装控制文件、数据库或完整 CAS；没有在这次保全中调用 Store、重放原生效果、重新执行模型请求或运行新的测试。当前证据不证明日用准备完成、恢复旅程通过或长期稳定。

文档迁移校验为 365 个迁移块、0 错误。原 001/002 `failure.txt` 各自包含末尾空行，常规 `git diff --check` 因而报告两个 `new blank line at EOF` 提示；这些是原始错误文本的字节，明确保留，没有为消除格式提示而裁剪日志。
