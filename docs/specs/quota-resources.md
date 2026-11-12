# Shenko 额度资源 — 功能合同（quota-resources.md）

**内核定位。** 参考插件，建立在[内核合同](kernel.md)之上。额度是准入策略和带来源的观察，不是内核规则；它作用于[内核 5](kernel.md#kernel-5)的推理效果，经[内核 7](kernel.md#kernel-7)接入。

适用条款：[§13.1](quota-resources.md#spec-13-1)、[§13.7](inference.md#spec-13-7)、[§42.1](quota-resources.md#spec-42-1)、[§42.8](quota-resources.md#spec-42-8)、[§47.5](inference.md#spec-47-5)；需求 R130；关联行为 B19、B20、B42；验收场景 QA01–QA06。本文是稳定的功能/接口/错误/恢复合同，不是任务板，也不宣称任何运行证据。

## 1. 职责边界

额度层负责：按**账户范围**（account scope）观测供应商周期额度、预测可规划预算、按策略决定准入节奏与模型选择、把变化写成耐久事件并让主体读得到。

不负责：推理传输与 provider 装配（`shenko-broker`，物理资源域/公平队列/KV 池是 broker 的合同）、主体上下文投影与准备（`shenko-context-policy`）、台账迁移与内容流日志的总体治理（`shenko-store` 聚合面由 ArchLedger 协调）、模型身份与 SEAL 协议（ArchSubject/ArchBroker）。

额度与计算许可、KV 驻留是三件事（[§13.1](quota-resources.md#spec-13-1)）：`QuotaAdmission` 表示"这笔预算现在能不能花"，`ComputeAllocation` 表示"后端现在有没有槽位"。额度等待（`quota_policy_wait`）不占计算许可，容量等待不是额度状态，二者不合并。

## 2. 观测面与来源合同（[§42.2](quota-resources.md#spec-42-2)）

每个观测面有自己的**来源合同**（`source_contract_ref`），不同合同的观测是不同事实，不可互换：

| 观测面 | 来源合同 | 证明什么 | 不能推出什么 |
| --- | --- | --- | --- |
| Claude Code statusline JSON | `anthropic:claude-code-statusline:v1` | 订阅共享的五小时/七天使用窗 | 不能换算成 token 数、请求数或美元余额 |
| API 响应 rate-limit 头 | `anthropic:api-rate-limit-headers:v1` | 该 API key 作用域的令牌桶与补充时间 | 补充时间不是订阅周期恢复时间；限流不是保证容量 |
| 管理 API 配置限额 | `anthropic:management-rate-limits:v1` | 组织/工作区**配置**的上限与继承关系 | 配置值不是实时剩余额度 |
| 管理 API 用量与成本 | `anthropic:management-usage-cost:v1` | 某周期**已消耗**的用量与成本（存在到达延迟） | 历史账单不是即时准入锁 |
| 本地实测表 | `local:measured-usage:v1` | 本节点账本内自己已派发尝试的消耗 | 不覆盖同账户其他客户端的消耗 |

缺省语义原样保留：statusline 的 `rate_limits` 只在适用登录且**首次 API 响应之后**出现，三个窗可独立缺失，过期窗被移除。缺失永远不是 0% 已用、100% 剩余或已耗尽；对应 `absent_windows` 记 `not_yet_observed`/`expired`/`unsupported`/`unknown`/`invalid`。`resets_at` 是 Unix epoch 秒；API 头的 refill 语义另行解析，令牌桶不套用周窗公式。

## 3. 账户范围与本地密钥绑定（[§42.3](quota-resources.md#spec-42-3)、[§47.5](inference.md#spec-47-5)）

`ScopeIdentity` 是**供应商真实范围**，不同范围是不同约束，永不相加：

- `DefaultWorkspace`：未配置 workspace id 时供应商自己的默认范围。
- `Organization { organization_id }` / `Workspace { workspace_id, organization_id }`：组织限额与工作区覆盖；**没有自己的覆盖就是继承组织限额**，记为 `inherits_from`，不是"无限额"。
- `ApiKey { key_scope }`：单个 API key 的作用域。
- `Subscription`：claude.ai / Claude Code / Desktop 共享的消费者订阅。
- `LocalMeter`：本节点自己的用量表。
- `Unknown`：来源没有说出范围。缺字段不是默认身份（[§47.5](inference.md#spec-47-5)），未知保持未知。

`account_scope_ref` 是**秘密管理引用**，不含凭据。`SecretBinding` 只说凭据在哪（环境变量名 / 文件路径），凭据在读取的瞬间解析，不进 URL、不进证据、不进日志、不进台账。`AccountConfig`（本地 JSON）描述本节点能观测的账户：provider、真实范围、密钥绑定、管理面 origin、**每个来源合同的真实端点路径**、provider API 版本、轮询间隔。端点路径属于 API 版本，因此由本地配置给出，运行时不猜测。

密钥 scope、rate limit 配置、订阅 usage、历史账单与实时计算许可继续分开（[§47.5](inference.md#spec-47-5)）。

## 4. 插件协议与注册（[§42.1](quota-resources.md#spec-42-1)）

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
| `quota_events` | [§42.6](quota-resources.md#spec-42-6) 的耐久事件 |

写入规则：同 id 同字节幂等，同 id 不同字节是冲突；预测必须引用已记录快照；决策必须引用已记录策略修订与预测，且**所引预测的来源必须被该策略在同合同、同 options 下承认**（`SourceObservationPolicy.contract`）。`quota_events` 与造成它的事实在**同一事务**提交：观测事件与快照同事务，约束事件与准入同事务，预测变化事件与预测同事务，路由变化事件与决策同事务。

事件种类：`quota.observed`、`quota.observation_stale`、`quota.forecast_changed`、`quota.constraint_reached`、`quota.reset_observed`、`quota.policy_changed`、`model.routing_changed`。事件带 provider、账户范围与所依据的引用。

## 6. 预测、预留、计费与对账（[§42.5](quota-resources.md#spec-42-5)）

- **来源绑定**：预测只能从被策略承认的合同的读数计算；策略不承认的读数不参与规划。多个来源冲突时保留原始快照，按来源、时效与已知覆盖范围决定当前视图；最新的本地估算不覆盖较早的服务端事实后丢掉不确定性。
- **覆盖**：`covered_through` 只由本地实测表给出（它包含本账本某 commit 之前的全部尝试）。没有覆盖证明时用区间，避免双计，也不假定在途请求已结算。
- **预留身份**：预留绑定 (request, window, 单位, 账本 commit)。**单调事实**：已 armed/未知计费的预留跨更新决策、跨来源修订、跨跨期重算都保留历史；只有**确实未发送**（`NeverDispatched`）的取消释放预算。较旧决策不能因为较新预测的覆盖而复用额度。
- **对账**：已 admitted 或已 armed 的请求在新决策下**对账**而不是重新准入：保留 armed 尝试行、计费历史、`admitted_at` 与节奏义务，不二次计费；覆盖不上则进入等待而不是死锁。
- **单调单位/账户**：百分比、token、金额、请求数不混用；窗口、单位、账户范围在预留、尝试、结算三处保持同一身份。

## 7. 准入与路由（[§42.5](quota-resources.md#spec-42-5)、[§42.6](quota-resources.md#spec-42-6)）

`QuotaAdmission.state`：`eligible | quota_policy_wait | provider_exhausted | observation_blocked | admitted | cancelled`。

- `quota_policy_wait`：策略为跨周期分配暂缓；主体仍有唯一 runnable 后继，不占计算许可，不冒称供应商已耗尽。
- `provider_exhausted`：必须有实际限额依据（provider 报告的 hard limit 且余额独立为零）；本地估算扣空、reserve 扣空、last-known 陈旧读数都不是耗尽。
- `observation_blocked`：来自策略明确配置的未知处理。
- 容量不可用是 broker 的 capacity wait，不是额度状态。

路由与语义预算在 **RequestSeal 之前**决定：被准入 profile 的 `model_ref` 必须等于封口绑定，否则 `MODEL_PROVIDER_BINDING_MISMATCH`；额度紧张不会在已封口请求内静默换模型，换模型需要新的 `request_id` 与封口。

准入路径（`shenko-subject/src/quota_gate.rs`，主体与有限模型 child 共用）：读最新策略 →（过期时）按已记录快照与 `quota_attempts` 重算预测 → decide/record → `Store::admit_quota_send` → `Store::arm_quota_send` → step CAS **同事务** claim 精确 send_id → `broker.submit` → 响应观测与结算。claim 是持久派发授权，不是已发送证据。

**已知接口缝**：当前 gate 的 backend 可用性由封口绑定推导（profile 本地/云与 `model_ref` 匹配）。真实"该 provider/model 此刻确实可跑"应由 broker 的实际 provider 集合给出；该访问器就绪前，此处仍是绑定推导，文档如实标注，不冒充实时资格检查。

**已知未闭合项（封口前路由）**：[§42.6](quota-resources.md#spec-42-6) 要求路由与语义预算在 RequestSeal **之前**决定。当前顺序是"先封口、再准入"，因此当策略决策选出的 profile 与封口模型不同时，结果是明确的 `MODEL_PROVIDER_BINDING_MISMATCH` 失败，而不是改路由。把决策前移到 S0 之前、并让封口使用被选中的模型，需要 subject 的封口路径与 broker 的 provider 装配共同改动（ArchSubject/ArchBroker 与主控），额度层不单方面改封口协议，也不放宽比较或引入模型名许可名单。

## 8. 主体可读视图与事件（[§42.6](quota-resources.md#spec-42-6)）

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
- 一轮采集/陈旧检查/通知失败**不停止**本消费者，也不影响主体主循环（额度观测是可选连接器）；失败按本消费者自己的 callsite 写成耐久 runtime_error 事实（[§45.14](cognition-units.md#spec-45-14)，主体可用 errors.read 读到），journal 写入本身失败只计数不外溢。

## 11. 完成状态

已实现（源码）：来源合同与插件协议、账户范围与本地密钥绑定、具名 provider 插件注册、statusline/管理面/本地表三类观测器、耐久快照/预测/策略/决策/预留/尝试/send/事件与事件合并、预测变化与陈旧观测事件、运行期采集与主体可读视图通知、`bin/shenko` 启动装配。

尚未取得运行证据：真实供应商账户上的观测与管理面端点（端点路径随 provider API 版本变化，由本地配置给出，未在本轮核对真实端点）、真实云账户额度行为、模型切换后的实际用量学习、跨周期（一周以上）误差统计。已知未闭合项：backend 实时资格检查仍为绑定推导（等 broker 的实际 provider 访问器）；封口前路由未实现（见 [§7](plugins.md#spec-7)）；`covered_through` 目前只有本地实测表一个生产者；多 scope 观测仍按策略绑定范围逐个处理。

<a id="spec-13-1"></a>
<!-- migrated-contract: SPEC:13.1 -->
## 13.1 三种资源必须分开

| 资源 | 含义 | 典型行为 |
| --- | --- | --- |
| 逻辑 Agent/task | 已存在的主体与工作任务 | 可以很多；等待时可持久停放 |
| 计算许可 C | 后端同时处理的 generation 请求 | C=1 时一次只有一个执行者 |
| 上下文驻留池 K | 可保留的 KV/模型 recurrent state 与相关缓存 | 可以同时保留主体和多个子 Agent，独立于 C |

**单并发不等于单上下文。** 主体计算完成后让出计算许可，其上下文仍可留在池里；子 Agent 使用另一个驻留上下文完成一步，再轮到主体。这正是本地单 GPU 场景需要正式支持的执行方式。缓存池解决重建成本，轮转调度解决计算机会，二者不可混为一项 semaphore。

所有请求必须进入同一 InferenceBroker，包括主体、孩子、记忆、重排、摘要、judge、ASR/VLM/声纹插件在共享设备上的推理。resource group 对应实际共享设备/engine，不是一个 URL；同 engine 的多个模型别名、Chat/Responses 路由、不同 host alias 不得获得独立满额上限。

严格“全局”只覆盖受控消费者。若其他应用可直连同一服务，它们也需走代理；否则必须有可强制执行且已从共享总额扣除的容量预留，才能保持相应硬保证。无法证明其上限时，整体保证降为共享负载下的软约束。插件不能用任意网络请求绕过已声明的模型入口。有模型调用的外部 memory 服务同样纳管；独立预算仅适用于不共享相关物理约束的资源域，不能用另一服务名把同一 GPU 的额度重复计算。

<!-- /migrated-contract: SPEC:13.1 -->

<a id="spec-13-2"></a>
<!-- migrated-contract: SPEC:13.2 -->
## 13.2 自动分配中的半池情形

根据用户最后澄清，比例与算法属于可替换 KVPolicy/BrokerPolicy 插件；在适合均分的理想条件下，自动算法应自行得到主/子约 50/50。不能把要求退化为“提供一个手动 half-pool 开关”。内核不硬编码 0.5，负载变化后算法应选择更合适的比例。

该验收场景中，扣除模型/运行暂存与安全余量后，可用上下文池为 K，C=1；主体与子 Agent 集体的有效驻留需求及保留收益使均分成为合理分配。算法无需用户填写比例，即分配约 K/2 给主体、K/2 给所有孩子，容忍真实 page 粒度和必要 headroom 的偏差。每个孩子不能各拿半池。验收同时改变主上下文大小、孩子工作集和换入成本，确认算法会离开不再合适的 50/50，而不是碰巧硬编码命中测试。

在真实支持分配/保留/换入换出的后端上，主上下文不因为孩子运行一步而被驱逐。子池按近期返回概率、优先级与重建成本保留多个 context；放不下时降到 host RAM/磁盘快照或只保留原文，具体取决于后端能力。等待 worker 不占计算许可，但可持有受限缓存租约。

不能用“给请求加一个 slot ID”冒充容量分区。若 backend 没有保留/配额控制，仅有 prefix cache，则该模式只能是软策略；UI 标明 soft/advisory，不能勾选硬保留验收。必要时开发针对该 backend 的扩展协议或适配补丁，完整目标仍保留。

半池也不是无限上下文承诺。若主体真实状态超过调度器当前赋予的可用份额，应按策略扩借空闲容量、缩短投影/切窗、spill 或明确排队，不能驱逐有保证的其他池后仍报告硬隔离。混合架构模型需要计入 recurrence/StateImage 等非 KV 状态；跨模型 token 数不能直接相加假装等价字节。

对算法在某个周期选出的配额，应区分硬上限与可收回权益：硬上限约束当前实占；可借用权益允许暂借闲置容量，界面分别显示 entitled、used、borrowed、reclaim-pending。自动策略可在安全边界更新配额，不要求永久保留该次比例。借用回收受实际迁移确认约束，不能把“稍后能腾出来”立即当空闲。共享 prefix 的物理页只计一次，释放某个逻辑 context 可能没有释放任何物理字节；公平归账与真实占用分开。

主体可为了任务、窗口与缓存保留多个 context，但它们合计使用调度器赋予主体的份额；在本均分验收场景中约为 K/2，不能每个 context 再领 K/2。D1 不要求按私聊/群聊做知识隔离，只有当前选定窗口进行下一步串行推理；记忆可跨这些窗口回忆。故障分叉的物理开销另按 [§5.4](nodes.md#spec-5-4) 和实际后端预算如实记账。

<!-- /migrated-contract: SPEC:13.2 -->

<a id="spec-13-3"></a>
<!-- migrated-contract: SPEC:13.3 -->
## 13.3 自动分配与主体调控

正式默认路线是算法自动，或主体给偏好、算法执行。固定比例只可作为选装调试/对照策略，不能用它替代自动分配验收。主体可在决定中给出注意力、预期返回时间、哪些任务应保留热上下文、期望主子份额、延迟/吞吐偏好和候选重置。它不直接写配置文件，也不能提高 owner 设置的硬并发/成本/内存上限。

建议算法按三个时间尺度工作：每请求选择下一个计算者；每次 context 生命周期变化决定保留/借用/回收；较慢周期根据真实命中、队列和内存压力调整份额。大体目标是降低“队列等待 + 重建 prefill + 换入换出”的加权代价，同时满足唯一主体、硬上限、期限与公平性。没有完整后端信息时不宣称全局最优。

驻留价值可由近期恢复概率、未缓存重建耗时、任务优先级和状态字节估计；主循环连续使用，因此通常有较高保留价值。孩子因长时间 SSH/搜索而暂时离开时，可借出未占用预算，但只在不破坏当前硬保留承诺时执行。回收发生在安全边界，避免频繁抖动；用滞回和最小驻留时间减少刚换出又换入。

**K 可以不填。** 首选 backend capability/配置/metrics 发现有效池与状态需求；其次使用有可信 tokenizer/模型配置支持的保守估算；信息不足时启用“容量未知”模式：从低并发和有限热 context 起步，依据 cache hit、延迟、内存压力与可观测拒绝谨慎调整。不能以不断触发 OOM 当探测算法，也不能在无法控制 engine eviction 时宣称已保证半池。

建议对信息不足的后端做慢速增加、资源压力时快速回退，并保存此前健康配置。可配置上限、估算置信区间、backend 的有效 slot 数、当前实际占用都在诊断页展示；未知值显示未知，不用模型推测填成确定数字。

### 自动策略的可执行算法规格

采用有界在线启发式，输入为 C、各 typed pool 容量/置信、ready DAG、deadline/aging、主体偏好、context 的实际恢复 frontier 与各 tier 副本，以及实测 prefill/搬运/切换代价。可选动作只能来自 adapter 声明的真实能力；不支持 pin/fork/spill 的后端不能进入相应候选方案。

每次资源状态变化时依次执行：

1. 先按紧急程度、公平份额、老化与依赖继承选出本轮必须推进的任务集合。

2. 在当前方案、按可观测工作集/成本算出的候选、对称均分候选及相邻份额/驻留方案之间作有限枚举；大型工作集用按边际收益排序与局部交换，限制 planner 自身耗时。均分只是候选，在相应条件下由成本/公平约束选中，不能无条件优先。

3. 检查完整资源向量：现有占用、正在使用的 source、已预约 destination、下一执行单元增长与临时峰值。最终能装下但迁移途中装不下的方案也不可行。

4. 对可行方案比较加权排队、搬运、未覆盖 prefill/replay 和预计后续重建成本。某个 context 的保留价值取决于下次复用概率与省下的真实成本，而非 token 越多越好。

5. 可选的性能优化仅在预计收益超过迁移成本与不确定性余量时切换，使用最小保持期与滞回防止频繁抖动。为满足公平服务、必要依赖推进、权限撤销或物理资源安全而必需的切换，不受该收益门槛阻止；不能因为热主体更便宜就永久阻止冷孩子恢复。

6. reserve → transfer/close → confirm → publish/release；实际状态变化后重新计算，不提前释放尚有 reader 或迁移未完成的空间。

7. 记录实际命中、队列、恢复时间和公平结果，更新下一周期估计；小规模离线 oracle 与固定策略用于以后评测“接近优良”的程度。

`resume_cost = transfer_time + uncovered_prefill_or_replay + switch_overhead` 是成本组成定义，不是统一硬件常数。每请求 quantum 通常是一段有限输出的完整调用；只有后端真正支持 pause/resume 才能更细。短 quantum 降低等待，但可能增加频繁换入；算法需要测总成本，不能把“每 token 轮转”当免费实现。

父任务释放 compute 后仍可能因硬 pin 占满状态池阻塞孩子。因此创建必要依赖前必须确认至少一个孩子有可推进路径；父 context 可按策略 spill，不能不可撤销地 pin 全池后无限等孩子。资源不足可报告明确 infeasible 或调整策略，不能悄悄越过硬上限。

<!-- /migrated-contract: SPEC:13.3 -->

<a id="spec-13-4"></a>
<!-- migrated-contract: SPEC:13.4 -->
## 13.4 计算顺序与无死锁规则

D3 已选择外部消息通常为最高紧急，允许打断 child；紧急程度与管理员权限分别处理。连续消息令孩子无进展时，进度插件向主体提出免打扰建议，由主体决定是否保护工作、时长、范围和结束后队列处理。公平性不靠永久压低来信优先级实现，也不把消息风暴下的饥饿藏起来；具体协议见 [§13.8](subject-runtime.md#spec-13-8)。

C=1 不空留一个永远闲置的紧急 slot。主体持续申请下一轮，children 在真实边界公平推进；外部消息可按 D3 请求暂停或取消孩子，重算成本如实记录。后端不支持请求内抢占时，有限 generation 结束后轮转仍可工作；不能把 priority 参数当成任意中途抢占保证。频繁打断时由主体决定免打扰，不改成低频 Tick。

父任务等待孩子时不仅要释放 inference permit，也要释放孩子启动必需的 worker 执行席位；逻辑 parked parent 不算 running worker。禁止持有 SQLite 写事务/工作区独占锁等待 LLM。分配多个资源采用可回滚预留或一致顺序，不先占一半再无限等待另一半。

所有请求入队前验证：渲染后的输入 token + 输出预留 + 模板/工具/多模态开销不超过有效窗口；有效窗口取模型、backend 与用户设置的共同限制。按 uncached prefill 量和状态预算 admission，不只数 HTTP 连接。

连接断开、客户端超时、本地 lease 到期不证明服务器已停止。取消进入 cancellation-pending；确认实际请求终止、实例销毁或能证明旧负载不再占用该物理资源域，才回收容量。仅逻辑 fencing、断网或换路由不足以释放 C。同一分支旧 attempt 未决时不得启动第二个可提交续写；按 D11 在线优先建立的显式新分支可使用独立可用资源域，不能靠改 branch ID 绕过原 Engine 的占用与全局上限。

<!-- /migrated-contract: SPEC:13.4 -->

<a id="spec-42"></a>
<!-- migrated-contract: SPEC:42 -->
# 42 Provider 周期额度、模型选择与持续思考的自适应调优

状态：可并入 Shenko SPEC 的研究合同；对应 U41 / B42，来源 REF-QUOTA。本节不是 V1 必须接入的云服务依赖，不改变优先本地推理、插件化主干、持续主体或 [§41](architecture.md#spec-41) 的耐久执行合同。以下接口、字段、控制策略与验收是 Shenko 设计；上游已提供的接口另行标明。

<!-- /migrated-contract: SPEC:42 -->

<a id="spec-42-1"></a>
<!-- migrated-contract: SPEC:42.1 -->
## 42.1 产品目标与边界

额度插件帮助主体知道某个账户还有多少可用资源、何时可能恢复、这些信息有多可靠，并在已有授权内自主选择 smart / cheap / local 模型、推理预算及任务分配。它可以追求“在整个周期内持续取得有用成果，接近周期结束时用到目标额度”，但不得承诺精确用完一周额度。

主体平均推理次数以有限 generation 为统计单位：分别统计全部主体步、云端主体步、不同模型的主体步和 child 请求。调整模型配比、每次推理的 effort / 输出预算、委派策略、云端承担比例以及主体在指定时间窗内的平均准入轮数。主体后继始终已登记；额度策略可以让 runnable 请求等待预算释放，并把原因记录为 quota_policy_wait。这与没有消息就休眠到低频 Tick 不同，也不冒称等待期间仍在进行模型推理；不生成没有任务价值的请求来消耗余额。没有任何可用推理后端时，必须报告真实的 capacity-blocked 状态；不能把不能执行包装成仍在持续思考。

QuotaObserver、QuotaForecaster、QuotaPolicy 及其 Provider 适配器均为可替换插件。主体可读这些插件给出的事实、预测与策略，并在现有资源和消费授权内修改策略；无须逐次询问用户。新增付款、提高用户已设费用上限、开启未授权的额外付费，不由“优化已购额度”自动推出授权。

<!-- /migrated-contract: SPEC:42.1 -->

<a id="spec-42-2"></a>
<!-- migrated-contract: SPEC:42.2 -->
## 42.2 已核验的 Anthropic 观测面

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

API 管理字段的解释同时绑定实际 `anthropic-version` 和 SDK/schema revision。2026-10-21 的 rate-limit、key scope 与 Enterprise Analytics 字段变化见 [§47](inference.md#spec-47)；缺字段表示未知或不适用，不能按旧字段缺省推断为额度充足。

<!-- /migrated-contract: SPEC:42.2 -->

<a id="spec-42-3"></a>
<!-- migrated-contract: SPEC:42.3 -->
## 42.3 规范类型

以下标识均在其账户 namespace 内稳定。`account_scope_ref` 是秘密管理引用，不包含凭据；`DataRef`、`CommitRef` 等沿用 [§41](architecture.md#spec-41)。数组和可空字段不是省略错误语义的许可。

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
  subject_round_target?: { horizon, scope: all | cloud_only, lower?, upper? },
  admission_pacing: { enabled, burst_policy_ref, next_eligibility_policy_ref,
                      urgent_reserve_policy_ref, minimum_progress_policy_ref },
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

<!-- /migrated-contract: SPEC:42.3 -->

<a id="spec-42-4"></a>
<!-- migrated-contract: SPEC:42.4 -->
## 42.4 观测、归因与账本边界

所有已派发请求关联账户、quota group、`request_id` / attempt、sealed model 参数与本地成本证据。请求完成、取消、断线和调用侧超时不等价于供应商免费或停止计费；仍不确定的请求保留费用区间。

管理报告或窗口百分比是某个时刻的汇总，不天然带有精确本地水位。适配器仅在有证据时填写 `covered_through`；没有覆盖证明时，预测必须保留 overlap 区间，不能把报告值加上全部本地请求造成双计，也不能为了避免双计而假定全部在途请求已结算。

若账户由其他客户端共享，短期余额变化可以更新 external consumption 的估算，但不能把全部变化都算成 Shenko 的真实消耗。无法观察其他客户端时，保持 `shared_unobserved`、储备余量与宽区间。管理员报告的数分钟延迟、CLI 的 last-known 数据、时钟偏移和舍入精度都纳入预测。

观测与预测记录采用 [§41](architecture.md#spec-41) 的持久事实 / 派生视图边界；预测可重建，原始来源证据保留引用。额度插件不能通过丢弃 usage 事实或改动系统时间使余额恢复。

<!-- /migrated-contract: SPEC:42.4 -->

<a id="spec-42-5"></a>
<!-- migrated-contract: SPEC:42.5 -->
## 42.5 自适应算法与准入节奏的最低合同

控制器、预测模型和路由都是可替换策略。首个实现至少提供下面这一可回放的滚动规划基线；以后可以用更好的算法替换，不能只挂一个“adaptive”开关而没有输入、决定和误差记录。

### 预算与成本估计

对有已知恢复边界的每个额度窗 j，先建立剩余量区间。将未反映的自身成本、预计共享账户外部消费、紧急储备和不确定性余量扣除，得到可规划预算。只有本地调用未被服务端报告覆盖的部分才另扣；没有覆盖证明时使用区间，避免双计。

一种保守规划实例为 A_j = max(0, R_j_lower − U_j_upper − E_j_plan − reserve_j − margin_j)。每个量必须具有同一单位、同一范围和已说明的覆盖关系；无法确定时返回 unknown，不硬凑数字。距有效周期边界为 T_j 时，以 r_j = A_j / T_j 作为持续使用的目标速率。token bucket 使用其真实 refill、可用 burst 和请求计量规则，不套用周窗公式。

按实际请求学习 profile/effort/上下文/工具轮次/缓存状态对应的成本分布 c_pj；数据稀少时保留宽区间，并采用有依据的先验或保守值。百分比变化不能精确归因到一个请求时，不伪造精确单次成本。全量观测、已知其他客户端消费和不确定请求共同更新模型。

### 同时选择质量、份额和平均次数

规划器枚举符合任务能力要求的模型/effort/输出预算组合，并同时给出主体与 child 的候选准入速率 λ_p。每个重叠额度窗均检查 sum_p(λ_p × c_pj) ≤ r_j 的规划条件，再检查短窗 burst 与已授权费用上限；不能只满足周窗却提前耗尽五小时窗。这里是带误差的消费预测，不是供应商容量保证。

目标优先取得有价值成果并保持任务所需质量，在目标利用率区间内减少过早耗尽和期末大额闲置。规划结果可以提高/降低 smart 模型份额、改 effort、转本地、调整 child 配额，也可以直接调整主体平均 generation 次数。all-model 与 cloud-only 的轮数分别记录；不能仅改变云端占比，却声称实现了主体平均频率调节。存在适合的本地 fallback 时可继续较高频推进，但是否采用及成本仍由当前策略决定。

质量较低的 profile 只有达到该任务能力下限才可候选；模型名里的“smart/cheap”不是质量证明。语义预算或模型改变在 seal 前决定；已 seal 请求需按 [§41](architecture.md#spec-41) 替代规则创建新 request。无价值请求不会为了期末利用率而产生。

### 可恢复的额度准入

每次策略输出持久保存 PolicyDecision：所依赖的快照、预测、策略 revision、各 profile 与轮数目标、reserve、有效期、下一次可准入条件及决策理由。非模型控制器在新观测、完成计量、紧急事件、窗口恢复或已计划的预算释放时重算；滞回和最小有效变化抑制模型抖动。

额度准入记录与 ComputeAllocation 分开：

```text
QuotaAdmission {
  admission_id, request_id, account_scope_refs, policy_decision_ref,
  estimated_cost_by_window, provisional_reservation_ref?,
  state: eligible | quota_policy_wait | provider_exhausted |
         observation_blocked | admitted | cancelled,
  next_eligible_at?, wake_conditions[], reason_ref,
  actual_usage_refs[], record_revision, updated_commit
}
```

quota_policy_wait 表示策略为了跨周期分配预算而暂缓：主体仍有唯一 runnable 后继，未占用 compute permit，也没有声称供应商已经耗尽。provider_exhausted 必须有实际限额依据；observation_blocked 来自当前明确配置的未知状态政策。后端无可用计算容量则是 broker 的 capacity wait，不能混成额度状态。

对于准许平滑释放的预算，控制器可用多维 token bucket 或等价虚拟时间计算下一 eligibility，额度预留只针对即将准入的有限请求；排队不能无限锁住整周余额。取消未发送请求释放临时预留；已 armed/unknown 的计费仍保留估计范围，不能因取消调用就全额退还。新紧急事件可以按当前策略使用 reserve 或触发重规划；不得静默提高用户费用上限。

每次 S1 仍立即登记唯一下一步；额度准入调整的是该步何时获得预算与模型，而非把“没有外部消息”设成停止思考条件。事件可随时到达并改变候选输入、优先级与准入决定。主体可以自主修改既有授权内的配额策略；策略等待本身及平均轮数都能被它观察和复查。

### 目标与误差

持续比较计划与实际，输出周期末余额区间、提前限额时长、各模型/主体/child 的轮数、质量、响应和等待指标。共享账户消费、突发消息、供应商政策变化与统计延迟使“恰好用完一周额度”不能成为硬保证；它是持续校正的优化目标。不得通过无价值工作补到 100%，也不得把有计划的额度等待伪报为 backend 故障或持续 decode。

<!-- /migrated-contract: SPEC:42.5 -->

<a id="spec-42-6"></a>
<!-- migrated-contract: SPEC:42.6 -->
## 42.6 与事件、RequestSeal、KV 和执行调度的衔接

以下变化可形成耐久事件：`quota.observed`、`quota.observation_stale`、`quota.forecast_changed`、`quota.constraint_reached`、`quota.reset_observed`、`quota.policy_changed`、`model.routing_changed`。事件包含相关快照 / 预测 / 策略引用及有效期；按显著变化合并通知，避免每个 token 都给主体插入重复内容。主体的可读视图同时展示剩余区间、预计恢复时间、来源新旧、共享消费及当前模型选择理由。

**路由和语义预算必须在 RequestSeal 之前决定。** Provider、模型、effort、输出预算、上下文裁剪或工具声明变化，都需要新的 `request_id` 和 RequestSeal，并在同一 lineage 关联 `supersedes_request_id`。额度紧张不能在已 sealed 的请求内静默替换模型。已入队旧请求可以保留其决定，或经现有取消 / 替代合同退休后重封口；结果按原请求和合同解释。

模型 / tokenizer / engine / 模板改变时，原 ContextLease 或 KV 绑定只有在后端明确证明兼容时才可复用；默认不承诺跨模型继承 KV。缓存节省只能按真实计量修正成本估算，不能把预计缓存命中写成已支付量。模型切换可能增加 prefill，需要计入决策成本。

额度接纳与实际 ComputeAllocation 分离。额度侧的临时预算保留不能代替推理槽；请求 canceled、unknown、内部 paused 或连接断开时，是否释放物理容量仍按 [§41](architecture.md#spec-41) 的真实后端状态处理。模型轮转及 smart / cheap 选择不越过全局 C 上限，也不在 C=1 时持有父请求槽等待一次依赖推理。

额度观测采用非模型的 Provider API / CLI 数据收集，不需要为查询额度占用模型推理 permit。若主体决定让 child 做预测分析，该 child 与其他请求公平使用相同 broker；策略算法本身无需调用 LLM。

<!-- /migrated-contract: SPEC:42.6 -->

<a id="spec-42-7"></a>
<!-- migrated-contract: SPEC:42.7 -->
## 42.7 验收场景

| 编号 | 注入条件 | 必须可观察的结果 |
|---|---|---|
| QA01 指标隔离 | 同时提供 API TPM header、workspace 继承限额、Pro/Max 七天百分比及 context length | 单位、账户和约束维度分开；不把 API refill 当订阅 reset，不把缺少 workspace override 当无限额；不把 context/KV 当余额 |
| QA02 缺省与陈旧 | statusline 尚未首次响应；仅有 five_hour；seven_day 过期被移除；`/usage` 返回 last-known 或查询自身 429 | 相应字段为未观察 / 陈旧 / 已失效；不能宣称全满或耗尽；查询失败不产生虚假的 inference admission 拒绝；新窗到来后预测按证据更新 |
| QA03 共享消费与延迟 | 其他 Claude 客户端突增消耗；管理报告延迟到达；一个本地 attempt 计费未知 | 保留共享归因与费用区间，修订预测；既不双计已覆盖请求，也不将未知取消成本退回；期末预测允许明显变化并给主体发送有依据的事件 |
| QA04 模型切换与 C=1 | 排队期间策略从 smart 改 cheap；旧 generation 已 armed 后连接断开；新模型 tokenizer/KV 不兼容 | 新输入新 seal / request_id；旧 attempt 不伪装完成或免费；在真实物理槽可用前不超并发；不跨模型冒用 KV；已收到旧结果按旧合同入账 |
| QA05 周期控制 | 回放一周有波动的主体与 child 工作、五小时和周双窗、外部随机消费，设置期末利用率目标 | 输出误差、期末剩余区间、质量与等待指标；自动调整模型配比、effort、本地份额及可观察的主体平均准入轮数；quota_policy_wait 与耗尽/容量等待分开，S1 的后继持续存在；不生成无价值请求补余额，不改成无消息低频 Tick，不声称必然精确用完 |
| QA06 自主性与消费授权 | 额度耗尽、观测插件暂时失效、供应商提供额外付费、存在本地 fallback | 主体收到准确状态，可自主改已有授权内策略并转本地；未授权时不自动提高费用上限或启用额外付款；没有任何可用后端时明确报告容量阻塞 |

<!-- /migrated-contract: SPEC:42.7 -->

<a id="spec-42-8"></a>
<!-- migrated-contract: SPEC:42.8 -->
## 42.8 来源、采用范围与证据边界

REF-QUOTA，核验日期 2026-10-22。下列官方服务合同会变化，适配器记录实际 CLI/API 版本及原始字段证据。类型、跨 Provider 归因、控制器、事件、seal/KV 衔接与 QA01–QA06 是 REF-DESIGN，不能归为 Anthropic 已提供的成套机制。

| 来源 | 精确定位与采用范围 |
| --- | --- |
| [Claude Usage and Length Limits](https://support.claude.com/en/articles/11647753-how-do-usage-and-length-limits-work) | Usage limits：五小时/weekly、模型与任务消耗因素、多个 Claude 产品共享使用 |
| [Claude Code statusline](https://code.claude.com/docs/en/statusline) | Available data 的 rate_limits 字段表及缺省说明。five_hour/seven_day 的 used_percentage/resets_at；适用登录、首次响应、独立缺失和过期移除。spend_limit 百分比/reset 文档条件 v2.1.251+；美元/period 要求 CLI 与 gateway v2.1.284+，不等于核心最低依赖 |
| [Claude Code costs](https://code.claude.com/docs/en/costs) | /usage、Session 费用与 Showing last-known usage：订阅使用与 API 成本分开，陈旧快照和查询自身限流分开 |
| [API Rate limits](https://platform.claude.com/docs/en/api/rate-limits) | token bucket、response headers、refill、舍入与实际更紧 limiter；不能解释为每周余额 |
| [Configured Rate Limits API](https://platform.claude.com/docs/en/manage-claude/rate-limits-api) | Authentication、organization/workspace 与继承：读取配置限额，非实时订阅余额 |
| [Usage & Cost Admin API](https://platform.claude.com/docs/en/manage-claude/usage-cost-api) | Authentication、Messages Usage、Cost report、Data freshness/polling：历史组织 API 报告及延迟，不覆盖通用 Pro/Max 余额 |

<!-- /migrated-contract: SPEC:42.8 -->
