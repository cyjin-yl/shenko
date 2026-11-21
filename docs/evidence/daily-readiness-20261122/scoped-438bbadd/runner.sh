#!/usr/bin/env bash
set -euo pipefail
evidence=/tank/shared/projects/shenko/build/daily-readiness-20261122/scoped-438bbadd
mkdir -p "$evidence"
exec 9>/tmp/shenko-cargo.lock
flock 9
cd /tank/shared/projects/shenko/worktrees/daily-readiness-20261122
export CARGO_HOME=/var/idw-target/poppy-cargo-home-20261116 CARGO_TARGET_DIR=/var/idw-target/daily-readiness-target-20261122
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=4 CARGO_NET_OFFLINE=true CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
export SHENKO_SOURCE_REVISION=438bbadd6e505fa91657715230572e42b72a27e0
test "$(git rev-parse HEAD)" = "$SHENKO_SOURCE_REVISION"
test -z "$(git status --porcelain)"
date -u +%FT%TZ > "$evidence/started.txt"
set +e
cargo test --offline --locked -j4 -p shenko-store --test store_roundtrip --all-features canonical_first_claim_preserves_s1_birth_and_never_reissues_a_recovered_worker -- --exact --nocapture > "$evidence/first-claim.log" 2>&1
status=$?
set -e
printf '%s\n' "$status" > "$evidence/first-claim.exit"
if [ "$status" -eq 0 ]; then
  python3 - "$evidence/first-claim.log" <<'PY'
import pathlib,sys
text=pathlib.Path(sys.argv[1]).read_text()
assert 'test result: ok. 1 passed; 0 failed;' in text
print('first-claim.log: one actual test passed',flush=True)
PY
fi
printf '%s\n' "$status" > "$evidence/exit.txt"
date -u +%FT%TZ > "$evidence/finished.txt"
exit "$status"
