# §42 后期增量：Provider 周期额度与自适应模型预算

状态：可并入 Shenko SPEC 的研究合同；对应 U41 / B42，来源 REF-QUOTA。本节不是 V1 必须接入的云服务依赖，不改变优先本地推理、插件化主干、持续主体或 §41 的耐久执行合同。以下接口、字段、控制策略与验收是 Shenko 设计；上游已提供的接口另行标明。

## Q1. 产品目标与边界

额度插件帮助主体知道某个账户还有多少可用资源、何时可能恢复、这些信息有多可靠，并在已有授权内自主选择 smart / cheap / local 模型、推理预算及任务分配。它可以追求“在整个周期内持续取得有用成果，接近周期结束时用到目标额度”，但不得承诺精确用完一周额度。

主体平均推理次数以有限 generation 为统计单位：分别统计全部主体步、云端主体步、不同模型的主体步和 child 请求。调整模型配比、每次推理的 effort / 输出预算、委派策略与云端承担的比例，不把持续主体改成低频 Tick，也不生成没有任务价值的请求来消耗余额。没有任何可用推理后端时，必须报告真实的 capacity-blocked 状态；不能把不能执行包装成仍在持续思考。

QuotaObserver、QuotaForecaster、QuotaPolicy 及其 Provider 适配器均为可替换插件。主体可读这些插件给出的事实、预测与策略，并在现有资源和消费授权内修改策略；无须逐次询问用户。新增付款、提高用户已设费用上限、开启未授权的额外付费，不由“优化已购额度”自动推出授权。

## Q2. 已核验的 Anthropic 观测面

| 观测面 | 官方目前提供什么 | 本合同不得推导什么 |
|---|---|---|
| Claude 订阅 Usage | 五小时与每周使用窗；claude.ai、Claude Code、Desktop 共享使用；复杂度、模型、功能与 effort 影响消耗 | 不能把订阅百分比换算成固定 token 数、固定请求数或 API 美元余额；其他客户端的消耗并不天然可归因给 Shenko |
| Claude Code statusline JSON | `rate_limits.five_hour`、`seven_day` 可含 `used_percentage` / `resets_at`；相应 Claude apps gateway 可提供 `spend_limit`；后者部分版本还能提供估算美元和 period | 只适用于接口实际覆盖的登录/网关配置。不是所有 API key 都能读到；不是通用 Pro/Max 订阅 REST API 的证据 |
| API 响应 rate-limit headers | 请求、输入 / 输出 token 等限流值、remaining 与 refill 时间；token bucket 连续补充；部分 token 数有舍入 | 这些 reset 表示补充时间，不能解释成订阅下周恢复时间；rate limit 不是保证获得的容量 |
| API Rate Limits API | 组织、workspace 配置限额及继承关系；需要该接口认可的凭据 / scope | 配置值不是实时剩余额度。workspace 未设 override 是继承组织限制，不是无限额 |
| API Usage & Cost Admin API | 组织 API 历史 token 与成本报告，存在数据到达延迟，需要合适的管理权限 | 不覆盖通用个人 Pro/Max 订阅余额；历史账单查询不是即时 admission lock |

**statusline 的缺省语义必须原样保留。** 官方说明 `rate_limits` 只在适用的 Pro/Max 或带 spend limit 的 gateway 环境出现，并且要到首次 API 响应之后才有数据；三个 window 可以独立缺失，CLI 会在 `resets_at` 过去后移除旧 window。缺失不能解释为 0% 已用、100% 剩余或已经耗尽。`resets_at` 是 Unix epoch 秒，API header 的时间格式和 refill 含义另行解析。适配器记录 CLI / gateway 版本并探测字段，不能假定所有版本都支持 `spend_limit` 的美元字段。

**观测失败与推理额度耗尽分开。** Claude Code `/usage` 可以显示最长一小时内的 last-known usage；查询本身被限流并不等于推理额度耗尽。该命令的 Session 美元统计也不能作为订阅剩余额度。本轮未找到官方承诺的通用 Pro/Max 订阅余额 REST API；实现使用上述公开观测面或经证实的新增官方接口，不依赖未公开内部 OAuth endpoint。

默认通过实际 CLI 会话的 statusline 或正常响应附带的数据收集快照，不为探测余额额外启动付费 generation。管理报告按其支持的频率缓存查询。UI 观测若作为可选适配器，必须标记为 UI 来源和相应版本，不能冒充稳定协议。

## Q3. 规范类型

以下标识均在其账户 namespace 内稳定。`account_scope_ref` 是秘密管理引用，不包含凭据；`DataRef`、`CommitRef` 等沿用 §41。数组和可空字段不是省略错误语义的许可。

