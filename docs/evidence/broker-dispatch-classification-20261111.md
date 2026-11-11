# Issue #274 — 派发前本地失败不得记为 Unknown 并永久占用推理槽

基线：`origin/main` `375238a`。修改文件：`crates/shenko-broker/src/execution.rs`（仅此一个）。

## 静态证据（修改前的缺陷位置）

- `crates/shenko-broker/src/execution.rs` `execute_admitted`：`may_dispatch()==Send` 后，
  `registry.runtime_for(&request.provider_binding)` 返回 `None`（provider runtime 已被卸载或
  从未安装），原实现用普通 `anyhow!("no installed provider runtime can serve this binding")`
  交给 `failure_result`。这是任何 provider 调用**之前**的本地失败，但 `failure_result`
  只对 `InvalidRequest` 映射 `never_dispatched`，其余一律 `TerminationEvidence::unknown()`。
  同一函数里 `!runtime.available()` 分支（紧邻其下）却正确地直接构造
  `Unavailable + TerminationEvidence::never_dispatched()`。两个同样"没发出去"的分支分类不一致。
- 同一文件 `execute_stream`：本地 `publisher.open(&request.provider_binding, now_ms)` 失败
  （durable stream journal 打不开）也走普通 `failure_result` → `unknown()`。同样发生在
  `runtime.stream()` 之前。
- `crates/shenko-broker/src/broker.rs` `Broker::finish`：`TerminationKind::Unknown` 把
  admission guard 放回 `accounting` 并把请求隔离为 `UnknownDebt`，额度**不释放**；
  `NeverDispatched` 才 `release_never_dispatched()`。所以上面两处把"确定没发出去"记成
  Unknown 后，C=1 场景下该槽被永久占住：后续请求在 FairQueue 里永远等不到 admit，
  只能靠对账（reconcile_recorded）拿原始终止证据才释放——而"没发出去"根本不会产生
  远端终止证据，实际上等于永久泄漏。

## 修法

在 `execution.rs` 内新增 `pub(crate) enum LocalDispatchError { NoProviderRuntime,
JournalOpenFailed(StreamSinkError) }`，实现 `Display`/`std::error::Error`（`JournalOpenFailed`
以原 `StreamSinkError` 为 `source`）。两个派发前失败点改用这个类型构造错误：

- `execute_admitted` 缺 runtime：`anyhow::Error::new(LocalDispatchError::NoProviderRuntime)`，
  `INFERENCE_PROVIDER_UNAVAILABLE` 的 error_sink 记录保留不变。
- `execute_stream` journal 打开失败：`anyhow::Error::new(LocalDispatchError::JournalOpenFailed(error))`。

`failure_status` 在 `ProviderRejection`/`ProviderWireError` 之后、默认 `Failed` 之前
downcast `LocalDispatchError`：`NoProviderRuntime → Unavailable`（与 `!available()` 分支一致，
下游 subject 把 Unavailable+NeverDispatched 归为 MODEL_PROVIDER_UNAVAILABLE）；
`JournalOpenFailed → Failed`（provider 可达，是本地持久化失败，如实报 Failed）。
`failure_result` 在 `InvalidRequest → never_dispatched` 之后、兜底 `unknown()` 之前加一支：
`_ if error.downcast_ref::<LocalDispatchError>().is_some() → never_dispatched()`。

未改动的对照语义：`ProviderRejection → TransportRejected`（HTTP 拒绝）、`runtime.complete()`/
`runtime.stream()` 发出后返回的普通 Err → 仍走 `_ => unknown()`、`DispatchFence::Unknown`/
fence 检查失败 → 仍 `unknown()`（可能已发出）。`finish` 的 `record` 失败仍保留 unknown
charge（回执落盘失败不被本修复掩盖）；`publish_terminal`/`reconcile_recorded` 的合同不动。
没有新增第二种失败分类体系：`LocalDispatchError` 只是把"派发边界确定没发出去"这个事实
带进既有的 `failure_status`/`failure_result` 管线。

## 复现与判定（容器内真实执行）

命令（CT102 容器 `shenko-dev-persistent-20261026`，`CARGO_TARGET_DIR=/var/idw-target`）：

```
cd /tank/shared/projects/shenko/shenko/worktrees/wt-broker-dispatch
cargo test --offline --locked -p shenko-broker --lib -j4
```

结果：`test result: ok. 10 passed; 0 failed`。新增 4 个机制测试（`execution::tests`），
全部 `--lib` 内可跑：

- `failure_classification_keeps_never_dispatched_local_and_unknown_remote`（同步真值表）：
  `ProviderWireError → InvalidRequest + NeverDispatched`（不回归）；
  `ProviderRejection{429} → Rejected + TransportRejected`；发出后普通错误 → `Failed + Unknown`；
  `LocalDispatchError::NoProviderRuntime → Unavailable + NeverDispatched`；
  `JournalOpenFailed → Failed + NeverDispatched`。
- `a_missing_provider_runtime_is_never_dispatched_and_releases_its_slot`：
  真实 Broker 路径——install runtime → `submit_recorded` 入队 → uninstall → `broker.run()` 派发。
  判定：provider `complete`/`stream` 调用次数 = 0（`reached` 为空）、结果 `Unavailable +
  NeverDispatched`、receipt 持久化 `NeverDispatched`、`active_count()==0`（C=1 槽释放）、
  重装 runtime 后第二个有效请求 `Success`（complete 次数 = 1）。修改前该路径结果是
  `Failed + Unknown` 且 `active_count()==1` 永久占用。
- `a_stream_journal_open_failure_is_never_dispatched_and_releases_its_slot`：
  `submit_stream_recorded` + `FailingJournal`（`open_stream` 返回 `StreamSinkError::Journal`）。
  判定：`stream`/`complete` 调用次数 = 0、`Failed + NeverDispatched`、receipt 持久化
  `NeverDispatched`、`active_count()==0`、随后完整请求 `Success`。
- `a_dropped_connection_after_dispatch_stays_unknown_until_reconciled`（对照组）：
  发出后断连 → `Failed + Unknown`、provider 调用次数 = 1、`active_count()==1`、
  第二个请求 150ms 内不被 admit（槽被占）；`reconcile_recorded` 拿到原始
  ProviderTerminal 证据后槽释放、第二个请求 `Success`。该行为未因本次修复改变。

## 本文件不证明什么

- 不证明真实外网/真实 provider 场景下的端到端行为；provider 是计数/可控失败的
  `ProviderRuntime` fixture，机制（dispatch fence、registry runtime_for、publisher.open、
  admission guard、receipt、reconcile）全部是产品真实代码路径。
- 不证明 `broker.rs`/`resource.rs` 的其他问题（属于其他 slice）；`finish`、`reconcile_recorded`、
  `UnknownDebt` 语义均未改动。
- 不覆盖 `DispatchFence::Unknown`、fence Err、`provider.available()==false` 的新行为——
  这些路径原本就正确，本次只是未触碰。
- `--lib` 测试绿不等于整个 workspace 绿；`--test` 目标（kv_unknown_read、request_recovery_tests
  等）未在本 slice 内运行，由集成负责人在合并树上重跑。
