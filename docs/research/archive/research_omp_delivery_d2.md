# OMP D2：子 Agent 的有限直接发布核验

研究日期：2026-10-22。只读源码与上游测试，未安装、未运行测试、未改产品主稿。

固定上游：`can1357/oh-my-pi`，commit **0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0**，2026-10-22T10:22:25Z。本报告全部源码链接固定到该版本。

## 结论

用户已决定：对话子 Agent 在有限情况下可直接发送结果；主体“灵魂”可自改；委派时可任意写子 Agent system prompt 并选择不同模型。这三项应成为产品合同。

OMP 可以提供很具体的**结果归属、有限回传、完成判定和避免重复通知**参考，但本次核验的 task/IRC 路径没有证明它会向 QQ、飞书等外部聊天平台直接发送。它的 async-result 默认进入拥有任务的 Agent 会话，IRC 是进程内 Agent 间通信，主 UI 的 relay 明确只是展示。个人 Agent 的直接外发是我们新增的渠道能力，不能冒充 OMP 已有功能。

## 一、上游实际行为

| 行为 | 固定版本证据 | 精确含义 |
|---|---|---|
| task 后台完成自动投递 | [agent-session.ts L2346–2359、L3300–3325](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/session/agent-session.ts#L2346-L2359)；[async-job-delivery.ts](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/session/async-job-delivery.ts) | 每个会话为自身 agent id 注册 sink；结果进入该会话 yield queue，receipt 在注入或丢弃后完成。不是默认向人类聊天渠道发布。 |
| 子 Agent 普通文本也可作为最终结果 | [executor.ts L2775–2795、L2861–2878](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/task/executor.ts#L2775-L2795) | `structured=false` 使用最后报告；structured 模式依赖实际 yield。跟进轮的普通文本要满足成功、非取消、非超限、非空及有效发起来源，才覆盖已接受的 canonical artifact。 |
| 有限的跟进结果发布来源 | [executor.ts L3343–3356](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/task/executor.ts#L3343-L3356)；[L3429–3450](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/task/executor.ts#L3429-L3450) | 父消息唤醒、父 steer 实际进入该轮、或用户聚焦该子会话直接发起，可以产生新的 message-result；仅仅收到父请求还不够，peer-only 的一般对话不能覆盖已有成果。用户聚焦子会话是 OMP 行为；我们仍遵守所有外部输入先到唯一主体的已定合同。 |
| 没有手动回复时自动转交最终报告 | [executor.ts L3053–3124](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/task/executor.ts#L3053-L3124) | 把 yield 摘要或最后 assistant 文本回传给唤醒它的 Agent；已有 async job 投递的 owner 不再走第二条 relay。不是把途中叙述都当最终结果。 |
| 防循环和有限去重 | 同上；[irc/bus.ts L83–113](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/irc/bus.ts#L83-L113) | broadcast 和 wakeRelay 不作为新回传起点；成功轮如果已给该唤醒者发消息，就跳过自动重复回传。判定使用进程内成功发送标记，不是跨进程、跨平台 exactly-once。 |
| 已发“收到”不能吞掉后续失败 | [executor.ts L3105–3112、L3159–3193](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/task/executor.ts#L3105-L3112) | 失败/取消是不同生命周期事实，即使此前发过消息仍通知；如果失败前已有成果，要同时说明成果与失败，不宣称“没有输出”。 |
| IRC receipt 不是人类已读或业务完成 | [irc/bus.ts L1–9、L66–81](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/irc/bus.ts#L66-L81)；[L357–380](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/irc/bus.ts#L357-L380) | injected/woken/revived 是交给接收 Agent 的方式；主会话 UI 的 `irc:relay` 是 display-only card，不改变投递语义。 |
| 无活跃 owner 不误投其他会话 | [job-manager.ts L1186–1211](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/async/job-manager.ts#L1186-L1211) | 缺失 owner sink 时 dead-letter；不是回退投给第一个主会话。结果暂留可查，不能据此宣称持久补投。 |

### 失败、取消、投递重试的边界

- 非取消的 job 失败会投递错误，取消后晚到的结果可以保存在 job row，但不会重新变成成功并入队；取消是设置状态并发 AbortSignal，不能撤销已经产生的外部效果。[job-manager.ts L410–439、L483–494](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/async/job-manager.ts#L410-L439)
- sink 抛错后有指数退避和 jitter，且只在 job 尚存在、未抑制时重试；默认未消费结果约 5 分钟、已消费结果约 30 秒后回收。这是进程内生命周期，不能代替我们断电恢复所需的 durable outbox。[job-manager.ts L8–23、L1229–1250](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/async/job-manager.ts#L1229-L1250)
- foreground wait 消费结果时抑制重复异步注入；会话切换还会递增 delivery epoch，防止旧结果在 job id 重用后跑进新会话。[job-manager.ts L600–636](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/async/job-manager.ts#L600-L636)；[agent-session.ts L3204–3216](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/session/agent-session.ts#L3204-L3216)

### system prompt 与模型：不要把不同接口混为一谈

OMP 的 AgentDefinition 有 `systemPrompt` 和优先模型列表；agent 文件、settings model overrides、role aliases、spawn hook 共同影响选择。普通 task wire 并没有为所有子 Agent 暴露任意 `systemPrompt` / `model` 参数，eval bridge 才有 invocation-local model override；seance 的 model 字段是特例。[types.ts L271–288](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/src/task/types.ts#L271-L288)；[task-agent-discovery.md L224–232](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/docs/task-agent-discovery.md#L224-L232)

因此本产品应直接把用户决定落成委派接口：每次可传完整子 Agent system prompt、模型选择和工具合同，不必照抄 OMP 的配置间接性。“灵魂”自改也应保留版本与变更出处；这不是把人格冻结成不可改的核心。

## 二、可采用的有限直接发布合同（本产品推导）

以下是产品建议，不声称已由 OMP 实现。

1. **先有明确委派，再允许直接发布。** 唯一主体先接收外部输入并形成任务；委派可一次赋予有限 `ReplyGrant`，包括 task/generation、目标渠道/会话/线程、所回复输入、受众范围、可发布消息种类、完成/取消条件和必要的有效期。范围可由主体自主决定；有效范围内无需再过一次主体 LLM 审稿或询问用户。权限不从子 Agent 自写的 system prompt 中解析。

2. **把“直接”实现为无需主体改写的发布路径。** 子 Agent 生成符合渠道的文本、文件或动态 UI intent，交给渠道插件的发布工具；插件校验有效 grant 和当前任务版本后发送。主 loop 不直接操作网络发送；工具仍属执行层。没有直接发布 grant 的研究子 Agent 默认把结果送回委派方。子 Agent 的公开回复仍属于同一个人格主体，不自行建立第二个长期主体 loop。

3. **最终成果与进展、失败分别建模。** final-success 需要已完成且有非空有效结果；进展、澄清、阶段性成果、失败通知使用各自消息种类及相应 grant。不要照搬“本轮发过任意消息就算已回答”；“我在查”不能抑制真正答案或随后失败。失败前已提交部分结果时保留该事实。

4. **每个逻辑回复只有一个发布责任人。** 用持久 `reply_intent_id`、task generation、result revision 和发送目标记录责任；同一结果经 direct 路径提交后，主体只消费发送回执，不再默认复述一次。不同子任务确有不同答案时可以各发其获准部分；需要综合的任务由主体决定把综合责任交给谁。

5. **持久发件箱与外部回执分离。** 记录 prepared → dispatched → confirmed / failed / unknown，重试同一逻辑回复复用幂等键；保存平台 message id、实际目标和内容 revision。平台不支持幂等或查询时，网络断开后的 unknown 不能假定没发而无限重发，也不能声称 exactly-once。由执行恢复策略核查或把不确定状态回报主体。

6. **撤销用 generation/epoch 阻挡晚结果。** 发送前重新验证 grant 与任务 generation；用户修改任务、主体重委派或取消后，旧子 Agent 的晚结果不能自动抢发。若平台已确认发送，则撤回/编辑/补充更正是独立可审计的外部操作，依平台能力处理，不能把任务取消当作消息从未存在。

7. **有限授权不冻结灵魂或模型。** 委派时记录生效的 prompt、人格 revision、模型及其实际 fallback；主体可以自改灵魂、修改下一轮委派和在需要时显式更新在跑子任务。子 Agent 可采用不同工作角色、完整自定义 system prompt；已有渠道/受众权限和发布责任是独立合同，不会因为 prompt 改写自动扩大。新人格 revision 是否使在途回复失效由具体任务变更决定，不能每次人格更新就统一中断所有工作。

8. **完成反馈与推理预算不绕过主体。** direct publish 的内容摘要、证据 handle、回执和失败事件都回灌唯一主 loop；模型调用仍走全局 broker。纯结果回传和渠道发送不应为了取得一个已完成结果再占用推理 slot；OMP 对用户恢复的完成结果可直接 `publishCompleted`、不再预占执行 slot 的路径可借鉴。出错后是否联系用户、如何选渠道，继续遵守主体的动态联系策略，不在插件里写死“失败就发某群/三分钟后打电话”。

## 三、应纳入验收的 5 个场景

1. 子 Agent 先发进展，再成功或失败：最终状态不得因“已经发过消息”消失；主体不重复刊发同一 final。
2. 主体已取消/修改任务，旧子 Agent 恰好完成且 job id 被复用：旧结果和旧 grant 都不能进入新对话或外发。
3. 外部平台已收消息但本地确认前断电：恢复使用相同逻辑发送身份，保留 unknown/核查流程，不无条件重复发送。
4. 子 Agent 收到 peer 广播或自动回传：可以内部处理，但不因此获得新的外部发言权，也不能形成自动回传循环。
5. 所有推理 slot 均忙时，已完成子 Agent 的提交/发送回执仍可处理；不同模型的子 Agent 不能突破同一硬件资源组并发上限。

上游测试的静态交叉证据：
[executor-result-acceptance.test.ts](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/test/task/executor-result-acceptance.test.ts) 包括 peer wake 不覆盖 accepted result、取消不覆盖 artifact、用户恢复结果在执行 slots 满时仍投递、父 wait 答案仅出现一次、只回传最终报告不回传之前 narration；
[agent-session-async-delivery.test.ts](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/test/agent-session-async-delivery.test.ts) 包括旧 session/复用 id 的晚投递隔离；
[irc-bridge-relay.test.ts](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/packages/coding-agent/test/session/irc-bridge-relay.test.ts) 包括 relay 标记与广播不算父请求。这里只核验测试源码存在，未宣称本地跑测通过。

