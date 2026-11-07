"""Owned MCP accounting fixture for real-model learning comparisons.

The connection profile and this script stay fixed. A separate, retained binding
selects one fresh subject Store and business workspace after the previous
runtime has joined. No endpoint or external account is involved.
"""

import contextlib
import hashlib
import json
import os
import pathlib
import sqlite3
import sys
import time
import uuid


binding_bytes = pathlib.Path(sys.argv[1]).read_bytes()
binding = json.loads(binding_bytes)
workspace = pathlib.Path(binding["workspace"])
workspace.mkdir(exist_ok=True)
binding_digest = hashlib.sha256(binding_bytes).hexdigest()
session_id = str(uuid.uuid4())
wire_request_id = None


def append(record):
    record = {"observed_ns": time.time_ns(), "binding_sha256": binding_digest,
              "session_id": session_id, "wire_request_id": wire_request_id, **record}
    with (workspace / "exchanges.jsonl").open("a") as stream:
        stream.write(json.dumps(record, separators=(",", ":")) + "\n")
        stream.flush()
        os.fsync(stream.fileno())


def matches_wire(request, method, params):
    """Project the two supported rmcp 3.8.0 requests, retaining authored metadata."""
    if request.get("method") != method or not isinstance(params, dict):
        return False
    metadata = params.get("_meta")
    if (not isinstance(metadata, dict) or "progressToken" not in metadata
            or type(metadata["progressToken"]) not in (int, str)):
        return False
    frozen = request.get("params")
    if method == "tools/list" and frozen is None:
        frozen = {}
    if not isinstance(frozen, dict):
        return False
    # These are the fields of PaginatedRequestParams and CallToolRequestParams.
    # The SDK omits their optional None values; opaque argument values survive.
    if method == "tools/list":
        expected = {key: frozen[key] for key in ("cursor",) if frozen.get(key) is not None}
    elif method == "tools/call":
        if "name" not in frozen:
            return False
        expected = {"name": frozen["name"]}
        expected.update({key: frozen[key] for key in ("arguments", "inputResponses", "requestState")
                         if frozen.get(key) is not None})
    else:
        return False
    authored = frozen.get("_meta")
    if authored is not None and not isinstance(authored, dict):
        return False
    expected["_meta"] = {**(authored or {}), "progressToken": metadata["progressToken"]}
    # JSON booleans and numbers must not compare equal through Python's True == 1.
    return (json.dumps(expected, sort_keys=True, separators=(",", ":"), allow_nan=False)
            == json.dumps(params, sort_keys=True, separators=(",", ":"), allow_nan=False))


