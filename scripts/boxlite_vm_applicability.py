#!/usr/bin/env python3
"""Decide whether the optional BoxLite Linux-VM adapter applies to this tree.

SPEC `docs/specs/desktop-runtime.md` §34.1 lists BoxLite only as a research
candidate ("尚未决定必须采用，也未开始开发"), and §34.7 / acceptance case A29
are executed **only if** a BoxLite adapter is adopted. This harness answers
one question with evidence instead of assumption:

    does this tree, on this host, actually hold the prerequisites to run the
    shippedVM seam with the BoxLite provider?

The shipped seam is provider-agnostic: `crates/shenko-exec-host/src/layerfs.rs`
drives `scripts/layerfs-companion.mjs`, which resolves a provider module at
runtime and reports typed-unavailable codes when anything is absent. This
harness never installs, downloads, vendors or fabricates a provider; it reads
the current tree, reports which prerequisite is missing (with its exact code),
and only starts a VM through the real companion when `--drive` is given
explicitly.

Exit codes
    0  a determination was recorded: `not_applicable`,
       `applicable_not_driven` or `applicable_verified`
    3  review required: the tree drifted from the documented seam, an
       undocumented BoxLite integration appeared, an environment override
       changes which provider is being judged, or a driven run really failed
    2  the harness itself could not determine or record anything
"""

import argparse
import collections
import datetime
import hashlib
import json
import os
import platform
import queue
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

SCHEMA_VERSION = 1

# Readiness states and lifecycle entry points the adapter ships today. Each
# anchor is a literal the harness re-finds; a miss is tree drift, never a pass.
SEAM_ANCHORS = (
    (
        "crates/shenko-exec-host/src/layerfs.rs",
        (
            'pub const LAYERFS_SDK_PIN: &str = "v1.0.0@',
            '.unwrap_or_else(|_| "boxlite".into())',
            "pub fn start_vm(&self",
            "pub fn stop_vm(&self",
            "pub fn vm_state(&self",
            "pub fn commit_receipt(&self",
            "pub fn capabilities(&self",
            '"layerfs_sdk_path_unset"',
            '"sdk_path_missing"',
            '"companion_script_missing"',
            '"configured_vm_not_started"',
        ),
    ),
    (
        "scripts/layerfs-companion.mjs",
        (
            "boxlite: ['boxlite', '@boxlite/sdk']",
            "'vm_provider_not_installed'",
            "'layerfs_sdk_path_unset'",
            "'layerfs_sdk_entry_missing'",
            "'layerfs_sdk_surface_mismatch'",
        ),
    ),
    (
        "bin/shenko/src/runtime_sessions.rs",
        ("ComputerUseVmHandler::new",),
    ),
    (
        "crates/shenko-exec-host/src/computer_use.rs",
        ("const VM_PROMPT", "computer_use.layerfs.vm/v1"),
    ),
)

# Every first-party reference to BoxLite that this tree documents. A hit in
# any other first-party file is an undocumented integration and needs review
# before the applicability statement can stand.
DOCUMENTED_REFERENCES = {
    "crates/shenko-exec-host/src/layerfs.rs":
        "provider seam: optional first VM provider id, lifecycle, readiness",
    "scripts/layerfs-companion.mjs":
        "provider module resolution (`boxlite` | `@boxlite/sdk`) and typed errors",
    "crates/shenko-exec-host/src/computer_use.rs":
        "computer.use.vm prompt naming the optional provider",
    "scripts/boxlite_vm_applicability.py":
        "this applicability harness",
    "crates/shenko-wasm/src/artifact.rs":
        "doc comment analogy only ('BoxLite-backed store'); no integration",
}

