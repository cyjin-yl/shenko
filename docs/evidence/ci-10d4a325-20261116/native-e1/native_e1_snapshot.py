#!/usr/bin/env python3
"""Retain selected original fixture rows and CAS bytes, never the installation.

This is a read-only evidence export, not a byte-for-byte SQLite backup. Node
databases, owner-token tables, SSH configuration and keys are never selected.
Only typed references in the listed fixture namespaces can disclose CAS bytes.
"""

import argparse
import base64
from collections import deque
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import zipfile

TABLES = (
    "subject_genesis", "subject_branch_heads", "subject_steps", "tasks",
    "task_results", "operations", "attempts", "evidence_receipts",
    "evidence_receipt_conflicts", "evidence_key_conflicts",
    "reconciliation_cases", "subject_intents", "inbox_events", "session_events",
    "subject_event_dispositions", "control_prepared_transitions",
    "control_materializations", "native_evidence_materializations",
    "native_evidence_seal_debts", "native_evidence_deliveries",
    "context_windows", "context_window_scopes", "cognition_origins", "commit_log",
)
REQUIRED_TABLES = {
    "commit_log", "tasks", "operations", "attempts", "evidence_receipts",
    "control_materializations", "native_evidence_materializations",
}
NAMESPACES = {
    "test", "commit-authority", "input_plans", "model_outcomes",
    "native-e1-fixture",
}
SECRET_FIELDS = {
    "password", "passwd", "secret", "private_key", "api_key", "access_token",
    "refresh_token", "authorization", "cookie", "token", "token_hash",
    "bearer_token", "credential", "credentials", "client_secret",
    "owner_control_token",
}
DIGEST = re.compile(r"blake3:([0-9a-f]{64})\Z")
MAX_OBJECTS = 4096
MAX_BYTES = 64 * 1024 * 1024


