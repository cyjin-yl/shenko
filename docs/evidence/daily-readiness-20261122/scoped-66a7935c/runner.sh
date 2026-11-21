#!/usr/bin/env bash
set -euo pipefail
evidence=/tank/shared/projects/shenko/build/daily-readiness-20261122/scoped-66a7935c
mkdir -p "$evidence"
exec 9>/tmp/shenko-cargo.lock
flock 9
cd /tank/shared/projects/shenko/worktrees/daily-readiness-20261122
export CARGO_HOME=/var/idw-target/poppy-cargo-home-20261116 CARGO_TARGET_DIR=/var/idw-target/daily-readiness-target-20261122
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=4 CARGO_NET_OFFLINE=true CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
export SHENKO_SOURCE_REVISION=66a7935cafa517b3a932f827f0b784569e8cf0e2
test "$(git rev-parse HEAD)" = "$SHENKO_SOURCE_REVISION"
test -z "$(git status --porcelain)"
date -u +%FT%TZ > "$evidence/started.txt"
run_test() {
  local label="$1" count="$2"
  shift 2
  set +e
  cargo test --offline --locked -j4 "$@" -- --nocapture > "$evidence/$label.log" 2>&1
  local result=$?
  set -e
  printf '%s\n' "$result" > "$evidence/$label.exit"
  if [ "$result" -ne 0 ]; then return "$result"; fi
  python3 - "$evidence/$label.log" "$count" <<'PY'
import pathlib,sys
text=pathlib.Path(sys.argv[1]).read_text()
count=int(sys.argv[2])
assert f'test result: ok. {count} passed; 0 failed;' in text, 'selected tests must actually run'
print(pathlib.Path(sys.argv[1]).name+f': {count} actual tests passed',flush=True)
PY
}
status=0
run_test first-claim 1 -p shenko-store --test store_roundtrip --all-features canonical_first_claim_preserves_s1_birth_and_never_reissues_a_recovered_worker || status=1
run_test native-chain 1 -p shenko-store --test store_roundtrip --all-features canonical_direct_native_chain_retains_original_e1_and_only_one_effect || status=1
run_test archive-admission 8 -p shenko-store --lib --all-features cognition::archive_admission_tests || status=1
run_test obligation-archive 1 -p shenko-subject --lib --all-features obligation_current_archive_reopens_replays_and_relays_exact_settlements || status=1
run_test canonical-recovery 1 -p shenko-store --test store_roundtrip --all-features canonical_s1_recovers_identical_results_on_a_distinct_ledger_without_dispatch || status=1
printf '%s\n' "$status" > "$evidence/exit.txt"
date -u +%FT%TZ > "$evidence/finished.txt"
exit "$status"
