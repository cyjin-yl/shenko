# U42 / U43 规范增量：主体 Web 输入、对话配置与持久 Playground

状态：拟定的产品要求与规范接口，未实现。来源：REF-USER（U42/R132、U43/R133–R134）、REF-DESIGN；复用 §9、§21、§36–41 的 Inbox、S0/S1、RequestSeal、Operation、Activation 和 Dynamic UI 合同。以下名称不声称是现成 SDK API。

## §21.12 默认 Web 界面：与主体直接交互的一条时间线

WebUI 初始只呈现主体的一条时间线和输入框。输入接入同一 subject 的 thought loop，不启动另一条 chat agent，不以另一个聊天人格替主体作答。时间线呈现用户输入、主体选择表达的内容、简短进展和结果；不要求展示原始逐 token 内部思维。任务、记忆、插件、配置、额度、KV、Seat 和 Playground 通过按需入口展开，不能重新挤占成默认监控首页。

发送提供 `append` 与 `steer` 两种放置方式，首版默认 append。append 等待当前已封口的主体 generation 到达可接纳边界；steer 请求尽早介入。界面按真实回执区分“已接收”“已选入某轮”“主体已作出处置”，不能把传输成功显示成主体已经读懂或任务已经完成。后端只能下一边界处理 steer 时，状态如实表达这个实际位置。

放置方式与 D3 的紧急程度分开。append 不是后台低优先级，不因主体永远思考而排不到；它不因自身到达打断当前主体 generation。对正在占槽的 child 是否抢占、何时处理积压及是否进入 DND，继续使用同一调度与主体政策。steer 也不因此获得管理员身份或新增工具权限。

## §41.UI WebInputSubmission 规范接口

```ts
type WebInputSubmission = {
  submission_id: string;               // 客户端持久保存，重试保持不变
  subject_id: string;
  mode: "append" | "steer";
  text: string;
  attachment_refs: ObjectRef[];
  rich_content_ref?: ObjectRef;
  observed_step_id?: string;            // 观察信息，不是控制权
  observed_request_id?: string;
};

type WebInputView = {
  submission_id: string;
  event_id: string;
  accepted_commit: CommitRef;
  presentations: PresentationReceipt[];
  dispositions: Array<{ step_id: string; commit_ref: CommitRef; disposition_ref: ObjectRef }>;
  placement: {
    requested: "append" | "steer";
    actual: "pending" | "next_boundary" | "live_steer" | "cancel_reseal";
    target_request_id?: string;
    operation_id?: string;
    evidence_refs: ObjectRef[];
  };
  withdrawal?: { cancel_id: string; commit_ref: CommitRef;
    result: "withdrawn_before_presentation" | "withdrawal_queued" };
};

submit_web_input(input: WebInputSubmission): Promise<WebInputView>;
get_web_input(subject_id: string, submission_id: string): Promise<
  { kind: "found"; value: WebInputView } |
  { kind: "not_admitted" } | { kind: "temporarily_unavailable" }
>;
withdraw_web_input(subject_id: string, submission_id: string, cancel_id: string): Promise<WebInputView>;
```

认证主体来自当前会话，不能由正文中的 actor/admin 字段自报。服务以 `(authenticated_actor, subject_id, submission_id)` 查重：相同内容与 mode 返回原事件和回执；同 key 不同 payload 返回 `SUBMISSION_CONFLICT`，不修改原消息。查重命中先于旧观察版本的 stale 检查。修改已发送内容或把 append 改为 steer，提交关联原事件的新意图，不重用旧 key 改写事实。

### 接纳、呈现和处置

| 回执/阶段 | 唯一依据 | 不代表什么 |
| --- | --- | --- |
| accepted | 原始内容、附件引用和 InboxEvent 已提交；source 为 Web provider，external_event_id 为 submission_id | 不代表模型已看到，不代表任何外部任务已启动 |
| selected | S0 的 InputPlan/PresentationReceipt，或有独立记录的 live-steering 呈现 | 不代表已处理；所在请求取消后原消息仍可能 unresolved |
| committed | S1 对该 event revision 的持久处置；可为解释、委派、解决或明确无需回复 | 不代表 child 完成、外部效果完成或用户已读 |

