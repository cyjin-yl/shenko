# Software-generation handoff

This contract covers SPEC §9, §10, §35.8, §41.12 and §44. Software selection,
local process ownership, node election authority, and external effects are
separate facts. A candidate build or source implementation is not runtime
qualification. No builds, smoke launches, migrations, activations, model calls
or rollback proofs were executed during this source phase.

## Interfaces and authority

`shenko-update` owns `BuildProvider`, immutable candidate descriptions,
`UpdateService`, `ActivationRecord`/`ActivationSelector` integration, and the
`update.control` native handler. The binary owns installation control transport
and runtime assembly. `update status`, `prepare`, `activate`, `rollback` and
`recover` need no model. A model child uses the same native Cordis registration,
not a mandatory self-development pipeline and not a human approval queue.

`PrimaryClaimSource` is supplied by the composition root. Its node adapter uses
`Node::committed_primary_claim`, `fencing_token`, `check_fencing`, `is_primary`,
and actual branch facts. A missing source is not authority. An explicit local
installation without configured membership/peers, or the exact local
availability branch matching subject/head/node identity, can update its own
local slot without claiming global quorum. A nonlocal committed primary is not
silently converted into an unfenced single-node primary. The scoped authority
snapshot is retained in the recovery closure and re-read before selection.
Node election epoch, selector epoch and subject controller epoch are distinct.

After genesis, `Store::installation_id_for_subject` obtains stable installation
identity from the verified immutable `genesis.bootstrap_event_ref` or an explicit
provisioning record, not the current ledger writer ID. Fresh CLI genesis already
seals that identity in the event; a separate `bootstrap_records` projection is
not required for restart. When both representations exist, their installation
and subject bindings must agree; missing or conflicting identity is an error,
not a new installation. A staged copy acquires a distinct current ledger writer ID before its first
local mutation; historical `commit_log.ledger_id` values and embedded source
CommitRefs are not rewritten. The installation, owner, subject and genesis do
not become new identities.

## Build, copies and typed migration

`CargoBuildProvider` builds a real candidate; prebuilt input is content-checked.
The current executable is also verified against its committed route and retained
as an executable recovery artifact. The candidate gets a separate SQLite backup
and copied state. Installation-level `versions`, shared CAS, node database and
control endpoint are excluded from recursive root copying. Slot names include a
digest of the generation; path components cannot escape into the parent tree.

Migration uses Store's declared cognitive recovery archive and its typed
mandatory-fact/closure/conflict rules. It does not copy arbitrary SQLite rows,
ignore unknown columns, overwrite conflicting task/effect units, or splice
independently allocated commit sequences. Retained archive files provide exact
source payloads. Blocking gaps/conflicts refuse selection. A narrow
same-installation `copy_paused_activation_boundary` validates and installs the
local clean pause request/head/successor before typed import; peer archive import
cannot manufacture this authority. The same handover's own snapshot is the
only export admitted while that boundary is paused
(`local_boundary_preserved`): a peer receiver has no boundary copy to anchor
a paused head to, so peer exports still refuse.

A future incompatible format requires its actual versioned reader/migrator;
this code's same-reader archive path must not be advertised as a universal
breaking-schema converter. Required decoder closure and compatibility evidence
remain obligations of the supplied candidate.

`launch_smoke` actually executes the selected candidate artifact against its
copy. A missing/malformed manifest, failed command or failed report is an error.
The current bounded smoke command opens and inspects Store, subject/head/step,
selector and managed state. Those checks alone are **not** the complete SPEC
message/tool/plugin/global-concurrency qualification; integrated runtime smoke
must supply that evidence rather than treating process startup as full success.

## Live control and the final writer boundary

The binary's `update_control` module binds a loopback ephemeral listener and
publishes an installation-private capability descriptor. Requests are bounded
JSON and authenticated with that local capability. The CLI first attempts the
existing exclusive `RuntimeOwner` directory lock. A failed RPC does not prove
inactivity and never authorizes an empty registry. A proven stopped installation
can legitimately have zero live participants.

A live activate/rollback request:

1. Freezes an immutable helper request in CAS and a private durable file,
   naming installation, subject, original selector/generation/epoch/route,
   source slot, command and actual helper executable digest.
2. Requests the subject's real finite boundary. Registered native-worker,
   REPL interpreter, Playground-service and inference-domain handles quiesce;
   their actual receipts and unresolved debt enter the activation record.
3. Spawns a **finite** helper without inheriting the old owner's CLOEXEC lock,
   acknowledges only `awaiting_old_process_retirement`, then stops the old
   runtime. This acknowledgment is not successful activation.
