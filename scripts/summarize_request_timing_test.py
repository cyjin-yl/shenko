#!/usr/bin/env python3
"""Discriminating checks for measured timing reports, not latency claims."""
import json
from pathlib import Path
import tempfile
import unittest

from summarize_request_timing import extract, summarize


def event(scope, phase, parent=None, elapsed=None, pid=42, binding=None, dropped=0):
    value = {"schema": 1, "event": "end" if elapsed is not None else "begin", "pid": pid,
             "scope_id": scope, "parent_id": parent, "phase": phase, "started_unix_us": 1000 + scope,
             "binding": binding or {}, "dropped_before": dropped}
    if elapsed is not None:
        value["elapsed_us"] = elapsed
    return "DEBUG shenko_timing: store phase timing_json=" + json.dumps(value) + "\n"


class TimingReportTests(unittest.TestCase):
    def report(self, *logs):
        with tempfile.TemporaryDirectory() as directory:
            paths = []
            for index, text in enumerate(logs):
                path = Path(directory) / f"{index}.log"
                path.write_text(text)
                paths.append(path)
            return summarize(paths)

    def test_nested_phases_subtract_only_immediate_children(self):
        report = self.report(event(1, "native.claim") + event(2, "control.snapshot", 1)
            + event(3, "sqlite.mutex_wait", 2) + event(3, "sqlite.mutex_wait", 2, 20)
            + event(2, "control.snapshot", 1, 80) + event(1, "native.claim", elapsed=100))
        rows = {row["scope_id"]: row for row in report["scopes"]}
        self.assertEqual(rows[1]["self_us"], 20)
        self.assertEqual(rows[2]["self_us"], 60)
        self.assertEqual(rows[3]["self_us"], 20)
        self.assertEqual(report["invalid_events"], [])

    def test_interleaved_processes_and_restarted_logs_keep_distinct_samples(self):
        first = event(1, "native.claim", pid=42) + event(1, "native.claim", pid=43)
        first += event(1, "native.claim", elapsed=100, pid=43) + event(1, "native.claim", elapsed=10, pid=42)
        report = self.report(first, event(1, "native.claim") + event(1, "native.claim", elapsed=1000))
        self.assertEqual(report["complete_count"], 3)
        self.assertEqual(report["phases"][0]["p50_us"], 100)
        self.assertEqual(report["phases"][0]["p95_us"], 1000)

    def test_incomplete_and_mismatched_observations_are_not_success_samples(self):
        report = self.report(event(1, "native.claim") + event(2, "control.snapshot", 1)
            + event(1, "native.claim", elapsed=100)
            + event(3, "canonical.prepare") + event(3, "different", elapsed=5)
            + event(4, "orphan", elapsed=8) + "timing_json={truncated\n")
        self.assertEqual(report["complete_count"], 1)
        self.assertEqual(report["incomplete_count"], 3)
        self.assertIsNone(report["scopes"][0]["self_us"])
        self.assertEqual(len(report["invalid_events"]), 2)

    def test_dropped_children_leave_self_time_unknown(self):
        report = self.report(event(1, "native.claim") + event(1, "native.claim", elapsed=100, dropped=2))
        self.assertEqual(report["complete_count"], 1)
        self.assertIsNone(report["scopes"][0]["self_us"])
        self.assertEqual(report["scopes"][0]["observed_drops_during_scope"], 2)
        self.assertEqual(report["observed_dropped_events"][0]["cumulative_count"], 2)

    def test_ansi_logs_keep_bindings_and_ignore_unrelated_log_text(self):
        raw = event(1, "native.claim", binding={"task_id": "original-task"})
        raw = raw.replace("timing_json=", "\x1b[3mtiming_json\x1b[0m\x1b[2m=\x1b[0m")
        self.assertEqual(extract(raw)["binding"], {"task_id": "original-task"})
        self.assertIsNone(extract("INFO unrelated model text\n"))


if __name__ == "__main__":
    unittest.main()
