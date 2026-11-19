#!/usr/bin/env python3
"""Read one fixed historical artifact; never execute or repair its contents.

A successful exit means bounded evidence inspection completed. It does not
change the original failed product run, its predicates, or any runtime state.
Only the Python standard library is used. CAS BLAKE3 is not reimplemented:
the known complete ZIP SHA256 authenticates these retained source bytes;
individual CAS resolutions additionally record path, length and SHA256.
"""
import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import resource
import signal
import stat
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile

REPOSITORY = "cyjin-yl/shenko"
RUN_ID = 43916477826
JOB_ID = 133471871117
ARTIFACT_ID = 14255345735
ZIP_BYTES = 8232052
ZIP_SHA256 = "ab362c507c24dae49f11429c73b58ecc81cf21c90ebf4d9cc99672323759e084"
HEAD = "be46be34593452f47b824d3ae62f625032e2d54c"
CHECKOUT = "5e7a3d5ade83d21fbc6e86b7eb8c3f19fb3fbe3c"
SOURCE_TREE = "58e5e59b230a5020ee56ccf800deea1466c3151b"
PARENT = "51a1ce435b3f83c52dcad11f19c522acb77cb59b"
MAX_MEMBERS = 25000
MAX_EXPANDED = 512 * 1024 * 1024
MAX_MEMBER = 256 * 1024 * 1024
MAX_JSON = 32 * 1024 * 1024
MAX_METADATA = 4 * 1024 * 1024
MAX_LOG_MATCHES = 2000
MAX_LINE = 256 * 1024
BLOCK = 256 * 1024


class InspectionError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise InspectionError(message)


def save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n",
                    encoding="utf-8")


def strict_json(raw):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, "duplicate JSON key")
            result[key] = value
        return result
    return json.loads(raw, object_pairs_hook=unique)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, response, code, message, headers, new_url):
        return None


def open_http(url, token=None):
    parsed = urllib.parse.urlsplit(url)
    require(parsed.scheme == "https" and parsed.username is None
            and parsed.password is None and parsed.port in (None, 443)
            and len(url) <= 16384, "unexpected download URL structure")
    headers = {"User-Agent": "shenko-fixed-model5-evidence-inspector"}
    if token is not None:
        require(parsed.hostname == "api.github.com", "token destination is not GitHub API")
        headers.update(Authorization="Bearer " + token,
                       Accept="application/vnd.github+json",
                       **{"X-GitHub-Api-Version": "2022-11-28"})
    else:
        host = parsed.hostname or ""
        require(host.endswith(".blob.core.windows.net")
                or host.endswith(".githubusercontent.com"),
                "unsigned download destination is outside artifact hosts")
    # No proxy, cookies, authentication handler or automatic redirect is reused.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    try:
        return opener.open(urllib.request.Request(url, headers=headers), timeout=30)
    except urllib.error.HTTPError as error:
        return error
    except Exception as error:
        # Do not stringify network exceptions: they may contain the signed URL.
        raise InspectionError("network request failed: " + type(error).__name__) from None


def bounded_read(response, limit):
    chunks = []
    size = 0
    while True:
        chunk = response.read(min(BLOCK, limit - size + 1))
        if not chunk:
            return b"".join(chunks)
        size += len(chunk)
        require(size <= limit, "HTTP body exceeded its fixed byte limit")
        chunks.append(chunk)


def api_json(out, token, suffix, name):
    url = "https://api.github.com/repos/" + REPOSITORY + suffix
    with open_http(url, token) as response:
        raw = bounded_read(response, MAX_METADATA)
        status = response.status
    destination = out / "source-api" / name
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(raw)
    require(status == 200, "GitHub metadata HTTP status " + str(status))
    return strict_json(raw)


