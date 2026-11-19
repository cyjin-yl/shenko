from pathlib import Path
import hashlib
import json
import subprocess

REPO = Path('/workspace/scratch/faaa3ccca531/shenko-review-retention-integration')
OUT = Path(__file__).parent
SOURCE = '4394226a443f25be48791fecea918d619490a284'
READER = 'a57aee9be5445dee27eb81ad2d04f90186f951f9'
EXPORT = 'a59753d624434c17afc359df18da2b3c6e6ca490'

def git(*args):
    return subprocess.check_output(['git', *args], cwd=REPO)

def blob(revision, path):
    return git('show', f'{revision}:{path}')

assert git('rev-parse', 'HEAD').decode().strip() == SOURCE
assert git('status', '--porcelain=v1') == b''
path = 'crates/shenko-update/src/handoff_tests.rs'
start = b'        let retained = reopened.retained_object_snapshot(shenko_store::RetentionLimits::default())?;\n'
end = b'        drop(reopened);\n'
reader_tests = blob(READER, path)
merged_tests = blob(SOURCE, path)
first = reader_tests.index(start)
block = reader_tests[first:reader_tests.index(end, first)]
assert merged_tests.count(block) == 1
assert merged_tests.replace(block, b'', 1) == blob(EXPORT, path)
assert merged_tests.index(end, merged_tests.index(start)) < merged_tests.index(
    b'        evidence.closed_installation(&dir);', merged_tests.index(start))
reader_files = [
    'crates/shenko-store/src/retained_objects.rs',
    'crates/shenko-store/src/retained_objects/artifacts.rs',
    'crates/shenko-store/src/retained_objects/carriers.rs',
    'crates/shenko-store/src/retained_objects/closure.rs',
    'crates/shenko-store/src/cas.rs', 'crates/shenko-store/src/store.rs',
    'crates/shenko-store/src/lib.rs', 'crates/shenko-store/tests/retained_objects.rs',
    'bin/shenko/src/lib.rs', 'bin/shenko/src/cli_dispatch.rs',
    'bin/shenko/tests/native_evidence_delivery.rs',
]
export_files = [
    'crates/shenko-update/src/test_evidence.rs',
    'crates/shenko-update/src/build_tests.rs', 'crates/shenko-update/src/lib.rs',
    '.github/workflows/ci.yml',
]
comparisons = []
for original, paths in [(READER, reader_files), (EXPORT, export_files)]:
    for name in paths:
        observed = blob(SOURCE, name)
        equal = observed == blob(original, name)
        assert equal, name
        comparisons.append({'path': name, 'original_source': original, 'byte_identical': equal,
                            'bytes': len(observed), 'sha256': hashlib.sha256(observed).hexdigest()})
result = subprocess.run(['git', 'diff', EXPORT, SOURCE, '--', path], cwd=REPO, capture_output=True)
(OUT / 'handoff-merge.diff').write_bytes(result.stdout)
(OUT / 'handoff-merge.stderr').write_bytes(result.stderr)
assert result.returncode == 0
check = subprocess.run(['git', 'diff', EXPORT, SOURCE, '--check', '--', path], cwd=REPO, capture_output=True)
(OUT / 'diff-check.stdout').write_bytes(check.stdout)
(OUT / 'diff-check.stderr').write_bytes(check.stderr)
assert check.returncode == 0
report = {
    'source': SOURCE, 'tree': git('rev-parse', 'HEAD^{tree}').decode().strip(),
    'reader_source': READER, 'export_source': EXPORT,
    'merged_handoff_equals_export_plus_exact_reader_block': True,
    'reader_block_bytes': len(block), 'reader_block_sha256': hashlib.sha256(block).hexdigest(),
    'all_original_export_bytes_and_assertions_preserved': True,
    'all_original_reader_block_bytes_and_assertions_preserved': True,
    'closed_installation_after_original_reopened_drop': True,
    'byte_comparisons': comparisons, 'git_diff_check_exit': check.returncode,
    'new_behavioral_windows': 0, 'source_edits': False,
    'checks_not_executed': ['Rust compilation', 'Clippy', 'runtime or exporter execution'],
    'readonly_call_path_review': [
        'run_owned creates/acquires installation ownership only for Command::Start',
        'runtime_lifecycle pre-dispatch services do not open this Store or prepare its home',
        'Check retained_objects branch returns before ordinary open_installation_store/bootstrap prepare',
        'resolve_live_slot reads/validates convenience pointer and existing paths only',
        'inspect_retained_objects opens existing CAS and private stable DB/WAL tuple; catalogue is separate in-memory SQLite',
        'retained_object_snapshot read transaction ends before fixture Store is dropped and copied',
    ],
    'limits': 'Source composition and read-only path audit; no assertion of executed compile, whole-installation GC or atomic CAS deletion exclusion.',
    'worktree_status': git('status', '--porcelain=v1').decode(),
}
(OUT / 'observations.json').write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
print(json.dumps({key: report[key] for key in ['source', 'tree', 'merged_handoff_equals_export_plus_exact_reader_block', 'git_diff_check_exit', 'new_behavioral_windows', 'worktree_status']}, indent=2))
