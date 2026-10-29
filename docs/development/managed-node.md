# Unified Node and native recovery / 统一节点与原生恢复

## Purpose and limits / 用途与边界

`bin/shenko` links the Rust 2024 `shenko-node` library. One Node identifies an
installation, not a device class: there is no PhoneNode or ServerNode.
`primary`, `executor`, `replica` and `recovery` are mutable roles. A persisted
role assignment is intent; a separately timestamped observation records the
roles started by an actual runtime. Neither is proof of current liveness,
model access, election authority or completed external work.

The earlier standalone `shenko-node` binary and C07 managed-installation journey
below exercise the pure `node.blake3.v1` task contract (at most 64KiB, no external
effect). That bounded contract does not make arbitrary effects transferable.
The main-app path additionally handles installable units and cognition recovery;
it is not a replacement subject or a complete ElectionProvider.

这是 #71 的跨包接线，不是完整 C17/A42/A47/A48/A49。SSH 是实际 OpenSSH，不冒称
WASM。不同内核的源/接收端仍在同一 PVE 故障域，不等于物理两机容灾。没有真实可用
模型时不声明智能资格；恢复和导出本身不依赖模型。

## Main-app control and replication / 主应用控制与复制

Use the real application's data directory, not the standalone adapter's private
directory. The Node identity is bound to the Store's actual subject genesis and
branch. An evolved conflicting identity is retained for reconciliation rather
than silently overwritten.

```sh
shenko --data-dir DATA node < NODE_REQUEST.json
shenko --data-dir DATA sync identity
shenko --data-dir DATA sync enroll SSH_HOST \
  --remote-data-dir REMOTE_DATA --remote-binary REMOTE_SHENKO
shenko --data-dir DATA sync push
shenko --data-dir DATA sync status
shenko --data-dir DATA sync sources
shenko --data-dir DATA sync recover-source SOURCE_ID --output NEW_DATABASE
```

`node` consumes a bounded JSON request; its role and unit commands use the same
Node, not device-specific services. `sync enroll` queries the peer's actual Node
ID over the already-authorized strict-host-key SSH connection. `sync push` sends
the recovery closure and installation packages and requires an identity-,
fingerprint- and high-water-bound verified receipt. Interrupted or rejected
transfers retain debt and exit unsuccessfully. `start` also reconciles configured
peers every 15 seconds independently of inference; one SSH exchange is bounded
to 120 seconds. An already-confirmed unchanged closure needs no new transfer;
its historical ACK is explicitly not a fresh peer-liveness probe.

`sync export` writes the framed transfer to stdout; `sync receive` reads stdin
and returns a receipt without dispatching imported work. `sync ack` accepts only
the exact previously exported and completely verified closure. Workspace paths
are opt-in (`--workspace` at export/enrollment and an explicit receiving root),
not a prerequisite for cognition recovery. Protect transfer bytes as private
data, even when the transport is already authenticated.

### Verified capability facts and coordination candidacy

Every transfer now carries a bounded `NodeCapabilityProfile` advertisement in
both directions: the `shenko-node-sync/2` header names the sender's real Node
ID, self identity, running artifact manifest, persisted role assignment
(intent, never liveness), latest runtime observation, the count of
observations that ever carried a bound subject, and the capability names
declared by currently enabled units. The verified receipt carries the
receiver's profile; `sync identity` also returns its profile so `enroll`
stores the same fact before any transfer. Mixed-version transports fail
closed at the framing/schema boundary; an older peer that omits the fact at
enrollment leaves that peer with no capability fact rather than a fabricated
one.

Advertisements carry a per-node strictly increasing `advertisement_seq` that
advances only on content change. `record_peer_capability` keeps at most the
newest verified fact per source: an older generation is `stale`, an identical
replay is `duplicate`, different bytes at the same generation are a retained
`conflict`, and every arrival is appended to the observation log with its
provenance. Facts are only ever bound to the Node ID inside the
authenticated/verified channel itself; a profile naming a different Node is
rejected.

