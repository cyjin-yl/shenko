#!/usr/bin/env python3
"""Finite, source-bound development checks; not full S2 qualification."""
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time

REPO = Path('/tank/shared/projects/shenko/worktrees/continuation-pr327-20261117-1312')
BASE = Path('/tank/shared/projects/shenko/build/continuation-pr327-20261117-1312')
EXPECTED = '8978816295429b65308e6dd6c8499cca360f8552'
OUT = BASE / 'validation-r2'
OUT.mkdir(exist_ok=False)
ENV = os.environ.copy()
ENV.update({
    'PATH': '/usr/local/rustup/toolchains/1.98.1-x86_64-unknown-linux-gnu/bin:/usr/local/cargo/bin:' + ENV.get('PATH', ''),
    'CARGO_HOME': '/usr/local/cargo', 'RUSTUP_HOME': '/usr/local/rustup',
    'CARGO_TARGET_DIR': '/var/idw-target/continuation-pr327-20261117-1312',
    'CARGO_INCREMENTAL': '0', 'CARGO_PROFILE_DEV_DEBUG': '0', 'CARGO_PROFILE_TEST_DEBUG': '0',
    'CARGO_BUILD_JOBS': '1', 'SHENKO_UPDATE_BUILD_EVIDENCE': str(OUT / 'update-originals'),
})

def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()

def save(name, value):
    (OUT / name).write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')

def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()

def git(*args):
    return subprocess.check_output(['git', '-c', 'safe.directory=' + str(REPO), *args],
        cwd=REPO, env=ENV, timeout=90).decode().strip()

records = []
def run(name, argv, limit, expected_passed=None):
    record = {'name': name, 'argv': argv, 'started_at': now(), 'deadline_seconds': limit,
              'timed_out': False, 'expected_passed': expected_passed}
    started = time.monotonic()
    with (OUT / (name + '.stdout')).open('xb') as stdout, (OUT / (name + '.stderr')).open('xb') as stderr:
        process = subprocess.Popen(argv, cwd=REPO, env=ENV, stdout=stdout, stderr=stderr, start_new_session=True)
        record['pid'] = process.pid
        try:
            code = process.wait(timeout=limit)
        except subprocess.TimeoutExpired:
            record['timed_out'] = True
            os.killpg(process.pid, signal.SIGTERM)
            try:
                code = process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                code = process.wait(timeout=15)
    record.update({'finished_at': now(), 'elapsed_seconds': time.monotonic() - started,
                   'returncode': code, 'leader_reaped': process.returncode is not None})
    for stream in ('stdout', 'stderr'):
        path = OUT / (name + '.' + stream)
        record[stream] = {'path': path.name, 'bytes': path.stat().st_size, 'sha256': sha(path)}
    if expected_passed is not None:
        text = (OUT / (name + '.stdout')).read_text(errors='replace')
        record['summaries'] = re.findall(r'test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored;', text)
        record['executed_expected_cases'] = any(status == 'ok' and int(passed) == expected_passed and int(failed) == 0
            for status, passed, failed, ignored in record['summaries'])
    record['passed'] = code == 0 and not record['timed_out'] and record.get('executed_expected_cases', True)
    records.append(record)
    save(name + '.json', record)
    save('commands.json', records)
    print(json.dumps(record, ensure_ascii=False), flush=True)
    return record['passed']

try:
    head = git('rev-parse', 'HEAD')
    status = git('status', '--porcelain', '--untracked-files=no')
    if head != EXPECTED or status:
        raise RuntimeError('source is not the expected clean candidate')
    inputs = ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml',
              'crates/shenko-types/src/installation.rs', 'crates/shenko-store/src/ledger.rs',
              'crates/shenko-update/tests/rollback_migration.rs', 'crates/shenko-update/src/handoff_tests.rs',
              'docs/specs/self-update.md']
    binding = {'head': head, 'tree': git('rev-parse', 'HEAD^{tree}'), 'observed_at': now(),
               'source': str(REPO), 'files': {p: sha(REPO / p) for p in inputs},
               'not_an_atomic_source_freeze': True,
               'environment': {k: ENV[k] for k in ['CARGO_HOME', 'RUSTUP_HOME', 'CARGO_TARGET_DIR',
                    'CARGO_INCREMENTAL', 'CARGO_PROFILE_DEV_DEBUG', 'CARGO_PROFILE_TEST_DEBUG', 'CARGO_BUILD_JOBS']}}
    save('source-before.json', binding)
    (OUT / 'runner.py').write_bytes(Path(__file__).read_bytes())
    run('rustc-version', ['rustc', '-Vv'], 30)
    run('cargo-version', ['cargo', '-V'], 30)
    run('metadata', ['cargo', 'metadata', '--no-deps', '--format-version=1', '--locked', '--offline'], 60)
    rollback = run('rollback-migration', ['cargo', 'test', '--locked', '--offline', '-j1', '-p', 'shenko-update',
        '--test', 'rollback_migration', '--', '--nocapture'], 900, 3)
    if rollback or 'running ' in (OUT / 'rollback-migration.stdout').read_text(errors='replace'):
        run('handoff-fence', ['cargo', 'test', '--locked', '--offline', '-j1', '-p', 'shenko-update', '--lib',
            'activate_commits_a_new_generation_and_fences_the_retired_writer', '--', '--nocapture'], 600, 1)
    after = {'head': git('rev-parse', 'HEAD'), 'status': git('status', '--porcelain', '--untracked-files=no'),
             'files': {p: sha(REPO / p) for p in inputs}, 'observed_at': now()}
    after['same_observed_source'] = after['head'] == head and not after['status'] and after['files'] == binding['files']
    save('source-after.json', after)
    save('result.json', {'commands_passed': all(r['passed'] for r in records),
        'same_observed_source': after['same_observed_source'], 'full_ship_work': False,
        'model_validation': False, 'long_run_validation': False, 'finished_at': now()})
    if not all(r['passed'] for r in records) or not after['same_observed_source']:
        raise RuntimeError('validation failed; retained original command receipts describe the failure')
finally:
    save('inventory.json', [{'path': str(p.relative_to(OUT)), 'bytes': p.stat().st_size, 'sha256': sha(p)}
        for p in sorted(OUT.rglob('*')) if p.is_file() and p.name != 'inventory.json'])
