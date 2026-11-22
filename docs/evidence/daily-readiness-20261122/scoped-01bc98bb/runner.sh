#!/usr/bin/env bash
set -euo pipefail
candidate="$1"
[[ "$candidate" =~ ^[0-9a-f]{40}$ ]]
tag="${candidate:0:8}"
root=/tank/shared/projects/shenko/build/daily-readiness-20261122
evidence="$root/scoped-$tag"
test ! -e "$evidence"
mkdir -p "$evidence/native-artifacts"
exec 9>/tmp/shenko-cargo.lock
flock 9
cd /tank/shared/projects/shenko/worktrees/daily-readiness-20261122
export CARGO_HOME=/var/idw-target/poppy-cargo-home-20261116
export CARGO_TARGET_DIR=/var/idw-target/daily-readiness-target-20261122
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=4 CARGO_NET_OFFLINE=true
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
export SHENKO_SOURCE_REVISION="$candidate"
export SHENKO_NATIVE_ARTIFACT_ROOT="$evidence/native-artifacts"
export SHENKO_NATIVE_E1_EVIDENCE="$evidence/native-e1"
export PATH="/var/idw-target/daily-readiness-node-v24.22.1/bin:$PATH"
test "$(git rev-parse HEAD)" = "$candidate"
test -z "$(git status --porcelain)"
test -d /run/sshd
cp /tank/shared/projects/shenko/build/daily-readiness-20261122-verify-anchored-native.sh "$evidence/runner.sh"
date -u +%FT%TZ > "$evidence/started.txt"
rustc --version > "$evidence/rustc.txt"
cargo --version > "$evidence/cargo.txt"
node --version > "$evidence/node.txt"
python3 - "$candidate" "$evidence" <<'PY'
from pathlib import Path
import datetime, hashlib, json, re, subprocess, sys, time

candidate, target = sys.argv[1:]
root = Path(target)
checks = [
    ("native-chain", 1, ["-p", "shenko-store", "--test", "store_roundtrip", "canonical_direct_native_chain_retains_original_e1_and_only_one_effect"], True),
    ("preparation-progress", 1, ["-p", "shenko-store", "--test", "store_roundtrip", "competing_s1_rejects_stale_writer_without_partial_commit"], True),
    ("e1-ssh", 1, ["-p", "shenko", "--test", "native_evidence_delivery", "original_native_e1_arrives_over_authenticated_ssh_without_t2_or_reexecution"], True),
    ("e1-source", 1, ["-p", "shenko", "--test", "native_evidence_delivery", "source_bound_original_e1_requires_local_source_custody_and_current_heads"], True),
    ("child-e1-reopen", 1, ["-p", "shenko-subject", "--lib", "loop_runner::recovery_tests::model_child_tests::failed_generation_reopens_after_e1_without_a_second_send"], True),
    ("archive-admission", 8, ["-p", "shenko-store", "--lib", "cognition::archive_admission_tests"], False),
]
records = []
for label, expected, selected, exact in checks:
    command = ["cargo", "test", "--offline", "--locked", "--all-features", "-j4", *selected, "--"]
    if exact:
        command.append("--exact")
    command.append("--nocapture")
    started = datetime.datetime.now(datetime.timezone.utc).isoformat()
    clock = time.monotonic()
    log = root / (label + ".log")
    with log.open("wb") as output:
        result = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT)
    (root / (label + ".exit")).write_text(str(result.returncode) + "\n")
    data = log.read_bytes()
    summaries = re.findall(r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out; finished in (.+)$", data.decode(errors="replace"), re.M)
    actual = summaries[-1] if summaries else None
    passed = result.returncode == 0 and actual is not None and tuple(map(int, actual[1:5])) == (expected, 0, 0, 0)
    record = {"label": label, "command": command, "expected_tests": expected, "exit_code": result.returncode,
              "passed": passed, "started_at_utc": started, "finished_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "elapsed_seconds": time.monotonic() - clock, "outer_summary": actual, "excluded_nested_summaries": summaries[:-1],
              "original_log": str(log), "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    records.append(record)
    (root / "summary.json").write_text(json.dumps({"source_revision": candidate, "checks": records,
        "completed_targets": len(records), "planned_targets": len(checks), "all_passed": len(records) == len(checks) and all(r["passed"] for r in records)}, indent=2) + "\n")
    print(json.dumps({"label": label, "exit": result.returncode, "outer_summary": actual, "verified_pass": passed}), flush=True)
status = int(not all(r["passed"] for r in records))
(root / "exit.txt").write_text(str(status) + "\n")
(root / "finished.txt").write_text(datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ\n"))
raise SystemExit(status)
PY
build="$root/release-$tag"
test ! -e "$build"
test ! -e "$root/shenko-release-$tag"
mkdir "$build"
date -u +%FT%TZ > "$build/started.txt"
set +e
cargo build --offline --locked --release -p shenko -j4 > "$build/build.log" 2>&1
status=$?
set -e
printf '%s\n' "$status" > "$build/exit.txt"
date -u +%FT%TZ > "$build/finished.txt"
if [ "$status" -ne 0 ]; then exit "$status"; fi
install -m 755 "$CARGO_TARGET_DIR/release/shenko" "$root/shenko-release-$tag"
python3 - <<'PY'
from pathlib import Path
import hashlib, json, os
root = Path('/tank/shared/projects/shenko/build/daily-readiness-20261122')
source = os.environ['SHENKO_SOURCE_REVISION']
tag = source[:8]
binary = root / ('shenko-release-' + tag)
data = binary.read_bytes()
directory = root / ('release-' + tag)
manifest = {'source_revision': source, 'binary': str(binary), 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest(),
            'started_at_utc': (directory / 'started.txt').read_text().strip(), 'finished_at_utc': (directory / 'finished.txt').read_text().strip(),
            'profile': 'release', 'command': ['cargo', 'build', '--offline', '--locked', '--release', '-p', 'shenko', '-j4']}
(directory / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
print(json.dumps(manifest), flush=True)
PY
