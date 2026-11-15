# Node authority, delayed votes and membership replay

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

The existing schema and wire remain unchanged. Receipt observations use
content-addressed, non-ballot scopes in the existing `node_votes` table; they
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

## Deliberate limits and remaining integration work

This is control-plane safety evidence, not full distributed product acceptance.
Certificates contain voter identities, not independent cryptographic vote
signatures; authenticated peer transport is still a required trust boundary.
This patch does not prove Byzantine tolerance or serialize every possible
concurrent membership proposer.

`bin/shenko/src/lib.rs` establishes a primary or explicit availability branch
before spawning `run_subject_loop`, and records a fenced role observation.
The long-running main loop does not continuously consume that node authority.
Therefore this patch alone cannot claim that a later local election result
stops an already-running old controller's new subject decisions/effects, or
that reconnect automatically hands control to the smartest usable peer.
The subject's branch/controller commit guard remains a separate local Store
mechanism. Connecting those boundaries must preserve the requirement that
isolated intelligent branches may continue with their own explicit history;
absence of a global quorum is not a blanket prohibition on local work.

No real-model behavior, physical multi-device partition, automatic takeover,
full update/rollback or long-running reliability result is asserted. Acceptance
and coverage status remain owned by the integration task; no requirement is
promoted to `verified_real` by this report.
