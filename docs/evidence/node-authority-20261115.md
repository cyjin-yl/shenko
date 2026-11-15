# Node authority, delayed votes and membership replay

Historical checkpoint: the commands, hashes and limits below describe the
initial authority fixes and their immediate follow-ups. The later
[durable-control continuation](control-quorum-20261115.md) replaces the
vote-only persistence boundary and wires the Store authority bridge, actual
branch continuation and runtime reconstruction through `4baed97`. Their final
combined behavior gate remains pending. The old observations below are evidence
for their stated source; they are not a claim that those three mechanisms are
still wholly absent today.

Date: 2026-11-15. Branch: `fix/readiness-distributed-20261115`.
Base: `054817b0d5729fed56ec5a974d101c31edfd1410`.
Candidate: `crates/shenko-node/src/election.rs` SHA-256
`6cf9a6dfb06d5ee25601d0d4526600b3baaac044da678a995897b222a88263b5`.

## Concrete failure interleavings and corrections

- An election request from term 2 receives a delayed term-3 response after the
  requester has already persisted term 4. Comparing response term only with
  request term formerly rewound durable state to 3. The receiver now compares
  with current durable term, retains the response as a non-ballot fact, and
  refuses to count it in another campaign. A higher current-member term still
  steps down the candidate. Historical membership authenticates a late fact,
  while only current voting scope may advance authority.
- Re-tallying a successful or superseded election formerly created another
  primary epoch from the old ballots. Tally now requires the current term and
  the local ballot's exact configuration. Repeating a committed tally returns
  its existing claim. A historical claim cannot pass the node fence after a
  higher observed term or removal of its leader; the full token's configuration
  revision is checked too.
- Re-tallying a committed member proposal formerly incremented configuration
  again; tally after a later campaign could label old votes with the new term.
  A proposal now names its retained base and exact joint/final transition.
  Replay returns the original configuration at `base_revision + 1`; changed
  content conflicts, and uncommitted proposals require current-term votes.
- Imported certificates formerly checked quorum against member sets supplied
  by the same entry. This both permitted an incomplete joint quorum and
  rejected legitimate new-member votes in a joint election. Append and push
  now check the complete prefix's configuration lineage, revision, term,
  distinct voters, both required majorities and primary epoch sequence before
  applying any new entries. A verified historical suffix can catch up without
  regressing a later observed term. Explicit full-prefix push also applies the
  projection of matching retained rows above its old recorded frontier.

At this initial checkpoint, the existing schema and wire remained unchanged.
Receipt observations use content-addressed, non-ballot scopes in the existing
`node_votes` table; they
cannot be counted by `votes_for`. No fact journal, child result or unknown
external effect is replayed by this change.

## Verification

Linux local filesystem, real SQLite, offline Cargo dependencies; shared build
resource lock, two build jobs, incremental compilation and debug info disabled.
The source hash above binds these commands and their outcomes:

```sh
cargo test --offline --locked -p shenko-node --lib election::tests:: -- --nocapture
cargo test --offline --locked -p shenko-node --test partition_rejoin --test reconciliation_receipts -- --nocapture
```

- Library: **16 passed, 0 failed**. The substring filter includes six existing
  `selection::tests`; ten are election tests. Exactly two new independent
  regression journeys were added: delayed campaign responses across reopen,
  and joint/final membership retry with contextual certificate validation.
- Existing process integration: **3 passed, 0 failed** (two partition/rejoin
  cases and one receipt/reopen case). These exercise separate local node
  processes, preserved conflicting facts and receipt deduplication; they do
  not exercise real SSH transport, a provider or distributed failure detection.

Raw command output is retained in the task's `evidence/node-election-fixed.log`
and `evidence/node-partition-fixed.log`; the integrator retains the enclosing
candidate and evidence manifest. Existing unused-variable/dead-code warnings
remain. No runtime services, remote nodes or user effects were started.

## Deliberate limits and remaining integration work at the initial checkpoint

