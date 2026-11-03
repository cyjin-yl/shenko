# Remote seats — functional contract

SPEC §17, §17.5, §35.5, §41.11; carries PR127 (native SSH transport) into
installed task authority. Work package: `architecture-remote-seats-20261101`.

## What a seat is

A `TaskSeat` is a stable product identity for one remote-agent work position
(Store `seat_records`). A tmux pane id or an SSH target is only its current
*location*, never its identity. When work moves, the old record is retired
and a new record names it via `supersedes`; history is never rewritten.

## The four installed native children

| kind | effect | durable facts |
| --- | --- | --- |
| `ssh.exec` v1 | one exact remote command over the PR127 transport (`native_transport::execute`) | frozen input, observation receipt (stdout/stderr/exit/interruption) |
| `ssh.pty` v1 | one live PTY seat: register seat, acquire v2 lease, attach adapter to `SeatRuntime` | seat record, v2 lease, attach receipt |
| `seat.observe` v1 | one observation of an attached seat | every reported question is a `seat_questions` row before the observation returns; `durable_only` status when no live seat is attached |
| `seat.respond` v1 | one answer to one open durable question | one `seat_answer_decisions` row per decision attempt, the exact effect digest, and the closing evidence |

All four follow the product's at-most-once contract: freeze the input to CAS,
arm the exact operation id (`arm_external_operation`), effect once, observe
with evidence. Reconcile is query-only; a crash after T1 never re-sends a
command, re-opens a PTY or re-answers a question.

## Writer lease v2 (SPEC §17.1 单写者)

`SeatLeaseRequest { lease_id, seat_id, holder_task_id, lease_epoch (>0),
fence_capability }` commits `SeatLease { schema_version: 2, lease_epoch,
fence_capability, legacy_writer, ... }`. `check_seat_write` refuses stored v1
rows (`seat_lease_legacy`) and non-current or non-fence leases with typed
errors. Relocation retires the predecessor and ends its lease in the same
commit; the successor is a new record, so stale bindings break instead of
ageing.

The writer authority travels *with* the effect: `SeatWriteFence { lease_id,
lease_epoch, fence_capability, seat_id, seat_incarnation, task_id,
task_revision, question_id, question_revision, question_record_revision }` is
handed to the adapter that performs the write. The runtime does not
check-then-act: the durable claim and the arm happen before the transport
runs, and the epoch the decision was made under is the epoch the write
carries.

## The respond contract (SPEC §41.11)

`Store::commit_seat_answer_decision` is the **pre-effect claim**. It runs
before any input reaches the remote side and compares every version the
decider actually read:

| expectation | refusal when it moved |
| --- | --- |
| `question_revision` + `question_record_revision` | `stale_question` |
| `seat_incarnation` + `seat_revision` | `seat_location_changed` / `stale_task` |
| `task_revision` | `stale_task` |
| `lease_epoch` | `stale_authority` / `seat_lease_ended` |

On success the question becomes `decision_committed`, so a second decision for
the same question is refused (`seat_question_not_open`). The only exception is
a previous decision that ended `not_sent` — the remote provably never applied
it — so a *new* decision id may be made.

The runtime then runs the SPEC order, one durable fact per step:

1. **commit** the decision (`decision_committed`).
2. **plan** the exact bytes (`SeatEffectPlan { transport, payload, digest,
   summary, question_key }`). No mapping without evidence → nothing is
   written and the decision ends `not_sent`.
3. **preflight** the live state: the adapter re-reads the *current* protocol
   or screen state, never the observation the decision was made from. A
   refusal is typed (`question_gone` vs `state_changed`); a question that is
   gone is closed as `externally_resolved`, a changed state only refuses this
   write and returns the question to `open`.
4. **arm** the effect (`Store::arm_seat_answer_decision`): the digest and the
   carrying operation are recorded, the question becomes `responding`. From
   here the effect may have reached the remote side, so recovery never
   re-sends it.
5. **execute** the transport, outside every database transaction.
6. **record the evidence** (`Store::complete_seat_answer_decision`):

| observed | decision state | question state |
| --- | --- | --- |
| remote acknowledged | `answered` | `answered` |
| written, no acknowledgement | `answer_unknown` | `answer_unknown` |
| transport outcome unknown | `answer_unknown` | `answer_unknown` |
| refused before the write | `not_sent` | `open` (a new decision may be made) |
| remote rejected | `rejected` | `withdrawn` |

`answer_unknown` is the honest end state of an unconfirmed answer: the
question disappearing from the screen is not evidence that our answer took
effect, and an unknown effect is never replayed.

Question states are the SPEC set: `open`, `decision_committed`, `responding`,
`answer_unknown`, `answered`, `expired`, `withdrawn`, `superseded`, plus
`externally_resolved` and `late_answer_ignored` for a remote-side
resolution. A question whose wording changed, or a closed question that
appears again, gets a **new** durable question id — the old record is closed
as `superseded` and keeps its history.

## Adapters (crates/shenko-exec-ssh/src/seat/)

