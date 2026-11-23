#!/usr/bin/env python3
"""Summarize opt-in shenko_timing events per run without inventing durations.

An end event describes a stack-frame exit, including errors; it is never a
successful operation receipt. Missing ends remain incomplete observations.
Only observed scopes are aggregated: the report never reconstructs a lost
scope, never turns an unknown into zero and never attributes an observed
duration to a cause.
"""
import argparse
from collections import defaultdict
import hashlib
import json
import math
from pathlib import Path
import re
import sys

ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
IDENTITY = ("phase", "parent_id", "started_unix_us", "binding")

# Fixed reasons for the quantities that stay unknown. Each one names missing
# evidence only. None of them infers a blocking resource or a cause.
UNKNOWN_NO_ROOT = "no root scope was observed for this process"
UNKNOWN_ROOT_INCOMPLETE = "at least one root scope has no complete end"
UNKNOWN_ROOT_LOST = "observed queue loss means an unobserved root scope cannot be ruled out"
UNKNOWN_ROOT_MISSING_PARENT = "a scope's parent is missing, so this process's tree is incomplete"
UNKNOWN_SELF = "self time is unknown for at least one root scope"
UNKNOWN_NO_PROCESS = "no timing scope was observed in this run"
UNKNOWN_RUN_ROOT = "at least one process in this run has an unknown observed root total"
UNKNOWN_WINDOW = "no event records process start or stop, and log transport delay is unobserved"
UNKNOWN_UNACCOUNTED = "the observation window is unknown, so time outside observed scopes cannot be computed"

LIMITS = [
    "Only observed scopes are aggregated; a missing begin or end is reported, never reconstructed and never given a zero duration.",
    "An observed duration has no inferred cause. No field names a lock, disk, CPU, network, scheduler or other resource as responsible for it.",
    "A scope end is a stack-frame exit. It is not a successful commit, a durable receipt or a physical send.",
    "A parent duration includes its children. Phase inclusive totals must never be summed as end-to-end latency.",
    "Per-run and per-process phase percentiles are never pooled. The top-level phases list is the earlier pooled view for schema 1 readers and is not a run-to-run comparison.",
    "The observation window is unknown because no event records process start or stop; unaccounted time outside observed scopes is therefore unknown rather than computed.",
    "Start times exist only to correlate a log line with a scope. Elapsed durations come from a monotonic clock, so wall-clock changes do not move them.",
    "These observations are diagnostics. They never change durable authority, transaction order, callback counts or an execution receipt.",
]

INTERPRETATION = (
    "Durations are monotonic wall time in microseconds, including instrumentation overhead. "
    "Aggregates are reported per run and per process; never sum phase totals as end-to-end latency "
    "because parent durations include children. self_us subtracts direct children only and is unknown "
    "for a source/process with an unobserved parent or observed queue loss. "
    "A null value with an accompanying *_unknown reason is an explicit unknown, not zero. "
    "Complete counts describe matched scope pairs, not a complete observation tree or product success. "
    "Scope completion is not a physical send and implies no cause. "
    "started_unix_us is for log correlation only; wall-clock changes do not affect elapsed_us."
)


def extract(line):
    line = ANSI.sub("", line)
    marker = "timing_json="
    if marker not in line:
        return None
    event, _ = json.JSONDecoder().raw_decode(line.split(marker, 1)[1])
    if not isinstance(event, dict) or event.get("schema") != 1:
        raise ValueError("unsupported timing schema")
    if event.get("event") not in ("begin", "end"):
        raise ValueError("unknown timing event")
    for field in ("pid", "scope_id", "started_unix_us", "dropped_before"):
        if type(event.get(field)) is not int or event[field] < 0:
            raise ValueError(f"invalid {field}")
    if "parent_id" not in event or (event["parent_id"] is not None and (
            type(event["parent_id"]) is not int or event["parent_id"] < 0)):
        raise ValueError("invalid parent_id")
    if not isinstance(event.get("phase"), str) or not isinstance(event.get("binding"), dict):
        raise ValueError("missing phase or binding")
    if event["event"] == "end" and (type(event.get("elapsed_us")) is not int or event["elapsed_us"] < 0):
        raise ValueError("invalid elapsed_us")
    return event


