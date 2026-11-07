# Subject runtime contract

The subject's own runtime: one finite step at a time, one durable request
identity per attempt, and no outcome that is guessed from memory. This
document states the interfaces other owners integrate against and the
recovery semantics they may rely on. It describes the intended and the
implemented contract; where a runtime proof is still missing, that is said
explicitly instead of being implied.

Owner: the subject slice (`shenko-subject` loop/context/dispatch/inbox/output,
`shenko-types` subject types, `shenko-store` step/head/request/state).

## The step

A step is the unit of the subject's life. `SubjectStep` moves through
`planned → preparing → awaiting_model → result_recorded → committed`, with
`retry_pending` and `retired` as the two exits that do not advance the head.

The continuous subject's `subject_root` task is not a finite native child.
Both the candidate page and its scan upper bound exclude that root; native
parking refuses it as well. Scanning for a missing child handler must never
park or mutate the subject's own root task.

| Phase | What commits | What may still fail |
| --- | --- | --- |
| S0 (`planned → preparing`) | the sealed input plan (schema 12) | the plan CAS may reject a moved source; the step stays `planned` and is re-projected |
| dispatch (`preparing → awaiting_model`) | the `AwaitingModel` transition, the quota send claim and the **armed request row** in ONE transaction | nothing may be sent before the arm is committed |
| result (`awaiting_model → result_recorded`) | the sealed model text and the sealed outcome | the result may be non-terminal; that is a retry fact, not a decision |
| S1 (`result_recorded → committed`) | the decision, the per-event dispositions, the intents, the head and the unique successor in ONE transaction | a rejected commit leaves the step resumable; it never creates a second successor |

## The sealed request (schema 12)

`FrozenStepInput` is the immutable projection of one step's input. It is
sealed into CAS and read back by the store, the retry path and the recovery
path, so all of them see the same facts.

| Field | Why it is sealed |
| --- | --- |
| `schema_version` | exactly `12`. A reader that sees another version refuses the step instead of guessing. |
| `step_id`, `step_no` | the plan belongs to exactly one step of one branch. |
| `events` | the original inbox events selected into this step, with their durable ids. A disposition may only name one of them. |
| `provider_binding`, `payload` | the exact wire request. The store re-checks the seal against the provider binding on every admission. |
| `context` | the projected window, its source commit and its budget provenance. |
| `semantic_wait` | the wait in force at S0; the store refuses a plan whose wait moved under it. |
| `identity`, `identity_provider_revision` | the persona snapshot this step reasoned about, and the identity policy revision that produced it. |
| `request_id` | the wire request identity of this step's FIRST attempt. Sealing it before any send is what makes a resumed step reuse the same identity instead of minting a fresh one for the same request. |
| `hook_graph_ref` | the hook graph this step dispatches under. |
| `obligations` | the commitments the subject is still carrying. |
| `attention` | the subject's own do-not-disturb decision in force. |

Readers of this schema: `shenko-store/src/store.rs`
(`validate_subject_context`, `commit_step_and_advance_head`),
`shenko-store/src/raw_session.rs`, `shenko-store/src/intents.rs`
(`validate_model_child_intent`), `shenko-store/src/semantic_wait.rs` and
`shenko-subject/src/loop_runner.rs`. They are migrated together; there is no
compatibility alias for version 11.

## Durable main-request receipts

`shenko-store/src/subject_request.rs` owns one table,
`subject_request_attempts`, keyed by `request_id`. Each row records the
attempt identity, the sealed plan, the payload and provider-binding digests,
the state and — once it exists — the terminal evidence.

The state ladder is monotone:

| State | Meaning |
| --- | --- |
| `armed` | committed before the send boundary. A crash here proves the request never dispatched. |
| `dispatched` | the send boundary passed. The request may be in flight; the outcome is unknown until terminal evidence lands. |
| `terminal` | terminal provider evidence is committed. The row is never rewritten. |

The broker's receipt sink (`shenko-subject/src/subject_receipt.rs`) owns the
send boundary: `may_dispatch` moves `armed → dispatched` exactly once and
reports `Unknown` when the request may already be in flight; `record` commits
the terminal evidence independently of whether the waiting step is still
alive. A late or lost caller never loses the result and never gains a reason
to replay.

### Recovery of an `awaiting_model` step

A resumed step consults its OWN receipts. It never re-sends blindly:

