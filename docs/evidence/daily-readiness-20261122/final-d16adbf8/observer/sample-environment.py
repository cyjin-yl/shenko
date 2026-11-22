#!/usr/bin/env python3
"""Bounded Linux environment evidence; no model, Store or process-argument reads."""

import argparse
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import select
import signal
import time


MEMORY_FIELDS = (
    "MemTotal", "MemAvailable", "MemFree", "Cached", "Dirty", "Writeback",
    "SwapTotal", "SwapFree", "SReclaimable", "Slab",
)


def positive_seconds(value):
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError("must be finite and greater than zero")
    return number


def positive_pid(value):
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be a positive PID")
    return number


def read_bounded(path, limit):
    with open(path, "rb") as source:
        value = source.read(limit + 1)
    if len(value) > limit:
        raise ValueError("proc observation exceeded its size bound")
    return value


def error_fact(error):
    return {"type": type(error).__name__, "errno": getattr(error, "errno", None)}


def parent_identity(pid):
    # Only state and start ticks are retained. The comm field is discarded;
    # cmdline, environ, process descriptors and other processes are never read.
    try:
        raw = read_bounded(f"/proc/{pid}/stat", 8192)
        fields = raw.rsplit(b") ", 1)[1].split()
        return {"state": fields[0].decode("ascii"), "start_ticks": int(fields[19])}, None
    except FileNotFoundError:
        return None, {"type": "parent_absent"}
    except (OSError, ValueError, IndexError, UnicodeError) as error:
        return None, error_fact(error)


def pressure(kind):
    rows = {}
    for line in read_bounded(f"/proc/pressure/{kind}", 4096).decode("ascii").splitlines():
        name, *values = line.split()
        row = dict(value.split("=", 1) for value in values)
        rows[name] = {key: int(row[key]) if key == "total" else float(row[key])
                      for key in ("avg10", "avg60", "avg300", "total")}
    if "some" not in rows:
        raise ValueError("pressure observation lacks some row")
    return rows


def load_average():
    values = read_bounded("/proc/loadavg", 1024).split()
    runnable, total = values[3].split(b"/", 1)
    return {"one_minute": float(values[0]), "five_minutes": float(values[1]),
            "fifteen_minutes": float(values[2]), "runnable_tasks": int(runnable),
            "total_tasks": int(total)}


def memory():
    rows = {}
    for line in read_bounded("/proc/meminfo", 65536).decode("ascii").splitlines():
        key, value = line.split(":", 1)
        if key in MEMORY_FIELDS:
            number, unit = value.split()
            if unit != "kB":
                raise ValueError("unexpected meminfo unit")
            rows[key] = int(number)
    return {key: rows.get(key) for key in MEMORY_FIELDS}


def environment():
    result = {"pressure": {}, "loadavg": None, "memory_kib": None, "errors": []}
    readers = [(f"pressure.{name}", lambda name=name: pressure(name))
               for name in ("io", "cpu", "memory")]
    readers.extend((("loadavg", load_average), ("memory_kib", memory)))
    for name, reader in readers:
        try:
            value = reader()
        except (OSError, ValueError, KeyError, IndexError, UnicodeError) as error:
            value = None
            result["errors"].append({"source": name, **error_fact(error)})
        if name.startswith("pressure."):
            result["pressure"][name.split(".", 1)[1]] = value
        else:
            result[name] = value
    if result["memory_kib"] is not None:
        missing = [key for key, value in result["memory_kib"].items() if value is None]
        if missing:
            result["errors"].append({"source": "memory_kib", "type": "missing_fields", "fields": missing})
    return result


def run(args):
    started = time.monotonic_ns()
    stop_signal = None
    samples = metric_errors = 0
    read_fd, write_fd = os.pipe2(os.O_NONBLOCK | os.O_CLOEXEC)
    previous_wakeup = signal.set_wakeup_fd(write_fd)
    previous_handlers = {}

    def request_stop(signum, _frame):
        nonlocal stop_signal
        stop_signal = signum

    def stamp():
        monotonic = time.monotonic_ns()
        return {"utc": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="microseconds"),
                "monotonic_ns": monotonic, "elapsed_ns": monotonic - started}

    try:
        for signum in (signal.SIGTERM, signal.SIGINT):
            previous_handlers[signum] = signal.signal(signum, request_stop)
        # Exclusive creation prevents an unrelated observation from being
        # overwritten or mixed in. Line flushes do not add fsync write load.
        with args.output.open("x", encoding="utf-8", buffering=1) as output:
            def emit(kind, body, observed=None):
                row = {"schema_version": 1, "record": kind, **(observed or stamp()), **body}
                output.write(json.dumps(row, separators=(",", ":"), allow_nan=False) + "\n")

            original, parent_error = parent_identity(args.parent_pid)
            emit("start", {"pid": os.getpid(), "parent_pid": args.parent_pid,
                           "parent_identity": original, "parent_error": parent_error,
                           "interval_seconds": args.interval, "max_duration_seconds": args.max_duration,
                           "sampler_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                           "scope": "system counters visible through this process's /proc; not a per-task or cgroup attribution"})
            reason, exit_code = "max_duration", 0
            next_sample = time.monotonic()
            deadline = started / 1_000_000_000 + args.max_duration
            while True:
                if stop_signal is not None:
                    reason = signal.Signals(stop_signal).name
                    break
                if original is None:
                    reason, exit_code = "parent_unavailable_at_start", 1
                    break
                current, parent_error = parent_identity(args.parent_pid)
                if current is None:
                    reason = "parent_exited" if parent_error["type"] == "parent_absent" else "parent_unobservable"
                    exit_code = 0 if reason == "parent_exited" else 1
                    break
                if current["start_ticks"] != original["start_ticks"]:
                    reason = "parent_replaced"
                    break
                if current["state"] in ("Z", "X", "x"):
                    reason = "parent_exited"
                    break
                now = time.monotonic()
                if now >= deadline:
                    break
                if now >= next_sample:
                    observed = stamp()
                    data = environment()
                    metric_errors += len(data["errors"])
                    emit("sample", {"seq": samples, "parent_state": current["state"],
                                    "read_elapsed_ns": time.monotonic_ns() - observed["monotonic_ns"],
                                    **data}, observed)
                    samples += 1
                    # Never burst to catch up after a delayed read/write.
                    next_sample = time.monotonic() + args.interval
                timeout = max(0, min(next_sample, deadline) - time.monotonic())
                ready, _, _ = select.select([read_fd], [], [], timeout)
                if ready:
                    os.read(read_fd, 4096)
            emit("end", {"reason": reason, "samples": samples, "metric_error_count": metric_errors,
                         "parent_error": parent_error, "exit_code": exit_code})
            return exit_code
    finally:
        signal.set_wakeup_fd(previous_wakeup)
        for signum, handler in previous_handlers.items():
            signal.signal(signum, handler)
        os.close(read_fd)
        os.close(write_fd)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parent-pid", type=positive_pid, required=True,
                        help="runner PID in the sampler's PID namespace; never signalled")
    parser.add_argument("--interval", type=positive_seconds, default=5.0)
    parser.add_argument("--max-duration", type=positive_seconds, required=True,
                        help="maximum elapsed seconds; also stops when the original parent exits")
    parser.add_argument("--output", type=Path, required=True, help="new NDJSON path; parent directory must exist")
    return run(parser.parse_args())


if __name__ == "__main__":
    raise SystemExit(main())
