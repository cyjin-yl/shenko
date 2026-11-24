#!/usr/bin/env python3
"""Matched A/B rebuild and A->B generation hand-over, on the real product.

This harness builds *two independent candidates from the same source tree*
(``--source-dir``), installs the first as generation A, installs the second as
generation B while A is still the selected generation, switches A->B through
the product's real process-hand-over control endpoint, and then proves two
things from original retained bytes:

1. every generation-bound receipt names the generation it was produced for
   (build outcome, build observation, activation record, selector, slot
   manifest, route authority, launch receipt, branch head), so an A-era
   receipt cannot be read as B evidence and neither generation's surfaces name
   the other's slot or selected artifact; and
2. no stale artifact grants authority after the switch: the launcher pointer,
   the selected slot manifest, the sealed route, the running
   ``/proc/<pid>/exe`` and the cold-relaunch trail all resolve to B, the
   retired A slot keeps its own bytes and manifest generation, and nothing
   inside B's slot aliases A's slot by hard link or carries an executable
   other than B's selected artifact or an inert, digest-named retained program.

Nothing is mocked at the update layer: real Cargo builds through the product's
``CargoBuildProvider``, real SQLite backup/delta import/smoke, the real
activation CAS, the real ``RuntimeOwner`` transfer across ``exec`` and real
launch receipts. A failure retains the same bytes and exits nonzero.

What this harness does NOT prove (also written into the receipt's boundaries):

* no model behaviour -- by default the runtime talks to a bounded loopback
  "scripted transport" fixture that only keeps subject selection and messaging
  admissible; ``--provider-url`` forwards a caller-provided OpenAI-compatible
  binding instead and still makes no model claim;
* no power-loss durability, no arbitrary filesystem hostility, no multi-node
  quorum and no complete SPEC acceptance;
* by default the source checkout is *not* required to be clean; the harness
  matches the two builds with its own content fingerprint of git-tracked and
  non-ignored untracked files. ``--require-clean-source`` turns a dirty
  checkout into a failure. Ignored files, environment, compiler closure and
  network state are never frozen.

Usage::

    python3 -I scripts/root_ab_rebuild_harness.py \
        --binary /abs/path/to/shenko \
        --source-dir /abs/path/to/source-worktree \
        --root /abs/fresh/evidence/root-ab-run1

``--root`` must not exist and must not be inside ``--source-dir``: writing
evidence into the measured tree would change the matched-source fingerprint and
dirty the observed checkout. Exit codes: 0 all required checks held; 1 a check
failed or a required step could not complete (evidence retained); 3 setup error.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import signal
import sqlite3
import stat
import subprocess
import sys
import threading
import time
import traceback
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import quote

# ---------------------------------------------------------------------------
# Product constants: the on-disk names of the update contract.
# ---------------------------------------------------------------------------
SLOT_DB = "shenko.db"
SLOT_MANIFEST = "slot.json"
CURRENT_SLOT = "current-slot"
CONTROL_ENDPOINT = "update-control.json"
MUTATION_LOCK = ".shenko-installation-mutation.lock"
RECOVERY_PROGRAMS = "recovery-programs"
BUILD_OUTPUT_PARENT = ".shenko-builds"

RECEIPT_KIND = "shenko_native_case_receipt"
RECEIPT_SCHEMA = 1
BINDING_KIND = "shenko_root_ab_rebuild_binding"
BINDING_SCHEMA = 1
SCOPES = ("native", "model", "failure", "recovery")

# Installation-level entries that a recursive copy must never carry into a
# generation slot (docs/specs/self-update.md: installation-level `versions`,
# shared CAS, node database and control endpoint are excluded from recursive
# root copying; staged/handoff control files have the same character).
FORBIDDEN_SLOT_NAMES = {
    "versions", "cas", "node", CURRENT_SLOT, CONTROL_ENDPOINT, MUTATION_LOCK,
    BUILD_OUTPUT_PARENT,
}
FORBIDDEN_SLOT_PREFIXES = (".update-control-", "update-handoff-",
                           ".shenko-stage-", ".object-", ".stream-")

HEX64 = re.compile(r"^[0-9a-f]{64}$")
EXECUTABLE = stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH
SCRIPTED_MODEL = "root-ab-scripted-transport"

# The canonical admission contract requires these envelope fields; the unit
# test checks that the published JSON schema lists exactly the same required
# sets (a schema that silently omits a required field would be misleading).
REQUIRED_RECEIPT_KEYS = (
    "kind", "schema_version", "evidence_id", "candidate_commit", "status",
    "source", "artifact", "build", "configuration", "environment",
    "boundaries", "run", "cases",
)
REQUIRED_CASE_KEYS = ("case_id", "status", "observations")
REQUIRED_OBSERVATION_KEYS = ("status", "checks", "output")
REQUIRED_RUN_KEYS = ("started_unix_ms", "finished_unix_ms", "exit_code")
REQUIRED_BINDING_KEYS = (
    "kind", "schema_version", "evidence_id", "candidate_commit", "source",
    "generations", "switch", "checks", "boundaries",
)
REQUIRED_GENERATION_KEYS = (
    "generation", "build_id", "artifact", "slot", "activation", "selector",
    "launch_receipts", "route", "build_observation",
)
SURFACE_KEYS = (
    "generation", "build_artifact_generation", "build_observation_generation",
    "activation_to_generation", "slot_manifest_generation", "route_generation",
    "launch_receipt_generation", "head_generation", "selector_generation",
    "build_id", "slot_dir", "slot_artifact_path", "manifest_artifact_path",
    "route_artifact_path", "selector_revision", "selector_epoch", "slot_epoch",
    "launch_receipt_revision", "launch_receipt_epoch", "head_epoch",
)
GENERATION_NAMED_SURFACES = tuple(
    key for key in SURFACE_KEYS
    if key.endswith("_generation") and key != "generation")


class HarnessStop(Exception):
    """A prerequisite failed; stop advancing and write a failed receipt."""


# ---------------------------------------------------------------------------
# Small utilities.
# ---------------------------------------------------------------------------
def sha256_bytes(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_new(path: Path, raw: bytes, mode: int = 0o600) -> None:
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    with os.fdopen(fd, "wb") as stream:
        stream.write(raw)
        stream.flush()
        os.fsync(stream.fileno())


def durable_json(path: Path, value) -> None:
    parent = path.parent
    parent.mkdir(parents=True, exist_ok=True)
    staging = parent / (".%s.stage-%s" % (path.name, uuid.uuid4()))
    write_new(staging, (json.dumps(value, ensure_ascii=False, indent=2,
                                   sort_keys=True) + "\n").encode())
    os.replace(staging, path)
    fd = os.open(parent, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def seal_file(root: Path, relative: str, value) -> dict:
    """Write one retained original and return its {path, sha256} binding."""
    path = root / relative
    durable_json(path, value)
    return {"path": relative, "sha256": file_sha256(path)}


def binding_of(path: Path, base: Path) -> dict:
    try:
        shown = str(path.resolve().relative_to(base.resolve()))
    except ValueError:
        shown = str(path)
    return {"path": shown, "sha256": file_sha256(path)}


def is_hex64(value) -> bool:
    return isinstance(value, str) and bool(HEX64.match(value))


def inside(path, root) -> bool:
    if not path or not root:
        return False
    try:
        Path(os.path.realpath(path)).relative_to(Path(os.path.realpath(root)))
        return True
    except ValueError:
        return False


def printable(raw: bytes) -> str:
    return raw.decode("utf-8", "backslashreplace")


# ---------------------------------------------------------------------------
# Evidence journal and per-case assertion collectors.
#
# The inventory is additive: every check records its observation and pass/fail,
# and a prerequisite failure stops the run without erasing what was already
# observed. Only a receipt whose every check is true is written as passed.
# ---------------------------------------------------------------------------
class Journal:
    def __init__(self, path: Path):
        self.path = path
        self.lock = threading.Lock()

    def emit(self, event: str, **fields) -> None:
        record = {"event": event, "at": round(time.time(), 6), **fields}
        line = (json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n").encode()
        with self.lock:
            fd = os.open(self.path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
            try:
                os.write(fd, line)
            finally:
                os.close(fd)


class Case:
    def __init__(self, harness: "Harness", case_id: str, scope: str):
        if scope not in SCOPES:
            raise ValueError("unknown observation scope: %s" % scope)
        self.h = harness
        self.case_id = case_id
        self.scope = scope
        self.checks: dict[str, bool] = {}
        self.detail: dict[str, dict] = {}
        self.notes: dict = {}

    def check(self, name: str, condition, **detail) -> bool:
        passed = bool(condition)
        self.checks[name] = passed
        self.detail[name] = detail
        if not passed:
            self.h.failures.append("%s:%s" % (self.case_id, name))
            self.h.journal.emit("check_failed", case=self.case_id,
                                check=name, **detail)
        return passed

    def require(self, name: str, condition, **detail) -> None:
        if not self.check(name, condition, **detail):
            raise HarnessStop("%s:%s" % (self.case_id, name))

    def note(self, **fields) -> None:
        self.notes.update(fields)

    def completed(self) -> bool:
        return bool(self.checks) and all(self.checks.values())


# ---------------------------------------------------------------------------
# Read-only product surfaces.
# ---------------------------------------------------------------------------
def cas_object_path(cas_dir: Path, digest: str) -> Path:
    if not isinstance(digest, str) or not digest.startswith("blake3:"):
        raise ValueError("CAS digest is not canonical blake3")
    hexdigest = digest[len("blake3:"):]
    if not HEX64.match(hexdigest):
        raise ValueError("CAS digest is not canonical blake3 hex")
    return cas_dir / hexdigest[:2] / hexdigest[2:4] / hexdigest


def read_cas(cas_dir: Path, reference) -> tuple[bytes, object]:
    """Read one sealed object by its own digest-sharded path.

    The path comes from the digest the product wrote; there is no directory
    scan or same-name fallback. The retained length must agree with the ref.
    """
    if not isinstance(reference, dict):
        raise ValueError("object reference is not an object")
    digest = reference.get("digest")
    if not isinstance(digest, str):
        raise ValueError("object reference has no digest")
    path = cas_object_path(cas_dir, digest)
    raw = path.read_bytes()
    if reference.get("bytes") is not None and int(reference["bytes"]) != len(raw):
        raise ValueError("%s: retained length differs from its reference" % path)
    return raw, json.loads(raw)


def read_pointer(data_dir: Path):
    path = data_dir / CURRENT_SLOT
    if not path.is_file():
        return None
    return json.loads(path.read_text())


def read_slot_manifest(slot_dir) -> dict | None:
    if not slot_dir:
        return None
    path = Path(slot_dir) / SLOT_MANIFEST
    if not path.is_file():
        return None
    return json.loads(path.read_text())


def read_control_runtime_id(data_dir: Path):
    """Only the non-secret runtime identity of the live control endpoint.

    The endpoint file also carries the private capability. It is never copied
    into evidence, hashed or logged.
    """
    path = data_dir / CONTROL_ENDPOINT
    if not path.is_file():
        return None
    try:
        endpoint = json.loads(path.read_text())
    except (OSError, ValueError):
        return None
    return endpoint.get("runtime_id")


def read_db(db) -> dict | None:
    """One consistent, read-only snapshot of the durable control state.

    A table this installation never used does not exist; "no rows" is the
    honest reading and is not an error.
    """
    if not db:
        return None
    db = Path(db)
    if not db.is_file():
        return None
    state = {"db": str(db), "sha256": file_sha256(db)}
    try:
        uri = "file:" + quote(str(db), safe="/") + "?mode=ro"
        connection = sqlite3.connect(uri, uri=True, timeout=5.0,
                                     isolation_level=None)
        connection.execute("PRAGMA query_only=ON")
        connection.execute("BEGIN")
    except sqlite3.Error as error:
        state["error"] = "open: %s" % error
        return state
    try:
        def rows(sql):
            try:
                return connection.execute(sql).fetchall()
            except sqlite3.Error:
                return []

        state["ledger_id"] = None
        for (ledger,) in rows("SELECT ledger_id FROM ledger_identity WHERE singleton=1"):
            state["ledger_id"] = ledger
        state["selectors"] = [json.loads(value) for (value,) in
                              rows("SELECT data FROM activation_selectors")]
        state["records"] = [
            {"activation_id": activation_id, "phase": phase,
             "record_revision": revision, "data": json.loads(value)}
            for activation_id, phase, revision, value in rows(
                "SELECT activation_id, phase, record_revision, data "
                "FROM activation_records")
        ]
        state["launches"] = [
            {"launch_id": launch_id, "selector_revision": revision,
             "data": json.loads(value)}
            for launch_id, revision, value in rows(
                "SELECT launch_id, selector_revision, data "
                "FROM activation_launch_receipts")
        ]
        state["heads"] = [json.loads(value) for (value,) in
                          rows("SELECT data FROM subject_branch_heads")]
        state["quiesce"] = [json.loads(value) for (value,) in
                            rows("SELECT data FROM subject_quiesce_requests")]
        state["successors"] = [
            {"ledger_id": ledger_id, "writer_incarnation": incarnation,
             "branch_id": branch, "genesis_ledger_id": genesis_ledger,
             "genesis_id": genesis, "selector": json.loads(selector),
             "selected_commit_seq": seq}
            for ledger_id, incarnation, branch, genesis_ledger, genesis, selector, seq
            in rows("SELECT ledger_id, writer_incarnation, branch_id, "
                    "genesis_ledger_id, genesis_id, selector_data, "
                    "selected_commit_seq FROM local_successor_materializations")
        ]
        state["build_observations"] = []
        for (value,) in rows("SELECT data FROM session_events "
                             "WHERE event_type='update_build_observation'"):
            try:
                state["build_observations"].append(json.loads(value))
            except ValueError:
                state["build_observations"].append({"unparsed": True})
        return state
    finally:
        connection.close()


def selector_of(state):
    selectors = (state or {}).get("selectors") or []
    return selectors[0] if selectors else None


def head_of(state):
    heads = (state or {}).get("heads") or []
    return heads[0] if heads else None


def launch_by_revision(state, revision):
    for row in (state or {}).get("launches") or []:
        if row.get("selector_revision") == revision:
            return row["data"]
    return None


def build_observation_for(state, build_id):
    for value in (state or {}).get("build_observations") or []:
        if value.get("build_id") == build_id:
            return value
    return None


def record_for(state, activation_id):
    for row in (state or {}).get("records") or []:
        if row.get("activation_id") == activation_id:
            return row
    return None


# ---------------------------------------------------------------------------
# Process observation. /proc is read directly; a PID alone proves nothing.
# ---------------------------------------------------------------------------
def process_identity(pid: int):
    base = Path("/proc") / str(pid)
    try:
        argv = [printable(part) for part in
                (base / "cmdline").read_bytes().split(b"\0") if part]
        exe = os.path.realpath(os.readlink(base / "exe"))
        stat_line = (base / "stat").read_text()
    except OSError:
        return None
    tail = stat_line[stat_line.rfind(")") + 2:].split()
    return {
        "pid": pid,
        "argv": argv,
        "exe": exe,
        "exe_sha256": file_sha256(Path(exe)) if os.access(exe, os.R_OK) else None,
        "ppid": int(tail[1]) if len(tail) > 1 else -1,
    }


def scan_processes(data_dir: Path, artifact: Path | None = None):
    """Processes whose argv names this installation's data directory."""
    wanted = str(data_dir)
    runtimes, helpers = [], []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        identity = process_identity(int(entry.name))
        if identity is None or wanted not in identity["argv"]:
            continue
        if "complete-update" in identity["argv"]:
            helpers.append(identity)
        elif "start" in identity["argv"]:
            runtimes.append(identity)
    if artifact is not None:
        target = os.path.realpath(artifact)
        runtimes = [row for row in runtimes if row["exe"] == target]
    return {"runtimes": runtimes, "helpers": helpers}


