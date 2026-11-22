#!/usr/bin/env python3
"""Derive links from the retained 0c evidence; no runtime or pass reclassification.

Read packets.tar.gz without extracting it. The sibling final-0c6b013e directory
holds the original ledger/browser receipts, which the packet only identifies.
Print deterministic JSON. Semantic conclusions belong to the accompanying note.
"""
import base64
import gzip
import hashlib
import json
from pathlib import Path
import tarfile


HERE = Path(__file__).resolve().parent
ORIGINALS = HERE.parent / "final-0c6b013e"


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def identity(raw):
    return {"bytes": len(raw), "sha256": sha(raw)}


def read_public(attempt, name):
    path = ORIGINALS / attempt / name
    if not path.is_file():
        path = Path(str(path) + ".gz")
    raw = path.read_bytes()
    decoded = gzip.decompress(raw) if path.suffix == ".gz" else raw
    return decoded, {"path": str(path.relative_to(HERE.parent)), **identity(raw),
                     "decoded_bytes": len(decoded), "decoded_sha256": sha(decoded)}


def response_stream(response):
    if response is None:
        return {"response_present": False}
    raw = response["raw_response"]
    content, reasons, models, fields = [], [], set(), set()
    for line in raw.splitlines():
        if not line or line == "[DONE]":
            continue
        chunk = json.loads(line)
        if chunk.get("model"):
            models.add(chunk["model"])
        for choice in chunk.get("choices", []):
            delta = choice.get("delta") or {}
            if isinstance(delta.get("content"), str):
                content.append(delta["content"])
            fields.update(key for key in ("reasoning", "reasoning_content") if delta.get(key))
            if choice.get("finish_reason") is not None:
                reasons.append(choice["finish_reason"])
    return {"response_present": True, "raw_response_utf8": identity(raw.encode()),
            "content_utf8": identity(response["content"].encode()),
            "delta_content_equals_stored_content": "".join(content) == response["content"],
            "models": sorted(models), "finish_reasons": reasons,
            "separate_reasoning_fields": sorted(fields), "content": response["content"]}


