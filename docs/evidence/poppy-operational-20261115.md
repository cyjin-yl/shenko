# Poppy 账号、签名传输与 UCP 连续性验证记录

日期：2026-11-15（UTC）。跟踪 [Issue #297](https://github.com/cyjin-yl/shenko/issues/297)，
交付 [PR #302](https://github.com/cyjin-yl/shenko/pull/302)，依赖
[PR #298](https://github.com/cyjin-yl/shenko/pull/298) 的发现与签名基础。

## 候选、基线与范围

- 初始功能候选：`7c97182e64dca32c57c3ac66dede47dc04d5825a`。
- 该候选对应发布提交：`f5b70b2ca97cfc4403935b07f398acc0e6415304`；源码树逐项校验为
  `e377168485e822d3ba73afa7ce46fbc5532a5ebb`。发布保留各来源提交、父提交关系及来源标记。
- CI 收尾源码提交：`0fa6fb938302a17f120b7c503137c449bb3973a5`，对应发布提交
  `38f9bd64c97a364f4fe2adc06a819926e74e5ff9`、源码树
  `3afffd567bf22da42becc4683f5acff6c3d79ff7`。下文说明首轮 CI 发现及其修复。
- 原始独立基线：`054817b0d5729fed56ec5a974d101c31edfd1410`。
- PR302 的分层基线：`2e20dfc52b14e9c8dcce3b51fd8e5247ceaa1eb3`（PR298）；
  PR298 自身的六项 CI 已通过，未用本候选抬高已有告警或结构基线。
- 环境：Linux、Rust 1.98.1；`CARGO_BUILD_JOBS=2`、`CARGO_INCREMENTAL=0`，dev/test debug 信息关闭。
- 相对上述 CI 收尾源码，最后的交付只补充证据文档。当前产品合同见
  [Poppy：身份、账号与商务接入](../specs/ecosystem-adapters.md#poppy-foundation)。

本增量新增恰好两个测试函数；连同[首个增量](poppy-foundation-20261115.md)的七个场景，
此轮功能共九个。额外运行既有 MCP 回归及仓库检查，没有新增第十个独立场景。
历史 foundation 记录只证明它标明的旧源码树，其“未实现”描述不代表本候选。

## 已接通的执行路径

Poppy 协议 crate 保持独立，节点安装、凭据、原生调用和业务回执由装配根拥有。
当前提供 discover、account、request、commerce 四个原生接口，覆盖：

- 本机加密 vault、持久 pairwise 身份、公开 CIMD/JWKS/UCP profile 导出与读取校验。
- 实际 HTTP/MCP 派发时的 Poppy Session、RFC 9421 HTTP Message Signature、DPoP、
  UCP profile、幂等键、追踪标识与正文摘要；资源专用 token 必须匹配实际服务 metadata。
- PKCE/PAR、Device、私有回调引用、刷新、撤销，以及受原生操作和浏览器 lease 双重约束的
  browser join。未知兑换、刷新和撤销保留原 flight，不重发已派发的账号请求。
- 按实际商家工具/OpenAPI schema 协商的 UCP 请求；checkout 完成必须绑定原观察、条款、
  公司、用户、操作者、grant、支付 handler 和节点凭据。
- 超时/失联后的原资源读取，以及从不可变恢复观察向原效果账本追加后置条件证据。
  已 armed 的修改不重发；原始尝试、幂等键和 unknown 回执不被替换。
- 任务取消、安装撤回或冻结连接变化时的派发前校验，及 MCP transport/SSE 的协作关闭。
  取消不会被报告为远端效果已停止。

这些是源码接线结论。以下分别列出实际测试与尚未取得的外部证据。

## 本地检查

命令在仓库根执行。构建共享同一 target；下面的 `--offline` 只使用已取得并由 lockfile
固定的依赖，不改变程序自身的网络策略。

| 检查 | 实际结果与边界 |
| --- | --- |
| `cargo test -p shenko --features compat-poppy --lib poppy:: --locked --offline -j2` | CI 收尾源码再次 3 passed：两个新增连续旅程和既有安装启停旅程。 |
| `cargo test -p shenko-compat-poppy --locked --offline -j2` | 初始功能候选 6 passed，覆盖四个发现与两个签名场景；doctest 0 个。此协议 crate 在 CI 收尾期间没有代码变化。 |
| `cargo test -p shenko-compat-mcp --tests --locked --offline -j2` | CI 收尾源码再次 20 passed：learning_efficacy 1 个、plugin_lifecycle 19 个；包含真实 stdio、JSON/SSE 边界、取消关闭、安装启停和原 unknown 回执接续。 |
| `cargo clippy -p shenko -p shenko-compat-mcp -p shenko-compat-poppy --features shenko/compat-poppy --all-targets --message-format=json --locked --offline -j2 -- --force-warn clippy::all --force-warn clippy::unwrap_used --force-warn clippy::expect_used` | 初始功能候选退出 0，全 targets 编译通过。该命令没有逐项 force Rust 编译器告警；后续补用完整 CI policy 复验，见下节。 |
| `cargo build -p shenko --bin shenko --features compat-poppy --locked --offline -j2` | 初始功能候选退出 0；随后实际执行 `shenko compat poppy --help` 和 `shenko compat poppy credential --help`，十个业务子命令及 guest credential 选项可见。 |
| `python3 scripts/check_docs.py` 及 `--verify-migration` | 通过，errors 为空；冻结迁移块未修改。使用仓库声明的 parser 依赖。 |
| `python3 scripts/check_kernel_boundary.py --baseline-ref 054817b0d5729fed56ec5a974d101c31edfd1410` | 通过，new_violations 为空、baseline_updated 为 false。首次收尾调用缺少 Cargo PATH，补回真实工具链路径后执行成功。 |
| `git diff --check` | 通过。 |

完整 workspace 的 Rust 检查/测试、web、docs、docs-migration、kernel-boundary 和
clippy-ratchet 六项远端门禁以 [PR302 的检查](https://github.com/cyjin-yl/shenko/pull/302/checks)
所绑定的实际提交为准。此记录不把本地所选包检查称为完整 workspace CI。

### 首轮 CI 发现与收尾

[首轮运行](https://github.com/cyjin-yl/shenko/actions/runs/43736781546)的 rust、web、docs、
docs-migration 和 kernel-boundary 均通过。clippy-ratchet 失败：两处新 MCP `select!`
绑定在宏展开中触发 `unused_variables`，一处浏览器 `std::pin::pin!` 内部展开触发
`unreachable_code`。前者改用实际仍被返回的 `_result` 绑定；后者改为 `Box::pin`，保留
future 固定、逐 poll guard、await 与取消语义，仅增加一次局部堆分配。

首轮 Clippy 只构建了 workspace 的默认 feature，未覆盖 bin 中受 feature 保护的 Poppy
宿主代码。已使基线和候选两个 scope 都明确启用 `shenko/compat-poppy`，与 Rust job 对齐。
原始基线、135 项可用 Rust 强制告警、三项 Clippy 选择及 source-policy 比较均保留。

本地复用首轮 artifact 中 `clippy/lint-policy.json` 的完整 `flags`，对 `shenko`、
`shenko-exec-host`、`shenko-compat-mcp` 和 `shenko-compat-poppy` 执行带 Poppy feature 的
`--all-targets` Clippy。由此又暴露出 Poppy 商务/派发中四处同类 `select!` 宏绑定，已保留
每个返回值和取消分支作相同修复。最终完整 policy 检查退出 0，上述新增/修改来源文件
没有诊断；既有其他文件的告警仍由独立基线比较。没有添加 `allow`、`expect` 或忽略测试，
没有调整基线以容纳新告警。最终远端结果须读取 PR 当前 head 的实际检查。

首次组合旅程暴露了测试 fixture 未启用依赖插件，已让 fixture 通过真实 Node 命令明确启用；
没有放宽生产安装检查。收尾还修正了新增的 lint 分支及 vendored resolver 的三处同类诊断。
本地构建曾遇到磁盘缓存占满和编译锁等待；仅清理自身可重建缓存、停止未完成的等待并重跑，
不将这些中断算作通过，也未改变门禁、测试忽略项或基线。

最终本地输出的 SHA-256 摘要如下。大日志留在执行环境，远端可审查证据由对应 CI job 与
artifact 提供；这些摘要本身不替代检查输出。

| 输出 | SHA-256 |
| --- | --- |
| CI 收尾宿主三个测试 | `45b7cd21c81151643ebd7d78b7eb0f32ba119cf185550c34e34b0b41fc9b52de` |
| 协议六个测试 | `76b1bf0c2f219d4c02dd32b7a31d5fd6186106fc21b84e5eea6d4c460e4b5fa6` |
| CI 收尾既有 MCP 20 个回归 | `be492a9c110196936ca92f04d54b4d360a3b7624687ac89e1ec810c3758e43c0` |
| 所选包 Clippy JSONL | `a5acfd00d15967663487fa19108dc4f20a417ff122d110a1689ad953cf47b49f` |
| CI 收尾完整 force-warn policy Clippy JSONL | `5a033b35f7f6219c0edea73186f85a8eac084cca0898410f3bb1befdfa76e401` |
| CLI 构建 | `f13fb63dfcd58cbd165968a2bcec9833f888bf8551e902e146d757e67d416f63` |
| 实际 CLI 帮助 | `29af4ef3670bd95983eba9c4496768d3ea496e805cfe0c4c859e4298eb36df1f` |
| 文档完整性及迁移（相同输出） | `ce5b19c30571ed129aa5a4d0e457be6a6bc346596bb1f67749a9d278d4776fed` |
| 内核边界 | `00604c55fd18e6b4b0a67008040ce104e98e844de9b9b7cc503a6c63bf20310c` |

## 两个新增连续旅程

### 账号未知结果与重开

[`account_continuity_preserves_unknown_refresh_and_revoke_across_reopen`](../../bin/shenko/src/poppy/account/tests.rs)
执行真实 vault、PKCE/client assertion/DPoP 构造、grant 与原 flight 持久化，再重开存储，
检查未知刷新、替换授权、归档及撤销的连续性。公开 snapshot 不含秘密。
商家响应由明确 fixture 构造；该测试没有执行真实用户同意页面或在线 token exchange。

### checkout 未知结果与不可变恢复

[`checkout_unknown_is_repaired_from_immutable_observation_without_replay`](../../bin/shenko/src/poppy/operational/tests.rs)
使用真实 Node 安装与精确启用状态、Store、CAS、RecordedCall、T1/E1 和 checkout 作用域
凭据，检查以下同一条旅程中的连续行为：

1. 用实际捕获的 `get_checkout` / `complete_checkout` 输入输出 schema 验证 fixture。
2. 由真实 RequestPlan/RequestSigner 构造最终请求字节，独立重建 RFC 9421 签名基串并以
   公共 JWKS 验证签名，重新计算正文 Content-Digest，核对身份、UCP 和原幂等/追踪头。
3. 保存原 completion 的 unknown 和后续 recovery E1，关闭/重开 Store，再从同一不可变
   读取观察修复原账本；重复本地 fold 不创建第二次业务尝试、不重发完成购买请求。
4. 核对原始输入、attempt、幂等键、checkout、订单与条款绑定，以及所检查的任务/CAS
   中没有节点 instrument 秘密。

此旅程没有网络请求，也没有调用 `commerce::execute` 到真实 RMCP initialize/tools/call
的整条路径。checkout、completed、order ID 与初始 unknown 都是 fixture。签名验证覆盖
这一个未登录、无 query 的 MCP 请求，不据此扩展为所有 OAuth、REST 或 query 路径的商家
互操作结论；重开 Store 也不是一次真实成交后强制终止进程的实验。

## 真实公开取样

2026-11-15 实际读取了 Allbirds 的公开 Poppy、OAuth resource/issuer 和 UCP metadata，
并取得匿名 MCP `tools/list`。这些读取通过工作环境的 HTTPS 代理完成，无商家账号、
购物车、付款或订单副作用。四份 metadata 的范围、时间和原始摘要见历史 foundation 记录。

匿名工具目录响应共 223502 字节，原始字节 SHA-256：
`76affd926e9288bcf7f7f94e319963f35465c5d9d6fbbdefcca1550841438751`。
checkout 旅程使用的[工具 schema fixture](../../bin/shenko/src/poppy/operational/fixtures/checkout-tool-schemas.json)
保留其中两项工具的实际输入/输出 JSON 子树，省略工具层 description。
捕获的工具目录不证明 Shenko 的公开身份、签名、账号权限或付款工具已被商家接受。

生产客户端禁用环境代理，连接前验证并固定 DNS 地址。本轮该执行环境仍不能直接解析
`www.allbirds.com`（`Temporary failure in name resolution`）；未取得生产 Rust 客户端经
DNS/TLS/HTTP 到真实商家的成功证据。没有为通过检查放宽代理、DNS、redirect 或凭据边界。

## 外部验收缺口与已知限制

| 项目 | 本轮实际状态与完成所需证据 |
| --- | --- |
| 公开操作者身份 | `init` / `identity` 只生成本地文档，返回 `public_urls_verified:false`。仍须实际托管 CIMD/JWKS/UCP profile 与回调接收服务，再证明商家能够读取匹配的公开文档。 |
| 商家账号 | 未提供真实授权/资源 grant，没有进行商家登录或用户同意。Direct/Device 启动结果不等于已授权；需要真实 token response、scope/resource 与账号行为证据。 |
| 浏览器加入 | 未做真实商家浏览器验收。204、表单 ACK 或导航不证明 cookie 的账号身份；代码保留 `merchant_session_cookie_verified:false` 和 `application_effect:unverified`。signed_out join 也不证明浏览器原有登录被清除。 |
| 支付与订单 | 默认平台 profile 没有支付 handler；没有真实提供方 instrument、明确交易参数或购买授权。本轮没有创建真实订单或付款。completed/order 观察仍不等于支付结算或履约独立验收。 |
| 模型使用能力 | 既有本地模型端点网络不可达（`Network is unreachable`）；未猜测模型 ID，未宣称真实模型会选择或正确使用新增接口。 |
| 节点迁移 | 业务事实与原回执可以保留，节点私钥和凭据不会随普通 Node/CAS 导出。另一节点没有本地秘密时必须重新具备相应授权，不能静默替换身份继续结算旧效果。 |
| 运行平台与时限 | vault 当前以 Unix 属主权限实现，其他系统拒绝启用。Schema 有大小、节点、深度和 regex 资源上限，但同步校验不是可抢占的墙钟截止。没有长期运行或商家所有实现的互操作保证。 |

Issue #297 保留真实部署、账号、浏览器及交易恢复验收项。代码交付、fixture、CI 与公开
取样各自提供其实际范围的证据，不代替这些尚未执行的外部步骤。

## 2026-11-16 PR302 两项 P2 的源码修复交接

修复工作树以 `983cc13f318d0389410ab152545630ebc0b4d5b0` 为原始来源；本节只记录尚未提交的
源码变化，不将上文历史 CI 或运行结果移用于新代码。子席没有运行构建、测试、lint、formatter、
模型或网络请求；由主席在既有 CT102 开发容器中验证同一持久源并整合 PR298/current main。

第一项修复把原始 UCP transport、service endpoint 与 OAuth resource 加入冻结 owner、
checkout 观察和 schema-2 instrument 记录，原请求、完成、凭据加载及恢复均作严格核对。
Session 可在服务/resource/grant 不变时更新，另一个服务中的同 ID/条款不能结算原请求。
缺失绑定的旧数据原样保留，拒绝推测历史身份。
commerce 的 implementation contract digest 随上述新合同更新；account/request 的该身份不变。

第二项修复复用 vault、RecordedCall、真实 CompanyAuthorizer 和 MCP HTTP 请求 API。
MCP 的新 `before_send` 回调位于实际 SDK 正文及授权不变检查之后；REST 进入同一回调。
最终请求的确切正文、SDK RPC ID、Request-Id、授权头、原幂等键、完成绑定和 instrument
摘要进入 AES-GCM 保护的不可变 `commerce-request-<attempt UUID>` 记录。普通 CAS/evidence
只保留公开引用与摘要；原 attempt 的 final receipt 不因封存事实而变成远端完成。
instrument 引用不可覆盖/移除，T1 记录其完整私有内容摘要；所有未知结果仍禁止自动重发。

只扩充两个既有消费者，没有新增测试函数：

- checkout 连续旅程增加同 issuer/user/grant、同资源 ID/条款但 endpoint 改变时的加载、支付
  绑定与恢复拒绝；加入 Session 更新的正面控制；原完成丢失回复后尝试改写 instrument、
  另行导入 instrument，再关闭/重开并核对原最终请求、RPC ID、Request-Id 与原始正文字节。
  商家响应仍是明确 fixture，没有真实购买或生产 `commerce::execute` 网络旅程。
- 既有 MCP HTTP 消费者在 SDK 的真实回调中同步每个 tools/call 的原始正文，独立 TCP fixture
  在收到请求时先核对同一 RPC ID 的已落盘字节，再处理请求；原 JSON/SSE 边界与关闭断言保留。

主席的精确原生选择器如下，均需在项目已授权的开发容器中执行；这里没有执行结果：

```sh
cargo test -p shenko --features compat-poppy --lib --locked --offline -j2 poppy::operational::tests::checkout_unknown_is_repaired_from_immutable_observation_without_replay -- --exact
cargo test -p shenko-compat-mcp --test plugin_lifecycle --locked --offline -j2 streamable_http_transport_bounds_json_and_sse -- --exact
```

这些选择器分别检验本机封存/恢复与真实 SDK/TCP 发送顺序，不替代商家登录、实际支付、物理
断电、多节点凭据迁移或模型行为验收。主席取得新运行结果后须单独绑定整合后的源码与输入。

### 同日：最终请求证据的接纳坐标

合并候选 `2b26a8bf62a34cf71e64f7544cbea259ca2acd83` 的独立审阅确认前两项 P2 已在源码中
关闭，同时发现最终请求事实把 T1 的 `prepared_commit` 当作自己的 `received_commit`。
该事实实际通过后来的 Store evidence 批次接纳，故不能把其接纳时间回填到 T1。

本次有界修复保留既有 `Store::commit` 非终态 evidence 路径，在唯一 Ledger append 事务中，
先作 receipt admission，只有 New 才把 `received_commit` 赋为该事务实际分配的 `commit_ref`，
随后序列化并插入 receipt；operation evidence cursor 使用同一坐标。Replay 仍先返回既有
身份，不改写首次接纳坐标。Poppy 调用方只提供未赋值占位，不读取或预测 ledger head/sequence。
没有新增账本权威，也未借用会结算 attempt 的外部响应 API。

既有 checkout 消费者在 Unknown 响应之前取得已持久化的 evidence cursor，断言它晚于 T1、
与请求事实的 `received_commit` 一致；再以不同占位坐标重接纳同一事实，要求保留原 receipt、
原 cursor、Armed 状态和空 final receipt。关闭/重开后仍要求该请求事实逐字段不变，坐标等于
先前真实 append cursor，且不等于 T1。原 service、credential、最终正文认证及禁止重发断言
全部保留，没有新增测试函数。

此坐标修复尚未运行任何构建、测试、lint 或 formatter；由主席使用上述 checkout 精确选择器
验证同一持久源码。此前候选和历史 CI 不作为本次修改的通过证据。
