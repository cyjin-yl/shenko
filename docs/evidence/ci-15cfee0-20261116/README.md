# CI 15cfee0：实际 SSH 通过与 Store 零写检查失败

## 候选与原件

[CI 43886488027](https://github.com/cyjin-yl/shenko/actions/runs/43886488027)
在 2026-11-16 14:30:43–14:43:16 UTC 观察了 PR 305 候选
`15cfee0317a6f0a7c3e088657040531c6fdd8575`。该轮总体失败。
本记录保留这次失败，不把后续修复或其他候选的结果算进本轮。

SSH 构建回执记录实际 checkout 为 PR merge
`7b59a3366df264432c368a8a617aa4f64982ed1d`。
[GitHub 比较原件](github/compare.json)报告从候选到该 merge 无文件差异；
二者 tree 均为 `6238291fb12a178f495393fc80fc64e20d74fc61`。
回执原有 `source_dirty: true` 保持不变。
[独立逐项比较](derived/source-input-comparison.json)确认回执列出的 97 项
guest 源输入 SHA-256 全部匹配候选，不能据此把整个工作目录改称 clean。

[Rust job 133365943517](https://github.com/cyjin-yl/shenko/actions/runs/43886488027/job/133365943517)
的原始 [rust-evidence artifact 14235587723](https://github.com/cyjin-yl/shenko/actions/runs/43886488027/artifacts/14235587723)
为 8,247,230 bytes，ZIP SHA-256 为
`529537d2177fbe71bd00df847eaf24c909988e1839fb4536918bf93c3d3b8b72`，
与 GitHub artifact 元数据一致。ZIP CRC 检查通过，包含 1,559 个文件，
解压共 185,118,055 bytes。原 ZIP 保留于诊断工作区；本目录保留完整
workspace 日志和 SSH 关键原件，不复制大批兼容性数据库。

[observation.json](observation.json)列出每个保留文件的原路径、字节数、
SHA-256 和变换方式。[workspace-check.log.gz](workspace-check.log.gz)与
[workspace-test.log.gz](workspace-test.log.gz)是完整原日志的可逆 gzip，
没有截断、去 ANSI、删行或归一化。解压后的 test log SHA-256 为
`69ad82d230a0ee96c42418cd59e8a748c41080a3c0aabc76035cd1269a1b4585`。
原日志中失败断言输出的两条数 MB 数组行也完整保留。
guest 构建日志与两份含 CRLF 的 OpenSSH 日志同样使用可逆 gzip；
原换行和字节没有为适应文本检查而改写。

完整 job 日志 API 两次返回 `Transport closed`，通用读取接口拒绝该
日志 URL；错误原件保留在 `github/`。本目录拥有 artifact 内完整的
workspace check/test 输出，**没有取得 job 的 setup、checkout、post-job
整份 stdout**，不将前者冒充后者。

## 实际门禁与旅程

各 job 结论来自保留的 [jobs 元数据](github/jobs.json)。本次没有另行下载
其他五个 job 的工件；它们的结果只按该次 API 结论记录。

| Job | 结论 |
| --- | --- |
| kernel-boundary | success |
| rust | failure |
| docs | success |
| clippy-ratchet | success |
| docs-migration | success |
| web | success |

Rust 的全部 workspace target 检查、固定版本 SSH WASI 构建和工件保留步骤
成功；workspace tests 步骤失败。完整日志末尾明确只有一个失败 target：
`-p shenko-store --test store_roundtrip`，该 target 为 37 passed、1 failed、
0 ignored，执行 6.03 秒。唯一失败 case 为
`bootstrap_identity_survives_reopen_without_an_index_and_rejects_divergence`。

以下结果均有该轮原日志中的实际 `... ok` 行，精确 case 名与行号列在
`observation.json`：

| 范围 | 实际观察 | 证据边界 |
| --- | --- | --- |
| CLI 安装与命名 SSH WASI 执行 | 唯一 OpenSSH 旅程通过，203.75 秒 | 真实 CLI、安装 CAS、选中工具、loopback OpenSSH；不是远程设备部署 |
| 启动暂停与重开 | `public_start_pause_survives_process_reopen_and_resume_alone_admits_inference` 通过，1.40 秒 | 第一个实际 Start 正常 SIGINT 退出，重开保留 subject/request，resume 后才发 HTTP；观察端没有提供模型答案 |
| 星形少数组件 | `minority_agreement_survives_reopen_and_supersession_fences_original_branch` 通过，0.34 秒 | 七份真实持久 Node 状态，三成员少数组件与无直连的两个 follower；同进程协议观察，不是三台机器或三个驻留 Start 的 SSH 路由证据 |
| canonical native arm | 原 intent 与恢复不重新发执行权的旅程通过 | 精确前驱及 Current/Prepared/Learned 边界；不能推出 foreign Running 接管 |
| canonical native dispatch | prepare、learn、restart 中保留唯一 live permit 的旅程通过 | 不把旧 prepared 第一次 choose 解释成 fresh 执行权 |
| canonical native T2 | 两条完成、finish、重开及 foreign debt 旅程通过 | 不把前驱不兼容的 foreign debt 解释成执行恢复成功 |
| canonical S1 | distinct-ledger 原结果物化与冲突/SQL 故障原子性两条旅程通过 | 不等于全部跨 ledger transition 已完成 |
| context inspection | 独立主体隔离旅程通过；本节唯一失败 fixture 的第二次零写比较失败 | 失败点之后的 Handoff 等断言没有执行，不能称该完整旅程已通过 |

## Store 失败的逐字节结果

断言在候选 `crates/shenko-store/tests/store_roundtrip.rs:305:9`：
`assert_eq!(read_tuple()?, before, "route inspection changed the live SQLite tuple")`。
因此原日志的 left 为 after，right 为 before。
从两条**完整原数组**解析出的 [差异记录](derived/tuple-difference.json)为：

| 成员 | 字节数 | 精确比较 |
| --- | ---: | --- |
| `head.db` | 1,323,008 | 全部相同 |
| `head.db-wal` | 82,432 | 全部相同 |
| `head.db-shm` | 32,768 | 唯一 offset 104：before 9 → after 20 |

源码中第一次 `before = read_tuple()` 后，原 raw metadata 和缺 CAS 的拒绝
检查已经完成一次精确零写比较。随后 fixture 在第二次比较之前调用了
**活 Store 连接**的 `reopened.activation_selector(agent)`，取回 route
引用。这次 live SELECT 位于待测的零写窗口内。

该调用位置与唯一 SHM reader mark 字节差异一致，支持“fixture 自己的
活连接读取污染了观察窗口”的诊断；本记录没有把这项推断改写成 staged
inspector 写库的事实，也没有忽略原来的失败。正确的修复方向是把
fixture 的 selector/CAS 准备读取放到 `before` 之前，继续保留原始三文件
逐字节相等断言。后续修复必须取得自己的运行结果。

## SSH 的实际阶段和停机

[构建回执](ssh-wasi/build/build-receipt.json)保留 Rust 1.98.1、WASI SDK
clang 23.1.0-wasi-sdk 的版本与编译器 hash。实际 guest 为 2,552,689 bytes，
SHA-256 `446fbc2f21751203bcba27887d13e938f4ed54e75ee465266b57f3345242b988`。
[旅程原件](ssh-wasi/openssh-journey.json)同时保留 CLI executable hash、
安装/环境 generation/selector、unit pointer 与实际 WASM 的两层身份，
以及安装前后相同的 installation、subject、branch、origin。
安装输入文件已移除，之后六次实际命令均按选中 `ssh` 工具从安装内容执行。

每次 `command_started` 和 `command_observed` 均记录显式 120 秒 run 预算与
180 秒外层 CLI 预算。run 的 `budget_started` 在 `Module::new` 之前；
编译、setup、socket 等仍计入原运行截止。artifact verification 在该
run 预算开始前，仍受外层 CLI 期限约束。本记录不改变生产默认值，
也不把这轮时序解释成较早 30 秒失败的唯一已证实原因。

以下为原日志派生值，单位秒；完整精度与 UTC 起点见
[ssh-timings.json](derived/ssh-timings.json)。原 stderr 保留 ANSI 和原字节。

| 命令 | 验证 artifact | 编译 | 实例化 | `_start` | run 总计 | CLI 总计 | 实际退出码 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| keygen | 7.444431 | 25.452852 | 0.003005 | 0.003246 | 25.460747 | 32.958389 | 0 |
| exec | 5.985404 | 25.356163 | 0.000545 | 0.130744 | 25.489621 | 31.514041 | 7 |
| SFTP put | 6.004677 | 25.475528 | 0.000498 | 0.421185 | 25.899551 | 31.959517 | 0 |
| SFTP get | 5.983937 | 25.392273 | 0.000530 | 0.513769 | 25.908924 | 31.923872 | 0 |
| 错误 host key | 6.032121 | 25.386413 | 0.000554 | 0.056911 | 25.446303 | 31.517325 | 255 |
| 缺网络 grant | 6.327520 | 25.371869 | 0.000506 | 0.002175 | 25.375984 | 31.756935 | 255 |

真实 exec 返回预期的 exit 7，并分别保留 `ssh-wasi-exec\n` stdout 与
`ssh-wasi-stderr\n` stderr；100,003 字节二进制 SFTP 上传、下载均精确
校验。错误 host key 没有创建命令 sentinel；缺网络 grant 返回
`no host-admitted TCP stream`。后两项是被实际拒绝的请求，不能改称
算法协商测试。

[processes.jsonl](ssh-wasi/diagnostics/processes.jsonl)记录 bootstrap 收到
SIGINT 并 joined exit 0，安装、inventory 及所有 guest 子进程均有实际
join；OpenSSH 收到 SIGTERM 并 joined exit 0，`forced: false`。
没有观察到 process deadline 事件。子进程 leader 已 join 不额外证明
所有可能的后代进程均已退出。

这里的独立启动边界测试在 resume 后由有界 kill/join 收尾；只证明
首次 paused runtime 的正常停机与同安装重开，不声称第二次 active
runtime 也已走优雅退出。HTTP observer 不生成模型结果，因而不能把
该测试或 SSH 成功当作真实模型输入→child→输出→重启闭环成功。

本轮还未包含之后的本地 provisioning 与三个独立 Shenko 进程切片；
没有浏览器实测、物理网络分区、7 天或 30 天运行验收结果。
