#!/usr/bin/env python3
"""Bounded read-only observer for two already-created candidate workflow runs."""
from pathlib import Path
import concurrent.futures
import datetime
import hashlib
import json
import subprocess
import time

SHA = "d16adbf89c9f79dfce8ef90d77843412bcda85bf"
RUNS = {"ci": (44963143476, "pull_request"), "nightly": (44963198484, "workflow_dispatch")}
ROOT = Path("/tank/shared/projects/shenko/build/daily-readiness-20261122/ci-nightly-d16adbf8-progress")
INTERVAL = 60
MAX_DURATION = 7200


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def save(name, data):
    with (ROOT / name).open("xb") as output:
        output.write(data)


def fetch(item):
    name, endpoint = item
    result = subprocess.run(["gh", "api", "repos/cyjin-yl/shenko/" + endpoint],
                            capture_output=True, timeout=45)
    if result.returncode:
        raise RuntimeError(f"read-only API query {name} exited {result.returncode}: "
                           + result.stderr.decode(errors="replace")[:500])
    return name, result.stdout, json.loads(result.stdout)


def main():
    ROOT.mkdir(exist_ok=True)
    start = time.monotonic()
    save("observer-start.json", (json.dumps({
        "source_revision": SHA, "started_at_utc": now(), "interval_seconds": INTERVAL,
        "max_duration_seconds": MAX_DURATION, "runs": RUNS,
        "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "github_mutations": False,
    }, indent=2) + "\n").encode())
    pending, last = dict(RUNS), {}
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
        while pending and time.monotonic() - start < MAX_DURATION:
            stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
            items = [(kind + suffix, f"actions/runs/{run_id}" + endpoint)
                     for kind, (run_id, _) in pending.items()
                     for suffix, endpoint in [("-run", ""), ("-jobs", "/jobs?per_page=100")]]
            responses = {}
            for name, raw, obj in executor.map(fetch, items):
                save(stamp + "-" + name + ".json", raw)
                responses[name] = obj
            for kind, (run_id, event) in list(pending.items()):
                run, jobs = responses[kind + "-run"], responses[kind + "-jobs"]
                assert run["id"] == run_id and run["head_sha"] == SHA
                assert run["event"] == event and run["run_attempt"] == 1
                assert jobs["total_count"] == len(jobs["jobs"])
                assert all(j["run_id"] == run_id and j["head_sha"] == SHA for j in jobs["jobs"])
                state = {"kind": kind, "run_id": run_id, "status": run["status"],
                         "conclusion": run["conclusion"], "jobs": [
                             {"id": j["id"], "name": j["name"], "status": j["status"],
                              "conclusion": j["conclusion"], "active_steps": [
                                  s["name"] for s in j["steps"] if s["status"] == "in_progress"]}
                             for j in jobs["jobs"]]}
                if state != last.get(kind):
                    print(json.dumps({"observed_at_utc": now(), **state}), flush=True)
                    last[kind] = state
                if run["status"] == "completed":
                    assert all(j["status"] == "completed" for j in jobs["jobs"])
                    del pending[kind]
            if pending:
                time.sleep(min(INTERVAL, max(0, MAX_DURATION - (time.monotonic() - start))))
    outcome = {"finished_at_utc": now(), "source_revision": SHA,
               "all_runs_completed": not pending, "remaining_runs": pending,
               "elapsed_seconds": time.monotonic() - start, "github_mutations": False}
    save("observer-finished.json", (json.dumps(outcome, indent=2) + "\n").encode())
    print(json.dumps(outcome), flush=True)
    return 0 if not pending else 2


if __name__ == "__main__":
    raise SystemExit(main())
