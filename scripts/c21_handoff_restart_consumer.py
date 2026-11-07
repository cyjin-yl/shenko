#!/usr/bin/env python3
"""Independent-process consumer for an interrupted update hand-over (C21).

This drives the *product binary* across two real processes and one killed
helper. It does not build anything, does not mock Store, does not touch QQ or
the network, and supplies only a scripted loopback model transport so the
subject loop can run without a real model.

Scenario:

1. start runtime P1 in a fresh installation;
2. ``update prepare --artifact <binary> --generation gen-restart-b`` (real
   copy, real SQLite backup, real typed delta import, real smoke launch);
3. ``update activate ...`` through the live control endpoint: P1 freezes the
   request, pauses the subject, quiesces its participants, spawns the finite
   helper and acknowledges ``awaiting_old_process_retirement``;
4. kill that helper before it can acquire the installation owner — the
   midpoint where a real hand-over dies with a paused subject, an open
   quiesce request and an open activation record;
5. restart runtime P2 in the same installation and observe the cold-start
   reconciliation: the *same* controller generation takes its boundary back
   (no new epoch, no generation switch, no second primary, no replayed
   effect), and the abandoned activation keeps its identity and receipts.

Nothing here is a power-loss, real-model, QQ, platform or performance claim.
The scripted provider is a transport fixture; it says nothing about model
behavior.

Usage:
    python3 scripts/c21_handoff_restart_consumer.py \
        --binary /abs/path/to/shenko \
        --root /tank/shared/projects/shenko/build/c21-handoff-restart-run1

Exit codes: 0 all assertions held; 1 an assertion failed (evidence retained);
2 the midpoint was missed (the helper won the race and committed a selection,
which is a real outcome of this scenario, not a pass); 3 setup error.
"""
import argparse
import hashlib
import json
import os
import signal
import sqlite3
import subprocess
import sys
import threading
import time
import traceback
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import quote

MODEL = "c21-restart-scripted"
REPLY = "A bounded synthetic continuation."
SLOT_DB = "shenko.db"
CURRENT_SLOT = "current-slot"
CONTROL = "update-control.json"


def sha256(raw):
    return hashlib.sha256(raw).hexdigest()


def file_sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def put_new(path, raw):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as stream:
        stream.write(raw)
        stream.flush()
        os.fsync(stream.fileno())


