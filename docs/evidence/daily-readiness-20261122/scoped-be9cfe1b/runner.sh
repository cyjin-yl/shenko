#!/usr/bin/env bash
set -euo pipefail
candidate=be9cfe1ba9ac99f34c69fe57eea4aa369a8371e3
root=/tank/shared/projects/shenko/build/daily-readiness-20261122
evidence="$root/scoped-be9cfe1b"
test ! -e "$evidence"
mkdir -p "$evidence/start-boundary" "$evidence/native-artifacts"
exec 9>/tmp/shenko-cargo.lock
flock 9
cd /tank/shared/projects/shenko/worktrees/daily-readiness-20261122
export CARGO_HOME=/var/idw-target/poppy-cargo-home-20261116 CARGO_TARGET_DIR=/var/idw-target/daily-readiness-target-20261122
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=4 CARGO_NET_OFFLINE=true CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
export SHENKO_SOURCE_REVISION="$candidate"
export SHENKO_START_BOUNDARY_EVIDENCE="$evidence/start-boundary" SHENKO_NATIVE_ARTIFACT_ROOT="$evidence/native-artifacts"
export PATH="/var/idw-target/daily-readiness-node-v24.22.1/bin:$PATH"
test "$(git rev-parse HEAD)" = "$candidate"
test -z "$(git status --porcelain)"
cp /tank/shared/projects/shenko/build/daily-readiness-20261122-verify-known-node-loss.sh "$evidence/runner.sh"
date -u +%FT%TZ > "$evidence/started.txt"
rustc --version > "$evidence/rustc.txt"
node --version > "$evidence/node.txt"
run_test() {
 local label="$1"
 shift
 set +e
 cargo test --offline --locked -j4 "$@" -- --exact --nocapture > "$evidence/$label.log" 2>&1
 local result=$?
 set -e
 printf '%s\n' "$result" > "$evidence/$label.exit"
 if [ "$result" -ne 0 ]; then return "$result"; fi
 python3 - "$evidence/$label.log" <<'PY'
import pathlib,re,sys
p=pathlib.Path(sys.argv[1]); results=re.findall(r'^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed;',p.read_text(),re.M)
assert results and tuple(map(int,results[-1]))==(1,0),results
print(p.name+': one outer actual test passed',flush=True)
PY
}
status=0
run_test authority -p shenko --lib --all-features runtime_authority::tests::reserved_store_commit_and_ancestor_cancellation_fence_live_owners || status=1
run_test start-boundary -p shenko --test start_boundary --all-features public_start_pause_survives_process_reopen_and_resume_alone_admits_inference || status=1
printf '%s\n' "$status" > "$evidence/exit.txt"
date -u +%FT%TZ > "$evidence/finished.txt"
exit "$status"
