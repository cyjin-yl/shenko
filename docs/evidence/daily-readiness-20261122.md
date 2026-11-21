# 单机日用候选的运行证据（2026-11-22）

本记录属于固定候选的集中集成工作。它区分代码修复、实际模型观察、完整任务旅程和长期运行；
任何一个局部通过都不代替其它层次。日期按 Asia/Shanghai，原始日志时间保留 UTC。
本次没有取得 7/30 天长跑、自改后带新事实回退、真实多 provider 自动切换或多节点验收。

## 来源与固定配置

原 main 为 `f3fa6f4d8bf22cfd2baf490d9c3a08bd39af4d42`。
[Shenko #384](https://github.com/cyjin-yl/shenko/pull/384) 保留了
[#381](https://github.com/cyjin-yl/shenko/pull/381) 和
[#383](https://github.com/cyjin-yl/shenko/pull/383) 的原作者、完整提交和合并关系。

| 已实际构建的候选 | 源码 | 二进制 SHA256 | 字节 |
| --- | --- | --- | ---: |
| 初始 debug baseline | `f3fa6f4d` | `6c06a5f2e2fba49b7e95e4643e6467a3049b079553951c5649d730db42343d9e` | 见原始构建回执 |
| debug 集成候选 | `cb1d0632` | `ad13f0e737fb5e17b282fdb65b58c023ace54bc7df5da31fb10ce8c9d1b8768c` | 332838480 |
| 同行 JSON 澄清前的 release | `72fc70d3` | `7d179ac65453a44e77e2e63091b753c7f68416cc1fd70187819ea09165e0aea7` | 89865568 |
| 同行 JSON 澄清后的 release | `9a7be2f8` | `c31da82e411b5e6ae77fcc5fd02f115e7823bb3afdfee85b58a4f7ce669fcc15` | 89869664 |
| native 见证、CAS 目录合批与进程级停止修复后的 release | `cdf71091` | `e1bec856a805687d18250907a39ff999bcf76cfc224f569dba961847ef59459b` | 89994976 |

Rust 构建运行在实际 `shenko-build-r71` 开发容器，Rust/Cargo 1.98.1，
使用持久源码、锁文件及共享构建锁。release 使用仓库声明的优化、thin LTO、单 codegen unit
与符号剥离；debug 与 release 同时改变了优化和工件体积，不能当作单因素耗时实验。

实际模型来自 doescompute 的 `http://10.10.10.11:8000`，名称为
`swift-qwen3.8-27b-nvfp4`。服务模型卡报告 `max_model_len=409600`。
它是现场的 `export-broader-conservative-v1` 派生 checkpoint，不能写成未修改的原始参考权重。
记录了磁盘权重、tokenizer、配置和实际启用模板的哈希；它们不证明显存中的张量身份。

| 部署证据 | SHA256 |
| --- | --- |
| `model-identity.json` 原始观察 | `32c4c5d851024b52485a59d102dcfdb0a106f6429351e9e259e4a97fc615daeb` |
| `model-active-inputs.json` 原始观察 | `0d26bba3c091f9792871a1c0ad9b49ed35fc81b549e376088999cd1215651d66` |
| 实际 `--chat-template` 文件 | `ee7b5403ac8d47ba15911a80931d518faa756254e7875214ce29c5fd0d9eae43` |
| 实际 `--hf-config-path` 中的 config | `1de5c7a7be5427a60cdb7375ce9a58a45ab2a1507d2a7b524a9749a3e0677264` |
| generation config | `cc62ca9bb8c38b8397bba4bedc6fd5f2d22e65596c285eb123d1b4e452de2333` |

观察使用现场已启用的 thinking 与服务配置，不改采样或主体 4096-token 输出预算。
首请求对照逐次核对 sealed/effective binding、endpoint、预算及 native options；
每次前后比较二进制哈希与模型卡。模型卡的响应生成字段 `permission`、`created`
不参与身份比较，完整原始响应仍保留。只比较模型卡不构成权重证明。

[最终候选构建前的进程与配置观察](daily-readiness-20261122/model-process-pre-final.json)
在 2026-11-21 19:38:11 UTC 再次核对了服务 PID、start ticks、boot ID 和 cmdline 哈希，
均与对照期间记录一致；三份实际启用文件的 SHA256 也一致。它仍不证明显存张量身份。

## 保留的失败与由此修复的边界

下列记录全部保留，后续尝试不能覆盖它们。失败的安装没有被拿来盲目重放外部效果。

| 原始目录 | 观察结果 | 可支持的结论 |
| --- | --- | --- |
| `baseline-journey` | 23.668 秒，首个模型请求前停止；真实 UI 发送 `steer`，旧 witness 硬要求 `append` | witness 落后于已经合入的 one-send UI 协议；修正为检查同一 socket 的实际协商与接纳 |
| `baseline-journey-r2` | 254.325 秒；已提交各一个 child intent，printf 有真实成功回执；model child 未完成，随后 runtime authority reconstruction，观察 socket 断开 | 完整旅程失败；保留 database-locked、fence 和 Unknown，首个恢复触发原因没有独立日志，不能唯一归因 |
| `candidate-journey-r1` | 533.7 秒；第一请求的跨行 JSON 被拒绝，第二请求达到 4096-token 长度限制，第三请求实际接纳委派，第四请求在清理中变为 Unknown；辅助 WS 的默认 20 秒 pong 窗口触发退出 | 完整旅程失败；需要澄清同行合同，并让观察期限显式化；不能把第三次纠错提交算成首轮成功 |
| `protocol-before` | 尝试 2 次，完成首请求观察 0 次，未尝试 6 次；启动 60 秒超时后取消 | 没有主体请求，不是模型协议成功率样本；当时存在并行构建，但不能由此证明构建造成失败 |
| `protocol-before-r2` | 尝试 3 次，完成首请求观察 0 次，未尝试 5 次；前两次启动超时，第三次取消 | 无本轮并行构建仍存在启动成本；需要明确启动观察窗口并单独测量就绪 |

失败输入、完整模型 wire、正文、S0/S1/R0 引用、关闭后 ledger、CAS 与文件哈希保留在实际设备。
原始根目录为 `/tank/shared/projects/shenko/build/daily-readiness-20261122`。
安装控制文件和原始私人状态不提交到公开仓库。

### 协议修正

旧合同的“`[delegate]` 后接一行 JSON”允许读者理解为换行，但 parser 只接受标签与完整 JSON
同一行。产品修复明确同一行，并让 empty-action 拒绝反馈说明此结构；parser 的接受条件不变。
首个真实失败及原始哈希见 [模型行为说明](../development/local-model-behavior.md)。

新的窄观察 driver 只判断最早请求及它自己的 step：原输入是否进入首 seal、首结果是否无 R0
而提交 S1、S1 是否实际接纳两个要求的 child。它不判断 child 结果、算术语义或完整任务完成。
[真实账本负控](daily-readiness-20261122/protocol-probe-negative-control.json) 含四次请求与第三次的
成功 S1，driver 仍把首个请求判为失败，没有借用后续成功。

### 观察与清理修正

辅助 WebSocket 保留 ping，显式使用声明的 phase 观察期限作为 pong 窗口；这不是响应速度修复。
实际浏览器自身的 frame、断连和接收间隔仍独立记录。

启动入口新增默认仍为 60 秒的 `--startup-timeout`。本次配对观察显式设为 300 秒，phase 900 秒，
模型请求 600000 ms；每次保存 `<phase>.startup.json`，记录真实 elapsed、ready、原始失败。
[独立子进程边界实验](daily-readiness-20261122/startup-boundary-experiment.json) 验证超时、就绪、
证据写入失败不掩盖原错；它只证明 driver 边界，不证明 Shenko 的启动性能。

清理只信号本 driver 创建的 session/PGID，检查活跃线程，并实际回收父进程。
超时、SIGTERM 收口或取消仍记录为不干净结束；清理成功不把任务失败变成通过。
相关 [中断实验](daily-readiness-20261122/interrupted-cleanup-experiment.json) 和
[leader 退出前](daily-readiness-20261122/leader-exit-before.json)、
[退出后](daily-readiness-20261122/leader-exit-after.json) 的回执保留。

## 请求路径与启动成本

[debug 候选计时](daily-readiness-20261122/debug-candidate-timing.json) 有 402 个完整 scope、
0 个无效 scope、0 个已报告丢弃。某次 host.printf claim 的实际区间为 27.571 秒：

| claim 的主要直接组成 | 秒 |
| --- | ---: |
| prepare | 11.046 |
| admission | 8.133 |
| apply | 6.076 |
| materialize closure、position 等其它直接 scope | 0.029 |
| 直接子 scope 之外的 self | 2.287 |

prepare 内部的 predecessor 为 9.722 秒，其中 snapshot 1.273 秒、validate 1.353 秒、
seal pages 7.095 秒。上述内部数字已包含于 prepare，不能再与顶层相加。
另一次 predecessor 为 15.500 秒，seal pages 占 13.094 秒。
已观察 Store mutex 最大约 30.552 毫秒，BEGIN 最大约 19 微秒；这些 scope 不覆盖所有 Node 锁。

该 debug 安装的最大 Node 对象就是 332838480 字节的当前 CLI 原生 implementation artifact，
其 SHA256 与候选二进制一致。它被当前计划引用，不能当作无关历史删除。
相关闭包包含读取、hash、对象比较等实际核验；seal-pages 自身没有直接读取这整个可执行对象。
现有 scope 未把 JSON/hash 和目录同步分别计量，不能把某个总区间全归给其中一项。

[第二次启动审计](daily-readiness-20261122/startup-shape-before-r2.json) 中，前两次
`Shenko starting → store opened` 分别为 32.548、32.569 秒。
其约 69–73 秒的 UI 日志出现在 driver 已开始停止之后，不是未中断的 endpoint-ready 回执。
源码存在大量没有统一外层事务的 schema DDL，但尚未实测各语句/同步占比；
Node 新库 schema 和 subject genesis 已各有事务，不属于同一优化机会。
不能直接把整个 migration 套进 BEGIN，因为内部已有事务和可选 FTS 失败路径。

[存储小探针](daily-readiness-20261122/storage-sync-probe.json) 使用
[保留的程序](daily-readiness-20261122/storage-sync-probe.py)，在两个全新目录各做两轮、每轮 20 次
SQLite WAL/FULL 提交，第二轮交换目录顺序。project 路径 40 次提交中位 125.6 ms，
target-volume 路径中位 151.4 ms；两者都报告 1 MiB filesystem block。
这是当时环境的独立探针，不是产品事务耗时的替代，也不证明 filesystem block 就是实际 I/O 粒度。
没有改动产品数据库、FULL 设置、目录持久化顺序或现有安装。
[首批环境快照](daily-readiness-20261122/protocol-before-startup-environment.json) 保留主机/cgroup
各自的 IO 压力范围和采样时间；采样在结束后，不能追认成启动期间的逐阶段度量。

## 验证范围

Nightly 环境、各次绿/红回执、完整编译器失败与上游 fixture 因果复现见
[Nightly 证据](nightly-environment-20261122.md)。
`9a7be2f8` 的 [CI 44935471497](https://github.com/cyjin-yl/shenko/actions/runs/44935471497)
六项成功；真实模型行为测试仍是显式启用项，不能把常规 CI 当作本次真实模型回执。

远端已完成 13 项针对性测试：有界计时锁安全、8 项 archive admission、legacy recovery、
129 MiB 无关历史下的真实 native 链与重开、活动 wait 路由、到期 wait 路由。
另外原有语义等待 fixture 及 frozen-input 拒绝/重开测试实际通过。

源码独立审查没有发现所查范围的新增 P1/P2。活动 wait 绑定原模型不绕过当前 quota，
原 runtime 缺失时保留等待并明确拒绝，不等于已完成自动 fallback。
archive 修复把 manifest schema/声明组合拒绝移到 `on_object` 前；
格式有效但 reader 不认识的 fact class 仍可能在 CAS 回调后、SQL/文件应用前拒绝，
不能声称“所有拒绝都零 CAS 写入”。

journey 的数字匹配是存在性启发式。完整旅程通过还需要审阅真实 child、最终正文和恢复正文，
确认它们实际报告正确结果且没有否定、引用或相互矛盾的答案。自动出现 `437` 和 marker
本身不能代替这项语义检查。

## 实际对照和完整旅程

### 第一组八个首请求观察

[`protocol-before-r3` 的完整观察](daily-readiness-20261122/protocol-before-r3-timing.json)
及[精简计时回执](daily-readiness-20261122/protocol-before-r3-timing-compact.json)绑定
`72fc70d3` release 与同一 `2b9707f6` driver。固定八个 held-out 输入全部实际完成首请求观察，
未尝试数为零；每个请求的 sealed/effective 配置比较成立。

| Case | 启动就绪秒 | 首请求终止 | 首请求 S1 | `prepare_s1` 秒 | 关闭退出码 |
| --- | ---: | --- | --- | ---: | ---: |
| 17×29 | 80.219 | stop，3153 tokens | 已提交 | 48.579 | 1 |
| 28×34 | 75.981 | length，4096 tokens | R0 | — | 1 |
| 46×27 | 76.990 | stop，2953 tokens | 已提交 | 32.710 | 1 |
| 63×18 | 69.975 | stop，1787 tokens | 已提交 | 44.227 | 1 |
| 42×57 | 68.546 | stop，2650 tokens | 已提交 | 86.152 | -15 |
| 73×16 | 67.754 | stop，1176 tokens | 已提交 | 6.406 | 1 |
| 84×25 | 15.505 | length，4096 tokens | R0 | — | 1 |
| 56×39 | 11.457 | stop，1095 tokens | 已提交 | 7.897 | 0 |

首请求三项检查全部通过 **6/8**；两个 R0 均为 `MODEL_RESULT_INCOMPLETE`。
保留清理检查的原始 `protocol_passed` 是 **1/8**，全部 owned 进程组最终回收完整。
两类数字分别回答首请求接纳与整条观测是否正常结束，均不证明 child 结果或完整重启旅程。
原 driver 的 `rates` 还要求该行没有清理错误，不应把其数值误读成纯首请求成功率。

计时共有 1164 个 scope，1162 个完整、2 个未闭合，0 个无效事件、没有观察到显式丢弃。
两个缺口在 case05 的停止尾部；其后续 request 的 Unknown 与首请求已提交事实分开保留。
同一冻结二进制的启动和封存成本有明显时间波动，尚未运行的新优化不能解释这一波动。

### 由真实观察推动的两项修复

普通 SIGINT 关闭 admission 后，正在处理的下一步可能被误报为 runtime authority failure。
修复通过明确的本地 stop 来源和当前 writer 身份产生 typed `RuntimeAdmissionClosed`，
只有同一个 writer 的正常停机才能消费它；真实 authority loss、存储错误、附加恢复错误
与 Unknown 不被转换成成功。没有延长 30 秒停止宽限，也没有回填旧失败。

[case01 封存图回执](daily-readiness-20261122/protocol-before-r3-case01-seal-shape.json)
把 47.346 秒的 scope 绑定到实际 S1 predecessor：86 行、19 页、1 根，共 106 个对象、139861 B。
源码路径会逐对象调用三层目录同步；这不是实测 syscall 数，也不证明 47 秒全来自 fsync。
目录合批修复只作用于一个 predecessor 图：每个新子文件仍同步，既存对象仍完整核验，
子目录从深到浅去重同步全部成功后，根才进入原有普通持久化路径。
[持久化与 lease 边界说明](../development/control-predecessor-publication.md)明确保留了
后续 authority admission 前的既有 custody 间隙，GC 仍未启用。

### 同一配置的 after 对照

[`protocol-after-r1` 完整计时回执](daily-readiness-20261122/protocol-after-r1-timing.json)
于 2026-11-21 19:28:14 UTC 完成全部八个输入，首 seal 含原输入为 8/8，
首请求无 R0 提交 S1 且接纳所需两 child 为 **8/8**；保留清理门禁的原始 `protocol_passed`
为 **2/8**，因此原 driver 仍以失败退出。没有删除失败或把某次后续模型纠错当作首请求。
[逐 case 配对记录](daily-readiness-20261122/protocol-before-after-paired.json)与
[独立审计](daily-readiness-20261122/protocol-ab-independent-audit-20261122.json)核对了
16 份 summary 哈希、最早 attempt 与其自身 step、首 seal 输入、原 plan/result/decision 的
保留字节及 S1 所创建的恰好两个任务；[只读审计程序](daily-readiness-20261122/protocol-ab-readonly-audit.py)
可重放这一核对。它没有重算全部 CAS 的 BLAKE3，也没有把保留的后续结果算入首请求。
两组均保留原始 4096-token 上限、native options、endpoint、provider serializer 与模型卡绑定。
before 中失败的两例是长度耗尽且可见正文为空；这八例没有复现最初 debug 旅程的跨行标签错误。
因此 6/8→8/8 是本次观测差异，不能声称已经证明同行澄清消除了格式错误或长度截断。
[case01 原始提示差异证据](daily-readiness-20261122/protocol-case01-prompt-diff-evidence.json)
显示，两组系统提示除同行 JSON 合同外，仅有安装感知的观察时间
和由此变化的目录修订。它们不是同一时刻的随机配对试验；八个样本不能换算为总体可靠率，
运行期间的环境差异也不能用来证明时延优化。

两项产品修复已提交并独立审查。`f70e2061` 的真实 CI 有四项通过、两项失败：
Rust 的 E0599 来自新测试对 `anyhow::Error` 直接调用 typed helper；Clippy production
另发现新增 `expect_used` 一处。`86b05db0` 修正 downcast 及 checked parent，不改变断言标准
或持久化屏障。[原 CI 诊断回执](daily-readiness-20261122/ci-f70-diagnostic-summary.json)保留两项失败与
旧日志、artifact 身份。修正后的[四项实际容器回归](daily-readiness-20261122/scoped-86b05db0/summary.json)
在 `86b05db0` 全部通过，原始完整日志按字节校验并以 gzip 原样保留（[归档索引](daily-readiness-20261122/scoped-86b05db0/archive-index.json)）：
[主体循环](daily-readiness-20261122/scoped-86b05db0/shutdown-loop.log.gz)、
[Node authority](daily-readiness-20261122/scoped-86b05db0/shutdown-authority.log.gz)、
[目录屏障故障](daily-readiness-20261122/scoped-86b05db0/cas-graph-barrier.log.gz)、
[跨账本恢复](daily-readiness-20261122/scoped-86b05db0/canonical-recovery.log.gz)。
它们使用 Rust 1.98.1、`--offline --locked --all-features`，每项 exact filter 实际执行一个测试。
这仍不是全工作区、断电或真实模型完整任务证据。

### 停止窗口内的已选择 claim 冲突

[三例尾部诊断](daily-readiness-20261122/protocol-after-r1-shutdown-compact.json)与
[完整计时关联](daily-readiness-20261122/protocol-after-r1-shutdown-diagnostic.json)确认，
after 的 case02、03、07 都记录了 `native_dispatcher` 的
`canonical Store transition: predecessor/schema differs for subject_steps`。
[目标 action 的 Node 回执](daily-readiness-20261122/protocol-after-r1-chosen-claims.json)
表明 claim 已有 index 7 / term 2 的真实 durability acknowledgement，Store prepared 各一条、
materialization 各零条；随后 branch-fork 的 `control:7` digest 与该 chosen 记录一致。
这不是“尚未选择、可以随便重新准备”的普通失败，不能通过重试潜在已发生效果来修复。

[冻结见证与停止后原行的只读比较](daily-readiness-20261122/protocol-after-r1-frozen-subject-comparison.json)
逐一核对三份原 claim 的两个主键。creator 的完整规范 cell 与 JSON 原文字节仍一致；
旧分支只有 successor 从 `preparing` 变为 `awaiting_model`、revision 1 变为 2，
并记录原第二请求的 `active_request_id`。这直接证明捕获后发生了 S0 推进。
停止后的全表另含恢复分支的新 step；它不是失败瞬间的事务快照，不能排除当时其它瞬时差异。

三例原主体与 broker 已退出，进程又恢复分支并创建新的 main request；两个原 child 仍 pending，
operation、attempt、task result 均为零。最后请求保持 dispatched、无 terminal，
provider stream 为 open/0 units；这些字段不证明远端还在计算或已停止。

完整日志没有普通 SIGINT 接收标记，旧 driver 也未记录初始 signal API 调用时刻，
所以不能断定取消分支和 SIGINT 哪个先赢。新 driver `21bb24ab` 仅补充发送调用前后
UTC/monotonic 观测；调用返回仍不等于应用接收，30 秒停止窗口和原失败判据均未改变。

与原错误及保留行变化相符的源码机制是：native 物化把另一主体步的 S0 变化纳入完整 `subject_steps` 见证。
当前收敛工作需保留原创建 step 的精确条件及当前 successor 的存在/绑定校验，
并在真实源码、现有恢复测试和完整模型旅程上验证，不能丢弃旧版本的完整见证或改写旧 chosen 事实。

### 新 native 见证的实际回归

`66a7935c` 在同一实际开发容器执行了 [12 项定向回归](daily-readiness-20261122/scoped-66a7935c/summary.json)：
11 项通过，1 项失败，原进程退出码为 1。已通过的包括带 post-choice S0 推进的 129 MiB 原生链、
8 项 archive admission、新 writer 的 obligation 归档重开/中继，以及跨账本 canonical S1 恢复。

[原 first-claim 失败日志](daily-readiness-20261122/scoped-66a7935c/first-claim.log.gz)
在最后正例恢复处取得 `ReconciliationRequired`。
[失败安装的只读比较](daily-readiness-20261122/scoped-66a7935c/first-claim-fixture-comparison.json)
证明测试负控的恢复 SQL 漏了第八列 `created_at`：source creator 仍符合冻结见证，
follower 的时间列被 `INSERT OR REPLACE` 默认值改写，其余规范列一致。完整行校验因此正确拒绝。
修复 `dcff5a86` 让三个负控各自通过真实 archive/import 和 learned S1 建立独立接收者，
保留各自协调债务，不再恢复或覆盖错误状态；最终正例使用未被修改的原 follower。
原失败安装、退出码和分母均保留。

修复后的同一个 [first-claim 测试实际重跑](daily-readiness-20261122/scoped-438bbadd/summary.json)
在 `438bbadd` 上通过，1 passed / 0 failed / 0 ignored，执行 52.51 秒，进程退出码 0。
它包含三个独立拒绝接收者的持久债务检查，以及干净接收者的成功恢复、幂等性和不重发许可断言。
这次只更改测试夹具；产品 native 见证仍为 `ba49f98d`。原 11/12 与后续 1/1 分开保留。

### 进程级停止回归的第一轮

`0d25d77f` 的 [四项实际回归](daily-readiness-20261122/scoped-0d25d77f/summary.json)
为 3 passed / 1 failed。owner 在退役 worker 完成前保持独占、model-free 启动/重开及选主错误保留、
原 authority fencing 三项通过。实际 `start_boundary` 的新增故障关闭段在发送 SIGINT 之前，
等待 HTTP 结束通道超时；整条测试仍为失败，release 被门禁阻止。

[保留的事件顺序](daily-readiness-20261122/scoped-0d25d77f/recovery-stop-order.json)
只有初始 held POST，没有旧失败时刻的屏障内部快照。最终日志的取消行不能倒填为屏障内观测。
源码中取消后的错误/E1 日志也会同步访问被测试 IMMEDIATE 事务占住的 Store，
因此 fixture 不能把 HTTP 连接先结束当作这个屏障的前提。
测试修正 `a533b331` 在相同三秒窗口内观察绑定原 request 的取消日志，
确认没有更早的 host stop 后才发送 SIGINT；仍持有 writer 检查 host latch 与安装 owner，
释放 writer 后必须看到真实本地 transport ending，再核原 Unknown、非零原错与无新启动/POST。
此处既不证明远端模型已停止，也不把测试定义当作重跑通过。

### 进程级停止回归的实际重跑

`cdf71091` 的[同一真实进程测试](daily-readiness-20261122/scoped-cdf71091/summary.json)
实际通过：1 passed / 0 failed / 0 ignored，测试执行 79.42 秒。
[原始事件顺序](daily-readiness-20261122/scoped-cdf71091/recovery-stop-order.json)
以 held POST 为零点：94 ms 注入 campaign 并继续进程，114 ms 观察到原请求取消后发送 SIGINT，
125 ms 观察到 host stop latch，然后才释放 Store writer；286 ms 观察到本地连接结束。
1361 ms 的[最终核验](daily-readiness-20261122/scoped-cdf71091/recovery-stop-receipt.json)
保留原始错误退出、原请求和 attempt 的 Unknown，确认没有重建实例或新增 POST。
这是本地取消、停止意图与 owner 生命周期的回归；不证明远端模型已经停止。
此前三项通过和本次一项重跑分开记录，原失败回执不变。

`0d25d77f` 的[实际 Clippy 失败](daily-readiness-20261122/ci-0d25d77f-clippy.json)
来自 `tokio::select!` 的 signal 绑定被判为 unused variable。
`c3f7b04c` 只修正该绑定命名，仍检查 signal 的错误并触发停止，没有增加 lint 豁免。

`cdf71091` release 于 2026-11-21 20:39:11–20:46:55 UTC 实际构建成功。
完整 Nightly [44947707192](https://github.com/cyjin-yl/shenko/actions/runs/44947707192)
的编译器与长 Rust job 均已通过；常规
[CI 44947215539](https://github.com/cyjin-yl/shenko/actions/runs/44947215539) 六项已全部成功。
这两个运行各自的 Rust 外层汇总均为 1240 passed / 0 failed / 11 ignored，
另有未配置真实模型的显式 skip，不把 19 个嵌套子进程结果重复计入分母。
完整编译器为 183 个套件通过、零失败、2 个跳过；测试用例为 2913 通过、零失败、9 个跳过。
[完整 Nightly 回执与范围](nightly-environment-20261122.md#complete-cdf71091-checkpoint)
还保留了实际 Chromium sandbox 与三个额外恢复证明；它们不替代下面失败的真实模型旅程。

### `cdf71091` 的三次完整旅程尝试：0/3

[原始 batch 回执](daily-readiness-20261122/final-cdf71091/batch-summary.json)
记录 2026-11-21 20:47:52–20:53:22 UTC 的三次新安装尝试：三次均完成观察、均失败，
未尝试数为零。driver 原进程退出 1，独立环境 sampler 退出 0；三个 owned 进程组都已实际回收，
每个 Shenko 原进程仍以 1 退出，不能算干净关闭。模型服务前后进程身份与三份实际配置哈希一致。
[逐次精简回执](daily-readiness-20261122/final-cdf71091/compact.json)和
[原始字节索引](daily-readiness-20261122/final-cdf71091/index.json)保留了全部分母、错误、
真实浏览器输入、账本、日志及截图；压缩日志/账本解码后的长度和 SHA256 均已复验。

| 尝试 | 启动就绪秒 | 观察总秒 | 实际最早具体拒绝 | 原生效果状态 |
| --- | ---: | ---: | --- | --- |
| 001 | 12.269 | 137.559 | `native.arm` 的 `attempts` 完整见证不再相符 | printf 没有 operation/attempt/E1；model child 的原 attempt 保留 Unknown |
| 002 | 13.675 | 100.924 | native settlement 的 `tasks` 完整见证不再相符 | printf 有真实成功 operation、ended attempt、postcondition-verified E1，但没有 T2/task result |
| 003 | 13.866 | 89.839 | `native.arm` 的 `attempts` 完整见证不再相符 | printf 没有 operation/attempt/E1；model child 的原 attempt 保留 Unknown |

三次首个真实模型结果都成功提交各一个 model child 和 host.printf 委派，并声明等待实际回执。
首请求输出为 3046、2938、1268 tokens，均为 provider stop；正文没有把尚未执行的结果宣称为成功。
它们没有完成两个 child、最终回答、暂停重启或后续消息旅程，首轮委派成功不能替代完整成功。

[001 的原 chosen/冻结见证审计](daily-readiness-20261122/final-cdf71091-attempt001-native-arm-witness.json)
表明实际 printf handler 使用的是 ordinary arm codec 1，而非先前定向回归的 paired arm codec 3。
其 Node index 11 已 chosen，Store materialization 为零；冻结的 attempts/operations 前像都为空，
停止后各有另一 model child 的一行。原账本明确记录 attempts predecessor 冲突；
停止后的行不是失败瞬间的事务快照。002 的原日志则明确记录 tasks 冲突，不能拿 001 的机制代替它。
这些已选择的失败不能通过重新发起潜在已有外部效果来修复；原 Unknown、成功 E1 和协调债务全部保留。

[当前候选的完整计时](daily-readiness-20261122/final-cdf71091/timing.json)包含 872 个完整 scope，
没有未闭合、无效或已报告丢弃事件。三次 `subject.prepare_s1` 分别为 5.012、5.833、6.250 秒；
三次 native claim 分别为 4.696、6.062、4.465 秒，native arm 为 2.715、2.967、2.856 秒。
002 的失败 native T2 为 6.522 秒，其中 prepare 3.798、admission 2.538、apply 0.180 秒；
这些 scope 退出包含失败路径，不等于成功派发。31 个 predecessor 中 seal-pages 的中位 0.820 秒、
最大 5.386 秒；已观测 Store mutex 最大 16.163 ms，IMMEDIATE begin 最大 47 μs。
嵌套 scope 不能相加作端到端时间，也不能将不同时间候选的差异全部归因于目录合批。

同期 67 个环境采样没有读取错误；可见系统的 IO some/full avg10 最大分别为 30.42/26.82，
CPU some avg10 最大 1.92。它们是 sampler 可见 `/proc` 的系统压力，不是 Shenko、其 cgroup
或 doescompute 模型服务的独占归因，不能据此定位某一次调用的原因。

人工查看失败截图还发现浏览器容器缺少 CJK 字体，中文呈缺字框；DOM 与 wire 中的实际中文原文仍完整。
原截图保持不变，未把它们算作中文视觉验收。随后为后续观察安装了固定版本
`fonts-noto-cjk 1:20220127+repack1-1`，并保留[字体文件哈希](daily-readiness-20261122/font-environment.json)。
该环境修复没有改变应用源码、模型、任务预算或原失败判据。

### 从实际拒绝收敛 native 依赖

后续产品改动把实际 `host.printf` 接到 paired native arm，并分别收窄首次 claim、
arm、直接 T2、model child 派发及普通 T2 的规范读取见证。新的 typed selector 由原任务、
原 operation/attempt/E1 和有界 owner ancestry 推导；不接受发送者自选 SQL 或任意主键集合。
源捕获、最终 IMMEDIATE 前像与后像验证使用相同规则，保留 schema/trigger 绑定、
主体结构检查、祖先取消的 Node revision fence 和原始结果身份。
[派发依赖与版本说明](../development/canonical-native-dispatch-witness.md)记录了
新 dispatch codec 2 / row schema 5 与原 codec 1 的独立恢复语义。

这没有给已 chosen 的失败另发执行权：只有原 live preparation 的 fresh grant 可以获得 `Won`，
prepared、learned 和重复材料化仍只能得到 `AlreadyCommitted`，Unknown 和外来分支的协调债务不变。
archive capability 11 声明了这组新读取能力，旧格式按其原规则读取；raw Node 传输兼容边界仍单独存在。
完整 predecessor、认知快照、闭包与 schema 检查仍有成本；arm 的逻辑身份匹配以及普通 T2 的
子任务范围仍需扫描、解析相关整表，不能把减少保守冲突写成已经消除了历史增长成本。

该组工作在既有测试中加入九个故障/并发观察窗口，没有增加测试函数。另一个启动边界修复
保留 plugin 安装期间已经到达的 HostStop：消费者启动前检查该停止意图，broker 沿原队列
完成停止并收集未派发回执。它不把真实 authority loss 或原请求 Unknown 转成成功。

### 原始失败与最小 fixture 复测

`5f1e426a` 的[完整定向回归](daily-readiness-20261122/scoped-5f1e426a/summary.json)
于 2026-11-21 21:42:06–21:51:35 UTC 执行 17 项：15 通过、2 失败，release 被门禁阻止。
其中 129 MiB native 完整链、两项普通 T2、CLI printf、broker 停止、真实进程暂停/恢复及
八项 archive admission 已实际通过。CLI 子进程的内层 harness 单独保留，不重复计数。

两项失败分别是首次 claim 的负控仍预期旧报错，以及新增独立任务在 authority 激活后使用
裸 WriteBatch 装配。前者改为明确验证 claim 3 / rows 4 的 selector 缺失拒绝；后者把独立任务
移到 authority attach 前的初始批次，所有 claim 仍走激活后的实际产品路径。

`b4d115d8` 的[两项实际复测](daily-readiness-20261122/scoped-b4d115d8-r2/summary.json)
中，首次 claim 通过（54.30 秒）；派发在 6.41 秒拒绝，因为独立任务缺少 `native_input`，
实际 claim 返回空。失败库中的该任务仍为 Pending，尚无 operation、attempt 或 E1；
这是一份失败后只读状态，不能冒称拒绝事务瞬间快照。随后两行 fixture 修复补齐与既有
parent/child 相同的输入，产品准入规则和派发/恢复断言均未放宽。

此前还保留了一次[仅观察器 preflight 的失败](daily-readiness-20261122/scoped-b4d115d8/summary.json)：
复用检查把 CLI 子进程和外层两行摘要误认为多个目标，因而在 cargo 前退出，实际执行测试数为零。
修正后的 runner 明确取单目标的最终外层摘要，并记录被排除的内层结果。
这些失败均保留原始字节、哈希和退出码，不被后续通过覆盖。

`0c6b013e` 的[唯一一项实际复测](daily-readiness-20261122/scoped-0c6b013e/summary.json)
于 22:01:08–22:01:35 UTC 完成，dispatch 通过（23.56 秒）。此前 15 项 `5f` 成功与一项
`b4` first-claim 成功单独列为复用证据；产品路径字节与 `5f` 相同，first-claim 函数正文与
`b4` 相同。因此该组 17 项定向覆盖已补齐，但不能写成 `0c` 当次重新执行了全部 17 项。
门禁据此开始 release 构建；构建与后面的实际模型旅程各自保留独立回执。

### `0c6b013e` 的三次真实浏览器旅程：仍为 0/3

该候选的 [release manifest](daily-readiness-20261122/final-0c6b013e/release-manifest.json)
记录 22:01:35–22:09:19 UTC 的实际成功构建。二进制为 90,162,848 字节，SHA256 为
`8765fb320736ec0d1cd0c3e454fc3a3855d0959442230d1490344ca5dae12ff1`。
[原 batch 回执](daily-readiness-20261122/final-0c6b013e/batch-summary.json)
记录 22:10:15–22:16:47 UTC 三次尝试全部完成观察、全部失败，未尝试数为零；
wrapper 于 22:16:50 UTC 结束。三个原进程均退出 1，owned 进程组已回收，不能算干净关闭。
观察器、任务输入规则、模型预算和部署模型均未为这次重跑调整。

[逐次汇总](daily-readiness-20261122/final-0c6b013e/compact.json)和
[原始字节索引](daily-readiness-20261122/final-0c6b013e/index.json)保留 89 份原件、原退出码和分母。
三次观察分别持续 112.228、104.311、174.177 秒。独立 sampler 和模型后验身份核验均退出 0，
服务 PID、start ticks、boot ID、命令行及三份活动配置哈希在整个批次前后一致。
本次只人工确认了 attempt-001 的 first-submitted 截图中文可读；这不等于完整 UI 旅程成功。

[三份结束后只读容量审计](daily-readiness-20261122/final-0c6b013e-native-e1-capacity.json)
证明每份原 printf 观察都有成功的原 operation、attempt 与 E1，精确 marker、exit code 0
和 postcondition 仍保留；但三份均没有 T2 task result，也没有 native evidence materialization。
各自的原 seal debt 都明确写着 `CAS closure byte budget exceeded`。
真实 paired T1 都引用同一个 90,162,848 字节的注册实现，超过旧 E1 全闭包的
67,108,864 字节上限 23,053,984 字节。这是证据封装容量拒绝，不能改写为 printf 未执行或磁盘 IO 失败。
三个 model child 的原请求均以 Unknown 保留，未完成最终结果、暂停重启或下一条消息验收。

另一个原 dispatcher 错误是 `subject predecessor changed before snapshot capture`。
它需要单独处理新动作准备与主 S1 推进的交错，不能用容量错误代替它的归因。
原错误没有携带 action ID；结束后的原 chosen/materialization 清单能约束推断，不能伪装成失败瞬间调用栈。

[本次完整计时](daily-readiness-20261122/final-0c6b013e/timing.json)
保留三次日志共 1,180 个 scope，全部配对，无丢失或无效记录；scope 退出仍包括失败路径。
常规 [CI 44954542873](https://github.com/cyjin-yl/shenko/actions/runs/44954542873) 的六项检查及
完整 [Nightly 44954638293](https://github.com/cyjin-yl/shenko/actions/runs/44954638293) 的两个 job
随后均自然通过。它们不把这份真实旅程的 0/3 改成成功。

### 保留停止观察失败及最小修复的实际回执

较早 `5f1e426a` 的[完整 CI/Nightly 原件](daily-readiness-20261122/ci-nightly-5f1e426a/summary.json)
除两份已定位的 fixture 失败外，还保留 Nightly 的第三个失败：原 start-boundary 在三秒内未观察到
请求取消。其最后清理产生的 EOF 不能倒填为屏障内取消；原记录不足以把该次超时唯一归因给 Store 锁。

独立源码检查发现，Node 已确定没有 authority stamp 时，旧 acquire 路径仍先读取可能被阻塞的
Store binding。`be9cfe1b` 让这一已知失权先触发 recovery cancellation；有 stamp 时的原
binding/incarnation 校验不变，也没有放宽三秒条件或把 Unknown 转为成功。
[实际 be9 回执](daily-readiness-20261122/scoped-be9cfe1b/summary.json)
保留单元测试因 E0282 未编译、零实际执行，以及原 start-boundary 测试通过（73.29 秒）。
[原事件顺序](daily-readiness-20261122/scoped-be9cfe1b/start-boundary/recovery-stop-order.json)
在 authority 改变后观察到原请求取消，之后才发送 SIGINT、观察 host stop、释放 writer 和看到 EOF。

`8f14a704` 仅补测试通道的 `()` 类型注解，产品字节与 be9 一致。
一次 [transport preflight 失败](daily-readiness-20261122/scoped-8f14a704-preflight/summary.json)
发生在源码准入和 cargo 之前，实际测试数为零；修正观察器换行解码后，
[唯一单元测试实际通过](daily-readiness-20261122/scoped-8f14a704-r2/summary.json)：
1 passed / 0 failed，执行 12.79 秒，原进程和外层 wrapper 均退出 0。
be9 的进程成功明确列为复用，未重复运行、未重复计数；这些回执不证明远端模型进程已停止。

固定产品候选仍需取得完整成功浏览器回执和重复运行结果；本记录尚不宣称首条完整真实任务已收口。
