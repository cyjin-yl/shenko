# Shenko 开发交接状态

**更新时间：2026-10-23**
**当前分支：`impl/s2-self-dev`**
**HEAD：d5dc750**
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

## 仍存在的主要差距（按优先级）

| # | SPEC 条款 | 状态 | 说明 |
|---|---|---|---|
| 1 | §7 cordis-rs | 未实现 | 注释声称使用，实际零依赖；Plugin trait 是自写的 |
| 2 | §8/§29 WASM 家园 | 空壳 | WasmRuntime 12 行，无 wasmtime 引擎/WASI/VFS |
| 3 | §20 S1 决策层 | 缺失 | SubjectStepDecision/StepDecision 零使用 |
| 4 | §35.7 自升级 A/B | 未实现 | prepare/copy/catch_up/smoke/handoff 流程缺失 |
| 5 | §43 Playground UI | 未实现 | 整章缺失 |
| 6 | §5.1 outbox/回复 | 部分 | 出站 QQ 无 operation ledger 回执 |
| 7 | §44 真实 seed bootstrap | 占位符 | genesis ref 全是 stub_obj() 字面量 |
| 8 | §37 cordis 生命周期 | 空壳 | Hook 注册表零调用点 |

## 下一步推荐工作（按因果链）

1. **Batch-5**：cordis-rs 真实依赖接入 + Plugin trait 对齐 + Hook 调用点
2. **Batch-6**：WASM 家园基础实现（wasmtime + WASI 最小 runtime）
3. **Batch-7**：seed bootstrap 真实内容（ObjectRef 封存代替 stub_obj）
4. **Batch-8**：自升级 A/B handoff §35.7

## 资源

- Qwen3.8-27B 端点：https://doesvm.icefish-exponential.ts.net:8446（Tailnet，无需真实 key）
- SPEC §22.1 基线模型：`swift-qwen3.8-27b-nvfp4`，262144 token 上下文
- PR #3：https://github.com/cyjin-yl/shenko/pull/3

## 验收计数（当前 impl/s2-self-dev）

- 已实现 R 项（partial）：R001 R003 R004 R005 R008 R009 R013 R014 R018 R019 R029 R030 R031 R051 R059 R065 R066 R082 R083 R086 R087 R092 R093（共 23/147）
- 测试：5/5 store roundtrip 通过
- 待验收：V1-01~V1-15 均未执行
