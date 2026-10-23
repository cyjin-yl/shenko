# 完整交付合同

## 1 这次目标的边界

目标是 [SPEC.md](../../SPEC.md) 的完整产品，**包含 S1–S5**。S2 首版自开发闭环是关键中间成果。阶段顺序可以调整，也可以提前交付；原有完整目标继续保留。朋友在自己的 fork 自主开发、合并和持续验证，本次交付形式是上游 ready PR；上游拥有者随后 review 与合入。

完整不是“覆盖率达到一个漂亮百分比”。每个原有 R 必须保留，映射到具体 B、规格章节、实现和证据。规格有些行要求研究/文档，有些要求运行行为，有些同时包含两者；采用相应证据。对真正可选的 BoxLite 等实现选择，可证明条件不适用；不能利用此机制豁免用户具名要求，如真实 SSH WASM、Node/Bun 目标、Android、两种 compact 或完整 computer use。

所有开发者遵循必读 [shenko-dev](../../.agents/skills/shenko-dev/SKILL.md)。本合同定义完整产品结果，不为每次增量增加人工 gate，也不要求重复新写完整验收套件；小改可零新增测试，同一连贯增量禁止十个或更多新测试，复用有效证据后继续交付。

## 2 完整状态的必要条件

| 范围 | 完成证据 |
| --- | --- |
| 需求覆盖 | `requirements.json` 的所有现行 R 有完整范围解释、实现或文档落点及真实证据；细分 subcase 不丢母项 |
| 上游完整度 | 每个已选 B 对应已采用上游行为、真实差异与验收；未采用上游无关功能不需要机械复刻 |
| 已枚举验收 | 现行 acceptance index 全部已裁定适用范围；适用项有通过结果，条件项有可审查依据；不能按困难标 N/A |
| 实现质量 | 产品可构建、安装、启动、配置、使用和维护，正常路径与相关异常路径真实运行；无伪成功入口或未接线关键功能 |
| 运行证据 | 模型、provider、tokenizer/template、adapter、runtime、系统、硬件、源码和 plugin lock 可关联；模拟与真实测试明确分开 |
| 可恢复性 | 实测 intent/receipt 各故障点、未知外部效果、主循环恢复、C=1、断网、满盘、迁移、新事实保留和节点分区等适用合同 |
| 长期性 | S5 的 7/30 天运行报告按真实墙钟时间和明确候选/环境形成；加速测试不能冒充 30 天无人运行 |
| 使用体验 | WebUI 是同一主体时间线，实际流式与交互稳定；Playground、渠道降级、媒体和电脑操作有真实证据 |
| 资料交付 | 新环境可依说明复现，NInfer/Strata 中英文教程、插件/awareness/Skill 编写、自解释、迁移与恢复资料准确 |
| PR 状态 | 所有交付提交已经 push；上游集成 PR URL、base/head 和证据版本一致，无未解决冲突或实质失败，已标 ready |

现行规格中没有实测数值的性能目标，由 Agent 根据指定设备先测基线、提出合理指标并记录 Why，再进行改进和回归。不能把未给数值当“无限性能”承诺，也不能因缺少人类逐项定阈值而停工。已明确的限制、4×128K 基线、连续主体和失败语义继续约束该选择。

## 3 全范围能力族

这些能力族用于交叉审计，不替代逐 R 与逐 case 的分母。

