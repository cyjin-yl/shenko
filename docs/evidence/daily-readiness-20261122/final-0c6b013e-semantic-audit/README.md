# 0c6b013e：三次失败旅程的独立原始语义审计

本审计保持原分母：**实际完成观察 3 次，完整旅程通过 0 次**。三份已关闭安装共保留 7 个主体模型请求、4 段真正提交的正文。逐一核对原始 sealed input、provider wire、模型结果、StepDecision 和浏览器帧后，**没有发现这 4 段正文提前声称已经拿到模型 child 的计算结果或两份成功回执**。这只说明已提交的等待/纠错正文没有预支任务结果，不证明真实任务完成，也不证明完整浏览器显示、重启回忆或日用已通过。[原批次回执](../final-0c6b013e/batch-summary.json) · [逐项派生记录](observations.json)

运行源码是 `0c6b013e7c0cc8c27e30a83189c14a36c71aaa74`，实际时间为 **2026-11-21 UTC**；证据目录继续沿用本轮的 `daily-readiness-20261122` 名称。三个进程都自然退出 1，原 shutdown receipt 的 `signal_calls=[]`、group complete/reaped、无剩余存活进程。审计读取关闭后的原 JSON/CAS/浏览器记录，没有再次启动主体或请求模型，没有改变原安装、driver 条件或原成功判据。[原运行汇总](../final-0c6b013e/compact.json) · [逐项 shutdown 原值](observations.json)

## 原件与复核方法

- [packets.tar.gz](packets.tar.gz)：既有只读 [export.py](export.py) 的三个原样 audit packet，以及其选取的 50 份逐尝试 CAS 原字节。文件为 407,469 bytes，SHA256 `c2e12bb2b39f71962dc23c49a02c58f90bfd6c207dedbedd06763d44bc343603`。
- [attempt-003-control-rejection.json](attempt-003-control-rejection.json)：从关闭后的第三份 CAS 单独补取的原始 `ControlOutputRejection/3`，1,948 bytes，SHA256 `a4d99beb6b60ae2841a416eac0b83efb6b417080cd5cf6a88176350d2dab0e73`。原导出器不跟随 `retry_reason_ref`，因此将此补取明确列为独立原件。
- [observations.json](observations.json)：[summarize.py](summarize.py) 只读上述原件及旁边既有公开 ledger/browser 回执后产生的确定性关联记录，包含全部 4 段提交正文、5 个有实际 response 的模型输出、7 份 payload messages、原始对象引用与 SHA256、浏览器帧及最终 DOM 行。脚本没有语义成功分类器，也没有新运行场景。
- [manifest.json](manifest.json)：本目录原件、导出器、派生脚本及派生 JSON 的文件哈希。

原 packet 内的 `semantic_verdict` 仍为 `not_assessed`。本文是人工解释；`observations.json` 的布尔项只检查身份/字节关联，不替换任务判据。50 份 CAS 原字节的长度和 SHA256 全部核对；每份 packet 所指的 8 份公开原件，其解压后字节均与原导出哈希一致，共 24/24。当前环境未安装 Python BLAKE3 模块，packet 的 `blake3_verified` 为 null；保留原 typed ObjectRef，但**不宣称独立重算过 BLAKE3**。

这是一份有边界的语义证据选集，**不是完整安装或完整 CAS 闭包的归档**。原 ledger/browser 文件继续在 [final-0c6b013e](../final-0c6b013e/) 中保留；packet 对它们记录哈希，不重复收录全部原件。原 E1 容量失败的闭包、二进制及停止数据库审计见 [独立容量证据](../final-0c6b013e-native-e1-capacity.json)。

在仓库根目录复算关联记录：

```sh
python3 docs/evidence/daily-readiness-20261122/final-0c6b013e-semantic-audit/summarize.py
```

## 七个主体请求与四段正式正文

下表中的终态是原 provider result；`success/stop` 只表示一次模型 generation 正常结束，不能代替控制协议接纳或任务成功。`S1` 是原 StepDecision 提交，序号属于该次安装自己的 ledger。[原模型结果、输入及提交关联](observations.json)

| 尝试 | 请求 ID | 原请求 `created_commit.seq` | 模型终态 | 主体结果 |
| --- | --- | ---: | --- | --- |
| 001 | `94360c3d-cd4a-45bb-b5ab-b0922f52bfa2` | 17 | success / stop，1538 tokens | step `e3a05380-ba11-47bd-a36c-b5711e3271f4`，S1 78；接纳两个 child，正文等待 |
| 001 | `f66377be-0bf3-4823-9aa2-7e0555563388` | 84 | success / stop，185 tokens | step `138f3f3f-3fe9-477e-bf58-a67c55d1c896`，S1 145；继续等待，无新增委派 |
| 002 | `b19f6c9e-278b-48e4-87cb-0bc09bcb8d6c` | 17 | success / stop，2156 tokens | step `4cdff4ca-950c-43c5-88c4-75cc6e00fa09`，S1 212；接纳两个 child，正文等待 |
| 002 | `7e3b0842-2c43-4765-a0fe-f114bdd8c0a9` | 217 | failed / Unknown，无 response | step `6b6f829d-aa92-4f97-b27c-357693951905` 最后为 awaiting_model；无 S1 正文 |
| 003 | `9c96c86f-9d9f-426d-ace8-df65ffd69c53` | 17 | success / stop，1730 tokens | step `8619364e-f311-4e60-a7dd-78c8fb4de589` 被 R0 退休；无 S1，动作未接纳 |
| 003 | `e14ba5d5-8e5c-4fb4-9deb-f44d8ab2cf15` | 160 | success / stop，2240 tokens | 新 step `449d0aa0-1ded-457b-bb0a-82bca424aacc`，S1 389；纠错后接纳两个 child |
| 003 | `7e687061-863a-48e1-a62a-10b83647aed0` | 396 | failed / Unknown，无 response | step `a45928d7-e6db-4a35-9c70-98659478a874` 最后为 awaiting_model；无 S1 正文 |

