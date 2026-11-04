# 通路观察：`net.probe` 的测量边界

本文对应 SPEC §9.4、§18.7 与 §45 的觉察要求：**自己此刻是否还能到达某个端点，只能由一次真实观察回答**，印象、上一次的成功或对目录条目的假设都不能替代它。

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
