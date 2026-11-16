#!/usr/bin/env python3
"""One pinned Node-WASIX feasibility journey; never installs a Shenko capability."""

import argparse
import ctypes
import datetime
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import resource
import shutil
import signal
import stat
import struct
import subprocess
import sys
import tarfile
import time
import traceback
import urllib.error
import urllib.request


MIB = 1024 * 1024
GIB = 1024 * MIB
WASMER_URL = "https://github.com/wasmerio/wasmer/releases/download/v7.5.0/wasmer-linux-amd64.tar.gz"
WASMER_BYTES = 302721336
WASMER_SHA256 = "23f99818deaad93859f8bb8292af81f0c39f25aea19eaccfb72fc7c9e8a3fae9"
EXECUTABLE_SHA256 = "72c57ef6be45d500cb528c0a684b13159620ddfb4b50d3e429a3a352d0b3f116"
SELECTED_MEMBERS = {"bin/wasmer": 219153104, "LICENSE": 1097, "ATTRIBUTIONS": 35351}
NODE_URL = "https://github.com/Multi-V-VM/node-wasix32/releases/download/wasix32/node.wasm"
NODE_BYTES = 112379740
NODE_SHA256 = "3e0b4c9f9c258fef4ecf522bf86c42844d50368138abb86d5c2700775cf84a1d"
DRIVER_PATH = "crates/shenko-repl/src/drivers/js_cell_driver.mjs"
DRIVER_SHA256 = "a9419c9aa0abbced7c69145806768153971d37121a8c20e63e0c623f0991f150"
FIRST_INPUT_SHA256 = "f1c895141dd1cbb589de36e6c93900da14b1f24f96ee24c2016afc303d43c0dc"
ADDRESS_SPACE_BYTES = 12 * GIB
WALL_SECONDS = 180
CPU_SECONDS = 150
DENIED_NETWORK = ["socket", "connect", "bind", "listen", "accept", "accept4", "sendto", "sendmsg", "sendmmsg"]
STOP_SIGNAL = None

# These are the unchanged sources from both retained local attempts.
FIRST_SOURCE = '''let probeValue = 37 * 41;
console.log(JSON.stringify({phase: "node-wasix-runtime", release: process.release.name, version: process.version, versions: process.versions, arch: process.arch, platform: process.platform}));
require("node:fs").writeFileSync("/workspace/witness.json", JSON.stringify({value: probeValue}), {flag: "wx"});
if (JSON.parse(require("node:fs").readFileSync("/workspace/witness.json", "utf8")).value !== 1517) throw new Error("real fs readback mismatch");
console.log(JSON.stringify({phase: "first-cell", value: probeValue, fs_readback: true}));
'''
SECOND_SOURCE = '''probeValue += 1;
if (probeValue !== 1518) throw new Error("live lexical state was lost");
console.log(JSON.stringify({phase: "second-cell", value: probeValue}));
'''
REOPEN_SOURCE = '''probeValue += 1;
if (probeValue !== 1519) throw new Error("reopened JSON checkpoint was not restored");
if (JSON.parse(require("node:fs").readFileSync("/workspace/witness.json", "utf8")).value !== 1517) throw new Error("original fs witness changed");
console.log(JSON.stringify({phase: "reopened-cell", value: probeValue, original_fs_witness: 1517}));
'''


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    with temporary.open("w") as output:
        json.dump(value, output, indent=2)
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())
    temporary.replace(path)


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def frames(cells):
    data = bytearray()
    for seq, source in cells:
        header = json.dumps({"seq": seq, "cell_dir": "/workspace/cells", "state_path": "/workspace/state.json", "env_dir": "/workspace/env"}).encode()
        body = source.encode()
        data.extend(struct.pack(">Q", len(header)) + header + struct.pack(">Q", len(body)) + body)
    return bytes(data)


def optional_text(path):
    try:
        return {"value": path.read_text().strip()}
    except OSError as error:
        return {"error": type(error).__name__, "errno": error.errno}


def mount_path(value):
    return re.sub(r"\\([0-7]{3})", lambda match: chr(int(match[1], 8)), value)