def verify_metadata(out, token):
    run = api_json(out, token, "/actions/runs/" + str(RUN_ID), "run.json")
    job = api_json(out, token, "/actions/jobs/" + str(JOB_ID), "job.json")
    artifact = api_json(out, token, "/actions/artifacts/" + str(ARTIFACT_ID), "artifact.json")
    head = api_json(out, token, "/git/commits/" + HEAD, "head-commit.json")
    checkout = api_json(out, token, "/git/commits/" + CHECKOUT, "checkout-commit.json")
    parent = api_json(out, token, "/git/commits/" + PARENT, "diagnostic-parent.json")
    require(run.get("id") == RUN_ID and run.get("head_sha") == HEAD
            and run.get("status") == "completed" and run.get("conclusion") == "failure",
            "historical run identity or terminal outcome differs")
    require(job.get("id") == JOB_ID and job.get("run_id") == RUN_ID
            and job.get("status") == "completed" and job.get("conclusion") == "failure",
            "historical job identity or terminal outcome differs")
    require(artifact.get("id") == ARTIFACT_ID
            and artifact.get("name") == "real-single-node-" + CHECKOUT
            and artifact.get("size_in_bytes") == ZIP_BYTES
            and artifact.get("digest") == "sha256:" + ZIP_SHA256
            and artifact.get("expired") is False
            and artifact.get("workflow_run", {}).get("id") == RUN_ID
            and artifact.get("workflow_run", {}).get("head_sha") == HEAD,
            "artifact metadata differs from the fixed source")
    require(head.get("sha") == HEAD and checkout.get("sha") == CHECKOUT
            and head.get("tree", {}).get("sha") == SOURCE_TREE
            and checkout.get("tree", {}).get("sha") == SOURCE_TREE
            and parent.get("sha") == PARENT, "source Git objects differ")
    return {"run_id": RUN_ID, "job_id": JOB_ID, "head": HEAD, "checkout": CHECKOUT,
            "tree": SOURCE_TREE, "artifact_id": ARTIFACT_ID,
            "run_conclusion": run["conclusion"], "job_conclusion": job["conclusion"],
            "job_steps": job.get("steps"), "metadata_verified": True}


def download_zip(out, token):
    url = "https://api.github.com/repos/" + REPOSITORY + "/actions/artifacts/" + str(ARTIFACT_ID) + "/zip"
    with open_http(url, token) as response:
        require(response.status == 302, "artifact endpoint did not return the expected redirect")
        location = response.headers.get("Location")
        require(isinstance(location, str), "artifact redirect has no location")
    # The signed location remains only in memory, and never receives a token.
    destination = out / "model5.original.zip"
    require(not destination.exists(), "refusing to replace an existing original ZIP")
    for redirect_count in range(3):
        with open_http(location) as response:
            if response.status in (301, 302, 303, 307, 308):
                location = response.headers.get("Location")
                require(isinstance(location, str), "unsigned redirect has no location")
                continue
            require(response.status == 200, "artifact storage HTTP status " + str(response.status))
            length = response.headers.get("Content-Length")
            require(length is None or int(length) == ZIP_BYTES, "artifact Content-Length differs")
            digest = hashlib.sha256()
            size = 0
            with destination.open("xb") as target:
                while True:
                    chunk = response.read(BLOCK)
                    if not chunk:
                        break
                    size += len(chunk)
                    require(size <= ZIP_BYTES, "ZIP exceeded the fixed original byte count")
                    target.write(chunk)
                    digest.update(chunk)
            receipt = {"bytes": size, "sha256": digest.hexdigest(),
                       "expected_bytes": ZIP_BYTES, "expected_sha256": ZIP_SHA256,
                       "verified": size == ZIP_BYTES and digest.hexdigest() == ZIP_SHA256,
                       "api_redirect_status": 302, "unsigned_redirects": redirect_count,
                       "authorization_sent_only_to": "api.github.com",
                       "signed_location_retained": False}
            save(out / "download-receipt.json", receipt)
            require(size == ZIP_BYTES and digest.hexdigest() == ZIP_SHA256,
                    "downloaded ZIP does not match the fixed original")
            return destination
    raise InspectionError("unsigned artifact redirect limit exceeded")


