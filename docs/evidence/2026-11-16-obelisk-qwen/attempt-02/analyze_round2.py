#!/usr/bin/env python3
"""Analyze retained round-two evidence. No model calls, Store writes, or resampling."""
import base64
import collections
import gzip
import hashlib
import json
import sqlite3
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent
OUT = ROOT / 'analysis'
OUT.mkdir(exist_ok=True)


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def load(path):
    return json.loads(path.read_bytes())


def dump(name, value):
    (OUT / name).write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')


def rel(path):
    return str(path.relative_to(ROOT))


def file_record(path):
    raw = path.read_bytes()
    return {'path': rel(path), 'bytes': len(raw), 'sha256': sha(raw)}


ref_checks = {}
exports = []


def read_ref(ref):
    path = Path(ref['locator'].removeprefix('file://'))
    assert path.is_relative_to(ROOT), path
    raw = path.read_bytes()
    assert len(raw) == ref['bytes'], path
    ref_checks[ref['locator']] = {
        **file_record(path), 'declared_digest': ref['digest'],
        'declared_byte_length_matches': True,
        'digest_note': 'SHA256 recomputed here; BLAKE3 reference retained, not independently recomputed by this analyzer',
    }
    return path, raw


def read_json_ref(ref):
    path, raw = read_ref(ref)
    return path, json.loads(raw)


def export_ref(condition, kind, ref):
    path, raw = read_ref(ref)
    name = condition + '-' + kind + ('.json' if ref.get('media_type') == 'application/json' else '.txt')
    (OUT / name).write_bytes(raw)
    exports.append({'condition': condition, 'kind': kind, 'reference': ref,
                    'cas_path': rel(path), 'cas_sha256': sha(raw), 'export': 'analysis/' + name,
                    'export_is_exact_original_bytes': True})
    return json.loads(raw) if ref.get('media_type') == 'application/json' else raw.decode()


def parse_row(row):
    value = dict(row)
    for key in ('data', 'input_plan_ref'):
        if isinstance(value.get(key), str):
            try:
                value[key] = json.loads(value[key])
            except json.JSONDecodeError:
                pass
    return value


def json_safe(value):
    if isinstance(value, bytes):
        return {'sqlite_blob_base64': base64.b64encode(value).decode()}
    return value


def expected_wire_object(plan):
    """Read-only equality check for this text-only captured serializer shape."""
    payload = plan['payload']
    assert payload['tools'] == []
    messages = [{'role': 'system', 'content': payload['system_prompt']}]
    for message in payload['messages']:
        assert all(part['type'] == 'text' for part in message['parts'])
        messages.append({'role': message['role'], 'content': ''.join(p['text'] for p in message['parts'])})
    return {'max_tokens': payload['max_tokens'], 'messages': messages,
            'model': plan['provider_binding']['model']}


report_path = next((ROOT / 'acceptance').glob('run-*/report.json'))
run = report_path.parent
report = load(report_path)
identity = load(ROOT / 'run-identity.json')
assert identity['source_head'] == '012370b76735daaa97623bbea4401313b3d9be06'
assert identity['executable_sha256'] == 'f043116ebbb25f98bda4a4619474e1e2b22a9b9e2ff2411052cecc394f985fdb'

