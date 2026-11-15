#!/usr/bin/env python3
"""Read-only analysis of completed real-model evidence; never resamples."""
import hashlib,json,sqlite3
from datetime import datetime,timezone
from pathlib import Path

ROOT=Path('/workspace/scratch/549c30a35cef/obelisk-qwen-real-20261116')
OUT=ROOT/'analysis';OUT.mkdir(exist_ok=True)
def sha(data):return hashlib.sha256(data).hexdigest()
def load(path):return json.loads(path.read_text())
def dump(name,value):(OUT/name).write_text(json.dumps(value,ensure_ascii=False,indent=2)+'\n')
def relative(path):return str(path.relative_to(ROOT))
def read_ref(reference):
 path=Path(reference['locator'].removeprefix('file://'))
 assert path.is_relative_to(ROOT)
 raw=path.read_bytes();assert len(raw)==reference['bytes']
 return path,json.loads(raw)
report_path=next((ROOT/'acceptance').glob('run-*/report.json'))
report=load(report_path)
requests=[]
by_response={}
for directory in sorted((ROOT/'relay/queue').iterdir()):
 request=load(directory/'request.json');remote=load(directory/'remote.json');delivery=load(directory/'delivery.json')
 req=(directory/'request.body').read_bytes();body=(directory/'response.body').read_bytes()
 assert sha(req)==request['request_sha256']==remote['request_sha256']
 assert sha(body)==remote['response_sha256']
 response=json.loads(body)
 row={'relay_id':directory.name,'method':request['method'],'path':request['path'],
  'request_path':relative(directory/'request.body'),'response_path':relative(directory/'response.body'),
  'headers_path':relative(directory/'response.headers'),'request_sha256':sha(req),'response_sha256':sha(body),
  'request_bytes':len(req),'response_bytes':len(body),'http_status':remote['http_status'],'curl_exit':remote['curl_exit'],
  'content_type':remote['content_type'],'inference_attempts':remote['inference_attempts'] if request['method']=='POST' else 0,
  'inference_retries':0,'local_relay_elapsed_ms':delivery['relay_elapsed_ms'],
  'remote_inference_elapsed_ms':remote['inference_elapsed_ms'],'remote_curl_time_total_seconds':remote['curl_time_total_seconds'],
  'remote_preflight_and_other_ms':remote['remote_elapsed_ms']-remote['inference_elapsed_ms'],
  'relay_minus_remote_inference_ms':delivery['relay_elapsed_ms']-remote['inference_elapsed_ms'],
  'delivery_status':delivery['status'],'capacity':remote['capacity'],'request_sha_verified_across_machines':True,
  'response_sha_verified_across_machines':True,'token_usage_source':'unmodified llama.cpp response' if request['method']=='POST' else None}
 if request['method']=='POST':
  payload=json.loads(req)
  row.update({'request_options':{k:v for k,v in payload.items() if k!='messages'},
    'stream_requested':payload.get('stream',False),'usage':response['usage'],
    'finish_reason':response['choices'][0]['finish_reason'],'raw_assistant_content':response['choices'][0]['message']['content'],
    'server_timings':response.get('timings'),'system_fingerprint':response.get('system_fingerprint')})
  assert row['capacity']['prompt_tokens']==response['usage']['prompt_tokens']
  row['capacity']['matches_actual_response_prompt_tokens']=True
  by_response[row['response_sha256']]=row
 requests.append(row)