def main():
    packet_raw = (HERE / "packets.tar.gz").read_bytes()
    result = {"schema_version": 1, "source_commit": "0c6b013e7c0cc8c27e30a83189c14a36c71aaa74",
              "scope": "Post-run evidence links, not a new test or changed journey verdict.",
              "packets_tar_gz": identity(packet_raw), "attempts": []}
    with tarfile.open(HERE / "packets.tar.gz") as archive:
        for number in (1, 2, 3):
            attempt = f"attempt-{number:03d}"
            packet_bytes = archive.extractfile(f"{attempt}/audit.json").read()
            packet = json.loads(packet_bytes)

            def object_bytes(reference):
                entry = packet["objects"][reference["digest"]]
                raw = archive.extractfile(f"{attempt}/{entry['export']}").read()
                if len(raw) != entry["actual_bytes"] or sha(raw) != entry["sha256"]:
                    raise ValueError(f"retained object SHA mismatch: {reference['digest']}")
                return raw

            def object_identity(reference):
                return {"reference": reference, **identity(object_bytes(reference))}

            for entry in packet["objects"].values():
                object_bytes(entry["references"][0])
            source_checks = []
            for name, expected in packet["source_artifacts"].items():
                decoded, actual = read_public(attempt, name)
                actual["matches_exported_decoded_bytes"] = identity(decoded) == {
                    "bytes": expected["decoded_bytes"], "sha256": expected["decoded_sha256"]}
                source_checks.append(actual)
            facts = json.loads(read_public(attempt, "final.ledger.json")[0])
            steps = {row["step_id"]: row["data"] for row in facts["subject_steps"]}
            requests = {row["attempt"]["request_id"]: row for row in packet["requests"]}
            source_input = packet["metadata"]["first.input.json"]
            receipt = packet["metadata"]["browser/summary.json"]["observations"]["first_input_mode"]["accepted_receipt"]
            web_row = facts["web_input_submissions"][0]
            output = {"attempt": attempt, "audit_json": identity(packet_bytes),
                      "exporter_sha256": packet["exporter_sha256"],
                      "retained_objects_checked_with_sha256": len(packet["objects"]),
                      "blake3_independently_checked": False,
                      "source_receipt_checks": source_checks, "source_summary": packet["metadata"]["summary.json"],
                      "original_submission": source_input, "accepted_receipt": receipt,
                      "web_submission_matches": (web_row["submission_id"] == source_input["submission_id"] == receipt["submission_id"]
                          and web_row["event_id"] == receipt["event_id"]
                          and web_row["payload"]["content"] == source_input["content"]),
                      "requests": [], "committed_decisions": [], "native_originals": [],
                      "final_counts": packet["snapshots"]["final.ledger.json"]["counts"],
                      "history_tool_result_rows": [row for row in packet["original_history"] if row["event_type"] == "tool_result"],
                      "final_browser": {}}
            for item in packet["requests"]:
                request = item["attempt"]
                plan = json.loads(object_bytes(request["input_plan_ref"]))
                terminal_ref = request["terminal"]["result_ref"]
                terminal = json.loads(object_bytes(terminal_ref))
                step = steps[request["step_id"]]
                messages = plan["payload"]["messages"]
                submitted = f"[event {receipt['event_id']}] [web_ui] {source_input['content']}"
                output["requests"].append({
                    "request_id": request["request_id"], "step_id": request["step_id"],
                    "final_step_state": step["state"], "created_commit": request["created_commit"],
                    "terminal_recorded_commit": request["terminal"]["recorded_commit"],
                    "input_plan": object_identity(request["input_plan_ref"]),
                    "input_identity_matches": (plan["request_id"] == request["request_id"] and plan["step_id"] == request["step_id"]),
                    "input_binding_equals_effective": plan["provider_binding"] == terminal["effective"]["provider_binding"],
                    "effective": terminal["effective"], "input_max_tokens": plan["payload"]["max_tokens"],
                    "system_prompt_utf8": identity(plan["payload"]["system_prompt"].encode()),
                    "messages": messages, "events": plan["events"], "sealed_context": plan["context"],
                    "original_submission_payload_parts": [{"message_index": mi, "part_index": pi}
                        for mi, message in enumerate(messages) for pi, part in enumerate(message["parts"])
                        if message["role"] == "user" and part.get("text") == submitted],
                    "runtime_tool_result_parts": [{"message_index": mi, "part_index": pi, "text": part["text"]}
                        for mi, message in enumerate(messages) for pi, part in enumerate(message["parts"])
                        if part.get("type") == "text" and part["text"].startswith("[runtime tool_result ")],
                    "terminal_result": object_identity(terminal_ref), "terminal_status": terminal["status"],
                    "termination": terminal["termination"], "stream": terminal["stream"],
                    "wire_content": response_stream(terminal["response"]),
                    "model_result_text_equals_response": (
                        object_bytes(step["model_result_ref"]).decode() == terminal["response"]["content"]
                        if step["model_result_ref"] is not None and terminal["response"] is not None else None),
                    "model_result_ref": step["model_result_ref"], "retry_reason_ref": step["retry_reason_ref"]})
            for item in packet["decisions"]:
                step = steps[item["step_id"]]
                decision = json.loads(object_bytes(step["decision_ref"]))
                request = requests[item["active_request_id"]]
                text = object_bytes(decision["model_result_ref"]).decode()
                body = decision["output"]["text"]
                commit = f"{item['commit_ref']['ledger_id']}/{item['commit_ref']['seq']}"
                frames = []
                for frame in packet["browser_timeline_parts"]:
                    message = frame["record"]["message"]
                    if message["step_id"] == item["step_id"]:
                        frames.append({**frame, "body_equals_decision": message["content"] == body,
                                       "commit_equals_decision": message["commit_ref"] == commit})
                output["committed_decisions"].append({"step_id": item["step_id"], "request_id": item["active_request_id"],
                    "commit_ref": item["commit_ref"], "decision": object_identity(step["decision_ref"]),
                    "snapshot_and_cas_equal": decision == item["decision"],
                    "request_input_ref_equals_decision": request["attempt"]["input_plan_ref"] == decision["input_plan_ref"],
                    "stored_model_text_equals_provider_content": text == request["provider_response"]["content"],
                    "output_text": body, "output_utf8": identity(body.encode()),
                    "created_tasks": [{"task_id": task["task_id"], "kind": task["kind"],
                        "same_commit": task["created_commit"] == item["commit_ref"],
                        "same_decision_input": task["input_ref"] == step["decision_ref"]}
                        for task in packet["tasks"].values() if task.get("created_by_step_id") == item["step_id"]],
                    "browser_committed_frames": frames})
            for row in facts["evidence_receipts"]:
                receipt_data = row["data"]
                raw_ref, normalized_ref = receipt_data["raw_ref"], receipt_data["normalized_ref"]
                raw, normalized = json.loads(object_bytes(raw_ref)), json.loads(object_bytes(normalized_ref))
                if raw_ref["codec"]["name"] == "host-printf-observation":
                    raw = {**raw, "decoded_stdout": base64.b64decode(raw["stdout_b64"]).decode(),
                           "decoded_stderr": base64.b64decode(raw["stderr_b64"]).decode()}
                original = {"receipt": receipt_data,
                    "raw_object": object_identity(raw_ref), "raw": raw,
                    "normalized_object": object_identity(normalized_ref), "normalized": normalized}
                operation = next(item["data"] for item in facts["operations"]
                                 if item["operation_id"] == receipt_data["operation_id"])
                input_ref = operation["final_input_ref"]
                frozen = json.loads(object_bytes(input_ref))
                original["original_operation"] = operation
                original["frozen_input_object"] = object_identity(input_ref)
                original["frozen_input"] = frozen
                if raw_ref["codec"]["name"] == "model-child-provider-result":
                    original["child_request_and_binding_chain_matches"] = (
                        frozen["request_id"] == raw["request_id"] == normalized["request_id"]
                        and frozen["provider_binding"] == raw["effective"]["provider_binding"] == normalized["provider_binding"]
                        and operation["task_id"] == normalized["task_id"])
                output["native_originals"].append(original)
            for name in ("final.dom.json", "final.aria.yml", "final.png"):
                decoded, source = read_public(attempt, "browser/" + name)
                output["final_browser"][name] = source
                if name == "final.dom.json":
                    output["final_browser"]["dom_rows"] = json.loads(decoded)["rows"]
            result["attempts"].append(output)
    rejected_raw = (HERE / "attempt-003-control-rejection.json").read_bytes()
    rejected = json.loads(rejected_raw)
    retired = steps[rejected["supersedes_step_id"]]
    replacement = steps[rejected["replacement_step_id"]]
    result["original_control_rejection"] = {**identity(rejected_raw), "value": rejected,
        "source_reference": retired["retry_reason_ref"],
        "bytes_equal_source_reference_length": len(rejected_raw) == retired["retry_reason_ref"]["bytes"],
        "retired_input_and_results_match": all(retired[key] == rejected[key]
            for key in ("input_plan_ref", "model_result_ref", "model_outcome_ref")),
        "retired_has_no_decision_or_commit": retired["state"] == "retired" and retired["decision_ref"] is None and retired["commit_ref"] is None,
        "replacement_is_committed": replacement["state"] == "committed"}
    result["semantic_verdict"] = "Manual interpretation in README.md; original journey denominator stays 0/3."
    print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
