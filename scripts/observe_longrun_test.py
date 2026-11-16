#!/usr/bin/env python3
"""Twenty independent C19 consumer scenarios; no long-run results are asserted.

Run: python3 -I scripts/observe_longrun_test.py

Inventory: intact window boundary; known sampling gap; missing header; torn
append; kernel boot-id path; PID replacement with failed sample/retry; log
creation directory fsync; zombie subject; sample sequence discontinuity;
sealed run lifecycle; checkpoint binding; seal-to-sample binding.
Also covers equal-stat different bytes, a replaced reported executable path,
and unknown executed-file identity in a real-observation-shaped fixture.
Synthetic clocks/processes exercise the actual manifest/log/report consumer.
Real procfs smoke and all execution belong to the integrating parent.
"""
import contextlib
import hashlib
import io
import json
import os
import select
import subprocess
import time
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


def scenario_sealed_run_lifecycle(tmp):
    """A stopped run is final: it reports its stop, and accepts no more records."""
    manifest = init_run(tmp)
    take(tmp, manifest, 1000.0)
    take(tmp, manifest, 1060.0)
    pending = report(tmp)
    assert pending["lifecycle"]["sealed"] is False
    assert pending["lifecycle"]["stop_reason"] is None
    assert pending["eligible_as_product_evidence"] is False
    code, out = cli(["checkpoint", "--dir", str(tmp), "--id", "review-1",
                     "--note", "first review point"])
    assert code == 0, out
    code, out = cli(["stop", "--dir", str(tmp), "--reason", "completed",
                     "--note", "declared demo window reached"])
    assert code == 0, out
    sealed = report(tmp)
    assert sealed["lifecycle"]["sealed"] is True
    assert sealed["lifecycle"]["stop_reason"] == "completed"
    assert sealed["lifecycle"]["stop_sample_seq"] == 2
    assert sealed["lifecycle"]["checkpoint_count"] == 1
    # A synthetic self-check run still never becomes product evidence.
    assert sealed["eligible_as_product_evidence"] is False
    for command in (["sample"], ["stop", "--reason", "completed"],
                    ["checkpoint", "--id", "late"]):
        code, out = cli([command[0], "--dir", str(tmp), *command[1:]])
        assert code == 2 and "run is sealed" in out, out
    runs, _ = ol.scan_log(ol.log_path(tmp))
    state = runs["run-a"]
    assert state["run_end_count"] == 1
    assert state["records_after_end"] == 0


def scenario_checkpoint_binding(tmp):
    """A checkpoint binds a written sample; an impossible one invalidates the run."""
    manifest = init_run(tmp)
    take(tmp, manifest, 1000.0)
    take(tmp, manifest, 1060.0)
    code, out = cli(["checkpoint", "--dir", str(tmp), "--id", "review-1"])
    assert code == 0, out
    recorded = [json.loads(line) for line in ol.log_path(tmp).read_bytes().splitlines()]
    checkpoints = [rec for rec in recorded if rec["record"] == "checkpoint"]
    assert len(checkpoints) == 1 and checkpoints[0]["sample_seq"] == 2
    code, out = cli(["checkpoint", "--dir", str(tmp), "--id", "review-1"])
    assert code == 2 and "already recorded" in out, out
    assert report(tmp)["integrity"]["intact"] is True
    # A checkpoint that names a sample the run had not written cannot be
    # ignored: the retained bytes stay, but the run stops qualifying.
    forged = dict(checkpoints[0], checkpoint_id="forged", sample_seq=7)
    ol.append_record(ol.log_path(tmp), forged)
    rep = report(tmp, "--require-observed", expected_code=3)
    assert rep["integrity"]["checkpoint_mismatches"] == [["forged", 7, 2]]
    assert not rep["integrity"]["intact"]
    assert rep["declared_windows"][0]["status"] == "unobserved"


def scenario_seal_binds_written_sample(tmp):
    """An end record claiming a later sample is a gap, not a finished run."""
    manifest = init_run(tmp)
    take(tmp, manifest, 1000.0)
    take(tmp, manifest, 1060.0)
    # A forged seal that names sample 7 of a two-sample run: valid record
    # fields, impossible binding. The bytes stay; the run stops qualifying.
    ol.append_record(ol.log_path(tmp), {
        "record": "run_end",
        "schema": ol.SCHEMA,
        "run_id": manifest["run_id"],
        "stop_reason": "completed",
        "note": None,
        "sample_seq": 7,
        "samples": 7,
        "epoch": 1061.0,
        "wall_utc": "1970-01-01T00:17:41.000Z",
        "manifest_sha256": ol.sha256_file(ol.manifest_path(tmp)),
    })
    rep = report(tmp, "--require-observed", expected_code=3)
    assert rep["lifecycle"]["sealed"] is True
    assert rep["lifecycle"]["stop_sample_seq"] == 7
    assert rep["integrity"]["run_end_mismatches"] == [{
        "recorded_sample_seq": 7,
        "last_sample_seq": 2,
        "recorded_samples": 7,
        "sample_records": 2,
    }]
    assert not rep["integrity"]["intact"]
    assert rep["declared_windows"][0]["status"] == "unobserved"
    assert rep["eligible_as_product_evidence"] is False
    # A mismatched seal also blocks further appends instead of extending it.
    code, out = cli(["sample", "--dir", str(tmp)])
    assert code == 2 and "invalid evidence" in out, out


