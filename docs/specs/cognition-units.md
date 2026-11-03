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
plugin contracts; the awareness path itself stays text.

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
  against a disabled baseline.
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

`cognition.unit.trial:1` runs the candidate against the same installation with
the unit switched off and compares what the model actually did.

What it runs on:

- a real throwaway installation — its own SQLite store, its own CAS, its own
  genesis, head and first step;
- the real subject loop (`run_subject_loop`), so the frozen input, the model
  request and the output parser are the production ones;
- the real dispatcher and a real native child service, so a model that asks
  for the unit body actually reads it and the answer reaches the next step;
- the real broker, so the trial's inference is metered like every other
  inference.

The caller supplies the scenarios: a name, the message the subject receives,
and the rubric clause that says what "the unit helped" means. The rubric is
given to the judge only; the subject never sees it. The judge is a model call
against the same broker, and it sees the user message, the output and the
rubric — **never the installed unit and never which condition produced the
output**. There is no keyword scoring, no expected-output script and no
caller-supplied verdict.

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
