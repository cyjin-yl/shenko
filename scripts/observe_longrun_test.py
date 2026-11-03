#!/usr/bin/env python3
"""Nine independent C19 consumer scenarios; no long-run results are asserted.

Run: python3 -I scripts/observe_longrun_test.py

Inventory: intact window boundary; known sampling gap; missing header; torn
append; kernel boot-id path; PID replacement with failed sample/retry; log
creation directory fsync; zombie subject; sample sequence discontinuity.
Synthetic clocks/processes exercise the actual manifest/log/report consumer.
Real procfs smoke and all execution belong to the integrating parent.
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
    out = io.StringIO()
    code = 0
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
        try:
            code = ol.main(argv)
        except SystemExit as err:
            code = err.code if isinstance(err.code, int) else 2
            if not isinstance(err.code, int) and err.code:
                out.write(str(err.code))
    return code, out.getvalue()


def init_run(tmp, windows=("demo=60",)):
    argv = ["init", "--dir", str(tmp), "--run-id", "run-a", "--workload",
            "synthetic self-check", "--candidate-commit", "0" * 40,
            "--interval", "60", "--pid", "7"]
    for window in windows:
        argv += ["--window", window]
    with patch.object(ol, "utcnow", return_value=1000.0):
        code, out = cli(argv)
    assert code == 0, out
    return ol.load_manifest(tmp)


def report(tmp, *extra, expected_code=0):
    code, out = cli(["report", "--dir", str(tmp), *extra])
    assert code == expected_code, out
    return json.loads(out)


@contextlib.contextmanager
def capture_frame(epoch, pid=7, start=100, process=None):
    main = dict(process) if process is not None else {
        "pid": pid, "alive": True, "comm": "subject", "rss_kb": 100,
        "proc_start_jiffies": start,
    }
    with patch.object(ol, "read_process_or_absent", return_value=main), \
            patch.object(ol, "read_sampler_clock", return_value={
                "boot_id": "boot-test", "uptime_seconds": epoch}), \
            patch.object(ol, "read_system", return_value={"boot_id": "boot-test"}):
        yield


def take(tmp, manifest, epoch, **frame):
    with capture_frame(epoch, **frame):
        return ol._take_sample(tmp, manifest, None, None, epoch=epoch)


def scenario_window_boundary(tmp):
    """An intact, cadence-compliant 60-second synthetic span qualifies exactly."""
    manifest = init_run(tmp)
    take(tmp, manifest, 1000.0)
    take(tmp, manifest, 1060.0)
    rep = report(tmp, "--require-observed")
    window = rep["declared_windows"][0]
    assert rep["integrity"]["intact"]
    assert window["qualifying_span_seconds"] == 60
    assert window["status"] == "observed"


def scenario_known_gap(tmp):
    """Two instants thirty days apart do not observe either declared window."""
    manifest = init_run(tmp, windows=("7d=604800", "30d=2592000"))
    take(tmp, manifest, 1000.0)
    take(tmp, manifest, 1000.0 + 2592000)
    rep = report(tmp, "--require-observed", expected_code=3)
    for window in rep["declared_windows"]:
        assert window["observed_span_seconds"] == 2592000
        assert window["unobserved_seconds"] == 2592000
        assert window["qualifying_span_seconds"] == 0
        assert window["status"] == "unobserved"


def scenario_missing_header(tmp):
    """Headerless samples cannot bind a manifest or admit another capture."""
    manifest = init_run(tmp)
    take(tmp, manifest, 1000.0)
    take(tmp, manifest, 1060.0)
    log = ol.log_path(tmp)
    body = b"".join(log.read_bytes().splitlines(keepends=True)[1:])
    log.write_bytes(body)
    rep = report(tmp, "--require-observed", expected_code=3)
    assert not rep["integrity"]["header_valid"]
    assert rep["declared_windows"][0]["status"] == "unobserved"
    code, out = cli(["sample", "--dir", str(tmp)])
    assert code == 2 and "refusing append" in out
    assert log.read_bytes() == body


def scenario_torn_append(tmp):
    """An interrupted tail stays byte-identical when continuation is refused."""
    manifest = init_run(tmp)
    take(tmp, manifest, 1000.0)
    take(tmp, manifest, 1060.0)
    log = ol.log_path(tmp)
    torn = b'{"record":"sample",'
    with log.open("ab") as stream:
        stream.write(torn)
    before = log.read_bytes()
    code, out = cli(["sample", "--dir", str(tmp)])
    assert code == 2 and "refusing append" in out
    assert log.read_bytes() == before
    rep = report(tmp, "--require-observed", expected_code=3)
    assert rep["integrity"]["unterminated_tail_bytes"] == len(torn)
    assert rep["integrity"]["unreadable_bytes"] == len(torn)
    assert rep["declared_windows"][0]["status"] == "unobserved"


def scenario_boot_id_path(tmp):
    """The fixture exposes the kernel's real path, not the former wrong path."""
    boot_id = tmp / "sys/kernel/random/boot_id"
    boot_id.parent.mkdir(parents=True)
    boot_id.write_text("d647486b-1488-4bed-8c68-bf3ba4a2d35b\n")
    assert ol.read_boot_id(tmp) == "d647486b-1488-4bed-8c68-bf3ba4a2d35b"