`sync identity`/`sync status` and the `coordination` Node request return a
deterministic candidacy ordering of the known member set (self plus enrolled
sync peers): observed-primary evidence, then any bound-subject evidence, then
verified reachability of the recovery closure (confirmed sync or local
durability), then newest advertisement generation, then node ID. It is
explicitly *not* an election — no member configuration, no quorum, no
fencing, no liveness — and a partitioned component must still run the §5.4.1
protocol before takeover. It supplies what that protocol may consider:
versioned capability facts and actual evidence, not names, static parameter
counts or cached claims.

### Installation units and local readiness

Unit transfer schema 3 carries `perception`, `awareness`, `skill` and `code` packages:
identity, revision, exact artifact bytes/digest, declared configuration,
applicability, required/recommended dependencies, a digest-bound root,
digest-specific selected edges and safe trial provenance. Exported schema-3
root identity does not depend on package ordering; an authored schema-3 closure
without a root version must contain only one version of the root identity.
Both exact and capability/topic choices retain the
selected revision and digest. Imports retain an existing satisfied selection;
conflicting transported pins fail instead of silently replacing either choice.
A later matching import re-resolves unresolved/omitted edges; if a transported
pin's target is missing, only that exact target can complete the selection.
If the retained target no longer satisfies the declared selector, the pin is
not silently replaced and the required closure remains incomplete.
Transfer fails rather than silently truncating a closure beyond the package bound.
Materializations of different digests use different paths. Import responses
report offered and retained edges and the resulting closure for every version;
these are installation facts, not runtime activation.

Node storage schema 4 validates the complete old schema fingerprint and
installation identity before its transactional migration. It retains the old v3
`unit_edges` rows unchanged in `unit_edges_legacy`: where one digest alone has
the old id/revision and the edge matches its descriptor, that choice is retained;
multiple digests, absent rows or mismatched declarations are explicitly
`legacy_ambiguous` and never gain an inferred selection. A required ambiguous
edge prevents a complete required closure; a recommended one remains visible.
Reimporting the same version retains the previous serialized version record in
the local `unit_version_history` table before updating its current record;
changed digest-bound edge records are retained in local `unit_edge_history`.
Unrelated imports do not clear the ambiguity. To make a *new* decision without
changing the current version, issue a Node `units` request whose nested
`command` is `resolve_version`, with the exact `unit_id`, `revision` and
`artifact_digest`, after making the desired
dependencies available. This keeps satisfied selections pinned and leaves an
ambiguous edge ambiguous if no candidate can be selected. Alternatively,
import the exact package with schema 3 and without `resolved_dependencies` for
that package; this can change the current version. Importing that exact version
with an explicit schema-3 source selection is a new choice from that source, not proof
of the old local choice. Neither operation claims the old selection was
recovered. The original legacy rows remain available as evidence.

New receivers accept schema-2 transfers, but schema 2 did not carry selected
edges and cannot prove their source-version ownership. Such edges remain
`legacy_ambiguous` unless an already retained local pin applies, and the import
response distinguishes the offered unknown from the retained local choice.
Schema-3 exports carry these unknowns as unknown rather than silently resolving
them at the next receiver. Old schema-2 receivers reject schema-3 exports
(including their new fields), so they cannot acknowledge a closure whose
selections they cannot preserve; upgrade the receiver before transferring it.

Copied/enabled is not ready. Readiness requires the currently materialized bytes,
a successful local artifact-bound trial in the current observed environment,
and the recursively satisfied exact required dependency closure. A successful
environment investigation alone or a foreign node's trial cannot prove local
readiness. Changed bytes and unresolved, failed or unknown attempts remain
visible. Unknown/foreign OS, arch or runtime labels are advisory: the caller may
investigate and try the actual artifact rather than receive a static denial.

Trials record risk, expected benefit and consequences for autonomous judgment;
there is no new human approval or product permission allowlist. Actual OS access
and missing programs still produce real failures. Time/output/package limits
bound resources, not product authority. A deadline signals the process group;
escaped-descendant uncertainty is retained instead of claiming universal kill.

Local raw trial arguments, output, paths and environment probes remain local.
Only typed `TrialProvenance` travels. No environment credential or provider grant
is discovered for export. Authored package bytes/configuration are copied
exactly, however: a secret explicitly authored there is part of that package,
not silently filtered by key name. This is not a credential-synchronization
policy or a claim that installing a package activates every Cordis plugin.

