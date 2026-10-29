#!/usr/bin/env python3
"""Scheduled, durable observation capture for real 7/30-day runs — C19 / SPEC S5.

This tool only *records* what a bounded sampler actually saw. An observed
window requires intact manifest-bound evidence and a gap-free sampled span,
corroborated by the same kernel boot's uptime. Missing samples, unverifiable
identities, clock discontinuities and damaged records cannot qualify a window.

Layout of the run directory (all writes are fsync'd):

  manifest.json      frozen at `init`: candidate/environment identity and the
                     *declared* observation windows. Editing it after samples
                     exist is detected as manifest drift at report time.
  records.jsonl      append-only records, each one JSON line fsynced before
                     the write returns: run_header, segment and sample records.

One directory holds one immutable run. A changed candidate or damaged run
requires a new directory/run_id; existing evidence bytes are never repaired or
rebound — see docs/governance/COMPLETION.md §4.

Commands:
  init    create the run directory and freeze the manifest
  sample  append exactly one sample (use from cron/systemd/tmux loops)
  run     sample every --interval seconds until --deadline-seconds elapses
  report  fold records into a per-run summary; declared windows the samples
          do not cover stay `unobserved` forever

The sampler is Linux /proc based. It observes RSS/uptime/FDs of the subject
PID (or the PID listed in --pid-file each tick, for supervised services),
host load/memory/filesystem headroom, and segment boundaries whenever the
subject's (boot_id, pid, starttime, comm) fingerprint changes. It does NOT
prove continuous behaviour between samples and does not detect restarts that
spawn a *different* PID unless the new PID is supplied (e.g. via --pid-file).
"""
import argparse
import hashlib
import json
import math
import os
import re
import sys
import time
from pathlib import Path

SCHEMA = 2
KIND = "c19_long_run_observation"
SCRIPT_NAME = Path(__file__).name
PROCFS = Path("/proc")
DEFAULT_STATE_INTERVAL = 300.0

_MANIFEST_REQUIRED = (
    "schema", "kind", "run_id", "workload", "candidate", "environment",
    "declared_windows_seconds", "interval_seconds", "main_pid_source",
    "extra_pids", "created_utc", "created_epoch", "created_by",
)


def utcnow():
    return time.time()


def iso_utc(epoch):
    return time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(epoch)) + (
        ".%03dZ" % int((epoch % 1) * 1000))


def sha256_file(path):
    digest = hashlib.sha256()
    digest.update(path.read_bytes())
    return digest.hexdigest()


def fsync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def create_run_dir_durable(path):
    path.mkdir(parents=True, exist_ok=True)
    # Sync reachability bottom-up, including parents created in an earlier
    # failed init. An existing directory is not proof its entry was synced.
    current = path.resolve(strict=True)
    while True:
        fsync_dir(current)
        parent = current.parent
        if parent == current:
            break
        current = parent


def write_json_durable(path, obj):
    raw = (json.dumps(obj, ensure_ascii=False, sort_keys=True,
                      indent=2) + "\n").encode("utf-8")
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        view = memoryview(raw)
        while view:
            view = view[os.write(fd, view):]
        os.fsync(fd)
    finally:
        os.close(fd)


def append_record(log_path, record):
    line = (json.dumps(record, ensure_ascii=False,
                       separators=(",", ":")) + "\n").encode("utf-8")
    fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        view = memoryview(line)
        while view:
            written = os.write(fd, view)
            if written == 0:
                raise OSError("record append made no progress")
            view = view[written:]
        os.fsync(fd)
    finally:
        os.close(fd)
    # Also persist a newly created filename. Sync on retries too: an earlier
    # file fsync may have succeeded before its directory fsync failed.
    fsync_dir(log_path.parent)


def read_json(path):
    return json.loads(path.read_text(encoding="utf-8"))


# ---------------------------------------------------------------- /proc reads

def read_boot_id(procfs=PROCFS):
    """Kernel boot identifier for restart attribution; None when the kernel
    does not expose it (restricted procfs). Never fabricated."""
    try:
        return (procfs / "sys/kernel/random/boot_id").read_text().strip() or None
    except OSError:
        return None


