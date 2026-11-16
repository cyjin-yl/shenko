# Shenko 首次启动与种子发行规范草案

状态：研究/SPEC，未实现。正文使用已由用户纠正的 **dshbox/cordis-rs**；colibri-rs 只保留在历史原话归档，不再作为待确认依赖。来源：REF-USER（首启、可独立安装的 awareness、Cordis 更正及既有自主性裁决）、REF-DESIGN；承接 §9、§21、§35、§38、§41、§43。Cordis 提供服务与生命周期组织；断电后的接纳、恢复和外部效果语义来自 Shenko 持久协议，不能归功于 `ctx.effect` 内存清理。

## 1 第一次打开会发生什么

首次打开仍是一条主体时间线和一个输入入口。尚未接通模型时，界面明确显示系统首启状态与简短配置入口，不假装已有 Agent 在回答，也不启动临时的 setup/chat agent。后台不依赖 LLM 完成发行包装配、持久目录初始化、WASM 家园启动和自身资料索引；完成的事实显示为系统状态。

交互首启只需解决两件事：**由有安装控制权的人明确初始化管理员，以及提供一个可用推理入口**。已有 provisioning/profile 可以预先提供这两项，UI 不重复询问。名字、人格、QQ、GitHub、SSH、公网域名、Playground 和完整硬件参数都不进入必填长向导。

管理员绑定与“连接并开始”可在同一短流程中完成。名字可留空，主体接通后可自己取名；实例身份使用稳定 ID，不靠显示名。完成首次启动以后，刷新页面、增加浏览器和重启服务均连接到既有主体，而不是“新建一段聊天”或再生一个主体。

## 2 无 LLM 时种子发行必须已经具备的闭包

Seed 是一个可替换、版本化的发行装配，不是把下列功能焊入 Rust 核心。manifest 固定实际工件摘要、平台 profile、依赖、文档及恢复闭包；启动器按 manifest 装载 Cordis 服务。发布的首版 seed 必须自带能在所声明平台运行的实际工件，不能把“之后让 Agent 安装 WASM”当首次启动方案。

| 默认 seed 服务组 | 无 LLM 时完成的工作 |
| --- | --- |
| Bootstrap / identity / secret / Web | 识别新安装或已有实例、认领入口、保存配置草稿与凭据句柄、显示同一时间线、提供确定性的故障配置入口 |
| Store / CAS / Inbox / task / effect | 打开 I/L 与对象存储、完成 schema/对象闭包检查、接纳可恢复输入和初始化任务、提供 T1/T2/E1 与观察协议 |
| 实际 WASM RuntimeProvider 与家园 FS | 启动所选平台的 WASM runtime、持久家园和已知可恢复的初始状态，记录挂载与副作用边界；加载种子中已验证的基础工具 |
| 默认 memory / history | 创建挂在主机私有数据目录上的本地 Git 记忆仓库并挂入 WASM；建立会话原文存储与可重建检索索引。默认不添加外部 Git remote，不需要云账号 |
| Broker / provider / capability probe | 先登记真实资源组、容量与后端身份，再进行任何模型探测；保存未知能力，不伪造 KV/取消能力 |
| Main / child harness 与委派服务 | 装载连续串行主体规则、有限 child、S0/S1 和任务恢复；主体只能委派、语义记忆、自解释及提交相关意图，无直接文件/SSH 工具 |
| Capability / self-knowledge | 离线列出实际已装能力、用法、source/doc/contract 身份与恢复边界；能说明哪些未连接或未实现，无需额外 LLM 分类 |
| 配置执行与恢复基元 | 持久配置读写、service reload、现有 Activation、探测/停止/查询及本地修复入口；保留不依赖模型回答的执行路径 |

Seed 还可预装几项彼此独立的小 awareness，例如“网络变更可能影响当前模型连接”“现有工具不足时可以补能力”“需要了解自身用法时查阅自身文档”。每项都是可单独安装、替换和停用的直觉/意识，有自己的身份、版本和适用说明；不要求这些例子全部预装，也不把它们合成一个总意识插件。无模型时可以完成其材料与实现的离线装载，但上表中的初始化不等待 awareness 先作 LLM 判断；缺少这些可选意识也不阻断确定性首启。