* `tmux` — existing tmux foreground TUI. Questions inferred from the styled
  screen; a selection is reported only from a preserved highlight attribute
  or a cursor glyph (`❯ › ➤ …`); numbered options map to their number;
  yes/no is recognized only on an observed `y/N` prompt and is never
  blind-typed. The guarded `send-keys` re-checks the pane on the far side:
  the pane must exist, must not be dead, must still run the observed program
  and must still host the observed root process (`pane_replaced`).
  A captured screen's final LF is a row terminator, not a live terminal
  newline: a full-height capture must retain its first row. Both `1.` and
  `1)` option markers retain their numeric input value; absence of a
  highlight or cursor marker leaves the selected option unknown.
* `ssh_pty` — managed remote agent over `native_transport::open_pty`. One
  single-owner pump feeds a `TerminalScreen` (full VT styles/grid,
  `terminal.rs`) and serializes writer commands, so writers never queue
  behind a held session lock. Same inference rules as tmux.
* `omp_rpc` — managed OMP (`oh-my-pi` v18 protocol): `ready`,
  `extension_ui_request` (select/confirm/input), `extension_ui_response`,
  `agent_end{isTerminal}`. A remote timeout is recorded
  `externally_resolved`, not an agent decision.
  Chunk assembly admits the first chunk's declared count and byte length,
  and requires the complete observed length to match before allocating the
  assembled payload; a parseable JSON prefix is not a complete message.
* `claude_sdk` — managed Claude Code control protocol
  (`--input-format stream-json --output-format stream-json
  --permission-prompt-tool-name stdio`; `initialize` before the first user
  message; `can_use_tool` → `AskUserQuestion` is an `information` question,
  any other tool is a `tool_permission`). `canUseTool` is not a uniform audit
  entry — calls an earlier rule approved never reach it — so task and effect
  ledgers remain this product's own records. An `AskUserQuestion` answer
  echoes the remote side's original `questions` array (kept verbatim in the
  question's `native_payload`) and keys its `answers` mapping by the
  sub-question the chosen option belongs to.

`SeatRuntime` (seat/runtime.rs) is the only registry; `seat.observe` /
`seat.respond` handlers call it, and it performs question persistence, the
durable claim, the live preflight, the arm and the closing evidence.

## Native SSH transport additions (PR127 preserved)

`native_transport/pty.rs` adds `NativeSshSession::open_pty(request,
handshake)` → `NativePtySession` (live `NativePtyEvent` stream, send/resize/
signal/eof, `close(grace)` that counts post-close frames as `Protocol`
interruption). The single-command `execute()` path is unchanged.

### Explicit file-backed installation

`shenko start --ssh-targets <installation.json>` installs the `ssh.native`
task handler before native-child recovery. The
`shenko.ssh.file-installation.v1` document names explicit targets, their
selected private-key file, configured public key, known-hosts file and
remote receipt root. It does not discover credentials or select a different
node in place of the durable node-selection decision.

`shenko ssh-public-identity --installation <installation.json> --target <id>`
prints the configured public key and fingerprint with status
`configured_unverified`. It runs before Store/Tokio startup and does not open
the referenced key or trust file, authenticate, or claim possession of the
key. Execution uses the selected key through a bounded zeroizing buffer and
checks the configured public identity. This `ssh.native` path opens the key
before TCP; it is distinct from `ssh.exec`'s KEX-before-key path.

If the first reply is lost after the remote helper records its receipt,
recovery queries that original operation and input digest. It does not
upload again or create another attempt merely because local completion is
unknown. A wrong pinned host key must not reach authentication or the helper.

## Schema cutover (one explicit step)

`migrations.rs::migrate_seat_v2` is the single v1 → v2 cutover:

* `seat_answer_decisions` loses its `UNIQUE(question_id)` constraint — the
  uniqueness rule now lives in the admitting transaction, so a justified
  retry after a `not_sent` refusal is possible. The rows are copied, never
  dropped.
* stored v1 decision documents are brought to the v2 shape: the original
  `confirmation` string is kept and the state is derived from it.
* stored v1 leases are marked `legacy_writer` — they never carried a writer
  epoch or a verified fence capability, so they were never writers. The lease
  is not *ended* by the migration: ending a lease is a lease transition that
  belongs to a ledger commit (the first v2 acquisition displaces it).

## Boundaries

* Bindings are secret *handles*: the frozen input carries the `SshBinding`
  (key path + CAS-sealed known_hosts ref); the key file is read locally at
  effect time and never sealed into receipts.
* `use_agent` and password bindings are refused by the native children
  (agent-backed `Signer` impls can be admitted later without schema change).
* Remote `exit 255` is a real status; the legacy "local ssh process died"
  retyping does not apply.
* A written answer is transport, not acceptance: `AnswerConfirmation` stays
  `Unconfirmed` unless the remote acknowledged it.
* The writer lease is *this product's write authority boundary*, not a
  protection against human input. A human typing into the same pane, or
  another process, is not fenced by it — tmux has no atomic "check the
  question and type". That race is reduced (live re-check, target side
  pane/process guard) and reported, never hidden.
* Recovery queries only the original operation receipt and the durable
  decision facts; `unknown` never authorizes a replay.