class Journal:
    def __init__(self, path):
        self.path = path
        self.lock = threading.Lock()
        self.pending = []

    def emit(self, event, **fields):
        record = {"event": event, "at": round(time.time(), 6), **fields}
        line = (json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n").encode()
        with self.lock:
            self.pending.append(record)
        fd = os.open(self.path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        try:
            os.write(fd, line)
        finally:
            os.close(fd)


class Assertions:
    def __init__(self, journal):
        self.journal = journal
        self.rows = []

    def require(self, condition, name, **detail):
        passed = bool(condition)
        self.rows.append({"assertion": name, "passed": passed, "detail": detail})
        self.journal.emit("assertion", name=name, passed=passed, **detail)
        if not passed:
            raise AssertionError(name)
        return True


def provider_handler(reply):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):
            return

        def send_json(self, payload):
            raw = json.dumps(payload, ensure_ascii=False).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def do_GET(self):
            if self.client_address[0] != "127.0.0.1" or self.path != "/v1/models":
                self.send_error(404)
                return
            self.send_json({"object": "list", "data": [{"id": MODEL, "object": "model"}]})

        def do_POST(self):
            if self.client_address[0] != "127.0.0.1" or self.path != "/v1/chat/completions":
                self.send_error(404)
                return
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length) if length else b""
            self.send_json({
                "id": "c21-restart-scripted",
                "object": "chat.completion",
                "created": 0,
                "model": MODEL,
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": reply},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            })

    return Handler


def live_db(data):
    pointer = data / CURRENT_SLOT
    if pointer.is_file():
        try:
            slot = json.loads(pointer.read_text())["slot_dir"]
        except (ValueError, KeyError):
            return data / SLOT_DB
        return Path(slot) / SLOT_DB
    return data / SLOT_DB


def read_state(data):
    """Read the durable control state through a read-only SQLite snapshot."""
    db = live_db(data)
    if not db.is_file():
        return None
    # Read-only, one transaction: a torn read across tables would fabricate a
    # state transition that never committed.
    uri = "file:" + quote(str(db), safe="/") + "?mode=ro"
    try:
        connection = sqlite3.connect(uri, uri=True, timeout=5.0, isolation_level=None)
    except sqlite3.Error:
        return None
    try:
        connection.execute("PRAGMA query_only=ON")
        connection.execute("BEGIN")

        def query(sql):
            # A table that this installation has never used yet does not exist;
            # "no rows" is the honest reading, not a failed snapshot.
            try:
                return connection.execute(sql).fetchall()
            except sqlite3.Error:
                return []

        state = {"db": str(db), "sha256": file_sha256(db)}
        state["selectors"] = [json.loads(value) for (value,) in query("SELECT data FROM activation_selectors")]
        state["heads"] = [json.loads(value) for (value,) in query("SELECT data FROM subject_branch_heads")]
        state["quiesce"] = [json.loads(value) for (value,) in query("SELECT data FROM subject_quiesce_requests")]
        row = query("SELECT activation_id, phase, record_revision, data FROM activation_records")
        state["activations"] = [
            {"activation_id": activation_id, "phase": phase,
             "record_revision": revision, "data": json.loads(value)}
            for activation_id, phase, revision, value in row
        ]
        row = query("SELECT coalesce(max(seq), 0) FROM commit_log")
        state["max_commit_seq"] = row[0][0] if row else 0
        return state
    except sqlite3.Error:
        return None
    finally:
        connection.close()


def selector_of(state):
    selectors = state.get("selectors") or []
    return selectors[0] if selectors else None


def head_of(state):
    heads = state.get("heads") or []
    return heads[0] if heads else None


def open_quiesce(state):
    return [row for row in state.get("quiesce") or []
            if row.get("state") in ("requested", "paused")]


def describe(state):
    if state is None:
        return None
    selector = selector_of(state) or {}
    head = head_of(state) or {}
    return {
        "db": state.get("db"),
        "selector_revision": selector.get("selector_revision"),
        "selector_generation": selector.get("generation"),
        "selector_epoch": selector.get("active_epoch"),
        "head_mode": head.get("mode"),
        "head_generation": head.get("software_generation"),
        "head_epoch": head.get("controller_epoch"),
        "head_next_step_id": head.get("next_step_id"),
        "head_revision": head.get("record_revision"),
        "open_quiesce": [(row.get("request_id"), row.get("state")) for row in open_quiesce(state)],
        "activations": [(row["activation_id"], row["phase"]) for row in state.get("activations") or []],
        "max_commit_seq": state.get("max_commit_seq"),
    }


def process_helpers(data, parent_pid, binary):
    """Owned, verified helper processes of the retired runtime."""
    wanted = str(data)
    real_binary = os.path.realpath(binary)
    found = []
    proc = Path("/proc")
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        try:
            raw = (entry / "cmdline").read_bytes()
            stat = (entry / "stat").read_text()
            exe = os.path.realpath(os.readlink(entry / "exe"))
        except OSError:
            continue
        argv = [part.decode(errors="replace") for part in raw.split(b"\0") if part]
        if "complete-update" not in argv or wanted not in argv:
            continue
        # stat field 4 is the parent pid; the command name may contain spaces,
        # so split after the closing parenthesis.
        tail = stat[stat.rfind(")") + 2:].split()
        ppid = int(tail[1]) if len(tail) > 1 else -1
        found.append({"pid": pid, "ppid": ppid, "argv": argv,
                      "exe": exe, "exe_sha256": file_sha256(exe) if os.access(exe, os.R_OK) else None,
                      "same_binary": exe == real_binary, "child_of_runtime": ppid == parent_pid})
    return found


class Consumer:
    def __init__(self, binary, root, deadline):
        self.binary = binary
        self.root = root
        self.deadline = deadline
        self.journal = Journal(root / "events.jsonl")
        self.asserts = Assertions(self.journal)
        self.data = root / "data"
        self.home = root / "private-home"
        self.home.mkdir(mode=0o700)
        self.processes = []
        self.server = None
        self.provider_url = None
        # A whitelist, not the caller's environment: no credentials, proxy,
        # QQ or remote discovery configuration reaches these processes.
        self.env = {
            "HOME": str(self.home),
            "PATH": "/usr/bin:/bin",
            "LANG": "C.UTF-8",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_TERMINAL_PROMPT": "0",
            "NO_PROXY": "127.0.0.1,localhost",
            "RUST_BACKTRACE": "0",
            "SHENKO_LOG": "debug",
        }

    def remaining(self):
        return max(0.0, self.deadline - time.monotonic())

    def start_provider(self):
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), provider_handler(REPLY))
        self.server.daemon_threads = True
        address, port = self.server.server_address
        self.asserts.require(address == "127.0.0.1", "provider_is_loopback", address=address)
        self.provider_url = f"http://127.0.0.1:{port}"
        self.env.update({
            "SHENKO_USE_LOCAL_MODEL": "1",
            "SHENKO_LOCAL_MODEL": MODEL,
            "SHENKO_LOCAL_MODEL_URL": self.provider_url,
        })
        threading.Thread(target=self.server.serve_forever,
                         kwargs={"poll_interval": 0.1}, daemon=True).start()
        self.journal.emit("provider_ready", url=self.provider_url, model=MODEL,
                          scope="scripted loopback transport, not a real model")

    def spawn_runtime(self, label):
        stdout = open(self.root / f"{label}.stdout", "xb", buffering=0)
        stderr = open(self.root / f"{label}.stderr", "xb", buffering=0)
        argv = [str(self.binary), "--data-dir", str(self.data), "start", "--port", "0"]
        process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=stdout,
                                   stderr=stderr, env=self.env, close_fds=True,
                                   start_new_session=True)
        self.processes.append({"label": label, "process": process, "argv": argv})
        self.journal.emit("runtime_started", label=label, pid=process.pid, argv=argv)
        return process

    def cli(self, label, *arguments, timeout=None):
        argv = [str(self.binary), "--data-dir", str(self.data), *arguments]
        limit = min(timeout or 120.0, max(1.0, self.remaining()))
        self.journal.emit("cli_started", label=label, argv=argv, timeout=limit)
        process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, env=self.env, close_fds=True,
                                   start_new_session=True)
        try:
            stdout, stderr = process.communicate(timeout=limit)
            code = process.returncode
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            stdout, stderr = process.communicate(timeout=10)
            code = None
        put_new(self.root / f"{label}.stdout", stdout)
        put_new(self.root / f"{label}.stderr", stderr)
        self.journal.emit("cli_finished", label=label, exit_code=code,
                          stdout_sha256=sha256(stdout), stderr_sha256=sha256(stderr))
        return code, stdout, stderr

    def cli_json(self, label, *arguments, timeout=None):
        code, stdout, stderr = self.cli(label, *arguments, timeout=timeout)
        if code != 0:
            raise RuntimeError(f"{label} exited {code}: {stderr.decode(errors='replace')[-2000:]}")
        return json.loads(stdout)

    def wait_for(self, predicate, label, timeout, interval=0.1):
        limit = time.monotonic() + min(timeout, max(0.1, self.remaining()))
        last = None
        while time.monotonic() < limit:
            last = predicate()
            if last:
                self.journal.emit("waited", label=label, satisfied=True)
                return last
            time.sleep(interval)
        self.journal.emit("waited", label=label, satisfied=False, observed=repr(last)[:2000])
        return None

    def stop(self, item, sig=signal.SIGTERM):
        process = item["process"]
        if process.poll() is not None:
            return process.returncode
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=10)
        self.journal.emit("runtime_stopped", label=item["label"], exit_code=process.returncode,
                          signal=sig)
        return process.returncode


