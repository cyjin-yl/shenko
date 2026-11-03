# Research memory — functional contract

SPEC §14.2–§14.5 (research epochs / reset-to-best), §12.4 (PMA intervention),
§10.5 (context contracts). This module is the research slice: a subject's
research lineages, their reset-to-best epochs, the durable bank that crosses
epochs, the PMA memory-intervention worker, and closure-complete retention
mechanisms.

## Architecture

```
shenko-memory         MemoryInterventionPolicy (trait) + Model/Silent impls
                      + MemoryInterventionRegistry (Cordis service)
                      + MemoryInterventionPlugin / PmaContributionRuntime
shenko-store          research.rs: epochs/candidates/bank/reminders/tombstones
                      + archive_moves; every write idempotent + ledger commit
shenko-subject        PmaEvaluatePlugin / PmaEvaluateHandler ("pma.evaluate")
shenko-exec-host      research.reset / research.candidate / research.bank /
                      retention.apply handlers + builtin.rs assembly
```

The policy decides; the store remembers; the native children execute inside
the T1-armed attempt boundary and record sealed E1 evidence. Nothing here is a
model tool schema: the subject reaches these through the standard
`[delegate:<kind>:1]` native-delegation path.

## Epochs and reset-to-best (research.reset)

`POST [delegate:research.reset:1]` with `{"op":"open"|"reset", "request_id",
"lineage_id", "trigger", "reason", "constraints", "budget", "effects",
"environment", "best_candidate_id"}`.

The closure uses the Store's structured facts without an alternate handler
schema: constraints carry `id`, `statement`, and optional `source`; effects
carry `effect_id`, `description`, `external`, and `recorded_at_ms`. Budget
fields are `max_tokens`, `max_duration_secs`, `max_cost_units`, and `notes`
(each optional). The environment records `environment_id`, `facts`,
`toolchain`, `dataset_versions`, and `reproducible`; a software-generation
label alone is not a reproducible environment.

- **open** creates epoch 1 of a lineage with a sealed worker brief and the
  authoritative native invocation's task identity. Recovery uses the original
  operation's task, not a caller-supplied ownership field.
- **reset** freezes the active epoch as an archive and opens the next one:
  candidates are frozen; the best candidate is **validated** (artifact present
  in CAS, digest matches, score recorded with its source, environment
  recorded); surviving bank entries are carried; the new brief carries
  constraints/budget/effects forward **without the old failure narrative**
  (failed entries stay in the frozen epoch, excluded by count).
- The candidate pool includes both candidates created in the current epoch
  and candidates retained in its sealed brief and reset ancestry. This also
  recovers candidates omitted by older reset writers, without rewriting their
  historical briefs or reading mutable historical candidate rows. Only
  candidate evidence is recovered from ancestors: old bank entries and failure
  narratives are not restored. Each snapshot's digest, identity and parent
  binding must verify; the ancestry must remain in the same lineage and reach
  epoch 1 through consecutive epoch numbers. Missing ancestors, cycles and
  conflicting evidence refuse the reset rather than publish a partial pool.
  This verification reads the sealed ancestor snapshots on each new reset.
  Consecutive resets preserve candidate identities, artifact references, scores
  and their sources, and environment references; they do not create duplicate
  candidate rows. The inherited best is revalidated on every reset. An invalid implicit best is
  reported in the validation receipt, not presented as the new starting point;
  an explicitly requested invalid best still refuses the reset. A missing or
  inconsistent required brief is an error, not an empty inheritance set.
- Triggers: `stagnation | isomorphic_proposal | evidence_direction_changed |
  planned_round | subject_request`.
- Idempotent on `request_id` (epochs.reset_request_id UNIQUE). A replay
  returns the same epoch identity and the sealed candidate set, carried bank
  IDs and excluded-failure count. Later bank mutations do not rewrite that
  result. Old epochs are never deleted; the reset changes what a new worker
  sees, not what the ledger remembers.

## Candidates (research.candidate)

`{"candidate_id", "epoch_id", "artifact_ref", "score", "score_source",
"environment"}`. `artifact_ref` is content-addressed; `score_source` names
where the score came from (evaluation / baseline / human). Candidates are the
raw material of reset-to-best. Idempotent on `candidate_id`.

The handler seals the complete environment body as a CAS object and stores
its `environment_ref` on the candidate. Its creation timestamp belongs to
the frozen input, so reconciliation does not invent a later creation time.

Candidate replay requires the same epoch, artifact, score, verifier source,
environment reference and Store notes. Reusing the ID with different evidence
is a conflict, not a successful acknowledgement of the new values. An identical
write retains the first stored creation timestamp and does not advance the
ledger. It remains replayable after a reset freezes the candidate, without
thawing it or replacing the evidence.

