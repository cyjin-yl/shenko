#!/usr/bin/env bash
set -euo pipefail
task_base=4a5b88aa58e5f5c61200066fa8d37b0c6fb4dfcb
task_head=ef2256e1a420a739b9ddccb18f57af96ff2979b9
task_tools=/var/idw-target/daily-readiness-node-v24.22.1
task_output=/var/idw-target/ui-corrected-ef2256e1
task_source="$task_output/source/shenko"
task_baseline="$task_output/baseline-source"
task_upstream="$task_output/source/proto-ui"
task_host=/tank/shared/projects/shenko/build/ui-corrected-ef2256e1
task_origin=/tank/shared/projects/shenko/worktrees/daily-readiness-20261122
task_gitdir=/tank/shared/projects/shenko/shenko/.git/worktrees/daily-readiness-20261122
task_upstream_origin=/var/idw-target/daily-web-pin-795b1b49-r4/source/proto-ui
test ! -e "$task_output"
mkdir -p "$task_output/generation" "$task_output/source" "$task_output/baseline"
exec > >(tee "$task_output/container.log") 2>&1
trap 'status=$?; printf "container_exit=%s time=%s\n" "$status" "$(date -u +%FT%TZ)" > "$task_output/exit.txt"' EXIT
# This is the container-local lock, distinct from the host's held cargo.lock.
exec 8>/tmp/shenko-cargo.lock
flock -x 8
export PATH="$task_tools/bin:$PATH" COREPACK_HOME="$task_tools/corepack"
export npm_config_store_dir="$task_tools/pnpm-store" HUSKY=0 GIT_OPTIONAL_LOCKS=0
test "$(node --version)" = v24.22.1
test "$(pnpm --version)" = 10.32.1
git -c safe.directory="$task_origin" -c safe.directory="$task_gitdir" clone --shared --no-checkout --upload-pack="git -c safe.directory=$task_gitdir upload-pack" "$task_origin" "$task_source"
git -C "$task_source" sparse-checkout init --cone
git -C "$task_source" sparse-checkout set crates/shenko-ui scripts/ci
git -C "$task_source" fetch "$task_host/source.bundle" HEAD
git -C "$task_source" checkout --detach "$task_head"
test -z "$(git -C "$task_source" status --porcelain)"
git clone --shared --no-checkout "$task_source" "$task_baseline"
git -C "$task_baseline" sparse-checkout init --cone
git -C "$task_baseline" sparse-checkout set crates/shenko-ui scripts/ci
git -C "$task_baseline" checkout --detach "$task_base"
cp "$task_source/crates/shenko-ui/web/tests/live-controller.test.mjs" "$task_baseline/crates/shenko-ui/web/tests/live-controller.test.mjs"
export TASK_OUTPUT="$task_output" TASK_SOURCE="$task_source" TASK_BASELINE="$task_baseline" TASK_UPSTREAM="$task_upstream" TASK_HEAD="$task_head" TASK_BASE="$task_base"
cd "$task_baseline/crates/shenko-ui/web"
pnpm install --frozen-lockfile > "$task_output/baseline/install.log" 2>&1
python3 - <<'PY'
import datetime,hashlib,json,os,re,subprocess,time
from pathlib import Path
out=Path(os.environ['TASK_OUTPUT'])/'baseline'; base=Path(os.environ['TASK_BASELINE']); source=Path(os.environ['TASK_SOURCE'])
fixture='crates/shenko-ui/web/tests/live-controller.test.mjs'; reducer='crates/shenko-ui/web/src/timeline-reducer.ts'
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
assert subprocess.check_output(['git','-C',str(base),'diff','--name-only'],text=True).splitlines()==[fixture]
assert (base/reducer).read_bytes()==subprocess.check_output(['git','-C',str(base),'show',os.environ['TASK_BASE']+':'+reducer])
assert (base/fixture).read_bytes()==(source/fixture).read_bytes()
patch=subprocess.check_output(['git','-C',str(base),'diff','--',fixture]); (out/'fixture.patch').write_bytes(patch)
command=['node','--test','--test-reporter=tap','--test-name-pattern=^a streamed prefix is not labelled committed and is replaced by the committed text$', 'tests/live-controller.test.mjs']
started=datetime.datetime.now(datetime.timezone.utc).isoformat(); clock=time.monotonic()
with (out/'replacement.log').open('wb') as log:
 result=subprocess.run(command,cwd=base/'crates/shenko-ui/web',stdout=log,stderr=subprocess.STDOUT)
