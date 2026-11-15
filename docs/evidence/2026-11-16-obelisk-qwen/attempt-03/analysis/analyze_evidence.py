#!/usr/bin/env python3
"""Read-only multi-step evidence analysis. --snapshot is explicitly provisional.

This program never invokes a model, edits a runner/relay, writes a Store, or retries
requests. Final mode requires both runner-exit.json and report.json. Database reads
use a read-only SQLite transaction; an interim snapshot is not cross-process atomic.
"""
import argparse
import base64
import collections
import gzip
import hashlib
import json
import sqlite3
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser()
parser.add_argument('--snapshot', action='store_true')
args = parser.parse_args()
NOW = datetime.now(timezone.utc)
OUT = ROOT / 'analysis'
if args.snapshot:
    OUT = OUT / 'snapshots' / NOW.strftime('%Y%m%dT%H%M%S.%fZ')
OUT.mkdir(parents=True, exist_ok=True)
ANSWER_LITERALS = ['src/auth/refresh.ts', 'tests/auth/refresh-race.test.ts', 'SameSite', 'None',
                   'Lax', 'generation', '旧响应', '最新请求']


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def load(path):
    return json.loads(path.read_bytes())


def rel(path):
    return str(path.relative_to(ROOT))


def dump(name, value):
    (OUT / name).write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')


def file_info(path):
    raw = path.read_bytes()
    return {'path': rel(path), 'bytes': len(raw), 'sha256': sha(raw)}


def json_safe(v):
    return {'sqlite_blob_base64': base64.b64encode(v).decode()} if isinstance(v, bytes) else v


def decoded_row(row):
    result = dict(row)
    for k, v in result.items():
        if isinstance(v, str) and (v.startswith('{') or v.startswith('[')):
            try:
                result[k] = json.loads(v)
            except json.JSONDecodeError:
                pass
    return result


ref_checks = {}
cas_exports = []


def read_ref(ref):
    path = Path(ref['locator'].removeprefix('file://'))
    assert path.is_relative_to(ROOT), path
    raw = path.read_bytes()
    assert len(raw) == ref['bytes'], path
    ref_checks[ref['locator']] = {**file_info(path), 'declared_digest': ref['digest'],
                                'declared_bytes_match': True, 'BLAKE3_recomputed_by_analyzer': False}
    return path, raw


def json_ref(ref):
    p, b = read_ref(ref)
    return p, json.loads(b)


def export_ref(condition, label, ref):
    p, raw = read_ref(ref)
    ext = '.json' if ref.get('media_type') == 'application/json' else '.txt'
    name = condition + '-' + label + ext
    (OUT / name).write_bytes(raw)
    cas_exports.append({'condition': condition, 'label': label, 'reference': ref,
                        'cas_path': rel(p), 'cas_sha256': sha(raw), 'export': rel(OUT / name),
                        'export_bytes_unchanged': True})
    try:
        return json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError):
        return raw.decode(errors='replace')


def plain_messages(payload):
    messages = [{'role': 'system', 'content': payload['system_prompt']}]
    assert payload['tools'] == [], 'This captured text-only serializer audit requires an explicit extension for native API tool fields'
    for m in payload['messages']:
        assert all(p['type'] == 'text' for p in m['parts'])
        messages.append({'role': m['role'], 'content': ''.join(p['text'] for p in m['parts'])})
    return messages


def expected_request(plan):
    return {'max_tokens': plan['payload']['max_tokens'], 'messages': plain_messages(plan['payload']),
            'model': plan['provider_binding']['model']}


def literal_counts(value):
    text = value if isinstance(value, str) else json.dumps(value, ensure_ascii=False)
    return {s: text.count(s) for s in ANSWER_LITERALS}


def walk(value, path='$'):
    yield path, value
    if isinstance(value, dict):
        for k, v in value.items():
            yield from walk(v, path + '.' + str(k))
    elif isinstance(value, list):
        for i, v in enumerate(value):
            yield from walk(v, path + '[' + str(i) + ']')


def aggregate(rows):
    posts = [r for r in rows if r['method'] == 'POST']
    observed = [r for r in posts if r['observed_model_output']]
    delivered = [r for r in posts if r.get('local_relay_elapsed_ms') is not None]
    return {
        'posted_requests': len(posts), 'responses_with_observed_usage': len(observed),
        'requests_without_observed_model_usage': len(posts) - len(observed),
        'observed_token_subtotal': {
            **{k: sum(r['usage'][k] for r in observed) for k in ('prompt_tokens', 'completion_tokens', 'total_tokens')},
            'cached_prompt_tokens': sum(r['usage'].get('prompt_tokens_details', {}).get('cached_tokens', 0) for r in observed),
        },
        'token_total_complete': len(observed) == len(posts),
        'relay_elapsed_ms_observed_receipts': sum(r['local_relay_elapsed_ms'] for r in delivered),
        'remote_attempt_elapsed_ms_observed_receipts': sum(r.get('remote_attempt_elapsed_ms') or 0 for r in posts),
        'remote_attempt_elapsed_ms_model_responses': sum(r.get('remote_attempt_elapsed_ms') or 0 for r in observed),
        'relay_minus_remote_attempt_ms_observed_receipts': sum(r['local_relay_elapsed_ms'] - (r.get('remote_attempt_elapsed_ms') or 0) for r in delivered),
        'native_latency_benchmark': False,
    }


