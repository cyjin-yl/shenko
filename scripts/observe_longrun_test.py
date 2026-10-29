#!/usr/bin/env python3
"""Self-checks for scripts/observe_longrun.py — C19 tooling slice.

Run: python3 -I scripts/observe_longrun_test.py

Four independent scenarios over the real run-directory layout. Scenario 1
reads the real /proc of this test process; scenarios 2–4 substitute the
process reader so boundaries, downtime and corruption are deterministic.
No scenario claims a long window: declared windows stay unobserved unless the
recorded span actually covers them.
"""
import contextlib
import io
import json
import os
import sys
import tempfile
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import observe_longrun as ol  # noqa: E402


def cli(argv):
    """Run the real argparse entry; return (exit_code, stdout+stderr text)."""
    out = io.StringIO()
    code = 0
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
        try:
            code = ol.main(argv)
        except SystemExit as err:
            if isinstance(err.code, int):
                code = err.code
            else:
                code = 2
                if err.code:
                    out.write(str(err.code))
    return code, out.getvalue()


def init_run(tmp, run_id="run-a", windows=("tiny=60", "day7=604800"), **kw):
    argv = ["init", "--dir", tmp, "--run-id", run_id, "--workload",
            "self-test subject", "--candidate-commit",
            "0" * 40]
    for name in windows:
        argv += ["--window", name]
    for key, value in kw.items():
        argv += ["--" + key.replace("_", "-"), str(value)]
    code, _ = cli(argv)
    assert code == 0
    return ol.load_manifest(Path(tmp))


def report(tmp, *extra):
    code, out = cli(["report", "--dir", tmp, *extra])
    assert code == 0, out
    return json.loads(out)


def scenario_real_pid(tmp):
    """Real /proc sample of this very process; windows honestly unobserved."""
    manifest = init_run(tmp, pid=os.getpid())
    assert manifest["candidate"]["commit"] == "0" * 40
    for _ in range(2):
        code, out = cli(["sample", "--dir", tmp])
        assert code == 0, out
        record = json.loads(out)
        assert record["main"]["alive"] and record["main"]["rss_kb"] > 0
        assert record["main"]["boot_id"] == ol.read_boot_id()
    rep = report(tmp)
    assert rep["samples"] == 2 and rep["subject_alive_samples"] == 2
    assert rep["restart_boundaries"] == 0
    assert len(rep["segments"]) == 1
    statuses = {w["window"]: w["status"] for w in rep["declared_windows"]}
    assert statuses == {"tiny": "unobserved", "day7": "unobserved"}, statuses
    assert rep["verdict"] == "incomplete_observation"
    code, _ = cli(["report", "--dir", tmp, "--require-observed"])
    assert code == 3
    return rep


def scenario_window_boundary(tmp):
    """A window is observed only when the recorded span actually covers it."""
    manifest = init_run(tmp, run_id="run-b",
                        windows=("covered=60", "uncovered=3600"))
    alive = {"pid": 7, "alive": True, "comm": "subj",
             "rss_kb": 100, "proc_start_jiffies": 500}
    with patch.object(ol, "read_process_or_absent", return_value=dict(alive)):
        for epoch in (1_000.0, 1_060.0):
            ol._take_sample(Path(tmp), manifest, 7, None, epoch=epoch)
    rep = report(tmp, "--run-id", "run-b")
    statuses = {w["window"]: w["status"] for w in rep["declared_windows"]}
    # span is exactly 60s: covered at the boundary, the 3600s window is not.
    assert statuses == {"covered": "observed", "uncovered": "unobserved"}, statuses
    assert rep["rss_main_process"]["drift_kb"] == 0
    return rep