def read_uptime_seconds(procfs=PROCFS):
    return float((procfs / "uptime").read_text().split()[0])


def read_sampler_clock():
    """Uptime corroborates elapsed wall time without inventing boot continuity."""
    try:
        uptime = read_uptime_seconds()
    except OSError:
        uptime = None
    return {"boot_id": read_boot_id(), "uptime_seconds": uptime}


def read_mem_available_kb(procfs=PROCFS):
    try:
        for line in (procfs / "meminfo").read_text().splitlines():
            if line.startswith("MemAvailable:"):
                return int(line.split()[1])
    except OSError:
        pass
    return None


def read_loadavg(procfs=PROCFS):
    try:
        return (procfs / "loadavg").read_text().split()[:3]
    except OSError:
        return None


def read_kernel_release():
    return os.uname().release


def read_starttime_jiffies(pid, procfs=PROCFS):
    """starttime is field 22, after `comm` which may contain spaces and ')'.

    Split at the LAST ')' so a comm like 'name (x)' cannot shift the fields.
    """
    text = (procfs / str(pid) / "stat").read_text()
    return int(text.rsplit(")", 1)[1].split()[19])


def read_process(pid, procfs=PROCFS):
    """Live snapshot of one process. Returns a dict with alive=False when the
    PID is absent; anything else is read from the real /proc entry."""
    base = procfs / str(pid)
    if not base.is_dir():
        return {"pid": pid, "alive": False}
    status = {}
    for line in (base / "status").read_text().splitlines():
        key, _, rest = line.partition(":")
        if key in ("Name", "State", "VmRSS", "VmSize", "Threads", "VmHWM"):
            status[key] = rest.strip()
    rss_kb = _kb_field(status.get("VmRSS"))
    vm_hwm_kb = _kb_field(status.get("VmHWM"))
    vm_size_kb = _kb_field(status.get("VmSize"))
    fd_count = None
    try:
        fd_count = len(list((base / "fd").iterdir()))
    except (FileNotFoundError, PermissionError):
        pass
    try:
        proc_start = read_starttime_jiffies(pid, procfs)
        uptime = read_uptime_seconds(procfs)
    except OSError:
        proc_start = None
        uptime = None
    state = status.get("State")
    alive = bool(state) and state.split()[0] not in ("Z", "X", "x")
    return {
        "pid": pid,
        "alive": alive,
        "comm": status.get("Name"),
        "state": state,
        "rss_kb": rss_kb,
        "vm_hwm_kb": vm_hwm_kb,
        "vm_size_kb": vm_size_kb,
        "threads": _int_field(status.get("Threads")),
        "fd_count": fd_count,
        "proc_start_jiffies": proc_start,
        # uptime (wall-clock seconds since boot) minus the process start time
        # in jiffies-derived seconds: starttime counts from boot at USER_HZ=100.
        "uptime_seconds": (round(uptime - proc_start / 100.0, 3)
                           if alive and proc_start is not None and uptime is not None
                           else None),
    }


def _kb_field(value):
    if not value:
        return None
    match = re.match(r"(\d+)", value)
    return int(match.group(1)) if match else None


def _int_field(value):
    try:
        return int(str(value).split()[0])
    except (TypeError, ValueError, IndexError):
        return None


def read_fs(path):
    st = os.statvfs(path)
    return {
        "avail_bytes": st.f_bavail * st.f_frsize,
        "total_bytes": st.f_blocks * st.f_frsize,
    }


def read_system(procfs, fs_path):
    return {
        "kernel": read_kernel_release(),
        "boot_id": read_boot_id(procfs),
        "mem_available_kb": read_mem_available_kb(procfs),
        "loadavg": read_loadavg(procfs),
        "fs": read_fs(fs_path),
    }


def fingerprint(proc_record):
    return (
        proc_record.get("boot_id"),
        proc_record.get("pid"),
        proc_record.get("proc_start_jiffies"),
        proc_record.get("comm"),
    )


# ------------------------------------------------------------------ run state

def manifest_path(run_dir):
    return run_dir / "manifest.json"