run = next((ROOT / 'acceptance').glob('run-*'))
report = load(run / 'report.json') if (run / 'report.json').exists() else None
runner_exit = load(ROOT / 'runner-exit.json') if (ROOT / 'runner-exit.json').exists() else None
if not args.snapshot and not (report is not None and runner_exit is not None):
    raise SystemExit('Final analysis requires retained report.json and runner-exit.json; use --snapshot only for provisional observations')
identity = load(ROOT / 'run-identity.json')
assert identity['source_head'] == '012370b76735daaa97623bbea4401313b3d9be06'
assert identity['executable_sha256'] == 'f043116ebbb25f98bda4a4619474e1e2b22a9b9e2ff2411052cecc394f985fdb'
report_arms = {a['condition']: a for a in report['arms']} if report else {}

requests = []
wire_objects = {}
wire_bytes = {}
response_map = collections.defaultdict(list)
for q in sorted((ROOT / 'relay/queue').iterdir()):
    if not q.is_dir() or not (q / 'request.json').exists():
        continue
    meta = load(q / 'request.json')
    req = (q / 'request.body').read_bytes()
    assert sha(req) == meta['request_sha256'] and len(req) == meta['body_bytes']
    row = {
        'relay_id': q.name, 'sequence': int(q.name[:4]), 'method': meta['method'], 'path': meta['path'],
        'request_path': rel(q / 'request.body'), 'request_sha256': sha(req), 'request_bytes': len(req),
        'received_at_utc': meta['received_at_utc'], 'authorization_header_present': meta['authorization_header_present'],
        'condition': None, 'purpose': 'service_models_probe' if meta['method'] == 'GET' else None,
        'observed_model_output': False, 'usage': None, 'raw_assistant_content': None, 'finish_reason': None,
        'delivery_status': 'pending_or_no_delivery_receipt',
    }
    assert not row['authorization_header_present']
    if meta['method'] == 'POST':
        obj = json.loads(req)
        wire_objects[q.name] = obj
        wire_bytes[q.name] = req
        row['request_options'] = {k: v for k, v in obj.items() if k != 'messages'}
        row['stream_requested'] = obj.get('stream', False)
    required = ['remote.json', 'transfer-verification.json', 'response.body', 'response.headers']
    if all((q / name).exists() for name in required):
        remote = load(q / 'remote.json')
        delivery = load(q / 'delivery.json') if (q / 'delivery.json').exists() else None
        verified = load(q / 'transfer-verification.json')
        body = (q / 'response.body').read_bytes()
        headers = (q / 'response.headers').read_bytes()
        assert sha(req) == remote['request_sha256'] == verified['request_sha256']
        assert sha(body) == remote['response_sha256'] == verified['response_sha256']
        assert sha(headers) == remote['headers_sha256']
        assert len(body) == remote['response_bytes']
        if delivery:
            assert sha(req) == delivery['request_sha256'] and sha(body) == delivery['response_sha256']
            assert len(body) == delivery['response_bytes']
        assert verified['request_bytes_identical'] and verified['response_bytes_identical']
        assert remote['inference_retries'] == 0
        if (q / 'response.body.gz').exists():
            compressed = (q / 'response.body.gz').read_bytes()
            assert sha(compressed) == remote['response_gzip_sha256'] and gzip.decompress(compressed) == body
        row.update({
            'response_path': rel(q / 'response.body'), 'response_sha256': sha(body), 'response_bytes': len(body),
            'headers_path': rel(q / 'response.headers'), 'headers_sha256': sha(headers),
            'origin_http_status': remote['http_status'], 'curl_exit': remote['curl_exit'],
            'origin_content_type': remote['content_type'],
            'delivery_status': delivery['status'] if delivery else 'remote_receipt_retained_delivery_unobserved',
            'local_relay_response_status_attempted': (502 if remote['curl_exit'] else remote['http_status']) if delivery else None,
            'local_relay_response_status_delivered': (502 if remote['curl_exit'] else remote['http_status']) if delivery and delivery['status'] in ('transport_error', 'forwarded_raw_response') else None,
            'local_relay_elapsed_ms': delivery['relay_elapsed_ms'] if delivery else None,
            'remote_elapsed_ms': remote['remote_elapsed_ms'], 'remote_attempt_elapsed_ms': remote['inference_elapsed_ms'],
            'remote_curl_time_total_seconds': remote['curl_time_total_seconds'],
            'remote_started_at_utc': remote['started_at_utc'], 'remote_finished_at_utc': remote['finished_at_utc'],
            'delivered_at_utc': delivery['delivered_at_utc'] if delivery else None,
            'transfer_verified_at_utc': verified['verified_at_utc'], 'capacity': remote['capacity'],
            'forward_attempts': remote['inference_attempts'], 'forward_retries': remote['inference_retries'],
            'request_response_SHA256_verified_across_machines': True,
            'remote_receipt_path': rel(q / 'remote.json'),
            'delivery_receipt_path': rel(q / 'delivery.json') if delivery else None,
        })
        try:
            response = json.loads(body) if body else None
        except json.JSONDecodeError:
            response = None
        if meta['method'] == 'POST' and remote['curl_exit'] == 0 and remote['http_status'] == 200 and response and response.get('choices'):
            row.update({'observed_model_output': True, 'usage': response['usage'],
                        'raw_assistant_content': response['choices'][0]['message']['content'],
                        'finish_reason': response['choices'][0]['finish_reason'],
                        'server_timings': response.get('timings'), 'system_fingerprint': response.get('system_fingerprint')})
            if 'prompt_tokens' in row['capacity']:
                assert row['capacity']['prompt_tokens'] == row['usage']['prompt_tokens']
                row['capacity']['matches_actual_response_prompt_tokens'] = True
            response_map[sha(body)].append(row)
        elif meta['method'] == 'POST':
            row['failure_note'] = 'Origin body is not a successful model output; usage remains unknown. Attempt time is not proof of inference. A relay-generated error is not an origin model answer; caller_disconnected does not prove status delivery.'
    requests.append(row)

