# CI 9b5a6cb：Store 与 SSH 通过，三进程旅程停在成员核对

## 候选与原始证据

[CI 43894092386](https://github.com/cyjin-yl/shenko/actions/runs/43894092386)
于 2026-11-16 15:13:48–15:27:16 UTC 运行，结果为 **failure**。
候选为 `9b5a6cb8987806879b8cfb713321744bd087bc0e`，独立基线为
`a574705ca742c11aa7b0a05203c3473a6c4103fe`。完整 job 日志确认实际 checkout
为 PR merge `6621968fbb2528475157d30488215ec16fa62e90`；
[run 原始元数据](github/run.json)与[merge 元数据摘录](github/merge-metadata.json)
记录二者 tree 同为 `6d48becc87eacfb1f00c2b050e6b27987b663e86`。
本记录没有把之后的 main 整合或 fixture 修复算进该候选。

[Rust job 133392924986](https://github.com/cyjin-yl/shenko/actions/runs/43894092386/job/133392924986)
的 [rust-evidence artifact 14237854481](https://github.com/cyjin-yl/shenko/actions/runs/43894092386/artifacts/14237854481)
共 8,266,536 bytes，ZIP SHA-256 为
`09cdd9fbac8c84201e4da9f5772a660b030f027883d7634b30e8c481f5be62d7`，
与 GitHub 元数据一致。全部 ZIP CRC 检查通过，共 1,744 项、
176,954,107 bytes 解压内容。[完整工件清单](artifact-manifest.json)
记录每一项的原路径、长度、CRC 和 SHA-256。原 ZIP 完整保留；这里保存
必要成员原件、SSH 原件与完整日志，没有复制兼容性数据库目录。

本轮成功取得[完整 job 日志](job.log.gz)，包括 setup、checkout、构建、
测试、artifact 上传、post-job 和清理。GitHub 日志工具返回的 UTF-8
文本保留原 BOM；未去 ANSI、删行或归一化。原日志为 484,030 bytes，
SHA-256 为 `2bf79356d9f8428988f1b8b70cc583577f5f4e8f2a00c5916faf95689a501eda`。
[workspace-check.log.gz](workspace-check.log.gz)和
[workspace-test.log.gz](workspace-test.log.gz)也保存完整原输出。
所有 gzip 均可逆，解压后逐字节核对；文件及原件 hash 列于
[observation.json](observation.json)。这次完整 job 日志获取成功，
与上一轮仅有 artifact 内 cargo 输出的范围不同。

## 实际结果

[jobs 元数据](github/jobs.json)记录 docs、docs-migration、kernel-boundary、
web、clippy-ratchet 五项 success。Rust 的全目标检查、固定 SSH guest
构建与工件上传成功；workspace tests 失败。完整测试日志末尾只列出
一个失败 target：`-p shenko --test component_process`。

| 旅程 | 本轮实际结果 | 边界 |
| --- | --- | --- |
| 三个独立 Start 的 SSH 星形旅程 | failed，69.02 秒 | 停在成员配置的前置核对，尚未启动后面的三个驻留进程 |
| Store roundtrip | 39 passed，0 failed，0 ignored，6.31 秒 | 包括新 canonical first claim、既有 arm/dispatch/T2/S1、精确零写与 Handoff 拒绝 |
| 真实 CLI 安装与命名 SSH WASI | passed，204.51 秒 | 真实安装 CAS、选中工具、loopback OpenSSH、执行与二进制 SFTP；不是物理多机部署 |
| 启动暂停、停止、重开与 resume | passed，1.41 秒 | 首次暂停进程正常停止，同一 request 重开，resume 后才准入推理；HTTP observer 不提供模型结果 |
| 少数组件协议旅程 | passed | 真实持久 Node 状态的协议旅程，不能替代本轮未到达的三个驻留 Start 旅程 |

精确 case 名及原日志行号保存在 `observation.json`。其中
`canonical_first_claim_preserves_s1_birth_and_never_reissues_a_recovered_worker`
实际通过；旧准备首次 choose、已学习恢复等窗口没有因获得原 claim 而
重新发 fresh worker 许可。此结果仍不能推出全部 foreign Running 效果接管。

`bootstrap_identity_survives_reopen_without_an_index_and_rejects_divergence`
实际执行完毕，包括 main/WAL/SHM 三文件精确字节相等、缺 CAS 拒绝、
复制但未选中的 slot 拒绝和 unresolved Handoff 拒绝。
前一候选失败的原断言没有删除；fixture 先完成活连接 setup 读取，再测量
零写区间。本轮证明该完整旅程通过；Handoff 的候选没有实际启动，
不能将拒绝歧义路由的检查描述为升级或激活完成。

## 唯一失败：把本地学习时间当作共识身份

原错误为 `member configuration was not actually retained`。
[完整比较](derived/membership-comparison.json)保留两个实际返回的 tuple：

| 字段 | B 的实际结果 | A 的实际结果 |
| --- | --- | --- |
| revision | 3 | 3 |
| term | 4 | 4 |
| members / joint_new / digest | 同一七成员配置、无 joint，新配置 digest 一致 | 完全相同 |
| committed_at | 2026-11-16T15:21:54.265924055Z | 2026-11-16T15:21:56.710511436Z |

只有 `committed_at` 不同。六次真实 `members push` 原回执都报告
index 6、term 4、同一完整 prefix digest；第一轮读取 A 的完整配置时
fixture 已失败，因此不宣称它后来读过其余成员的完整 metadata。
[commands](component/commands.jsonl)、[原失败结果](component/result.json)和
[清理结果](component/cleanup.json)未改变。清理记录 `errors: []`，
只证明已拥有的进程 leader 与 SSH listener 被收尾，不证明任意逃逸后代。

这个差异与既有生产语义一致：

- `Node::apply_control_entry` 在 bounded Append 首次学到配置时，保存该节点
  的 `Utc::now()`。`ConfigurationEntry` 不含此时间；`control_value_digest`
  绑定 index、term、kind、payload，`config_digest` 绑定成员集合。
- quorum、投票与 `ComponentPresence` 使用配置的 revision、digest 和成员；
  没有用 `committed_at` 推断远端新鲜性或物理效果停止。
- 完整 `MemberConfiguration` equality 确实用于同一 Node 的 stale snapshot
  检查，包括 `open_availability_branch_inner`、RuntimeAuthority 恢复和
  isolated episode 复用。这个本地字段必须保持稳定，不能删掉或规范化。
- 显式 full-prefix `adopt_member_configuration` 对新学到的配置另有保留源
  `committed_at` 的语义；本次 CLI `members push` 使用 bounded transfer，
  没有将这两种路径混为一谈，也没有修改任何生产时间或历史记录。

实现依据在 [election.rs](../../../crates/shenko-node/src/election.rs)、
[control.rs](../../../crates/shenko-node/src/control.rs)、
[control_transport.rs](../../../crates/shenko-node/src/control_transport.rs)与
[component.rs](../../../crates/shenko-node/src/component.rs)。

后续独立 fixture 修复 `1784d91cf59da524dc6781012ee4db99dc3ad206` 严格比较
五个共识字段与每节点完整 `ControlFrontier`，再通过另一真实 CLI 进程
重开，要求该节点自己的完整配置 metadata（包括学习时间）完全相同。
七成员/三成员少数、安装身份、证书、无模型 follower、最终权限拒绝与
全部后半旅程断言保留。没有新增 case、ignore 或生产协议变更。

## SSH 时序与后续编译检查

实际 guest 为 2,552,689 bytes，SHA-256 为
`446fbc2f21751203bcba27887d13e938f4ed54e75ee465266b57f3345242b988`。
[构建原件](ssh-wasi/build-receipt.json)中的 `source_dirty: true` 原样保留；
[独立逐项核对](derived/source-input-comparison.json)确认 97 个声明源输入
全都匹配候选 git 对象，不据此改称整个目录 clean。

[SSH 旅程原件](ssh-wasi/openssh-journey.json)保留安装前后相同身份，
安装源输入已移除，六次执行按选中 `ssh` 从安装内容解析。
实际 exit 依次为 0、7、0、0、255、255；后两项分别为错误 host key 和
缺网络 grant 拒绝。二进制 SFTP payload 为 100,003 bytes。
六次冷编译为 25.293–25.496 秒，`_start` 为 0.002–0.506 秒；
[完整精度时序](derived/ssh-timings.json)列出原 deadline 起点、编译、
实例化、执行与 CLI 总耗时。显式 run 预算 120 秒、外层 180 秒，
编译/setup 仍计入 run 截止；生产默认 60 秒没有因 fixture 改动。
bootstrap SIGINT 和 sshd SIGTERM 均实际 joined exit 0，`forced: false`。

root 后续在整合源码 `c1c915fa` 上执行了
`cargo check --offline --locked -p shenko --test component_process -j1`，
exit 0、耗时 1 分 29 秒。完整原输出保存于
[followup/component-local-time-metadata.log.gz](followup/component-local-time-metadata.log.gz)，
完整源码 SHA、原日志 SHA 和范围列于 `observation.json`。
这只是目标 metadata 编译，不是旅程执行，也不能把本轮原 failure 改为 success。

本记录没有新的真实模型、浏览器、物理分区、7 天或 30 天运行结果。
