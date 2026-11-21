#!/usr/bin/env python3
"""Discriminating checks for measured timing reports, not latency claims."""
import json
from pathlib import Path
import tempfile
import unittest

from summarize_request_timing import summarize


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

    def test_dropped_children_leave_self_time_unknown(self):
        report = self.report(event(1, "native.claim") + event(1, "native.claim", elapsed=100, dropped=2))
        self.assertEqual(report["complete_count"], 1)
        self.assertIsNone(report["scopes"][0]["self_us"])
        self.assertEqual(report["scopes"][0]["observed_drops_during_scope"], 2)
        self.assertEqual(report["observed_dropped_events"][0]["cumulative_count"], 2)



if __name__ == "__main__":
    unittest.main()
