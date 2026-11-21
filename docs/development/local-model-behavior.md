# 本地模型行为验收 / Local-model behavioral acceptance

## Why this exists

Shenko 由**本地模型**驱动。一个行为即使有完整的 Rust 测试、正确的 SQLite 事务和
正确的 parser，也可能在真实运行中不可用：模型可能从不输出控制行，可能把控制行缩进
或用代码块包起来，也可能输出一个它根本无从得知的号码。测试通过只证明"代码按约定
执行"，**不证明模型会按约定行动**。

所以对提示词、控制协议、事件投影这类会被模型读到的改动，验收对象是**真实本地模型
在若干次采样中适配该行为的成功率**，而不是"看起来写得对"。

这条规则不改变用户目标，也不新增人工审批：它只是把"验证"放到正确的层面。

## What is measured / 测什么

同一份证据必须绑定四件事，缺一不可：

| 组成 | 实际来源（不得复制） |
| --- | --- |
| system prompt | `shenko_subject::canonical_system_prompt()` + `IdentityService::persona_section()` |
| 事件投影 | `shenko_subject::staged_message_text()`、`IDLE_CONTINUATION_PROMPT` |
| 模型调用 | `shenko_broker::local_provider::LocalModelProvider`（真实 OpenAI wire） |
| 输出判定 | `shenko_subject::output::parse_output()` |

任何一个组件改成测试内的复制品，都会让评测与产品分叉：产品改了提示词而评测仍用旧
文案，就会给出假的绿灯。新增这类可见文本时，把它放进产品代码并导出，而不是写进测试。

评测对每个行为采样 N 次（默认 8），统计成功率，要求不低于阈值（默认 0.85）。
采样使用真实采样参数：vLLM/OpenAI 端默认温度，不由评测偷偷设为贪心，因为产品运行
时就是那个分布。失败样本原样打印并被计数。

## 与具体模型解耦 / Model-agnostic

评测不写死模型名、供应商或地址：

```sh
SHENKO_LOCAL_MODEL_URL=http://10.10.10.11:8000 \
SHENKO_LOCAL_MODEL=swift-qwen3.8-27b-nvfp4 \
SHENKO_BEHAVIOR_SAMPLES=8 SHENKO_BEHAVIOR_MIN_RATE=0.85 \
cargo test -p shenko-subject --test local_model_behavior -- --nocapture
```

- 未设置 `SHENKO_LOCAL_MODEL_URL` / `SHENKO_LOCAL_MODEL` 时测试**跳过**并说明原因，
  `cargo test` 保持离线、确定性、无网络。
- 任何提供 OpenAI-compatible `/v1/chat/completions` 的服务都能跑：本地 vLLM、局域网
  服务器、另一个小模型。换模型时不需要改测试代码，只改环境变量。
- 断言只看协议与语义（是否解析成功、目标号码是否来自消息头、内容是否保留事实），
  不断言具体措辞，因此不把某个模型的文风写进产品合同。
- 当前在用的模型 ID 从引擎真实响应读取（`GET /v1/models`），不要从历史记录推断：
  `/tank/hot/models/...` 里的目录名不是服务名。

## What a run proves, and what it does not / 边界

证明：

- 这份提示词 + 这份事件投影，在当前这个真实模型上，能让模型以声明的成功率产出可执行
  的控制行（或在不该回复时保持思考）。
- 改动前后的成功率差异，可以直接作为提示词有效性的证据。

不证明：

- 产品功能完成、真实 QQ 送达、provider 额度、长期运行稳定；
- 未被场景覆盖的行为（工具委派、child 协作、多轮记忆召回等）同样可用；
- 阈值是产品质量承诺。它是开发门槛：低于它就先改提示词/上下文，而不是宣布完成。

## Rules for developing with it / 开发规则

1. 改提示词、控制协议、事件可见文本、或任何会被模型读到的内容时，先跑一次基线，
   改完再跑一次，把两次的成功率都记下来。
2. 失败要在**产品侧**修：补充消息头里的可回复号码、把规则写清楚、减少歧义。
   不允许通过放宽判定、特判样例、调高温度或换一句话让测试"看起来过"。
3. 同一连贯增量不要为每个行为新写一个测试文件。评测是一个 harness + 一张小场景表；
   只有出现**真正独立的新行为要求**时才加一行场景。
4. 证据写进 `docs/evidence/local-model-behavior-<date>.json`：模型 ID、端点、prompt
   digest、每个场景的样本数与成功率、失败原文（截断）、以及未覆盖范围。日志与原始
   输出留在私有位置，仓库只保留不含凭据的摘要与哈希。
5. 失败的运行也保留。一次"失败→修复→通过"的真实记录，比一个从未失败的绿灯更有价值。

## Current first measurement / 首次实测

