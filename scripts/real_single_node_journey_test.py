#!/usr/bin/env python3
"""Driver evidence and owned-process cleanup; these are not model acceptance."""
import argparse
import asyncio
import contextlib
import copy
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import real_single_node_journey as journey


class JourneyEvidenceTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform.startswith("linux"), "observes Linux process groups and zombies")
    def test_exited_parent_does_not_hide_live_owned_child(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "runtime.log"
            log = log_path.open("w")
            # The child announces its PID only after installing SIGTERM ignore.
            # Its leader exits 0, so escalation must not rewrite the leader's
            # real exit status. A separate session must remain untouched.
            child_code = "import os,signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); print(os.getpid(),flush=True); time.sleep(30)"
            parent_code = ("import subprocess,sys; "
                           f"child=subprocess.Popen([sys.executable,'-c',{child_code!r}],stdout=subprocess.PIPE,text=True); "
                           "print(child.stdout.readline().strip(),flush=True)")
            process = journey.spawn_owned_runtime([sys.executable, "-c", parent_code],
                                                   stdout=log, stderr=subprocess.STDOUT)
            unrelated = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"],
                                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                         start_new_session=True)
            try:
                self.assertEqual(process.wait(timeout=5), 0)
                child_pid = int(log_path.read_text().strip())
                receipt = asyncio.run(journey.stop(process, log, terminate_timeout=0.1, kill_timeout=2))
                self.assertEqual(receipt["parent_exit"], 0)
                self.assertFalse(receipt["clean"])
                self.assertEqual(receipt["errors"], [])
                cleanup = receipt["group_cleanup"]
                self.assertEqual(cleanup["after_parent"]["live_pids"], [child_pid])
                self.assertTrue(cleanup["required"])
                self.assertTrue(cleanup["forced"])
                self.assertEqual(cleanup["signals"], ["SIGTERM", "SIGKILL"])
                self.assertTrue(cleanup["complete"])
                self.assertEqual(cleanup["remaining"]["live_pids"], [])
                self.assertTrue(log.closed)
                self.assertIsNone(unrelated.poll())
            finally:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=5)
                log.close()
                unrelated.terminate()
                unrelated.wait(timeout=5)

    def test_retained_success_does_not_hide_reexecution(self):
        original = {
            "tasks": [{"task_id": "child", "data": {"kind": "host.printf"}}],
            "operations": [{"operation_id": "op", "data": {"task_id": "child"}}],
            "attempts": [{"attempt_id": "attempt", "operation_id": "op"}],
            "task_results": [{"result_id": "result", "task_id": "child", "data": {"result": "original"}}],
            "evidence_receipts": [{"receipt_id": "receipt", "operation_id": "op", "data": {"raw": "original"}}],
        }
        self.assertTrue(all(journey.evaluate_recovery(original, copy.deepcopy(original)).values()))
        repeated = copy.deepcopy(original)
        repeated["tasks"].append({"task_id": "duplicate", "data": {"kind": "host.printf"}})
        repeated["attempts"].append({"attempt_id": "new-attempt", "operation_id": "op"})
        observed = journey.evaluate_recovery(original, repeated)
        self.assertFalse(observed["no_child_redelegation_after_restart"])
        self.assertFalse(observed["original_child_operations_and_attempts_unchanged"])
        self.assertTrue(observed["original_child_result_bytes_retained"])
        repeated["task_results"][0]["data"]["result"] = "overwritten"
        repeated["evidence_receipts"][0]["data"]["raw"] = "overwritten"
        observed = journey.evaluate_recovery(original, repeated)
        self.assertFalse(observed["original_child_result_bytes_retained"])
        self.assertFalse(observed["original_child_receipt_bytes_retained"])

    def test_failed_attempt_stays_in_batch_denominator(self):
        async def fake_run(args):
            args.evidence_dir.mkdir()
            passed = args.evidence_dir.name != "attempt-002"
            journey.save(args.evidence_dir / "summary.json", {
                "passed": passed, "elapsed_seconds": 1,
                "errors": [] if passed else ["synthetic retained failure"],
            })
            return 0 if passed else 1

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "binary"
            binary.write_bytes(b"synthetic identity only")
            args = argparse.Namespace(binary=binary, evidence_dir=root / "batch", repeat=3,
                                      source_revision="a" * 40, model="test-model", model_url="http://unused")
            with patch.object(journey, "run", fake_run), patch.object(journey, "get_json", return_value={
                "data": [{"id": "test-model", "max_model_len": 32768}]
            }), contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(asyncio.run(journey.repeat(args)), 1)
            report = json.loads((args.evidence_dir / "batch-summary.json").read_text())
            self.assertEqual((report["passed_runs"], report["failed_runs"], report["unattempted_runs"]), (2, 1, 0))
            self.assertEqual(report["success_rate"], 2 / 3)
            self.assertEqual(len(report["runs"]), 3)
            self.assertFalse(report["passed"])
            self.assertEqual(report["runs"][1]["errors"], ["synthetic retained failure"])


if __name__ == "__main__":
    unittest.main()
