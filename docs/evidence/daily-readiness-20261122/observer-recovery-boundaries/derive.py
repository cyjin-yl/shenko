#!/usr/bin/env python3
"""Derive two observer diagnoses from the original closed c931/001 files.

This reads captures only. It does not open the Store, replay a submission,
run a model/browser, or replace the original failed summary.
"""
import hashlib
import json
from pathlib import Path
import subprocess
import sys

repo = Path(__file__).resolve().parents[4]
sys.path.insert(0, str(repo / "scripts"))
import real_single_node_journey as journey
from real_browser_witness import BrowserWitness, committed_output_frame

source = Path(sys.argv[1])
names = ["summary.json", "first.input.json", "restart.websocket.jsonl",
         "browser/events.jsonl", "browser/before-restart.dom.json",
         "browser/journey-finished.dom.json", "browser/final.dom.json",
         "first.ledger.json", "restart.ledger.json", "browser/restart-output.aria.yml"]
originals = {}
for name in names:
    raw = (source / name).read_bytes()
    originals[name] = {"bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}

summary = json.loads((source / "summary.json").read_text())
submission = json.loads((source / "first.input.json").read_text())
wire = [json.loads(line) for line in (source / "restart.websocket.jsonl").read_text().splitlines()]
page = [json.loads(line) for line in (source / "browser/events.jsonl").read_text().splitlines()]
history = [row["message"] for row in page if row.get("direction") == "received" and "message" in row]
accepted = journey.accepted_receipt(history, submission)
replay = journey.accepted_receipt(wire, submission)
mixed = journey.accepted_receipt(history + wire, submission)

def matches(receipt):
    return bool(accepted and receipt and receipt.get("replayed") is True
                and receipt.get("event_id") == accepted.get("event_id")
                and receipt.get("accepted_commit") == accepted.get("accepted_commit"))

witness = BrowserWitness.__new__(BrowserWitness)
witness.before_restart = json.loads((source / "browser/before-restart.dom.json").read_text())
dom = {}
for name in ["journey-finished", "final"]:
    state = json.loads((source / f"browser/{name}.dom.json").read_text())
    dom[name] = {"retains_original_facts": witness.stable_messages_match(state),
                 "rows": state["rows"]}

page_commits = {}
for phase in ["first", "restart"]:
    facts = json.loads((source / f"{phase}.ledger.json").read_text())
    step_id = summary[f"{phase}_output_input"]["step_id"]
    output = next(row for row in facts["decisions"] if row["step"]["step_id"] == step_id)
    commit = output["step"]["commit_ref"]
    commit_ref = f"{commit['ledger_id']}/{commit['seq']}"
    content = output["decision"]["output"]["text"]
    found = [row for row in page if committed_output_frame(row, step_id, commit_ref, content)]
    page_commits[phase] = {
        "step_id": step_id, "commit_ref": commit_ref, "committed_content": content,
        "original_browser_check": summary["checks"][f"browser_{phase}_committed_output"],
        "actual_page_committed_frames": found,
        "actual_page_matching_stream_parts": sum(
            row.get("direction") == "received" and row.get("message", {}).get("type") == "stream_part"
            and row["message"].get("message_id") == step_id for row in page),
        "independent_restart_socket_committed_frames": [
            {"line": index, "message": row} for index, row in enumerate(wire, 1)
            if committed_output_frame({"direction": "received", "message": row}, step_id, commit_ref, content)],
    }

assert summary["passed"] is False
assert [key for key, value in summary["checks"].items() if not value] == [
    "replayed_input_same_event", "browser_existing_messages_retained_once_at_finish"]
assert not matches(mixed) and matches(replay)
assert dom["journey-finished"]["retains_original_facts"] is False
assert dom["final"]["retains_original_facts"] is True
assert len(page_commits["first"]["actual_page_committed_frames"]) == 2
assert page_commits["restart"]["actual_page_committed_frames"] == []
assert len(page_commits["restart"]["independent_restart_socket_committed_frames"]) == 1
result = {
    "schema_version": 1,
    "checked_observer_revision": subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip(),
    "checked_source_files": {name: {"bytes": len((repo / name).read_bytes()),
        "sha256": hashlib.sha256((repo / name).read_bytes()).hexdigest()}
        for name in ["scripts/real_single_node_journey.py", "scripts/real_browser_witness.py",
                     "scripts/real_protocol_observation.py"]},
    "derivation_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
    "original_binary_revision": "c931526c8ae8cd16f3e0cf56d733f1e4275d5bb0",
    "original_observer_revision": "e537896f771233ea2b8d01758b3acb86fb0c6345",
    "original_attempt_passed": summary["passed"],
    "original_files": originals,
    "accepted_receipt": accepted,
    "actual_restart_socket_receipt": replay,
    "actual_restart_socket_receipt_line": next(index for index, row in enumerate(wire, 1) if row == replay),
    "original_browser_receipt_line": next(index for index, row in enumerate(page, 1) if row.get("message") == accepted),
    "history_first_projection": {"selected": mixed, "matches_original_replay": matches(mixed),
        "scope": "Decoded projection of actual captured history then actual socket frames; the original combined in-memory array was not retained."},
    "actual_restart_socket_matches_original_replay": matches(replay),
    "original_stable_rows": [row for row in witness.before_restart["rows"] if row["stable_fact"]],
    "original_dom_captures": dom,
    "selected_output_delivery": page_commits,
    "scope": "Read-only observer diagnosis. No new model/browser run and no regrading of the original failed attempt. The subsequent bounded render wait still requires a new real journey.",
}
print(json.dumps(result, ensure_ascii=False, indent=2))
