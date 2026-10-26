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
