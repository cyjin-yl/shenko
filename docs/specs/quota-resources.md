# Shenko 额度资源 — 功能合同（quota-resources.md）

适用条款：SPEC §13.1/§13.7、§42.1–§42.8、§47.5；需求 R130；关联行为 B19、B20、B42；验收场景 QA01–QA06。本文是稳定的功能/接口/错误/恢复合同，不是任务板，也不宣称任何运行证据。

## 1. 职责边界

额度层负责：按**账户范围**（account scope）观测供应商周期额度、预测可规划预算、按策略决定准入节奏与模型选择、把变化写成耐久事件并让主体读得到。

不负责：推理传输与 provider 装配（`shenko-broker`，物理资源域/公平队列/KV 池是 broker 的合同）、主体上下文投影与准备（`shenko-context-policy`）、台账迁移与内容流日志的总体治理（`shenko-store` 聚合面由 ArchLedger 协调）、模型身份与 SEAL 协议（ArchSubject/ArchBroker）。

额度与计算许可、KV 驻留是三件事（§13.1）：`QuotaAdmission` 表示"这笔预算现在能不能花"，`ComputeAllocation` 表示"后端现在有没有槽位"。额度等待（`quota_policy_wait`）不占计算许可，容量等待不是额度状态，二者不合并。

## 2. 观测面与来源合同（§42.2）

每个观测面有自己的**来源合同**（`source_contract_ref`），不同合同的观测是不同事实，不可互换：

| 观测面 | 来源合同 | 证明什么 | 不能推出什么 |
| --- | --- | --- | --- |
| Claude Code statusline JSON | `anthropic:claude-code-statusline:v1` | 订阅共享的五小时/七天使用窗 | 不能换算成 token 数、请求数或美元余额 |
| API 响应 rate-limit 头 | `anthropic:api-rate-limit-headers:v1` | 该 API key 作用域的令牌桶与补充时间 | 补充时间不是订阅周期恢复时间；限流不是保证容量 |
| 管理 API 配置限额 | `anthropic:management-rate-limits:v1` | 组织/工作区**配置**的上限与继承关系 | 配置值不是实时剩余额度 |
| 管理 API 用量与成本 | `anthropic:management-usage-cost:v1` | 某周期**已消耗**的用量与成本（存在到达延迟） | 历史账单不是即时准入锁 |
| 本地实测表 | `local:measured-usage:v1` | 本节点账本内自己已派发尝试的消耗 | 不覆盖同账户其他客户端的消耗 |

缺省语义原样保留：statusline 的 `rate_limits` 只在适用登录且**首次 API 响应之后**出现，三个窗可独立缺失，过期窗被移除。缺失永远不是 0% 已用、100% 剩余或已耗尽；对应 `absent_windows` 记 `not_yet_observed`/`expired`/`unsupported`/`unknown`/`invalid`。`resets_at` 是 Unix epoch 秒；API 头的 refill 语义另行解析，令牌桶不套用周窗公式。

## 3. 账户范围与本地密钥绑定（§42.3/§47.5）

`ScopeIdentity` 是**供应商真实范围**，不同范围是不同约束，永不相加：

- `DefaultWorkspace`：未配置 workspace id 时供应商自己的默认范围。
- `Organization { organization_id }` / `Workspace { workspace_id, organization_id }`：组织限额与工作区覆盖；**没有自己的覆盖就是继承组织限额**，记为 `inherits_from`，不是"无限额"。
- `ApiKey { key_scope }`：单个 API key 的作用域。
- `Subscription`：claude.ai / Claude Code / Desktop 共享的消费者订阅。
- `LocalMeter`：本节点自己的用量表。
- `Unknown`：来源没有说出范围。缺字段不是默认身份（§47.5），未知保持未知。

`account_scope_ref` 是**秘密管理引用**，不含凭据。`SecretBinding` 只说凭据在哪（环境变量名 / 文件路径），凭据在读取的瞬间解析，不进 URL、不进证据、不进日志、不进台账。`AccountConfig`（本地 JSON）描述本节点能观测的账户：provider、真实范围、密钥绑定、管理面 origin、**每个来源合同的真实端点路径**、provider API 版本、轮询间隔。端点路径属于 API 版本，因此由本地配置给出，运行时不猜测。