def checked_members(archive):
    infos = archive.infolist()
    require(len(infos) <= MAX_MEMBERS, "ZIP member count exceeded")
    seen = set()
    total = 0
    for info in infos:
        name = info.filename
        require(name == info.orig_filename and "\x00" not in name and "\\" not in name
                and not any(ord(char) < 32 for char in name), "unsafe ZIP member name")
        path = PurePosixPath(name)
        parts = name.rstrip("/").split("/")
        require(name and len(name) <= 1024 and not path.is_absolute() and all(part not in ("", ".", "..") for part in parts)
                and not any(":" in part for part in parts), "unsafe ZIP member path")
        normalized = "/".join(parts)
        require(normalized not in seen, "duplicate ZIP member path")
        seen.add(normalized)
        mode = (info.external_attr >> 16) & 0xffff
        kind = stat.S_IFMT(mode)
        require(kind in (0, stat.S_IFREG, stat.S_IFDIR) and not stat.S_ISLNK(mode),
                "ZIP contains a link or special file")
        require(not info.flag_bits & 1, "encrypted ZIP member")
        require(info.compress_type in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED),
                "unexpected ZIP compression method")
        require(0 <= info.file_size <= MAX_MEMBER, "ZIP member size exceeded")
        total += info.file_size
        require(total <= MAX_EXPANDED, "ZIP expanded byte limit exceeded")
    return infos, total


def key_original(name):
    path = PurePosixPath(name)
    return ("installation" not in path.parts and "cas" not in path.parts
            and path.suffix in (".json", ".jsonl", ".log", ".txt", ".sha256"))


def inventory_zip(out, archive):
    infos, declared = checked_members(archive)
    records = []
    actual_total = 0
    for info in infos:
        digest = hashlib.sha256()
        actual = 0
        target = None
        if not info.is_dir() and key_original(info.filename):
            target_path = out / "key-originals" / info.filename
            target_path.parent.mkdir(parents=True, exist_ok=True)
            target = target_path.open("xb")
        try:
            with archive.open(info) as member:
                while True:
                    chunk = member.read(BLOCK)
                    if not chunk:
                        break
                    actual += len(chunk)
                    actual_total += len(chunk)
                    require(actual <= info.file_size and actual_total <= MAX_EXPANDED,
                            "expanded ZIP stream exceeded its declared bounds")
                    digest.update(chunk)
                    if target is not None:
                        target.write(chunk)
        finally:
            if target is not None:
                target.close()
        require(actual == info.file_size, "ZIP member length differs from its central directory")
        records.append({"path": info.filename, "bytes": actual, "sha256": digest.hexdigest(),
                        "compressed_bytes": info.compress_size, "crc32": format(info.CRC, "08x"),
                        "compression": info.compress_type, "external_attr": info.external_attr,
                        "header_offset": info.header_offset, "date_time": info.date_time,
                        "key_original_retained": key_original(info.filename) and not info.is_dir()})
        with (out / "members.verified.jsonl").open("a", encoding="utf-8") as journal:
            journal.write(json.dumps(records[-1], ensure_ascii=False) + "\n")
    require(actual_total == declared, "ZIP expanded totals differ")
    save(out / "members.json", {"members": records, "expanded_bytes": actual_total,
                              "member_count": len(records), "all_streams_read_and_crc_checked": True})
    return {record["path"]: record for record in records}


