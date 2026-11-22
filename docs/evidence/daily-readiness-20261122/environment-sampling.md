# Bounded environment sampling for the final journey

`sample-environment.py` is a Python 3 standard-library evidence helper, not
product runtime code. It samples the Linux counters visible through its own
`/proc`: PSI `io`, `cpu` and `memory`, load averages, runnable/total task counts,
and selected memory counters including `MemAvailable`, `Dirty`, `Writeback`
and `SwapFree`. Every record has UTC, monotonic nanoseconds and elapsed
nanoseconds; each sample also records how long its reads took.

The default interval is five seconds. A positive finite `--max-duration` is
required. The helper follows the specified PID and original `/proc/PID/stat`
start ticks; disappearance, zombie/exit state or replacement ends sampling.
It retains only that process's state and start ticks. It does not read process
arguments, environment variables, descriptors, other process records, Store
databases or model endpoints, and it never signals the tracked process.

## Runner invocation

Use the already-owned journey driver PID in the **same PID namespace** as the
sampler. In a Docker run, start both inside the same container; a host PID
does not identify the same process inside that container.

After starting the configured journey command as a child and retaining its
PID as `journey_runner_pid`, start the sampler alongside it:

```sh
python3 -B docs/evidence/daily-readiness-20261122/sample-environment.py \
  --parent-pid "$journey_runner_pid" \
  --interval 5 \
  --max-duration 7200 \
  --output "$journey_evidence_dir/environment.ndjson" &
environment_sampler_pid=$!

journey_status=0
wait "$journey_runner_pid" || journey_status=$?
wait "$environment_sampler_pid"
exit "$journey_status"
```

The evidence directory must already exist and the output file must be new;
exclusive creation prevents replacement or mixing of observations. Bind the
actual driver child, not a shell which will wait for the sampler itself.
After the driver exits, the sampler observes that boundary within one sampling
interval. TERM and INT wake its wait immediately and produce an `end` record.

## Evidence and limits

NDJSON consists of one `start`, sequential `sample` records and one `end`.
The start record binds sampler SHA256, parent identity, interval and duration.
The end records its reason, sample count and metric-error count. Missing or
unreadable counters remain `null` with a typed error; they are never zero
pressure. Memory values use `/proc/meminfo`'s 1024-byte `kB` units. PSI averages
remain the kernel's `avg10`/`avg60`/`avg300` values and `total` remains
microseconds; no task-level attribution is inferred from system counters.

The only writes are the requested observation file. It is line-flushed and
closed at exit, with no added `fsync` workload and no claim of power-loss
durability. An incomplete file remains visibly incomplete if the process or
machine is killed. These samples can show coincident environment changes;
they cannot establish which Store phase or device caused a delay.

`environment-sampler-smoke.json` and `environment-sampler-smoke/*.ndjson`
retain four short real-process checks in the local editing environment:
maximum duration, tracked parent exit, TERM and INT. All four exited zero
with an end record. This environment has no `/proc/pressure/{io,cpu,memory}`;
those missing counters are retained as three errors per sample. Load and
memory were available. This is a lifecycle and partial-data check, not a
successful remote PSI observation or a product regression test.
