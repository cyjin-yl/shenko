#!/usr/bin/env python3
"""Run an explicit, bounded real-model single-node journey.

Requires an actual OpenAI-compatible model service, Python 3.11+ and websockets 16.
Starts the real Shenko CLI, submits over its public WebSocket, inspects durable
child/E1/decision facts read-only, restarts the same installation, replays the
same input identity, and asks the model to continue from its prior context.
With --browser, both new inputs come from the actual Web UI composer in the
same installation; the observing socket still replays the original identity.
No model response, task, receipt, or successful outcome is injected. Failure
retains the same evidence and exits nonzero; this is not a default CI test.
"""
import argparse
import asyncio
import copy
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import time
import traceback
import urllib.request
import uuid

def save(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def get_json(url, timeout=5):
    # The model and UI are explicit endpoints; local evidence must not depend
    # on a shell's HTTP proxy rewriting loopback requests.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(url, timeout=timeout) as response:
        return json.load(response)


def observer_websocket_options(timeout):
    # This socket observes the real browser/runtime; its library's default
    # 20-second pong deadline must not silently replace the explicit phase
    # observation limit. Browser frames and Store timings retain real stalls.
    return {"proxy": None, "max_size": 16 * 1024 * 1024,
            "ping_interval": 20, "ping_timeout": timeout}


def read_cas(data_dir, reference):
    digest = reference["digest"].removeprefix("blake3:")
    path = data_dir / "cas" / digest[:2] / digest[2:4] / digest
    raw = path.read_bytes()
    if len(raw) != reference["bytes"]:
        raise ValueError("CAS length differs from the durable reference")
    return json.loads(raw)


TABLES = (
    "subject_branch_heads", "subject_steps", "subject_genesis", "tasks",
    "task_results", "operations", "attempts", "evidence_receipts",
    "web_input_submissions", "subject_intents", "session_events", "inbox_events",
    "subject_request_attempts", "content_streams", "content_stream_units",
    "subject_quiesce_requests", "context_windows", "context_window_scopes",
    "commit_log",
)


def snapshot(data_dir):
    connection = sqlite3.connect(f"file:{data_dir / 'shenko.db'}?mode=ro", uri=True)
    connection.row_factory = sqlite3.Row
    result = {}
    try:
        # All tables describe one durable point while the real runtime keeps
        # advancing. A sequence of independent SELECT snapshots can combine
        # facts that were never present together.
        connection.execute("BEGIN")
        tables = {row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        for table in TABLES:
            if table not in tables:
                continue
            rows = []
            # Window bodies and their call-state baselines are SQLite facts,
            # not CAS objects. Keep them in this same snapshot. Commit-log
            # evidence needs only public ledger/sequence/time coordinates.
            query = ("SELECT ledger_id, seq, created_at FROM commit_log ORDER BY ledger_id, seq"
                     if table == "commit_log" else f"SELECT * FROM {table}")
            for row in connection.execute(query):
                value = dict(row)
                for key, item in list(value.items()):
                    if isinstance(item, bytes):
                        value[key] = {"bytes": len(item), "sha256": hashlib.sha256(item).hexdigest()}
                    elif key in ("data", "payload") and isinstance(item, str):
                        try:
                            value[key] = json.loads(item)
                        except json.JSONDecodeError:
                            pass
                rows.append(value)
            result[table] = rows
        result["decisions"] = [
            {"step": row["data"], "decision": read_cas(data_dir, row["data"]["decision_ref"])}
            for row in result.get("subject_steps", []) if row["data"].get("decision_ref")
        ]
        result["subject_request_results"] = [
            {"attempt": row["data"], "result": read_cas(data_dir, row["data"]["terminal"]["result_ref"])}
            for row in result.get("subject_request_attempts", [])
            if row["data"].get("terminal") and row["data"]["terminal"].get("result_ref")
        ]
        return result
    finally:
        connection.close()


def outcome_succeeded(result):
    outcome = result.get("outcome")
    return outcome == "succeeded" or isinstance(outcome, dict) and outcome.get("status") == "succeeded"


def contains_expected_product(text):
    return isinstance(text, str) and re.search(r"(?<!\d)437(?!\d)", text) is not None


def output_input_evidence(data_dir, facts, output, results, *, accepted=None, submission=None):
    """Bind a selected S1 to the originals in its actual sealed model request.

    A late S1 commit does not imply its earlier S0 read a completed child.
    Check the real provider payload as well as a sealed event or the exact
    history prefix that supplied it. Later rows in the final snapshot cannot
    retroactively become that request's input. This is a provenance check;
    the independent real-model review still judges what the answer says.
    """
    step, decision = output["step"], output["decision"]
    request_id = step.get("active_request_id")
    attempt = next((row["data"] for row in facts.get("subject_request_attempts", [])
                    if row["request_id"] == request_id), None)
    reference = decision.get("input_plan_ref")
    evidence = {"step_id": step.get("step_id"), "request_id": request_id,
                "input_plan_ref": reference, "input_identity_matches": False,
                "results": [], "submitted_input": None, "consumed": False}
    if (step.get("state") != "committed" or not reference or not attempt
            or decision.get("step_id") != step.get("step_id")
            or attempt.get("request_id") != request_id
            or attempt.get("step_id") != step.get("step_id")
            or step.get("active_attempt_id") not in (None, attempt.get("attempt_id"))
            or step.get("input_plan_ref") != reference
            or attempt.get("input_plan_ref") != reference):
        return evidence
    plan = read_cas(data_dir, reference)
    # RetryPending arms a new request/attempt but keeps the immutable S0.
    # Bind its original identity as well as the selected active attempt; a
    # later retry must not supply a later availability boundary for this seal.
    original = next((row["data"] for row in facts.get("subject_request_attempts", [])
                     if row["request_id"] == plan.get("request_id")), None)
    if (plan.get("step_id") != step["step_id"] or not original
            or original.get("request_id") != plan.get("request_id")
            or original.get("step_id") != step["step_id"]
            or original.get("agent_id") != attempt.get("agent_id")
            or original.get("input_plan_ref") != reference
            or not original.get("payload_digest")
            or original["payload_digest"] != attempt.get("payload_digest")
            or not original.get("provider_binding_digest")
            or original["provider_binding_digest"] != attempt.get("provider_binding_digest")):
        return evidence
    created, active_created = original["created_commit"], attempt["created_commit"]
    if (created["ledger_id"] != active_created["ledger_id"]
            or created["seq"] > active_created["seq"]
            or (original["request_id"] != request_id
                and (created["seq"] == active_created["seq"]
                     or original["attempt_id"] == attempt["attempt_id"]))):
        return evidence
    evidence["input_identity_matches"] = True
    evidence["sealed_request_id"] = original["request_id"]
    evidence["sealed_request_created_commit"] = created
    evidence["active_request_created_commit"] = active_created
    source = plan.get("context", {}).get("source", {})
    history = [row for row in facts.get("session_events", [])
               if row.get("session_id") == source.get("scope_id")
               and isinstance(row.get("seq"), int) and isinstance(source.get("leaf_seq"), int)
               and row["seq"] <= source["leaf_seq"]]
    parts = [(mi, pi, part.get("text", ""))
             for mi, message in enumerate(plan.get("payload", {}).get("messages", []))
             if message.get("role") == "user"
             for pi, part in enumerate(message.get("parts", [])) if part.get("type") == "text"]

    def source_rows(event_matches, event_type):
        fresh = [index for index, event in enumerate(plan.get("events", [])) if event_matches(event)]
        prior = [{"event_id": row["event_id"], "scope": row["session_id"], "seq": row["seq"]}
                 for row in history if row.get("event_type") == event_type
                 and event_matches(row.get("data", {}).get("inbox_event", {}))]
        return fresh, prior

    for result in results:
        def original(event):
            return (event.get("source") == "native-child" and event.get("kind") == "task_result"
                and event.get("event_id") == result["result_id"] and event.get("payload") == result)

        fresh, prior = source_rows(original, "tool_result")
        matches = []
        event_prefix = f"[event {result['result_id']}] "
        for mi, pi, text in parts:
            header, separator, body = text.partition("\n")
            if (not separator or not header.startswith(f"[runtime tool_result call_id={result['task_id']} ")
                    or not header.endswith("]") or not body.startswith(event_prefix)):
                continue
            try:
                exact_result = json.loads(body[len(event_prefix):]) == result
            except json.JSONDecodeError:
                exact_result = False
            if exact_result:
                matches.append({"message_index": mi, "part_index": pi})
        committed = result["commit_ref"]
        available_before_seal = (created["ledger_id"] == committed["ledger_id"]
                                 and created["seq"] > committed["seq"])
        evidence["results"].append({"task_id": result["task_id"], "result_id": result["result_id"],
            "operation_id": result["operation_id"], "receipt_id": result["receipt_id"],
            "t2_commit": committed, "available_before_seal": available_before_seal,
            "new_event_indices": fresh, "sealed_history_rows": prior, "payload_parts": matches,
            "consumed": available_before_seal and bool(matches) and bool(fresh or prior)})
    evidence["consumed"] = (len(results) == 2 and len({row["task_id"] for row in results}) == 2
                            and all(row["consumed"] for row in evidence["results"]))
    if accepted is not None or submission is not None:
        if not accepted or not submission:
            evidence["consumed"] = False
            return evidence

        def submitted(event):
            return (event.get("source") == "web_ui" and event.get("kind") == "message"
                and event.get("event_id") == accepted["event_id"]
                and event.get("payload", {}).get("submission_id") == submission["submission_id"]
                and event.get("payload", {}).get("content") == submission["content"])

        fresh, prior = source_rows(submitted, "user")
        expected = f"[event {accepted['event_id']}] [web_ui] {submission['content']}"
        matches = [{"message_index": mi, "part_index": pi} for mi, pi, text in parts if text == expected]
        ledger, sequence = accepted["accepted_commit"].rsplit("/", 1)
        bound = (accepted.get("submission_id") == submission["submission_id"]
                 and attempt.get("agent_id") == submission["subject_id"]
                 and created["ledger_id"] == ledger and created["seq"] > int(sequence)
                 and bool(matches) and bool(fresh or prior))
        evidence["submitted_input"] = {"submission_id": submission["submission_id"],
            "event_id": accepted["event_id"], "new_event_indices": fresh,
            "sealed_history_rows": prior, "payload_parts": matches, "consumed": bound}
        evidence["consumed"] = evidence["consumed"] and bound
    return evidence


def evaluate_first(facts, marker, expected_model, expected_url, data_dir):
    tasks = {row["task_id"]: row["data"] for row in facts.get("tasks", [])}
    model_tasks = {key for key, task in tasks.items() if task.get("kind") == "model.child"}
    native_tasks = {key for key, task in tasks.items() if task.get("kind") == "host.printf"}
    results = [row["data"] for row in facts.get("task_results", [])]
    model = [row for row in results if tasks.get(row["task_id"], {}).get("kind") == "model.child"
             and outcome_succeeded(row) and row.get("result", {}).get("termination") == "completed"
             and contains_expected_product(row.get("result", {}).get("answer"))]
    native = [row for row in results if tasks.get(row["task_id"], {}).get("kind") == "host.printf"
              and outcome_succeeded(row) and row.get("result", {}).get("stdout_utf8") == marker
              and row.get("result", {}).get("exact_postcondition") is True]
    required = model[:1] + native[:1]
    # This is one bounded task on a fresh installation, not a best-of sample.
    # Failed or unstarted extra children remain in the ledger and fail this
    # strict journey too; they are never erased by selecting one later success.
    native_operations = [row for row in facts.get("operations", [])
                         if row["data"].get("task_id") in native_tasks]
    native_operation_ids = {row["operation_id"] for row in native_operations}
    native_attempts = [row for row in facts.get("attempts", [])
                       if row["operation_id"] in native_operation_ids]
    native_receipts = [row for row in facts.get("evidence_receipts", [])
                       if row.get("operation_id") in native_operation_ids]

    def bound_model(result):
        binding = result.get("provider_binding", {})
        transcript = result.get("transcript", [])
        return (result.get("model") == expected_model and binding.get("kind") == "local"
            and binding.get("model") == expected_model and binding.get("base_url") == expected_url
            and bool(transcript) and all(step.get("provider_binding") == binding for step in transcript))

    native_once = (len(native_tasks) == len(native) == len(native_operations)
        == len(native_attempts) == len(native_receipts) == 1)
    if native_once:
        operation, attempt, receipt = native_operations[0], native_attempts[0], native_receipts[0]
        native_once = (native[0]["operation_id"] == operation["operation_id"]
            and operation["data"].get("operation_name") == "host.printf.v1"
            and operation["data"].get("outcome") == "succeeded"
            and not operation["data"].get("outcome_conflict")
            and not operation["data"].get("possible_duplicate_of")
            and receipt["data"].get("attempt_id") == attempt["attempt_id"]
            and attempt["data"].get("final_receipt_id") == receipt["receipt_id"]
            and receipt["data"].get("source_id") == "host.printf.v1"
            and receipt["data"].get("observation_level") == "postcondition_verified")
    operation_ids = {row["operation_id"] for row in required}
    attempts = {row["operation_id"] for row in facts.get("attempts", [])}
    evidence = {row.get("operation_id") for row in facts.get("evidence_receipts", [])}
    receipt_seq = max((row["commit_ref"]["seq"] for row in required), default=0)
    outputs = [row for row in facts.get("decisions", [])
               if row["step"].get("commit_ref", {}).get("seq", 0) > receipt_seq
               and marker in row["decision"]["output"]["text"]
               and contains_expected_product(row["decision"]["output"]["text"])]
    consumed = [row for row in outputs if len(required) == 2
                and output_input_evidence(data_dir, facts, row, required)["consumed"]]
    checks = {
        "one_child_of_each_requested_kind": len(model_tasks) == len(native_tasks) == 1,
        "finite_model_child_succeeded": bool(model),
        "child_model_matches_requested_binding": len(model) == 1 and bound_model(model[0].get("result", {})),
        "native_host_process_exact_result": bool(native),
        "native_effect_observed_once": native_once,
        "both_children_have_attempt_and_e1": len(required) == 2 and operation_ids <= attempts & evidence,
        "committed_output_after_child_results": bool(outputs),
        "committed_output_consumed_original_child_results": bool(consumed),
    }
    return checks


def evaluate_recovery(before, after):
    """Compare actual child effects, not just retained result IDs or prose.

    This bounded scenario requests one model child and one native child. After
    restart it requests recall only. Another child of either kind, or another
    operation/attempt for an original child, is therefore a repeated execution
    attempt even if the original successful result remains in the ledger.
    """
    kinds = {"model.child", "host.printf"}
    original_tasks = {row["task_id"] for row in before.get("tasks", [])
                      if row["data"].get("kind") in kinds}
    later_tasks = {row["task_id"] for row in after.get("tasks", [])
                   if row["data"].get("kind") in kinds}

    def effects(facts):
        operations = {row["operation_id"]: row for row in facts.get("operations", [])
                      if row["data"].get("task_id") in original_tasks}
        attempts = {row["attempt_id"]: row for row in facts.get("attempts", [])
                    if row["operation_id"] in operations}
        return operations, attempts

    original_results = {row["result_id"]: row for row in before.get("task_results", [])
                        if row["task_id"] in original_tasks}
    later_results = {row["result_id"]: row for row in after.get("task_results", [])
                     if row["task_id"] in original_tasks}
    original_operations, original_attempts = effects(before)
    later_operations, later_attempts = effects(after)
    original_receipts = {row["receipt_id"]: row for row in before.get("evidence_receipts", [])
                         if row.get("operation_id") in original_operations}
    later_receipts = {row["receipt_id"]: row for row in after.get("evidence_receipts", [])
                      if row.get("operation_id") in original_operations}
    return {
        "no_child_redelegation_after_restart": bool(original_tasks) and original_tasks == later_tasks,
        "original_child_operations_and_attempts_unchanged": bool(original_operations and original_attempts)
            and original_operations == later_operations and original_attempts == later_attempts,
        "original_child_result_bytes_retained": bool(original_results) and original_results == later_results,
        "original_child_receipt_bytes_retained": bool(original_receipts) and original_receipts == later_receipts,
    }


async def collect(ws, process, data_dir, log_path, timeout, predicate):
    deadline = time.monotonic() + timeout
    messages = []
    facts = {}
    with log_path.open("a") as log:
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(f"Shenko exited while the journey was active: {process.returncode}")
            try:
                message = json.loads(await asyncio.wait_for(ws.recv(), timeout=1))
                messages.append(message)
                log.write(json.dumps(message, ensure_ascii=False) + "\n")
                log.flush()
            except asyncio.TimeoutError:
                pass
            facts = snapshot(data_dir)
            if predicate(facts, messages):
                break
    return facts, messages


async def start(args, phase, data_dir, pause_request_id=None, *, on_interrupt=None):
    started = time.monotonic()
    startup = {"schema_version": 1, "phase": phase,
               "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
               "startup_timeout_seconds": args.startup_timeout, "ready": False}
    startup_path = args.evidence_dir / f"{phase}.startup.json"
    environment = os.environ.copy()
    environment.update(SHENKO_USE_LOCAL_MODEL="1", SHENKO_LOCAL_MODEL_URL=args.model_url,
                       SHENKO_LOCAL_MODEL=args.model,
                       SHENKO_LOG="info,shenko_timing=debug" if args.timing else "info")
    command = [str(args.binary), "--data-dir", str(data_dir), "start", "--port", str(args.port)]
    if pause_request_id is not None:
        command.extend(["--pause-at-start", pause_request_id])
    process = log = None
    try:
        log = (args.evidence_dir / f"{phase}.runtime.log").open("w")
        process = spawn_owned_runtime(command, env=environment, stdout=log, stderr=subprocess.STDOUT)
        startup["pid"] = process.pid
        deadline = started + args.startup_timeout
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(f"Shenko startup exited {process.returncode}; see {phase}.runtime.log")
            try:
                status = get_json(f"http://127.0.0.1:{args.port}/api/self",
                                  timeout=min(5, max(0.001, deadline - time.monotonic())))
            except (OSError, ValueError):
                status = {}
            if time.monotonic() >= deadline:
                break
            if status.get("subject_id"):
                startup.update(ready=True, subject_id=status["subject_id"],
                               elapsed_seconds=time.monotonic() - started)
                save(args.evidence_dir / f"{phase}.self.json", status)
                save(startup_path, startup)
                return process, log, status["subject_id"]
            await asyncio.sleep(0.2)
        raise TimeoutError(f"Shenko did not expose a bound subject within {args.startup_timeout} seconds")
    except BaseException as error:
        # Freeze startup time before owned cleanup. Retention failure must not
        # replace the original timeout, exit, cancellation or startup error.
        startup.setdefault("elapsed_seconds", time.monotonic() - started)
        startup["error"] = {"type": type(error).__name__, "message": str(error)}
        error.shenko_startup = startup
        try:
            save(startup_path, startup)
        except BaseException as retention_error:
            error.add_note(f"startup observation could not be saved: {retention_error}; {startup}")
            if not isinstance(retention_error, Exception) and on_interrupt is not None:
                on_interrupt(retention_error)
        if process is not None:
            stopped = await stop(process, log, on_interrupt=on_interrupt)
            error.shenko_startup_shutdown = stopped
            try:
                save(args.evidence_dir / f"{phase}.startup-shutdown.json", stopped)
            except Exception as retention_error:
                error.add_note(f"startup shutdown receipt could not be saved: {retention_error}; {stopped}")
        elif log is not None:
            try:
                log.close()
            except Exception as close_error:
                error.add_note(f"startup runtime log close failed: {close_error}")
        raise


def spawn_owned_runtime(command, **kwargs):
    process = subprocess.Popen(command, start_new_session=True, **kwargs)
    # Only this creation path gives stop() authority over a process group.
    # Never derive an arbitrary caller's group from os.getpgid(process.pid).
    process.shenko_owned_pgid = process.pid
    return process


def owned_group_state(pgid):
    """Observe whole processes, including live threads of a zombie leader."""
    if sys.platform.startswith("linux"):
        live, zombies, live_threads = [], [], []
        for entry in Path("/proc").iterdir():
            if not entry.name.isdecimal():
                continue
            try:
                # comm is parenthesized and may contain spaces or ')'. Fields
                # after its final ')' start with state, ppid, pgrp, session.
                fields = (entry / "stat").read_text().rsplit(")", 1)[1].split()
            except (FileNotFoundError, ProcessLookupError):
                continue
            if int(fields[2]) != pgid:
                continue
            if int(fields[3]) != pgid:
                raise RuntimeError("the recorded runtime group is no longer its original session")
            if fields[0] not in ("Z", "X", "x"):
                live.append(int(entry.name))
                continue
            # A thread-group leader may exit before its workers. /proc/PID/stat
            # then says Z while the process can still execute effects and is
            # not waitable. Only an entirely dead thread group is a zombie.
            members = []
            try:
                for thread in (entry / "task").iterdir():
                    try:
                        state = (thread / "stat").read_text().rsplit(")", 1)[1].split()[0]
                    except (FileNotFoundError, ProcessLookupError):
                        continue
                    if state not in ("Z", "X", "x"):
                        members.append(int(thread.name))
            except (FileNotFoundError, ProcessLookupError):
                continue
            if members:
                live.append(int(entry.name))
                live_threads.extend(members)
            else:
                zombies.append(int(entry.name))
        return {"live_pids": sorted(live), "zombie_pids": sorted(zombies),
                "live_thread_ids": sorted(live_threads), "alive": bool(live)}
    # Other POSIX hosts cannot provide Linux's zombie distinction here. Keep
    # that limit explicit rather than claim that unobserved members exited.
    try:
        os.killpg(pgid, 0)
        alive = True
    except ProcessLookupError:
        alive = False
    return {"live_pids": None, "zombie_pids": None, "alive": alive}


async def stop(process, log, *, parent_timeout=30, terminate_timeout=2, kill_timeout=5, on_interrupt=None):
    def clock_observation():
        return {"utc": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="microseconds"),
                "monotonic_ns": time.monotonic_ns()}

    receipt = {"parent_exit": None, "parent_timed_out": False, "clean": False, "interrupted": False,
               "started": clock_observation(), "signal_calls": [],
               "group_cleanup": {"pgid": getattr(process, "shenko_owned_pgid", None),
                                 "required": False, "forced": False, "signals": [],
                                 "complete": False}, "errors": []}
    group = receipt["group_cleanup"]
    pgid = group["pgid"]
    owned = isinstance(pgid, int) and pgid > 0 and pgid == process.pid and pgid != os.getpgrp()

    def record_error(stage, error):
        receipt["errors"].append(f"{stage}: {type(error).__name__}: {error}")
        interrupted = isinstance(error, (asyncio.CancelledError, KeyboardInterrupt, SystemExit))
        receipt["interrupted"] |= interrupted
        if interrupted and on_interrupt is not None:
            # The owner retains the original exception and propagates it only
            # after this receipt and the rest of the evidence are saved.
            on_interrupt(error)

    def signal_group(sig):
        if not owned:
            raise RuntimeError("refusing to signal a group not created by this journey")
        event = {"target": "owned_group", "id": pgid, "signal": sig.name,
                 "started": clock_observation(), "result": "pending"}
        receipt["signal_calls"].append(event)
        try:
            os.killpg(pgid, sig)
            event["result"] = "returned"
            group["signals"].append(sig.name)
            group["forced"] |= sig == signal.SIGKILL
        except ProcessLookupError:
            event["result"] = "group_absent"
        except BaseException:
            event["result"] = "raised"
            raise
        finally:
            event["finished"] = clock_observation()

    async def wait_group(timeout):
        deadline = time.monotonic() + timeout
        while True:
            process.poll()  # Reap our own leader; never wait on someone else's child.
            state = owned_group_state(pgid)
            # /proc can observe the exit between poll() and this snapshot.
            # Keep the bounded wait until our own parent's status is reaped;
            # group liveness alone cannot supply its exit code.
            if (not state["alive"] and process.poll() is not None) or time.monotonic() >= deadline:
                return state
            await asyncio.sleep(0.05)

    try:
        if not owned:
            raise RuntimeError("runtime has no owned start_new_session process group")
        if process.poll() is None:
            event = {"target": "owned_parent", "id": process.pid, "signal": "SIGINT",
                     "started": clock_observation(), "result": "pending"}
            receipt["signal_calls"].append(event)
            try:
                # Popen guards against a reaped/reused PID and may return if
                # the parent just exited. A returned call is not proof that
                # Shenko observed the signal; retain its runtime log separately.
                process.send_signal(signal.SIGINT)
                event["result"] = "returned"
            except BaseException:
                event["result"] = "raised"
                raise
            finally:
                event["finished"] = clock_observation()
            deadline = time.monotonic() + parent_timeout
            while process.poll() is None and time.monotonic() < deadline:
                await asyncio.sleep(0.05)
            receipt["parent_timed_out"] = process.poll() is None
        group["after_parent"] = owned_group_state(pgid)
        group["required"] = group["after_parent"]["alive"] or process.poll() is None
        if group["required"]:
            signal_group(signal.SIGTERM)
            remaining = await wait_group(terminate_timeout)
            if remaining["alive"] or process.poll() is None:
                signal_group(signal.SIGKILL)
                remaining = await wait_group(kill_timeout)
        else:
            remaining = group["after_parent"]
        group["remaining"] = remaining
        group["complete"] = not remaining["alive"] and process.poll() is not None
    except BaseException as error:
        record_error("runtime cleanup", error)
        # Cancellation/inspection failures must not strand the group or prevent
        # the caller retaining its failure summary. This is still only our PGID.
        if owned:
            try:
                signal_group(signal.SIGKILL)
                group["remaining"] = await wait_group(kill_timeout)
                group["complete"] = not group["remaining"]["alive"] and process.poll() is not None
            except BaseException as cleanup_error:
                record_error("forced cleanup", cleanup_error)
    finally:
        receipt["parent_exit"] = process.poll()
        group["parent_reaped"] = receipt["parent_exit"] is not None
        group["complete"] &= group["parent_reaped"]
        if owned and not group["parent_reaped"]:
            receipt["errors"].append("owned parent exit status remains unavailable after bounded cleanup")
        try:
            log.close()
        except BaseException as error:
            record_error("runtime log close", error)
    receipt["clean"] = (receipt["parent_exit"] == 0 and not receipt["parent_timed_out"]
                        and group["complete"] and not group["required"] and not group["forced"]
                        and not receipt["errors"])
    receipt["finished"] = clock_observation()
    return receipt


async def runtime_control(args, data_dir, operation, request_id):
    command = [str(args.binary), "--data-dir", str(data_dir), "runtime", operation]
    command.extend(["--request-id", request_id] if operation == "pause" else [request_id])
    process = await asyncio.create_subprocess_exec(*command, stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE, start_new_session=True)
    stdout = stderr = b""
    output_complete = False
    communication = asyncio.create_task(process.communicate())
    try:
        stdout, stderr = await asyncio.wait_for(asyncio.shield(communication), 30)
        output_complete = True
    except BaseException:
        if process.returncode is None:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        # Preserve the one reader's already-collected prefix on timeout.
        # Restarting communicate() after cancellation can silently lose it.
        stdout, stderr = await asyncio.wait_for(asyncio.shield(communication), 5)
        output_complete = True
        raise
    finally:
        if not communication.done():
            communication.cancel()
            await asyncio.gather(communication, return_exceptions=True)
        with (args.evidence_dir / "runtime-controls.jsonl").open("a") as log:
            log.write(json.dumps({"operation": operation, "request_id": request_id,
                "returncode": process.returncode, "output_complete": output_complete,
                "stdout": stdout.decode(errors="replace"),
                "stderr": stderr.decode(errors="replace")}, ensure_ascii=False) + "\n")
    if process.returncode != 0:
        raise RuntimeError(f"runtime {operation} failed for {request_id}; see runtime-controls.jsonl")
    return json.loads(stdout)


def request_ids(facts):
    return {row["request_id"] for row in facts.get("subject_request_attempts", [])}


def clean_pause(facts, request_id):
    boundary = next((row["data"] for row in facts.get("subject_quiesce_requests", [])
                     if row["request_id"] == request_id), None)
    if boundary is None or boundary.get("state") != "paused" or not boundary.get("boundary_at"):
        return False
    head = next((row["data"] for row in facts.get("subject_branch_heads", [])
                 if row["agent_id"] == boundary["agent_id"]), None)
    step = next((row["data"] for row in facts.get("subject_steps", [])
                 if head and row["step_id"] == head["next_step_id"]), None)
    return bool(head and step and head["mode"] == "paused" and step["state"] == "planned"
        and not step.get("active_request_id") and not step.get("active_attempt_id")
        and boundary["epoch"] == head["controller_epoch"]
        and boundary["generation"] == head["software_generation"] and not boundary.get("closed_at"))


async def observe_pause(args, process, data_dir, request_id, phase, expected_requests=None):
    deadline = time.monotonic() + args.timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError("Shenko exited before reaching the requested clean boundary")
        facts = snapshot(data_dir)
        if clean_pause(facts, request_id):
            status = await runtime_control(args, data_dir, "status", request_id)
            if status.get("state") != "paused" or status.get("request_id") != request_id:
                raise AssertionError("public runtime status does not confirm the observed pause")
            if expected_requests is not None and request_ids(facts) != expected_requests:
                raise AssertionError("inference was admitted while the startup boundary was paused")
            save(args.evidence_dir / f"{phase}.paused.ledger.json", facts)
            return facts
        await asyncio.sleep(0.2)
    raise TimeoutError("subject did not reach a clean durable boundary within the observation limit")


def accepted_receipt(messages, submission):
    return next((row for row in messages if row.get("type") == "receipt"
        and row.get("submission_id") == submission["submission_id"]
        and row.get("stage") == "accepted" and row.get("accepted_commit") and row.get("event_id")), None)


async def admit_then_resume(args, ws, browser, process, data_dir, phase, submission, boundary_id, prior_requests):
    facts, messages = await collect(ws, process, data_dir, args.evidence_dir / f"{phase}.websocket.jsonl", 30,
        lambda _facts, observed: accepted_receipt(observed + (browser.received() if browser else []), submission) is not None)
    # Preserve this connection's observations before adding the page's full
    # history. Only this fresh socket receives the explicit replay below.
    wire_messages = messages.copy()
    if browser:
        messages.extend(browser.received())
    accepted = accepted_receipt(messages, submission)
    if accepted is None or not clean_pause(facts, boundary_id) or request_ids(facts) != prior_requests:
        raise AssertionError("input was not durably accepted before inference admission at the paused boundary")
    save(args.evidence_dir / f"{phase}.accepted.ledger.json", facts)
    resumed = await runtime_control(args, data_dir, "resume", boundary_id)
    if resumed.get("state") != "resumed":
        raise AssertionError("public runtime resume did not release the original boundary")
    return accepted, messages, wire_messages


def first_request_contains_input(data_dir, facts, prior_requests, accepted, submission):
    attempts = [row["data"] for row in facts.get("subject_request_attempts", [])
                if row["request_id"] not in prior_requests]
    if not attempts or accepted is None:
        return False
    first = min(attempts, key=lambda attempt: attempt["created_commit"]["seq"])
    # The public v2 receipt spells CommitRef as "ledger UUID/sequence";
    # stored request facts retain its structured representation.
    try:
        accepted_ledger, accepted_sequence = accepted["accepted_commit"].rsplit("/", 1)
        accepted_sequence = int(accepted_sequence)
    except (AttributeError, KeyError, ValueError):
        return False
    head = next((row["data"] for row in facts.get("subject_branch_heads", [])
                 if row["agent_id"] == submission["subject_id"]), None)
    step = next((row["data"] for row in facts.get("subject_steps", [])
                 if row["step_id"] == first["step_id"]), None)
    if (not head or not step or first["agent_id"] != submission["subject_id"]
            or step["branch_id"] != head["branch_id"]
            or first["created_commit"]["ledger_id"] != accepted_ledger
            or first["created_commit"]["seq"] <= accepted_sequence):
        return False
    plan = read_cas(data_dir, first["input_plan_ref"])
    if plan.get("request_id") != first["request_id"] or plan.get("step_id") != first["step_id"]:
        return False
    bound_event = any(event.get("event_id") == accepted["event_id"]
        and event.get("payload", {}).get("submission_id") == submission["submission_id"]
        and event.get("payload", {}).get("content") == submission["content"] for event in plan.get("events", []))
    sent_text = any(submission["content"] in part.get("text", "")
        for message in plan.get("payload", {}).get("messages", []) if message.get("role") == "user"
        for part in message.get("parts", []) if part.get("type") == "text")
    return bound_event and sent_text


async def run(args):
    import websockets

    started = time.monotonic()
    args.evidence_dir.mkdir(parents=True, exist_ok=False)
    data_dir = args.evidence_dir / "installation"
    marker = "S1-" + uuid.uuid4().hex[:12]
    resume_marker = "CONTINUE-" + uuid.uuid4().hex[:12]
    startup_boundary = str(uuid.uuid4())
    restart_boundary = str(uuid.uuid4())
    manifest = {
        "schema_version": 1, "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "binary": str(args.binary), "binary_sha256": sha256(args.binary),
        "source_revision": args.source_revision, "model": args.model, "model_url": args.model_url,
        "driver_sha256": sha256(Path(__file__)),
        "websockets_version": websockets.__version__, "marker": marker, "resume_marker": resume_marker,
        "configured_local_request_timeout_ms": os.environ.get("SHENKO_LOCAL_MODEL_REQUEST_TIMEOUT_MS"),
        "startup_timeout_seconds": args.startup_timeout,
        "phase_timeout_seconds": args.timeout,
        "observer_websocket": observer_websocket_options(args.timeout),
        "request_path_timing": args.timing,
        "input_surface": "live_browser_composer" if args.browser else "public_websocket",
        "startup_boundary": startup_boundary, "restart_boundary": restart_boundary,
        "scope": "real configured model and installed CLI only; no stronger model baseline, NapCat, coding seat, long-run or physical-failure claim",
    }
    save(args.evidence_dir / "manifest.json", manifest)
    summary = {"passed": False, "checks": {}, "errors": [], "interruptions": []}
    process = log = browser = None
    recovery_baseline = None
    pending_interrupt = None

    def record_failure(stage, error):
        nonlocal pending_interrupt
        summary["passed"] = False
        summary["errors"].append(f"{stage}: {type(error).__name__}: {error}")
        if not isinstance(error, Exception):
            if pending_interrupt is None:
                pending_interrupt = error
            summary["interruptions"].append({"stage": stage, "type": type(error).__name__, "message": str(error)})

    def interrupted_shutdown(error):
        record_failure("runtime cleanup interrupted", error)

    def retain_browser_report(report):
        summary["browser"] = report
        summary["checks"].update(report["checks"])
        summary["errors"].extend(report["errors"])
        summary["passed"] = summary["passed"] and not report["errors"] and all(report["checks"].values())

    try:
        save(args.evidence_dir / "models.json", get_json(args.model_url.rstrip("/") + "/v1/models"))
        process, log, subject = await start(args, "first", data_dir, startup_boundary, on_interrupt=interrupted_shutdown)
        await observe_pause(args, process, data_dir, startup_boundary, "first", set())
        summary["checks"]["startup_paused_before_inference"] = True
        if args.browser:
            from real_browser_witness import BrowserWitness
            browser = BrowserWitness(args.evidence_dir, f"http://127.0.0.1:{args.port}")
            await browser.open()
        submission = {
            "schema_version": 2, "type": "submit", "submission_id": str(uuid.uuid4()),
            "message_id": str(uuid.uuid4()), "subject_id": subject, "mode": "append",
            "content": f"请完成一次有真实回执的有限任务：委派一个使用 {args.model} 的有限模型 child，独立计算 19×23。另委派已注册的 host.printf 原生 child，输入 text 为 {marker}。等两个任务实际返回结果，再在正文中报告计算结果和原样回显的标记；不要模拟执行，不要提前宣称成功。",
        }
        async with websockets.connect(f"ws://127.0.0.1:{args.port}/ws", **observer_websocket_options(args.timeout)) as ws:
            if browser:
                submission = await browser.submit("first", submission["content"], subject)
            else:
                await ws.send(json.dumps(submission, ensure_ascii=False))
            save(args.evidence_dir / "first.input.json", submission)
            accepted, initial_messages, _ = await admit_then_resume(args, ws, browser, process, data_dir,
                "first", submission, startup_boundary, set())
            summary["checks"]["input_accepted_before_inference"] = True
            first, messages = await collect(ws, process, data_dir, args.evidence_dir / "first.websocket.jsonl", args.timeout,
                lambda facts, _: all(evaluate_first(facts, marker, args.model, args.model_url, data_dir).values()))
            messages = initial_messages + messages
        if browser:
            # Receipts are sent to the submitting connection. Keep the
            # original receipt assertion, using that page's actual frames.
            messages.extend(browser.received())
        save(args.evidence_dir / "first.ledger.json", first)
        summary["checks"].update(evaluate_first(first, marker, args.model, args.model_url, data_dir))
        # Start the immutable-effect comparison at the first successful
        # answer. Browser observation and requesting pause can take time;
        # repetitions in that tail must not become part of a later baseline.
        if all(summary["checks"].values()):
            recovery_baseline = first
        accepted = accepted_receipt(messages, submission)
        summary["checks"]["input_durably_accepted"] = bool(accepted and accepted.get("accepted_commit"))
        summary["checks"]["first_request_contains_exact_input"] = first_request_contains_input(
            data_dir, first, set(), accepted, submission)
        head = first["subject_branch_heads"][0]["data"]
        before_ids = {row["task_id"] for row in first.get("task_results", [])}
        requested_tasks = {row["task_id"] for row in first.get("tasks", [])
                           if row["data"].get("kind") in {"model.child", "host.printf"}}
        required_results = [row["data"] for row in first.get("task_results", [])
                            if row["task_id"] in requested_tasks]
        output = None
        if summary["checks"]["committed_output_consumed_original_child_results"]:
            output = next(row for row in reversed(first["decisions"])
                if evaluate_first(dict(first, decisions=[row]), marker, args.model, args.model_url, data_dir)
                    ["committed_output_consumed_original_child_results"])
            summary["first_output_input"] = output_input_evidence(data_dir, first, output, required_results)
        if browser:
            if output is not None:
                await browser.observe_output("first", output["step"]["step_id"], (marker, "437"))
            else:
                browser.observations["first_output"] = {"observed": False, "reason": "no qualifying committed model result"}
        if not all(summary["checks"].values()):
            raise AssertionError("first phase did not satisfy the child/binding/single-execution/output checks")
        await runtime_control(args, data_dir, "pause", restart_boundary)
        paused = await observe_pause(args, process, data_dir, restart_boundary, "before-restart")
        pause_checks = evaluate_first(paused, marker, args.model, args.model_url, data_dir)
        pause_checks.update(evaluate_recovery(recovery_baseline, paused))
        summary["checks"].update(pause_checks)
        if not all(pause_checks.values()):
            raise AssertionError("child effects changed between the first answer and the restart boundary")
        prior_requests = request_ids(paused)
        summary["checks"]["clean_boundary_before_restart"] = True
        if browser:
            await browser.prepare_restart("未发送草稿 · " + marker)
        summary["first_shutdown"] = await stop(process, log, on_interrupt=interrupted_shutdown)
        summary["first_exit"] = summary["first_shutdown"]["parent_exit"]
        process = log = None
        summary["checks"]["first_shutdown_clean"] = summary["first_shutdown"]["clean"]
        if not summary["checks"]["first_shutdown_clean"]:
            raise RuntimeError(f"paused runtime did not shut down cleanly (parent exit {summary['first_exit']}); see first_shutdown and first.runtime.log")
        if browser:
            await browser.observe_stopped()
        process, log, resumed_subject = await start(args, "restart", data_dir, restart_boundary, on_interrupt=interrupted_shutdown)
        await observe_pause(args, process, data_dir, restart_boundary, "restart", prior_requests)
        summary["checks"]["restart_kept_original_pause"] = True
        summary["checks"]["same_subject_after_restart"] = resumed_subject == subject
        if browser:
            await browser.observe_restarted()
        resumed_input = dict(submission, submission_id=str(uuid.uuid4()), message_id=str(uuid.uuid4()),
            content=f"刚才运行进程已重启。请从已有上下文继续：报告上一条任务的原始标记和 child 计算结果，再附上本次继续工作的标记 {resume_marker}。不要重新执行上一条任务。")
        async with websockets.connect(f"ws://127.0.0.1:{args.port}/ws", **observer_websocket_options(args.timeout)) as ws:
            # This is the pre-existing idempotency check, never a new input:
            # the page's exact original IDs and payload are replayed intact.
            await ws.send(json.dumps(submission, ensure_ascii=False))
            if browser:
                resumed_input = await browser.submit("restart", resumed_input["content"], resumed_subject)
            else:
                await ws.send(json.dumps(resumed_input, ensure_ascii=False))
            save(args.evidence_dir / "restart.input.json", resumed_input)
            resumed_accepted, initial_messages, replay_wire = await admit_then_resume(args, ws, browser, process, data_dir,
                "restart", resumed_input, restart_boundary, prior_requests)
            summary["checks"]["continued_input_accepted_before_inference"] = True
            def continued(facts, messages):
                return any(row["step"]["step_no"] > head["committed_step_no"]
                    and all(text in row["decision"]["output"]["text"] for text in (marker, resume_marker))
                    and contains_expected_product(row["decision"]["output"]["text"])
                    and output_input_evidence(data_dir, facts, row, required_results,
                                             accepted=resumed_accepted, submission=resumed_input)["consumed"]
                    for row in facts.get("decisions", []))
            after, messages = await collect(ws, process, data_dir, args.evidence_dir / "restart.websocket.jsonl", args.timeout, continued)
            replay = accepted_receipt(replay_wire + messages, submission)
            messages = initial_messages + messages
        save(args.evidence_dir / "restart.ledger.json", after)
        summary["checks"]["model_continued_prior_context"] = continued(after, messages)
        summary["checks"]["continued_request_contains_exact_input"] = first_request_contains_input(
            data_dir, after, prior_requests, resumed_accepted, resumed_input)
        summary["checks"]["original_results_retained"] = before_ids <= {row["task_id"] for row in after.get("task_results", [])}
        summary["checks"].update(evaluate_recovery(recovery_baseline, after))
        summary["replayed_input_receipt"] = replay
        summary["checks"]["replayed_input_same_event"] = bool(accepted and replay and replay.get("replayed") is True
            and replay.get("event_id") == accepted.get("event_id") and replay.get("accepted_commit") == accepted.get("accepted_commit"))
        summary["checks"]["same_branch_after_restart"] = after["subject_branch_heads"][0]["data"]["branch_id"] == head["branch_id"]
        output = None
        if summary["checks"]["model_continued_prior_context"]:
            output = next(row for row in reversed(after["decisions"])
                          if continued(dict(after, decisions=[row]), messages))
            summary["restart_output_input"] = output_input_evidence(data_dir, after, output, required_results,
                                                                    accepted=resumed_accepted, submission=resumed_input)
        if browser:
            if output is not None:
                await browser.observe_output("restart", output["step"]["step_id"], (marker, resume_marker, "437"))
            else:
                browser.observations["restart_output"] = {"observed": False, "reason": "no qualifying continued model result"}
            await browser.finish_journey()
        summary["passed"] = all(summary["checks"].values()) and not summary["errors"]
    except BaseException as error:
        record_failure("journey", error)
        if hasattr(error, "shenko_startup"):
            summary["startup"] = error.shenko_startup
        if hasattr(error, "shenko_startup_shutdown"):
            summary["startup_shutdown"] = error.shenko_startup_shutdown
        try:
            (args.evidence_dir / "failure.txt").write_text(traceback.format_exc())
        except BaseException as retention_error:
            record_failure("failure traceback retention", retention_error)
    finally:
        if browser is not None:
            try:
                report = await browser.close()
                retain_browser_report(report)
            except BaseException as error:
                record_failure("browser evidence/cleanup", error)
                summary["checks"]["browser_cleanup_complete"] = False
                if hasattr(error, "shenko_browser_cleanup"):
                    retain_browser_report(error.shenko_browser_cleanup)
        if process is not None:
            try:
                summary["last_shutdown"] = await stop(process, log, on_interrupt=interrupted_shutdown)
                summary["last_exit"] = summary["last_shutdown"]["parent_exit"]
                summary["checks"]["final_shutdown_clean"] = summary["last_shutdown"]["clean"]
                summary["passed"] = summary["passed"] and summary["checks"]["final_shutdown_clean"]
                if not summary["checks"]["final_shutdown_clean"]:
                    summary["errors"].append(f"owned runtime did not shut down cleanly (parent exit {summary['last_exit']}); see last_shutdown, runtime logs and final ledger")
            except BaseException as error:
                record_failure("runtime cleanup failed", error)
                summary["checks"]["final_shutdown_clean"] = False
        if (data_dir / "shenko.db").exists():
            try:
                final_facts = snapshot(data_dir)
                save(args.evidence_dir / "final.ledger.json", final_facts)
                final_checks = evaluate_first(final_facts, marker, args.model, args.model_url, data_dir)
                summary["checks"].update(final_checks)
                summary["passed"] = summary["passed"] and all(final_checks.values())
                if recovery_baseline is not None:
                    # The subject continues while the browser observes and
                    # shutdown completes. Include that tail in the no-replay
                    # verdict instead of checking only its first answer.
                    recovery_checks = evaluate_recovery(recovery_baseline, final_facts)
                    summary["checks"].update(recovery_checks)
                    summary["passed"] = summary["passed"] and all(recovery_checks.values())
            except BaseException as error:
                record_failure("final ledger snapshot", error)
        # The private installation stays local. Published evidence is the
        # explicit ledger/CAS subset, never owner claim/control secrets.
        if (data_dir / "cas").exists():
            try:
                shutil.copytree(data_dir / "cas", args.evidence_dir / "cas")
            except BaseException as error:
                record_failure("CAS evidence retention", error)
        summary["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        summary["elapsed_seconds"] = time.monotonic() - started
        try:
            save(args.evidence_dir / "summary.json", summary)
        except BaseException as error:
            record_failure("summary retention", error)
            # The caller's captured stdout still receives the complete failure
            # below, even when the evidence filesystem itself cannot write.
        print(json.dumps(summary, ensure_ascii=False, indent=2))
    if pending_interrupt is not None:
        raise pending_interrupt
    if any(summary.get(key, {}).get("interrupted")
           for key in ("startup_shutdown", "first_shutdown", "last_shutdown")):
        raise asyncio.CancelledError("runtime cleanup was interrupted; retained the attempt evidence")
    return 0 if summary["passed"] else 1


async def repeat(args):
    """Repeat the complete unchanged journey with fresh, retained installations.

    Failures stay in the denominator. A changed binary or served model card
    stops the batch instead of combining different candidates' success rates.
    Server metadata is an observed binding, not a weight-file attestation.
    """
    args.evidence_dir.mkdir(parents=True, exist_ok=False)
    report = {"schema_version": 1, "source_revision": args.source_revision,
              "startup_timeout_seconds": args.startup_timeout,
              "requested_runs": args.repeat, "runs": [], "passed": False,
              "errors": [], "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}

    def checkpoint():
        runs = report["runs"]
        report.update(attempted_runs=len(runs), completed_runs=sum(row["completed"] for row in runs),
                      passed_runs=sum(row["passed"] for row in runs),
                      failed_runs=sum(not row["passed"] for row in runs),
                      unattempted_runs=args.repeat - len(runs))
        report["success_rate"] = report["passed_runs"] / len(runs) if runs else None
        report["passed"] = len(runs) == args.repeat and all(row["passed"] for row in runs) and not report["errors"]
        save(args.evidence_dir / "batch-summary.json", report)

    def model_card():
        cards = [card for card in get_json(args.model_url.rstrip("/") + "/v1/models").get("data", [])
                 if card.get("id") == args.model]
        if len(cards) != 1:
            raise ValueError("the selected model must have one unambiguous served model card")
        # vLLM may regenerate both permission IDs and the response's created
        # timestamp without restarting the loaded model. Neither is serving
        # identity. Raw cards remain in every attempt's models.json.
        return {key: value for key, value in cards[0].items() if key not in {"permission", "created"}}

    checkpoint()
    try:
        report["binary_sha256"] = sha256(args.binary)
        report["model_card"] = model_card()
        report["model_card_non_identity_fields"] = ["permission", "created"]
        report["model_url"] = args.model_url
        checkpoint()
        for index in range(1, args.repeat + 1):
            if sha256(args.binary) != report["binary_sha256"] or model_card() != report["model_card"]:
                raise RuntimeError("binary or served model identity changed during the frozen batch")
            attempt = copy.copy(args)
            attempt.evidence_dir = args.evidence_dir / f"attempt-{index:03d}"
            row = {"attempt": index, "directory": attempt.evidence_dir.name,
                   "completed": False, "passed": False, "errors": []}
            # Persist the denominator before starting an external process.
            # A driver/cleanup crash is an attempted failure, not an omitted run.
            report["runs"].append(row)
            checkpoint()
            try:
                exit_code = await run(attempt)
                summary_path = attempt.evidence_dir / "summary.json"
                summary = json.loads(summary_path.read_text())
                row.update(summary_sha256=sha256(summary_path), exit_code=exit_code,
                           elapsed_seconds=summary["elapsed_seconds"], errors=summary["errors"],
                           completed=True)
                if any(not summary[key]["group_cleanup"]["complete"]
                       for key in ("startup_shutdown", "first_shutdown", "last_shutdown") if key in summary):
                    raise RuntimeError("the previous runtime group is still live or its cleanup could not be verified")
                if sha256(args.binary) != report["binary_sha256"] or model_card() != report["model_card"]:
                    raise RuntimeError("binary or served model identity changed within an attempt")
                row["passed"] = exit_code == 0 and summary["passed"]
            except BaseException as error:
                row["errors"].append(f"{type(error).__name__}: {error}")
                raise
            finally:
                checkpoint()
    except Exception as error:
        report["errors"].append(f"{type(error).__name__}: {error}")
        (args.evidence_dir / "batch-failure.txt").write_text(traceback.format_exc())
    finally:
        report["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        checkpoint()
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if report["passed"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--source-revision", default="unverified")
    parser.add_argument("--model-url", required=True,
                        help="LocalModelProvider base URL without /v1; the provider appends its versioned API paths")
    parser.add_argument("--model", required=True)
    parser.add_argument("--evidence-dir", required=True, type=Path)
    parser.add_argument("--port", type=int, default=17860)
    parser.add_argument("--browser", action="store_true",
                        help="Use the actual UI composer and observe one Chromium page across the same CLI restart")
    parser.add_argument("--timeout", type=int, default=300)
    parser.add_argument("--startup-timeout", type=int, default=60,
                        help="Seconds to observe a bound subject after each spawn (default: 60)")
    parser.add_argument("--repeat", type=int, default=1,
                        help="Run complete journeys sequentially; retain every attempt and its success-rate denominator")
    parser.add_argument("--timing", action="store_true",
                        help="Enable only the structured shenko_timing debug target alongside normal info logs")
    args = parser.parse_args()
    if args.repeat < 1 or args.timeout < 1 or args.startup_timeout < 1:
        parser.error("--repeat, --timeout and --startup-timeout must be positive")
    args.binary = args.binary.resolve()
    args.evidence_dir = args.evidence_dir.resolve()
    raise SystemExit(asyncio.run(repeat(args) if args.repeat > 1 else run(args)))


if __name__ == "__main__":
    main()