## Durable bank (research.bank)

Ops: `put | list | supersede | tombstone`. Entries have `kind`
(`requirement|environment_fact|attempt|diagnosis|open_goal`),
`scope` (an exact free-form string), `confidence`, `source_refs`,
`supersedes/superseded_by`, `carried_from`, `tombstoned_at_ms`. A tombstone
keeps the content readable — it only stops injection. On reset, entries whose
kind does not survive reset and tombstoned entries are not carried.

`list` accepts `selection=all|carried|failures` and an optional exact scope
filter. `all` returns non-tombstoned entries; `carried` selects requirements,
environment facts and open goals that are not tombstoned; `failures` is an
explicit audit view of attempts and diagnoses, including tombstoned entries.
The Store's `bank_entry(id)` API keeps every kind readable by identity,
including tombstoned rows, without restoring eligibility for injection.
The reset's excluded-failure count includes retired failure entries retained
in the archive. Default PMA bank observations use the active `all` view.
Bank-entry creation time is frozen once, independently of reconciliation.

Bank replay requires the same epoch, kind, content, provenance references,
scope, confidence, base revision and reset ancestry (`carried_from`). A supplied
`supersedes` target must match the retained relation; an original write without
a target may replay after an explicit correction link. Replay neither changes
those links nor removes a tombstone, and retains the first creation timestamp.
Conflicting evidence or a different explicit correction target refuses before
allocating a new ledger commit. Adding a correction link or retiring an
entry uses the explicit lifecycle operations, not a repeated `put`.

## PMA intervention worker (pma.evaluate)

`PmaEvaluateHandler` (shenko-subject) evaluates the requested research epoch;
it does not drain the reminder queue. The queue reader
`store.pending_intervention_reminders(agent_id, now_ms, limit)` is separate
from both this evaluation and the library memory intents
(`store/src/memory.rs::pending_library_memory_intents`). The handler builds a
bounded observation of the epoch's bank + attempts, asks the installed
`MemoryInterventionPolicy`, and executes the decision:

- **silent**: recorded as a decision (a missed reminder is a fact, not an
  absence); nothing is injected.
- **remind**: one transient, source-bound reminder with provenance and
  relevance, delivered through the context contract. Accepted S0 usage is the
  authoritative contribution consumption receipt; a decision row or provider
  answer alone does not consume it.

No personal-interest relevance gate and no approval step: the policy is the
only authority, the decision is bounded, durable and reproducible.

Prepared PMA contributions use the common `ContributionPayload { v, text }`
reader contract. The sealed object also retains reminder identity, provenance,
relevance and bank references; these metadata fields are not injected as model
text. The producer revision is `pma.intervention/v2` and the reminder-body
codec is version 2; original sealed objects are never rewritten.

A transient reminder with a deadline uses `ThisRequestUntilMillis`: it expires
if still unused at that deadline and is consumed when an accepted S0 first
adopts it. Re-publication and replay of S0 usage do not revive it. Ordinary
`ExpiresAtMillis` remains reusable until its deadline. This contribution-level
contract does not by itself prove default-runtime producer installation or
provider delivery.

Reminder delivery is resolved from **committed epoch and task ownership**, not
from the address string alone. `subject` and `epoch:<id>` are addresses; the
`subject` address resolves through the epoch's owning task's agent to its
current subject branch. For `epoch:<id>`, the epoch's owning task is walked to
its nearest model child, or to the subject root. A model child owns the task scope
`{ scope_id: task_id, task_id: Some(task_id) }`; the subject root owns the
scope-wide branch scope `{ scope_id: branch_id, task_id: None }`.

An epoch id is **not** a collection scope. No request collects
`scope_wide(epoch_id)`, so a reminder published under the epoch id was
prepared for a scope nobody reads: the decision was durable and the reminder
was still unreachable. An epoch-directed reminder now reaches the worker that
owns the epoch, and a subject-directed reminder reaches the subject's own
branch scope. The scope string `subject` is not a global broadcast address,
and a child's collection does not also receive subject-directed reminders.
Preparation selects active epochs and retains the existing expiry
and consumption filters. Historical epochs without an owning task remain
readable, but cannot establish a delivery scope; the producer does not guess
an owner from whichever subject happens to run. A malformed ownership chain is
an explicit conflict, not an empty plan.

The immediate publisher refreshes the resolved delivery scope — the complete
committed batch for that destination, not a singleton that could overwrite
another pending reminder. The publication ticket is acquired before
preparation, and registration withdrawal invalidates all of its tickets. A
restart re-derives every pending scope from the durable decisions, so a
prepared batch lost with the process is rebuilt for the same owner.