db_audits = []
arm_data = {}
cas_index = []
for installation in sorted(p for p in run.iterdir() if p.is_dir() and (p / 'shenko.db').exists()):
    condition = installation.name.split('-', 1)[0]
    db = sqlite3.connect('file:' + str(installation / 'shenko.db') + '?mode=ro', uri=True)
    db.row_factory = sqlite3.Row
    db.execute('BEGIN')
    schema = [dict(r) for r in db.execute("SELECT name,sql FROM sqlite_master WHERE type='table' ORDER BY name")]
    audit = {'condition': condition, 'db_path': rel(installation / 'shenko.db'),
             'snapshot_at_utc': datetime.now(timezone.utc).isoformat(), 'read_mode': 'mode=ro with BEGIN transaction; no checkpoint',
             'table_counts': {}, 'tables': {}, 'omitted_derived_tables': []}
    tables = {}
    for entry in schema:
        table = entry['name']
        quoted = '"' + table.replace('"', '""') + '"'
        count = db.execute('SELECT count(*) FROM ' + quoted).fetchone()[0]
        audit['table_counts'][table] = count
        if '_fts' in table or table == 'sqlite_sequence':
            audit['omitted_derived_tables'].append(table)
            continue
        rows = [dict(r) for r in db.execute('SELECT * FROM ' + quoted)]
        tables[table] = [decoded_row(r) for r in rows]
        audit['tables'][table] = {'schema_sql': entry['sql'], 'row_count': count,
                                  'rows': [{k: json_safe(v) for k, v in r.items()} for r in rows]}
    db.rollback()
    db.close()
    db_audits.append(audit)
    cas_objects = []
    for path in sorted((installation / 'cas').rglob('*')):
        if path.is_file():
            cas_index.append({'condition': condition, **file_info(path), 'declared_digest': 'blake3:' + path.name})
            try:
                cas_objects.append((path, load(path)))
            except (UnicodeDecodeError, json.JSONDecodeError):
                pass
    arm_data[condition] = {'installation': installation, 'tables': tables, 'cas_objects': cas_objects,
                           'plans': {}, 'traces': [], 'rejections': [], 'native_results': []}