三者是不同事实回执，不是可省略中间证据的单轴进度条。界面可显示当前待处理状态，同时保留此前请求被取消的 presentation。低于本地持久等级的浏览器乐观气泡标为待发送；提交前不得冒充 accepted。

append 在接受后的首个适用 S0 成为 ready 候选。有已 seal 的主体请求时，它不触发该请求的 stop/steer；无已 seal 请求时可纳入即将建立的 S0。预算放不下时保存显式 deferred/未处理集合并按等待老化继续选择；选择策略不能以“主循环尚未结束”为永久等待条件。当前 generation 被其他原因中断，不要求 append 再等一个人为构造的完整旧轮次。

steer 在接纳后绑定当时实际可作用的 request/engine incarnation。adapter 有已核验 live steering 时，通过 operation 发送并保存真实生效边界；只有 cancel 时，持久 stop intent、确认旧请求停止，再构建同一 subject step 的替代输入计划。无请求内控制能力则转 next_boundary。目标请求已经结束时，不将旧 stop 操作自动改绑下一请求。一次请求的 stop intent 合并去重，不因十条输入重复发十次取消。

任何语义 reseal 都产生新 request_id，在同一 request_lineage_id 中递增 request_revision，并记录 supersedes_request_id；旧 RequestSeal、partial 输出和历史不改写。真实 live steering 使用以下不可变记录，不改写原 seal：

```ts
type SteeringApplication = {
  application_id: string;
  request_id: string;
  attempt_id: string;
  engine_incarnation: string;
  sequence: number;
  previous_application_digest: Digest | null;
  exact_input_ref: ObjectRef;
  event_revisions: Array<{ event_id: string; event_revision: string }>;
  effective_output_frontier: {
    native_frontier_ref: ObjectRef;
    persisted_output_frontier_ref: ObjectRef;
  };
  adapter_contract_ref: ObjectRef;
  evidence_refs: ObjectRef[];
  application_digest: Digest;
  recorded_commit: CommitRef;
};

// 并入现有模型结果 manifest，不另建第二份结果记录。
type ResultAppliedSteering = {
  request_id: string;
  attempt_id: string;
  engine_incarnation: string;
  through_sequence: number;             // 0 表示没有已应用的 steering
  application_chain_digest: Digest;      // 0 时绑定该 request 的初始 seal
  result_output_frontier_ref: ObjectRef;
  evidence_refs: ObjectRef[];
};
```

`sequence` 在同一 request/attempt/engine 的应用链中递增，前序 digest 连到上一条已记录应用；`application_digest` 覆盖精确输入、事件版本、身份和生效边界等内容，提交后不可改写。effective output frontier 表示适配器合同能证明新输入已生效的输出边界；本地收到控制 ACK 的时刻不能代替它。模型结果携带 `ResultAppliedSteering`，S1 校验身份、完整链、结果 frontier 与后端证据，只能消费原 InputPlan 集合以及该结果 applied frontier 覆盖的事件版本。较晚应用、其他 attempt 或无证据的输入保持未处理，不能靠“已经提交过 steer”算作本轮使用。

适配器若只能证明控制请求已收到，不能给出可验证生效边界及结果对应关系，则本协议不把它登记为可用 live steering，实际路径降为 cancel_reseal 或 next_boundary。

### 撤回、未知与恢复

withdraw 的取消目标是这条输入的后续处理，不是所有已产生效果。原始 InboxEvent 位于 I；PresentationReceipt、EventDisposition、withdraw 控制和 S0/S1 位于 L。撤回事务在 **L** 检查呈现/处置并登记 withdraw 控制，与 S0 在同一数据库中排序：若它先于任何 sealed presentation/S1 消费，保存撤回事实并使原事件不再参与普通选择，返回 `withdrawn_before_presentation`；若已被选入或已作出处置，则在 L 追加有因果关联的撤回控制供后继步骤处理，返回 `withdrawal_queued`。不能声称模型没见过，不能删除已提交决定、自动撤销 SSH/发信或改写过去。I 中原始输入仍保留；不依赖 I/L 的虚构跨库事务裁定这场竞争。