SCAN_ROOTS = ("crates", "bin", "mobile", "scripts", "adapters", "formal")
SCAN_SKIP_DIRS = {"target", "node_modules", ".git", "dist", "build", "out", "artifacts"}
TEXT_SUFFIXES = {
    ".rs", ".mjs", ".cjs", ".js", ".ts", ".tsx", ".mts", ".cts",
    ".py", ".toml", ".json", ".sh", ".yml", ".yaml",
}
MAX_SCAN_BYTES = 1 << 20
MAX_SCAN_FILES = 6000

PROVIDER_MODULES = ("boxlite", "@boxlite/sdk")

# Resolve from the companion's own directory, mirroring its bare-specifier
# import. Resolution only: no module is imported, so no provider code runs.
PROVIDER_PROBE_SOURCE = r"""
const path = require('node:path');
const { createRequire } = require('node:module');
const scripts = process.env.SHENKO_PROBE_SCRIPT_DIR;
const req = createRequire(path.join(scripts, 'layerfs-companion.mjs'));
const out = {};
for (const name of ['boxlite', '@boxlite/sdk']) {
  try {
    out[name] = { resolved: req.resolve(name) };
  } catch (error) {
    out[name] = { error: (error && error.code) || 'unknown' };
  }
}
process.stdout.write(JSON.stringify(out));
"""


def now_iso():
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds")