首个场景集覆盖三步基础行为：私聊消息的回复控制、`[remember]` 事实保存、无外部消息时
继续思考。首次运行时发现一个真实产品缺陷：事件投影只把`[昵称] 正文`交给模型，
模型看不到 QQ 号，因此 `[reply:private:QQ号]` 这条控制**无法被执行**，虽然它能被
解析。修复是把平台真实上报的回复目标放进消息头（`[昵称 · 私聊 QQ 号]` /
`[昵称 · 群 群号]`），并在输出合同里说明号码取自消息头。实测数据见对应 evidence
文件；它不是完整 C09/C16 或真实渠道送达的验收。

## 首请求控制协议对照 / First-request protocol observation

`scripts/real_protocol_observation.py` 补充一个范围更窄、使用实际 CLI 的观察入口。
上面的 Rust harness 保持原有基础行为覆盖；它直接调用 provider，不能替代工具委派的
真实 Store、完整能力目录、subject step 和 S1 接纳证据。

这次澄清来自候选 `cb1d0632` 的实际失败：首个响应以 `stop` 结束，但将 `[delegate]`
与 JSON 分成两行，得到 `CONTROL_PROTOCOL_INVALID`（第 3 行，`empty action`）。
完整旧 system prompt 的 SHA256 为
`e7f06f9159ca450e9aa75b187aa9a103def1a281931ab277b8399b47a4308d9c`；原始结果 CAS
为 `blake3:430000314a04e2ae5eec5dde9e004be714a68558f17f7ed05e92efdc6c7ee095`，
SHA256 为 `16674c93c35fb468d549221fce726e287f0a456339c7aa7af821aab84d1a80af`。
原始输入、完整 wire、失败正文和下一轮的完整纠错反馈均保留在该运行证据中。
本次产品变化只把“后接一行 JSON”明确为“标签后同一行写完整 JSON”，并在
empty-action 诊断中说明这一点。parser 的成功条件、4096-token 主体预算和模型配置不变；
跨行 JSON 仍拒绝。不能由文字变更本身推断成功率已经改善。

前后对照使用 release 二进制：before 为 `72fc70d3`，after 为上述文字/诊断修复。
两边都用同一份新 driver 和 journey helper；`33363d95` 的观察 socket 配置只修正
观察期限，不能作为模型行为或性能改善的因素。模型由实际环境指定，不修改温度、
reasoning 配置、主体或 child 预算。每边各运行脚本内固定的 8 个新任务；每个配对具有
相同的计算题、原样 marker 和用户文字，每次使用全新安装。样例不含控制行或预期答案，
也不复用写作/诊断中的 19×23 例子。

```sh
SHENKO_LOCAL_MODEL_REQUEST_TIMEOUT_MS=600000 \
python3 scripts/real_protocol_observation.py \
  --binary /absolute/path/to/release/shenko \
  --source-revision ACTUAL_COMMIT \
  --model-url http://10.10.10.11:8000 \
  --model swift-qwen3.8-27b-nvfp4 \
  --evidence-dir /absolute/path/to/new-before-or-after-directory \
  --timeout 900 --startup-timeout 300 --timing
```

每个安装通过实际 CLI `--pause-at-start` 启动，确认未派发模型请求，通过真实 WebSocket
接受输入后再执行公开的 `runtime resume`。观察只等**最早的主体请求**到达 terminal，且
它自己的 step 已 committed 或 retired；不会等待纠错、child 完成或最终回答。原始
installation、模型结果和 wire、第一次/最终 ledger、全部 CAS、文件哈希及 owned runtime
关闭回执都保留。关闭引起的后续 Unknown 和任何失败留在尾部事实中。

`--startup-timeout` 是独立的启动观测边界，接受正整数，默认仍为 60 秒。本次前后两组
统一使用同一份新 driver 和显式 300 秒边界，before/after 二进制不因驱动变化而改变。
manifest 与批次摘要记录配置；每个安装的 `first.startup.json` 保留实际耗时、是否就绪与
原始失败，耗时不包含随后 owned cleanup。扩长观测窗口不代表启动延迟得到修复，
也不改变请求预算、协议/耐久成功条件或清理判据；此前未就绪的失败尝试仍保留。

报告的三个指标分别是：原始输入进入首个 seal；首结果完成并实际提交 S1、没有 R0；
该 S1 恰好接纳各一个 `model.child` 与 `host.printf`，child 绑定指定模型/endpoint，原生
输入为对应 marker，且实际 task 行绑定同一 decision/commit。它不判 child 的计算语义、
执行结果或用户任务完成，也不证明重启恢复、完整浏览器旅程或延迟改善。

`batch-summary.json` 在 spawn 前写入每次尝试，保留失败分母和未尝试数。超时、证据缺失、
身份/预算不符或不干净的关闭不能计作通过；原始测量结果仍单独保留，不能用后续成功覆盖。
二进制哈希和实际 `/v1/models` card 在每次前后校验，唯独响应生成的 `created` 和
`permission` 元数据不参与身份比较；完整响应仍保存。模型 card 不是内存权重的证明，实际
服务启动配置、模板和权重哈希应由部署证据另行绑定。前后报告还必须核对同一场景表哈希、
model card、请求时限与 sealed/effective 配置；任一批次不足 8 个已观察样本时明确保留缺口。
