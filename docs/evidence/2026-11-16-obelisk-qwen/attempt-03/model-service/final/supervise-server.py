#!/usr/bin/env python3
import json,os,signal,subprocess,time,hashlib
from pathlib import Path
from datetime import datetime,timezone
ROOT=Path('/workspace/shenko-obelisk-qwen4b-20261116')
def utc():return datetime.now(timezone.utc).isoformat()
def mem():
 vals={}
 for line in Path('/proc/meminfo').read_text().splitlines():
  k,v=line.split(':',1)
  if k in ['MemTotal','MemAvailable','SwapTotal','SwapFree']:vals[k+'_kib']=int(v.split()[0])
 return vals
def dump(name,value):(ROOT/name).write_text(json.dumps(value,indent=2)+'\n')
assert (ROOT/'metadata/model-download.ok').exists()
assert not Path('/proc/48637').exists(),'Original 1.7B PID still exists; do not load concurrently'
initial=mem()
weight_bytes=(ROOT/'downloads/Qwen3-4B-Q4_K_M.gguf').stat().st_size
kv_bytes=36*8*128*16384*2*34//32
estimated_total=weight_bytes+kv_bytes+448*1024*1024
plan={'observed_at_utc':utc(),'initial_memory':initial,'weights_bytes':weight_bytes,'q8_0_kv_estimate_bytes':kv_bytes,'other_and_cache_reserve_bytes':448*1024*1024,'estimated_total_bytes':estimated_total,'available_bytes':initial['MemAvailable_kib']*1024,'max_observed_rss_before_owned_termination_kib':4*1024*1024,'min_available_before_owned_termination_kib':128*1024,'prior_1_7b_pid_absent':True,'automatic_restart':False,'cache_ram_mib':64,'repack':False}
dump('metadata/resource-plan.json',plan)
assert initial['MemAvailable_kib']*1024>=estimated_total,'Insufficient observed memory for resource plan'
with (ROOT/'logs/server.log').open('wb') as log:
 child=subprocess.Popen(['bash',str(ROOT/'start-server.sh')],stdin=subprocess.DEVNULL,stdout=log,stderr=subprocess.STDOUT,cwd=ROOT)
 (ROOT/'metadata/server.pid').write_text(str(child.pid)+'\n')
 dump('metadata/owned-process.json',{'supervisor_pid':os.getpid(),'server_pid':child.pid,'started_at_utc':utc(),'start_script_sha256':hashlib.sha256((ROOT/'start-server.sh').read_bytes()).hexdigest(),'auto_restart':False})
 reason=None
 with (ROOT/'logs/resources.jsonl').open('a') as resources:
  while child.poll() is None:
   current=mem();rss=None
   try:
    for line in Path('/proc/'+str(child.pid)+'/status').read_text().splitlines():
     if line.startswith('VmRSS:'):rss=int(line.split()[1])
   except FileNotFoundError:pass
   entry={'utc':utc(),'pid':child.pid,'rss_kib':rss,**current};resources.write(json.dumps(entry)+'\n');resources.flush()
   if (rss is not None and rss>4*1024*1024) or current['MemAvailable_kib']<128*1024:
    reason={'kind':'owned_resource_limit_observed','observation':entry,'note':'This is an explicit resource guard, not a kernel OOM diagnosis'}
    dump('metadata/resource-stop.json',reason);child.terminate();break
   time.sleep(2)
  try:code=child.wait(timeout=10)
  except subprocess.TimeoutExpired:
   child.kill();code=child.wait();reason={'kind':'owned_cleanup_after_resource_stop','previous':reason}
 dump('metadata/server-exit.json',{'server_pid':child.pid,'ended_at_utc':utc(),'returncode':code,'signal':-code if code<0 else None,'observed_guard_reason':reason,'memory_after':mem(),'kernel_cause_not_inferred':True})