```text
QuotaSnapshot {
  snapshot_id, provider_id, account_scope_ref,
  source_kind: api_headers | admin_api | cli_statusline | cli_ui | manual | local_meter,
  source_contract_ref, source_version?, raw_evidence_ref,
  observed_at, provider_reported_at?,
  freshness: fresh | stale | unknown,
  observation_status: available | unavailable | unsupported,
  observation_error?,
  attribution: exclusive | shared_known | shared_unobserved | unknown,
  covered_through?: { provider_cursor?, local_ledger_commit? },
  windows: QuotaWindow[],
  absent_windows: { expected_window_key, reason: not_yet_observed | expired | unsupported | unknown }[]
}

QuotaWindow {
  window_key, provider_quota_group_id?, scope_ref,
  applicable_models_or_group_ref,
  period_id?,
  kind: fixed | rolling | calendar | token_bucket | unknown,
  metric: usage_percent | requests | input_tokens | output_tokens | currency,
  unit, currency_code?,
  used?: Measurement, remaining?: Measurement, capacity?: Measurement,
  resets_at?, replenishes_by?, time_uncertainty?,
  enforcement: provider_hard_limit | configured_cap | informational | unknown,
  overlaps_or_inherits: WindowRelation[]
}

Measurement {
  value?, lower?, upper?,
  basis: provider_reported | locally_measured | estimated | unknown,
  precision_or_rounding?, derivation_ref?
}

QuotaForecast {
  forecast_id, account_scope_ref, based_on_snapshot_ids,
  local_ledger_commit, policy_revision, cost_model_version,
  horizon, assumptions, confidence,
  per_window: {
    window_key, period_id?, reset_time_bounds?,
    unreflected_own_cost_range,
    future_external_cost_range_or_unknown,
    available_for_planning_range_or_unknown,
    predicted_exhaustion_time_range_or_unknown,
    predicted_remaining_at_horizon_range_or_unknown
  }[],
  subject_round_estimates: { horizon, all_models, cloud_only, per_profile },
  scenarios: { profile_mix, effort_policy, expected_useful_work, quota_cost_ranges }[],
  invalidated_by
}

QuotaPolicy {
  policy_id, revision, account_scope_refs,
  objective: preserve_until_reset | target_utilization | balanced,
  planning_horizon, target_utilization_range,
  emergency_reserve_by_window, uncertainty_margin,
  external_usage_assumption, stale_observation_policy,
  model_profiles: { profile_id, actual_model_ref, capability_requirements,
                    quality_floor, effort_bounds, output_budget_bounds }[],
  allowed_profile_mix, switching_hysteresis,
  subject_cloud_round_target?: { horizon, lower?, upper? },
  child_budget_policy, local_preference, available_fallbacks,
  maximum_authorized_spend, allow_overage, spending_authority_ref,
  observation_refresh_policy, snapshot_ttl_by_source,
  policy_change_authority_ref
}
```

1. `smart`、`cheap`、`local` 是可配置 profile，不是供应商保证的能力等级或价格顺序。真实模型、供应商分组及成本依据必须可追溯；同一模型对不同额度窗的计费权重可能不同。
2. `QuotaWindow` 是多维约束。五小时、七天、特定模型、组织与 workspace 限额可能同时约束同一请求；禁止求和当成可用总量。百分比、token、金额、请求数不混用单位。
3. context window、全局请求并发、execution lanes、KV / Host resident capacity 不在 `QuotaWindow` 中伪装成余额；仍由各自真实能力合同管理。额度充足不保证存在执行槽。
4. 时间过去只是旧窗失效条件。只有新观测或供应商明确保证的周期规则，才能产生带相应依据的新窗；不能仅删除旧窗就宣称余额全满。服务端临时 reset、调整计划或模型分组变更都使相关预测失效。
5. 多来源冲突保留原始快照，按该指标的来源、时效和已知覆盖范围决定当前视图。不能用最新本地估算覆盖较早服务端事实后丢掉不确定性。

## Q4. 观测、归因与账本边界

所有已派发请求关联账户、quota group、`request_id` / attempt、sealed model 参数与本地成本证据。请求完成、取消、断线和调用侧超时不等价于供应商免费或停止计费；仍不确定的请求保留费用区间。

管理报告或窗口百分比是某个时刻的汇总，不天然带有精确本地水位。适配器仅在有证据时填写 `covered_through`；没有覆盖证明时，预测必须保留 overlap 区间，不能把报告值加上全部本地请求造成双计，也不能为了避免双计而假定全部在途请求已结算。

