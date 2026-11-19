# CI 51a1ce43：三进程通过，首次 E1 在源确认准备阶段失败

这轮真实 CI 的五个独立门通过，Rust workspace 只有首次 native E1 旅程失败。三进程 SSH star 旅程实际通过；E1 旅程在源端 S1 acknowledgement setup 因旧 decision schema 被拒绝，尚未进入 SSH 传输，九个交付窗口没有取得运行通过。后续 `713190cf` 仅修正夹具版本，未编译或运行；不能用它改写本轮失败。

## 精确源码与运行绑定

- [CI run 43922860026](https://github.com/cyjin-yl/shenko/actions/runs/43922860026)，attempt 1，事件 `pull_request`；2026-11-16 17:52:09 UTC 开始，18:06:11 UTC 更新为 failure。
- 公开 head：`51a1ce435b3f83c52dcad11f19c522acb77cb59b`。
- Rust 实际 checkout：`61a813576b19b3aea0403ef6dd4fc3d64238ad0b`，是 PR305 的合并提交。完整日志第 457–460 行和 [checkout 原 API](checkout-commit-61a81357.json) 相互对应。
- head 与 checkout 的 tree 都是 `f7c39079c0788091c311c3f01264fa2846e398f3`。事件 baseline 是 `e5fbf732124ee035f235a5a9f1a20308dad5d062`；其身份来自 run 原 API 和 checkout 第一父提交。Clippy 的独立 baseline 解析步骤通过，但 ZIP 内的 baseline 文件尚未展开检查。
- 同 head 的 [43922858938](https://github.com/cyjin-yl/shenko/actions/runs/43922858938) 于 17:52:09 创建、17:52:11 更新为 cancelled；[原记录](cancelled-run-43922858938.json) 保留，不计为通过。

## 实际结果

| 检查 | 结果与原件位置 |
| --- | --- |
| docs、docs-migration、web、kernel-boundary、clippy-ratchet | 五个 job 全部 success；[完整 jobs API](jobs-43922860026.json) |
| Rust all-target check、pinned SSH WASI guest build | 对应步骤 success |
| 三个真实 Start 进程的 SSH star 旅程 | passed，85.61 秒；完整 Rust 日志第 3924–3930 行 |
| 首次 native E1 旅程 | failed，0.90 秒；第 3932–3949 行 |
| Workspace | exit 101；第 5876–5878 行明确唯一失败 target 为 `native_evidence_delivery` |

原错误为 `conflict: an older decision schema cannot carry reconciliation acknowledgements`。精确公开源码表明错误发生在 `source.commit_step_and_advance_head` 的 acknowledgement setup，早于 `fixture.prepare_ssh()`。不能把该失败称为认证传输已完成，也不能把未到达的九个窗口、最终两次效果和零 T2 断言算作通过。

## 原件与保留边界

[完整解码 Rust job 原文](rust-job-133494449011.log) 按 connector 返回的 UTF-8 文本原样保存，没有裁剪或假造 gzip。共 481,505 个字符；按换行分割为 5,933 项（含末尾换行项）。同目录保留 [run](run-43922860026.json)、[jobs](jobs-43922860026.json)、[artifacts](artifacts-43922860026.json)、[head](head-commit-51a1ce43.json)、checkout 与取消记录的完整 API 解码 JSON 文本。

Clippy 解码 job log 接口两次返回 `Transport closed`；[第一次完整响应](clippy-log-read-1.json) 与 [第二次完整响应](clippy-log-read-2.json) 均保留。它们是取证工具的失败，不是 Clippy job 失败；实际 job `133494449291` 为 success。

两个原 ZIP 已由 GitHub 下载接口返回可复用文件引用：

| 原件 | Artifact ID | GitHub 报告 SHA-256 |
| --- | --- | --- |
| rust-evidence，8,305,765 bytes | [14256045836](https://api.github.com/repos/cyjin-yl/shenko/actions/artifacts/14256045836/zip) | `2a8609e899f1858a4d27ab4fc4c63d5f8dda90c909b0fb31804f4ddd42e762b6` |
| clippy-ratchet-evidence，2,768,629 bytes | [14254808026](https://api.github.com/repos/cyjin-yl/shenko/actions/artifacts/14254808026/zip) | `f7292c315d1aa5bfe400c8a4ecabe922a4630823e25efe9855a82902d6c76523` |

准确 source URL 和 tool file_id 保存在 [observation.json](observation.json)。上述摘要由 GitHub 报告；本次环境失联后没有独立计算 ZIP hash、展开成员或把 ZIP 字节纳入此提交。ZIP 的持久字节归档与独立核验仍待完成。其余 artifact 的原 metadata 保留在完整 artifacts API，不假称已下载。

## 夹具修正与后续验收

[713190cfafe1c1b1c7c06b9aecb4aab2f361c894](https://github.com/cyjin-yl/shenko/commit/713190cfafe1c1b1c7c06b9aecb4aab2f361c894) 仅修改 `bin/shenko/tests/native_evidence_delivery.rs`：acknowledgement body 使用既有 reconciliation decision schema 常量（9），CAS codec major 取同一值。共享 helper 已封存真实 docket 的 schema 13 S0；Store 的 case 身份、当前状态、理由 receipt 和上下文校验均保留。

全部原九个窗口、效果计数、恢复和 T2=0 断言原样保留，没有新场景、产品逻辑改动或校验放宽。本地环境在修正期间不可用，候选仅经过精确源码/合同核对与一文件 compare，下一 CI 才能判断实际旅程是否通过。离线未提交的后继 codec 3 草稿完全未包含。

此处结果只覆盖具名候选和实际执行范围。E1 旅程的 control grant 仍为测试夹具，Store-aware GC 根生产者与排他、runtime GC 装配仍未完成：完整 registry 声明可触发拒绝回收，但漏报 namespace 不会自动获得保护。三进程通过也不证明物理多设备、7/30 天运行、正式参考模型或完整 S1–S5。协议范围见[独立 E1 交付](../../development/canonical-native-e1-delivery.md)。
