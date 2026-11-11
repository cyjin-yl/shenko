# 需求、行为、来源和验收索引

现行规格已分域到 [docs/specs](../specs/)，从 [文档索引](../INDEX.md) 进入具体合同与 Why。这里有 **165 项 requirements、63 组 behaviors、59 条 sources、236 条具名 acceptance cases**；这些是真实索引数量，不是已完成数量。

schema 2 的每条记录以 `source_path`、`source_markdown_line`、`source_row` 绑定当前具体文档。`migration_source` 保留原路径、行、原文与源提交，不能作为现行链接。`contract_manifest` 给出完整迁移保全映射；顶部 `spec_path` 仅为导航入口，不再表示一份包含全部表格的文件。旧 ID、原话和未完成义务均保留。

[coverage](../state/coverage.json) 记录各义务的实现与证据状态。空数组不代表无需验证；不能删项提高比例，不能以文档存在代替运行结果。[文档检查](../../scripts/check_docs.py) 只验证文档完整性。保持当前来源行与源文档一致，历史证据不重写。

R 描述用户要求，B 说明行为，Ref 定位证据，case 给出可验证情境。遵循 [shenko-dev](../../.agents/skills/shenko-dev/SKILL.md)，复用有效证据，以相称的检查解决实际疑问。