若账户由其他客户端共享，短期余额变化可以更新 external consumption 的估算，但不能把全部变化都算成 Shenko 的真实消耗。无法观察其他客户端时，保持 `shared_unobserved`、储备余量与宽区间。管理员报告的数分钟延迟、CLI 的 last-known 数据、时钟偏移和舍入精度都纳入预测。

观测与预测记录采用 §41 的持久事实 / 派生视图边界；预测可重建，原始来源证据保留引用。额度插件不能通过丢弃 usage 事实或改动系统时间使余额恢复。

## Q5. 自适应算法的最低合同

控制器是可替换策略，以下只固定输入、约束与输出，不指定唯一算法。

对于有有限恢复周期的窗口 j，基于证据形成剩余量区间，扣除尚未反映的本地成本、预计外部消耗、紧急储备及不确定性余量，得到可规划预算区间。只有在单位一致、时间边界有效且区间可计算时，才以该预算除以距周期边界的时间，估算可持续消耗率。token bucket 的补充速率必须单独处理，不能套用“本周剩余额度”算法。

按真实观测学习不同模型 profile、effort、上下文长度、工具轮次和缓存情形的成本分布，结合主体有限 generation 的实际速率与 child 任务量，比较若干模型配比方案。给定一组 profile 配比，各个重叠窗口都必须评估，不能只优化周窗而提前触发五小时窗。

主体云端平均推理次数是可配置目标及预测量：控制器可让更多主体步转由本地或 cheap profile 承担，把 smart profile 留给更有收益的推理；也可以在预计大量额度到期作废时提高有价值任务的模型或 effort。不得为了达到次数目标而推迟一个本可继续运行的主体到下一低频 Tick。质量最低要求和任务实际需要优先于机械地维持某个 smart / cheap 比例。

策略输出包括：选用 profile、generation 预算、可接纳后台工作范围、储备理由和预测误差。持续比较预测与实际余额，调整成本模型与共享消耗估计；使用滞回与最小有效变化，避免模型来回切换。未知额度不等于零额度，也不等于无限额度；使用配置的未知状态策略，在已授权范围内继续、换本地、降低云端占比或等待真实可用容量。

目标是可测量的期末剩余区间、服务质量和连续工作能力。共享账户消费、突发消息、供应商策略变更和统计延迟使“恰好一周耗尽”无法成为硬保证。实际偏差要报告，不得通过无价值工作把指标补到 100%。

## Q6. 与事件、RequestSeal、KV 和执行调度的衔接

以下变化可形成耐久事件：`quota.observed`、`quota.observation_stale`、`quota.forecast_changed`、`quota.constraint_reached`、`quota.reset_observed`、`quota.policy_changed`、`model.routing_changed`。事件包含相关快照 / 预测 / 策略引用及有效期；按显著变化合并通知，避免每个 token 都给主体插入重复内容。主体的可读视图同时展示剩余区间、预计恢复时间、来源新旧、共享消费及当前模型选择理由。

**路由和语义预算必须在 RequestSeal 之前决定。** Provider、模型、effort、输出预算、上下文裁剪或工具声明变化，都需要新的 `request_id` 和 RequestSeal，并在同一 lineage 关联 `supersedes_request_id`。额度紧张不能在已 sealed 的请求内静默替换模型。已入队旧请求可以保留其决定，或经现有取消 / 替代合同退休后重封口；结果按原请求和合同解释。

模型 / tokenizer / engine / 模板改变时，原 ContextLease 或 KV 绑定只有在后端明确证明兼容时才可复用；默认不承诺跨模型继承 KV。缓存节省只能按真实计量修正成本估算，不能把预计缓存命中写成已支付量。模型切换可能增加 prefill，需要计入决策成本。

额度接纳与实际 ComputeAllocation 分离。额度侧的临时预算保留不能代替推理槽；请求 canceled、unknown、内部 paused 或连接断开时，是否释放物理容量仍按 §41 的真实后端状态处理。模型轮转及 smart / cheap 选择不越过全局 C 上限，也不在 C=1 时持有父请求槽等待一次依赖推理。

额度观测采用非模型的 Provider API / CLI 数据收集，不需要为查询额度占用模型推理 permit。若主体决定让 child 做预测分析，该 child 与其他请求公平使用相同 broker；策略算法本身无需调用 LLM。

## Q7. 验收场景