def sha256_file(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_text_limited(path, limit=MAX_SCAN_BYTES):
    raw = Path(path).read_bytes()[:limit]
    if b"\0" in raw:
        return None
    return raw.decode("utf-8", errors="replace")


def run_bounded(argv, timeout, env=None, cwd=None):
    """Run a short owned command with its own process group and a hard bound."""
    try:
        process = subprocess.Popen(
            argv,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=env,
            cwd=cwd,
            start_new_session=True,
        )
    except OSError as error:
        return {"argv": argv, "spawn_error": f"{type(error).__name__}: {error}"}
    try:
        stdout, stderr = process.communicate(timeout=timeout)
        return {
            "argv": argv,
            "exit_code": process.returncode,
            "stdout": stdout[-8192:],
            "stderr": stderr[-8192:],
        }
    except subprocess.TimeoutExpired:
        try:
            os.killpg(os.getpgid(process.pid), signal.SIGKILL)
        except OSError:
            pass
        stdout, stderr = process.communicate()
        return {
            "argv": argv,
            "timeout_seconds": timeout,
            "stdout": stdout[-8192:],
            "stderr": stderr[-8192:],
        }


# ---------------------------------------------------------------------------
# Tree facts
# ---------------------------------------------------------------------------


def parse_sdk_pin(layerfs_text):
    match = re.search(r'LAYERFS_SDK_PIN: &str = "(v1\.0\.0@[0-9a-f]+)"', layerfs_text)
    return match.group(1) if match else None


def scan_integration(repo):
    """Find every first-party BoxLite mention and classify it.

    Classification is documentation, not approval: an unlisted path means the
    applicability statement must be re-derived, so it is reported as review
    material instead of being silently blessed.
    """
    hits = []
    scanned = 0
    truncated = False
    for root in SCAN_ROOTS:
        base = repo / root
        if not base.is_dir():
            continue
        for directory, subdirs, files in os.walk(base):
            subdirs[:] = sorted(name for name in subdirs if name not in SCAN_SKIP_DIRS)
            for name in sorted(files):
                path = Path(directory) / name
                if path.suffix not in TEXT_SUFFIXES:
                    continue
                if scanned >= MAX_SCAN_FILES:
                    truncated = True
                    break
                try:
                    text = read_text_limited(path)
                except OSError as error:
                    hits.append({"path": str(path.relative_to(repo)), "error": str(error)})
                    continue
                if text is None:
                    continue
                scanned += 1
                if "boxlite" in text.lower():
                    relative = str(path.relative_to(repo))
                    lines = [
                        index
                        for index, line in enumerate(text.splitlines(), start=1)
                        if "boxlite" in line.lower()
                    ]
                    hits.append({
                        "path": relative,
                        "lines": lines[:16],
                        "line_count": len(lines),
                        "documented_as": DOCUMENTED_REFERENCES.get(relative),
                    })
            if truncated:
                break
        if truncated:
            break

    unexpected = [
        hit["path"]
        for hit in hits
        if "documented_as" in hit and hit["documented_as"] is None
    ]

    dependency_hits = []
    for manifest in ("Cargo.toml", "Cargo.lock"):
        path = repo / manifest
        if not path.is_file():
            continue
        text = read_text_limited(path) or ""
        if "boxlite" in text.lower():
            dependency_hits.append(manifest)

    named_paths = []
    for entry in sorted(repo.iterdir()):
        if entry.name in {"docs", "build", "target", ".git", "artifacts", "source-packets"}:
            continue
        if "boxlite" in entry.name.lower():
            named_paths.append(entry.name)

    return {
        "scan_roots": list(SCAN_ROOTS),
        "files_scanned": scanned,
        "scan_truncated": truncated,
        "hits": hits,
        "unexpected_integration": unexpected,
        "dependency_manifests_with_boxlite": dependency_hits,
        "top_level_paths_named_boxlite": named_paths,
    }


def probe_tree(repo):
    facts = {"repo": str(repo), "anchors": [], "drift": [], "files": {}}
    head = run_bounded(["git", "-C", str(repo), "rev-parse", "HEAD"], 30)
    facts["head"] = (head.get("stdout") or "").strip() or None
    if head.get("exit_code") != 0:
        facts["head_error"] = head.get("stderr", "").strip() or head.get("spawn_error")

    for relative, anchors in SEAM_ANCHORS:
        path = repo / relative
        if not path.is_file():
            facts["drift"].append({"path": relative, "missing_file": True})
            facts["anchors"].append({"path": relative, "missing_file": True})
            continue
        text = read_text_limited(path) or ""
        missing = [anchor for anchor in anchors if anchor not in text]
        facts["files"][relative] = sha256_file(path)
        facts["anchors"].append({
            "path": relative,
            "sha256": facts["files"][relative],
            "anchors_checked": len(anchors),
            "missing_anchors": missing,
        })
        if missing:
            facts["drift"].append({"path": relative, "missing_anchors": missing})

    layerfs = facts["files"].get("crates/shenko-exec-host/src/layerfs.rs")
    if layerfs:
        facts["layerfs_sha256"] = layerfs
    layerfs_text = read_text_limited(repo / "crates/shenko-exec-host/src/layerfs.rs") or ""
    facts["sdk_pin"] = parse_sdk_pin(layerfs_text)
    if facts["sdk_pin"] is None:
        facts["drift"].append({"path": "crates/shenko-exec-host/src/layerfs.rs",
                               "missing_anchors": ["LAYERFS_SDK_PIN value"]})

    facts["integration"] = scan_integration(repo)
    return facts


# ---------------------------------------------------------------------------
# Host facts
# ---------------------------------------------------------------------------


def probe_kvm():
    path = Path("/dev/kvm")
    facts = {"path": str(path), "exists": path.exists(), "open_read_write": None, "error": None}
    if not facts["exists"]:
        return facts
    try:
        descriptor = os.open(path, os.O_RDWR)
        os.close(descriptor)
        facts["open_read_write"] = True
    except OSError as error:
        facts["open_read_write"] = False
        facts["error"] = f"{error.strerror} (errno {error.errno})"
    return facts


def probe_provider_modules(node, scripts_dir):
    env = dict(os.environ)
    env["SHENKO_PROBE_SCRIPT_DIR"] = str(scripts_dir)
    result = run_bounded([node, "-e", PROVIDER_PROBE_SOURCE], 60, env=env)
    parsed = None
    stdout = (result.get("stdout") or "").strip()
    if stdout:
        try:
            parsed = json.loads(stdout)
        except ValueError:
            parsed = None
    resolved = {}
    for name in PROVIDER_MODULES:
        entry = (parsed or {}).get(name) or {}
        resolved[name] = {
            "resolved": entry.get("resolved"),
            "error": entry.get("error"),
        }
    return {
        "probe": result.get("argv"),
        "exit_code": result.get("exit_code"),
        "stderr": (result.get("stderr") or "").strip()[-2048:],
        "modules": resolved,
    }


def probe_host(repo):
    facts = {"platform": platform.platform(), "machine": platform.machine()}

    node = os.environ.get("SHENKO_LAYERFS_NODE") or shutil.which("node")
    node_facts = {"path": node, "version": None, "ok": bool(node)}
    if node:
        version = run_bounded([node, "--version"], 30)
        node_facts["version"] = (version.get("stdout") or "").strip() or None
        node_facts["ok"] = version.get("exit_code") == 0
    facts["node"] = node_facts

    sdk_env = os.environ.get("SHENKO_LAYERFS_SDK_PATH") or ""
    entry = Path(sdk_env) / "sdk" / "js" / "dist" / "index.js" if sdk_env else None
    package = Path(sdk_env) / "sdk" / "js" / "package.json" if sdk_env else None
    sdk_facts = {
        "env_name": "SHENKO_LAYERFS_SDK_PATH",
        "configured": bool(sdk_env),
        "path": sdk_env or None,
        "is_directory": bool(sdk_env) and Path(sdk_env).is_dir(),
        "companion_entry": str(entry) if entry else None,
        "companion_entry_exists": bool(entry) and entry.is_file(),
        "companion_entry_sha256": sha256_file(entry) if entry and entry.is_file() else None,
        "package_json_exists": bool(package) and package.is_file(),
        "package_version": None,
    }
    if sdk_facts["package_json_exists"]:
        try:
            document = json.loads(package.read_text())
            version = document.get("version")
            sdk_facts["package_version"] = version if isinstance(version, str) else None
        except (OSError, ValueError):
            sdk_facts["package_version"] = None
    facts["pinned_sdk"] = sdk_facts

    facts["kvm"] = probe_kvm()
    facts["provider_override"] = os.environ.get("SHENKO_LAYERFS_VM_PROVIDER")
    facts["provider_probe"] = (
        probe_provider_modules(node, repo / "scripts") if node_facts["ok"] else None
    )

    modules = (facts["provider_probe"] or {}).get("modules") or {}
    provider_ok = any(entry.get("resolved") for entry in modules.values())
    facts["provider_installed"] = provider_ok

    facts["prerequisites"] = [
        {
            "code": "node_binary_unavailable",
            "ok": node_facts["ok"],
            "detail": node_facts["path"],
        },
        {
            "code": "layerfs_sdk_path_unset",
            "ok": sdk_facts["configured"],
            "detail": sdk_facts["path"],
        },
        {
            "code": "layerfs_sdk_path_missing",
            "ok": (not sdk_facts["configured"]) or sdk_facts["is_directory"],
            "detail": sdk_facts["path"],
        },
        {
            "code": "layerfs_sdk_entry_missing",
            "ok": (not sdk_facts["is_directory"]) or sdk_facts["companion_entry_exists"],
            "detail": sdk_facts["companion_entry"],
        },
        {
            "code": "vm_provider_not_installed",
            "ok": provider_ok,
            "detail": {name: modules.get(name) for name in PROVIDER_MODULES},
        },
    ]
    return facts


# ---------------------------------------------------------------------------
# Companion protocol (the shipped seam; the only thing that starts a VM)
# ---------------------------------------------------------------------------


class Companion:
    def __init__(self, node, script, env, stderr_limit=80):
        self.events = []
        self.stderr = collections.deque(maxlen=stderr_limit)
        self.fatal = None
        self.closed = False
        self.replies = queue.Queue()
        self.process = subprocess.Popen(
            [node, script],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=env,
            start_new_session=True,
        )
        threading.Thread(target=self._pump_stdout, daemon=True).start()
        threading.Thread(target=self._pump_stderr, daemon=True).start()

    def _pump_stdout(self):
        try:
            for line in self.process.stdout:
                self.replies.put(line)
        finally:
            self.replies.put(None)

    def _pump_stderr(self):
        try:
            for line in self.process.stderr:
                self.stderr.append(line.rstrip("\n"))
        except (OSError, ValueError):
            pass

    def fatal_detail(self):
        if isinstance(self.fatal, dict):
            return json.dumps(self.fatal, ensure_ascii=False, sort_keys=True)
        return None

    def _take(self, timeout):
        try:
            line = self.replies.get(timeout=timeout)
        except queue.Empty:
            raise TimeoutError(f"no companion reply within {timeout:.0f}s")
        if line is None:
            raise EOFError(
                "companion stdout closed: "
                + (self.fatal_detail() or "no fatal event reported")
            )
        return line

    def request(self, seq, cmd, args, timeout):
        payload = json.dumps({"seq": seq, "cmd": cmd, "args": args}, ensure_ascii=False)
        try:
            self.process.stdin.write(payload + "\n")
            self.process.stdin.flush()
        except (BrokenPipeError, OSError) as error:
            raise EOFError(f"companion stdin closed: {error}")
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"no reply to {cmd!r} within {timeout:.0f}s")
            raw = self._take(remaining)
            try:
                value = json.loads(raw)
            except ValueError:
                self.events.append({"event": "unparseable_line", "raw": raw[:1024]})
                continue
            if not isinstance(value, dict):
                self.events.append({"event": "unexpected_line", "raw": raw[:1024]})
                continue
            if "event" in value:
                self.events.append(value)
                if value.get("event") == "fatal":
                    self.fatal = value.get("detail")
                continue
            if value.get("seq") == seq:
                return value
            # A late answer to a request this harness never sent is retained,
            # never read as the current request's result.
            self.events.append({"event": "out_of_order_reply", "seq": value.get("seq")})

    def close(self):
        if self.closed:
            return
        self.closed = True
        try:
            if self.process.poll() is None:
                try:
                    os.killpg(os.getpgid(self.process.pid), signal.SIGTERM)
                except OSError:
                    pass
                try:
                    self.process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(os.getpgid(self.process.pid), signal.SIGKILL)
                    except OSError:
                        pass
                    try:
                        self.process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        pass
        finally:
            for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
                try:
                    stream.close()
                except (OSError, ValueError):
                    pass