def scenario_restart_sample_retry(tmp):
    """One PID replacement stays one restart when its first sample write fails."""
    manifest = init_run(tmp)
    take(tmp, manifest, 1000.0)
    append = ol.append_record

    def fail_sample(log, record):
        if record["record"] == "sample":
            raise OSError("injected sample-write failure before any sample bytes")
        append(log, record)

    with capture_frame(1060.0, pid=8, start=200), \
            patch.object(ol, "append_record", side_effect=fail_sample):
        try:
            ol._take_sample(tmp, manifest, None, None, epoch=1060.0)
        except OSError as err:
            assert "injected" in str(err)
        else:
            raise AssertionError("failed sample was acknowledged")
    pending = report(tmp, "--require-observed", expected_code=3)
    assert pending["restart_boundaries"] == 0
    assert pending["integrity"]["unmatched_restart_boundaries"] == 1
    take(tmp, manifest, 1060.0, pid=8, start=200)
    rep = report(tmp, "--require-observed")
    runs, _ = ol.scan_log(ol.log_path(tmp))
    assert len(runs["run-a"]["segments"]) == 1
    assert runs["run-a"]["segments"][0]["sample_seq"] == 2
    assert rep["restart_boundaries"] == 1
    assert rep["restart_reasons"] == ["pid_changed"]


def scenario_log_creation_fsync(tmp):
    """Acknowledging a newly created log syncs its file and parent directory."""
    tmp.mkdir()
    log = ol.log_path(tmp)
    with patch.object(ol.os, "fsync", wraps=os.fsync) as sync_file, \
            patch.object(ol, "fsync_dir", wraps=ol.fsync_dir) as sync_dir:
        ol.append_record(log, {"durability_probe": True})
    sync_dir.assert_called_once_with(tmp)
    assert sync_file.call_count == 2  # log fd, then directory fd
    assert log.read_bytes() == b'{"durability_probe":true}\n'


def scenario_zombie_subject(tmp):
    """An unreaped subject is absent, with no increasing live-process uptime."""
    procfs = tmp / "proc"
    process_dir = procfs / "7"
    process_dir.mkdir(parents=True)
    (process_dir / "status").write_text("Name:\tsubject\nState:\tZ (zombie)\n")
    (process_dir / "stat").write_text("7 (subject) Z " + "0 " * 18 + "100 0\n")
    (procfs / "uptime").write_text("1000.00 0.00\n")
    zombie = ol.read_process(7, procfs)
    assert not zombie["alive"] and zombie["uptime_seconds"] is None
    manifest = init_run(tmp / "run")
    take(tmp / "run", manifest, 1000.0, process=zombie)
    rep = report(tmp / "run")
    assert rep["subject_alive_samples"] == 0
    assert rep["subject_absent_samples"] == 1
    assert rep["subject_sample_uptime_fraction"] == 0


def scenario_sequence_discontinuity(tmp):
    """A skipped sample sequence cannot qualify an otherwise sufficient span."""
    manifest = init_run(tmp)
    take(tmp, manifest, 1000.0)
    take(tmp, manifest, 1060.0)
    log = ol.log_path(tmp)
    records = [json.loads(line) for line in log.read_bytes().splitlines()]
    records[-1]["seq"] = 3
    log.write_bytes(b"".join((json.dumps(record) + "\n").encode()
                             for record in records))
    rep = report(tmp, "--require-observed", expected_code=3)
    assert rep["integrity"]["sequence_gaps"] == [[1, 3]]
    assert rep["declared_windows"][0]["status"] == "unobserved"


def main():
    build = Path(__file__).resolve().parents[1] / "build"
    build.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="c19-observe-", dir=build) as directory:
        base = Path(directory)
        scenario_window_boundary(base / "boundary")
        scenario_known_gap(base / "gap")
        scenario_missing_header(base / "header")
        scenario_torn_append(base / "torn")
        scenario_boot_id_path(base / "boot")
        scenario_restart_sample_retry(base / "retry")
        scenario_log_creation_fsync(base / "durability")
        scenario_zombie_subject(base / "zombie")
        scenario_sequence_discontinuity(base / "sequence")
    print("observe_longrun_test: 9 independent scenarios passed")


if __name__ == "__main__":
    main()