This is control-plane safety evidence, not full distributed product acceptance.
Certificates contain voter identities, not independent cryptographic vote
signatures; authenticated peer transport is still a required trust boundary.
This initial patch did not prove Byzantine tolerance or full log-commit safety.
Membership ballots were scoped only by proposal digest, so the same voter
could grant two different proposals from one base in a term. The follow-up
below fences that concrete conflict. At that checkpoint, a proposal vote did
not durably append the proposed entry to the voter's control log before the
proposer called it committed. Thus a crash before replication could leave
another voting majority unaware of that entry. Refusing divergent prefixes
preserved evidence but did not by itself supply Raft's log-commit safety.
Quorum persistence of the actual
control entry was a distributed-release blocker, addressed by the later
prepare/accept continuation rather than by these initial tests.

At the initial checkpoint, `bin/shenko/src/lib.rs` established a primary or
explicit availability branch fact before spawning `run_subject_loop`, and
recorded a fenced role observation. The long-running main loop did not
continuously consume that Node authority. Therefore this patch alone could
not claim that a later local election result stops an already-running old
controller's new subject decisions/effects, or
that reconnect automatically hands control to the smartest usable peer.
The subject's branch/controller commit guard was a separate local Store
mechanism. Connecting those boundaries had to preserve the requirement that
isolated intelligent branches may continue with their own explicit history;
absence of a global quorum is not a blanket prohibition on local work.

The initial startup no-quorum path additionally created a Node `BranchFact`
with a fresh identifier, without changing the Store head or the Node's subject
identity. It therefore did not perform the actual subject branch fork. The
existing update adapter correctly requires all three branch identities to
match; creating a branch fact alone cannot satisfy that binding.

A bounded next integration therefore needed an explicit durable authority
binding at the Store's existing S1 and dispatch fences, and a crash-recoverable Node/Store
transition with a fixed lock order. S1 already checks `checked_head`; native
dispatch already checks controller epoch and dispatcher ownership; publication
arming already checks the authoring epoch. A newly observed authoritative
change must fence those same resources while retaining late result/receipt
facts. Merely polling Node between Store operations leaves a check/commit race.
The availability path also needed a real fresh-branch continuation with causal
parent and unknown-effect history, rather than overwriting the old branch or
gating all local work on global quorum.

## Follow-up: current token consumers and membership ownership

The follow-up keeps the historical claim API intact while making
`fencing_token` return only a currently usable fence, with its claim, term and
configuration read in one SQLite snapshot. `check_fencing` compares the exact
token. Status projections no longer label a historical claim as current quorum.
The update adapter returns the fields of the exact token it checked; it no
longer reads one claim and then potentially validates a different newer token.
An already merged availability branch cannot supply local update authority.

New member proposals and uncommitted tallies now require the current committed
leader. A voter's existing proposal rows reserve one compatible proposal and
candidate for each base/term, including ballots retained from older writers.
Checking and granting use the same immediate transaction, so two concurrent
requests cannot both win different reservations. Compatible replay keeps the
original row; a changed proposal is explicitly refused. A stale/outside
configuration request or membership proposal cannot advance the term. The
CLI establishes current primary authority before add/remove, so singleton
bootstrap remains usable. Historical committed tally replay remains readable
after a real leader change. These restrictions govern domain reconfiguration,
not an isolated branch's right to retain experience and work locally.

Source hashes for that follow-up:

| File | SHA-256 |
| --- | --- |
| `crates/shenko-node/src/election.rs` | `e1f58f951e0f77947bca123a1661e59e20723eb86526501ac7953b148734ec0e` |
| `crates/shenko-node/src/capability.rs` | `d84f70fe1d6bd754a1bf663f6cd0d9dac2ae01678fa95eade5a8872090135d42` |
| `bin/shenko/src/node_sync.rs` | `d2326a043fdda4fe23aa7eb3c02642db279db3bfeb139d261f432f4ab085080a` |
| `bin/shenko/src/update_runtime.rs` | `24fe37bb5e543b49db805e8a87d268eb7fecf6920329c6655ac97c1952c8a611` |

