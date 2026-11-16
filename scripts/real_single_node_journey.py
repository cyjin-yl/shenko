#!/usr/bin/env python3
"""Run an explicit, bounded real-model single-node journey.

Requires an actual OpenAI-compatible model service and Python websockets 16.
Starts the real Shenko CLI, submits over its public WebSocket, inspects durable
child/E1/decision facts read-only, restarts the same installation, replays the
same input identity, and asks the model to continue from its prior context.
No model response, task, receipt, or successful outcome is injected. Failure
retains the same evidence and exits nonzero; this is not a default CI test.
"""
import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import sqlite3
import subprocess
import time
import traceback
import urllib.request
import uuid

import websockets


def save(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def get_json(url):
    # The model and UI are explicit endpoints; local evidence must not depend
    # on a shell's HTTP proxy rewriting loopback requests.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(url, timeout=5) as response:
        return json.load(response)


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
)


def snapshot(data_dir):
    connection = sqlite3.connect(f"file:{data_dir / 'shenko.db'}?mode=ro", uri=True)
    connection.row_factory = sqlite3.Row
    result = {}
    try:
        tables = {row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        for table in TABLES:
            if table not in tables:
                continue
            rows = []
            for row in connection.execute(f"SELECT * FROM {table}"):
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
        return result
    finally:
        connection.close()


def outcome_succeeded(result):
    outcome = result.get("outcome")
    return outcome == "succeeded" or isinstance(outcome, dict) and outcome.get("status") == "succeeded"


def evaluate_first(facts, marker):
    tasks = {row["task_id"]: row["data"] for row in facts.get("tasks", [])}
    results = [row["data"] for row in facts.get("task_results", [])]
    model = [row for row in results if tasks.get(row["task_id"], {}).get("kind") == "model.child"
             and outcome_succeeded(row) and "437" in json.dumps(row.get("result"))]
    native = [row for row in results if tasks.get(row["task_id"], {}).get("kind") == "host.printf"
              and outcome_succeeded(row) and row.get("result", {}).get("stdout_utf8") == marker
              and row.get("result", {}).get("exact_postcondition") is True]
    required = model[:1] + native[:1]
    operation_ids = {row["operation_id"] for row in required}
    attempts = {row["operation_id"] for row in facts.get("attempts", [])}
    evidence = {row.get("operation_id") for row in facts.get("evidence_receipts", [])}
    receipt_seq = max((row["commit_ref"]["seq"] for row in required), default=0)
    outputs = [row for row in facts.get("decisions", [])
               if row["step"].get("commit_ref", {}).get("seq", 0) > receipt_seq
               and marker in row["decision"]["output"]["text"]
               and "437" in row["decision"]["output"]["text"]]
    checks = {
        "finite_model_child_succeeded": bool(model),
        "native_host_process_exact_result": bool(native),
        "both_children_have_attempt_and_e1": len(required) == 2 and operation_ids <= attempts & evidence,
        "committed_output_after_child_results": bool(outputs),
    }
    return checks


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


async def start(args, phase, data_dir):
    environment = os.environ.copy()
    environment.update(SHENKO_USE_LOCAL_MODEL="1", SHENKO_LOCAL_MODEL_URL=args.model_url,
                       SHENKO_LOCAL_MODEL=args.model, SHENKO_LOG="info")
    log = (args.evidence_dir / f"{phase}.runtime.log").open("w")
    process = subprocess.Popen([str(args.binary), "--data-dir", str(data_dir), "start", "--port", str(args.port)],
                               env=environment, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    try:
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(f"Shenko startup exited {process.returncode}; see {phase}.runtime.log")
            try:
                status = get_json(f"http://127.0.0.1:{args.port}/api/self")
                if status.get("subject_id"):
                    save(args.evidence_dir / f"{phase}.self.json", status)
                    return process, log, status["subject_id"]
            except (OSError, ValueError):
                pass
            await asyncio.sleep(0.2)
        raise TimeoutError("Shenko did not expose a bound subject within 60 seconds")
    except BaseException:
        await stop(process, log)
        raise


async def stop(process, log):
    if process.poll() is None:
        process.send_signal(signal.SIGINT)
        try:
            await asyncio.wait_for(asyncio.to_thread(process.wait), 30)
        except asyncio.TimeoutError:
            # This is the owned test process group, never an unrelated runtime.
            os.killpg(process.pid, signal.SIGKILL)
            await asyncio.to_thread(process.wait)
    log.close()
    return process.returncode


async def run(args):
    args.evidence_dir.mkdir(parents=True, exist_ok=False)
    data_dir = args.evidence_dir / "installation"
    marker = "S1-" + uuid.uuid4().hex[:12]
    resume_marker = "CONTINUE-" + uuid.uuid4().hex[:12]
    manifest = {
        "schema_version": 1, "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "binary": str(args.binary), "binary_sha256": sha256(args.binary),
        "source_revision": args.source_revision, "model": args.model, "model_url": args.model_url,
        "websockets_version": websockets.__version__, "marker": marker, "resume_marker": resume_marker,
        "scope": "real configured model and installed CLI only; no stronger model baseline, NapCat, coding seat, long-run or physical-failure claim",
    }
    save(args.evidence_dir / "manifest.json", manifest)
    save(args.evidence_dir / "models.json", get_json(args.model_url.rstrip("/") + "/models"))
    summary = {"passed": False, "checks": {}, "errors": []}
    process = log = None
    try:
        process, log, subject = await start(args, "first", data_dir)
        submission = {
            "schema_version": 2, "type": "submit", "submission_id": str(uuid.uuid4()),
            "message_id": str(uuid.uuid4()), "subject_id": subject, "mode": "append",
            "content": f"请完成一次有真实回执的有限任务：委派一个使用 {args.model} 的有限模型 child，独立计算 19×23。另委派已注册的 host.printf 原生 child，输入 text 为 {marker}。等两个任务实际返回结果，再在正文中报告计算结果和原样回显的标记；不要模拟执行，不要提前宣称成功。",
        }
        save(args.evidence_dir / "first.input.json", submission)
        async with websockets.connect(f"ws://127.0.0.1:{args.port}/ws", proxy=None, max_size=16*1024*1024) as ws:
            await ws.send(json.dumps(submission, ensure_ascii=False))
            first, messages = await collect(ws, process, data_dir, args.evidence_dir / "first.websocket.jsonl", args.timeout,
                lambda facts, _: all(evaluate_first(facts, marker).values()))
        save(args.evidence_dir / "first.ledger.json", first)
        summary["checks"].update(evaluate_first(first, marker))
        receipts = [row for row in messages if row.get("type") == "receipt" and row.get("submission_id") == submission["submission_id"]]
        accepted = next((row for row in receipts if row.get("stage") == "accepted"), None)
        summary["checks"]["input_durably_accepted"] = bool(accepted and accepted.get("accepted_commit"))
        head = first["subject_branch_heads"][0]["data"]
        before_ids = {row["task_id"] for row in first.get("task_results", [])}
        summary["first_exit"] = await stop(process, log)
        process = log = None
        process, log, resumed_subject = await start(args, "restart", data_dir)
        summary["checks"]["same_subject_after_restart"] = resumed_subject == subject
        resumed_input = dict(submission, submission_id=str(uuid.uuid4()), message_id=str(uuid.uuid4()),
            content=f"刚才运行进程已重启。请从已有上下文继续：报告上一条任务的原始标记和 child 计算结果，再附上本次继续工作的标记 {resume_marker}。不要重新执行上一条任务。")
        save(args.evidence_dir / "restart.input.json", resumed_input)
        async with websockets.connect(f"ws://127.0.0.1:{args.port}/ws", proxy=None, max_size=16*1024*1024) as ws:
            await ws.send(json.dumps(submission, ensure_ascii=False))
            await ws.send(json.dumps(resumed_input, ensure_ascii=False))
            def continued(facts, messages):
                return any(row["step"]["step_no"] > head["committed_step_no"]
                    and all(text in row["decision"]["output"]["text"] for text in (marker, "437", resume_marker))
                    for row in facts.get("decisions", []))
            after, messages = await collect(ws, process, data_dir, args.evidence_dir / "restart.websocket.jsonl", args.timeout, continued)
        save(args.evidence_dir / "restart.ledger.json", after)
        summary["checks"]["model_continued_prior_context"] = continued(after, messages)
        summary["checks"]["original_results_retained"] = before_ids <= {row["task_id"] for row in after.get("task_results", [])}
        replay = next((row for row in messages if row.get("type") == "receipt" and row.get("submission_id") == submission["submission_id"]), None)
        summary["checks"]["replayed_input_same_event"] = bool(accepted and replay and replay.get("replayed") is True
            and replay.get("event_id") == accepted.get("event_id") and replay.get("accepted_commit") == accepted.get("accepted_commit"))
        summary["checks"]["same_branch_after_restart"] = after["subject_branch_heads"][0]["data"]["branch_id"] == head["branch_id"]
        summary["passed"] = all(summary["checks"].values())
    except Exception as error:
        summary["errors"].append(f"{type(error).__name__}: {error}")
        (args.evidence_dir / "failure.txt").write_text(traceback.format_exc())
    finally:
        if process is not None:
            summary["last_exit"] = await stop(process, log)
        if (data_dir / "shenko.db").exists():
            try:
                save(args.evidence_dir / "final.ledger.json", snapshot(data_dir))
            except Exception as error:
                summary["passed"] = False
                summary["errors"].append(f"final ledger snapshot: {type(error).__name__}: {error}")
        # The private installation stays local. Published evidence is the
        # explicit ledger/CAS subset, never owner claim/control secrets.
        if (data_dir / "cas").exists():
            shutil.copytree(data_dir / "cas", args.evidence_dir / "cas")
        save(args.evidence_dir / "summary.json", summary)
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    return 0 if summary["passed"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--source-revision", default="unverified")
    parser.add_argument("--model-url", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--evidence-dir", required=True, type=Path)
    parser.add_argument("--port", type=int, default=17860)
    parser.add_argument("--timeout", type=int, default=300)
    args = parser.parse_args()
    args.binary = args.binary.resolve()
    args.evidence_dir = args.evidence_dir.resolve()
    raise SystemExit(asyncio.run(run(args)))


if __name__ == "__main__":
    main()
