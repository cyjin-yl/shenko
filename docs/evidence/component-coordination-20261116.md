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

- The original protocol required every participant to contact every other
  participant. The star-topology continuation below allows followers without
  a direct route to each other. The preferred coordinator still needs direct
  reciprocal configured authenticated routes to every participant. A preferred
  leaf requiring relay, longer chains and asymmetric routes remain waiting
  views; this is not complete discovery/routing support for every topology.
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

## Real Start and pinned SSH continuation

The isolated continuation from `09a235e` adds
[`component_process.rs`](../../bin/shenko/tests/component_process.rs), one
continuous process journey. It shares the existing SSH fixture's bounded
owned-process and OpenSSH provisioning helpers; the named SSH WASI journey
keeps its original protocol assertions. No Observe response, certificate,
member identity, Store fork or admission result is supplied by a fixture closure.

The runner creates seven separate Node/Store installations and authenticates
their actual IDs over seven real loopback OpenSSH servers. Each source has a
distinct private client key and each destination a pinned host key. Private
configuration launchers exec the system `ssh` and exact candidate CLI without
altering their protocol input/output. No ambient account, provider or channel
configuration is inherited. The journey uses public `sync enroll`, `sync push`,
`sync adopt`, Joint/Final membership commands and the failed-election branch
path. It retains one verified source genesis while A and C obtain independent
local ledgers, homes and explicit availability branches. This starts new branch
work; it does not take over imported Running tasks.

The public bootstrap chain exposed a production identity defect before this
journey was executed: `sync receive` prepared a receiver home using the receiver
ID, but later Start resolved the imported immutable bootstrap to the source
installation ID and rejected that receiver home. The local provisioning change
above fixes that distinction while retaining the original bootstrap event,
source origin and strict home/path admission. Neither a manifest's arbitrary
ID nor a peer's source receipt can create the local provisioning row. The
journey checks that peer import does not copy this row or make a subject local,
and that the legitimate fork retains the original genesis and local home ID.

After the initial setup, accepted source keys enforce only A↔B and B↔C;
A↔C authentication fails, and D through G's owned SSH servers stop. Three
independent, owned Start processes run against the seven-member electorate.
A/C have no configured model. B has a configured but explicitly **unprobed**
binding to an owned socket that never serves model responses, plus a real
`--pause-at-start` boundary. The socket must receive no connection, and all
three stores must retain zero inference/external attempts. This verifies no
model capability, intelligence comparison or physical network partition.

The success conditions require the actual three-member certificate and full
observed neighbourhoods, B's agreed paused Store branch and authority binding,
and both live followers' absence of consumers/authority binding. A real follower
attempt to open B's branch must fail without changing its Store head/attempts.
Each resident must renew the same persisted runtime ID, then all owned Start
leaders must exit successfully on SIGINT and every owned SSH listener must
close. Bounded process-group signalling and leader joins are retained; these
observations do not prove arbitrary escaped descendants have stopped.

The execution budget is 900 seconds, each one-shot CLI is limited to 180 seconds,
and each Start readiness wait to 180 seconds within that work budget. Teardown
separately allows each owned Start 20 seconds and each SSH server 3 seconds,
followed by a bounded SIGKILL/join fallback that makes the journey fail. Logs,
binary SHA-256, actual public argv, SSH pins/topology, identity/genesis/home
observations, certificate, process joins and failure result survive cleanup.
Private client keys, owner controls, input files and installation databases
are not copied to the artifact. Both normal CI and Nightly already provision
OpenSSH; their workspace suite includes this ordinary test target and additionally
retain `.ci/evidence/component-process` on success or failure.

Focused entry point, after the same OpenSSH prerequisites as those workflows:

```bash
cargo test --locked -p shenko --test component_process -- --nocapture
```

