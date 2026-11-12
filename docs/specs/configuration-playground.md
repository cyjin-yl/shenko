# Configuration Service & Playground — Functional Contract

**内核定位。** 参考插件，建立在[内核合同](kernel.md)之上。配置变更是带回执的效果（[内核 5](kernel.md#kernel-5)），服务与处理器经[内核 7](kernel.md#kernel-7)登记。

[§21](web-interface.md#spec-21) (single subject timeline / Proto-UI app), [§23](verification.md#spec-23)、[§41.9](inference.md#spec-41-9)、[§41.10](inference.md#spec-41-10)、[§43](configuration-playground.md#spec-43)
(configuration & playground), [§44.4](plugins.md#spec-44-4) (ownership). Runtime modules:
`shenko-ui::{config_service,owner,playground,playground_backend}`; shared
composition root: `bin/shenko/src/lib.rs`.

<a id="ownership--444"></a>
## Ownership

[安装认领合同](plugins.md#spec-44-4)

- Before home setup, connectors or subject work, startup prepares the local
  installation control channel. `<data_dir>/bootstrap/claim-secret.json`
  delivers the capability to the installer; `claim-capability.json` in the
  same directory contains only its verifier digest. On supported Unix native
  targets the directory is 0700 and files are 0600, owned by the runtime user;
  opens reject symlinks, hardlinks, unsafe permissions and special files
  before reading. The native runtime does not fall back to unprotected
  plaintext storage on unsupported platforms.
- The data root itself is created with mode 0700 at creation time, not by
  tightening it afterwards. A shared parent may carry a default ACL that
  grants group or named-user access; inheriting it would leave the root
  writable by others, and owner-control would then refuse the installation.
  New directories therefore carry their own restrictive mode, and an existing
  data root keeps the permissions it already had: a root that is writable by
  others is reported as such instead of being silently rewritten or repaired
  behind the owner's back.
- Plaintext never enters stdout/stderr, logs, model context, CAS or replicated
  tables. Startup reports only the control-file path. Delivery publishes
  atomically without replacement, before its verifier; restarting between
  those publications completes the same claim. Corrupt or foreign-installation
  records fail visibly without rotation. Existing digest-only installations
  retain their previously delivered capability, rather than issuing another.
- `POST /api/owner/claim` with the capability creates the durable owner grant
  (`owner_grants` table), issues the admin bearer (`sat_*`, stored as a
  digest in the local-only `owner_admin_tokens` table — never replicated),
  and consumes the plaintext delivery file before committing the grant. The
  digest-only verifier remains local to recognize retries; claim identity is
  stable for that capability. Replaying it, including after restart, returns
  the original grant (`replayed` / `owner_already_claimed`), never a second
  grant or another bearer. A lost bearer requires explicit owner recovery;
  replay does not silently rotate administrative credentials.
- `human_principal_ref` is derived from the capability/grant, never from a
  request body. An HTTP visit is never proof. The default HTTP listener is
  `127.0.0.1:<port>`; remote access requires an explicitly authenticated
  deployment path, not an implicit wildcard listener.
- After the claim, administrative actions resolve the actor from
  `x-shenko-admin: <bearer>` (`OwnerState::actor` → `AuthenticatedActor`).
  Ordinary chat stays open: claiming installs an owner, it does not close
  the default conversation.
- `GET /api/owner/state` reports `{claimed, claim_pending}` only — no
  credential material. A consumed verifier does not mean a pending claim;
  storage errors are not reported as an unclaimed installation. The same
  ownership service is installed in model-free and model-connected startup.
- The CLI bootstrap view recognizes the committed `owner_grants` row, not
  just a separately provisioned `bootstrap_records.owner_grant_ref`. Its CAS
  reference contains the complete original grant, not the principal or
  authority object. This view is derived from the existing replicated fact;
  recovery does not require an additional grant record or any local token.

## Configuration service — [§43.1](configuration-playground.md#spec-43-1)

- One `ConfigService` per process (`shenko_ui::config_service`), constructed
  in the Start arm and shared into `UiServer` (`with_config`). Without a
  store the routes answer `available: false`.
- Namespaces register a `ConfigNamespaceHandler` (descriptor / read /
  consumers / apply). A namespace with no handler is not advertised.
- Key shape: `namespace` (e.g. `shenko.context-policy`), `logical_key`
  (e.g. `compaction_preference`), JSON `value`.
- Routes:
  - `GET /api/config/namespaces` — discover (open to every session).
  - `GET /api/config/entries/{namespace}/{key}` — read + adoption state.
  - `POST /api/config/preview` — preview a change (open; looking is never
    privileged). Returns stale base, sealed candidate ref, diff, affected
    consumers.
  - `POST /api/config/apply` — durable change. The actor is taken from the
    request credentials (`Owner` requires the admin bearer; `Subject`
    requires a delegated task), never from the body. There is **no routine
    approval gate**: the durable change receipt is the record, and consumer
    adoption is observed separately.
  - `GET /api/config/adoptions/{namespace}/{key}` — what consumers adopted.
- Durable schema (`crates/shenko-store/src/config.rs`): `owner_grants`,
  `config_entries`, `config_changes` with `UNIQUE(origin, logical_key)`
  (cross-node divergence on the same origin+key fails import honestly),
  `config_adoptions`, `owner_admin_tokens` (local-only, excluded from
  cognition TABLES). All replicated tables are `Verbatim` with
  `local_seq = updated_commit_seq`.
- Optimistic base: `expected_base_revision` mismatch → `BASE_CONFIG_STALE`,
  never a silent overwrite.

## Playground — [§43.2](configuration-playground.md#spec-43-2)、[§41.9](inference.md#spec-41-9)、[§41.10](inference.md#spec-41-10)

### Content vs business truth

- `ArtifactVersion` (file-backed, immutable version chain: bytes +
  `{revision}.json` + selected `meta.json`) is the artifact's *content*
  store. `shenko_store::ArtifactRecord` / `ArtifactManifest` /
  `RuntimeInstanceRecord` are the durable business facts; they replicate
  through cognition TABLES so a peer receives the same closure and a lost
  workspace rebuilds from facts, not from disk.

### Manifests, staging, publishing

- `PlaygroundState::sync_durable` builds the versioned manifest on every
  content write: `source_refs`, `build_and_dependency_lock_ref`, optional
  `frontend` bundle binding, optional `backend` (entry ref, sealed
  `runtime_ref`, sealed `service_contract_ref`), `state_bindings_ref`,
  `action_schemas_ref`, `operation_contracts_ref`, `health_contract_ref`,
  `recovery_closure_ref`. The manifest digest binds to its canonical JSON.
- `POST /playground/{id}/stage` (`stage_artifact`) — current head becomes a
  durable **candidate** manifest; lifecycle `staged`; the active manifest is
  unchanged.
- `POST /playground/{id}/publish` (`publish_artifact`) — one staged candidate
  (by digest) becomes active; other candidates are demoted, not deleted.
  Publishing a digest that was never staged is refused.
- `GET /api/playground` — the Proto-UI surface projection: enumerable roots
  (`PlaygroundState::roots()` from durable records — a stray playground
  directory is never mistaken for an artifact), artifacts with lifecycle,
  and the registered config namespaces.

### Backend runtimes

`BackendBinding` binds a backend to exactly one runtime provider:

- `wasi-command` — one isolated WASI preview1 guest per action; bounded
  stdout/stderr; `/home` isolated persistent rw state (16MiB/256-entry
  profile); `/artifact` read-only. Existing receipt path unchanged.
- `host-process` — a long-lived host-managed process speaking one JSON
  request/response per line on stdin/stdout (`jsonl-1`). The executable
  descriptor (`HostProcessDescriptor{program, argv, env, protocol}`) is
  sealed into CAS; the launcher re-reads and verifies it from CAS — the
  frozen input is identity, never parameters.

`ArtifactServiceController` is the launcher/owner:

- Every instance is a real selected root (`canonicalize`d artifact dir +
  dev/ino `root_identity`) plus a durable `RuntimeInstanceRecord` with
  `ProcessIdentity{pid, process_start_token, executable_digest}`.
- `recover_on_boot` (called before any launch, and in `UiServer::run`):
  durable `running` rows are re-verified against `/proc/<pid>/stat` field 22
  and the exe digest; a stale row is marked `unknown`, never silently reused,
  so a restart never starts a second copy of a live service.
- Without the controller, host-process backends answer
  `BACKEND_UNAVAILABLE` — no fail-open.

### Same task/effect operations, same launcher with Upgrade

- Every backend action (both runtimes) runs through the existing native
  task/operation/T1/E1/T2 ledger with the same `playground.backend` kind,
  idempotent `request_id`, original-receipt-wins replay, and the same
  evidence semantics (stream overflow/failure ⇒ INCONCLUSIVE, never a
  verified success from exit code alone).
- WASI completion retains the actual process exit code. A zero exit plus a
  valid bounded result and verified state snapshot may succeed; a nonzero
  exit remains failure even if stdout is valid JSON. A trap, cancellation,
  or missing conclusive observation does not acquire an invented exit code.
  A host-service JSON response is not a process-exit observation.
- Generation hand-off: the update seam stops long-lived services through
  `ArtifactServiceController::quiesce_for_handoff()` (returns exactly the
  live processes it stopped; the handoff receipt must name those) and
  restarts them after an abandoned activation via
  `resume_after_abandoned_activation()` under the current selected root,
  re-verifying root identity so a copied slot is never adopted. Handle is
  reachable via `UiServer::artifact_services()`. (The seam that calls these
  during the [§41.12](self-update.md#spec-41-12) phase machine is the update slice's; this slice provides
  the launcher contract it drives.)

### Frontend — [§21](web-interface.md#spec-21)

- The one served UI is the Proto-UI app. The Playground is a view of it:
  `projection.ts` adds the `playground` view + `playground_surface` node;
  `react-renderer.tsx` renders `PlaygroundSurface` (roots, artifacts,
  config namespaces — only what the API returned); `live-controller.ts`
  fetches `/api/playground` + `/api/config/namespaces` on view select.
  Unavailable is rendered as unavailable; no placeholder data.

## Native proof recipe (for the integration owner)

1. Obtain the claim capability from the local private `claim-secret.json`
   control file without copying it into command arguments or logs;
   `POST /api/owner/claim` over the protected local/access channel; keep the
   returned bearer out of transcripts and evidence archives. Startup stderr
   is not an acceptable credential-delivery channel.
2. `GET /api/owner/state` → `{claimed: true, claim_pending: false}`; a
   second claim with the same capability → `replayed` with the original
   grant and no bearer. Restart and repeat; no new delivery file appears.
3. Register one namespace handler; `discover` → read → `preview` (stale
   base check) → `apply` with and without `x-shenko-admin` (denied vs
   receipted); restart; read again — the entry and its change receipt
   survive; adoption row appears only after the consumer reports.
4. Playground: `PUT /playground/{id}` content → `PUT .../backend` (wasi) →
   `POST .../backend/action` with a `request_id`; repeat with the same
   `request_id` → original receipt, `replayed: true`. Bind a host-process
   descriptor; invoke twice; `ps` shows one service process; kill the
   process; restart the node; the durable row says `unknown` (or is
   re-adopted only after the live check passes); no second copy runs.
5. `POST /playground/{id}/stage` then `/publish` with the candidate digest;
   `GET /api/playground` shows lifecycle `published` and the manifest under
   the artifact's record.
6. UI: open the Playground nav view; roots, artifacts and namespaces render
   from the API; disconnect the store path and the surface reports
   unavailable instead of placeholder rows.

## Honest boundaries

`ArtifactServiceController` handoff closes admission before taking its bounded
service lock. Linux host services launch in a dedicated process group; quiesce
captures the exact PID/start-token/executable identity before signalling and
waits for group termination before releasing the runtime claim. A failed stop
or unproved group scope retains the owned handle/claim and returns an error.
This is a process-group contract, not proof of containment for a service that
deliberately escapes that group; stronger providers must supply their actual
container/cgroup boundary. Resume retains unfinished restart identities and
fails on missing runtime rows, unreadable descriptors or CAS digest mismatch.
No post-wait PID lookup is used to manufacture an empty stop receipt.

- Builds/tests/native runs were not executed in this slice (parent
  serializes integrated verification); the proof recipe above is the
  acceptance path.
- `owner_admin_tokens` is local-only by design (credential); config and
  artifact tables replicate, so a same-origin config change diverging
  across nodes fails import instead of merging silently.
- Workspace replication of the playground *directory* into a peer's
  cognition export uses the existing peer `workspaces` config
  (`node_sync`); the enumerable-roots contract itself is the durable
  `artifact_records`, not the directory listing.

<a id="spec-43"></a>
<!-- migrated-contract: SPEC:43 -->
# 43 对话配置与持久 Playground

<!-- /migrated-contract: SPEC:43 -->

<a id="spec-43-1"></a>
<!-- migrated-contract: SPEC:43.1 -->
## 43.1 所有配置可通过对话管理

主体能通过对话发现、解释、修改自身与已接入能力的配置；人类也有可查看实际配置的面板。配置命名空间由插件注册，覆盖当前实现暴露的所有设置，不设一份永久只读/不可改字段白名单。尚未暴露成配置的编译选项或行为，可由主体委派开发 schema、插件或代码并走自更新；不能为了宣称“配置可改”虚构尚未生效的字段。 awareness 是可独立安装的直觉与意识，帮助想到相关情境、知识缺口与后果；不同意识分别成为小单元，不设包办全部配置/恢复的总插件，见 [§45](cognition-units.md#spec-45)。

“不改坏”是产品体验目标，由当前采用的能力、策略、执行证据和实际恢复路径共同实现；不承诺任意错误代码、外部断网或已销毁数据都能无损恢复。准备候选、检查、应用、观察及确认是可选的计划步骤，不是本节配置接口的唯一合法顺序。配置、探测、恢复及 awareness 的实现和 core 均可替换，不新增每次改配置必须经人类批准的步骤。

~~~ts
type ConfigNamespace = {
  namespace: string;
  schema: SchemaRef;
  owner_plugin: VersionedRef;
  read_contract_ref: VersionedRef;
  apply_contract_ref: VersionedRef;
  capability_refs: ObjectRef[]; // 可发现的检查、观测、恢复等能力；不要求全有
  application: "hot_reload" | "restart_service" | "generation_handoff";
  effect_contract_ref: ObjectRef;
};

type ConfigChange = {
  change_id: string;
  logical_key: string;
  origin_event_or_task_ref: ObjectRef;
  actor_and_authority_ref: ObjectRef;
  target_refs: ObjectRef[];
  desired_patch_ref?: ObjectRef;
  base_snapshot_ref?: ObjectRef;
  candidate_snapshot_ref?: ObjectRef;
  coordinating_task_id?: string;
  awareness_assessment_refs: ObjectRef[];
  decision_refs: ObjectRef[];
  operation_refs: string[];
  observation_refs: ObjectRef[];
  recovery_program_refs: ObjectRef[];
  activation_refs: ObjectRef[];
  strategy_projection?: { schema_ref: SchemaRef; data_ref: ObjectRef };
  record_revision: bigint;
  created_commit: CommitRef;
  updated_commit: CommitRef;
};

record_config_intent(intent_ref: ObjectRef, logical_key: string): Promise<ConfigChange>;
get_config_change(change_id: string): Promise<ConfigChange>;
// 实际动作使用选定 capability 的操作合同和 §41 的 T1/E1/T2；
// capability 选择不是省略当前配置 revision、真实授权或 effect identity。
~~~

配置服务规定操作事实和组合边界；以下条目不是要求依序经过的阶段：

1. 对话中的修改由主体提交意图并委派执行，或由已经获得相应委派的任务执行。每次实际写操作冻结明确参数、目标 schema、授权和 expected base revisions；不以一个尚会变化的草稿作为已派发 operation 的输入。以 `(origin_event_or_task_identity, logical_key)` 查重，幂等命中先于当前 revision 校验；不同 payload 不复用同一个 logical key。CAS 冲突返回 `BASE_CONFIG_STALE`，由执行者复核/重建意图，不静默覆盖新值。

2. 检查能力分别声明检查什么、输入版本、结果与成本。当前策略可以先检查、复用已有检查、事后检查或明确不检查；不存在全局 `validated` 标记作为 apply 的通行证。具体操作仍须满足其数据格式、引用可解析性、权限和实际执行前置条件，这些不等于业务策略的预验证。网络/模型探测作为独立 operation，使用真实目标、路由和凭据句柄；模型探测计入同一物理 broker。

3. hot reload 由消费者声明实际采用 revision 的提交/确认点；涉及服务 restart 或软件 generation handoff 时使用适用的既有执行/Activation 合同。需要多个消费者分别采用时如实返回逐消费者状态；无法原子改变多个目标时，具体计划声明暂态和部分成功。保存配置不等于服务已采用；可以直接提交变更，不强制先创建一份 candidate snapshot。

4. 实际采用、健康观测和策略判断分开记录。某个服务可能已采用新代理但连接失败；界面不能把这种事实写成“尚未生效”，也不能仅因已采用就显示健康通过。策略可选择观察窗口、复用证据、跳过某项检查或继续跟进。未执行标为 not_performed/unknown，过期或矛盾证据明确保留；`effective` 若用于 UI，限定为消费者已采用，不能兼指全部健康保证。

5. 失败后的选择由主体或受委派执行者按策略决定：恢复旧配置、前进修复、调整目标、继续观察或保留已知异常。选定恢复动作按当前 revisions、实际状态与效果合同执行，不能覆盖无关新改动。已物化的恢复程序可在没有在线模型时执行；没有相应程序或控制路径时如实报告覆盖缺口，不假称自动恢复已准备。首启没有旧模型时的落点见 [§44](plugins.md#spec-44)。

6. 配置恢复不撤销已经发出的消息、远程命令或数据迁移。这些 effects 继续由 [§41](architecture.md#spec-41) 账本处理。secret 字段保存凭据句柄；面板显示名称、来源和状态，不暴露私钥/token 原文。策略、awareness 实现或 UI 的更换不改写已提交事实。

**可能切断自身模型/代理/消息入口时。** 默认策略可借助相关 awareness 能力识别控制链依赖，并优先选择已有可用恢复路径，或委派恢复能力持久化一个不依赖受影响模型的 RecoveryProgram。其工件包括适用条件、原 route/secret refs、执行入口、操作合同、完成/停止判据及真实恢复范围；它是独立执行服务承担的任务，不由 awareness 能力全包，也不依赖继续装着最初提醒它的插件。策略可改为前进修复、复用已有机制或接受明确异常；没有程序时不得声称已有自动回退保证。外部凭据撤销、物理断网或存储损坏超出覆盖时，保留未决事实。首次模型配置失败则返回可用首启入口，不要求一份不存在的旧模型配置。

可由恢复能力物化的最小记录如下；它属于具体任务的执行与恢复服务，并非 awareness 单元的必填接口。

~~~ts
type RecoveryProgram = {
  recovery_id: string;
  program_ref: VersionedRef;
  owning_supervisor_task_id: string;
  executor_service_ref: VersionedRef;
  executor_recovery_closure_ref: ObjectRef;
  captured_inputs_ref: ObjectRef;
  target_binding_refs: ObjectRef[];
  control_route_and_secret_refs: ObjectRef[];
  authority_ref: ObjectRef;
  operation_contract_refs: ObjectRef[];
  applicability_predicate_ref: ObjectRef;
  trigger_contract_ref?: ObjectRef;
  completion_predicates_ref: ObjectRef;
  resource_scope_ref: ObjectRef;
  checkpoint_schema: SchemaRef;
  replacement_or_retirement_decision_ref?: ObjectRef;
};
~~~

没有自动 trigger 的程序可以由任务显式调用；自动触发采用哪个条件属于所选计划。程序使用的实际配置与服务版本、适用条件及停止证据都要可查。停用相关直觉不隐式撤销已提交任务；替换承载执行器的代码则处理它自己的依赖与持久交接。

端点探测 armed 后断连时，可能已占满 C=1。如果当前计划选择恢复配置，已物化的程序可经独立于候选模型/代理的确定性控制路径执行，不必先等待另一次模型健康探测取得 permit。原 ComputeAllocation 仍 held；恢复配置、换 URL 或等超时均不证明探测停止。执行者继续 query/stop，或按合同隔离/销毁 engine，取得真实 QuiescenceProof 后才释放原占用；仅断开网络仍不足以证明执行已停。立即换后端推理必须确认属于独立物理资源组且有真实容量。可用控制路径、实际动作、停止证据与后续模型验证分别记录。

管理员身份仍来自人类明确设置。具有相应权限管理权的已认证人类（包括受信初始化/恢复入口中的所有者）可以在对话中明确授予/撤销管理员，系统保存 authenticated actor、具体对象、权限范围和决定引用，由权限插件执行；面板不是唯一入口。联系人别名、声纹候选和模型对人的判断不能自行变成管理员凭据。普通配置改动不被这一条扩张成一律需要人类审批。

配置面板读取同一服务，分别展示保存的 desired revision、各消费者实际采用 revision、健康证据/未检查状态，以及恢复程序的可用性与覆盖。选定策略如有候选和阶段，可按其 schema 展示，不为所有修改套同一进度条。面板编辑也提交同一 intent/操作合同，不写另一份与对话脱节的配置。入口按需打开，首页保持主体时间线。

<!-- /migrated-contract: SPEC:43.1 -->

<a id="spec-43-2"></a>
<!-- migrated-contract: SPEC:43.2 -->
## 43.2 Playground 是长期存在的全栈作品空间

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

**持久与发布。** 源码、依赖锁、前端产物、后端入口、数据 schema 和恢复闭包进入版本化存储，实际位置遵守 managed snapshot / host bind / remote 的边界合同。代码版本与数据 revision 分开；升级代码不自动清空数据。具体发布策略可以组织候选构建、能力检查、隔离 smoke 与当前 manifest 切换，也可复用产物或采用其他适用流程；这不是所有作品的全局五阶段门槛。当前 manifest 的真实选择点、外部效果和 runtime/state handoff 仍遵循其执行/Activation 合同。旧页面携带旧 manifest 的 action 使用兼容旧合同或返回 `ARTIFACT_VERSION_STALE`，不能套用新同名 action 的不同语义。软件回退保留新版本期间的用户数据与外部事实，依据实际 schema 兼容或迁移能力恢复。

**后端与能力。** Artifact backend 使用注册的 RuntimeProvider，可位于 WASM、宿主受管进程、BoxLite/容器或远端节点，声明实际部署能力；无所需 runtime 时保持可恢复的 draft/stopped 状态，不把静态前端可见冒充后端正在运行。任务、网络、文件和远端操作遵循 manifest 中 operation/effect 合同；需要模型的 artifact 后端也经过共享 broker。凭据留在相应 secret/runtime 服务，不能把后端私钥打进前端 bundle。入口的实际授权遵循当前主体/用户/委派关系，点击网页不自行获得 artifact 创建者的全部权限。

**UI action 到主体感知。** `invoke_artifact_action()` 从认证会话取得 actor，按 `(artifact_id, authenticated_actor, action_id)` 查重；相同 payload 返回原 receipt，冲突返回 `ACTION_CONFLICT`。查重命中先于 expected state revision 检查。服务校验 manifest/action schema、当前权限与目标状态，再持久接纳 action 和执行意图；外部效果按 T1/T2 运行。

有合作后端的部署中，artifact **实际业务数据库**是业务状态变更的权威：业务状态/revision、action 去重记录与完成 receipt、待导出的稳定 event_id/outbox 记录，在同一源数据库事务中提交。不能先写业务数据，再到 Shenko ledger 补一条幂等标记来冒充原子执行。Shenko 导入端按稳定 event_id 持久接纳 InboxEvent 后才 ACK；源端可重复投递，ACK 丢失时导入去重，不再次执行业务动作。源数据库回执与 Shenko 导入回执分别保留。

第三方或非合作远端后端不支持此源事务合同，或者动作还包含源 DB 之外的外部效果时，不能承诺“重复请求普遍只执行一次”。适配器按真实幂等、查询与 receipt 能力处理；结果不明时进入原 operation 的 reconcile/unknown，不能通过重复发送补做。主体感知的是已确认事实及明确标注的不确定状态。

已提交的 `artifact.action`、`artifact.state_changed`、后台结果、故障及适用的外部状态变化进入**同一主体 Inbox**，包含 artifact/manifest/state revision、actor、cause、delta 摘要和精确原始引用。模型在 S0 选择、S1 处置这些事件，能观察人的交互和作品的变化；不能只让浏览器改 DOM 而没有服务端事实。浏览器本地临时 hover/滚动/动画不逐个触发模型，需要表达业务意义的动作由 action schema 显式定义。

**反馈抑制不等于失去感知。** 事件携带 root cause、operation/action identity 和 origin（human/subject/worker/external）；actor 与 origin 从认证和服务端任务谱系取得，不相信客户端自报“这是主体自己的更新”。同一状态变化的服务端回执、UI 回显和消息同步合并成一个语义刺激；主体刚提交的变更可作为自发事实归档而不再次伪装用户新指令。原始记录保留，模型仍可查询。高频拖动、采样或运行日志可确定性聚合并保留范围/分页引用；不合并掉不同的用户提交，不以内容 hash 消除两次相同但独立的动作。不要求每个浏览器事件额外跑一次 LLM 分类器，也不禁止主体有意持续观察自己的实验。

**作品生命周期与界面。** 作品的 desired lifecycle、后台任务 ownership、数据保留和 runtime 实际状态分别记录；重开后按这些记录恢复，先核对原服务/operation，避免启动第二份。关浏览器不停止后台；暂停/归档显式形成决定。存储不足后的迁移、归档和清理仍由既有存储插件自主处理，默认不因“聊天结束”设置短 TTL。

Playground 使用 [§21](web-interface.md#spec-21) 的 Proto-UI/shadcn 与动态界面能力协商，保留 compiler 接缝；其前后端可以是模型生成和持续演进的实际应用。Web、窄屏和 IM surface 根据能力提供交互、只读视图、链接或摘要，不能展示实际不可用的控制却假报成功。模型通过能力/DSL 描述与操作 schema 知道能生成什么。Playground 是按需入口，作品的必要结果或互动可以嵌入主体时间线，首页依然不变成作品管理大屏。

<!-- /migrated-contract: SPEC:43.2 -->

<a id="spec-43-3"></a>
<!-- migrated-contract: SPEC:43.3 -->
## 43.3 本轮验收增量

| Case | 情况 | 可判定结果 |
| --- | --- | --- |
| UI01 | 主体 generation 中连续发送 append | 当前请求不因这些输入被打断；输入可靠接纳，在后继 S0 公平选择；不建 chat agent |
| UI02 | 后端无请求内控制却发送 steer | 真实显示 next_boundary；不篡改旧 seal，不假报已打断 |
| UI03 | accepted 响应丢失，浏览器重开并同 key 重试 | 只有一条 InboxEvent；可恢复 presentation 和 S1 处置；不重复触发任务 |
| UI04 | withdraw 与 S0 竞争，或 S1 后才撤回 | 按提交先后返回未呈现撤回或撤回已排队；已产生的外部效果不被谎称撤销 |
| CFG01 | 当前策略已物化恢复程序，替换端点/代理失败；另测 armed 探测失联并占满 C=1 | desired、实际采用与健康分别记录；程序可经独立控制路径恢复或按计划修复，不等第二次模型探测；原 Allocation 仍 held，真实停止证明后才释放；无旧 endpoint 的首次连接失败返回首启配置入口，不虚构回退 |
| CFG02 | 采用不同合法策略修改配置；期间断电后重交同 change key | 依据原 intent、operation、receipt、消费者 revision 和策略专属状态恢复；不要求固定阶段，不把候选当实际采用，不把未做检查当通过，不重复外部效果 |
| CFG03 | 有权限管理权的已认证人类明确设置管理员；另有声纹高相似候选或普通已登录群友 | 前者有手动决定与权限回执；声纹候选或普通登录本身不自动获得管理/授予权 |
| PG01 | 创建带后端与数据的作品后关页面、compact、重启 | 源码/版本/数据保留，按实际 runtime 能力恢复；无重复后台进程 |
| PG02 | 满足源业务 DB 原子事务合同的部署，action 已提交但响应/导入 ACK 丢失后重发；另测非合作后端 | 前者幂等命中原 receipt，业务变化一次，稳定 event_id 重投只导入一次；后者按真实能力查询/核对并保留 unknown，不普遍承诺 exactly-once |
| PG03 | 发布新 action schema 后旧页面继续提交 | 正确执行旧合同或显式 stale；不使用新语义误解释 |
| PG04 | 主体改 UI，UI 回显，用户随后做两个相同动作 | 自己的同一变更不形成反馈风暴；两个独立用户动作均保留且可感知 |
| PG05 | 回退作品代码，而新版本期间已收用户数据 | 新事实保留，按兼容 reader/迁移恢复；不靠删除新数据让旧版本看似健康 |

<!-- /migrated-contract: SPEC:43.3 -->

<a id="spec-57-6"></a>
<!-- migrated-contract: SPEC:57.6 -->
## 57.6 作品是主体伸向界面的手

**合同。** 新增 `playground.manage:1` 原生任务：主体可创建、迭代、暂存、发布、回滚作品，并按 [§36](durable-state.md#spec-36) 声明副作用与恢复边界。作品状态变化经持久事件进入主体同一 inbox。作品在时间线内可打开、可操作，动作按 revision 绑定（[§21.8](web-interface.md#spec-21-8)）。

**动态界面是界面，不是调试视图。** `rich` 文档由真实组件渲染并按 `SurfaceCapabilities` 降级（[§21.5](web-interface.md#spec-21-5)、[§21.7](web-interface.md#spec-21-7)）；节点与组件可由插件扩展。DSL 与工具两条入口共享同一语义对象与动作账本（[§21.6](web-interface.md#spec-21-6)）。

<!-- /migrated-contract: SPEC:57.6 -->