4. The helper waits for and verifies exclusive `RuntimeOwner`. Only this
   process-lifetime boundary proves old local channel/UI/node/Store writers
   cannot append beyond the final watermark. Cancellation alone is not proof.
5. After validating the frozen source identity again, the helper performs final
   typed catchup and the selector CAS. Candidate selector, activated record,
   new-generation clean successor and new controller epoch commit together.
   Old invocations/effect debt retain their original identities.
6. The retiring copy mirrors the committed selection and fences its old head;
   the launcher pointer follows. A versioned, durably replaced pointer can name
   either the installation's original root or one declared version slot.
7. The helper verifies the selected executable and `exec`s it while transferring
   the **same locked directory file description**. The successor adopts and
   verifies it before opening Store and restores CLOEXEC for later subprocesses.
   There is no release-and-spawn ownership gap or second infinite primary.

Native `update.control` returns its armed operation before making this request
through a detached controller, so the native caller can finish and cannot wait
for its own boundary. Accepted/lost responses stay nonterminal. Reconciliation
never reruns an effect: it requires the exact original activation/rollback,
expected selector revision and a real persisted successor launch receipt before
reporting success. Different observations have distinct content-bound E1 keys.

## Resource receipts

The registry is asynchronous. A partial quiesce failure resumes all attempted
participants in reverse order, including the failing participant; resume failures
are reported rather than hidden. Actual native workers are gracefully joined;
a timeout retains their handles instead of dropping a `spawn_blocking` guest's
outer future. REPL journals survive interpreter stop. Playground captures process
identity before termination, closes admission, bounds lock acquisition and waits
for its owned Linux process group before releasing its runtime claim. Missing
resume descriptors or CAS payloads fail explicitly.

Broker quiesce closes submission under the actual accounting lock, waits for
queued/running local provider readers and durable receipt writes, and snapshots
request bindings/payloads, resource-domain identities, KV, external reservations
and quarantined charges. It never equates HTTP cancellation or `active_count`
with remote termination. Unknown requests remain debt. The process helper's
exclusive-owner barrier is still required before final catchup.

The controller's own late E1 observation routes to the selected Store while
retaining its original producer/attempt identity. If old process teardown wins
that race, the successor reconciles the original durable T1 from selection and
launch facts; an RPC disconnect is never fabricated into terminal success.

## Failure, rollback and recovery

`finish_committed_handoff` reads the candidate's actual selector and atomic
activated record. It can finish an interrupted mirror/fence/pointer publication;
it does not decide who won from the last exception or a PID. Before a selection
commit, compensation may resume the same old controller. After a commit, old
admissions remain closed and recovery completes the selected route.

Rollback uses the retained previous slot/executable and is a new activation with
an increasing selector revision/epoch. Current facts pass through typed recovery
into the previous copy; conflicts and unreadable formats are retained/refused,
not treated as successful import. The previous executable is smoke-launched
again against that copy. The rollback record names its original activation and
captures the **current** scoped authority, not an obsolete earlier incarnation.
A helper error re-reads committed selection before restoring the previous
closure. New facts, revocations, tombstones and external debt cannot disappear
because a workspace or software pointer moves backwards.
Cognitive and home rollback follows SPEC §51.4: append a new node whose
content matches a historical node, keep the log, and keep `agent_id`.
This section describes the current software-generation activation. It is
not the definition of that rollback, and it does not allow deleting the
commit log.

Launch receipts are durable `activation_launch_receipts` rows bound to launch ID,
subject, selector revision/epoch/generation/origin and actual slot. Retrying the
same launch identity with different binding fails. A selected-but-not-yet-started
helper result explicitly says `launch_succeeded: false`.

## Integrated qualification still required

Parent integration must bind managed home/memory/REPL/Playground paths to the
selected state copy using their owners' actual rebind contracts; an installation
root mount cannot masquerade as migrated candidate state. It must include the
new launch facts in Ledger's declared schema/closure membership, reconcile the
stable installation callers, and qualify incompatible-schema migrator/reader
closures. These are not proven by the current source or local SQLite equality.

The serialized native receipt recipe is: run real A; prepare/build B; observe
actual candidate smoke; hold an in-flight tool and unknown inference debt; request
activate through CLI and separately through the real native route; verify old
owner retirement, exact CAS, old-writer fence, transferred lock and B's durable
launch receipt; commit new messages/task/effect/revocation facts on B; rollback
through the real retained reader; verify A's new revision/epoch/launch receipt and
preserved B facts/debt. Interrupt before/after candidate CAS, mirror, fence,
pointer publication and successor exec; reopen and reconcile each state without
blind replay. Native/model/protocol/platform observations and power-loss claims
require their own real receipts. None was executed in this source phase.