def full_workspace_label(guest):
    """Mirror the adapter's honest guest label; never upgrade an unprobed fact."""
    if not guest.get("exec"):
        return "missing_guest_exec"
    if guest.get("docker") and guest.get("fuse") and guest.get("mountsObserved"):
        return "probed_docker_fuse_mounts"
    return "incomplete"


def drive(node, script, name, timeouts, commit_receipt):
    env = dict(os.environ)
    env["SHENKO_LAYERFS_VM_PROVIDER"] = "boxlite"
    env["SHENKO_LAYERFS_VM_NAME"] = name
    plan = [("start", timeouts["start"], False)]
    plan.append(("state", timeouts["state"], False))
    if commit_receipt:
        plan.append(("commit_receipt", timeouts["receipt"], False))
    plan.append(("stop", timeouts["stop"], True))

    record = {"vm_name": name, "requests": [], "events": [], "stderr": [], "fatal": None}
    companion = None
    try:
        companion = Companion(node, script, env)
        for seq, (cmd, timeout, _) in enumerate(plan, start=1):
            try:
                reply = companion.request(seq, cmd, {}, timeout)
            except (TimeoutError, EOFError) as error:
                record["requests"].append({
                    "seq": seq,
                    "cmd": cmd,
                    "error": type(error).__name__,
                    "detail": str(error),
                })
                break
            record["requests"].append({"seq": seq, "cmd": cmd, "reply": reply})
    finally:
        if companion is not None:
            record["events"] = list(companion.events)
            record["stderr"] = list(companion.stderr)
            record["fatal"] = companion.fatal
            companion.close()
    return record