def run(binary, root, deadline):
    root.mkdir(mode=0o700, parents=False, exist_ok=False)
    consumer = Consumer(binary, root, deadline)
    consumer.data.mkdir(mode=0o700)
    consumer.journal.emit("binary", path=str(binary), sha256=file_sha256(binary))
    consumer.start_provider()

    # 1. A real installation with a real genesis selector.
    p1 = consumer.spawn_runtime("p1")
    state = consumer.wait_for(
        lambda: (lambda s: s if s and selector_of(s) and head_of(s) else None)(read_state(consumer.data)),
        "p1_genesis_ready", 120)
    if not state:
        raise RuntimeError("the first runtime never published a genesis selector/head")
    before = describe(state)
    consumer.asserts.require(before["head_mode"] == "active", "p1_subject_active", head_mode=before["head_mode"])
    consumer.asserts.require(consumer.wait_for(lambda: (consumer.data / CONTROL).is_file(), "control_ready", 60),
                            "live_control_published")

    # 2. Prepare a real candidate from the same binary under a new generation.
    prepared = consumer.cli_json(
        "prepare", "update", "prepare", "--generation", "gen-restart-b",
        "--artifact", str(binary), timeout=240)
    activation_id = prepared["activation_id"]
    consumer.journal.emit("prepared", activation=activation_id, record=prepared)
    consumer.asserts.require(prepared["to_generation"] == "gen-restart-b",
                             "prepare_targets_new_generation", to_generation=prepared["to_generation"])

    # 3. Ask the live runtime to hand over. It acknowledges only retirement.
    revoked = consumer.cli_json(
        "activate", "update", "activate", "--activation", activation_id,
        "--expected-selector-revision", str(before["selector_revision"]), timeout=180)

    # 4. The midpoint: kill the finite helper before it owns the installation.
    helpers = []
    def helper_ready():
        nonlocal helpers
        helpers = [item for item in process_helpers(consumer.data, p1.pid, binary)
                   if item["same_binary"] and item["child_of_runtime"]]
        return helpers
    consumer.wait_for(helper_ready, "helper_spawned", 5, interval=0.02)
    consumer.asserts.require(bool(helpers), "handoff_helper_observed",
                             acknowledged=revoked, helpers=helpers)
    helper = helpers[0]
    os.kill(helper["pid"], signal.SIGKILL)
    consumer.journal.emit("helper_killed", pid=helper["pid"], argv=helper["argv"],
                          exe_sha256=helper["exe_sha256"])
    consumer.wait_for(lambda: not (Path("/proc") / str(helper["pid"])).exists(),
                      "helper_gone", 30)
    consumer.asserts.require(
        revoked.get("state") == "awaiting_old_process_retirement"
        and revoked.get("activation_succeeded") is False,
        "activation_acknowledged_retirement_only", acknowledgement=revoked)
    consumer.stop(p1)
    consumer.asserts.require(p1.poll() is not None, "old_runtime_retired", pid=p1.pid)

    # 5. The midpoint state: paused subject, open boundary, open activation.
    midpoint_raw = consumer.wait_for(
        lambda: (lambda s: s if s and (head_of(s) or {}).get("mode") == "paused" else None)(read_state(consumer.data)),
        "midpoint_paused", 60)
    consumer.asserts.require(bool(midpoint_raw), "subject_paused_at_midpoint")
    midpoint = describe(midpoint_raw)
    if midpoint["selector_revision"] != before["selector_revision"]:
        consumer.journal.emit("midpoint_missed", before=before, midpoint=midpoint,
                             reason="the helper won the ownership race and committed a selection")
        return 2
    consumer.asserts.require(midpoint["selector_revision"] == before["selector_revision"],
                             "selector_unchanged_at_midpoint",
                             before=before["selector_revision"], midpoint=midpoint["selector_revision"])
    consumer.asserts.require(midpoint["head_generation"] == before["head_generation"],
                             "generation_unchanged_at_midpoint", generation=midpoint["head_generation"])
    consumer.asserts.require(midpoint["head_epoch"] == before["head_epoch"],
                             "epoch_unchanged_at_midpoint", epoch=midpoint["head_epoch"])
    consumer.asserts.require(bool(open_quiesce(midpoint_raw)), "boundary_record_open_at_midpoint",
                             quiesce=midpoint["open_quiesce"])
    consumer.asserts.require(
        any(phase == "handoff" for _activation, phase in midpoint["activations"]),
        "activation_open_at_midpoint", activations=midpoint["activations"])
    consumer.asserts.require((consumer.data / CONTROL).is_file(),
                             "frozen_handoff_request_files_present",
                             files=sorted(path.name for path in consumer.data.glob("update-handoff-*")))

    # 6. Cold start: the same controller generation takes the boundary back.
    p2 = consumer.spawn_runtime("p2")
    resumed_raw = consumer.wait_for(
        lambda: (lambda s: s if s and (head_of(s) or {}).get("mode") == "active"
                  and any(row.get("state") == "resumed" for row in s.get("quiesce") or [])
                  else None)(read_state(consumer.data)),
        "cold_start_reconciled", min(180, 90))
    consumer.asserts.require(bool(resumed_raw), "cold_start_resumed_the_boundary")
    resumed = describe(resumed_raw)
    consumer.asserts.require(resumed["selector_revision"] == midpoint["selector_revision"],
                             "no_generation_switch_on_resume",
                             before=midpoint["selector_revision"], after=resumed["selector_revision"])
    consumer.asserts.require(resumed["selector_generation"] == midpoint["selector_generation"],
                             "selector_generation_unchanged", generation=resumed["selector_generation"])
    consumer.asserts.require(resumed["head_generation"] == midpoint["head_generation"],
                             "head_generation_unchanged", generation=resumed["head_generation"])
    consumer.asserts.require(resumed["head_epoch"] == midpoint["head_epoch"],
                             "controller_epoch_not_advanced", epoch=resumed["head_epoch"])
    consumer.asserts.require(resumed["head_next_step_id"] == midpoint["head_next_step_id"],
                             "planned_successor_preserved", next_step=resumed["head_next_step_id"])
    consumer.asserts.require(
        any(phase == "abandoned" for _activation, phase in resumed["activations"]),
        "interrupted_activation_abandoned", activations=resumed["activations"])
    abandoned = [row for row in resumed_raw["activations"]
                 if row["phase"] == "abandoned" and row["activation_id"] == activation_id]
    consumer.asserts.require(bool(abandoned), "original_activation_identity_retained", activation=activation_id)
    consumer.asserts.require(bool(abandoned[0]["data"].get("last_error_ref")),
                             "abandonment_reason_sealed", activation=activation_id)
    consumer.asserts.require(
        not [item for item in process_helpers(consumer.data, p2.pid, binary)],
        "no_second_helper_dispatch")
    signal_lines = (consumer.root / "p2.stderr").read_text(errors="replace")
    consumer.journal.emit("p2_reconciliation_log",
                          resumed=("resumed by the same controller generation" in signal_lines))

    # 7. Facts still flow from the same generation afterwards.
    advanced = consumer.wait_for(
        lambda: (lambda s: s if s and s.get("max_commit_seq", 0) > resumed["max_commit_seq"]
                  else None)(read_state(consumer.data)),
        "same_generation_progress", 120)
    consumer.journal.emit("progress", advanced=describe(advanced) if advanced else None,
                          note="progress is observed, not required: the scripted transport may not drive a step")
    consumer.stop(p2)

    # 8. The deterministic offline recovery read agrees with the restart.
    recovered = consumer.cli_json("recover", "update", "recover", timeout=120)
    consumer.journal.emit("offline_recover", report=recovered)
    consumer.asserts.require("interrupted_handoff" in recovered,
                             "recover_reports_interrupted_handoff", report=recovered)
    offline = recovered.get("interrupted_handoff")
    if offline is not None:
        consumer.asserts.require(offline.get("state") == "nothing_to_do",
                                 "offline_recover_has_nothing_to_do", state=offline.get("state"))
    consumer.journal.emit("state_sequence", before=before, midpoint=midpoint, resumed=resumed)
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--deadline", type=float, default=600.0)
    arguments = parser.parse_args()
    binary = arguments.binary.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        print(json.dumps({"error": "binary is not an executable file", "binary": str(binary)}))
        return 3
    if arguments.root.exists():
        print(json.dumps({"error": "root already exists; use a fresh evidence directory",
                          "root": str(arguments.root)}))
        return 3
    deadline = time.monotonic() + arguments.deadline
    try:
        code = run(binary, arguments.root, deadline)
    except BaseException as error:
        receipt = {"verdict": "failed", "error": repr(error),
                   "traceback": traceback.format_exc()}
        try:
            put_new(arguments.root / "receipt.json",
                    (json.dumps(receipt, ensure_ascii=False, sort_keys=True, indent=2) + "\n").encode())
        except OSError:
            pass
        print(json.dumps({"verdict": "failed", "error": repr(error)}))
        return 1
    verdict = {0: "reconciled", 2: "midpoint_missed"}.get(code, "unknown")
    receipt = {"verdict": verdict, "exit_code": code,
               "binary": {"path": str(binary), "sha256": file_sha256(binary)}}
    put_new(arguments.root / "receipt.json",
            (json.dumps(receipt, ensure_ascii=False, sort_keys=True, indent=2) + "\n").encode())
    print(json.dumps(receipt))
    return code


if __name__ == "__main__":
    sys.exit(main())