### Cognition recovery closure and debt

The Store snapshot includes durable experiences/raw sessions, decisions, identity
revisions, goals, tasks, inbox/outbox facts, execution checkpoints and the
recursively referenced CAS bodies. Memory working-tree files and actual Git
HEAD/history are included; independent source refs are retained under a
source-specific namespace. Deletions apply only against the known source
baseline and preserve prior bytes in a registered retirement directory.
Receiver divergence is retained and prevents a complete-recovery ACK.

The declared `napcat-ingress.sqlite` is captured through an online read-only
SQLite backup, including committed WAL contents, pending raw events,
observations, cursor and gaps. Its chunked CAS root is part of the closure.
`sync sources` lists it; `recover-source` verifies and extracts a new SQLite
path without activating a provider or replaying an input. This does not scan
arbitrary provider databases, private environment files or credential stores.

Manifest schema 3 binds source ledger/high water, durable snapshot order,
fingerprint, object descriptors, file origins and durable memory-intent
bindings. Schema 2 remains readable when its pending memory identities are
recoverable; an older schema-2-only receiver must be upgraded to receive a
schema-3 archive. Missing binding identity, unknown fact tables, stale order,
missing/corrupt retained CAS and forged fingerprints block verified recovery.
An intent, its binding and associated memory entry reconcile as one conflict
unit; a conflicting source cannot publish only its entry or attach its
reference to a retained locator-only intent.
Imported S1 history keeps original ledger/commit references, while local replay
uses its own sequence and a step-order cursor so an imported same-sequence page
cannot reorder or skip committed steps. Imported unknown effects are not
automatically redispatched.

`sync status` separates `local_durable`, peer ACK watermarks, `unsent_changes`,
`unverified_transfer` and `verified_recoverable`. Exported but unacknowledged
changes may have `unsent_changes=false` while `unverified_transfer=true`; they
are not protected merely because export finished. No configured peer means
unprotected. Offline availability remains; global strong consistency and zero
loss are not claimed.