def assess_drive(record, pinned_sdk):
    checks = []

    def add(name, ok, detail=None):
        checks.append({"check": name, "ok": bool(ok), "detail": detail})

    by_command = {}
    for item in record["requests"]:
        by_command.setdefault(item["cmd"], item)

    start = by_command.get("start") or {}
    start_reply = start.get("reply") or {}
    start_body = start_reply.get("detail") if isinstance(start_reply.get("detail"), dict) else {}
    add("start_request_answered", start_reply.get("ok") is True,
        start.get("error") or start_reply.get("error"))
    add("start_reports_state",
        isinstance(start_body.get("state"), str) and bool(start_body.get("state")),
        start_body.get("state"))
    add("start_does_not_claim_upstream_commit",
        start_body.get("layerfsCommitted") is False,
        start_body.get("layerfsCommitted"))
    sdk = start_body.get("sdk") if isinstance(start_body.get("sdk"), dict) else {}
    exports = sdk.get("exports") if isinstance(sdk.get("exports"), list) else []
    add("start_binds_pinned_sdk_surface",
        bool(pinned_sdk) and sdk.get("pin") == pinned_sdk and "Store" in exports,
        {"expected_pin": pinned_sdk, "pin": sdk.get("pin"), "exports": exports})
    guest = start_body.get("guest")
    add("start_surfaces_guest_probe", isinstance(guest, dict),
        "probed" if isinstance(guest, dict) else guest)
    if isinstance(guest, dict):
        label = full_workspace_label(guest)
        add("guest_label_never_upgrades_unprobed",
            label in {"missing_guest_exec", "incomplete", "probed_docker_fuse_mounts"},
            label)

    state = by_command.get("state") or {}
    state_reply = state.get("reply") or {}
    state_body = state_reply.get("detail") if isinstance(state_reply.get("detail"), dict) else {}
    add("state_request_answered", state_reply.get("ok") is True,
        state.get("error") or state_reply.get("error"))
    add("state_does_not_claim_upstream_commit",
        state_body.get("layerfsCommitted") is False,
        state_body.get("layerfsCommitted"))

    receipt = by_command.get("commit_receipt")
    if receipt is not None:
        receipt_reply = receipt.get("reply") or {}
        receipt_body = (receipt_reply.get("detail")
                        if isinstance(receipt_reply.get("detail"), dict) else {})
        add("receipt_request_answered", receipt_reply.get("ok") is True,
            receipt.get("error") or receipt_reply.get("error"))
        # The upstream SDK receipt is `layerfs_committed`; nothing in this
        # protocol may upgrade it to this product's own durable publication.
        claims_product_durability = any(
            "product_durable" in str(key).lower() for key in receipt_body
        )
        add("receipt_is_upstream_only",
            not claims_product_durability and receipt_body.get("layerfsCommitted") is True,
            {"layerfsCommitted": receipt_body.get("layerfsCommitted"),
             "claims_product_durable": claims_product_durability})

    stop = by_command.get("stop") or {}
    stop_reply = stop.get("reply") or {}
    stop_body = stop_reply.get("detail") if isinstance(stop_reply.get("detail"), dict) else {}
    add("stop_request_answered", stop_reply.get("ok") is True,
        stop.get("error") or stop_reply.get("error"))
    add("stop_reports_stopped", stop_body.get("state") == "stopped", stop_body.get("state"))

    return checks