def scenario_restart_downtime_integrity(tmp):
    """Restart boundaries, absent samples and log/manifest damage surface."""
    manifest = init_run(tmp, run_id="run-c", windows=("w=1200",), pid=7)
    frames = [
        (2_000.0, {"pid": 7, "alive": True, "comm": "subj",
                   "rss_kb": 100, "proc_start_jiffies": 100}),
        (2_300.0, {"pid": 7, "alive": True, "comm": "subj",
                   "rss_kb": 110, "proc_start_jiffies": 200}),
        (2_600.0, {"pid": 7, "alive": False}),
        (2_900.0, {"pid": 7, "alive": False}),
        (3_200.0, {"pid": 7, "alive": True, "comm": "subj",
                   "rss_kb": 120, "proc_start_jiffies": 200}),
    ]
    for epoch, proc in frames:
        with patch.object(ol, "read_process_or_absent", return_value=dict(proc)):
            ol._take_sample(Path(tmp), manifest, 7, None, epoch=epoch)
    rep = report(tmp, "--run-id", "run-c")
    assert rep["samples"] == 5
    assert rep["subject_absent_samples"] == 2
    assert rep["subject_sample_uptime_fraction"] == 0.6
    assert rep["restart_boundaries"] == 2
    assert rep["restart_reasons"] == ["reappeared",
                                    "starttime_changed_same_pid"], rep
    # All gap < 3*interval(300s) → no unobserved inter-sample time; the 1200s
    # window IS covered by the 3200-2000=1200s recorded span.
    statuses = {w["window"]: w["status"] for w in rep["declared_windows"]}
    assert statuses["w"] == "observed", statuses

    log = Path(tmp) / "records.jsonl"
    # A fake record skipping seq 6 → sequence gap (5,7) is reported.
    fake = {"record": "sample", "run_id": "run-c", "seq": 7,
            "epoch": 3500.0, "wall_utc": ol.iso_utc(3500.0),
            "main": {"pid": 7, "alive": False}, "extra": {}, "system": {}}
    with open(log, "ab") as fh:
        fh.write((json.dumps(fake) + "\n").encode())
    # A torn tail (power loss mid-append) stays unreadable bytes.
    with open(log, "ab") as fh:
        fh.write(b'{"record":"sample",')
    rep = report(tmp, "--run-id", "run-c")
    assert rep["integrity"]["unreadable_lines"] == 1
    assert rep["integrity"]["unreadable_bytes"] > 0
    assert (5, 7) in [tuple(g) for g in rep["integrity"]["sequence_gaps"]]

    # Rewriting the frozen manifest after samples exist marks every window
    # unobserved instead of silently binding to different terms.
    mfile = Path(tmp) / "manifest.json"
    manifest = ol.read_json(mfile)
    manifest["environment"]["note"] = "edited after capture"
    mfile.write_text(json.dumps(manifest))
    rep = report(tmp, "--run-id", "run-c")
    assert rep["integrity"]["manifest_drift"] is True
    assert all(w["status"] == "unobserved"
               for w in rep["declared_windows"])
    return rep


def scenario_manifest_and_empty(tmp):
    """Manifest is immutable per run_id; empty logs are explicit errors."""
    init_run(tmp, run_id="run-d")
    code, out = cli(["init", "--dir", tmp, "--run-id", "run-d",
                     "--workload", "duplicate", "--window", "x=1"])
    assert code != 0 and "immutable" in out + ""
    code, _ = cli(["report", "--dir", tmp, "--run-id", "run-d"])
    assert code != 0  # no records yet → explicit error, not an empty pass
    code, out = cli(["sample", "--dir", tmp])
    assert code == 0, out
    record = json.loads(out)
    # No pid configured anywhere: unresolved observation, not a guessed pid.
    assert record["main"]["alive"] is False
    assert "unresolved" in record["main"]


def main():
    with tempfile.TemporaryDirectory(prefix="shenko-observe-") as d:
        base = Path(d)
        scenario_real_pid(str(base / "s1"))
        scenario_window_boundary(str(base / "s2"))
        scenario_restart_downtime_integrity(str(base / "s3"))
        scenario_manifest_and_empty(str(base / "s4"))
    print("observe_longrun_test: 4 scenarios passed")


if __name__ == "__main__":
    main()
