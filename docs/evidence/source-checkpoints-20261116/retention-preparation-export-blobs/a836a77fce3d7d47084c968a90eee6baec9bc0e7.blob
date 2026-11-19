# Installation write permits: implementation plan

This is a source audit and a proposed implementation boundary, not an installed
permit or an acceptance result. The audit is bound to
`c9db3aa8e315a1ea320d67584fdbff1129312762` (tree
`39f65e771eb299d2c342d61ceb2dc8528f028276`). The existing seat admission/outcome
increment has nine distinct scenarios and the [PTY receipt increment](pty-write-flush-receipts.md)
has five; this plan adds none. It does not reduce the
[remote-seat contract](../specs/remote-seats.md) or
[completion scope](../governance/COMPLETION.md).

## Boundary to implement together

The useful boundary is cooperative seat writing within one verified physical
installation, including every local mutation that can invalidate its task,
lease, incarnation or selected software generation. A process-local seat mutex,
an extra check before a transport await, or a lock held only by the caller cannot
establish that boundary.

Two non-cloneable capabilities are needed:

- An installation mutation permit orders cooperating live mutations and final
  slot cutovers. It grants neither primary authority nor permission to send.
- A physical seat-write permit is obtained only by the Current admission path,
  after its actual final T1 checks. It names the original installation, selected
  slot, task invocation, seat/incarnation, lease/epoch/capability and exact input.
  Prepared or Learned recovery cannot manufacture it from an Armed row.

The low-level coordination primitive must be shared by Store, Node, update and
the physical owner without a Store/Node dependency cycle. Its stable inode is
separate from every SQLite file and from the root directory already locked by
`RuntimeOwner`. Validate the directory and named inode on admission. An advisory
lock is coordination, not a new source of installation identity or selection.
Initial enrollment, existing installation admission and same-installation slot
copies must use the same verified physical domain; an archive or foreign
manifest cannot supply it. Unsupported backends must refuse this capability,
not return an unlocked guard.

## Final admission order

1. Construct and seal the original proposal. Complete provider preparation and
   quorum/network work without holding an installation permit.
2. Acquire the installation permit before any Node reservation or Store/import
   transaction. A cancelled waiter cannot later become a sender.
3. Acquire the actual source reservation and perform the existing final checks
   and T1 transaction with a borrowed permit.
4. Finish the Store transaction and drop the Node/source guard. Only the Current
   path may move the original physical permit into the actual writer.
5. Keep that permit until the original local write/flush completes or the
   original local writer is proven unable to issue more writes.
6. Release physical ownership before starting a new E1/settlement admission.

The proposal is not re-evaluated on a lock conflict. Existing chosen debt,
fixed IDs/results, exact predecessor checks and original receipts remain.
`AuthorityReservation::Drop` must retain its existing nonblocking
`action_unmaterialized` behavior and release source guards before its permit.

Explicit borrowed-permit APIs are necessary for nested calls. Proposed shapes
include `open_under`, `with_mutation_under`, `reserve_*_under` and
`import_*_under`; these are design names, not existing APIs. Do not infer
reentrancy from a thread-local flag or open a second lock descriptor in a nested
call. Holding a Node transaction while waiting for a physical writer would
reverse the required order.

## Live callsite coverage

| Current source surface | Required integration |
| --- | --- |
| `Store::open`, schema initialization, `Ledger::new` | Obtain the domain permit before persistent PRAGMAs, migrations and identity writes. `channel::migrate_publication_targets` at the end of open must borrow it rather than reacquire. |
| `Ledger::with_mutation` | Cover every ordinary commit, seat/task/control mutation, generic `WriteBatch`, canonical application and factual settlement while retaining the original writer-failure latch. |
| `authority.rs` reservation entry points | Move domain acquisition before `provider.acquire`; preserve network preparation outside the final permit. The current provider acquires a Node transaction and then reads Store. |
| `RuntimeAuthority`, `runtime_branches`, installed Node RPC/control paths | Use the same domain before Node initialization/reservation and authority-changing transactions; nested request handling borrows the original permit. |
| `seat.rs` | Include registration, relocation, retirement, incarnation, lease acquire/end, questions and decisions. Covering only `arm_seat_answer_decision` leaves invalidators outside exclusion. |
| `cognition/import.rs` | Coordinate the existing import gate and live file/Git/row publication, including task/seat semantic units. Downloaded archive content does not grant a physical permit. |
| `recovery_sources.rs` | Include the actual live metadata INSERT currently outside `with_mutation`; private staging databases are not installed writers. |
| `slots.rs` | Cover consistent backup, copied ledger identity change, manifest publication and final pointer publication. Do not copy the physical lock into a slot. The currently unused public READ_WRITE `open_slot_db` is an API bypass to remove or constrain. |
| `handoff.rs` | Apply one final-cutover protocol to `activate_inner`, `rollback_inner` and `finish_committed_handoff`, including candidate CAS, source mirror, head fence and pointer. |
| CLI/runtime openers | Start/bootstrap, branch, tools, context, update and other live Store openers must carry the same installed domain. A legacy constructor cannot silently bypass it for the same installed database. |
| `SeatRuntime` and adapters | Move the original physical permit to the actual writer. A plain `SeatWriteFence` data value is not an ownership capability. |