requests = []
by_response = {}
wire_objects = {}
wire_bytes = {}
for directory in sorted((ROOT / 'relay/queue').iterdir()):
    if not (directory / 'request.json').exists():
        continue
    request = load(directory / 'request.json')
    remote = load(directory / 'remote.json')
    delivery = load(directory / 'delivery.json')
    verified = load(directory / 'transfer-verification.json')
    req = (directory / 'request.body').read_bytes()
    body = (directory / 'response.body').read_bytes()
    headers = (directory / 'response.headers').read_bytes()
    compressed = (directory / 'response.body.gz').read_bytes()
    assert sha(req) == request['request_sha256'] == remote['request_sha256'] == delivery['request_sha256'] == verified['request_sha256']
    assert sha(body) == remote['response_sha256'] == delivery['response_sha256'] == verified['response_sha256']
    assert sha(headers) == remote['headers_sha256']
    assert sha(compressed) == remote['response_gzip_sha256'] and gzip.decompress(compressed) == body
    assert len(req) == request['body_bytes'] == remote['request_bytes']
    assert len(body) == remote['response_bytes'] == delivery['response_bytes']
    assert verified['request_bytes_identical'] and verified['response_bytes_identical']
    assert not request['authorization_header_present'] and remote['inference_retries'] == 0
    response = json.loads(body) if body else None
    observed_output = request['method'] == 'POST' and remote['curl_exit'] == 0 and remote['http_status'] == 200 and bool(response and response.get('choices'))
    row = {
        'relay_id': directory.name, 'sequence': int(directory.name[:4]),
        'method': request['method'], 'path': request['path'],
        'request_path': rel(directory / 'request.body'), 'response_path': rel(directory / 'response.body'),
        'headers_path': rel(directory / 'response.headers'), 'remote_receipt_path': rel(directory / 'remote.json'),
        'delivery_receipt_path': rel(directory / 'delivery.json'),
        'request_sha256': sha(req), 'response_sha256': sha(body), 'headers_sha256': sha(headers),
        'request_bytes': len(req), 'response_bytes': len(body),
        'origin_http_status': remote['http_status'], 'curl_exit': remote['curl_exit'],
        'origin_content_type': remote['content_type'], 'delivery_status': delivery['status'],
        'local_relay_http_status': 502 if delivery['status'] == 'transport_error' else remote['http_status'],
        'observed_model_output': observed_output,
        'request_sha_verified_across_machines': True, 'response_sha_verified_across_machines': True,
        'headers_sha_verified': True, 'gzip_transport_roundtrip_verified': True,
        'received_at_utc': request['received_at_utc'], 'delivered_at_utc': delivery['delivered_at_utc'],
        'remote_started_at_utc': remote['started_at_utc'], 'remote_finished_at_utc': remote['finished_at_utc'],
        'local_relay_elapsed_ms': delivery['relay_elapsed_ms'],
        'remote_elapsed_ms': remote['remote_elapsed_ms'],
        'remote_attempt_elapsed_ms': remote['inference_elapsed_ms'],
        'remote_curl_time_total_seconds': remote['curl_time_total_seconds'],
        'relay_minus_remote_attempt_ms': delivery['relay_elapsed_ms'] - remote['inference_elapsed_ms'],
        'capacity': remote['capacity'], 'forward_attempts': remote['inference_attempts'],
        'forward_retries': remote['inference_retries'],
        'usage': None, 'finish_reason': None, 'raw_assistant_content': None,
        'condition': None, 'purpose': 'service_models_probe' if request['method'] == 'GET' else None,
    }
    if request['method'] == 'POST':
        payload = json.loads(req)
        wire_objects[directory.name] = payload
        wire_bytes[directory.name] = req
        row.update({'request_options': {k: v for k, v in payload.items() if k != 'messages'},
                    'stream_requested': payload.get('stream', False)})
        if observed_output:
            row.update({'usage': response['usage'], 'finish_reason': response['choices'][0]['finish_reason'],
                        'raw_assistant_content': response['choices'][0]['message']['content'],
                        'server_timings': response.get('timings'), 'system_fingerprint': response.get('system_fingerprint'),
                        'token_usage_source': 'unmodified llama.cpp HTTP response'})
            assert row['capacity']['prompt_tokens'] == response['usage']['prompt_tokens']
            row['capacity']['matches_actual_response_prompt_tokens'] = True
            assert row['response_sha256'] not in by_response
            by_response[row['response_sha256']] = row
        else:
            row['failure_note'] = ('Empty origin body is not model output. Remote attempt timing includes a failed connection; '
                                   'actual inference and token usage are unobserved. The relay generated HTTP502; '
                                   'its error body is not represented as an origin model response.')
            row['local_relay_status_basis'] = 'delivery.json transport_error plus relay_server.py curl-error branch'
    requests.append(row)


