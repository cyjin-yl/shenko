#!/usr/bin/env python3
"""Read retained journey evidence and export a bounded causal audit packet.

No runtime/SQLite access, network, model call, or pass/fail reclassification.
Exact tool-result matches are provenance aids; semantic judgments remain manual.
Missing fresh events are not failures: the same original can be in real history.
"""
import argparse
import datetime
import gzip
import hashlib
import json
from pathlib import Path
import re

try:
    import blake3
except ImportError:
    blake3 = None


def raw_file(path):
    raw = path.read_bytes()
    return raw, gzip.decompress(raw) if path.suffix == ".gz" else raw


def json_value(value):
    if isinstance(value, str):
        try:
            return json.loads(value)
        except ValueError:
            pass
    return value


def record(row):
    return json_value(row.get("data", row))


def object_ref(value):
    return isinstance(value, dict) and isinstance(value.get("digest"), str) and "codec" in value and "bytes" in value


def references(value, label):
    if object_ref(value):
        yield value, label
    elif isinstance(value, dict):
        for key, item in value.items():
            yield from references(item, f"{label}/{key}")
    elif isinstance(value, list):
        for index, item in enumerate(value):
            yield from references(item, f"{label}/{index}")


class Export:
    # Only semantic request/result bodies, never control-predecessor closures,
    # installed source trees, authority graphs, or arbitrary ObjectRef locators.
    FOLLOW = {"subject_step_input", "StepDecision", "SubjectModelOutcome",
              "subject-request-result", "model-child-frozen-input",
              "model-child-step-input", "model-child-result",
              "model-child-provider-result", "model-child-step-record",
              "model-child-text", "host-printf-frozen-input",
              "host-printf-observation", "host-printf-result"}

    def __init__(self, source, cas, output):
        self.source, self.cas, self.output = source, cas, output
        self.objects, self.decoded, self.sources, self.errors = {}, {}, {}, []

    def source_json(self, name):
        path = self.source / name
        if not path.is_file():
            path = self.source / (name + ".gz")
        if not path.is_file():
            return None
        raw, decoded = raw_file(path)
        self.sources[str(path.relative_to(self.source))] = {
            "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest(),
            "decoded_bytes": len(decoded), "decoded_sha256": hashlib.sha256(decoded).hexdigest()}
        return json.loads(decoded)

    def retain(self, ref, use):
        if not object_ref(ref):
            return None
        digest = ref["digest"]
        if digest in self.objects:
            entry = self.objects[digest]
            entry["uses"].append(use)
            if ref not in entry["references"]:
                entry["references"].append(ref)
            if entry.get("actual_bytes") is not None and ref["bytes"] != entry["actual_bytes"]:
                self.errors.append({"use": use, "error": "alias length mismatch", "reference": ref})
                return None
            return self.decoded.get(digest)
        entry = self.objects[digest] = {"references": [ref], "uses": [use]}
        if not re.fullmatch(r"blake3:[0-9a-f]{64}", digest):
            entry["status"] = "invalid_digest"
            return None
        hexdigest = digest[7:]
        relative = Path("cas") / hexdigest[:2] / hexdigest[2:4] / hexdigest
        path = self.cas / hexdigest[:2] / hexdigest[2:4] / hexdigest
        try:
            raw = path.read_bytes()
        except OSError as error:
            entry.update(status="unavailable", error=str(error))
            return None
        entry.update(actual_bytes=len(raw), sha256=hashlib.sha256(raw).hexdigest(),
                     blake3_verified=(blake3.blake3(raw).hexdigest() == hexdigest if blake3 else None))
        target = self.output / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(raw)
        entry["export"] = str(relative)
        if len(raw) != ref["bytes"] or entry["blake3_verified"] is False:
            entry["status"] = "integrity_mismatch"
            return None
        entry["status"] = "retained"
        try:
            value = json.loads(raw)
        except (ValueError, UnicodeError):
            return None
        self.decoded[digest] = value
        for child, label in references(value, use):
            if child.get("codec", {}).get("name") in self.FOLLOW or label.endswith("/output_ref"):
                self.retain(child, label)
        return value