def percentile(values, quantile):
    """Nearest-rank percentile; a single observation stays a single sample."""
    return sorted(values)[max(0, math.ceil(quantile * len(values)) - 1)]


def phase_aggregates(rows):
    """Aggregate complete rows by phase. Incomplete rows are never samples."""
    samples = defaultdict(list)
    for row in rows:
        if row["status"] == "complete":
            samples[row["phase"]].append(row)
    phases = []
    for phase, phase_rows in sorted(samples.items()):
        values = [row["inclusive_us"] for row in phase_rows]
        self_values = [row["self_us"] for row in phase_rows if row["self_us"] is not None]
        phases.append({"phase": phase, "count": len(values), "inclusive_total_us": sum(values),
                       "p50_us": percentile(values, 0.5), "p95_us": percentile(values, 0.95),
                       "max_us": max(values), "self_sample_count": len(self_values),
                       "self_total_us": sum(self_values) if len(self_values) == len(values) else None})
    return phases


def process_summary(pid, rows, dropped_before, tree_incomplete):
    """One observed process: counts, root totals and phase statistics.

    Root scopes of a synchronous stack cannot overlap, so their measured
    inclusive durations add up while every root is a complete pair and no
    observation was dropped. Anything else stays unknown.
    """
    complete = [row for row in rows if row["status"] == "complete"]
    roots = [row for row in rows if row["parent_id"] is None]
    if not roots:
        unknown_root = UNKNOWN_NO_ROOT
    elif any(row["status"] != "complete" for row in roots):
        unknown_root = UNKNOWN_ROOT_INCOMPLETE
    elif dropped_before:
        unknown_root = UNKNOWN_ROOT_LOST
    else:
        unknown_root = None
    observed_root_total = None if unknown_root else sum(row["inclusive_us"] for row in roots)
    if unknown_root:
        unknown_self = unknown_root
    elif tree_incomplete:
        unknown_self = UNKNOWN_ROOT_MISSING_PARENT
    elif any(row["self_us"] is None for row in roots):
        unknown_self = UNKNOWN_SELF
    else:
        unknown_self = None
    unmeasured = None if unknown_self else sum(row["self_us"] for row in roots)
    return {"pid": pid, "scope_count": len(rows), "complete_count": len(complete),
            "incomplete_count": len(rows) - len(complete),
            "observed_dropped_before_max": dropped_before, "root_scope_count": len(roots),
            "observed_root_total_us": observed_root_total,
            "observed_root_total_unknown": unknown_root,
            "unmeasured_within_roots_us": unmeasured,
            "unmeasured_within_roots_unknown": unknown_self,
            "phases": phase_aggregates(rows)}


