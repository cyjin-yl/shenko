#!/usr/bin/env python3
"""Read already-created CI/Nightly runs; never dispatch, retry or cancel them.

Exit codes: 0 = both runs and their observed jobs succeeded; 1 = both runs
completed with a non-success outcome; 2 = observation deadline; 3 = observer
error, including an identity mismatch. No observer exit changes a workflow.
"""
import argparse
import concurrent.futures
import datetime
import hashlib
import json
from pathlib import Path
import re
import subprocess
import time


def positive(value):
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def save(root, name, data):
    with (root / name).open("xb") as output:
        output.write(data)


def save_json(root, name, value):
    save(root, name, (json.dumps(value, indent=2) + "\n").encode())


def fetch(item):
    name, endpoint, timeout = item
    command = ["gh", "api", "--method", "GET", "--hostname", "github.com",
               "repos/cyjin-yl/shenko/" + endpoint]
    try:
        result = subprocess.run(command, capture_output=True, timeout=timeout)
        return name, result.returncode, result.stdout, result.stderr
    except subprocess.TimeoutExpired as error:
        return name, None, error.stdout or b"", error.stderr or b""


def observe(args):
    root = args.directory
    root.mkdir(parents=True, exist_ok=False)
    start = time.monotonic()
    deadline = start + args.max_duration
    runs = {"ci": (args.ci_run, "pull_request"),
            "nightly": (args.nightly_run, "workflow_dispatch")}
    save_json(root, "observer-start.json", {
        "source_revision": args.sha, "branch": args.branch,
        "started_at_utc": now(), "interval_seconds": args.interval,
        "max_duration_seconds": args.max_duration,
        "api_timeout_seconds": args.api_timeout, "runs": runs,
        "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "github_mutations": False,
    })
    pending, last = dict(runs), {}
    failure = None
    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
            while pending and time.monotonic() < deadline:
                stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
                timeout = min(args.api_timeout, max(0.001, deadline - time.monotonic()))
                items = [(kind + suffix, f"actions/runs/{run_id}" + endpoint, timeout)
                         for kind, (run_id, _) in pending.items()
                         for suffix, endpoint in [("-run", ""), ("-jobs", "/jobs?per_page=100")]]
                responses, errors = {}, []
                futures = [executor.submit(fetch, item) for item in items]
                for future in concurrent.futures.as_completed(futures):
                    name, exit_code, raw, stderr = future.result()
                    save(root, stamp + "-" + name + ".json", raw)
                    if stderr:
                        save(root, stamp + "-" + name + ".stderr", stderr)
                    if exit_code != 0:
                        errors.append({"query": name, "exit_code": exit_code,
                                       "timed_out": exit_code is None})
                        continue
                    try:
                        responses[name] = json.loads(raw)
                    except json.JSONDecodeError as error:
                        errors.append({"query": name, "json_error": str(error)})
                if errors:
                    save_json(root, stamp + "-query-errors.json", errors)
                    raise RuntimeError("Read-only API observation failed; original responses retained")
                for kind, (run_id, event) in list(pending.items()):
                    run, jobs = responses[kind + "-run"], responses[kind + "-jobs"]
                    assert run["id"] == run_id and run["head_sha"] == args.sha, "run identity mismatch"
                    assert run["head_branch"] == args.branch, "branch identity mismatch"
                    assert run["event"] == event and run["run_attempt"] == 1, "event/attempt mismatch"
                    assert jobs["total_count"] == len(jobs["jobs"]), "incomplete job list"
                    assert all(job["run_id"] == run_id and job["head_sha"] == args.sha
                               for job in jobs["jobs"]), "job identity mismatch"
                    state = {"kind": kind, "run_id": run_id, "status": run["status"],
                             "conclusion": run["conclusion"], "jobs": [
                                 {"id": job["id"], "name": job["name"], "status": job["status"],
                                  "conclusion": job["conclusion"], "active_steps": [
                                      step["name"] for step in job["steps"] if step["status"] == "in_progress"]}
                                 for job in jobs["jobs"]]}
                    if state != last.get(kind):
                        print(json.dumps({"observed_at_utc": now(), **state}), flush=True)
                        last[kind] = state
                    # Concurrent GETs may straddle the completion boundary.
                    # Retain that poll and wait for terminal job metadata too.
                    if run["status"] == "completed" and all(job["status"] == "completed" for job in jobs["jobs"]):
                        del pending[kind]
                if pending:
                    time.sleep(min(args.interval, max(0, deadline - time.monotonic())))
    except Exception as error:
        failure = {"type": type(error).__name__, "message": str(error)}
    all_success = not pending and all(
        state["conclusion"] == "success" and state["jobs"]
        and all(job["conclusion"] == "success" for job in state["jobs"])
        for state in last.values()
    )
    exit_code = 3 if failure else (2 if pending else (0 if all_success else 1))
    outcome = {
        "finished_at_utc": now(), "source_revision": args.sha, "branch": args.branch,
        "all_runs_completed": not pending, "all_runs_successful": bool(all_success),
        "remaining_runs": pending, "last_states": last, "observer_error": failure,
        "elapsed_seconds": time.monotonic() - start, "github_mutations": False,
        "observer_exit_code": exit_code,
    }
    save_json(root, "observer-finished.json", outcome)
    print(json.dumps(outcome), flush=True)
    return exit_code


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sha", required=True)
    parser.add_argument("--branch", required=True)
    parser.add_argument("--ci-run", required=True, type=positive)
    parser.add_argument("--nightly-run", required=True, type=positive)
    parser.add_argument("--directory", required=True, type=Path)
    parser.add_argument("--interval", type=positive, default=60)
    parser.add_argument("--max-duration", type=positive, default=7200)
    parser.add_argument("--api-timeout", type=positive, default=45)
    args = parser.parse_args()
    if not re.fullmatch("[0-9a-f]{40}", args.sha):
        parser.error("--sha must be an exact lowercase 40-character commit")
    if args.ci_run == args.nightly_run:
        parser.error("CI and Nightly must have different run IDs")
    return observe(args)


if __name__ == "__main__":
    raise SystemExit(main())
