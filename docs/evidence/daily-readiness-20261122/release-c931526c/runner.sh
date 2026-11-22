#!/usr/bin/env bash
set -euo pipefail
candidate="$1"
[[ "$candidate" =~ ^[0-9a-f]{40}$ ]]
tag="${candidate:0:8}"
root=/tank/shared/projects/shenko/build/daily-readiness-20261122
exec 9>/tmp/shenko-cargo.lock
flock 9
cd /tank/shared/projects/shenko/worktrees/daily-readiness-20261122
export CARGO_HOME=/var/idw-target/poppy-cargo-home-20261116
export CARGO_TARGET_DIR=/var/idw-target/daily-readiness-target-20261122
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=4 CARGO_NET_OFFLINE=true
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
export SHENKO_SOURCE_REVISION="$candidate"
export PATH="/var/idw-target/daily-readiness-node-v24.22.1/bin:$PATH"
test "$(git rev-parse HEAD)" = "$candidate"
test -z "$(git status --porcelain)"
python3 - "$candidate" <<'PY'
from pathlib import Path
import hashlib, json, subprocess, sys
root = Path('/tank/shared/projects/shenko/build/daily-readiness-20261122')
current = sys.argv[1]
fixed = '95cdc272866a5f31cedb790d263761523e9b0e2c'
subprocess.run(['git', 'merge-base', '--is-ancestor', fixed, current], check=True)
changed = subprocess.check_output(['git', 'diff', '--name-only', fixed, current], text=True).splitlines()
assert all(p.startswith(('docs/', 'scripts/')) for p in changed), changed
scope = root / 'scoped-frozen-closure-95cdc272'
s = json.loads((scope / 'summary.json').read_text())
assert s['source_revision'] == fixed and s['completed_targets'] == s['planned_targets'] == 3
assert s['all_passed']
assert [(r['label'], r['expected_tests']) for r in s['checks']] == [('predecessor-membership', 1), ('native-chain', 1), ('archive-admission', 8)]
for r in s['checks']:
    o = r['outer_summary']
    assert r['passed'] and r['exit_code'] == 0 and o[0] == 'ok'
    assert tuple(map(int, o[1:5])) == (r['expected_tests'], 0, 0, 0)
    data = (scope / (r['label'] + '.log')).read_bytes()
    assert len(data) == r['bytes'] and hashlib.sha256(data).hexdigest() == r['sha256']
static=Path('crates/shenko-ui/src/static')
m=json.loads((static/'build-manifest.json').read_text())
actual={p.relative_to(static).as_posix() for p in static.rglob('*') if p.is_file() and p.name!='build-manifest.json'}
assert actual==set(m['artifacts'])
for name,record in m['artifacts'].items():
    data=(static/name).read_bytes()
    assert len(data)==record['bytes'] and hashlib.sha256(data).hexdigest()==record['sha256'],name
gate={'source_revision':current,'scoped_source_revision':fixed,'scoped_summary_sha256':hashlib.sha256((scope/'summary.json').read_bytes()).hexdigest(),'scoped_executions_passed':10,'scoped_targets':3,'changed_paths_since_scoped_source':changed,'static_inventory_verified':len(actual),'static_manifest_sha256':hashlib.sha256((static/'build-manifest.json').read_bytes()).hexdigest(),'product_tree_identical_to_scoped_source':True,'model_journey_tested':False}
with (root/('release-'+current[:8]+'-preflight.json')).open('x') as output:
    output.write(json.dumps(gate,indent=2)+'\n')
print(json.dumps(gate),flush=True)
PY
build="$root/release-$tag"
test ! -e "$build"
test ! -e "$root/shenko-release-$tag"
mkdir "$build"
cp /tank/shared/projects/shenko/build/daily-readiness-20261122-build-frozen-candidate.sh "$build/runner.sh"
rustc --version > "$build/rustc.txt"
cargo --version > "$build/cargo.txt"
node --version > "$build/node.txt"
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