数据目录由平台 profile 给出本机默认，初始化时持久保存绝对绑定身份；UI 可改位置。目录、Git 仓库和 CAS 对象的创建不塞进一笔 SQL 事务：各步骤使用稳定初始化 identity 和自己的 operation/receipt，恢复按已完成事实继续。只有对象闭包准备好才登记可用引用。发现同路径已有非本安装数据时报告冲突，保留数据，由明确的新路径或导入选择解决，不覆盖现有内容。

发行包含目标平台真实支持的基础工具，不因“WASM/POSIX”名字推定已有完整 Linux、gh、Node 或任意 native 扩展。缺少的增强能力接通模型后可自行发现、移植和安装。主机开发工具与未来的 BoxLite/远端 runtime 不取代首版 WASM 家园。

## 3 真正必填、自动推导与后续发展

| 项目 | 首次启动的处理 |
| --- | --- |
| 安装控制权与首位管理员 | 必须有明确的人类初始化动作或既有 provisioning 授予记录；见 §4。不要求额外云账号 |
| 推理入口 | 必须有一个已配置的本地/Tailnet/远端 endpoint 或等价 RuntimeProvider 绑定；已有受信部署 profile 可直接导入，不重复填写 |
| API 凭据 | 仅后端要求时输入或导入 secret handle；无认证的本地服务不强迫创建 API key |
| 模型选择 | 仅存在多个可用模型且无法从 profile 确定时展示一次选择；唯一模型或预配置模型直接采用 |
| 数据目录、端口、WASM profile、日志 | 根据平台与 seed 给出可查看默认；不让用户先理解整套目录/服务拓扑 |
| 并发、窗口、KV | 导入已有成功 profile 的真实 C/上下文与适配器能力；用户已确定的四流 128K 不被静默改成 C=1。没有可用证据的新部署先用 C=1 与有限短请求；KV 池容量允许 unknown |
| 实例名字、人格/灵魂 | 可空；接通后由用户命名或让主体自命名/演进。人格不替代工具 schema 与角色合同 |
| NapCat、飞书、其他渠道 | 首次思考不依赖它们；已有授权配置可导入，其余可稍后在对话中接入 |
| tmux/SSH/开发仓库 | 自开发路径的可选接入参数，不是开始个人主体的前置条件；启用后按 §35 完成真实闭环 |
| 外部 Git、网盘、语音、Computer Use、REPL、Playground | 由已有能力/连接条件按需启用或由主体委派安装和发展 |

“C=1”是未知部署的初始调度选择，不是宣称该设备最多一流。初始短探测使用 adapter 声明的有效有限请求；可作为 seed 默认将探测输出上限设为 64 token。普通主体请求继续用实际 model profile 的有限预算。没有可靠上下文上限时，先使用 seed 的保守请求预算并把后端上限标为 unknown；例如 8192 token 的输入预算是本产品初始政策上限，不是已核验的模型能力。超限拒绝形成事实，由 provider/后续主体调整；不能凭模型名称宣称已支持 128K。

确定性探测只访问明确提供/导入的 endpoint。已知原生 NInfer/Strata/vLLM 等 profile 优先绑定其具体 adapter，不把所有后端强行降成 OpenAI-compatible。未识别扩展能力时如实使用可证实的基础请求能力，并记录待适配项。

## 4 首位 owner/admin 与普通聊天

默认首启入口绑定本机，或使用部署者明确配置的受认证访问方式。发行生成 installation-scoped、一次性的 claim capability，经安装控制通道提供；“第一个访问 HTTP 的人”不自动成为管理员。用户在该通道执行明确的初始化认领，或者提交已有 provisioning 中的授权记录，服务才产生 OwnerGrant。

```ts
type OwnerClaim = {
  claim_id: string;
  installation_id: string;
  human_principal_ref: ObjectRef;
  explicit_initialization_ref: ObjectRef;
  claim_capability_ref: ObjectRef;
};

type OwnerGrant = {
  grant_id: string;
  installation_id: string;
  human_principal_ref: ObjectRef;
  claim_id: string;
  authority_ref: ObjectRef;
  committed_at: CommitRef;
};
```