def producer(method, params):
    """Bind the actual request to armed T1, without inventing producer IDs."""
    with contextlib.closing(sqlite3.connect(
        pathlib.Path(binding["store_db"]).as_uri() + "?mode=ro", uri=True
    )) as source:
        source.execute("BEGIN")
        candidates = []
        rows = source.execute(
            "SELECT data FROM operations WHERE "
            "json_extract(data,'$.operation_name')='compat.mcp.request' "
            "AND json_extract(data,'$.state')='in_flight'"
        ).fetchall()
        cas = pathlib.Path(binding["store_cas"]).resolve()
        for (data,) in rows:
            operation = json.loads(data)
            reference = operation["final_input_ref"]
            digest = reference["digest"]
            hex_digest = digest.removeprefix("blake3:")
            if (not digest.startswith("blake3:") or len(hex_digest) != 64
                    or any(character not in "0123456789abcdef" for character in hex_digest)
                    or operation["input_digest"] != digest):
                raise RuntimeError("fixture frozen input digest is not canonical")
            path = cas / hex_digest[:2] / hex_digest[2:4] / hex_digest
            raw = path.read_bytes()
            if len(raw) != reference["bytes"]:
                raise RuntimeError("fixture frozen input length changed")
            frozen = json.loads(raw)
            request = frozen["request"]
            if frozen["unit"] != binding["unit"] or not matches_wire(request, method, params):
                continue
            attempts = source.execute(
                "SELECT data FROM attempts WHERE operation_id=?", [operation["operation_id"]]
            ).fetchall()
            if len(attempts) != 1:
                raise RuntimeError("fixture operation has ambiguous attempts")
            attempt = json.loads(attempts[0][0])
            task_row = source.execute("SELECT data FROM tasks WHERE task_id=?", [operation["task_id"]]).fetchone()
            if task_row is None:
                raise RuntimeError("fixture operation has no task")
            task = json.loads(task_row[0])
            if (attempt["dispatch_stage"] != "armed" or attempt.get("final_receipt_id") is not None
                    or attempt["operation_id"] != operation["operation_id"]
                    or attempt["armed_commit"] != operation["prepared_commit"]
                    or task["task_id"] != operation["task_id"] or task["agent_id"] != binding["agent_id"]
                    or task["kind"] != "compat.mcp.request" or task["definition_version"] != 1
                    or task["state"]["status"] != "running"
                    or task["state"]["invocation_id"] != attempt["invocation_id"]
                    or task["state"]["epoch"] != attempt["epoch"]):
                raise RuntimeError("fixture request was not armed before dispatch")
            candidates.append({
                "task_id": operation["task_id"],
                "operation_id": operation["operation_id"],
                "attempt_id": attempt["attempt_id"],
                "input_ref": reference,
                "input_sha256": hashlib.sha256(raw).hexdigest(),
            })
        # Identical concurrent requests cannot be distinguished by their wire
        # bodies. Retain that ambiguity instead of assigning a convenient ID.
        return candidates