浏览器断连不取消输入或主体。发送前保存稳定 submission_id 与 payload；响应丢失后查询或用同 key 重试。`temporarily_unavailable`、超时和空白页面不是 `not_admitted`，不能据此生成新 key 重发。writer 重开后从 Inbox、PresentationReceipt 和 S1 恢复视图；steer operation 状态不明时按原 operation 对账，不重复取消或重新派生主体。时间线订阅从持久 cursor 续接；需要重新快照时返回显式 resnapshot，按 item identity 去重。

## §43 对话配置与持久 Playground

### 43.1 所有配置可通过对话管理

主体能通过对话发现、解释、修改自身与已接入能力的配置；人类也有可查看实际配置的面板。配置命名空间由插件注册，覆盖当前实现暴露的所有设置，不设一份永久只读/不可改字段白名单。尚未暴露成配置的编译选项或行为，可由主体委派开发 schema、插件或代码并走自更新；不能为了宣称“配置可改”虚构尚未生效的字段。

“不改坏”落实为候选验证、可观察生效、保留可恢复副本和自动恢复，不承诺任意错误代码、外部断网或已销毁数据总能无损恢复。配置、验证器、恢复器和 core 均可替换；本节没有永久不可变部件，也没有新增每次改配置必须经人类批准的步骤。

```ts
type ConfigNamespace = {
  namespace: string;
  schema: SchemaRef;
  owner_plugin: VersionedRef;
  read_ref: VersionedRef;
  validate_ref: VersionedRef;
  apply_ref: VersionedRef;
  health_ref: VersionedRef;
  recovery_ref: VersionedRef;
  application: "hot_reload" | "restart_service" | "generation_handoff";
  effect_contract_ref: ObjectRef;
};

type ConfigChange = {
  change_id: string;
  logical_key: string;
  origin_event_or_task_ref: ObjectRef;
  actor_and_authority_ref: ObjectRef;
  expected_revisions: Record<string, string>;
  patches_ref: ObjectRef;               // 各 namespace/schema 的精确 typed patch
  base_snapshot_ref: ObjectRef;
  candidate_snapshot_ref: ObjectRef;
  validation_evidence_refs: ObjectRef[];
  health_contract_ref: ObjectRef;
  recovery_plan_ref: ObjectRef;
  activation_id?: string;
  phase: "prepared" | "validated" | "applying" | "observing" | "effective" |
         "rejected" | "recovering" | "reverted" | "recovery_blocked";
  operation_refs: string[];
  created_commit: CommitRef;
  updated_commit: CommitRef;
};

prepare_config_change(intent_ref: ObjectRef, logical_key: string): Promise<ConfigChange>;
validate_config_change(change_id: string): Promise<ConfigChange>;
apply_config_change(change_id: string): Promise<ConfigChange>;
get_config_change(change_id: string): Promise<ConfigChange>;
recover_config_change(change_id: string, reason_ref: ObjectRef): Promise<ConfigChange>;
```

配置服务采用以下合同：

1. 对话形成的修改由主体提交明确意图并委派执行。参数、授权、当前实际配置和目标 schema 在准备时解析；candidate 与现用配置分开保存。以 `(origin_event_or_task_identity, logical_key)` 查重，幂等命中先于 expected revision 校验；不同 patch 不能复用同一个 logical key。apply 再核对实际 base revisions，已被别的修改替换则返回 `BASE_CONFIG_STALE`，由主体/child 自主重建候选，不能覆盖新值。
2. `validated` 需要 schema、交叉依赖、目标 runtime 能力和恢复计划检查通过。网络/模型探测作为独立 operation，使用真实 target、路由和凭据句柄；模型探测仍计入同一物理 broker。检查失败保留原配置、候选和诊断，不自动写入半份配置。
3. hot reload 由所属插件声明原子生效点；需要 restart/handoff 时引用既有 ActivationRecord，不另造一次无账本的重启。多配置不能原子生效时，计划明确步骤、暂态和恢复次序。保存 desired/candidate/effective 的区别，不能把“文件已写”当服务已经采用。
4. apply 后进入 observing；health contract 用可执行探测及有界窗口核验真实路径，保存证据后才称 effective。主动跳过某项探测也是独立的版本化策略决定，不伪造通过结果。
5. 失败进入 recovering，以已保存的 base/known-good 配置与相匹配服务闭包恢复；先比较当前版本，避免覆盖其他已提交改动。无法原样恢复时生成新恢复/迁移计划和 diagnosis，保留 recovery_blocked 的真实范围，其余工作可继续。
6. 配置恢复不撤销配置期间已经发出的消息、远程命令或数据迁移。这些 effects 继续由 §41 账本处理。secret 字段保存凭据句柄；面板显示名称、来源和状态，不暴露私钥/token 原文。

