# Shenko 开发交接状态

**更新时间：2026-10-23**
**当前分支：`impl/s2-self-dev`**
**HEAD：pending commit**
**upstream PR：[#3](https://github.com/cyjin-yl/shenko/pull/3)**

## 立即可继续的工作

```bash
cd /tank/shared/projects/shenko/shenko
git log --oneline -8          # 查看最近提交
cargo check --workspace       # 验证编译
cargo test -p shenko-store    # 运行测试
```

## 已完成（impl/s2-self-dev，等待合并到 main）

### S2 核心实现（原始提交）
- TmuxSeat §35.5：list_panes, capture-pane -e, send_keys, new_window
- BuildProvider §35.3/§35.4：git worktree 隔离, cargo check/build/test, apply_patch
- NapCat IngressSpool §35.6：dedup key=provider:account:conversation:message_id, gap recording
- HookRegistry §37：6 phases, deterministic order, check_allowed()
- EffectContract §36.2：5 distinct action kinds (Reset/Restore/Replay/Cancel/Compensate)
- SelfKnowledgeProvider §38：self://runtime/docs/source/design URIs

### Batch-1 关键断链修复（b825638）
- main.rs：BrokerHandle 接入 SubjectLoopState → 主体调用真实 Anthropic API
- ws.rs：WebSocket 用户消息路由到 subject inbox
- self_knowledge.rs：虚假 WASM 能力声明已修正
- IMPLEMENTATION_DOCS_MAP.md：章节号纠正（§12 记忆、§24 CU、§12.3 compact）
- Cargo.toml：rust-version 1.82→1.85（edition 2024 需 ≥1.85）

### Batch-2 gap 修复（10004da）
- IngressSpool 接入 NapCat client（dedup + gap recording 生效）
- Broker：BinaryHeap 优先级队列（Urgent>Subject>Normal>Low per §13.1）
- ChildDispatcher：真实 tokio channel dispatch，DelegationIntent 转发 §5.2
- MemoryStore：accept_intent() channel + stub worker §12.2

### Batch-3 gap 修复（12372a2）
- S0 封口 input_plan_ref（blake3 of event IDs）per §41.5
- BudgetCompact：真实 should_compact/compact_messages §12.3/§35.3
- SnapCompact：真实 snapshot triggers，与 BudgetCompact 独立 §35.3
- inbox_events 表写入通过 WriteBatch §12.1

### Batch-4 gap 修复（d5dc750）
- LocalModelProvider：Qwen3.8-27B via Tailnet，OpenAI-compat §22.1
  - env: SHENKO_USE_LOCAL_MODEL=1, SHENKO_LOCAL_MODEL_URL (optional)
  - 端点：https://doesvm.icefish-exponential.ts.net:8446（私有 Tailnet，无真实 key）
- TmuxSeat：parse_ansi_to_plain() + extract_question_lines()，SeatObservation §17.5
- session_events 持久化通过 WriteBatch §12.1
- PR #3 描述已更新

### Batch-5 gap 修复（4f2509f）
- shenko-kernel：Plugin trait 对齐 cordis-rs rev 1754062（Context/Service/generation/effect_cleanup）§7
- shenko-subject：HookRegistry 接入 SubjectLoopState，4 个阶段触发点 §37
- shenko-subject：S1 记录 SubjectStepDecision（action+reasoning+delegations）§41.5
- main.rs：HookRegistry 创建并传入 subject loop

### Batch-6 gap 修复
_工作树改动尚未提交（HEAD：pending commit）。_

- shenko-wasm：wasmtime 45 Engine + Module 实例化 + call_func，`WasmRuntime` 真实执行 §8/§29
  - checkpoint/restore 目前只写/读 `.wasm_checkpoint` 标记；WASI/VFS/宿主挂载未实现
- bin/shenko：bootstrap_genesis 经 `cas.seal_object` 封存真实 seed_manifest/profile/home/bootstrap_event 对象 §44.6
  - 仍是最小 genesis；seed 内容装配、WASM 家园与首启 probe 未完成
- shenko-subject：outbox 模块 + broadcast 通道，主体每步模型输出发布到时间线 §5.1/§21.12
- main.rs / shenko-ui：同一 outbox broadcast 共享给 subject loop 与 UI；UI 输入经 inbox 回主体
  - outbox 为内存 broadcast，尚无持久 operation ledger / 投递回执 §15.2

## 仍存在的主要差距（按优先级）

| # | SPEC 条款 | 状态 | 说明 |
|---|---|---|---|
| 1 | §8/§29 WASM 家园 | 部分实现（engine+module+call 可用；WASI/VFS 未实现） | wasmtime 45 Engine/Module/call_func 已接通；checkpoint 仅标记文件 |
| 2 | §35.7 自升级 A/B | 未实现 | prepare/copy/catch_up/smoke/handoff 流程缺失 |
| 3 | §43 Playground UI | 未实现 | 整章缺失 |
| 4 | §5.1 outbox/回复 | 部分实现（UI broadcast 已通） | subject→UI 时间线 broadcast 已接通；出站 QQ operation ledger/回执仍缺 |
| 5 | §44 真实 seed bootstrap | 部分实现（真实 CAS 对象） | genesis ref 经 CAS seal_object 封存；完整 seed 装配/WASM 家园/首启 probe 未完成 |

## 下一步推荐工作（按因果链）

1. **Batch-7**：WASI/VFS 最小 runtime + checkpoint 真实状态快照（§8/§29）
2. **Batch-8**：seed manifest/profile 装配与首启 probe（§44），outbox 持久 ledger + 回执（§5.1/§15.2）
3. **Batch-9**：自升级 A/B handoff §35.7

## 资源

- Qwen3.8-27B 端点：https://doesvm.icefish-exponential.ts.net:8446（Tailnet，无需真实 key）
- SPEC §22.1 基线模型：`swift-qwen3.8-27b-nvfp4`，262144 token 上下文
- PR #3：https://github.com/cyjin-yl/shenko/pull/3

## 验收计数（当前 impl/s2-self-dev）

- 已实现 R 项（partial）：R001 R003 R004 R005 R008 R009 R013 R014 R018 R019 R029 R030 R031 R051 R059 R065 R066 R082 R083 R086 R087 R092 R093 R132 R135（共 25/147）
  - Batch-5 新增覆盖：§7 cordis-rs 对齐、§37 hook 调用点、§41.5 S1 决策记录
  - Batch-6 新增覆盖：§8/§29 wasmtime runtime、§44.6 CAS genesis、§5.1/§21.12 subject outbox → UI
- 测试：5/5 store roundtrip 通过
- 待验收：V1-01~V1-15 均未执行