TOOLS = [
    {"name": "ledger.read", "description": "Read the current balance, applied entries and pending submissions.",
     "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}},
    {"name": "ledger.adjust", "description": "Submit a signed integer adjustment. References are unique; a repeated reference is rejected. A submission may remain pending; ledger.read and ledger.job report actual state.",
     "inputSchema": {"type": "object", "properties": {
         "delta": {"type": "integer"}, "reference": {"type": "string", "minLength": 1}},
         "required": ["delta", "reference"], "additionalProperties": False}},
    {"name": "ledger.job", "description": "Read the retained state of one submission ticket.",
     "inputSchema": {"type": "object", "properties": {"ticket": {"type": "string"}},
         "required": ["ticket"], "additionalProperties": False}},
]


def answer(value, failed=False):
    return {"content": [{"type": "text", "text": json.dumps(value, separators=(",", ":"))}],
            "isError": failed}


def business(name, arguments):
    if not isinstance(arguments, dict):
        return answer({"status": "rejected", "reason": "arguments must be an object"}, True)
    with contextlib.closing(sqlite3.connect(workspace / "ledger.sqlite3")) as db:
        if name == "ledger.read":
            if arguments:
                return answer({"status": "rejected", "reason": "ledger.read takes no arguments"}, True)
            balance = db.execute("SELECT balance FROM account WHERE singleton=1").fetchone()[0]
            rows = db.execute("SELECT ticket,reference,delta,status FROM submissions ORDER BY rowid").fetchall()
            return answer({"balance": balance, "submissions": [
                dict(zip(("ticket", "reference", "delta", "status"), row)) for row in rows]})
        if name == "ledger.job":
            if set(arguments) != {"ticket"} or not isinstance(arguments["ticket"], str):
                return answer({"status": "rejected", "reason": "one ticket string is required"}, True)
            row = db.execute("SELECT reference,delta,status FROM submissions WHERE ticket=?", [arguments["ticket"]]).fetchone()
            if row is None:
                return answer({"status": "not_found", "ticket": arguments["ticket"]}, True)
            return answer({"ticket": arguments["ticket"], **dict(zip(("reference", "delta", "status"), row))})
        if name != "ledger.adjust":
            return answer({"status": "rejected", "reason": "unknown tool"}, True)
        if (set(arguments) != {"delta", "reference"} or type(arguments["delta"]) is not int
                or not -(2**31) <= arguments["delta"] < 2**31
                or not isinstance(arguments["reference"], str) or not 1 <= len(arguments["reference"]) <= 128):
            return answer({"status": "rejected", "reason": "delta and a nonempty reference are required"}, True)
        db.execute("BEGIN IMMEDIATE")
        existing = db.execute("SELECT ticket,status FROM submissions WHERE reference=?", [arguments["reference"]]).fetchone()
        if existing is not None:
            db.rollback()
            return answer({"status": "duplicate_reference", "ticket": existing[0], "retained_state": existing[1]}, True)
        balance = db.execute("SELECT balance FROM account WHERE singleton=1").fetchone()[0]
        if balance + arguments["delta"] < 0:
            db.rollback()
            return answer({"status": "rejected", "reason": "insufficient_balance", "available": balance,
                           "requested_delta": arguments["delta"]}, True)
        ticket = str(uuid.uuid4())
        status = "accepted" if binding["behavior"] == "accepted_only" else "applied"
        db.execute("INSERT INTO submissions(ticket,reference,delta,status) VALUES (?,?,?,?)",
                   [ticket, arguments["reference"], arguments["delta"], status])
        if status == "applied":
            db.execute("UPDATE account SET balance=balance+? WHERE singleton=1", [arguments["delta"]])
        db.commit()
        append({"phase": "business_commit", "ticket": ticket, "reference": arguments["reference"],
                "delta": arguments["delta"], "status": status})
        if binding["behavior"] == "drop_after_effect":
            os._exit(0)
        return answer({"status": status, "ticket": ticket})


if len(sys.argv) == 3:
    with contextlib.closing(sqlite3.connect(workspace / "ledger.sqlite3")) as db:
        if sys.argv[2] == "--initialize":
            db.execute("CREATE TABLE account(singleton INTEGER PRIMARY KEY CHECK(singleton=1),balance INTEGER NOT NULL)")
            db.execute("CREATE TABLE submissions(ticket TEXT PRIMARY KEY,reference TEXT NOT NULL UNIQUE,delta INTEGER NOT NULL,status TEXT NOT NULL)")
            db.execute("INSERT INTO account(singleton,balance) VALUES (1,?)", [binding["initial_balance"]])
            db.commit()
        elif sys.argv[2] == "--snapshot":
            db.execute("BEGIN")
            balance = db.execute("SELECT balance FROM account WHERE singleton=1").fetchone()[0]
            rows = db.execute("SELECT ticket,reference,delta,status FROM submissions ORDER BY rowid").fetchall()
            log = workspace / "exchanges.jsonl"
            exchanges = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            print(json.dumps({"observed_ns": time.time_ns(), "balance": balance,
                "submissions": [dict(zip(("ticket", "reference", "delta", "status"), row)) for row in rows],
                "exchanges": exchanges}), flush=True)
        elif sys.argv[2] == "--apply-pending":
            total = db.execute("SELECT coalesce(sum(delta),0) FROM submissions WHERE status='accepted'").fetchone()[0]
            db.execute("UPDATE account SET balance=balance+? WHERE singleton=1", [total])
            db.execute("UPDATE submissions SET status='applied' WHERE status='accepted'")
            db.commit()
        else:
            raise ValueError("unknown fixture action")
    sys.exit(0)


for line in sys.stdin:
    message = json.loads(line)
    method, identity = message.get("method"), message.get("id")
    wire_request_id = identity
    if identity is None:
        continue
    if method == "initialize":
        result = {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}},
                  "serverInfo": {"name": "owned-ledger-workspace", "version": "1"}}
    elif method == "tools/list":
        params = message.get("params", {})
        append({"phase": "request", "method": method, "params": params, "producers": producer(method, params)})
        result = {"tools": TOOLS}
    elif method == "tools/call":
        params = message["params"]
        producers = producer(method, params)
        append({"phase": "request", "method": method, "params": params, "producers": producers})
        result = (business(params["name"], params.get("arguments", {})) if len(producers) == 1
                  else answer({"status": "rejected", "reason": "no unique matching armed producer"}, True))
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": identity,
                          "error": {"code": -32601, "message": "unknown method"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": identity, "result": result}), flush=True)
