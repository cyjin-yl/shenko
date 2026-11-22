from pathlib import Path
import gzip
import hashlib
import json
import re
import runpy

root = Path('/workspace/scratch/0e9f736f5ffa/shenko-timing/docs/evidence/daily-readiness-20261122/ci-nightly-c931526c')
sha = 'c931526c8ae8cd16f3e0cf56d733f1e4275d5bb0'


def read(name):
    data = (root / name).read_bytes()
    return gzip.decompress(data) if name.endswith('.gz') else data


def obj(name):
    return json.loads(read(name))


def save(name, value):
    (root / name).write_text(json.dumps(value, indent=2, ensure_ascii=False) + '\n')


ci = obj('ci/rust-summary.json')
nightly = obj('nightly/rust-summary.json')
compiler = obj('compiler/compiler-summary.json')
audits = {kind: obj(kind + '/rust-outer-audit.json') for kind in ('ci', 'nightly')}
runs = {kind: obj(kind + '/run.json') for kind in ('ci', 'nightly')}
jobs = {kind: obj(kind + '/jobs.json') for kind in ('ci', 'nightly')}
for kind, expected_count, event in [('ci', 6, 'pull_request'), ('nightly', 2, 'workflow_dispatch')]:
    assert runs[kind]['head_sha'] == sha and runs[kind]['event'] == event
    assert runs[kind]['status'] == 'completed' and runs[kind]['conclusion'] is not None
    assert jobs[kind]['total_count'] == len(jobs[kind]['jobs']) == expected_count
    assert all(job['head_sha'] == sha and job['run_id'] == runs[kind]['id']
               and job['status'] == 'completed' and job['conclusion'] is not None for job in jobs[kind]['jobs'])
    assert not any(audits[kind]['validation'].values())

# Recompute the accounting from the transferred original bytes with the same
# retained auditor; only the raw file location changes. This is not a test run.
audit_fn = runpy.run_path(str(root / 'collection/rust_outer_harness_audit.py'))['audit']
scratch = Path('/workspace/scratch/0e9f736f5ffa/observations/c931-ci-nightly-local-audit')
scratch.mkdir(exist_ok=True)
for kind in ('ci', 'nightly'):
    directory = scratch / kind
    directory.mkdir(exist_ok=True)
    path = directory / 'rust.log'
    path.write_bytes(read(kind + '/rust.log.gz'))
    computed = audit_fn(path, runs[kind]['id'], obj(kind + '/job.json')['id'], sha)
    for key in ('checkout', 'outer_summary', 'nested_summary', 'validation', 'targets'):
        assert computed[key] == audits[kind][key], (kind, key)
    original = audits[kind]['explicit_long_proofs']
    assert len(computed['explicit_long_proofs']) == len(original)
    for actual, expected in zip(computed['explicit_long_proofs'], original):
        if 'receipt' in actual:
            actual['receipt']['path'] = expected['receipt']['path']
        assert actual == expected

checkouts = {kind: [value for _, value in audit['checkout'] if re.fullmatch('[0-9a-f]{40}', value)]
             for kind, audit in audits.items()}
assert len(checkouts['ci']) == 1 and checkouts['ci'][0] != sha
assert checkouts['nightly'] == [sha]
proofs = nightly['explicit_long_proofs']
assert all(p['outer_result'] for p in proofs)
assert all(all(sha in text for _, text in p['source_revision_lines']) and p['source_revision_lines'] for p in proofs)
selection = obj('start-boundary/selection.json')
assert selection['source_revision'] == sha and len(selection['selected']) == 3
for row in selection['selected']:
    name = 'start-boundary/' + row['path']
    if not (root / name).exists():
        name += '.gz'
    data = read(name)
    assert len(data) == row['bytes'] and hashlib.sha256(data).hexdigest() == row['sha256']
fixture = obj('start-boundary/start-boundary/result.json')
order = obj('start-boundary/start-boundary/recovery-stop-order.json')
assert fixture['source_revision'] == sha
assert fixture['model_completion_tested'] is False and fixture['remote_completion_claimed'] is False
browser = obj('compiler/compiler-browser.json')
assert browser['candidate'] == sha and browser['status'] == 'passed' and browser['uid'] != 0 and browser['chromiumSandbox']
assert compiler['candidate'] == sha and compiler['job_conclusion'] is not None
for name, item in compiler['selected'].items():
    filename = Path(item['path']).name
    stored = 'compiler/' + filename
    if not (root / stored).exists():
        stored += '.gz'
    raw = read(stored)
    assert len(raw) == item['bytes'] and hashlib.sha256(raw).hexdigest() == item['sha256'], name