密钥 scope、rate limit 配置、订阅 usage、历史账单与实时计算许可继续分开（§47.5）。

## 4. 插件协议与注册（§42.1）

额度观察者是可替换插件。协议版本 `QUOTA_OBSERVER_PROTOCOL_VERSION = 1`：

- `QuotaObserver`：`descriptor()` + `observe(&ObservationInput) -> Result<QuotaSnapshot, ObserveError>`。观察者**不做 I/O**：运行期取回真实载荷后交给它解析。
- `ObservationInput`：`StatuslineJson{json, first_response_seen}`、`ApiHeaders`、`ManagementLimits{json}`、`ManagementUsage{json}`、`LocalAttempts{attempts}`、`Unavailable{error, reason}`。读取失败是**不可用观测**，不是余额；`Unavailable` 的 `reason` 区分"还没产出过"与"读了但没说"。
- `QuotaProviderPlugin`：一个**具名 provider** 贡献它能被真实读取的合同观察者；`QuotaPluginRegistry::register_plugin` 按协议版本、描述完整性与重复性拒绝安装。
- `ProviderQuotaPlugin`：把本地账户配置里声明的 (账户范围, 来源合同) 变成观察者实例。**未配置的 provider 没有插件**，未配置端点的合同不被读取；没有"默认存在的 Anthropic 观察者"。
- `observe` 永不发起付费探测；statusline 只读本地已存在的 CLI 会话文档。

## 5. 持久事实（`shenko-store/src/quota.rs`）

| 表 | 事实 |
| --- | --- |
| `quota_snapshots` | 一次观测（provider、账户范围、来源合同、范围身份、原始证据、观测状态） |
| `quota_forecasts` | 一次预测（绑定来源 snapshot、options、逐窗规划） |
| `quota_policies` | 一个策略修订（profile、假设、准入节奏、来源承认表） |
| `quota_decisions` | 一次决策（绑定策略字节 digest、所引预测、可用性、选择、逐窗约束、有效期） |
| `quota_pacer` | 单写者预留/节奏状态（每请求一条 reservation） |
| `quota_attempts` | 已派发尝试的计费行（request/window/账本 commit 三元身份） |
| `quota_sends` | 一次 wire send 的血缘（request→send、claim 序号） |
| `quota_events` | §42.6 的耐久事件 |

写入规则：同 id 同字节幂等，同 id 不同字节是冲突；预测必须引用已记录快照；决策必须引用已记录策略修订与预测，且**所引预测的来源必须被该策略在同合同、同 options 下承认**（`SourceObservationPolicy.contract`）。`quota_events` 与造成它的事实在**同一事务**提交：观测事件与快照同事务，约束事件与准入同事务，预测变化事件与预测同事务，路由变化事件与决策同事务。

事件种类：`quota.observed`、`quota.observation_stale`、`quota.forecast_changed`、`quota.constraint_reached`、`quota.reset_observed`、`quota.policy_changed`、`model.routing_changed`。事件带 provider、账户范围与所依据的引用。

## 6. 预测、预留、计费与对账（§42.5）

- **来源绑定**：预测只能从被策略承认的合同的读数计算；策略不承认的读数不参与规划。多个来源冲突时保留原始快照，按来源、时效与已知覆盖范围决定当前视图；最新的本地估算不覆盖较早的服务端事实后丢掉不确定性。
- **覆盖**：`covered_through` 只由本地实测表给出（它包含本账本某 commit 之前的全部尝试）。没有覆盖证明时用区间，避免双计，也不假定在途请求已结算。
- **预留身份**：预留绑定 (request, window, 单位, 账本 commit)。**单调事实**：已 armed/未知计费的预留跨更新决策、跨来源修订、跨跨期重算都保留历史；只有**确实未发送**（`NeverDispatched`）的取消释放预算。较旧决策不能因为较新预测的覆盖而复用额度。
- **对账**：已 admitted 或已 armed 的请求在新决策下**对账**而不是重新准入：保留 armed 尝试行、计费历史、`admitted_at` 与节奏义务，不二次计费；覆盖不上则进入等待而不是死锁。
- **单调单位/账户**：百分比、token、金额、请求数不混用；窗口、单位、账户范围在预留、尝试、结算三处保持同一身份。

