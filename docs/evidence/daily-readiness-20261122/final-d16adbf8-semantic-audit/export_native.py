#!/usr/bin/env python3
"""Export exact native E1/T2 proof identities from an already closed journey.

No runtime/Store API, model request, write transaction, replay, or success verdict.
This supplements the unchanged semantic exporter; it does not export or revalidate
the full predecessor graph. All reads require the original finished/reaped receipt.
"""
import argparse
import datetime
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3

try:
    import blake3
except ImportError:
    blake3 = None


def identity(raw):
    return {"bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}


def json_file(directory, name):
    path = directory / name
    if not path.is_file():
        path = directory / (name + ".gz")
    raw = path.read_bytes()
    decoded = gzip.decompress(raw) if path.suffix == ".gz" else raw
    return json.loads(decoded), {"path": str(path), **identity(raw), "decoded": identity(decoded)}


def parse_record(value):
    if isinstance(value, str):
        return json.loads(value)
    return value


def file_state(path):
    if not path.exists():
        return {"exists": False}
    before = path.stat()
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    after = path.stat()
    return {"exists": True, "bytes": after.st_size, "sha256": digest.hexdigest(),
            "mtime_ns": after.st_mtime_ns,
            "stable_during_hash": (before.st_size, before.st_mtime_ns) == (after.st_size, after.st_mtime_ns)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--data-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    source, data, output = [p.resolve() for p in (args.evidence_dir, args.data_dir, args.output_dir)]
    if data != (source / "installation").resolve():
        parser.error("data directory must be this frozen journey's own evidence-dir/installation")
    if any(output == p or output.is_relative_to(p) or p.is_relative_to(output) for p in (source, data)):
        parser.error("output must be a fresh independent directory outside both inputs")
    summary, summary_source = json_file(source, "summary.json")
    shutdown = summary.get("last_shutdown") or summary.get("first_shutdown") or summary.get("startup_shutdown") or {}
    group = shutdown.get("group_cleanup", {})
    remaining = group.get("remaining", {})
    if not (summary.get("finished_at") and group.get("complete") is True
            and group.get("parent_reaped") is True and remaining.get("alive") is False
            and remaining.get("live_pids") == [] and remaining.get("live_thread_ids") == []):
        parser.error("original summary does not establish a finished/reaped/empty owned group")
    pgid = group.get("pgid")
    if type(pgid) is not int or pgid <= 1:
        parser.error("closed receipt must identify an explicit owned process group")
    try:
        os.killpg(pgid, 0)
    except ProcessLookupError:
        pass
    else:
        parser.error("recorded process group currently exists; do not read this installation")
    snapshot, snapshot_source = json_file(source, "final.ledger.json")
    output.mkdir(parents=True, exist_ok=False)
    report = {"schema_version": 1, "observed_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "exporter_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "source_directory": str(source), "private_data_directory": str(data),
              "summary_source": summary_source, "final_snapshot_source": snapshot_source,
              "closed_process_receipt": shutdown, "original_passed": summary.get("passed"),
              "scope": "Closed Store SQL identity export and selected exact CAS objects only. No full closure revalidation, transport proof, semantic or journey verdict.",
              "seal_debt_boundary": "An empty final debt table establishes only no debt retained at this final observation. Earlier runtime warnings and errors remain in original logs/history.",
              "errors": [], "sql_raw": {}, "objects": {}, "facts": [], "t2": [],
              "semantic_verdict": "not_assessed"}
    database = data / "shenko.db"
    components = [database, Path(str(database) + "-wal"), Path(str(database) + "-shm")]
    report["sqlite_files_before"] = {str(p): file_state(p) for p in components}

    def retain(reference, use):
        if not isinstance(reference, dict) or not re.fullmatch(r"blake3:[0-9a-f]{64}", reference.get("digest", "")):
            report["errors"].append({"use": use, "error": "missing or noncanonical reference", "reference": reference})
            return None
        digest = reference["digest"]
        if digest in report["objects"]:
            entry = report["objects"][digest]
            entry["uses"].append(use)
            if reference not in entry["references"]:
                entry["references"].append(reference)
            if entry.get("bytes") != reference.get("bytes"):
                report["errors"].append({"use": use, "error": "typed alias length differs", "reference": reference})
            return decoded.get(digest)
        name = digest[7:]
        relative = Path(name[:2]) / name[2:4] / name
        entry = {"references": [reference], "uses": [use]}
        report["objects"][digest] = entry
        try:
            value = (source / "cas" / relative).read_bytes()
        except OSError as error:
            entry.update(status="unavailable", error=str(error))
            report["errors"].append({"use": use, "error": "CAS unavailable", "detail": str(error), "reference": reference})
            return None
        entry.update(identity(value), blake3_verified=(blake3.blake3(value).hexdigest() == name if blake3 else None))
        target = output / "cas" / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(value)
        entry["export"] = str(target.relative_to(output))
        entry["status"] = "retained" if len(value) == reference.get("bytes") and entry["blake3_verified"] is not False else "integrity_mismatch"
        if entry["status"] != "retained":
            report["errors"].append({"use": use, "error": entry["status"]})
            return None
        try:
            decoded[digest] = json.loads(value)
        except (ValueError, UnicodeError) as error:
            report["errors"].append({"use": use, "error": "proof CAS is not JSON", "detail": str(error), "reference": reference})
            return None
        return decoded[digest]

    decoded = {}
    conn = sqlite3.connect(database.as_uri() + "?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    try:
        conn.execute("PRAGMA query_only=ON")
        conn.execute("BEGIN")
        report["sqlite_transaction_started_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        tables = {row[0] for row in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        for table in ["native_evidence_materializations", "native_evidence_seal_debts"]:
            report["sql_raw"][table] = [dict(row) for row in conn.execute(f"SELECT * FROM {table} ORDER BY receipt_id")] if table in tables else None
        receipts = [dict(row) for row in conn.execute("SELECT * FROM evidence_receipts ORDER BY receipt_id")]
        report["sql_raw"]["evidence_receipts"] = receipts
        expected = {row["receipt_id"]: parse_record(row["data"]) for row in snapshot.get("evidence_receipts", [])}
        observed = {row["receipt_id"]: parse_record(row["data"]) for row in receipts}
        report["closed_receipts_equal_final_snapshot"] = observed == expected
        if observed != expected:
            report["errors"].append({"error": "closed receipt rows differ from retained final snapshot"})
        actions = set()
        for row in report["sql_raw"].get("native_evidence_materializations") or []:
            marker = parse_record(row["data"])
            reference = marker["evidence_ref"]
            envelope = retain(reference, "materialization/" + row["receipt_id"])
            fact = retain(envelope.get("fact_ref"), "inner/" + row["receipt_id"]) if isinstance(envelope, dict) and reference.get("codec", {}).get("major") == 6 else envelope
            item = {"receipt_id": row["receipt_id"], "marker": marker, "root_reference": reference, "envelope": envelope if reference.get("codec", {}).get("major") == 6 else None, "fact": fact}
            report["facts"].append(item)
            if not isinstance(fact, dict):
                continue
            item["receipt_exactly_matches_final_snapshot"] = fact.get("receipt") == expected.get(row["receipt_id"])
            arm = fact.get("arm", {})
            admission = arm.get("admission", {})
            if admission.get("action_id"):
                actions.add(admission["action_id"])
            for key, reference in [("arm_transition", arm.get("transition_ref")), ("arm_payload", admission.get("payload_ref"))]:
                if reference:
                    retain(reference, key + "/" + row["receipt_id"])
        prepared = [dict(row) for row in conn.execute("SELECT * FROM control_prepared_transitions WHERE json_extract(transition_ref,'$.codec.name')='store-native-t2-transition' ORDER BY action_id")]
        report["sql_raw"]["t2_prepared_transitions"] = prepared
        for row in prepared:
            actions.add(row["action_id"])
            reference = parse_record(row["transition_ref"])
            transition = retain(reference, "t2/" + row["action_id"])
            report["t2"].append({"action_id": row["action_id"], "reference": reference, "transition": transition})
            if isinstance(transition, dict) and transition.get("evidence_fact"):
                reference = transition["evidence_fact"]
                envelope = retain(reference, "t2_evidence/" + row["action_id"])
                if isinstance(envelope, dict) and reference.get("codec", {}).get("major") == 6:
                    retain(envelope.get("fact_ref"), "t2_inner/" + row["action_id"])
        report["sql_raw"]["related_control_materializations"] = []
        report["sql_raw"]["related_control_prepared_transitions"] = []
        for action in sorted(actions):
            for table, target in [("control_materializations", "related_control_materializations"), ("control_prepared_transitions", "related_control_prepared_transitions")]:
                report["sql_raw"][target].extend(dict(row) for row in conn.execute(f"SELECT * FROM {table} WHERE action_id=?", (action,)))
        report["sqlite_transaction_finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    finally:
        conn.close()
        report["sqlite_files_after"] = {str(p): file_state(p) for p in components}
        report["sqlite_metadata_boundary"] = "Read-only WAL access can change shm metadata; before/after are recorded separately, with no blanket unchanged-files claim. Only the Store DB was read, in one read transaction."
        report["objects_unavailable_or_invalid"] = sum(row.get("status") != "retained" for row in report["objects"].values())
        target = output / "native-proof.json"
        target.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"output": str(target), **identity(target.read_bytes()), "objects": len(report["objects"]), "errors": report["errors"], "semantic_verdict": "not_assessed"}, ensure_ascii=False))


if __name__ == "__main__":
    main()