**LLM 端点、代理和消息入口的恢复不依赖 LLM 可用。** 激活新配置前持久保存可运行的旧服务闭包、旧 route/secret refs、恢复动作和探测计划；本地恢复 worker 使用确定性 I/O 执行，不能等待被错误配置切断的模型来决定怎样恢复。探测能区分连接、协议、认证及实际生成失败。修改代理必须保留未受候选配置覆盖的本地恢复入口，或者已验证可还原宿主网络配置的恢复路径。若外部撤销了凭据、物理网络中断或存储损坏超出该计划覆盖，如实进入相应 blocked 状态；不能宣称旧配置一定恢复外部服务。

端点探测在 armed 后断连时，可能已占满 C=1。恢复计划必须能通过预先保留的、独立于候选模型/代理的确定性控制路径，**先恢复旧配置，而不等待另一次模型健康探测取得 permit**。原探测的 ComputeAllocation 仍 held，恢复配置不等于探测停止；恢复 worker 继续 query/stop，或执行合同允许的 engine 隔离/销毁，取得真实 QuiescenceProof 后才释放原占用。网络隔离若不能证明执行停止，原占用仍保留；仅修改本地 engine ID、路由或等待超时不能清槽。若需要立即恢复推理，只能使用证实属于独立物理资源组且有可用容量的 backend；同一后端换 URL 不算独立。其余健康验证在资源确实可用后执行。

管理员身份仍来自人类明确设置。已认证的人类可以在对话中明确授予/撤销管理员，系统保存 authenticated actor、具体对象、权限范围和决定引用，由权限插件执行；面板不是唯一入口。联系人别名、声纹候选和模型对人的判断不能自行变成管理员凭据。普通配置改动不被这一条扩张成一律需要人类审批。

配置面板读取同一配置服务，展示实际 effective 值、候选差异、版本、来源、重启需求和恢复状态。面板如提供编辑，也提交同一 typed change 合同，不写另一份与对话脱节的配置。入口按需打开，首页保持主体时间线。

### 43.2 Playground 是长期存在的全栈作品空间

主体可因自己的兴趣或交流任务创建、运行、改进和保留 artifacts。作品可以有界面、后端、持久数据、定时工作及对其他 runtime 的连接；不限定为静态 HTML、一次性 widget 或当前聊天页中的临时附件。人类可从 Playground 入口再次打开、交互和查看作品，作品不能仅因浏览器关闭、对话切换或主体 compact 而消失。