for condition, data in arm_data.items():
    tables = data['tables']
    steps = {r['step_id']: r['data'] for r in tables['subject_steps']}
    for ordinal, row in enumerate(sorted(tables['subject_request_attempts'], key=lambda r: r['created_commit_seq']), 1):
        attempt = row['data']
        plan_ref = attempt.get('input_plan_ref')
        if not plan_ref:
            continue
        _, plan = json_ref(plan_ref)
        data['plans'][attempt['step_id']] = plan
        expected = expected_request(plan)
        matching = [r for r in requests if r['method'] == 'POST' and wire_objects[r['relay_id']] == expected]
        if len(matching) != 1:
            if args.snapshot and not matching:
                continue
            raise AssertionError((condition, attempt['request_id'], 'request matches', len(matching)))
        wire = matching[0]
        encoded = json.dumps(expected, ensure_ascii=False, separators=(',', ':'), sort_keys=True).encode()
        assert encoded == wire_bytes[wire['relay_id']]
        wire.update({'condition': condition, 'purpose': 'subject', 'store_request_id': attempt['request_id'],
                     'step_id': attempt['step_id'], 'subject_attempt_ordinal': ordinal,
                     'input_plan_ref': plan_ref, 'request_matches_CAS_input_plan_bytes': True})
        terminal = attempt.get('terminal') or {}
        result_ref = terminal.get('result_ref')
        result = export_ref(condition, f'subject-attempt-{ordinal:02d}-result', result_ref) if result_ref else None
        if result and result.get('response'):
            raw_response = result['response']['raw_response'].encode()
            exact_matches = response_map[sha(raw_response)]
            if wire not in exact_matches:
                if not args.snapshot:
                    raise AssertionError('CAS model result is not the captured origin body')
            else:
                assert raw_response == (ROOT / wire['response_path']).read_bytes()
                assert result['response']['content'] == wire['raw_assistant_content']
                wire['response_CAS_raw_response_bytes_match'] = True
                wire['purpose_attribution_basis'] = 'CAS response.raw_response exact SHA256/byte identity plus exact input-plan request byte identity'
        else:
            wire['response_CAS_raw_response_bytes_match'] = None
            wire['purpose_attribution_basis'] = 'Unique exact input-plan request bytes; no model response is fabricated for pending/failed attempt'
        step = steps[attempt['step_id']]
        trace = {
            'attempt_ordinal': ordinal, 'relay_id': wire['relay_id'], 'request_id': attempt['request_id'],
            'attempt_id': attempt['attempt_id'], 'step_id': step['step_id'], 'step_no': step['step_no'], 'step_state': step['state'],
            'T1_input_plan_ref': plan_ref, 'T1_request_created_commit': attempt['created_commit'],
            'E1_request_result_ref': result_ref, 'E1_result_recorded_commit': terminal.get('recorded_commit'),
            'E1_model_result_ref': step.get('model_result_ref'), 'E1_model_outcome_ref': step.get('model_outcome_ref'),
            'T2_decision_ref': step.get('decision_ref'), 'T2_commit_ref': step.get('commit_ref'),
            'terminal_outcome': terminal.get('outcome'), 'retry_reason_ref': step.get('retry_reason_ref'),
            'semantic_wait_in_frozen_input': plan.get('semantic_wait'),
            'input_event_ids': [{'event_id': e.get('event_id'), 'kind': e.get('kind')} for e in plan.get('events', [])],
        }
        for key in ('model_result_ref', 'model_outcome_ref', 'decision_ref'):
            if step.get(key):
                read_ref(step[key])
        if step.get('model_outcome_ref'):
            _, outcome = json_ref(step['model_outcome_ref'])
            if wire.get('response_path'):
                assert outcome['raw_response'].encode() == (ROOT / wire['response_path']).read_bytes()
                trace['E1_outcome_raw_response_byte_match'] = True
        if step.get('decision_ref'):
            _, decision = json_ref(step['decision_ref'])
            trace['committed_controls'] = decision['output']['controls']
            trace['committed_text'] = decision['output']['text']
        if step.get('retry_reason_ref'):
            rp, rejection = json_ref(step['retry_reason_ref'])
            own = rejection.get('supersedes_step_id') == step['step_id']
            trace['retry_reason_is_this_attempt_rejection'] = own
            if own:
                data['rejections'].append({'condition': condition, 'relay_id': wire['relay_id'],
                                            'cas_path': rel(rp), 'cas_sha256': sha(rp.read_bytes()),
                                            'reference': step['retry_reason_ref'], **rejection})
                wire['protocol_rejection'] = rejection['error']
        data['traces'].append(trace)

    # Associate recorded judges, including a CAS result that may exist before a final arm report.
    judge_candidates = []
    ra = report_arms.get(condition, {})
    if ra.get('judge_receipt'):
        jr = ra['judge_receipt']
        judge = export_ref(condition, 'judge-result', jr['result_ref'])
        export_ref(condition, 'judge-request', jr['request_ref'])
        judge_candidates.append((judge, jr['result_ref'], 'runner judge_receipt'))
    for path, obj in data['cas_objects']:
        if isinstance(obj, dict) and obj.get('purpose') == 'judge' and obj.get('response'):
            judge_candidates.append((obj, {'locator': path.as_uri(), 'bytes': path.stat().st_size, 'digest': 'blake3:' + path.name}, 'CAS purpose=judge'))
    for judge, reference, basis in judge_candidates:
        response = judge.get('response')
        if not response or not response.get('raw_response'):
            continue
        raw = response['raw_response'].encode()
        for wire in response_map.get(sha(raw), []):
            assert raw == (ROOT / wire['response_path']).read_bytes()
            assert wire['purpose'] in (None, 'judge')
            wire.update({'condition': condition, 'purpose': 'judge', 'store_request_id': judge.get('request_id'),
                         'response_CAS_raw_response_bytes_match': True, 'judge_result_ref': reference,
                         'purpose_attribution_basis': basis + '; exact CAS raw_response SHA256/byte identity'})
    for kind in ('facts', 'installation', 'configuration', 'sample'):
        if ra.get(kind + '_ref'):
            data[kind] = export_ref(condition, kind, ra[kind + '_ref'])

