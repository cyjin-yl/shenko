# 通路观察：`net.probe` 的测量边界

**内核定位。** 参考插件：一个感知单元及其执行，建立在[内核合同](kernel.md)之上。它以单元的形式安装（[内核 8](kernel.md#kernel-8)），经[内核 7](kernel.md#kernel-7)登记，只提供带来源和时间的观察，不替主体作判断（[内核 9](kernel.md#kernel-9)）。

本文对应 [§9.4](network-observation.md#spec-9-4)、[§18.7](cognition-units.md#spec-18-7) 与 [§45](cognition-units.md#spec-45) 的觉察要求：**自己此刻是否还能到达某个端点，只能由一次真实观察回答**，印象、上一次的成功或对目录条目的假设都不能替代它。

## 存在的理由

"刚才还能上网" 是一段历史，不是现在的通路状态。症状与假设冲突时（页面打不开、某个 API 突然失败、升级前的连通性检查），正确的第一步是测量，而不是解释。此前产品没有任何专门的通路观察入口，模型只能凭记忆断言，或在没有依据时编造"我检查了一下"。

`net.probe`（native 定义版本 1）补上这个缺口，并且只补这一件事。

## 输入与边界

```json
{"url": "http://host:port/path", "timeout_ms": 2000}
```

- 只接受 `http` 与 `https`；URL ≤ 1024 字节；`timeout_ms` 取值 100–5000，缺省 2000。
- 拒绝 URL 里的凭据、query、fragment 与端口 0：观察者不携带身份，也不创造新的请求语义。
- 冻结的是 URL 本身（`FrozenProbe`），后续每一步都与这串字节比对，探测不能在中途换目标。

## 一次观察做了什么

1. **DNS**：解析主机，最多保留 4 个地址。解析在独立线程上进行，等待受 `timeout_ms` 约束——系统的解析器本身没有超时，黑洞 DNS 不能让这次观察无限延长；超时记为 `dns_timeout`，与 `dns_failed` 区分。
2. **TCP**：按解析顺序逐个 `connect_timeout`；全部失败则记为 `connect_failed`，并保留最后一个错误。
3. **HTTP 状态行**（仅 `http`）：发送一个 `GET` 后只读第一行状态码，最多 512 字节，随后关闭连接。不读响应体、不跟随重定向、不带 Cookie、不发第二个请求。`https` 只做到 TCP 连接——本观察者不做 TLS 握手，也不假装验证过证书。

## 输出

```json
{
  "reachable": true,
  "dns": {"ok": true, "elapsed_ms": 1, "detail": "10.10.10.11:8000"},
  "connect": {"ok": true, "elapsed_ms": 2, "detail": "10.10.10.11:8000"},
  "http": {"status": 200, "elapsed_ms": 3, "detail": "HTTP/1.1 200 OK"},
  "elapsed_ms": 6,
  "error_kind": null
}
```

- `reachable` 是"连接建立成功"这一事实，不是"服务健康"。
- 没走到的阶段保持缺失或 `ok:false`，不补零、不猜测。
- **负结果也是一次成功的观察**：child 的操作结果是 `Succeeded`，`reachable:false` 与 `error_kind` 才是被测到的事实。调用方不应把"不可达"当成 child 失败。
- `scope` 明示：这是此刻、从这个节点的一次尝试，不能推出前后连续性。

## 生命周期

- T1/E1 与其它 native child 相同：冻结输入封入 CAS，一次 armed attempt，一个 observation，一个 evidence receipt；reconcile 在同一 attempt 下重新测量，不重放旧结论。
- 观察只写本安装的 CAS 与账本，无外部副作用；`EffectStatus::None`。
- 卸载定义后已测得的观察仍可读；升级不重发在途探测。

## 验证

`crates/shenko-exec-host/src/net_probe.rs` 内联测试跑真实 socket：

- 真实 HTTP 应答 → `reachable:true` 且状态码为实际返回的 204；
- 绑过又释放的端口 → `connect_failed`、`http` 为 `None`；
- `.invalid` 主机 → `dns_failed`，与连接失败分开；
- URL 解析边界（协议、凭据、query/fragment、端口 0、超时范围）。

这些证明的是观察路径本身；"模型在被问到通路时是否真的去观察"属于 awareness 效能测量，不在本文范围内。

<a id="spec-9-4"></a>
<!-- migrated-contract: SPEC:9.4 -->
## 9.4 自主更新策略与联网配置

自动更新策略本身属于 Agent 可自主决定的运行状态。主体可以选择启用或停用自动更新、立即更新、延后、跳过某个版本、限定更新对象、调整检查与部署时机，以及在失败后回滚或再次尝试。发现新版本不等于必须安装；“自己决定是否更新”和“无需逐次询问用户”同时成立，涵盖插件、包、harness、kernel 与救援程序。开发任务核验最新 Proto-UI 主线的要求，与运行中的 Agent 是否立即部署新版本分别管理。

UpdatePolicy 与 NetworkPolicy 均为可替换插件。版本发布、依赖不兼容、配置选择、连接异常和代理可用性变化都可产生结构化事件，例如 update.available、configuration.question、network.health.changed，并通过持久收件箱进入同一主体序列。事件携带来源、当前配置 revision、受影响组件、可用选项或能力、相关证据和处理状态；软件弹出的配置问题可由 Agent 回答，不默认转成等待人类的确认框。没有外部消息时，主体也可自行发起检查和改进。

U57 要求对自身各已授权环境主动发现漏洞、损坏和过时依赖，选择修复或更新并委派执行，必要时自主开发缺少的检测/修复能力。漏洞来源、受影响实际版本、修复动作、采用结果和剩余风险分别保留；检查未执行不能报环境健康。目标设备的权限与恢复闭包按事实使用，正常维护无新增人类 gate；失去模型后的日志、导出和已有确定性恢复不依赖主体重新推理，见 [§22.2](nodes.md#spec-22-2)。

主体可以自主决定直连或代理、选用哪个已可用代理、按 provider/目标/runtime 采用何种路由，以及何时切换。路由合同声明适用协议、目标解析位置和现有连接如何处理，不能只设置环境变量便假定 HTTP、WebSocket、SSH 与 WASM socket 全部已经采用。它通过语义决定与 handoff 让子 Agent 探测、生成候选配置并执行；主体仍无直接网络、文件写入或 shell 工具。现有管理员登记、外部账户权限与用户明确设定的资源/网络约束保持有效，但不额外添加“代理变更必须人工批准”的产品门槛。

配置变更记录原因、意图、旧/新 revision、实际采用配置的消费者、观测与所选策略。主体或受委派任务可使用 awareness 能力识别相关情境，再选择直接应用、候选试验、预探测、事后观察、修复或恢复等执行能力；不存在所有修改统一遵循的先验证后应用门槛。界面分别显示 desired、实际采用 revision 和健康证据。代理切换中断连接不证明旧推理请求已经终止；同一后端经直连、代理或隧道仍属同一资源组，并发、未知请求和 KV 继续按 [§13](inference.md#spec-13) 记账。代理 URL 改变不产生额外许可。凭据使用已有 secret handle。

默认发行可提供网络/配置相关 awareness 能力，帮助主体识别可能切断自身控制链的修改；当前策略通常会复用或物化不依赖在线模型的恢复程序。识别、决策、配置执行、探测与恢复由各自能力承担，不要求一个插件全包。主体可选择恢复旧配置、继续修复、前进迁移或暂时保留异常。没有旧配置的首次接入失败，返回仍可用的 bootstrap 入口，不虚构 known-good 模型。恢复程序的可用性及覆盖范围按真实工件和控制路径记录；跳过或替换策略不能冒称已具备恢复保证。通知与请求批准分开；正常自维护无需人类逐次确认。详见 [§43](configuration-playground.md#spec-43)–[§45](cognition-units.md#spec-45)。

<!-- /migrated-contract: SPEC:9.4 -->

<a id="spec-50-3"></a>
<!-- migrated-contract: SPEC:50.3 -->
## 50.3 通路观察的测量入口

本节记录 2026-11-04 实现的 `net.probe`（native 定义版本 1），作为 [§9.4](network-observation.md#spec-9-4)、[§18.7](cognition-units.md#spec-18-7) 的补充事实，不修改 [§1](architecture.md#spec-1)–[§49](ecosystem-adapters.md#spec-49) 原文：

- 输入 `{url,timeout_ms?}`：仅 `http`/`https`，URL ≤ 1024 字节，超时 100–5000 ms；拒绝 URL 内的凭据、query、fragment 与端口 0。冻结的是 URL 字节本身，探测不能中途换目标。
- 一次观察依次做 DNS（独立线程等待，受同一预算约束，超时记 `dns_timeout` 而非 `dns_failed`）、逐地址 TCP `connect_timeout`，`http` 再发一个 `GET` 只读第一行状态码（≤512 字节）；不读响应体、不跟随重定向、不带 Cookie、不发第二个请求；`https` 只到 TCP 连接，不假装完成 TLS 验证。
- 输出 `reachable`（连接是否建立）、各阶段耗时与 `detail`、`error_kind`、`scope`。没走到的阶段保持缺失或 `ok:false`，不补零。负结果是成功的观察：child 的操作结果为 `Succeeded`，`reachable:false` 是被测到的事实，调用方不得把它当成 child 失败。
- 生命周期沿用 native child：冻结输入 CAS、一次 armed attempt、一个 observation、一个 evidence receipt；reconcile 在同一 attempt 下重新测量；`EffectStatus::None`；卸载后已测观察仍可读。
- 观察路径由真实 socket 的内联测试覆盖（真实 HTTP 应答的状态码、释放端口的 `connect_failed`、`.invalid` 的 `dns_failed`、URL 边界）。模型是否在被问到通路时真的去观察，属于 awareness 效能测量，不由本定义证明。

<!-- /migrated-contract: SPEC:50.3 -->