# ---------------------------------------------------------------------------
# Determination
# ---------------------------------------------------------------------------


def determine(tree, host, drive_result):
    reasons = []

    if tree["drift"]:
        return "review_required", [{"code": "tree_seam_drift", "detail": tree["drift"]}]
    if tree["integration"]["unexpected_integration"]:
        return "review_required", [{
            "code": "undocumented_boxlite_integration",
            "detail": tree["integration"]["unexpected_integration"],
        }]
    if tree["integration"]["dependency_manifests_with_boxlite"]:
        return "review_required", [{
            "code": "boxlite_dependency_in_manifests",
            "detail": tree["integration"]["dependency_manifests_with_boxlite"],
        }]
    override = host.get("provider_override")
    if override and override != "boxlite":
        return "review_required", [{
            "code": "provider_override",
            "detail": f"SHENKO_LAYERFS_VM_PROVIDER={override!r} judges another provider",
        }]

    missing = [item for item in host["prerequisites"] if not item["ok"]]
    for item in missing:
        reasons.append({"code": item["code"], "detail": item["detail"]})
    if missing:
        return "not_applicable", reasons

    if drive_result is None:
        return "applicable_not_driven", [{
            "code": "drive_not_requested",
            "detail": "prerequisites are present; pass --drive to exercise the real seam",
        }]

    failing = [check for check in drive_result["checks"] if not check["ok"]]
    if failing:
        reasons.append({"code": "drive_check_failed", "detail": failing})
        if drive_result.get("fatal"):
            reasons.append({"code": "companion_fatal", "detail": drive_result["fatal"]})
        return "applicable_run_failed", reasons
    return "applicable_verified", [{
        "code": "drive_checks_passed",
        "detail": [check["check"] for check in drive_result["checks"]],
    }]


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", type=Path,
                        default=Path(__file__).resolve().parents[1],
                        help="repository root (default: this script's parent)")
    parser.add_argument("--evidence-dir", type=Path, required=True,
                        help="directory that receives applicability.json and events.jsonl")
    parser.add_argument("--node", default=os.environ.get("SHENKO_LAYERFS_NODE"),
                        help="node binary (default: SHENKO_LAYERFS_NODE or PATH)")
    parser.add_argument("--vm-name", default="shenko-boxlite-applicability",
                        help="VM name the shipped companion would own")
    parser.add_argument("--drive", action="store_true",
                        help="start/stop a real VM through the shipped companion")
    parser.add_argument("--commit-receipt", action="store_true",
                        help="also ask the guest SDK for its upstream commit receipt "
                             "(only valid with --drive; executes SDK code in the guest "
                             "with effects this harness does not verify)")
    parser.add_argument("--start-timeout", type=float, default=300.0)
    parser.add_argument("--state-timeout", type=float, default=30.0)
    parser.add_argument("--stop-timeout", type=float, default=120.0)
    parser.add_argument("--receipt-timeout", type=float, default=60.0)
    parser.add_argument("--force", action="store_true",
                        help="overwrite an existing applicability.json")
    return parser.parse_args(argv)