def log_path(run_dir):
    return run_dir / "records.jsonl"


def validate_manifest(manifest):
    if not isinstance(manifest, dict):
        raise ValueError("manifest must be a JSON object")
    missing = [k for k in _MANIFEST_REQUIRED if k not in manifest]
    if missing:
        raise ValueError(f"manifest misses required fields: {missing}")
    if manifest["schema"] != SCHEMA or manifest["kind"] != KIND:
        raise ValueError(
            "manifest schema/kind does not match this recorder "
            f"(expected schema {SCHEMA}, kind {KIND})")
    if not isinstance(manifest["run_id"], str) or not manifest["run_id"]:
        raise ValueError("manifest must name one run_id")
    windows = manifest["declared_windows_seconds"]
    if not isinstance(windows, dict) or not windows or any(
            not isinstance(name, str) or not name or not finite_number(value)
            or value <= 0 for name, value in windows.items()):
        raise ValueError("manifest windows must have names and positive durations")
    if not finite_number(manifest["interval_seconds"]) or manifest["interval_seconds"] <= 0:
        raise ValueError("manifest interval must be positive and finite")
    for key in ("candidate", "environment", "created_by", "main_pid_source"):
        if not isinstance(manifest[key], dict):
            raise ValueError(f"manifest {key} must be an object")
    if not isinstance(manifest["extra_pids"], list):
        raise ValueError("manifest extra_pids must be a list")
    if not digest_string(manifest["created_by"].get("script_sha256")):
        raise ValueError("manifest must identify its recorder")
    return manifest


def load_manifest(run_dir):
    return validate_manifest(read_json(manifest_path(run_dir)))


def finite_number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return False
    try:
        return math.isfinite(value)
    except OverflowError:
        return False