class Evidence:
    def __init__(self, out, archive, members):
        self.out, self.archive, self.members = out, archive, members
        self.missing, self.parse_errors, self.cas = [], [], []
        candidates = [name for name in members if name.endswith("real-single-node/manifest.json")]
        require(len(candidates) <= 1, "ambiguous journey manifest path")
        self.base = candidates[0].rsplit("/", 1)[0] + "/" if candidates else "real-single-node/"

    def json(self, name, optional=True):
        if name not in self.members:
            self.missing.append(name)
            if optional:
                return None
            raise InspectionError("required JSON member is missing")
        require(self.members[name]["bytes"] <= MAX_JSON, "JSON member exceeds parsing budget")
        try:
            return strict_json(self.archive.read(name))
        except (UnicodeError, json.JSONDecodeError, RecursionError, InspectionError) as error:
            self.parse_errors.append({"path": name, "error_type": type(error).__name__})
            raise InspectionError("retained JSON member could not be parsed: " + name) from None

    def journey(self, name):
        return self.json(self.base + name)

    def resolve_cas(self, reference, origin):
        if not isinstance(reference, dict):
            return None
        digest = reference.get("digest")
        if not isinstance(digest, str) or not re.fullmatch(r"blake3:[0-9a-f]{64}", digest):
            self.cas.append({"origin": origin, "available": False, "reason": "reference is not a BLAKE3 object"})
            return None
        value = digest.removeprefix("blake3:")
        name = self.base + "cas/" + value[:2] + "/" + value[2:4] + "/" + value
        record = {"origin": origin, "reference": reference, "path": name,
                  "blake3_reverified": False,
                  "integrity_scope": "fixed original ZIP SHA256 plus member SHA256 and reference byte length"}
        if name not in self.members:
            record.update(available=False, reason="referenced CAS member absent")
            self.cas.append(record)
            return None
        info = self.members[name]
        require(info["bytes"] <= MAX_JSON, "referenced CAS JSON exceeds parsing budget")
        require(type(reference.get("bytes")) is int and reference["bytes"] == info["bytes"],
                "CAS reference byte length differs")
        record.update(available=True, bytes=info["bytes"], sha256=info["sha256"])
        self.cas.append(record)
        target = self.out / "key-originals" / name
        if not target.exists():
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(self.archive.read(name))
        return self.json(name)


def rows_at(facts, table):
    if not isinstance(facts, dict) or table not in facts:
        return None
    rows = facts[table]
    require(isinstance(rows, list), "ledger table is not an array: " + table)
    return rows


def row_data(row):
    return row.get("data", row) if isinstance(row, dict) else row


def counted(rows, field):
    if rows is None:
        return {"available": False}
    values = Counter(json.dumps(row_data(row)[field], sort_keys=True, ensure_ascii=False)
                     if field in row_data(row) else "<field absent>" for row in rows)
    return {"available": True, "rows": len(rows), "field": field,
            "values": dict(sorted(values.items()))}


def outcome_succeeded(result):
    outcome = result.get("outcome")
    return outcome == "succeeded" or isinstance(outcome, dict) and outcome.get("status") == "succeeded"


def contains_expected_product(text):
    return isinstance(text, str) and re.search(r"(?<!\d)437(?!\d)", text) is not None


def evaluate_first(facts, marker):
    # Copied from the original be46 journey; this is a separate observation,
    # never a rewrite of summary.json or a substitute for its other checks.
    tasks = {row["task_id"]: row["data"] for row in facts.get("tasks", [])}
    results = [row["data"] for row in facts.get("task_results", [])]
    model = [row for row in results if tasks.get(row["task_id"], {}).get("kind") == "model.child"
             and outcome_succeeded(row) and row.get("result", {}).get("termination") == "completed"
             and contains_expected_product(row.get("result", {}).get("answer"))]
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
               and contains_expected_product(row["decision"]["output"]["text"])]
    return {
        "finite_model_child_succeeded": bool(model),
        "native_host_process_exact_result": bool(native),
        "both_children_have_attempt_and_e1": len(required) == 2 and operation_ids <= attempts & evidence,
        "committed_output_after_child_results": bool(outputs),
    }


