# Native seat fleet — observed validation (2026-11-11)

`seat.fleet:1` discovers durable seats without a task UUID and optionally filters
by the owning task. It returns up to 32 records, a truncation flag, durable
history and bounded observations of already attached transports. The functional
contract is [Remote seats](../specs/remote-seats.md#fleet-observations).

## Sources and component checks

The measured production source is commit
`705a1fcbbaabad2a585c15d7d7706d8adfa478c1`, including merged main `0b31283`.
The final documentation and a Unix-only guard on the shell lifecycle test were
added after measurement; neither changes the production code or model input.

The initial component run passed 92 `shenko-exec-ssh` library tests, the native
fleet integration case and 10 Store seat-lifecycle cases. After making the task
selector optional, the targeted runtime, native-dispatch and Store inventory
cases passed. The Store case initially assumed a UUID sort order for records
created in the same commit; that incidental-order assertion was replaced by
checks of the actual stable identities and history. The original failed run
remains in the retained evidence.

The native case uses actual task claim, frozen input, original operation,
external evidence and CAS receipts. It checks bounded inventory, open-question
expiry, unchanged writer authority and settled reconciliation returning the
original receipt after a new seat is registered. The runtime case observes a
real owned process before and after exit, then reopens the durable Store; it
does not establish protocol readiness or the state of a remote agent.

## Served-model discovery measurement

The env-gated harness is
`crates/shenko-exec-host/tests/seat_fleet_behavior.rs`. It runs real product
subject, broker, parser, native dispatcher and Store/CAS paths in nine fresh
installations. The task does not name the handler or supply a task UUID. A
complete snapshot requires a consumed task result linked through its original
operation to evidence containing the actual randomly generated seat identities.

| Scenario | Observed result |
| --- | --- |
| Handler installed | Complete committed fleet snapshot in 3/3 trials |
| Handler absent | Complete committed fleet snapshot in 0/3 trials |
| Unrelated task with handler installed | No fleet invocation in 3/3 trials |

All nine trials completed; the report has no fatal error. The endpoint's model
metadata before the run identified alias `swift-qwen3.8-27b-nvfp4`, checkpoint
`Merkyor-Coder390-NVFP4-W4A16-3e644f23`, and `max_model_len: 262144`. This is the
owner's served Coder 390 checkpoint. No post-run metadata observation is claimed.
The small sample measures discovery and committed inventory consumption, not
final-answer correctness, general model efficacy, remote control or long-run
reliability.

The original run is `fleet-model-20261111-r1`, report
`sandboxes/run-98af4576-4633-4069-9069-ee17bd33c211/report.json`. Original
attempts, Store/CAS roots and the report were retained; no rerun replaced them.

## CLI consumer compilation

`cargo check --offline --locked -p shenko -j2` passed in the development
container using the worktree-specific target
`/var/idw-target/worktrees/a9cd18177a81`. The receipt records exit 0 and equal
source hashes before and after. An earlier check in the shared target reported
missing Store symbols despite correct source exports and Cargo package paths;
that failure is retained. Only the build namespace changed for the passing run.
Stale shared metadata is the inferred cause, not a diagnosed Cargo defect.

The final receipt SHA-256 is
`695e47dc902be69f4f698e0964eca13784a8dc0d6d54239aca67c25776c89afe`;
the log SHA-256 is
`717ca052e6c607840c6ef160af5bb07e4a612c36a42bc437b3c1765aa4b8b29b`.
Both copied files were compared with the original container bytes under
`build/fleet-cli-provenance-20261111-8e93afc1/retained-r2/transfer.json`.

## Retained evidence boundary

The private frozen archive is
`build/evidence-archive-fleet1111-r1/fleet-evidence-20261111-r1.zip`, SHA-256
`69957c7a2b80137d597e947732104912a303270dd757051bf5558f91f55aee79`.
It contains the complete nine-world model runtime, the failed CLI run, selected
receipt-bound source files and prior component receipts. Its payload has 564
members; the ZIP has 565 including `manifest.json`. Independent readback
reported no problems for these declared scopes. It is not a dependency or
toolchain backup. The passing CLI run is retained separately as described above.

Archival-attempt retention was violated: the archive worker deleted three of
its generated ZIP drafts and overwrote generated sidecars. Their old bytes are
unavailable; original product sources, runtime roots and test receipts were
preserved. The original closure's unqualified “no deletions” statement is
superseded by the additive
`build/evidence-archive-fleet1111-r1/CLOSURE.correction-20261111.json`.
Current-snapshot integrity does not establish complete retention of failed
archival drafts or runtime replay. No requirement or acceptance case is marked
fully satisfied solely by this measurement.
