# PTY local write and flush receipts

This increment connects the existing SSH PTY seat to an actual local transport
boundary. It grants no Store authority and does not close the installation-wide
writer exclusion gap.

The separate [native command cleanup observation](native-command-cleanup-receipts.md)
retains local session errors for the already installed command consumers; it
does not change this PTY protocol or its five scenarios.

## Production path

`SshPtySeat` admits each original input into its existing single-owner pump.
Admission is bounded at 32 commands, including replies whose caller has not yet
resumed. Each record keeps the complete original bytes (including the carriage
return added by `submit`) or the exact `INT` signal request, its digest, an
independent local write identity and the supplied writer epoch. That epoch is
attribution, not a new lease validation.

The vendored client's `FlushOutbound` travels on the same sender FIFO as the
channel write. It completes only after prior channel-window/rekey queues and the
packet writer have drained through `PacketWriter::flush_into`, including the
underlying stream's flush. It does not use `ChannelTx::poll_flush`, which only
returns queue-interface success. A channel-scoped receipt also requires the
original channel still to exist at the boundary, so a peer close cannot silently
turn discarded data into a successful receipt.

The same pump continues feeding terminal output while waiting for that write.
The reader and borrowed writer are disjoint views of the original session, not
two physical sessions or an alternative execution route. Successful local flush
maps to `Unconfirmed`, never to remote application acceptance.

## Cancellation and failure

A caller cancelled after admission synchronously closes the original owned
stream through its shared stop handle. The stream serializes that close with a
single actual nonblocking I/O poll; once close returns, no later inner write is
polled. The original pump still owns close and join. Its active command becomes
`Unknown`, and queued commands that never began become `NotSent`. Original bytes
and observed errors remain in the pump journal and close observation; they are
never selected again or replayed. Cancelling before admission cannot enqueue an
input.

The pump's last completed command and at most 32 outstanding records are kept.
This is a bound on command count, not a new byte or RSS bound. Store's original
decision/result path remains responsible for durable evidence. A dropped owner
closes the physical stream before aborting the task. A cancelled closer retains
the original join handle and any already-observed failure. An observed session
error remains separate from observing that the local session has ended.

## Boundaries and validation

The initial source was authored without a usable local Rust toolchain.
Subsequently [CI run 43968540714](https://github.com/cyjin-yl/shenko/actions/runs/43968540714)
ran the original five-window journey successfully: the original workspace log
records `original_pty_writes_observe_flush_cancel_errors_and_channel_loss ... ok`.
Its actual checkout was `1c01c95a4915856312746b5c7ef404c1d7a84cec` (tree
`1ccf8f0bee35915c9f6c4d886b5a7bf54050d694`), merging candidate
`3e62c0e517a246872101cd1e61a694b2156bc919`. That run's Rust job succeeded;
the overall CI run failed on Clippy. This is a test outcome, not preserved
per-write receipts: libtest captured successful `eprintln!` output, so the
archived workspace log did not contain those original observations.

The existing close-cancellation journey and all nine scenarios from the
preceding admission/outcome increment remain unchanged. The continuous journey
uses the existing public fixture keys/trust registration
and real authenticated SSH packet/PTY protocol over bounded streams. Its five
distinct risk windows are:

1. A held underlying flush cannot produce a successful receipt; after release,
   128 KiB in each direction completes and the original submit carriage return
   is present in both the peer transcript and local receipt.
2. A subsequent `INT` request traverses the same FIFO and remains unconfirmed
   application input even when its actual SSH signal message is observed.
3. Cancelling the original blocked write synchronously stops further underlying
   polls, preserves that input as Unknown, and leaves the next queued input
   NotSent even after the stream gate is released.
4. An actual underlying flush error retains its original diagnostic and full
   input, closes that stream, and refuses subsequent admission without replay.
5. Closing the actual SSH channel before its blocked ciphertext flush prevents a
   successful channel-scoped receipt after the stream is released.

The gate controls the underlying stream's actual write/flush readiness or I/O
error. It does not inject a `SeatResponse`, a channel observation, or a receipt.
The original successful test does not supply the missing captured receipt
bytes. Nothing below reconstructs or backfills that run. These protocol checks
are not a remote OpenSSH process, shell, OS PTY, or device acceptance journey.

## Original observation export

The fixture now accepts an explicit `SHENKO_PTY_WRITE_EVIDENCE` directory and
writes `original-pty-write-journey.jsonl` as observations occur. This is an
export-only follow-up: the original five windows, their inputs, assertions and
timeouts are unchanged. No additional scenario is introduced. Without this
environment variable, the original captured-log behavior remains available.

The JSONL records retain the complete original submit arguments and write
observations, the actual response including its complete receipt detail, peer
input/authentication/signal observations, underlying I/O counters, each actual
close result and the original protocol/worker cleanup results. The first data
receipt is saved before the following INT replaces the pump's last completed
write. Errors retain their original Debug or contextual text, labelled by the
observation point; neither a timeout nor an abort is converted into a join.

Each serialized record is written and flushed while the original value is
available. The file uses create-new semantics so a repeated run cannot mix with
or overwrite an earlier run. Source metadata records the workflow-provided SHA,
run and attempt when present, explicitly as reported environment; it does not
invent source identity when those fields are absent. Sequence numbers describe
export attempts, not effects or successful assertions.

An export error is printed and retained, but does not bypass the original
transport cleanup. After that cleanup, it fails the test and retains any
original assertion/transport error. A panic or early dropped owner can save
only the observations already made, with an explicit unfinished marker; no
missing receipt or unobserved join is filled in. A killed process can leave only
a prefix. `journey_result` records the original test result, so that fact alone
is not a claim that the subsequent export succeeded.

The export implementation itself has not been compiled or executed locally:
the environment still has no `cargo`, `rustc` or `rustfmt`. It awaits a new CI
run with the explicit evidence path. The old successful test is not evidence
that this new export has run.

Continuous cross-process installation exclusion, canonical/import/slot mutation
coordination, selection checks on old Store handles, and the managed stdio wire
boundary remain separate work; the [installation permit plan](installation-write-permit-plan.md)
records their callsites and still-unimplemented requirements. Closing this local stream cannot retract bytes
already transmitted, stop unrelated human input, prove a remote process stopped,
or promise a physical effect happened exactly once.
