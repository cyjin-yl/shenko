#!/usr/bin/env python3
"""Copy only completed c931 workflow originals; no workflow or build mutation."""
from pathlib import Path
import base64
import datetime
import gzip
import hashlib
import io
import json
import tarfile

ROOT = Path('/tank/shared/projects/shenko/build/daily-readiness-20261122')
SHA = 'c931526c8ae8cd16f3e0cf56d733f1e4275d5bb0'
OUT = ROOT / 'ci-nightly-c931526c-originals'
START = datetime.datetime.now(datetime.timezone.utc).isoformat()


def digest(data):
    return hashlib.sha256(data).hexdigest()


def stat(path):
    value = path.stat()
    return {'bytes': value.st_size, 'mtime_ns': value.st_mtime_ns,
            'inode': value.st_ino, 'device': value.st_dev}


for kind, run_id, expected in [('ci', 44968265233, 'success'), ('nightly', 44968307539, 'failure')]:
    directory = ROOT / f'{kind}-c931526c'
    run = json.loads((directory / 'run.json').read_bytes())
    jobs = json.loads((directory / 'jobs.json').read_bytes())
    assert run['id'] == run_id and run['head_sha'] == SHA
    assert run['status'] == 'completed' and run['conclusion'] == expected
    assert jobs['total_count'] == len(jobs['jobs'])
    assert all(job['run_id'] == run_id and job['head_sha'] == SHA
               and job['status'] == 'completed' for job in jobs['jobs'])
observer = json.loads((ROOT / 'ci-nightly-c931526c-progress/observer-finished.json').read_bytes())
assert observer['source_revision'] == SHA and observer['all_runs_completed']
assert observer['observer_error'] is None and observer['observer_exit_code'] == 1
assert observer['github_mutations'] is False
native = ROOT / 'nightly-c931526c/selected-native-artifact'
selection = json.loads((native / 'selection.json').read_bytes())
assert selection['source_revision'] == SHA and len(selection['selected']) == 3

OUT.mkdir(exist_ok=False)
entries = []


def retain(source, relative):
    assert source.is_file() and not source.is_symlink(), source
    before = stat(source)
    raw = source.read_bytes()
    after = stat(source)
    assert before == after and len(raw) == before['bytes'], source
    compress = (source.suffix == '.log' or source.name in
                {'inputs.json', 'artifact-index.json', 'partition-receipt-from-job-log.json'}
                or (relative.startswith('observation/') and source.name not in
                    {'observer-start.json', 'observer-finished.json'}))
    stored = gzip.compress(raw, mtime=0) if compress else raw
    relative += '.gz' if compress else ''
    target = OUT / relative
    target.parent.mkdir(parents=True, exist_ok=True)
    with target.open('xb') as stream:
        stream.write(stored)
    entries.append({'path': relative, 'source': str(source), 'source_bytes': len(raw),
                    'source_sha256': digest(raw), 'bytes': len(stored), 'sha256': digest(stored),
                    'encoding': 'gzip' if compress else 'original',
                    'source_stat_before': before, 'source_stat_after': after})


for kind in ['ci', 'nightly']:
    directory = ROOT / f'{kind}-c931526c'
    for path in sorted(directory.iterdir()):
        if path.is_file():
            retain(path, f'{kind}/{path.name}')
for path in sorted((ROOT / 'ci-c931526c/auxiliary').iterdir()):
    retain(path, f'ci/auxiliary/{path.name}')
for path in sorted((ROOT / 'nightly-c931526c/compiler').iterdir()):
    if path.name != 'compiler-artifact.zip':
        retain(path, f'compiler/{path.name}')
for path in sorted(native.rglob('*')):
    if path.is_file():
        retain(path, 'start-boundary/' + path.relative_to(native).as_posix())
for prefix, directory in [('dispatch', ROOT / 'nightly-c931526c-dispatch'),
                          ('observation', ROOT / 'ci-nightly-c931526c-progress')]:
    for path in sorted(directory.iterdir()):
        retain(path, prefix + '/' + path.name)
initial = ROOT / 'nightly-c931526c-compiler-failure'
for name in ['run.json', 'job.json', 'artifacts.json']:
    retain(initial / name, 'initial-compiler-failure/' + name)
assert (initial / 'job.log').read_bytes() == (ROOT / 'nightly-c931526c/compiler/compiler.log').read_bytes()
for name in ['collect_completed_rust_receipt.py', 'rust_outer_harness_audit.py',
             'collect_completed_compiler_receipt.py', 'select_github_artifact_members.py',
             'watch_candidate_ci_nightly.py']:
    retain(ROOT / name, 'collection/' + name)
retain(Path(__file__), 'collection/collect_originals.py')

index = {'schema_version': 1, 'source_revision': SHA, 'started_at_utc': START,
         'finished_at_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
         'files': entries,
         'byte_identical_original_alias': {
             'source': str(initial / 'job.log'), 'retained_as': 'compiler/compiler.log.gz',
             'raw_sha256': digest((initial / 'job.log').read_bytes())},
         'excluded': ['Full native artifact ZIP: only selected members and central directory fetched',
                      'Compiler artifact ZIP: fully fetched and hash-verified remotely; selected originals retained'],
         'new_test_model_build_or_workflow_mutations': 0}
(OUT / 'retention-index.json').write_text(json.dumps(index, indent=2) + '\n')
buffer = io.BytesIO()
with tarfile.open(fileobj=buffer, mode='w') as archive:
    for path in sorted(OUT.rglob('*')):
        if path.is_file():
            raw = path.read_bytes()
            info = tarfile.TarInfo(path.relative_to(OUT).as_posix())
            info.size = len(raw)
            info.mode = 0o644
            info.mtime = 0
            archive.addfile(info, io.BytesIO(raw))
packed = gzip.compress(buffer.getvalue(), mtime=0)
archive_path = ROOT / 'ci-nightly-c931526c-originals.tar.gz'
with archive_path.open('xb') as stream:
    stream.write(packed)
parts = ROOT / 'ci-nightly-c931526c-transfer'
parts.mkdir(exist_ok=False)
count = 0
for offset in range(0, len(packed), 12288):
    (parts / f'{count:03d}.txt').write_text(base64.b64encode(packed[offset:offset + 12288]).decode())
    count += 1
print(json.dumps({'original_files': len(entries), 'archive_bytes': len(packed),
                  'archive_sha256': digest(packed), 'parts': count, 'part_raw_bytes': 12288,
                  'directory': str(OUT)}, indent=2))