def scan_logs(evidence):
    keywords = ("rejected", "invalid", "unknown", "timeout", "timed out", "deadline",
                "protocol", "panic", "fatal", "error", "cancel", "shutdown", "r0")
    results = {}
    for name, member in evidence.members.items():
        if not name.endswith((".runtime.log", "model-server.log", "failure.txt")):
            continue
        matches, counts, total, omitted = [], Counter(), 0, 0
        with evidence.archive.open(name) as source:
            while True:
                raw = source.readline(MAX_LINE + 1)
                if not raw:
                    break
                total += 1
                require(len(raw) <= MAX_LINE, "log line exceeds the parsing limit")
                line = raw.decode("utf-8", errors="replace").rstrip("\r\n")
                lowered = line.lower()
                kinds = [word for word in keywords if word in lowered]
                wire = re.search(r"\b(GET|POST|PUT|DELETE|PATCH)\s+(/[^\s]+)", line)
                if wire:
                    kinds.append("http_log_line")
                    counts[wire.group(1) + " " + wire.group(2)] += 1
                if kinds:
                    if len(matches) < MAX_LOG_MATCHES:
                        matches.append({"line": total, "matched_terms": kinds, "text": line})
                    else:
                        # Retain the earliest context and the latest terminal diagnostics.
                        matches.pop(MAX_LOG_MATCHES // 2)
                        matches.append({"line": total, "matched_terms": kinds, "text": line})
                        omitted += 1
        results[name] = {"source_sha256": member["sha256"], "lines": total,
                         "http_log_line_counts": dict(counts), "matches": matches,
                         "omitted_match_count": omitted,
                         "interpretation": "literal text/log matches only; not request count or causal proof",
                         "decoding": "UTF-8 replacement is only for this derivative; original bytes retained"}
    save(evidence.out / "log-observations.json", results)
    return {name: {key: value for key, value in info.items() if key != "matches"}
            for name, info in results.items()}, results


def protocol_observations(evidence, facts):
    terms = ("reject", "protocol", "malformed", "invalid", "output_error", "r0")
    matches = []
    availability = {}
    for table in ("session_events", "inbox_events"):
        rows = rows_at(facts, table)
        availability[table] = rows is not None
        if rows is None:
            continue
        for index, row in enumerate(rows):
            rendered = json.dumps(row, ensure_ascii=False, sort_keys=True)
            found = [term for term in terms if term in rendered.lower()]
            if found:
                matches.append({
                    "source": evidence.base + "final.ledger.json#/" + table + "/" + str(index),
                    "matched_terms": found, "original_row": row})
    save(evidence.out / "protocol-observations.json", {
        "table_availability": availability, "matches": matches,
        "interpretation": "literal retained-fact matches, not an inferred rejection count or failure cause"})
    return {"table_availability": availability, "matching_rows": len(matches),
            "complete_derivative": "protocol-observations.json",
            "last_matches": [
                {"source": entry["source"], "matched_terms": entry["matched_terms"],
                 "row_excerpt": json.dumps(entry["original_row"], ensure_ascii=False)[:2048],
                 "truncated": len(json.dumps(entry["original_row"], ensure_ascii=False)) > 2048,
                 "excerpt_limit_characters": 2048}
                for entry in matches[-20:]],
            "interpretation": "literal matches only; full unchanged rows remain in the original ledger"}


def analyze(out, archive, members):
    evidence = Evidence(out, archive, members)
    manifest = evidence.journey("manifest.json")
    summary = evidence.journey("summary.json")
    if manifest is not None:
        require(manifest.get("source_revision") == CHECKOUT, "journey manifest source differs")
    models = evidence.journey("models.json")
    snapshots = {phase: evidence.journey(phase + ".ledger.json")
                 for phase in ("first.paused", "first.accepted", "first", "before-restart.paused",
                               "restart.paused", "restart.accepted", "restart", "final")}
    facts = snapshots["final"]
    required = ("tasks", "task_results", "attempts", "evidence_receipts", "decisions")
    recomputed = {}
    if manifest is not None:
        for phase in ("first", "final"):
            snapshot = snapshots[phase]
            if isinstance(snapshot, dict) and all(key in snapshot for key in required):
                recomputed[phase] = {
                    "source": evidence.base + phase + ".ledger.json",
                    "checks": evaluate_first(snapshot, manifest["marker"]),
                    "scope": "only original four child/result/output predicates, no overall acceptance claim"}
            else:
                recomputed[phase] = {"available": False, "reason": "complete required snapshot unavailable"}
    requests = rows_at(facts, "subject_request_attempts")
    request_details = []
    if requests is not None:
        for index, row in enumerate(requests):
            attempt = row_data(row)
            origin = evidence.base + "final.ledger.json#/subject_request_attempts/" + str(index) + "/data"
            plan = evidence.resolve_cas(attempt.get("input_plan_ref"), origin + "/input_plan_ref")
            detail = {"source": origin, "attempt": attempt}
            if plan is not None:
                payload = plan.get("payload")
                detail["frozen_plan"] = {
                    "schema_version": plan.get("schema_version"), "request_id": plan.get("request_id"),
                    "step_id": plan.get("step_id"), "context": plan.get("context"),
                    "message_count": len(payload.get("messages", [])) if isinstance(payload, dict) else None,
                    "reserved_max_tokens": payload.get("max_tokens") if isinstance(payload, dict) else None}
                detail["plan_identity_matches_attempt"] = (
                    all(isinstance(value, str) and value for value in
                        (plan.get("request_id"), plan.get("step_id"),
                         attempt.get("request_id"), attempt.get("step_id")))
                    and plan["request_id"] == attempt["request_id"]
                    and plan["step_id"] == attempt["step_id"])
            terminal = attempt.get("terminal")
            if isinstance(terminal, dict):
                result = evidence.resolve_cas(terminal.get("result_ref"), origin + "/terminal/result_ref")
                if result is not None:
                    detail["terminal_result"] = result
            request_details.append(detail)
    save(out / "request-observations.json", {
        "available": requests is not None, "requests": request_details,
        "interpretation": "durable attempt/terminal facts; unknown remains unknown, absence is not success"})
    save(out / "cas-resolution.json", evidence.cas)
    table_fields = {
        "subject_branch_heads": "mode", "subject_steps": "state", "tasks": "kind",
        "task_results": "outcome", "operations": "state", "attempts": "dispatch_stage",
        "evidence_receipts": "decode_status", "subject_request_attempts": "state",
        "subject_quiesce_requests": "state", "subject_intents": "state"}
    table_counts = {table: counted(rows_at(facts, table), field)
                    for table, field in table_fields.items()}
    save(out / "ledger-facts.json", {
        "source": evidence.base + "final.ledger.json", "available": facts is not None,
        "counts": table_counts,
        "original_facts": {key: rows_at(facts, key) for key in
                           ("subject_branch_heads", "subject_steps", "task_results", "operations",
                            "attempts", "evidence_receipts", "decisions", "subject_request_results",
                            "subject_quiesce_requests", "subject_intents")},
        "table_absence_is_not_zero": True})
    logs, log_details = scan_logs(evidence)
    protocol = protocol_observations(evidence, facts)
    reported_checks = summary.get("checks") if isinstance(summary, dict) else None
    clean = {}
    for key in ("first_exit", "last_exit"):
        present = isinstance(summary, dict) and key in summary
        value = summary[key] if present else None
        clean[key] = {"observed": present, "exit": value,
                      "clean": value == 0 if present and type(value) is int else None,
                      "source": evidence.base + "summary.json#/" + key}
    provider_prefix = evidence.base.removesuffix("real-single-node/")
    runtime_invocation = evidence.json(provider_prefix + "model-runtime-invocation.json")
    runtime_loaded = evidence.json(provider_prefix + "model-runtime-loaded.json")
    if runtime_invocation is not None:
        require(runtime_invocation.get("source_revision") == CHECKOUT,
                "model runtime invocation source differs")
    analysis = {
        "schema_version": 1,
        "scope": "read-only analysis of one authenticated historical ZIP; no model/CLI/browser execution",
        "source": {"run_id": RUN_ID, "job_id": JOB_ID, "head": HEAD, "checkout": CHECKOUT,
                   "tree": SOURCE_TREE, "artifact_id": ARTIFACT_ID, "zip_sha256": ZIP_SHA256},
        "original_summary_path": evidence.base + "summary.json",
        "original_summary": summary, "original_reported_checks": reported_checks,
        "original_manifest": manifest, "served_models": models,
        "model_runtime_invocation": runtime_invocation, "model_runtime_loaded": runtime_loaded,
        "final_ledger_available": facts is not None, "table_counts": table_counts,
        "durable_requests": [{"source": row["source"],
                             "request_id": row["attempt"].get("request_id"),
                             "attempt_id": row["attempt"].get("attempt_id"),
                             "state": row["attempt"].get("state"),
                             "created_commit": row["attempt"].get("created_commit"),
                             "terminal": row["attempt"].get("terminal"),
                             "frozen_plan": row.get("frozen_plan")}
                            for row in request_details],
        "recomputed_original_predicates": recomputed,
        "restart_evidence": {name: evidence.base + name in members for name in
                             ("restart.self.json", "restart.runtime.log", "restart.ledger.json",
                              "restart.input.json", "restart.websocket.jsonl")},
        "clean_stop": clean, "log_summary": logs, "protocol_fact_matches": protocol,
        "missing_members": sorted(set(evidence.missing)), "parse_errors": evidence.parse_errors,
        "limits": {"zip_bytes": ZIP_BYTES, "members": MAX_MEMBERS, "expanded_bytes": MAX_EXPANDED,
                   "member_bytes": MAX_MEMBER, "parsed_json_bytes": MAX_JSON},
        "acceptance_boundary": "original failure is unchanged; missing checks/stages are unconfirmed; "
                               "provider terminal facts are not child success, Unknown is not failure, "
                               "HTTP log matches are not an authoritative request count",
        "cas_boundary": "known ZIP SHA256 and member SHA256/length only; no stdlib BLAKE3 verifier",
        "new_product_tests_run": False}
    save(out / "analysis.json", analysis)
    # Keep stdout reviewable without dumping prompts, provider answers or source host data.
    console = {key: analysis[key] for key in
               ("source", "original_summary", "final_ledger_available", "table_counts",
                "recomputed_original_predicates", "restart_evidence", "clean_stop",
                "protocol_fact_matches", "missing_members", "acceptance_boundary", "cas_boundary")}
    console["durable_requests"] = [
        {key: row[key] for key in ("source", "request_id", "attempt_id", "state",
                                  "created_commit", "terminal")}
        for row in analysis["durable_requests"]]
    console["runtime_diagnostic_lines"] = {
        name: info["matches"][-30:] for name, info in log_details.items()
        if name.endswith((".runtime.log", "failure.txt"))}
    rendered = json.dumps(console, ensure_ascii=False, indent=2)
    if len(rendered.encode("utf-8")) > 1024 * 1024:
        original_errors = summary.get("errors") if isinstance(summary, dict) else None
        errors = original_errors if isinstance(original_errors, list) else []
        checks = reported_checks if isinstance(reported_checks, dict) else {}
        compact = {
            "console_truncated": True, "complete_derivative": "analysis.json",
            "source": analysis["source"], "clean_stop": clean,
            "restart_evidence": analysis["restart_evidence"],
            "original_product_passed": summary.get("passed") if isinstance(summary, dict) else None,
            "summary_errors_available": isinstance(original_errors, list),
            "summary_error_count": len(errors) if isinstance(original_errors, list) else None,
            "summary_error_excerpts": [
                {"source": evidence.base + "summary.json#/errors/" + str(index),
                 "text": str(error)[:4096], "truncated": len(str(error)) > 4096}
                for index, error in enumerate(errors[:10])],
            "reported_check_count": len(checks) if isinstance(reported_checks, dict) else None,
            "reported_checks": {str(key)[:256]: value if type(value) is bool else str(value)[:256]
                                for key, value in list(checks.items())[:100]},
            "final_ledger_available": facts is not None,
            "table_counts": {table: {key: value for key, value in info.items() if key != "values"}
                             for table, info in table_counts.items()},
            "durable_request_count": len(request_details) if requests is not None else None,
            "runtime_diagnostic_excerpts": {
                name: [{"source_path": name, "line": entry["line"], "text": entry["text"][:1024],
                        "truncated": len(entry["text"]) > 1024}
                       for entry in info["matches"][-10:]]
                for name, info in list((name, info) for name, info in log_details.items()
                                       if name.endswith((".runtime.log", "failure.txt")))[:6]},
            "excerpt_boundary": "At most 10 errors/100 checks/6 logs x 10 lines; text excerpts "
                                "are bounded. Complete original and derived files are uploaded.",
            "acceptance_boundary": analysis["acceptance_boundary"]}
        rendered = json.dumps(compact, ensure_ascii=False, indent=2)
    print(rendered)
    return {"analysis_complete": True, "original_product_passed":
            summary.get("passed") if isinstance(summary, dict) else None}


def output_index(out):
    files = []
    for path in sorted(out.rglob("*")):
        if path.is_file() and path.name != "output-index.json":
            digest = hashlib.sha256()
            with path.open("rb") as source:
                for chunk in iter(lambda: source.read(BLOCK), b""):
                    digest.update(chunk)
            files.append({"path": str(path.relative_to(out)), "bytes": path.stat().st_size,
                          "sha256": digest.hexdigest()})
    save(out / "output-index.json", {"files": files})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    state = {"inspection_complete": False, "original_product_outcome_unchanged": True,
             "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
             "diagnostic_source_revision": os.environ.get("GITHUB_SHA"),
             "diagnostic_run_id": os.environ.get("GITHUB_RUN_ID"),
             "inspector_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
             "python": sys.version, "phase": "initializing", "errors": []}
    def deadline(_signum, _frame):
        raise InspectionError("fixed 480 second inspection deadline reached")
    signal.signal(signal.SIGALRM, deadline)
    signal.alarm(480)
    try:
        resource.setrlimit(resource.RLIMIT_AS, (1024 * 1024 * 1024, 1024 * 1024 * 1024))
        token = os.environ.pop("GH_TOKEN", None)
        require(isinstance(token, str) and bool(token), "read-only GitHub token unavailable")
        state["phase"] = "metadata"
        save(out / "inspection-state.json", state)
        state["metadata"] = verify_metadata(out, token)
        state["phase"] = "download"
        save(out / "inspection-state.json", state)
        original = download_zip(out, token)
        del token
        state["phase"] = "member_inventory"
        save(out / "inspection-state.json", state)
        with zipfile.ZipFile(original) as archive:
            members = inventory_zip(out, archive)
            state["phase"] = "analysis"
            save(out / "inspection-state.json", state)
            state.update(analyze(out, archive, members))
        state["inspection_complete"] = True
        state["phase"] = "complete"
    except Exception as error:
        # Only our fixed diagnostics are safe to stringify; library network
        # exceptions may embed a signed location. Originals remain untouched.
        message = str(error) if isinstance(error, InspectionError) else type(error).__name__
        state["errors"].append({"type": type(error).__name__, "message": message})
        print(json.dumps({"inspection_failed": True, "phase": state["phase"],
                          "errors": state["errors"]}, ensure_ascii=False))
    finally:
        signal.alarm(0)
        state["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        save(out / "inspection-state.json", state)
        output_index(out)
    return 0 if state["inspection_complete"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