# Resolve real native effect/receipt chains and distinguish model visibility from committed handling.
for condition, data in arm_data.items():
    tables = data['tables']
    tasks = {r['task_id']: r['data'] for r in tables['tasks']}
    steps = {r['step_id']: r['data'] for r in tables['subject_steps']}
    intents = {r['intent_id']: r for r in tables['subject_intents']}
    operations = {r['operation_id']: r['data'] for r in tables['operations']}
    receipts = {r['receipt_id']: r['data'] for r in tables['evidence_receipts']}
    native_attempts = {r['attempt_id']: r['data'] for r in tables['attempts']}
    events = {r['event_id']: r for r in tables['session_events']}
    raw_event_json = {r['event_id']: r['data'] for audit in db_audits if audit['condition'] == condition
                      for r in audit['tables']['session_events']['rows']}
    for index, rr in enumerate(sorted(tables['task_results'], key=lambda r: r['commit_seq']), 1):
        result = rr['data']
        task = tasks.get(rr['task_id'], {})
        if rr['kind'] != 'native_child':
            continue
        origin_step = steps.get(task.get('created_by_step_id'), {})
        intent = intents.get(task.get('memos', {}).get('intent_id', rr['task_id']))
        operation = operations.get(result.get('operation_id'))
        receipt = receipts.get(result.get('receipt_id'))
        attempt = native_attempts.get(receipt.get('attempt_id')) if receipt else None
        evidence = export_ref(condition, f'native-result-{index:02d}-evidence', result['evidence_ref']) if result.get('evidence_ref') else None
        frozen_input = export_ref(condition, f'native-result-{index:02d}-frozen-input', operation['final_input_ref']) if operation and operation.get('final_input_ref') else None
        normalized = export_ref(condition, f'native-result-{index:02d}-normalized', receipt['normalized_ref']) if receipt and receipt.get('normalized_ref') else None
        raw_receipt = export_ref(condition, f'native-result-{index:02d}-raw-receipt', receipt['raw_ref']) if receipt and receipt.get('raw_ref') else None
        origin_decision = json_ref(task['input_ref'])[1] if task.get('input_ref') else None
        exact_result_equals_evidence = evidence == result.get('result') if evidence is not None else None
        normalized_equals_result = normalized == result.get('result') if normalized is not None else None
        nested_observation_equals_raw = result.get('result', {}).get('observation') == raw_receipt if raw_receipt is not None else None
        chain_checks = {
            'operation_task_id_matches': operation.get('task_id') == rr['task_id'] if operation else None,
            'receipt_operation_id_matches': receipt.get('operation_id') == result.get('operation_id') if receipt else None,
            'attempt_operation_id_matches': attempt.get('operation_id') == result.get('operation_id') if attempt else None,
            'attempt_invocation_id_matches': attempt.get('invocation_id') == result.get('invocation_id') if attempt else None,
            'attempt_final_receipt_matches': attempt.get('final_receipt_id') == result.get('receipt_id') if attempt else None,
            'result_commit_seq_matches_DB_column': result.get('commit_ref', {}).get('seq') == rr['commit_seq'],
            'task_memo_native_result_id_matches': task.get('memos', {}).get('native_result_id') == rr['result_id'],
            'T2_control_action_id_matches_task': any(c.get('action_id') == rr['task_id'] for c in (origin_decision or {}).get('output', {}).get('controls', [])),
            'T2_delegated_intent_id_matches_task': any(i.get('intent_id') == rr['task_id'] for i in (origin_decision or {}).get('delegated_task_intents', [])),
            'normalized_receipt_equals_task_result_JSON': normalized_equals_result,
            'raw_receipt_equals_task_result_evidence_CAS': raw_receipt == evidence if raw_receipt is not None and evidence is not None else None,
        }
        consumed = []
        for step_id, plan in data['plans'].items():
            for event in plan.get('events', []):
                if event.get('kind') == 'task_result' and event.get('event_id') == rr['result_id']:
                    trace = next((t for t in data['traces'] if t['step_id'] == step_id), None)
                    consumed.append({'step_id': step_id, 'step_state': steps[step_id]['state'],
                                     'step_no': steps[step_id]['step_no'], 'relay_id': trace['relay_id'] if trace else None,
                                     'event_payload_equals_task_result': event.get('payload') == result,
                                     'T2_commit_ref': steps[step_id].get('commit_ref')})
        wire_visibility = []
        marker = '[event ' + rr['result_id'] + '] '
        for wire in requests:
            if wire.get('condition') != condition or wire.get('purpose') != 'subject':
                continue
            for mi, message in enumerate(wire_objects[wire['relay_id']]['messages']):
                text = message['content']
                if marker in text:
                    suffix = text.split(marker, 1)[1]
                    try:
                        visible_object, _ = json.JSONDecoder().raw_decode(suffix)
                    except json.JSONDecodeError:
                        visible_object = None
                    wire_visibility.append({'relay_id': wire['relay_id'], 'message_index': mi,
                                            'entire_task_result_JSON_semantically_equal': visible_object == result,
                                            'message_content_sha256': sha(text.encode()),
                                            'wire_request_sha256': wire['request_sha256']})
        returned_sources = []
        for source_path, value in walk(result.get('result')):
            if not isinstance(value, dict):
                continue
            anchor_keys = [k for k in ('event_id', 'event_ref', 'seq', 'session_id', 'message_uuid', 'uuid',
                                      'file', 'path', 'source_key', 'source_instance', 'provider', 'offset_utf16') if k in value]
            if not anchor_keys:
                continue
            source = {'result_json_path': source_path, 'anchors': {k: value[k] for k in anchor_keys},
                      'answer_literal_counts_in_this_object': literal_counts(value),
                      'anchor_scope': 'returned_record' if any(k in source_path for k in ('.events[', '.hits[', '.messages[')) else 'metadata_or_other'}
            if value.get('event_id') in events:
                raw_event = events[value['event_id']]
                source['resolved_raw_Store_event'] = {k: raw_event[k] for k in ('event_id', 'session_id', 'seq', 'role', 'event_type')}
                source['raw_event_is_synthetic_fixture'] = isinstance(raw_event['data'], dict) and raw_event['data'].get('synthetic') is True
                if isinstance(value.get('data_preview'), str) and isinstance(value.get('data_preview_start_char'), int):
                    start = value['data_preview_start_char']
                    preview = value['data_preview']
                    raw_text = raw_event_json[value['event_id']]
                    source['preview_verification'] = {
                        'source': 'original session_events.data SQLite string; Unicode scalar positions',
                        'raw_data_sha256': sha(raw_text.encode()), 'preview_sha256': sha(preview.encode()),
                        'start_char': start, 'preview_chars': len(preview), 'raw_data_chars': len(raw_text),
                        'matches_exact_raw_Store_slice': raw_text[start:start + len(preview)] == preview,
                        'truncated_before': value.get('data_preview_truncated_before'),
                        'truncated_after': value.get('data_preview_truncated_after'),
                    }
            for key in ('data_preview', 'text', 'content', 'excerpt', 'snippet'):
                if isinstance(value.get(key), str):
                    source[key] = value[key]
            returned_sources.append(source)
        dispositions = [r for r in tables['subject_event_dispositions'] if r.get('event_id') == rr['result_id'] or rr['result_id'] in json.dumps(r, ensure_ascii=False)]
        data['native_results'].append({
            'condition': condition, 'task_id': rr['task_id'], 'task_kind': task.get('kind'),
            'result_id': rr['result_id'], 'task_result_DB_commit_seq': rr['commit_seq'],
            'task_state': task.get('state'), 'task_result': result, 'task_result_kind': rr['kind'],
            'origin_step_id': task.get('created_by_step_id'), 'origin_step_state': origin_step.get('state'),
            'T2_origin_decision_ref': origin_step.get('decision_ref'), 'T2_origin_commit_ref': origin_step.get('commit_ref'),
            'task_input_ref': task.get('input_ref'),
            'task_input_matches_origin_T2_decision': task.get('input_ref') == origin_step.get('decision_ref'),
            'native_intent': intent, 'frozen_native_input': frozen_input,
            'operation': operation, 'native_attempt': attempt, 'evidence_receipt': receipt,
            'native_result_equals_evidence_CAS_JSON': exact_result_equals_evidence,
            'normalized_receipt_equals_task_result_JSON': normalized_equals_result,
            'nested_observation_equals_raw_receipt_JSON': nested_observation_equals_raw,
            'native_chain_checks': chain_checks,
            'evidence_shape_note': 'history.query directly returns its observation; compat history.external.query returns a normalized wrapper whose observation is the raw evidence_ref and whose entire value matches normalized_ref',
            'input_plan_consumption': consumed, 'provider_wire_visibility': wire_visibility,
            'subject_event_dispositions': dispositions,
            'actual_result_answer_literal_counts': literal_counts(result.get('result')),
            'returned_source_anchors': returned_sources,
            'returned_record_array_counts': [{'json_path': p, 'count': len(v)} for p, v in walk(result.get('result'))
                                             if isinstance(v, list) and p.rsplit('.', 1)[-1] in ('events', 'hits', 'messages')],
            'interpretation_limit': 'A succeeded native call or visible task_result alone does not prove retrieval of relevant fixture facts or correct use in the final answer',
        })

