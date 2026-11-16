# Shenko 文档索引

## 权威关系

最新明确用户决定控制目标；[领域合同](specs/architecture.md)定义行为及其 Why，当前源码和真实运行证据说明实现状态。根 `SPEC.md`、`WHY.md` 已退役为导航，不能作为内容来源。实现 Agent 从本索引进入具体文档，引用实际标题/锚点，不再写“完整合同见 SPEC §…”。

[内核合同](specs/kernel.md)规定九条不变量，回答哪些规则属于内核、哪些是默认安装并可以替换的参考插件；其余领域合同各自在开头标明相对内核的定位。既有领域义务保持有效，不能通过新内核文档的优先级减损或改写。结构差距与代码的收敛步骤列在[内核合同的差异一节](specs/kernel.md#与现行合同的差异)和[内核收敛计划](development/kernel-convergence.md)。

[迁移决定](decisions/0005-domain-contract-migration-2026-11-11.md)说明保全与边界；[机器映射](contract-migration.json)连接旧章节和现行目标。原编号仅为稳定追踪标识，不要求读者往返单体规格。历史归档不是当前指令。

## 当前入口

- [内核合同](specs/kernel.md)：先读这一份。九条不变量，每条带意图、前提、可观察效果、灵活边界、失效条件、来源，以及当前实现状态。
- [内核九条与参考插件](decisions/0007-kernel-and-reference-plugins-2026-11-11.md)：上述合同的来源与边界。
- [内核收敛计划](development/kernel-convergence.md)：代码向内核合同收敛的步骤；[依赖边界基线](state/kernel-boundary.json)由 `scripts/check_kernel_boundary.py` 检查，只查结构。
- [AGENTS](../AGENTS.md) 与 [shenko-dev](../.agents/skills/shenko-dev/SKILL.md)：开发哲学与协作。
- [实现地图](IMPLEMENTATION_DOCS_MAP.md)：调用点与实现缺口，不冒充运行验收。
- [可选外部历史研究](specs/external-history.md)：Obelisk 查询、完整原文归档、可追溯引用；[实施 TODO 与验收](development/obelisk-history.md)。
- [完整交付合同](governance/COMPLETION.md)：正式范围及证据等级。
- [需求索引](requirements/README.md)：165 项需求、63 组行为、59 条来源、236 条具名验收。
- [用户原话](requirements/user-sources.md)、[裁决记录](decisions/product-rulings.md)、[追踪矩阵](requirements/traceability.md)、[设计来源](requirements/design-provenance.md)。
- [覆盖账本](state/coverage.json)与[运行证据](evidence/)：候选及验证状态。
- [历史归档](history/)：不可用旧方法覆盖新的明确决定。
- [默认元感知与元 awareness 接续](decisions/0006-meta-perception-continuation-2026-11-11.md)：Issue #279 摘要的工程合同；原始用户决定全文尚未取得，实现与模型效果仍需各自验收。

## 领域合同

按[领域与拥有者](IMPLEMENTATION_DOCS_MAP.md#领域合同)选择本次修改涉及的合同；正文、接口、失败和恢复一起阅读。认知单元可直接进入[为何分开安装](specs/cognition-units.md#why-6)、[安装与依赖](specs/cognition-units.md#spec-45-10)、[元概念与版本](specs/cognition-units.md#spec-51)或[有限沙箱比较](specs/cognition-units.md#measuring-a-unit-the-finite-sandbox-comparison)。长篇条款按问题查阅，不把每份全文都变成每次修改的前置阅读。

## Why 按领域阅读

完整的 [Why 导航](IMPLEMENTATION_DOCS_MAP.md#why-按领域阅读)与领域列表放在同一张地图。每份合同先说明意图与边界，再链接细节；修改某条行为时读取其完整条款和相关验收，不减少已有义务。

## 章节迁移导航

[旧章节到现行条款的完整映射](IMPLEMENTATION_DOCS_MAP.md#章节迁移导航)保留每个入口；[机器映射](contract-migration.json)保存原件、块与哈希。旧章节编号是来源身份，不能代替现行合同。

## 现行合同清单

[contracts.json](contracts.json) 枚举现行合同文档，消费者读取每份文档全文，包括迁移标记外的新条目。新增合同文档时同步此清单与机器索引。历史迁移映射只证明原件保全和去向，不规定未来合同边界，也不要求新条目虚构旧章节。
