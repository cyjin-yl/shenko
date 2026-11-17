# Installation write permits: implementation plan

The original source audit and proposed implementation boundary below is bound to
`c9db3aa8e315a1ea320d67584fdbff1129312762` (tree
`39f65e771eb299d2c342d61ceb2dc8528f028276`). The existing seat admission/outcome
increment has nine distinct scenarios and the [PTY receipt increment](pty-write-flush-receipts.md)
has five; this plan adds none. It does not reduce the
[remote-seat contract](../specs/remote-seats.md) or
[completion scope](../governance/COMPLETION.md).

## Implemented coordination boundary (2026-11-17)

`shenko-types::installation` now owns a stable, non-cloneable physical
installation permit. Installed Store opens and mutations, Node initialization
and write transactions, cognition publication and update slot publication use
that domain. Source authority reservations carry the original permit through
the explicit `*_under` APIs in installation → Node → Store order. Network
preparation, async participant drain and candidate smoke happen before the
final reservation. Forward activation, rollback and committed-cutover recovery
retain one reservation from the final snapshot through both selectors, the
retired head, the launcher pointer and both Observing projections.

Fresh Store mutations revalidate the actual selected slot, selector/head and
sealed route. An old Store handle is refused after cutover; original evidence
intake and learned historical materialization retain their separate factual
path. An interrupted preselection abandonment carries the original boundary
identity so a crash before its resume can finish that same resume. The existing
successful handoff test also exercises an old handle's refused control mutation,
a late factual receipt and the successor's permitted control mutation.

This is a short mutation/cutover coordination boundary, not a certificate that
arbitrary external processes have stopped. It is not held across a whole model,
WASI, REPL or SSH execution. The adapter-specific physical-write capability and
crash/flush proofs described below remain separate obligations; an Armed row,
this mutex or a maintenance permit alone never authorizes a new send.

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

The concrete entry points above are in
[`store.rs`](../../crates/shenko-store/src/store.rs),
[`ledger.rs`](../../crates/shenko-store/src/ledger.rs),
[`authority.rs`](../../crates/shenko-store/src/authority.rs),
[`runtime_authority.rs`](../../bin/shenko/src/runtime_authority.rs),
[`runtime_branches.rs`](../../bin/shenko/src/runtime_branches.rs),
[`seat.rs`](../../crates/shenko-store/src/seat.rs),
[`cognition/import.rs`](../../crates/shenko-store/src/cognition/import.rs),
[`recovery_sources.rs`](../../crates/shenko-store/src/recovery_sources.rs),
[`slots.rs`](../../crates/shenko-update/src/slots.rs) and
[`handoff.rs`](../../crates/shenko-update/src/handoff.rs).

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

## Native tmux prerequisite: actual call audit

This section proposes the next transport prerequisite. It does not install it,
make the OpenSSH path continuously fenced, or change an existing capability.
The source binding is the same `c9db3aa8e315a1ea320d67584fdbff1129312762` above.

### Existing paths are not interchangeable

[`TmuxSeatAdapter::remote`](../../crates/shenko-exec-ssh/src/seat/tmux.rs)
constructs [`TmuxSeat::remote`](../../crates/shenko-exec-ssh/src/ssh_exec.rs),
which uses [`SshExecutor::over_ssh`](../../crates/shenko-exec-ssh/src/executor.rs).
It always uses real SSH, including a loopback endpoint. Each command starts
an independent OpenSSH process. It does not load user/system ssh_config or
reuse ControlMaster, so preserving unspecified ProxyCommand configuration is
not a requirement of this existing API.

| Property | Existing OpenSSH tmux path | Existing installed native transport |
| --- | --- | --- |
| Target | `SshTarget` host, port and account; validated before process spawn | The same endpoint fields in a complete `SshBinding`, plus a `NativeInstallationSeal` for installed use |
| Private identity | Explicit key path, explicitly enabled ambient agent, or both; default private identities are disabled | File opener reads one exact absolute file, checks permissions/type/size and matches the installed public key; no passphrase source or agent discovery |
| Agent support | `use_agent=true` requires the caller's `SSH_AUTH_SOCK`; OpenSSH selects from that authorized agent | Protocol API accepts an explicit public identity and `Signer`; the installed file adapter does not provide an agent implementation |
| Public trust | Strict OpenSSH validation; explicit known_hosts disables global trust, otherwise OpenSSH's default trust files apply | Exact CAS-sealed known_hosts bytes; parser accepts literal hosts/ports and revocations, and rejects hashed patterns, wildcard/negated rules, CA markers and certificates |
| Signature policy | Actual installed OpenSSH policy under explicit flags | No RSA/SHA-1 fallback; installed file opener selects RSA/SHA-512 for an RSA key |
| Limits | `TmuxSeat` currently calls unbounded `run_command`; stdout/stderr each cap at 2 MiB and continue draining | Explicit handshake/command/cleanup budgets and separate byte limits; single-use session |
| Results | `ExecResult` contains lossy UTF-8 output, local process status and retained uncertainty; SSH process 255 remains ambiguous | `NativeCommandObservation` retains output bytes, remote exit/status, exec acceptance, channel closure, interruption and local session-end observation; a remote exit 255 is an actual status |
| Cancellation | Process-group kill request and child kill-on-drop; the disappearing caller does not establish a joined writer | The original `OwnedStream` close guard prevents later underlying I/O polls; this is not remote process termination or remote application acknowledgement |