baseline_data = arm_data.get('baseline')
isolation = None
if baseline_data:
    first_subject = next((r for r in requests if r.get('condition') == 'baseline' and r.get('purpose') == 'subject'), None)
    if first_subject:
        starting = load(baseline_data['installation'] / 'starting-context.json')
        text = '\n'.join(m['content'] for m in wire_objects[first_subject['relay_id']]['messages'])
        originals = [e for e in baseline_data['tables']['session_events'] if isinstance(e['data'], dict) and e['data'].get('synthetic') is True]
        isolation = {
            'first_subject_relay_id': first_subject['relay_id'], 'request_sha256': first_subject['request_sha256'],
            'request_bytes': first_subject['request_bytes'], 'starting_context': starting,
            'checked_fixture_events': [{k: e[k] for k in ('event_id', 'session_id', 'seq', 'role')} | {
                'fixture_text_sha256': sha(e['data']['text'].encode()),
                'first_subject_wire_occurrences': text.count(e['data']['text']), 'original_retained_in_Store': True,
            } for e in originals],
            'first_subject_wire_answer_literal_counts': literal_counts(text),
            'pointer_marker_occurrences': text.count('[context derivative/pointer to raw seq 1..4; not a new instruction]'),
            'scope': 'Literal and complete-fixture-message checks on the first subject request only; raw Store events remain available to native history.query',
        }

arms = []
for condition, data in arm_data.items():
    ra = report_arms.get(condition)
    native = data['native_results']
    starting = load(data['installation'] / 'starting-context.json')
    output = ra.get('committed', {}).get('output', {}).get('text') if ra else None
    arm = {
        'condition': condition, 'installation': rel(data['installation']), 'runner_arm': ra,
        'acceptance_json_exists': (data['installation'] / 'acceptance.json').exists(),
        'starting_context_path': rel(data['installation'] / 'starting-context.json'),
        'starting_context_capabilities': starting['capabilities'],
        'subject_request_attempt_count': len(data['tables']['subject_request_attempts']),
        'subject_step_state_counts': dict(collections.Counter(r['state'] for r in data['tables']['subject_steps'])),
        'subject_step_number_counts': dict(collections.Counter(r['step_no'] for r in data['tables']['subject_steps'])),
        'subject_attempts': data['traces'], 'protocol_rejections': data['rejections'],
        'committed_control_kind_counts': dict(collections.Counter(c['action']['kind'] for t in data['traces'] for c in t.get('committed_controls', []))),
        'task_kind_counts': dict(collections.Counter(r['data'].get('kind') for r in data['tables']['tasks'])),
        'native_result_count': len(native), 'native_succeeded_result_count': sum(n['task_result'].get('outcome') == 'succeeded' for n in native),
        'native_results_visible_to_provider_count': sum(any(v['entire_task_result_JSON_semantically_equal'] for v in n['provider_wire_visibility']) for n in native),
        'native_results_presented_in_committed_step_count': sum(any(c['T2_commit_ref'] and c['event_payload_equals_task_result'] for c in n['input_plan_consumption']) for n in native),
        'native_retrieval_details_path': rel(OUT / 'native-retrievals.json'),
        'actual_retrieved_answer_literal_counts': {s: sum(n['actual_result_answer_literal_counts'][s] for n in native) for s in ANSWER_LITERALS},
        'final_committed_output_text': output, 'final_output_literal_counts': literal_counts(output) if output is not None else None,
        'judge_relay_ids': [r['relay_id'] for r in requests if r.get('condition') == condition and r.get('purpose') == 'judge'],
        'usage_and_timing': aggregate([r for r in requests if r.get('condition') == condition]),
        'observed_facts': data.get('facts'),
        'raw_runner_gate_preserved': {k: ra.get(k) for k in ('status', 'effective', 'used_expected_reader', 'history_query_reads', 'native_obelisk_cli_reads')} if ra else None,
        'assessment_note': 'Structural observation only. Preserve raw judge/gates; relevance and factual support must be checked against actual native result content and cited sources.',
    }
    arms.append(arm)

