# CI bf49f387：原生闭环与容量旅程通过，驻留启动与 lint 仍失败

[CI 43905866500](https://github.com/cyjin-yl/shenko/actions/runs/43905866500)
于 2026-11-16 16:17:34–16:36:17 UTC 实际运行，最终为 **failure**。
候选 `bf49f38733bc69975f8825c3f5056b227e83adc0` 的实际 PR checkout 为
`92135ff8956c425748baa2560e1fbafed72498d4`；二者 tree 同为
`e0d8e9fb2b2f472c8f150fc7c62c216db8479a21`。独立基线是
`f5c6bfa573bf2c96cb84dd2164759329732b9d8e`，不是随后前进的 PR303 或 main。
[run](github/run.json)、[jobs](github/jobs.json)、[merge](github/checkout-merge.json)
及 [kernel baseline](github/kernel-base.json) 保留原始绑定。

## 实际通过与尚未到达

docs、docs-migration、kernel-boundary、web 四门成功。Rust 全目标检查、
固定 SSH WASI guest 构建成功，workspace tests 失败；Clippy 独立双树比较失败。
完整 workspace 输出末尾只有 `-p shenko --test component_process` 一个失败 target。

| 本轮实际旅程 | 结果 | 证据边界 |
| --- | --- | --- |
| Store roundtrip | 40 passed，0 failed，0 ignored，8.69 秒 | 新 canonical native 九窗口旅程实际通过，保留原 E1 与唯一效果；不能推出一般多 operation、child、独立 E1 或 Unknown 链全部完成 |
| Subject | 116 passed，0 failed，7 个原有 ignored，44.97 秒 | 新纠错容量等待、取消、重开和 oversized tail 拒绝旅程通过；没有增加忽略项，也不称整个 workspace 零忽略 |
| 安装后的 SSH WASI/OpenSSH | passed，246.55 秒 | 原真实 CLI 安装、具名 guest、认证、执行、二进制 SFTP 和拒绝路径；不是物理多设备验收 |
| 原 Start pause/stop/reopen/resume | passed，1.89 秒 | 本候选原旅程通过；下述新 pending-metadata 取消窗口尚未在此轮源码中 |
| 三驻留 Start 的 SSH 星形旅程 | failed，270.89 秒 | 七成员核对与 A/C fork 已到达；B 在实际模型元数据等待中无法按时 ready，后半共同证书、renewal 与最终权限断言未到达 |

[精确 case 与行号](derived/workspace-result-lines.json)指向未经删行的
[workspace-test.log.gz](workspace-test.log.gz)。新通过的两个 case 分别为：

- `canonical_direct_native_chain_retains_original_e1_and_only_one_effect`；
- `loop_runner::recovery_tests::correction_capacity_waits_cancels_reopens_and_never_seals_an_oversized_tail`。

原生链限定于本地实际物化的 canonical S1、first direct-root claim、paired
arm/dispatch、first definite E1 和无 ordinary child 的 `native.complete`。
源 E1 身份、CommitRef 与时间保留，接收安装的物理 receipt 另记；缺 optional
proof 时原 E1 与 seal debt 保留，不能重发效果或伪造完成。
独立 E1 传播、Unknown、后续 operation/child、发布与完整 Store-aware GC 不在本轮通过范围。

容量实现读取实际 served `meta.n_ctx`，严格核对 advertised list 和绑定模型，
冻结 schema 14 的 payload 布局、父 seal 与原输出 reserve。纠错等待/取消/拒绝
保留 `ResultRecorded`，R0 重新核对完整 tail、预算和新执行观察。
这些 Rust 结果没有把具名启发式计数变成 provider tokenizer，也没有证明模型任务成功。

## 驻留失败的真实边界

[原失败结果](component-process/result.json)为
`b readiness timed out; original logs retained`。
上一轮的成员时间比较问题已经越过：七个节点的五个共识 identity 字段、完整
`ControlFrontier` 和各自另一个 CLI 重开返回的完整本地 metadata 都被检查。
[membership](component-process/membership.json)、[A fork](component-process/a-fork.json)
和 [C fork](component-process/c-fork.json)与完整命令输出均保留。

B 在 `16:26:47.803529Z` 实际记录 durable startup pause，boundary 为其 ledger
seq 12。随后安装路径调用 `broker.discover_capabilities`，向测试中从不 accept
的 TCP listener 等待实际只读 HTTP GET。暂停禁止准入主体推理，不禁止能力元数据观察。
原 fixture 把这两种行为混为“从未接触模型地址”，因此它既没有提供 HTTP 拒绝，
也没有让 B 正常完成这一启动阶段。

B 在 `16:29:40.297609Z` 记录 `PROVIDER_MODEL_METADATA_MISSING`，随后才进入 UI
与正常停止。这里必须保留事件顺序：readiness 错误使 `journey` 返回，局部 listener
先被 drop，然后 fixture 才进行 SIGINT 清理。因此这次输出**不能证明 SIGINT
取消了正在等待的 GET**。原每个 HTTP 请求各有默认 180 秒、可配置更长的 deadline；
两个连续读取也不是一个统一 180 秒启动期限。

[component-originals.zip](component-originals.zip)完整保留全部 237 个 component
原成员，包括 resident stderr/stdout、全部 CLI 命令、process receipts、失败和清理结果。
[清理结果](component-process/cleanup.json)为 `errors: []`，范围仍是 owned leaders
joined 和 SSH listeners 检查，不扩张为任意逃逸后代的证明。

后续修复源 `8253eab6ec59357399e5d50da0fec013595bb613` 分别处理 fixture 和生产取消：

- component 使用真实、有界且显式 joined 的 HTTP 503 observer，保留完整请求字节。
  只允许精确的 `GET /v1/models HTTP/1.1` 和 `GET /metrics HTTP/1.1`；POST、其他请求、
  越界或不完整读取都使旅程失败。七成员、少数共同协调者、follower 权限和所有后半断言保留。
- 生产代码只在 read-only metadata await 上观察 SIGINT 和故障 token。用户 SIGINT
  正常早退，故障仍返回错误；Start 对两种早退都显式停止已有 consumers，同时保留原错误
  和清理错误。不取消整个 plugin 安装 future，也不改变已准入效果或生成期限。
- 原 `start_boundary` 在同一安装、同一 pause ID 中增加一个连续窗口：观察完整 GET 后
  保持连接且不回复，发 SIGINT，要求进程在原 30 秒停止界内 exit 0，并观察客户端 EOF/RST；
  随后继续原有 reopen、resume 和真实 POST。这个修改后旅程仍需新候选实跑。

## Clippy 的真实增量及后续编译

[完整 ratchet](clippy/ratchet.json)保存四项计数增量：

| 范围 | 包 / lint | 增量及源码原因 |
| --- | --- | --- |
| production | shenko-broker / unused_imports | +1：不再需要的 `anyhow::Context` import |
| all-targets | shenko-broker / unused_imports | +2：相同 import 在相应目标重复报告 |
| all-targets | shenko / needless_range_loop | +1：component 的节点索引循环 |
| all-targets | shenko-subject / unused_variables | +2：新旅程 `tokio::select!` 展开产生的 `outcome`、`entered` 绑定诊断 |

诊断定位原件在 [clippy-confirmed-diagnostics.json](derived/clippy-confirmed-diagnostics.json)。
后续提交 `45f11df1` / `ce18c5ec89163d19f07037e5e9489ef33c376fc9`
去掉实际未用 import，以 `enumerate` 保留原节点顺序，并用现有 futures 的
`select` 与显式 `Either` 分支保留原 pending future、提前完成拒绝、timeout 传播和取消断言。
没有加 allow、忽略项、改变 lint policy 或抬高独立基线。

`ce18c5ec` 的三个包 all-target metadata/Clippy 使用本轮 CI 原始 276 flags，
exit 0，耗时 1 分 58 秒，检查期间 source 不变。
`8253eab6` 的两个 CLI test target 后续 metadata/Clippy 同样使用这些 flags，
exit 0，11.16 秒。首次实现误用了 CLI 未声明的 futures，exit 101 的完整原件也保留；
最终实现改为标准库 `poll_fn`，没有新增依赖。
[五文件独立逐字核对](derived/metadata-stop-source-comparison.json)将最终检查字节
绑定到集成源码。它仅证明这些源码字节一致；不是整个集成候选的运行结果。

所有命令、源码摘要、环境、完整 stdout/stderr 和 docs 校验在 `followup/`。
后续独立提交 `a4eb2be1a886dc181c776d3b8bd7a216397ca15a` 让原
Start 旅程在成功和失败时都向 CI 明确目录保留顶层诊断文件；原安装目录不导出。
结果记录实际 source revision、原错误及确实观察到的连接结束，原清理顺序不变。
该留存调整只做格式、diff 和 YAML 静态检查，未在本地执行。

这些都是**编译 metadata**，不是修改后进程旅程的执行，也不是新候选的完整双树 ratchet。

## 原件完整性与保存范围

[observation.json](observation.json)记录各个保留原件的长度和 SHA-256；所有 gzip
都逐字节解压核对，未删除 ANSI、BOM、失败或中间输出。

| 原始工件 | bytes / 项数 | SHA-256 |
| --- | --- | --- |
| Rust ZIP | 8,143,986 / 1,745 | `f4cbadb9bb082dd01f43f8d5bbf009f12b717237aafc1335d3970e86c5e3fb3b` |
| Clippy ZIP | 2,718,668 / 18 | `6569942e0c1ad1039bc7b0f3cbc03cc331f34ba61d266d684f28c451a2cc6abb` |
| 完整 Rust job 日志 | 482,726 | `3913e25e85a0a4deb5b90f39d7e4e39727d73c2cf23c6a1dfda2355c4ed43bde` |

原始 ZIP digest 与 GitHub 元数据一致，所有成员实际读取并通过 CRC，
[Rust 全项清单](rust-artifact-manifest.json)和
[Clippy 全项清单](clippy-artifact-manifest.json)包含每项长度、CRC、SHA-256。
完整 Rust job、check/test 输出和其余四个 gate job 日志都保留。
Clippy job log API 两次返回 `Transport closed`，[原错误](github/log-fetch-errors.json)
没有删去；[clippy-original.zip](clippy-original.zip)仍包含全部四组完整 compiler
JSONL/stderr、policy、ratchet、lint discovery/probe，不能据此改称取得该 job log。

[ssh-originals.zip](ssh-originals.zip)保留 43 个非二进制 SSH 原件；
[journey](ssh-wasi/openssh-journey.json)与 [build receipt](ssh-wasi/build/build-receipt.json)
也可直接读取。原 guest SHA 为
`446fbc2f21751203bcba27887d13e938f4ed54e75ee465266b57f3345242b988`；
`source_dirty: true` 等原字段未被改写。原 Rust ZIP 在源工件与本地保留；此目录没有
重复复制兼容性数据库和 guest 二进制。重新打包的 component/SSH ZIP 每个成员都与原 ZIP
逐字节比较，改变的只有容器。

本记录没有第五次真实模型结果、独立 E1/Unknown 新旅程、Node/Bun WASM 安装可用性、
浏览器无障碍、物理多设备或 7 天/30 天结果。旧 9b CI 与第四次模型 failure 原件和结论保留。