The reminder body is **not** tool-result context. The normalized decision
result carries the decision identity and a sealed reference to the recorded
decision; the reminder text reaches a model only as a prepared contribution,
in the contribution scope its owner collects. A decision row, a provider
answer or a sealed decision object is evidence, not delivery.

### The decision and its settlement are one fact

The bounded call produces one decision, and that decision *is* the effect of
the operation:

- the provider's answer becomes durable E1 evidence on its own
  (`record_intervention_provider_evidence`). The operation stays unresolved
  while the policy still has to read the answer, so no path can settle the
  attempt as a success before a decision exists;
- the decision row and the settlement that accepts it commit in one ledger
  transaction (`record_intervention_settlement`): a successful intervention
  and its durable decision are one fact, and neither exists without the
  other. The settlement names the decision it carries, and the row keeps the
  first writer's clock when the same decision is replayed;
- an answer that carries no usable decision is a failed bounded call. It
  settles as a failure, keeps the original answer as evidence, and never
  becomes a fabricated silence;
- recovery re-derives the decision from the frozen observation and the sealed
  answer, so a call that stopped between the answer and its decision commits
  the same decision instead of being replayed. The observation is frozen with
  the request for exactly that reason; a sealed input of an older revision
  parks the attempt instead of inventing a decision.

## Retention (retention.apply)

`{"request_id", "mechanism", "target_refs", "reason", ...}`.

- `semantic_forget` / `erase`: a monotonic tombstone whose sealed closure
  lists every target digest and states that the content store has no object
  delete — bytes remain recoverable at their digest. Erase records erasure
  intent and its recovery path; it does not destroy data (R066 erasure stays a
  separate owner protocol).
- `archive_move`: records a complete object closure leaving for an
  authenticated target (`provider/namespace/locator`, `manifest_ref`), with
  `hash_verified` / `readback_verified` and an explicit local-release note.
- `status`: read-only report of the targets' tombstones and archive moves.

All mechanisms are idempotent on `request_id` (tombstone/archive identity).
No mechanism deletes user data in development.

## Runtime registration

The default CLI installs `exec_host::builtin::research_handlers(store)` on
its existing `NativeChildPlugin` through `NativeTaskHandlerPlugin`. Thus
`research.reset`, `research.candidate`, `research.bank` and `retention.apply`
are present both in live dispatch and in the `capabilities` command, including
model-free startup. There is no separate, unused built-in supervisor.

With a configured model, the CLI also installs `MemoryInterventionPlugin`
and `PmaEvaluatePlugin`. The latter uses the same broker as the subject and
model children, and declares dependencies on the native runtime, the policy
registry and the contribution runtime. Replacing those memory services
rebuilds the handler rather than retaining a withdrawn publisher. Policy
replacement remains `MemoryInterventionRegistry::install`; no keyword,
model-selection or human-approval gate is introduced.

The memory plugin restores the current subject branch's prepared batch before
startup completes. Afterward it reconciles ledger changes on a 250 ms background
tick; committed reminder decisions also request an immediate full-batch
refresh. Branch changes withdraw the previous branch's batch. Expiry is checked
at collection, and accepted S0 history reconstructs single-use consumption on
process/service replacement. Preparation is outside the request collector and
does not consume a reminder. The background projection only reads committed
decisions; it does not trigger an inference or choose what the subject does.

Cordis teardown closes publication, cancels and joins the projection worker,
then removes that exact registration. A retained service handle cannot publish
after closure; same-Fiber restart gets a fresh worker and registration. A
projection failure is journaled and stops its owning runtime rather than
silently serving a stale successful projection.

Initial native-plugin publication installs the catalog without recovering or
acquiring dispatcher authority. A live composition root explicitly calls
`native.recover()` after owning the dispatcher. Same-Fiber native restart still
recovers aborted claims after its close/join barrier. Capability discovery
therefore cannot recover or fence another live dispatcher's tasks.

## Failure and recovery semantics

Every handler: freeze input → seal → arm T1 (`arm_external_operation`) →
idempotent write → sealed E1 evidence (`record_external_evidence`) → resolved
operation. Crash before the write: reconcile re-runs the frozen write (safe
by idempotency). Unknown/conflict evidence is authoritative and is never
erased by a later success.

The memory-intervention worker settles in that order with one extra step:
answer → the policy reads the answer → decision row + settlement in one
transaction. An answer alone never resolves the operation, and a decision
without its settlement never becomes visible.
