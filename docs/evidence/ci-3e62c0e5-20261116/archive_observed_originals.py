#!/usr/bin/env python3
"""Archive already observed CI originals; never execute or replay product work."""
from pathlib import Path
import datetime
import gzip
import hashlib
import json
import re
import shutil
import subprocess
import zipfile

ROOT = Path('/workspace/scratch/faaa3ccca531')
SRC = ROOT / 'ci-3e62c0e5-43968540714'
REPO = ROOT / 'shenko'
DST = REPO / 'docs/evidence/ci-3e62c0e5-20261116'
HEAD = '3e62c0e517a246872101cd1e61a694b2156bc919'

def sha(data):
    return hashlib.sha256(data).hexdigest()

def write_json(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + '\n')

def copy(source, target):
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, target)
    assert source.read_bytes() == target.read_bytes()

def compressed(source, target):
    raw = source.read_bytes()
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(gzip.compress(raw, mtime=0))
    assert gzip.decompress(target.read_bytes()) == raw

def unwrap(path):
    obj = json.loads(path.read_text())
    while isinstance(obj, dict) and 'structuredContent' in obj:
        obj = obj['structuredContent']
    if isinstance(obj, dict) and isinstance(obj.get('content'), str):
        obj = json.loads(obj['content'])
    return obj

DST.mkdir(exist_ok=True)
for source in sorted(SRC.glob('*.tool.json')):
    compressed(source, DST / 'connector-originals' / (source.name + '.gz'))
for source in sorted(SRC.glob('*.job.original.log')):
    copy(source, DST / source.name)
for source in sorted(SRC.glob('*.download-verification.json')):
    copy(source, DST / source.name)
for name in ['archive-transport-failure.json', 'clippy-derived-summary.json']:
    copy(SRC / name, DST / name)

mapping = {
    'final-run.tool.json': 'run.api.json',
    'jobs-final.tool.json': 'jobs.api.json',
    'final-artifacts.tool.json': 'artifacts.api.json',
    'actual-checkout.tool.json': 'actual-checkout.api.json',
    'requested-head.tool.json': 'requested-head.api.json',
}
for source, target in mapping.items():
    write_json(DST / target, unwrap(SRC / source))
run = unwrap(SRC / 'final-run.tool.json')
assert run['head_sha'] == HEAD and run['conclusion'] == 'failure'
checkout = unwrap(SRC / 'actual-checkout.tool.json')
head = unwrap(SRC / 'requested-head.tool.json')
assert checkout['tree']['sha'] == head['tree']['sha']
artifacts = unwrap(SRC / 'final-artifacts.tool.json')['artifacts']
inventory = []
for item in artifacts:
    path = SRC / (item['name'] + '.original.zip')
    raw = path.read_bytes()
    assert len(raw) == item['size_in_bytes']
    assert 'sha256:' + sha(raw) == item['digest']
    copy(path, DST / path.name)
    with zipfile.ZipFile(path) as archive:
        assert archive.testzip() is None
        members = [{'path': x.filename, 'bytes': x.file_size, 'crc32': f'{x.CRC:08x}',
                    'sha256': sha(archive.read(x))} for x in archive.infolist()]
    inventory.append({'artifact_id': item['id'], 'file': path.name, 'bytes': len(raw),
                      'sha256': sha(raw), 'members': members, 'crc_ok': True})
write_json(DST / 'artifact-inventory.derived.json', inventory)

for name in ['workspace-test.log', 'workspace-check.log']:
    copy(SRC / 'rust-evidence' / name, DST / name)
copy(SRC / 'clippy-ratchet-evidence/clippy/ratchet.json', DST / 'clippy-ratchet.json')
copy(SRC / 'clippy-ratchet-evidence/base.json', DST / 'clippy-base.json')
copy(SRC / 'web-rebuild-evidence/comparison.json', DST / 'web-comparison.json')
for path in sorted((SRC / 'rust-evidence/native-e1').rglob('*')):
    if path.is_file():
        copy(path, DST / 'native-e1' / path.relative_to(SRC / 'rust-evidence/native-e1'))

test_log = (SRC / 'rust-evidence/workspace-test.log').read_text()
pattern = r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out.*'
summaries = []
for line in test_log.splitlines():
    match = re.search(pattern, line)
    if match:
        summaries.append({'original_line': line, 'status': match[1],
            **dict(zip(['passed', 'failed', 'ignored', 'measured', 'filtered_out'], map(int, match.groups()[1:])))})
counts = {key: sum(row[key] for row in summaries) for key in ['passed', 'failed', 'ignored', 'measured', 'filtered_out']}
assert len(summaries) == 152 and counts == dict(passed=1094, failed=0, ignored=11, measured=0, filtered_out=90)
focus = ['original_native_e1_arrives_over_authenticated_ssh_without_t2_or_reexecution',
    'original_pty_writes_observe_flush_cancel_errors_and_channel_loss',
    'redirected_cargo_rebuild_keeps_the_actual_artifact_and_failed_attempt',
    'two_real_matching_cargo_targets_are_refused_as_ambiguous',
    'overflowing_real_compiler_output_is_stopped_and_never_an_artifact',
    'prepare_keeps_its_build_observation_in_raw_history_and_the_recovery_closure']