认领在 L 以 installation identity 和 claim identity 幂等接纳；首次授予使用条件写，消耗相应 claim capability。同一认领重试返回原 grant；另一浏览器不得因为先收到响应就覆盖 owner。已初始化实例导入保留其授权，不重新开放 first-owner 入口。管理员恢复/转移使用可替换权限插件的显式人类决定，不依赖声纹相似、第一条问候或模型猜测。

human principal 由安装控制通道的认证上下文解析，不能靠请求体自行填写一个 human_principal_ref 冒充。首启模型配置/启动接口使用该已认领权限或明确的 provisioning 权限；浏览和普通输入不因此被当作管理授权。

OwnerGrant 是安装管理权限，不是在主体人格中注入“必须如何对待主人”的关系设定。初始化以后，默认聊天仍开放；成为普通会话参与者不要求先获得 admin。渠道建立后群成员可按既定开放聊天政策交互，管理权限继续单独记录。

## 5 Bootstrap 记录与接口

以下类型引用 §41 的唯一 Store/Operation/Task/Broker/Activation 记录，不建立平行的任务或容量账本。

```ts
type GenesisRef = { ledger_id: string; genesis_id: string };

type BootstrapRecord = {
  bootstrap_id: string;
  installation_id: string;
  record_rev: bigint;
  subject_id: string;                  // 首次装配即分配，重试不变
  conversation_id: string;
  seed_manifest_ref: ObjectRef;
  plugin_lock_ref: ObjectRef;
  ledgers_ref: ObjectRef;
  home_manifest_ref?: ObjectRef;
  owner_grant_ref?: ObjectRef;
  model_profile_ref?: ObjectRef;
  initialization_operation_refs: string[];
  probe_operation_refs: string[];
  readiness_evidence_refs: ObjectRef[];
  unresolved_issue_refs: ObjectRef[];
  launch_id?: string;
  genesis_ref?: GenesisRef;
  selector_ref?: { subject_id: string; selector_revision: bigint };
  updated_commit: CommitRef;
};

type BootstrapView = {
  record: BootstrapRecord;
  needs: Array<"owner_claim" | "model_binding" | "credential" | "model_choice" | "local_repair">;
  state: "preparing" | "awaiting_input" | "ready_to_start" |
         "starting" | "attached" | "blocked";
  effective_capabilities_ref: ObjectRef;
  pending_capacity_ref?: ObjectRef;
};

inspect_bootstrap(installation_id: string): Promise<BootstrapView>;
claim_installation(input: OwnerClaim): Promise<OwnerGrant>;
configure_initial_model(bootstrap_id: string, submission_id: string,
                        profile_intent_ref: ObjectRef): Promise<BootstrapView>;
start_subject(bootstrap_id: string, launch_id: string): Promise<ObjectRef>;
resume_bootstrap(bootstrap_id: string): Promise<BootstrapView>;
```

`BootstrapView.state/needs` 从实际记录和证据派生，不是 Rust core 内固定的审批流程。主体与 conversation 的身份先保留；在根记录不存在之前，它只是待启用身份，不意味着已经有一条思考流。初始化/探测使用确定性 bootstrap 服务任务；所需 conversation/task scope 可预建但不运行另一条模型聊天循环。

配置命令按 `(bootstrap_id, submission_id)` 查重，查询原结果先于当前 revision 条件；同 identity 不同 payload 返回冲突。profile 更新保留先前草稿与诊断。任意模型 probe 必须在 Store、effect ledger、同一 broker 和真实 resource group 已可用后进行；probe 属于原账本中的实际 task/request，不是首启绕过容量的特殊直连。

首次没有可用旧 endpoint，**不要求先有 rollback 目标才能连接第一个模型**。连接或探测失败时保留 endpoint 草稿、secret handle 与证据，回到仍可用的确定性配置入口；此时没有“已恢复旧模型”的成功状态。若 probe 已 armed 后失联，原 Allocation 仍 held，按 query/stop/实际停止证明对账；重新配置 URL 或超时不能清槽。独立控制路径可以修正配置而不用先取得模型 permit；需要推理时仍遵守实际资源约束。

