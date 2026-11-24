# Request-path timing sample

`sample.runtime.log.txt` is a short hand-written log in the format the runtime
emits under `SHENKO_LOG=info,shenko_timing=debug`: an unrelated `info` line,
then `shenko_timing` records that each carry a versioned `timing_json` object.
It uses a `.log.txt` name because the repository ignores `*.log`, and this
fixture must stay tracked. `sample.report.json` is the `schema: 2` report that
`scripts/summarize_request_timing.py` produces for that log.

`scripts/summarize_request_timing_test.py` runs the summarizer on the log and
compares everything except the log path plus its recorded byte count and digest,
so the sample cannot silently drift away from the code. The structure of the
report is documented in
[request-path-timing-report.schema.json](../../../docs/development/request-path-timing-report.schema.json).

The sample shows the explicit unknowns instead of a prettier trace:
`unaccounted.measurement_window_us` and `unaccounted.unaccounted_us` are null
because no event records process start or stop, while the four observed scopes
keep their measured durations. It is not a latency measurement, it is not
evidence for any real run, and it does not show a cause for any duration.