Bare live SQLite/file mutation paths must be audited before calling this set
complete. Merely adding a new optional API while old installed paths remain
unguarded is not this increment.

## Current selection and historical facts

Fresh physical admission must verify the actual selector/head projection,
sealed route identity and physical root/slot paths under the permit. The checks
in `context_windows.rs::verify_selected_context_slot` provide useful principles,
not a complete active-generation resolver:

- Match SQL selector projections with their original body, generation and head.
- Validate the immutable route's bytes, digest, schema, subject and origin.
- Do not require route epoch/revision equality: a legitimate takeover can
  retain an older sealed route. Future values remain invalid.
- Refuse the exact unresolved source Handoff revision, even if its old
  head/selector/route still agree before the mirror is repaired.
- Treat `current-slot` as a convenience pointer, never selection authority.

After asynchronous drain/primary checks and local preparation, final cutover
must acquire one permit, re-read both slots, perform the candidate CAS, mirror
it, fence the original head and publish the pointer. Recovery performs the
corresponding original repair under the same permit. Old handles must fail fresh
physical admission after a cutover, even if an old lease still looks Current in
their retained database.

Historical E1, learned materialization and original reconciliation still
participate in domain serialization, but cannot be required to become the fresh
selected writer. A blanket stale-slot rejection would discard legitimate late
facts. Neither historical admission nor an update maintenance operation yields
a send permit.

## Physical ownership remains adapter-specific

### SSH PTY

Move the permit into the original queued `Command` and then `Pump.active`.
Keep it across channel-window/rekey queues and the actual channel-scoped flush
receipt. Cancellation closes the original `OwnedStream` before releasing it.
Keep original input/error retention, Queued-to-NotSent and Writing-to-Unknown;
late success cannot replace Unknown. This is a local write boundary, not remote
acceptance or withdrawal of bytes already transmitted.

### Managed stdio

The existing `ManagedProcess::write_frame` holds the original stdin mutex across
`write_all` and `flush`, and retains a cancelled frame's identity/Unknown latch.
It does not spawn another writer in this repository. The permit belongs inside
that physical writer ownership, not in its outer adapter caller. The pinned
platform implementation of Tokio `ChildStdin` must still be verified before
asserting that cancellation leaves no background writer. The
[Tokio 1.54.1 public implementation](https://docs.rs/tokio/1.54.1/src/tokio/process/mod.rs.html)
delegates to the platform implementation; that delegation alone is not the
missing proof.

Plan the final JSONL bytes, including newline, before T1. The current managed
answer path parses a planned JSON payload and serializes it again in the writer;
submit/interrupt allocate protocol IDs after the outer native T1. Preserve old
readable facts, but do not treat their payload digest as a newly proven exact
wire digest. A typed plan/seal/seat-aware arm path is required. The expected
frozen seat ID must also reach `SeatRespondRequest` and its final check.

### tmux through OpenSSH or a local shell

Current `SshExecutor` cancellation requests process-group termination and relies
on `kill_on_drop`; it does not establish a joined physical writer before the
caller disappears. Parent-process crashes do not run Rust Drop.

Direct inheritance of an extra permit FD into OpenSSH is not a valid solution:
the [official portable client source](https://github.com/openssh/openssh-portable/blob/master/ssh.c)
calls `closefrom(STDERR_FILENO + 1)` at the start of `main`. This audit read that
official source, not the exact binary installed by a particular runner. A
supervisor holding the FD is also incomplete if its own crash releases the lock
while its child can continue writing.

Before claiming this adapter is covered, either connect its original guarded
commands to an actually owned native SSH transport, preserving authentication
and complete results, or implement an execution owner with durable in-flight
facts and a verifiable stop boundary across its own crash. The local shell path
needs its own equivalent proof. Unknown never becomes an automatic resend. If
an adapter cannot provide the capability, a request for it must be refused,
not silently routed through the original unguarded path.

## Boundedness and exclusions

An indefinitely blocked frame cannot hold the whole installation hostage.
The original owner needs an explicit bounded write/stop policy and cancellation
outside the database gate. It cannot require a task-cancel transaction to obtain
the same permit before the physical writer learns it must stop. Time elapsed
does not itself prove a local writer stopped or a remote action ended.

This design does not authorize a primary, fence arbitrary older binaries or
external SQL writers, stop independent human input, prove remote execution has
ended, or provide physical exactly-once. It does not automatically cover model
requests, WASM, host tools, independent SSH operations or CAS reclamation.

The concurrent retained-object work is a single-database read-only snapshot and
private inspection path. It neither registers a GC producer nor grants this
permit. Do not turn its readable closure result into deletion exclusion or fresh
effect authority. No new implementation or runtime verification is claimed by
this plan; the first publishable scope and its finite new scenarios remain to
be chosen after reviewing these concrete boundaries.
