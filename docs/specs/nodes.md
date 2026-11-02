# Node membership, election and authority — functional contract

SPEC §5.4, §22, §38, §41, §44; C17. This document is the focused contract of
the `shenko-node` election slice: the interfaces, why they exist, what errors
mean, and how recovery works. It distinguishes committed runtime facts from
runtime evidence that this slice deliberately does *not* produce (liveness,
reachability, provider readiness).

## What is decided here, and by what

The current primary of a connected domain is decided by a **committed member
configuration** plus a **quorum of verified vote receipts** — never by the
number of nodes a process happens to see, a heartbeat, a model name, an OS, or
a count of replicated agent processes (one replicated identity is one vote).

- Membership is a *configuration fact*: `MemberConfiguration { members,
  joint_new, revision, term, digest }`, committed through the node's durable
  control log. The quorum is a majority of **every** configured set (joint
  consensus: old AND new during a membership change).
- A vote is a *verified receipt*: `VoteRequest`/`VoteResponse` at
  `ELECTION_WIRE_SCHEMA = 1`, exchanged only over an authenticated route
  (the node's own SSH path). Votes from non-members or against a different
  configuration digest are rejected and never counted.
- Authority is a *fencing epoch*: every committed decision carries a
  monotonic `FencingToken { term, epoch }`. Authority-bearing writes check the
  epoch (`Node::check_fencing`); an instance that lost the election cannot
  keep acting as primary once a newer epoch is committed locally.
- Eligibility has no OS gate, no model-name gate and no local-LLM requirement
  (SPEC §5.4.1): a member is a capable node; credentials and grants are never
  exported, copied, or discovered by this slice.

## Interfaces

Library (`shenko-node`, all durable, additive storage schema 6):

- Configuration: `ensure_member_configuration` (genesis `{self}`),
  `adopt_member_configuration` (verified push), `member_configuration`,
  `propose_member_change` / `propose_final_member_change` /
  `tally_member_change` (joint then final), `control_log`.
- Election: `campaign`, `handle_vote_request`, `record_vote_response`,
  `tally`, `committed_primary_claim`, `current_term`, `election_role`,
  `fencing_token`, `check_fencing`, `is_primary`, `election_status`.
- Replication: `append_control_entries` (verifies the committed quorum
  certificate before accepting; an unverified entry is rejected, not queued).
- Partition/adoption/reconciliation: `open_availability_branch`, `branches`,
  `adopt_identity` (retains experience; records `AdoptionRecord` +
  `BranchFact`), `reconcile_peer_experience` (dedup by identity, conflicts
  retained side by side — never last-write-wins; a settled task is never
  un-settled; an unknown external effect is never re-sent),
  `reconciliation_records`.
- Assignment fencing: `record_assignment_fence`, `assignment_fence`; the
  `assignment` and `accept` paths consult `node_assignment_fences`.
- Runtime roles: `observe_runtime_roles(..., fence: Option<&FencingToken>)`;
  the `Primary` role is refused without a valid committed fence
  (`"primary observation requires a committed election fencing token"`).

Node requests (JSON, `command`-tagged): `election_status`, `members`,
`member_change`, `record_vote`, `tally`, `tally_member_change`, `vote`,
`append`, `push_config`, `adopt`, `reconcile`.

CLI (`shenko sync` in `bin/shenko/src/node_sync.rs`):

- `sync election status|campaign` — print facts; or ensure configuration,
  campaign, solicit votes over each member's authenticated route, commit on
  quorum. No reachable quorum → availability branch, exit honestly.
- `sync members show|add|remove|push` — show facts; joint+final membership
  change by quorum; push committed configuration + control log to one peer.
- `sync adopt --identity-file ... --trusted-node ... --provenance ...` —
  adopt a verified existing self. Adoption and the installation's subject
  binding are one fact: the binding is projected from durable subject
  genesis/head state, so `adopt` is refused before the node records anything
  when the trusted identity is a different self than the bound subject — the
  store cannot transition that binding without the adopted subject's own
  durable facts, and a reported success would break every later `open_node`.
  Adopting the already-bound identity is allowed and is the path that
  re-converges a diverged node/binding pair.
- `sync reconcile --peer <id>` — pull the peer's verified events/receipts
  over its authenticated route and merge as parallel facts.
- `sync identity` response now also carries the committed primary claim and
  member configuration; the candidacy ordering (`coordination` request)
  remains explicitly `is_election: false`.

Startup wiring (`bin/shenko/src/main.rs`): before the subject loop spawns,
`node_sync::establish_primary` ensures the genesis/committed configuration,
campaigns, solicits votes and commits the claim; the returned
`Option<FencingToken>` is passed to `observe`. `None` (partitioned, no
quorum) starts the subject on an explicit availability branch with no
domain-primary claim. The deterministic no-model path observes with `None`.

## Errors that mean something

- `no committed member configuration` — membership is unknown; run
  `ensure_member_configuration` or adopt a pushed configuration. Votes are
  never counted without one.
- `unsupported election wire schema` — mixed-version control plane; fails
  closed. Upgrade the peer; never bridge schemas.
- `vote was given against a different configuration/proposal` — the voter is
  behind or forked; its receipt is evidence, not a vote.
- `candidate_log_not_up_to_date` — the candidate must catch up from the
  committed control log before it may lead ( Raft-safety guard).
- `primary observation requires a committed election fencing token` /
  `stale fencing token` — a runtime tried to record `Primary` without (or
  against) the committed election; refused.
- `replication response is from a different Node` / capability-name
  mismatch — authenticated-channel binding failed; the receipt is discarded.
- `sync adopt refused` — the trusted identity is not the installation's
  bound subject and the adoption cannot transition the binding by itself
  (the adopted subject's genesis/retained facts are not yet committed in
  this store); the node is left untouched so the refusal cannot strand the
  installation mid-transition.

## Recovery

- All election state is durable in the node database (schema 6 tables:
  `node_election_state`, `node_control_log`, `node_member_configs`,
  `node_votes`, `node_branches`, `node_reconciliations`,
  `node_assignment_fences`, `node_observation_fences`). Reopening the node
  replays nothing; committed facts are read directly.
- A partitioned node keeps thinking on an explicit availability branch; the
  branch fact records that `global_quorum: false`. Rejoining is not LWW: the
  node reconciles peer experience (events deduped by identity, conflicts as
  parallel facts) and may `adopt_identity` to resume one self, retaining all
  experience.
- An interrupted vote solicitation leaves no partial claim: only `tally`
  commits, and only on quorum of the committed configuration.

## What is deliberately not evidence here

- Persisted observations are not liveness; `role_status` reports
  `persisted_observation_is_liveness: false`.
- The election does not measure peer liveness (`current_peer_liveness:
  "not_measured"`); reachability is established per vote RPC, and a refused
  or lost vote is recorded, not retried into existence.
- Capability advertisements and the candidacy ordering are facts the
  election may consider; they confer no authority.
- Model/provider locality, privacy posture and reachability live in the
  capability/quota slices; this slice keeps subject_id/count truth bound and
  separate from role observation.