At this change's preparation, **the new real process journey has not run**.
The final metadata command
`cargo check --offline --locked -j1 -p shenko -p shenko-store -p shenko-seed --tests`
passed in 1 minute 33 seconds under the shared lock and target. It covers the
new process target and the reused seed capability-migration and Store bootstrap
identity journeys, including local provenance not hiding source identity
conflicts. It executes none of them. Normal documentation checking passed with
`errors: []`. Migration conservation initially reported the existing
`SPEC:41.4` mismatch on the `09a235e` base; applying the already integrated
documentation-only `d690647` fix moved its implementation note outside the
immutable block, and `--verify-migration` then passed with `errors: []`.
Both document reports explicitly state `product_tested: false`; no manifest or
checksum baseline changed. New Rust files passed formatting checks and the
diff passed whitespace checks. Targeted Clippy metadata for `component_process`
initially reported seven new loop/conditional diagnostics. They were corrected
without changing the scenario or assertions; the same target then completed in
1.20 seconds without a diagnostic in the new test. Existing library warnings
remain, and this local command is not the remote full ratchet.
Existing Node protocol successes above do not stand
in for this new CLI/SSH observation. Hosted-runner execution, coordinator loss,
physical multi-device faults, actual model readiness and long-run acceptance
remain separate evidence obligations.

## Star-topology continuation

The isolated `feat/component-star-topology-20261116` increment starts from
`a3739f2e581b25e3ca841c8402b3cc118fdeb222`. It addresses a concrete unsupported
minority: among seven configured voters, three residents form a reciprocal
star, the preferred candidate reaches both followers, and the followers cannot
directly reach one another. The old equality between each direct neighbour
set and the entire component rejected that connected view.

`ComponentProposal::validate` now preserves each sorted complete neighbour
set, requires the member set to be closed over every observation, checks
reciprocal edges, and requires the existing selected candidate to directly
cover the full component. Candidate rank and the stable identity tie break are
unchanged. `prepare_component` still compares the endpoint's independently
probed view with its exact proposal observation before the durable ACK; the
global quorum and all-member certificate requirements do not change.

The production confirmation adapter uses `ComponentObserve` to recheck each
contacted member's complete local view, certificate and runtime observation.
The coordinator contacts every member before new Store admission. A follower
contacts its own neighbours, including the coordinator, only to keep following;
it cannot turn this local confirmation into branch or execution authority.
Unknown responses remain refusals. A new outside neighbour or a changed local
view prevents using the old agreement, while original certificates, branches
and external results stay retained. Final local reservations remain in force.

The existing Node agreement journey is extended to a real seven-voter
Joint/Final configuration and a three-member star, with one model-free resident.
It retains incomplete-view/partial-certificate refusal, ACK fencing, reopening,
exact replay, coordinator-only branch opening, supersession, retirement and
history checks. The same proposal also checks closure and refusal when a
supplied preference chooses a leaf that cannot directly confirm all members.
No new test function, ignored path or topology matrix is added. These are
SQLite Node protocol observations; they do not manufacture an SSH transport
or successful model response.

Local verification for this increment used the same locked offline toolchain,
shared Cargo target and `/tmp/shenko-cargo.lock` described above:

- `cargo check -p shenko-node --test component_agreement --offline --locked -j1`
  completed successfully in 14.71 seconds, including the changed journey's
  compilation metadata.
- `cargo check -p shenko --lib --offline --locked -j1` completed successfully
  in 57.33 seconds, covering the production confirmation adapter.
- The documentation checker returned `errors: []` and
  `product_tested: false`. `git diff --check` and formatting checks on the
  changed Node source and journey passed.

These are compilation and structural checks, not an executed star journey.
The changed test was not run locally. Earlier passing two-member Node and
archive-exchange journeys do not prove this changed view validation or the
real Start/SSH adapter. A real multi-node route/startup journey, relay support
and asymmetric topology handling remain outstanding.

### Later 9b5a6cb process observation

[CI 43894092386](https://github.com/cyjin-yl/shenko/actions/runs/43894092386)
actually passed the reciprocal-star Node protocol, but the new seven-SSH-identity
process journey failed in 69.02 seconds during its membership setup. It had not
reached the three resident Start processes. The two observed configurations
matched on revision, term, members, joint configuration and digest; only their
node-local bounded-Append learning times differed. The original failure, full
tuples, real transfer frontiers, cleanup and complete logs remain in
[the source-bound observation](ci-9b5a6cb-20261116/README.md).

The follow-up fixture compares those consensus fields and complete control
frontiers across members, then reopens each Node through another CLI process
and requires its own full configuration, including its local time, to remain
identical. It preserves every later certificate, installation, follower,
authority and heartbeat assertion. Its metadata compilation passed on
`c1c915fa`; the corrected whole process journey has not yet run.