model = load(ROOT / 'model-service/service-manifest.json')
preflight_path = ROOT / 'model-service/service-preflight.json'
preflight = load(preflight_path) if preflight_path.exists() else None
if preflight:
    assert sha(base64.b64decode(preflight['models_response_raw_base64'])) == preflight['models_response_sha256']
    assert preflight['models']['data'][0]['meta']['n_ctx'] == model['server']['context_tokens']
assert sha((ROOT / 'model-service/start-server.sh').read_bytes()) == model['server']['startup_script_sha256']
service_final = None
final_root = ROOT / 'model-service/final'
final_names = ['metadata/post-run-pre-stop-observation.json', 'metadata/operator-service-stop.json',
               'metadata/server-exit.json', 'metadata/post-run-resource-summary.json', 'logs/resources.jsonl',
               'logs/server.log', 'local-transfer-verification.json']
if all((final_root / n).exists() for n in final_names):
    before_stop = load(final_root / final_names[0])
    stop = load(final_root / final_names[1])
    exit_record = load(final_root / final_names[2])
    resources = load(final_root / final_names[3])
    samples = [json.loads(line) for line in (final_root / 'logs/resources.jsonl').read_text().splitlines()]
    assert len(samples) == resources['resource_samples']
    assert max(s.get('rss_kib') or 0 for s in samples) == resources['max_observed_rss_kib']
    assert min(s['MemAvailable_kib'] for s in samples) == resources['min_observed_available_kib']
    service_final = {
        'current_observed_state': 'stopped_after_owned_cleanup',
        'server_PID_identity_is_historical': True,
        'files': [file_info(final_root / n) for n in final_names],
        'post_run_pre_stop_observed_at_utc': before_stop['observed_at_utc'],
        'post_run_pre_stop_health': before_stop['health'],
        'post_run_pre_stop_server_exists': before_stop['processes']['server']['exists'],
        'owned_operator_stop': stop, 'supervisor_exit_record': exit_record,
        'resource_summary': resources, 'resource_sample_aggregates_independently_recomputed': True,
        'remote_transfer_verification': load(final_root / 'local-transfer-verification.json'),
        'operator_stop_followed_runner_exit': runner_exit is not None and runner_exit['finished_at_utc'] < stop['requested_at_utc'],
        'timing_limit': 'Server log records long prompt processing and later request cancellations; it does not supply successful provider usage for the timed-out requests',
    }
support_review = None
if baseline_data and 'candidate' in arm_data:
    originals = [e for e in baseline_data['tables']['session_events'] if isinstance(e['data'], dict) and e['data'].get('synthetic') is True]
    source_messages = []
    for path in sorted((arm_data['candidate']['installation'] / 'synthetic-codex').rglob('*.jsonl')):
        sid = None
        for line_no, line in enumerate(path.read_text().splitlines(), 1):
            obj = json.loads(line)
            if obj['type'] == 'session_meta':
                sid = obj['payload']['id']
            elif obj['type'] == 'event_msg':
                source_messages.append({'file': rel(path), 'line': line_no, 'session_id': sid,
                                        'timestamp': obj['timestamp'], 'type': obj['payload']['type'],
                                        'text': obj['payload']['message']})
    original_texts = [e['data']['text'] for e in originals]
    assert sorted(original_texts) == sorted(m['text'] for m in source_messages)
    review_calls = []
    for condition, ad in arm_data.items():
        for native in ad['native_results']:
            raw_input = (native.get('native_intent') or {}).get('payload', {}).get('definition', {}).get('input')
            query = (raw_input or {}).get('query') if condition == 'baseline' else (raw_input or {}).get('request', {}).get('query')
            review_calls.append({
                'condition': condition, 'task_id': native['task_id'], 'result_id': native['result_id'],
                'raw_native_input': raw_input, 'frozen_native_input': native['frozen_native_input'],
                'outcome': native['task_result'].get('outcome'), 'returned_record_array_counts': native['returned_record_array_counts'],
                'returned_known_fixture_event_ids': [s['anchors']['event_id'] for s in native['returned_source_anchors'] if s.get('raw_event_is_synthetic_fixture')],
                'query_literal_occurrences_in_fixture': [text.count(query) for text in original_texts] if isinstance(query, str) else None,
                'actual_result_answer_literal_counts': native['actual_result_answer_literal_counts'],
                'provider_visible': native['provider_wire_visibility'], 'subject_event_dispositions': native['subject_event_dispositions'],
            })
    support_review = {
        'scope': 'This retained run only; exact returned records and fixture text checks, not a revised scoring rubric',
        'baseline_Store_original_texts_equal_candidate_source_file_texts': True,
        'baseline_fixture_event_ids': [e['event_id'] for e in originals],
        'candidate_source_messages': source_messages, 'native_calls': review_calls,
        'committed_reply_controls_by_arm': {a['condition']: a['committed_control_kind_counts'].get('reply', 0) for a in arms},
        'judge_calls_by_arm': {a['condition']: len(a['judge_relay_ids']) for a in arms},
        'assessment': [
            'The native execution and receipt chains are real: three baseline history.query results and two candidate history.external.query results are retained.',
            'The baseline search returns this run question and its own call records, not fixture seq1..4. Two list calls use issue/patch filters and return empty events.',
            'Both candidate live searches return mode literal_index_fallback with empty hits; their CLI processes exit0 and are reaped with readers drained.',
            'None of these actual result payloads contains the checked fixture-answer literals. Both arms terminate without a final committed reply or judge.',
            'This establishes execution of the native paths, not successful retrieval of the requested historical answer or a model efficacy/token-saving result.',
        ],
    }
