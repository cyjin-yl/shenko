# Request-path timing

The time between a child becoming eligible and its provider being called can
include snapshot construction, authority acquisition, row capture and durable
dispatch admission. The earlier observed delay does not identify which of
those consumed the time. This instrumentation measures those synchronous Store
boundaries on the actual candidate without weakening a witness or changing the
execution protocol.

## Collect

The normal `info` level has no timing output. Enable just the timing target for
the runtime being measured:

```sh
SHENKO_LOG=info,shenko_timing=debug shenko start --port 7860 2>runtime.log
python3 scripts/summarize_request_timing.py runtime.log --output request-timing.json
```

The real single-node driver's `--timing` option passes this filter into the
child's environment. The same report
can consume both `first.runtime.log` and `restart.runtime.log`. Keep the source
revision, binary hash, model identity, installation size and the original logs
with the existing journey evidence. This diagnostic does not call a model or
alter the journey's acceptance predicates.

Each `shenko_timing` record contains a versioned `timing_json` object. It names
the process, scope, parent scope, fixed phase label and available subject,
task, operation, attempt and action identities. It includes no prompt, SQL,
error detail, endpoint or credential. Begin records are sampled before the
measured work; end records carry monotonic wall time in integer microseconds.
The Unix start time is only a log-correlation aid; clock adjustments do not
change the elapsed duration. Correlation keys in the report also contain the
source log, so a reused process ID after restart cannot merge samples.

Events enter a bounded 512-item queue using `try_send`; a separate thread uses
the original tracing subscriber to format and write them. A slow log sink
cannot block a Store mutex or transaction. A full or disconnected queue drops
diagnostic events and increments a process-local counter, reported by the next
delivered event as `dropped_before`. The sink is best effort and not durable:
process termination can lose queued tail events, and the final unseen loss
count is unknown. Preserve those gaps rather than rerun an external effect to
obtain a prettier trace.

## Report

The report is versioned `schema: 2`. Each positional log is one run; inside a
run every observed process keeps its own scope counts, root totals and phase
statistics, so samples from two runs or two processes are never pooled.
Percentiles are nearest-rank per process and a single sample stays a single
sample. The top-level `phases` list is the earlier pooled view kept for readers
of schema 1 reports, so it must not be used to compare runs. The
[report schema](request-path-timing-report.schema.json) states the required
keys, and `scripts/fixtures/request-timing/sample.report.json` is an instance of
that structure for a short sample log.

Time that the instrumentation did not observe stays unknown and is never
estimated:

- `unaccounted.measurement_window_us` is always unknown. No timing event
  records process start or stop, and log transport delay is not observed, so
  the report cannot state how long the run lasted.
- `unaccounted.unaccounted_us` is therefore always unknown as well: time
  outside observed scopes cannot be derived from an unknown window.
- A process's `observed_root_total_us` is a sum only while every root scope of
  that process is a complete begin/end pair and no queue loss was observed for
  it. Otherwise it is `null` and `observed_root_total_unknown` names the
  missing evidence.
- `unmeasured_within_roots_us` sums the root scopes' self time only while every
  root's self time is known. It is the observed top-level time that no observed
  child accounts for; it contains uninstrumented work and instrumentation
  overhead and is not evidence for any particular cause.
- A run whose log contains no timing record is listed in
  `runs_without_observations` and makes the summarizer exit nonzero instead of
  printing an empty but successful report.

## Interpret

| Phase | Measured boundary |
| --- | --- |
| `native.claim`, `native.arm`, `native.dispatch_commit`, `native.t2` | The corresponding canonical Store call, including its nested preparation, admission and application. Native arm may atomically record its original dispatch marker. |
| `canonical.prepare`, `subject.prepare_s1` | Frozen transition preparation, including predecessor capture. Existing prepared plans can return earlier. |
| `control.predecessor` | Complete predecessor construction. |
| `control.snapshot` | Cognition snapshot export, including snapshot rows and object closure. |
| `snapshot.rows` | Consistent factual read plus its original export-order transaction. |
| `snapshot.closure`, `snapshot.validate_facts` | Object traversal and required-fact checks during export. |
| `control.validate`, `control.seal_pages` | Manifest validation and immutable control-tree encoding/publication. |
| `control.inspect`, `control.inspect_closure` | Receiver-side predecessor reconstruction, closure and validation. |
| `sqlite.mutex_wait` | In-process connection mutex acquisition at the instrumented boundaries. |
| `sqlite.begin_immediate`, `sqlite.begin_deferred`, `sqlite.commit` | The actual SQLite call, including any internal wait and I/O. BEGIN time is not asserted to be pure lock contention. |
| `snapshot.cas_publication_wait`, `snapshot.installation_wait`, `authority.installation_wait` | The indicated shared publication or installation acquisition. |
| `capture.rows` | Row-witness capture; nested schema, evaluation, postimage, preimage and closure phases separate its work. |
| `canonical.admission`, `authority.order`, `authority.acquire` | Sealed action ordering and authority acquisition. |
| `canonical.apply`, `subject.apply_s1` | Validation and application of an admitted transition. |
| `materialize.rows` | Precondition checks, exact writes and postcondition checks as distinct nested phases. |