def result_presence(plan, result, history):
    payload_matches = []
    for mi, message in enumerate(plan.get("payload", {}).get("messages", [])):
        for pi, part in enumerate(message.get("parts", [])):
            if part.get("type") != "text":
                continue
            text = part.get("text", "")
            prefix, separator, body = text.partition("\n")
            if (message.get("role") == "user" and separator
                    and prefix.startswith(f"[runtime tool_result call_id={result['task_id']} ")
                    and json_value(body) == result):
                payload_matches.append({"message_index": mi, "part_index": pi, "header": prefix})
    fresh = [index for index, event in enumerate(plan.get("events", []))
             if event.get("source") == "native-child" and event.get("kind") == "task_result"
             and event.get("event_id") == result["result_id"] and event.get("payload") == result]
    source = plan.get("context", {}).get("source", {})
    originals = []
    for row in history:
        data = record(row)
        event = data.get("inbox_event", {}) if isinstance(data, dict) else {}
        if (row.get("event_type") == "tool_result" and event.get("source") == "native-child"
                and event.get("event_id") == result["result_id"] and event.get("payload") == result):
            originals.append({"event_id": row["event_id"], "scope": row["session_id"], "seq": row["seq"],
                              "same_sealed_scope": row["session_id"] == source.get("scope_id"),
                              "within_sealed_leaf": row["seq"] <= source.get("leaf_seq", -1),
                              "staged_text_equals_result": json_value(data.get("text")) == result})
    return {"task_id": result["task_id"], "result_id": result["result_id"],
            "operation_id": result["operation_id"], "receipt_id": result["receipt_id"],
            "t2_commit": result["commit_ref"], "new_event_indices": fresh,
            "exact_full_result_in_actual_payload": payload_matches, "original_history_rows": originals}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--cas-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    source, cas, output = (p.resolve() for p in (args.evidence_dir, args.cas_dir, args.output_dir))
    if (output == source or output == cas or source.is_relative_to(output) or cas.is_relative_to(output)
            or output.is_relative_to(source) or output.is_relative_to(cas)):
        parser.error("output must be a new independent directory, not an input or its ancestor")
    output.mkdir(parents=True, exist_ok=False)
    audit = Export(source, cas, output)
    meta = {name: audit.source_json(name) for name in
            ("manifest.json", "summary.json", "first.input.json", "restart.input.json", "browser/summary.json")}
    snapshots = {}
    for path in sorted(source.glob("*.ledger.json*")):
        if not path.name.endswith((".ledger.json", ".ledger.json.gz")):
            continue
        name = path.name.removesuffix(".gz")
        if name not in snapshots:
            snapshots[name] = audit.source_json(name)
    if not snapshots:
        parser.error("no retained ledger snapshots; do not inspect a live database instead")
    primary_name = next((name for name in ("final.ledger.json", "restart.ledger.json", "first.ledger.json")
                         if name in snapshots), next(reversed(snapshots)))
    primary = snapshots[primary_name]
    tasks = {row["task_id"]: record(row) for row in primary.get("tasks", [])}
    results = [record(row) for row in primary.get("task_results", [])]
    attempts = {row["request_id"]: record(row) for row in primary.get("subject_request_attempts", [])}
    responses = {row["attempt"]["request_id"]: row["result"] for row in primary.get("subject_request_results", [])}
    plans, requests = {}, []
    for request_id, attempt in attempts.items():
        parsed = audit.retain(attempt.get("input_plan_ref"), f"request/{request_id}/input")
        plan = parsed if isinstance(parsed, dict) else None
        if parsed is not None and plan is None:
            audit.errors.append({"request_id": request_id, "error": "sealed input is not a JSON object"})
        terminal = attempt.get("terminal") or {}
        audit.retain(terminal.get("result_ref"), f"request/{request_id}/terminal")
        if isinstance(plan, dict):
            plans[request_id] = plan
        response = responses.get(request_id, {})
        requests.append({"attempt": attempt, "plan_first_request_id": (plan or {}).get("request_id"),
                         "plan_step_id": (plan or {}).get("step_id"), "context": (plan or {}).get("context"),
                         "provider_binding": (plan or {}).get("provider_binding"),
                         "result_status": response.get("status"), "termination": response.get("termination"),
                         "effective": response.get("effective"), "provider_response": response.get("response"),
                         "result_presence": [result_presence(plan, result, primary.get("session_events", []))
                                             for result in results] if isinstance(plan, dict) else None})
    for table, field_names in (("tasks", ("input_ref",)), ("operations", ("final_input_ref",)),
                              ("evidence_receipts", ("raw_ref", "normalized_ref")),
                              ("task_results", ("evidence_ref",)),
                              ("subject_steps", ("input_plan_ref", "decision_ref", "model_result_ref", "model_outcome_ref"))):
        for index, row in enumerate(primary.get(table, [])):
            data = record(row)
            for field in field_names:
                audit.retain(data.get(field), f"{table}/{index}/{field}")
            if table == "task_results":
                for ref, label in references(data.get("result"), f"task_results/{index}/result"):
                    if ref.get("codec", {}).get("name") in audit.FOLLOW or label.endswith("/output_ref"):
                        audit.retain(ref, label)
    decisions = []
    for item in primary.get("decisions", []):
        step, decision = item["step"], item["decision"]
        request_id = step.get("active_request_id")
        original = audit.retain(step.get("decision_ref"), f"decision/{step['step_id']}")
        decisions.append({"step_id": step["step_id"], "branch_id": step["branch_id"],
                          "step_no": step["step_no"], "state": step["state"], "commit_ref": step.get("commit_ref"),
                          "active_request_id": request_id, "request_retained": request_id in attempts,
                          "request_step_matches": attempts.get(request_id, {}).get("step_id") == step["step_id"],
                          "input_ref_matches": attempts.get(request_id, {}).get("input_plan_ref") == decision.get("input_plan_ref"),
                          "decision_cas_equals_snapshot": original == decision if original is not None else None,
                          "output_text": decision["output"]["text"], "decision": decision})
    decisions.sort(key=lambda row: ((row["commit_ref"] or {}).get("ledger_id", ""),
                                    (row["commit_ref"] or {}).get("seq", -1), row["step_id"]))
    timeline = []
    path = source / "browser/events.jsonl"
    if not path.is_file():
        path = source / "browser/events.jsonl.gz"
    if path.is_file():
        raw, decoded = raw_file(path)
        audit.sources[str(path.relative_to(source))] = {
            "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest(),
            "decoded_bytes": len(decoded), "decoded_sha256": hashlib.sha256(decoded).hexdigest()}
        step_ids = {row["step_id"] for row in decisions}
        for line_no, line in enumerate(decoded.splitlines(), 1):
            try:
                row = json.loads(line)
            except ValueError as error:
                audit.errors.append({"artifact": str(path), "line": line_no, "error": str(error)})
                continue
            message = row.get("message", {})
            if (row.get("direction") == "received" and message.get("type") == "timeline_part"
                    and (message.get("message_id") in step_ids or message.get("step_id") in step_ids)):
                timeline.append({"line": line_no, "record": row})
    report = {"schema_version": 1, "created_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "exporter_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "source_directory": str(source), "primary_snapshot": primary_name, "metadata": meta,
              "scope": "Evidence export only; no inferred success or changed denominator. Exact payload matches are sufficient aids, not a requirement for two new events. Other native observations/derived history require manual inspection.",
              "integrity": "Raw bytes retain SHA256. BLAKE3 is checked only when the installed Python module is available; null is unverified.",
              "source_artifacts": audit.sources, "objects": audit.objects, "errors": audit.errors,
              "snapshots": {name: {"counts": {table: len(rows) for table, rows in data.items() if isinstance(rows, list)},
                                    "heads": data.get("subject_branch_heads"),
                                    "task_results": data.get("task_results"), "operations": data.get("operations"),
                                    "attempts": data.get("attempts"), "evidence_receipts": data.get("evidence_receipts")}
                            for name, data in snapshots.items()},
              "tasks": tasks, "results": results, "requests": requests, "decisions": decisions,
              "original_history": primary.get("session_events"), "context_windows": primary.get("context_windows"),
              "browser_timeline_parts": timeline, "semantic_verdict": "not_assessed"}
    (output / "audit.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"audit": str(output / "audit.json"), "objects": len(audit.objects),
                      "unavailable_or_invalid": sum(row.get("status") != "retained" for row in audit.objects.values()),
                      "requests": len(requests), "decisions": len(decisions), "semantic_verdict": "not_assessed"}))


if __name__ == "__main__":
    main()
