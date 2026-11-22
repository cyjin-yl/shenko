#!/usr/bin/env python3
"""Derive original d16 evidence links; no DB, model, replay, or verdict change.

The packet archives preserve selected exact CAS bytes and closed-store exports.
The separate final-d16adbf8 directory preserves the original ledger/browser logs.
SHA256/length checks here do not recompute BLAKE3 or revalidate a full CAS closure.
Print deterministic JSON. Human semantic and screenshot review is in README.md.
"""
import argparse
import base64
import gzip
import hashlib
import json
from pathlib import Path
import re
import tarfile


HERE = Path(__file__).resolve().parent


def identity(raw):
    return {"bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}


def value(raw):
    return json.loads(raw) if isinstance(raw, (str, bytes)) else raw


def public_file(root, attempt, name):
    path = root / attempt / name
    if not path.is_file():
        path = Path(str(path) + ".gz")
    raw = path.read_bytes()
    decoded = gzip.decompress(raw) if path.suffix == ".gz" else raw
    return decoded, {"path": str(path.relative_to(root)), **identity(raw),
                     "decoded_bytes": len(decoded), "decoded_sha256": identity(decoded)["sha256"]}


def provider_content(response):
    if response is None:
        return {"response_present": False}
    raw = response["raw_response"]
    content, finishes, models, reasoning, forms = [], [], set(), set(), set()
    for line in raw.splitlines():
        if line.startswith("data: "):
            line = line[6:]
        if not line or line == "[DONE]":
            continue
        chunk = json.loads(line)
        if chunk.get("model"):
            models.add(chunk["model"])
        for choice in chunk.get("choices", []):
            # Subject uses streaming deltas; these finite children returned
            # ordinary chat.completion messages. Neither reasoning field is
            # silently concatenated into the answer channel.
            for field in ("delta", "message"):
                part = choice.get(field)
                if isinstance(part, dict):
                    forms.add(field)
                    if isinstance(part.get("content"), str):
                        content.append(part["content"])
                    reasoning.update(key for key in ("reasoning", "reasoning_content") if part.get(key))
            if choice.get("finish_reason") is not None:
                finishes.append(choice["finish_reason"])
    return {"response_present": True, "raw_response_utf8": identity(raw.encode()),
            "content_utf8": identity(response["content"].encode()),
            "wire_content_equals_stored_content": "".join(content) == response["content"],
            "wire_content_fields": sorted(forms), "models": sorted(models),
            "finish_reasons": finishes, "separate_reasoning_fields": sorted(reasoning),
            "content": response["content"]}


def result_presence(plan, attempt, result, history):
    matches = []
    for mi, message in enumerate(plan["payload"]["messages"]):
        if message["role"] != "user":
            continue
        for pi, part in enumerate(message["parts"]):
            header, newline, body = part.get("text", "").partition("\n")
            prefix = f"[event {result['result_id']}] "
            if (newline and header.startswith(f"[runtime tool_result call_id={result['task_id']} ")
                    and header.endswith("]") and body.startswith(prefix)
                    and json.loads(body[len(prefix):]) == result):
                matches.append({"message_index": mi, "part_index": pi, "header": header})
    def same(event):
        return (event.get("source") == "native-child" and event.get("kind") == "task_result"
                and event.get("event_id") == result["result_id"] and event.get("payload") == result)
    fresh = [i for i, event in enumerate(plan["events"]) if same(event)]
    source = plan["context"]["source"]
    original_rows = [{"event_id": row["event_id"], "scope": row["session_id"], "seq": row["seq"],
        "same_scope": row["session_id"] == source["scope_id"], "within_leaf": row["seq"] <= source["leaf_seq"]}
        for row in history if row["event_type"] == "tool_result" and same(row["data"].get("inbox_event", {}))]
    before = (attempt["created_commit"]["ledger_id"] == result["commit_ref"]["ledger_id"]
              and result["commit_ref"]["seq"] < attempt["created_commit"]["seq"])
    return {"result_id": result["result_id"], "task_id": result["task_id"],
            "operation_id": result["operation_id"], "receipt_id": result["receipt_id"],
            "t2_commit": result["commit_ref"], "before_this_request_creation": before,
            "new_event_indices": fresh, "original_history_rows": original_rows,
            "exact_payload_parts": matches,
            "causal_input_present": bool(before and matches and
                (fresh or any(row["same_scope"] and row["within_leaf"] for row in original_rows)))}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--originals-dir", type=Path, default=HERE.parent / "final-d16adbf8")
    args = parser.parse_args()
    output = {"schema_version": 1, "product_revision": "d16adbf89c9f79dfce8ef90d77843412bcda85bf",
              "scope": "Independent post-run evidence links; no new model/test execution or changed original verdict.",
              "original_journey_denominator": {"attempted": 3, "passed": 0, "failed": 3},
              "attempts": []}
    for number in (1, 2, 3):
        name = f"attempt-{number:03d}"
        packet_path = HERE / (name + ".tar.gz")
        with tarfile.open(packet_path) as archive:
            prefix = f"causal-audit-d16adbf8/{name}"
            native_prefix = f"native-proof-d16adbf8/{name}"
            packet_raw = archive.extractfile(prefix + "/audit.json").read()
            native_raw = archive.extractfile(native_prefix + "/native-proof.json").read()
            packet, native = json.loads(packet_raw), json.loads(native_raw)
            all_objects, object_checks = {}, []
            for family, document in ((prefix, packet), (native_prefix, native)):
                for digest, entry in document["objects"].items():
                    raw = archive.extractfile(f"{family}/{entry['export']}").read()
                    expected_bytes = entry.get("actual_bytes", entry.get("bytes"))
                    assert len(raw) == expected_bytes and identity(raw)["sha256"] == entry["sha256"]
                    assert entry["status"] == "retained" and all(ref["bytes"] == len(raw) for ref in entry["references"])
                    assert digest not in all_objects or all_objects[digest] == raw
                    all_objects[digest] = raw
                    object_checks.append({"path": f"{family}/{entry['export']}", "digest": digest,
                        **identity(raw), "typed_references": entry["references"],
                        "blake3_verified_by_exporter": entry["blake3_verified"]})

            def cas(ref):
                raw = all_objects[ref["digest"]]
                assert len(raw) == ref["bytes"]
                return raw

            def ref_identity(ref):
                return {"reference": ref, **identity(cas(ref))}

            sources = []
            for path, expected in packet["source_artifacts"].items():
                decoded, info = public_file(args.originals_dir, name, path)
                info["matches_exported_decoded_bytes"] = (len(decoded) == expected["decoded_bytes"]
                    and identity(decoded)["sha256"] == expected["decoded_sha256"])
                assert info["matches_exported_decoded_bytes"]
                sources.append(info)
            facts = json.loads(public_file(args.originals_dir, name, "final.ledger.json")[0])
            steps = {row["step_id"]: row["data"] for row in facts["subject_steps"]}
            request_index = {row["attempt"]["request_id"]: row for row in packet["requests"]}
            result_index = {row["result_id"]: row for row in packet["results"]}
            source_input = packet["metadata"]["first.input.json"]
            accepted = packet["metadata"]["browser/summary.json"]["observations"]["first_input_mode"]["accepted_receipt"]
            row = next(row for row in facts["web_input_submissions"] if row["submission_id"] == accepted["submission_id"])
            item = {"attempt": name, "packet_archive": identity(packet_path.read_bytes()),
                "semantic_packet": identity(packet_raw), "native_packet": identity(native_raw),
                "exporter_sha256": packet["exporter_sha256"], "native_exporter_sha256": native["exporter_sha256"],
                "source_receipt_checks": sources, "cas_checks": object_checks,
                "export_errors": packet["errors"] + native["errors"],
                "original_summary": packet["metadata"]["summary.json"],
                "original_input": source_input, "accepted_receipt": accepted,
                "web_submission_identity_matches": (row["submission_id"] == source_input["submission_id"]
                    and row["event_id"] == accepted["event_id"] and row["payload"]["content"] == source_input["content"]),
                "final_counts": packet["snapshots"]["final.ledger.json"]["counts"],
                "requests": [], "committed_decisions": [], "original_results": packet["results"],
                "native_receipts": [], "native_proof": {}, "browser": {},
                "runtime_error_history": [row for row in facts["session_events"] if row["event_type"] == "runtime_error"]}
            for observed in packet["requests"]:
                attempt = observed["attempt"]
                plan = json.loads(cas(attempt["input_plan_ref"]))
                step = steps[attempt["step_id"]]
                terminal_ref = attempt["terminal"]["result_ref"]
                terminal = json.loads(cas(terminal_ref))
                submitted = f"[event {accepted['event_id']}] [web_ui] {source_input['content']}"
                item["requests"].append({"request_id": attempt["request_id"], "step_id": attempt["step_id"],
                    "final_step_state": step["state"], "created_commit": attempt["created_commit"],
                    "terminal_recorded_commit": attempt["terminal"]["recorded_commit"],
                    "input_plan": ref_identity(attempt["input_plan_ref"]),
                    "plan_request_and_step_identity_matches": (plan["request_id"] == attempt["request_id"]
                        and plan["step_id"] == attempt["step_id"]),
                    "sealed_context_source": plan["context"]["source"], "payload_messages": plan["payload"]["messages"],
                    "sealed_events": plan["events"], "system_prompt_utf8": identity(plan["payload"]["system_prompt"].encode()),
                    "input_max_tokens": plan["payload"]["max_tokens"],
                    "input_binding_equals_effective": plan["provider_binding"] == terminal["effective"]["provider_binding"],
                    "effective": terminal["effective"],
                    "submitted_input_payload_parts": [{"message_index": mi, "part_index": pi}
                        for mi, message in enumerate(plan["payload"]["messages"]) for pi, part in enumerate(message["parts"])
                        if message["role"] == "user" and part.get("text") == submitted],
                    "result_presence": [result_presence(plan, attempt, result, facts["session_events"])
                                        for result in packet["results"]],
                    "terminal_result": ref_identity(terminal_ref), "status": terminal["status"],
                    "termination": terminal["termination"], "wire": provider_content(terminal["response"]),
                    "model_text_equals_provider_content": (cas(step["model_result_ref"]).decode() == terminal["response"]["content"]
                        if step["model_result_ref"] and terminal["response"] else None),
                    "retry_reason_ref": step["retry_reason_ref"]})
            request_details = {row["request_id"]: row for row in item["requests"]}
            for original in packet["decisions"]:
                step = steps[original["step_id"]]
                decision = json.loads(cas(step["decision_ref"]))
                request = request_index[step["active_request_id"]]
                body = decision["output"]["text"]
                commit = f"{step['commit_ref']['ledger_id']}/{step['commit_ref']['seq']}"
                frames = [{**frame, "exact_body": frame["record"]["message"]["content"] == body,
                    "exact_commit": frame["record"]["message"]["commit_ref"] == commit}
                    for frame in packet["browser_timeline_parts"]
                    if frame["record"]["message"]["step_id"] == step["step_id"]]
                item["committed_decisions"].append({"step_id": step["step_id"], "request_id": step["active_request_id"],
                    "commit_ref": step["commit_ref"], "decision": ref_identity(step["decision_ref"]),
                    "decision_cas_equals_snapshot": decision == original["decision"],
                    "same_request_input_ref": decision["input_plan_ref"] == request["attempt"]["input_plan_ref"],
                    "provider_text_equals_decision_model_text": cas(decision["model_result_ref"]).decode() == request["provider_response"]["content"],
                    "output_text": body, "output_utf8": identity(body.encode()),
                    "result_presence": request_details[step["active_request_id"]]["result_presence"],
                    "created_tasks": [{"task_id": task["task_id"], "kind": task["kind"],
                        "same_commit": task["created_commit"] == step["commit_ref"],
                        "same_decision_ref": task["input_ref"] == step["decision_ref"]}
                        for task in packet["tasks"].values() if task.get("created_by_step_id") == step["step_id"]],
                    "received_committed_frames": frames})
            for row in facts["evidence_receipts"]:
                receipt = row["data"]
                raw, normalized = json.loads(cas(receipt["raw_ref"])), json.loads(cas(receipt["normalized_ref"]))
                operation = next(row["data"] for row in facts["operations"] if row["operation_id"] == receipt["operation_id"])
                frozen = json.loads(cas(operation["final_input_ref"]))
                detail = {"receipt": receipt, "raw_object": ref_identity(receipt["raw_ref"]),
                    "normalized_object": ref_identity(receipt["normalized_ref"]), "normalized": normalized,
                    "operation": operation, "frozen_input": ref_identity(operation["final_input_ref"])}
                if receipt["raw_ref"]["codec"]["name"] == "host-printf-observation":
                    detail["raw_observation"] = {**raw, "decoded_stdout": base64.b64decode(raw["stdout_b64"]).decode(),
                                                "decoded_stderr": base64.b64decode(raw["stderr_b64"]).decode()}
                    detail["frozen_input_value"] = frozen
                elif receipt["raw_ref"]["codec"]["name"] == "model-child-provider-result":
                    detail["provider_wire"] = provider_content(raw["response"])
                    detail["provider_termination"] = raw["termination"]
                    detail["request_and_binding_chain_matches"] = (frozen["request_id"] == raw["request_id"] == normalized["request_id"]
                        and frozen["provider_binding"] == raw["effective"]["provider_binding"] == normalized["provider_binding"]
                        and operation["task_id"] == normalized["task_id"])
                    detail["transcript_text_equals_wire_and_cas"] = (normalized["text"] == raw["response"]["content"]
                        and cas(normalized["output_ref"]).decode() == normalized["text"])
                else:
                    detail["raw_equals_normalized"] = raw == normalized
                item["native_receipts"].append(detail)
            materializations = {row["action_id"]: value(row["data"]) for row in native["sql_raw"]["related_control_materializations"]}
            t2 = []
            for row in native["t2"]:
                transition = json.loads(cas(row["reference"]))
                marker = materializations.get(row["action_id"])
                result = transition["result"]
                t2.append({"action_id": row["action_id"], "transition": ref_identity(row["reference"]),
                    "transition_equals_export": transition == row["transition"],
                    "result_id": result["result_id"], "task_id": result["task_id"], "logical_commit": result["commit_ref"],
                    "actual_task_result_exists": result["result_id"] in result_index,
                    "actual_task_result_equals_prepared": result_index.get(result["result_id"]) == result,
                    "materialization": marker, "marker_transition_matches": marker.get("transition_ref") == row["reference"] if marker else None,
                    "marker_logical_result_matches": marker.get("logical_result") == result["commit_ref"] if marker else None,
                    "evidence_fact": transition.get("evidence_fact")})
            e1 = []
            for row in native["facts"]:
                envelope = json.loads(cas(row["root_reference"]))
                fact = json.loads(cas(envelope["fact_ref"]))
                arm = fact["arm"]
                marker = materializations.get(arm["admission"]["action_id"])
                e1.append({"receipt_id": row["receipt_id"], "root": ref_identity(row["root_reference"]),
                    "inner": ref_identity(envelope["fact_ref"]), "envelope": envelope,
                    "inner_equals_export": fact == row["fact"],
                    "receipt_exactly_matches_final_snapshot": row["receipt_exactly_matches_final_snapshot"],
                    "predecessors_exactly_original_arm_payload": envelope["predecessor_roots"] == [arm["admission"]["payload_ref"]],
                    "arm_transition": ref_identity(arm["transition_ref"]),
                    "arm_payload": ref_identity(arm["admission"]["payload_ref"]),
                    "arm_materialization": marker,
                    "arm_marker_admission_and_transition_match": bool(marker and marker["admission"] == arm["admission"]
                        and marker["transition_ref"] == arm["transition_ref"]), "e1_marker": row["marker"]})
            item["native_proof"] = {"observed_at": native["observed_at"],
                "transaction_started": native["sqlite_transaction_started_at"], "transaction_finished": native["sqlite_transaction_finished_at"],
                "closed_process_receipt": native["closed_process_receipt"],
                "closed_receipts_equal_final_snapshot": native["closed_receipts_equal_final_snapshot"],
                "final_seal_debts": native["sql_raw"]["native_evidence_seal_debts"],
                "debt_limit": "Final read only; an empty final table does not prove no debt ever occurred.",
                "sqlite_files_before": native["sqlite_files_before"], "sqlite_files_after": native["sqlite_files_after"],
                "sqlite_observed_changes": {Path(path).name: [key for key, old in entry.items() if native["sqlite_files_after"][path].get(key) != old]
                    for path, entry in native["sqlite_files_before"].items()}, "anchored_e1": e1, "t2": t2,
                "scope_limit": "One closed Store transaction per installation; no Node DB or full predecessor closure verification by this audit."}
            for name_in_browser in ("final.dom.json", "final.aria.yml", "final.png"):
                decoded, info = public_file(args.originals_dir, name, "browser/" + name_in_browser)
                item["browser"][name_in_browser] = info
                if name_in_browser == "final.dom.json":
                    item["browser"]["captured_dom"] = json.loads(decoded)
            log, info = public_file(args.originals_dir, name, "first.runtime.log")
            lines = log.decode().splitlines()
            item["runtime_log"] = {"source": info, "warning_error_lines": [{"line": index, "text": re.sub(r"\x1b\[[0-9;]*m", "", line)}
                for index, line in enumerate(lines, 1) if " WARN" in line or " ERROR" in line],
                "final_lines": [re.sub(r"\x1b\[[0-9;]*m", "", line) for line in lines[-8:]]}
            output["attempts"].append(item)
    output["limits"] = ["Semantic judgments and screenshot inspection are manual, in README.md.",
        "The frozen semantic exporter result_presence field omitted the production [event id] prefix; this derivation matches that prefix and complete original TaskResult JSON explicitly.",
        "Raw CAS bytes and typed references are retained; independent BLAKE3 and complete predecessor closure validation were not performed.",
        "Received timeline frames bind S1 content/commit; final DOM/PNG are separate visibility observations and do not invent the failed Inspect action.",
        "All three original full journeys remain failed; no restart/recall phase was reached."]
    print(json.dumps(output, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