```ts
type ArtifactManifest = {
  artifact_id: string;
  version_digest: Digest;
  source_refs: ObjectRef[];
  build_and_dependency_lock_ref: ObjectRef;
  frontend?: { bundle_ref: ObjectRef; entry: string; surface_contract_ref: ObjectRef };
  backend?: { entry_ref: ObjectRef; runtime_ref: ObjectRef; service_contract_ref: ObjectRef };
  state_bindings_ref: ObjectRef;         // schema、持久位置、恢复域与迁移关系
  action_schemas_ref: ObjectRef;
  operation_contracts_ref: ObjectRef;
  secret_handles_ref?: ObjectRef;
  health_contract_ref: ObjectRef;
  recovery_closure_ref: ObjectRef;
};

type ArtifactRecord = {
  artifact_id: string;
  subject_id: string;
  creator_task_id: string;
  title: string;
  active_manifest_ref?: ObjectRef;
  candidate_manifest_refs: ObjectRef[];
  state_revision: string;
  state_ref: ObjectRef;
  lifecycle: "draft" | "running" | "stopped" | "archived" | "recovering";
  runtime_instance_refs: ObjectRef[];
  desired_lifecycle_ref: ObjectRef;
  provenance_and_causation_ref: ObjectRef;
  updated_commit: CommitRef;
};

type ArtifactAction = {
  action_id: string;
  artifact_id: string;
  manifest_digest: Digest;
  action_name: string;
  input_ref: ObjectRef;
  expected_state_revision?: string;
  causal_parent_refs: ObjectRef[];
};

stage_artifact(manifest: ArtifactManifest): Promise<ObjectRef>;
publish_artifact(artifact_id: string, candidate_ref: ObjectRef,
                 expected_active_manifest?: Digest): Promise<ArtifactRecord>;
invoke_artifact_action(action: ArtifactAction): Promise<ObjectRef>;
get_artifact(artifact_id: string): Promise<ArtifactRecord>;
```

**持久与发布。** 源码、依赖锁、前端产物、后端入口、数据 schema 和恢复闭包都进入版本化存储，实际数据位置遵守 managed snapshot / host bind / remote 的边界合同。代码版本与数据 revision 分开；升级代码不自动清空数据。发布按 candidate→构建/能力检查→隔离 smoke→持久选择当前 manifest 的过程执行；需要迁移 runtime/state 时复用 Activation 与 operation 协议。旧页面仍带旧 manifest 的 action，使用兼容的旧合同或返回 `ARTIFACT_VERSION_STALE`，不能套用新同名 action 的不同语义。软件回退保留新版本期间产生的用户数据和外部事实，再依据 schema 兼容或迁移计划恢复。

**后端与能力。** Artifact backend 使用注册的 RuntimeProvider，可位于 WASM、宿主受管进程、BoxLite/容器或远端节点，声明实际部署能力；无所需 runtime 时保持可恢复的 draft/stopped 状态，不把静态前端可见冒充后端正在运行。任务、网络、文件和远端操作遵循 manifest 中 operation/effect 合同；需要模型的 artifact 后端也经过共享 broker。凭据留在相应 secret/runtime 服务，不能把后端私钥打进前端 bundle。入口的实际授权遵循当前主体/用户/委派关系，点击网页不自行获得 artifact 创建者的全部权限。

**UI action 到主体感知。** `invoke_artifact_action()` 从认证会话取得 actor，按 `(artifact_id, authenticated_actor, action_id)` 查重；相同 payload 返回原 receipt，冲突返回 `ACTION_CONFLICT`。查重命中先于 expected state revision 检查。服务校验 manifest/action schema、当前权限与目标状态，再持久接纳 action 和执行意图；外部效果按 T1/T2 运行。

有合作后端的部署中，artifact **实际业务数据库**是业务状态变更的权威：业务状态/revision、action 去重记录与完成 receipt、待导出的稳定 event_id/outbox 记录，在同一源数据库事务中提交。不能先写业务数据，再到 Shenko ledger 补一条幂等标记来冒充原子执行。Shenko 导入端按稳定 event_id 持久接纳 InboxEvent 后才 ACK；源端可重复投递，ACK 丢失时导入去重，不再次执行业务动作。源数据库回执与 Shenko 导入回执分别保留。

第三方或非合作远端后端不支持此源事务合同，或者动作还包含源 DB 之外的外部效果时，不能承诺“重复请求普遍只执行一次”。适配器按真实幂等、查询与 receipt 能力处理；结果不明时进入原 operation 的 reconcile/unknown，不能通过重复发送补做。主体感知的是已确认事实及明确标注的不确定状态。

已提交的 `artifact.action`、`artifact.state_changed`、后台结果、故障及适用的外部状态变化进入**同一主体 Inbox**，包含 artifact/manifest/state revision、actor、cause、delta 摘要和精确原始引用。模型在 S0 选择、S1 处置这些事件，能观察人的交互和作品的变化；不能只让浏览器改 DOM 而没有服务端事实。浏览器本地临时 hover/滚动/动画不逐个触发模型，需要表达业务意义的动作由 action schema 显式定义。

