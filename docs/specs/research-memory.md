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
shenko-store          research.rs: epochs/candidates/bank/reminders/tombstones
                      + archive_moves; every write idempotent + ledger commit
shenko-subject        PmaEvaluateHandler (native kind "pma.evaluate")
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

- **open** creates epoch 1 of a lineage with a sealed worker brief.
- **reset** freezes the active epoch as an archive and opens the next one:
  candidates are frozen; the best candidate is **validated** (artifact present
  in CAS, digest matches, score recorded with its source, environment
  recorded); surviving bank entries are carried; the new brief carries
  constraints/budget/effects forward **without the old failure narrative**
  (failed entries stay in the frozen epoch, excluded by count).
- Triggers: `stagnation | isomorphic_proposal | evidence_direction_changed |
  planned_round | subject_request`.
- Idempotent on `request_id` (epochs.reset_request_id UNIQUE). A replay
  returns the same epoch. Old epochs are never deleted; the reset changes what
  a new worker sees, not what the ledger remembers.

## Candidates (research.candidate)

`{"candidate_id", "epoch_id", "artifact_ref", "score", "score_source",
"environment"}`. `artifact_ref` is content-addressed; `score_source` names
where the score came from (evaluation / baseline / human). Candidates are the
raw material of reset-to-best. Idempotent on `candidate_id`.

## Durable bank (research.bank)

Ops: `put | list | supersede | tombstone`. Entries have `kind`
(`durable_fact|tool_knowledge|interface_contract|pitfall|preference`),
`scope` (`interface|external|internal`), `confidence`, `source_refs`,
`supersedes/superseded_by`, `carried_from`, `tombstoned_at_ms`. A tombstone
keeps the content readable — it only stops injection. On reset, entries whose
kind does not survive reset and tombstoned entries are not carried.

## PMA intervention worker (pma.evaluate)

`PmaEvaluateHandler` (shenko-subject) drains
`store.pending_intervention_reminders(agent_id, now_ms, limit)` — the
agent-scoped reminder queue, a different durable set from the library memory
intents (`store/src/memory.rs::pending_library_memory_intents`). It builds a
bounded observation of the epoch's bank + attempts, asks the installed
`MemoryInterventionPolicy`, and executes the decision:

- **silent**: recorded as a decision (a missed reminder is a fact, not an
  absence); nothing is injected.
- **remind**: one transient, source-bound reminder with provenance and
  relevance, delivered through the context contract; the ledger records the
  usage receipt (`record_intervention_use`).

No personal-interest relevance gate and no approval step: the policy is the
only authority, the decision is bounded, durable and reproducible.

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

`exec_host::builtin::{spawn_builtin_runtime, builtin_handlers}` installs the
full set — research.reset, research.candidate, research.bank,
retention.apply, pma.evaluate — onto a real Cordis supervisor through
`NativeChildPlugin` + `NativeTaskHandlerPlugin`, the same path every native
child uses. A composition root calls `spawn_builtin_runtime(store,
model_child_handle, generation)`; recovery (`native.recover()`) re-drives
Armed attempts through the idempotent writes.

## Failure and recovery semantics

Every handler: freeze input → seal → arm T1 (`arm_external_operation`) →
idempotent write → sealed E1 evidence (`record_external_evidence`) → resolved
operation. Crash before the write: reconcile re-runs the frozen write (safe
by idempotency). Unknown/conflict evidence is authoritative and is never
erased by a later success.