def encoded(value):
    return (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode()


def sha256(raw):
    return hashlib.sha256(raw).hexdigest()


def secret_field(value):
    if isinstance(value, dict):
        for key, item in value.items():
            if key.lower() in SECRET_FIELDS and item not in (None, "", [], {}):
                return key
            if found := secret_field(item):
                return found
    elif isinstance(value, list):
        for item in value:
            if found := secret_field(item):
                return found
    elif isinstance(value, str) and value.lstrip()[:1] in ("{", "["):
        try:
            return secret_field(json.loads(value))
        except (json.JSONDecodeError, RecursionError):
            pass
    return None


def references(value, via):
    if isinstance(value, dict):
        if (
            isinstance(value.get("digest"), str)
            and DIGEST.fullmatch(value["digest"])
            and isinstance(value.get("bytes"), int)
            and value["bytes"] >= 0
            and all(key in value for key in ("media_type", "store_namespace", "codec"))
        ):
            yield value, via
            return
        for key, item in value.items():
            yield from references(item, f"{via}/{key}")
    elif isinstance(value, list):
        for index, item in enumerate(value):
            yield from references(item, f"{via}/{index}")
    elif isinstance(value, str) and value.lstrip()[:1] in ("{", "["):
        # RowTransition's Cell::Text preserves row JSON as an encoded string.
        # Follow its typed references without rewriting the original string,
        # and retain the extra decode step in the evidence traversal route.
        try:
            nested = json.loads(value)
        except json.JSONDecodeError:
            return
        yield from references(nested, f"{via}/<json>")


def cell(value):
    if isinstance(value, bytes):
        return {
            "sqlite_type": "blob", "base64": base64.b64encode(value).decode(),
            "bytes": len(value), "sha256": sha256(value),
        }
    return value


def snapshot(data_dir):
    database = data_dir / "shenko.db"
    if not database.exists():
        return None, [], {"reason": "Store database was not created before failure"}
    if database.is_symlink():
        raise ValueError("fixture database is a symlink")
    roots = []
    result = {
        "schema_version": 1, "source_database": str(database),
        "scope": "selected original row values from one read transaction; not a SQLite backup",
        "text_values": "original strings, including data JSON text, are not decoded or rewritten",
        "blob_values": "lossless base64 with byte length and SHA256",
        "tables": {}, "missing_tables": [], "omitted_tables": [], "excluded_rows": [],
        "omitted_columns": {"commit_log": "only ledger_id, seq, created_at, entity_count"},
    }
    with sqlite3.connect(database.resolve().as_uri() + "?mode=ro", uri=True) as connection:
        connection.row_factory = sqlite3.Row
        connection.execute("BEGIN")
        available = {
            row[0] for row in connection.execute(
                "SELECT name FROM sqlite_master WHERE type='table'"
            )
        }
        result["omitted_tables"] = sorted(available.difference(TABLES))
        for table in TABLES:
            if table not in available:
                result["missing_tables"].append(table)
                continue
            query = (
                "SELECT ledger_id,seq,created_at,entity_count FROM commit_log ORDER BY seq,ledger_id"
                if table == "commit_log" else f'SELECT rowid AS "__rowid__",* FROM "{table}" ORDER BY rowid'
            )
            kept = []
            for row in connection.execute(query):
                raw = dict(row)
                if field := secret_field(raw):
                    result["excluded_rows"].append({
                        "table": table, "rowid": raw.get("__rowid__"),
                        "reason": f"credential-bearing field {field}; original not disclosed",
                    })
                    continue
                values = {key: cell(value) for key, value in raw.items()}
                kept.append({
                    "values": values,
                    "text_sha256": {
                        key: sha256(value.encode("utf-8"))
                        for key, value in raw.items() if isinstance(value, str)
                    },
                })
                for key, value in raw.items():
                    if isinstance(value, str):
                        try:
                            roots.extend(references(json.loads(value), f"{table}/{raw.get('__rowid__')}/{key}"))
                        except json.JSONDecodeError:
                            pass
            result["tables"][table] = kept
        connection.rollback()
    return result, roots, None


def closure(data_dir, roots):
    pending = deque(roots)
    seen = set()
    expanded = set()
    retained = {}
    excluded = {}
    records = []
    total = 0
    cas = data_dir / "cas"
    while pending:
        reference, via = pending.popleft()
        signature = json.dumps(reference, sort_keys=True)
        if signature in seen:
            continue
        seen.add(signature)
        if len(seen) > MAX_OBJECTS * 4:
            raise ValueError("fixture reference budget exceeded")
        digest = DIGEST.fullmatch(reference["digest"]).group(1)
        record = {"reference": reference, "via": via}
        records.append(record)
        if reference["store_namespace"] not in NAMESPACES:
            record["excluded"] = "namespace not in fixture disclosure allowlist"
            excluded[digest] = record["excluded"]
            continue
        path = cas / digest[:2] / digest[2:4] / digest
        if path.is_symlink() or not path.resolve().is_relative_to(cas.resolve()):
            record["excluded"] = "CAS reference is not a regular fixture object path"
            excluded[digest] = record["excluded"]
            continue
        try:
            raw = path.read_bytes()
        except FileNotFoundError:
            record["missing"] = "referenced original CAS bytes absent; no replacement sealed"
            continue
        record.update({
            "actual_bytes": len(raw), "sha256": sha256(raw),
            "length_matches": len(raw) == reference["bytes"],
            "blake3_recomputed": False,
        })
        try:
            value = json.loads(raw)
        except (json.JSONDecodeError, UnicodeDecodeError):
            value = None
        if field := secret_field(value):
            record["excluded"] = f"credential-bearing field {field}; original not disclosed"
            excluded[digest] = record["excluded"]
            continue
        if digest not in retained:
            total += len(raw)
            if len(retained) >= MAX_OBJECTS or total > MAX_BYTES:
                raise ValueError("fixture CAS retention budget exceeded")
            retained[digest] = raw
        media = reference["media_type"]
        if (
            isinstance(media, str)
            and (media == "application/json" or media.endswith("+json"))
            and digest not in expanded
        ):
            expanded.add(digest)
            if value is not None:
                pending.extend(references(value, "cas/" + digest))
            else:
                record["parse_error"] = "declared JSON is not decodable; original bytes retained"
    # An excluded alias wins over an allowed alias of the same object.
    for digest in excluded:
        retained.pop(digest, None)
    return retained, {
        "references": records, "excluded_digests": excluded,
        "untyped_digests": "identity/artifact hashes without typed ObjectRef metadata are not disclosure authority",
        "scope": "typed reference closure only, with explicit missing/excluded objects; no complete-Store claim",
        "max_objects": MAX_OBJECTS, "max_bytes": MAX_BYTES,
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--additional-store", choices=("causal", "diverged"), action="append", default=[])
    args = parser.parse_args()
    inventory = []
    manifest = {
        "schema_version": 1, "source_revision": args.source_revision,
        "scope": "native E1 fixture selected original rows and typed CAS closure; read-only export",
        "not_a_full_sqlite_backup": True, "tables_selected": TABLES,
        "namespace_allowlist": sorted(NAMESPACES),
        "excluded_by_design": [
            "all Node databases", "owner_admin_tokens and other unselected tables",
            "SSH keys and configuration", "credential-bearing fields or non-allowlisted CAS namespaces",
        ],
        "stores": {}, "errors": [],
    }
    with zipfile.ZipFile(args.output, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        def write(name, raw):
            archive.writestr(name, raw)
            inventory.append({"member": name, "bytes": len(raw), "sha256": sha256(raw)})

        write("exporter.py", Path(__file__).read_bytes())
        for name in ("source", "receiver", "parked", *dict.fromkeys(args.additional_store)):
            try:
                rows, roots, absent = snapshot(args.root / name)
                if absent is not None:
                    manifest["stores"][name] = absent
                    manifest["errors"].append({"store": name, "error": absent["reason"]})
                    continue
                write(name + "/ledger-rows.json", encoded(rows))
                missing_required = sorted(REQUIRED_TABLES.intersection(rows["missing_tables"]))
                if missing_required or not rows["tables"].get("commit_log"):
                    manifest["errors"].append({
                        "store": name,
                        "error": "required Store schema or original commit rows are absent",
                        "missing_required_tables": missing_required,
                    })
                objects, report = closure(args.root / name, roots)
                write(name + "/cas-references.json", encoded(report))
                for digest, raw in objects.items():
                    write(f"{name}/cas/{digest[:2]}/{digest[2:4]}/{digest}", raw)
                manifest["stores"][name] = {
                    "retained_tables": list(rows["tables"]), "retained_objects": len(objects),
                    "missing_reference_count": sum("missing" in item for item in report["references"]),
                    "excluded_object_count": len(report["excluded_digests"]),
                    "excluded_row_count": len(rows["excluded_rows"]),
                }
            except Exception as error:
                manifest["errors"].append({"store": name, "error": f"{type(error).__name__}: {error}"})
        effects = args.root / "effects"
        if effects.is_file() and not effects.is_symlink():
            write("physical-effects.raw", effects.read_bytes())
        else:
            manifest["physical_effects"] = "file not created before failure"
        write("inventory.json", encoded(inventory))
        archive.writestr("manifest.json", encoded(manifest))
    print(json.dumps(manifest, ensure_ascii=False, indent=2))
    return 1 if manifest["errors"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
