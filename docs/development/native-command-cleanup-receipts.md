# Native SSH command cleanup observations

The existing single-command native SSH transport closed its original owned
stream and then used `timeout(..., handle).await.is_ok()` to record local session
end. That boolean correctly distinguished a bounded wait from an observed end,
but discarded an error actually returned by the original session handle.

[`NativeSshSession::execute`](../../crates/shenko-exec-ssh/src/native_transport/mod.rs)
now preserves three separate facts in `NativeCommandObservation`:

- `local_stream_closed` records that the original synchronous `OwnedStream`
  close completed before the cleanup wait. Later underlying I/O polls are
  refused by that stream; this is not a claim that the remote process stopped.
- `session_end_observed` records a completed original session handle, including
  an error completion. A cleanup deadline leaves this false.
- `session_error` retains the Debug rendering of that handle's actual error.
  It is absent when no such error was observed, including a timed-out wait;
  absence alone does not prove a successful join.

These are additive fields in the existing observation. They do not replace the
command's original interruption, binary stdout/stderr, exec acceptance, channel
closure, or real remote exit status. In particular, a remote exit 255 remains a
remote status, and a command deadline remains uncertain even if the local
session subsequently ends. The cleanup bound and its validation are unchanged.
Cancelling the whole caller before it returns still leaves no completed
observation; the existing durable T1/recovery path must preserve that uncertainty
and cannot resend the original command.

## Actual consumers

This is an existing execution path, not a new installer or unused permit API:

- [`ssh.exec`](../../crates/shenko-exec-ssh/src/native.rs) serializes the complete
  observation into its original E1 payload. Its conclusive transport rules are
  unchanged; an exit is not an application postcondition.
- [`SshTaskRunner::record_observation`](../../crates/shenko-exec-ssh/src/task_runner.rs)
  seals the complete native observation alongside the original helper receipt
  and dispatch error. The installed `ssh.native` handler already consumes this
  path. No legacy OpenSSH result or stored historical fact is rewritten.

The stored error is specifically the result returned by the original russh
handle. This increment does not claim that every earlier internal protocol
error survives a later library shutdown error, or change the library's error
precedence. It does not introduce a new remote effect, physical-write permit,
trust source, credential choice, retry path or installed tmux capability.

## Validation boundary

The existing real authenticated SSH protocol journeys over bounded in-memory
streams gain assertions for two already exercised paths: a completed 128 KiB
duplex exchange with remote exit 255, and an accepted command that reaches its
original deadline. In each, the server retains its original session until the
client explicitly closes the owned stream. The new assertions require the
actual returned local-close error and preserve all previous output, exit and
uncertainty assertions. They do not fabricate a session result or add a second
execution route.

The two existing `ssh.exec` receipt projection checks also carry the new fields
and assert that serialization preserves them without changing Complete versus
Unknown. Thus four existing checks are amended, of which two perform real SSH
protocol exchanges; no new test function or scenario matrix is added. The prior
nine seat admission/outcome scenarios and five PTY write scenarios are unchanged.

The current local environment has no `cargo`, `rustc` or `rustfmt`. Source review
and `git diff --check` are the available checks for this increment; its Rust
compilation and actual protocol assertions await CI. The full installation
mutation/slot exclusion and explicit native tmux attachment remain separate
work.
