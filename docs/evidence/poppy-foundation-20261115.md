# Poppy 首个增量的验证记录

日期：2026-11-15。跟踪 [Issue #297](https://github.com/cyjin-yl/shenko/issues/297)。

本记录只描述下列历史源码树。后续账号、签名传输与 UCP 接线的当前状态和验证边界见
[后续增量验证记录](poppy-operational-20261115.md)；此处的“未实现”不代表后续候选的状态。

## 候选与范围

- 独立基线：`054817b0d5729fed56ec5a974d101c31edfd1410`。
- 本地源码提交：`a377ef671028e35439d853263256152660c9d033`。
- 已验证源码树：`f9078d286245abb3d44c836d09a59771e88e6835`；后续证据提交只补充文档。
- 环境：Linux、Rust 1.98.1；`CARGO_BUILD_JOBS=2`、dev/test debug 信息关闭。
- 合同：[Poppy：发现与签名基础](../specs/ecosystem-adapters.md#poppy-foundation)。

本次新增 7 个测试函数：4 个 discovery 场景、2 个签名场景和 1 个真实安装生命周期场景。
生命周期场景连续检查两种启用顺序；它没有连接商家或调用模型。另复用 1 个已有 MCP 回归。

## 实际执行

| 检查 | 结果与能证明的范围 |
| --- | --- |
| `cargo test -p shenko-compat-poppy --locked` | 6 passed。包括版本选择、issuer 混淆拒绝、私网 redirect 拒绝、Allbirds 取样回放、公开密钥验证两层签名和过期拒签。 |
| `cargo test -p shenko --features compat-poppy --lib poppy::tests:: --locked` | 1 passed。真实 Store、Node、Cordis 和 native registry；另一 Node 连接改变安装状态，验证同时启用、分别停用、重新启用和最终撤回。 |
| `cargo test -p shenko-compat-mcp --test plugin_lifecycle installed_changes_retire_connections_and_independent_plugins --locked` | 1 passed。复用既有真实 stdio/持久回执回归，确认共享 watcher 改动没有破坏独立 MCP 路径。 |
| `cargo check -p shenko --features compat-poppy --all-targets --locked` | 通过，包含默认 MCP/Skill 与 Poppy 的组合装配。 |
| `cargo check -p shenko --no-default-features --features compat-poppy --all-targets --locked` | 通过，Poppy 可单独编译。 |
| `cargo clippy -p shenko-compat-poppy --all-targets --locked -- -D warnings` | 通过。其他既有 crate 的编译警告没有在此增量中清理。 |
| `cargo run -p shenko --features compat-poppy --locked -- compat poppy --help` | 真实二进制显示 `discover` 子命令。 |
| `python3 scripts/check_docs.py` 及 `--verify-migration` | 通过；检查文档引用与冻结迁移块，不证明产品行为。使用仓库声明的 parser 依赖。 |
| `python3 scripts/check_kernel_boundary.py --baseline-ref 054817b0d5729fed56ec5a974d101c31edfd1410` | 通过，new_violations 为空，未改基线；只证明依赖结构没有新增越界。 |
| `git diff --check` | 通过。 |

组合审查曾发现 MCP/Poppy 各自登记 `compat.artifact.read` 会冲突。已改为装配根持有一个
共享 reader watcher：任一精确安装启用即保留，最后一个停用才撤回。新生命周期测试覆盖
MCP 先启用、Poppy 先启用及分别停用；没有使用“已登记就跳过”的错误拥有关系。

## 真实公开资料与传输边界

2026-11-15 11:11–11:12 UTC，经工作环境提供的 HTTPS 代理进行了四次公开 metadata GET。
四次均返回 HTTP 200、没有 redirect，也没有账号登录或商务调用：

1. `https://www.allbirds.com/.well-known/poppy.json`
2. `https://www.allbirds.com/.well-known/oauth-protected-resource`
3. `https://shopify.com/.well-known/oauth-authorization-server/authentication/11044168`
4. `https://www.allbirds.com/.well-known/ucp`

公开资料声明 Poppy `0.4`、issuer `https://shopify.com/authentication/11044168`、
UCP `2026-08-25`。[取样快照](../../crates/shenko-compat-poppy/tests/fixtures/allbirds-2026-11-15.json)
保留实际 URL、时间和原始读取正文的摘要。快照中的正文是规范化 JSON；离线 fixture 重新序列化
响应，因此其合成响应摘要不是原始线上字节摘要。测试复用生产 `discover_using`，严格只允许
上述四份资料，不跟随 UCP profile 的服务端点。

生产传输另执行了：

```sh
cargo run -p shenko-compat-poppy --locked --example discover -- https://www.allbirds.com
```

实际退出 1，错误：

```text
Error: resolving metadata host
Caused by:
    failed to lookup address information: Temporary failure in name resolution
```

当前容器可通过环境代理取资料，但本实现的独立 HTTP client 禁用环境代理，并在每次连接前
解析、校验和固定 DNS 地址。没有为通过检查取消这项边界。**未取得生产 Rust 传输到 Allbirds
的 DNS/TLS/HTTP 端到端成功证据**；代理取样及离线回放不替代它。

## 保留为未验证或后续实现

- 未运行真实本地模型的新能力选择/使用采样；native 注册通过不代表模型成效。
- 未向商家发送 Session JWT；公钥托管、持久 pairwise user ID、节点凭据与真实商家接受结果尚未接线。
- 未实现 OAuth/DPoP、浏览器 join、HTTP Message Signature、UCP profile 协商或交易/订单接续。
- 没有账号、购物车、付款或订单副作用，没有长期运行证据。
- 本地执行的是上述有界检查；完整 workspace CI 的实际状态以对应 PR 检查为准。

