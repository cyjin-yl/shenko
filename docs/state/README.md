# 当前状态地图

这个目录记录**当前状态**，不记录历史快照与进度板。

| 文件 | 内容 | 维护者 |
| --- | --- | --- |
| [coverage.json](coverage.json) | 147 项需求逐项的规格状态、实现状态、证据状态、验收 case 归属、阻塞与说明 | 验证与证据切片 |

阅读方式：

- `specification_state` 说明需求在 SPEC 中的存在形式；`implementation_state` 说明源码里的实现程度；`evidence_state` 说明**真实运行证据**是否存在。三者是不同事实，不互相替代。
- `satisfied` 只有在有真实证据引用时才为真。源码存在、类型齐备、模拟通过都不构成满足。
- `acceptance_case_ids` 指向 [../requirements/acceptance-cases.json](../requirements/acceptance-cases.json) 中的具名验收场景；需求没有具名 case 时仍需补足验证，条件不适用要保留依据，不能删项提高比例。

历史上的 goal 状态快照（2026-10-27）与旧进度板、22 张社区任务卡已按 §48.5 归档到 [../history/progress-20261101/](../history/progress-20261101/) 与 [../history/task-cards-20261101/](../history/task-cards-20261101/)，只保留来源，不是现行状态。

当前源码的接线程度（已接线 / 仅源码 / 未实现）按实现范围记录在 [../IMPLEMENTATION_DOCS_MAP.md](../IMPLEMENTATION_DOCS_MAP.md)；真实运行证据在 [../evidence/](../evidence/README.md)。

更新保留可恢复事实，不记私有思维链或凭据。接续规则见 [../governance/CONTINUATION.md](../governance/CONTINUATION.md)。
