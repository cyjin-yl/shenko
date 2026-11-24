#!/usr/bin/env python3
"""Discriminating checks for measured timing reports, not latency claims."""
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
from summarize_request_timing import (UNKNOWN_NO_PROCESS, UNKNOWN_ROOT_INCOMPLETE, UNKNOWN_RUN_ROOT,
                                      summarize)

def event(scope, phase, parent=None, elapsed=None, pid=42, binding=None, dropped=0):
    value = {"schema": 1, "event": "end" if elapsed is not None else "begin", "pid": pid,
             "scope_id": scope, "parent_id": parent, "phase": phase, "started_unix_us": 1000 + scope,
             "binding": binding or {}, "dropped_before": dropped}
    if elapsed is not None:
        value["elapsed_us"] = elapsed
    return "DEBUG shenko_timing: store phase timing_json=" + json.dumps(value) + "\n"

REPORT_KEYS = {"schema", "logs", "scope_count", "complete_count", "incomplete_count",
               "runs_without_observations", "invalid_events", "observed_dropped_events",
               "runs", "phases", "scopes", "limits", "interpretation"}
LOG_KEYS = {"run", "path", "bytes", "sha256"}
RUN_KEYS = {"run", "path", "bytes", "sha256", "scope_count", "complete_count", "incomplete_count",
            "processes", "observed_root_total_us", "observed_root_total_unknown", "unaccounted"}
PROCESS_KEYS = {"pid", "scope_count", "complete_count", "incomplete_count",
                "observed_dropped_before_max", "root_scope_count", "observed_root_total_us",
                "observed_root_total_unknown", "unmeasured_within_roots_us",
                "unmeasured_within_roots_unknown", "phases"}
PHASE_KEYS = {"phase", "count", "inclusive_total_us", "p50_us", "p95_us", "max_us",
              "self_sample_count", "self_total_us"}
UNACCOUNTED_KEYS = {"measurement_window_us", "measurement_window_unknown", "unaccounted_us",
                    "unaccounted_unknown"}
CAUSE_KEYS = {"cause", "causes", "blocked_by", "root_cause", "attributed_to"}