raw=(out/'replacement.log').read_bytes(); text=raw.decode()
receipt={'production_source':os.environ['TASK_BASE'],'fixture_source':os.environ['TASK_HEAD'],'command':command,'started_at':started,'finished_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),'elapsed_seconds':time.monotonic()-clock,'exit_code':result.returncode,'reducer_sha256':sha(base/reducer),'fixture_sha256':sha(base/fixture),'fixture_patch_sha256':sha(out/'fixture.patch'),'log_sha256':sha(out/'replacement.log'),'summary_lines':re.findall(r'^# (?:tests|pass|fail|skipped|cancelled|duration_ms) .*$',text,re.M),'production_unchanged':True,'candidate_fixture_exact':True}
(out/'receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
assert result.returncode==1 and '# fail 1' in text and 'stream-first' in text and 'corrected-first' in text, 'Baseline did not expose the expected two-order assertion failure; retain its actual result and stop.'
print(json.dumps(receipt,indent=2),flush=True)
PY
printf 'baseline_defect_reproduced %s\n' "$(date -u +%FT%TZ)"
git clone --shared --no-checkout "$task_upstream_origin" "$task_upstream"
git -C "$task_upstream" checkout --detach 795b1b4958333cf904547e484acb4de9d4b3ee80
test -z "$(git -C "$task_upstream" status --porcelain)"
export PROTO_UI_SOURCE="$task_upstream"
export PROTO_UI_BUILD_RECEIPT="$task_output/generation/upstream-build.json"
python3 - <<'PY'
import importlib.util,json,os,subprocess
from pathlib import Path
source=Path(os.environ['TASK_SOURCE']);out=Path(os.environ['TASK_OUTPUT']);upstream=Path(os.environ['TASK_UPSTREAM'])
spec=importlib.util.spec_from_file_location('web_assets',source/'scripts/ci/web_assets.py');m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
data={'source_head':subprocess.check_output(['git','-C',str(source),'rev-parse','HEAD'],text=True).strip(),'upstream_head':subprocess.check_output(['git','-C',str(upstream),'rev-parse','HEAD'],text=True).strip(),'node':subprocess.check_output(['node','--version'],text=True).strip(),'pnpm':subprocess.check_output(['pnpm','--version'],text=True).strip(),'web_inputs':m.files(source/'crates/shenko-ui/web',m.EXCLUDED),'upstream_inputs':m.checkout_inputs(upstream)}
(out/'generation/inputs-before.json').write_text(json.dumps(data,indent=2)+'\n')
PY
cd "$task_upstream"
pnpm install --frozen-lockfile > "$task_output/generation/upstream-install.log" 2>&1
node scripts/build/public-packages.mjs --package @proto.ui/adapter-react --package @proto.ui/prototypes-shadcn --package @proto.ui/cli --plan --json > "$task_output/generation/upstream-plan.json" 2> "$task_output/generation/upstream-plan.log"
node scripts/build/public-packages.mjs --package @proto.ui/adapter-react --package @proto.ui/prototypes-shadcn --package @proto.ui/cli --json > "$PROTO_UI_BUILD_RECEIPT" 2> "$task_output/generation/upstream-build.log"
printf 'public_closure_built %s\n' "$(date -u +%FT%TZ)"
cd "$task_source/crates/shenko-ui/web"
pnpm install --frozen-lockfile > "$task_output/generation/web-install.log" 2>&1
pnpm run check > "$task_output/generation/web-types.log" 2>&1
pnpm run test > "$task_output/generation/web-tests.log" 2>&1
pnpm run build > "$task_output/generation/web-build.log" 2>&1
python3 - <<'PY'
import importlib.util,json,os
from pathlib import Path
source=Path(os.environ['TASK_SOURCE']);out=Path(os.environ['TASK_OUTPUT']);upstream=Path(os.environ['TASK_UPSTREAM'])
spec=importlib.util.spec_from_file_location('web_assets',source/'scripts/ci/web_assets.py');m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
before=json.loads((out/'generation/inputs-before.json').read_text())
after={'web_inputs':m.files(source/'crates/shenko-ui/web',m.EXCLUDED),'upstream_inputs':m.checkout_inputs(upstream)}
(out/'generation/inputs-after.json').write_text(json.dumps(after,indent=2)+'\n')
assert before['web_inputs']==after['web_inputs'],'Frontend source changed during generation'
assert before['upstream_inputs']==after['upstream_inputs'],'Upstream source changed during generation'
receipt=json.loads((out/'generation/upstream-build.json').read_text());plan=json.loads((out/'generation/upstream-plan.json').read_text())
assert receipt['selectedCount']==42==plan['selectedCount']
assert [x['name'] for x in receipt['results']]==plan['packages']
manifest=json.loads((source/'crates/shenko-ui/src/static/build-manifest.json').read_text())
m.validate_static(source/'crates/shenko-ui/src/static',manifest)
m.compiler_files(source/'crates/shenko-ui/web',upstream,manifest)
summary={'source_head':before['source_head'],'upstream_head':before['upstream_head'],'node':before['node'],'pnpm':before['pnpm'],'web_inputs':before['web_inputs'],'upstream_input_count':len(before['upstream_inputs']),'inputs_unchanged':True,'original_inventories':{name:m.record(out/'generation'/name) for name in ['inputs-before.json','inputs-after.json']}}
(out/'generation/input-summary.json').write_text(json.dumps(summary,indent=2)+'\n')
print('producer_verified: 42 packages; complete static and compiler/prototype digests; unchanged inputs',flush=True)
PY
printf 'generation_verified %s\n' "$(date -u +%FT%TZ)"
cd "$task_source"
python3 scripts/ci/web_assets.py prepare --output "$task_output/rebuild" > "$task_output/rebuild-prepare.log" 2>&1
python3 scripts/ci/web_assets.py check --output "$task_output/rebuild" > "$task_output/rebuild-check.log" 2>&1
printf 'independent_rebuild_verified %s\n' "$(date -u +%FT%TZ)"
mkdir "$task_output/delivery"
cp -a "$task_source/crates/shenko-ui/src/static" "$task_output/delivery/static"
cp -a "$task_output/baseline" "$task_output/delivery/baseline"
mkdir "$task_output/delivery/generation" "$task_output/delivery/rebuild"
find "$task_output/generation" -maxdepth 1 -type f ! -name 'inputs-before.json' ! -name 'inputs-after.json' -exec cp '{}' "$task_output/delivery/generation/" ';'
find "$task_output/rebuild" -maxdepth 1 -type f ! -name 'inputs.json' -exec cp '{}' "$task_output/delivery/rebuild/" ';'
cp "$task_output/rebuild/source/src/static/build-manifest.json" "$task_output/delivery/rebuilt-manifest.json"
cp "$task_output/rebuild-prepare.log" "$task_output/rebuild-check.log" "$task_output/delivery/"
cp "$task_host/container-build-web.sh" "$task_host/host-build-web.sh" "$task_host/source.bundle" "$task_output/delivery/"
tar -C "$task_output/delivery" -czf "$task_output/delivery.tar.gz" .
sha256sum "$task_output/delivery.tar.gz"
printf 'delivery_ready %s\n' "$(date -u +%FT%TZ)"
