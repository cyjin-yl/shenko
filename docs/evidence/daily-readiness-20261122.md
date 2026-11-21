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
原失败安装、退出码和分母均保留，重跑回执须另行记录。

固定最终 release 候选的完整浏览器回执和新候选计时仍待取得。
在这些回执写入前，本记录不宣称首条完整真实任务已收口或重复运行已通过。
