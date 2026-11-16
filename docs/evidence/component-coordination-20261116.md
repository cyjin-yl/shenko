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

Resident process observations refresh every five seconds and expire locally after
30 seconds. A changed or expired observation forces a fresh agreement; the
stored certificate alone cannot keep a dead coordinator selected indefinitely.
Global follower selection likewise checks this fresh observation instead of
treating an answering one-shot SSH command as a living primary process. Model
candidacy remains optional and separate from process presence: the runtime
advertises only a configured model binding as a candidate. Optional supplied
preference ranks retain their model-evidence digest, while the default uses
stable Node identity as the coordination tie break. No inference success or
comparative intelligence is fabricated from configuration, model name or OS.
Registration binds a new owned runtime once; heartbeat renewal compares that
runtime ID under the same Node transaction. A delayed old blocking heartbeat
cannot replace the newer process observation.
The selected runtime also returns its heartbeat task to the owned Start
lifecycle. Every Start exit cancels and joins it before runtime/owner teardown;
selection failures and cancellation join it inside the selection loop.
An observation write failure, superseded process, or worker failure cancels its
owned runtime immediately and retains the original error through that join.
Durable authority reservations do not treat presence as a lease; explicit local
cancellation stops fresh admissions while preserving already admitted facts.
An observation failure remains a runtime failure even if concurrent authority
recovery was requested, rather than being hidden by an automatic rebuild.

The follow-up connects the Store's `PreparedStepRecovery` API from
`6769353`, `d4158ec` and `2ba90aa` before controller activation. A follower
only projects learned results. A selected source resumes the same prepared
descriptor; a temporary authority refusal returns to resident selection, and
an incompatible predecessor retains typed chosen/local debt. A blocked
component coordinator durably retires its exact certificate and negotiates a
new component branch. A former component that becomes isolated cannot reuse
the old component branch as its new singleton episode. Unrelated branch
adoption/rejoin also invalidates the old component projection.

## Verification observed

One new integration journey,
`minority_agreement_survives_reopen_and_supersession_fences_original_branch`,
constructs a real five-member configuration through durable Joint and Final
membership transitions, then exercises a two-member minority agreement. It
checks incomplete/asymmetric view refusal, refusal of partial certificates,
fencing of the preceding singleton origin at ACK, reopen and exact commit
replay, coordinator-only branch opening, superseded-certificate refusal,
explicit retirement and fresh agreement, global-term invalidation, and retention
of original certificate/branch facts.
It uses real SQLite Node state. It does not emulate a successful remote model
or claim a production SSH journey.

The first `cargo check --offline --locked -j1 -p shenko-node -p shenko`
attempt found two connection/transaction helper type errors; both were fixed.
The following targeted Node library test build was killed by the operating
system with `SIGKILL` during rustc code generation. **Zero tests ran.** The
journey was then moved to its own integration-test target to avoid compiling
the large Node unit-test module. Further code generation was deferred while
the shared host ran the real-model acceptance worker; this document claims
neither a final compilation pass nor a passing journey from that failed local
build. The later remote execution below is separate evidence.

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
The subsequent run using
`/workspace/scratch/d013f3e3b824/poppy-docs-venv/bin/python` passed with
`errors: []` and `product_tested: false`. These results do not substitute for
compiling or executing the protocol.

### Observed remote candidate execution

The combined candidate `b2886c2b9b10d68ffb27d5b0702a896419f1ced6`
ran in [CI 43860564971](https://github.com/cyjin-yl/shenko/actions/runs/43860564971)
and [Nightly 43860596861](https://github.com/cyjin-yl/shenko/actions/runs/43860596861).
The CI checkout used the PR merge commit
`adf05188be9c4768a5f46ab94f1332079a5c274a`; GitHub's comparison from the
candidate to that merge returned no changed files. This is evidence for that
source tree, not an earlier successful main branch.

The CI workspace check and workspace test steps completed successfully. The
Rust job subsequently failed in its explicitly invoked SSH WASI journey after
the pinned guest build succeeded. That journey returned `No such file or
directory (os error 2)` before any SSH exchange, with zero passing tests and
one failed test; its relative artifact path was resolved from Cargo's test
working directory. This does not supply SSH execution evidence. The
Nightly `nightly-rust-long` job (`133276074128`) completed successfully;
its downloaded original log explicitly reports these tests as `ok`:

- `minority_agreement_survives_reopen_and_supersession_fences_original_branch`
  — one executed test, none ignored, 0.23 seconds.
- `canonical_s1_recovers_identical_results_on_a_distinct_ledger_without_dispatch`
  and `canonical_s1_conflicting_readset_and_partial_sql_fault_never_publish_a_receipt`.
- `reserved_store_commit_and_ancestor_cancellation_fence_live_owners` and
  `chosen_unmaterialized_action_reopens_on_a_real_branch_with_original_unknown`.

The same Nightly job explicitly ran the existing `partition_orchestrator`,
native helper lost-reply recovery, and helper identity-refusal fixtures after
the workspace suite. All three report `ok`. The retained partition receipt
reports `passed`, all 17 checks true, and no failed checks. Its own stated scope
is independent OS processes sharing one quiesced origin, real parser/Store/CAS
code and archive exchange; it sends no model, provider or network request and
does not prove physical partition or power-loss behavior. Both helper result
receipts report `passed`, and their preparation identifies the exact candidate
above.

The `nightly-native-evidence` artifact is `14224410491`, with verified ZIP SHA-256
`100763bc0f67eea0b52b1f3c23ddb68e0e03841432757ef5770401772bbd3a8b`.
The CI Clippy job still failed on that candidate: its retained comparison names
one new ignored SSH fixture, a WASM select-macro unused binding, a subject test's
macro-generated `expect`, and one Store test's `unwrap_err` (five errors across
the production/all-target scopes). It reports no new component or heartbeat
warning. Those diagnostics are separate from the passing tests; subsequent
fixes require their own candidate result. No all-CI-green claim is made here.

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
- The new Node journey passed on the named combined candidate above. Production
  Start/SSH cancellation and takeover behavior still need their corresponding
  real-service journey; the protocol test does not supply that evidence.
- Physical partitions, delayed external results, multi-device service failures
  and long-run observation remain unverified. No physical exactly-once effect
  or global single activity is claimed under network partition.