arms=[];db_audits=[];cas_exports=[]
for arm in report['arms']:
 installation=Path(arm['installation'])
 db=sqlite3.connect('file:'+str(installation/'shenko.db')+'?mode=ro',uri=True);db.row_factory=sqlite3.Row
 audit={'condition':arm['condition'],'db_path':relative(installation/'shenko.db'),'tables':{}}
 for table in ['subject_request_attempts','subject_steps','tasks','task_results','commit_log']:
  rows=[]
  for raw in db.execute('SELECT * FROM '+table):
   value=dict(raw)
   for key in ['data','input_plan_ref']:
    if isinstance(value.get(key),str):
     try:value[key]=json.loads(value[key])
     except json.JSONDecodeError:pass
   rows.append(value)
  audit['tables'][table]=rows
 db.close();db_audits.append(audit)
 attempts=audit['tables']['subject_request_attempts'];assert len(attempts)==1
 attempt=attempts[0]['data']
 subject_path,subject_result=read_ref(attempt['terminal']['result_ref'])
 judge_path,judge_result=read_ref(arm['judge_receipt']['result_ref'])
 judge_request_path,judge_request=read_ref(arm['judge_receipt']['request_ref'])
 facts_path,facts=read_ref(arm['facts_ref'])
 installation_path,installation_value=read_ref(arm['installation_ref'])
 subject=by_response[sha(subject_result['response']['raw_response'].encode())]
 judge=by_response[sha(judge_result['response']['raw_response'].encode())]
 subject['condition']=arm['condition'];subject['purpose']='subject';subject['store_request_id']=attempt['request_id']
 judge['condition']=arm['condition'];judge['purpose']='judge';judge['store_request_id']=arm['judge_receipt']['request_id']
 assert subject['raw_assistant_content']==subject_result['response']['content']
 assert judge['raw_assistant_content']==judge_result['response']['content']==arm['judge_text']
 assert facts['retrievals']==arm['committed']['consumed_results']==[]
 for kind,path,value in [('subject_result',subject_path,subject_result),('judge_result',judge_path,judge_result),('judge_request',judge_request_path,judge_request),('observed_facts',facts_path,facts),('installation',installation_path,installation_value)]:
  name=arm['condition']+'-'+kind+'.json';dump(name,value)
  cas_exports.append({'condition':arm['condition'],'kind':kind,'cas_path':relative(path),'cas_sha256':sha(path.read_bytes()),'export':str(Path('analysis')/name)})
 committed_steps=[r['data'] for r in audit['tables']['subject_steps'] if r['state']=='committed'];assert len(committed_steps)==1
 step=committed_steps[0]
 prompt_payload=load(ROOT/subject['request_path'])
 prompt='\n'.join(m['content'] for m in prompt_payload['messages'])
 subject_output=arm['committed']['output']['text']
 usage={key:subject['usage'][key]+judge['usage'][key] for key in ['prompt_tokens','completion_tokens','total_tokens']}
 usage['cached_prompt_tokens']=sum(r['usage'].get('prompt_tokens_details',{}).get('cached_tokens',0) for r in [subject,judge])
 row={k:arm[k] for k in ['condition','status','effective','used_expected_reader','history_query_reads','native_obelisk_cli_reads','elapsed_ms','judge_error','judge_text','judge_receipt']}
 row.update({'strict_arm_pass':arm['effective'] is True and arm['used_expected_reader'] is True,
  'consumed_results':arm['committed']['consumed_results'],'retrievals':facts['retrievals'],
  'committed_output_text':subject_output,'committed_control_kinds':[c['action']['kind'] for c in arm['committed']['output']['controls']],
  'subject_relay_id':subject['relay_id'],'judge_relay_id':judge['relay_id'],'usage_subject_and_judge':usage,
  'subject_usage':subject['usage'],'judge_usage':judge['usage'],
  'remote_inference_elapsed_ms_subject_and_judge':subject['remote_inference_elapsed_ms']+judge['remote_inference_elapsed_ms'],
  'relay_elapsed_ms_subject_and_judge':subject['local_relay_elapsed_ms']+judge['local_relay_elapsed_ms'],
  'store_request_id':attempt['request_id'],'request_attempt_count':len(attempts),'subject_terminal_outcome':attempt['terminal']['outcome'],
  'subject_request_created_commit':attempt['created_commit'],'subject_result_recorded_commit':attempt['terminal']['recorded_commit'],
  'T1_input_plan_ref':step['input_plan_ref'],'E1_model_result_ref':step['model_result_ref'],'E1_model_outcome_ref':step['model_outcome_ref'],
  'T2_decision_ref':step['decision_ref'],'T2_commit_ref':step['commit_ref'],
  'subject_and_judge_CAS_raw_response_matches_relay_bytes':True,
  'subject_prompt_literal_presence':{s:prompt.count(s) for s in ['src/auth/refresh.ts','tests/auth/refresh-race.test.ts','generation','Lax','history.external.query','history.query']},
  'task_kinds':[r['data'].get('kind') for r in audit['tables']['tasks']],
  'task_result_row_count':len(audit['tables']['task_results'])})
 if arm['condition']=='candidate':
  row['candidate_output_vs_judge_checks']={
   'output_mentions_auth_service_py':'auth_service.py' in subject_output,
   'output_mentions_database_py':'database.py' in subject_output,
   'output_mentions_cache_service_py':'cache_service.py' in subject_output,
   'output_mentions_correct_refresh_ts':'src/auth/refresh.ts' in subject_output,
   'output_mentions_correct_race_test_ts':'tests/auth/refresh-race.test.ts' in subject_output,
   'output_mentions_generation':'generation' in subject_output,
   'judge_claims_correct_refresh_ts':'src/auth/refresh.ts' in arm['judge_text'],
   'judge_claims_generation':'generation' in arm['judge_text'],
   'judge_said_effective_despite_empty_retrievals':arm['effective'] and facts['retrievals']==[]}
 arms.append(row)