## 6 首个 Genesis、根和唯一第一步

需要补充 §41 的初始状态，不能以一个虚构的“第 0 轮模型决定”初始化已有 head 字段：

```ts
type SubjectGenesis = {
  kind: "genesis";
  genesis_id: string;
  subject_id: string;
  bootstrap_id: string;
  launch_id: string;
  seed_manifest_ref: ObjectRef;
  initial_profile_ref: ObjectRef;
  home_manifest_ref: ObjectRef;
  bootstrap_event_ref: ObjectRef;
  created_commit: CommitRef;
};

// 扩展/明确已有 SubjectBranchHead 的初始可取值：
// committed_step_no = 0
// committed_step_id = null
// committed_at = genesis 的 CommitRef
// genesis_ref = SubjectGenesis 的持久引用
// next_step_id = 唯一的 first SubjectStep ID，step_no = 1
```

Genesis 是 L 中的不可变记录，`genesis_id` 在提交前分配，`created_commit` 在同笔提交中赋值。`GenesisRef` 定位该记录，不要求提前把一个含未知提交序号的对象封存进 CAS；它引用的 seed/home/profile 等 CAS 对象仍须先封存。

默认单机首启的提交先后如下：

| 阶段 | 必须准备/提交的事实 | 重开行为 |
| --- | --- | --- |
| 装配 | seed 工件/对象闭包、I/L、WASM/home、实际挂载与基础插件服务已就绪；按稳定步骤记录外部初始化 effects | 读已有 receipt 继续，不重复初始化仓库或清空路径 |
| 认领与模型绑定 | OwnerGrant、实际 model/provider profile、broker 容量/未决探测记录；名字不必已确定 | 缺什么才显示什么；未知探测先核对，配置入口独立可用 |
| 引入启动事实 | I 中以稳定 event identity 提交 bootstrap event，包含当前家园、能力、配置/自解释引用和起名可选意图；相关 CAS 对象已封存 | I 已有而 L 尚未 launch 时保留同一事件，不再创造另一份“出生” |
| launch 原子提交 | **L 同一事务**创建 SubjectGenesis、初始 ActivationSelector、subject root task、branch head 和唯一 first SubjectStep，并保存 launch receipt/调度意图；引用已提交的 I/CAS 对象 | 事务已提交则返回原 receipt；未提交则按原 launch identity 重试。没有跨目录/I/L/CAS 的伪原子承诺 |
| 发起第一步 | 以已提交 first_step_id 去重进入既有 S0→broker→S1；S1 原子登记唯一后继 | crash 在提交后、调度前由 outbox/ready 索引恢复；浏览器重连不产生第二个 root |

首次 `ActivationSelector` 允许“尚无 selector”的初始化条件写，采用 seed generation、初始有效 epoch 和当前 state ref；同安装/subject 的既有 selector 不被 bootstrap 覆盖。首个有效 head 指向 genesis，S1 为 step 1 检查其 expected parent 为 genesis。之后 `committed_step_id` 指向真实模型步，不再可为 null。

`launch_id` 是命令去重键；另以 installation/subject 对首次根和 genesis 建唯一约束。因此两个浏览器即使提交了不同 launch_id，也只能得到同一个既有根或明确的冲突，不能生成两个主体。第一次“连接并开始”可顺带提交 launch 意图，不再要求一轮额外人工审批。导入或恢复实例直接定位既有 genesis/head，不运行新实例的出生路径。

Bootstrap probe 所用 broker/Engine 占用在接管后继续保留，不因生成了 SubjectGenesis 就初始化第二份满额许可。待处理 Web 输入已在 I 中可靠接纳时，用原事件进入第一批或后续 InputPlan；不能让启动事实水位覆盖更早的真实输入。

第一个真实 S1 之后，主体正常持续提交有限思考步并立即登记后继，不因没有聊天消息改成 timer tick。它可选择自命名、形成兴趣或委派改进，不要求先完成一串欢迎问答。文件、仓库、挂载、安装和 SSH 操作继续由服务/child 执行，主体本身不获得直接文件工具。