## 7. 准入与路由（§42.5/§42.6）

`QuotaAdmission.state`：`eligible | quota_policy_wait | provider_exhausted | observation_blocked | admitted | cancelled`。

- `quota_policy_wait`：策略为跨周期分配暂缓；主体仍有唯一 runnable 后继，不占计算许可，不冒称供应商已耗尽。
- `provider_exhausted`：必须有实际限额依据（provider 报告的 hard limit 且余额独立为零）；本地估算扣空、reserve 扣空、last-known 陈旧读数都不是耗尽。
- `observation_blocked`：来自策略明确配置的未知处理。
- 容量不可用是 broker 的 capacity wait，不是额度状态。

路由与语义预算在 **RequestSeal 之前**决定：被准入 profile 的 `model_ref` 必须等于封口绑定，否则 `MODEL_PROVIDER_BINDING_MISMATCH`；额度紧张不会在已封口请求内静默换模型，换模型需要新的 `request_id` 与封口。

准入路径（`shenko-subject/src/quota_gate.rs`，主体与有限模型 child 共用）：读最新策略 →（过期时）按已记录快照与 `quota_attempts` 重算预测 → decide/record → `Store::admit_quota_send` → `Store::arm_quota_send` → step CAS **同事务** claim 精确 send_id → `broker.submit` → 响应观测与结算。claim 是持久派发授权，不是已发送证据。

**已知接口缝**：当前 gate 的 backend 可用性由封口绑定推导（profile 本地/云与 `model_ref` 匹配）。真实"该 provider/model 此刻确实可跑"应由 broker 的实际 provider 集合给出；该访问器就绪前，此处仍是绑定推导，文档如实标注，不冒充实时资格检查。

**已知未闭合项（封口前路由）**：§42.6 要求路由与语义预算在 RequestSeal **之前**决定。当前顺序是"先封口、再准入"，因此当策略决策选出的 profile 与封口模型不同时，结果是明确的 `MODEL_PROVIDER_BINDING_MISMATCH` 失败，而不是改路由。把决策前移到 S0 之前、并让封口使用被选中的模型，需要 subject 的封口路径与 broker 的 provider 装配共同改动（ArchSubject/ArchBroker 与主控），额度层不单方面改封口协议，也不放宽比较或引入模型名许可名单。

## 8. 主体可读视图与事件（§42.6）

`QuotaScopeView` 是投影，不是新的来源：剩余量是**区间**（没有就是 unknown）、预计恢复时间（周期 `resets_at` 与桶 `replenishes_by` 分开）、来源合同与新鲜度、共享消费归因（`exclusive | shared_known | shared_unobserved | unknown`）、当前决策选中的 profile/model 与理由。渲染有界（最多 8 个窗，其余留在耐久快照）。

事件到达主体：运行期把耐久事件合并成每个账户范围一条通知，写入耐久 inbox 事件（`admit_inbox_event`）并向主体 inbox 发送唤醒。合并规则是"显著变化"：同一范围的多个事件合并为该范围最新读数；内容未变不重复通知。视图事件 id 由 (provider, 范围, 内容) 稳定派生，重复发布是幂等的。事件流按批读取（每批 128 条，每轮最多 64 批）：突发事件不会把某个范围的最新事件藏在批次上限之后，剩下的留给下一轮，因此既不丢事件也不饿死范围。

## 9. 运行时接线（`bin/shenko`）