def scenario_equal_stat_different_binary(tmp):
    """Equal size and mtime do not identify executable content."""
    tmp.mkdir(parents=True)
    actual = tmp / "actual"
    other = tmp / "other"
    actual.write_bytes(b"actual-A")
    other.write_bytes(b"other--B")
    metadata = actual.stat()
    os.utime(other, ns=(metadata.st_atime_ns, metadata.st_mtime_ns))
    procfs = tmp / "proc"
    (procfs / "7").mkdir(parents=True)
    (procfs / "7" / "exe").symlink_to(actual)
    reader = ol.read_subject_exe
    with patch.object(ol, "read_subject_exe",
                      side_effect=lambda pid: reader(pid, procfs)):
        positive = ol.subject_binary_state(7, {
            "binary_measured": ol.measure_binary(actual)})
        negative = ol.subject_binary_state(7, {
            "binary_measured": ol.measure_binary(other)})
    assert positive["matches_declared_candidate"] is True
    assert negative["matches_declared_candidate"] is False
    assert negative["exe"]["sha256"] == hashlib.sha256(b"actual-A").hexdigest()


def scenario_reported_path_is_not_executed_file(tmp):
    """The descriptive readlink path cannot replace the open executable."""
    tmp.mkdir(parents=True)
    actual = tmp / "actual"
    replacement = tmp / "reported-path"
    actual.write_bytes(b"running executable")
    replacement.write_bytes(b"replacement bytes")
    procfs = tmp / "proc"
    (procfs / "7").mkdir(parents=True)
    (procfs / "7" / "exe").symlink_to(actual)
    with patch.object(ol.os, "readlink", return_value=str(replacement)):
        measured = ol.read_subject_exe(7, procfs)
    assert measured["sha256"] == hashlib.sha256(b"running executable").hexdigest()
    assert measured["path"] == str(replacement)


def scenario_unknown_executable_stays_unobserved(tmp):
    """A sealed fixture with no executed-file identity cannot qualify."""
    argv = ["init", "--dir", str(tmp), "--run-id", "unknown-executable",
            "--workload", "synthetic unavailable-executable fixture",
            "--candidate-commit", "0" * 40, "--binary-path", sys.executable,
            "--profile", "fixture", "--evidence-class", "real_observation",
            "--interval", "60", "--pid", "7", "--window", "demo=60"]
    with patch.object(ol, "utcnow", return_value=1000.0):
        code, out = cli(argv)
    assert code == 0, out
    manifest = ol.load_manifest(tmp)
    with patch.object(ol, "read_subject_exe", return_value=None):
        take(tmp, manifest, 1000.0)
        take(tmp, manifest, 1060.0)
    with patch.object(ol, "utcnow", return_value=1061.0):
        code, out = cli(["stop", "--dir", str(tmp), "--reason", "completed"])
    assert code == 0, out
    rep = report(tmp, "--require-observed", expected_code=3)
    assert rep["lifecycle"]["sealed"] is True
    assert rep["integrity"]["candidate_binary_unverified_sample_seqs"] == [1, 2]
    assert not rep["integrity"]["intact"]
    assert rep["declared_windows"][0]["status"] == "unobserved"
    assert rep["eligible_as_product_evidence"] is False


def spawn_locked_command(tmp, argv):
    # A second *process*, not a reused Python-level lock/file description.
    module_dir = str(Path(ol.__file__).resolve().parent)
    code = (
        "import sys\n"
        f"sys.path.insert(0, {module_dir!r})\n"
        "import observe_longrun as ol\n"
        "print('ready', flush=True)\n"
        f"sys.exit(ol.main({argv!r}))\n"
    )
    child = subprocess.Popen([sys.executable, "-I", "-c", code],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             text=True)
    ready, _, _ = select.select([child.stdout], [], [], 5)
    if not ready or child.stdout.readline().strip() != "ready":
        child.kill()
        stdout, stderr = child.communicate(timeout=5)
        raise AssertionError(f"child never became ready: {stdout} {stderr}")
    return child


def finish_locked_child(child):
    try:
        stdout, stderr = child.communicate(timeout=10)
    except subprocess.TimeoutExpired:
        child.kill()
        child.communicate(timeout=5)
        raise AssertionError("observation command did not finish after lock release")
    return child.returncode, stdout + stderr


def scenario_competing_local_samplers(tmp):
    """Two real processes cannot mint two headers or the same sample sequence."""
    manifest = init_run(tmp)
    argv = ["sample", "--dir", str(tmp)]
    with ol.observation_lock(tmp):
        first = spawn_locked_command(tmp, argv)
        second = spawn_locked_command(tmp, argv)
        time.sleep(0.15)
        assert first.poll() is None and second.poll() is None
    assert finish_locked_child(first)[0] == 0
    assert finish_locked_child(second)[0] == 0
    runs, issues = ol.scan_log(ol.log_path(tmp))
    state = runs["run-a"]
    assert len(state["headers"]) == 1
    assert [item["seq"] for item in state["samples"]] == [1, 2]
    assert ol.run_integrity(manifest, ol.sha256_file(ol.manifest_path(tmp)),
                            runs, issues)["intact"] is True