unmatched = [r['relay_id'] for r in requests if r['method'] == 'POST' and r.get('purpose') is None]
successful_without_CAS = [r['relay_id'] for r in requests if r['observed_model_output'] and not r.get('response_CAS_raw_response_bytes_match')]
summary = {
    'schema_version': 3, 'analysis_at_utc': NOW.isoformat(), 'phase': 'provisional_snapshot' if args.snapshot else 'final_retained_evidence',
    'run_id': run.name, 'run_identity': identity, 'model': model['model'], 'server': model['server'],
    'model_service_final': service_final,
    'runner_exit': runner_exit, 'runner_report': report,
    'strict_result': report.get('status') if report and not args.snapshot else None,
    'complete': report.get('complete') if report and not args.snapshot else None,
    'cleanup_ok': report.get('cleanup_ok') if report and not args.snapshot else None,
    'terminal_evidence_available': report is not None and runner_exit is not None,
    'final_conclusion_authorized_by_this_analysis': not args.snapshot,
    'analysis_scope': 'One paired-smoke invocation, with dependent subject continuations. No model calls or resampling by this analyzer.',
    'snapshot_limit': 'Read-only SQLite transactions are internally consistent, but live wire/CAS/Store snapshots are not cross-process atomic' if args.snapshot else None,
    'transport': {'whole_response_buffering': True, 'native_latency_benchmark': False,
                  'all_observed_forward_retries': sorted(set(r['forward_retries'] for r in requests if 'forward_retries' in r)),
                  'pending_delivery_requests': [r['relay_id'] for r in requests if r['delivery_status'] == 'pending_or_no_delivery_receipt'],
                  'remote_receipts_without_local_delivery_receipt': [r['relay_id'] for r in requests if r['delivery_status'] == 'remote_receipt_retained_delivery_unobserved'],
                  'unattributed_posts': unmatched, 'model_responses_without_CAS_match': successful_without_CAS,
                  'timing_note': 'Includes transfer, queue, polling and preflight. Remote attempt time for failed requests is not evidence of inference; prefix-cache state is retained.'},
    'usage_and_timing_all_posts': aggregate(requests),
    'context_capacity': {
        'configured_tokens': model['server']['context_tokens'],
        'both_arm_profiles_match': all(a['starting_context_capabilities']['max_context_tokens'] == model['server']['context_tokens'] for a in arms),
        'all_observed_capacity_checks_fit': all(r['capacity'].get('fits') is True for r in requests if r['method'] == 'POST' and r.get('capacity') and 'prompt_tokens' in r['capacity']),
        'largest_prompt_plus_requested_budget_observed': max((r['capacity']['prompt_plus_max_tokens'] for r in requests if r.get('capacity') and 'prompt_plus_max_tokens' in r['capacity']), default=None),
        'note': 'Template/tokenize counts for failed requests are capacity diagnostics, not actual provider token usage',
    },
    'retrieval_support_review_path': rel(OUT / 'retrieval-support-review.json') if support_review else None,
    'baseline_prompt_isolation_path': rel(OUT / 'baseline-prompt-isolation.json') if isolation else None,
    'db_audit_path': rel(OUT / 'db-audit.json'), 'native_retrievals_path': rel(OUT / 'native-retrievals.json'),
    'cas_index_path': rel(OUT / 'cas-index.json'), 'arms': arms, 'requests': requests,
    'limitations': [
        'Do not infer a successful answer from a reader count, successful empty result, or judge label alone.',
        'Task_result visibility, committed input handling, relevant-source retrieval and final answer support are distinct observations.',
        'Unknown/failed request usage stays null; aggregate token values are observed subtotals when any response is missing.',
        'Qwen3-4B Q4_K_M with q8 KV is a separate small-model experiment, not a large-model baseline or a single-variable comparison to 1.7B.',
        'Original CAS BLAKE3 references are retained; this analyzer independently checks bytes and SHA256, not BLAKE3.',
    ],
}
dump('result-summary.json', summary)
dump('db-audit.json', db_audits)
dump('native-retrievals.json', [n for a in arm_data.values() for n in a['native_results']])
dump('protocol-rejections.json', [r for a in arm_data.values() for r in a['rejections']])
dump('baseline-prompt-isolation.json', isolation)
dump('cas-index.json', cas_index)
dump('cas-reference-verification.json', list(ref_checks.values()))
dump('cas-export-map.json', cas_exports)
dump('model-service-final-summary.json', service_final)
dump('retrieval-support-review.json', support_review)
print(json.dumps({'summary': str(OUT / 'result-summary.json'), 'phase': summary['phase'],
                  'terminal_evidence_available': summary['terminal_evidence_available'], 'requests': len(requests),
                  'unattributed_posts': unmatched, 'model_responses_without_CAS_match': successful_without_CAS,
                  'arms': [{k: a[k] for k in ('condition', 'subject_request_attempt_count', 'native_result_count',
                                             'native_succeeded_result_count', 'native_results_visible_to_provider_count',
                                             'native_results_presented_in_committed_step_count', 'subject_step_state_counts')} for a in arms]},
                 ensure_ascii=False, indent=2))
