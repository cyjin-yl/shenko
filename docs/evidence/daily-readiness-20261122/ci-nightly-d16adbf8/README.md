# d16adbf8：完整 CI 与 Nightly 原始回执

冻结源码 **`d16adbf89c9f79dfce8ef90d77843412bcda85bf`** 的常规 CI 六项检查和
完整 Nightly 两项任务均自然结束并通过。本目录保全原始 API metadata、八项 job
日志、独立长测试回执、编译器工件成员，以及单次派发和只读观察记录。
它只证明这个候选的回归结果，**不代替真实模型任务旅程，也不改写此前任何失败**。
[派生摘要](summary.json) · [目录完整性清单](file-manifest.json)

## 两种实际 checkout

| 运行 | GitHub API 的候选 head | 原 job 日志中的实际 checkout | 终态 |
| --- | --- | --- | --- |
| [CI 44963143476](https://github.com/cyjin-yl/shenko/actions/runs/44963143476) | `d16adbf89c9f79dfce8ef90d77843412bcda85bf` | `5f4b34ed11a95103c1b904e2451db5e16b804622`，d16 合入 main `f3fa6f4d…` 的 GitHub 合并提交 | 六项 success |
| [Nightly 44963198484](https://github.com/cyjin-yl/shenko/actions/runs/44963198484) | 同上 | `d16adbf89c9f79dfce8ef90d77843412bcda85bf` 本身 | 两项 success |

CI 的实际 checkout 由六项 job 原日志交叉确认，Rust 审计保留原行号；不能把 API
中的 `head_sha` 写成 CI 实际直接测试的提交。Nightly 的原 checkout 和三个独立
长测试打印的 `SHENKO_SOURCE_REVISION` 都精确绑定 d16。
[CI run](ci/run.json) · [CI jobs](ci/jobs.json) · [Nightly run](nightly/run.json) ·
[Nightly jobs](nightly/jobs.json)

## Rust 计数

| 范围 | 外层通过 | 外层失败 | ignored | 另列的嵌套结果 | 不计通过的额外目标 |
| --- | ---: | ---: | ---: | --- | --- |
| CI workspace，compat-poppy | 1240 | 0 | 11 | 19 条子进程结果，各通过 1 项 | 1 个 harness=false 模型程序显式 SKIP |
| Nightly workspace，all-features | 1240 | 0 | 11 | 同上，独立属于这次 Nightly | 同上 |

两次 workspace 各包含 158 个 Cargo 目标：125 个 executable、33 个 doctest；
其中 157 个目标有外层 libtest 汇总，另一个是显式 SKIP 的程序。计数器按每个
`Running` / `Doc-tests` 目标的**最后一条**汇总计外层，核对首个 `running N`；
同一目标较早的子进程汇总单列，不叠加到 1240。两份审计的 stray、mismatch、
missing result、outer filtered 和失败检查均为空。转回后又从原 gzip 解码的日志
重新计算一次，目标、行号和计数与原审计逐项相同；这一步没有重新执行测试。

模型程序的原输出为 `SKIP cognition_self_learning`，要求显式配置
`SHENKO_USE_LOCAL_MODEL`、模型 URL、模型身份及 `--binary`。它没有运行真实
模型完成，不应因为所在 workspace 命令成功而记作模型验收通过。
[CI 原日志](ci/rust.log.gz) · [CI 目标审计](ci/rust-outer-audit.json) ·
[Nightly 原日志](nightly/rust.log.gz) · [Nightly 目标审计](nightly/rust-outer-audit.json)

Nightly 在 workspace 之后还实际执行三个已有长测试，分别计分：

| 独立长测试 | 实际外层结果 | 外层 libtest 耗时 | 嵌套结果 |
| --- | --- | ---: | --- |
| 真实进程 partition/recovery | 1 passed | 1.53 s | 六个进程阶段，各 1 passed |
| 受限 SSH helper 丢回复恢复 | 1 passed | 3.97 s | 无 |
| 受限 SSH helper 错误身份拒绝 | 1 passed | 2.07 s | 无 |

上述 libtest 时间不包括各步骤的编译/装配时间，也不是部署响应延迟。三个外层
结果和六个嵌套结果不再加进 workspace 的 1240；helper 的 `prepare` 是构建准备，
不是第四项测试。分区原 JSON 的 17 项检查全部通过，并明确只覆盖真实 parser、
Store、CAS、两个 OS 进程、归档交换、拒绝、收敛与重开；它不是掉电、多机网络或
真实模型效果证明。[分区原回执](nightly/partition-receipt-from-job-log.json.gz) ·
[三个长测试原汇总与范围](nightly/rust-summary.json)

## 编译器与真实 Chromium 沙箱

完整编译器回归使用固定上游
`795b1b4958333cf904547e484acb4de9d4b3ee80`。原 Vitest 输出为
**183 个套件通过、2 个跳过；2913 个测试通过、9 个跳过**，总耗时 **839.18 s**。
这些数值来自这次 d16 的原输出，没有沿用旧候选的耗时或将跳过项改记通过。
[编译器原 job 日志](compiler/compiler.log.gz) ·
[原 Vitest 日志](compiler/compiler-vitest.log.gz) · [原摘要](compiler/compiler-summary.json)

浏览器回执记录 UID 1001、Chromium sandbox 开启，PID/network namespace 与
seccomp-BPF 检查通过。该回执绑定 d16、Node v24.22.1 和实际 Chromium 版本；
没有通过关闭 sandbox 绕过此前 root 容器装配问题。
[原浏览器回执](compiler/compiler-browser.json) · [完整 inputs.json](compiler/inputs.json.gz)

编译器工件 ZIP **1,152,604 B** 已在源设备完整下载并独立核对 SHA256：
`130ce356cf06e16335488a4a6fb93a2b37a1e241d70e6078e69811d9815f412f`。
本目录保留所列原成员、完整 inputs.json 和原 ZIP index；完整 ZIP 留在源设备，
没有再复制一份进 Git。[工件 metadata](compiler/compiler-artifact.json) ·
[ZIP 成员清单](compiler/compiler-artifact-index.json)

## 停止/重开受控进程回执

Nightly 的 `public_start_pause_survives_process_reopen_and_resume_alone_admits_inference`
实际通过。原回执明确 `model_completion_tested=false`、`remote_completion_claimed=false`；
原 inference 和 metadata 连接最终都观察到 `client_eof`。
[原 result](start-boundary/start-boundary/result.json) ·
[原停止顺序](start-boundary/start-boundary/recovery-stop-order.json) ·
[原 runtime log](start-boundary/start-boundary/recovery-stopped.log.gz)

从 fixture 已持住原 POST 的零点计：51 ms 改变 authority 后继续进程，71 ms
观察到原请求取消，72 ms 发 SIGINT 并观察 host stop latch，73 ms 释放原 E1
writer，89 ms 确认真正 transport 结束，119 ms 确认原错误和 Unknown 被保留且
没有重建。这是受控屏障下的阶段观察，不是生产环境关停时延；它也不抹去旧
5f Nightly 的 3 秒观测失败。

原生工件 ZIP 大小为 **1,116,505,239 B**。只用四个 bounded HTTP Range
请求取得中央目录和上述三个成员，共 **1,217,004 B**；成员经 ZIP CRC 和各自
SHA256 校验。**完整原生 ZIP 的 SHA256 仅来自 GitHub metadata，未独立下载核验。**
[选择范围和成员摘要](start-boundary/selection.json) ·
[工件 metadata](start-boundary/artifact-metadata.json) ·
[完整 ZIP 成员索引](start-boundary/artifact-index.json.gz)

## 单次派发与原件保全

Nightly 只派发一次：23:43:44 UTC 记录请求，23:43:46 收到 exit 0。第一次读取
尚未看到新 run，原 `matches=[]` 和 `run_not_yet_visible=true` 完整保留；后续
只读查询才按 exact head、branch、event 和 attempt=1 绑定 run 44963198484。
没有因为首次空列表重新派发。[原请求](dispatch/dispatch-attempt.json) ·
[原 ACK](dispatch/dispatch-result.json) · [原首次空确认](dispatch/dispatch-confirmation.json) ·
[后来观察到的 run](dispatch/dispatch-run-observed.json)

有限只读 observer 每分钟取一次 API snapshot，最长两小时。它在两次运行都
完整结束后于 00:09:45 UTC 自然退出，未派发、重跑或取消任何 workflow。
所有阶段原 JSON 都保留；观测终态时间和 GitHub job 自身的完成时间分别记录。
[开始](observation/observer-start.json) · [结束](observation/observer-finished.json)

两份保全索引共列 142 个原文件副本；日志和较大 JSON 仅编码为 mtime=0 gzip，
同时保留原字节长度/SHA256。85 个 gzip 已在本地重解压核验。目录清单另覆盖
本文、派生摘要和保全索引；GitHub 原 metadata、初次空确认及原汇总均未改写。
两份传输包分别为 4,180,782 B / `057016af0e5d65c27a8af75cf68e8eda4a79f150647544383e5db6eb2baae959`
和 370,890 B / `4975b71b7d24267672ab76b094df96b6c7f9d6c7cee84574851170ec11ad61be`，
拼接后的整体摘要和每份原件的 raw/stored 摘要均核对通过。
[完成部分原件索引](retention-index-part1.json) · [Nightly 与观察原件索引](retention-index-part2.json)

此目录不执行或证明 d16 之后的新候选。后续产品修复、真实浏览器/模型任务旅程
及长期运行观察仍各自需要对应源码和环境的实际证据。