Source/artifact-bound observations, failures and remaining scope are recorded in
[the #71 evidence](../evidence/node-sync-cognition-20261026.json) and
[C17](../tasks/C17.md). Full election, partition authority, every platform,
arbitrary-effect fencing, encrypted disk-image recovery and 7/30-day observations
remain separate acceptance obligations.

## Standalone journey authority / 独立旅程权限来源

The control CLI requires a private OS account/data directory or its already
authorized strict-host-key SSH channel. `register`, `join`, `create`, `transfer`
and `accept` are trusted installation/controller operations. Do not expose their
JSON directly to chat/model arguments or an unauthenticated network endpoint.
`from_node` must be supplied by that authenticated transport, never a peer's
unverified message. The eventual registry/IPC adapter must derive invocation
and source identity itself; it is not implemented here.

`Identity.origin_proof` references an **already verified** earlier self supplied
by provisioning. It does not discover trust or prove age by itself. Prepare
checks that this identity equals the actual controller store. First adoption
archives a virgin independent identity; an already joined/evolved branch is
rejected for explicit merge, never silently erased. No competing subject loop
or daemon is started. Provider settings, private keys, model weights and secret
bytes are absent from the node synchronization schema.

## Build and inspect / 构建与确认实际工件

Use a clean product commit and the shared build budget in the managed container.
For a standalone caller, the equivalent native command is

```sh
SHENKO_SOURCE_REVISION=$(git rev-parse HEAD) cargo build -p shenko-node
/path/to/shenko-node --manifest
```

Record the actual source SHA used by the build. `--manifest` binds the running
executable's blake3 digest, compile-time source, OS, architecture and schema 1.
If stripping/copying it, inspect the final executable again. Unbound source or
mismatched bytes/platform/schema cannot join. This is a builder provenance
contract, not remote attestation or a substitute for verifying the source tree.

## Standalone pure-task wire / 独立纯任务控制协议

`shenko-node PRIVATE_DATA_DIR` reads one bounded JSON request on stdin and returns
one JSON response after committed work. Existing unrelated data is rejected;
never use a live main-app directory. Storage schema **5** is separate from
record/wire schema **1**. Existing databases first undergo an immutable/read-only
application/schema/installation preflight: even WAL sidecar creation or journal
recovery is not allowed for an unknown database. If an interrupted DELETE-mode
migration makes that view inconsistent, Node serializes cooperating initializers,
captures the database and optional journal under SQLite's own exclusive lock,
then lets SQLite recover only a private copy before repeating the full fingerprint
and identity check. Staged recovery refuses WAL, extension loading and paths
outside its temporary files, including a referenced super-journal; it never
promotes that copy over the original. A second content/presence check precedes
ordinary SQLite recovery and locked migration of an admitted original. The lock
handoff to the original pager is not atomic against a hostile actor replacing
files in the private data directory; this does not add inode confinement. The
exceptional recovery path does not process WAL/SHM sidecars. Admission requires
the main database and any rollback journal to be separate single-link regular
files, routes any `-journal`/`-wal`/`-shm` sidecar through the confined staged
preflight so a
crafted super-journal name cannot direct ordinary recovery outside the private
directory, and tolerates a journal removed by a concurrent normal commit rather
than rejecting a healthy installation. The fingerprint rejects any trigger or
view by case-insensitive object type: SQLite reserves the `sqlite_` name prefix
and compares schema types case-insensitively, so a name-filtered, lower-case
comparison alone could hide one. Single-link checks cover ordinary hardlink
aliases; bind-mount aliases of the same inode, reflink/snapshot clones and
hostile replacements inside the private data directory are not detected and
remain outside this admission boundary. WAL/SHM presence is refused rather
than silently discarded.
Then the writable
open rechecks ownership before persistent PRAGMAs. New files use exclusive
creation and 0600; unrelated directory entries and symlinks are rejected.

The only unmarked legacy format eligible for migration is the exact original v1
schema (all non-SQLite schema objects) with valid durable node/ledger/identity
metadata. Its rows, node/self IDs, ledger and seq remain unchanged. One migration
transaction adds the installation UUID, SHNK application_id, receipts table and
the current storage version. Role/unit/peer/capability tables are added without replacing
old facts. This is a trusted local format migration, not cryptographic
proof of remote ownership. Unknown, corrupt or incomplete uninitialized databases
fail closed; this adapter does not overwrite them or repair arbitrary data. New counters on the wire are canonical
unsigned decimal strings. Node UUIDs, agent/branch UUIDs and task UUIDs are distinct.

- `initialize`: stable `node_id`, optional provisioning `identity`; reopen never
  creates or resets an identity. `join`: stable `operation_id`, `expected_node`,
  verified `identity` and actual `manifest`; payload mismatch is a conflict.
- `status`: optional join `operation_id`; returns the old immutable join receipt
  separately from a fresh `running_manifest`, observation time and capabilities.
- `capabilities`: this Node's current self-declared advertisement;
  `capability_facts`: newest verified fact per known source; `coordination`:
  the deterministic candidacy ordering above (`is_election=false`).
- `register`: immutable `node_id`, `target_id`, public-trust/binding digest.
- `create`: stable `task_id`, registered `node_id`, bounded `input` byte array;
  returns the same ticket on duplicate input and rejects conflicting creation.
- `execute`: `ticket` and exact `input`; arm before compute, then persist one
  original result/event. Duplicate tickets query; an armed-without-result record
  stays unknown. `query`: original `task_id` and string `epoch`, no redispatch.
- `transfer`: `task_id`, expected string record revision/epoch and registered
  `to_node`; this pure contract alone allows explicit reassignment. It increments
  execution epoch and semantic/record revisions without changing TaskRecord owner.
- `accept`: authenticated `from_node` and original `receipt`. Import stable
  source/event/digest facts, but only the current ticket may create one terminal
  result. Late/stale results remain evidence, not new authority. For authenticated,
  known-schema receipts, E1 commits the full decoded receipt and source event
  **before** task lookup or settlement predicates. Missing-task receipts become
  `unassociated`; transcript/contract/predicate rejections become `invalid`.
  A failed T2 cannot roll back E1; pending receipts remain available for explicit
  reconciliation. Authentication/schema/digest failures are still rejected before
  E1 by this typed adapter; it is not the complete unknown-schema EvidenceIngress.
- `export`: string `after` cursor and `limit` 1..128; native-journey raw events,
  independent of any LLM. `export_receipts` has its own cursor and returns full
  canonical decoded receipts, authenticated provenance and current classification,
  including unassociated/invalid evidence. Its cursor enumerates receipt arrival,
  **not** subsequent classification changes; it is not a resumable status watch.
  Neither command is the complete main-app history exporter. Explicit reaccept
  of the same receipt may reconcile after the task arrives; duplicates never
  grant an old invocation control or settle a terminal task again.

Objects are bounded and stored in the **same SQLite transaction** as references,
so this adapter has no separate filesystem CAS publication window. Assignments,
original work receipts, source events and conflict observations are retained.
The experimental profile uses DELETE journal and read-back FULL/foreign_keys;
it claims process-crash continuity only. It does not claim the SPEC default
WAL/power-loss profile or upgrade the baseline SQLite 3.46.0 WAL guarantees.

## SSH deployment / 部署与断线恢复

`scripts/managed_node.py` consumes an **already persisted trusted C07 parent
TaskInvocation**, not a fabricated production admission. Its config names
`artifact`, `controller_binary`, `controller_data`, `node_id`, `remote_root`,
`identity`, `ssh_driver`, and `parent` (the existing `task_ssh` JSON including
target/receipt-root/context). Key **paths/handles** stay in the local private
plan, never put credentials or these live configs in the repository.

```sh
python3 -I scripts/managed_node.py prepare NEW_PRIVATE_PLAN CONFIG_JSON
python3 -I scripts/managed_node.py deploy PRIVATE_PLAN
python3 -I scripts/managed_node.py query PRIVATE_PLAN install
python3 -I scripts/managed_node.py call PRIVATE_PLAN STABLE_LABEL REQUEST_JSON
```

Prepare creates a schema-2 plan freezing the artifact, deployment UUID, installer
(including C07's shared atomic writer), root-creation implementation and
`staging_root=remote_root/staging-<deployment_id>`. Every upload's destination
is therefore part of the frozen C07 input/resource binding. Two plans sharing a
registered target/root cannot overwrite each other's chunks. Existing plan
retries keep their original paths, code, operation IDs and bytes; changed inputs
require a new explicit plan/operation, never a reseal under an old ID.

Each upload/install/join stage has a stable derived operation ID and logical key.
Installation verifies SHA256, writes and syncs private temporary artifact/receipt
bytes, validates the receipt, then atomically publishes with a no-clobber hard
link and syncs the directory. The receipt preparation reuses C07 `write_atomic`;
its replace semantics are confined to a unique private temporary name. A final
identity-bound name is never overwritten. This is native POSIX/Python3 managed
writing, not a generic cross-platform framework or power-loss test. Temporary
and damaged files remain evidence; existence alone is not a durability marker.

A retry queries existing C07 operations; changed inputs conflict. Transport loss
means unknown, not failure/permission to install again. Query the original
plan/label; if its receipt remains unknown, retain that responsibility rather
than generate another plan to bypass uncertainty. Query uses C07's CAS-sealed
original operation/trust, not the current local `known_hosts` file. Removal or an
unrelated edit to that pathname cannot imply that the remote effect never
happened. New admissions still require the original registered trust digest;
CAS trust still strictly rejects a changed real host key, and remote key
revocation still denies authentication. Local pathname removal is not a
revocation API; authenticated revocation/fencing must occur at the real trust or
authorization boundary. A fresh read-only status uses its own observation
operation, not an old cached reply.

### Existing plans and explicit repair

Schema-1 plans remain queryable with their original deployment/operation identity;
this adapter does not silently execute a legacy staging/installer fallback.
Preserve their original reader/source and all raw receipts. Both `deploy` and
`call` reject such a plan before any task read, request write or SSH execution;
the shared non-query admission boundary also covers direct `invoke`/`node_call`.
It never rebuilds an old sealed operation with new code.

A malformed or conflicting final receipt/artifact is **not** durable success and
is not overwritten, renamed away or deleted by installation. Reconcile the old
C07 operation first. If an explicit repair is needed, create a new config from
the original authorized binding, add `repair_of_plan` pointing to the retained
old plan directory, and `prepare NEW_REPAIR_PLAN CONFIG_JSON`. The new plan records
the original deployment ID and exact plan hash, validates the same target/root,
node and self, and allocates distinct staging/artifact/receipt and C07 operation
IDs. Then deploy that new plan. It does not mutate the old seal or pretend the
old unknown operation never occurred. This lineage is plan-local evidence, not
a claim of full production `OperationRecord.compensates` integration.

## Reproduce the two journeys / 两条真实隔离组合场景

Build the existing `task_ssh` example, then run

```sh
python3 -I crates/shenko-node/tests/managed_journey.py \
  REPO NODE_BINARY SSH_DRIVER SSHD LIB_DIR NEW_PRIVATE_ROOT [V1_DATA_DIR]
```

The fixture requires a native Linux container, root account, OpenSSH server/keygen
and Python3. It creates only a new loopback sshd/proxy with generated test keys;
its temporary authorized-key fixture uses `StrictModes no`. This does not alter
production sshd configuration. The test-only C07 `fixture-init` parent and
identity anchor are synthetic and explicitly not main-app integration.

1. Interleave different artifacts A/B on one registered target/root; resume A
   after B stages, and later install B without overwriting A. Actually close SSH
   only after decoding the complete install **and C07** receipts and checking the
   artifact digest. Query the original operation with missing/unrelated local
   trust, while rejecting new admissions and actual remote key revocation/host
   replacement; resume adoption and repeat without another mutating attempt.
   The same installation journey now also refuses an unrelated WAL database
   without changing bytes/directory entries. If V1_DATA_DIR is supplied, SQLite
   backup into a new directory verifies migration preserves every old table row;
   the original fixture is read-only, never migrated in place.
   An old-format plan projection rejects deploy/call with zero fresh attempts,
   queries its original CAS install twice, then explicitly prepares and deploys a
   distinct schema-2 repair recording predecessor ID/hash. Two repair queries
   retain one install attempt; predecessor plan bytes stay unchanged. This checks
   the schema-1 format boundary, not an original old-executable installation.
2. Query the original durable task result, transfer the pure assignment, reject
   old-owner settlement, execute on the new owner and accept once. Preserve both
   raw sources after separate process reopen; never claim old work was stopped.
   The same task's genuine receipt is delivered to an authenticated receiver
   before its task is known, and a contradictory outer receipt is refused at T2;
   both full receipts remain exportable after reopen. These are assertions within
   the two inherited journeys, not new independent test scenarios. The separate
   wrong-computation predicate branch is statically checked; it is not claimed as
   another directly injected runtime fault.

A bounded direct Python process-fault lifecycle also kills receipt serialization,
checks that no partial final was published, preserves damaged/conflicting finals,
and completes a distinct repair identity. This is a publication-primitive check,
not real SSH interruption, C07 redispatch or physical power-loss evidence.
All owned listeners stop on both success/failure and are read back as refused.

No physical machine, actual model inference, election, partition quorum,
production child scheduler, arbitrary resource fencing, image resurrection or
long-run acceptance follows from these two scenarios. See the machine-readable
[evidence](../evidence/c17-native-first-journey-20261025.json) and [C17](../tasks/C17.md)
for exact candidate and remaining obligations.

## Focused review repair / 复核接续

[PR40](https://github.com/cyjin-yl/shenko/pull/40) on main `04c09a4` repairs
installation ownership and E1/T2 separation; final product source `453fff7e754e46af920dc7fcbc46c9321f41d819`.
The [repair evidence](../evidence/c17-ownership-evidence-repair-20261025.json)
binds the actual binary, schema and inherited journeys. Ledger identity/seq
continuity in the C07/main Store remains an independent C01/#6 P1; this adapter's
own durable ledger does not repair it. Full C17 and production integration remain
incomplete.

The focused script recovery continuation is PR [44](https://github.com/cyjin-yl/shenko/pull/44).
Its [source/artifact-bound evidence](../evidence/c17-script-receipts-20261025.json)
separates the real SSH results from direct Python faults and records the immutable
review snapshot; it does not supersede the original remaining C17 obligations.
