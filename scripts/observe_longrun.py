#!/usr/bin/env python3
"""Scheduled, durable observation capture for real 7/30-day runs — C19 / SPEC S5.

This tool only *records* what a bounded sampler actually saw. It never
extrapolates: `report` compares the recorded samples against the declared
windows written at `init` time and marks any window that the data does not
cover as `unobserved`. Missing samples, subject restarts, absent processes,
downtime of the sampler itself and torn tails are surfaced, not hidden.

Layout of the run directory (all writes are fsync'd):

  manifest.json      frozen at `init`: candidate/environment identity and the
                     *declared* observation windows. Editing it after samples
                     exist is detected as manifest drift at report time.
  records.jsonl      append-only records, each one JSON line fsynced before
                     the write returns: run_header, segment and sample records.

Records are independent per run_id; one directory may host several runs (e.g.
a new candidate closure must start a new run rather than silently splicing
time onto an old one — see docs/governance/COMPLETION.md §4).

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
import os
import re
import sys
import time
from pathlib import Path

SCHEMA = 1
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
            view = view[os.write(fd, view):]
        os.fsync(fd)
    finally:
        os.close(fd)


def read_json(path):
    return json.loads(path.read_text(encoding="utf-8"))


# ---------------------------------------------------------------- /proc reads

def read_boot_id(procfs=PROCFS):
    """Kernel boot identifier for restart attribution; None when the kernel
    does not expose it (restricted procfs). Never fabricated."""
    try:
        return (procfs / "sys/kernel/boot_id").read_text().strip()
    except OSError:
        return None


def read_uptime_seconds(procfs=PROCFS):
    return float((procfs / "uptime").read_text().split()[0])


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
    return {
        "pid": pid,
        "alive": True,
        "comm": status.get("Name"),
        "state": status.get("State"),
        "rss_kb": rss_kb,
        "vm_hwm_kb": vm_hwm_kb,
        "vm_size_kb": vm_size_kb,
        "threads": _int_field(status.get("Threads")),
        "fd_count": fd_count,
        "proc_start_jiffies": proc_start,
        # uptime (wall-clock seconds since boot) minus the process start time
        # in jiffies-derived seconds: starttime counts from boot at USER_HZ=100.
        "uptime_seconds": (round(uptime - proc_start / 100.0, 3)
                           if proc_start is not None and uptime is not None
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
    missing = [k for k in _MANIFEST_REQUIRED if k not in manifest]
    if missing:
        raise ValueError(f"manifest misses required fields: {missing}")
    if manifest["schema"] != SCHEMA or manifest["kind"] != KIND:
        raise ValueError(
            "manifest schema/kind does not match this recorder "
            f"(expected schema {SCHEMA}, kind {KIND})")
    if not manifest["declared_windows_seconds"]:
        raise ValueError("manifest declares no observation windows")
    return manifest


def load_manifest(run_dir):
    return validate_manifest(read_json(manifest_path(run_dir)))


def scan_log(log):
    """Fold records.jsonl into per-run state. A trailing torn line (power loss
    mid-append) is counted as unreadable bytes, never silently skipped."""
    runs = {}
    order = []
    issues = {"unreadable_lines": 0, "unreadable_bytes": 0}
    if not log.exists():
        return runs, order, issues
    with open(log, "rb") as fh:
        for raw_line in fh:
            line = raw_line.decode("utf-8", "replace").strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                issues["unreadable_lines"] += 1
                issues["unreadable_bytes"] += len(raw_line)
                continue
            run_id = record.get("run_id")
            if not isinstance(run_id, str):
                continue
            state = runs.get(run_id)
            if state is None:
                state = runs[run_id] = {
                    "header": None, "segments": [], "samples": [],
                    "last_seq": 0, "last_main": None,
                }
                order.append(run_id)
            kind = record.get("record")
            if kind == "run_header":
                state["header"] = record
            elif kind == "segment":
                state["segments"].append(record)
            elif kind == "sample":
                state["samples"].append(record)
                state["last_seq"] = max(state["last_seq"],
                                        record.get("seq", 0))
                state["last_main"] = record.get("main")
    return runs, order, issues


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
    run_dir.mkdir(parents=True, exist_ok=True)
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
    write_json_durable(manifest_file, manifest)
    fsync_dir(run_dir)
    print(json.dumps({"initialized": str(manifest_file),
                      "run_id": args.run_id}, ensure_ascii=False))
    return 0


def _take_sample(run_dir, manifest, fixed_pid, pid_file, epoch=None):
    """Append one fsync'd sample record; return the record."""
    log = log_path(run_dir)
    runs, order, _ = scan_log(log)
    run_id = manifest["run_id"]
    state = runs.get(run_id)
    epoch = utcnow() if epoch is None else epoch
    if state is None or state["header"] is None:
        append_record(log, {
            "record": "run_header",
            "schema": SCHEMA,
            "run_id": run_id,
            "manifest_sha256": sha256_file(manifest_path(run_dir)),
            "recorder": {"script": SCRIPT_NAME,
                         "script_sha256": sha256_file(
                             Path(__file__).resolve())},
            "epoch": epoch,
            "wall_utc": iso_utc(epoch),
        })
        state = {"header": True, "segments": [], "samples": [],
                 "last_seq": 0, "last_main": None}

    resolved = resolve_pid(manifest["main_pid_source"], fixed_pid, pid_file)
    if isinstance(resolved, dict):
        main = {"pid": None, "alive": False,
                "unresolved": resolved["error"]}
    else:
        main = read_process_or_absent(resolved)
        if main["alive"]:
            main["boot_id"] = read_boot_id()

    # Segment boundaries keep a changed identity explicit: a restarted
    # subject never silently continues the previous segment's history.
    last = state["last_main"]
    if main["alive"]:
        last_fp = fingerprint(last) if last and last.get("alive") else None
        cur_fp = fingerprint(main)
        if last is not None and last_fp != cur_fp:
            reason = "reappeared"
            if last.get("alive"):
                if last_fp[:1] != cur_fp[:1]:
                    reason = "boot_id_changed"
                elif last_fp[2:3] != cur_fp[2:3]:
                    reason = "starttime_changed_same_pid"
                elif last_fp[1:2] != cur_fp[1:2]:
                    reason = "pid_changed"
                else:
                    reason = "comm_changed"
            append_record(log, {
                "record": "segment",
                "schema": SCHEMA,
                "run_id": run_id,
                "boundary": "restart",
                "reason": reason,
                "previous": last_fp,
                "current": cur_fp,
                "epoch": epoch,
                "wall_utc": iso_utc(epoch),
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
    runs, order, issues = scan_log(log_path(run_dir))
    run_id = args.run_id or (order[-1] if order else None)
    if run_id is None or run_id not in runs:
        raise SystemExit(f"no records for run {run_id!r} in {log_path(run_dir)}")
    state = runs[run_id]
    samples = state["samples"]
    seq_gaps = [
        (prev["seq"], cur["seq"]) for prev, cur in zip(samples, samples[1:])
        if cur["seq"] != prev["seq"] + 1
    ]
    epochs = [s["epoch"] for s in samples]
    span = (epochs[-1] - epochs[0]) if len(epochs) >= 2 else 0.0
    gaps = []
    unobserved = 0.0
    for idx, (prev, cur) in enumerate(zip(epochs, epochs[1:])):
        gap = cur - prev
        if gap > manifest["interval_seconds"] * args.gap_factor:
            gaps.append({"after_seq": samples[idx]["seq"],
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
    restarts = [seg for seg in state["segments"]
                if seg.get("boundary") == "restart"]
    manifest_drift = bool(
        state["header"]
        and state["header"].get("manifest_sha256") !=
        sha256_file(manifest_path(run_dir)))
    integrity = {
        "manifest_drift": manifest_drift,
        "unreadable_lines": issues["unreadable_lines"],
        "unreadable_bytes": issues["unreadable_bytes"],
        "sequence_gaps": seq_gaps,
        "record_count": len(samples),
        "sample_records": len(alive) + len(absent),
        "first_sample_epoch": epochs[0] if epochs else None,
        "last_sample_epoch": epochs[-1] if epochs else None,
    }
    declared = []
    for name, required in manifest["declared_windows_seconds"].items():
        covered = bool(epochs) and span >= required and not manifest_drift
        declared.append({
            "window": name,
            "required_seconds": required,
            "observed_span_seconds": round(span, 3),
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
            "windows exceeding the recorded span stay unobserved",
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
                             "as unobserved time (default 3.0)")
    report.add_argument("--require-observed", action="store_true",
                        help="exit 3 unless every declared window is observed")
    report.add_argument("--write", action="store_true",
                        help="also persist the report under DIR/reports/")
    report.set_defaults(func=cmd_report)
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