## 7 可独立安装的 awareness 与执行基元

**每项 awareness 是可以独立安装的直觉/意识，用来补足模型因世界知识不足而“没想到”的事情。** 它与 Skill 相近：Skill 更侧重怎样执行，awareness 更侧重意识到什么值得考虑、还可能缺少什么。这里描述的是有具体内容和作用方式的可安装单元，不只是一个抽象 capability 接口；不同意识分别成为不同 awareness，各自可安装、改写、替换或停用。它们可使用插件机制承载和组合，也可与 Skill、工具和文档配合，不要求存在负责所有意识的总插件或统一审批器。

Seed 可带 §2 所列的若干小 awareness。其说明应让主体知道它提醒什么、在什么情境有用、如何使用及相关成本和效果边界；不要求每次生效都额外运行一次 LLM。配置执行、探测、健康读取、版本管理、effect 查询和恢复仍分别提供独立 capabilities。awareness 帮助主体想到这些能力或注意到相关问题，主体或明确选择的策略/plan 决定怎样调用和组合。“准备候选→验证→应用→观察→确认”只是某个 plan 可以采用的次序，不是 awareness 的定义，也不是 core 强制所有修改经过的五阶段流程。未装、替换或停用某项 awareness 时，配置等执行基元仍可直接调用，内核不得隐式补回相同的固定流程。

例如小规模可原子修改可以直接应用；端点替换可先探测再应用；现有服务已坏时可先恢复可用路径再检查。需要模型参与的能力实现和策略执行同样使用共享 broker，可作为 child/协作能力工作，不建立第二条永久主意识流。没有模型时，确定性配置、探测和恢复实现仍按各自合同工作。

稳定身份、真实回执、提交可见、schema/实际权限、Engine 占用和当前公布合同的恢复边界属于执行事实语义；选择改进流程属于主体及其采用的策略。插件不能把“没有做健康检查”写成检查已通过，但策略可以明确选择不做、改变检查方法或先处理更重要的事情。能力实现、策略及 core 本身仍可通过版本化协议演进，没有永久锁死的组件。

§43 的 ConfigChange phase 因而只能描述某个具体执行计划的进展，不能成为所有配置 API 的全局唯一强制状态机。普通配置服务仍返回当前 desired/effective revision、实际 effects 和可调用基元；plan-specific phase 由策略 schema/ref 标识，并记录实际使用的能力实现版本。初次没有模型的确定性 bootstrap 是 seed 中插件和能力的有限装配流程，不依赖主体先批准它自己存在，也不以安装某项 awareness 实现为前提。

## 8 NapCat 与 tmux 首版可选接入

首次开启主体不要求绑定 QQ。开发者发行可预装但不自动连接 NapCat、HostRuntime/SSH、Seat、source/worktree、build/update 等插件；启用时读取明确提供的已有连接和权限。NapCat 需要 endpoint/连接凭据及真实账号状态，tmux 需要已授权 runtime 与 seat 选择，开发任务需要实际源码/worktree identity；能够从已授权部署配置取得的内容不重复问人类。

已有 NapCat 和本机 tmux 时可以在初始 profile 中同时接入：接收 QQ 入 I，首个/后续主体步委派 child，child 通过注册的 HostRuntime/Seat 控制现有 Claude Code/OMP，按 §35 继续开发 Shenko。启动器不把这个过程改成主体直接 shell，也不要求先把主机工具链移植进 WASM。缺少 API、TUI 高亮或其他观察能力时，主体可利用能力目录和自解释提出并委派插件开发。

默认第一条时间线始终是同一个主体。QQ、Web、Playground 等输入接入同一 Inbox 与注意力体系；后续连接新渠道不会再启动一个“该渠道的主 Agent”。

## 9 错误合同与必要验收

