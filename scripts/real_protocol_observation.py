#!/usr/bin/env python3
"""Observe eight first requests through the real CLI/Store/provider/parser.

This is a narrow protocol comparison, not successful task execution or a
complete journey. Each fresh installation receives one held-out input through
the real WebSocket while paused. Observation ends when its FIRST request is
terminal and that same step is committed or retired. Later corrections, child
results and shutdown-induced Unknowns remain evidence, never replacements for
the first-request verdict. Requires Python 3.11+ and websockets==16.0.
"""
import argparse
import asyncio
import copy
import hashlib
import json
import os
from pathlib import Path
import shutil
import time
import traceback
import uuid

import real_single_node_journey as journey


SCOPE = "first-request protocol and durable S1 admission only; no child completion, arithmetic correctness, recovery, latency or full-journey success claim"
CASES = tuple({"id": f"multiply-{a}-{b}", "left": a, "right": b,
               "marker": f"PROTO-PAIR-{index:02d}-{suffix}"}
    for index, (a, b, suffix) in enumerate([
        (17, 29, "a2f709c1"), (28, 34, "c8e105b7"),
        (46, 27, "f3a682d0"), (63, 18, "b59d207e"),
        (42, 57, "e7c041a6"), (73, 16, "d1b896f2"),
        (84, 25, "c4f270e9"), (56, 39, "a8d613b5"),
    ], 1))
METRICS = ("input_in_first_seal", "first_result_committed_without_r0", "s1_requested_children_admitted")


def digest(value):
    return hashlib.sha256(value).hexdigest()


def case_content(case, model):
    return (f"请完成一次有真实回执的有限任务：委派一个使用 {model} 的有限模型 child，"
        f"独立计算 {case['left']}×{case['right']}。另委派已注册的 host.printf 原生 child，"
        f"输入 text 为 {case['marker']}。等两个任务实际返回结果，再在正文中报告计算结果和"
        "原样回显的标记；不要模拟执行，不要提前宣称成功。")


def first_observation(facts):
    attempts = [row["data"] for row in facts.get("subject_request_attempts", [])]
    if not attempts:
        return None
    first = min(attempts, key=lambda row: row["created_commit"]["seq"])
    if not first.get("terminal"):
        return None
    step = next((row["data"] for row in facts.get("subject_steps", [])
                 if row["step_id"] == first["step_id"]), None)
    return (first, step) if step and step["state"] in {"committed", "retired"} else None


def retain_object(data_dir, evidence_dir, name, reference):
    key = reference["digest"].removeprefix("blake3:")
    raw = (data_dir / "cas" / key[:2] / key[2:4] / key).read_bytes()
    if len(raw) != reference["bytes"]:
        raise ValueError("source CAS length differs from the frozen reference")
    (evidence_dir / name).write_bytes(raw)
    return {"file": name, "reference": reference, "sha256": digest(raw)}


