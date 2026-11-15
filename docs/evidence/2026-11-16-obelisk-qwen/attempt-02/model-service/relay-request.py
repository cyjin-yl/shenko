#!/usr/bin/env python3
"""One exact HTTP attempt. Inspect capacity without modifying inference bytes."""
import base64,gzip,hashlib,json,re,subprocess,sys,time
from pathlib import Path
from datetime import datetime,timezone
ROOT=Path('/workspace/shenko-obelisk-qwen-20261116')
def utc(): return datetime.now(timezone.utc).isoformat()
def sha(b): return hashlib.sha256(b).hexdigest()
def dump(p,v):
 t=p.with_name(p.name+'.tmp');t.write_text(json.dumps(v,ensure_ascii=False,indent=2)+'\n');t.replace(p)
identity=sys.argv[1]
assert re.fullmatch(r'[0-9]{4}-[a-f0-9]{32}',identity)
prefix=ROOT/'requests'/('relay-'+identity)
meta=json.loads(Path(str(prefix)+'.transport.json').read_text())
assert meta['id']==identity
assert (meta['method'],meta['path']) in [('GET','/v1/models'),('POST','/v1/chat/completions')]
request=Path(str(prefix)+'.request.body')
raw=request.read_bytes()
assert sha(raw)==meta['request_sha256']
assert '\n' not in meta['content_type'] and '\r' not in meta['content_type']
with Path(str(prefix)+'.execution.lock').open('x') as f:f.write(utc()+'\n')
started=utc();began=time.monotonic()
capacity=None
if meta['method']=='POST':
 # Read-only template/tokenization diagnostics. The inference file stays unchanged.
 template_path=Path(str(prefix)+'.template.response.json')
 c=subprocess.run(['curl','--noproxy','*','-sS','--connect-timeout','5','--max-time','15','-H','Content-Type: application/json','--data-binary','@'+str(request),'http://127.0.0.1:18831/apply-template','-o',str(template_path)],capture_output=True)
 if c.returncode==0:
  try:
   template=json.loads(template_path.read_text())['prompt']
   token_request=Path(str(prefix)+'.tokenize.request.json')
   token_request.write_text(json.dumps({'content':template,'add_special':False,'parse_special':True},ensure_ascii=False))
   token_response=Path(str(prefix)+'.tokenize.response.json')
   t=subprocess.run(['curl','--noproxy','*','-sS','--connect-timeout','5','--max-time','15','-H','Content-Type: application/json','--data-binary','@'+str(token_request),'http://127.0.0.1:18831/tokenize','-o',str(token_response)],capture_output=True)
   if t.returncode==0:
    tokens=json.loads(token_response.read_text())['tokens']; request_data=json.loads(raw)
    budget=request_data.get('max_tokens',request_data.get('max_completion_tokens',1024))
    capacity={'prompt_tokens':len(tokens),'requested_max_tokens':budget,'configured_context':16384,'prompt_plus_max_tokens':len(tokens)+budget,'fits':len(tokens)+budget<=16384,'source':'real server /apply-template followed by /tokenize; no inference performed','request_sha256':sha(raw)}
  except Exception as e:capacity={'preflight_error':str(e),'request_sha256':sha(raw)}
 else:capacity={'preflight_curl_exit':c.returncode,'request_sha256':sha(raw)}
 dump(Path(str(prefix)+'.capacity.json'),capacity)
headers=Path(str(prefix)+'.response.headers'); body=Path(str(prefix)+'.response.body')
argv=['curl','--noproxy','*','-sS','--no-buffer','--connect-timeout','5','--max-time','175','-D',str(headers),'-o',str(body),'-w','%{http_code}\n%{time_total}\n','-X',meta['method']]
if meta['method']=='POST':argv+=['-H','Content-Type: '+meta['content_type'],'--data-binary','@'+str(request)]
argv+=['http://127.0.0.1:18831'+meta['path']]
inference_started=utc();inference_began=time.monotonic()
result=subprocess.run(argv,capture_output=True)
Path(str(prefix)+'.curl.stderr').write_bytes(result.stderr)
Path(str(prefix)+'.curl.metrics.raw').write_bytes(result.stdout)
parts=result.stdout.decode('ascii',errors='replace').splitlines()
status=int(parts[0]) if parts and parts[0].isdigit() else 0
raw_body=body.read_bytes() if body.exists() else b''
raw_headers=headers.read_bytes() if headers.exists() else b''
content_type='application/octet-stream'
for block in raw_headers.split(b'\r\n\r\n'):
 if block.startswith(b'HTTP/'):
  for line in block.split(b'\r\n')[1:]:
   if line.lower().startswith(b'content-type:'):content_type=line.split(b':',1)[1].strip().decode('latin-1')
compressed=gzip.compress(raw_body,mtime=0)
Path(str(prefix)+'.response.body.gz').write_bytes(compressed)
receipt={'id':identity,'method':meta['method'],'path':meta['path'],'started_at_utc':started,'inference_started_at_utc':inference_started,'finished_at_utc':utc(),'remote_elapsed_ms':(time.monotonic()-began)*1000,'inference_elapsed_ms':(time.monotonic()-inference_began)*1000,'curl_time_total_seconds':float(parts[1]) if len(parts)>1 else None,'curl_exit':result.returncode,'http_status':status,'content_type':content_type,'request_sha256':sha(raw),'request_bytes':len(raw),'response_sha256':sha(raw_body),'response_bytes':len(raw_body),'headers_sha256':sha(raw_headers),'response_gzip_sha256':sha(compressed),'response_gzip_bytes':len(compressed),'inference_attempts':1,'inference_retries':0,'curl_max_time_seconds':175,'capacity':capacity,'model_service_manifest':'/workspace/shenko-obelisk-qwen-20261116/service-manifest.json'}
assert request.read_bytes()==raw
receipt_path=Path(str(prefix)+'.receipt.json');dump(receipt_path,receipt)
Path(str(prefix)+'.response.ready').touch()
print(json.dumps(receipt,ensure_ascii=False))
