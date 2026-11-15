# Durable Node control quorum — 2026-11-15 continuation

Status: implementation and verification in progress. The first source checkpoint
introduces the safety protocol below; it is not a completed distributed release.
In particular, the first checkpoint still repeats full prefixes and embeds object
bytes in control messages. The existing RPC size limit makes that transport
unsuitable for continuous S1 admissions. Bounded object transfer, incremental
prefix catch-up, exact candidate verification and integration evidence are being
completed before this slice is reported finished.

## Why the previous boundary was insufficient

At base `299447344bad7c3d844a9f2411963c50f803f0b5`, an election or membership
vote quorum caused the proposer to insert and apply a control entry locally.
Peers had approved an intention, but had not necessarily persisted the entry's
bytes. Losing that proposer could lose the only copy of a claimed committed
control decision. A certificate listing voters does not establish replication.

The control plane now distinguishes durable prepare promises, durable accepted
values, actual accept acknowledgements, and learned committed facts. Experiences,
raw inputs, external results and isolated availability branches remain outside
this consensus log. Lack of a connected control quorum must lead to an explicit
branch continuation; it is not a global prohibition on intelligence.

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
schema-2 control values carry independent persistence proofs, and a usable
primary must match the current term and committed membership revision.

## Store admissions and recovery

`ControlKind::Admission` orders a frozen `CommitAdmission` from the generic
`shenko-types::authority` contract. Its conflict key identifies a branch
successor or logical effect position across controller changes. Its action ID
is stable for that position. Another value at an already committed key or
identity is refused before acceptance, not after poisoning the committed log.

The original frozen plan and required CAS closure are retained on acceptors
before they acknowledge. Each object is checked against its digest and byte
length; typed JSON references are followed and must be present. Opaque bytes
are not treated as reference graphs. A surviving replica can provide the
original unapplied plan for Store recovery.

An exact retry returns the original grant. Only the call that created the
original durable round can receive a fresh admission result; restart or a
concurrent retry does not renew physical dispatch. Replayed dispatch is an
original-attempt reconciliation boundary. The Store still applies its local
head/owner/cancellation CAS while the Node authority reservation is held.

`ControlAdmissionError` distinguishes unavailable authority/quorum and retained
original work requiring recovery from malformed descriptors and storage errors.
Runtime supervision must not repeatedly restart on an invalid plan or hide
corruption as a partition.

## Evidence at the first checkpoint

- `cargo check --offline --locked -p shenko-node --lib`, shared target, one build
  job, exited 0 in 12.29 seconds for the initial complete control source. This
  predates the final transport and integration work, so it is not final evidence.
- Existing election and process journeys are being migrated to actual separate
  accept and commit calls. Pure prefix-validation fixtures remain distinguishable
  from real persistence evidence.
- Exactly two new real-process journeys are authored: lost accept reply with
  proposer loss, old-value recovery, stale-owner admission refusal and former
  leader catch-up; and chosen membership transition recovery requiring both
  voting sets through Final. Each request runs a new `shenko-node` process with
  its own SQLite installation. Their results are pending at this checkpoint.
- Root/UI integration owns Store authority guards, original-plan application,
  real availability branches and startup/runtime restart ordering. This file
  does not claim those behaviors from the Node protocol alone.
