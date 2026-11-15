# Durable Node control quorum — 2026-11-15 continuation

Status: the Node protocol and bounded transport have passed their focused
process journeys and existing election regressions. This proves the stated
control-plane paths and retention of original frozen plans at the source scope
recorded below. The continuation now wires Store authority reservations, actual
availability forks with inherited input selections, and live runtime
reconstruction through `58b30ba`. Four integrated runtime/branch journeys plus
the existing owner-lock regression passed at `60e9963`. The separate final Node
library run passed 34/34; [the combined record](readiness-continuation-20261115.json)
binds these results and the archive/Clippy checks to their exact inputs. Earlier
Node results below retain their own narrower source scope.
There is no canonical cross-node S1 materialization claim. See the
[current distributed boundary](../specs/nodes.md#implementation-and-evidence-status)
for the three original gaps and their distinct evidence status.

## Why the previous boundary was insufficient

At base `299447344bad7c3d844a9f2411963c50f803f0b5`, an election or membership
vote quorum caused the proposer to insert and apply a control entry locally.
Peers had approved an intention, but had not necessarily persisted the entry's
bytes. Losing that proposer could lose the only copy of a claimed committed
control decision. A certificate listing voters does not establish replication.

The control plane now distinguishes durable prepare promises, durable accepted
values, actual accept acknowledgements, and learned committed facts. Experiences,
raw inputs, external results and isolated availability branches remain outside
this consensus log. Lack of a global control quorum is not a prohibition on
local intelligence. The current automatic election fallback is narrower than
the full component contract: it requires no authenticated response from another
configured voter during that recovery attempt. Connected minority coordination
and a persistent follower remain implementation gaps.

## Protocol and assumptions

This is a sequential prepare/accept control log, using the state-machine
construction and highest-accepted-value rule from [Lamport, Paxos Made Simple,
§§2.2–2.5 and 3](https://lamport.azurewebsites.net/pubs/paxos-simple.pdf). It is
not an implementation of Raft. [Ongaro and Ousterhout, extended Raft paper,
§§5.4 and 6](https://raft.github.io/raft.pdf) provides the comparison: log safety
and configuration transitions require actual persistence rules, not just an
additional election certificate.

The fault model is authenticated, non-Byzantine nodes with durable SQLite
storage, process crashes, message loss, duplication and arbitrary delay.
Acknowledgements are bound to the actual configured peer's authenticated route.
They are not cryptographic Byzantine quorum signatures. OS/account credentials
that can replace a node installation remain an actual trust boundary.

A ballot is a term and proposer identity. Each acceptor durably promises at most
one proposer per term; a competing proposer must advance the term. There is no
wall-clock lease. One identity remains one voter.

1. A campaign seals the exact committed prefix and its governing configuration.
   Positive responses persist the promise before returning and report the
   highest accepted next-slot value. The original promise snapshot is retained.
2. A prepare quorum may propose the next control slot. If any response reports
   an accepted value, it must preserve the value with the highest accepted
   ballot, including another node's primary or membership proposal.
3. Acceptors verify the prefix, ballot, configured identities and selected value,
   then persist the accepted bytes and their acknowledgement in one transaction.
   The acknowledgement names the exact slot, value digest and prefix digest.
4. Only acknowledgements from a majority of the configured sets can commit the
   slot. Membership transitions require both the old and new majorities for
   Joint and Final. A recovered configuration requires a new prepare under the
   newly committed configuration.
5. A stable proposer reuses its prepare only after every intervening slot was
   committed under that same ballot. Learning another ballot's newer prefix
   requires a new prepare. A candidate behind a reachable quorum learns the
   certified suffix from responses and retries against the actual prefix.
6. A successor may learn an old primary as an immutable fact. Its original term
   remains unchanged; it does not elect the successor or restore the old owner's
   current authority. The successor commits its own primary in a later slot.
7. A learned slot is never overwritten. Earlier accepted values and late replies
   remain durable evidence. A late valid quorum may teach a chosen old value
   after a higher promise, without lowering the locally observed term.

Existing schema-1 control history is retained as legacy, including its original
vote certificates. Reading it does not create a usable primary fence. New
schema-3 wire values carry independent persistence proofs (retained schema-2
proofs remain historical data), and a usable
primary must match the current term and committed membership revision.

## Store admissions and recovery

`ControlKind::Admission` orders a frozen `CommitAdmission` from the generic
`shenko-types::authority` contract. Its conflict key identifies a branch
successor or logical effect position across controller changes. Its action ID
is stable for that position. Another value at an already committed key or
identity is refused before acceptance, not after poisoning the committed log.
Write aliases consume shared predecessor positions in that same slot, so two
logical operations cannot both use one task revision. Read fences require an
ancestor position to remain unconsumed without reserving it exclusively; many
child actions can read the same live parent. A fresh local grant rechecks these
reads under the Node reservation. Historical grants remain available for ordered
recovery after a later cancellation and do not renew a send permit.

The original frozen plan and required CAS closure are retained on acceptors
before they acknowledge. Each object is checked against its digest and byte
length; typed JSON references are followed and must be present. Opaque bytes
are not treated as reference graphs. A surviving replica can provide the
original unapplied plan for inspection and reconciliation. Recovering those
bytes does not establish that a Store applied the plan.

An exact retry returns the original grant. Only the call that created the
original durable round can receive a fresh admission result; restart or a
concurrent retry does not renew physical dispatch. Replayed dispatch is an
original-attempt reconciliation boundary. The Store still applies its local
head/owner/cancellation CAS while the Node authority reservation is held.

`ControlAdmissionError` distinguishes unavailable authority/quorum and retained
original work requiring recovery from malformed descriptors and storage errors.
Runtime supervision must not repeatedly restart on an invalid plan or hide
corruption as a partition.

## Chosen plans, local materialization and branch continuation

The control log chooses an original plan. The existing Store mutation paths
allocate local commit references, IDs and timestamps; executing that plan on
another ledger would not replay one fixed canonical result. The predecessor
snapshot is therefore retained source evidence, not a second Store database
or an instruction to overwrite newer facts. The follow-up in `25207eb` binds
head, expected tasks/ancestors and registered steps to one frozen snapshot,
then stores stable CAS row pages and their required closure. This also closes
the source-review gap in which task revision R could name aliases while a
later snapshot silently carried R+1. This report records that source correction;
its integrated Store behavior checks belong to the enclosing candidate.

For a chosen plan whose materialization cannot be confirmed, the live runtime
adapter preserves its exact descriptor, original payload and CAS evidence and
records durable reconciliation debt. If the original selected local authority
is still current, it creates an actual availability branch before starting new
consumers. This selected-plan recovery path is distinct from isolated-election
fallback. It cannot rebase the original epoch, assign its request/attempt to the
new owner, or gain another physical send opportunity. A reconstructed original
grant is historical evidence. The integrated process-exit and reconstruction
journey now passes; its limits remain in the runtime authority evidence.

Canonical transition replay remains a separate requirement: it needs an
explicit typed read set, fixed write/result identities, and a logical decision
identity distinct from a node's local materialization receipt. This continuation
does not manufacture that capability by rerunning S1 or substituting local
`CommitRef`s into another node's sealed plan.

The three original gaps are now narrowed as follows:

| Original gap | Source checkpoint | Evidence boundary |
| --- | --- | --- |
| Vote quorum without persisted entries | `97602f5`, bounded transport/key fences `31010ab`, legacy-admission refusal `e0dffa0` | Focused Node journeys and library/Clippy results below; the final four-line legacy refusal awaits the combined candidate's checks. |
| Node authority not coupled to Store commit | Reservation `c429dea`, fresh-grant read checks `e7d4571`, Store/runtime integration `2169cb8`, failure-lifecycle correction `b066224`, final wiring `4baed97` | The reservation spans exact grant validation and Store CAS; local materialization follows successful COMMIT. Integrated reservation/cancellation and reconstruction journeys passed; the local lock is not a remote lease. |
| Availability fact without a real Store fork | Store/Node/context fork `d09b087`, coherent predecessor/head validation `25207eb`, inherited-input/closure/version corrections `004df65`/`4a4a365`/`aa745a4`, origin/branch CAS `61ba367`/`43f4580`, final wiring `4baed97`/`58b30ba` | Combined fork/rebuild, inherited-input, parallel rejoin/refusal and authority-change journeys passed at the recorded source. Real physical partitions remain a distinct acceptance level. |

Targeted fork review found a delivery boundary separate from raw retention.
A web/Playground/media/self-wake input staged in the parent's current step has
already set `inbox_events.consumed = 1`. Its original raw event survives in the
child's frozen history, but the initial fork does not give the child's new step
a `subject_raw_inputs` reference. The ordinary pending query and S0 recovery
therefore omit it from the child's selected `InputPlan.events`; an obligation
naming that input is then refused as unselected. The source correction in
`004df65` freezes the unresolved selections in the fork receipt and links the
child's first step to the same raw originals. It changes the old unique-raw-link
index into a nonunique index without changing any row; `(step, source)` remains
unique. Original raw/session/sequence, normalization, inbox flags, requests and
attempts retain their provenance. Multiple generations inherit those references;
media still verifies every link against the original admitted event.

Independent review caught a follow-up import refusal: the post-merge closure
reader omitted `subject_raw_inputs`, so complete incoming forks with inherited
inputs were revalidated against an artificially empty link map. `4a4a365`
includes those rows in the actual reconciled closure. Review also identified
an intermediate-format distinction: `aa745a4` makes new complete receipts schema
2. An old schema-1 receipt is accepted only when it has no inherited selections
and its frozen parent prefix has no unresolved selections. Otherwise complete
import explicitly requires continuation repair and preserves the original
files/database; it does not fabricate an old proof. These schema-1 receipts
belong to unpublished continuation candidates, not the pre-continuation baseline.

The targeted source review found no additional defect in the index migration,
frozen ancestor normalization, multiple-generation first-step references or
media admission binding after those corrections. Two existing branch recovery
journeys were extended by the root slice; this read-only review added no
independent scenario or runtime test. The subsequent combined run passed both
extended journeys, as recorded in the branch-continuity evidence.

## Final source integration boundary

The Store provider now prepares an exact action before local locks, then holds
the Node reservation across the Store's own head/task/epoch/cancellation CAS
and COMMIT. Fresh grants recheck read dependencies; old grants remain readable
without renewing dispatch. The applied-here marker is local and nonreplicated,
so an imported inbox receipt cannot claim local materialization. The live
runtime stops and drains its consumers on an authority-recovery signal before
the next owned Start reconstructs them. A closed writer or storage error keeps
its actual failure; it is not converted into an endless election retry.

The final election adapter records authenticated contact across catch-up,
prepare, accept and dissemination, including refusing or malformed replies via
`PeerResponded`/`rpc_observed`. It permits at most three prefix/term recovery
rounds. After connected failure it waits 200–455 ms, catches up once without
recampaigning, then returns `PrimaryNotSelected` and stops this local Start.
Only an attempt with no authenticated other-voter response can use automatic
isolated fallback, and only if the failed term/configuration and no-fence
observation still match the reserved Node state. This is neither a proof of
permanent isolation nor an implementation of connected-minority coordination.
There is no resident follower lifecycle in this path.

Availability opening retains holder, term, configuration revision/digest and
the exact optional fence in the same immediate transaction as the fact. The
conditional API compares the complete expected authority snapshot and branch
fact. That branch CAS refuses a concurrent rejoin that changed only merged
state or reconciliation floor, instead of reopening it from an older snapshot.
The Store-fork adapter checks expected parent identity and target opening origin
under the reservation. Runtime admission uses `is_current_availability` and
includes the origin in its authority stamp; a historical branch fact alone is
not current permission. Receipt-proven repair of an already committed Store fork
still preserves the original fact even after authority changed, without
restoring fresh local authority.

The episode correction in `58b30ba` freezes validity before the next campaign.
Only a previously current no-fence availability branch, unchanged identity,
configuration and full branch fact, with exactly one new local term, may reuse
its branch identity. A branch that recovered quorum or learned a newer term
must create a new real fork on later isolation; its old metadata cannot hide
that new divergence. Missing legacy origins are not reused.

These source changes close the earlier claim that PLAN quorum, the Store bridge
and real branch transition are wholly disconnected. They do not supply canonical
cross-ledger S1 state-machine replay, a unique coordinator in each connected
minority component, or real model/multi-device/long-run acceptance. The product
requirements remain unchanged. The integrated local checks are now recorded
against their actual source, independently of the historical focused results below.

## Bounded transfer and immutable closure retention

Normal election, admission and membership catch-up use the same callback-driven
transport in `crates/shenko-node/src/control_transport.rs`; the production adapter
supplies its authenticated SSH route. A compact frontier binds index, term and
prefix digest. The receiver imports only a missing suffix in bounded pages.
Locally cached prefix digests and the contiguous ballot run avoid rescanning all
past decisions on each S1. A stale candidate pulls the verified missing prefix
and starts a fresh prepare; learning a higher term also permits a fresh campaign.

New admission entries contain the frozen descriptor and its root reference.
Their CAS bytes remain in the existing Node object store. Transfers use 64 KiB
raw chunks with durable cursors and exact duplicate checks; only the final
assembly hashes the complete object. A persisted whole-closure marker is created
only after all typed children have been verified. Later transfers can skip an
already verified immutable subtree, while an incomplete root cannot pretend
that its missing children are present.

Before accepting a new value, Node serializes its worst-case certificate with
every eligible voter and requires it to fit the 256 KiB entry metadata frame.
This bounds metadata, not frozen plan content. Control pages target 512 KiB;
prepare witnesses name accepted values by digest and include the highest value
only once. Replication proof frames are bounded before any ACK is emitted.
An oversized new descriptor therefore fails before it can strand a chosen slot.
The schema-10 migration retains old values, accepted facts and receipt bytes,
backfills prefix/alias indexes, copies prior inline plan objects to CAS and starts
a new ballot before old full-prefix prepare records could be reused.

A valid value can have different accepting certificates after recovery. Its
identity excludes certificate metadata and committed copies are never rewritten.
A stale request receives the current-term refusal before attempting to revalidate
its obsolete live prepare. A stable prepare that cannot cover a peer's retained
certificate run requires a fresh campaign; it is never inferred from a cached
primary token or a wall-clock lease.

## Focused evidence

The checks below use the shared native Cargo target, `--offline --locked`, one
build job, disabled incremental compilation and debug info. Source is the
bounded-transport follow-up to `97602f5`, with the shared predecessor fields
from `f452a3a` and `6f27f70`.

- `cargo test -p shenko-node --test partition_rejoin -- --nocapture`: **4 passed,
  0 failed**, 12.38 seconds after compilation. The two pre-existing process
  journeys pass together with exactly two new independent journeys.
- The lost-acceptance journey loses the sender's receipt, recovers the original
  chosen primary before electing a successor, refuses the old owner's new
  admission, recovers the exact unapplied frozen plan from a survivor, and
  catches up the former leader before its new campaign. A 5 MiB binary attachment
  makes the former inline JSON closure exceed the production 16 MiB message
  bound; the actual protocol uses bounded frames. A changed logical operation
  cannot reuse the consumed task revision alias, and retries never renew the
  original dispatch opportunity.
- The membership journey loses the proposer after Joint was chosen and proves
  that the new-set majority alone cannot commit recovery. Both configured
  majorities remain necessary through the joint election and Final transition.
- `cargo test -p shenko-node --lib election -- --nocapture`: **17 passed,
  0 failed**, 0.15 seconds after compilation. This reuses the existing retained
  conflict, idempotent replay, nonrewinding vote, membership and selection checks.
- `cargo test -p shenko-node --lib`: **34 passed, 0 failed**, 0.24 seconds
  after compilation. This covers the existing SQLite installation recovery,
  package migration and identity/selection regressions affected by schema 10.
  An earlier attempt hit transient `ENOSPC`; the next run succeeded after a
  fresh disk check and exact-directory create/write probe, without deleting any
  shared target or unrelated work.
- Focused Clippy uses the existing ratchet's `--force-warn clippy::all`,
  `unwrap_used` and `expect_used` policy. Its first completed collection found
  three new diagnostic kinds in control code; the quorum comparisons, boxed
  retained-grant error and chunk-boundary expression were corrected. The final
  all-target collection at `31010ab` exited 0 and contains no Clippy diagnostic
  in `control.rs` or `control_transport.rs`. The fresh grant read-fence helper
  has an expected unused-function warning in this isolated Node worktree until
  the separately integrated Store guard calls it; no suppression was added.
  The whole-workspace immutable-baseline ratchet remains an integration gate.
- The first process run found a stale-request refusal-order defect and a fixture
  that compared different replicas' local observation timestamps. The corrected
  run above follows the actual protocol and compares the same replica's retained
  configuration when proving that an insufficient quorum did not change it.

Authority requests and remote accepts run in fresh `shenko-node` processes with
private SQLite installations. The shared transfer driver opens the durable
source and sends each remote frame to another process. This does not exercise
SSH reachability, real model choice, a heterogeneous deployment, long-run
availability, or physical provider exactly-once delivery. In particular, the
Node evidence does not substitute for a canonical Store transition unit or
for integrated behavior when an original chosen plan remains unapplied.