# ---------------------------------------------------------------------------
# Source observation: the harness's own matched-input fingerprint.
#
# The product records its own `source_before`/`source_after` per build (HEAD,
# porcelain status, manifest and lockfile digests). The harness adds a content
# fingerprint over git-tracked and non-ignored untracked regular files, taken
# immediately before each build, so "same source" does not rest on either
# build's own claim.
# ---------------------------------------------------------------------------
def git(source: Path, arguments, env) -> tuple[int, bytes, bytes]:
    completed = subprocess.run(
        # A mounted checkout can have a different owner inside the builder.
        ["git", "--no-optional-locks", "-c", f"safe.directory={source}",
         "-C", str(source), *arguments],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, timeout=300)
    return completed.returncode, completed.stdout, completed.stderr


def porcelain_paths(raw: bytes):
    entries, index = [], 0
    while index < len(raw):
        record = raw[index:]
        if len(record) < 4:
            break
        status_code = record[:2]
        end = record.find(b"\0", 3)
        if end < 0:
            break
        entries.append((status_code, record[3:end]))
        index += end + 1
        if status_code[:1] in (b"R", b"C"):
            end = raw.find(b"\0", index)
            index = len(raw) if end < 0 else end + 1
    return entries


def source_fingerprint(source: Path, env) -> dict:
    code, listing, error = git(source, ["ls-files", "-z"], env)
    if code != 0:
        raise RuntimeError("git ls-files failed: %s" % printable(error))
    tracked = [path for path in listing.split(b"\0") if path]
    code, status, error = git(
        source, ["status", "--porcelain=v1", "-z", "--untracked-files=all"], env)
    if code != 0:
        raise RuntimeError("git status failed: %s" % printable(error))
    entries = porcelain_paths(status)
    untracked = [path for status_code, path in entries if status_code == b"??"]
    head = None
    code, raw_head, _ = git(source, ["rev-parse", "--verify", "HEAD"], env)
    if code == 0:
        head = printable(raw_head).strip() or None
    records = []
    for path in sorted(set(tracked) | set(untracked)):
        absolute = source / os.fsdecode(path)
        try:
            metadata = absolute.lstat()
        except OSError:
            records.append((printable(path), "missing"))
            continue
        if not stat.S_ISREG(metadata.st_mode):
            records.append((printable(path), "non-regular"))
            continue
        records.append((printable(path), file_sha256(absolute)))
    material = "\n".join("%s\t%s" % (digest, path) for path, digest in records)
    dirty = [printable(path) for status_code, path in entries if status_code != b"??"]
    return {
        "repo_root": str(source),
        "head": head,
        "tracked_files": len(tracked),
        "untracked_files": len(untracked),
        "dirty_tracked": dirty[:200],
        "dirty_tracked_count": len(dirty),
        "fingerprint": sha256_bytes(material.encode()),
        "fingerprint_entries": len(records),
        "status_clean": not entries,
        "source_snapshot_frozen": False,
        "ignored_files_excluded": True,
    }


# ---------------------------------------------------------------------------
# Bounded loopback transport fixture: keeps subject selection and messaging
# admissible without a real model. It is a transport, never behaviour evidence.
# ---------------------------------------------------------------------------
def scripted_handler(reply: str, latency: float):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):
            return

        def _json(self, payload, code=200):
            raw = json.dumps(payload, ensure_ascii=False).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def do_GET(self):
            if self.client_address[0] != "127.0.0.1" or self.path != "/v1/models":
                self.send_error(404)
                return
            self._json({"object": "list", "data": [
                {"id": SCRIPTED_MODEL, "object": "model"}]})

        def do_POST(self):
            if self.client_address[0] != "127.0.0.1" \
                    or self.path != "/v1/chat/completions":
                self.send_error(404)
                return
            length = int(self.headers.get("Content-Length") or 0)
            if length:
                self.rfile.read(min(length, 1 << 20))
            if latency > 0:
                time.sleep(latency)
            self._json({
                "id": "root-ab-scripted",
                "object": "chat.completion",
                "created": 0,
                "model": SCRIPTED_MODEL,
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": reply},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1,
                          "total_tokens": 2},
            })

    return Handler