1. **持续个人主体与协作**：永续串行主流、同一模型以旧/新上下文持续轮询的最长600秒主动语义等待、灵魂/人格与实例命名、统一知识、任意 child prompt/model、双向讨论、有限 child 直发、长期自主兴趣、DND/过滤及自主修复。
2. **最小与可自改运行系统**：Rust 2024/cordis-rs、bootstrap/seed、可改 core/rescue、真实 WASM 家园、mount、Nix-like 闭包、插件 generation、跨版本 hook、完整自开发/迁移/继续运行。
3. **存储与记忆**：原始 session/FTS5 中文检索、默认 host-mounted 私有 Git memory、二进制版本/CAS、effect ledger、两种独立 compact、可安装 memory 系统/移植支持、PMA 与 child reset-to-best、永久默认保留和空间处置；全部LLM失效后的系统日志、凭据授权读取/导出与离线磁盘镜像恢复。
4. **工具与执行环境**：实际具名 WASM 工具/认证 Git/SSH/keygen、远端文件与执行、Python/Bun/Node REPL 的真实支持与恢复等级、原版 LayerFS Linux VM、条件采用的 BoxLite、所有适用资源/副作用边界。
5. **推理与资源**：NInfer/Strata/vLLM/1Cat 原生透传、未知容量、同引擎 alias、取消失联、C=1 轮转、真实 KV 驻留/换出/replay 和自动半池案例；云周期额度能力也完成，只是实现顺序靠后；实际采用 provider 的 API/SDK/model/gateway 版本合同、请求投影、鉴权与自主迁移按 §47 验收。
6. **渠道与身份**：NapCat/napcat-cli/官方 QQ/iLinkBot/飞书具名接入、开放聊天与手设 admin、富文本附件、身份候选/Life 复用、插件化消息和 API provider。
7. **远程与多节点**：已有 Claude/OMP/其他适配 seats、TUI 属性/高亮与缺能力自补、已授权SSH身份、Linux/Windows/macOS和rootAndroid能力、自主环境漏洞修补与更新；较晚身份加入、连通域主节点、异构本地/API模型与provider隐私、递归接续和孤立智能分支重连合并。
8. **声音与共同活动**：通话、旁听、录音、转写修订、声纹感知与可演进身份关联、同一主流的工具委派、可声明的实际资源与存储成本。
9. **可扩展认知与生态**：能力发现且正确使用、独立 perception/awareness/Skill/code、从任意实际输入即时汲取并自主演进 awareness、时间/未竟事/重复/项目/邮件的自然察觉、自动版本依赖闭包、传统 Skill/MCP 市场适配与原生插件优先、自解释和 Why；dreaming 保持可选社区组合。
10. **界面与作品**：Proto-UI 主线/shadcn/compiler seam、Magpie/Alma 参照的稳定流式、默认主流 UI、所有配置可对话与面板查看、跨渠道生成 UI、持久可交互全栈 Playground 与主体感知。

## 4 证据记录

一条证据至少能回答：验证了哪个要求/场景，运行哪个源码和环境，执行了什么，观察到什么，在哪里能复查。日志、截图、录屏、结构化报告、真实 provider receipt 与独立重跑可以组合。秘密留在授权的本地凭据设施，不进入公开或普通 PR 附件；报告保留非秘密的环境标识。

建议记录：`evidence_id`、`requirements`、`cases`、`candidate_commit`、`consumed_inputs`（实现、依赖、配置、模型/设备等实际相关输入）、`method`、`environment`、`started_at`、`finished_at`、`result`、`artifacts`、`reproducer`、`reviewer`。字段可以随实现演进，意图是让证明可追溯，不能用填写表格代替真实运行。

证据可以按影响复用：无关文案改动不使已验证的存储行为自动失效。若相关代码、配置、依赖、模型或环境改变，重新验证受影响部分并解释有效性边界。最终报告绑定当前交付 commit 和有效证据集合；不能凭一次旧 SHA 的绿灯宣布任意新版本通过。长跑中重大运行闭包变化必须分段或重启相应观察，不偷偷把不同候选时间拼成稳定 30 天。

## 5 状态与不能冒充完成的东西

`specified`、`implemented`、`buildable`、`simulated`、`verified_real`、`verified_long_run` 是不同事实。`blocked_external`、`waiting_observation`、`not_implemented` 和 `failed` 都不是 complete。初始文件只证明规格与交接材料存在。

以下均不能单独结束 goal：空 TODO、源码文件数量、文档齐全、模型说看起来完成、测试数量未知或零执行、全部测试被 skip、展示 mock 的截图、接口返回 200、只把“unsupported”错误做得友好、仅完成首版、PR 已建立或 review 迟迟没有人回复。

缺失设备、真实服务账号或目标环境时，继续准备代码、适配与可运行验证；记录精确等待，不伪造硬件或凭据。模型判断“impossible”也不是用户批准移除功能。先查证能否移植、替换实现或分解问题；确有矛盾时提出最小的产品裁决问题，其他范围继续推进。

## 6 最终报告与 goal evaluator

最终完整性审计由独立 worker/可复现检查对照原始 R、B、case 和实际交付执行。作者自述不够，另一个 Agent 单纯复述作者也不够。输出 `docs/evidence/completion.json` 及可阅读报告，列出完整分母、适用性依据、当前候选、证据有效性、失败/阻塞数、长期观察、PR head。未解决的实质缺口必须为零；真实可选条件的排除逐项说明。

Claude `/goal` evaluator 只看到对话里呈现的内容，不自己打开这些文件。最后一轮应在 transcript 中展示审计命令、结果摘要、关键证据和 PR 当前状态，才能让其判定有依据。goal 清除和产品完成必须分别记录。
