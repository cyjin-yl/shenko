from pathlib import Path
from collections import Counter
import hashlib
import json

BASE = Path('/tank/shared/projects/shenko/build/daily-readiness-20261122')
METRICS = ('input_in_first_seal', 'first_result_committed_without_r0', 's1_requested_children_admitted')

def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def read(path):
    return json.loads(path.read_text())

def require(ok, label):
    if not ok:
        raise AssertionError(label)

report = {'schema_version': 1, 'scope': 'read-only audit of frozen first-request evidence; no model or build invocation', 'groups': [], 'issues': []}
manifests = []
pair_contents = {}
for name in ('protocol-before-r3', 'protocol-after-r1'):
    folder = BASE / name
    batch = read(folder / 'batch-summary.json')
    group = {'directory': name, 'batch_summary_sha256': sha(folder / 'batch-summary.json'),
             'source_revision': batch['source_revision'], 'binary_sha256': batch['binary_sha256'],
             'finished_at': batch['finished_at'], 'case_count': len(batch['runs']),
             'cases_file_sha256': sha(folder / 'cases.json'), 'model_card': batch['model_card'],
             'batch_rates': batch['rates'], 'cases': []}
    report['groups'].append(group)
    require(batch['requested_runs'] == batch['attempted_runs'] == batch['completed_observations'] == len(batch['runs']) == 8, name + ' denominator')
    require(batch['unattempted_runs'] == 0 and not batch['errors'], name + ' incomplete batch')
    require(group['cases_file_sha256'] == batch['cases_file_sha256'], name + ' cases hash')
    for entry in batch['runs']:
        case_folder = folder / entry['directory']
        item = {'case_id': entry['case_id']}
        group['cases'].append(item)
        try:
            summary = read(case_folder / 'summary.json')
            obs = read(case_folder / 'first.observation.json')
            facts = read(case_folder / 'first.ledger.json')
            final = read(case_folder / 'final.ledger.json')
            manifest = read(case_folder / 'manifest.json')
            plan = read(case_folder / 'first.input-plan.json')
            result = read(case_folder / 'first.request-result.json')
            submission = read(case_folder / 'submission.json')
            messages = [json.loads(line) for line in (case_folder / 'first.websocket.jsonl').read_text().splitlines()]
            require(sha(case_folder / 'summary.json') == entry['summary_sha256'], 'summary hash')
            require(summary['checks'] == entry['checks'] == obs['checks'], 'projected checks differ')
            require(summary['case_id'] == manifest['case']['id'] == entry['case_id'], 'case identity')
            require(manifest['binary_sha256'] == batch['binary_sha256'] and manifest['source_revision'] == batch['source_revision'], 'binary binding')
            for model_file in ('models.json', 'models.after.json'):
                cards = [c for c in read(case_folder / model_file)['data'] if c.get('id') == batch['model']]
                require(len(cards) == 1 and {k: v for k, v in cards[0].items() if k not in {'permission', 'created'}} == batch['model_card'], 'observed model card binding')
            require(manifest['content'] == submission['content'] and hashlib.sha256(submission['content'].encode()).hexdigest() == manifest['content_sha256'], 'submitted content')
            pair_contents.setdefault(entry['case_id'], []).append(manifest['content_sha256'])
            manifests.append(manifest)
            for f in ('first.paused.ledger.json', 'first.accepted.ledger.json'):
                require(not read(case_folder / f)['subject_request_attempts'], 'request before accepted resume')
            attempt = min((r['data'] for r in facts['subject_request_attempts']), key=lambda r: r['created_commit']['seq'])
            step = next(r['data'] for r in facts['subject_steps'] if r['step_id'] == attempt['step_id'])
            require(attempt['request_id'] == obs['request_id'] == summary['request_id'] == result['request_id'], 'first request identity')
            require(step['step_id'] == obs['step_id'] and step['state'] == obs['step_state'], 'first step identity')
            require(step['state'] in ('committed', 'retired') and attempt['terminal'], 'unsettled first boundary')
            final_first = min((r['data'] for r in final['subject_request_attempts']), key=lambda r: r['created_commit']['seq'])
            require(final_first == attempt, 'first attempt changed or replaced in shutdown tail')
            require(plan['request_id'] == attempt['request_id'] and plan['step_id'] == step['step_id'], 'first plan binding')
            source_by_file = {r['file']: r for r in obs['sources']}
            require(source_by_file['first.input-plan.json']['reference'] == attempt['input_plan_ref'], 'plan source reference')
            require(source_by_file['first.request-result.json']['reference'] == attempt['terminal']['result_ref'], 'result source reference')
            for field, filename in (('model_result_ref', 'first.output.txt'), ('model_outcome_ref', 'first.model-outcome.json'), ('decision_ref', 'first.decision.json'), ('retry_reason_ref', 'first.rejection.json')):
                if step.get(field):
                    require(source_by_file[filename]['reference'] == step[field], 'step source reference')
            cas_manifest = read(case_folder / 'cas-sha256.json')
            for source in obs['sources']:
                retained = case_folder / source['file']
                ref = source['reference']
                d = ref['digest'].removeprefix('blake3:')
                relative = f'{d[:2]}/{d[2:4]}/{d}'
                require(sha(retained) == source['sha256'] == cas_manifest[relative], 'retained source hash')
                require(retained.stat().st_size == ref['bytes'], 'retained source size')
                require(retained.read_bytes() == (case_folder / 'cas' / relative).read_bytes(), 'retained CAS source bytes')
            receipts = [r for r in messages if r.get('type') == 'receipt' and r.get('stage') == 'accepted' and r.get('submission_id') == submission['submission_id']]
            require(bool(receipts), 'no accepted wire receipt')
            accepted = receipts[0]
            ledger, seq = accepted['accepted_commit'].rsplit('/', 1)
            require(accepted['schema_version'] == 2 and accepted['durability'] == 'durable' and accepted['mode'] == submission['mode'] == 'append' and accepted['replayed'] is False, 'acceptance contract')
            require(accepted['placement']['requested'] == 'append' and accepted['placement']['actual'] == 'next_boundary', 'accepted placement')
            input_bound = (attempt['created_commit']['ledger_id'] == ledger and attempt['created_commit']['seq'] > int(seq)
                and any(e.get('event_id') == accepted['event_id'] and e.get('payload', {}).get('submission_id') == submission['submission_id'] and e.get('payload', {}).get('content') == submission['content'] for e in plan['events'])
                and any(submission['content'] in p.get('text', '') for m in plan['payload']['messages'] if m['role'] == 'user' for p in m['parts'] if p.get('type') == 'text'))
            binding = plan['provider_binding']
            effective = result['effective']
            same_configuration = (binding['kind'] == 'local' and binding['model'] == batch['model'] and binding['base_url'] == batch['model_url']
                and plan['payload']['max_tokens'] == effective['max_tokens'] == 4096
                and plan['payload'].get('native_options', {}) == effective['native_options'] == {}
                and effective['provider_binding'] == binding and effective['profile_id'] == 'local.openai.chat_completions'
                and effective['profile_revision'] == binding['serializer_revision'] and effective['api_version'] is None)
            require(same_configuration == obs['comparison_valid'] == summary['comparison_valid'] == entry['comparison_valid'] is True, 'sealed/effective configuration')
            require(hashlib.sha256(plan['payload']['system_prompt'].encode()).hexdigest() == obs['system_prompt_sha256'] == summary['system_prompt_sha256'], 'system prompt hash')
            decision = read(case_folder / 'first.decision.json') if step.get('decision_ref') else None
            terminal = result['termination']
            committed = bool(step['state'] == 'committed' and step.get('commit_ref') and decision and decision['step_id'] == step['step_id']
                and result['status'] == 'success' and terminal['kind'] == 'provider_terminal' and terminal['length'] is False and terminal['stop_reason'] in ('stop', 'end_turn', 'stop_sequence'))
            intents = decision.get('delegated_task_intents', []) if decision else []
            model = [r for r in intents if (r.get('definition') or {}).get('kind') == 'model.child']
            native = [r for r in intents if (r.get('definition') or {}).get('kind') == 'host.printf']
            children = committed and len(intents) == 2 and len(model) == len(native) == 1
            if children:
                spec = model[0]['definition']['input']
                child_binding = spec['provider_binding']
                tasks = {r['task_id']: r['data'] for r in facts['tasks']}
                children = (spec['spec']['model'] == batch['model'] and child_binding['kind'] == 'local'
                    and child_binding['model'] == batch['model'] and child_binding['base_url'] == batch['model_url']
                    and native[0]['definition']['definition_version'] == 1 and native[0]['definition']['input']['text'] == manifest['case']['marker']
                    and all(i['intent_id'] in tasks and tasks[i['intent_id']]['created_by_step_id'] == step['step_id']
                        and tasks[i['intent_id']]['created_commit'] == step['commit_ref'] and tasks[i['intent_id']]['input_ref'] == step['decision_ref']
                        and tasks[i['intent_id']]['kind'] == i['definition']['kind'] for i in intents))
            actual = dict(zip(METRICS, (input_bound, committed, children)))
            require(actual == obs['checks'], 'independently reconstructed metric differs')
            shutdown = summary['shutdown']
            strict = all(actual.values()) and same_configuration and summary['evidence_retained'] and not summary['errors']
            require(strict == summary['protocol_passed'] == entry['protocol_passed'], 'strict verdict')
            require(shutdown['group_cleanup']['complete'] is True and summary['cleanup_complete'] is True, 'incomplete owned group')
            item.update(verified=True, request_id=attempt['request_id'], step_state=step['state'], raw_checks=actual,
                protocol_passed=strict, parent_exit=shutdown['parent_exit'], parent_timed_out=shutdown['parent_timed_out'], clean=shutdown['clean'],
                summary_sha256=sha(case_folder/'summary.json'), observation_sha256=sha(case_folder/'first.observation.json'),
                output_sha256=sha(case_folder/'first.output.txt') if (case_folder/'first.output.txt').exists() else None,
                final_request_count=len(final['subject_request_attempts']))
            if not committed:
                item['rejection'] = obs['rejection']
                item['termination'] = terminal
        except Exception as error:
            item.update(verified=False, error=f'{type(error).__name__}: {error}')
            report['issues'].append({'group': name, 'case_id': entry['case_id'], 'error': item['error']})
    group['reconstructed_raw_pass_counts'] = {k: sum(r.get('raw_checks', {}).get(k, False) for r in group['cases']) for k in METRICS}
    group['reconstructed_strict_passes'] = sum(r.get('protocol_passed', False) for r in group['cases'])
    group['parent_exit_counts'] = dict(Counter(str(r.get('parent_exit')) for r in group['cases']))
    group['parent_timed_out_count'] = sum(r.get('parent_timed_out', False) for r in group['cases'])
report['paired_content_identical'] = all(len(values) == 2 and values[0] == values[1] for values in pair_contents.values()) and len(pair_contents) == 8
keys = ('driver_sha256', 'journey_helper_sha256', 'model', 'model_url', 'websockets_version', 'configured_local_request_timeout_ms', 'startup_timeout_seconds', 'phase_timeout_seconds', 'observer_socket', 'request_path_timing')
report['shared_manifest_configuration'] = {key: json.loads(next(iter({json.dumps(m[key], sort_keys=True) for m in manifests}))) for key in keys if len({json.dumps(m[key], sort_keys=True) for m in manifests}) == 1}
report['manifest_configuration_equal'] = len(report['shared_manifest_configuration']) == len(keys) and len(manifests) == 16
report['model_cards_equal'] = report['groups'][0]['model_card'] == report['groups'][1]['model_card']
print('AB_AUDIT_BEGIN')
print(json.dumps(report, ensure_ascii=False))
print('AB_AUDIT_END')