def assess(args, case, facts, accepted, submission):
    attempt, step = first_observation(facts)
    data_dir = args.evidence_dir / "installation"
    plan = journey.read_cas(data_dir, attempt["input_plan_ref"])
    result = journey.read_cas(data_dir, attempt["terminal"]["result_ref"])
    sources = [retain_object(data_dir, args.evidence_dir, "first.input-plan.json", attempt["input_plan_ref"]),
               retain_object(data_dir, args.evidence_dir, "first.request-result.json", attempt["terminal"]["result_ref"])]
    for key, name in [("model_result_ref", "first.output.txt"), ("model_outcome_ref", "first.model-outcome.json"),
                      ("decision_ref", "first.decision.json"), ("retry_reason_ref", "first.rejection.json")]:
        if step.get(key):
            sources.append(retain_object(data_dir, args.evidence_dir, name, step[key]))
    decision = journey.read_cas(data_dir, step["decision_ref"]) if step.get("decision_ref") else None
    rejection = journey.read_cas(data_dir, step["retry_reason_ref"]) if step.get("retry_reason_ref") else None
    binding = plan.get("provider_binding", {})
    effective = result.get("effective", {})
    comparison_valid = (binding.get("kind") == "local" and binding.get("model") == args.model
        and binding.get("base_url") == args.model_url
        and plan["payload"].get("max_tokens") == effective.get("max_tokens") == 4096
        and plan["payload"].get("native_options", {}) == effective.get("native_options") == {}
        and effective.get("provider_binding") == binding
        and effective.get("profile_id") == "local.openai.chat_completions"
        and effective.get("profile_revision") == binding.get("serializer_revision")
        and effective.get("api_version") is None)
    terminal = result.get("termination", {})
    committed = (step["state"] == "committed" and bool(step.get("commit_ref"))
        and decision is not None and decision.get("step_id") == step["step_id"]
        and result.get("request_id") == attempt["request_id"] and result.get("status") == "success"
        and terminal.get("kind") == "provider_terminal" and terminal.get("length") is False
        and terminal.get("stop_reason") in {"stop", "end_turn", "stop_sequence"})
    intents = decision.get("delegated_task_intents", []) if decision else []
    model = [row for row in intents if (row.get("definition") or {}).get("kind") == "model.child"]
    native = [row for row in intents if (row.get("definition") or {}).get("kind") == "host.printf"]
    tasks = {row["task_id"]: row["data"] for row in facts.get("tasks", [])}
    children = committed and len(intents) == 2 and len(model) == len(native) == 1
    if children:
        contract = model[0]["definition"]["input"]
        binding = contract.get("provider_binding", {})
        children = (contract.get("spec", {}).get("model") == args.model
            and binding.get("kind") == "local" and binding.get("model") == args.model
            and binding.get("base_url") == args.model_url
            and native[0]["definition"].get("definition_version") == 1
            and native[0]["definition"]["input"].get("text") == case["marker"]
            and all(intent["intent_id"] in tasks
                and tasks[intent["intent_id"]].get("created_by_step_id") == step["step_id"]
                and tasks[intent["intent_id"]].get("created_commit") == step["commit_ref"]
                and tasks[intent["intent_id"]].get("input_ref") == step["decision_ref"]
                and tasks[intent["intent_id"]].get("kind") == intent["definition"]["kind"]
                for intent in intents))
    return {"scope": SCOPE, "request_id": attempt["request_id"], "step_id": step["step_id"],
        "step_state": step["state"], "commit_ref": step.get("commit_ref"), "sources": sources,
        "system_prompt_sha256": digest(plan["payload"]["system_prompt"].encode()),
        "sealed_max_tokens": plan["payload"].get("max_tokens"), "effective": effective,
        "comparison_valid": comparison_valid,
        "termination": terminal, "rejection": rejection,
        "checks": {"input_in_first_seal": journey.first_request_contains_input(data_dir, facts, set(), accepted, submission),
            "first_result_committed_without_r0": committed, "s1_requested_children_admitted": children}}


