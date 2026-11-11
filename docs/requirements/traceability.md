# 需求追踪矩阵


<a id="spec-32"></a>
<!-- migrated-contract: SPEC:32 -->
# 32 需求追踪矩阵

本表将全部 147 项需求映射到正文，并逐项列出用户原话 U 与 [§39](design-provenance.md#spec-39) 的 B 行为比较入口；B 再指向精确 Ref、采用范围、差异和验证。原始需求与后续澄清逐字见 [§33](user-sources.md#spec-33)（U01–U62）。有正文落点表示规格覆盖，不表示已经实现或兼容性已经验证。最新用户裁决优先，工程默认可由开发 Agent 自主落实；[§41](../specs/architecture.md#spec-41) 给出首版跨模块执行合同，[§40.9](../governance/development-contract.md#spec-40-9) 说明 Why 与自主调整；[§42](../specs/quota-resources.md#spec-42)–[§45](../specs/cognition-units.md#spec-45) 补额度、配置、Playground、首次启动及开放来源/可原子化的 awareness、感知与依赖装配。

## 原始需求 R001–R045

| ID | 一句需求 | 正文落点 | 用户来源与行为对照 |
|---|---|---| --- |
| R001 | 面向个人聊天与群聊的 Agent Bot，具备多说话人和会话边界。 | [§1](../specs/architecture.md#spec-1)、[§4](../specs/subject-runtime.md#spec-4)、[§15](../specs/channels.md#spec-15)–[§16](../specs/channel-adapters.md#spec-16) | U01；B01/B22 |
| R002 | 以 2026 年 10 月最新可验证技术为研究依据，标明版本和未知状态。 | [§1](../specs/architecture.md#spec-1)、[§3](../research/product-positioning.md#spec-3)、[§8.1](../specs/wasm-home.md#spec-8-1)–[§8.2](../specs/wasm-tools.md#spec-8-2)、[§21.2](../specs/web-interface.md#spec-21-2)；[§41](../specs/architecture.md#spec-41) | U01；B41 |
| R003 | Agent 拥有跨重启、崩溃和升级延续的持久运行环境。 | [§5.4](../specs/nodes.md#spec-5-4)、[§8](../specs/wasm-home.md#spec-8)、[§10](../specs/durable-state.md#spec-10)；[§41](../specs/architecture.md#spec-41) | U01；B04/B05 |
| R004 | 基础执行环境采用具有实用 Linux/POSIX 兼容目标的 WASM 沙箱。 | [§8.1](../specs/wasm-home.md#spec-8-1)–[§8.2](../specs/wasm-tools.md#spec-8-2) | U01；B05 |
| R005 | 沙箱可挂载任意宿主进程有权访问的路径，明确 RO/RW/COW 等语义。 | [§8.3](../specs/wasm-home.md#spec-8-3)、[§10.1](../specs/durable-state.md#spec-10-1) | U01；B05/B08 |
| R006 | Agent 可以自主记录、整理和检索文件化文档与记忆。 | [§5.2](../specs/subject-runtime.md#spec-5-2)、[§10.3](../specs/memory.md#spec-10-3)、[§12.2](../specs/memory.md#spec-12-2)–[§12.3](../specs/context-policies.md#spec-12-3)；[§41](../specs/architecture.md#spec-41) | U01；B03/B10/B15 |
| R007 | 沙箱实际执行工具，提供输入输出、取消、超时和任务生命周期。 | [§8.2](../specs/wasm-tools.md#spec-8-2)–[§8.3](../specs/wasm-home.md#spec-8-3) | U01；B05/B06 |
| R008 | 通过通用接口安装基础或大型工具，保留 toybox、gcc-wasm 等具名目标。 | [§8.2](../specs/wasm-tools.md#spec-8-2)、[§9](../specs/self-update.md#spec-9) | U01；B06 |
| R009 | Agent 可以自行安装、配置、升级和回滚工具。 | [§5.2](../specs/subject-runtime.md#spec-5-2)、[§9](../specs/self-update.md#spec-9) | U01；B07 |
| R010 | 兼容外部本地记忆系统，明确依赖、移植和替代后端边界。 | [§3.2](../research/product-positioning.md#spec-3-2)、[§8.1](../specs/wasm-home.md#spec-8-1)–[§8.2](../specs/wasm-tools.md#spec-8-2)、[§12.3](../specs/context-policies.md#spec-12-3) | U01；B05/B15 |
| R011 | 沙箱之外的 Rust 2024 控制框架保持最小，故障范围和常驻负担可测。 | [§1](../specs/architecture.md#spec-1)、[§2](../specs/architecture.md#spec-2)、[§7](../specs/plugins.md#spec-7)、[§22](../specs/nodes.md#spec-22)、[§40.4](../governance/development-contract.md#spec-40-4) | U01/U59；B02/B05 |
| R012 | 沙箱损坏后自动恢复，最终能回到初始可启动环境。 | [§7](../specs/plugins.md#spec-7)、[§10.4](../specs/durable-state.md#spec-10-4)、[§23](../specs/verification.md#spec-23) | U01；B05/B08 |
| R013 | 提供对二进制友好的版本、快照、分支、比较和恢复机制。 | [§10.1](../specs/durable-state.md#spec-10-1)–[§10.2](../specs/durable-state.md#spec-10-2) | U01；B08 |
| R014 | 支持认证的私有 Git 记忆/知识库，保留 gh/tea WASM 兼容目标。 | [§8.2](../specs/wasm-tools.md#spec-8-2)、[§10.3](../specs/memory.md#spec-10-3) | U01；B06/B10 |
| R015 | 默认记忆是宿主目录挂载进初始 WASM 环境后初始化的本地 Git 仓库。 | [§2](../specs/architecture.md#spec-2)、[§10.3](../specs/memory.md#spec-10-3)–[§10.4](../specs/durable-state.md#spec-10-4) | U01；B10 |
| R016 | Agent 按需要创建并维护自己的 SSH 身份密钥。 | [§11](../specs/remote-seats.md#spec-11) | U01；B11 |
| R017 | 人类操作界面能展示和复制 Agent 的 SSH 公钥与指纹。 | [§11](../specs/remote-seats.md#spec-11)、[§21.1](../specs/web-interface.md#spec-21-1) | U01；B11/B35 |
| R018 | SSH 可连接人类机器、未来完整容器及 Agent 自有节点。 | [§11](../specs/remote-seats.md#spec-11)、[§17](../specs/remote-seats.md#spec-17)、[§22](../specs/nodes.md#spec-22)；[§41](../specs/architecture.md#spec-41) | U01；B11/B24 |
| R019 | 实际支持 SSH WASM，不能以宿主 SSH 接口代替该具名目标。 | [§8.2](../specs/wasm-tools.md#spec-8-2)、[§11](../specs/remote-seats.md#spec-11)、[§23](../specs/verification.md#spec-23) | U01；B06/B11 |
| R020 | 包管理采用 Nix 或 Nix-like 的声明式闭包、锁定和 generation 回滚语义。 | [§9](../specs/self-update.md#spec-9) | U01；B07 |
| R021 | 长期运行开销低，包含可核验的路由器部署目标。 | [§7](../specs/plugins.md#spec-7)、[§22](../specs/nodes.md#spec-22)、[§23](../specs/verification.md#spec-23) | U01；B02/B05 |
| R022 | 原始 session 是真相源，预算切窗或 compact 后仍能检索并重读证据。 | [§12.1](../specs/memory.md#spec-12-1)–[§12.3](../specs/context-policies.md#spec-12-3) | U01；B12/B13 |
| R023 | 研究 LayerFS 并按最新决定在真实 Linux VM 直接用原版；WASM 状态独立实现。 | [§3.3](../research/product-positioning.md#spec-3-3)、[§10.2.1](../specs/durable-state.md#spec-10-2-1)、[§34](../specs/desktop-runtime.md#spec-34) | U01/U19；B09 |
| R024 | 人不在时仍持续自主思考、维护自己并推进事情。 | [§4](../specs/subject-runtime.md#spec-4)、[§5.1](../specs/subject-runtime.md#spec-5-1)、[§5.4](../specs/nodes.md#spec-5-4)、[§9](../specs/self-update.md#spec-9)；[§41](../specs/architecture.md#spec-41) | U01；B01/B03/B07 |
| R025 | 详细研究 OpenAI Dot、Claude Duet、Meta Muse 并说明自主性差异。 | [§3.1](../research/product-positioning.md#spec-3-1) | U01；B01 |
| R026 | 极致隐私与本地 AI 优先，数据、凭据、模型和遥测边界明确。 | [§1](../specs/architecture.md#spec-1)、[§6](../specs/subject-runtime.md#spec-6)、[§10](../specs/durable-state.md#spec-10)–[§13](../specs/inference.md#spec-13)、[§21.3](../specs/web-interface.md#spec-21-3)、[§22](../specs/nodes.md#spec-22) | U01；B01/B10/B11/B12/B19 |
| R027 | 可详细配置 API 的并发、上下文及真实共享资源预算。 | [§13.1](../specs/quota-resources.md#spec-13-1)、[§13.3](../specs/quota-resources.md#spec-13-3)、[§13.6](../specs/inference.md#spec-13-6)；[§41](../specs/architecture.md#spec-41) | U01；B19/B20 |
| R028 | 所有受控推理进入按紧急程度和公平性调度的全局资源入口。 | [§13.1](../specs/quota-resources.md#spec-13-1)、[§13.4](../specs/quota-resources.md#spec-13-4)；[§41](../specs/architecture.md#spec-41) | U01；B20 |
| R029 | 用 Rust edition 2024 实现最小核心控制功能，实际工具链另行锁定。 | [§2](../specs/architecture.md#spec-2)、[§7](../specs/plugins.md#spec-7)、[§22](../specs/nodes.md#spec-22)、[§40.4](../governance/development-contract.md#spec-40-4) | U01/U59；B02 |
| R030 | 提供 Webserver 和基于 Proto-UI 的正式操作界面。 | [§7](../specs/plugins.md#spec-7)、[§21](../specs/web-interface.md#spec-21) | U01；B34 |
| R031 | 界面活跃鲜明，用布局表达状态，遵循当期设计、响应式和无障碍要求。 | [§21.1](../specs/web-interface.md#spec-21-1) | U01；B35 |
| R032 | 使用 Proto-UI 当前主线与 shadcn，每轮开发核验并 bump 最新主线。 | [§21.2](../specs/web-interface.md#spec-21-2) | U01；B34 |
| R033 | 预留迁移至 Proto-UI compiler 的适配与插桩边界。 | [§21.2](../specs/web-interface.md#spec-21-2) | U01；B34 |
| R034 | 将 DeepSeek Harness 作为执行及扩展设计的重要研究对象。 | [§3.2](../research/product-positioning.md#spec-3-2)、[§7](../specs/plugins.md#spec-7) | U01；B02 |
| R035 | 接入 iLinkBot、QQ 官方机器人、NapCat/napcat-cli、飞书等渠道。 | [§15.3](../specs/channel-adapters.md#spec-15-3)、[§16](../specs/channel-adapters.md#spec-16) | U01；B22 |
| R036 | 默认所有可达用户均可对话，管理员权限由人类明确设置。 | [§2](../specs/architecture.md#spec-2)、[§16](../specs/channel-adapters.md#spec-16) | U01；B22 |
| R037 | 子 Agent 能低开销递归搜索和深研，并在合适时收敛。 | [§5](../specs/subject-runtime.md#spec-5)、[§13](../specs/inference.md#spec-13)、[§14.5](../specs/model-children.md#spec-14-5) | U01；B18 |
| R038 | 多方案分支探索与全局并发上限同时成立，不发生父子资源死锁。 | [§10.2](../specs/durable-state.md#spec-10-2)、[§13.4](../specs/quota-resources.md#spec-13-4)、[§14.3](../specs/research-memory.md#spec-14-3)–[§14.5](../specs/model-children.md#spec-14-5)；[§41](../specs/architecture.md#spec-41) | U01；B08/B18/B20 |
| R039 | 透传真实 backend KV slot/affinity 能力，减少多上下文切换的重复 prefill。 | [§13.2](../specs/quota-resources.md#spec-13-2)–[§13.6](../specs/inference.md#spec-13-6)；[§41](../specs/architecture.md#spec-41) | U01；B19/B20 |
| R040 | 提供 NInfer 和 Strata 各自的中英文接入、KV 配置及排障教程。 | [§25](../specs/inference.md#spec-25)、[§26](../specs/inference.md#spec-26)、[§27](../specs/verification.md#spec-27) | U01；B19 |
| R041 | 用高质量 SSH 文件、执行和 memory 工具减少本地模型的操作负担。 | [§11](../specs/remote-seats.md#spec-11)、[§12.2](../specs/memory.md#spec-12-2)–[§12.3](../specs/context-policies.md#spec-12-3)、[§18.2](../specs/plugins.md#spec-18-2)、[§18.4](../specs/cognition-units.md#spec-18-4) | U01；B11/B15/B26 |
| R042 | 深入参考 oh-my-pi、pi、DeepSeek Harness 与 cordis-rs 的可扩展性。 | [§3.2](../research/product-positioning.md#spec-3-2)、[§7](../specs/plugins.md#spec-7)、[§18](../specs/plugins.md#spec-18)；[§41](../specs/architecture.md#spec-41) | U01；B02/B03/B26/B27 |
| R043 | 用 SQLite FTS5 类设计保存原始会话及可重建全文索引，支持中文检索。 | [§12.1](../specs/memory.md#spec-12-1)、[§12.3](../specs/context-policies.md#spec-12-3) | U01；B12 |
| R044 | 核心适配多设备，保留 Android 本地运行而非仅手机访问网页。 | [§22](../specs/nodes.md#spec-22)、[§23](../specs/verification.md#spec-23) | U01；B05/B31 |
| R045 | 完成逐字复述、深入研究、规格追问和完整 SPEC，当前不开发。 | [§1](../specs/architecture.md#spec-1)、[§3](../research/product-positioning.md#spec-3)、[§28](../decisions/product-rulings.md#spec-28)、[§29](../specs/verification.md#spec-29)；原文附录与研究材料；[§41](../specs/architecture.md#spec-41) | U01/U12；B41 |

## 唯一主体与委派 R046–R054

| ID | 一句需求 | 正文落点 | 用户来源与行为对照 |
|---|---|---| --- |
| R046 | 正常选主后的同一身份只有一条顺序决策流；D11 故障分支各自串行并最终选主汇合。 | [§5.1](../specs/subject-runtime.md#spec-5-1)、[§5.4](../specs/nodes.md#spec-5-4)；[§41](../specs/architecture.md#spec-41) | U02/U20；B03/B31 |
| R047 | 主体持续思考下一步，不能以无新消息为由退成被动 Bot。 | [§5.1](../specs/subject-runtime.md#spec-5-1)、[§13.4](../specs/quota-resources.md#spec-13-4)、[§22](../specs/nodes.md#spec-22)；[§41](../specs/architecture.md#spec-41) | U02/U15；B03/B20 |
| R048 | 主体积极 handoff 探索任务，委派和等待不阻塞全局资源。 | [§5.2](../specs/subject-runtime.md#spec-5-2)–[§5.3](../specs/model-children.md#spec-5-3)、[§13.4](../specs/quota-resources.md#spec-13-4)；[§41](../specs/architecture.md#spec-41) | U02；B03/B18/B20 |
| R049 | 子 Agent 能实际开展自我改进，产出、验证和提升候选。 | [§9.1](../specs/self-update.md#spec-9-1)–[§9.2](../specs/self-update.md#spec-9-2)、[§18.5](../specs/cognition-units.md#spec-18-5) | U02；B07/B25/B36 |
| R050 | 版本、任务进度材料和实际记忆维护由子 Agent 执行。 | [§5.2](../specs/subject-runtime.md#spec-5-2)、[§9](../specs/self-update.md#spec-9)、[§10](../specs/durable-state.md#spec-10)、[§12.2](../specs/memory.md#spec-12-2)；[§41](../specs/architecture.md#spec-41) | U02；B03/B10/B12 |
| R051 | 主体仅有思考、语义 memory 和委派等极窄能力集合。 | [§5.2](../specs/subject-runtime.md#spec-5-2)–[§5.3](../specs/model-children.md#spec-5-3)；[§41](../specs/architecture.md#spec-41) | U02；B03 |
| R052 | 主体不直接操作文件、shell、SSH、Git 或 REPL，runtime 仍可持久化状态。 | [§5.2](../specs/subject-runtime.md#spec-5-2)、[§19](../specs/repl.md#spec-19)、[§23](../specs/verification.md#spec-23)；[§41](../specs/architecture.md#spec-41) | U02；B03 |
| R053 | 人格附加在稳定核心 system 之后，并按版本及轮次边界生效。 | [§6](../specs/subject-runtime.md#spec-6)、[§13.5](../specs/inference.md#spec-13-5)；[§41](../specs/architecture.md#spec-41) | U02/U20；B03/B40 |
| R054 | 人格不改变工具协议、权限、恢复和资源合同。 | [§6](../specs/subject-runtime.md#spec-6)、[§7](../specs/plugins.md#spec-7)、[§9.2](../specs/self-update.md#spec-9-2)；[§41](../specs/architecture.md#spec-41) | U02；B03/B40 |

## 后续能力与框架定位 R055–R117

| ID | 一句需求 | 正文落点 | 用户来源与行为对照 |
|---|---|---| --- |
| R055 | 外部消息几乎总是进入同一主体序列，不另起自主主循环。 | [§5.1](../specs/subject-runtime.md#spec-5-1)、[§15.1](../specs/channels.md#spec-15-1)；[§41](../specs/architecture.md#spec-41) | U03；B03/B21/B22 |
| R056 | 消息封套包含 provider、别名、稳定 ID、内容和附件来源。 | [§15.1](../specs/channels.md#spec-15-1) | U03；B22 |
| R057 | 消息 provider 是可独立扩展和替换的插件。 | [§7](../specs/plugins.md#spec-7)、[§15.3](../specs/channel-adapters.md#spec-15-3) | U03；B22/B27 |
| R058 | 模型/API provider 插件化，并参与统一能力协商与资源管理。 | [§7](../specs/plugins.md#spec-7)、[§13.1](../specs/quota-resources.md#spec-13-1)、[§13.6](../specs/inference.md#spec-13-6) | U03；B19/B27 |
| R059 | 通过联系人编号和身份插件持续认识人、维护别名及跨平台映射。 | [§15.2](../specs/contacts.md#spec-15-2) | U03；B23 |
| R060 | 声纹作为可选身份线索插件，保留置信、未知和误识别语义。 | [§15.2](../specs/contacts.md#spec-15-2)、[§20](../specs/media.md#spec-20) | U03；B23/B29 |
| R061 | C=1 时仍可保留多份上下文，计算与 KV 驻留分别管理。 | [§13.1](../specs/quota-resources.md#spec-13-1)–[§13.2](../specs/quota-resources.md#spec-13-2)；[§41](../specs/architecture.md#spec-41) | U03；B19/B20 |
| R062 | 自动算法在需求和成本适合均分时自行形成主/子集体约各半，并随负载变化适应；手动设 50/50 跑通不足。 | [§13.2](../specs/quota-resources.md#spec-13-2)–[§13.3](../specs/quota-resources.md#spec-13-3)、[§23](../specs/verification.md#spec-23) A04；[§41](../specs/architecture.md#spec-41) | U03/U16；B20 |
| R063 | 支持自动 KV 分配及主体通过语义意图调整策略。 | [§5.2](../specs/subject-runtime.md#spec-5-2)、[§13.3](../specs/quota-resources.md#spec-13-3)；[§41](../specs/architecture.md#spec-41) | U03；B20 |
| R064 | 尽可能自动发现容量，无法确认时如实显示 estimated/unknown。 | [§13.3](../specs/quota-resources.md#spec-13-3)、[§13.6](../specs/inference.md#spec-13-6)；[§41](../specs/architecture.md#spec-41) | U03；B19/B20 |
| R065 | 研究主动遗忘论文并支持研究 worker 从有限已验证种子重新开始。 | [§14.1](../specs/research-memory.md#spec-14-1)、[§14.3](../specs/research-memory.md#spec-14-3) | U03；B17 |
| R066 | research reset/semantic forget 保留原始历史，owner 或主体空间处置的真正 erase 另有协议。 | [§12.4](../specs/memory.md#spec-12-4)、[§14.2](../specs/research-memory.md#spec-14-2)–[§14.3](../specs/research-memory.md#spec-14-3) | U03/U20；B17/B32 |
| R067 | 核验 PMA 论文的方法、触发、成本、消融和局限，不夸大收益。 | [§14.1](../specs/research-memory.md#spec-14-1)、[§14.4](../specs/research-memory.md#spec-14-4) | U03；B16 |
| R068 | 主动 memory worker 按需要提醒主体或孩子，也可保持沉默且不绕 broker。 | [§13.1](../specs/quota-resources.md#spec-13-1)、[§14.4](../specs/research-memory.md#spec-14-4)；[§41](../specs/architecture.md#spec-41) | U03；B16/B20 |
| R069 | 通过正式插件控制 SSH/其他 runtime 上的 Claude Code、omp、tmux 等 Agent seats。 | [§17.1](../specs/remote-seats.md#spec-17-1)；[§41](../specs/architecture.md#spec-41) | U04；B24 |
| R070 | 能根据记忆找回已有远程任务与工作位，并现场确认身份和进度。 | [§17.1](../specs/remote-seats.md#spec-17-1)；[§41](../specs/architecture.md#spec-41) | U04；B15/B24 |
| R071 | 在既有授权内自主回答远程选项和 permission，不逐项等待人类。 | [§17.2](../specs/remote-seats.md#spec-17-2)；[§41](../specs/architecture.md#spec-41) | U04；B24 |
| R072 | 不确定时按人格和情境主动寻找可达用户渠道，再继续原任务。 | [§17.3](../specs/remote-seats.md#spec-17-3)；[§41](../specs/architecture.md#spec-41) | U04；B22/B24 |
| R073 | 支持传统 Skills、MCP 和市场生态，说明真实兼容范围。 | [§18.1](../specs/plugins.md#spec-18-1) | U04；B27 |
| R074 | 常用能力优先提供本产品原生语义插件，同时兼容 Skills/MCP。 | [§7](../specs/plugins.md#spec-7)、[§18.1](../specs/plugins.md#spec-18-1) | U04；B27 |
| R075 | 插件不仅可安装，还应被发现、学会并在合适任务中正确使用。 | [§18.2](../specs/plugins.md#spec-18-2)、[§18.5](../specs/cognition-units.md#spec-18-5) | U06；B25/B26 |
| R076 | 研究过程知识、工具检索和渐进披露，以任务评测验证设计。 | [§18.3](../specs/plugins.md#spec-18-3)、[§18.5](../specs/cognition-units.md#spec-18-5) | U06；B26 |
| R077 | 针对本地约 27B 模型的知识、推理、工具选择和状态维持分别优化。 | [§18.4](../specs/cognition-units.md#spec-18-4) | U06；B26 |
| R078 | 为未来更强模型保留替换或简化当前发现、教学和上下文机制的路径。 | [§7](../specs/plugins.md#spec-7)、[§18.3](../specs/plugins.md#spec-18-3) | U06；B26/B27 |
| R079 | Python in WASM REPL 是独立真实目标，区分 WASI 与 Pyodide 路线。 | [§19](../specs/repl.md#spec-19) | U05；B28 |
| R080 | Bun in WASM REPL 是独立具名目标，不以普通 JS 或转译器替代。 | [§19](../specs/repl.md#spec-19) | U05；B28 |
| R081 | Node in WASM REPL 是独立具名目标，不混淆 Node 执行 WASM 与自身运行于 WASM。 | [§19](../specs/repl.md#spec-19) | U05；B28 |
| R082 | REPL 可恢复，明确日志、对象与 VM 状态级别及副作用重放边界。 | [§19](../specs/repl.md#spec-19)、[§23](../specs/verification.md#spec-23)；[§41](../specs/architecture.md#spec-41) | U05；B28/B37 |
| R083 | REPL 通过插件动态安装依赖，维护 ABI、锁和环境 generation。 | [§9](../specs/self-update.md#spec-9)、[§19](../specs/repl.md#spec-19) | U05；B07/B28 |
| R084 | 原生预留实时通话、旁听课堂和持续媒体输入的插件接口。 | [§20](../specs/media.md#spec-20) | U07；B29 |
| R085 | 媒体输入仍进入同一主体，通话和听课时保留完整工具委派能力。 | [§5](../specs/subject-runtime.md#spec-5)、[§20](../specs/media.md#spec-20)；[§41](../specs/architecture.md#spec-41) | U07；B03/B29 |
| R086 | Agent 可自主组织录音、声纹及相关媒体插件的开发和改善。 | [§9](../specs/self-update.md#spec-9)、[§15.2](../specs/contacts.md#spec-15-2)、[§20](../specs/media.md#spec-20) | U07；B23/B25/B29 |
| R087 | 保持极小核心，一切业务能力插件化，并合计默认插件的实际常驻成本。 | [§7](../specs/plugins.md#spec-7)、[§22](../specs/nodes.md#spec-22) | U08；B02/B27 |
| R088 | Agent 可通过子 Agent 自主重写任何部分，包括 kernel/rescue，按代际协议交接恢复。 | [§2](../specs/architecture.md#spec-2)、[§7](../specs/plugins.md#spec-7)、[§9.1](../specs/self-update.md#spec-9-1)–[§9.2](../specs/self-update.md#spec-9-2)、[§23](../specs/verification.md#spec-23) | U09/U15；B02/B07/B36 |
| R089 | 产品是面向 personal agent 的最小框架，持续个人主体与临时 coding workers 分离。 | [§1](../specs/architecture.md#spec-1)、[§5](../specs/subject-runtime.md#spec-5)、[§7](../specs/plugins.md#spec-7)、[§17](../specs/remote-seats.md#spec-17)、[§29](../specs/verification.md#spec-29) | U11；B01 |
| R090 | WASM 是基础，获给的 root Android、Linux/Windows/macOS 屏幕、shell、应用与文件能力由正式 Computer Use/执行插件扩展，按实际权限使用。 | [§11](../specs/remote-seats.md#spec-11)、[§22](../specs/nodes.md#spec-22)、[§24](../specs/desktop-runtime.md#spec-24)；[§7](../specs/plugins.md#spec-7)、[§23](../specs/verification.md#spec-23) | U10/U57；B30/B53 |
| R091 | 参考用户称为 OpenAI Dynamic Response、官方公开资料称为 Intelligent UI 的方向，通过目标 DSL/元素说明或 UI 工具发现界面能力，生成并按需降级动态 UI。 | [§21.4](../specs/web-interface.md#spec-21-4)–[§21.9](../specs/web-interface.md#spec-21-9)、[§23](../specs/verification.md#spec-23) A25/A26 | U13；B33 |
| R092 | Agent 自主决定是否启用自动更新、是否安装具体版本及何时更新，可调整策略，无需默认询问用户。 | [§2.27](../specs/architecture.md#spec-2)、[§9.4](../specs/network-observation.md#spec-9-4)、[§23](../specs/verification.md#spec-23) A27 | U17；B07 |
| R093 | 配置问题、版本通知和网络/代理状态可作为消息进入同一主体，由 Agent 自主选择并委派处理。 | [§9.4](../specs/network-observation.md#spec-9-4)、[§15.1](../specs/channels.md#spec-15-1)、[§23](../specs/verification.md#spec-23) A27–A28 | U17；B07/B22 |
| R094 | 联网直连/代理及可用路由由 Agent 自主选择；子 Agent 执行、验证与恢复，切换不绕过全局推理记账。 | [§9.4](../specs/network-observation.md#spec-9-4)、[§13](../specs/inference.md#spec-13)、[§23](../specs/verification.md#spec-23) A28；[§41](../specs/architecture.md#spec-41) | U17；B07/B20 |
| R095 | 研究 BoxLite 作为完整 Linux runtime、持久工作区与 Computer Use 插件候选的适用性，不自动替换 WASM 基线或决定采用。 | [§8.1](../specs/wasm-home.md#spec-8-1)、[§19](../specs/repl.md#spec-19)、[§24.1](../specs/desktop-runtime.md#spec-24-1)、[§34](../specs/desktop-runtime.md#spec-34) | U18；B09/B30 |
| R096 | 能发现自身能力缺口，自主配置、安装、编写插件/外部应用并验证原阻塞任务。 | [§18.7](../specs/cognition-units.md#spec-18-7)、[§17.5](../specs/remote-seats.md#spec-17-5) | U19；B25 |
| R097 | 主/child 共享能力全集，可持久双向对话，实际执行入口按 role 区分。 | [§5.2](../specs/subject-runtime.md#spec-5-2)、[§5.5](../specs/model-children.md#spec-5-5)；[§41](../specs/architecture.md#spec-41) | U19；B03/B25 |
| R098 | NInfer/Strata/vLLM/1Cat 原生 API 与能力真实透传，按版本/传输/配置协商。 | [§13.6](../specs/inference.md#spec-13-6)–[§13.7](../specs/inference.md#spec-13-7) | U19；B19 |
| R099 | 参考 Pi Durable 设计持久任务、取消、补偿和调用重放，不止进程内 cleanup。 | [§10.6](../specs/durable-state.md#spec-10-6)；[§41](../specs/architecture.md#spec-41) | U19；B04 |
| R100 | 原版 LayerFS 放入合格真实 Linux VM，WASM 状态独立；提交与持久保证分层。 | [§10.2.1](../specs/durable-state.md#spec-10-2-1)、[§34](../specs/desktop-runtime.md#spec-34) | U19；B09 |
| R101 | Agent 可在有限空间内自主移植/安装 WASM 能力，记录真实目标与验证。 | [§18.8](../specs/wasm-tools.md#spec-18-8) | U19；B06/B25 |
| R102 | 复用 Life 身份/声纹材料，保留录音范围、候选、修订和手动管理员边界。 | [§15.2.1](../specs/contacts.md#spec-15-2-1) | U19；B23 |
| R103 | UI 提供功能与体验 brief 给 Claude Opus；细节按需展开，不强制监控大屏布局。 | [§21.1](../specs/web-interface.md#spec-21-1) | U19；B35 |
| R104 | 主体统一知道个人经历并自主选择保密或披露，不按受众屏蔽记忆或硬设亲密度。 | [§6](../specs/subject-runtime.md#spec-6)、[§12.3](../specs/context-policies.md#spec-12-3) | U20；B23/B40 |
| R105 | 灵魂可用一句或数句话表达处事哲学与自我认知，由 Agent 自主修改。 | [§6](../specs/subject-runtime.md#spec-6)、[§9](../specs/self-update.md#spec-9) | U20；B40 |
| R106 | 每次委派可任意编写 child system prompt 并选择不同模型，保留角色与资源合同。 | [§5.2](../specs/subject-runtime.md#spec-5-2)、[§6](../specs/subject-runtime.md#spec-6)；[§41](../specs/architecture.md#spec-41) | U20；B03/B40 |
| R107 | 对话 child 有限情况下可直接发送，借鉴 OMP 的归属/成果/防重复机制。 | [§5.3](../specs/model-children.md#spec-5-3)；[§41](../specs/architecture.md#spec-41) | U20；B03 |
| R108 | 外部消息默认最高紧急可打断 child；停滞时询问主体决定临时免打扰并在结束后处理队列。 | [§13.8](../specs/subject-runtime.md#spec-13-8) | U20；B21 |
| R109 | Agent 自设垃圾过滤，通配符误屏蔽所有消息时可从独立健康证据自主发现修复。 | [§13.8](../specs/subject-runtime.md#spec-13-8) | U20；B21 |
| R110 | 更新优先承担磁盘成本、允许 break change；旧副本运行，迁移烟测后最后交接自身，插件化。 | [§9.5](../specs/self-update.md#spec-9-5)；[§41](../specs/architecture.md#spec-41) | U20；B07/B37 |
| R111 | 文字/录音/屏幕帧默认永久；空间压力下主体可自主归档自有存储或遗忘，插件化。 | [§12.4](../specs/memory.md#spec-12-4) | U20；B32 |
| R112 | 无人时可长期持续投入自主兴趣，有限 child/阶段不构成兴趣的统一寿命上限。 | [§14.6](../specs/research-memory.md#spec-14-6) | U20；B18 |
| R113 | 在线优先，每个连通域一个统筹主节点；可访问 LLM 的孤岛独立演进，恢复连通后选主、停止旧主体并合并记忆任务与真实效果。 | [§5.4](../specs/nodes.md#spec-5-4)、[§22](../specs/nodes.md#spec-22) | U20/U60/U61；B31/B53 |
| R114 | 首个实机基线为用户 Tailnet Qwen3.8-27B 四流 128K，133120 是冗余。 | [§22.1](../specs/nodes.md#spec-22-1)；[§41](../specs/architecture.md#spec-41) | U20/U21；B19/B20 |
| R115 | 深入参考 Codex budget compact 与 OMP snapcompact，原始历史、任务续接和 KV 成本分别处理。 | [§12.3.1](../specs/context-policies.md#spec-12-3-1)–[§12.3.2](../specs/context-policies.md#spec-12-3-2) | U21；B13/B14 |
| R116 | budget compact 与 snapcompact 均须正式实现为独立可安装/更新/卸载/替换插件，视觉能力限制影响启用而非交付范围。 | [§12.3.1](../specs/context-policies.md#spec-12-3-1)–[§12.3.2](../specs/context-policies.md#spec-12-3-2)、A45 | U22；B13/B14/B27 |
| R117 | 框架／产品名与实例姓名分离；初始化时由用户命名或让 Agent 自主取名，不强制统一称呼。 | [§1](../specs/architecture.md#spec-1)、[§6](../specs/subject-runtime.md#spec-6)、[§21](../specs/web-interface.md#spec-21)、A46 | U24；B40 |

## 命名、首版自开发与插件补充 R118–R124

| ID | 一句需求 | 正文落点 | 用户来源与行为对照 |
| --- | --- | --- | --- |
| R118 | 框架名应有亲切的中文意象、参考 dots/Duet/Muse；最终正式定名 Shenko（深刻），旧候选仅保留历史。 | [§1](../specs/architecture.md#spec-1)、[§21](../specs/web-interface.md#spec-21)、[§39](design-provenance.md#spec-39)；原文 U25–U31/U39 | U25–U31/U38/U39；B40 |
| R119 | 不要 slogan 或解释性品牌副标题，让布局与设计表达。 | [§2](../specs/architecture.md#spec-2)、[§21](../specs/web-interface.md#spec-21) | U30；B35/B40 |
| R120 | 第一版部署开发者机器即能接 NapCat，控制现有 tmux 中 Claude Code/OMP/其他已适配 Agent，继续开发、验证并升级自身。 | [§9](../specs/self-update.md#spec-9)、[§17](../specs/remote-seats.md#spec-17)、[§29](../specs/verification.md#spec-29)、[§35](../specs/self-update.md#spec-35) | U32；B36 |
| R121 | 插件明确标示沙箱重置/回退/恢复范围和外部副作用，真实调用绑定具体边界。 | [§10](../specs/durable-state.md#spec-10)、[§36](../specs/durable-state.md#spec-36)；[§41](../specs/architecture.md#spec-41) | U33；B37 |
| R122 | 大量消息及插件注入需要明确时序、可新增 hook 与 API 先后兼容，可参考 Fabric。 | [§37](../specs/context-policies.md#spec-37)；[§41](../specs/architecture.md#spec-41) | U34；B38 |
| R123 | cordis-rs（dshbox/cordis-rs）为指定插件主干；注入设计只扩展 API 兼容与时序；U45 已纠正此前拼写。 | [§7](../specs/plugins.md#spec-7)、[§37.1](../specs/context-policies.md#spec-37-1)；[§41](../specs/architecture.md#spec-41) | U35/U45；B02/B38 |
| R124 | 参考 OMP 让 Agent 查自身代码/手册/设计哲学，回答自身问题并辅助写插件；自解释作为默认内置、可替换插件。 | [§18](../specs/plugins.md#spec-18)、[§35.3](../specs/plugins.md#spec-35-3)、[§38](../specs/cognition-units.md#spec-38)；[§41](../specs/architecture.md#spec-41) | U36；B39 |

## 来源、执行合同、界面、持续作品与首启 R125–R139

| ID | 一句需求 | 正文落点 | 用户来源与行为对照 |
| --- | --- | --- | --- |
| R125 | 所有想法有来源 Ref，并用于比较所采用上游行为的完整程度，不能只致谢或列项目名。 | [§31](research-sources.md#spec-31)–[§32](traceability.md#spec-32)、[§38](../specs/cognition-units.md#spec-38)–[§39](design-provenance.md#spec-39) | U37；B41/B39 |
| R126 | 开发 Agent 高度自主，合理默认、实现、验证、修复和范围内集成连续推进，不默认留人类 gate。 | [§29](../specs/verification.md#spec-29)、[§40](../governance/development-contract.md#spec-40) | U37；B41 |
| R127 | 界面风格与美观参考 Magpie，用具体来源画面、状态与交互质量比较。 | [§21.10](../specs/web-interface.md#spec-21-10)、[§39](design-provenance.md#spec-39)–[§40](../governance/development-contract.md#spec-40) | U37；B35 |
| R128 | 正式产品/框架名 Shenko（深刻），记录用户给出的命名来源和读音，无 slogan，实例名称独立。 | [§1](../specs/architecture.md#spec-1)、[§6](../specs/subject-runtime.md#spec-6)、[§21](../specs/web-interface.md#spec-21)、[§39](design-provenance.md#spec-39) | U39；B40 |
| R129 | 以 Pi Durable 固定 SPEC 为参照提高具体程度，给出实现可据以执行的状态、接口、原子边界、失败交错与验收；真正未定的产品问题可继续问。 | [§41](../specs/architecture.md#spec-41)；关联 [§5](../specs/subject-runtime.md#spec-5)、[§9](../specs/self-update.md#spec-9)、[§10](../specs/durable-state.md#spec-10)、[§13](../specs/inference.md#spec-13)、[§17](../specs/remote-seats.md#spec-17)、[§35](../specs/self-update.md#spec-35)–[§40](../governance/development-contract.md#spec-40) | U40；B03/B04/B20/B24/B37/B38/B39/B41/B56 |
| R130 | 以插件观测/估计供应商周期额度，影响模型、effort、主体平均准入次数与任务预算，让主体感知余额；追求跨周期接近用完，本地 AI 优先，云额度后排。 | [§42](../specs/quota-resources.md#spec-42)；[§13](../specs/inference.md#spec-13)、[§41](../specs/architecture.md#spec-41) | U41；B42/B19/B20 |
| R131 | 以 Magpie 和 Alma 的流式体验为参照，正文/滚动/选区/表单稳定，真实流与恢复不抖动、不重复、不串 attempt。 | [§21.11](../specs/web-interface.md#spec-21-11)、[§39](design-provenance.md#spec-39)–[§40](../governance/development-contract.md#spec-40) | U41；B35/B33 |
| R132 | 默认 WebUI 仅主体时间线与输入，append/steer 进入同一持久 thought loop；其他能力按需打开，不另建 chat agent。 | [§21.12](../specs/web-interface.md#spec-21-12)、[§41.18](../specs/web-interface.md#spec-41-18) | U42；B35/B03/B38 |
| R133 | 所有配置可经对话发现和修改并有查看/编辑面板；通过可组合能力和真实执行/观测/恢复实现可靠改变，不强制五阶段。 | [§43.1](../specs/configuration-playground.md#spec-43-1)、[§43.3](../specs/configuration-playground.md#spec-43-3)、[§45](../specs/cognition-units.md#spec-45)；[§9](../specs/self-update.md#spec-9)、[§41](../specs/architecture.md#spec-41) | U43/U44/U46/U47；B43/B07/B41/B45 |
| R134 | 持久 Playground 容纳兴趣/交流产生的 artifacts 和有后端、界面、数据的 Intelligent UI；真实状态变化能被主体感知，支持版本、恢复与长期运行。 | [§43.2](../specs/configuration-playground.md#spec-43-2)、[§43.3](../specs/configuration-playground.md#spec-43-3)；[§21](../specs/web-interface.md#spec-21) | U43；B44/B33/B37 |
| R135 | 定义第一次打开需要什么；以最少推理/管理员输入和可用默认完成真实 WASM、存储、资源与唯一持续主体的 bootstrap，失败可恢复。 | [§44](../specs/plugins.md#spec-44)；[§7](../specs/plugins.md#spec-7)、[§41](../specs/architecture.md#spec-41) | U44；B46/B03/B20 |
| R136 | 不同直觉/意识可以原子化安装，弥补没想到；thought loop 也从任意输入即时产生 awareness，有价值再自主沉淀，不能只限静态目录或全塞总插件。 | [§45](../specs/cognition-units.md#spec-45)；[§18](../specs/plugins.md#spec-18)、[§37](../specs/context-policies.md#spec-37)、[§38](../specs/cognition-units.md#spec-38)、[§43](../specs/configuration-playground.md#spec-43)–[§44](../specs/plugins.md#spec-44) | U44/U46/U47/U54/U56；B45/B52/B26/B38/B43 |
| R137 | 将 colibri-rs 笔误统一更正为 dshbox/cordis-rs，保留历史原话、撤去错误身份待确认项。 | [§2](../specs/architecture.md#spec-2)、[§7](../specs/plugins.md#spec-7)、[§37](../specs/context-policies.md#spec-37)、[§39](design-provenance.md#spec-39)；[§33](user-sources.md#spec-33) U45 | U45；B02/B38 |
| R138 | 感知、直觉、Skill 与插件代码均能原子化安装/查阅；NapCat 等 provider 自动安装所依赖单元，插件含版本依赖，直觉依赖可笼统或精确。 | [§9](../specs/self-update.md#spec-9)、[§18](../specs/plugins.md#spec-18)、[§45](../specs/cognition-units.md#spec-45) | U48；B47/B25/B26/B27/B45 |
| R139 | 说明能力/规则/流程的顶层 Why、目标与适用条件，帮助模型在工具失败或特殊情况中自主调整；awareness、Skill、自解释及开发规范均体现，避免生成新的固定说明手续。 | [§18.2](../specs/plugins.md#spec-18-2)、[§38.4](../specs/cognition-units.md#spec-38-4)、[§40.9](../governance/development-contract.md#spec-40-9)、[§45.2](../specs/cognition-units.md#spec-45-2)–[§45.5](../specs/cognition-units.md#spec-45-5) | U49；B48/B26/B39/B41/B45 |

## 完整开发交接 R140–R141

| ID | 一句需求 | 正文落点 | 用户来源与行为对照 |
| --- | --- | --- | --- |
| R140 | 整理并建立包含完整 SPEC、WHY、README、AGENTS.md 的仓库；研究近期上游治理，交接朋友的 Claude Opus 5.5 通过真实 /goal、ultracode 与并行子 Agent 完整实施，自主配置本人 GitHub/gh、持续提交更新 PR，不等待 review 才继续，以完整证据定义结束。 | [§40](../governance/development-contract.md#spec-40)、[§46](../governance/COMPLETION.md#spec-46)；GOAL.md 与 docs/governance | U50；B49/B41/B48 |
| R141 | 朋友的开发 Agent 使用实际 bypassPermissions 启动参数，在自己的 fork 自主开发、合并、重构、配置工具和 CI、并行委派与持续验证，向上游提交和更新 PR；上游由实际 GitHub 权限处理，交接以目标、Why、证据和接续为主。 | [§46](../governance/COMPLETION.md#spec-46)；AGENTS.md、GOAL.md 与 docs/governance/CLAUDE.md | U51；B49/B41 |


## Provider 协议与 SDK 演进 R142

| ID | 一句需求 | 正文落点 | 用户来源与行为对照 |
| --- | --- | --- | --- |
| R142 | 跟进 Claude API 2026-10-21 等真实版本变化，以插件化的 provider/model/API/SDK/gateway 合同处理采样、消息格式、鉴权、额度字段与升级；主体能感知变化并自主迁移，保留本地原生能力、原始历史与在途请求事实。 | [§13](../specs/inference.md#spec-13)、[§41.8](../specs/context-policies.md#spec-41-8)、[§42](../specs/quota-resources.md#spec-42)、[§45](../specs/cognition-units.md#spec-45)、[§47](../specs/inference.md#spec-47)；docs/research/claude-api-2026-10-21.md | U53；B50/B19/B38/B42/B47 |

## 持续察觉、跨设备自我与开发哲学 R143–R147

| ID | 一句需求 | 正文落点 | 用户来源与行为对照 |
| --- | --- | --- | --- |
| R143 | 主体默认持续接续；只由自己主动选择最多 600 秒的语义等待，同一主模型不断根据前文与新增上下文轮询退出，期限不是时间唤醒；child 可 sleep，真实资源/挂起状态另记。 | [§5.1](../specs/subject-runtime.md#spec-5-1)、[§41.5](../specs/subject-runtime.md#spec-41-5) | U55；B51/B03 |
| R144 | 时间、未竟义务、重复/无聊、维护项目、SNS/IM 与邮件察觉各自原子化并可自解释组合；dreaming 仅社区可选；任意输入形成的 awareness 可即时采用、沉淀、改写或停用，来源与权限不升格。 | [§18](../specs/plugins.md#spec-18)、[§38](../specs/cognition-units.md#spec-38)、[§45](../specs/cognition-units.md#spec-45) | U54/U56；B52/B45/B47/B39 |
| R145 | 多设备同步同一自我，较晚新节点撤销独立身份并加入可信既有身份；使用已授权 SSH Key/内网/Tailnet/ZeroTier 与真实设备能力，自主修复环境；异构模型/provider/隐私与实际可达能力决定 child 放置及递归选主，分区经历保留。 | [§5.4](../specs/nodes.md#spec-5-4)、[§9.4](../specs/network-observation.md#spec-9-4)、[§11](../specs/remote-seats.md#spec-11)、[§22](../specs/nodes.md#spec-22)、[§38](../specs/cognition-units.md#spec-38)、[§44.1](../specs/plugins.md#spec-44-1) | U57/U60/U61；B53/B31/B11/B30/B39 |
| R146 | 所有开发者必读 shenko-dev 哲学 Skill，按需采用兼容 OpenAI/Anthropic 的原子开发 Skill；CLAUDE.md 符号链接到 AGENTS.md，正文单源；完全自主开发、无额外人类 gate，一个增量不新增十个或更多测试。 | [§40.5](../governance/development-contract.md#spec-40-5)、[§46](../governance/COMPLETION.md#spec-46) | U55/U58；B54/B41/B49 |
| R147 | 智能完全失效时仍以确定性控制记录系统日志，并供持有 credentials 的外部工具提取原始记忆/经历；可从磁盘镜像在其他节点恢复或合并，准确保留解密、介质和未决副作用边界。 | [§22.2](../specs/nodes.md#spec-22-2)；[§5.4](../specs/nodes.md#spec-5-4)、[§10](../specs/durable-state.md#spec-10)、[§38](../specs/cognition-units.md#spec-38)、[§44](../specs/plugins.md#spec-44) | U62；B55/B04/B05/B37/B56 |

<!-- /migrated-contract: SPEC:32 -->

<a id="spec-56"></a>
<!-- migrated-contract: SPEC:56 -->
# 56 验收追踪矩阵 / Acceptance Traceability Matrix

[§32](traceability.md#spec-32) 的需求矩阵追踪到 R 编号；本节把 [§51](../specs/cognition-units.md#spec-51)–[§55](../specs/verification.md#spec-55) 的认知与自主条款映射到当前实现面与证据，作为下一轮验收的对照表。状态由证据目录（`build/`）与原生验证结果判定，不由「代码存在」判定。

| 条款 | 实现面 | 验收状态 | 备注 |
|---|---|---|---|
| [§51.1](../specs/cognition-units.md#spec-51-1) 认知原子 | `shenko-store` cognition/context_notes、`shenko-subject` loop_runner | 部分 | 一等态度/理由指针与「旧版本数量+最新修改时间」投影尚未落地为运行时记录 |
| [§51.2](../specs/cognition-units.md#spec-51-2) awareness=能力 | `shenko-awareness-core`、watcher notice | 部分 | 文档式 awareness 已退役；能力宿主存在，原子化拆分未全部完成 |
| [§51.4](../specs/cognition-units.md#spec-51-4) 可学习/版本化根 | `shenko-store` cognitive_root、`shenko-update`、PR191 rollback | 部分 | 追加节点回滚已验证；线性内存/执行栈/对象图纳入根未实现 |
| [§51.7](../specs/cognition-units.md#spec-51-7) 遗忘与归档 | `forgetting` 宏包、`shenko-store` tombstone | 未开始 | 阶段顺序与信号权重已有 SPEC，运行时状态机未实现 |
| [§51.8](../specs/cognition-units.md#spec-51-8) 能力依赖 | `cap.*` 契约、[§45.10](../specs/cognition-units.md#spec-45-10) DEP01/DEP02 | 部分 | 依赖写能力的合同在，多提供者共存与按原子能力判冲突未验证 |
| [§52](../specs/channels.md#spec-52) 通道协议 | `shenko-channel-adapters`、`shenko-channel-napcat(-link)`、`shenko-ui` | 部分 | NapCat 实测通；SnowLuma/官方 QQ/Discord 适配未全部落地 |
| [§53](../specs/model-children.md#spec-53) 子代理命名 | `ChildIdentity`、journal 署名 | 部分 | 命名字段存在，自主选名与改名 revision 未全部走通 |
| [§54](../specs/architecture.md#spec-54) 自主设计原则 | 见 [§55](../specs/verification.md#spec-55) 各节 | 见 [§55](../specs/verification.md#spec-55) | 原则已写，本矩阵把每项落成可判定 |
| [§55.1](../specs/verification.md#spec-55-1) 自主性 | 持续思考、缺口自补、自主修复、拒绝代替提问 | 未验收 | 缺无提示继续与缺口自补的真实运行证据 |
| [§55.2](../specs/verification.md#spec-55-2) 无模型边界 | 投影/适配层隔离 | 待审 | 需审计内核/账本中的模型特定假设 |
| [§55.3](../specs/verification.md#spec-55-3) 插件化 | 能力契约、ChannelPlugin、版本化根 | 部分 | 安装/替换/学习/插拔/回滚五条各需一条原生证据 |
| [§55.4](../specs/verification.md#spec-55-4) 演进 | awareness 原子、perception 融合、态度驱动、效能测量 | 部分 | 原子拆分与多源融合有基础；态度驱动行为与留出场景测量未达标 |

验收规则：任何一行的「状态」只能由指向 `build/` 下对应原生验证目录的真实证据支撑；「代码存在」或「有测试文件」不算验收。未达标项在 [§55](../specs/verification.md#spec-55) 对应子节有验收定义，按定义补齐后才可改判。

<!-- /migrated-contract: SPEC:56 -->

<a id="spec-57-11"></a>
<!-- migrated-contract: SPEC:57.11 -->
## 57.11 需求

| R148 | 主体角色框架由内容单元提供，harness 不再把角色文本写死为编译期常量；主体可按既有内容修订路径提出新版本。 | [§57.1](../specs/subject-runtime.md#spec-57-1) | U64；B57 |
| R149 | 角色框架按能力契约解析：没有提供者时使用编译回退；两个不同单元同时提供时报告歧义而不替主体选择。 | [§57.1](../specs/subject-runtime.md#spec-57-1) | U64；B57 |
| R150 | 空闲是感知而不是运行时命令：没有新外部输入时主体收到一次由内容单元措辞的观察。 | [§57.2](../specs/subject-runtime.md#spec-57-2) | U64；B57 |
| R151 | 注意力预算与可见来源由主体的注意力政策决定；harness 只强制防饿死与批处理上界。 | [§57.3](../specs/subject-runtime.md#spec-57-3) | U64；B58 |
| R152 | 未被主体处置的输入默认进入 deferred 而不是自动确认，并在后续 S0 持续可见直到被处置或被政策放任。 | [§57.3](../specs/subject-runtime.md#spec-57-3) | U64；B58 |
| R153 | 注意力暂停真正生效：被暂停的来源不进入当前窗口，事实保留，review_after 到期自动复审。 | [§57.3](../specs/subject-runtime.md#spec-57-3) | U64；B58 |
| R154 | 通道插件拥有自己的行内表面语法与平台渲染，内核回复动词保持不变。 | [§57.4](../specs/channels.md#spec-57-4) | U64；B59 |
| R155 | 平台事件（撤回、入群、离开、戳一戳）是可感知的观察，进入主体输入序列并保留来源。 | [§57.4](../specs/channels.md#spec-57-4) | U64；B59 |
| R156 | 通道能力声明必须可达：只有 describe 能实际产出的动作才算该通道的能力。 | [§57.4](../specs/channels.md#spec-57-4) | U64；B59 |
| R157 | 真实流式通路接通：chunk 先落盘再广播、控制行不外泄、committed-only 发布不变；没有生产者时诚实报告不可用。 | [§57.5](../specs/web-interface.md#spec-57-5) | U64；B60 |
| R158 | 主体能通过专用原生任务创建、迭代、暂存、发布与回滚作品，作品事件进入同一 inbox。 | [§57.6](../specs/configuration-playground.md#spec-57-6) | U64；B61 |
| R159 | 动态界面文档由真实组件渲染并按能力降级，不以 JSON 调试视图充当界面。 | [§57.6](../specs/configuration-playground.md#spec-57-6) | U64；B60 |
| R160 | 界面保留诚实词汇与阅读稳定性；浅色偏好、屏幕阅读器宣布与单一样式来源是正式要求。 | [§57.7](../specs/web-interface.md#spec-57-7) | U64；B60 |
| R161 | 主体接口不因缺少派发器而被静默标记为不可用，缺口以可安装能力的诚实记录出现。 | [§57.8](../specs/subject-runtime.md#spec-57-8) | U64；B62 |
| R162 | 自改留下带理由的态度记录，回滚按只增版本链执行，旧字节不覆盖、外部副作用不撤回。 | [§57.9](../specs/cognition-units.md#spec-57-9) | U64；B62 |
| R163 | 能力契约优先于包名：多个提供者可共存，运行时切换提供者不需要重启。 | [§57.9](../specs/cognition-units.md#spec-57-9)、[§57.10](../specs/self-update.md#spec-57-10) | U64；B62 |
| R164 | 不新增人工审批、产品 allowlist 或静态 hard deny；真实账号、设备与平台限制按事实记录。 | [§57.9](../specs/cognition-units.md#spec-57-9) | U64；B62 |
| R165 | 主体与 child 使用同一套能力目录与插件体系，child 角色框可由主体修订。 | [§57.10](../specs/self-update.md#spec-57-10) | U64；B57；B63 |

<!-- /migrated-contract: SPEC:57.11 -->