def summarize(paths):
    scopes = {}
    invalid = []
    dropped = defaultdict(int)
    logs = []
    for run, path in enumerate(paths):
        digest = hashlib.sha256()
        size = 0
        with path.open("rb") as log:
            for number, raw in enumerate(log, 1):
                digest.update(raw)
                size += len(raw)
                try:
                    event = extract(raw.decode("utf-8", errors="replace"))
                    if event is None:
                        continue
                    key = (run, event["pid"], event["scope_id"])
                    dropped[(run, event["pid"])] = max(dropped[(run, event["pid"])], event["dropped_before"])
                    scope = scopes.setdefault(key, {"run": run, "pid": event["pid"],
                                                    "scope_id": event["scope_id"]})
                    kind = event["event"]
                    if kind in scope:
                        scope["invalid"] = True
                        raise ValueError(f"duplicate {kind}")
                    scope[kind] = event
                    if "begin" in scope and "end" in scope and any(
                            scope["begin"][field] != scope["end"][field] for field in IDENTITY):
                        scope["invalid"] = True
                        raise ValueError("begin/end identity mismatch")
                except (ValueError, KeyError) as error:
                    invalid.append({"source": run, "line": number, "reason": str(error)})
        logs.append({"run": run, "path": str(path), "bytes": size, "sha256": digest.hexdigest()})

    children = defaultdict(list)
    incomplete_trees = set()
    for key, scope in scopes.items():
        observation = scope.get("begin", scope.get("end"))
        if observation["parent_id"] is not None:
            parent = (key[0], key[1], observation["parent_id"])
            children[parent].append(scope)
            if parent not in scopes:
                invalid.append({"source": key[0], "pid": key[1], "scope_id": key[2],
                                "parent_id": parent[2], "reason": "parent scope not observed"})
                # A clipped/filtered log can lose an intermediate parent pair
                # without changing the producer's queue-loss counter. We know
                # the child work exists but cannot locate its missing parent's
                # ancestors, so do not attribute any self time in this tree.
                incomplete_trees.add((key[0], key[1]))

    rows = []
    for key, scope in scopes.items():
        observation = scope.get("begin", scope.get("end"))
        paired = "begin" in scope and "end" in scope and not scope.get("invalid")
        row = {"run": key[0], "source": key[0], "pid": key[1], "scope_id": key[2],
               **{field: observation[field] for field in IDENTITY},
               "status": "complete" if paired else "invalid" if scope.get("invalid") else
                   "incomplete" if "begin" in scope else "missing_begin"}
        if paired:
            row["inclusive_us"] = scope["end"]["elapsed_us"]
            row["observed_drops_during_scope"] = max(0, scope["end"]["dropped_before"] - scope["begin"]["dropped_before"])
            direct = children[key]
            if (key[0], key[1]) not in incomplete_trees and not row["observed_drops_during_scope"] and all(
                    "begin" in child and "end" in child and not child.get("invalid") for child in direct):
                nested = sum(child["end"]["elapsed_us"] for child in direct)
                # Equal-level phases are synchronous and cannot overlap. A
                # corrupt/truncated tree must not manufacture a negative cost.
                if nested <= row["inclusive_us"]:
                    row["self_us"] = row["inclusive_us"] - nested
                else:
                    row["self_us"] = None
                    invalid.append({"source": key[0], "scope_id": key[2],
                                    "reason": "direct child time exceeds parent"})
            else:
                row["self_us"] = None
        rows.append(row)

    runs = []
    for run, log in enumerate(logs):
        run_rows = [row for row in rows if row["run"] == run]
        processes = [process_summary(pid, [row for row in run_rows if row["pid"] == pid],
                                     dropped[(run, pid)], (run, pid) in incomplete_trees)
                     for pid in sorted({row["pid"] for row in run_rows})]
        known = bool(processes) and all(item["observed_root_total_us"] is not None for item in processes)
        runs.append({"run": run, "path": log["path"], "bytes": log["bytes"], "sha256": log["sha256"],
                     "scope_count": len(run_rows),
                     "complete_count": sum(row["status"] == "complete" for row in run_rows),
                     "incomplete_count": sum(row["status"] != "complete" for row in run_rows),
                     "processes": processes,
                     "observed_root_total_us": sum(item["observed_root_total_us"] for item in processes)
                         if known else None,
                     "observed_root_total_unknown": None if known else
                         (UNKNOWN_NO_PROCESS if not processes else UNKNOWN_RUN_ROOT),
                     "unaccounted": {"measurement_window_us": None,
                                     "measurement_window_unknown": UNKNOWN_WINDOW,
                                     "unaccounted_us": None,
                                     "unaccounted_unknown": UNKNOWN_UNACCOUNTED}})

    return {"schema": 2, "logs": logs, "scope_count": len(rows),
            "complete_count": sum(row["status"] == "complete" for row in rows),
            "incomplete_count": sum(row["status"] != "complete" for row in rows),
            "runs_without_observations": [run["run"] for run in runs if not run["scope_count"]],
            "invalid_events": invalid,
            "observed_dropped_events": [{"source": source, "pid": pid, "cumulative_count": count}
                for (source, pid), count in sorted(dropped.items()) if count],
            "runs": runs, "phases": phase_aggregates(rows), "scopes": rows,
            "limits": LIMITS, "interpretation": INTERPRETATION}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", nargs="+", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    report = summarize(args.logs)
    body = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.output:
        args.output.write_text(body)
    else:
        sys.stdout.write(body)
    if report["scope_count"] == 0:
        print("No timing observations; enable SHENKO_LOG=info,shenko_timing=debug for the measured runtime.", file=sys.stderr)
    else:
        for run in report["runs_without_observations"]:
            print(f"Run {run} contains no timing observation; its unaccounted time stays unknown.", file=sys.stderr)
    return 1 if report["invalid_events"] or report["observed_dropped_events"] or report["runs_without_observations"] or not report["scope_count"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
