# Agent-owned node choice / Agent 自主选择执行节点

## Actual installed binding → policy input — 2026-10-31, source only

Base `1dc81377b33ad651420d495fbb705d6c77385074` has
[50 passing tests](https://github.com/cyjin-yl/shenko/pull/127#issuecomment-6550185169).
This new source and its three test functions/four new scenarios are **UNRUN**;
old evidence below stays attached to its original source.

The permission call no longer sees only a logical binding reference.
`InstalledSshBinding` consumes an existing explicit `SshBinding`, normally
created by the real `SshBinding::register` API. Its private, non-deserializable
fields bind one installation, node, provider/object/revision, endpoint, account,
sealed public-trust object and local authentication selection. The trusted
installer supplies this value directly; there is no new registry, environment
discovery, model JSON loader, default localhost/root binding or default allow.

`inspect_selected_permission` requires that exact installed input, checks it
against the original Node-registered descriptor, verifies the sealed trust in
the current CAS, and passes actual host/port/account plus public-trust digest
and a non-secret local auth-selection name to the injected policy code. Schema
`ssh.permission-inspection.v2` binds those facts and the complete local binding
digest into the same nonce/action/task request. The old internal call site is
migrated directly, not kept as a permissive legacy path. The bridge rechecks
the binding/CAS after await and keeps the preceding Store/Node fences.

Key bytes, private-key paths, agent sockets, known_hosts contents, CAS locators
and receipt directories are not in the policy projection or public result.
No key/agent/environment is read or probed. The auth label is installer-owned
non-secret metadata, not a token or proof of possession; credential availability
is explicitly `not_checked`. The full binding digest is correlation, not a
grant. Endpoint/host-trust data is installation-supplied, not a remote handshake
or policy assertion. Existing CAS/path TOCTOU limits are not fixed by this read.

Why: real policy must know the actual destination and trust/configuration it
is judging, rather than infer them from an opaque reference. This is a concrete
library integration with existing registration/CAS, not another always-allow
provider. Matching installation metadata still grants nothing: the result
remains `PolicyObserved / not_granted / not_dispatched`. No execution API takes
it as permission; original SSH execution and armed-operation recovery are unchanged.

Positive acceptance uses real registration → sealed CAS → exact installed
reference → actual synthetic policy callback with resolved destination facts.
Changing the original public known_hosts source file does not replace sealed
trust. Negative controls cover missing/wrong-revision installation before the
callback and corruption of this test's public CAS object during callback await.
Three new test functions/four new scenarios; existing policy tests also assert
the minimal projection. Fixtures contain only synthetic public trust and an
explicitly absent private-key path; no credential file is created/read and no
SSH process is started. Planned same-filter regression is **53 tests, not passed**.

Remaining real interfaces are explicit: Bootstrap's opaque owner-grant ref is
not an SSH policy interpreter; `NativeTaskDescriptor.authority_refs` is not a
target grant; SyncPeer is a replication route, not this registered action
binding. A production user-policy/privacy/revocation resolver, resource
admission/fencing and installed SSH child with fresh live dispatch authority
are still needed. The trusted installer must supply its actual registered
binding/ref and a non-secret authentication-selection name; this slice neither
creates credentials nor reads a user's machine to find them. Missing inputs
fail closed, rather than inventing authority or declaring the final product done.

Only exec-ssh and this scope document change. Cargo.lock stays `7dabe683…`;
no new dependency/version/download/execution. Fresh main `9aac97c`, #114
`47b26f98`, #91 `c61913d0`, #130 `4fc8a5c` remain untouched; #127 stays Draft.
No bin/parser/Store/Node ownership changes or combined latest-main validation.

## Store-fenced permission callback — 2026-10-31, source only

Base `9720bcd3d8abd62efb317d424c0c34f6b6e5847b` has separately recorded
[44 passing tests](https://github.com/cyjin-yl/shenko/pull/127#issuecomment-6550009239).
The older source-only sections below are historical checkpoints; their later
execution does not validate this new increment. This candidate and its six
new tests are **not compiled or run**.

The bridge now calls the existing `Store::native_invocation_current` before
handing the action to policy code and again after the callback and reference
re-read. Its transaction checks the current dispatcher, subject controller
epoch/software generation/mode, task invocation/revision/cancellation and
active ancestor ownership. The existing metadata/assignment checks remain.
False returns `InvocationNotCurrent`; missing/unreadable state or a failed
ownership check returns `InvocationCheckUnavailable`, without raw Store errors.
There is no missing-head fallback and no new Store API or policy table.

Why: a valid provider reply must not survive a local invocation that was
superseded while policy awaited. The checks are snapshots, **not a reservation
or an atomic dispatch gate**. No Store/Node transaction is held across await.
The true policy provider still owns action scope, privacy, historical choice
lineage and policy revocation. Even a current invocation and `Allows` reply
produce only `PolicyObserved / not_granted / not_dispatched`; no execution API
consumes that as authority. Real binding, resource admission and installed SSH
child integration remain open, as do live dispatch permission and T1 fencing.

The five existing policy tests now use real `initialize_genesis`,
`start_subject_dispatcher` and `claim_native_task` calls in a synthetic fixture.
Six new negative controls cover missing runtime state, stale generation before
payload disclosure, and generation change / ancestor-only cancellation / task
revocation / dispatcher takeover during a genuinely suspended policy callback.
Cancellation and takeover use public Store APIs. Only generation replacement
uses explicit SQLite fault injection because this base has no public setter;
this does **not** test a production migration path. All refuse observation and
leave no SSH operation/T1/evidence. Existing valid observation still grants
nothing; existing later policy denial and original-only recovery stay covered.

The only dependency change is a test-only edge to the already locked
`rusqlite` workspace package. Cargo.lock gains that one edge; the complete
package/version/source/checksum set, 401 registry archives, cordis revision and
features requested from the workspace remain unchanged. No dependency was
downloaded or executed for this candidate. No Store/Subject/Node/bin source,
schema, real credential, SSH, model, production account or deployment changes.
Fresh ownership at 22:35 UTC: main `9aac97c`, #114 `47b26f98`, #91 `c61913d0`,
#130 `4fc8a5c`; their product source stays untouched. The shared Cargo.lock
overlap with #130 is separate package edges (SSH test rusqlite vs Subject
channel dependency), inspected statically, not a combined integration claim.
#127 stays Draft with #114 dependency.
Planned fixed-source regression is **50 tests, not passed**: selected SSH 23,
selection handler 3, query handler 3, startup 2 and Node 19. Current-main
compatibility was inspected statically, not integrated/executed.

## Injected permission-policy inspection — 2026-10-31, source only

Base `426ebf06550e5b86935d996c854ee83631c550c1` was separately verified with
[39 passing tests](https://github.com/cyjin-yl/shenko/pull/127#issuecomment-6548886214).
The five new permission-bridge tests below have **not been compiled or run**.
This increment does not extend the old result to the new source.

`SshTaskRunner::inspect_selected_permission` now actually calls an injected
`SshPermissionProvider`, rather than stopping at the descriptor's unresolved
permission reference. This is a library integration seam, **not a shipped
production policy resolver, installed SSH child or dispatch admission**.
There is no default-allow implementation and no provider loaded from a model
string. The trusted installer supplies code and verifies its implementation
digest; a self-reported digest alone is not authentication.

The bridge reads the existing task/subject-bound target reference, matches the
exact provider ID before disclosing action bytes, and constructs a request
covering task invocation/generation/assignment, delegation, subject, observer,
explicit decision/node, operation, logical key, timeout, descriptor digest and
all provider references, implementation digest and exact serialized action
digest. A fresh observation ID and schema domain are included in every request
digest. The provider can inspect the complete borrowed action/stdin/upload
payload in process. Raw action bytes are neither placed in this result nor
automatically logged/sealed by this bridge; remote policy forwarding needs its
own existing authorization and data-access contract.

Missing provider, wrong provider, denial, unknown policy, fault, timeout,
response mismatch and changed references remain typed `Unavailable` outcomes.
No raw provider error string is projected. A 5-second cooperative async deadline
does not preempt blocking native code; the trusted adapter must not block the
executor. Node locks are released before the callback. After the callback, the
bridge rechecks provider identity/version and the persisted task/Node reference
snapshot. Repeated calls have fresh identities and query policy again: there is
no cached allow, automatic retry or durable permission lease.

Even a matching `Allows` reply yields only `PolicyObserved`, with explicit
`execution_authority: not_granted` and `dispatch: not_dispatched`. No execution
API accepts this output as a permit. A nonce/digest proves correlation, not the
truth of a provider's answer. `selected_target_requirements` still does not
prove current software generation or that an old selection belongs to the
current delegation; the actual user policy resolver must check delegation,
privacy, generation, current policy and revocation, or report unavailable.
Real target/account binding and resource admission are still unresolved. Before
actual dispatch a real child must obtain live authority and use Store T1
fencing; an earlier inspection can never replace those checks. Armed work is
refused by the existing helper and remains on its original recovery path.

Why: wire the missing policy call without putting user policy in the kernel or
manufacturing authority from metadata. No credential is created/read, no new
ledger/cache/schema/dependency is added, and the existing trusted SSH execution
API is unchanged. Provider inspection is read-only; reservations or other
effects must use the real operation/attempt contract, not this callback.

Five test functions (seven independent new scenarios, below the increment's
ten-test limit) use an explicitly synthetic in-process policy adapter: absent
and wrong provider before payload disclosure; full-scope observation without
T1 and subsequent denial; replayed old response for identical input; task
change during the callback; and sanitized provider failure. They are **UNRUN**.
No real user policy, credential, SSH transport or model behavior is tested.

Fresh ownership snapshot 22:04 UTC: main `9aac97c`, #127 base `426ebf0`, #114
`47b26f98`, #91 `c61913d0`, #130 `4fc8a5c`; no new competing claims. Only
exec-ssh and this scope document change. Keep #114 dependency, Draft status,
all historical failures and evidence, and the other owners' files. Current-main
API compatibility is inspected statically, not integrated or executed.

Remaining: install a real policy provider; resolve actual target binding and
resource admission/fencing; install the SSH child with original-only recovery;
validate current-main integration and real model/remote behavior separately.

## Registered target-reference inspection candidate — 2026-10-31, not executed

Base is fixed `6f877d90fd64e87acf8d214a556332d978c06c98`. That base's **35 passing
tests** cover the installed observation/choice journey and selected SSH library
fixtures ([exact result](https://github.com/cyjin-yl/shenko/pull/127#issuecomment-6547339982)).
They do **not** cover this new reference-inspection code or its four new tests.
This candidate is static source only, not compiled, installed or executed.

The real missing boundary is now explicit: Node `Register`/`members` retains
only `node_id`, `target_id` and an opaque `binding_digest`. Native `AuthorityRef`
binds a task/revision/originating decision, **not target permissions or resource
admission**. No production SSH target/permission/resource provider was found in
current main `9aac97c` or the inspected open PR scopes. A key path, SSH agent,
capability advertisement or registered descriptor cannot fill that gap.

### Actual source boundary

- `Node::selection_target_reference_for_subject` reads the original sealed
  selection receipt and existing registered-target metadata in the **same Node
  identity-bound SQLite snapshot**. Metadata is capped at 4096 bytes in SQL.
  It adds no table, schema migration, credential store or administrative RPC.
- `SshTaskRunner::selected_target_requirements` checks the supplied persisted
  task/revision/assignment/authority/operation bindings and same-agent Node
  identity, then recognizes only an exact registered descriptor digest. The
  descriptor is at most 8 KiB, strict JSON, version `ssh.target-references.v1`.
  Caller-supplied bytes cannot replace the registered node/target/digest.
- The descriptor has `node_id`, `target_id` and three logical references:
  `binding_ref`, `permission_ref`, `resource_ref`. Each is
  `{provider, object_id, revision}` with bounded logical identifiers, not a
  URI/path, credential value, token, lease or grant. Unknown fields, including
  credential fields, are rejected. Legacy/unknown digest schemes and unknown
  descriptor schemas are refused, never silently promoted or rewritten.
- The result type has **no Authorized, Ready or Admitted variant**. Even a fully
  valid descriptor returns `RequiresProviders` with `not_granted` and
  `not_dispatched`. It does not inspect provider objects, discover credentials,
  probe a host, arm T1 or execute any SSH action.
- Already-armed work refuses current-registry resolution. Original frozen
  selection/binding and query-only SSH recovery remain unchanged, including
  legacy v1/v2 receipt compatibility. A new descriptor cannot redirect an old
  effect or create a replacement attempt.

Why this slice: it makes a real, bounded bridge from existing Node registration
and choice evidence to named **unresolved** provider dependencies, instead of
inventing an allow policy. It is a trusted library inspection API, **not an
installed SSH child and not a new enforcement gate on `execute_selected`**.
That existing API still requires an already-authenticated, independently
authorized caller; this inspection cannot be used as an admission token.
Descriptor retrieval from an installation's actual content/provider service
is still a caller responsibility; this method accepts verified bytes and never
follows model-supplied locators or filesystem paths.

### Smallest remaining data/interfaces, not permission inferred from metadata

1. A real installed target-binding provider resolving the exact logical ID and
   revision into the existing `SshBinding`: actual target identity, immutable
   host trust and node-local existing credential **handle**, not copied secrets.
2. A real permission authority answering for the current subject/delegation,
   selected node, exact action/data scope and operation, including revocation
   and privacy restrictions. A matching descriptor/hash is not that answer.
3. A real resource/admission authority with current limits/reservation identity,
   executor fencing and unknown-effect retention/reconciliation rules. Do not
   release a resource merely because a local process timed out.
4. An installed SSH child connecting those providers to existing Store T1 and
   original-only recovery. Fresh T1 must recheck current runtime generation and
   owner. Historical Node receipts alone do not prove current delegation.
   Query/reconciliation authority must not depend on inventing a fresh grant.

Existing `Register` rows are immutable; this candidate does not overwrite old
registrations to make the new descriptor fit. Real revision/adoption changes
must preserve old references/effects through an explicit supported migration.
No production provider objects or credentials are created in this increment.

Four tests are authored, not run: registration/descriptor absence through
valid-but-still-unresolved references with no T1/E1; unknown credential fields
and descriptor substitution refusal; foreign subject/stale worker refusal;
and armed-operation rejection using the existing missing-trust fixture before
OpenSSH spawn. The shared SSH fixture now has an explicit synthetic Node/Task
identity so the reference checks have real persisted bindings; no actual
account, key, network or model is involved. Existing 35-test coverage must be
re-run at the fixed new candidate before extending its evidence to this source.

Ownership rechecked 2026-10-31 20:24 UTC: #114 `47b26f98`, #91 `c61913d0`, #127
`6f877d9`; new #130 `4fc8a5c` owns channel/parser integration. This slice changes
none of their active core/parser/bin files. Main is still `9aac97c`; public
NativeTaskInvocation/TaskInvocation/SSH runner interfaces used here were
statically compared, but combined current-main execution is not claimed.
#127 remains Draft with #114 dependency. Real model/remote validation and full
C07/C17 completion remain open.


## Installed explicit-choice recorder candidate — 2026-10-31, not executed

The next #127 slice starts from `5e8c463fa9becf867180b9e1df0be627533874c7`
and adds a separate `node.selection.record/v1` installed handler. The existing
read-only `node.capabilities.query/v1` contract stays read-only. This new source
and its five authored tests are **not yet compiled or executed**. The previous
22 passing tests belong only to the exact base, not this changed candidate.

```text
[delegate:node.selection.record:1] {"node_id":"<observed node UUID>","observed_profile_digest":"<exact observed blake3 digest>","reason":"why I choose this node"}
```

Only these three model fields are accepted. The registry-owned operation ID
supplies `decision_id`; neither task/subject identity, authority, filesystem
paths nor SSH actions may be supplied by model JSON. The reason is nonempty
and at most 4096 UTF-8 bytes; the opaque digest must have its full canonical
format. The installed native task's originating subject step and registry
claim/authority are checked before T1. Cordis owns discovery and withdrawal.

T1 freezes the exact node choice/reason, observer Node ID and subject identity
in the **existing Store operation/attempt**, not a second preparation ledger.
Only fresh admission may record a receipt in the **existing Node commands**.
The Node checks installation/subject identity inside the same transaction as
reading or inserting a receipt, including repeated IDs. No admin RPC, new
schema/table, dependency or lockfile change is introduced. A changed profile
returns `unavailable` with current observation, never a substituted node.
`recorded` means only the local choice is retained, not readiness or permission.

Store and Node are separate databases: T1 and the local journal commit are
not atomic. Recovery therefore **only queries the original decision receipt**
under its frozen binding, validates its original command seal and full choice,
and never records again or rechecks newer advertisements. A receipt that later
becomes visible can resolve the original attempt. Missing/unreadable receipts
or an identity mismatch remain `Reconciling`/`Unknown`; absence is not proof
that a delayed original writer cannot commit. Repeated unchanged absence does
not append identical unknown evidence. Once resolved, E1 is returned unchanged.
No replay, substitute decision, new attempt or synthetic no-effect success is
allowed. Cancellation/subject ownership and T2 remain the existing scheduler's
responsibility; late receipt evidence is not authority to dispatch or resume.

An available local receipt produces `EffectComplete` for
`effect_scope: local_selection_journal_only`: `recorded` is Succeeded and
`unavailable` is Failed, but **both only describe that local journal entry**.
Every result preserves `dispatch: not_dispatched` and
`execution_authority: not_granted`. Raw provider/database error text and paths
are not projected. A decision receipt does not confer a target/resource grant.

Five focused tests are authored: strict choice fields/changed observation;
crash after Node commit before E1 with original-only recovery; absent receipt
staying unknown until a delayed original write; another connection adopting
identity causing old-bound reads/writes to refuse; and the real startup helpers
plus deterministic parser/S1 → read observation → explicit choice → T1/Node
journal/E1/T2/task_result and install/withdraw lifecycle. The integration fixture
uses no served model or SSH and cannot establish model judgment. Real model
sampling, installed SSH dispatch, authoritative target/resource resolution,
real remote execution, and full C17 remain unverified/unimplemented boundaries.

Why this next slice: the now-installed reader exposes observations, but a real
subject task could not yet retain its explicit node choice. Node `Register`
still has only ID/digest metadata, not authoritative SSH grants/resources; this
change records intent without inventing a resolver or taking #114/#91 work.

### Previous installed-reader evidence, fixed 5e8c463 only

On 2026-10-31 16:30–16:31 UTC, fixed `5e8c463` passed three query-handler tests,
one actual registration/parser integration fixture, and eighteen Node tests:
**22 passed** under the already pinned Rust/Cargo 1.98.1, unchanged lock,
401 checksum-verified registry archives, empty credential-free environment,
Cargo offline and socket-denied isolation allowing anonymous IPC. Six other
host tests and eleven binary tests were filtered, not executed. This was not
real SSH/model execution, a full `shenko start`, CI or merge readiness.
[Exact result and evidence hashes](https://github.com/cyjin-yl/shenko/pull/127#issuecomment-6546126668).
The earlier `09b304e` host/binary compile failures (four new tests not executed)
and its eighteen passing Node tests remain preserved separately, as do the
original eight selected SSH library tests at `0b27a109`. Neither older source's
results are relabelled as validation of this explicit-choice candidate.


## Historical observation bridge implementation — 09b304e, initially static only

The earlier #127 increment wired `node.capabilities.query/v1` into the actual
startup `NativeTaskHandlerPlugin` registry. Its prompt is discoverable only
while installed. This section preserves its initial source-only checkpoint;
the later fixed `5e8c463` execution evidence is recorded above. The fixed
`0b27a109` results below do not cover this installed handler.
No new dependency, lockfile, administrative RPC or dispatch ledger is added.

Why this slice: the retained-advertisement library was inaccessible to the
subject. The existing Node `Register` metadata contains only target ID/digest,
not a usable SSH binding or permission/resource grant. Inventing those grants
would be unsafe. A bounded read-only child is independently useful, remains
within #127 ownership, and leaves #114, shared child/store machinery and the
SSH authorization resolver untouched.

Minimal path / 最小路径:

```text
[delegate:node.capabilities.query:1] {"limit":8}
```

The real task result contains one retained-observation page. To continue, pass
its `snapshot.next_after` as `after`; omit `after` for the first page. Only these
two model fields are accepted. `limit` is 1–32, default 8. The existing native
registry supplies the trusted task/invocation; the handler requires an actual
originating step on the current subject branch and matches the installation's
Node identity. A model cannot supply an identity, grant, path or SSH command.

主体通过有限 child 查询本机与已登记同步节点的缓存能力声明；node-ID 顺序仅用于
分页，不表示推荐。`unknown` / `oversized` 不表示已就绪，也不证明不能使用。
`provides` 是未验证的外部描述，不是指令或权限。每页有实际 `observed_at`，
但不保证声明新鲜、节点可达或跨页一致；不隐式刷新、探测、选择或执行。

The new `node_choice_snapshot_for_subject` checks the frozen Node/subject
identity **inside the same SQLite snapshot** as its observations. A process-local
Mutex alone would not stop another administrative connection adopting a new
identity between the check and the read; static review found and fixed that
race. Observation output never includes SSH routes, paths or provenance text.

T1 freezes page, Node ID and subject identity using the existing operation/attempt
ledger. With T1 but no E1, read-only recovery may read that same page later from
the same identity; the timestamp describes the actual later observation, not
T1-time state. It never arms another attempt. Once E1 resolves the operation,
recovery returns that evidence unchanged. A changed identity or unreadable observation yields a finite, sanitized
Failed/EffectNone receipt rather than silently rebinding or looping forever;
CAS/evidence-write failures remain recoverable errors. Results over the existing normalized-result budget fail
explicitly, retain exact raw evidence, and request a smaller new task.

At that initial checkpoint, four focused tests were authored but **not executed**: strict page input and
projection; original-attempt crash recovery without duplicate evidence;
wrong-installation recovery failure; actual startup registration/withdrawal
and deterministic parser/S1 → handler/T1/E1 → T2/task_result delivery through
Cordis. The latter uses fixture output, not a model. Static diff checks do not establish compilation, task execution,
model behavior, real SSH, remote permissions, resource admission or full C17.
An isolated fixed-candidate run must separately cover the host handler, binary
registration test and existing node regression before calling this verified.
The model-facing prompt still needs real model sampling under its own scope.

The remaining execution bridge is deliberately not claimed: recording an
agent-selected node (addressed only as unexecuted source in the next candidate above),
installed authenticated SSH dispatch, authoritative
registered-target permission/resource resolution, and real remote/model
validation still require their own completion evidence. #127 stays Draft and
retains #114's dependency.

## Fixed 0b27a109 verified — 2026-10-31

The previously authorized source `0b27a109c819f365e4fb7422499deabaf997bc83`
now freshly compiles and passes **8 selected SSH tests and 18 node tests**,
both exit 0, in the dot online execution environment. Six other SSH tests were
filtered out; node binary/doc suites each had zero tests. Source, tree and the
original lock remained unchanged. These results do not cover newer main.

The three missing packages and their official index records were recovered;
all **401/401** locked registry archive checksums and the exact Git dependency
were verified. Restored Rust/Cargo/rustdoc 1.98.1 binary hashes match the prior
approved toolchain. Tests used empty HOME, separate Cargo/target/temp, one build
job, `--locked --offline`, and inherited network-socket denial.

An initial wrapper also denied anonymous `socketpair` IPC, preventing the first
compiler process from starting (both commands exit 101, zero tests). That failure
is retained. One corrected-wrapper retry permits only anonymous process-local IPC
while still denying every new socket/connect/bind/listen/accept and io_uring.
IPv4, IPv6 and Unix socket probes returned EPERM. No mount-namespace isolation
or real SSH/model execution is claimed.

[Exact source-bound evidence](../evidence/agent-selected-ssh-0b27a109-20261031.json)
records commands, test names, preparation, failures, toolchain and log hashes.
This evidence-only update changes no executable/dependency/test source.
The installed authenticated SSH handler, registered-target permission/resource
resolver and real remote validation are still absent; the newer read-only
observation bridge above remains unexecuted and model-unvalidated.
PR #127 remains Draft and retains its #114 dependency. Earlier failures below
are historical observations, not the latest fixed-candidate test status.

## Historical fixed 0b27a109 follow-up — 2026-10-30 11:28 UTC

Separate owner authorization now covers static download of the three exact
locked public packages/index records and the fixed repaired candidate's original
8 selected-SSH tests plus node regression. The single preparation attempt failed
resolving `static.crates.io` before downloading any bytes. This is now a network
availability blocker, **not missing user permission**; no other download route,
package version or dependency change was attempted.

Both approved `0b27a109` commands were actually attempted once with Rust/Cargo
1.98.1, original lock, empty HOME, independent Cargo/target/temp and inherited
socket denial. Each exited **101 at offline dependency resolution** (missing
`x11rb` index): **zero compilation and zero tests executed**. All 398 available
archives reverified; the same three of 401 locked packages remain missing.
The socket probe returned EPERM and source/tree/lock stayed unchanged. This
supersedes the repaired candidate's "not attempted" status, not its unverified
build/test status. Historical passing results are not extrapolated.

See [candidate-bound result and logs](../evidence/agent-selected-ssh-0b27a109-20261030.json).
No executable/dependency/test changes accompany this evidence checkpoint.
Installed handler/permission-resource resolver and real remote/model validation
remain absent. The earlier failures below are preserved.

## Historical 7c2c7a98 verification and repair — 2026-10-30

Owner-approved `7c2c7a98` execution was attempted under exact Rust/Cargo 1.98.1,
empty HOME, independent Cargo/target/temp, original lock, offline plus inherited
socket denial. Both the selected-SSH and node commands exited **101 before
compilation: zero tests ran**. The inherited main `bc33b654` host-executor
manifest declared the identical `blake3 = { workspace = true }` twice (lines
15/26); both source versions have the same manifest blob `2c0b7300`.

The source repair removes only the second identical declaration. It does not
change dependency versions/features, lock bytes or workspace members. All 24
tracked Cargo manifests now parse in a static TOML check. Static offline Cargo
metadata progresses to a missing cached `x11rb` index entry; no repaired-source
compilation or test has run. Of 401 locked registry archives, 398 existing
archives were checksum-verified; three newly inherited X11-related packages
remain absent. A prefetch was denied as outside the offline test scope and was
not retried; separate preparation authorization is required, and actual tests
remain offline. Full failed-command evidence and limitations are retained in
[the fixed-source record](../evidence/agent-selected-ssh-7c2c7a98-20261030.json).

## Historical integration checkpoint — 2026-10-30, initially source only

This continuation normally integrates main
`bc33b654bcb78012c671827e1968f652aa69056b` with the existing #127 head
`be4c9ad85b70bfe392b09539ec82ec3d1875574f`, preserving both histories and
the unmerged #114 dependency. Main now carries the #125 model-child/quota
work and adds `TaskInvocation.software_generation` plus native T1 generation
validation. The historical test results below do **not** validate this combined
source or its changed dependency closure.

Fresh selected SSH inputs use `ssh-task-node-selection.v2` and bind the actual
invocation's runtime generation. A matching retry must retain that generation;
another generation cannot reuse the selected API to query or dispatch under
the old invocation. A newly claimed recovery worker uses the separately
authorized existing operation query/reconcile path, not a forged invocation.

The frozen v1 reader is retained explicitly because already-armed operations
must remain recoverable. Its generation is unknown: it is never defaulted to
`seed` or inferred from the current runtime. Existing `query(operation_id)` can
read that v1 closure without rewriting its CAS bytes; `execute_selected` refuses
to treat it as a generation-bound fresh/repeated invocation. The query reader
also rejects a v2 schema paired with a generation-less v1 invocation.

Two focused test journeys are prepared: changed-generation rejection without
another observation/attempt, and synthetic v1 reader/serialization coverage plus
fresh-admission rejection before T1. V1-with-generation, v2-without-generation,
unknown legacy fields and wrongly typed generation are negative checks. The
v1 fixture is not an old-executable migration or a remote/query integration test.
Even the crate-private fresh-admission helper rejects legacy selected inputs;
production legacy recovery remains exclusively the existing query/reconcile
path, whose `query` mode supplies no action payload. Existing fixture generation
and the previously reported unused import are corrected. Tests/build have
**not run for this integration**; `git diff --check` only checks whitespace.
No installed authenticated handler, registered-target permission/resource
resolver, real model/remote execution or full distributed completion is claimed.

## Why and source

The owner's 2026-10-29 clarification: expose distributed capabilities to the
central thinking subject, or a scheduling agent it delegates to; do not replace
their choice with a mechanical capability router. This slice follows SPEC
§5.4, §11, §38, §40 and the existing #114 advertisements. It does not implement
an election, a sleeping subject, or an Infinity-style durable runtime.

**Status: fixed candidate `ca6dcd9a` built and passed 18 node tests, including
the five new selection journeys, on 2026-10-29 at 19:40 UTC.** No real model,
remote node or end-to-end distributed task has been validated. Based on #114 `47b26f98` plus
main `d1cf8d64`; it preserves both histories. No new dependency/schema migration.

## Narrow capability card

- Purpose: inspect retained node capability declarations, choose a particular
  node explicitly, retain that exact choice/reason across restart.
- API: `Node::node_choice_snapshot(after, limit)`,
  `record_node_selection(NodeSelection)`, `node_selection_receipt(decision_id)`.
- Effect: snapshot/recovery are local reads; recording writes one idempotent
  command receipt. No network, execution, lease or resource reservation.
- Audience: trusted adapter code. This is **not** a new administrative RPC.
  The newer candidate above also installs a separate explicit-choice recorder;
  its execution is not yet verified. SSH dispatch is **not installed as a subject/child tool**.
- Unknowns: advertisement age, liveness, model availability, resource capacity
  and task authorization are not inferred. `provides` strings are untrusted
  descriptions, not instructions or grants.
- `profile_digest` identifies the complete retained advertisement (including
  fields omitted from this projection). Treat it as an opaque observation ID;
  it cannot be recomputed from `CapabilityObservation` alone and is not a
  signature or proof of the remote node's current state.

## Minimal path / 最小使用路径

1. 已认证节点通道继续负责 #114 的 advertisement 接收；需要更新本机声明时，
   管理侧显式调用已有 `capability_profile()`。查询自身也只读已缓存事实，
   不暗中扫描、探测远端或刷新节点配置。
2. Adapter 读取 `node_choice_snapshot(None, 32)`，向主体或其委派调度 Agent
   投影候选。稳定 node-ID 顺序仅用于分页，**不是优先级或自动推荐**。
   未知/过大事实仍有候选条目，不机械删除节点。每页是同一 SQLite snapshot，
   多页不是冻结全集；记录时重新核对目标和观测。
3. Agent 明确提供 `node_id`、观测 `profile_digest` 和 `reason`。可信库调用方
   提供新的 `decision_id`；上方已安装 handler 候选将它绑定到可信 operation ID，
   不接受模型传入。Adapter 必须从可信调用上下文核对身份/委派关系，不能采信
   模型自报的身份或权限，再调用对应的受绑定记录 API。
4. 相同 decision ID 和完全相同请求返回原回执；改理由或节点必须用新 ID。
   `unavailable` 给出 `not_enrolled`、`observation_unknown`、
   `observation_oversized` 或 `observation_changed`；后者附当前观测。
   由 Agent 选择更新观察、保持目标、改计划或选择其他节点，库不自动替换。
5. 重启后通过 decision ID 读取历史 receipt；它不变成新的决定或隐式重试。
   每个 receipt 的 dispatch 始终是 `not_dispatched`。

English: read a bounded snapshot, let the authenticated subject/delegate choose
one node and explain why, seal the choice against the observed profile digest,
then recover the same durable receipt by decision ID. On changed or missing
facts, return an explained unavailable result to the agent, never substitute
another node. A successful `recorded` result means only that the observed choice
was recorded, **not** that the target is ready, authorized or dispatched.

## Storage and limits

Reads project only node ID, profile digest, generation, advertisement time and
declared capability names. SSH target, binary/data paths, role reasons and
transport provenance are not exposed. At most 32 candidates per page, at most
16 KiB of stored advertisement per candidate (including the local envelope);
SQL guards byte length before returning profile text. Oversized profiles are
reported, never truncated into an apparently complete capability set. Limits
bound payload/allocation, not SQL runtime or all storage costs.
Malformed stored JSON or a mismatched node identity fails the entire read
closed with an error; it is not silently converted into an unknown fact or a
partially successful snapshot.

Validation and receipt insert share an immediate transaction. Existing
`commands(id,seal,result)` supplies durable idempotency; the seal includes the
`node.selection.v1` domain and complete request. Reused IDs cannot overwrite
another decision/command. Successful and unavailable decisions are immutable.
The versioned receipt keeps its original reason, observation and timestamp.
Future lifecycle changes must retain readers for this schema, or explicitly
migrate with history preserved; an unknown schema fails rather than re-plans.

## Original library integration dependencies — see installed candidates above

- The candidates above address authenticated observation/choice handlers and
  discoverable capability registration; SSH dispatch remains separate. Coordinate with #112 self-knowledge and #125 model-child owners.
  Never give a model the Node `Initialize`, `Join`, `Create` or other admin RPC.
  These receipts do not contain an authenticated caller or durable task binding;
  those bindings must be supplied by that bridge, not guessed from the reason.
- Actual remote dispatch/admission bridge. It must bind the decision receipt to
  the real durable task/operation, preserve the chosen node/reason on recovery,
  and independently enforce authorization, current resources and identity.
  An advertisement digest is no lease, permission token or fencing epoch.
- Recheck changed conditions at dispatch; return their concrete reason to the
  agent instead of rerouting. Unknown external effects retain operation IDs,
  executor fencing, receipts and reconciliation; never blindly replay.
- Event-indexed waits, lost-wake protection, bounded activation and lifecycle
  compatibility of active work are separate durable-runtime work, not provided
  by storing a selection receipt. Model KV residency is also separate.

## Validation ledger

Five new journeys: bounded/non-ranked snapshot with unknowns and no routes;
explicit choice/restart/replay/conflicting ID with zero task creation; changed
observation/re-decision without substitution; absent/removed enrollment and
missing facts; oversized observation reporting. Initially source-only; after
the owner's fixed-version execution approval, the exact `ca6dcd9a` candidate
passed `cargo test --locked --offline -p shenko-node --message-format=json`:
18 passed, 0 failed, 0 ignored; binary and doc-test suites each had 0 tests.
Rust/Cargo 1.98.1, original lock unchanged, 398 downloaded registry archives
matched its checksums; node artifacts were freshly compiled. Empty HOME,
independent Cargo/target/temp directories, explicit credential-free test
environment, Cargo offline and inherited seccomp socket denial were used.
This is not a mount-namespace isolation claim. The failed bwrap system probe
and initial libgit DNS fetch failure are retained in the
[candidate-bound evidence](../evidence/agent-node-selection-ca6dcd9a-20261029.json).
Normal restart/replay is tested; crash/commit-fault injection is not.
No claim of CI success: repository snapshot has no GitHub workflow directory.
`git diff --check` is a whitespace check only, not Rust compilation or runtime
verification. A genuine subject-discovery/selection journey still requires
model validation after the authenticated bridge exists.

## Trusted SSH bridge increment (2026-10-29, locally tested; not installed)

The next source increment adds `SshTaskRunner::execute_selected` in
`shenko-exec-ssh/src/selected_task.rs`, reusing the real existing SSH runner:

```text
installed authenticated child adapter
  → trusted TaskInvocation + AuthorityRef + registered node/SSH binding
  → explicit recorded selection / current observation check
  → existing Store::arm_external_operation (T1)
  → existing SSH helper / sealed receipt / query-only recovery (E1)
```

This is not another preparation ledger. The complete selection receipt/reason,
invocation, authority, chosen node and operation ID are in the **same frozen
SSH input** whose digest T1 stores and the existing remote helper receipt seals.
The task revision, active invocation/epoch, persisted worker assignment,
delegation decision and reserved native operation ID are checked before fresh
admission. T1 still conditionally checks the actual current task. Node locks
are released before T1/network awaits; a changed advertisement is explained,
never used to choose a substitute. Observations and T1 are not a cross-database
transaction, so an advertisement is explicitly not a capacity/permission lease.

An already-armed matching operation reads its original frozen binding/selection
and queries the existing receipt; it does not refresh its choice from Node or
send another action. A changed action, target, decision or authority is a
structured refusal. This retry requires the exact original `TaskInvocation`;
a newly claimed recovery worker uses the existing authorized
`query(operation_id)`/reconcile API instead of pretending to be that original
invocation. Optional `FrozenInput.selection` uses serde default
and skip-None so old two-field frozen inputs keep their exact serialization.
No model-facing admin RPC or unguarded target registry was added.

The node-choice receipt's `not_dispatched` remains the historical result of
recording a choice. If this trusted API is later actually run, the **SSH operation
and attempt/effect receipts**, not that immutable choice record, describe dispatch.
Source presence is not evidence that a remote task has run.

### Exact ownership / installation dependency

Snapshot 2026-10-29 20:17 UTC: #114 `47b26f98` remains the capability/transport
owner; #112 `e4a2d186` owns installed self-knowledge discovery; #125 `421045dd`
owns the active model-child/subject/Store cutover. The latter's published changed
files include `shenko-store/src/external.rs`, `child.rs`, subject child/model-child
and shared types, **not** `shenko-exec-ssh/src/task_runner.rs`, `lib.rs` or its
Cargo manifest. This increment edits none of those active Store/subject/types
files. The #125 native authority construction and assignment memo names were
statically checked and still match this boundary; combined execution is untested.

The remaining shortest integration is an installed narrow child handler:
1. expose capability observations/explicit choice, not Node management RPC;
2. obtain invocation, role/authority and resource limits from its authenticated
   registry context, never from model JSON;
3. resolve only the explicitly chosen node through the existing authorized
   registered-target map; missing permission/resource capacity returns a reason;
4. call `execute_selected`; settle the task through the current scheduler and
   reconcile armed work using its original operation ID.

The installed SSH handler and actual registered-target authority/resource resolver
are still dependencies; the observation and choice handlers above do not supply them. The trusted API cannot authenticate arbitrary callers
or invent grants; it preserves existing SSH trust/permission boundaries and T1,
not a new general permission system. Model discovery and real remote-node
validation remain unperformed. No external owner/Agent was contacted for this
increment; dependencies are recorded in this PR rather than presumed accepted.

### Bridge validation — separate from the older 18 passing tests above

Six new tests cover actual local T1/E1 selection persistence and
query-only recovery; forged worker refusal; unrelated authority refusal;
registered-target mismatch; conflicting replay action; and byte-identical legacy
input serialization. Their SSH fixture deliberately references a missing public
host-trust CAS object, causing the existing runner to fail **before spawning
OpenSSH**. It tests unknown-effect bookkeeping, not remote success. Following
fixed-version approval, exact `d1c861f4` freshly compiled and passed all six
selected tests (six other SSH tests filtered out, not run), then all 18 node
regression tests. Both commands exited 0 with source/tree/lock unchanged under
the same Rust/Cargo 1.98.1, empty-home/offline/socket-denied conditions. One
unused test import warning was retained; it was not hidden by changing the
tested source. The [exact evidence](../evidence/agent-selected-ssh-d1c861f4-20261029.json)
contains test names, artifact/log hashes, diagnostics and scope boundaries.
This does not establish real remote execution or installed model discovery.
The lock
changes only by adding the already-existing local `shenko-node` package as a
`shenko-exec-ssh` dependency; no external dependency version is changed.


## Native installed child candidate — 2026-11-01, UNEXECUTED

The next increment actually consumes the existing bridge through
`NativeSshTaskHandler`, registered by `shenko start --ssh-targets /absolute/config.json`.
The CLI is explicit: no HOME, SSH config, key enumeration, agent discovery or
credential environment lookup. Without this option the SSH child is not
installed; startup explains the missing capability. There is no always-allow
policy shim, product command allowlist or per-command human approval gate.

```text
real native registry/subject task + recorded explicit node choice
  → installed target/node map + local connection slot
  → Store T1 freezes operation/attempt/action/selection/installation identity
  → exact local signer + CAS-pinned host/account over native russh
  → current generation/owner/ancestor check before fresh exec
  → helper receipt / E1 / bounded result + full-receipt CAS reference
  → existing scheduler T2 only for genuinely known effects
```

The JSON schema is `shenko.ssh.file-installation.v1`, with a `targets` list.
Each entry explicitly provides `node_id`, `target_id`, `revision`, `host`, `port`,
`account`, `private_key_file`, `public_key_openssh`, `known_hosts_file`, and
`receipt_root`. Locations must be absolute. Do not put private key contents,
passphrases or tokens in this file. Startup seals the selected public trust;
private material is read only after durable T1. The installed model projection
contains target-to-node IDs, not host/account/key locations. This is the
installer's explicit relationship to an already enrolled Node, **not** a claim
that SSH authentication cryptographically certifies the remote Node process.
No Node membership or another owner's target registration is overwritten.

The first concrete installer supports an explicitly selected unencrypted
OpenSSH/compatible private-key file and its expected public identity. On Unix
it opens without following a final symlink or blocking on a replaced FIFO,
checks a regular file and disallows group/other-readable key permissions.
Configuration and known-host files use the same bounded regular/no-follow/nonblocking
Unix open discipline, without imposing private-key mode rules on public material.
A private key mode of 0400 is valid; the rule is no group/other access, not exactly 0600.
Transient read buffers use the already locked `zeroize` package; this is not a
claim that every library/OS copy is erased. Windows ACL equivalence is untested.
Encrypted-file passphrases, managed agents, hardware signers and CA/hashed-host
trust remain uninstalled capabilities; `NativeSessionOpener` reuses the native
signer/session API for a real future adapter, not a fake success implementation.
RSA file authentication selects SHA-512 and never falls back to SHA-1; RSA-SHA256-
only servers need a later evidenced algorithm-selection improvement.

The new non-secret seal binds ledger, Node mapping, revision, complete endpoint/
account/trust binding and actual authenticated public key. Changing any of these
cannot redirect an armed operation. Recovery reads the original target from CAS,
requires the original installation seal and queries the same operation/attempt
with empty stdin. Old v1 records without this seal remain readable by their
historical explicit query reader; the new installed handler does not manufacture
an original signer identity for them. The optional absent seal retains the old
serialized shape. Historical receipts and observation codecs are not rewritten.

`permission_ref` in `ssh.target-references.v1` still means a provider-owned
object/version **reference**, not a credential, grant, resource lease or approval.
The older read-only `PolicyObserved` evidence is not promoted into authority.
This installed route consumes actual selected OS/account/signer capabilities,
the existing task authority/fencing and local resource capacity; it does not
require an invented positive verdict from the metadata-only inspection API.
The four local slots bound local connections, not remote effects. The existing
native bounds remain unchanged: at most 1 MiB encoded action JSON before T1,
2 MiB helper stdout, 1 MiB helper stderr; decoded upload/stdin size alone is not
the encoded-envelope size. Larger helper receipts hit the transport output cap
and remain unknown rather than being truncated into a successful receipt.
Aborting or
closing a connection does not prove a remote command stopped; a race after the
last fence and a lost receipt remain unknown. POSIX/Python3 helper availability,
remote account capabilities and target directory protection remain real runtime
requirements. The old host-ssh/tmux path is not a native fallback here.

Nine new static tests target concrete risks: (1) real parser/Store/Node/child T1
before authentication and successful receipt/T2; (2) lost reply plus Store reopen
queries the original operation without action replay; (3) cancellation while
awaiting an authenticated opener prevents fresh exec; (4) a different, genuinely
authenticated public identity cannot impersonate the installed signer;
(5) the real startup file installer diagnoses an owned missing key after T1
without credential discovery or host-ssh fallback; (6) a generated, owned
protected key file loads the exact identity and becomes unavailable if made
group-readable; (7) the shared public configuration/trust reader refuses an
owned FIFO without waiting for a writer; (8) a same-agent but different
branch/origin installation cannot reach T1 or its opener; (9) one detached task
snapshot/call-context pair is refused before T1 or the opener. The last two
are combined negative identities, not separate tests isolating each individual
field predicate. The protocol fixtures use 1 KiB memory streams and synthetic
helper receipts; they do not execute the remote Python helper or a real host.
**These nine and the modified source have not been compiled or run.** Prior
71-pass/84-selected evidence belongs to earlier exact versions, not this one.
No real model has sampled the new installed usage prompt; model-behavior
acceptance, actual SSH interoperability, platform ACLs, independently hosted CI
and long-run recovery remain open.


## 2026-11-01 native installed child validation checkpoint

The static candidate above was executed as tree `42f65b6c`: 78 passed / 2 failed.
Both owned negative fixtures bound the pending placeholder implementation rather
than the registered handler descriptor, so the correct production implementation
guard stopped them before the intended identity/context checks. That failed run
and its original source/ledger/logs remain preserved.

Only the owned fixture helper was corrected to use the real Store claim API with
the installed descriptor; four kind/version/plugin/artifact preconditions were
added. The existing precise rejection, no-T1, no-opener and no-exec assertions
were not weakened. All other 542 source leaves, product code and dependencies
remain byte-identical to the first run. Fixed recovery tree `cb59d5a4` then passed
**all 80 selected tests**, after nine exact name listings, with 0 failures/ignored.
The two direct-Store negative fixtures are not described as full registry dispatch;
the five positive installed-child journeys use the actual service registry.

The source/dependency/lock/toolchain pre/post guards matched (543 source leaves,
493 registry archives and 25,840 extracted files). Existing compilation cache was
reused; this is not a clean full rebuild. See the [version-bound result and retained
failures](../evidence/native-installed-ssh-child-verified-20261101.json).

This proves the selected owned in-memory child/receipt/recovery paths and bounded
file/FIFO fixture behavior, not real SSH hosts/remote helper, real model use,
encrypted private-key/agent/hardware installation, Windows ACL or WASM acceptance.
No command allowlist or fake permission grant was introduced. C07/#12 remains open;
PR127 remains Draft and independent hosted CI is still missing.