write_json(DST / 'test-summaries.derived.json', {'source': 'workspace-test.log', 'count': len(summaries),
    'totals': counts, 'summaries': summaries,
    'selected_original_result_lines': [line for line in test_log.splitlines() if any(name in line for name in focus)],
    'raw_receipt_retention_gap': 'Successful PTY eprintln was libtest-captured; build and handoff temporary originals were deleted by their fixtures. These assertions passed, but their missing raw originals cannot be reconstructed from this run.'})

nested = SRC / 'rust-evidence/native-e1/020-native-e1-originals.zip'
with zipfile.ZipFile(nested) as archive:
    assert archive.testzip() is None
    original_inventory = json.loads(archive.read('inventory.json'))
    for record in original_inventory:
        raw = archive.read(record['member'])
        assert len(raw) == record['bytes'] and sha(raw) == record['sha256']
    manifest = json.loads(archive.read('manifest.json'))
    sides = {}
    text_count = 0
    for side in manifest['stores']:
        ledger = json.loads(archive.read(side + '/ledger-rows.json'))
        for rows in ledger['tables'].values():
            for row in rows:
                for key, expected in row.get('text_sha256', {}).items():
                    assert sha(row['values'][key].encode()) == expected
                    text_count += 1
        references = json.loads(archive.read(side + '/cas-references.json'))
        sides[side] = {
            'scope': ledger['scope'],
            'row_counts': {key: len(value) for key, value in ledger['tables'].items()},
            'branch_head_original_data': [json.loads(row['values']['data']) for row in ledger['tables']['subject_branch_heads']],
            'reconciliation_original_data': [json.loads(row['values']['data']) for row in ledger['tables']['reconciliation_cases']],
            'seal_debt_original_data': [json.loads(row['values']['data']) for row in ledger['tables']['native_evidence_seal_debts']],
            'missing_references': [entry for entry in references['references'] if 'missing' in entry],
            'excluded_digest_count': len(references['excluded_digests']),
            'untyped_digest_count': len(references['untyped_digests']),
        }
    effects = archive.read('physical-effects.raw')
    assert effects == b'effect\neffect\n'
    validation = {
        'source': 'native-e1/020-native-e1-originals.zip', 'bytes': nested.stat().st_size,
        'sha256': sha(nested.read_bytes()), 'members': len(archive.infolist()), 'crc_ok': True,
        'original_inventory_entries_verified': len(original_inventory), 'original_text_hashes_verified': text_count,
        'blake3_recomputed_locally': False,
        'blake3_limit': 'The local Python environment has no blake3 module; SHA-256/length/CRC and original text digests are independently checked. This is not a new product execution.',
        'original_manifest': manifest, 'physical_effects_hex': effects.hex(), 'physical_effect_lines': 2,
        'stores': sides,
    }
    (DST / 'native-e1/nested-manifest.original.json').write_bytes(archive.read('manifest.json'))
write_json(DST / 'native-e1-originals-validation.derived.json', validation)

paths = [
    '.github/workflows/ci.yml', 'scripts/ci/clippy_ratchet.py',
    'crates/shenko-exec-ssh/src/seat/ssh_pty/pump.rs',
    'crates/shenko-exec-ssh/src/native_transport/tests/pty_write.rs',
    'crates/shenko-update/src/build.rs', 'crates/shenko-update/src/build_tests.rs',
    'crates/shenko-update/src/handoff_tests.rs',
    'crates/shenko-store/src/native_evidence_successor.rs',
    'bin/shenko/tests/native_evidence_delivery.rs',
]
blobs = []
for path in paths:
    result = subprocess.run(['git', 'show', HEAD + ':' + path], cwd=REPO, capture_output=True, check=True)
    data = result.stdout
    blob = subprocess.check_output(['git', 'rev-parse', HEAD + ':' + path], cwd=REPO, text=True).strip()
    target = DST / 'source-blobs' / (blob + '.blob')
    target.parent.mkdir(exist_ok=True)
    target.write_bytes(data)
    assert hashlib.sha1(b'blob ' + str(len(data)).encode() + b'\0' + data).hexdigest() == blob
    blobs.append({'path': path, 'git_blob': blob, 'bytes': len(data), 'sha256': sha(data),
                  'retained_as': str(target.relative_to(DST))})
write_json(DST / 'source-blobs.json', {'source_head': HEAD, 'actual_checkout': checkout['sha'],
    'tree': head['tree']['sha'], 'files': blobs})
print(json.dumps({'archive': str(DST), 'artifact_members': sum(len(x['members']) for x in inventory),
    'native_members': sum(p.is_file() for p in (SRC / 'rust-evidence/native-e1').rglob('*')),
    'nested_members': validation['members'], 'nested_inventory_verified': len(original_inventory),
    'text_hashes_verified': text_count, 'tests': counts, 'product_rerun': False}))
