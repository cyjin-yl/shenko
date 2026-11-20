#!/usr/bin/env python3
"""Filesystem custody and retained-receipt checks; no compiler or guest is run."""

import copy
import hashlib
import json
import os
from pathlib import Path
import signal
import tempfile
import unittest

import build_node_wasix_journey as builder


class PersistentBuildCustodyTests(unittest.TestCase):
    def setUp(self):
        root = Path(os.environ["SHENKO_NODE_WASIX_TEST_ROOT"]).resolve(strict=True)
        self.directory = tempfile.TemporaryDirectory(prefix="custody-", dir=root)
        self.addCleanup(self.directory.cleanup)
        self.work = Path(self.directory.name)

    def test_changed_object_is_detected_with_unchanged_length_and_mtime(self):
        target = self.work / "source/out/Release/probe.o"
        target.parent.mkdir(parents=True)
        target.write_bytes(b"first retained object")
        before = builder.work_inventory(self.work)
        metadata = target.stat()
        target.write_bytes(b"other retained object")
        os.utime(target, ns=(metadata.st_atime_ns, metadata.st_mtime_ns))
        after = builder.work_inventory(self.work)
        original = next(entry for entry in before if entry["path"] == "source/out/Release/probe.o")
        changed = next(entry for entry in after if entry["path"] == original["path"])
        self.assertEqual(original["bytes"], changed["bytes"])
        self.assertEqual(original["mtime_ns"], changed["mtime_ns"])
        self.assertEqual(original["sha256"], hashlib.sha256(b"first retained object").hexdigest())
        self.assertNotEqual(original["sha256"], changed["sha256"])
        self.assertNotEqual(before, after)

    def test_tool_bytes_and_literal_links_remain_in_input_custody(self):
        tools = self.work / "toolchains"
        tools.mkdir()
        compiler = tools / "compiler"
        compiler.write_bytes(b"retained compiler bytes")
        compiler.chmod(0o700)
        wrappers = self.work / "bin"
        wrappers.mkdir()
        wrapper = wrappers / "cc"
        wrapper.symlink_to("../toolchains/compiler")
        before = builder.work_inventory(self.work, inputs_only=True)
        record = next(entry for entry in before if entry["path"] == "bin/cc")
        self.assertEqual(record["link"], "../toolchains/compiler")
        self.assertNotIn("sha256", record)
        compiler.write_bytes(b"different compiler bytes")
        self.assertNotEqual(before, builder.work_inventory(self.work, inputs_only=True))
        wrapper.unlink()
        wrapper.symlink_to("../../outside-custody")
        with self.assertRaisesRegex(RuntimeError, "link escapes"):
            builder.work_inventory(self.work)

    def test_retained_settled_term_deadline_keeps_resume_authority(self):
        receipt = Path(os.environ["SHENKO_NODE_WASIX_COMPILE_RECEIPT"])
        raw = receipt.read_bytes()
        self.assertEqual(hashlib.sha256(raw).hexdigest(),
                         "b382fda9e6d1168b4e7a36515fb0bd71d54b0fa8bfd5c6e6b35750fe52668f0e")
        observed = json.loads(raw)
        self.assertTrue(builder.resumable_compile_stop(observed))
        killed = copy.deepcopy(observed)
        killed["exit_code"] = -int(signal.SIGKILL)
        self.assertFalse(builder.resumable_compile_stop(killed))
        unsettled = copy.deepcopy(observed)
        unsettled["group_remaining"] = [{"pid": 1}]
        self.assertFalse(builder.resumable_compile_stop(unsettled))
        failed = copy.deepcopy(observed)
        failed["stopped"]["reason"] = "rss_observation_limit"
        self.assertFalse(builder.resumable_compile_stop(failed))
        no_term = copy.deepcopy(observed)
        no_term["stopped"]["actions"] = []
        self.assertFalse(builder.resumable_compile_stop(no_term))

    def test_clean_memory_reserve_stop_keeps_the_original_safety_gate(self):
        receipt = Path(os.environ["SHENKO_NODE_WASIX_MEMORY_RESERVE_RECEIPT"])
        raw = receipt.read_bytes()
        self.assertEqual(hashlib.sha256(raw).hexdigest(),
                         "da622c5c7efc433039b98d88787bfcdf450ebb745078caea06359416b3139eac")
        observed = json.loads(raw)
        self.assertEqual(observed["stopped"]["reason"], "observed_memory_reserve")
        self.assertTrue(builder.resumable_compile_stop(observed))
        killed = copy.deepcopy(observed)
        killed["exit_code"] = -int(signal.SIGKILL)
        self.assertFalse(builder.resumable_compile_stop(killed))
        unsettled = copy.deepcopy(observed)
        unsettled["stopped"]["remaining_group"] = [{"pid": 1}]
        self.assertFalse(builder.resumable_compile_stop(unsettled))
        cleanup_error = copy.deepcopy(observed)
        cleanup_error["stopped"]["errors"] = [{"operation": "SIGTERM", "type": "OSError"}]
        self.assertFalse(builder.resumable_compile_stop(cleanup_error))
        execution_error = copy.deepcopy(observed)
        execution_error["execution_error"] = None
        self.assertFalse(builder.resumable_compile_stop(execution_error))
        other_guard = copy.deepcopy(observed)
        other_guard["stopped"]["reason"] = "free_disk_reserve"
        self.assertFalse(builder.resumable_compile_stop(other_guard))


if __name__ == "__main__":
    unittest.main()