class VisibleResources:
    """Observe the current v2 membership and every ancestor up to its mount."""

    def __init__(self, work):
        self.work = work
        self.membership = Path("/proc/self/cgroup").read_text()
        self.mounts = []
        self.levels = []
        self.error = None
        for line in Path("/proc/self/mountinfo").read_text().splitlines():
            before, _, after = line.partition(" - ")
            if after.split()[0] == "cgroup2":
                fields = before.split()
                self.mounts.append({"root": mount_path(fields[3]), "mountpoint": mount_path(fields[4])})
        try:
            membership = next(line[3:] for line in self.membership.splitlines() if line.startswith("0::"))
            current = PurePosixPath(membership)
            require(current.is_absolute() and ".." not in current.parts, "invalid cgroup v2 membership")
            candidates = [item for item in self.mounts if current.is_relative_to(PurePosixPath(item["root"]))]
            require(bool(candidates), "no visible cgroup v2 mount for this process")
            selected = max(candidates, key=lambda item: len(PurePosixPath(item["root"]).parts))
            require(selected["root"] == "/", "visible cgroup mount hides ancestors; resource admission is unavailable")
            mount = Path(selected["mountpoint"]).resolve(strict=True)
            node = mount.joinpath(*current.relative_to("/").parts)
            require(node.resolve(strict=True).is_relative_to(mount), "cgroup membership escapes its mount")
            while True:
                self.levels.append(node)
                if node == mount:
                    break
                node = node.parent
        except (OSError, StopIteration, RuntimeError) as error:
            self.error = str(error) or type(error).__name__

    def snapshot(self):
        memory = {}
        for line in Path("/proc/meminfo").read_text().splitlines():
            key, _, value = line.partition(":")
            if key in ("MemTotal", "MemAvailable", "SwapTotal", "SwapFree", "CommitLimit", "Committed_AS"):
                memory[key] = int(value.split()[0]) * 1024
        result = {
            "observed_at": now(), "memory_bytes": memory,
            "disk_free_bytes": shutil.disk_usage(self.work).free,
            "process_cgroup": Path("/proc/self/cgroup").read_text(),
            "cgroup_mounts": self.mounts, "cgroup_levels": [],
            "cpu_affinity": sorted(os.sched_getaffinity(0)),
            "scope": "system memory and visible cgroup v2 ancestors; unexposed host ancestors are not observed",
            "error": self.error,
        }
        headrooms = [memory["MemAvailable"]]
        require(result["process_cgroup"] == self.membership, "process cgroup changed during the observation")
        for index, node in enumerate(self.levels):
            values = {name: optional_text(node / name) for name in (
                "memory.current", "memory.max", "memory.high", "memory.events", "memory.swap.max",
                "cpu.max", "cpuset.cpus.effective", "cgroup.controllers",
            )}
            level = {"path": str(node), "files": values}
            result["cgroup_levels"].append(level)
            # The actual hierarchy root has no memory controller limit files.
            # Do not apply that exception to an unreadable non-root ancestor.
            root_without_limits = index == len(self.levels) - 1 and all(
                values[name].get("errno") == errno.ENOENT for name in ("memory.current", "memory.max", "memory.high")
            ) and "value" in values["cgroup.controllers"]
            if root_without_limits:
                level["hierarchy_root_without_limit_files"] = True
                continue
            if not all("value" in values[name] for name in ("memory.current", "memory.max", "memory.high")):
                result["error"] = "memory limits or usage unavailable for a visible cgroup ancestor"
                continue
            current = int(values["memory.current"]["value"])
            for name in ("memory.max", "memory.high"):
                raw = values[name]["value"]
                if raw != "max":
                    limit = int(raw)
                    require(current >= 0 and limit >= 0, "negative cgroup memory observation")
                    headrooms.append(max(0, limit - current))
        if not self.levels:
            result["error"] = result["error"] or "no visible cgroup ancestor observations"
        result["effective_headroom_bytes"] = min(headrooms)
        return result


