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
cp /tank/shared/projects/shenko/build/daily-readiness-20261122-verify-native-dispatch.sh "$evidence/runner.sh"
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
python3 - "$candidate" "$evidence" <<'REUSE'
from pathlib import Path
import subprocess,json,sys,re,hashlib
baseline='5f1e426a047390159f038bf58b6d19948ab4ffc9'
candidate=sys.argv[1]; dst=Path(sys.argv[2]); previous=dst.parent/'scoped-5f1e426a'
changed=subprocess.check_output(['git','diff','--name-only',baseline,candidate],text=True).splitlines()
assert changed==['crates/shenko-store/tests/store_roundtrip.rs'],changed
assert (previous/'exit.txt').read_text().strip()=='1'
expected={'native-chain':(1,0),'first-claim':(0,1),'native-dispatch':(0,1),'legacy-arm':(1,0),'generic-t2':(1,0),'generic-t2-children':(1,0),'cli-printf':(1,0),'broker-queue-stop':(1,0),'start-boundary':(1,0),'archive-admission':(8,0)}
records=[]
for label,counts in expected.items():
 log=previous/(label+'.log'); raw=log.read_bytes(); text=raw.decode(errors='replace')
 results=re.findall(r'^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed;',text,re.M)
 assert results and tuple(map(int,results[-1]))==counts,(label,results)
 status=int((previous/(label+'.exit')).read_text())
 assert (status==0)==(counts[1]==0),(label,status)
 records.append({'label':label,'passed':counts[0],'failed':counts[1],'exit':status,'original_log':str(log),'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest(),'reused_success':counts[1]==0,'outer_summary_selection':'last summary from the single cargo target; prior subprocess summaries excluded','excluded_nested_summaries':results[:-1]})
assert sum(r['passed'] for r in records)==15 and sum(r['failed'] for r in records)==2
prior=dst.parent/'scoped-b4d115d8-r2'
assert (prior/'exit.txt').read_text().strip()=='1'
assert (prior/'first-claim.exit').read_text().strip()=='0'
assert (prior/'native-dispatch.exit').read_text().strip()=='101'
raw=(prior/'first-claim.log').read_bytes()
found=re.findall(r'^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed;',raw.decode(errors='replace'),re.M)
assert found and tuple(map(int,found[-1]))==(1,0),found
current_test=Path('crates/shenko-store/tests/store_roundtrip.rs').read_text()
prior_test=subprocess.check_output(['git','show','b4d115d887b11d5734dc0afa4d01be2c9b9eb32b:crates/shenko-store/tests/store_roundtrip.rs'],text=True)
name='fn canonical_first_claim_preserves_s1_birth_and_never_reissues_a_recovered_worker'
def test_body(text):
 start=text.index(name);end=text.index('\n#[test]',start)
 return text[start:end]
assert test_body(current_test)==test_body(prior_test)
reused_first_claim={'source_revision':'b4d115d887b11d5734dc0afa4d01be2c9b9eb32b','passed':1,'failed':0,'exit':0,'log':str(prior/'first-claim.log'),'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest(),'first_claim_test_body_identical':True,'retained_failed_dispatch_log':str(prior/'native-dispatch.log')}
v={'source_revision':candidate,'baseline_revision':baseline,'changed_paths':changed,'product_source_identical':True,'previous_successes_reused':16,'previous_fixture_failures_retained':3,'rerun_labels':['native-dispatch'],'reused_first_claim':reused_first_claim,'reason':'Only existing fixture initialization and exact negative-control expectations changed. Product source is byte-identical to the completed 5f scoped run.','previous_results':records}
(dst/'reuse.json').write_text(json.dumps(v,indent=2)+'\n')
print('Verified 16 unchanged-product successes and retained all three prior fixture failures',flush=True)
REUSE
status=0
run_test native-dispatch -p shenko-store --test store_roundtrip --all-features canonical_native_dispatch_retains_one_live_permit_through_prepare_learn_and_restart || status=1
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
