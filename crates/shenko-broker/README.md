# shenko-broker

## 真实保证与配置

`Broker::new()` 与 `BrokerHandle::submit()` 保留原接口。`new()` 从
`SHENKO_INFERENCE_CONCURRENCY` 读取正整数并发上限，未配置或配置非法时保守使用
**C=1**（非法配置有告警）。程序也可使用
`Broker::with_config(BrokerConfig::new(2)?)` 明确选择 C>1；
`BrokerConfig::from_env()` 可让需要严格配置的调用者直接接收错误。
`AdmissionController::nominal()` 的四流常量仅保留兼容，不再作为 broker 默认值。
配置上限不是实际后端容量探测结果，更不是已验证四流 128K。

Why：原先在一个循环内 await provider，四份名义许可实际只运行一次调用；
绝对优先级又会让持续申请的主体饿死低优先级 child。现在每个调用由独立异步任务
执行，许可在派发前取得，在确认完整返回后释放。暂时无容量的请求留在队列，
不会以最终 `QueuedCapacityWait` 结果丢弃。输入、完成回收、派发每批最多处理
64 项，完成分支先被检查；不会无限 drain 输入。

调度仅比较四级 FIFO 队首。每等待 `aging_rounds` 次实际派发，队首有效优先级提升
一级，上限为 Urgent；同有效级按较早入队序决定。默认 `aging_rounds=4`，
一个 Low 请求最多经历 12 次其他派发即具有最高有效级；此后新来的高优先级不能
仅凭优先级插队。此前的请求数量有限时它可在有限轮次内获准；这个界限是派发轮次，
不是远端请求耗时保证。同原始优先级严格 FIFO。

无真正 Urgent 队首当选且两侧均 ready 时，同侧不连续服务超过两个 generation。
目前请求类型没有独立主体/child 角色，**Subject 优先级视作主体，其他级视作 child**。
这是当前接口下的轮转约定，不是完整的任务 lineage 公平份额实现。紧急请求仍有
响应优势；aging 防止持续紧急流永久阻止早先的普通请求。后端请求内抢占、DND 与
attention-starvation 建议不在本增量中。

生产 provider 选择仍为显式本地模型开关、其次 Anthropic 配置。二者都未配置时
请求返回 `Failed` 且无内容，不再返回伪造的 stub 成功。fake 只在 `cfg(test)` 的
内部可注入 provider 中使用，不是生产开关。无配置状态可通过诊断明确区分。

## 终止未知不是失败或取消确认

`BrokerHandle::snapshot()` 无需 LLM，即可读取配置上限、`provider_configured`、
held 许可数和每个已派发请求的 `Active` / `TerminationUnknown` 状态。
`provider_configured` 仅说明选择了真实绑定，不证明鉴权、网络或后端可用。
诊断不包含 prompt、凭据、endpoint 或远端错误正文。

现有 HTTP adapter 返回的 `anyhow` 错误不足以证明请求从未执行或已经停止。
因此**所有尚无静止证明的 adapter 错误（包括超时、断连、解析失败和未细分的 HTTP
错误）保守记为 TerminationUnknown，继续占用许可**。这有意可能降低可用性，
而不把未知请求算成空闲并无限重发。完整成功返回和未配置而从未外发的请求可释放。

现有 `InferenceStatus` 无未知状态且调用者会据终态继续下一步；为不破坏其 exhaustive
match，本增量不增加枚举成员。unknown 保留结果 oneshot，不返回虚假的 Failed、
Cancelled 或成功。即使调用者丢弃结果 receiver，也不停止远端或释放许可。
armed 后任务 panic / broker future 被本地 abort 也不释放 held 计数，诊断转为 unknown。
本地释放 guard 不是远端取消证明。

当前没有远端 request handle、查询/取消确认协议或人工强制释放接口；unknown 会停留，
同域受影响容量不可再利用。后续 adapter 对账需绑定原 request/attempt 与 engine
incarnation，取得匹配的终态或真实引擎销毁证据后，才可实现回收。现阶段恢复受影响
域需要在仓库之外的受管恢复路径确认旧引擎/物理执行已停止，再重建 broker；
**仅重启 broker、切 URL、等待超时或改请求 ID 都不构成该证明**。

## 仍未完成的合同

- 许可与诊断只在当前进程内；重启、升级 A/B 交接、跨 broker/节点的持久
  ComputeAllocation、attempt、QuiescenceProof、去重与 late receipt 回收尚未实现。
  新 broker 不能自动知道旧远端占用。不得让旧/新实例各自获得满额配置。
- 硬上限仅覆盖同一 broker 实例的受控请求。不同 URL/model alias 若共用 engine
  必须复用同一资源入口；外部 coding seat、直接调用 provider 的客户端及其他应用
  尚不能因此被宣称已纳管。当前无物理资源域发现或强制代理。
- 不实现 KV/StateImage 驻留、迁移、半池自动分配或原生 NInfer/Strata/vLLM/1Cat
  能力协商；上下文输入/token 预算校验、sealed wire profile 与 provider 原生完成
  证据仍须按其各自合同接线。计算许可不是这些合同的替代品。
- 公平性依赖请求有限结束且槽位可用；全部槽位 unknown、永久不结束的远端调用
  或错误地把 child 标成 Subject 时，不宣称墙钟活性。当前 unbounded 入队接口仍
  需要后续背压与持久队列。

本增量依据 SPEC §§13.1–13.8、35.9、41.8–41.10、47，改善 R027/R028/R038/
R047/R048/R064 的部分实现；A03、V1-11、RT03、RT05、RT08、RT12 等完整验收仍有
上述未完成范围，不标记 satisfied。两个新增隔离测试覆盖实际 C=2 并发/容量保留与
C=1 持续主体/child FIFO 公平推进；公平断言使用明确握手、入队序和派发轮次，
测试超时只用于卡住时退出，不充当公平阈值。未调用真实付费或私有模型服务。

```sh
CARGO_BUILD_JOBS=2 cargo test -p shenko-broker -- --test-threads=1
```