model=load(ROOT/'model-service/service-manifest.json')
summary={'schema_version':1,'analyzed_at_utc':datetime.now(timezone.utc).isoformat(),
 'scenario':report['scenario'],'run_id':report_path.parent.name,'strict_result':report['status'],
 'complete':report['complete'],'cleanup_ok':report['cleanup_ok'],'passed':False,
 'report_path':relative(report_path),'runner_exit':load(ROOT/'runner-exit.json'),'identity':load(ROOT/'run-identity.json'),
 'model':model['model'],'server':model['server'],'model_service_manifest':'model-service/service-manifest.json',
 'corpus_digest':report['corpus_digest'],'planned_arms':2,'performed_pairs':1,'resamples':0,
 'transport':{'mechanism':'same-namespace loopback HTTP server plus exact UTF-8 files via Box connector; raw response bytes transferred with base64/gzip and SHA256 checks',
  'native_provider_latency_benchmark':False,'all_request_bytes_verified':True,'all_response_bytes_verified':True,
  'real_models_gets':sum(r['method']=='GET' for r in requests),'actual_inference_posts':sum(r['method']=='POST' for r in requests),
  'all_http_statuses':[r['http_status'] for r in requests],'all_curl_exit_codes':[r['curl_exit'] for r in requests],
  'inference_retries':0,'actual_requests_were_streamed':False,'SSE_note':'No SSE request occurred; buffering path is present but not exercised in this pair',
  'credential_headers_sent':False,'provider_timeout_seconds':180,'remote_curl_max_time_seconds':175,
  'latency_note':'Arm and relay durations include queueing, connector transfers, polling, and template/tokenization preflight; prefix-cache state was not reset between requests'},
 'context_preflight':{'method':'unchanged request file to /apply-template, then returned prompt to /tokenize; diagnostics perform no inference',
  'context_tokens':16384,'kv_type_k':'f16','kv_type_v':'f16','all_fit':all(r['capacity']['fits'] for r in requests if r['method']=='POST'),
  'all_counts_equal_actual_response_prompt_tokens':all(r['capacity']['matches_actual_response_prompt_tokens'] for r in requests if r['method']=='POST'),
  'largest_prompt_plus_requested_max_tokens':max(r['capacity']['prompt_plus_max_tokens'] for r in requests if r['method']=='POST')},
 'findings':[
  'Both arms returned a committed reply and completed real LLM judging, but neither used its required history reader.',
  'The candidate reply invented Python files and lock/cache changes absent from the synthetic corpus.',
  'Both real small-model judge calls returned EFFECTIVE despite empty retrievals; the candidate judge described correct facts not present in its actual candidate output.',
  'The independent used_expected_reader gate correctly prevented final passed_smoke.',
  'The baseline original subject request already contained corpus facts through the real Store context projection; its factual reply does not demonstrate native history.query use.',
  'No actual Obelisk external query was dispatched in this pair. This run supplies no evidence of successful external retrieval, efficacy, or token savings.'
 ],
 'limitations':['One unseeded bounded pair only.','Qwen3-1.7B Q8_0 is not equivalent to Qwen3.8-27B baseline.','LLM judge is empirically unreliable for these two samples.','Transport and prefix caching prevent native latency comparison.','No prompt, output, reader result, token usage, judge text, or strict condition was changed to obtain success.'],
 'arms':arms,'requests':requests}
dump('db-audit.json',db_audits);dump('cas-export-map.json',cas_exports);dump('result-summary.json',summary)
inventory=[]
for path in sorted(ROOT.rglob('*')):
 if path.is_file() and path.name not in ['inventory.sha256.json','full-evidence.tar.gz']:
  data=path.read_bytes();inventory.append({'path':relative(path),'bytes':len(data),'sha256':sha(data)})
dump('inventory.sha256.json',inventory)
print(json.dumps({'summary':str(OUT/'result-summary.json'),'strict_result':summary['strict_result'],'complete':summary['complete'],'cleanup_ok':summary['cleanup_ok'],'arms':[{'condition':a['condition'],'used_expected_reader':a['used_expected_reader'],'judge_effective':a['effective'],'usage':a['usage_subject_and_judge']} for a in arms],'files_hashed':len(inventory)},ensure_ascii=False,indent=2))
