# Control predecessor directory publication

## Measured reason for this change

The fixed release baseline `72fc70d3325dc8b2ddacd142744b0d4b8b184705`
(`7d179ac65453a44e77e2e63091b753c7f68416cc1fd70187819ea09165e0aea7`,
89,865,568 bytes) produced a complete first S1 in
`protocol-before-r3/case-01` on 2026-11-21. Its `subject.prepare_s1`
scope took 48.579323 seconds; its nested `control.seal_pages` took
47.346063 seconds. The original run still failed its cleanup check and is
not a complete task or clean-shutdown success.

The stopped case's retained CAS gives a concrete shape for that seal scope:
86 rows, 19 tree pages and one predecessor root, totaling 139,861 serialized
bytes across 106 distinct objects. The rows include 22 content-stream units
and 43 subject-output parts. Seventeen objects also occur in the preceding
snapshot graph; that comparison is not a measured dedup hit counter.
The referenced model-result bytes are not re-sealed inside this scope.

The original publisher synchronizes the leaf shard, first shard and CAS root
for every object, including a fully verified existing object. The 106 calls
therefore imply 318 directory-sync calls along the successful source path.
That is a source-derived count, not a syscall trace. Neither the phase timer
nor directory metadata identifies file-sync latency, filesystem dirty-page
behavior or the fraction attributable to hashing. This change has no measured
performance benefit until the changed binary is run under a fixed deployment.

The detailed observation is retained as
`protocol-before-r3-case01-seal-shape.json`, SHA256
`0ab3323240d143474eb29661e68bbbc96b636572ef7ff333cb507679abfcb0b1`.
Its original runtime log is SHA256
`a369bad3bc590f7a1edeb5ba4f733bdab3a1dbee1c8f1e7217778c3e068d4102`.
The selected predecessor is
`blake3:d381dbc3a856f77c97c048d094e3f090969e4d4b34e4ee7a4719f83e1621d06c`.
Its reference also occurs in the retained S1 admission envelope, and its
creation interval lies inside the measured seal scope.

## One graph, one child-directory barrier

Only `prepare_control_predecessor_inner` uses the internal graph publisher.
It keeps the existing snapshot, closure validation, row serialization, tree
partitioning, codecs and object identities. The change does not cache verified
objects, skip hashes, reduce a read witness or change admission/SQL ordering.

`PendingObjectGraph` produces provisional `ObjectRef` values solely inside the
graph-building closure. It does not produce a `SealedObjectRef`. The closure
returns serialized root bytes to `seal_graph_root`, which performs this order:

1. Each new child keeps its original unique staging name in the CAS root,
   `write_all`, file `sync_all`, and no-overwrite hard-link publication.
   An existing object, including a hard-link race winner, must still pass the
   full stored-length and BLAKE3 verification.
2. Each child records its leaf shard, first shard and CAS-root directory.
   Its owned staging file is closed and removal is attempted with the same
   cleanup/error semantics as ordinary sealing. No shared staging name is used.
3. After the complete graph is built, the distinct directories synchronize
   from deepest to shallowest, with the CAS root last. Every child has already
   completed its staging-cleanup attempt. The set is bounded by the two-byte
   shard namespace, rather than by the number of historical objects.
4. Only after the child barrier succeeds does the root use the ordinary
   `seal_object` path, including its own file and directory barriers. Only
   that durable result is returned to the caller for root-reference admission.

The shared byte-writing helper preserves ordinary single-object ordering;
`seal_reader` still performs its original per-object directory barrier.
No root or public sealed-reference promise escapes a failed child barrier.
Distinct shard directories and new child files still require their own syncs;
batching does not reduce all synchronization to one call.

## Failure and publication custody

A child write, verification or directory error prevents the root seal from
starting. A failed root seal returns no successful result. Unreferenced child
objects can remain after either failure, as with earlier aborted Store
preparations. Existing content-addressed targets are never replaced. The
original staging location and owned-file cleanup behavior remain intact so
staging discovery and future reclamation do not acquire a new hidden path.

The graph publisher acquires no lease, Node reservation or SQLite lock.
`export_cognition` already takes a shared `CasWriteLease`; the export's
unmoved `_publication` field remains live through the entire predecessor
builder, including the child barrier and durable root seal. Ordinary
`reserve_admission` also holds an outer publication lease from before this
call through `AuthorityReservation` and the Store transaction.

The canonical `prepare_*_transition` paths have an existing custody gap:
the predecessor helper returns before `reserve_authority` acquires the lease
which covers prepared-transition publication. `reserve_sealed_admission`
also explicitly requires callers to protect earlier external sealing.
This increment neither creates an additional lease under an existing
Node/SQLite lock nor claims to close those preexisting caller gaps. It keeps
the helper's current custody lifetime and does not enable Store reclamation.
Complete writer/root-retirement integration remains required before GC.

The required ordering comes from
[durable state §41.2.2](../specs/durable-state.md#spec-41-2-2): objects and their
necessary directories must be durable before metadata publishes references.
This source-level argument does not claim an actual power-loss experiment.

## Verification boundary

One added CAS journey uses a real temporary filesystem. It first gives a
same-length corrupted existing child to the graph publisher and checks that
the publisher refuses without overwriting it or publishing a root. After
repairing this owned fixture, it moves a child shard before the directory
barrier, verifies that the real filesystem error prevents root publication,
restores the shard, retries, and reopens CAS to compare both original children.
It also checks that owned object-staging files were removed. No test-only
publication implementation, global failure hook or synthetic sync success is
introduced.

The intended focused command is:

```sh
cargo test -p shenko-store --lib cas::tests::graph_root_waits_for_children_and_reopens_without_overwrite -- --exact
```

The existing Store recovery consumer exercises the resulting predecessor
through real canonical S1 admission and recovery:

```sh
cargo test -p shenko-store --test store_roundtrip canonical_s1_recovers_identical_results_on_a_distinct_ledger_without_dispatch -- --exact
```

At authoring, only source review and `git diff --check` ran; Rust/Cargo are not
installed in the editing environment. The integration owner must attach the
actual devcontainer/CI receipts before calling these checks passed. The
already-running protocol comparison retains its original frozen binaries and
does not test this publication change.
