#!/usr/bin/env python3
"""Retain one completed compiler job and independently verify its small artifact."""
import argparse
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import zipfile


def api(path):
    return subprocess.check_output(["gh", "api", "repos/cyjin-yl/shenko/" + path])


def retain(path, data):
    if path.exists():
        if path.read_bytes() != data:
            raise ValueError(f"Different original bytes already retained: {path.name}")
    else:
        path.write_bytes(data)
    return {"path": str(path), "bytes": len(data),
            "sha256": hashlib.sha256(data).hexdigest()}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--run", required=True, type=int)
    parser.add_argument("--job", required=True, type=int)
    parser.add_argument("--artifact", required=True, type=int)
    parser.add_argument("--sha", required=True)
    parser.add_argument("--directory", required=True, type=Path)
    args = parser.parse_args()
    args.directory.mkdir(parents=True, exist_ok=True)
    job_raw = api(f"actions/jobs/{args.job}")
    job = json.loads(job_raw)
    assert job["id"] == args.job and job["run_id"] == args.run
    assert job["head_sha"] == args.sha and job["status"] == "completed"
    retain(args.directory / "compiler-job.json", job_raw)
    log = api(f"actions/jobs/{args.job}/logs")
    log_receipt = retain(args.directory / "compiler.log", log)
    log_receipt["lines"] = len(log.splitlines())
    artifact_raw = api(f"actions/artifacts/{args.artifact}")
    artifact = json.loads(artifact_raw)
    assert artifact["id"] == args.artifact
    assert artifact["workflow_run"]["id"] == args.run
    assert artifact["workflow_run"]["head_sha"] == args.sha
    assert artifact["name"] == "nightly-compiler-evidence" and not artifact["expired"]
    assert 0 < artifact["size_in_bytes"] <= 32 * 1024 * 1024
    retain(args.directory / "compiler-artifact.json", artifact_raw)
    archive_bytes = api(f"actions/artifacts/{args.artifact}/zip")
    archive_sha = hashlib.sha256(archive_bytes).hexdigest()
    assert len(archive_bytes) == artifact["size_in_bytes"]
    assert artifact["digest"] == "sha256:" + archive_sha
    retain(args.directory / "compiler-artifact.zip", archive_bytes)
    selected = {}
    with zipfile.ZipFile(io.BytesIO(archive_bytes)) as archive:
        members = archive.infolist()
        index = [{"name": item.filename, "bytes": item.file_size,
                  "compressed_bytes": item.compress_size, "crc32": f"{item.CRC:08x}"}
                 for item in members]
        retain(args.directory / "compiler-artifact-index.json",
               (json.dumps(index, indent=2) + "\n").encode())
        for name, stored in [
            ("evidence/compiler-browser.json", "compiler-browser.json"),
            ("evidence/compiler-vitest.log", "compiler-vitest.log"),
            ("web/inputs.json", "inputs.json"),
        ]:
            matches = [item for item in members if item.filename == name]
            if len(matches) != 1:
                raise ValueError(f"Expected one original artifact member {name}")
            item = matches[0]
            assert not item.is_dir() and item.file_size <= 16 * 1024 * 1024
            raw = archive.read(item)
            assert len(raw) == item.file_size
            receipt = {"archive_path": name, **retain(args.directory / stored, raw),
                       "crc32": f"{item.CRC:08x}"}
            if stored == "compiler-browser.json":
                receipt["receipt"] = json.loads(raw)
                assert receipt["receipt"]["candidate"] == args.sha
            elif stored == "inputs.json":
                inputs = json.loads(raw)
                receipt["receipt"] = {key: inputs.get(key) for key in ("pin", "package_manager")}
            else:
                lines = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", raw.decode()).splitlines()
                receipt["summary"] = [line for line in lines
                                      if re.match(r"\s*(Test Files|Tests|Duration)\s+", line)]
                receipt["scroll_fixture"] = [line for line in lines
                                              if "scroll-reveal-consumers.test.ts" in line]
                parsed = {}
                for label, key in (("Test Files", "suites"), ("Tests", "tests")):
                    matches = [line for line in receipt["summary"]
                               if re.match(r"\s*" + label + r"\s+", line)]
                    if matches:
                        line = matches[-1]
                        counts = {kind: int(count) for count, kind in
                                  re.findall(r"(\d+)\s+(passed|failed|skipped|todo)", line)}
                        total = re.search(r"\((\d+)\)\s*$", line)
                        if not total or sum(counts.values()) != int(total[1]):
                            raise ValueError(f"Unaccounted Vitest count: {line}")
                        parsed[key] = {**counts, "total": int(total[1])}
                durations = [line for line in receipt["summary"]
                             if re.match(r"\s*Duration\s+", line)]
                if durations:
                    value = re.search(r"\bDuration\s+([\d.]+)s\b", durations[-1])
                    if value:
                        parsed["duration_seconds"] = float(value[1])
                receipt["parsed"] = parsed
            selected[name] = receipt
    summary = {
        "run_id": args.run, "job_id": args.job, "candidate": args.sha,
        "job_conclusion": job["conclusion"], "started_at": job["started_at"],
        "completed_at": job["completed_at"], "artifact_id": args.artifact,
        "artifact_bytes": len(archive_bytes), "artifact_sha256": archive_sha,
        "artifact_zip_downloaded_and_hash_verified": True,
        "selected": selected, "raw_job_log": log_receipt,
    }
    summary_file = retain(args.directory / "compiler-summary.json",
                          (json.dumps(summary, indent=2) + "\n").encode())
    print(json.dumps({"summary": summary_file, "conclusion": job["conclusion"],
                      "artifact_bytes": len(archive_bytes), "artifact_sha256": archive_sha,
                      "vitest": selected["evidence/compiler-vitest.log"]["parsed"]}, indent=2))


if __name__ == "__main__":
    main()