| 编号 | 注入条件 | 必须可观察的结果 |
|---|---|---|
| QA01 指标隔离 | 同时提供 API TPM header、workspace 继承限额、Pro/Max 七天百分比及 context length | 单位、账户和约束维度分开；不把 API refill 当订阅 reset，不把缺少 workspace override 当无限额；不把 context/KV 当余额 |
| QA02 缺省与陈旧 | statusline 尚未首次响应；仅有 five_hour；seven_day 过期被移除；`/usage` 返回 last-known 或查询自身 429 | 相应字段为未观察 / 陈旧 / 已失效；不能宣称全满或耗尽；查询失败不产生虚假的 inference admission 拒绝；新窗到来后预测按证据更新 |
| QA03 共享消费与延迟 | 其他 Claude 客户端突增消耗；管理报告延迟到达；一个本地 attempt 计费未知 | 保留共享归因与费用区间，修订预测；既不双计已覆盖请求，也不将未知取消成本退回；期末预测允许明显变化并给主体发送有依据的事件 |
| QA04 模型切换与 C=1 | 排队期间策略从 smart 改 cheap；旧 generation 已 armed 后连接断开；新模型 tokenizer/KV 不兼容 | 新输入新 seal / request_id；旧 attempt 不伪装完成或免费；在真实物理槽可用前不超并发；不跨模型冒用 KV；已收到旧结果按旧合同入账 |
| QA05 周期控制 | 回放一周有波动的主体与 child 工作、五小时和周双窗、外部随机消费，设置期末利用率目标 | 输出误差、期末剩余区间、质量与等待指标；自动调整模型配比 / effort / 本地份额；不生成无价值请求补余额，不改成低频主体 Tick，不声称必然精确用完 |
| QA06 自主性与消费授权 | 额度耗尽、观测插件暂时失效、供应商提供额外付费、存在本地 fallback | 主体收到准确状态，可自主改已有授权内策略并转本地；未授权时不自动提高费用上限或启用额外付款；没有任何可用后端时明确报告容量阻塞 |

## REF-QUOTA：官方来源与核验定位

核验日期：2026-10-22。以下 URL 是 Anthropic 官方当前文档，属于可变化的服务合同；实现应记录所依赖的 CLI / API 版本和接口证据，而非把本文日期视为永久保证。

1. **Claude 订阅窗口及跨产品共享**：<https://support.claude.com/en/articles/11647753-how-do-usage-and-length-limits-work>；定位 “Usage limits” 部分，五小时 / weekly、影响消耗的因素、跨 Claude 产品计入共享使用。研究引用 `turn217view3` L45–50。
2. **Claude Code statusline JSON**：<https://code.claude.com/docs/en/statusline>；定位 “Available data” 中 `rate_limits.*` 字段表及 rate limits 的缺省说明。研究引用 `turn217view1` / `turn219view7`：L264–267 是 five_hour / seven_day / spend_limit 字段，L410 是适用账号、首次响应、window 可缺失及过期移除。`spend_limit` 百分比 / reset 要求文档所列 v2.1.251+；美元 / period 字段要求 CLI 与 gateway 均 v2.1.284+。这些是观测适配器版本条件，不是 Shenko 核心最低 CLI 版本。
3. **Claude Code 用量统计与陈旧快照**：<https://code.claude.com/docs/en/costs>；定位 `/usage` 的 Session 费用含义、plan usage 与 “Showing last-known usage”。研究引用 `turn217view2` / `turn218view5`：L129–131、L201–203。不可用 Session 美元值替代订阅余额；观测端限流和推理限流分开。
4. **API Rate limits**：<https://platform.claude.com/docs/en/api/rate-limits>；定位 token bucket / response headers。研究引用 `turn218view1` / `turn219view8` / `turn219view9`：L156–157 连续 refill 与非保证容量，L347–359 headers / 时间 / 舍入，L366 当前更紧的 limiter。
5. **Configured Rate Limits API**：<https://platform.claude.com/docs/en/manage-claude/rate-limits-api>；定位 Authentication、organization / workspace endpoints 与 inherited limits。研究引用 `turn218view0` / `turn219view5` / `turn219view6`：L56、L63、L83、L179–184、L268–271。只读取被配置的限额，不能冒充消费订阅余额查询。
6. **Usage & Cost Admin API**：<https://platform.claude.com/docs/en/manage-claude/usage-cost-api>；定位 Authentication、Messages Usage、Cost report、Data freshness / polling FAQ。研究引用 `turn217view0` / `turn218view6` / `turn218view7` / `turn218view8`：L69–80 区分认证与产品，L139 / L261 endpoints，L326–332 通常完成后约五分钟可见及一次每分钟的持续查询支持。

以上六项覆盖本节事实依据；类型、归因账本、控制器、事件、seal/KV 衔接及验收是 Shenko 推导设计，不声称 Anthropic 已提供这些跨 Provider 合同。