class TimingReportTests(unittest.TestCase):
    def report(self, *logs):
        with tempfile.TemporaryDirectory() as directory:
            paths = []
            for index, text in enumerate(logs):
                path = Path(directory) / f"{index}.log"
                path.write_text(text)
                paths.append(path)
            return summarize(paths)

    def assertReportShape(self, report):
        self.assertEqual(set(report), REPORT_KEYS)
        self.assertEqual(report["schema"], 2)
        self.assertTrue(report["limits"])
        for log in report["logs"]:
            self.assertEqual(set(log), LOG_KEYS)
        for run in report["runs"]:
            self.assertEqual(set(run), RUN_KEYS)
            self.assertEqual(set(run["unaccounted"]), UNACCOUNTED_KEYS)
            self.assertIsNone(run["unaccounted"]["measurement_window_us"])
            self.assertTrue(run["unaccounted"]["measurement_window_unknown"])
            self.assertIsNone(run["unaccounted"]["unaccounted_us"])
            self.assertTrue(run["unaccounted"]["unaccounted_unknown"])
            self.assertEqual(run["observed_root_total_us"] is None,
                             isinstance(run["observed_root_total_unknown"], str))
            for process in run["processes"]:
                self.assertEqual(set(process), PROCESS_KEYS)
                self.assertEqual(process["observed_root_total_us"] is None,
                                 isinstance(process["observed_root_total_unknown"], str))
                self.assertEqual(process["unmeasured_within_roots_us"] is None,
                                 isinstance(process["unmeasured_within_roots_unknown"], str))
                for phase in process["phases"]:
                    self.assertEqual(set(phase), PHASE_KEYS)
                    self.assertGreater(phase["count"], 0)

    def test_nested_phases_subtract_only_immediate_children(self):
        report = self.report(event(1, "native.claim") + event(2, "control.snapshot", 1)
            + event(3, "sqlite.mutex_wait", 2) + event(3, "sqlite.mutex_wait", 2, 20)
            + event(2, "control.snapshot", 1, 80) + event(1, "native.claim", elapsed=100))
        rows = {row["scope_id"]: row for row in report["scopes"]}
        self.assertEqual(rows[1]["self_us"], 20)
        self.assertEqual(rows[2]["self_us"], 60)
        self.assertEqual(rows[3]["self_us"], 20)
        self.assertEqual(report["invalid_events"], [])
        self.assertReportShape(report)

    def test_dropped_children_leave_self_time_unknown(self):
        report = self.report(event(1, "native.claim") + event(1, "native.claim", elapsed=100, dropped=2))
        self.assertEqual(report["complete_count"], 1)
        self.assertIsNone(report["scopes"][0]["self_us"])
        self.assertEqual(report["scopes"][0]["observed_drops_during_scope"], 2)
        self.assertEqual(report["observed_dropped_events"][0]["cumulative_count"], 2)

        # External log clipping can remove a parent pair without a producer
        # queue drop. Its surviving child still proves that work is missing.
        clipped = self.report(event(1, "native.claim") + event(3, "sqlite.mutex_wait", 2)
            + event(3, "sqlite.mutex_wait", 2, 20) + event(1, "native.claim", elapsed=100))
        self.assertEqual(clipped["observed_dropped_events"], [])
        self.assertEqual(clipped["invalid_events"], [{"source": 0, "pid": 42, "scope_id": 3,
            "parent_id": 2, "reason": "parent scope not observed"}])
        self.assertTrue(all(row["self_us"] is None for row in clipped["scopes"]))
        self.assertTrue(all(phase["self_total_us"] is None for phase in clipped["phases"]))
        self.assertIsNone(clipped["runs"][0]["processes"][0]["unmeasured_within_roots_us"])

    def test_each_run_is_aggregated_separately_even_with_reused_identifiers(self):
        first = event(1, "native.claim") + event(1, "native.claim", elapsed=100)
        # The restart reuses the same process and scope identifiers. Only the
        # positional log identifies the run, so these samples must stay apart.
        second = event(1, "native.claim") + event(1, "native.claim", elapsed=7)
        report = self.report(first, second)
        self.assertEqual([run["scope_count"] for run in report["runs"]], [1, 1])
        self.assertEqual([run["processes"][0]["phases"][0]["inclusive_total_us"]
                          for run in report["runs"]], [100, 7])
        self.assertEqual(report["runs"][0]["observed_root_total_us"], 100)
        self.assertEqual(report["runs"][1]["observed_root_total_us"], 7)
        self.assertEqual(report["scope_count"], 2)
        self.assertEqual(report["invalid_events"], [])
        # The legacy pooled list still merges both runs and must not be read as
        # one latency distribution.
        self.assertEqual([phase["count"] for phase in report["phases"]], [2])
        self.assertReportShape(report)

    def test_unaccounted_time_stays_unknown_instead_of_being_computed(self):
        paired = self.report(event(1, "native.claim") + event(2, "control.snapshot", 1)
            + event(2, "control.snapshot", 1, 60) + event(1, "native.claim", elapsed=100))
        run = paired["runs"][0]
        self.assertIsNone(run["unaccounted"]["measurement_window_us"])
        self.assertIsNone(run["unaccounted"]["unaccounted_us"])
        self.assertEqual(run["observed_root_total_us"], 100)
        self.assertIsNone(run["observed_root_total_unknown"])
        self.assertEqual(run["processes"][0]["unmeasured_within_roots_us"], 40)
        self.assertIsNone(run["processes"][0]["unmeasured_within_roots_unknown"])

        incomplete = self.report(event(1, "native.claim") + event(2, "control.snapshot", 1)
            + event(2, "control.snapshot", 1, 60))
        run = incomplete["runs"][0]
        self.assertIsNone(run["observed_root_total_us"])
        self.assertEqual(run["observed_root_total_unknown"], UNKNOWN_RUN_ROOT)
        self.assertIsNone(run["unaccounted"]["unaccounted_us"])
        process = run["processes"][0]
        self.assertIsNone(process["observed_root_total_us"])
        self.assertEqual(process["observed_root_total_unknown"], UNKNOWN_ROOT_INCOMPLETE)
        self.assertIsNone(process["unmeasured_within_roots_us"])
        self.assertEqual(incomplete["incomplete_count"], 1)
        self.assertEqual([phase["phase"] for phase in process["phases"]], ["control.snapshot"])
        self.assertReportShape(incomplete)

    def test_a_run_without_timing_records_is_reported_not_silently_skipped(self):
        report = self.report(event(1, "native.claim") + event(1, "native.claim", elapsed=100),
                             "INFO shenko: no timing target enabled\n")
        self.assertEqual(report["runs_without_observations"], [1])
        self.assertEqual(report["runs"][1]["scope_count"], 0)
        self.assertEqual(report["runs"][1]["processes"], [])
        self.assertIsNone(report["runs"][1]["observed_root_total_us"])
        self.assertEqual(report["runs"][1]["observed_root_total_unknown"], UNKNOWN_NO_PROCESS)
        self.assertIsNone(report["runs"][1]["unaccounted"]["unaccounted_us"])
        self.assertReportShape(report)

    def test_no_field_assigns_a_cause_to_an_observed_duration(self):
        report = self.report(event(1, "native.claim") + event(2, "sqlite.mutex_wait", 1)
            + event(2, "sqlite.mutex_wait", 1, 20) + event(1, "native.claim", elapsed=100))
        found = set()

        def keys(value):
            if isinstance(value, dict):
                found.update(value)
                for item in value.values():
                    keys(item)
            elif isinstance(value, list):
                for item in value:
                    keys(item)

        keys(report)
        self.assertFalse(found & CAUSE_KEYS)

    def test_checked_in_sample_log_reproduces_its_sample_report(self):
        fixture = Path(__file__).resolve().parent / "fixtures" / "request-timing"
        report = summarize([fixture / "sample.runtime.log.txt"])
        sample = json.loads((fixture / "sample.report.json").read_text())
        self.assertReportShape(report)
        self.assertReportShape(sample)
        self.assertEqual(report["logs"][0]["bytes"], sample["logs"][0]["bytes"])
        self.assertEqual(report["logs"][0]["sha256"], sample["logs"][0]["sha256"])
        self.assertEqual(report["limits"], sample["limits"])
        self.assertEqual(report["runs_without_observations"], sample["runs_without_observations"])

        def stable(run):
            return {key: value for key, value in run.items() if key not in ("path", "bytes", "sha256")}

        self.assertEqual([stable(run) for run in report["runs"]],
                         [stable(run) for run in sample["runs"]])

if __name__ == "__main__":
    unittest.main()