def digest_string(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def positive_int(value):
    return type(value) is int and value > 0


def valid_fingerprint(value):
    return (isinstance(value, list) and len(value) == 4
            and (value[0] is None or isinstance(value[0], str))
            and positive_int(value[1])
            and (value[2] is None or type(value[2]) is int and value[2] >= 0)
            and (value[3] is None or isinstance(value[3], str)))


def valid_record(record):
    if not isinstance(record, dict) or record.get("schema") != SCHEMA:
        return False
    if not isinstance(record.get("run_id"), str) or not record["run_id"]:
        return False
    if not finite_number(record.get("epoch")):
        return False
    kind = record.get("record")
    if kind == "run_header":
        recorder = record.get("recorder")
        return (digest_string(record.get("manifest_sha256"))
                and isinstance(recorder, dict)
                and isinstance(recorder.get("script"), str)
                and digest_string(recorder.get("script_sha256")))
    if kind == "sample":
        main = record.get("main")
        return (positive_int(record.get("seq")) and isinstance(main, dict)
                and type(main.get("alive")) is bool
                and isinstance(record.get("clock"), dict)
                and isinstance(record.get("extra"), dict)
                and isinstance(record.get("system"), dict))
    if kind == "segment":
        previous, current = record.get("previous"), record.get("current")
        return (record.get("boundary") == "restart"
                and positive_int(record.get("sample_seq"))
                and record.get("reason") in (
                    "reappeared", "boot_id_changed", "pid_changed",
                    "starttime_changed_same_pid", "comm_changed")
                and (previous is None or valid_fingerprint(previous))
                and valid_fingerprint(current))
    return False


def scan_log(log):
    """Preserve all bytes; malformed, unknown or unterminated records invalidate
    admission, even when a trailing JSON object happens to be parseable."""
    runs = {}
    issues = {"unreadable_lines": 0, "unreadable_bytes": 0,
              "unterminated_tail_bytes": 0, "invalid_records": 0,
              "total_bytes": 0}
    if not log.exists():
        return runs, issues
    with open(log, "rb") as fh:
        for raw_line in fh:
            issues["total_bytes"] += len(raw_line)
            if not raw_line.endswith(b"\n"):
                issues["unterminated_tail_bytes"] += len(raw_line)
            try:
                record = json.loads(raw_line.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                issues["unreadable_lines"] += 1
                issues["unreadable_bytes"] += len(raw_line)
                continue
            if not valid_record(record):
                issues["invalid_records"] += 1
                continue
            run_id = record["run_id"]
            state = runs.get(run_id)
            if state is None:
                state = runs[run_id] = {
                    "headers": [], "segments": [], "samples": [],
                    "last_seq": 0, "last_main": None,
                    "first_record": record["record"],
                }
            kind = record["record"]
            if kind == "run_header":
                state["headers"].append(record)
            elif kind == "segment":
                state["segments"].append(record)
            elif kind == "sample":
                state["samples"].append(record)
                state["last_seq"] = record["seq"]
                state["last_main"] = record["main"]
    return runs, issues


def run_integrity(manifest, manifest_sha256, runs, issues):
    """One intact first header binds this directory's only run to its manifest."""
    state = runs.get(manifest["run_id"])
    headers = state["headers"] if state else []
    header = headers[0] if len(headers) == 1 else None
    manifest_drift = bool(header and header["manifest_sha256"] != manifest_sha256)
    header_valid = bool(
        header and state["first_record"] == "run_header" and not manifest_drift
        and header["recorder"] == {
            "script": manifest["created_by"].get("script"),
            "script_sha256": manifest["created_by"]["script_sha256"],
        })
    samples = state["samples"] if state else []
    seq_gaps = []
    last_seq = 0
    identity_unverified = []
    for sample in samples:
        if sample["seq"] != last_seq + 1:
            seq_gaps.append((last_seq, sample["seq"]))
        last_seq = sample["seq"]
        main = sample["main"]
        if main["alive"] and not (
                isinstance(main.get("boot_id"), str) and main["boot_id"]
                and positive_int(main.get("pid"))
                and type(main.get("proc_start_jiffies")) is int
                and main["proc_start_jiffies"] >= 0
                and isinstance(main.get("comm"), str) and main["comm"]):
            identity_unverified.append(sample["seq"])
    integrity = {
        "header_valid": header_valid,
        "header_count": len(headers),
        "manifest_drift": manifest_drift,
        "unexpected_run_ids": sorted(set(runs) - {manifest["run_id"]}),
        "unreadable_lines": issues["unreadable_lines"],
        "unreadable_bytes": issues["unreadable_bytes"],
        "unterminated_tail_bytes": issues["unterminated_tail_bytes"],
        "invalid_records": issues["invalid_records"],
        "sequence_gaps": seq_gaps,
        "identity_unverified_sample_seqs": identity_unverified,
    }
    integrity["intact"] = header_valid and not any(
        integrity[key] for key in (
            "unexpected_run_ids", "unreadable_lines", "unterminated_tail_bytes",
            "invalid_records", "sequence_gaps", "identity_unverified_sample_seqs"))
    return integrity


def restart_transition(last, main, sample_seq):
    if last is None or not main["alive"]:
        return None
    previous = fingerprint(last) if last.get("alive") else None
    current = fingerprint(main)
    if previous == current:
        return None
    reason = "reappeared"
    if previous is not None:
        if previous[0] != current[0]:
            reason = "boot_id_changed"
        elif previous[1] != current[1]:
            reason = "pid_changed"
        elif previous[2] != current[2]:
            reason = "starttime_changed_same_pid"
        else:
            reason = "comm_changed"
    return {"sample_seq": sample_seq, "reason": reason,
            "previous": list(previous) if previous is not None else None,
            "current": list(current)}


def transition_key(segment):
    return (segment["sample_seq"], segment["reason"],
            tuple(segment["previous"]) if segment["previous"] is not None else None,
            tuple(segment["current"]))


def resolve_pid(source, fixed_pid, pid_file):
    """Pick the subject PID for this sample. CLI options override the source
    recorded in the manifest; a missing/unreadable pid-file is an unresolved
    observation, never a guessed PID."""
    if pid_file is not None:
        try:
            return int(pid_file.read_text().strip())
        except (OSError, ValueError) as err:
            return {"error": f"pid-file unreadable: {err}"}
    fixed = fixed_pid if fixed_pid is not None else source.get("pid")
    if fixed is not None:
        return int(fixed)
    stored = source.get("pid_file")
    if stored:
        try:
            return int(Path(stored).read_text().strip())
        except (OSError, ValueError) as err:
            return {"error": f"pid-file unreadable: {err}"}
    return {"error": "no pid configured"}


def read_process_or_absent(pid):
    """A subject may die between the directory check and the status read; a
    vanished PID is an honest absent observation, not a sampler failure."""
    try:
        return read_process(pid)
    except OSError as err:
        return {"pid": pid, "alive": False,
                "read_error": str(err)}


# ------------------------------------------------------------------ commands

def cmd_init(args):
    run_dir = Path(args.dir)
    manifest_file = manifest_path(run_dir)
    if manifest_file.exists():
        raise SystemExit(
            f"{manifest_file} already exists; a declared manifest is immutable "
            "— start a new run_id for a changed candidate or window")
    create_run_dir_durable(run_dir)
    now = utcnow()
    windows = {}
    for item in args.window or []:
        name, _, spec = item.partition("=")
        if not name or not spec:
            raise SystemExit(f"bad --window {item!r}; use NAME=SECONDS")
        windows[name] = int(spec)
    if not windows:
        raise SystemExit("declare at least one --window NAME=SECONDS "
                         "(e.g. 7d=604800 30d=2592000)")
    manifest = {
        "schema": SCHEMA,
        "kind": KIND,
        "run_id": args.run_id,
        "workload": args.workload,
        "candidate": {
            "commit": args.candidate_commit,
            "binary_path": args.binary_path,
            "binary_sha256": args.binary_sha256,
            "source": args.candidate_source,
        },
        "environment": {
            "host": args.host,
            "os": args.os,
            "note": args.note,
        },
        "declared_windows_seconds": windows,
        "interval_seconds": args.interval,
        "main_pid_source": {"pid": args.pid,
                            "pid_file": args.pid_file},
        "extra_pids": args.extra_pid or [],
        "created_utc": iso_utc(now),
        "created_epoch": round(now, 3),
        "created_by": {
            "script": SCRIPT_NAME,
            "script_sha256": sha256_file(Path(__file__).resolve()),
            "argv": sys.argv,
        },
    }
    validate_manifest(manifest)
    write_json_durable(manifest_file, manifest)
    fsync_dir(run_dir)
    print(json.dumps({"initialized": str(manifest_file),
                      "run_id": args.run_id}, ensure_ascii=False))
    return 0


def _take_sample(run_dir, manifest, fixed_pid, pid_file, epoch=None):
    """Append one fsync'd sample record; return the record."""
    log = log_path(run_dir)
    runs, issues = scan_log(log)
    run_id = manifest["run_id"]
    state = runs.get(run_id)
    manifest_sha256 = sha256_file(manifest_path(run_dir))
    recorder = {"script": SCRIPT_NAME,
                "script_sha256": sha256_file(Path(__file__).resolve())}
    if recorder != {"script": manifest["created_by"].get("script"),
                    "script_sha256": manifest["created_by"]["script_sha256"]}:
        raise ValueError("recorder changed since init; preserve this run and start a new one")
    if issues["total_bytes"] and not run_integrity(
            manifest, manifest_sha256, runs, issues)["intact"]:
        raise ValueError("refusing append to invalid evidence; preserve all bytes and start a new run")
    epoch = utcnow() if epoch is None else epoch
    clock = read_sampler_clock()
    if state is None:
        append_record(log, {
            "record": "run_header",
            "schema": SCHEMA,
            "run_id": run_id,
            "manifest_sha256": manifest_sha256,
            "recorder": recorder,
            "epoch": epoch,
            "wall_utc": iso_utc(epoch),
        })
        state = {"segments": [], "last_seq": 0, "last_main": None}

    resolved = resolve_pid(manifest["main_pid_source"], fixed_pid, pid_file)
    if isinstance(resolved, dict):
        main = {"pid": None, "alive": False,
                "unresolved": resolved["error"]}
    else:
        main = read_process_or_absent(resolved)
        if main["alive"]:
            main["boot_id"] = clock["boot_id"]

    # A boundary belongs to the next sample sequence. A failed sample write
    # may leave its complete boundary durable; retry that transition, not a
    # second restart. Reports count only boundaries corroborated by samples.
    transition = restart_transition(state["last_main"], main, state["last_seq"] + 1)
    if transition is not None and not any(
            transition_key(segment) == transition_key(transition)
            for segment in state["segments"]):
        append_record(log, {
            "record": "segment", "schema": SCHEMA, "run_id": run_id,
            "boundary": "restart", **transition,
            "epoch": epoch, "wall_utc": iso_utc(epoch),
        })

    extra = {}
    for pid in manifest["extra_pids"]:
        try:
            extra[str(pid)] = read_process_or_absent(int(pid))
        except (TypeError, ValueError) as err:
            extra[str(pid)] = {"pid": pid, "alive": False,
                               "read_error": str(err)}

    try:
        system = read_system(PROCFS, run_dir)
    except OSError as err:
        system = {"read_error": str(err)}

    record = {
        "record": "sample",
        "schema": SCHEMA,
        "run_id": run_id,
        "seq": state["last_seq"] + 1,
        "epoch": round(epoch, 3),
        "wall_utc": iso_utc(epoch),
        "clock": clock,
        "main": main,
        "extra": extra,
        "system": system,
    }
    append_record(log, record)
    return record


def cmd_sample(args):
    run_dir = Path(args.dir)
    manifest = load_manifest(run_dir)
    record = _take_sample(run_dir, manifest, args.pid, Path(args.pid_file)
                          if args.pid_file else None)
    print(json.dumps(record, ensure_ascii=False))
    return 0


def cmd_run(args):
    run_dir = Path(args.dir)
    manifest = load_manifest(run_dir)
    pid_file = Path(args.pid_file) if args.pid_file else None
    deadline = utcnow() + args.deadline_seconds
    interval = args.interval or manifest["interval_seconds"]
    count = 0
    while True:
        _take_sample(run_dir, manifest, args.pid, pid_file)
        count += 1
        if args.max_samples and count >= args.max_samples:
            break
        remaining = deadline - utcnow()
        if remaining <= 0:
            break
        time.sleep(min(interval, remaining))
    print(json.dumps({"run_id": manifest["run_id"],
                      "samples_taken": count,
                      "deadline_reached": utcnow() >= deadline},
                     ensure_ascii=False))
    return 0


# -------------------------------------------------------------------- report

def cmd_report(args):
    run_dir = Path(args.dir)
    manifest = load_manifest(run_dir)
    if not finite_number(args.gap_factor) or not 1 <= args.gap_factor <= 3:
        raise ValueError("gap-factor must be between 1 and 3; known downtime cannot be waived")
    runs, issues = scan_log(log_path(run_dir))
    run_id = args.run_id or manifest["run_id"]
    if run_id != manifest["run_id"]:
        raise ValueError("requested run_id does not match this directory's manifest")
    if run_id not in runs:
        raise SystemExit(f"no records for run {run_id!r} in {log_path(run_dir)}")
    state = runs[run_id]
    samples = state["samples"]
    integrity = run_integrity(
        manifest, sha256_file(manifest_path(run_dir)), runs, issues)
    epochs = [s["epoch"] for s in samples]
    span = (epochs[-1] - epochs[0]) if len(epochs) >= 2 else 0.0
    gaps = []
    unobserved = 0.0
    qualifying_elapsed = 0.0
    clock_unverified = []
    clock_discontinuities = []
    for sample in samples:
        clock = sample["clock"]
        if not (isinstance(clock.get("boot_id"), str) and clock["boot_id"]
                and finite_number(clock.get("uptime_seconds"))
                and clock["uptime_seconds"] >= 0
                and (not sample["main"]["alive"]
                     or clock["boot_id"] == sample["main"].get("boot_id"))):
            clock_unverified.append(sample["seq"])
    for prev, cur in zip(samples, samples[1:]):
        wall_gap = cur["epoch"] - prev["epoch"]
        same_clock = (prev["seq"] not in clock_unverified
                      and cur["seq"] not in clock_unverified
                      and prev["clock"]["boot_id"] == cur["clock"]["boot_id"])
        uptime_gap = (cur["clock"]["uptime_seconds"]
                      - prev["clock"]["uptime_seconds"]) if same_clock else None
        if wall_gap <= 0 or uptime_gap is None or uptime_gap <= 0:
            clock_discontinuities.append((prev["seq"], cur["seq"]))
        else:
            # A forward wall-clock step cannot manufacture elapsed coverage.
            qualifying_elapsed += min(wall_gap, uptime_gap)
        gap = max(wall_gap, uptime_gap if uptime_gap is not None else 0.0)
        if gap > manifest["interval_seconds"] * args.gap_factor:
            gaps.append({"after_seq": prev["seq"],
                         "gap_seconds": round(gap, 3)})
            unobserved += gap
    alive = [s for s in samples if s.get("main", {}).get("alive")]
    absent = [s for s in samples if not s.get("main", {}).get("alive")]
    rss_values = [s["main"]["rss_kb"] for s in alive
                  if s["main"].get("rss_kb") is not None]
    rss = None
    if rss_values:
        rss = {
            "sample_count": len(rss_values),
            "min_kb": min(rss_values),
            "max_kb": max(rss_values),
            "first_kb": rss_values[0],
            "last_kb": rss_values[-1],
            "drift_kb": rss_values[-1] - rss_values[0],
            "mean_kb": round(sum(rss_values) / len(rss_values), 1),
        }
    expected_restarts = set()
    for prev, cur in zip(samples, samples[1:]):
        transition = restart_transition(prev["main"], cur["main"], cur["seq"])
        if transition is not None:
            expected_restarts.add(transition_key(transition))
    recorded_restarts = {transition_key(seg): seg for seg in state["segments"]}
    restarts = [seg for key, seg in recorded_restarts.items()
                if key in expected_restarts]
    unmatched_restarts = len(set(recorded_restarts) - expected_restarts)
    missing_restarts = len(expected_restarts - set(recorded_restarts))
    integrity.update({
        "clock_unverified_sample_seqs": clock_unverified,
        "clock_discontinuities": clock_discontinuities,
        "unmatched_restart_boundaries": unmatched_restarts,
        "missing_restart_boundaries": missing_restarts,
        "record_count": len(samples),
        "sample_records": len(alive) + len(absent),
        "first_sample_epoch": epochs[0] if epochs else None,
        "last_sample_epoch": epochs[-1] if epochs else None,
    })
    integrity["intact"] = integrity["intact"] and not any((
        clock_unverified, clock_discontinuities, unmatched_restarts, missing_restarts))
    # Deliberately reject the whole declared run when any known gap exists;
    # do not splice disconnected spans into a 7/30-day observation claim.
    qualifying_span = qualifying_elapsed if integrity["intact"] and not gaps else 0.0
    declared = []
    for name, required in manifest["declared_windows_seconds"].items():
        covered = bool(samples) and qualifying_span >= required
        declared.append({
            "window": name,
            "required_seconds": required,
            "observed_span_seconds": round(span, 3),
            "qualifying_span_seconds": round(qualifying_span, 3),
            "max_inter_sample_gap_seconds": round(
                max([cur - prev for prev, cur in zip(epochs, epochs[1:])],
                    default=0.0), 3),
            "largest_gaps": gaps,
            "unobserved_seconds": round(unobserved, 3),
            "status": "observed" if covered else "unobserved",
        })
    all_observed = all(w["status"] == "observed" for w in declared) \
        and bool(samples)
    # Subject uptime is approximated between samples: a sample only proves a
    # point in time. Report the share of samples seeing the subject alive.
    uptime_fraction = (len(alive) / len(samples)) if samples else None
    segments = []
    current = None
    for s in samples:
        fp = fingerprint(s["main"]) if s.get("main", {}).get("alive") else None
        if fp != current:
            current = fp
            segments.append({"fingerprint": fp,
                             "first_seen_epoch": s["epoch"],
                             "last_seen_epoch": s["epoch"],
                             "samples": 0})
        if fp is not None:
            segments[-1]["last_seen_epoch"] = s["epoch"]
            segments[-1]["samples"] += 1
    report = {
        "schema": SCHEMA,
        "kind": "c19_long_run_observation_report",
        "run_id": run_id,
        "generated_utc": iso_utc(utcnow()),
        "subject": manifest["workload"],
        "candidate": manifest["candidate"],
        "environment": manifest["environment"],
        "samples": len(samples),
        "subject_alive_samples": len(alive),
        "subject_absent_samples": len(absent),
        "subject_sample_uptime_fraction": uptime_fraction,
        "restart_boundaries": len(restarts),
        "restart_reasons": sorted({r["reason"] for r in restarts}),
        "segments": [
            {k: v for k, v in seg.items() if k != "fingerprint"} | {
                "pid": seg["fingerprint"][1] if seg["fingerprint"] else None,
                "proc_start_jiffies":
                    seg["fingerprint"][2] if seg["fingerprint"] else None,
            }
            for seg in segments],
        "rss_main_process": rss,
        "integrity": integrity,
        "declared_windows": declared,
        "verdict": ("all_declared_windows_observed"
                    if all_observed else "incomplete_observation"),
        "never_claimed": [
            "samples are points in time; behaviour between them is not proven",
            "absent/dead subject or sampler downtime stays recorded, not filled",
            "endpoint span alone never qualifies a window",
            "damaged, unbound, clock-discontinuous or gapped runs stay unobserved",
        ],
    }
    if args.write:
        out = run_dir / "reports" / f"report-{iso_utc(utcnow()).replace(':', '-')}.json"
        out.parent.mkdir(exist_ok=True)
        write_json_durable(out, report)
        fsync_dir(out.parent)
    print(json.dumps(report, ensure_ascii=False, indent=2))
    if args.require_observed and not all_observed:
        return 3
    return 0


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)

    init = sub.add_parser("init", help="freeze the run manifest")
    init.add_argument("--dir", required=True)
    init.add_argument("--run-id", required=True)
    init.add_argument("--workload", required=True,
                      help="what is being observed (e.g. 'shenko start C=1')")
    init.add_argument("--candidate-commit")
    init.add_argument("--candidate-source")
    init.add_argument("--binary-path")
    init.add_argument("--binary-sha256")
    init.add_argument("--host")
    init.add_argument("--os")
    init.add_argument("--note")
    init.add_argument("--window", action="append",
                      help="NAME=SECONDS; repeatable, e.g. 7d=604800")
    init.add_argument("--interval", type=float,
                      default=DEFAULT_STATE_INTERVAL)
    init.add_argument("--pid", type=int)
    init.add_argument("--pid-file")
    init.add_argument("--extra-pid", type=int, action="append")
    init.set_defaults(func=cmd_init)

    for name, func in (("sample", cmd_sample), ("run", cmd_run)):
        sp = sub.add_parser(name)
        sp.add_argument("--dir", required=True)
        sp.add_argument("--pid", type=int)
        sp.add_argument("--pid-file")
        if name == "run":
            sp.add_argument("--interval", type=float)
            sp.add_argument("--deadline-seconds", type=float, required=True)
            sp.add_argument("--max-samples", type=int)
        sp.set_defaults(func=func)

    report = sub.add_parser("report")
    report.add_argument("--dir", required=True)
    report.add_argument("--run-id")
    report.add_argument("--gap-factor", type=float, default=3.0,
                        help="inter-sample gap above interval*factor counts "
                             "as unobserved time; 1..3 only (default 3.0)")
    report.add_argument("--require-observed", action="store_true",
                        help="exit 3 unless every declared window is observed")
    report.add_argument("--write", action="store_true",
                        help="also persist the report under DIR/reports/")
    report.set_defaults(func=cmd_report)
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return args.func(args)
    except (OSError, ValueError) as err:
        parser.exit(2, f"error: {err}\n")


if __name__ == "__main__":
    sys.exit(main())
