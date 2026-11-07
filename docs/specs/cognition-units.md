# Cognition units — functional contract (SPEC §18, §38, §45; C10)

Perception, awareness, Skill and code are **one kind of installable unit**.
They share a single installation authority — the Node's unit store
(`shenko-node/src/units.rs`) — and one read model — `CognitionCatalog`
(`crates/shenko-awareness-core/src/cognition.rs`). There is no second awareness
registry; the catalog owns no installation state of its own and every write
goes back through the Node, so version, enable state, supersession,
provenance and dependencies have exactly one owner.

Pure text units install no daemon, register no Cordis service and add no
kernel hook. A unit that ships an executable helper keeps using the existing
plugin contracts. The awareness bytes in this store are still text. That is
the current harness, not the capability (SPEC §51). The text grants no
authority and dispatches nothing.

Awareness bodies express observations and principles; they do not contain
prescribed passing replies or the judge's expected actions. Positive `cues`
describe applicability. `negative_cues` describe when the unit usually does
not apply, so an unverified capability claim belongs among positive cues.

## What a unit is

The authored form is a minimal package: one Markdown entry with a small
frontmatter (`id`, `version`, `kind`, `title`, `capability_card`, optional
`why`/`cues`/`negative_cues`/`applicability`/`knowledge`/`provenance`/
`learning_*`/`supersedes`/`enabled`) and a body. The body is the material the
model reads; the frontmatter carries identity, card and provenance. The whole
entry (frontmatter included) is the stored artifact, so installed content
stays self-describing. Bounds: entry ≤ 64 KiB, card ≤ 512 bytes, disclosed
index ≤ 4 KiB, one read body ≤ 16 KiB, one discovery page ≤ 32 cards.

Kinds: `awareness`, `perception`, `skill`, `code` — one storage and install
path, distinguished only by the kind field.