The existing delayed-response regression also checks absent current tokens,
false current-quorum status and retention of the historical claim. The same
membership journey now checks conflicting proposals, exact ballot replay,
an actual A-to-B primary change, and stale/outside requests that leave the
term unchanged. No new independent test journey is added.

The same focused library command passed **16/16** with one build job;
`evidence/node-consumers-fixed.log` retains that result. The same existing
process integration command passed **3/3**, retained in
`evidence/node-consumers-process-fixed.log`. The combined application build
and final integration evidence belong to the integrator's candidate.

No real-model behavior, physical multi-device partition, automatic takeover,
full update/rollback or long-running reliability result is asserted. Acceptance
and coverage status remain owned by the integration task; no requirement is
promoted to `verified_real` by this report.

## Follow-up: interrupted CLI membership changes

The strict prohibition on a second joint transition exposed a CLI recovery
gap: add/remove always began a new joint round, even when the previous command
had committed its joint configuration and stopped before Final. The CLI now
compares the requested target against the retained joint target, deriving that
request from the old member set. A different target is an explicit conflict.
A read-only Node lookup reconstructs the original joint proposal from its
retained predecessor and the existing proposal digest rules.

For a matching resume, the CLI pushes the validated retained prefix to the
old/new voter union before establishing current primary authority. This lets
voters that granted ballots but missed the committed entry catch up before an
election under the joint quorum. Existing divergent-prefix refusals remain in
force. The command then proposes only Final under the current primary; it does
not author another Joint. Both phases' catch-up cover the voter union, including
surviving old members and removed members that need to learn the final fact.
When no transition is pending and the requested set is already current, the
command reports that fact without producing another pair of configurations.

The existing membership journey now reconstructs the proposal from a follower's
retained prefix for finalization and checks that no pending proposal remains
after Final. This adds no independent test journey. The earlier command results
and hashes above describe their recorded candidates, not this later change;
the updated source and combined CLI compilation remain the integrator's gate.
At that checkpoint this recovery adapter did not supply quorum persistence of
actual control-log entries or automatic partition/failover qualification. The
later durable-control continuation supersedes the first missing mechanism;
these recorded CLI checks do not qualify its behavior retroactively.

## Continuation status: the three distributed gaps

The Node control path now persists actual accepted values and acknowledgements
on a configured quorum before learning a commit. It recovers the highest
accepted original value after proposer loss and uses bounded suffix/object
transfer. Those facts supersede this report's vote-only log-persistence blocker;
they have focused process and library evidence in the continuation report.

The Node/Store boundary now has a reservation held across local grant validation
and Store CAS, plus exact admission keys/read fences in the control log. The
applied-here record is local, nonreplicated and set only after successful Store
COMMIT. The real fork path retains a Store parent head, frozen history and
uncommitted input selections, creates new branch/task/step identities, and
repairs Node projection from the Store's own receipt. The runtime adapter and
owned-consumer reconstruction path are source-wired through `4baed97`; their
combined behavior gate is pending, not an absent integration.

The remaining canonical-materialization boundary is explicit: a quorum-chosen
original plan is not a deterministic cross-node S1 result. Unconfirmed apply
retains original plan/CAS evidence and durable reconciliation debt. A still
selected local primary rechecks its original authority before continuing on a
real child branch ahead of new consumers; it cannot rebase the old epoch or
reissue its physical attempt.

Automatic election fallback is separately restricted to a recovery attempt
with no authenticated response from another configured voter and an unchanged
failed-campaign authority snapshot. Connected refusal or minority without
quorum returns `PrimaryNotSelected` and stops local Start after bounded
catch-up; unique minority-component coordination and a resident follower remain
unimplemented. The opening origin binds holder, term, configuration and fence.
Conditional open compares the complete branch fact too, so a concurrent rejoin
cannot be undone from a stale snapshot. The runtime checks this origin at fresh
admission; receipt-proven historical projection repair grants no new authority.

The continuation also records the staged-but-uncommitted input-inheritance
source correction and its pending integrated behavior gate. See the
[current Node contract](../specs/nodes.md#implementation-and-evidence-status)
for the current mechanism/evidence mapping; these later changes do not alter
the historical checks or source hashes above.