def job_values(kind):
    keys = ('id', 'name', 'head_sha', 'status', 'conclusion', 'started_at', 'completed_at', 'html_url')
    return [{key: job.get(key) for key in keys} for job in jobs[kind]['jobs']]


summary = {
    'schema_version': 1, 'source_revision': sha,
    'scope': 'Completed ordinary CI and full Nightly only; independent of real-model task-journey acceptance.',
    'regression_gates_passed': all(run['conclusion'] == 'success' for run in runs.values()), 'real_model_journey_included': False,
    'ci': {'conclusion': runs['ci']['conclusion'], 'run_id': ci['run_id'], 'url': ci['run_url'], 'event': ci['event'], 'run_attempt': ci['run_attempt'],
           'jobs': job_values('ci'), 'actual_checkout': checkouts['ci'][0],
           'checkout_kind': 'GitHub pull-request synthetic merge; exact checkout hash is taken from the original Rust job log',
           'command': audits['ci']['command'][1], 'outer_workspace': ci['outer_summary'], 'nested_subprocesses': ci['nested_summary']},
    'nightly': {'conclusion': runs['nightly']['conclusion'], 'run_id': nightly['run_id'], 'url': nightly['run_url'], 'event': nightly['event'],
                'run_attempt': nightly['run_attempt'], 'jobs': job_values('nightly'), 'actual_checkout': checkouts['nightly'][0],
                'command': audits['nightly']['command'][1], 'outer_workspace': nightly['outer_summary'],
                'nested_subprocesses': nightly['nested_summary'], 'explicit_long_proofs': proofs,
                'helper_preparation_counted_as_test': False, 'retried_or_cancelled_by_collector': False},
    'compiler': {'job_id': compiler['job_id'], 'run_id': compiler['run_id'], 'job_conclusion': compiler['job_conclusion'],
                 'upstream_pin': browser['upstream']['commit'],
                 'failure': {'file': 'packages/compiler/test/scroll-reveal-consumers.test.ts', 'line': 155,
                             'profile': 'react-dom-source-v1', 'case': 'rejects an item owned by another surface, a sibling, self and another family before physical movement',
                             'assertion': 'surface.scrollTop', 'expected': 50, 'observed': 300,
                             'event_loop_cause_established_by_this_receipt': False},
                 **compiler['selected']['evidence/compiler-vitest.log']['parsed'],
                 'artifact_id': compiler['artifact_id'], 'artifact_bytes': compiler['artifact_bytes'],
                 'artifact_sha256': compiler['artifact_sha256'], 'artifact_zip_downloaded_and_hash_verified': True,
                 'artifact_zip_copied_to_this_directory': False, 'complete_inputs_json_retained': True,
                 'browser_receipt': 'compiler/compiler-browser.json'},
    'selected_process_fixture': {'test': 'public_start_pause_survives_process_reopen_and_resume_alone_admits_inference',
                                'result': fixture, 'order': order, 'verification': selection,
                                'earlier_failures_reclassified': False,
                                'scope_limit': 'Controlled process fixture; neither model completion nor deployment shutdown latency.'},
    'dispatch': {'attempt': obj('dispatch/dispatch-attempt.json'), 'result': obj('dispatch/dispatch-result.json'),
                 'first_confirmation': obj('dispatch/dispatch-confirmation.json'),
                 'subsequently_observed_run_id': obj('dispatch/dispatch-run-observed.json')['id'],
                 'original_empty_confirmation_preserved': True},
    'observation': {'start': obj('observation/observer-start.json'), 'finish': obj('observation/observer-finished.json')},
    'counting_method': {'description': ci['method'], 'nested_results_added_to_outer': False,
                        'explicit_long_proofs_added_to_workspace_count': False,
                        'harness_false_skip_counted_as_pass': False,
                        'local_reaudit_matches_transferred_original_logs': True,
                        'audit_script_sha256': hashlib.sha256(read('collection/rust_outer_harness_audit.py')).hexdigest()},
    'retention_limits': ['Native ZIP digest is GitHub metadata only; only three listed members and the central directory were range-read.',
                        'Compiler ZIP was independently fully downloaded and SHA256-checked remotely; selected original members, including complete inputs.json, are retained here.',
                        'Earlier failed candidates and actual model-journey failures remain separate; no outcome is replaced by these regression results.',
                        'This evidence does not execute or validate a later source candidate.'],
}
save('summary.json', summary)
print(json.dumps({'source_revision': sha, 'ci_outer': ci['outer_summary'], 'nightly_outer': nightly['outer_summary'],
                  'explicit_long_proofs': len(proofs), 'explicit_nested_results': sum(len(p['nested_results']) for p in proofs),
                  'compiler': summary['compiler']['suites'], 'local_reaudit': 'matched'}))