def scenario_stop_and_sample_are_ordered(tmp):
    """A concurrent final sample cannot appear after a committed run_end."""
    manifest = init_run(tmp)
    take(tmp, manifest, 1000.0)
    with ol.observation_lock(tmp):
        stopping = spawn_locked_command(
            tmp, ["stop", "--dir", str(tmp), "--reason", "completed"])
        sampling = spawn_locked_command(tmp, ["sample", "--dir", str(tmp)])
        time.sleep(0.15)
        assert stopping.poll() is None and sampling.poll() is None
    stopped, error = finish_locked_child(stopping)
    assert stopped == 0, error
    sampled, error = finish_locked_child(sampling)
    assert sampled in (0, 2), error
    runs, issues = ol.scan_log(ol.log_path(tmp))
    state = runs["run-a"]
    assert state["run_end_count"] == 1
    assert state["records_after_end"] == 0
    assert state["run_end"]["sample_seq"] == len(state["samples"])
    assert ol.run_integrity(manifest, ol.sha256_file(ol.manifest_path(tmp)),
                            runs, issues)["intact"] is True


def scenario_simultaneous_init_does_not_replace_manifest(tmp):
    """Only one of two independent initializers owns the original run."""
    tmp.mkdir()
    args = ["init", "--dir", str(tmp), "--run-id", "race",
            "--workload", "synthetic", "--window", "demo=60", "--pid", "7"]
    with ol.observation_lock(tmp):
        first = spawn_locked_command(tmp, args)
        second = spawn_locked_command(tmp, args)
        time.sleep(0.15)
        assert first.poll() is None and second.poll() is None
    results = [finish_locked_child(first), finish_locked_child(second)]
    assert sorted(code == 0 for code, _ in results) == [False, True], results
    manifest = ol.load_manifest(tmp)
    assert manifest["run_id"] == "race"
    assert manifest["declared_windows_seconds"] == {"demo": 60}


def scenario_lock_symlink_is_not_followed(tmp):
    """Refuse a substituted lock inode without touching either evidence log."""
    init_run(tmp)
    original = ol.manifest_path(tmp).read_bytes()
    log = ol.log_path(tmp)
    before = log.read_bytes() if log.exists() else None
    target = tmp.parent / "outside-lock-target"
    target.write_bytes(b"must remain unchanged")
    lock = tmp / ol._OBSERVATION_LOCK_FILE
    lock.unlink()
    lock.symlink_to(target)
    code, output = cli(["sample", "--dir", str(tmp)])
    assert code == 2, output
    assert target.read_bytes() == b"must remain unchanged"
    assert ol.manifest_path(tmp).read_bytes() == original
    assert (log.read_bytes() if log.exists() else None) == before


def scenario_report_reads_a_single_locked_snapshot(tmp):
    """A report cannot read a half-written sample while another writer owns it."""
    init_run(tmp)
    with ol.observation_lock(tmp):
        reporting = spawn_locked_command(tmp, ["report", "--dir", str(tmp)])
        time.sleep(0.15)
        assert reporting.poll() is None
    code, output = finish_locked_child(reporting)
    assert code == 1 or code == 0, output
    # No sample was written. The report must not claim evidence.
    assert "no records for run" in output


def main():
    build = Path(__file__).resolve().parents[1] / "build"
    build.mkdir(exist_ok=True)
    scenarios = (
        ("boundary", scenario_window_boundary),
        ("gap", scenario_known_gap),
        ("header", scenario_missing_header),
        ("torn", scenario_torn_append),
        ("boot", scenario_boot_id_path),
        ("retry", scenario_restart_sample_retry),
        ("durability", scenario_log_creation_fsync),
        ("zombie", scenario_zombie_subject),
        ("sequence", scenario_sequence_discontinuity),
        ("sealed", scenario_sealed_run_lifecycle),
        ("checkpoint", scenario_checkpoint_binding),
        ("seal-binding", scenario_seal_binds_written_sample),
        ("equal-stat-binary", scenario_equal_stat_different_binary),
        ("reported-path", scenario_reported_path_is_not_executed_file),
        ("unknown-executable", scenario_unknown_executable_stays_unobserved),
        ("competing-samplers", scenario_competing_local_samplers),
        ("stop-sample", scenario_stop_and_sample_are_ordered),
        ("simultaneous-init", scenario_simultaneous_init_does_not_replace_manifest),
        ("lock-symlink", scenario_lock_symlink_is_not_followed),
        ("report-snapshot", scenario_report_reads_a_single_locked_snapshot),
    )
    with tempfile.TemporaryDirectory(prefix="c19-observe-", dir=build) as directory:
        base = Path(directory)
        for name, scenario in scenarios:
            scenario(base / name)
    print(f"observe_longrun_test: {len(scenarios)} independent scenarios passed")


if __name__ == "__main__":
    main()
