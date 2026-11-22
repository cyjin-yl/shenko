#!/usr/bin/env python3
"""Apply the retained CDF outer-harness audit method to an original job log."""
import argparse
import hashlib
import json
from pathlib import Path
import re

RESULT = re.compile(r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out; finished in ([\d.]+)s")
RUNNING = re.compile(r"^running (\d+) tests?$")
HEADER = re.compile(r"^(?:Running .+\(.+\)|Doc-tests \S+)$")
TIMESTAMP = re.compile(r"^\d{4}-\d\d-\d\dT\S+\s+")
COUNTS = ["passed", "failed", "ignored", "measured", "filtered"]


def result(line_number, line):
    match = RESULT.search(line)
    return [line_number, match[1], *map(int, match.group(2, 3, 4, 5, 6)), float(match[7])] if match else None


def counts(items):
    return {key: sum(item[index + 2] for item in items) for index, key in enumerate(COUNTS)}


def audit(path, run_id, job_id, sha):
    raw = path.read_bytes()
    numbered = [(number, TIMESTAMP.sub("", line).strip())
                for number, line in enumerate(raw.decode().splitlines(), 1)]
    starts = [index for index, (_, line) in enumerate(numbered)
              if line.startswith("##[group]Run cargo test --workspace ")]
    if len(starts) != 1:
        raise ValueError(f"Expected one workspace command group, found {len(starts)}")
    start = starts[0]
    end = next((index for index in range(start + 1, len(numbered))
                if numbered[index][1].startswith("##[group]Run ")), len(numbered))
    targets, stray = [], []
    for number, line in numbered[start:end]:
        if HEADER.fullmatch(line):
            targets.append({"header_line": number, "header": line,
                            "run_declarations": [], "results": [], "skip_lines": []})
        declaration, found = RUNNING.fullmatch(line), result(number, line)
        if not targets:
            if found:
                stray.append(found)
            continue
        target = targets[-1]
        if declaration:
            target["run_declarations"].append([number, int(declaration[1])])
        if found:
            target["results"].append(found)
        if "SKIP" in line:
            target["skip_lines"].append([number, line])

    outer, nested, non_libtest = [], [], []
    validation = {"stray": stray, "mismatches": [], "outer_filtered": [],
                  "outer_non_ok": [], "missing_results": [], "explicit_non_ok": []}
    for target in targets:
        results = target["results"]
        if not results:
            if target["skip_lines"] and not target["run_declarations"]:
                non_libtest.append(target)
            else:
                validation["missing_results"].append(target["header_line"])
            continue
        final = results[-1]
        outer.append(final)
        nested.extend(results[:-1])
        declarations = target["run_declarations"]
        if not declarations or declarations[0][1] != sum(final[2:6]):
            validation["mismatches"].append({"header_line": target["header_line"],
                "first_running": declarations[0] if declarations else None, "outer_result": final})
        if final[6]:
            validation["outer_filtered"].append(final)
        if final[1] != "ok" or final[3]:
            validation["outer_non_ok"].append(final)

    explicit = []
    for index, (number, line) in enumerate(numbered):
        if not line.startswith(("##[group]Run python3 scripts/ci/ignored_test.py ",
                                "##[group]Run python3 scripts/ci/native_ssh_memory_helper.py run ")):
            continue
        boundary = next((other for other in range(index + 1, len(numbered))
                         if numbered[other][1].startswith("##[group]Run ")), len(numbered))
        section = numbered[index:boundary]
        results = [found for n, value in section if (found := result(n, value))]
        proof = {"command_line": number, "command": line,
                 "outer_result": results[-1] if results else None,
                 "nested_results": results[:-1],
                 "source_revision_lines": [[n, value] for n, value in section
                                           if "SHENKO_SOURCE_REVISION:" in value]}
        if not results or results[-1][1] != "ok" or results[-1][3]:
            validation["explicit_non_ok"].append(number)
        if "partition_orchestrator" in line:
            receipts = []
            for n, value in section:
                if not value.startswith("{"):
                    continue
                try:
                    document = json.loads(value)
                except json.JSONDecodeError:
                    continue
                if isinstance(document, dict) and "checks" in document and "failed_checks" in document:
                    receipts.append((n, value, document))
            if len(receipts) == 1:
                n, value, document = receipts[0]
                receipt_path = path.parent / "partition-receipt-from-job-log.json"
                receipt_bytes = (value + "\n").encode()
                if receipt_path.exists():
                    if receipt_path.read_bytes() != receipt_bytes:
                        raise ValueError("A different partition receipt already occupies this run directory")
                else:
                    receipt_path.write_bytes(receipt_bytes)
                proof["receipt"] = {"raw_line": n, "path": str(receipt_path),
                    "bytes": len(receipt_bytes), "sha256": hashlib.sha256(receipt_bytes).hexdigest(),
                    "state": document.get("state"), "check_count": len(document["checks"]),
                    "checks": [{"check": item.get("check"), "passed": item.get("passed")}
                               for item in document["checks"]],
                    "failed_checks": document["failed_checks"], "scope": document.get("scope"),
                    "limits": document.get("limits")}
                if document.get("state") != "passed" or document["failed_checks"]:
                    validation["explicit_non_ok"].append(number)
            else:
                proof["receipt_count"] = len(receipts)
                validation["explicit_non_ok"].append(number)
        explicit.append(proof)

    checkout = [[number, line] for number, line in numbered[:start]
                if line.startswith("HEAD is now at ") or re.fullmatch(r"[0-9a-f]{40}", line)]
    return {"kind": "rust_workspace_outer_harness_audit", "run_id": run_id, "job_id": job_id,
        "head_sha": sha, "raw_log": {"path": str(path), "bytes": len(raw),
            "sha256": hashlib.sha256(raw).hexdigest(), "lines": len(numbered)},
        "command": list(numbered[start]), "checkout": checkout,
        "outer_summary": {"cargo_targets": len(targets),
            "executable_targets": sum(target["header"].startswith("Running ") for target in targets),
            "doctest_targets": sum(target["header"].startswith("Doc-tests ") for target in targets),
            "result_rows": len(outer), **counts(outer), "non_libtest_skip_targets": len(non_libtest)},
        "nested_summary": {"result_rows": len(nested), **counts(nested)}, "validation": validation,
        "method": "Use final libtest result in each Cargo Running/Doc-tests target. Retain earlier subprocess results separately; verify first running-N count against outer result. Preserve separate harness=false SKIP.",
        "explicit_long_proofs": explicit,
        "explicit_preparations": [[number, line] for number, line in numbered
            if line.startswith("##[group]Run python3 scripts/ci/native_ssh_memory_helper.py prepare")],
        "targets": targets}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("log", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--run-id", required=True, type=int)
    parser.add_argument("--job-id", required=True, type=int)
    parser.add_argument("--head-sha", required=True)
    args = parser.parse_args()
    report = audit(args.log, args.run_id, args.job_id, args.head_sha)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({key: value for key, value in report.items() if key != "targets"}, indent=2))
    if any(report["validation"].values()):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