class ScriptedTransport:
    def __init__(self, reply: str, latency: float):
        self.server = ThreadingHTTPServer(
            ("127.0.0.1", 0), scripted_handler(reply, latency))
        self.server.daemon_threads = True
        self.thread = None

    def start(self) -> str:
        host, port = self.server.server_address
        if host != "127.0.0.1":
            raise RuntimeError("scripted transport is not loopback")
        self.thread = threading.Thread(
            target=self.server.serve_forever, kwargs={"poll_interval": 0.1},
            daemon=True)
        self.thread.start()
        return "http://127.0.0.1:%d" % port

    def stop(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        if self.thread is not None:
            self.thread.join(timeout=5)


# ---------------------------------------------------------------------------
# Binding evaluation. Pure functions so the unit test can drive every failure
# mode with fixture dictionaries.
# ---------------------------------------------------------------------------
def generation_surface_checks(label: str, facts: dict) -> dict:
    """Every generation-bound surface must name exactly this generation."""
    generation = facts.get("generation")
    checks = {"%s_generation_is_nonempty" % label: bool(generation)}
    for key in GENERATION_NAMED_SURFACES:
        checks["%s_%s" % (label, key)] = facts.get(key) == generation
    return checks


def _paths(facts: dict):
    return [
        ("artifact_path", facts.get("artifact_path")),
        ("slot_artifact_path", facts.get("slot_artifact_path")),
        ("manifest_artifact_path", facts.get("manifest_artifact_path")),
        ("route_artifact_path", facts.get("route_artifact_path")),
        ("slot_dir", facts.get("slot_dir")),
    ]


def binding_pair_checks(a: dict, b: dict) -> dict:
    """Cross-generation checks: distinct identity, no crossover references."""
    checks = {
        "distinct_generations": bool(a.get("generation"))
        and a.get("generation") != b.get("generation"),
        "distinct_build_ids": bool(a.get("build_id"))
        and a.get("build_id") != b.get("build_id"),
        "distinct_slot_dirs": bool(a.get("slot_dir"))
        and a.get("slot_dir") != b.get("slot_dir"),
        "distinct_slot_artifact_paths": bool(a.get("slot_artifact_path"))
        and a.get("slot_artifact_path") != b.get("slot_artifact_path"),
        "selector_revision_advanced_by_one":
            isinstance(a.get("selector_revision"), int)
            and b.get("launch_receipt_revision") == a["selector_revision"] + 1,
        "selector_epoch_advanced_by_one":
            isinstance(a.get("selector_epoch"), int)
            and b.get("launch_receipt_epoch") == a["selector_epoch"] + 1,
        "b_head_binds_started_controller_epoch":
            isinstance(b.get("launch_receipt_epoch"), int)
            and b.get("head_epoch") == b.get("selector_epoch")
            and b["head_epoch"] == b["launch_receipt_epoch"] + 1,
        "b_manifest_artifact_inside_b_slot":
            inside(b.get("manifest_artifact_path"), b.get("slot_dir")),
        "b_route_artifact_inside_b_slot":
            inside(b.get("route_artifact_path"), b.get("slot_dir")),
        "b_selected_route_differs_from_a_slot_artifact":
            bool(b.get("route_artifact_path"))
            and os.path.realpath(b["route_artifact_path"])
            != os.path.realpath(a.get("slot_artifact_path") or "/nonexistent"),
    }
    crossover = []
    for label, facts, foreign in (("a", a, b), ("b", b, a)):
        for name, path in _paths(facts):
            if not isinstance(path, str) or not path:
                continue
            if foreign.get("slot_dir") and inside(path, foreign["slot_dir"]):
                crossover.append("%s.%s names foreign slot" % (label, name))
            if foreign.get("slot_artifact_path") and \
                    os.path.realpath(path) == \
                    os.path.realpath(foreign["slot_artifact_path"]):
                crossover.append("%s.%s names foreign artifact" % (label, name))
    checks["no_cross_generation_surface_reference"] = not crossover
    checks["crossover_detail"] = tuple(crossover)
    return checks


# ---------------------------------------------------------------------------
# Slot-tree observation.
# ---------------------------------------------------------------------------
def walk_tree(root) -> list[dict]:
    """Every entry beneath root; symlinks are recorded but never followed."""
    root = Path(root)
    found, pending = [], [root]
    while pending:
        directory = pending.pop()
        try:
            entries = sorted(os.scandir(directory), key=lambda entry: entry.name)
        except OSError as error:
            found.append({"path": str(directory), "error": str(error)})
            continue
        for entry in entries:
            path = Path(entry.path)
            try:
                relative = str(path.relative_to(root))
                metadata = entry.stat(follow_symlinks=False)
            except OSError as error:
                found.append({"path": str(path), "error": str(error)})
                continue
            if stat.S_ISDIR(metadata.st_mode):
                kind = "dir"
            elif stat.S_ISLNK(metadata.st_mode):
                kind = "symlink"
            elif stat.S_ISREG(metadata.st_mode):
                kind = "file"
            else:
                kind = "other"
            found.append({"path": relative, "kind": kind,
                          "mode": metadata.st_mode, "dev": metadata.st_dev,
                          "ino": metadata.st_ino, "size": metadata.st_size})
            if kind == "dir":
                pending.append(path)
    return found


def slot_tree_checks(a_slot, b_slot, *, b_artifact_path: Path,
                     b_artifact_sha256: str,
                     recorded_artifact_sha256: set) -> dict:
    """No installation-level entry, foreign executable or hard-link alias."""
    a_slot, b_slot = Path(a_slot), Path(b_slot)
    entries = walk_tree(b_slot)
    forbidden, staging, executables, foreign = [], [], [], []
    for entry in entries:
        relative = entry.get("path", "")
        parts = Path(relative).parts if relative else ()
        if not parts:
            continue
        if parts[0] in FORBIDDEN_SLOT_NAMES:
            forbidden.append(relative)
            continue
        if any(part.startswith(prefix) for part in parts
               for prefix in FORBIDDEN_SLOT_PREFIXES):
            staging.append(relative)
            continue
        if entry.get("kind") != "file" or not entry.get("mode", 0) & EXECUTABLE:
            continue
        executables.append(relative)
        if parts[0] == "artifact":
            expected = (os.path.realpath(b_artifact_path), b_artifact_sha256)
            actual = (os.path.realpath(b_slot / relative),
                      file_sha256(b_slot / relative))
            if actual != expected:
                foreign.append(relative)
        elif parts[0] == RECOVERY_PROGRAMS:
            named = len(parts) == 3 and parts[1].startswith("blake3-") \
                and HEX64.match(parts[1][len("blake3-"):] or "")
            if not named or file_sha256(b_slot / relative) not in recorded_artifact_sha256:
                foreign.append(relative)
        else:
            foreign.append(relative)

    a_inodes = {(entry.get("dev"), entry.get("ino"))
                for entry in walk_tree(a_slot) if entry.get("kind") == "file"}
    aliased = [entry["path"] for entry in entries
               if entry.get("kind") == "file"
               and (entry.get("dev"), entry.get("ino")) in a_inodes]
    carried = sorted({entry["path"].split(os.sep)[0] for entry in entries
                      if entry.get("path") and
                      entry["path"].split(os.sep)[0] == RECOVERY_PROGRAMS})
    return {
        "checks": {
            "b_slot_excludes_installation_control_entries": not forbidden,
            "b_slot_excludes_staging_and_handoff_entries": not staging,
            "b_slot_executables_are_selected_artifact_or_retained_program":
                not foreign,
            "no_hardlink_alias_between_a_slot_and_b_slot": not aliased,
        },
        "forbidden": forbidden, "staging": staging,
        "foreign_executables": foreign, "hardlink_aliases": aliased,
        "executable_files": executables,
        "carried_installation_entries": carried,
    }


# ---------------------------------------------------------------------------
# Receipt assembly and mechanical validation.
#
# crates/shenko-evidence/src/receipt.rs is the admission contract: kind/schema,
# evidence/candidate identity, passed status, digest-bound source/artifact
# originals, non-empty build/configuration/environment, explicit boundaries, an
# ordered successful run, unique passed cases and at least one scope
# observation per case with an all-true check map and a digest-bound output.
# This validator implements that contract and nothing more; it is not a truth
# judge for the observed behaviour.
# ---------------------------------------------------------------------------
def _bound_file_errors(base: Path, binding, label: str, errors: list) -> None:
    if not isinstance(binding, dict):
        errors.append("%s is not a binding object" % label)
        return
    path, digest = binding.get("path"), binding.get("sha256")
    if not isinstance(path, str) or not path.strip():
        errors.append("%s has no path" % label)
        return
    if not is_hex64(digest):
        errors.append("%s has no lowercase SHA-256" % label)
        return
    candidate = Path(path)
    absolute = candidate if candidate.is_absolute() else base / candidate
    if not absolute.is_file():
        errors.append("%s is not a readable regular file: %s" % (label, absolute))
        return
    if file_sha256(absolute) != digest:
        errors.append("%s SHA-256 mismatch: %s" % (label, absolute))


def validate_receipt(receipt, base: Path, require_passed: bool = True) -> list[str]:
    errors = []
    if not isinstance(receipt, dict):
        return ["receipt is not an object"]
    for key in REQUIRED_RECEIPT_KEYS:
        if key not in receipt:
            errors.append("missing receipt field: %s" % key)
    if receipt.get("kind") != RECEIPT_KIND:
        errors.append("receipt kind is not %s" % RECEIPT_KIND)
    if receipt.get("schema_version") != RECEIPT_SCHEMA:
        errors.append("receipt schema_version is not 1")
    for key in ("evidence_id", "candidate_commit"):
        value = receipt.get(key)
        if not isinstance(value, str) or not value.strip():
            errors.append("%s is empty" % key)
    status = receipt.get("status")
    if status not in ("passed", "failed"):
        errors.append("receipt status is not passed/failed")
    if require_passed and status != "passed":
        errors.append("receipt is not a passed observation")
    _bound_file_errors(base, receipt.get("source"), "source", errors)
    _bound_file_errors(base, receipt.get("artifact"), "artifact", errors)
    for key in ("build", "configuration", "environment"):
        value = receipt.get(key)
        if not isinstance(value, dict) or not value:
            errors.append("%s is not a non-empty object" % key)
    boundaries = receipt.get("boundaries")
    if not isinstance(boundaries, list):
        errors.append("boundaries is not an array")
    else:
        for index, item in enumerate(boundaries):
            if not isinstance(item, str) or not item.strip():
                errors.append("boundary %d is not a non-empty string" % index)
    run = receipt.get("run")
    if not isinstance(run, dict):
        errors.append("run is not an object")
    else:
        for key in REQUIRED_RUN_KEYS:
            if key not in run:
                errors.append("missing run field: %s" % key)
        started, finished = run.get("started_unix_ms"), run.get("finished_unix_ms")
        if not isinstance(started, int) or not isinstance(finished, int):
            errors.append("run timestamps are not integers")
        elif finished < started:
            errors.append("run finished before it started")
        code = run.get("exit_code")
        if not isinstance(code, int):
            errors.append("run exit_code is not an integer")
        elif require_passed and code != 0:
            errors.append("run exit_code is not zero")
        if run.get("status") is not None and run.get("status") != status:
            errors.append("run status disagrees with the receipt status")
    cases = receipt.get("cases")
    if not isinstance(cases, list):
        errors.append("cases is not an array")
        cases = []
    if require_passed and not cases:
        errors.append("receipt has no cases")
    seen = set()
    for row in cases:
        if not isinstance(row, dict):
            errors.append("case is not an object")
            continue
        for key in REQUIRED_CASE_KEYS:
            if key not in row:
                errors.append("missing case field: %s" % key)
        case_id = row.get("case_id")
        if not isinstance(case_id, str) or not case_id.strip():
            errors.append("case has no case_id")
            continue
        if case_id in seen:
            errors.append("duplicate case: %s" % case_id)
        seen.add(case_id)
        if require_passed and row.get("status") != "passed":
            errors.append("case did not pass: %s" % case_id)
        observations = row.get("observations")
        if not isinstance(observations, dict) or not observations:
            errors.append("case has no observations: %s" % case_id)
            continue
        for scope, observation in observations.items():
            if scope not in SCOPES:
                errors.append("%s: unknown observation scope %s" % (case_id, scope))
                continue
            if not isinstance(observation, dict):
                errors.append("%s/%s is not an object" % (case_id, scope))
                continue
            for key in REQUIRED_OBSERVATION_KEYS:
                if key not in observation:
                    errors.append("%s/%s missing %s" % (case_id, scope, key))
            checks = observation.get("checks")
            if not isinstance(checks, dict) or not checks:
                errors.append("%s/%s has no observed checks" % (case_id, scope))
            elif not all(isinstance(value, bool) for value in checks.values()):
                errors.append("%s/%s check map is not boolean" % (case_id, scope))
            elif require_passed and not all(checks.values()):
                errors.append("%s/%s contains a failed condition" % (case_id, scope))
            if require_passed and observation.get("status") != "passed":
                errors.append("%s/%s did not pass" % (case_id, scope))
            _bound_file_errors(base, observation.get("output"),
                               "%s/%s output" % (case_id, scope), errors)
    ab = receipt.get("ab")
    if ab is not None:
        if not isinstance(ab, dict):
            errors.append("ab extension is not an object")
        else:
            if ab.get("kind") != BINDING_KIND:
                errors.append("ab extension kind is not %s" % BINDING_KIND)
            _bound_file_errors(base, ab.get("binding"), "ab.binding", errors)
    return errors


def validate_binding(binding, require_passed: bool = True) -> list[str]:
    errors = []
    if not isinstance(binding, dict):
        return ["binding is not an object"]
    for key in REQUIRED_BINDING_KEYS:
        if key not in binding:
            errors.append("missing binding field: %s" % key)
    if binding.get("kind") != BINDING_KIND:
        errors.append("binding kind is not %s" % BINDING_KIND)
    if binding.get("schema_version") != BINDING_SCHEMA:
        errors.append("binding schema_version is not 1")
    for key in ("evidence_id", "candidate_commit"):
        value = binding.get(key)
        if not isinstance(value, str) or not value.strip():
            errors.append("binding %s is empty" % key)
    if not isinstance(binding.get("source"), dict):
        errors.append("binding source is not an object")
    generations = binding.get("generations")
    if not isinstance(generations, dict):
        errors.append("binding generations is not an object")
        generations = {}
    for label in ("a", "b"):
        facts = generations.get(label)
        if not isinstance(facts, dict):
            errors.append("binding generations.%s is not an object" % label)
            continue
        for key in REQUIRED_GENERATION_KEYS:
            if key not in facts:
                errors.append("binding generations.%s missing %s" % (label, key))
    if not isinstance(binding.get("switch"), dict):
        errors.append("binding switch is not an object")
    checks = binding.get("checks")
    if not isinstance(checks, dict) or not checks:
        errors.append("binding has no checks")
    elif not all(isinstance(value, bool) for value in checks.values()):
        errors.append("binding checks are not boolean")
    elif require_passed and not all(checks.values()):
        errors.append("binding contains a failed check")
    if not isinstance(binding.get("boundaries"), list):
        errors.append("binding boundaries is not an array")
    return errors


# ---------------------------------------------------------------------------
# The harness.
# ---------------------------------------------------------------------------
class Harness:
    def __init__(self, binary: Path, source: Path, root: Path, *, options):
        self.binary = binary
        self.source = source
        self.root = root
        self.data = root / "data"
        self.home = root / "private-home"
        self.profile = options.profile
        self.cargo_args = options.cargo_args
        self.generation_a = options.generation_a
        self.generation_b = options.generation_b
        self.total_deadline = options.deadline
        self.deadline = time.monotonic() + options.deadline
        self.build_timeout = options.build_timeout
        self.require_clean_source = options.require_clean_source
        self.provider_url = options.provider_url
        self.provider_model = options.provider_model
        self.scripted_latency = options.scripted_latency
        self.evidence_id = "root-ab-rebuild-%s" % uuid.uuid4()
        self.journal = Journal(root / "events.jsonl")
        self.failures: list[str] = []
        self.cases: dict[str, Case] = {}
        self.processes: list[dict] = []
        self.transport = None
        self.env = self._environment(options.env)
        self.started_ms = int(time.time() * 1000)
        self.finished_ms = None
        self.candidate_commit = None
        self.source_binding = None
        self.artifact_binding = None
        self.binding_document = None
        self.receipt_written = False
        self._cache: dict = {}

    # -- environment ------------------------------------------------------
    def _environment(self, forwarded: list[str]) -> dict:
        # Explicit allowlist: no ambient credential, proxy or discovery
        # configuration reaches the product beyond what the caller names.
        names = (
            "PATH", "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN",
            "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
            "SSL_CERT_FILE", "SSL_CERT_DIR", "CC", "CXX", "AR",
            "PKG_CONFIG_PATH", "LD_LIBRARY_PATH", "CARGO_NET_OFFLINE",
            "CARGO_BUILD_JOBS", "SHENKO_LOG",
        )
        env = {name: os.environ[name] for name in names if name in os.environ}
        env.setdefault("PATH",
                       "/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin")
        env.update({
            "HOME": str(self.home),
            "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8",
            "TZ": "UTC",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_TERMINAL_PROMPT": "0",
            "NO_PROXY": "127.0.0.1,localhost",
            "RUST_BACKTRACE": "0",
            "SHENKO_LOG": os.environ.get("SHENKO_LOG", "info"),
        })
        for name in forwarded:
            env[name] = os.environ[name]
        return env

    def start_transport(self) -> None:
        if self.provider_url:
            self.env.update({
                "SHENKO_USE_LOCAL_MODEL": "1",
                "SHENKO_LOCAL_MODEL": self.provider_model or "root-ab-external",
                "SHENKO_LOCAL_MODEL_URL": self.provider_url,
            })
            self.journal.emit("external_model_binding", url=self.provider_url,
                              scope="caller-provided binding; not evaluated")
            return
        self.transport = ScriptedTransport(
            "A bounded synthetic continuation.", self.scripted_latency)
        url = self.transport.start()
        self.env.update({
            "SHENKO_USE_LOCAL_MODEL": "1",
            "SHENKO_LOCAL_MODEL": SCRIPTED_MODEL,
            "SHENKO_LOCAL_MODEL_URL": url,
        })
        self.journal.emit("scripted_transport_ready", url=url,
                          scope="loopback transport fixture, not a model claim")

    # -- time -------------------------------------------------------------
    def remaining(self) -> float:
        return max(0.0, self.deadline - time.monotonic())

    def case(self, case_id: str, scope: str) -> Case:
        row = Case(self, case_id, scope)
        self.cases[case_id] = row
        return row

    def wait_for(self, predicate, label: str, timeout: float,
                 interval: float = 0.1):
        limit = time.monotonic() + min(timeout, max(0.1, self.remaining()))
        last = None
        while time.monotonic() < limit:
            last = predicate()
            if last:
                self.journal.emit("waited", label=label, satisfied=True)
                return last
            time.sleep(interval)
        self.journal.emit("waited", label=label, satisfied=False,
                          observed=repr(last)[:2000])
        return None

    # -- process control --------------------------------------------------
    def spawn_runtime(self, label: str, artifact: Path | None = None, *,
                      pause_at_start: str | None = None) -> dict:
        artifact = artifact or self.binary
        stdout = open(self.root / ("%s.stdout" % label), "xb", buffering=0)
        stderr = open(self.root / ("%s.stderr" % label), "xb", buffering=0)
        argv = [str(artifact), "--data-dir", str(self.data), "start",
                "--port", "0"]
        if pause_at_start:
            argv.extend(["--pause-at-start", pause_at_start])
        process = subprocess.Popen(
            argv, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
            env=self.env, close_fds=True, start_new_session=True)
        row = {"label": label, "process": process, "argv": argv,
               "stdout": str(self.root / ("%s.stdout" % label)),
               "stderr": str(self.root / ("%s.stderr" % label))}
        self.processes.append(row)
        self.journal.emit("runtime_started", label=label, pid=process.pid,
                          argv=argv)
        return row

    def cli(self, label: str, *arguments, timeout: float | None = None):
        argv = [str(self.binary), "--data-dir", str(self.data), *arguments]
        limit = min(timeout or 120.0, max(1.0, self.remaining()))
        self.journal.emit("cli_started", label=label, argv=argv, timeout=limit)
        process = subprocess.Popen(
            argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, env=self.env, close_fds=True,
            start_new_session=True)
        try:
            stdout, stderr = process.communicate(timeout=limit)
            code = process.returncode
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            stdout, stderr = process.communicate(timeout=30)
            code = None
        write_new(self.root / ("%s.stdout" % label), stdout)
        write_new(self.root / ("%s.stderr" % label), stderr)
        self.journal.emit("cli_finished", label=label, exit_code=code,
                          stdout_sha256=sha256_bytes(stdout),
                          stderr_sha256=sha256_bytes(stderr))
        return code, stdout, stderr

    def cli_json(self, label: str, *arguments, timeout: float | None = None):
        code, stdout, stderr = self.cli(label, *arguments, timeout=timeout)
        if code != 0:
            raise HarnessStop("%s exited %s: %s" % (
                label, code, stderr.decode(errors="replace")[-4000:]))
        return json.loads(stdout)

    def stop(self, row: dict, sig=signal.SIGINT, timeout: float = 60.0):
        process = row["process"]
        if process.poll() is not None:
            return process.returncode
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=30)
        self.journal.emit("runtime_stopped", label=row["label"],
                          exit_code=process.returncode, signal=int(sig))
        return process.returncode

    # -- product reads ----------------------------------------------------
    def status(self, label: str):
        return self.cli_json(label, "update", "status", timeout=120)

    def live_db(self) -> Path:
        pointer = read_pointer(self.data)
        if pointer and pointer.get("slot_dir"):
            return Path(pointer["slot_dir"]) / SLOT_DB
        return self.data / SLOT_DB

    def cas(self) -> Path:
        return self.data / "cas"

    def pause_subject(self, label: str, case: Case,
                      timeout: float = 240.0) -> str | None:
        request_id = str(uuid.uuid4())
        code, _stdout, stderr = self.cli(
            "%s-pause" % label, "runtime", "pause", "--request-id", request_id,
            timeout=120)
        if code != 0:
            case.note(**{"%s_pause_error" % label:
                         stderr.decode(errors="replace")[-2000:]})
            return None
        record = self.wait_for(
            lambda: (lambda raw: raw if raw and raw.get("state") == "paused"
                     else None)(self._quiesce_status(request_id)),
            "%s_paused" % label, timeout, interval=0.2)
        return request_id if record else None

    def _quiesce_status(self, request_id: str):
        code, stdout, _stderr = self.cli(
            "quiesce-status-%s" % request_id, "runtime", "status", request_id,
            timeout=60)
        if code != 0:
            return None
        try:
            return json.loads(stdout)
        except ValueError:
            return None

    def prepare(self, label: str, generation: str, timeout=None):
        arguments = ["update", "prepare", "--generation", generation,
                     "--source-dir", str(self.source), "--profile", self.profile]
        if self.cargo_args:
            arguments.extend(["--extra-args", self.cargo_args])
        return self.cli_json(label, *arguments,
                             timeout=timeout or self.build_timeout)

    def activate(self, label: str, activation_id: str, expected_revision: int,
                 timeout: float = 300.0):
        return self.cli_json(
            label, "update", "activate", "--activation", activation_id,
            "--expected-selector-revision", str(expected_revision),
            timeout=timeout)

    @staticmethod
    def expect_refusal(label: str, code, stdout, stderr, needles) -> dict:
        text = (stdout + stderr).decode(errors="replace")
        lowered = text.lower()
        return {
            "label": label,
            "exit_code": code,
            "refused": code is not None and code != 0,
            "matched": [needle for needle in needles
                        if needle.lower() in lowered],
            "needed": list(needles),
            "stderr_tail": text[-2000:],
        }

    # -- preparation chain ------------------------------------------------
    def read_preparation(self, record, source_state: dict) -> dict:
        """Follow the product's own sealed preparation chain from the record."""
        cas_dir = self.cas()
        _raw, closure = read_cas(cas_dir, record["recovery_closure_ref"])
        observation_ref = closure.get("build_receipt_ref")
        _raw, observation = read_cas(cas_dir, observation_ref)
        outcome_ref = observation.get("build_outcome_ref")
        _raw, outcome = read_cas(cas_dir, outcome_ref)
        _raw, to_state = read_cas(cas_dir, record["to_state_ref"])
        slot_dir = str(to_state["slot_dir"])
        slot_manifest = read_slot_manifest(slot_dir)
        db_state = read_db(Path(slot_dir) / SLOT_DB)
        selector = selector_of(db_state)
        head = head_of(db_state)
        launch = launch_by_revision(
            db_state, (record.get("expected_selector_revision") or 0) + 1)
        route = None
        if selector and selector.get("route_authority_ref"):
            _raw, route = read_cas(cas_dir, selector["route_authority_ref"])
        artifact = outcome.get("artifact") or {}
        slot_artifact = Path(slot_dir) / "artifact" / "shenko"
        facts = {
            "generation": record.get("to_generation"),
            "build_id": outcome.get("build_id"),
            "slot_dir": slot_dir,
            "slot_artifact_path": str(slot_artifact),
            "slot_artifact_sha256":
                file_sha256(slot_artifact) if slot_artifact.is_file() else None,
            "artifact_record": artifact,
            "artifact_file_sha256": (
                file_sha256(Path(artifact["path"]))
                if artifact.get("path") and Path(artifact["path"]).is_file()
                else None),
            "artifact_digest": artifact.get("digest"),
            "slot_manifest": slot_manifest,
            "db_state": db_state,
            "closure": closure,
            "closure_digest": record["recovery_closure_ref"].get("digest"),
            "build_observation": observation,
            "observation_digest": (observation_ref or {}).get("digest"),
            "build_outcome": outcome,
            "output_layout": outcome.get("output_layout"),
            "source_before": outcome.get("source_before"),
            "source_after": outcome.get("source_after"),
            "activation_id": record.get("activation_id"),
            "record": record,
            "_source_state": source_state,
        }
        self._refresh_selection(facts, selector, head, launch, route,
                                slot_manifest)
        return facts

    @staticmethod
    def _refresh_selection(facts, selector, head, launch, route, manifest=None):
        facts["selector"] = selector
        facts["head"] = head
        facts["launch_receipt"] = launch
        facts["route"] = route
        if manifest is not None:
            facts["slot_manifest"] = manifest
        manifest = facts.get("slot_manifest") or {}
        facts.update({
            "build_artifact_generation": (facts.get("artifact_record") or {}).get("generation"),
            "build_observation_generation":
                ((facts.get("build_observation") or {}).get("prepare_request")
                 or {}).get("generation"),
            "activation_to_generation": (facts.get("record") or {}).get("to_generation"),
            "slot_manifest_generation": manifest.get("generation"),
            "route_generation": (route or {}).get("generation"),
            "launch_receipt_generation": (launch or {}).get("generation"),
            "head_generation": (head or {}).get("software_generation"),
            "selector_generation": (selector or {}).get("generation"),
            "selector_revision": (selector or {}).get("selector_revision"),
            "selector_epoch": (selector or {}).get("active_epoch"),
            "slot_epoch": (route or {}).get("active_epoch"),
            "launch_receipt_revision": (launch or {}).get("selector_revision"),
            "launch_receipt_epoch": (launch or {}).get("selector_epoch"),
            "head_epoch": (head or {}).get("controller_epoch"),
            "manifest_artifact_path": manifest.get("artifact_path"),
            "manifest_artifact_digest": manifest.get("artifact_digest"),
            "route_artifact_path": (route or {}).get("artifact_path"),
            "route_artifact_digest": (route or {}).get("artifact_digest"),
            "artifact_path": (facts.get("artifact_record") or {}).get("path"),
        })

    # -- the run ----------------------------------------------------------
    def run(self) -> None:
        self.start_transport()
        bootstrap = self._bootstrap()
        facts_a = self._install_a(bootstrap)
        facts_b = self._install_b(facts_a)
        self._switch(facts_a, facts_b)
        self._refusals_live(facts_a, facts_b)
        self._cold_relaunch(facts_a, facts_b)
        self._refusals_retained(facts_a, facts_b)
        self._assembly(facts_a, facts_b)

    def _bootstrap(self) -> dict:
        case = self.case("AB-01", "native")
        pause = str(uuid.uuid4())
        row = self.spawn_runtime("bootstrap", pause_at_start=pause)
        state = self.wait_for(
            lambda: (lambda db: db if db and selector_of(db) and head_of(db)
                     else None)(read_db(self.live_db())),
            "bootstrap_genesis", 300)
        case.require("bootstrap_genesis_visible", bool(state))
        endpoint = self.wait_for(lambda: read_control_runtime_id(self.data),
                                 "bootstrap_control", 180)
        case.require("bootstrap_control_endpoint_published", bool(endpoint))

        def read_paused_boundary():
            current = read_db(self.live_db())
            head = head_of(current)
            if not selector_of(current) or not head or head.get("mode") != "paused":
                return None
            for request in current["quiesce"]:
                if (request.get("request_id") == pause
                        and request.get("state") == "paused"
                        and request.get("agent_id") == head.get("agent_id")
                        and request.get("epoch") == head.get("controller_epoch")
                        and request.get("generation") == head.get("software_generation")
                        and request.get("boundary_at") is not None
                        and request.get("closed_at") is None):
                    return current, request
            return None

        boundary = self.wait_for(read_paused_boundary, "bootstrap_requested_pause", 180)
        case.require("bootstrap_head_is_paused_at_requested_boundary", bool(boundary))
        state, pause_receipt = boundary
        selector = selector_of(state)
        head = head_of(state)
        genesis_launch = launch_by_revision(state, 0) or {}
        case.require("bootstrap_genesis_launch_binds_revision_zero",
                     genesis_launch.get("selector_revision") == 0
                     and genesis_launch.get("selector_epoch") == 1
                     and genesis_launch.get("generation") == selector.get("generation")
                     and (selector.get("origin") or {}).get("kind") == "genesis",
                     launch=genesis_launch, selector=selector)
        case.require("bootstrap_selector_binds_started_controller",
                     selector.get("selector_revision") == 1
                     and selector.get("active_epoch") == 2
                     and selector.get("active_epoch") == head.get("controller_epoch"),
                     selector=selector, head=head)
        bootstrap = {
            "pid": row["process"].pid,
            "generation": selector.get("generation"),
            "selector_revision": selector.get("selector_revision"),
            "selector_epoch": selector.get("active_epoch"),
            "origin": selector.get("origin"),
            "head_mode": head.get("mode"),
            "head_epoch": head.get("controller_epoch"),
            "control_runtime_id": endpoint,
            "binary": str(self.binary),
            "binary_sha256": file_sha256(self.binary),
            "pause_request": pause,
            "pause_boundary": pause_receipt["boundary_at"],
        }
        case.note(bootstrap=bootstrap)
        self._cache["bootstrap"] = bootstrap
        self._cache["bootstrap_row"] = row
        return bootstrap

    def _source_material(self, case: Case, stage: str) -> dict:
        fingerprint = source_fingerprint(self.source, self.env)
        case.note(**{stage: fingerprint})
        self.journal.emit("source_fingerprint", stage=stage,
                          fingerprint=fingerprint["fingerprint"],
                          head=fingerprint["head"],
                          clean=fingerprint["status_clean"])
        return fingerprint

    def _started_selection(self, slot_db, label, generation, launch_revision,
                           launch_epoch, mode, timeout):
        # Launch binds the selected authority before the new process takes over.
        # The takeover advances the ongoing selector and head together.
        def observe():
            state = read_db(slot_db)
            launch = launch_by_revision(state, launch_revision) or {}
            if (launch.get("generation") != generation
                    or launch.get("selector_epoch") != launch_epoch):
                return None
            selector = selector_of(state) or {}
            head = head_of(state) or {}
            if (selector.get("generation") == generation
                    and selector.get("selector_revision") == launch_revision + 1
                    and selector.get("active_epoch") == launch_epoch + 1
                    and head.get("software_generation") == generation
                    and head.get("controller_epoch") == launch_epoch + 1
                    and head.get("mode") == mode):
                return state, launch
            return None

        return self.wait_for(observe, label, timeout)

    def _install_a(self, bootstrap: dict) -> dict:
        case = self.case("AB-03", "native")
        before = self._source_material(case, "source_before_a")
        self._cache["source_before_a"] = before
        self.candidate_commit = before.get("head") or "UNBOUND"
        status = self.status("status_before_a")
        selector = status.get("selector") or {}
        case.require("bootstrap_selector_still_names_started_controller",
                     selector.get("selector_revision") == bootstrap["selector_revision"]
                     and selector.get("active_epoch") == bootstrap["selector_epoch"]
                     and selector.get("generation") == bootstrap["generation"],
                     selector=selector)
        record = self.prepare("prepare_a", self.generation_a)
        self._remember("prepared_a", record)
        case.require("a_prepare_targets_new_generation",
                     record.get("to_generation") == self.generation_a,
                     to_generation=record.get("to_generation"),
                     from_generation=record.get("from_generation"))
        facts = self.read_preparation(record, before)
        self._cache["a"] = facts

        case.check("a_build_artifact_generation_is_a",
                   facts["build_artifact_generation"] == self.generation_a,
                   observed=facts["build_artifact_generation"])
        case.check("a_build_observation_generation_is_a",
                   facts["build_observation_generation"] == self.generation_a,
                   observed=facts["build_observation_generation"])
        case.check("a_activation_from_generation_is_bootstrap",
                   record.get("from_generation") == bootstrap["generation"],
                   observed=record.get("from_generation"),
                   bootstrap=bootstrap["generation"])
        launch_revision = bootstrap["selector_revision"] + 1
        launch_epoch = bootstrap["selector_epoch"] + 1
        case.require("a_activation_expected_selector_revision_binds_bootstrap",
                     record.get("expected_selector_revision") == launch_revision - 1,
                     observed=record.get("expected_selector_revision"))
        case.require("a_activation_target_epoch_advances_once",
                     record.get("target_epoch") == launch_epoch,
                     observed=record.get("target_epoch"),
                     bootstrap=bootstrap["selector_epoch"])
        case.check("a_slot_manifest_generation_is_a",
                   facts["slot_manifest_generation"] == self.generation_a,
                   observed=facts["slot_manifest_generation"])
        case.check("a_slot_manifest_digest_is_build_digest",
                   facts["manifest_artifact_digest"] == facts["artifact_digest"],
                   manifest=facts["manifest_artifact_digest"],
                   build=facts["artifact_digest"])

        live_state = read_db(self.data / SLOT_DB)
        rooted = build_observation_for(live_state, facts["build_id"])
        case.check("a_build_observation_rooted_in_live_history", bool(rooted),
                   build_id=facts["build_id"])
        if rooted:
            case.check("a_rooted_observation_names_same_receipt",
                       ((rooted.get("build_receipt_ref") or {}).get("digest")
                        == facts["observation_digest"]))

        ack = self.activate("activate_a", facts["activation_id"],
                            record.get("expected_selector_revision"))
        self._remember("activated_a_ack", ack)
        case.require("a_handoff_acknowledged_retirement_only",
                     ack.get("state") == "awaiting_old_process_retirement"
                     and ack.get("activation_succeeded") is False, ack=ack)
        bootstrap_row = self._cache["bootstrap_row"]
        case.require("a_bootstrap_process_retired",
                     bool(self.wait_for(
                         lambda: bootstrap_row["process"].poll() is not None,
                         "bootstrap_retired", 600)),
                     pid=bootstrap_row["process"].pid)

        slot_db = Path(facts["slot_dir"]) / SLOT_DB
        selected = self._started_selection(slot_db, "a_selected", self.generation_a,
                                           launch_revision, launch_epoch, "active", 600)
        case.require("a_started_authority_committed", bool(selected))
        committed, launch = selected
        running = self.wait_for(
            lambda: scan_processes(self.data, Path(facts["slot_artifact_path"]))["runtimes"],
            "a_runtime_process", 600)
        case.require("a_runtime_process_runs_selected_artifact",
                     bool(running) and running[0]["exe"]
                     == os.path.realpath(facts["slot_artifact_path"]),
                     processes=running)
        self._cache["a_pid"] = running[0]["pid"]
        case.require("a_control_endpoint_replaced",
                     bool(self.wait_for(
                         lambda: read_control_runtime_id(self.data)
                         != bootstrap["control_runtime_id"],
                         "a_control", 600)))
        selector_a = selector_of(committed)
        head_a = head_of(committed)
        _raw, route_a = read_cas(self.cas(), selector_a["route_authority_ref"])
        manifest_a = read_slot_manifest(facts["slot_dir"])
        self._refresh_selection(facts, selector_a, head_a, launch, route_a,
                                manifest_a)
        facts["manifest_after_install"] = manifest_a
        case.check("a_selector_origin_is_activation_a",
                   (selector_a.get("origin") or {}).get("activation_id")
                   == facts["activation_id"], origin=selector_a.get("origin"))
        case.check("a_launch_receipt_binds_generation",
                   (launch or {}).get("generation") == self.generation_a
                   and (launch or {}).get("selector_revision") == launch_revision
                   and (launch or {}).get("selector_epoch") == launch_epoch
                   and os.path.realpath((launch or {}).get("slot_dir") or "/nonexistent")
                   == os.path.realpath(facts["slot_dir"]),
                   launch=launch)
        case.check("a_route_generation_binds_a",
                   (route_a or {}).get("generation") == self.generation_a
                   and (route_a or {}).get("selector_revision") == launch_revision
                   and (route_a or {}).get("active_epoch") == launch_epoch
                   and os.path.realpath((route_a or {}).get("artifact_path") or "/nonexistent")
                   == os.path.realpath(facts["slot_artifact_path"]),
                   route=route_a)
        case.check("a_head_active_in_selected_generation",
                   (head_a or {}).get("software_generation") == self.generation_a
                   and (head_a or {}).get("mode") == "active"
                   and (head_a or {}).get("controller_epoch") == launch_epoch + 1,
                   head=head_a)
        case.check("a_launcher_pointer_names_a_slot",
                   os.path.realpath((read_pointer(self.data) or {}).get("slot_dir")
                                    or "/nonexistent")
                   == os.path.realpath(facts["slot_dir"]),
                   pointer=read_pointer(self.data))
        self.journal.emit("installed_a", selector=selector_a, launch=launch)
        return facts

    def _install_b(self, facts_a: dict) -> dict:
        source_case = self.case("AB-02", "native")
        b_case = self.case("AB-04", "native")
        pause = self.pause_subject("pause_a", b_case)
        b_case.require("a_subject_paused_before_b_prepare", bool(pause))
        before = self._source_material(source_case, "source_before_b")
        live_before = read_db(Path(facts_a["slot_dir"]) / SLOT_DB)
        b_case.require("a_live_state_readable_before_b",
                       bool(live_before) and bool(selector_of(live_before)))
        record = self.prepare("prepare_b", self.generation_b)
        self._remember("prepared_b", record)
        b_case.require("b_prepare_targets_new_generation",
                       record.get("to_generation") == self.generation_b,
                       to_generation=record.get("to_generation"),
                       from_generation=record.get("from_generation"))
        facts = self.read_preparation(record, before)
        self._cache["b"] = facts
        b_case.check("b_activation_from_generation_is_a",
                     record.get("from_generation") == self.generation_a,
                     observed=record.get("from_generation"))
        b_case.check("b_activation_expected_selector_revision_is_a_revision",
                     record.get("expected_selector_revision")
                     == facts_a["selector"]["selector_revision"],
                     observed=record.get("expected_selector_revision"),
                     a_revision=facts_a["selector"]["selector_revision"])
        b_case.check("b_slot_manifest_generation_is_b",
                     facts["slot_manifest_generation"] == self.generation_b,
                     observed=facts["slot_manifest_generation"])

        pre = read_db(Path(facts["slot_dir"]) / SLOT_DB)
        b_case.check("b_copy_ledger_identity_differs_from_a",
                     bool(pre) and pre.get("ledger_id")
                     and pre.get("ledger_id") != (live_before or {}).get("ledger_id"),
                     b_ledger=(pre or {}).get("ledger_id"),
                     a_ledger=(live_before or {}).get("ledger_id"))
        b_case.check("b_candidate_selector_still_names_a",
                     (selector_of(pre) or {}).get("generation") == self.generation_a,
                     selector=selector_of(pre))
        b_case.check("b_candidate_selector_revision_is_prepared_value",
                     (selector_of(pre) or {}).get("selector_revision")
                     == record.get("expected_selector_revision"),
                     selector=selector_of(pre))
        b_case.check("launcher_still_names_a_slot",
                     os.path.realpath((read_pointer(self.data) or {}).get("slot_dir")
                                      or "/nonexistent")
                     == os.path.realpath(facts_a["slot_dir"]),
                     pointer=read_pointer(self.data))
        rooted = build_observation_for(live_before, facts["build_id"])
        b_case.check("b_build_observation_rooted_in_a_history", bool(rooted),
                     build_id=facts["build_id"])
        if rooted:
            b_case.check("b_rooted_observation_names_same_receipt",
                         ((rooted.get("build_receipt_ref") or {}).get("digest")
                          == facts["observation_digest"]))
        a_pid = self._cache.get("a_pid")
        alive = process_identity(a_pid) if a_pid else None
        b_case.check("a_process_alive_and_unchanged_during_b_prepare",
                     bool(alive)
                     and alive["exe"] == os.path.realpath(facts_a["slot_artifact_path"]),
                     a_pid=a_pid, identity=alive)

        self._source_checks(source_case, facts_a, facts)
        self._build_identity_checks(source_case, facts_a, facts)
        return facts

    def _source_checks(self, case: Case, a: dict, b: dict) -> None:
        source_a = a.get("_source_state") or {}
        source_b = b.get("_source_state") or {}
        observations_a = a.get("source_before") or {}
        observations_b = b.get("source_before") or {}
        artifact_a = a.get("artifact_record") or {}
        artifact_b = b.get("artifact_record") or {}
        case.check("source_fingerprint_equal_across_both_builds",
                   bool(source_a.get("fingerprint"))
                   and source_a.get("fingerprint") == source_b.get("fingerprint"),
                   fingerprint_a=source_a.get("fingerprint"),
                   fingerprint_b=source_b.get("fingerprint"))
        case.check("harness_fingerprint_covers_source_files",
                   (source_a.get("fingerprint_entries") or 0) > 0
                   and (source_b.get("fingerprint_entries") or 0) > 0,
                   entries_a=source_a.get("fingerprint_entries"),
                   entries_b=source_b.get("fingerprint_entries"))
        case.check("both_builds_observed_source_before_and_after",
                   bool(observations_a) and bool(observations_b)
                   and bool(a.get("source_after")) and bool(b.get("source_after")))
        case.check("source_manifest_digests_equal",
                   artifact_a.get("manifest_digest") is not None
                   and artifact_a.get("manifest_digest")
                   == artifact_b.get("manifest_digest"),
                   a=artifact_a.get("manifest_digest"),
                   b=artifact_b.get("manifest_digest"))
        case.check("source_lockfile_digests_equal",
                   artifact_a.get("lockfile_digest") is not None
                   and artifact_a.get("lockfile_digest")
                   == artifact_b.get("lockfile_digest"),
                   a=artifact_a.get("lockfile_digest"),
                   b=artifact_b.get("lockfile_digest"))
        case.check("source_revision_observations_agree",
                   artifact_a.get("source_revision")
                   == artifact_b.get("source_revision"),
                   a=artifact_a.get("source_revision"),
                   b=artifact_b.get("source_revision"))
        if self.require_clean_source:
            case.check("source_checkout_clean_for_both_builds",
                       artifact_a.get("source_revision") is not None
                       and artifact_b.get("source_revision") is not None,
                       a=artifact_a.get("source_revision"),
                       b=artifact_b.get("source_revision"))
        case.check("neither_build_reported_a_refusal",
                   (a.get("build_outcome") or {}).get("refusal") is None
                   and (b.get("build_outcome") or {}).get("refusal") is None)
        case.note(candidate_commit=self.candidate_commit,
                  source_clean=bool(artifact_a.get("source_revision")))

    def _build_identity_checks(self, case: Case, a: dict, b: dict) -> None:
        case.check("build_ids_differ",
                   bool(a.get("build_id")) and a.get("build_id") != b.get("build_id"),
                   a=a.get("build_id"), b=b.get("build_id"))
        case.check("requested_generations_differ",
                   a.get("generation") != b.get("generation"))
        case.check("build_artifact_generations_match_requests",
                   a.get("build_artifact_generation") == a.get("generation")
                   and b.get("build_artifact_generation") == b.get("generation"))
        layout_a = a.get("output_layout") or {}
        layout_b = b.get("output_layout") or {}
        case.check("owned_output_directories_differ",
                   bool(layout_a.get("owned_directory"))
                   and layout_a.get("owned_directory")
                   != layout_b.get("owned_directory"),
                   a=layout_a.get("owned_directory"),
                   b=layout_b.get("owned_directory"))
        case.check("owned_output_directories_exist",
                   bool(layout_a.get("owned_directory"))
                   and Path(layout_a["owned_directory"]).is_dir()
                   and bool(layout_b.get("owned_directory"))
                   and Path(layout_b["owned_directory"]).is_dir())
        record_a = a.get("artifact_record") or {}
        record_b = b.get("artifact_record") or {}
        case.check("build_artifacts_inside_their_owned_tree",
                   bool(record_a.get("path"))
                   and inside(record_a["path"], layout_a.get("owned_directory"))
                   and bool(record_b.get("path"))
                   and inside(record_b["path"], layout_b.get("owned_directory")))
        case.check("build_artifact_bytes_match_recorded_sha256",
                   bool(a.get("artifact_file_sha256"))
                   and bool(b.get("artifact_file_sha256"))
                   and a["artifact_file_sha256"]
                   == file_sha256(Path(record_a["path"]))
                   and b["artifact_file_sha256"]
                   == file_sha256(Path(record_b["path"])))
        case.check("slot_artifact_paths_differ",
                   a.get("slot_artifact_path") != b.get("slot_artifact_path"))
        relation = ("equal"
                    if a.get("artifact_file_sha256") == b.get("artifact_file_sha256")
                    else "distinct")
        case.note(artifact_bytes_relation=relation)
        self.journal.emit("build_identity", a=a.get("build_id"),
                          b=b.get("build_id"), relation=relation)

    def _switch(self, facts_a: dict, facts_b: dict) -> None:
        case = self.case("AB-05", "native")
        launch_revision = facts_a["selector_revision"] + 1
        launch_epoch = facts_a["selector_epoch"] + 1
        ack = self.activate("activate_b", facts_b["activation_id"],
                            facts_b["record"].get("expected_selector_revision"),
                            timeout=900)
        self._remember("activated_b_ack", ack)
        case.require("b_handoff_acknowledged_retirement_only",
                     ack.get("state") == "awaiting_old_process_retirement"
                     and ack.get("activation_succeeded") is False, ack=ack)
        old_pid = self._cache.get("a_pid")
        case.require("retired_a_process_exited",
                     bool(old_pid) and bool(self.wait_for(
                         lambda: process_identity(old_pid) is None,
                         "a_retired", 900)),
                     a_pid=old_pid)
        slot_db = Path(facts_b["slot_dir"]) / SLOT_DB
        selected = self._started_selection(slot_db, "b_selected", self.generation_b,
                                           launch_revision, launch_epoch, "active", 900)
        case.require("b_started_authority_committed", bool(selected))
        committed, launch = selected
        selector_b = selector_of(committed)
        head_b = head_of(committed)
        _raw, route_b = read_cas(self.cas(), selector_b["route_authority_ref"])
        running = self.wait_for(
            lambda: scan_processes(self.data, Path(facts_b["slot_artifact_path"]))["runtimes"],
            "b_runtime_process", 900)
        case.require("new_runtime_process_runs_b_artifact", bool(running),
                     processes=running)
        manifest_b = read_slot_manifest(facts_b["slot_dir"])
        self._refresh_selection(facts_b, selector_b, head_b, launch, route_b,
                                manifest_b)
        facts_b["manifest_after_switch"] = manifest_b
        facts_b["pid"] = running[0]["pid"]
        self._cache["b_pid"] = running[0]["pid"]

        case.check("selector_revision_advanced_by_one",
                   launch.get("selector_revision") == launch_revision)
        case.check("selector_epoch_advanced_by_one",
                   launch.get("selector_epoch") == launch_epoch)
        case.check("selector_origin_is_activation_b",
                   (selector_b.get("origin") or {}).get("activation_id")
                   == facts_b["activation_id"])
        case.check("launcher_pointer_names_b_slot",
                   os.path.realpath((read_pointer(self.data) or {}).get("slot_dir")
                                    or "/nonexistent")
                   == os.path.realpath(facts_b["slot_dir"]),
                   pointer=read_pointer(self.data))
        case.check("launch_receipt_binds_b_generation",
                   (launch or {}).get("generation") == self.generation_b
                   and os.path.realpath((launch or {}).get("slot_dir")
                                        or "/nonexistent")
                   == os.path.realpath(facts_b["slot_dir"]),
                   launch=launch)
        case.check("new_process_exe_is_b_slot_artifact",
                   running[0]["exe"]
                   == os.path.realpath(facts_b["slot_artifact_path"]),
                   exe=running[0]["exe"])
        case.check("b_head_active_in_selected_generation",
                   (head_b or {}).get("software_generation") == self.generation_b
                   and (head_b or {}).get("mode") == "active"
                   and (head_b or {}).get("controller_epoch") == launch_epoch + 1,
                   head=head_b)
        case.check("b_route_generation_binds_b",
                   (route_b or {}).get("generation") == self.generation_b
                   and (route_b or {}).get("selector_revision") == launch_revision
                   and (route_b or {}).get("active_epoch") == launch_epoch,
                   route=route_b)

        retired = read_db(Path(facts_a["slot_dir"]) / SLOT_DB)
        facts_a["retired_state"] = retired
        mirrored = selector_of(retired) or {}
        case.check("retired_head_fenced_in_handoff_mode",
                   (head_of(retired) or {}).get("mode") == "handoff"
                   and (head_of(retired) or {}).get("software_generation")
                   == self.generation_a, head=head_of(retired))
        case.check("retired_selector_mirrors_selected_authority",
                   mirrored.get("selector_revision") == launch_revision
                   and mirrored.get("active_epoch") == launch_epoch
                   and mirrored.get("generation") == self.generation_b,
                   mirrored=mirrored)
        generations = self.status("status_after_switch").get("generations") or []
        case.check("both_generation_slots_retained",
                   len(generations) >= 2
                   and os.path.basename(facts_a["slot_dir"]) in generations
                   and os.path.basename(facts_b["slot_dir"]) in generations,
                   generations=generations)
        helpers = self.wait_for(lambda: scan_processes(self.data)["helpers"],
                                "handoff_helper", 1) or []
        case.note(handoff_helpers_observed=helpers)
        self._cache["switch_ack"] = ack
        self.journal.emit("switched", selector=selector_b, launch=launch)

    def _cold_relaunch(self, facts_a: dict, facts_b: dict) -> None:
        case = self.case("AB-09", "recovery")
        pause = self.pause_subject("pause_b", case)
        case.require("b_subject_paused_before_cold_restart", bool(pause))
        pid = facts_b.get("pid")
        if pid:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        case.check("selected_process_terminated_for_restart",
                   bool(pid) and bool(self.wait_for(
                       lambda: process_identity(pid) is None, "b_killed", 180)))
        pointer = read_pointer(self.data)
        case.check("launcher_pointer_names_b_slot_after_kill",
                   os.path.realpath((pointer or {}).get("slot_dir") or "/nonexistent")
                   == os.path.realpath(facts_b["slot_dir"]), pointer=pointer)
        launcher_slot = (pointer or {}).get("slot_dir")
        manifest = read_slot_manifest(launcher_slot)
        case.check("launcher_slot_manifest_generation_is_b",
                   (manifest or {}).get("generation") == self.generation_b,
                   manifest=manifest)
        launcher_artifact = Path((manifest or {}).get("artifact_path")
                                 or "/nonexistent")
        case.check("launcher_artifact_path_inside_b_slot",
                   launcher_artifact.is_file()
                   and inside(launcher_artifact, facts_b["slot_dir"]),
                   artifact=str(launcher_artifact))
        case.check("launcher_artifact_is_the_selected_artifact",
                   launcher_artifact.is_file()
                   and os.path.realpath(launcher_artifact)
                   == os.path.realpath(facts_b["slot_artifact_path"]),
                   artifact=str(launcher_artifact))
        case.require("launcher_artifact_exists", launcher_artifact.is_file(),
                     artifact=str(launcher_artifact))

        row = self.spawn_runtime("relaunch", artifact=launcher_artifact)
        launch_revision = facts_b["selector_revision"]
        launch_epoch = facts_b["selector_epoch"]
        slot_db = Path(facts_b["slot_dir"]) / SLOT_DB
        selected = self._started_selection(slot_db, "relaunch_state", self.generation_b,
                                           launch_revision, launch_epoch, "paused", 600)
        case.require("relaunch_rebound_selected_store", bool(selected))
        state, relaunch = selected
        running = self.wait_for(
            lambda: scan_processes(self.data, launcher_artifact)["runtimes"],
            "relaunch_process", 600)
        case.require("relaunched_process_runs_launcher_artifact", bool(running),
                     processes=running)
        original = launch_by_revision(state, facts_b["launch_receipt_revision"]) or {}
        case.check("relaunch_preserves_original_b_launch",
                   original == facts_b["launch_receipt"], original=original)
        ids = {original.get("launch_id"), relaunch.get("launch_id")}
        case.check("launch_receipt_ids_are_distinct", None not in ids and len(ids) == 2,
                   original=original, relaunch=relaunch)
        case.check("launch_receipts_bind_the_same_selected_generation",
                   all(launch.get("generation") == self.generation_b
                       and os.path.realpath(launch.get("slot_dir") or "/nonexistent")
                       == os.path.realpath(facts_b["slot_dir"])
                       for launch in (original, relaunch)),
                   original=original, relaunch=relaunch)
        header = head_of(state) or {}
        case.check("deliberate_pause_preserved_across_relaunch",
                   header.get("mode") == "paused"
                   and any(record.get("request_id") == pause
                           and record.get("state") == "paused"
                           and record.get("epoch") == header.get("controller_epoch")
                           and record.get("boundary_at") is not None
                           and record.get("closed_at") is None
                           for record in state["quiesce"]), head=header)
        a_launch = self._retained_launch(facts_a)
        case.check("retired_a_launch_receipt_still_names_a",
                   a_launch == facts_a["launch_receipt"],
                   launch=a_launch)
        self.stop(row)
        case.check("relaunch_process_stopped", row["process"].poll() is not None)

        recovered = self.cli_json("recover_after_relaunch", "update", "recover",
                                  timeout=600)
        reconciliation = recovered.get("interrupted_handoff")
        case.check("offline_recover_leaves_deliberate_pause_alone",
                   isinstance(reconciliation, dict)
                   and reconciliation.get("state") == "nothing_to_do",
                   reconciliation=reconciliation)
        final = self.status("status_final")
        case.check("offline_status_matches_selected_generation",
                   (final.get("selector") or {}).get("generation")
                   == self.generation_b
                   and (final.get("selector") or {}).get("selector_revision") == launch_revision + 1
                   and (final.get("selector") or {}).get("active_epoch") == launch_epoch + 1,
                   selector=final.get("selector"))
        case.check("offline_status_reports_two_generations",
                   len(final.get("generations") or []) >= 2,
                   generations=final.get("generations"))
        case.note(relaunch=describe_runtime(running), final_status=final,
                  recover=recovered)

    def _refusals_live(self, facts_a: dict, facts_b: dict) -> None:
        """Synchronous refusals through the live control endpoint.

        None of these quiesces, freezes or spawns a helper: each is rejected
        before the product would leave its current selection. The retained-
        generation rebuild needs a stopped installation and runs later.
        """
        case = self.case("AB-08", "failure")
        before = self.status("status_before_refusals")
        selector_before = before.get("selector")
        self._cache["refusal_snapshot"] = {
            "selector": selector_before,
            "history": len(before.get("history") or []),
            "generations": list(before.get("generations") or []),
            "a_sha": file_sha256(Path(facts_a["slot_artifact_path"])),
            "b_sha": file_sha256(Path(facts_b["slot_artifact_path"])),
        }
        a_artifact = Path(facts_a["slot_artifact_path"])
        observations = []

        code, stdout, stderr = self.cli(
            "refuse_prepare_current", "update", "prepare",
            "--generation", self.generation_b, "--artifact", str(a_artifact),
            timeout=600)
        row = self.expect_refusal("prepare_current", code, stdout, stderr,
                                  ["already the current one"])
        observations.append(row)
        case.check("prepare_of_current_generation_refused",
                   row["refused"] and row["matched"], observation=row)

        code, stdout, stderr = self.cli(
            "refuse_stale_revision", "update", "activate",
            "--activation", facts_b["activation_id"],
            "--expected-selector-revision", str(facts_a["selector_revision"]), timeout=300)
        row = self.expect_refusal("stale_revision", code, stdout, stderr,
                                  ["SELECTOR_REVISION_CONFLICT"])
        observations.append(row)
        case.check("activation_with_stale_selector_revision_refused",
                   row["refused"] and row["matched"], observation=row)

        code, stdout, stderr = self.cli(
            "refuse_unknown_activation", "update", "activate",
            "--activation", str(uuid.uuid4()),
            "--expected-selector-revision", str(facts_b["selector_revision"]),
            timeout=300)
        row = self.expect_refusal("unknown_activation", code, stdout, stderr,
                                  ["activation missing"])
        observations.append(row)
        case.check("activation_with_unknown_identity_refused",
                   row["refused"] and row["matched"], observation=row)

        case.note(live_refusals=observations)

    def _refusals_retained(self, facts_a: dict, facts_b: dict) -> None:
        """A rebuild into a retained generation directory must be refused.

        The installation is stopped here, so a prebuilt artefact is the only
        input: the refusal comes from generation-directory admission rather
        than from a compiler failure or a stale mutable manifest.
        """
        case = self.cases["AB-08"]
        snapshot = self._cache["refusal_snapshot"]
        a_artifact = Path(facts_a["slot_artifact_path"])
        b_artifact = Path(facts_b["slot_artifact_path"])
        code, stdout, stderr = self.cli(
            "refuse_prepare_retained", "update", "prepare",
            "--generation", self.generation_a, "--artifact", str(a_artifact),
            timeout=900)
        row = self.expect_refusal("prepare_retained", code, stdout, stderr,
                                  ["already exists"])
        case.check("prepare_into_retained_generation_refused",
                   row["refused"] and row["matched"], observation=row)
        after = self.status("status_after_refusals")
        case.check("refusals_left_selector_unchanged",
                   after.get("selector") == snapshot["selector"],
                   before=snapshot["selector"], after=after.get("selector"))
        case.check("refusals_left_activation_history_unchanged",
                   len(after.get("history") or []) == snapshot["history"],
                   before=snapshot["history"],
                   after=len(after.get("history") or []))
        case.check("refusals_left_generation_inventory_unchanged",
                   sorted(after.get("generations") or [])
                   == sorted(snapshot["generations"]),
                   before=snapshot["generations"],
                   after=after.get("generations"))
        case.check("refusals_left_a_artifact_bytes_unchanged",
                   a_artifact.is_file()
                   and file_sha256(a_artifact) == snapshot["a_sha"])
        case.check("refusals_left_b_artifact_bytes_unchanged",
                   b_artifact.is_file()
                   and file_sha256(b_artifact) == snapshot["b_sha"])
        case.note(retained_refusal=row, state_after_refusals=after)

    def _assembly(self, facts_a: dict, facts_b: dict) -> None:
        surface_a = self._surface(facts_a)
        surface_b = self._surface(facts_b)
        binding_case = self.case("AB-06", "native")
        for name, passed in generation_surface_checks("a", surface_a).items():
            binding_case.check(name, passed, surface=surface_a)
        for name, passed in generation_surface_checks("b", surface_b).items():
            binding_case.check(name, passed, surface=surface_b)
        pair = binding_pair_checks(surface_a, surface_b)
        crossover = list(pair.pop("crossover_detail"))
        for name, passed in pair.items():
            binding_case.check(name, passed, surface_a=surface_a,
                               surface_b=surface_b)
        binding_case.note(crossover=crossover)

        retention = self.cases["AB-05"]
        retention.check("retired_a_launch_receipt_retained",
                        self._retained_launch(facts_a) is not None)
        retention.check("selected_b_launch_receipt_retained",
                        self._current_launch(facts_b) is not None)
        retention.check("activation_records_for_both_generations",
                        self._records_bind_generations(facts_a, facts_b))

        leak = self.case("AB-07", "native")
        tree = slot_tree_checks(
            facts_a["slot_dir"], facts_b["slot_dir"],
            b_artifact_path=Path(facts_b["slot_artifact_path"]),
            b_artifact_sha256=facts_b["slot_artifact_sha256"],
            recorded_artifact_sha256={
                value for value in (
                    self._cache["bootstrap"]["binary_sha256"],
                    facts_a.get("slot_artifact_sha256"),
                    facts_b.get("slot_artifact_sha256"),
                    facts_a.get("artifact_file_sha256"),
                    facts_b.get("artifact_file_sha256"),
                ) if value})
        for name, passed in tree["checks"].items():
            leak.check(name, passed, forbidden=tree["forbidden"],
                       staging=tree["staging"],
                       foreign_executables=tree["foreign_executables"],
                       hardlink_aliases=tree["hardlink_aliases"])
        leak.check("b_slot_manifest_artifact_inside_b_slot",
                   bool(facts_b["manifest_after_switch"])
                   and inside(facts_b["manifest_after_switch"]["artifact_path"],
                              facts_b["slot_dir"]))
        leak.check("b_route_artifact_inside_b_slot",
                   inside(facts_b["route"]["artifact_path"], facts_b["slot_dir"]))
        leak.check("b_selected_route_differs_from_a_slot_artifact",
                   os.path.realpath(facts_b["route"]["artifact_path"])
                   != os.path.realpath(facts_a["slot_artifact_path"]))
        leak.check("a_slot_artifact_bytes_retained",
                   Path(facts_a["slot_artifact_path"]).is_file()
                   and file_sha256(Path(facts_a["slot_artifact_path"]))
                   == facts_a["slot_artifact_sha256"])
        manifest_now = read_slot_manifest(facts_a["slot_dir"])
        leak.check("a_slot_manifest_generation_still_a",
                   (manifest_now or {}).get("generation") == self.generation_a
                   and (manifest_now or {}).get("artifact_digest")
                   == facts_a["artifact_digest"], manifest=manifest_now)
        retained = (self.data / RECOVERY_PROGRAMS
                    / str(facts_a.get("artifact_digest", "")).replace(":", "-")
                    / "shenko")
        leak.check("a_retained_recovery_program_matches_a_digest",
                   retained.is_file()
                   and file_sha256(retained) == facts_a["slot_artifact_sha256"],
                   path=str(retained))
        outputs = [(facts_a.get("output_layout") or {}).get("owned_directory"),
                   (facts_b.get("output_layout") or {}).get("owned_directory")]
        leak.check("build_output_directories_outside_both_slots",
                   all(path and not inside(path, facts_a["slot_dir"])
                       and not inside(path, facts_b["slot_dir"])
                       for path in outputs), outputs=outputs)
        leak.check("selected_selector_does_not_name_a",
                   (facts_b["selector"] or {}).get("generation")
                   == self.generation_b)
        leak.note(slot_carried_installation_entries=
                  tree["carried_installation_entries"],
                  executable_files=tree["executable_files"],
                  a_slot_manifest_now=manifest_now)

        binding = {
            "kind": BINDING_KIND,
            "schema_version": BINDING_SCHEMA,
            "evidence_id": self.evidence_id,
            "candidate_commit": self.candidate_commit,
            "source": {
                "path": "source/source.json",
                "observations": [facts_a.get("_source_state"),
                                 facts_b.get("_source_state")],
                "matched": bool((facts_a.get("_source_state") or {}).get("fingerprint")
                                and (facts_a.get("_source_state") or {}).get("fingerprint")
                                == (facts_b.get("_source_state") or {}).get("fingerprint")),
            },
            "generations": {
                "bootstrap": dict(self._cache["bootstrap"]),
                "a": self._generation_document(facts_a),
                "b": self._generation_document(facts_b),
            },
            "switch": {
                "acknowledgement": self._cache.get("switch_ack"),
                "retired_pid": self._cache.get("a_pid"),
                "selected_pid": facts_b.get("pid"),
                "retired_selector": (facts_a.get("retired_state") or {}).get("selectors"),
                "retired_head": head_of(facts_a.get("retired_state")),
                "launcher_pointer": read_pointer(self.data),
                "cross_generation": pair,
            },
            "checks": {
                **{("binding:%s" % name): value for name, value in pair.items()},
                **{("slot:%s" % name): value for name, value in tree["checks"].items()},
                **{("case:%s:%s" % (case.case_id, name)): value
                   for case in (binding_case, retention, leak)
                   for name, value in case.checks.items()},
            },
            "boundaries": self._boundaries(),
        }
        self.binding_document = binding
        seal_file(self.root, "binding/binding.json", binding)
        errors = validate_binding(binding)
        if errors:
            self.failures.extend("binding:%s" % error for error in errors)
            self.journal.emit("binding_invalid", errors=errors)
        self._write_receipt(facts_b)

    # -- assembly helpers -------------------------------------------------
    @staticmethod
    def _surface(facts: dict) -> dict:
        return {
            "generation": facts.get("generation"),
            "build_artifact_generation": facts.get("build_artifact_generation"),
            "build_observation_generation": facts.get("build_observation_generation"),
            "activation_to_generation": facts.get("activation_to_generation"),
            "slot_manifest_generation": facts.get("slot_manifest_generation"),
            "route_generation": facts.get("route_generation"),
            "launch_receipt_generation": facts.get("launch_receipt_generation"),
            "head_generation": facts.get("head_generation"),
            "selector_generation": facts.get("selector_generation"),
            "build_id": facts.get("build_id"),
            "slot_dir": facts.get("slot_dir"),
            "slot_artifact_path": facts.get("slot_artifact_path"),
            "manifest_artifact_path": facts.get("manifest_artifact_path"),
            "route_artifact_path": facts.get("route_artifact_path"),
            "selector_revision": facts.get("selector_revision"),
            "selector_epoch": facts.get("selector_epoch"),
            "slot_epoch": facts.get("slot_epoch"),
            "launch_receipt_revision": facts.get("launch_receipt_revision"),
            "launch_receipt_epoch": facts.get("launch_receipt_epoch"),
            "head_epoch": facts.get("head_epoch"),
            "artifact_path": facts.get("artifact_path"),
        }

    def _retained_launch(self, facts: dict):
        retired = facts.get("retired_state") or read_db(
            Path(facts["slot_dir"]) / SLOT_DB)
        return launch_by_revision(retired, facts["launch_receipt_revision"])

    def _current_launch(self, facts: dict):
        return launch_by_revision(read_db(Path(facts["slot_dir"]) / SLOT_DB),
                                  facts["launch_receipt_revision"])

    def _records_bind_generations(self, facts_a: dict, facts_b: dict) -> bool:
        state = read_db(Path(facts_b["slot_dir"]) / SLOT_DB)
        a = record_for(state, facts_a["activation_id"])
        b = record_for(state, facts_b["activation_id"])
        if not a or not b:
            return False
        return (a["data"].get("to_generation") == self.generation_a
                and a["data"].get("from_generation")
                == self._cache["bootstrap"]["generation"]
                and b["data"].get("to_generation") == self.generation_b
                and b["data"].get("from_generation") == self.generation_a)

    @staticmethod
    def _generation_document(facts: dict) -> dict:
        artifact = facts.get("artifact_record") or {}
        manifest = (facts.get("manifest_after_install")
                    or facts.get("manifest_after_switch")
                    or facts.get("slot_manifest"))
        return {
            "generation": facts.get("generation"),
            "build_id": facts.get("build_id"),
            "artifact": {
                "path": artifact.get("path"),
                "sha256": facts.get("artifact_file_sha256"),
                "blake3": artifact.get("digest"),
                "generation": artifact.get("generation"),
                "source_revision": artifact.get("source_revision"),
                "manifest_digest": artifact.get("manifest_digest"),
                "lockfile_digest": artifact.get("lockfile_digest"),
                "toolchain": artifact.get("toolchain"),
                "profile": artifact.get("profile"),
            },
            "slot": {
                "dir": facts.get("slot_dir"),
                "artifact_path": facts.get("slot_artifact_path"),
                "artifact_sha256": facts.get("slot_artifact_sha256"),
                "manifest": manifest,
                "ledger_id": (facts.get("db_state") or {}).get("ledger_id"),
            },
            "activation": facts.get("record"),
            "selector": facts.get("selector"),
            "launch_receipts": ([facts["launch_receipt"]]
                                if facts.get("launch_receipt") else []),
            "route": facts.get("route"),
            "build_observation": {
                "ref_digest": facts.get("observation_digest"),
                "closure_digest": facts.get("closure_digest"),
                "prepare_request_generation":
                    facts.get("build_observation_generation"),
                "outcome_digest":
                    ((facts.get("build_observation") or {}).get("build_outcome_ref")
                     or {}).get("digest"),
                "source_before": facts.get("source_before"),
                "source_after": facts.get("source_after"),
                "output_layout": facts.get("output_layout"),
            },
        }

    def _boundaries(self) -> list[str]:
        head_observed = bool(
            ((self._cache.get("a") or {}).get("artifact_record") or {})
            .get("source_revision"))
        boundaries = [
            ("no model-behaviour claim: the runtime used a bounded loopback "
             "transport fixture")
            if not self.provider_url else
            ("no model-behaviour claim: a caller-provided model binding was used "
             "but not evaluated by this harness"),
            ("no power-loss, filesystem-hostility, multi-node quorum or complete "
             "SPEC acceptance claim"),
            ("ignored files, environment variables outside the harness allowlist, "
             "compiler closure and network state are not frozen"),
            ("blake3 digests are read from the product's own sealed references and "
             "CAS addressing; the harness recomputes SHA-256 over the same bytes"),
            ("Store::record_activation_launch's in-process retry-with-different-"
             "binding refusal is not exercised; durable launch rows are observed"),
            ("exit-code-zero and JSON parseability are local facts only, not "
             "behaviour proof"),
        ]
        if not head_observed:
            boundaries.append(
                "the source checkout was dirty or unobservable during at least one "
                "build; matched inputs rest on the harness's tracked/untracked "
                "fingerprint, not on a clean HEAD")
        if not self.require_clean_source:
            boundaries.append("--require-clean-source was not set")
        return boundaries

    # -- receipt ----------------------------------------------------------
    def _write_receipt(self, facts_b: dict | None) -> None:
        if self.receipt_written:
            return
        self.receipt_written = True
        facts_b = facts_b or {}
        status = "passed" if not self.failures else "failed"
        self.finished_ms = int(time.time() * 1000)
        cases = []
        for case_id in sorted(self.cases):
            case = self.cases[case_id]
            if not case.checks:
                continue
            observation = seal_file(
                self.root, "cases/%s.%s.json" % (case.case_id, case.scope),
                {"schema_version": 1, "case_id": case.case_id,
                 "scope": case.scope, "checks": case.checks,
                 "checks_detail": case.detail, **case.notes})
            cases.append({
                "case_id": case.case_id,
                "status": "passed" if case.completed() else "failed",
                "observations": {case.scope: {
                    "status": "passed" if case.completed() else "failed",
                    "checks": dict(case.checks),
                    "output": observation,
                }},
            })
        source_document = {
            "schema_version": 1,
            "repo_root": str(self.source),
            "candidate_commit": self.candidate_commit,
            "binary": {"path": str(self.binary),
                       "sha256": file_sha256(self.binary)},
            "fingerprints": [
                ((self._cache.get("a") or {}).get("_source_state")),
                ((self._cache.get("b") or {}).get("_source_state")),
            ],
            "source_snapshot_frozen": False,
            "boundaries": [
                "git-tracked and non-ignored untracked regular files only",
                "ignored files, environment and the compiler closure are not frozen",
            ],
        }
        self.source_binding = seal_file(self.root, "source/source.json",
                                        source_document)
        artifact_path = Path(facts_b.get("slot_artifact_path")
                             or (self.root / "data"))
        self.artifact_binding = (
            binding_of(artifact_path, self.root) if artifact_path.is_file()
            else {"path": str(artifact_path), "sha256": ""})
        binding_path = self.root / "binding" / "binding.json"
        binding_binding = (binding_of(binding_path, self.root)
                           if binding_path.is_file() else None)
        configuration = {
            "profile": self.profile,
            "cargo_args": self.cargo_args,
            "generation_a": self.generation_a,
            "generation_b": self.generation_b,
            "source_dir": str(self.source),
            "model": ("caller-provided binding; not evaluated"
                      if self.provider_url else
                      "bounded loopback transport fixture; not a model claim"),
            "scripted_latency_seconds":
                None if self.provider_url else self.scripted_latency,
            "require_clean_source": self.require_clean_source,
            "environment_allowlist": sorted(self.env),
            "deadline_seconds": self.total_deadline,
        }
        environment = {
            "platform": sys.platform,
            "python": sys.version.split()[0],
            "host": os.uname().nodename if hasattr(os, "uname") else None,
            "cwd": str(Path.cwd()),
            "binary": {"path": str(self.binary),
                       "sha256": file_sha256(self.binary)},
        }
        facts_a = self._cache.get("a") or {}
        receipt = {
            "kind": RECEIPT_KIND,
            "schema_version": RECEIPT_SCHEMA,
            "evidence_id": self.evidence_id,
            "candidate_commit": self.candidate_commit or "UNBOUND",
            "status": status,
            "source": self.source_binding,
            "artifact": self.artifact_binding,
            "build": {
                "provider": "CargoBuildProvider through the product update service",
                "generation_a": {
                    "build_id": facts_a.get("build_id"),
                    "generation": self.generation_a,
                    "artifact_digest": facts_a.get("artifact_digest"),
                    "artifact_sha256": facts_a.get("artifact_file_sha256"),
                    "output_directory":
                        (facts_a.get("output_layout") or {}).get("owned_directory"),
                },
                "generation_b": {
                    "build_id": facts_b.get("build_id"),
                    "generation": self.generation_b,
                    "artifact_digest": facts_b.get("artifact_digest"),
                    "artifact_sha256": facts_b.get("artifact_file_sha256"),
                    "output_directory":
                        (facts_b.get("output_layout") or {}).get("owned_directory"),
                },
            },
            "configuration": configuration,
            "environment": environment,
            "boundaries": self._boundaries(),
            "run": {
                "started_unix_ms": self.started_ms,
                "finished_unix_ms": self.finished_ms,
                "exit_code": 0 if status == "passed" else 1,
                "status": status,
            },
            "cases": cases,
            "ab": {
                "kind": BINDING_KIND,
                "schema_version": BINDING_SCHEMA,
                "binding": binding_binding,
                "generation_a": self.generation_a,
                "generation_b": self.generation_b,
                "build_ids_distinct":
                    facts_a.get("build_id") != facts_b.get("build_id"),
                "matched_source": bool(
                    (facts_a.get("_source_state") or {}).get("fingerprint")
                    and (facts_a.get("_source_state") or {}).get("fingerprint")
                    == (facts_b.get("_source_state") or {}).get("fingerprint")),
                "failures": list(self.failures),
            },
        }
        errors = validate_receipt(receipt, self.root,
                                  require_passed=(status == "passed"))
        self.journal.emit("receipt_written", status=status, errors=errors,
                          failures=self.failures)
        if errors:
            receipt["ab"]["validation_errors"] = errors
        durable_json(self.root / "receipt.json", receipt)

    def _remember(self, key: str, value) -> None:
        self._cache[key] = value
        if key == "prepared_a":
            self.journal.emit("prepared_a", activation=value)
        elif key == "prepared_b":
            self.journal.emit("prepared_b", activation=value)
        elif key == "activated_a_ack":
            self.journal.emit("activated_a_ack", ack=value)
        elif key == "activated_b_ack":
            self.journal.emit("activated_b_ack", ack=value)


