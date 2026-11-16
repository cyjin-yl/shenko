#!/usr/bin/env python3
"""Keep the local relay and real runner in the same exec network namespace."""
import hashlib,json,os,subprocess,sys,threading,urllib.request
from pathlib import Path

ROOT=Path('/workspace/scratch/549c30a35cef/obelisk-qwen-real-20261116')
sys.path.insert(0,str(ROOT/'relay'))
from relay_server import Relay,sha,utc,write_json

server=Relay(('127.0.0.1',18832),ROOT/'relay')
thread=threading.Thread(target=server.serve_forever,kwargs={'poll_interval':0.2},daemon=True)
thread.start()
write_json(ROOT/'relay/server.json',{'pid':os.getpid(),'host':'127.0.0.1','port':18832,'started_at_utc':utc(),'source_sha256':sha((ROOT/'relay/relay_server.py').read_bytes()),'upstream':'Box 127.0.0.1:18831','inference_retries':0,'body_transformations':'none','streaming':'buffered without changing SSE bytes','namespace':'relay and runner share this persistent exec session; other exec calls use file queue only'})
print(json.dumps({'event':'relay_ready_for_real_models_probe','pid':os.getpid(),'utc':utc()}),flush=True)
try:
 with urllib.request.urlopen('http://127.0.0.1:18832/v1/models',timeout=90) as response:
  body=response.read()
  assert response.status==200,response.status
  (ROOT/'relay/models-probe.body').write_bytes(body)
  (ROOT/'relay/models-probe.headers').write_text(str(response.headers))
  write_json(ROOT/'relay/models-probe.json',{'status':response.status,'content_type':response.headers.get('Content-Type'),'body_sha256':sha(body),'body_bytes':len(body),'observed_at_utc':utc(),'source':'real remote GET through the same relay route'})
 print(json.dumps({'event':'real_models_probe_passed','body_sha256':sha(body),'utc':utc()}),flush=True)
 child=subprocess.Popen([sys.executable,str(ROOT/'run_acceptance.py')],cwd=ROOT)
 result=child.wait()
 print(json.dumps({'event':'runner_driver_finished','driver_exit_code':result,'utc':utc()}),flush=True)
 if not (ROOT/'runner-exit.json').exists():
  write_json(ROOT/'runner-exit.json',{'driver_exit_code':result,'status':'driver_failed_before_runner_completion','utc':utc()})
finally:
 server.shutdown()
 server.server_close()
