"""Read-only counts from retained streams; not a replacement ratchet run."""
import collections
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent


def counts(kind):
    counts = collections.Counter()
    locations = {}
    finished = False
    source = ROOT / "clippy-artifact" / "clippy" / (kind + ".jsonl")
    for number, line in enumerate(source.read_text().splitlines(), 1):
        record = json.loads(line)
        if record.get("reason") == "build-finished":
            finished = record.get("success")
        if record.get("reason") != "compiler-message" or record["message"]["level"] != "warning":
            continue
        package = record["package_id"]
        if not package.startswith("path+file:///__w/shenko/shenko/"):
            continue
        path, identity = package.rsplit("#", 1)
        name = identity.split("@")[0] if "@" in identity else path.rsplit("/", 1)[1]
        message = record["message"]
        code = (message.get("code") or {}).get("code") or message["message"]
        if kind.endswith("all-targets") and code in ("clippy::unwrap_used", "clippy::expect_used"):
            continue
        key = (name, code)
        counts[key] += 1
        locations.setdefault(key, []).append({
            "jsonl_line": number,
            "message": message["message"],
            "spans": [
                {"file": span["file_name"], "line": span["line_start"]}
                for span in message["spans"] if span.get("is_primary")
            ],
        })
    return counts, locations, finished


result = {"scope": "derived only; no Cargo invocation or source-policy evaluation", "comparisons": []}
for scope in ("production", "all-targets"):
    old, _, old_finished = counts("base-" + scope)
    new, locations, new_finished = counts("candidate-" + scope)
    result["comparisons"].append({
        "scope": scope,
        "baseline_build_finished": old_finished,
        "candidate_build_finished": new_finished,
        "observed_increases": [
            {"package": key[0], "lint": key[1], "increase": count,
             "candidate_locations": locations[key]}
            for key, count in sorted((new - old).items())
        ],
    })
print(json.dumps(result, indent=2, ensure_ascii=False))
