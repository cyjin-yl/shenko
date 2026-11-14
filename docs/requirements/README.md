# 需求、行为、来源和验收索引

现行规格已分域到 [docs/specs](../specs/)，从 [文档索引](../INDEX.md) 进入具体合同与 Why。这里有 **165 项 requirements、63 组 behaviors、59 条 sources、236 条具名 acceptance cases**；这些是真实索引数量，不是已完成数量。

schema 3 的每条记录以 `source_path`、`source_anchor` 和记录自身的 R/B/Ref/`case_id` 绑定当前定义。锚点必须是文档里唯一的显式 `<a id="…">`，语义 ID 必须在现行合同中只有一个定义，并由这个锚点拥有。`source_row` 保留完整原行，在解析出的语义身份处核对，不再按行号寻找；在表格上方插入无关内容不会改变身份。`source_heading_path` 保留描述性上下文，不参与定位。

原生 `shenko-evidence` 逐份解析清单中的完整文档，再合并 R/B/case 定义并解析跨文档引用；重复语义 ID、没有显式归属锚点或归属锚点在本篇内重复，均拒绝输入。围栏规则与 Python `outside_fences` 一致：开头最多三个空白字符、至少三个反引号或波浪号；只有同一字符、长度不小于开围栏且尾部仅有空白的行才能关闭。围栏内的表格与锚点不进入索引，文档边界重新开始围栏、表格和锚点状态。报告直接保留定义的文档与锚点，当前行号只用于错误诊断。

`migration_source` 保留原路径、行、原文与源提交；`reference_migration` 保留此次切换之前的索引行号与原始实现引用。这些字段只作来源保全，不能作为现行链接或定位失败时的回退。`contract_manifest` 是完整现行文档清单；迁移保全映射另见 [contract-migration.json](../contract-migration.json)。顶部 `spec_path` 仅为导航入口。旧 ID、原话、状态和未完成义务均保留。

[coverage](../state/coverage.json) 使用 schema 2，记录各义务的实现与证据状态。coverage 和验收索引的 `implementation_refs` 共用一种 `仓库相对路径#身份` 形式，由 [解析器](../../scripts/doc_refs.py) 验证实际声明、唯一性和归属：

- Rust：`path.rs#Type::method`；字段用 `Type.field`，与同名方法保持不同身份；内联模块用 `module::symbol`，trait 实现用 `<Type as Trait>::method`。trait 名按该实现的实际源码限定名书写。宏生成、动态名称或同名多定义无法唯一解析时明确失败，不猜选某个配置分支。
- TypeScript/TSX、JavaScript：`path.tsx#Component`、`Class::method`；Node 测试用 `path.test.mjs#test::完整字面测试名`。重载或重复定义不能被悄悄选成第一项。
- Python：`path.py#Class::method`，以标准库 AST 定位，字符串和注释中的声明不算目标。
- HTML：`index.html#id:app`、唯一的 `element:html` 或 `script:inline`；CSS：`app.css#selector::root::property:--radius`，选择器或属性归属重复时失败。
- JSON：`path.json#/object/key` 使用 RFC 6901 pointer；Markdown 合同使用显式锚点，认知单元使用其 YAML frontmatter 中唯一的 `unit:单元ID`。

Rust/TypeScript/JavaScript 使用 tree-sitter 的原生语法树，CSS 使用 tinycss2，frontmatter 使用 PyYAML；[requirements-ci.txt](../../scripts/requirements-ci.txt) 钉住支持版本。缺少依赖、语言不支持、目标缺失或身份有歧义都产生错误。裸文件名、`file:line`、旧 `file::Symbol` 写法、子串命中和文件存在都不能代替当前身份；旧字面引用完整保留在迁移来源里。解析器只证明追踪目标存在，不证明行为正确。

要求两个独立门槛：`python3 scripts/check_docs.py` 核对完整现行合同、全部索引和实现身份；`python3 scripts/check_docs.py --verify-migration` 在同样的现行检查之外，核对原始冻结迁移块的保全。新增合同放在冻结块之外并加入现行清单与索引，两个门槛都会包含新增义务；迁移检查不能代替现行检查。父级在安装上述固定依赖后运行 `python3 scripts/doc_refs_test.py` 与受影响的 Rust 合同测试。

空数组不代表无需验证；不能删项提高比例，不能以文档或符号存在代替运行结果。历史证据、原有内联引用和状态不因迁移改写或提升。R164/G60 中 `facts.rs#snapshot` 仅说明 forensic harness 如何读取并分类事实，不是对整个产品“没有审批门”的语义证明；原有有限 grep/diff 证据和未验收状态继续保留。T7 的源码注释迁移仍需独立建立覆盖证据。

R 描述用户要求，B 说明行为，Ref 定位证据，case 给出可验证情境。遵循 [shenko-dev](../../.agents/skills/shenko-dev/SKILL.md)，复用有效证据，以相称的检查解决实际疑问。

Rust 语法固定为 [tree-sitter-rust 0.24.2](https://pypi.org/project/tree-sitter-rust/0.24.2/)，包含[上游修复 #288](https://github.com/tree-sitter/tree-sitter-rust/commit/261b20226c04ef601adbdf185a800512a5f66291)：区分普通变量 `raw` 的 `&raw` 借用与 `&raw const`/`&raw mut` 原始指针，并识别 Rust 2024 的 `unsafe extern` 块。该版本生成的语法 ABI 为 15，与固定的 tree-sitter 0.25.2 支持范围一致；解析器仍消费未经改写的完整源码并拒绝含语法错误的树。Rust 消费者夹具同时覆盖这些语法、嵌套外部声明的完整归属，以及目标声明之外的畸形语法拒绝。
