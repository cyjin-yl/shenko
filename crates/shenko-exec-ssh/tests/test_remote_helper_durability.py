"""Two local helper checks; existing durable root, synthetic files, no SSH.

Run: python3 -I -S test_helper_durability.py /absolute/remote_helper.py
These are syscall-order/fault-injection checks, not a power-loss test.
"""
import base64
import contextlib
import io
import json
import os
from pathlib import Path
import runpy
import sys
import tempfile
import unittest
from unittest.mock import patch

HELPER = Path(sys.argv.pop(1)).resolve()
OPERATION = "ccf3fbd2-3655-4629-ac98-349020891fee"


class HelperDurability(unittest.TestCase):
    def invoke(self, root, destination, mode="dispatch"):
        payload = {"kind": "upload", "path": str(destination),
                   "data_b64": base64.b64encode(b"owned durable fixture").decode()}
        output = io.StringIO()
        with patch.object(sys, "argv", [str(HELPER), str(root), OPERATION, "fixture-seal", mode]), \
             patch.object(sys, "stdin", io.StringIO(json.dumps(payload))), \
             contextlib.redirect_stdout(output):
            try:
                runpy.run_path(str(HELPER), run_name="__main__")
            except SystemExit as error:
                self.assertEqual(error.code, 0)
        return json.loads(output.getvalue())

    def test_operation_entry_synced_before_intent_and_effect(self):
        with tempfile.TemporaryDirectory(prefix="shenko-helper-order-") as temporary:
            root = Path(temporary) / "receipts"
            root.mkdir(mode=0o700)
            destination = Path(temporary) / "result"
            root_identity = (root.stat().st_dev, root.stat().st_ino)
            real_fsync = os.fsync
            observed = []

            def observe(fd):
                st = os.fstat(fd)
                if (st.st_dev, st.st_ino) == root_identity:
                    observed.append(True)
                    self.assertTrue((root / OPERATION).is_dir())
                    self.assertFalse((root / OPERATION / "intent.json").exists())
                    self.assertFalse(destination.exists())
                real_fsync(fd)

            with patch.object(os, "fsync", observe):
                receipt = self.invoke(root, destination)
            self.assertEqual(observed, [True], "operation directory entry was not synced")
            self.assertEqual(receipt["state"], "completed")
            self.assertEqual(receipt["exit_code"], 0)
            self.assertEqual(destination.read_bytes(), b"owned durable fixture")
            destination.write_bytes(b"later independent state")
            self.assertEqual(self.invoke(root, destination, "query"), receipt)
            self.assertEqual(destination.read_bytes(), b"later independent state")

    def test_failed_parent_sync_stops_before_intent_or_effect(self):
        with tempfile.TemporaryDirectory(prefix="shenko-helper-fault-") as temporary:
            root = Path(temporary) / "receipts"
            root.mkdir(mode=0o700)
            destination = Path(temporary) / "result"
            root_identity = (root.stat().st_dev, root.stat().st_ino)
            real_fsync = os.fsync

            def fail_root_sync(fd):
                st = os.fstat(fd)
                if (st.st_dev, st.st_ino) == root_identity:
                    raise OSError("owned injected receipt-root sync failure")
                real_fsync(fd)

            with patch.object(os, "fsync", fail_root_sync):
                with self.assertRaisesRegex(OSError, "owned injected"):
                    self.invoke(root, destination)
            self.assertFalse(destination.exists())
            self.assertEqual(list((root / OPERATION).iterdir()), [])
            receipt = self.invoke(root, destination, "query")
            self.assertEqual(receipt["state"], "unknown")
            # Even a repeated dispatch must not replay an uncertain operation.
            self.assertEqual(self.invoke(root, destination), receipt)
            self.assertFalse(destination.exists())
            self.assertEqual(list((root / OPERATION).iterdir()), [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
