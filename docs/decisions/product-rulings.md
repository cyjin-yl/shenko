# 产品裁决记录


<a id="spec-28"></a>
<!-- migrated-contract: SPEC:28 -->
# 28 规格裁决记录与仍未决的问题

这是围绕真实冲突的规格访谈，不是要求重新授权已经明确的自主性。默认开放聊天、单主体、主无直接文件工具、子 Agent 执行、任何代码可自改、插件优先、Computer Use、实时媒体与动态 UI 等不再重问“要不要”。前面三个未回答的问题——宿主服务边界、所有权模型、最低硬件——仍标未决，不阻断本文完成。

D1/D2/D3/D7/D8/D9/D11/D12 已收到用户答案并落实正文；四流 128K 与 133120 冗余的随后更正同样生效。D4/D5/D6/D10 仍有未决部分，保留原编号方便继续讨论，不阻碍规格整理。U50 现已规定完整开发交接：接收 GOAL.md 的开发 Agent 按 [§40](../governance/development-contract.md#spec-40)、[§46](../governance/COMPLETION.md#spec-46) 自主实施；技术未定不自动变成人类 gate。原始问答逐字保存在 [§33](../requirements/user-sources.md#spec-33) U20/U21。

| ID | 状态与用户裁决 | 正文落点 / 尚未决定的部分 |
| --- | --- | --- |
| D1 | 已裁决：主体统一知道经历，按人格/灵魂自主决定说与不说；可改变保密选择，不硬设亲密度。灵魂可自改，child prompt/model 可自由指定 | [§5.2](../specs/subject-runtime.md#spec-5-2)、[§6](../specs/subject-runtime.md#spec-6)；取消按群聊受众限制主体记忆的默认方案 |
| D2 | 已裁决：有限情况下允许 child 直接发结果，参考 OMP | [§5.3](../specs/model-children.md#spec-5-3)；ReplyGrant、结果归属、失败/取消与去重作为具体协议草案 |
| D3 | 已裁决：外部消息通常最高紧急，可打断 child；频繁打断导致停滞时问主体是否临时 DND，结束后统一处理队列；可自设并修复垃圾过滤 | [§13.8](../specs/subject-runtime.md#spec-13-8)；真实后端边界与取消确认照常，不承诺任意时刻零代价抢占 |
| D4 | 方向已定；交付顺序采用可自主调整的工程默认：先核验首台 1Cat 四流 128K 的真实能力并接入受管资源域，再补尚缺硬保留/恢复能力 | [§13.7](../specs/inference.md#spec-13-7)、[§22.1](../specs/nodes.md#spec-22-1)、[§41.9](../specs/inference.md#spec-41-9)–[§41.10](../specs/inference.md#spec-41-10)；自动半池仍是验收场景，不以普通 API 假装硬配额，也不等待人类选择内部实现顺序 |
| D5 | 未裁决：陌生群友请求操作用户服务器时，既有广泛委托与请求者/任务授权的适用关系 | 保留默认开放对话、管理员手动设置；不把未答变成关闭所有聊天或逐条审批 |
| D6 | 个体偏好可通过对话/配置继续形成；工程默认保持主体基于人格、上下文与已有授权动态选择联系方法，不新增固定联系人或固定升级时钟 | [§17](../specs/remote-seats.md#spec-17)、[§43.1](../specs/configuration-playground.md#spec-43-1)；明确给出的安静时段/受众偏好作为版本化记忆或策略；D3 的主体 DND 与人类联系偏好分开 |
| D7 | 已裁决：优先磁盘成本，允许 breaking change，旧副本继续运行，迁移/烟测后最后交接自己 | [§9.5](../specs/self-update.md#spec-9-5)，SelfUpdate/Migration 插件；自主决定，无逐次用户审批 |
| D8 | 已裁决：文字、录音、屏幕帧默认永久；空间不足由主体决定遗忘或迁往用户自有存储 | [§12.4](../specs/memory.md#spec-12-4)，Retention/Archive 插件，区分 forget/move/erase |
| D9 | 已裁决：可以长期持续研究自主兴趣，这就是应有行为 | [§14.6](../specs/research-memory.md#spec-14-6)；有限工作单元不形成兴趣的终身期限或固定汇报义务 |
| D10 | 工程默认同时提供渠道内降级与受认证 Web 增强入口，由主体按能力/任务选择；后续偏好可对话配置 | [§21.7](../specs/web-interface.md#spec-21-7)、[§43](../specs/configuration-playground.md#spec-43)；不能迫使用户打开 Web 才能理解基本回复，也不能在弱渠道展示实际上不能执行的控件 |
| D11 | 已裁决：优先持续在线，在线节点可接管；多主体时动态投票、留下新主体并合并记忆任务 | [§5.4](../specs/nodes.md#spec-5-4)；正常单主体，故障分叉显式记录、重连收敛；原先分区绝不分叉的默认已被替换 |
| D12 | 已裁决：用户自有 Qwen3.8-27B，经 Tailnet；随后更正为四流 128K，133120 是冗余 | [§22.1](../specs/nodes.md#spec-22-1)；Life 历史生产记录与后续实际运行 manifest 分开，不以本轮只读研究冒充新实测 |

技术上的未决问题还包括：guest 内完整 Nix 与 Nix-like 是否都必须；toybox 指定 applets；gcc-wasm 的 hosted/target 需求；gh/Gitea WASM 上游 port 的产品化投入；多租户是否属于首发范围。这些都已保留目标，不用未答问题为删需求找理由。

<!-- /migrated-contract: SPEC:28 -->

<a id="input-disposition-definition-conflict-20261112"></a>
## 未处置输入的定义冲突（2026-11-12，未解决）

[R152](../requirements/traceability.md) 要求未被主体处置的输入默认进入 `deferred`，在后续 S0 持续可见；[领域正文 §57.3](../specs/subject-runtime.md#spec-57-3)、[B58 的原文](../requirements/design-provenance.md) 与 [AW13](../specs/verification.md#spec-57-13) 则规定：未处置输入记为 `ignored` 并附理由，显式 `deferred` 的输入才继续留队。这是两份现存定义的差异，不能以证据索引的转述消除。

当前 `dispositions_for_events()` 采用 `ignored` 与“主体未在本轮处置”理由。证据账本保留 R152、B58、AW13 的逐字来源与迁移来源，B58 的派生 `contract`、`verification` 字段恢复为其自身原表的语义；R152 的实现状态记为 `partial`，冲突列入 blocker，仍为 `satisfied=false`。本记录不选择新的默认行为，不改变原规范，也不把尚未运行的 AW13 判为通过；其余已获授权的开发与验证继续执行。
