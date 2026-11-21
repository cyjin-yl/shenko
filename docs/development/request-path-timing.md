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

The real single-node driver owns the child's environment; it must pass this
filter to the child rather than replace it with plain `info`. The same report
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

The report lists every observed scope and groups complete pairs by phase with
sample count and nearest-rank p50/p95/max. A single sample is not a latency
distribution. **Parent elapsed time includes children**: adding totals across
phases double-counts work. `self_us` subtracts only direct children, and is
unknown if a child's end is missing or its observation is invalid. Self time is
also unknown when the observed drop counter advances during that scope,
including a completely missing child pair. Remaining
self time includes uninstrumented work and diagnostic overhead; it is not
automatically attributable to the CPU, disk or a lock.

An end record means the stack frame exited, including an early error or unwind.
It proves neither a successful COMMIT nor a physical dispatch. A process killed
mid-scope leaves its begin record incomplete. Malformed or mismatched records
are retained as report diagnostics and make the summarizer exit nonzero; absent
timing records or observed queue loss also exit nonzero. Missing ends are reported, never given zero
duration or counted as completed samples. Use durable E1/request receipts and
the original journey assertions to decide whether the task actually succeeded.

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