The report lists every observed scope and groups complete pairs by phase
within one process, with sample count and nearest-rank p50/p95/max. A single
sample is not a latency distribution. **Parent elapsed time includes children**:
adding totals across phases double-counts work. `self_us` subtracts only direct
children, and is
unknown if a child's end is missing or its observation is invalid. Self time is
also unknown when the observed drop counter advances during that scope,
including a completely missing child pair. A parent ID missing from the
observed source/process produces an explicit `parent scope not observed`
diagnostic. This can occur when a log is clipped or filtered outside the
producer, even with a zero queue-loss counter. Because the missing parent's
ancestors are then unknown, all `self_us` and corresponding `self_total_us`
values for that source/process remain unknown; surviving grandchildren are
never attributed to an observed ancestor's self time. Complete counts describe
matched begin/end pairs, not a complete observation tree. Remaining
self time includes uninstrumented work and diagnostic overhead; it is not
automatically attributable to the CPU, disk or a lock.

An end record means the stack frame exited, including an early error or unwind.
It proves neither a successful COMMIT nor a physical dispatch. A process killed
mid-scope leaves its begin record incomplete. Malformed or mismatched records
are retained as report diagnostics and make the summarizer exit nonzero; absent
timing records, unobserved parents, observed queue loss or a run without any
timing record also exit nonzero. Missing ends are reported, never given zero
duration or counted as completed samples. Use durable E1/request receipts and
the original journey assertions to decide whether the task actually succeeded.

## Limits

No report field names a cause. The summarizer never converts an unknown into
zero, never rebuilds a scope whose begin or end is missing, and never claims
that an observed duration was spent in a lock, on disk or network I/O, in the
CPU, waiting for a scheduler, or in another task. The following stay
unmeasured and are reported as unknown rather than inferred:

- the run's wall time, and therefore all time outside observed scopes;
- scopes dropped by the bounded queue, truncated from the log, or filtered
  before the summarizer saw them;
- work performed while no instrumented scope was open, including task-queue
  residence, model and network time, and scheduler delay;
- whether the underlying operation succeeded, which still needs the durable
  receipt or the original journey assertion.

Aggregates describe the observed scopes of one process. They are not a latency
service objective, not a percentile of user-visible requests, and not a
comparison between two binaries unless both runs were recorded with the same
source, environment and enabled filters.

## Coverage and remaining work

This measures canonical S1 and native claim/T1/dispatch/T2 Store work. It does
not measure every Store call, task-queue residence, asynchronous model/network
time, or all OS lock waits. A native dispatch-commit scope ends at the durable
decision boundary, before a later physical provider send. The instrumentation
does not remove full-table witnesses, the complete cognition snapshot, closure
validation or conservative conflicts documented in
[capture capacity](canonical-capture-budget.md#remaining-issue-316-boundaries).
Timing-enabled and disabled runs should retain the same original outcomes;
the overhead belongs in the recorded environment, not an assumed correction.

## Scoped verification

This increment keeps eight discriminating checks: one Rust test holds an actual
SQLite transaction while a deliberately blocked subscriber fills the bounded
queue, and seven Python checks detect double-counted descendants, refuse to
attribute missing child time after recorded queue loss or a missing parent in an
externally clipped log, keep two runs that reuse process and scope identifiers
separate, preserve every unknown instead of filling it in, report a run whose
log holds no timing record, assert that no field names a cause, and reproduce
the checked-in sample report from its sample log. They run with:

```sh
cargo test --locked -p shenko-store --lib timing::tests::slow_log_destination_drops_observations_without_blocking_sqlite -- --exact
python3 scripts/summarize_request_timing_test.py
```

The instrumentation does not change SQL, callback counts or transaction order;
the existing canonical/native journeys remain the behavioral regression. Extra
tests mirroring the disabled-target branch, field formatting and routine parser
cases were removed after integration review to keep this one increment within
the repository's test-scope rule. Those branches were reviewed rather than
repackaged into a combined test. The earlier three checks passed locally. The
per-run, unknown-preservation and sample-reproduction checks added here were not
executed locally and must be reported by the same Python runner, together with
the Rust check and the existing Store journeys, against the actual candidate.