| 错误 | 规定结果 |
| --- | --- |
| `SEED_CLOSURE_INCOMPLETE` / `WASM_PROFILE_UNAVAILABLE` | 保留已有状态，显示缺少工件/profile；不能宣称家园已 ready。确定性入口继续提供诊断/导入正确发行 |
| `DATA_PATH_CONFLICT` | 不覆盖已有目录；保留绑定问题及可选导入/新路径 |
| `OWNER_ALREADY_CLAIMED` / `CLAIM_INVALID` | 返回已有认领状态或拒绝，不按访问先后授予 admin |
| `MODEL_AUTH_REQUIRED` / `MODEL_SELECTION_REQUIRED` | 只请求缺失项，不要求重做整个首启流程 |
| `MODEL_UNAVAILABLE` / `PROBE_EFFECT_UNKNOWN` | 草稿/凭据/证据保留；确定性入口可继续配置；未知 Allocation 不清槽 |
| `LAUNCH_CONFLICT` | 查询已有 genesis/root/selector；不创建另一主体 |
| `COMMIT_UNCERTAIN` | 按 §41 停止旧 writer 新派发，重开以原 identity 查真实提交；不通过新 launch_id 猜测性补启动 |

| Case | 测试情形 | 通过条件 |
| --- | --- | --- |
| BOOT01 | 无网络、无模型启动完整 seed；中途在 home/Git 初始化后断电 | 确定性 UI、持久存储、真实 WASM 与自解释基础闭包可初始化/续接；不伪装正在思考，不覆盖既有数据 |
| BOOT02 | 首个陌生网络访问与有 claim capability 的明确人类认领竞争；认领响应丢失后重试 | 网络先到不获 admin；明确认领只产生一个 grant；普通聊天权限与 admin 独立 |
| BOOT03 | 第一次模型连接无旧 endpoint；probe armed 后失联且 C=1 | 配置入口仍可用，草稿/诊断保留；没有虚构旧模型回退；probe 占用留账，实际停止或独立资源证明前不超配 |
| BOOT04 | 两浏览器同时 launch；在 genesis 提交前、落盘未响应、提交后未调度分别终止 | 同一安装只有一个 genesis/root/first step；第 0 步是 genesis 事实而非模型思考；恢复以原 receipt 调度同一 step 1 |
| BOOT05 | 导入成功的四流 128K profile；模型接通后不发消息 | 保留真实已知配置；主体以有限 S1 后继持续串行推进，child 能获资源；主工具集无直接文件/SSH；不因无人输入停成低频 tick |
| BOOT06 | 单独安装、替换或停用一项 awareness，例如网络变更影响意识；直接调用配置基元，并在无模型时恢复首启；之后接入 NapCat/tmux 或重开既有实例 | 其它 awareness 的安装状态不被连带改变；配置执行、探测和恢复 capabilities 按各自合同仍可调用，确定性首启不等待意识的 LLM 判断，无隐藏五阶段硬门或强制总插件；接入不生成第二主体，QQ→child→seat 可按 §35继续；恢复不重做出生/认领 |

## 10 合并时需要修改的既有字段与描述

1. 正文插件主干统一为 cordis-rs，删除“colibri-rs 身份待确认”的现行问题；原话保留为历史。
2. `SubjectBranchHead` 增加 `genesis_ref`；允许初始化 `committed_step_no=0`、`committed_step_id=null`、`committed_at=genesis commit`，明确 step 1 的 expected-parent 语义。
3. `ActivationSelector` 增加无既有 selector 时的首次初始化 CAS 语义；首版单机的 genesis/root/head/firstStep/selector 在 L 同事务建立。
4. 允许已分配 subject/conversation identity 在无主体根时作为 bootstrap 目标；bootstrap 任务/模型 probe 有普通持久 task/request 身份，不能绕过 broker。
5. §43 区分可独立安装的具体 awareness、承载机制、独立配置/探测/恢复 capabilities 和组合它们的版本化策略/plan；awareness 偏向补足“没想到”的直觉，固定候选/验证/应用/观察流程只属于具体 plan，已有事实、效果和恢复基元的合同继续成立。
6. 首启失败无旧 profile 时的落点是可用的确定性配置入口；不得把“先有可回退旧模型”写成首次接通模型的必要条件。