async def observe(args, case, models):
    import websockets

    started = time.monotonic()
    args.evidence_dir.mkdir()
    data_dir = args.evidence_dir / "installation"
    boundary = str(uuid.uuid4())
    content = case_content(case, args.model)
    summary = {"scope": SCOPE, "case_id": case["id"], "checks": dict.fromkeys(METRICS, False),
               "observation_recorded": False, "evidence_retained": False, "cleanup_complete": True,
               "comparison_valid": False, "errors": [], "interruptions": []}
    process = log = None
    pending_interrupt = None

    def failed(stage, error):
        nonlocal pending_interrupt
        summary["errors"].append(f"{stage}: {type(error).__name__}: {error}")
        if not isinstance(error, Exception):
            if pending_interrupt is None:
                pending_interrupt = error
            summary["interruptions"].append({"stage": stage, "type": type(error).__name__, "message": str(error)})

    try:
        journey.save(args.evidence_dir / "manifest.json", {"schema_version": 1, "scope": SCOPE,
            "case": case, "content": content, "content_sha256": digest(content.encode()),
            "source_revision": args.source_revision, "binary": str(args.binary), "binary_sha256": journey.sha256(args.binary),
            "driver_sha256": journey.sha256(Path(__file__)), "journey_helper_sha256": journey.sha256(Path(journey.__file__)),
            "model": args.model, "model_url": args.model_url, "websockets_version": websockets.__version__,
            "configured_local_request_timeout_ms": os.environ.get("SHENKO_LOCAL_MODEL_REQUEST_TIMEOUT_MS"),
            "startup_timeout_seconds": args.startup_timeout,
            "phase_timeout_seconds": args.timeout, "observer_socket": journey.observer_websocket_options(args.timeout),
            "request_path_timing": args.timing, "startup_boundary": boundary})
        journey.save(args.evidence_dir / "models.json", models)
        process, log, subject = await journey.start(args, "first", data_dir, boundary,
            on_interrupt=lambda error: failed("runtime cleanup interrupted", error))
        await journey.observe_pause(args, process, data_dir, boundary, "first", set())
        submission = {"schema_version": 2, "type": "submit", "submission_id": str(uuid.uuid4()),
            "message_id": str(uuid.uuid4()), "subject_id": subject, "mode": "append", "content": content}
        async with websockets.connect(f"ws://127.0.0.1:{args.port}/ws",
                **journey.observer_websocket_options(args.timeout)) as ws:
            await ws.send(json.dumps(submission, ensure_ascii=False))
            journey.save(args.evidence_dir / "submission.json", submission)
            accepted, _, _ = await journey.admit_then_resume(args, ws, None, process, data_dir,
                "first", submission, boundary, set())
            if (accepted.get("schema_version") != 2 or accepted.get("durability") != "durable"
                    or accepted.get("mode") != submission["mode"] or accepted.get("replayed") is not False
                    or accepted.get("placement", {}).get("requested") != submission["mode"]
                    or accepted.get("placement", {}).get("actual") != "next_boundary"):
                raise AssertionError("the new input lacks the exact durable v2 append acceptance receipt")
            facts, _ = await journey.collect(ws, process, data_dir, args.evidence_dir / "first.websocket.jsonl",
                args.timeout, lambda facts, _: first_observation(facts) is not None)
        journey.save(args.evidence_dir / "first.ledger.json", facts)
        if first_observation(facts) is None:
            raise TimeoutError("first request did not reach its own terminal committed/retired boundary")
        observation = assess(args, case, facts, accepted, submission)
        journey.save(args.evidence_dir / "first.observation.json", observation)
        summary["observation_recorded"] = True
        summary["checks"].update(observation["checks"])
        summary["comparison_valid"] = observation["comparison_valid"]
        summary["request_id"] = observation["request_id"]
        summary["system_prompt_sha256"] = observation["system_prompt_sha256"]
        if not observation["comparison_valid"]:
            raise ValueError("sealed/effective provider, 4096-token budget or native options differ from the frozen comparison")
    except BaseException as error:
        failed("observation", error)
        if hasattr(error, "shenko_startup"):
            summary["startup"] = error.shenko_startup
        if hasattr(error, "shenko_startup_shutdown"):
            summary["startup_shutdown"] = error.shenko_startup_shutdown
            summary["cleanup_complete"] = error.shenko_startup_shutdown["group_cleanup"]["complete"]
            if not error.shenko_startup_shutdown["clean"]:
                summary["errors"].append("startup runtime cleanup was not clean; see startup_shutdown")
        try:
            (args.evidence_dir / "failure.txt").write_text(traceback.format_exc())
        except BaseException as error:
            failed("failure retention", error)
    finally:
        if process is not None:
            try:
                summary["shutdown"] = await journey.stop(process, log,
                    on_interrupt=lambda error: failed("runtime cleanup interrupted", error))
                summary["cleanup_complete"] = summary["shutdown"]["group_cleanup"]["complete"]
                if not summary["shutdown"]["clean"]:
                    summary["errors"].append("runtime cleanup was not clean; see shutdown")
            except BaseException as error:
                summary["cleanup_complete"] = False
                failed("runtime cleanup", error)
        # The observation verdict is already frozen. Capture the tail without
        # promoting a later correction/child result into the first request.
        retained = {}
        try:
            if (data_dir / "shenko.db").exists():
                journey.save(args.evidence_dir / "final.ledger.json", journey.snapshot(data_dir))
            retained["ledger"] = True
        except BaseException as error:
            failed("final ledger retention", error)
        try:
            if (data_dir / "cas").exists():
                destination = args.evidence_dir / "cas"
                shutil.copytree(data_dir / "cas", destination)
                journey.save(args.evidence_dir / "cas-sha256.json", {
                    str(path.relative_to(destination)): journey.sha256(path)
                    for path in sorted(destination.rglob("*")) if path.is_file()})
            retained["cas"] = True
        except BaseException as error:
            failed("CAS retention", error)
        summary["evidence_retained"] = retained == {"ledger": True, "cas": True}
        summary["protocol_passed"] = (all(summary["checks"].values()) and summary["comparison_valid"]
            and summary["evidence_retained"] and not summary["errors"])
        summary["elapsed_seconds"] = time.monotonic() - started
        summary["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        try:
            journey.save(args.evidence_dir / "summary.json", summary)
        except BaseException as error:
            summary["evidence_retained"] = False
            summary["protocol_passed"] = False
            failed("summary retention", error)
        print(json.dumps(summary, ensure_ascii=False))
    if pending_interrupt is not None:
        raise pending_interrupt
    return summary


async def batch(args):
    args.evidence_dir.mkdir(parents=True, exist_ok=False)
    case_table = [{**case, "content": case_content(case, args.model)} for case in CASES]
    encoded_cases = json.dumps(case_table, ensure_ascii=False, sort_keys=True).encode()
    journey.save(args.evidence_dir / "cases.json", case_table)
    report = {"schema_version": 1, "scope": SCOPE, "source_revision": args.source_revision,
        "startup_timeout_seconds": args.startup_timeout,
        "binary_sha256": journey.sha256(args.binary), "canonical_case_table_sha256": digest(encoded_cases),
        "cases_file_sha256": journey.sha256(args.evidence_dir / "cases.json"),
        "requested_runs": len(CASES), "runs": [], "errors": [], "model": args.model,
        "model_url": args.model_url, "model_card_non_identity_fields": ["permission", "created"]}
    pending_interrupt = None

    def checkpoint():
        runs = report["runs"]
        report["attempted_runs"] = len(runs)
        report["unattempted_runs"] = len(CASES) - len(runs)
        report["completed_observations"] = sum(row.get("completed", False) for row in runs)
        report["protocol_passed_runs"] = sum(row.get("protocol_passed", False) for row in runs)
        report["protocol_failed_runs"] = len(runs) - report["protocol_passed_runs"]
        report["rates"] = {}
        for key in METRICS:
            passed = sum(bool(row.get("completed") and row.get("comparison_valid") and not row["errors"]
                and row.get("checks", {}).get(key)) for row in runs)
            report["rates"][key] = {"passed": passed, "attempted": len(runs),
                "rate": passed / len(runs) if runs else None}
        journey.save(args.evidence_dir / "batch-summary.json", report)

    def identity():
        models = journey.get_json(args.model_url.rstrip("/") + "/v1/models")
        cards = [card for card in models.get("data", []) if card.get("id") == args.model]
        if len(cards) != 1:
            raise ValueError("configured model must have one unambiguous served model card")
        return models, {key: value for key, value in cards[0].items() if key not in {"permission", "created"}}

    checkpoint()
    try:
        initial_models, report["model_card"] = identity()
        journey.save(args.evidence_dir / "models.initial.json", initial_models)
        checkpoint()
        for index, case in enumerate(CASES, 1):
            models, card = identity()
            if card != report["model_card"] or journey.sha256(args.binary) != report["binary_sha256"]:
                raise RuntimeError("frozen binary or served model card changed before the next case")
            attempt = copy.copy(args)
            attempt.evidence_dir = args.evidence_dir / f"case-{index:02d}"
            row = {"case_id": case["id"], "directory": attempt.evidence_dir.name,
                   "completed": False, "protocol_passed": False, "errors": []}
            report["runs"].append(row)
            checkpoint()  # Preserve the attempted denominator before spawn.
            try:
                summary = await observe(attempt, case, models)
                row.update(completed=summary["observation_recorded"] and summary["evidence_retained"],
                    protocol_passed=summary["protocol_passed"], checks=summary["checks"],
                    comparison_valid=summary["comparison_valid"], errors=list(summary["errors"]),
                    cleanup_complete=summary["cleanup_complete"],
                    summary_sha256=journey.sha256(attempt.evidence_dir / "summary.json"))
                models_after, card_after = identity()
                journey.save(attempt.evidence_dir / "models.after.json", models_after)
                if card_after != report["model_card"] or journey.sha256(args.binary) != report["binary_sha256"]:
                    row["comparison_valid"] = False
                    raise RuntimeError("frozen binary or served model card changed within a case")
                if summary["observation_recorded"] and not summary["comparison_valid"]:
                    raise RuntimeError("sealed/effective inference configuration differs from the comparison contract")
                if not summary["cleanup_complete"] or not summary["evidence_retained"]:
                    raise RuntimeError("previous owned runtime cleanup or evidence retention is incomplete")
            except BaseException as error:
                row["protocol_passed"] = False
                row["errors"].append(f"{type(error).__name__}: {error}")
                raise
            finally:
                checkpoint()
    except BaseException as error:
        report["errors"].append(f"{type(error).__name__}: {error}")
        if not isinstance(error, Exception):
            pending_interrupt = error
    finally:
        report["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        try:
            checkpoint()
        except BaseException as error:
            report["errors"].append(f"batch summary retention: {type(error).__name__}: {error}")
            if pending_interrupt is None and not isinstance(error, Exception):
                pending_interrupt = error
        print(json.dumps(report, ensure_ascii=False, indent=2))
    if pending_interrupt is not None:
        raise pending_interrupt
    return 0 if (len(report["runs"]) == len(CASES) and not report["errors"]
        and all(row["completed"] and row["protocol_passed"] and not row["errors"] for row in report["runs"])) else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--model-url", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--evidence-dir", required=True, type=Path)
    parser.add_argument("--port", type=int, default=17861)
    parser.add_argument("--timeout", type=int, default=900)
    parser.add_argument("--startup-timeout", type=int, default=60,
                        help="Seconds to observe a bound subject after each spawn (default: 60)")
    parser.add_argument("--timing", action="store_true")
    args = parser.parse_args()
    if args.timeout < 1 or args.startup_timeout < 1:
        parser.error("--timeout and --startup-timeout must be positive")
    args.binary = args.binary.resolve()
    args.evidence_dir = args.evidence_dir.resolve()
    raise SystemExit(asyncio.run(batch(args)))


if __name__ == "__main__":
    main()
