# Shenko 开发交接状态

**更新时间：2026-10-25（C01 独立复核；以下明确区分当前与历史证据）**

## 当前 checkpoint：PR28 仍为 Draft，不能合入

- 执行身份 `cyjin-yl`；[#6](https://github.com/cyjin-yl/shenko/issues/6) 已 assign 同一真实账号，并有开工/接口/交付/本轮复核认领 comments。认领不形成排他锁或人工 gate。
- 复核对象是 [PR28](https://github.com/cyjin-yl/shenko/pull/28) / `fix/c01-durable-recovery-20261024` 的实际 head `ee9a3e009bec6d32437fec5cb9fbc26557d23204`，重点逐项读回 `b525f8d→ee9a3e0` 的13文件增量，而非复述 fixed_findings。
- GitHub PR 接口读回的 base SHA 为 `21b85e2cf272e0c2ce9e8e17de426696c59370cb`；本轮另外新 clone 的远端 main 为 `9a1a0a41d6d72add9fb3c5b7135f51ba7bc0c409`（PR30 已合入）。两者不是同一事实；本轮未复核 PR30，也未把 PR28 与其组合构建。
- PR28 实际状态仍为 **OPEN / DRAFT**、无 CI checks；本轮未 mark ready、未 merge，故没有 PR28 merge SHA。没有关闭 #6，也没有宣称 C01/全产品完成。

### 两个仍需修复的 P1

已在 [逐行 review](https://github.com/cyjin-yl/shenko/pull/28#pullrequestreview-5639070800) 提交真实调用链、触发状态和行号；这是当前 SHA 的静态确认，未伪称新增运行复现。

1. `bin/shenko/src/main.rs:169` 先启动恢复主体，NapCat 出站订阅到 `:284` 才建立。`ResultRecorded` 无需 broker 即可接续 S1；若 ingress 打开/迁移较慢，S1 后的 broadcast 先于订阅。保留但无人消费的 `_outbox_rx` 不会补转历史，新订阅也不接收旧消息。输入已持久处置、`publication_intents` 为空，回复没有对应 outbound operation，重放输入也不会补做。先建立实际消费者，再放行恢复执行，并保留已提交发布的稳定可补投身份。
2. `bin/shenko/src/main.rs:387` 在等待主体结算之前取消共享 token，同时结束 NapCat 接收/回复与委派消费者。在途模型随后真实返回时，主体仍能 S1/处置输入并正常退出，但回复/委派接收者已经停止；回复可能连持久 intent 都没有。分开 admission/主步停止与已提交工作消费者的生命周期，在消费者具备持久交接/恢复边界后再关闭。模型有真实结果不等于全部后续效果已结算，也不证明远端停止。

### 本轮实际证据与边界

- 全部实际命令通过 SSH 在受管 doescontainer 的容器执行；原共享源码、原 worktree 和未提交改动未写入。只在容器磁盘 `/tmp` 新建独立 Git clone；没有改权限、全局 safe.directory、分支保护或强推。
- 原候选 `/tmp/shenko-c01-recover-tcqxw1p2/repo` HEAD 读回为 `ee9a3e0`，工作区干净。按已公布算法（排序 tracked Rust/Cargo 路径 + NUL + bytes + NUL）重新计算 source SHA-256，确为 `46a7d2df69a3005b65d41e7aa09409967399895f78517fe6363bbbff3ee95e61`；`p1-fixed.bundle` SHA-256 确为 `46e2832133e01802a2dfe2fa597f654a6be079603bf985f8259dbcad4669f8ba`。
- 独立读取原始 `/tmp/shenko-c01-p1-tests-r4.log`：相关 suites 共23 passed、0 failed/ignored；复用此前串行 Cargo 证据，**本轮未重跑 Cargo、未新增测试**。原日志确有 >256槽背压接收、typed重试/冻结输入/结果恢复、未知请求阻断、CAS失败、竞争S1等通过输出。
- 独立读取 `/tmp/shenko-c01-process-l5dc3k9u/rejected.log`、`settled.log`：确有 HTTP429 失败退出、原实例重开、SIGINT 后等待真实模型响应再退出的轨迹。这只是 loopback 模拟；没有 NapCat/委派消费者结算的证据，不能覆盖上述两个组合窗口。source/hash与原始日志读回不等于本轮重新测得历史命令 exit code。
- socket/分页投递已真实拆开；安全 Preparing、带正面证书的 RetryPending 与真实 ResultRecorded 有恢复路径；generic/transport unknown 仍阻断；稳定源身份的昵称/内容冲突保留为非执行观察；主体返回错误/handle监督与UI易失 admission 的表达都有实际补丁。这些局部进展不抵消上述未解决的输出交接缺陷。
- 不补造真实 NapCat/provider 账号验收、32事件完整模型/ACK组合、第二节点/异构设备或7/30天长跑证据。selector/多分支/full writer fencing、完整 genesis/launch、S1后 durable outbox/task/memory replay 和持久 Web ingress 等仍是后续义务。

### 下一动作

从 PR28 已推的实际 head 建 focused continuation 分支修复两个 P1，保留原作者/提交和朋友后继栈（含 PR29），在 #6/PR comments 说明接口、base/head及checkpoint；不用等待认领释放或额外人类审批。复用已有恢复/停机旅程，不建立新测试工厂。验证通过后再按真实权限推进 ready/合入；main 已有 PR30，届时须核对并验证真实整合候选，不能把各分支结果直接相加。#12 SSH 和 #22 第二节点各自独立认领/PR，可先推进不依赖缺失前置证据的部分，不冒称 C01 全链已通。

本次仅更新交接文档，不改产品代码/覆盖计数；记录本文的 docs 分支为 `docs/c01-independent-review-20261025`，提交/PR/合入结果由其真实 GitHub 记录及 #6 接续 comment 定位。

## 历史基线（保留来源，不代表当前 main 或本轮复测）

以下内容源于旧 checkpoint `093b417`、PR3及2026-10-24协作记录；26个测试、coverage状态、资源与推荐顺序均不是本轮新测结论。与上方当前 checkpoint 冲突时以上方为准。PR30 的真实 cordis 接线已在 main，不再沿用下方“尚无 runtime 绑定”作为当前事实；本轮没有独立评审它。

## 工作区事实（已核对）

- 18 个 workspace package：`crates/` 下 17 个库 crate + `bin/shenko`（binary）。
  库：types、kernel、store、broker、subject、memory、channel-napcat、
  channel-napcat-link、exec-host、exec-ssh、wasm、compact-budget、compact-snap、
  awareness-core、ui、seed、update。
- Rust **2024 edition**（workspace `rust-version = "1.85"`），wasmtime 45。
- 测试：`cargo test --workspace` = **26 passed, 0 failed, 0 ignored**，分布：
  store 11（内联 3 + memory_search 3 + store_roundtrip 5）、wasm 3、
  awareness-core 3、types 2、channel-napcat 2、memory 2、update 2、ui 1。
  其余 9 个 crate 尚无测试。
- `docs/state/coverage.json`：147 项 R 中 **27 partial / 120 not_assessed / 0 satisfied**；
  `coverage_status = s1_s2_partial_verified`。

## 已具备的能力（真实状态）

- Store §41.2：SQLite WAL + CAS + ledger + FTS5（CJK 走 LIKE 回退）。
- Subject §5.1/§41.5：连续 S0/S1 loop、S1 决策记录、genesis CAS 封存、outbox broadcast。
- Inference §13/§47：Anthropic broker 优先级队列；本地 Qwen3.8-27B OpenAI-compat provider。
- Channels §16：NapCat OneBot WS 接入、IngressSpool dedup/gap、OutboundLedger
  intent→ack→unknown→reconcile（15s 超时=Unknown，不盲重发）。
- WASM §8/§29：wasmtime 45 Engine/Module/call_func，WASI p1 command 执行，
  `/home` 持久 home preopen；checkpoint 目前只写 `.wasm_checkpoint` 标记。
- Self-dev §35.3–§35.6：BuildProvider（worktree + cargo）、TmuxSeat/SeatObservation、
  HookRegistry 4 阶段、EffectContract 5 种动作、SelfKnowledgeProvider。
- Self-upgrade §35.7：`shenko-update` A/B 槽位 + rusqlite 一致性备份 +
  Prepare→CatchUp→Smoke→Handoff→Observe→Retire 状态机（smoke 为本地离线检查）。
- Playground §43：`/playground` CRUD、原子写、`artifact_changed` inbox 事件。
- Awareness §45/§38：独立 awareness 单元安装/启用/停用 + SelfKnowledge。
- Plugin §7：Plugin trait 按 cordis-rs rev 1754062 的
  Plugin/Context/Service/effect-cleanup 形状设计；`cordis-core` 已在 Cargo.toml
  声明但尚未在 runtime 绑定，也无 crate 使用 cordis 类型。

## 仍存在的主要差距（按审计真实现状）

| # | SPEC 条款 | 状态 | 说明 |
|---|---|---|---|
| 1 | §7 cordis 插件主干 | 未接 runtime | trait 为 Shenko 自有 facade；`cordis-core` 已声明但零引用；TODO 在真实 cordis 生命周期上托管插件 |
| 2 | §8/§29 WASM 家园 | 部分实现 | engine + WASI p1 + home preopen 可用；`MountSpec` VFS 未接线，真实家园/首启未接，checkpoint 仅标记文件；`WasmRuntime` 在 crate 外无调用者 |
| 3 | §35.7 自升级 A/B | 部分实现 | 状态机与一致性备份已具备；`shenko-update` 不是 `bin/shenko` 依赖，quiesce/catch-up/epoch 切换未接线 |
| 4 | §43 Playground | 部分实现 | CRUD + 原子写 + `artifact_changed` 已具备；`POST /playground/{id}/backend` 仅回显，真实进程后端未做 |
| 5 | §12 记忆循环 | 部分实现 | FTS5/LIKE 搜索与 intents 存储已具备；thought loop 已于 S1 后消费标记内容并提交 intent（093b417），`spawn_worker` 仍无调用者 |
| 6 | §35.3 Compact | 部分实现 | Budget/Snap 为独立 crate，未注册为插件、未进入默认插件闭包 |
| 7 | §35.4/§35.5 seat | 部分实现 | `TmuxSeat`/`SeatObservation` 位于 `shenko-exec-ssh`；`main.rs` 仍是 TODO，未绑定 |
| 8 | §5.1/§15.2 QQ 出站 | 部分实现 | UI broadcast + QQ ledger 已具备；真实 QQ 端到端回执未验证 |
| 9 | §44.6 seed/bootstrap | 部分实现 | genesis 经 `cas.seal_object` 封存；seed 内容装配、WASM 家园、首启 probe 未完成 |
| 10 | §16 NapCatLink backend | 可选实现 | `shenko-channel-napcat-link` 的 `napcat-link` feature 默认关闭，非 bin 依赖，未接入 |

## 下一步推荐工作（按因果链）

1. **§7**：把 `cordis-core` 真正绑定到 Plugin 生命周期；删除或落实 facade 的 TODO。
2. **§8/§29**：`MountSpec` VFS 接线 + 真实 checkpoint 快照 + 首启 WASM 家园。
3. **§35.4/§35.7**：TmuxSeat 接入 main；`shenko-update` 接入 runtime 完成 A/B handoff。
4. **§12/§35.3**：memory intents 由 thought loop 消费；Compact 注册为插件。
5. **§44.6**：完整 seed 装配与首启 probe。
6. **验收**：V1-01~V1-15 尚未执行。

## 资源

- Qwen3.8-27B 端点：https://doesvm.icefish-exponential.ts.net:8446（Tailnet，无需真实 key）
- SPEC §22.1 基线模型：`swift-qwen3.8-27b-nvfp4`，262144 token 上下文
- PR #3：https://github.com/cyjin-yl/shenko/pull/3（已合并）

## 验收计数

- R：27/147 partial，0 satisfied（以 `docs/state/coverage.json` 为准）
- 测试：26 passed / 0 failed（见上）
- 待验收：V1-01~V1-15 均未执行

## 社区任务栈接续（2026-10-24）

- [#4总追踪](https://github.com/cyjin-yl/shenko/issues/4)、22个工作包#5–#26已发布；[任务图](../plans/active/community-stack.md)、[朋友PROMPT](../../PROMPT.md)。归属映射不改变实现/验收状态。
- 当前公开代码盘点基线`d4b612b`；此文档增量不含产品实现和新构建结果。
- 恢复线暂停，本地`291d5a5`候选及ACK接线保留，32入站只渲染10却全ACK的问题仍待验证。Broker/SSH、UI和cordis候选排队，不当作已merge。先#6，再#12，再#22第二节点接走任务。
- 朋友离线不锁任务：从最后已推checkpoint开continuation分支，保留作者/提交/PR，不等交还/审批，未知effects先对账。
