#!/usr/bin/env python3
"""Package completed round-two evidence; never runs or mutates a model or Store."""
import hashlib
import json
import shutil
import tarfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent
COMPACT = ROOT.with_name(ROOT.name + '-compact')
FULL_TAR = ROOT.with_name(ROOT.name + '-full-evidence.tar.gz')
COMPACT_TAR = ROOT.with_name(ROOT.name + '-compact-evidence.tar.gz')
DELIVERY = ROOT.with_name(ROOT.name + '-delivery-index.json')


def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b''):
            h.update(chunk)
    return h.hexdigest()


def dump(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')


def files(root):
    return sorted(p for p in root.rglob('*') if p.is_file() and '__pycache__' not in p.parts)


def record(path, root):
    return {'path': str(path.relative_to(root)), 'bytes': path.stat().st_size, 'sha256': sha(path)}


summary = json.loads((ROOT / 'analysis/result-summary.json').read_text())
assert summary['complete'] is False and summary['cleanup_ok'] is True
assert summary['operator_interrupted'] is False and summary['protocol_rejection_count'] == 14
required_final_files = ['model-service/service-final-observation.json', 'model-service/logs/server-context16384.log']
for name in required_final_files:
    assert (ROOT / name).is_file(), name
final_service_hashes = [record(ROOT / name, ROOT) for name in required_final_files]

inventory_path = ROOT / 'analysis/inventory.sha256.json'
source_inventory = [record(p, ROOT) for p in files(ROOT) if p != inventory_path]
dump(inventory_path, {'scope': 'All full-evidence regular files excluding __pycache__ and this self-referential inventory',
                      'files': source_inventory})

# The compact copy mirrors relative paths so result-summary and CAS-index paths remain useful.
COMPACT.mkdir(exist_ok=False)
included = []
omitted = []
for path in files(ROOT):
    relative = path.relative_to(ROOT)
    is_acceptance = relative.parts[0] == 'acceptance'
    is_cas = is_acceptance and 'cas' in relative.parts
    text_acceptance = path.suffix in ('.json', '.jsonl', '.txt', '.md', '.log')
    include = not is_acceptance or is_cas or text_acceptance
    if include:
        target = COMPACT / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, target)
        assert sha(target) == sha(path)
        included.append(str(relative))
    else:
        omitted.append(record(path, ROOT))

cas_index = json.loads((ROOT / 'analysis/cas-index.json').read_text())
dump(COMPACT / 'CAS-LOCATORS.json', {
    'note': 'Original locators inside receipts are unchanged. Resolve relocated CAS bytes through this exact mapping.',
    'files': [{'original_locator': (ROOT / item['path']).as_uri(),
               'portable_path': item['path'], 'condition': item['condition'],
               'bytes': item['bytes'], 'sha256': item['sha256'],
               'declared_digest': item['declared_digest_from_filename']} for item in cas_index],
})
dump(COMPACT / 'INDEX.json', {
    'schema_version': 1, 'created_at_utc': datetime.now(timezone.utc).isoformat(),
    'round': 2, 'run_id': summary['run_id'], 'source_head': summary['identity']['source_head'],
    'executable_sha256': summary['identity']['executable_sha256'],
    'strict_result': summary['strict_result'], 'complete': False, 'cleanup_ok': True,
    'operator_interrupted': False, 'entrypoint': 'EVIDENCE.md',
    'machine_readable_summary': 'analysis/result-summary.json',
    'source_root': str(ROOT), 'all_original_CAS_included': True, 'CAS_count': len(cas_index),
    'database_row_export': 'analysis/db-audit.json',
    'original_database_and_installation_binary_files': 'Full archive only',
    'model_ELF_and_server_binaries_bundled': False,
    'final_service_observation_and_log': final_service_hashes,
    'omitted_from_compact': omitted,
    'note': 'Compact inventory covers copied bytes plus compact metadata. analysis/inventory.sha256.json covers the full source tree and may list paths intentionally omitted here.',
})
compact_files = [record(p, COMPACT) for p in files(COMPACT)]
dump(COMPACT / 'FILES.sha256.json', {'scope': 'Every compact file except this inventory', 'files': compact_files})

for item in source_inventory:
    original = ROOT / item['path']
    assert original.stat().st_size == item['bytes'] and sha(original) == item['sha256'], item['path']

archives = []
for source, target in ((ROOT, FULL_TAR), (COMPACT, COMPACT_TAR)):
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
                archived_sha = hashlib.sha256(f.read()).hexdigest()
            assert archived_sha == sha(path), path
    archives.append({'kind': 'full' if source == ROOT else 'compact', 'path': str(target),
                     'bytes': target.stat().st_size, 'sha256': sha(target),
                     'file_count': len(source_files), 'all_members_sha256_verified_against_sources': True})

delivery = {
    'schema_version': 1, 'created_at_utc': datetime.now(timezone.utc).isoformat(),
    'round': 2, 'source_root': str(ROOT), 'compact_root': str(COMPACT),
    'summary': str(ROOT / 'analysis/result-summary.json'),
    'compact_review_entry': str(COMPACT / 'EVIDENCE.md'),
    'source_head': summary['identity']['source_head'],
    'executable_sha256': summary['identity']['executable_sha256'],
    'strict_result': summary['strict_result'], 'complete': False, 'cleanup_ok': True,
    'operator_interrupted': False, 'service_exit_cause': 'unknown',
    'protocol_rejection_count': 14,
    'observed_token_subtotals': {a['condition']: a['usage_and_timing']['observed_token_subtotal'] for a in summary['arms']},
    'candidate_final_failed_request_usage': None,
    'final_service_observation_and_log': final_service_hashes,
    'archives': archives,
    'first_round_modified_by_this_packager': False,
    'no_model_calls_or_resampling': True,
}
dump(DELIVERY, delivery)
print(json.dumps(delivery, ensure_ascii=False, indent=2))