def describe_runtime(rows) -> list[dict]:
    if not rows:
        return []
    return [{key: row.get(key) for key in ("pid", "exe", "exe_sha256", "ppid")}
            for row in rows]


def run(binary: Path, source: Path, root: Path, options) -> int:
    root.mkdir(mode=0o700, parents=False, exist_ok=False)
    (root / "private-home").mkdir(mode=0o700)
    (root / "data").mkdir(mode=0o700)
    harness = Harness(binary, source, root, options=options)
    harness.journal.emit("start", binary=str(binary),
                         binary_sha256=file_sha256(binary),
                         source=str(source), root=str(root))
    try:
        harness.run()
    except HarnessStop as stop:
        harness.failures.append("stopped: %s" % stop)
        harness.journal.emit("stopped", reason=str(stop))
    except BaseException as error:  # noqa: BLE001 - evidence must survive
        harness.failures.append("error: %r" % (error,))
        harness.journal.emit("error", error=repr(error),
                             traceback=traceback.format_exc())
    finally:
        if harness.transport is not None:
            harness.transport.stop()
        for row in harness.processes:
            if row["process"].poll() is None:
                harness.stop(row, signal.SIGKILL, timeout=30)
        try:
            harness._write_receipt(harness._cache.get("b"))
        except BaseException as error:  # noqa: BLE001
            harness.journal.emit("receipt_failure", error=repr(error))
    return 0 if not harness.failures else 1


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", required=True, type=Path,
                        help="existing shenko executable used as the bootstrap generation")
    parser.add_argument("--source-dir", required=True, type=Path,
                        help="git worktree that both candidates are built from")
    parser.add_argument("--root", required=True, type=Path,
                        help="fresh evidence directory (must not exist)")
    parser.add_argument("--profile", default="debug", choices=("debug", "release"))
    parser.add_argument("--cargo-args", default="",
                        help="extra Cargo arguments forwarded to each build "
                             "(for example \"--offline --jobs 8\")")
    parser.add_argument("--generation-a", default="root-ab-a")
    parser.add_argument("--generation-b", default="root-ab-b")
    parser.add_argument("--deadline", type=float, default=7200.0,
                        help="total wall-clock budget in seconds")
    parser.add_argument("--build-timeout", type=float, default=2400.0,
                        help="per-prepare CLI timeout in seconds")
    parser.add_argument("--require-clean-source", action="store_true",
                        help="fail unless both builds observe a clean checkout HEAD")
    parser.add_argument("--provider-url", default=None,
                        help="use this OpenAI-compatible endpoint instead of the "
                             "scripted transport; still not a model-behaviour claim")
    parser.add_argument("--provider-model", default=None)
    parser.add_argument("--scripted-latency", type=float, default=0.25)
    parser.add_argument("--env", action="append", default=[], metavar="NAME",
                        help="forward the named variable from the caller's environment")
    options = parser.parse_args(argv)
    binary = options.binary.resolve()
    source = options.source_dir.resolve()
    root = options.root.resolve()
    missing = [name for name in options.env if name not in os.environ]
    if missing:
        print(json.dumps({"error": "forwarded environment variables are absent",
                          "names": missing}))
        return 3
    if not binary.is_file() or not os.access(binary, os.X_OK):
        print(json.dumps({"error": "binary is not an executable file",
                          "binary": str(binary)}))
        return 3
    if not (source / ".git").exists():
        print(json.dumps({"error": "source-dir is not a git worktree",
                          "source": str(source)}))
        return 3
    if root.exists():
        print(json.dumps({"error": "root already exists; use a fresh evidence "
                                   "directory", "root": str(root)}))
        return 3
    if root == source or inside(root, source):
        print(json.dumps({
            "error": "root must be outside the measured source tree; evidence "
                     "inside it would change the matched-source fingerprint",
            "root": str(root), "source": str(source)}))
        return 3
    code = run(binary, source, root, options)
    print(json.dumps({"verdict": "passed" if code == 0 else "failed",
                      "root": str(root), "exit_code": code}))
    return code


if __name__ == "__main__":
    sys.exit(main())