| Durable fact | What the step does |
| --- | --- |
| a terminal receipt whose result matches the sealed request AND carries provider-terminal text | the result is adopted, the step becomes `result_recorded`, no send happens |
| a terminal receipt without provider text (provider rejection, unavailable, invalid request, cancelled fence) | the same typed policy the live path applies decides: a 429 persists `ProviderRejected` as a positive retry fact, every other refusal is a typed protocol error. A refusal is never adopted as model output |
| a `dispatched` row without terminal evidence | the outcome is unknown: the step records `DispatchLost`, the spent send stays armed and unsettled, and the next attempt gets a NEW request identity |
| an `armed` row only | the send boundary never passed: the step persists `NeverDispatched` and arms a new attempt with a NEW request identity |

The original request identity is restored with the broker
(`register_recovered`) only when the durable row proves the request may have
dispatched (`dispatched`) or already carries terminal evidence. An `armed`
row is positive proof that no send happened, so it is never registered as
unknown debt: a request that never left the process cannot consume broker
capacity. A request that may still be in flight keeps its recoverable debt
and a request whose receipt already landed is reconciled against that
receipt.

Live delivery and restart share one admission policy for a provider result
(`admit_model_result`). A recovered receipt is interpreted exactly like the
live one: only provider-terminal text becomes model output, a 429 persists the
same retry fact, and every other refusal raises the same typed error. The
store's legal transition set is unchanged: `awaiting_model → awaiting_model`
is not legal, so a recovered step always reaches a new attempt through
`retry_pending → awaiting_model` with a new request identity.

A retry is always a NEW attempt with a NEW request identity. The spent
identity is never re-used: the store only allows the request identity to
change on a `retry_pending → awaiting_model` transition whose sealed retry
evidence binds to the previous request.

## Off-S0 preparation

The slow context work does not run inside an S0 seal. `shenko-context-policy`
owns the API; the subject calls it in this order:

1. `observe_pressure(caps, used)` — cheap, no history read.
2. `needs_preparation(caps, used)` — cheap; the installed strategy decides.
3. `adopt_prepared(&history)` — verifies the candidate against the live scope,
   source window, model generation, policy epoch, leaf and capacity. A
   candidate that fails any check is dropped, never committed.
4. When there is nothing to adopt, the subject launches the preparer on the
   blocking pool (`prepare_and_publish`) and keeps the current window. The
   step's context selection records `Preparing` — it never claims a
   compaction that did not happen.

## Hooks

The hook graph a step dispatches under is sealed with the request. A missing
hook runtime is a typed refusal (`HOOK_RUNTIME_UNAVAILABLE`), never a sealed
empty graph: a zero digest would claim that hooks ran.

The dispatch itself uses one pinned graph for the whole invocation
(`snapshot()`), and the sealed graph digest is compared with the pinned
graph's digest before the send. A swap between S0 and the send is a typed
conflict.

## Prepared contributions

Prepared material (a reminder, an awareness perspective, a plugin notice)
travels through the contribution registry, not through the prompt by
accident:

1. The subject collects with a **scope-wide** scope (the branch scope). A
   task-bound contribution is therefore reported as a `scope_mismatch`
   omission rather than silently included.
2. A missing **required** contribution is a typed conflict that names the
   keys that could not be included. An ordinary omission (conflict, expired,
   scope mismatch, over budget) is data: it is sealed with the request and
   does not fail the step.
3. The model-facing text of a contribution is the text of the sealed
   document its CAS reference points at, read through the registry's own
   document reader (a newer document version is an error, not a guess). The
   provenance — producer id, version, artifact digest — is **not** put in
   front of the model; it reaches the raw call record through the seal.
4. Collection happens **before** the final budget check. The appended body is
   re-estimated with the same heuristic and charged together with the context
   and tool cost (SPEC §37.7); the rendered estimate and the declared
   contribution cost are two different facts and both are recorded. The limit
   is compared against the **complete** payload — the selected window's
   estimate plus the appended body — never against the prefix that existed
   before the body was appended. A contribution that understates its own cost
   therefore cannot be admitted into a payload that exceeds the observed
   context limit. The selected window's image estimate is preserved instead
   of being recomputed without it, and an unobserved capacity stays unknown.
5. Only after Store accepts S0 is usage projected from that exact frozen
   input into the registry. A CAS object alone, a rejected source snapshot
   or a failed S0 transaction does not consume material. The durable step is
   adopted before updating the in-memory view, so a projection failure cannot
   make an accepted preparation look like a new `planned` step.
6. Startup and contribution-service replacement reconstruct usage from all
   sealed steps in the branch, including retired attempts and older committed
   steps. The registry instance is then pinned for the step. A producer that
   republishes before or after recovery cannot resurrect a consumed identity.
   No new archive field or consumption table is needed: schema 12 already
   contains the full contribution identities, omissions, tokens and request id.

