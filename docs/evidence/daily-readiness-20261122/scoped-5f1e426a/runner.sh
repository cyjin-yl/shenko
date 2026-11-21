#!/usr/bin/env bash
set -euo pipefail
candidate="$1"
[[ "$candidate" =~ ^[0-9a-f]{40}$ ]]
tag="$(printf '%s' "$candidate" | cut -c1-8)"
root=/tank/shared/projects/shenko/build/daily-readiness-20261122
evidence="$root/scoped-$tag"
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
cp /tank/shared/projects/shenko/build/daily-readiness-20261122-verify-native-selector.sh "$evidence/runner.sh"
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
import pathlib,sys
text=pathlib.Path(sys.argv[1]).read_text()
assert 'test result: ok. 1 passed; 0 failed;' in text, 'exact test must actually run once'
print(pathlib.Path(sys.argv[1]).name+': one actual test passed',flush=True)
PY
}
status=0
run_test native-chain -p shenko-store --test store_roundtrip --all-features canonical_direct_native_chain_retains_original_e1_and_only_one_effect || status=1
run_test first-claim -p shenko-store --test store_roundtrip --all-features canonical_first_claim_preserves_s1_birth_and_never_reissues_a_recovered_worker || status=1
run_test native-dispatch -p shenko-store --test store_roundtrip --all-features canonical_native_dispatch_retains_one_live_permit_through_prepare_learn_and_restart || status=1
run_test legacy-arm -p shenko-store --test store_roundtrip --all-features canonical_native_arming_keeps_original_intent_without_reissuing_execution || status=1
run_test generic-t2 -p shenko-store --test store_roundtrip --all-features canonical_native_t2_keeps_original_e1_through_settlement_restart_and_foreign_debt || status=1
run_test generic-t2-children -p shenko-store --test store_roundtrip --all-features canonical_native_t2_completing_and_finish_recover_as_separate_atomic_results || status=1
run_test cli-printf -p shenko --lib --all-features recovery_integration_tests::native_printf_result_survives_real_process_reopen || status=1
run_test broker-queue-stop -p shenko-broker --lib --all-features broker::tests::fair_rotation_real_concurrency_fifo_and_zero_capacity_queueing || status=1
run_test start-boundary -p shenko --test start_boundary --all-features public_start_pause_survives_process_reopen_and_resume_alone_admits_inference || status=1
set +e
cargo test --offline --locked --all-features -p shenko-store --lib cognition::archive_admission_tests -j4 -- --nocapture > "$evidence/archive-admission.log" 2>&1
archive_status=$?
set -e
printf '%s\n' "$archive_status" > "$evidence/archive-admission.exit"
if [ "$archive_status" -ne 0 ]; then status=1; else
 python3 - "$evidence/archive-admission.log" <<'PY'
import pathlib,sys
text=pathlib.Path(sys.argv[1]).read_text()
assert 'test result: ok. 8 passed; 0 failed;' in text, 'all eight actual archive admission tests must run'
print('archive-admission.log: eight actual tests passed',flush=True)
PY
fi
printf '%s\n' "$status" > "$evidence/exit.txt"
date -u +%FT%TZ > "$evidence/finished.txt"
if [ "$status" -ne 0 ]; then exit "$status"; fi
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
import hashlib,json,os
root=Path('/tank/shared/projects/shenko/build/daily-readiness-20261122')
source=os.environ['SHENKO_SOURCE_REVISION'];tag=source[:8]
p=root/('shenko-release-'+tag);b=p.read_bytes();d=root/('release-'+tag)
v={'source_revision':source,'binary':str(p),'bytes':len(b),'sha256':hashlib.sha256(b).hexdigest(),'started_at_utc':(d/'started.txt').read_text().strip(),'finished_at_utc':(d/'finished.txt').read_text().strip(),'profile':'release','command':['cargo','build','--offline','--locked','--release','-p','shenko','-j4']}
(d/'manifest.json').write_text(json.dumps(v,indent=2)+'\n')
print(json.dumps(v),flush=True)
PY