def main(argv=None):
    args = parse_args(sys.argv[1:] if argv is None else argv)
    repo = args.repo.resolve()
    steps = []

    def step(name, **detail):
        steps.append({"at": now_iso(), "step": name, **detail})

    step("start", repo=str(repo))
    tree = probe_tree(repo)
    step("tree_probed", drift=len(tree["drift"]),
         hits=len(tree["integration"]["hits"]))
    host = probe_host(repo)
    if args.node:
        host["node"]["path"] = args.node
        host["node"]["ok"] = Path(args.node).exists()
    step("host_probed", provider_installed=host["provider_installed"],
         kvm_read_write=host["kvm"]["open_read_write"])

    drive_result = None
    if args.drive:
        reasons = []
        if tree["drift"]:
            reasons.append("tree seam drifted")
        missing = [item for item in host["prerequisites"] if not item["ok"]]
        if missing:
            reasons.append("prerequisites missing: " +
                           ", ".join(item["code"] for item in missing))
        if reasons:
            step("drive_refused", reasons=reasons)
        else:
            step("drive_started", vm_name=args.vm_name,
                 commit_receipt=bool(args.commit_receipt))
            record = drive(
                host["node"]["path"],
                str(repo / "scripts" / "layerfs-companion.mjs"),
                args.vm_name,
                {
                    "start": args.start_timeout,
                    "state": args.state_timeout,
                    "stop": args.stop_timeout,
                    "receipt": args.receipt_timeout,
                },
                args.commit_receipt,
            )
            record["checks"] = assess_drive(record, tree["sdk_pin"])
            record["scrubbing_note"] = (
                "companion stderr/events are retained as observed; the harness "
                "records no environment values except SHENKO_LAYERFS_* identities"
            )
            drive_result = record
            step("drive_finished", checks=len(record["checks"]),
                 failing=sum(1 for check in record["checks"] if not check["ok"]))

    if args.commit_receipt and not args.drive:
        step("commit_receipt_ignored", reason="--commit-receipt requires --drive")

    determination, reasons = determine(tree, host, drive_result)
    step("determined", determination=determination,
         reasons=[reason["code"] for reason in reasons])

    evidence = {
        "schema_version": SCHEMA_VERSION,
        "harness": {
            "path": "scripts/boxlite_vm_applicability.py",
            "sha256": sha256_file(Path(__file__)),
        },
        "generated_at": now_iso(),
        "spec": {
            "conditional_acceptance": "docs/specs/desktop-runtime.md#spec-34-7",
            "research_only": "docs/specs/desktop-runtime.md#spec-34-1",
            "case": "A29 in docs/specs/verification.md#spec-23",
            "provider_agnostic_requirement": "docs/specs/durable-state.md#spec-10-2-1",
        },
        "candidate": {
            "repo": str(repo),
            "head": tree.get("head"),
            "head_error": tree.get("head_error"),
        },
        "determination": determination,
        "reasons": reasons,
        "sdk_pin": tree.get("sdk_pin"),
        "tree": {
            "anchors": tree["anchors"],
            "drift": tree["drift"],
            "integration": tree["integration"],
        },
        "host": host,
        "drive": drive_result,
        "claims": [
            "the shipped provider seam and its typed-unavailable codes still exist "
            "at the recorded hashes",
            "only the recorded prerequisites were observed on this host",
            "when --drive ran, the companion's own replies are the only VM facts used",
        ],
        "non_claims": [
            "this determination does not decide whether to adopt BoxLite",
            "not_applicable means this host lacks the named prerequisites; it is not "
            "evidence that BoxLite is unusable elsewhere",
            "no §34.7 scenario (GUI, clone isolation, network paths, cancellation, "
            "broker, archive/secret handling) is verified by this harness",
            "applicable_verified covers lifecycle/guest-probe honesty only, never "
            "product durability",
        ],
        "steps": steps,
    }

    directory = args.evidence_dir
    target = directory / "applicability.json"
    if target.exists() and not args.force:
        print(f"refusing to overwrite {target}; pass --force", file=sys.stderr)
        return 2
    try:
        directory.mkdir(parents=True, exist_ok=True)
        target.write_text(json.dumps(evidence, ensure_ascii=False, indent=2) + "\n")
        events = directory / "events.jsonl"
        events.write_text("".join(
            json.dumps(step, ensure_ascii=False) + "\n" for step in steps
        ))
    except OSError as error:
        print(f"could not record evidence in {directory}: {error}", file=sys.stderr)
        return 2

    print(json.dumps({
        "determination": determination,
        "reasons": [reason["code"] for reason in reasons],
        "sdk_pin": tree.get("sdk_pin"),
        "evidence": str(target),
    }, ensure_ascii=False, indent=2))
    return 0 if determination in {
        "not_applicable", "applicable_not_driven", "applicable_verified",
    } else 3


if __name__ == "__main__":
    raise SystemExit(main())