2026-11-05 conceptual contract ([SPEC §51](../../SPEC.md#51-认知元概念内容态度与理由)):
awareness is a capability, not a document the model can be assumed to follow.
Text units and an external watcher are the present harness. fact and conclusion
are not extra kinds; they are one content type whose justification stops at an
observation or at other content. This file still describes the installed-unit
mechanism. It does not implement attitudes, justification pointers, or metapackages.

## Disclosure: bounded cards, paged discovery, bodies read on demand

`CognitionCatalog::disclosure()` returns the index text that travels with the
prompt. It is **strictly bounded by `MAX_INDEX_BYTES` (4 KiB)**, including its
own trailing metadata line: whole cards that fit, then one line naming the
total number of installed units, the installation revision, and the exact
call that continues where the text stopped. There is no overflow list and no
unbounded tail — an earlier shape appended every card that did not fit and
claimed a bound it did not have.

Every unit stays discoverable because the index is paged, not truncated:

- `CognitionCatalog::discovery(start, count)` cuts one page and returns
  `CognitionDiscovery { catalog_revision, total, start, count, next_start,
  cards }`.
- `CognitionCatalog::catalog_revision()` is a digest of the installation
  facts themselves, so two processes that see the same installation agree on
  it and any change to the installation changes it. A page carries the
  revision it was cut from; `cognition.unit.list:1` refuses a page asked for
  against an older revision, so pages of two installations are never mixed.
- The disclosure's next-page hint names the **index of the first card that did
  not fit**, not a page number. A disclosure that fits less than a full page
  therefore never skips a card.

Only short cards of units in state `enabled`, `candidate` or `adopted` are
offered; the card names the unit, its revision, when it tends to apply, and
the exact call that reads its body. The body of a unit is **never injected
whole**: the model reads it on demand at an exact version through
`cognition.unit.read:1`.

No lexical gate decides relevance. A symptom names a symptom, not the concept
that would explain it, so a keyword filter hides exactly the perspective that
was meant to help. The model receives the offered cards and decides what, if
anything, it means here.

## Native children

Every child follows the pinned finite-child contract: frozen input sealed in
CAS, one armed attempt, one observation, one evidence receipt; reconcile
re-observes state, it never replays.

- `cognition.unit.read:1` — input `{unit_id, revision}`; returns that exact
  version's body, provenance, learning record, adoption record, supersession
  and dependencies, or an explicit read failure. Read-only.
- `cognition.unit.list:1` — input `{start, count, catalog_revision?}`; returns
  one bounded page of cards with the total, the next index and the revision
  the page was cut from. Read-only.
- `cognition.unit.evidence:1` — input `{unit_id}`; returns the observations
  this installation registered for the unit and the trial records the Node
  holds for it. Read-only. This is where a judgement finds real facts to
  cite.
- `cognition.unit.write:1` — input carries the authored unit plus optional
  `derived_from` (the real observation it came from: kind + reference +
  optional digest) and optional `judgement` (`adopt|reject|retire`, `method`,
  `evidence`, notes, enable). Installs the version, records the judgement and
  returns the fresh unit facts. Effect status `Complete`; reconcile re-reads
  installation state, it does not write a second version.
- `cognition.unit.trial:1` — the measurement path, described below. It runs
  metered inference and no external effect.

## Learning: bound to what really happened

A learned unit keeps its source as a `LearningRecord`: the kind and reference
of the actual observation plus `source_verified` and `verified_by`:

| `derived_from.kind` | what is checked | `verified_by` |
| --- | --- | --- |
| `cas_object` | the digest exists in this installation's content store | `store_cas` |
| `observation` | the digest is an observation this installation registered for the unit | `node_observation` |
| anything else | nothing — the reference stays a claim | `claimed` |

An unconfirmed reference keeps `source_verified: false` and the unit installs
as a **candidate**: visible in the index, not yet settled. A self-written unit
without a measurement stays tentative; being enabled is not being measured.

## Judgement: what the installation can verify

`UnitRequest::RecordAdoption` takes a `method` and structured `evidence`, and
the Node verifies every item against its own durable state:

- `AdoptionMethod::SandboxComparison` — a finite comparison of the candidate
  against a candidate-absent control that excludes every version of its unit id.
- `AdoptionMethod::LocalTrial` — a local execution trial of the artifact.
- `AdoptionMethod::OperatorStatement` — what the owner or an operator said.
  It records intent; it is never a measurement.

Evidence items are `TrialAttempt { attempt_id }` or
`StoredObservation { digest }`:

- A **trial attempt** is runner-owned. The Node opened the attempt before the
  run, bound it to the exact unit version, artifact digest, scenarios and
  rubrics, candidate and control configuration and model binding, and closed
  it with the receipts the run produced. Only such an attempt can carry
  `empirical: true`.
- A **stored observation** is bytes this Node holds and can re-hash, bound to
  that exact unit version. It proves the bytes exist; it never proves that a
  measurement produced them, so it never carries `empirical: true`. A
  caller-written report registered as an observation stays an observation.

Enabling is an installation fact and needs no evidence: a rejected or retired
decision never enables a unit, and an imported unit stays enabled without any
judgement. *Empirical acceptance* is a separate fact and only a verified
trial attempt produces it. The recorded adoption therefore distinguishes
`decision`, `method`, `evidence`, `evidence_verified` and `empirical`, and
the unit's state follows: `adopted` only when the judgement is adopted, the
evidence verified and the evidence empirical; `enabled` otherwise.

A recorded judgement and the observations bound to it are **local facts of
that installation**: they are stripped from every package on export and on
import, so a peer never inherits an acceptance it did not measure.

## Measuring a unit: the finite sandbox comparison

`cognition.unit.trial:1` compares the candidate with a candidate-absent control
and measures what the model actually did. Disabling alone is insufficient: a
learned candidate can remain discoverable and its exact body readable.

Preparation freezes the full typed Node export and the source configuration in
CAS. It preserves exact entry bytes, descriptors, learning and provenance
records, and dependency pins, without reparsing, truncating or normalizing a
BOM. Candidate import enables the exported root. Control removes **all
versions** of the candidate unit id, preserves every other package and pin,
and imports with the root disabled; nonroot imports are disabled in both
conditions. A pin to the absent candidate remains unresolved through the real
Node import. This preserves the dependency boundary rather than claiming the
two installed closures are equivalent.

Each condition uses an exclusively created UUID root and a fresh Node. Its
actual installation/import outcome is sealed in CAS. The native handler's
trial plan and execution share the same `PreparedTrial` and frozen
`configuration_ref`. Schema-2 reports and sample receipts retain the
configuration and installation references alongside the genuine committed
output.

What it runs on:

- a real throwaway installation — its own SQLite store, its own CAS, its own
  genesis, head and first step;
- the real subject loop (`run_subject_loop`), so the frozen input, the model
  request and the output parser are the production ones;
- the real dispatcher and a real native child service, so a model that asks
  for the unit body actually reads it and the answer reaches the next step;
- the real broker, so the trial's inference is metered like every other
  inference.

The sandbox publishes the Supervisor's services and acquires dispatcher
ownership before starting the subject. Its original message is queued before
startup. Measurement follows committed decisions in order, beginning with the
decision that consumes that message. Delegated work stays outstanding until
its genuine T2 result appears in a sealed input and receives a committed,
non-deferred disposition. A consuming step may delegate further work; that
work must also settle before the first non-delegating continuation is scored.
Successful cognition reads bind the returned unit, revision and artifact
digest. The sample receipt retains these results and the selected step/input
identities. A model that never requests a body is judged on its actual
message-consuming decision.

An unresolved delegation or an expired subject/judge deadline fails the
attempt. Cleanup closes subject delivery, joins the subject and dispatcher,
and awaits the Supervisor's native-worker shutdown barrier on every setup or
observation outcome. Cleanup errors remain visible alongside the original
failure and also prevent a successful observation from being scored. Aborting
a local task does not cancel or erase an already admitted provider request.
Failure to seal a sample receipt is reported as a failed attempt without a
receipt digest.

`PreparedSandbox` and `run_prepared_sandbox` expose that same subject,
dispatcher, observation and cleanup lifecycle to caller-owned runtimes. The
caller supplies its fresh Node, baseline and native plugins through
`SandboxRuntime`; installation finishes before the subject's capability
snapshot. A runtime may accept a particular failed result or original
unresolved observation only after authenticating it. The default cognition
runtime retains its strict successful-read and effect-free contract.

The selected continuation is the first eligible committed S1. The runtime
captures its independent facts before cleanup, rather than selecting a later
favourable result or reconstructing facts after shutdown. A typed
`SandboxCleanupFailure` also tells a caller that later mutable bindings cannot
be reused. The caller keeps ownership of its broker and original receipt
writers and must drain them after the comparison.

A subject that returns an error and is normally joined still fails the
sample, with its original execution error and provider request retained. That
execution failure alone does not mark later mutable bindings unsafe. Failure
to join an owned task, an owned-task panic or failed service shutdown remains
a `SandboxCleanupFailure`.

The caller supplies the scenarios: a name, the message the subject receives,
and the rubric clause that says what "the unit helped" means. The rubric is
given to the judge only; the subject never sees it. The judge is a model call
against the same broker, and it sees the user message, the output and the
rubric — **never the installed unit and never which condition produced the
output**. There is no keyword scoring, no expected-output script and no
caller-supplied verdict.

`judge_sandbox_output` additionally accepts an authenticated CAS object of
independently collected facts, up to 128 KiB. Its caller owns their connection
to actual effects and supplies a blinded projection without candidate bodies,
condition labels or installation paths. The facts reference and full original
judge exchange are retained. This does not change the native cognition
judge's prompt or grant an execution capability.

The judge's full original request is retained before dispatch, including the
payload, identities, binding, priority and epoch; only the oneshot response
channel is omitted. The raw full `InferenceResult` is sealed and journaled
before verdict parsing. A receipt snapshot may precede a late result and lack
its `result_ref`; its `journal_id` is the durable authority for discovering
the later terminal reference. A timed-out or otherwise failed judge remains
a failed attempt even if that late result says `EFFECTIVE`.

The capability-gap efficacy experiment retains the required twelve held-out
scenarios. That count is an experiment requirement, not evidence of efficacy:
tentative cases remain unaccepted until the real model comparison supports
them. Retained reports are not overwritten to hide unsuccessful trials.

The retained R7 run on source `22f27228bffc266e9290b3c157e9fb2524f65c27`
planned 52 samples across thirteen scenarios and two repetitions per condition.
Candidate samples were effective in 20 of 23 scored attempts, with 3 failures;
control samples were effective in 9 of 18 scored attempts, with 8 failures.
All thirteen scenarios remain `accepted: false`. The test exited 101: its
eleven failed slots include five empty judge verdicts, three attempts that
could not start within their pair budget, and three subject/deadline failures.
This is a retained initial measurement, not six-repetition acceptance or a
completed live-network fixture. The original requests, results, failures and
broker retirement snapshot remain under
`build/ecosystem-compat-20261104/cognition-model-r7/native/run-49b56b81-f817-4dcb-a68d-364644257d41/`.
The broker snapshot reports its local provider readers drained and makes no
remote-termination claim. Nine native mechanics regressions passed separately
under `build/ecosystem-compat-20261104/cognition-preparation-after-r7/`;
they establish preparation, consumption and receipt behavior, not model efficacy.

The control-exclusion regressions require every retained candidate version to
be absent and the exact known-ID/revision native read to return its original
`unit_not_readable` failure, without a successful sample receipt, judge receipt
or scored control attempt. A consuming S1 must contain that same complete T2
in its authenticated sealed input. If the sandbox deadline stops observation
before consumption, the original T2 must remain pending for the same agent,
without a committed disposition. That deadline establishes exclusion and a
failed read; the separate failed-read regression still requires actual S1
consumption.

The first merged-main check under `cognition-main-6c7f87a0-r1/` retained eight
passing cases and the seed control's deadline-sensitive assertion failure.
Its original failed read and pending T2 remain unchanged. The corrected
assertions passed all nine native cases, and the model consumer compiled,
under `build/ecosystem-compat-20261104/cognition-control-terminal-after-r1/`.
That run consumed its control failures; it does not replace the earlier
deadline receipt or establish model efficacy.

The integration consumer is
`cargo test -p shenko-exec-host --test awareness_efficacy -- --nocapture`.
It calls `run_sandbox_trial`, the same measurement engine used by the native
trial child, with the judge using the same metered broker. Each scenario and
repetition gets a separate public runner call with `trials_per_condition: 1`
and its own production deadline shared by that candidate/control pair. The
complete `Batch`, including its repetition number and production report, is
persisted before the next call. There are no scenario chunks or repetition
offsets. Every consumer invocation creates a fresh persistent run directory
under `SHENKO_SANDBOX_ROOT` before starting its broker. This consumer measures
behaviour; the native trial handler owns the Node's trial lifecycle and
adoption evidence registration.

After measurement, including a measurement error, the consumer keeps the
broker task alive during `quiesce_for_handoff`'s bounded 120-second drain of
local provider readers and receipt writes. It persists the returned snapshot
in `broker-shutdown.json`, including any unknown requests and active or
unknown domain charges. An incomplete drain is explicitly recorded as an
error with `remote_termination_claimed: false`; drain, artifact and local join
failures are reported alongside the primary error before the final test
result. Only after draining and attempting to persist that artifact does it
abort and join the local broker task. A drained local reader does not prove
remote termination or release unknown debt, and late completions never
rewrite scenario results.

Every attempt is reported. A provider error, a judge error or an exhausted
budget is a failure of that attempt: it is counted, it appears in the report
with its reason, and it is never quietly dropped to make a rate look better.
The totals of each condition state `attempted`, `scored`, `failed` and
`effective`, and the rate is over the scored attempts with the attempted count
next to it. The verdict is `supported`, `not_supported` or `inconclusive`;
`inconclusive` is the answer when one side has no scored attempt, and it is
not a pass.

The comparison is bound durably: the Node opens the attempt before the run
and closes it with the per-sample receipts (each sealed in this
installation's content store), and the report itself is registered as an
observation. A judgement may then cite the attempt id; the Node verifies it
and only then does the unit carry `empirical: true`.

## Supersession and versions

Every install is one exact version (`unit_id`, `revision`, artifact digest of
the entry bytes). Installing a new revision records `supersedes` relations to
the named prior versions; superseded versions stay retained and readable at
their exact digest for audit and recovery, but their cards leave the offered
index.

## Self-knowledge from real facts

`SelfKnowledgeProvider` answers from what this running artifact actually is:

- `self://runtime/capabilities` — the native definitions, the live
  agent-visible capability catalog and the installed cognition units that
  actually registered. Installing, enabling or judging a unit rebuilds this
  view from the installation authority again, so the answer about "is it
  enabled" is the live fact and not the snapshot from startup.
- `self://runtime/manifest` — the build manifest (source revision, artifact
  digest, spec version) of this binary, invalidated with its generation.
- `self://source/layout` + `self://source/files/…` + `self://docs/files/…` +
  `self://design/files/…` — an index of the real files on this node when
  `SHENKO_SOURCE_ROOT` is set, each entry with its own content digest. When
  the source tree is absent the index says so instead of describing a
  repository that is not here: a feature of main is not a capability of this
  instance.

## Ownership

- `shenko-node/src/units.rs` — installation authority: unit facts, the
  trial attempt lifecycle, observations, and adoption verification.
- `shenko-awareness-core/src/cognition.rs` — package form, parse, catalog
  read model, paged discovery, bounded disclosure, seeds, `install_dir`.
- `shenko-awareness-core/src/self_knowledge.rs` — runtime facts a subject can
  query about itself.
- `shenko-exec-host/src/cognition_task.rs` — the read and write children.
- `shenko-exec-host/src/cognition_query.rs` — the list and evidence children.
- `shenko-exec-host/src/cognition_trial.rs` — the sandbox comparison engine
  and its child.
- `bin/shenko/src/main.rs` — startup wiring: open the Node, build the catalog,
  install seeds and authored packages, spawn the children, publish the runtime
  facts into self-knowledge.
- `shenko-subject` — prompt disclosure site (`context_projection.rs`) and the
  loop state's catalog handle.