**反馈抑制不等于失去感知。** 事件携带 root cause、operation/action identity 和 origin（human/subject/worker/external）；actor 与 origin 从认证和服务端任务谱系取得，不相信客户端自报“这是主体自己的更新”。同一状态变化的服务端回执、UI 回显和消息同步合并成一个语义刺激；主体刚提交的变更可作为自发事实归档而不再次伪装用户新指令。原始记录保留，模型仍可查询。高频拖动、采样或运行日志可确定性聚合并保留范围/分页引用；不合并掉不同的用户提交，不以内容 hash 消除两次相同但独立的动作。不要求每个浏览器事件额外跑一次 LLM 分类器，也不禁止主体有意持续观察自己的实验。

**作品生命周期与界面。** 作品的 desired lifecycle、后台任务 ownership、数据保留和 runtime 实际状态分别记录；重开后按这些记录恢复，先核对原服务/operation，避免启动第二份。关浏览器不停止后台；暂停/归档显式形成决定。存储不足后的迁移、归档和清理仍由既有存储插件自主处理，默认不因“聊天结束”设置短 TTL。

Playground 使用 §21 的 Proto-UI/shadcn 与动态界面能力协商，保留 compiler 接缝；其前后端可以是模型生成和持续演进的实际应用。Web、窄屏和 IM surface 根据能力提供交互、只读视图、链接或摘要，不能展示实际不可用的控制却假报成功。模型通过能力/DSL 描述与操作 schema 知道能生成什么。Playground 是按需入口，作品的必要结果或互动可以嵌入主体时间线，首页依然不变成作品管理大屏。

### 43.3 本轮验收增量

| Case | 情况 | 可判定结果 |
| --- | --- | --- |
| UI01 | 主体 generation 中连续发送 append | 当前请求不因这些输入被打断；输入可靠接纳，在后继 S0 公平选择；不建 chat agent |
| UI02 | 后端无请求内控制却发送 steer | 真实显示 next_boundary；不篡改旧 seal，不假报已打断 |
| UI03 | accepted 响应丢失，浏览器重开并同 key 重试 | 只有一条 InboxEvent；可恢复 presentation 和 S1 处置；不重复触发任务 |
| UI04 | withdraw 与 S0 竞争，或 S1 后才撤回 | 按提交先后返回未呈现撤回或撤回已排队；已产生的外部效果不被谎称撤销 |
| CFG01 | 修改端点/代理失败；另测探测请求 armed 后断线并占满 C=1 | candidate/active 分离；确定性独立控制路径能先恢复旧配置，不等第二次模型探测；原 Allocation 仍 held，真实停止/隔离证明后才释放，或使用真正独立且有容量的 backend；保留失败证据 |
| CFG02 | 修改期间断电，再次提交同 change key | 采用已提交阶段；不把候选当 effective，不重复发出外部配置效果 |
| CFG03 | 已认证人类明确设置管理员；另有声纹高相似候选 | 前者有手动决定与权限回执；后者不自动获管理员 |
| PG01 | 创建带后端与数据的作品后关页面、compact、重启 | 源码/版本/数据保留，按实际 runtime 能力恢复；无重复后台进程 |
| PG02 | 满足源业务 DB 原子事务合同的部署，action 已提交但响应/导入 ACK 丢失后重发；另测非合作后端 | 前者幂等命中原 receipt，业务变化一次，稳定 event_id 重投只导入一次；后者按真实能力查询/核对并保留 unknown，不普遍承诺 exactly-once |
| PG03 | 发布新 action schema 后旧页面继续提交 | 正确执行旧合同或显式 stale；不使用新语义误解释 |
| PG04 | 主体改 UI，UI 回显，用户随后做两个相同动作 | 自己的同一变更不形成反馈风暴；两个独立用户动作均保留且可感知 |
| PG05 | 回退作品代码，而新版本期间已收用户数据 | 新事实保留，按兼容 reader/迁移恢复；不靠删除新数据让旧版本看似健康 |