- `SHENKO_QUOTA_POLICY`：一个序列化 `QuotaPolicy` 的文件。启动时记录；不可读/非法即启动失败，不静默按无额度运行。已存在。
- `SHENKO_QUOTA_ACCOUNTS`：一个序列化 `AccountConfig` 的文件（本次新增）。启动时装配 `QuotaRuntime`：按 provider 注册插件、构建管理面客户端、启动采集/通知循环。未设置=没有观察者、没有采集；设置但不可读或非法=启动失败。
- 采集循环每 30 秒一轮；每个账户按自己的 `refresh_interval_secs`（默认 300 秒）到期才读一次。statusline 读本地文件：文件不存在=该会话还没产出文档，**不记录任何观测**（没有观测就没有事实）；文件不可读=记录不可用观测。管理面按本地配置的路径 GET，凭据即时解析，401/403→`unauthorized`，429→`rate_limited`，超时→`timeout`，其它非 2xx→`invalid_data`，都记为不可用观测。
- 普通流量的观测不需要采集器：`anthropic-ratelimit-*` 响应头随每次真实响应进入 `InferenceResult.quota_headers`，由准入路径转成快照并记录。
- 额度观测不占用模型推理许可：它是运行期后台服务，不是主体循环的一步。

本地账户配置示例（一个文件、多个账户；每个账户一个真实范围）：

```json
{
  "accounts": [
    {
      "provider_id": "anthropic",
      "scope": {
        "scope_ref": "anthropic:workspace:ws-1",
        "provider_id": "anthropic",
        "identity": { "kind": "workspace", "workspace_id": "ws-1", "organization_id": "org-1" }
      },
      "secret": { "source": "environment", "reference": "ANTHROPIC_ADMIN_KEY" },
      "management_base_url": "https://api.anthropic.com",
      "api_version": "2026-10-21",
      "refresh_interval_secs": 300,
      "endpoints": [
        { "source_contract_ref": "anthropic:claude-code-statusline:v1",
          "endpoint": { "kind": "statusline_file", "path": "/home/user/.claude/statusline.json" } },
        { "source_contract_ref": "anthropic:management-rate-limits:v1",
          "endpoint": { "kind": "management_api", "path": "/v1/organizations/rate_limits" } }
      ]
    }
  ]
}
```

端点路径由本地配置给出，因为路径属于 provider 的 API 版本；运行时不猜测端点，也不为探测余额额外启动付费生成。

## 10. 错误与恢复

- 观测失败与额度耗尽分开：查询自身被限流不是推理额度耗尽；失败观测不产生虚假的准入拒绝。
- 读取失败只保留类型化分类，原始响应体、URL、凭据都不进证据。
- 未知保持未知：解析不出的字段不会被猜成数字；无法确定的量返回 unknown 而不是凑数。
- 观测器拒绝载荷（来源不匹配）只记录告警，不影响其它来源。
- 台账写入被拒（幂等冲突）记录告警，不覆盖既有事实。
- 主体通知写不进耐久 inbox（无 head 等）记录告警；耐久事件本身仍可在台账查询。
- 一轮采集/陈旧检查/通知失败**不停止**本消费者，也不影响主体主循环（额度观测是可选连接器）；失败按本消费者自己的 callsite 写成耐久 runtime_error 事实（§45.14，主体可用 errors.read 读到），journal 写入本身失败只计数不外溢。

## 11. 完成状态

已实现（源码）：来源合同与插件协议、账户范围与本地密钥绑定、具名 provider 插件注册、statusline/管理面/本地表三类观测器、耐久快照/预测/策略/决策/预留/尝试/send/事件与事件合并、预测变化与陈旧观测事件、运行期采集与主体可读视图通知、`bin/shenko` 启动装配。

尚未取得运行证据：真实供应商账户上的观测与管理面端点（端点路径随 provider API 版本变化，由本地配置给出，未在本轮核对真实端点）、真实云账户额度行为、模型切换后的实际用量学习、跨周期（一周以上）误差统计。已知未闭合项：backend 实时资格检查仍为绑定推导（等 broker 的实际 provider 访问器）；封口前路由未实现（见 §7）；`covered_through` 目前只有本地实测表一个生产者；多 scope 观测仍按策略绑定范围逐个处理。
