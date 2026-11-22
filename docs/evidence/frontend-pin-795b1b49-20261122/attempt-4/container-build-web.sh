#!/usr/bin/env bash
set -euo pipefail
task_tools=/var/idw-target/daily-readiness-node-v24.22.1
task_output=/var/idw-target/daily-web-pin-795b1b49-r4
task_source=/var/idw-target/daily-web-pin-795b1b49-r4/source/shenko
task_upstream=/var/idw-target/daily-web-pin-795b1b49-r4/source/proto-ui
task_host=/tank/shared/projects/shenko/build/daily-web-pin-795b1b49-r4
mkdir -p "$task_tools"
mkdir "$task_output"
mkdir "$task_output/generation"
exec > >(tee "$task_output/container.log") 2>&1
trap 'status=$?; printf "container_exit=%s time=%s\n" "$status" "$(date -u +%FT%TZ)" > "$task_output/exit.txt"' EXIT
if [ ! -x "$task_tools/node_modules/node/bin/node" ]; then
  /usr/local/bin/npm install --prefix "$task_tools" --no-audit --no-fund --save-exact node@24.22.1 > "$task_output/node-install.log" 2>&1
fi
mkdir -p "$task_tools/bin"
ln -sfn "$task_tools/node_modules/node/bin/node" "$task_tools/bin/node"
cat > "$task_tools/bin/pnpm" <<'SHIM'
#!/bin/sh
exec /usr/local/bin/corepack pnpm@10.32.1 "$@"
SHIM
chmod 755 "$task_tools/bin/pnpm"
export PATH="$task_tools/bin:$PATH"
export COREPACK_HOME="$task_tools/corepack"
export npm_config_store_dir="$task_tools/pnpm-store"
export HUSKY=0 GIT_OPTIONAL_LOCKS=0
export GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0="$task_source"
export GIT_CONFIG_KEY_1=safe.directory GIT_CONFIG_VALUE_1="$task_upstream"
# clone sanitizes repository-local environment before spawning upload-pack.
# Match the successful no-checkout clone preflight: each upload process gets
# only its observed administrative Git directory through its own -c option.
# Only read the host-owned repositories. Private Git metadata, index and
# checked-out files live on the writable persistent volume and belong to this
# process's user, so package-manager bin chmod never touches host-owned source.
mkdir "$task_output/source"
git -c safe.directory=/tank/shared/projects/shenko/worktrees/shenko-web-pin-795b1b49 -c safe.directory=/tank/shared/projects/shenko/shenko/.git/worktrees/shenko-web-pin-795b1b49 clone --shared --no-checkout --upload-pack="git -c safe.directory=/tank/shared/projects/shenko/shenko/.git/worktrees/shenko-web-pin-795b1b49 upload-pack" /tank/shared/projects/shenko/worktrees/shenko-web-pin-795b1b49 "$task_source"
git -C "$task_source" sparse-checkout init --cone
git -C "$task_source" sparse-checkout set crates/shenko-ui scripts/ci
git -C "$task_source" checkout --detach 9a7be2f88e4922b8b70ba13ec2078145129f5ce0
git -C "$task_source" apply --check "$task_host/shenko-pin.patch"
git -C "$task_source" apply "$task_host/shenko-pin.patch"
git -c safe.directory=/tank/shared/projects/shenko/worktrees/proto-ui-web-pin-795b1b49 -c safe.directory=/tank/shared/projects/shenko/worktrees/proto-ui-scroll-fixture-20261122/.git/worktrees/proto-ui-web-pin-795b1b49 clone --shared --no-checkout --upload-pack="git -c safe.directory=/tank/shared/projects/shenko/worktrees/proto-ui-scroll-fixture-20261122/.git/worktrees/proto-ui-web-pin-795b1b49 upload-pack" /tank/shared/projects/shenko/worktrees/proto-ui-web-pin-795b1b49 "$task_upstream"
git -C "$task_upstream" checkout --detach 795b1b4958333cf904547e484acb4de9d4b3ee80
test "$(node --version)" = v24.22.1
test "$(pnpm --version)" = 10.32.1
test "$(git -C "$task_upstream" rev-parse HEAD)" = 795b1b4958333cf904547e484acb4de9d4b3ee80
export PROTO_UI_SOURCE="$task_upstream"
export PROTO_UI_BUILD_RECEIPT="$task_output/generation/upstream-build.json"
export TASK_OUTPUT="$task_output" TASK_SOURCE="$task_source" TASK_UPSTREAM="$task_upstream"
python3 - <<'PY'
import hashlib,importlib.util,json,os,platform,subprocess
from pathlib import Path
source=Path(os.environ['TASK_SOURCE']); out=Path(os.environ['TASK_OUTPUT']); upstream=Path(os.environ['TASK_UPSTREAM'])
spec=importlib.util.spec_from_file_location('web_assets',source/'scripts/ci/web_assets.py'); module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
data={'source_head':subprocess.check_output(['git','-C',str(source),'rev-parse','HEAD'],text=True).strip(),'source_diff':subprocess.check_output(['git','-C',str(source),'diff','--','crates/shenko-ui/web'],text=True),'upstream_head':subprocess.check_output(['git','-C',str(upstream),'rev-parse','HEAD'],text=True).strip(),'node':subprocess.check_output(['node','--version'],text=True).strip(),'pnpm':subprocess.check_output(['pnpm','--version'],text=True).strip(),'platform':platform.platform(),'web_inputs':module.files(source/'crates/shenko-ui/web',module.EXCLUDED),'upstream_inputs':module.checkout_inputs(upstream)}
(out/'generation/inputs-before.json').write_text(json.dumps(data,indent=2)+'\n')
PY
printf 'toolchain_ready %s\n' "$(date -u +%FT%TZ)"
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
print('producer_verified: 42 packages; complete static and compiler/prototype digests; unchanged inputs')
PY
printf 'generation_verified %s\n' "$(date -u +%FT%TZ)"
cd "$task_source"
python3 scripts/ci/web_assets.py prepare --output "$task_output/rebuild" > "$task_output/rebuild-prepare.log" 2>&1
python3 scripts/ci/web_assets.py check --output "$task_output/rebuild" > "$task_output/rebuild-check.log" 2>&1
printf 'independent_rebuild_verified %s\n' "$(date -u +%FT%TZ)"
mkdir "$task_output/delivery"
cp -a "$task_source/crates/shenko-ui/src/static" "$task_output/delivery/static"
cp -a "$task_output/generation" "$task_output/delivery/generation"
mkdir "$task_output/delivery/rebuild"
find "$task_output/rebuild" -maxdepth 1 -type f -exec cp '{}' "$task_output/delivery/rebuild/" ';'
cp "$task_output/rebuild-prepare.log" "$task_output/rebuild-check.log" "$task_output/delivery/"
cp "$task_host/container-build-web.sh" "$task_host/host-build-web.sh" "$task_host/shenko-pin.patch" "$task_output/delivery/"
tar -C "$task_output/delivery" -czf "$task_output/delivery.tar.gz" .
sha256sum "$task_output/delivery.tar.gz"
printf 'delivery_ready %s\n' "$(date -u +%FT%TZ)"
