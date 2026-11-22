#!/usr/bin/env python3
"""Summarize opt-in shenko_timing events without summing nested durations.

An end event describes a stack-frame exit, including errors; it is never a
successful operation receipt. Missing ends remain incomplete observations.
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


def summarize(paths):
    scopes = {}
    invalid = []
    dropped = defaultdict(int)
    logs = []
    for source, path in enumerate(paths):
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
                    key = (source, event["pid"], event["scope_id"])
                    dropped[(source, event["pid"])] = max(dropped[(source, event["pid"])], event["dropped_before"])
                    scope = scopes.setdefault(key, {"source": source, "pid": event["pid"],
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
                    invalid.append({"source": source, "line": number, "reason": str(error)})
        logs.append({"path": str(path), "bytes": size, "sha256": digest.hexdigest()})

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
    phase_samples = defaultdict(list)
    for key, scope in scopes.items():
        observation = scope.get("begin", scope.get("end"))
        paired = "begin" in scope and "end" in scope and not scope.get("invalid")
        row = {"source": key[0], "pid": key[1], "scope_id": key[2],
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
            phase_samples[row["phase"]].append(row)
        rows.append(row)

    phases = []
    for phase, samples in sorted(phase_samples.items()):
        values = [sample["inclusive_us"] for sample in samples]
        self_values = [sample["self_us"] for sample in samples if sample["self_us"] is not None]
        phases.append({"phase": phase, "count": len(values), "inclusive_total_us": sum(values),
                       "p50_us": percentile(values, 0.5), "p95_us": percentile(values, 0.95),
                       "max_us": max(values), "self_sample_count": len(self_values),
                       "self_total_us": sum(self_values) if len(self_values) == len(values) else None})
    return {"schema": 1, "logs": logs, "scope_count": len(rows),
            "complete_count": sum(row["status"] == "complete" for row in rows),
            "incomplete_count": sum(row["status"] != "complete" for row in rows),
            "invalid_events": invalid,
            "observed_dropped_events": [{"source": source, "pid": pid, "cumulative_count": count}
                for (source, pid), count in sorted(dropped.items()) if count],
            "phases": phases, "scopes": rows,
            "interpretation": "Durations are monotonic wall time in microseconds, including instrumentation overhead. "
                "Parent totals include child time; never sum the phase totals as end-to-end latency. "
                "self_us subtracts direct children only and is unknown for a source/process with an unobserved parent. "
                "Complete counts describe matched scope pairs, not a complete observation tree or product success. "
                "Scope completion is not a physical send. "
                "started_unix_us is for log correlation only; wall-clock changes do not affect elapsed_us."}


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
    return 1 if report["invalid_events"] or report["observed_dropped_events"] or not report["scope_count"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