These differences are enforced in
[`native_installation.rs`](../../crates/shenko-exec-ssh/src/native_installation.rs),
[`native_transport/trust.rs`](../../crates/shenko-exec-ssh/src/native_transport/trust.rs),
[`native_transport/mod.rs`](../../crates/shenko-exec-ssh/src/native_transport/mod.rs)
and [`native_transport/stream.rs`](../../crates/shenko-exec-ssh/src/native_transport/stream.rs).
An OpenSSH configuration that native transport cannot represent must remain
on its explicitly chosen compatibility path or be refused when native mode is
requested. Silently selecting a different key, trust source or transport is not
a migration.

### Runtime entry and identity gaps

The audited repository has no production caller of
`TmuxSeatAdapter::remote`. The real startup path in
[`runtime_sessions.rs`](../../bin/shenko/src/runtime_sessions.rs) only reattaches
the durable local tmux records through
[`SeatRuntime::reattach_record`](../../crates/shenko-exec-ssh/src/seat/runtime.rs).
That method explicitly skips `host != "local"`, because the location record
does not carry an SSH binding. Adding only a native constructor would therefore
leave the new behavior unused by Start.

There are two additional target-selection constraints to fix in a real native
attachment path:

- The current adapter's `discover` always writes `host="local"` and uses the
  raw pane ID as `SeatHandle.location_id`, even for its remote constructor.
  Multiple remote servers can legitimately have the same pane ID. The new
  mode needs a stable installed-target/binding/pane identity, while retaining
  the raw pane ID used by tmux on that server.
- `SeatRuntime::attach` chooses `discovered.remove(0)`. An explicitly requested
  remote pane must be selected by exact identity before attachment, not by the
  server's list order. The Store already checks the active `(host, pane_id)`
  claim in `register_seat`; that durable check must remain.

Actual configured native SSH is already wired:
[`runtime_start.rs`](../../bin/shenko/src/runtime_start.rs) receives `ssh_targets`,
calls [`install_native_ssh`](../../bin/shenko/src/runtime_plugins.rs), then
[`NativeSshTaskHandler::from_file`](../../crates/shenko-exec-host/src/ssh_task.rs)
builds an immutable map of `NativeSshInstallation` values. That handler uses
[`execute_selected_installed`](../../crates/shenko-exec-ssh/src/selected_task.rs)
and [`SshTaskRunner`](../../crates/shenko-exec-ssh/src/task_runner.rs): target
selection is evidence, T1 precedes authentication, and retry queries the
original frozen operation. The map is currently private to that handler;
the seat generation does not have the same installed registry.

### Proposed smallest live-wired native mode

Implement one explicit installed-native tmux attachment and answer path as a
coherent prerequisite, without claiming installation exclusion. Keep existing
local and OpenSSH constructors and their original records readable. The
proposed scope is:

| Source area | Necessary change, still proposed |
| --- | --- |
| `native_installation.rs`, `shenko-exec-host/src/ssh_task.rs` | Share the same immutable installed target registry with the real consumers; preserve exact public identity, binding revision, sealed trust and explicit opener. Do not construct targets from a model-supplied host/path or select a fallback identity. |
| `runtime_start.rs`, `runtime_plugins.rs`, `runtime_sessions.rs` | Load the existing explicitly selected configuration once and make the native tmux handler part of the same seat Cordis generation. Preserve `ssh.native`, its Node/selection checks and the old no-configuration behavior. A retained Arc to an earlier seat generation is not an acceptable shortcut. |
| A native tmux handler in `shenko-exec-ssh` | Admit an exact installed target and pane, with original seat/lease IDs and bounded limits, through a real native operation. Freeze installation selection before T1. Replayed admission must query original facts and cannot attach a new writable seat or send a command again. Any reused selection helper must preserve the existing distinction between selection and authorization. |
| `ssh_exec.rs`, `seat/tmux.rs` | Keep the existing guarded command's pane/dead/program/PID checks, shell quoting and explicit Enter behavior. Add an explicitly selected native backend, target-qualified location and exact-pane attachment. Preserve the original process guard and its honest race with independent human input. |
| `seat/runtime.rs`, only where required | Attach the requested exact handle under its real durable binding. Do not change all adapters to trust the first discovered pane. Retain the existing closing generation checks and Store location claim. |

The native mode must consume the already owned
`NativeSshSession::execute`, not enqueue bytes into an unowned child process.
Its full guarded command and limits must be planned before the relevant T1;
the existing tmux text-only plan is not a proof that a subsequently rebuilt
remote shell command was frozen. Actual command completion remains an
unconfirmed terminal input, never proof that the TUI accepted an answer.

The old `terminal_write_response` helper's catch-all error-to-NotSent branch
is valid only for its current validated OpenSSH call path. Native post-exec
observations must use a typed branch that retains their complete original
observation and preserves Unknown before interpreting reserved guard exit
codes. Do not coerce native remote exit 255 into legacy SSH-process-255
uncertainty, or coerce a lost native output into a target-side refusal.

Automatic remote reattachment is not available merely because this new handler
can attach once. It needs an immutable original binding/installation reference
whose complete identity still matches the installed registry and original
seat/task/lease, with an explicit stale-binding refusal. A host string or a
new entry with the same target label is insufficient. Until that additional
route is implemented, remote records must stay visibly durable-only after a
restart; they must not silently acquire new leases or replay input.

This proposed prerequisite changes neither Store permit admission nor any
other writer. The separate local-shell/OpenSSH crash boundary and the whole
installation mutation/slot protocol remain open. Its implementation and finite
validation scope require a separate review; no new scenarios or Rust execution
are claimed here.

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
