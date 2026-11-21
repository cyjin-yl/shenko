#!/usr/bin/env bash
set -euo pipefail
candidate=0d25d77fde4b96ecbc3a8af88baeee9fadaf362f
root=/tank/shared/projects/shenko/build/daily-readiness-20261122
evidence="$root/scoped-0d25d77f"
mkdir -p "$evidence/start-boundary" "$evidence/component-order"
exec 9>/tmp/shenko-cargo.lock
flock 9
cd /tank/shared/projects/shenko/worktrees/daily-readiness-20261122
export CARGO_HOME=/var/idw-target/poppy-cargo-home-20261116 CARGO_TARGET_DIR=/var/idw-target/daily-readiness-target-20261122
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=4 CARGO_NET_OFFLINE=true CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
export SHENKO_SOURCE_REVISION="$candidate"
export SHENKO_START_BOUNDARY_EVIDENCE="$evidence/start-boundary" SHENKO_SSH_WASI_EVIDENCE="$evidence/component-order"
export PATH="/var/idw-target/daily-readiness-node-v24.22.1/bin:$PATH"
test "$(git rev-parse HEAD)" = "$candidate"
test -z "$(git status --porcelain)"
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
run_test start-boundary -p shenko --test start_boundary --all-features public_start_pause_survives_process_reopen_and_resume_alone_admits_inference || status=1
run_test runtime-owner -p shenko --lib --all-features runtime_owner::tests::same_directory_inode_excludes_another_owner_without_creating_a_file || status=1
run_test component-order -p shenko --lib --all-features node_sync::component_runtime::start_order_tests::model_free_start_binds_node_before_owned_services_and_reopen || status=1
run_test authority -p shenko --lib --all-features runtime_authority::tests::reserved_store_commit_and_ancestor_cancellation_fence_live_owners || status=1
printf '%s\n' "$status" > "$evidence/exit.txt"
date -u +%FT%TZ > "$evidence/finished.txt"
if [ "$status" -ne 0 ]; then exit "$status"; fi
build="$root/release-0d25d77f"
mkdir -p "$build"
test ! -e "$root/shenko-release-0d25d77f"
date -u +%FT%TZ > "$build/started.txt"
set +e
cargo build --offline --locked --release -p shenko -j4 > "$build/build.log" 2>&1
status=$?
set -e
printf '%s\n' "$status" > "$build/exit.txt"
date -u +%FT%TZ > "$build/finished.txt"
if [ "$status" -ne 0 ]; then exit "$status"; fi
install -m 755 "$CARGO_TARGET_DIR/release/shenko" "$root/shenko-release-0d25d77f"
python3 - <<'PY'
from pathlib import Path
import hashlib,json,os
root=Path('/tank/shared/projects/shenko/build/daily-readiness-20261122')
p=root/'shenko-release-0d25d77f';b=p.read_bytes();d=root/'release-0d25d77f'
v={'source_revision':os.environ['SHENKO_SOURCE_REVISION'],'binary':str(p),'bytes':len(b),'sha256':hashlib.sha256(b).hexdigest(),'started_at_utc':(d/'started.txt').read_text().strip(),'finished_at_utc':(d/'finished.txt').read_text().strip(),'profile':'release','command':['cargo','build','--offline','--locked','--release','-p','shenko','-j4']}
(d/'manifest.json').write_text(json.dumps(v,indent=2)+'\n')
print(json.dumps(v),flush=True)
PY
