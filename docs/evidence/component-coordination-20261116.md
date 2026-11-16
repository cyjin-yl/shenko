# Minority component agreement and resident following — 2026-11-16

## Source scope and decisions

This continuation starts from `110dedc2764abc7983aed6d7bb0618846730b9a4`
on `feat/component-coordinator-20261116`. It adds an explicit component
agreement to `shenko-node`, connects it to the production SSH request adapter,
and keeps an unselected owned runtime resident before its consumers start.
It does not report the distributed completion contract as accepted.

The global committed electorate remains unchanged. A minority can agree only
when every member independently reports the same complete reachable member
set, committed configuration and subject origin. Every member durably advances
its local component revision before acknowledging the exact new proposal.
The original proposal, acknowledgement and full committed certificate remain
in command history. A complete old certificate cannot renew a superseded
promise, and a changed global term or configuration invalidates the current
component selection.

The coordinator publishes the full certificate to every other participant
before activation and confirms their retained selections. It creates a new
actual Store branch using the existing reserved fork/projection path. Followers
cannot open that branch. The availability origin binds the component revision
and certificate digest in addition to its existing term/configuration/fence.
Store admission confirms the component before taking the final local Node
reservation; a newly learned local change fences subsequent mutations. These
checks do not constitute a distributed lease or undo an external effect already
admitted by an older runtime.

`Start` now retains its owner lock while following a reachable primary or
waiting for a complete component agreement. It catches up control facts,
invokes `recover_before_consumers`, and retries selection without constructing
subject/native consumers. Canonical materialization is a separate Store
increment: this caller permits its learned, eligible current-branch results
to materialize on a follower without granting that follower execution authority.
Local storage and malformed durable-state errors remain failures.

Candidate willingness refreshes every five seconds and expires locally after
30 seconds. A changed or expired observation forces a fresh agreement; the
stored certificate alone cannot keep a dead coordinator selected indefinitely.
The runtime advertises only a configured model binding. Optional supplied
preference ranks retain their model-evidence digest, while the default uses
stable Node identity as the coordination tie break. No inference success or
comparative intelligence is fabricated from configuration, model name or OS.

## Verification observed

One new integration journey,
`minority_agreement_survives_reopen_and_supersession_fences_original_branch`,
constructs a real five-member configuration through durable Joint and Final
membership transitions, then exercises a two-member minority agreement. It
checks incomplete/asymmetric view refusal, refusal of partial certificates,
fencing of the preceding singleton origin at ACK, reopen and exact commit
replay, coordinator-only branch opening, superseded-certificate refusal,
global-term invalidation, and retention of original certificate/branch facts.
It uses real SQLite Node state. It does not emulate a successful remote model
or claim a production SSH journey.

The first `cargo check --offline --locked -j1 -p shenko-node -p shenko`
attempt found two connection/transaction helper type errors; both were fixed.
The following targeted Node library test build was killed by the operating
system with `SIGKILL` during rustc code generation. **Zero tests ran.** The
journey was then moved to its own integration-test target to avoid compiling
the large Node unit-test module. Further code generation was deferred while
the shared host ran the real-model acceptance worker; this document claims
neither a final compilation pass nor a passing journey from that failed build.

The intended focused command for the combined candidate is:

```bash
cargo test --offline --locked -j1 -p shenko-node --test component_agreement -- --nocapture
```

Build attempts use `/home/agent/.cargo/bin` on `PATH`,
`CARGO_TARGET_DIR=/workspace/scratch/d013f3e3b824/shenko-target`,
`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`,
`CARGO_PROFILE_TEST_DEBUG=0`, and the shared `/tmp/shenko-cargo.lock`.
Formatting/document checks are structural evidence only.

All changed Rust files parsed through `rustfmt`; the three new Rust files
passed its formatting check, and `git diff --check` passed. The initial
`python3 scripts/check_docs.py` run could not finish its symbol/CSS checks
because that interpreter lacked `scripts/requirements-ci.txt` parser packages.
These results do not substitute for compiling or executing the protocol.

## Remaining acceptance and implementation boundaries

- The protocol currently requires reciprocal configured authenticated routes.
  Transitive-only and asymmetric network components remain waiting views;
  this is not complete discovery/routing support for every connected topology.
- Configuration and a live process observation do not prove a usable model.
  Autonomous measured model readiness, comparison and recursive best-model
  takeover remain separate product work.
- A resident follower catches up control and eligible canonical Store results;
  continuous complete cognition/receipt reconciliation and handoff need their
  own real service journeys and retained provenance checks.
- The new Node journey and production Start/SSH cancellation/takeover behavior
  still require successful execution on the final combined candidate.
- Physical partitions, delayed external results, multi-device service failures
  and long-run observation remain unverified. No physical exactly-once effect
  or global single activity is claimed under network partition.
