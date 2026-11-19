#!/usr/bin/env python3
"""Read only original model5 ZIP and exact-source copies; emit evidence review.

This is an offline evidence analysis, not a product test or Rust parser run.
It does not open the live SQLite/CAS, call a provider, or change acceptance.
CAS objects are bound by the verified ZIP SHA256, exact member name, and
declared length. This stdlib-only review does NOT recompute BLAKE3.
"""

import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import zipfile

SOURCE = "5e7a3d5ade83d21fbc6e86b7eb8c3f19fb3fbe3c"
ZIP_SHA256 = "ab362c507c24dae49f11429c73b58ecc81cf21c90ebf4d9cc99672323759e084"
LEDGER = "real-single-node/final.ledger.json"


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def decoded(row):
    value = row.get("data", row)
    return json.loads(value) if isinstance(value, str) else value


def text_parts(message):
    return "\n".join(
        part["text"]
        for part in message.get("parts", [])
        if part.get("type") == "text"
    )


def source_excerpt(source, first, last):
    return [
        {"line": number, "text": line}
        for number, line in enumerate(source.splitlines(), 1)
        if first <= number <= last
    ]


def review(archive_path, source_dir):
    with archive_path.open("rb") as stream:
        archive_sha = hashlib.file_digest(stream, "sha256").hexdigest()
    if archive_sha != ZIP_SHA256:
        raise ValueError("Unexpected original ZIP SHA256")

    member_reads = {}
    reference_reads = {}
    source_manifest = json.loads(
        (source_dir.parent / "source-manifest.json").read_text(encoding="utf-8")
    )
    sources = {}
    for entry in source_manifest["sources"]:
        raw = (source_dir / entry["path"]).read_bytes()
        if len(raw) != entry["utf8_bytes"] or sha256(raw) != entry["sha256"]:
            raise ValueError("Source copy changed: " + entry["path"])
        if entry["source_revision"] != SOURCE:
            raise ValueError("Unexpected source revision")
        sources[entry["path"]] = raw.decode("utf-8")

    with zipfile.ZipFile(archive_path) as archive:
        names = set(archive.namelist())

        def member(name):
            raw = archive.read(name)
            member_reads[name] = {"bytes": len(raw), "sha256": sha256(raw)}
            return raw

        def document(name):
            return json.loads(member(name))

        def reference(ref):
            algorithm, digest = ref["digest"].split(":", 1)
            if algorithm != "blake3" or len(digest) != 64:
                raise ValueError("Unexpected CAS reference format")
            name = f"real-single-node/cas/{digest[:2]}/{digest[2:4]}/{digest}"
            raw = member(name)
            if len(raw) != ref["bytes"]:
                raise ValueError("CAS reference length mismatch: " + name)
            reference_reads[name] = {
                "reference": ref,
                "length_matches": True,
                "zip_member_sha256": sha256(raw),
                "blake3_recomputed_by_this_script": False,
            }
            return name, raw

        ledger = document(LEDGER)
        manifest = document("real-single-node/manifest.json")
        summary = document("real-single-node/summary.json")
        loaded = document("model-runtime-loaded.json")
        if manifest["source_revision"] != SOURCE:
            raise ValueError("ZIP manifest does not identify the reviewed source")
        runtime_member = "real-single-node/first.runtime.log"
        runtime_log = member(runtime_member).decode("utf-8")
        server_member = "model-server.log"
        server_log = member(server_member).decode("utf-8")
        attempts = sorted(
            [decoded(row) for row in ledger["subject_request_attempts"]],
            key=lambda row: row["created_commit"]["seq"],
        )
        steps = {decoded(row)["step_id"]: decoded(row) for row in ledger["subject_steps"]}
        commits = {
            (row["ledger_id"], row["seq"]): row["created_at"]
            for row in ledger["commit_log"]
        }
        events = [
            {"index": i, **row, "decoded": decoded(row)}
            for i, row in enumerate(ledger["session_events"])
        ]
        rejections = {
            row["decoded"]["request_id"]: row
            for row in events
            if row["decoded"].get("rejection", {}).get("code") == "CONTROL_PROTOCOL_INVALID"
        }

        def commit_time(commit):
            if commit is None:
                return None
            return commits.get((commit["ledger_id"], commit["seq"]))

        requests = []
        plans = []
        contents = []
        provider_ids = set()
        for ordinal, attempt in enumerate(attempts, 1):
            plan_member, plan_raw = reference(attempt["input_plan_ref"])
            plan = json.loads(plan_raw)
            plans.append(plan)
            step = steps[attempt["step_id"]]
            if plan["request_id"] != attempt["request_id"] or plan["step_id"] != attempt["step_id"]:
                raise ValueError("Frozen plan identity mismatch")
            if step["active_request_id"] != attempt["request_id"]:
                raise ValueError("Step/request identity mismatch")
            terminal = attempt["terminal"]
            result_member, result_raw = reference(terminal["result_ref"])
            result = json.loads(result_raw)
            if result["request_id"] != attempt["request_id"]:
                raise ValueError("Terminal request identity mismatch")
            response = result.get("response")
            response_content = response["content"] if response else None
            contents.append(response_content)
            messages = plan["payload"]["messages"]
            rejection = rejections.get(attempt["request_id"])
            model_result_ref = step.get("model_result_ref")
            model_result = None
            model_result_member = None
            if model_result_ref is not None:
                model_result_member, raw_model_result = reference(model_result_ref)
                model_result = raw_model_result.decode("utf-8")
            stream_facts = None
            if response is not None:
                raw_stream = response["raw_response"]
                fragments, completion_ids, stop_reasons = [], set(), []
                done_count = 0
                chunks = 0
                for line in raw_stream.splitlines():
                    line = line.strip()
                    if not line:
                        continue
                    if line.startswith("data:"):
                        line = line[5:].strip()
                    if line == "[DONE]":
                        done_count += 1
                        continue
                    chunk = json.loads(line)
                    chunks += 1
                    if "id" in chunk:
                        completion_ids.add(chunk["id"])
                    for choice in chunk.get("choices", []):
                        fragment = choice.get("delta", {}).get("content")
                        if fragment is not None:
                            if not isinstance(fragment, str):
                                raise ValueError("Non-text stream fragment")
                            fragments.append(fragment)
                        if choice.get("finish_reason") is not None:
                            stop_reasons.append(choice["finish_reason"])
                rebuilt = "".join(fragments)
                provider_ids.update(completion_ids)
                stream_facts = {
                    "raw_bytes": len(raw_stream.encode("utf-8")),
                    "raw_sha256": sha256(raw_stream.encode("utf-8")),
                    "json_chunks": chunks,
                    "done_markers": done_count,
                    "completion_ids": sorted(completion_ids),
                    "finish_reasons": stop_reasons,
                    "rebuilt_matches_terminal_content": rebuilt == response_content,
                    "rebuilt_matches_original_model_result": rebuilt == model_result,
                }
            system = plan["payload"]["system_prompt"]
            requests.append({
                "ordinal": ordinal,
                "request_id": attempt["request_id"],
                "attempt_id": attempt["attempt_id"],
                "step_id": attempt["step_id"],
                "created_commit": attempt["created_commit"],
                "created_at_from_commit_log": commit_time(attempt["created_commit"]),
                "recorded_commit": terminal["recorded_commit"],
                "recorded_at_from_commit_log": commit_time(terminal["recorded_commit"]),
                "final_step_state": step["state"],
                "attempt_ledger_pointer": f"{LEDGER}#/subject_request_attempts",
                "plan_member": plan_member,
                "plan_reference": attempt["input_plan_ref"],
                "plan_schema": plan["schema_version"],
                "semantic_wait": plan.get("semantic_wait"),
                "original_worker_source": plan["context"]["source"],
                "message_count": len(messages),
                "first_message_text": text_parts(messages[0]) if messages else None,
                "last_message_text": text_parts(messages[-1]) if messages else None,
                "system_prompt_bytes": len(system.encode("utf-8")),
                "system_prompt_characters": len(system),
                "system_prompt_contains_host_printf": "host.printf" in system,
                "system_prompt_contains_compute_child": "compute.child" in system,
                "system_prompt_has_finite_model_delegate": "[delegate] {" in system,
                "payload_tools": plan["payload"]["tools"],
                "reserved_max_tokens": plan["payload"]["max_tokens"],
                "provider_binding": plan["provider_binding"],
                "terminal_member": result_member,
                "terminal_status": result["status"],
                "terminal_termination": result["termination"],
                "terminal_usage": response.get("usage") if response else None,
                "terminal_effective": result["effective"],
                "terminal_outcome": terminal["outcome"],
                "original_response_content": response_content,
                "model_result_member": model_result_member,
                "model_result_reference": model_result_ref,
                "stream": stream_facts,
                "recorded_rejection": (
                    {
                        "ledger_pointer": f"{LEDGER}#/session_events/{rejection['index']}",
                        "session_seq": rejection["seq"],
                        "created_at": rejection["created_at"],
                        "data": rejection["decoded"],
                    } if rejection else None
                ),
            })

        corrections = []
        for index in range(len(requests) - 1):
            previous, following = requests[index:index + 2]
            event = rejections.get(previous["request_id"])
            if event is None:
                continue
            messages = plans[index + 1]["payload"]["messages"]
            feedback = event["decoded"]["text"]
            corrections.append({
                "original_request_id": previous["request_id"],
                "replacement_request_id": following["request_id"],
                "distinct_request_identity": previous["request_id"] != following["request_id"],
                "original_terminal_content_preserved_as_assistant": any(
                    message["role"] == "assistant" and text_parts(message) == contents[index]
                    for message in messages
                ),
                "exact_recorded_rejection_feedback_preserved_as_user": any(
                    message["role"] == "user" and text_parts(message) == feedback
                    for message in messages
                ),
                "original_first_message_preserved": (
                    plans[index]["payload"]["messages"][0] == messages[0]
                ),
                "original_worker_source_preserved": (
                    previous["original_worker_source"] == following["original_worker_source"]
                ),
                "next_plan_member": following["plan_member"],
            })

        first_line = contents[0].splitlines()[0]
        first_json_tail = first_line.split("]", 1)[1].lstrip()
        first_json, consumed = json.JSONDecoder().raw_decode(first_json_tail)
        native_tail = first_json_tail[consumed:]
        successful = [row for row in requests if row["terminal_status"] == "success"]
        runtime_rows = [
            {"line": i, "text": line}
            for i, line in enumerate(runtime_log.splitlines(), 1)
            if any(token in line for token in (
                "shutdown signal", "outcome not conclusive",
                "subject runtime terminated", "inference broker stopped",
                "Error:", "provider inference did not produce",
            ))
        ]
        shutdown_events = [
            {
                "ledger_pointer": f"{LEDGER}#/session_events/{row['index']}",
                "session_seq": row["seq"],
                "created_at": row["created_at"],
                "data": row["decoded"],
            }
            for row in events
            if row["event_type"] == "runtime_error" and row["seq"] >= 82
        ]
        source_anchors = {
            "native_json_and_finite_model_control_grammar": (
                "crates/shenko-subject/src/output.rs", 351, 383),
            "wait_current_frozen_context_and_one_control": (
                "crates/shenko-subject/src/output.rs", 235, 266),
            "host_printf_input_and_bounds": (
                "crates/shenko-exec-host/src/native_task.rs", 25, 34),
            "host_printf_descriptor_omits_prompt": (
                "crates/shenko-exec-host/src/native_task.rs", 301, 314),
            "registry_prompt_selection": (
                "crates/shenko-subject/src/child.rs", 645, 659),
            "runtime_installs_printf_handler": (
                "bin/shenko/src/runtime_plugins.rs", 108, 120),
            "r0_preserves_observation_and_new_identity": (
                "crates/shenko-subject/src/loop_runner.rs", 2401, 2506),
            "model_result_admission_and_unknown": (
                "crates/shenko-subject/src/loop_runner.rs", 2549, 2603),
            "harness_collect_deadline": (
                "scripts/real_single_node_journey.py", 152, 170),
            "harness_cleanup_and_result": (
                "scripts/real_single_node_journey.py", 468, 504),
            "harness_owned_sigint": (
                "scripts/real_single_node_journey.py", 204, 216),
            "workflow_1800_second_journey": (
                ".github/workflows/real-single-node.yml", 136, 147),
        }
        anchors = {}
        for label, (path, first, last) in source_anchors.items():
            anchors[label] = {
                "source_revision": SOURCE, "path": path,
                "url": f"https://github.com/cyjin-yl/shenko/blob/{SOURCE}/{path}#L{first}-L{last}",
                "excerpt": source_excerpt(sources[path], first, last),
            }

        return {
            "schema_version": 1,
            "scope": "Independent, offline original-evidence/source review; no product test or model run",
            "run_id": 43916477826,
            "artifact_id": 14255345735,
            "published_head": "be46be34593452f47b824d3ae62f625032e2d54c",
            "actual_checkout": SOURCE,
            "actual_tree": "58e5e59b230a5020ee56ccf800deea1466c3151b",
            "archive": {
                "path": str(archive_path), "bytes": archive_path.stat().st_size,
                "sha256": archive_sha, "expected_sha256_matched": True,
                "member_count": len(names),
            },
            "script_sha256": sha256(Path(__file__).read_bytes()),
            "source_manifest": source_manifest,
            "original_manifest": manifest,
            "original_summary": summary,
            "table_counts": {table: len(rows) for table, rows in ledger.items()},
            "summary": {
                "request_count": len(requests),
                "terminal_status_counts": dict(Counter(row["terminal_status"] for row in requests)),
                "step_state_counts": dict(Counter(row["state"] for row in steps.values())),
                "recorded_protocol_rejection_count": len(rejections),
                "recorded_protocol_reasons": dict(Counter(
                    row["decoded"]["rejection"]["details"]["reason"]
                    for row in rejections.values()
                )),
                "successful_raw_streams": len(successful),
                "successful_streams_equal_response_and_model_result": all(
                    row["stream"]["rebuilt_matches_terminal_content"]
                    and row["stream"]["rebuilt_matches_original_model_result"]
                    for row in successful
                ),
                "distinct_completion_ids": len(provider_ids),
                "successful_completion_ids": sorted(provider_ids),
                "all_successful_streams_have_done": all(
                    row["stream"]["done_markers"] == 1 for row in successful),
                "successful_finish_reason_counts": dict(Counter(
                    reason for row in successful for reason in row["stream"]["finish_reasons"])),
                "successful_model_result_ref_counts": dict(Counter(
                    row["model_result_reference"]["digest"] for row in successful)),
                "successful_input_token_range": [
                    min(row["terminal_outcome"]["usage"]["input_tokens"] for row in successful),
                    max(row["terminal_outcome"]["usage"]["input_tokens"] for row in successful),
                ],
                "all_successful_terminations_provider_terminal_without_length": all(
                    row["terminal_termination"]["kind"] == "provider_terminal"
                    and not row["terminal_termination"]["length"] for row in successful
                ),
                "all_frozen_semantic_waits_absent": all(row["semantic_wait"] is None for row in requests),
                "all_frozen_system_prompts_omit_host_printf": all(
                    not row["system_prompt_contains_host_printf"] for row in requests),
                "all_frozen_payload_tool_arrays_empty": all(row["payload_tools"] == [] for row in requests),
                "correction_transitions": len(corrections),
                "all_correction_transitions_preserve_original_output_feedback_and_baseline": all(
                    row["distinct_request_identity"]
                    and row["original_terminal_content_preserved_as_assistant"]
                    and row["exact_recorded_rejection_feedback_preserved_as_user"]
                    and row["original_first_message_preserved"]
                    and row["original_worker_source_preserved"] for row in corrections
                ),
            },
            "first_rejection_direct_explanation": {
                "request_id": requests[0]["request_id"],
                "model_result_member": requests[0]["model_result_member"],
                "first_line": first_line,
                "first_json_value": first_json,
                "non_whitespace_after_first_json_value": native_tail.strip(),
                "has_illegal_trailing_content_for_native_json_control": bool(native_tail.strip()),
                "method": "JSON raw_decode plus exact Rust parser source and original recorded rejection; Rust parser not rerun",
            },
            "requests": requests,
            "corrections": corrections,
            "shutdown": {
                "final_request": requests[-1],
                "runtime_member": runtime_member,
                "runtime_lines": runtime_rows,
                "final_runtime_events": shutdown_events,
                "missing_restart_members": [
                    name for name in (
                        "real-single-node/before-restart.paused.ledger.json",
                        "real-single-node/restart.runtime.log",
                        "real-single-node/restart.ledger.json",
                    ) if name not in names
                ],
                "original_failure": (
                    member("real-single-node/failure.txt").decode("utf-8")
                    if "real-single-node/failure.txt" in names else None
                ),
                "interpretation": (
                    "The 1800-second first-phase deadline failed unchanged child/result/output "
                    "checks; finally sends owned SIGINT. Last request is failed/Unknown and "
                    "its original step stays awaiting_model. Missing completed stream/provider "
                    "log entry does not prove NeverDispatched. The exact producer of the last "
                    "Unknown is not established by this evidence review."
                ),
            },
            "runtime_loaded_evidence": {
                "member": "model-runtime-loaded.json",
                "argv": loaded["argv"], "cpu_affinity": loaded["cpu_affinity"],
                "server_log_member": server_member,
                "server_log_lines": len(server_log.splitlines()),
            },
            "source_anchors": anchors,
            "read_members": member_reads,
            "cas_references": reference_reads,
            "limits": [
                "No Rust parser, product tests, provider request, or runtime restart executed by this review.",
                "This review verifies original ZIP SHA256 and reference lengths; BLAKE3 is not recomputed by the stdlib script.",
                "Frozen request payloads and original response streams are present; no separate full outbound HTTP request-wire capture was identified.",
                "Missing host.printf prompt section is an independent implementation gap, not proof it alone caused the repeated invalid outputs.",
                "Results concern this Qwen3-0.6B configuration; they do not measure the unavailable formal 27B reference model.",
                "The original first-phase, clean-stop, and restart acceptance requirements remain failed or unobserved.",
            ],
        }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--zip", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source-dir", type=Path, default=Path(__file__).parent / "source")
    args = parser.parse_args()
    result = review(args.zip.resolve(), args.source_dir.resolve())
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({
        "output": str(args.output.resolve()),
        "archive_sha256": result["archive"]["sha256"],
        "actual_checkout": result["actual_checkout"],
        "summary": {key: value for key, value in result["summary"].items()
                    if key != "successful_completion_ids"},
    }, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
