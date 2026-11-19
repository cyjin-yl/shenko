# CI 7905e841：六项 gate 通过，首次原生 E1 九窗口实际通过

[Run 43928793872](https://github.com/cyjin-yl/shenko/actions/runs/43928793872) 于 2026-11-16 18:40:08 UTC 完成为 success。docs、docs-migration、web、kernel-boundary、clippy-ratchet、rust 六项全部通过。同期 [43928793776](https://github.com/cyjin-yl/shenko/actions/runs/43928793776) cancelled 原记录保留，不计为另一次通过。

公开 head 为 `7905e8413b96015d96ea301bd1f3b6937dcfff90`。真实 job checkout 为 `419ca5c6efc5b9eb5c156672e7474794ee0667dc`，tree `241909742fd73225b08b88fea967197a74f65334` 与 head 相同；实际 baseline 为 `e5fbf732124ee035f235a5a9f1a20308dad5d062`。原 Rust 日志、GitData checkout 原件和 Clippy ZIP 的 base.json 相互绑定，不能用当前 PR merge 字段代替实际运行源码。

## 运行结果

| 实际范围 | 结果 |
| --- | --- |
| 全 workspace check 与 pinned SSH WASI guest 构建 | 通过 |
| 三个真实 Start 进程的 pinned SSH star | 85.28 秒通过 |
| first native E1 原九窗口旅程 | 28.09 秒通过 |
| Rust job 的 152 条 test-result 汇总 | 1,079 passed、0 failed、11 ignored、88 filtered；包含该 job 输出的 unit/integration/doctest 汇总，不作为完成比例 |
| Clippy 双树 ratchet | errors=[]，独立原 baseline/candidate 见 ZIP 内 base.json |

上一轮 51a 的 acknowledgement setup 失败没有改写。713190cf 只使测试 body 使用现行 reconciliation schema 9，并让 CAS codec major 一致；九窗口、两次外部效果、T2=0 及 Store 校验都保留。

本次九窗口包含：first definite 与 Unknown、认证交换中断后继续、缺本地 T1 后补齐、仅停放 archive 不代替物化、分页、未知 codec、后续观察后的 replay、缺 source proof 的债务。它们是同一已存在测试的窗口，不是新增九套重复测试。

[native-e1/observations.jsonl](native-e1/observations.jsonl) 保留首次 applied=2、重放 already_applied=2、原 receipt／源 CommitRef／observed_at、不同 receiver ledger 的本地 open case、physical_effect_count=2 与 chosen_t2_count=0。完整 27 个 native-e1 输出原件保留，包含认证 SSH 过程、原 CLI 输出、命令、进程及清理结果。

### 实际通过的边界

控制 grant 仍是 retained-value Store fixture；测试真实运行 OpenSSH、CLI 和 CAS 交换，不等于真实模型 quorum election 或物理多设备分区。交换中断窗口是 SSH 认证后、CLI 执行前的受控退出，不伪称已产生回应后丢失网络包。后继 E1、一般 child／多 operation canonical transition、自动 Gap 修复和 Store-aware GC 根／排他仍未由本旅程完成。

原 retained 输出包含 receipt 引用，但原临时 private 目录的全部 ledger/CAS 原字节未包含在这 27 个文件中。当前证据支持精确源码下的实际断言通过；不能声称全部临时 CAS 已归档。正在沿同一 fixture 补齐安全留存，原 SSH 私钥和凭据不会作为普通证据发布。

## 完整原件

- [Rust ZIP](rust-original.zip)：artifact 14258012778，8,339,388 bytes，SHA-256 `0a3e8622533b889df0cd66a6d5dc0942eb50784a8367d35ec4f9c0273f5c71bb`；1,867 个成员均独立校验 CRC 与 SHA-256。
- [Clippy ZIP](clippy-original.zip)：artifact 14256759220，2,730,084 bytes，SHA-256 `3ca26ccc8429c976e404afe79cdbdb69c6e64ce02a610de752891c544a85f01e`；18 个成员均独立校验。
- [完整 Rust job 日志](rust-job-133515356476.log.gz)：解压后与 connector 原文 473,287 字符逐字读回一致。
- run、jobs、artifacts、cancelled run、checkout、全 member 清单、verification、workspace counts 和 Clippy 原 ratchet 均在同目录；archive-index.json 给出归档字节哈希。

这些结果绑定 7905／419ca5c6 实际 source tree。后续 main 合并、handler 说明或产品逻辑变更需要按影响验证；本轮通过不代表真实模型单机闭环、S2、Node/Bun、真实服务、屏幕阅读器、7／30 天或完整 S1–S5。
