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
original = '972e16ef34213f568eda3b236fc33be50e40d7fb'
fixture = '4a5b88aa58e5f5c61200066fa8d37b0c6fb4dfcb'
subprocess.run(['git', 'merge-base', '--is-ancestor', fixture, current], check=True)
changed = subprocess.check_output(['git','diff','--name-only', fixture, current], text=True).splitlines()
assert all(p.startswith(('crates/shenko-ui/','docs/')) for p in changed), changed
assert subprocess.check_output(['git','diff','--name-only',original,fixture], text=True).splitlines() == ['crates/shenko-store/tests/store_roundtrip.rs']
for source, directory, expected in [(original, 'scoped-972e16ef', (12,1)), (fixture, 'scoped-native-chain-4a5b88aa', (1,0))]:
    s=json.loads((root/directory/'summary.json').read_text())
    assert s['source_revision']==source and s['completed_targets']==s['planned_targets']
    outcomes=[r['outer_summary'] for r in s['checks']]
    assert all(o is not None for o in outcomes)
    assert (sum(int(o[1]) for o in outcomes),sum(int(o[2]) for o in outcomes))==expected
    assert all(int(o[3])==0 and int(o[4])==0 for o in outcomes)
    if source==original:
        assert [r['label'] for r in s['checks'] if not r['passed']]==['native-chain']
    else:
        assert s['all_passed'] and s['planned_targets']==1
    for r in s['checks']:
        data=(root/directory/(r['label']+'.log')).read_bytes()
        assert len(data)==r['bytes'] and hashlib.sha256(data).hexdigest()==r['sha256']
static=Path('crates/shenko-ui/src/static')
m=json.loads((static/'build-manifest.json').read_text())
actual={p.relative_to(static).as_posix() for p in static.rglob('*') if p.is_file() and p.name!='build-manifest.json'}
assert actual==set(m['artifacts'])
for name,record in m['artifacts'].items():
    data=(static/name).read_bytes()
    assert len(data)==record['bytes'] and hashlib.sha256(data).hexdigest()==record['sha256'],name
gate={'source_revision':current,'reused_scope':original,'scope_passes':12,'fixed_native_chain':fixture,'fixed_native_chain_passes':1,'changed_paths_since_fixture':changed,'static_inventory_verified':len(actual),'static_manifest_sha256':hashlib.sha256((static/'build-manifest.json').read_bytes()).hexdigest()}
with (root/('release-'+current[:8]+'-preflight.json')).open('x') as output:
    output.write(json.dumps(gate,indent=2)+'\n')
print(json.dumps(gate),flush=True)
PY
build="$root/release-$tag"
test ! -e "$build"
test ! -e "$root/shenko-release-$tag"
mkdir "$build"
cp /tank/shared/projects/shenko/build/daily-readiness-20261122-build-reviewed-candidate.sh "$build/runner.sh"
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
