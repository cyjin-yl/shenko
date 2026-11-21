#!/usr/bin/env bash
set -euo pipefail
candidate=8f14a704c59ce723246286d72ea7cb724b2b3c0e
baseline=be9cfe1ba9ac99f34c69fe57eea4aa369a8371e3
root=/tank/shared/projects/shenko/build/daily-readiness-20261122
evidence="$root/scoped-8f14a704"
test ! -e "$evidence"
mkdir -p "$evidence/native-artifacts"
finish() {
 local result=$?
 trap - EXIT
 printf '%s\n' "$result" > "$evidence/exit.txt"
 date -u +%FT%TZ > "$evidence/finished.txt"
 exit "$result"
}
trap finish EXIT
exec 9>/tmp/shenko-cargo.lock
flock 9
cd /tank/shared/projects/shenko/worktrees/daily-readiness-20261122
export CARGO_HOME=/var/idw-target/poppy-cargo-home-20261116 CARGO_TARGET_DIR=/var/idw-target/daily-readiness-target-20261122
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=4 CARGO_NET_OFFLINE=true CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
export SHENKO_SOURCE_REVISION="$candidate" SHENKO_NATIVE_ARTIFACT_ROOT="$evidence/native-artifacts"
export PATH="/var/idw-target/daily-readiness-node-v24.22.1/bin:$PATH"
test "$(git rev-parse HEAD)" = "$candidate"
test -z "$(git status --porcelain)"
cp /tank/shared/projects/shenko/build/daily-readiness-20261122-verify-known-node-loss-8f14a704.sh "$evidence/runner.sh"
cp /tank/shared/projects/shenko/build/daily-readiness-20261122-host-verify-known-node-loss-8f14a704.sh "$evidence/host-runner.sh"
date -u +%FT%TZ > "$evidence/started.txt"
rustc --version > "$evidence/rustc.txt"
node --version > "$evidence/node.txt"
python3 - "$root" "$evidence" "$baseline" "$candidate" <<'PY'
from pathlib import Path
import hashlib,json,re,subprocess,sys
root,evidence,baseline,candidate=Path(sys.argv[1]),Path(sys.argv[2]),sys.argv[3],sys.argv[4]
def git(*args): return subprocess.check_output(['git',*args])
path='bin/shenko/src/runtime_authority_tests.rs'
assert git('diff','--name-only',baseline,candidate).decode().splitlines()==[path]
old,new=git('show',baseline+':'+path),git('show',candidate+':'+path)
before=b'let (release_binding, proceed) = mpsc::sync_channel(1);'
after=b'let (release_binding, proceed) = mpsc::sync_channel::<()>(1);'
assert old.count(before)==1 and old.replace(before,after)==new
process_path='bin/shenko/tests/start_boundary.rs'
assert git('show',baseline+':'+process_path)==git('show',candidate+':'+process_path)
prior=root/'scoped-be9cfe1b'
process=(prior/'start-boundary.log').read_bytes()
results=re.findall(rb'^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed;[^\n]*',process,re.M)
assert results and tuple(map(int,results[-1]))==(1,0),results
assert (prior/'start-boundary.exit').read_text().strip()=='0'
original=(prior/'authority.log').read_bytes()
assert (prior/'authority.exit').read_text().strip()=='101'
assert b'error[E0282]: type annotations needed' in original
assert b'runtime_authority_tests.rs:289:42' in original
assert not re.findall(rb'^test result:',original,re.M)
record={'schema_version':1,'candidate':candidate,'baseline':baseline,
 'only_source_change':'Explicit unit type on the existing drop-only release channel in the authority test.',
 'all_other_source_bytes_identical':True,'start_boundary_source_sha256':hashlib.sha256(git('show',candidate+':'+process_path)).hexdigest(),
 'reused_process_test':{'path':str(prior/'start-boundary.log'),'bytes':len(process),'sha256':hashlib.sha256(process).hexdigest(),'cargo_exit':0,'actual_passed':1,'actual_failed':0,'excluded_earlier_summaries':[list(map(int,pair)) for pair in results[:-1]]},
 'retained_original_compile_failure':{'path':str(prior/'authority.log'),'bytes':len(original),'sha256':hashlib.sha256(original).hexdigest(),'cargo_exit':101,'diagnostic':'E0282 at runtime_authority_tests.rs:289:42','actual_authority_test_executions':0},
 'rerun':'Only runtime_authority::tests::reserved_store_commit_and_ancestor_cancellation_fence_live_owners',
 'release_build_requested':False,'real_model_requested':False}
(evidence/'reuse.json').write_text(json.dumps(record,indent=2)+'\n')
print('Verified one-line fixture-only change and unchanged successful process test; original compile failure retained',flush=True)
PY
date -u +%FT%TZ > "$evidence/authority.started.txt"
set +e
cargo test --offline --locked -j4 -p shenko --lib --all-features runtime_authority::tests::reserved_store_commit_and_ancestor_cancellation_fence_live_owners -- --exact --nocapture > "$evidence/authority.log" 2>&1
result=$?
set -e
printf '%s\n' "$result" > "$evidence/authority.exit"
date -u +%FT%TZ > "$evidence/authority.finished.txt"
if [ "$result" -ne 0 ]; then exit "$result"; fi
python3 - "$evidence/authority.log" <<'PY'
import pathlib,re,sys
p=pathlib.Path(sys.argv[1]);results=re.findall(r'^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed;',p.read_text(),re.M)
assert results and tuple(map(int,results[-1]))==(1,0),results
print(p.name+': one outer actual test passed',flush=True)
PY