Publication tickets bind prepared material to a registration, scope and
preparation generation. Disposing a producer withdraws its prepared batches;
late completions cannot publish into a replacement. An accepted S0 remains
immutable after disposal. Consumption means reservation by the sealed request,
not proof that a provider received it; delivery still requires its own receipt.

## Authored input controls

The subject decides, in its own words, what to do with an input and what it
owes. The controls are parsed at the model boundary and re-validated by the
store at S1.

| Control | Durable effect |
| --- | --- |
| `[dispose]` | the per-event disposition, its reason, its priority and its review time. Every selected event is disposed explicitly; an event the model did not mention is acknowledged, never silently dropped. |
| `[obligation]` | a durable commitment, read back into every later S0 until a later committed step completes or cancels it. It is not a scheduler: nothing wakes the subject by itself. |
| `[dialogue]` | an authored turn to a durable task. |
| `[dnd]` | the subject's own attention decision. `off` requires the silenced sources and a review time; an unbounded silent switch is refused at S1 admission. |
| `[steer]` | the adoption of one live steer that was carried into this step. The submission key must name a steer that was actually selected into this S0; the store re-checks that binding at S1. |

## R0 rejection and correction

A syntactically valid disposition or obligation origin that names an event outside
its own sealed S0 is rejected BEFORE the step can be committed, at the same parsed-result boundary
that validates the rest of the control protocol. This is not an S1 conflict
discovered late and it is not a reason to kill the subject.

* A `[dispose]` may name only an event that was actually selected into this
  step's sealed `events`. A disposition for an unknown or unselected event is
  `CONTROL_PROTOCOL_INVALID` (`scope: request`, `retry: new_revision`); the
  store never sees it as an S1 commit attempt, so nothing about the step's
  other intents is partially applied.
* An `[obligation]` with `origin_event_id` follows the same selected-event
  check and correction path. A rejected origin does not create a commitment
  or stop the subject loop; the Store independently preserves its S1
  admission check.
* Rejection retires the step: the original sealed input, the exact model
  text, the sealed outcome and a `StepRejection` record are all durable. The
  rejection names its replacement step; the next attempt gets a NEW request
  identity. A correction re-seals the same selected events, so a crash
  between retire and reopen still reconstructs the correction from the
  durable facts alone.
* The model's own text is preserved verbatim — including any `[remember]` or
  other controls it wrote alongside the invalid disposition or obligation. They were never
  committed; they are not adopted into the correction either. The subject
  must re-author them in the new attempt.

Bounded native proof (2026-11-04): the real default CLI against a scripted
provider received `[remember] …` plus a `[dispose]` naming a random event id,
retired the step with `CONTROL_PROTOCOL_INVALID`, preserved the verbatim
model output in CAS, sealed the rejection record with its replacement, sent
a second request under a new request identity, committed the correction
exactly once and wrote NO memory intent. Receipt:
`build/spec-architecture-20261101/subject-controls-native-r2/receipt.json`;
build and source-integrity binding
`subject-controls-verification-r6/receipt.json`. This proves the correction
mechanism with a real store/broker/CLI; it is not live-model or physical
power-loss evidence.

The 2026-11-05 obligation-origin regression failed before the correction with
the same late Store conflict seen in the retained Coder390 acquisition. After
the repair, all 64 subject library cases passed, including a close/reopen of
the durable correction. The actual CLI smoke then made two scripted provider
requests with distinct identities, retained the rejected output, committed
the correction, wrote no partial memory intent and exited normally. Receipts:
`build/ecosystem-compat-20261104/compat-obligation-origin-after-20261105-r1/receipt.json`
and `compat-obligation-origin-cli-20261105-r1/receipt.json`. The CLI binary's
SHA-256 was `0bcf38de159f9855f42a4cb78a94219210b2053c56f9c0ec7320be74813f2b9a`.
This is native correction and controlled reopen evidence; it does not score
the original failed model acquisition or establish physical-crash recovery.

## Identity

* An identity edit that does not fit the profile bounds is REFUSED with a
  reason. It is never silently truncated: a truncated profile would make the
  subject believe it wrote something it did not write.
* An operator edit commits through the control ledger like every other
  durable fact. The operator is not a writer outside the ledger.

## What is not proven yet

This document describes the implemented source contract. One bounded native
proof ran for the R0 rejection/correction path above (scripted provider,
no physical crash). The broader native recipe — a real store, a real
broker, a real provider turn, a crash between the arm and the send, and a
resumed step — remains unexecuted in this phase; recovery paths beyond the
bounded proof are source-level contracts, not observed runtime behaviour.