七个请求的原 `subject_step_input/15` 都与原 request_id/step_id 一致；sealed provider binding 与 terminal result 的 effective binding 也相等。所有主体请求的实际 max_tokens 为 4096，模型为 `swift-qwen3.8-27b-nvfp4`，通路为 `local.openai.chat_completions`，原 endpoint `http://10.10.10.11:8000`。每份实际 payload 都含原用户输入的确切事件头和正文；它可在历史消息中，不要求在当次 `events` 中重复出现。

五份有 response 的原 wire 均逐行解析并仅拼接 `choices[].delta.content`；结果与存储的 `response.content` 全等，均有终止 `stop`，没有 JSON 行解析失败。原 wire 的 `reasoning` / `reasoning_content` 字段独立存在，没有混进这五份存储正文。两份 Unknown 结果没有 response，也没有主体流输出（原 `public_chunks=0`）。四份提交的 `model_result_ref` 原文本又与对应 `response.content` 全等，StepDecision CAS 与 ledger 里的决定也全等。[完整关联与原 wire 的 UTF-8 哈希](observations.json)

### 001：等待延续，没有再次委派

第一份正式正文明确表示已派出两个 child，并“现在等待它们的真实回执，不提前宣称成功”。该 S1 的两个 intent 对应唯一的 `model.child` 与 `host.printf` TaskRecord，二者 `created_by_step_id`、`created_commit`、`input_ref` 都绑定这个原决定。

第二个 sealed payload 中有前次原输出及两个 `ledger_status=Pending` 的实际 runtime tool_call，原用户事件再次选入，但没有 runtime tool_result。第二段正文准确地说“还没有收到它们的真实 task_result”，输出 `[wait:continue]`；该 S1 没有新增 delegated intent。这不是又启动一套 child。[001 输入、正文、task 绑定](observations.json) · [001 原 ledger](../final-0c6b013e/attempt-001/final.ledger.json.gz)

### 002：没有提前宣称拿到 child 结果

唯一正式正文说：

> 两个都还没返回，所以我此刻不敢说任何结果，只启动、只等待。

正文后续把报告计算值和 stdout 写成将来的动作，并要求看到退出码、stdout 与 stderr；没有说已经观察到 437 或成功回显。该次 S1 正常创建两个指定 child。后一个 sealed payload 保留原用户消息、原输出及两个 Pending tool_call，`events=[]`，另有运行时连续思考提示；这个请求最后为 Unknown、无 response、无新决定。[002 全文和实际 payload](observations.json) · [002 原 ledger](../final-0c6b013e/attempt-002/final.ledger.json.gz)

正文的“当前没有注册回复通道”也能回溯到该次实际 system prompt：原合同区分系统时间线展示与外部消息发送，并明确当前无外发回复通道。不能将这句话当成模型凭空编造通道状态；也不能据它否认浏览器展示了正文。它与“已完成 child 结果”是不同问题。

### 003：一次真实协议拒绝，纠错实际进入新输入

第一次完整 response 使用了错误的 `[delegate:model.child:1]`。原 `ControlOutputRejection/3` 明确绑定旧请求、旧输入、旧模型结果，记录 `CONTROL_PROTOCOL_INVALID`、`line=3`、`protocol=3`、`reason="invalid native definition kind"`，以及 replacement step `449d0aa0-1ded-457b-bb0a-82bca424aacc`。旧 step 的 `decision_ref` / `commit_ref` 都为空，状态为 retired。[原拒绝 CAS 字节](attempt-003-control-rejection.json) · [003 原 ledger](../final-0c6b013e/attempt-003/final.ledger.json.gz)

第二个实际 sealed payload 的 messages 依次包含原用户输入、原错误 response，以及运行时反馈：“此结果的全部动作均未接纳，输入事件仍待处理。请依据原输入纠正完整结果……”。因此这里确有原失败进入新请求的证据。随后正式正文解释语法错误，把有限模型 child 改为 `[delegate]`，原生任务保留 `[delegate:host.printf:1]`，并明确“依旧不模拟结果、不提前宣称成功”。只有这个新 S1 创建了两个任务；不能把先前未接纳的文本当成一次真实执行，也不能把它从模型请求分母中删除。[纠错前后原消息与正式决定](observations.json)