def aggregate(rows):
    observed = [r for r in rows if r['observed_model_output']]
    return {
        'posted_requests': len(rows), 'responses_with_observed_usage': len(observed),
        'requests_with_unobserved_usage': len(rows) - len(observed),
        'observed_token_subtotal': {
            **{key: sum(r['usage'][key] for r in observed) for key in ('prompt_tokens', 'completion_tokens', 'total_tokens')},
            'cached_prompt_tokens': sum(r['usage'].get('prompt_tokens_details', {}).get('cached_tokens', 0) for r in observed),
        },
        'remote_attempt_elapsed_ms_all': sum(r['remote_attempt_elapsed_ms'] for r in rows),
        'remote_attempt_elapsed_ms_observed_model_responses': sum(r['remote_attempt_elapsed_ms'] for r in observed),
        'relay_elapsed_ms_all': sum(r['local_relay_elapsed_ms'] for r in rows),
        'relay_minus_remote_attempt_ms_all': sum(r['relay_minus_remote_attempt_ms'] for r in rows),
        'token_total_is_complete': len(observed) == len(rows),
        'timing_is_native_provider_benchmark': False,
    }


arms = []
db_audits = []
cas_index = []
protocol_rejections = []
baseline_facts = None
baseline_audit = None
baseline_first = None
for report_arm in report['arms']:
    condition = report_arm['condition']
    installation = next(run.glob(condition + '-*'))
    db_path = installation / 'shenko.db'
    db = sqlite3.connect('file:' + str(db_path) + '?mode=ro', uri=True)
    db.row_factory = sqlite3.Row
    table_info = [dict(r) for r in db.execute("SELECT name, sql FROM sqlite_master WHERE type='table' ORDER BY name")]
    tables = {}
    audit = {'condition': condition, 'db_path': rel(db_path), 'read_mode': 'SQLite mode=ro; WAL observed without checkpoint',
             'table_counts': {}, 'tables': {}, 'omitted_derived_tables': []}
    for info in table_info:
        name = info['name']
        quoted = '"' + name.replace('"', '""') + '"'
        count = db.execute('SELECT count(*) FROM ' + quoted).fetchone()[0]
        audit['table_counts'][name] = count
        if '_fts' in name or name == 'sqlite_sequence':
            audit['omitted_derived_tables'].append(name)
            continue
        raw_rows = [dict(r) for r in db.execute('SELECT * FROM ' + quoted)]
        tables[name] = [parse_row(r) for r in raw_rows]
        audit['tables'][name] = {'schema_sql': info['sql'], 'row_count': count,
                                'rows': [{k: json_safe(v) for k, v in r.items()} for r in raw_rows]}
    db.close()
    db_audits.append(audit)
    for path in sorted((installation / 'cas').rglob('*')):
        if path.is_file():
            cas_index.append({'condition': condition, **file_record(path),
                              'cas_relative_path': str(path.relative_to(installation / 'cas')),
                              'declared_digest_from_filename': 'blake3:' + path.name})
    attempts = sorted(tables['subject_request_attempts'], key=lambda r: r['created_commit_seq'])
    steps = {r['step_id']: r['data'] for r in tables['subject_steps']}
    attempt_summaries = []
    for number, raw_attempt in enumerate(attempts, 1):
        attempt = raw_attempt['data']
        terminal = attempt.get('terminal') or {}
        result_ref = terminal.get('result_ref')
        result = export_ref(condition, f'subject-attempt-{number:02d}-result', result_ref) if result_ref else None
        plan_path, plan = read_json_ref(attempt['input_plan_ref'])
        wanted = expected_wire_object(plan)
        matching = [r for r in requests if r['method'] == 'POST' and wire_objects[r['relay_id']] == wanted]
        assert len(matching) == 1, (condition, number, len(matching))
        wire = matching[0]
        serialized = json.dumps(wanted, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()
        exact_request_match = serialized == wire_bytes[wire['relay_id']]
        assert exact_request_match
        wire.update({'condition': condition, 'purpose': 'subject', 'store_request_id': attempt['request_id'],
                     'subject_attempt_ordinal': number, 'store_step_id': attempt['step_id'],
                     'input_plan_ref': attempt['input_plan_ref'], 'request_matches_CAS_payload_bytes': exact_request_match})
        if result and result.get('response'):
            raw_response = result['response']['raw_response'].encode()
            assert by_response[sha(raw_response)] is wire
            assert raw_response == (ROOT / wire['response_path']).read_bytes()
            assert result['response']['content'] == wire['raw_assistant_content']
            wire['purpose_attribution_basis'] = 'Exact SHA256 and byte match of CAS subject result response.raw_response to origin response.body; input-plan request bytes also match'
            wire['response_CAS_raw_response_sha256_matches'] = True
        else:
            assert not wire['observed_model_output']
            wire['purpose_attribution_basis'] = 'Unique byte-exact serialization match of CAS input-plan payload/binding to original request.body; failed CAS response is null and cannot be matched by response SHA'
            wire['response_CAS_raw_response_sha256_matches'] = None
        step = steps[attempt['step_id']]
        trace = {
            'attempt_ordinal': number, 'relay_id': wire['relay_id'], 'request_id': attempt['request_id'],
            'attempt_id': attempt['attempt_id'], 'step_id': attempt['step_id'], 'step_no': step['step_no'],
            'step_state': step['state'], 'T1_input_plan_ref': attempt['input_plan_ref'],
            'T1_request_created_commit': attempt['created_commit'],
            'E1_request_result_ref': result_ref, 'E1_result_recorded_commit': terminal.get('recorded_commit'),
            'E1_model_result_ref': step['model_result_ref'], 'E1_model_outcome_ref': step['model_outcome_ref'],
            'T2_decision_ref': step['decision_ref'], 'T2_commit_ref': step['commit_ref'],
            'retry_reason_ref': step['retry_reason_ref'], 'terminal_outcome': terminal.get('outcome'),
            'semantic_wait_in_frozen_input': plan.get('semantic_wait'),
            'request_byte_match': True, 'model_response_byte_match': wire['response_CAS_raw_response_sha256_matches'],
        }
        for key in ('model_result_ref', 'model_outcome_ref', 'decision_ref'):
            if step.get(key):
                read_ref(step[key])
        if step['model_outcome_ref']:
            _, outcome = read_json_ref(step['model_outcome_ref'])
            assert outcome['raw_response'].encode() == (ROOT / wire['response_path']).read_bytes()
            trace['E1_model_outcome_raw_response_sha256_matches'] = True
        if step['retry_reason_ref']:
            rejection_path, rejection = read_json_ref(step['retry_reason_ref'])
            # awaiting_model inherits the previous rejection; count only its original superseded step.
            is_own_rejection = rejection['supersedes_step_id'] == step['step_id']
            trace['retry_reason_is_this_attempt_rejection'] = is_own_rejection
            if is_own_rejection:
                entry = {'condition': condition, 'relay_id': wire['relay_id'],
                         'cas_path': rel(rejection_path), 'cas_sha256': sha(rejection_path.read_bytes()),
                         'reference': step['retry_reason_ref'], **rejection}
                protocol_rejections.append(entry)
                wire['protocol_rejection'] = rejection['error']
                assert step['state'] == 'retired' and step['decision_ref'] is None and step['commit_ref'] is None
        attempt_summaries.append(trace)
    starting = load(installation / 'starting-context.json')
    arm = {
        'condition': condition, 'runner_arm': report_arm,
        'installation': rel(installation), 'acceptance_json_exists': (installation / 'acceptance.json').exists(),
        'starting_context_path': rel(installation / 'starting-context.json'),
        'starting_context_capabilities': starting['capabilities'],
        'strict_arm_pass': bool(report_arm.get('effective') and report_arm.get('used_expected_reader')),
        'request_attempt_count': len(attempts),
        'step_state_counts': dict(collections.Counter(s['state'] for s in steps.values())),
        'step_number_counts': dict(collections.Counter(s['step_no'] for s in steps.values())),
        'task_kinds': [r['data'].get('kind') for r in tables['tasks']],
        'task_result_row_count': len(tables['task_results']),
        'subject_intent_row_count': len(tables['subject_intents']),
        'semantic_wait_row_count': len(tables['semantic_waits']),
        'subject_output_parts_row_count': len(tables['subject_output_parts']),
        'subject_attempts': attempt_summaries,
    }
    if report_arm.get('judge_receipt'):
        judge_result = export_ref(condition, 'judge-result', report_arm['judge_receipt']['result_ref'])
        export_ref(condition, 'judge-request', report_arm['judge_receipt']['request_ref'])
        raw_judge = judge_result['response']['raw_response'].encode()
        judge = by_response[sha(raw_judge)]
        assert raw_judge == (ROOT / judge['response_path']).read_bytes()
        assert judge_result['response']['content'] == judge['raw_assistant_content'] == report_arm['judge_text']
        judge.update({'condition': condition, 'purpose': 'judge', 'store_request_id': report_arm['judge_receipt']['request_id'],
                      'purpose_attribution_basis': 'Exact SHA256 and byte match of CAS judge result response.raw_response to origin response.body',
                      'response_CAS_raw_response_sha256_matches': True})
        arm['judge_relay_id'] = judge['relay_id']
        for kind in ('facts', 'installation', 'configuration', 'sample'):
            value = export_ref(condition, kind, report_arm[kind + '_ref'])
            if kind == 'facts':
                arm['retrievals'] = value['retrievals']
                if condition == 'baseline':
                    baseline_facts = value
        arm['committed_output_text'] = report_arm['committed']['output']['text']
        arm['committed_controls'] = report_arm['committed']['output']['controls']
        arm['consumed_results'] = report_arm['committed']['consumed_results']
    else:
        arm['judge_relay_id'] = None
        arm['retrievals'] = None
        arm['retrievals_note'] = 'No candidate facts/acceptance artifact was produced; absence of executed retrieval is instead shown by Store tasks/intents/task_results and null T2 references'
        arm['committed_output_text'] = None
        arm['consumed_results'] = None
    arm['usage_and_timing'] = aggregate([r for r in requests if r['condition'] == condition and r['method'] == 'POST'])
    arms.append(arm)
    if condition == 'baseline':
        baseline_audit = tables
        baseline_first = next(r for r in requests if r['condition'] == 'baseline' and r['purpose'] == 'subject')

assert all(r['purpose'] for r in requests)
baseline_installation = next(run.glob('baseline-*'))
starting = load(baseline_installation / 'starting-context.json')
baseline_prompt = '\n'.join(m['content'] for m in wire_objects[baseline_first['relay_id']]['messages'])
fixture_checks = []
for index, item in enumerate(baseline_facts['corpus'], 1):
    raw_event = next(r for r in baseline_audit['session_events'] if r['seq'] == index)
    fixture_checks.append({'fixture_ordinal': index, 'fixture_text_sha256': sha(item['text'].encode()),
                           'first_wire_occurrences': baseline_prompt.count(item['text']),
                           'store_event_id': raw_event['event_id'], 'store_seq': raw_event['seq'],
                           'retained_in_raw_store': raw_event['data']['text'] == item['text']})
literals = ['src/auth/refresh.ts', 'tests/auth/refresh-race.test.ts', 'SameSite', 'None', 'Lax',
            'generation', '旧响应', '最新请求']
isolation = {
    'condition': 'baseline', 'starting_context_path': rel(baseline_installation / 'starting-context.json'),
    'first_subject_relay_id': baseline_first['relay_id'], 'first_wire_request_sha256': baseline_first['request_sha256'],
    'first_wire_request_bytes': baseline_first['request_bytes'], 'context_tokens': starting['capabilities']['max_context_tokens'],
    'policy_id': starting['window']['policy_id'], 'window_entries': starting['window']['entries'],
    'covers_through': starting['window']['covers_through'], 'fixture_checks': fixture_checks,
    'answer_literal_occurrences_in_all_message_content': {s: baseline_prompt.count(s) for s in literals},
    'pointer_marker_occurrences': baseline_prompt.count('[context derivative/pointer to raw seq 1..4; not a new instruction]'),
    'fixture_raw_store_basis': 'Original session_events seq 1..4 retain all four fixture texts; this is not evidence that the subject invoked history.query',
    'conclusion': 'The first baseline subject wire contains a Pointer and no complete fixture message or checked answer literal; original raw Store events remain present',
}
assert len(starting['window']['entries']) == 1 and 'Pointer' in starting['window']['entries'][0]
assert all(c['first_wire_occurrences'] == 0 and c['retained_in_raw_store'] for c in fixture_checks)
assert not any(isolation['answer_literal_occurrences_in_all_message_content'].values())
assert len(protocol_rejections) == len({r['reference']['digest'] for r in protocol_rejections}) == 14

model = load(ROOT / 'model-service/service-manifest.json')
preflight = load(ROOT / 'service-preflight.json')
final_service = load(ROOT / 'model-service/service-final-observation.json')
shutdown = load(run / 'broker-shutdown.json')
assert sha(base64.b64decode(preflight['models_response_raw_base64'])) == preflight['models_response_sha256']
assert preflight['models']['data'][0]['meta']['n_ctx'] == 16384
assert sha((ROOT / 'model-service/start-server.sh').read_bytes()) == preflight['start_script_sha256']
assert sha((ROOT / 'model-service/relay-request.py').read_bytes()) == preflight['remote_helper_sha256']
for arm in arms:
    assert arm['starting_context_capabilities']['max_context_tokens'] == 16384
successful = [r for r in requests if r['observed_model_output']]
posts = [r for r in requests if r['method'] == 'POST']
candidate = next(a for a in arms if a['condition'] == 'candidate')
baseline = next(a for a in arms if a['condition'] == 'baseline')
assert candidate['step_state_counts'] == {'retired': 14, 'awaiting_model': 1}
assert candidate['task_kinds'] == ['subject_root'] and candidate['task_result_row_count'] == 0
assert candidate['judge_relay_id'] is None and not candidate['acceptance_json_exists']

summary = {
    'schema_version': 2, 'analyzed_at_utc': datetime.now(timezone.utc).isoformat(),
    'run_id': run.name, 'scenario': report['scenario'], 'corpus_digest': report['corpus_digest'],
    'report_path': rel(report_path), 'strict_result': report['status'], 'passed': False,
    'complete': report['complete'], 'cleanup_ok': report['cleanup_ok'],
    'operator_interrupted': False, 'operator_interrupted_basis': 'Natural exit1 receipt and service-final-observation operator record; no stop was sent during this round',
    'runner_exit': load(ROOT / 'runner-exit.json'), 'identity': identity,
    'planned_arms': report['planned_arms'], 'paired_smoke_invocations': 1, 'additional_resamples': 0,
    'subject_protocol_revisions_are_not_independent_samples': True,
    'model': model['model'], 'server': model['server'],
    'service_manifest_note': 'Model/server identity manifest was retained from setup; service-preflight.json observes this round and service-final-observation.json supersedes its running-health state',
    'service_preflight_path': 'service-preflight.json', 'service_final_observation': final_service,
    'broker_shutdown_summary': {
        k: shutdown[k] for k in ('local_admission_closed', 'local_provider_readers_drained', 'remote_termination_claimed')
    },
    'broker_unresolved_requests': [{k: r.get(k) for k in ('request_id', 'state', 'charged', 'reader_available')} for r in shutdown['requests']],
    'transport': {
        'mechanism': 'Loopback HTTP relay plus exact UTF-8 request file transfer via Box; origin response bytes transferred with gzip/base64 and SHA256 checks',
        'request_count': len(requests), 'real_models_get_count': sum(r['method'] == 'GET' for r in requests),
        'chat_post_forward_attempts': len(posts), 'observed_model_responses': len(successful),
        'connection_failed_post_count': sum(r['curl_exit'] != 0 for r in posts), 'forward_retries': 0,
        'all_request_bytes_verified': True, 'all_origin_response_bytes_verified': True,
        'all_successful_model_responses_matched_to_CAS': all(r.get('response_CAS_raw_response_sha256_matches') for r in successful),
        'credential_headers_sent': False, 'provider_timeout_seconds': 180, 'remote_curl_max_time_seconds': 175,
        'streamed_requests_observed': False, 'SSE_note': 'No SSE request occurred; buffering support was not exercised by this pair',
        'latency_note': 'Relay and arm times include connector transfer/queue/polling/preflight; prefix-cache state was not reset. These are not native latency measurements',
    },
    'context_preflight': {
        'context_tokens': 16384, 'both_arm_profiles_bound_to_same_observed_limit': True,
        'kv_type_k': 'f16', 'kv_type_v': 'f16', 'successful_post_counts_observed': len(successful),
        'all_successful_posts_fit': all(r['capacity']['fits'] for r in successful),
        'all_successful_prompt_counts_match_response': all(r['capacity']['matches_actual_response_prompt_tokens'] for r in successful),
        'largest_observed_prompt_plus_requested_max_tokens': max(r['capacity']['prompt_plus_max_tokens'] for r in successful),
        'unobserved_final_capacity': posts[-1]['capacity'],
        'note': 'The final failed connection yielded no tokenize count; no capacity, actual inference, or token use is inferred for it',
    },
    'usage_and_timing_all_posts': aggregate(posts),
    'baseline_prompt_isolation_path': 'analysis/baseline-prompt-isolation.json',
    'candidate_protocol_rejections_path': 'analysis/candidate-protocol-rejections.json',
    'protocol_rejection_count': len(protocol_rejections),
    'findings': [
        'Runner ended naturally with exit1 and report complete=false, cleanup_ok=true; no operator interruption occurred.',
        'Baseline first wire contains only the genuine Pointer for fixture seq1..4, with none of the four original messages or checked answer literals; originals remain in Store.',
        'Baseline subject only asked the user for logs and performed no history read; the real judge nevertheless returned EFFECTIVE and narrated fixture facts missing from that output. The raw judge is preserved as a false-positive observation, not overwritten.',
        'Candidate first response emitted the live-search delegate shape, but wait:replace had no active semantic wait. The entire output was rejected before T2 and no delegate was dispatched.',
        'Thirteen subsequent wait:end responses were rejected for the same absent-wait condition: fourteen unique control rejections total, not fourteen successful steps.',
        'Candidate has fifteen incarnations of step_no1: fourteen retired and one awaiting_model; zero subject intents, zero task results, and only the root task. There is no candidate committed answer or judge.',
        'Final request failed to connect (curl7/http0, empty origin body). Relay generated HTTP502; Store recorded failed outcome with unknown termination and no response, and preserved awaiting_model.',
        'Later observation found the model PID and listener absent. Exit status and cause are unknown; kernel evidence was unavailable and the last log lines contain no termination diagnostic.',
        'This bounded run demonstrates neither successful external retrieval nor efficacy or token savings. Local cleanup success does not prove the remote process termination cause.',
    ],
    'limitations': [
        'One unseeded paired-smoke invocation. Rejected protocol revisions are dependent continuations, not independent trials.',
        'Qwen3-1.7B Q8_0 is not equivalent to the requested large-model baseline.',
        'The same small-model judge produced a visible false positive on baseline.',
        'Final failed request has unknown actual token usage; aggregate tokens are observed subtotals only.',
        'Box connector transport and prefix caching preclude native latency comparison.',
        'No prompt/output/reader/judge result or strict gate was changed by this analysis; no model was invoked.',
    ],
    'arms': arms, 'requests': requests,
}
dump('db-audit.json', db_audits)
dump('cas-index.json', cas_index)
dump('cas-export-map.json', exports)
dump('cas-reference-verification.json', list(ref_checks.values()))
dump('baseline-prompt-isolation.json', isolation)
dump('candidate-protocol-rejections.json', protocol_rejections)
dump('result-summary.json', summary)
print(json.dumps({'summary': str(OUT / 'result-summary.json'), 'strict_result': summary['strict_result'],
                  'complete': summary['complete'], 'cleanup_ok': summary['cleanup_ok'],
                  'protocol_rejections': len(protocol_rejections), 'cas_files': len(cas_index),
                  'requests': len(requests), 'usage': summary['usage_and_timing_all_posts'],
                  'arms': [{'condition': a['condition'], 'usage': a['usage_and_timing']} for a in arms]},
                 ensure_ascii=False, indent=2))
