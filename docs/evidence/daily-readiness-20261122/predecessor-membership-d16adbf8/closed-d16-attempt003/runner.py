#!/usr/bin/env python3
"""Compile a read-only CAS probe against the completed exact-d16 release rlib."""
from pathlib import Path
import datetime
import hashlib
import json
import os
import re
import subprocess

root = Path('/tank/shared/projects/shenko/build/daily-readiness-20261122')
checkout = Path('/tank/shared/projects/shenko/worktrees/daily-readiness-20261122')
source = 'd16adbf89c9f79dfce8ef90d77843412bcda85bf'
attempt = root / 'final-d16adbf8/attempt-003'
summary = json.loads((attempt / 'summary.json').read_text())
group = summary['last_shutdown']['group_cleanup']
assert summary.get('finished_at') and group['complete'] and group['parent_reaped']
assert not group['remaining']['alive'] and not group['remaining']['live_pids']
assert not group['remaining']['live_thread_ids']
try:
    os.killpg(group['pgid'], 0)
except ProcessLookupError:
    pass
else:
    raise RuntimeError('original owned process group exists')
assert subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip() == source
assert not subprocess.check_output(['git', '-C', str(checkout), 'status', '--porcelain'], text=True).strip()
assert (root / 'release-d16adbf8/exit.txt').read_text().strip() == '0'
target = Path('/var/idw-target/daily-readiness-target-20261122/release')
store = target / 'deps/libshenko_store-4c6d5975f5c1167a.rlib'
fp = target / '.fingerprint/shenko-store-4c6d5975f5c1167a/lib-shenko_store.json'
fingerprint = json.loads(fp.read_text())
assert fingerprint['features'] == '[]'
dependencies = {row[1]: row[3] for row in fingerprint['deps']}


def dependency(name):
    matches = []
    for identity in target.glob(f'.fingerprint/{name}-*/lib-{name}'):
        if int.from_bytes(bytes.fromhex(identity.read_text().strip()), 'little') == dependencies[name]:
            digest = identity.parent.name.rsplit('-', 1)[1]
            matches.append(target / f'deps/lib{name}-{digest}.rlib')
    assert len(matches) == 1, (name, matches)
    return matches[0]


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


output = root / 'closed-predecessor-d16adbf8-attempt003'
output.mkdir()
program = root.parent / 'daily-readiness-20261122-inspect-closed-predecessor.rs'
(output / 'inspect.rs').write_bytes(program.read_bytes())
(output / 'runner.py').write_bytes(Path(__file__).read_bytes())
binary = output / 'inspect'
libs = {'shenko_store': store, 'serde_json': dependency('serde_json'), 'anyhow': dependency('anyhow')}
command = ['rustc', '--edition=2024', '-C', 'debuginfo=0', str(program), '-o', str(binary), '-L', f'dependency={target / "deps"}']
for name, path in libs.items():
    command += ['--extern', f'{name}={path}']
for native in sorted(target.glob('build/*/out')):
    command += ['-L', f'native={native}']
manifest = {
    'source_revision': source, 'started_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
    'scope': 'Read-only original closed CAS inspection, using the completed release rlib. No Store/SQL open, new model, repair, effect or replay.',
    'program_sha256': sha(program), 'runner_sha256': sha(Path(__file__)),
    'release_manifest_sha256': sha(root / 'release-d16adbf8/manifest.json'),
    'source_roots_sha256': sha(checkout / 'crates/shenko-store/src/roots.rs'),
    'source_control_predecessor_sha256': sha(checkout / 'crates/shenko-store/src/control_predecessor.rs'),
    'libraries': {name: {'path': str(path), 'bytes': path.stat().st_size, 'sha256': sha(path)} for name, path in libs.items()},
    'fingerprint': fingerprint, 'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip(),
    'command': command, 'original_shutdown': summary['last_shutdown'],
}
with (output / 'compile.log').open('w') as log:
    result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
manifest['compile_exit'] = result.returncode
if result.returncode == 0:
    cas = attempt / 'installation/cas'
    payload_digest = '2c16c35cd03f62889bc95cd6e68d59394940c1d6301c4836798d5fb45cf84ddd'
    payload_path = cas / payload_digest[:2] / payload_digest[2:4] / payload_digest
    payload_raw = payload_path.read_bytes()
    payload = json.loads(payload_raw)
    reference = payload['predecessor_ref']
    manifest['payload'] = {'digest': 'blake3:' + payload_digest, 'bytes': len(payload_raw), 'sha256': hashlib.sha256(payload_raw).hexdigest()}
    (output / 'payload.json').write_bytes(payload_raw)
    name = reference['digest'].removeprefix('blake3:')
    (output / 'predecessor.json').write_bytes((cas / name[:2] / name[2:4] / name).read_bytes())
    def inventory():
        return {str(path.relative_to(cas)): {'bytes': path.stat().st_size, 'mtime_ns': path.stat().st_mtime_ns}
                for path in cas.glob('*/*/*') if re.fullmatch('[0-9a-f]{64}', path.name)}
    before = inventory()
    run_command = [str(binary), str(cas), json.dumps(reference)]
    with (output / 'observation.json').open('w') as log, (output / 'run.stderr').open('w') as errors:
        result = subprocess.run(run_command, stdout=log, stderr=errors)
    manifest['run_exit'] = result.returncode
    manifest['run_command'] = run_command
    after = inventory()
    manifest['cas_file_sizes_and_mtimes_unchanged'] = before == after
    manifest['cas_file_count'] = len(before)
    (output / 'cas-metadata-before.json').write_text(json.dumps(before, indent=2) + '\n')
    (output / 'cas-metadata-after.json').write_text(json.dumps(after, indent=2) + '\n')
manifest['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
(output / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
if manifest.get('run_exit') == 0:
    observation = json.loads((output / 'observation.json').read_text())
    print(json.dumps({'output': str(output), 'compile_exit': manifest['compile_exit'], 'run_exit': manifest['run_exit'],
                      'expected': observation['expected_object_count'], 'roots': observation['root_count'],
                      'observations': [{k: v for k, v in row.items() if k != 'objects'} for row in observation['observations']],
                      'unchanged_metadata': before == after}, indent=2))
else:
    print(json.dumps({'output': str(output), 'compile_exit': manifest['compile_exit'], 'run_exit': manifest.get('run_exit')}, indent=2))
    print((output / 'compile.log').read_text())
raise SystemExit(manifest.get('run_exit', manifest['compile_exit']))
