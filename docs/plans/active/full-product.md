# 完整开发执行计划

状态：规划基线，产品实现尚未开始。目标与授权见 [GOAL.md](../../../GOAL.md)，完整范围与停止条件见 [COMPLETION.md](../../governance/COMPLETION.md)。下面的分组是可调整工作顺序，不是必须逐阶段审批的流程。

所有开发者先读 [shenko-dev](../../../.agents/skills/shenko-dev/SKILL.md)，再按需使用原子 Skill。Rust 采用 2024 edition；开发无新增人工 gate，验证用最少有效证据，不为一个连贯增量新增十个或更多测试。

## 初始工作图

| 工作组 | 首要结果 | 可并行/依赖关系 |
| --- | --- | --- |
| W01 文档和实现覆盖 | 读现行 R/B/case，建立 feature→证据与完整范围审计 | 协调者从开始持续维护；不要求所有 worker 反复通读原文 |
| W02 Seed/WASM/包闭包 | 可启动持久家园、mount、默认 Git、包/generation、正确 bootstrap | 与 W03/W04 定义持久边界；工具移植可并行 |
| W03 Durable/存储/副作用 | 任务、原始历史、intent/receipt/unknown、快照与迁移基础 | 为 W05/W06/W07 提供稳定合同；先小范围真实故障验证 |
| W04 推理和主流 | 持续主体、同模型主动语义等待最多600秒、C=1公平进展、四流基线、真实 admission 与 §47 wire profile | children 可 sleep；主等待仍承接前文+新增上下文轮询；与节点真实本地/API模型能力共同调度 |
| W05 Cordis/能力/注入 | 独立 awareness/感知/Skill/code、依赖解析、自解释；从任意实际输入当场形成新 awareness，并按价值自主沉淀 | 与 W02/W03/W04 核对版本和资源；时间、未竟工作、重复/无聊、项目和邮件可自然组合，dreaming可选 |
| W06 NapCat/SSH/Seats | QQ 收发、实际已有 Claude/OMP seat、TUI 属性/高亮、问答/授权、能力自补 | 可以用隔离 fixtures 开发；最终用真实服务与 seat 验收 |
| W07 自开发/自迁移 | QQ→主体→child→代码候选→验证→交接→继续；核心/恢复器也可换 | 依赖 W02–W06；不能只产 patch 或永久锁死宿主 |
| W08 WebUI/配置/Playground | 单主体输入/稳定流式、真实配置、长期 full-stack artifacts | UI 状态协议可先与 W03/W04 对齐；设计/实现/视觉核验独立推进 |
| W09 记忆/研究/compact | FTS5原文、两个独立 compact、主动提醒、reset-to-best、外部 memory 移植 | 所有模型调用经过 W04；不把 summaries 变唯一真相 |
| W10 具名工具与 REPL | toybox/gcc、认证 Git/gh/tea、真实 WASM SSH；Python/Bun/Node可恢复REPL | 可分别移植；host wrapper 仅阶段辅助，具名目标完整交付 |
| W11 渠道/声音/身份 | 其他具名渠道、通话/课堂/声纹、Life关联、开放聊天/manual admin | 与 W05事件和W09历史协议组合，最终真实音视频/附件路径 |
| W12 Runtime/设备/节点 | Linux VM原版LayerFS、条件BoxLite、Win/macOS/Linux与rootAndroid操作；已授权SSH；身份加入、异构provider、递归选主/分区合并、供应商隐私、自主环境修复、无LLM日志/导出/镜像恢复 | 可分设备adapter；同一自我不强制同步provider/凭据；按真实通路和权限接续，以少量关键场景复用验证 |
| W13 云额度与生态 | 周期预算插件、市场/传统Skill/MCP、中英文教程和自解释；Claude API/SDK/代理迁移与 PV01–PV08 | 实施顺序靠后，仍属完整目标；与 W04/W05 协作完成 R142/B50 的自主感知与迁移；不依赖用户为每项裁决 |
| W14 组合/长期/交付 | 全范围convergence、性能与7/30天观察、证据有效性、ready PR | 可提前开始稳定候选长跑；候选变化影响按真实范围判定 |

## 工作包最小内容

写清要改变的实际行为、关联 R/B/case、依赖与输入版本、ownership、可以修改的范围、完成证据及下一执行动作。复杂任务可以有独立计划；小改动用现有任务记录即可。不要先建设一个大项目管理平台才开始实现。

## 集成与收敛

协调者把可独立工作分给隔离 worktrees，把协议变更通知依赖者；选择已验证的提交进入自己的集成分支。相关测试在集成候选再跑，不能把各分支成功简单相加。定期从完整需求端重新检查已完成标记，新增遗漏工作，保持 S3–S5 在总目标里。

每次 checkpoint 记录当前真实可执行任务；不把这张初始表机械当成固定流水线。目标不变时方法由开发者自主修正。