def choose_budget(observation):
    require(observation["error"] is None, observation["error"])
    headroom = observation["effective_headroom_bytes"]
    reserve = max(GIB, headroom // 4)
    rss = min(6 * GIB, headroom - reserve) // (64 * MIB) * (64 * MIB)
    require(rss >= 2 * GIB, "observed capacity cannot leave the reserve and a 2 GiB starting RSS budget")
    disk_reserve = max(GIB, min(4 * GIB, observation["disk_free_bytes"] // 5))
    return {
        "rss_monitor_bytes": rss, "available_memory_reserve_bytes": reserve,
        "free_disk_reserve_bytes": disk_reserve, "address_space_hard_bytes": ADDRESS_SPACE_BYTES,
        "wall_seconds_per_guest": WALL_SECONDS, "cpu_soft_seconds": CPU_SECONDS,
        "file_hard_bytes": 16 * MIB, "monitor_interval_seconds": 0.05,
        "basis": "min(system MemAvailable, finite visible ancestor max/high minus current); reserve max(1 GiB, one quarter); RSS remainder capped at 6 GiB and rounded down to 64 MiB",
        "enforcement": "RLIMIT_AS/CPU/file-size are hard per-process limits; RSS and remaining headroom are sampled group/system/cgroup guards, not hard resident-memory enforcement",
    }


class DownloadReader:
    def __init__(self, source, size, deadline, work, reserve):
        self.source, self.size, self.deadline = source, size, deadline
        self.work, self.reserve = work, reserve
        self.count = 0
        self.sha = hashlib.sha256()

    def read(self, size=-1):
        require(STOP_SIGNAL is None, "download interrupted by signal")
        require(time.monotonic() < self.deadline, "asset download deadline exceeded")
        require(shutil.disk_usage(self.work).free > self.reserve, "download disk reserve reached")
        chunk = self.source.read(min(size if size >= 0 else MIB, MIB))
        self.count += len(chunk)
        require(self.count <= self.size, "asset exceeded its published byte length")
        self.sha.update(chunk)
        return chunk


def fetch_assets(work, evidence, reserve, *, download_node=True):
    deadline = time.monotonic() + 240
    runtime = work / "wasmer-7.5.0"
    runtime.mkdir()
    assets = [
        ("wasmer", WASMER_URL, WASMER_BYTES, WASMER_SHA256),
    ]
    if download_node:
        assets.append(("node", NODE_URL, NODE_BYTES, NODE_SHA256))
    for name, url, size, expected in assets:
        report = {"url": url, "expected_bytes": size, "expected_sha256": expected, "started_at": now(), "verified": False}
        reader = None
        try:
            with urllib.request.urlopen(url, timeout=30) as response:
                reader = DownloadReader(response, size, deadline, work, reserve)
                if name == "wasmer":
                    extracted = {}
                    with tarfile.open(fileobj=reader, mode="r|gz") as archive:
                        for member in archive:
                            if member.name not in SELECTED_MEMBERS:
                                continue
                            require(member.isreg() and member.size == SELECTED_MEMBERS[member.name], "unexpected selected archive member")
                            require(member.name not in extracted, "duplicate selected archive member")
                            target = runtime / member.name
                            target.parent.mkdir(parents=True, exist_ok=True)
                            source = archive.extractfile(member)
                            require(source is not None, "archive member has no readable body")
                            with target.open("xb") as output:
                                shutil.copyfileobj(source, output, MIB)
                            require(target.stat().st_size == member.size, "extracted member byte count differs")
                            target.chmod(0o600)
                            extracted[member.name] = {"bytes": member.size, "sha256": digest(target)}
                    report["extracted"] = extracted
                    require(set(extracted) == set(SELECTED_MEMBERS), "archive is missing a selected member")
                else:
                    with (work / "node.wasm").open("xb") as output:
                        while chunk := reader.read(MIB):
                            output.write(chunk)
                while reader.read(MIB):
                    pass
                require(reader.count == size and reader.sha.hexdigest() == expected, "published asset size or SHA256 mismatch")
                report["verified"] = True
        finally:
            if reader is not None:
                report.update(actual_bytes=reader.count, actual_sha256=reader.sha.hexdigest())
            report["finished_at"] = now()
            write_json(evidence / (name + "-download.json"), report)
    executable = runtime / "bin/wasmer"
    require(digest(executable) == EXECUTABLE_SHA256, "extracted Wasmer executable SHA256 mismatch")
    if download_node:
        require(digest(work / "node.wasm") == NODE_SHA256, "saved Node module SHA256 mismatch")
    executable.chmod(0o700)
    return executable, work / "node.wasm"


def install_child_limits(budget, deny_network=True):
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    cpu = budget["cpu_soft_seconds"]
    resource.setrlimit(resource.RLIMIT_CPU, (cpu, cpu + 5))
    address_space = budget["address_space_hard_bytes"]
    resource.setrlimit(resource.RLIMIT_AS, (address_space, address_space))
    file_size = budget["file_hard_bytes"]
    resource.setrlimit(resource.RLIMIT_FSIZE, (file_size, file_size))
    resource.setrlimit(resource.RLIMIT_NOFILE, (256, 256))
    if not deny_network:
        return
    seccomp = ctypes.CDLL("libseccomp.so.2", use_errno=True)
    seccomp.seccomp_init.argtypes = [ctypes.c_uint32]
    seccomp.seccomp_init.restype = ctypes.c_void_p
    seccomp.seccomp_syscall_resolve_name.argtypes = [ctypes.c_char_p]
    seccomp.seccomp_syscall_resolve_name.restype = ctypes.c_int
    seccomp.seccomp_rule_add.argtypes = [ctypes.c_void_p, ctypes.c_uint32, ctypes.c_int, ctypes.c_uint]
    seccomp.seccomp_rule_add.restype = ctypes.c_int
    seccomp.seccomp_load.argtypes = [ctypes.c_void_p]
    seccomp.seccomp_load.restype = ctypes.c_int
    seccomp.seccomp_release.argtypes = [ctypes.c_void_p]
    ctx = seccomp.seccomp_init(0x7FFF0000)
    require(bool(ctx), "seccomp_init failed")
    try:
        for name in DENIED_NETWORK:
            syscall = seccomp.seccomp_syscall_resolve_name(name.encode())
            require(syscall >= 0 and seccomp.seccomp_rule_add(ctx, 0x00050000 | errno.EPERM, syscall, 0) == 0, "cannot deny network syscall " + name)
        require(seccomp.seccomp_load(ctx) == 0, "seccomp_load failed")
    finally:
        seccomp.seccomp_release(ctx)


def group_resources(pgid):
    members = []
    for path in Path("/proc").iterdir():
        if not path.name.isdigit():
            continue
        try:
            fields = (path / "stat").read_text().rsplit(")", 1)[1].split()
            if int(fields[2]) != pgid:
                continue
            status = {}
            for line in (path / "status").read_text().splitlines():
                name, _, value = line.partition(":")
                if name in ("VmRSS", "VmSize", "Threads"):
                    status[name] = int(value.split()[0])
            members.append({"pid": int(path.name), **status})
        except (FileNotFoundError, ProcessLookupError):
            continue
    return members


def stop_owned(process, reason):
    actions = []
    errors = []
    remaining = None
    deadline = time.monotonic() + 7
    for sig, seconds in ((signal.SIGTERM, 2), (signal.SIGKILL, 3)):
        try:
            os.killpg(process.pid, sig)
            actions.append(sig.name)
        except ProcessLookupError:
            pass
        except OSError as error:
            errors.append({"operation": sig.name, "type": type(error).__name__, "message": str(error)})
        try:
            process.wait(timeout=seconds)
        except subprocess.TimeoutExpired:
            pass
        try:
            remaining = group_resources(process.pid)
        except OSError as error:
            remaining = None
            errors.append({"operation": "inspect_group", "type": type(error).__name__, "message": str(error)})
        if remaining == [] and process.poll() is not None:
            break
    while remaining != [] and time.monotonic() < deadline:
        time.sleep(0.05)
        try:
            remaining = group_resources(process.pid)
        except OSError as error:
            remaining = None
            errors.append({"operation": "inspect_group", "type": type(error).__name__, "message": str(error)})
    return {"reason": reason, "actions": actions, "leader_reaped": process.poll() is not None, "remaining_group": remaining, "errors": errors}


def run_owned(label, command, input_bytes, work, evidence, resources, budget, wall=WALL_SECONDS,
              *, cwd=None, environment=None, deny_network=True, sample_seconds=1):
    environment = dict(environment) if environment is not None else {
        "PATH": "/usr/bin:/bin", "LANG": "C", "WASMER_DIR": str(work / "runtime-home"),
        "WASMER_CACHE_DIR": str(work / "cache"), "TMPDIR": str(work / "tmp"),
        "RAYON_NUM_THREADS": "1", "TOKIO_WORKER_THREADS": "1",
    }
    report = {
        "label": label, "command": command, "environment": environment,
        "cwd": str(cwd or work / "guest"),
        "denied_network_syscalls": DENIED_NETWORK if deny_network else [],
        "network_policy": "denied by seccomp" if deny_network else "allowed for the recorded public source fetch only",
        "limits": {**budget, "wall_seconds": wall, "sample_seconds": sample_seconds},
        "started_at": now(), "resources_before": resources.snapshot(),
        "rss_peak_bytes_observed": 0, "samples": [], "spawn_requested": False, "owned_pid": None,
        "exit_code": None, "leader_reaped": False, "group_remaining": None,
        "stopped": None, "cleanup_attempts": [],
    }
    observation_path = evidence / (label + ".observation.json")
    input_path = evidence / (label + ".stdin")
    input_path.write_bytes(input_bytes)
    write_json(observation_path, report)
    process = None
    started = time.monotonic()
    try:
        before = report["resources_before"]
        require(STOP_SIGNAL is None, "runtime start interrupted by signal")
        require(before["error"] is None, before["error"])
        require(before["effective_headroom_bytes"] >= budget["rss_monitor_bytes"] + budget["available_memory_reserve_bytes"], "runtime headroom changed before admission")
        require(before["disk_free_bytes"] > budget["free_disk_reserve_bytes"], "runtime disk reserve reached")
        with input_path.open("rb") as source, (evidence / (label + ".stdout")).open("xb") as out, (evidence / (label + ".stderr")).open("xb") as err:
            report["spawn_requested"] = True
            write_json(observation_path, report)
            process = subprocess.Popen(command, cwd=cwd or work / "guest", env=environment, stdin=source, stdout=out, stderr=err, close_fds=True, start_new_session=True, preexec_fn=lambda: install_child_limits(budget, deny_network))
            report["owned_pid"] = process.pid
            write_json(observation_path, report)
            last_sample = -1
            while process.poll() is None:
                elapsed = time.monotonic() - started
                members = group_resources(process.pid)
                rss = sum(member.get("VmRSS", 0) * 1024 for member in members)
                observed = resources.snapshot()
                report["rss_peak_bytes_observed"] = max(report["rss_peak_bytes_observed"], rss)
                if int(elapsed / sample_seconds) != last_sample:
                    report["samples"].append({"seconds": round(elapsed, 3), "rss_bytes": rss, "members": members, "resources": observed})
                    last_sample = int(elapsed / sample_seconds)
                    write_json(observation_path, report)
                reason = None
                if STOP_SIGNAL is not None:
                    reason = "signal_" + signal.Signals(STOP_SIGNAL).name
                elif elapsed > wall:
                    reason = "wall_deadline"
                elif observed["error"] is not None:
                    reason = "resource_observation_unavailable"
                elif rss > budget["rss_monitor_bytes"]:
                    reason = "rss_observation_limit"
                elif observed["effective_headroom_bytes"] < budget["available_memory_reserve_bytes"]:
                    reason = "observed_memory_reserve"
                elif observed["disk_free_bytes"] < budget["free_disk_reserve_bytes"]:
                    reason = "free_disk_reserve"
                elif budget.get("output_monitor_bytes") is not None and out.tell() + err.tell() > budget["output_monitor_bytes"]:
                    reason = "output_observation_limit"
                if reason:
                    report["stopped"] = stop_owned(process, reason)
                    report["cleanup_attempts"].append(report["stopped"])
                    break
                time.sleep(budget["monitor_interval_seconds"])
    except BaseException as error:
        report["execution_error"] = {"type": type(error).__name__, "message": str(error)}
        raise
    finally:
        try:
            if process is not None:
                try:
                    members = group_resources(process.pid)
                except OSError as error:
                    members = None
                    report["cleanup_inspection_error"] = {"type": type(error).__name__, "message": str(error)}
                if process.poll() is None or members != []:
                    cleanup = stop_owned(process, "finally_cleanup")
                    report["cleanup_attempts"].append(cleanup)
                    if report["stopped"] is None:
                        report["stopped"] = cleanup
                    members = cleanup["remaining_group"]
                report["exit_code"] = process.poll()
                report["group_remaining"] = members
                report["leader_reaped"] = process.poll() is not None
        except BaseException as error:
            report["cleanup_error"] = {"type": type(error).__name__, "message": str(error)}
        finally:
            report["elapsed_seconds"] = time.monotonic() - started
            report["finished_at"] = now()
            try:
                report["resources_after"] = resources.snapshot()
            except BaseException as error:
                report["resource_observation_error"] = {"type": type(error).__name__, "message": str(error)}
            finally:
                write_json(observation_path, report)
    require(report["leader_reaped"] and report["group_remaining"] == [], "owned runtime group failed to stop and join")
    return report


def capture_guest(work, evidence):
    destination = evidence / "guest"
    destination.mkdir(exist_ok=True)
    copied = 0
    count = 0
    for source in sorted((work / "guest").rglob("*")):
        metadata = source.lstat()
        relative = source.relative_to(work / "guest")
        target = destination / relative
        if stat.S_ISDIR(metadata.st_mode):
            target.mkdir(exist_ok=True)
            continue
        require(stat.S_ISREG(metadata.st_mode), "guest evidence contains a non-regular file: " + str(relative))
        count += 1
        copied += metadata.st_size
        require(count <= 256 and copied <= 64 * MIB, "guest evidence exceeds its retained file/byte budget")
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)


def groups_settled(evidence):
    for path in evidence.glob("*.observation.json"):
        observation = read_json(path)
        if (observation.get("spawn_requested") or observation.get("owned_pid") is not None) and (
            observation.get("leader_reaped") is not True or observation.get("group_remaining") != []
        ):
            return False
    return True


def read_json(path):
    return json.loads(path.read_text()) if path.is_file() else {}


def output_values(path):
    values = []
    if path.is_file():
        for line in path.read_text().splitlines():
            try:
                value = json.loads(line)
                if isinstance(value, dict):
                    values.append(value)
            except json.JSONDecodeError:
                continue
    return values


def run_journey(executable, module, work, evidence, resources, budget, summary):
    for label, option in (("runtime-version", "--version"), ("runtime-help", "run")):
        command = [str(executable), option] + (["--help"] if label == "runtime-help" else [])
        result = run_owned(label, command, b"", work, evidence, resources, budget, wall=15)
        require(result["exit_code"] == 0 and result["stopped"] is None, label + " failed")
    require((evidence / "runtime-version.stdout").read_text().strip() == "wasmer 7.5.0", "actual Wasmer version differs")
    command = [str(executable), "run", "--wasmer-dir", str(work / "runtime-home"), "--cache-dir", str(work / "cache"), "--cranelift", "--compiler-threads", "1", "--disable-cache", "--offline", "--no-tty", "--volume", str(work / "guest") + ":/workspace", "--cwd", "/workspace", str(module), "--", "/workspace/js_cell_driver.mjs"]
    first = run_owned("first", command, frames([(1, FIRST_SOURCE), (2, SECOND_SOURCE)]), work, evidence, resources, budget)
    checks = summary["checks"]
    checks["first_exit_zero"] = first["exit_code"] == 0 and first["stopped"] is None
    checks["owned_group_stopped"] = first["leader_reaped"] and not first["group_remaining"]
    guest = work / "guest"
    if checks["first_exit_zero"]:
        for seq in (1, 2):
            report = read_json(guest / "cells" / (str(seq) + ".report.json"))
            checks["cell_" + str(seq) + "_report_ok"] = report.get("ok") is True and report.get("state_verified") is True
        checks["checkpoint_1518"] = read_json(guest / "state.json").get("probeValue") == 1518
        values = output_values(guest / "cells/1.out")
        runtime = next((value for value in values if value.get("phase") == "node-wasix-runtime"), {})
        versions = runtime.get("versions", {})
        checks["observed_real_node_versions"] = runtime.get("release") == "node" and isinstance(versions.get("node"), str) and isinstance(versions.get("v8"), str) and bool(versions.get("v8")) and runtime.get("version") == "v" + versions.get("node", "")
        checks["first_cell_output"] = any(value.get("phase") == "first-cell" and value.get("value") == 1517 and value.get("fs_readback") is True for value in values)
    capture_guest(work, evidence)
    write_json(evidence / "first-checks.json", checks)
    if not checks or not all(checks.values()):
        return
    # Preserve the verified first checkpoint before the conditional reopen.
    shutil.copyfile(guest / "state.json", evidence / "checkpoint-before-reopen.json")
    summary["reopen_attempted"] = True
    reopened = run_owned("reopened", command, frames([(3, REOPEN_SOURCE)]), work, evidence, resources, budget)
    report = read_json(guest / "cells/3.report.json")
    checks["reopened_exit_zero"] = reopened["exit_code"] == 0 and reopened["stopped"] is None
    checks["reopened_group_stopped"] = reopened["leader_reaped"] and not reopened["group_remaining"]
    checks["reopened_cell_ok"] = report.get("ok") is True and report.get("state_verified") is True
    checks["reopened_checkpoint_1519"] = read_json(guest / "state.json").get("probeValue") == 1519


def handle_signal(number, _frame):
    global STOP_SIGNAL
    STOP_SIGNAL = number


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--evidence-dir", required=True, type=Path)
    parser.add_argument("--work-dir", required=True, type=Path)
    args = parser.parse_args()
    evidence, work = args.evidence_dir.resolve(), args.work_dir.resolve()
    require(evidence != work and not evidence.is_relative_to(work) and not work.is_relative_to(evidence), "work and evidence directories must be separate")
    evidence.mkdir(parents=True, mode=0o700)
    os.umask(0o077)
    summary = {"started_at": now(), "source_revision_requested": args.source_revision, "harness_sha256": digest(Path(__file__)), "checks": {}, "reopen_attempted": False, "feasibility_passed": False, "product_capability_installed": False, "named_target_acceptance": False}
    lock = None
    owned_work = False
    try:
        require(sys.platform == "linux" and platform.machine() == "x86_64", "this pinned runtime requires Linux x86_64")
        for number in (signal.SIGTERM, signal.SIGINT):
            signal.signal(number, handle_signal)
        work.mkdir(parents=True, mode=0o700)
        owned_work = True
        for directory in ("guest", "runtime-home", "cache", "tmp"):
            (work / directory).mkdir(mode=0o700)
        lock = open("/tmp/shenko-cargo.lock", "a")
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        repository = Path(__file__).resolve().parent.parent
        head = subprocess.run(["git", "-C", str(repository), "rev-parse", "HEAD"], check=True, capture_output=True, text=True, timeout=5).stdout.strip()
        summary["source_revision_observed"] = head
        require(re.fullmatch(r"[0-9a-f]{40}", args.source_revision) and head == args.source_revision, "requested source revision does not match this checkout")
        summary["driver_sha256_observed"] = digest(repository / DRIVER_PATH)
        require(summary["driver_sha256_observed"] == DRIVER_SHA256, "original driver changed; this fixed-input experiment requires explicit revision")
        first_input = frames([(1, FIRST_SOURCE), (2, SECOND_SOURCE)])
        require(hashlib.sha256(first_input).hexdigest() == FIRST_INPUT_SHA256, "original framed input changed")
        (evidence / "planned-first.stdin").write_bytes(first_input)
        (evidence / "planned-reopened.stdin").write_bytes(frames([(3, REOPEN_SOURCE)]))
        shutil.copyfile(Path(__file__), evidence / "runner.py")
        shutil.copyfile(repository / DRIVER_PATH, work / "guest/js_cell_driver.mjs")
        for directory in ("env", "cells"):
            (work / "guest" / directory).mkdir()
        (work / "guest/env/package.json").write_text('{"name":"node-wasix-probe","private":true}\n')
        write_json(evidence / "manifest.json", {
            "source_revision": head, "harness_sha256": digest(Path(__file__)), "driver_path": DRIVER_PATH, "driver_sha256": DRIVER_SHA256,
            "first_input_sha256": FIRST_INPUT_SHA256, "platform": platform.platform(),
            "wasmer": {"version": "7.5.0", "source_commit": "82ff099e082da586b38af79986abbb8b677f8baf", "url": WASMER_URL, "archive_bytes": WASMER_BYTES, "archive_sha256": WASMER_SHA256, "executable_sha256": EXECUTABLE_SHA256},
            "node": {"url": NODE_URL, "bytes": NODE_BYTES, "sha256": NODE_SHA256, "release_tag_commit": "0d66fea37737024056983f35e84af3e8448ece08", "source_build_attestation": None, "tag_tracked_binary_differs": True},
            "product_capability_installed": False, "named_target_acceptance": False,
            "resource_policy": "observe actual system and visible ancestors before download and execution; no claim about unexposed host ancestors",
            "inherited_process_limits": Path("/proc/self/limits").read_text(),
        })
        resources = VisibleResources(work)
        before = resources.snapshot()
        write_json(evidence / "resources-before-download.json", before)
        preliminary = choose_budget(before)
        required_disk = sum(SELECTED_MEMBERS.values()) + NODE_BYTES + preliminary["free_disk_reserve_bytes"]
        require(before["disk_free_bytes"] > required_disk, "insufficient observed disk for selective extraction and reserve")
        executable, module = fetch_assets(work, evidence, preliminary["free_disk_reserve_bytes"])
        before_guest = resources.snapshot()
        write_json(evidence / "resources-before-runtime.json", before_guest)
        budget = choose_budget(before_guest)
        write_json(evidence / "resource-budget.json", budget)
        run_journey(executable, module, work, evidence, resources, budget, summary)
    except BaseException as error:
        # Redirect signatures and ambient environment are never written here.
        message = "HTTP status " + str(error.code) if isinstance(error, urllib.error.HTTPError) else str(error)
        summary["error"] = {"type": type(error).__name__, "message": message}
        (evidence / "failure.txt").write_text("".join(traceback.format_exception_only(type(error), error)) if not isinstance(error, urllib.error.HTTPError) else message + "\n")
    finally:
        try:
            summary["owned_groups_settled"] = groups_settled(evidence)
            require(summary["owned_groups_settled"], "an owned process outcome remains unconfirmed; guest files are not a stable snapshot")
            if owned_work and (work / "guest").is_dir():
                capture_guest(work, evidence)
        except BaseException as error:
            summary["evidence_capture_error"] = {"type": type(error).__name__, "message": str(error)}
        summary["compiled_cache_files"] = [str(path.relative_to(work)) for path in (work / "cache").rglob("*") if path.is_file()] if owned_work else []
        summary["stop_signal"] = STOP_SIGNAL
        summary["finished_at"] = now()
        summary["feasibility_passed"] = bool(summary["checks"]) and all(summary["checks"].values()) and summary["reopen_attempted"] and not any(key in summary for key in ("error", "evidence_capture_error")) and not summary["compiled_cache_files"] and STOP_SIGNAL is None
        write_json(evidence / "summary.json", summary)
        entries = [{"path": str(path.relative_to(evidence)), "bytes": path.stat().st_size, "sha256": digest(path)} for path in sorted(evidence.rglob("*")) if path.is_file() and path.name != "files.json"]
        write_json(evidence / "files.json", entries)
        if lock is not None:
            lock.close()
    print(json.dumps(summary), flush=True)
    return 0 if summary["feasibility_passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
