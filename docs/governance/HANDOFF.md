# Shenko 开发交接状态

**更新时间：2026-10-24（协作入口；历史构建数据不代表本轮复测）**
**当前分支：`main`**
**HEAD：`093b417`（本文件编写时可见的最后提交）**
**upstream PR：[#3](https://github.com/cyjin-yl/shenko/pull/3)（已合并）**

## 立即可继续的工作

```bash
cd /tank/shared/projects/shenko/shenko
git log --oneline -5          # 查看最近提交
cargo check --workspace       # 验证编译
cargo test --workspace        # 当前 26 passed / 0 failed
```

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