第三个请求又封存了原输入、拒绝反馈、纠正后的原输出及两个 Pending tool_call；最后为 Unknown、无 response，没有新的正式正文。唯一提交正文中的语义等待条件描述的是何时退出等待，不是声称两份结果已经到达。

## 原生事实与未到达的完成阶段

每个关闭后的 ledger 都只有一个指定模型 child、一个 host.printf child，各有一个 operation、attempt、E1，但 **task_results 均为 0**。三个模型 child 的原 provider result 都是 `failed / unknown / response=null`；对应原 normalized step record 为 `complete=false`、`text=""`、`reply=null`、`output_ref=null`。其 request_id、task_id 与实际冻结的 local provider binding 保留在证据中；child 实际预算分别为 128、512、512，这是主体当次委派的原值，不能重写成观察到的完整计算答案。[三个 child 的原 raw/normalized 字节和引用](observations.json)

三个 printf 原 observation 各有 `exit_code=0`、空 stderr、`exact_postcondition=true`，解码后的 stdout 分别精确为 `S1-bcb82df6078d`、`S1-77f6a42c0f38`、`S1-54b9a0b98430`。这证明已有真实局部效果；它们没有形成 T2 task_result。原失败仍是原 E1 封闭证明容量债等运行错误，不能把局部成功改记为旅程成功，也不能把 Unknown child 解释成错误计算或已经回答。[局部原件](observations.json) · [独立容量审计](../final-0c6b013e-native-e1-capacity.json)

七份 payload 的实际 messages 没有 runtime tool_result，原 session history 也没有 tool_result 行。它们都没有形成“主体读取两份原 T2 后提交最终回答”的链。这里不以“缺两个新 event”做推断：后续候选若一份结果来自本轮事件、另一份来自封存的真实历史，仍须按实际 payload 与原 T2 关联来审，而不是要求重复投递。**本三次样本的最终结果报告、durable pause 后重启、下一消息回忆及重启后无重复效果均未到达。**[原 driver checks 与错误](observations.json)

## 浏览器：收到绑定帧与显示绑定正文分开记

四段原 StepDecision 正文都在原 browser events 中找到完整 `timeline_part`：message_id/step_id 精确相同，`commit_ref` 等于相应 S1，`terminal=true`、`committed_only=true`，content 与决定正文逐字节相等。对应原 JSONL 行为 001 的 94/148、002 的 198、003 的 398。比较使用 `commit_ref`，没有将旁边的投影 `commit_seq` 错当成 S1 序号。[逐帧原值](observations.json)

这四个帧不能单独证明最终对话视图已正确展示提交后的版本。原 driver 在 `evaluate_first` 全部满足后才调用 `observe_output`；本批次都在之前失败，因此没有 `first-output` 或 restart 的浏览器成功观察。最终截图、ARIA 与 DOM 实际是：

| 尝试 | 最终对话视图的实际证据 | 本审计的边界 |
| --- | --- | --- |
| 001 | 历史读取失败，显示“其他 2 轮（输入历史尚未全部读取）”，正文不在当前对话视图 | 两个绑定帧已收到；最终画面不能证明两段正文可见 |
| 002 | 显示上面的等待正文、输入“已处理”，两个 child 仍运行中 | 可以观察到等待正文；没有结果完成的回答 |
| 003 | 显示旧拒绝正文和同一纠错 step 的旧 `text/0` 流式行，`stable_fact=false`；还残留实际 `[delegate]` / `[wait]` 行，并把“模型[child]当成了……”截断到“模型” | 正确绑定帧的 part_id 是 `text/0@b1abb45e-e88b-4485-8fdd-dc3a65aced3e/389`；收到了该帧，不等于对话视图替换到了这份提交正文 |

原件：[001 PNG](../final-0c6b013e/attempt-001/browser/final.png)、[ARIA](../final-0c6b013e/attempt-001/browser/final.aria.yml)、[DOM](../final-0c6b013e/attempt-001/browser/final.dom.json)；[002 PNG](../final-0c6b013e/attempt-002/browser/final.png)、[ARIA](../final-0c6b013e/attempt-002/browser/final.aria.yml)、[DOM](../final-0c6b013e/attempt-002/browser/final.dom.json)；[003 PNG](../final-0c6b013e/attempt-003/browser/final.png)、[ARIA](../final-0c6b013e/attempt-003/browser/final.aria.yml)、[DOM](../final-0c6b013e/attempt-003/browser/final.dom.json)。本文保留这些界面差异，未修改 UI、driver 或原失败分数。

## 后续候选沿用的独立审计边界

后续只在每次 summary 已落盘、进程已 reaped 后读取同样的原件。必须分别核清：原用户输入进入真正 sealed request；真实 child 输出和模型身份；printf 原 E1 的精确输出及效果次数；最终回答所用 request 的实际 payload/history 确实包含两份原结果；提交决定与浏览器所见正文的绑定；重启后的回答以及没有新任务或重复效果。仅有正确数字、marker 或 S1 序号晚于 T2，不能替代这些来源关联。所有失败、未接纳请求、Unknown 与未到达阶段继续留在分母和原记录中。
