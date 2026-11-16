#!/usr/bin/env python3
"""Package final retained evidence only, after an explicit archive-ready checkpoint.

No inference, process control, repository edits, Store writes or retries occur here.
"""
import hashlib
import json
import shutil
import tarfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
COMPACT = ROOT.with_name(ROOT.name + '-compact')
FULL_TAR = ROOT.with_name(ROOT.name + '-full-evidence.tar.gz')
COMPACT_TAR = ROOT.with_name(ROOT.name + '-compact-evidence.tar.gz')
DELIVERY = ROOT.with_name(ROOT.name + '-delivery-index.json')


def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def load(path):
    return json.loads(path.read_bytes())


def dump(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')


def files(root):
    return sorted(p for p in root.rglob('*') if p.is_file() and '__pycache__' not in p.parts)


def record(path, root):
    return {'path': str(path.relative_to(root)), 'bytes': path.stat().st_size, 'sha256': sha(path)}


ready_path = ROOT / 'analysis/archive-ready.json'
if not ready_path.exists():
    raise SystemExit('archive-ready.json is required; do not package a live run or incomplete service evidence')
ready = load(ready_path)
assert ready.get('runner_termination_confirmed') is True
assert ready.get('final_service_evidence_confirmed') is True
assert ready.get('required_files')
for required in ready['required_files']:
    path = ROOT / required['path']
    assert path.is_file() and sha(path) == required['sha256'], required
summary = load(ROOT / 'analysis/result-summary.json')
assert summary['phase'] == 'final_retained_evidence' and summary['terminal_evidence_available']
assert summary['runner_exit'] == load(ROOT / 'runner-exit.json')
report_path = next((ROOT / 'acceptance').glob('run-*/report.json'))
assert summary['runner_report'] == load(report_path)
assert (ROOT / 'EVIDENCE.md').is_file()

inventory_path = ROOT / 'analysis/inventory.sha256.json'
source_inventory = [record(p, ROOT) for p in files(ROOT) if p != inventory_path]
dump(inventory_path, {
    'scope': 'All regular full-evidence files excluding __pycache__ and this self-referential inventory',
    'files': source_inventory,
})
COMPACT.mkdir(exist_ok=False)
omitted = []
copied = []
for path in files(ROOT):
    relative = path.relative_to(ROOT)
    is_snapshot = relative.parts[:2] == ('analysis', 'snapshots')
    is_acceptance = relative.parts[0] == 'acceptance'
    is_cas = is_acceptance and 'cas' in relative.parts
    is_database_or_binary = (path.suffix in ('.db', '.sqlite', '.sqlite3', '.bin') or
                             path.name.endswith(('-wal', '-shm')))
    include = not is_snapshot and (is_cas or not (is_acceptance and is_database_or_binary))
    if include:
        target = COMPACT / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, target)
        assert sha(target) == sha(path)
        copied.append(str(relative))
    else:
        omitted.append({**record(path, ROOT), 'reason': 'provisional_snapshot' if is_snapshot else 'original_database_or_binary_in_full_archive'})

cas_index = load(ROOT / 'analysis/cas-index.json')
for item in cas_index:
    assert (COMPACT / item['path']).is_file()
    assert sha(COMPACT / item['path']) == item['sha256']
dump(COMPACT / 'CAS-LOCATORS.json', {
    'note': 'Original locators inside receipts remain unchanged. This maps every retained CAS object to a portable path.',
    'files': [{'original_locator': (ROOT / item['path']).as_uri(), 'portable_path': item['path'],
               'condition': item['condition'], 'bytes': item['bytes'], 'sha256': item['sha256'],
               'declared_digest': item['declared_digest']} for item in cas_index],
})
dump(COMPACT / 'INDEX.json', {
    'schema_version': 1, 'created_at_utc': datetime.now(timezone.utc).isoformat(),
    'run_id': summary['run_id'], 'source_head': summary['run_identity']['source_head'],
    'executable_sha256': summary['run_identity']['executable_sha256'], 'model': summary['model'],
    'runner_status': summary['strict_result'], 'complete': summary['complete'], 'cleanup_ok': summary['cleanup_ok'],
    'entrypoint': 'EVIDENCE.md', 'machine_readable_summary': 'analysis/result-summary.json',
    'native_retrieval_trace': 'analysis/native-retrievals.json',
    'source_root': str(ROOT), 'all_original_CAS_included': True, 'CAS_count': len(cas_index),
    'database_text_export': 'analysis/db-audit.json', 'omitted_from_compact': omitted,
    'model_weights_ELF_and_server_binaries_bundled': False,
    'note': 'Raw runner labels are preserved; read the actual retrieval/results and EVIDENCE.md for factual-support limits. The source inventory can include paths intentionally omitted from compact.',
})
dump(COMPACT / 'FILES.sha256.json', {
    'scope': 'Every compact file except this self-referential inventory',
    'files': [record(p, COMPACT) for p in files(COMPACT)],
})
for item in source_inventory:
    source = ROOT / item['path']
    assert source.stat().st_size == item['bytes'] and sha(source) == item['sha256'], item['path']

archives = []
for source, target, kind in ((ROOT, FULL_TAR, 'full'), (COMPACT, COMPACT_TAR, 'compact')):
    source_files = files(source)
    with tarfile.open(target, 'w:gz', compresslevel=6) as tar:
        for path in source_files:
            tar.add(path, arcname=str(Path(source.name) / path.relative_to(source)), recursive=False)
    with tarfile.open(target, 'r:gz') as tar:
        members = {m.name: m for m in tar.getmembers() if m.isfile()}
        assert len(members) == len(source_files)
        for path in source_files:
            arcname = str(Path(source.name) / path.relative_to(source))
            with tar.extractfile(members[arcname]) as f:
                archived_hash = hashlib.sha256(f.read()).hexdigest()
            assert archived_hash == sha(path), path
    archives.append({'kind': kind, 'path': str(target), 'bytes': target.stat().st_size,
                     'sha256': sha(target), 'file_count': len(source_files),
                     'all_archive_members_SHA256_verified_against_sources': True})
delivery = {
    'schema_version': 1, 'created_at_utc': datetime.now(timezone.utc).isoformat(),
    'run_id': summary['run_id'], 'source_root': str(ROOT), 'compact_root': str(COMPACT),
    'summary': str(ROOT / 'analysis/result-summary.json'), 'review_entry': str(COMPACT / 'EVIDENCE.md'),
    'source_head': summary['run_identity']['source_head'],
    'executable_sha256': summary['run_identity']['executable_sha256'], 'model': summary['model'],
    'runner_exit': summary['runner_exit'], 'runner_status': summary['strict_result'],
    'complete': summary['complete'], 'cleanup_ok': summary['cleanup_ok'],
    'arms': [{'condition': a['condition'], 'raw_runner_gate': a['raw_runner_gate_preserved'],
              'native_result_count': a['native_result_count'],
              'native_results_visible_to_provider_count': a['native_results_visible_to_provider_count'],
              'usage_and_timing': a['usage_and_timing']} for a in summary['arms']],
    'final_checkpoint': ready, 'archives': archives,
    'prior_rounds_or_product_modified': False, 'model_calls_or_resampling_by_analyzer': False,
}
dump(DELIVERY, delivery)
print(json.dumps(delivery, ensure_ascii=False, indent=2))
