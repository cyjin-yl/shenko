#!/usr/bin/env python3
"""Retain completed GitHub job bytes and audit outer Cargo harnesses."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import runpy
import subprocess


def api(path):
    return subprocess.check_output(["gh", "api", "repos/cyjin-yl/shenko/" + path])


def retain(path, data):
    if path.exists() and path.read_bytes() != data:
        raise ValueError(f"Different retained bytes already exist: {path}")
    if not path.exists():
        path.write_bytes(data)
    return {"path": str(path), "bytes": len(data),
            "sha256": hashlib.sha256(data).hexdigest()}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--run", required=True, type=int)
    parser.add_argument("--job", required=True, type=int)
    parser.add_argument("--sha", required=True)
    parser.add_argument("--directory", required=True, type=Path)
    parser.add_argument("--audit-script", required=True, type=Path)
    args = parser.parse_args()
    run_raw = api(f"actions/runs/{args.run}")
    job_raw = api(f"actions/jobs/{args.job}")
    run, job = json.loads(run_raw), json.loads(job_raw)
    assert run["head_sha"] == args.sha, run["head_sha"]
    assert job["run_id"] == args.run
    assert job["status"] == "completed", job["status"]
    args.directory.mkdir(parents=True, exist_ok=True)
    raw = api(f"actions/jobs/{args.job}/logs")
    log_path = args.directory / "rust.log"
    raw_log = retain(log_path, raw)
    retain(args.directory / "run.json", run_raw)
    retain(args.directory / "job.json", job_raw)
    artifacts_raw = api(f"actions/runs/{args.run}/artifacts")
    retain(args.directory / "artifacts.json", artifacts_raw)
    audit = runpy.run_path(str(args.audit_script))["audit"](
        log_path, args.run, args.job, args.sha)
    audit_bytes = (json.dumps(audit, indent=2) + "\n").encode()
    audit_file = retain(args.directory / "rust-outer-audit.json", audit_bytes)
    lines = raw.decode().splitlines()
    failed = [line for line in lines
              if re.search(r"test \S+ \.\.\. FAILED(?:\s|$)", line)]
    summary = {key: value for key, value in audit.items() if key != "targets"}
    summary.update({"run_url": run["html_url"], "job_url": job["html_url"],
        "event": run["event"], "run_attempt": run["run_attempt"],
        "head_branch": run["head_branch"], "job_conclusion": job["conclusion"],
        "started_at": job["started_at"], "completed_at": job["completed_at"],
        "audit_file": audit_file, "failed_test_lines": failed,
        "artifacts": json.loads(artifacts_raw),
        "artifact_verification_scope": "GitHub API identity and digest metadata; artifact ZIPs are not downloaded by this Rust collector."})
    summary_file = retain(args.directory / "rust-summary.json",
                          (json.dumps(summary, indent=2) + "\n").encode())
    print(json.dumps({"summary": summary_file, "audit": audit_file,
        "raw_log": raw_log, "outer": audit["outer_summary"],
        "nested": audit["nested_summary"], "validation": audit["validation"],
        "failed_test_lines": failed, "job_conclusion": job["conclusion"],
        "explicit_long_proofs": audit["explicit_long_proofs"]}, indent=2))


if __name__ == "__main__":
    main()
