# 0010 继续使用 Adapter；上游 compiler 须原样编译这些源文件

状态：owner 决定，2026-11-25。本文补充 [0008 §1](0008-files-encounters-attachments-2026-11-19.md#1-界面proto-ui-compiler-与-imessage-质感) 与 [web-interface §6](../specs/web-interface.md)，不减损既有义务。

## 来源（逐字）

> 先继续adapter吧但要求对方的编译器能原样编译我们走adapter的文件并规定好他们compiler的linting规范

这句话答复两个问题。背景是上游 Proto-UI 准备推倒重写 compiler（[#1809](https://github.com/Proto-UI/Proto-UI/issues/1809)），[#1915](https://github.com/Proto-UI/Proto-UI/pull/1915) 删除旧实现、提交 0.4 规格。两个问题是：已采用的 family 现在是否切到编译产物；Shenko 自有的 Prototype 在旧 compiler 删除后怎么办。

## 1 Shenko 继续经 Adapter 运行

**决定。**
- 已采用的上游 family 继续经运行时 Adapter 执行，现阶段不切换到旧 compiler 的产物。
- 当 pin 越过旧 compiler 的删除点时，Shenko 自有的 Prototype（`transport-badge`、`message-bubble`）也改为经 Adapter 运行，不为保留编译而停止跟进上游。

**可观察效果。**
- 构建清单如实标明每个 Prototype 走 Adapter 还是编译产物。
- 0008 §1 以编译为目标的意图不变。切换的条件见下一节：上游新 compiler 能原样编译这些文件，并通过行为比对。

## 2 对上游的要求：原样编译，并有明确的 lint 规范

**原样编译。** Shenko 经 Adapter 运行的文件，上游新 compiler 必须能**不经修改**地编译。“同一文件”指同一份源文件同时满足两点：
- Adapter 能运行它；
- compiler 能编译它，且两者行为一致。

不得另写一份“可编译版本”，也不得要求 Shenko 改写。如果新语言判定某处写法不合法，由上游修改它自己的官方源文件，让 Adapter 与 compiler 读取同一份新文件；Shenko 自有文件需要改动时，规则必须提供自动修复。

**文件范围。** 统计基于上游 main `67fd94f5`：
- 16 个已采用 shadcn family 的源文件，及其传递依赖的 base 源文件：共 204 个文件，其中 98 个 `.proto.ts`，18,483 行；
- 加上 Shenko 自有的 2 个 Prototype。

**lint 规范。** 上游需要提供一份版本化的 lint 规范，让作者在编辑时就知道一个文件能否编译。要点：
- **与 compiler 同源：** lint 由 compiler 前端的同一份分析产生，不另维护一张准入表。
- **区分作者错误与 compiler 欠账：**
  - 只有违反语言定义的写法（`source-invalid`）是作者错误；
  - compiler 或后端尚未实现的合法写法，是上游自己的工作项，不能报成作者错误。
- **规则可追溯：** 每条规则有稳定 ID、对应的合同条款、正反例；机械性问题附自动修复。
- **CI 门禁：** 官方库与登记的消费方文件集合（包括 Shenko 的这一份）必须 lint 通过并能编译，回退会被 CI 阻止。

## 实现状态

- 本文写成时，Shenko 的 pin 是 `710a90df`，早于旧 compiler 的删除，两个自有 Prototype 仍由旧 compiler 编译。
- 要求已提交到上游 [#1915 的评论](https://github.com/Proto-UI/Proto-UI/pull/1915#issuecomment-7170144913)，附 `proto-lint/v1` 规则草案、19 处首处阻塞的分类，以及建议写进合同的三项。上游尚未答复。
