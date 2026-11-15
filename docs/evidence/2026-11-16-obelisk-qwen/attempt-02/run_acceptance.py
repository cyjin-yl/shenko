#!/usr/bin/env python3
"""Execute the already-built, hash-bound acceptance binary through an open fd."""
import hashlib,json,os,subprocess,time
from datetime import datetime,timezone
from pathlib import Path

ROOT=Path('/workspace/scratch/549c30a35cef/obelisk-qwen-round2-20261116')
REPO=Path('/workspace/scratch/549c30a35cef/shenko-obelisk')
BINARY=Path('/workspace/scratch/d013f3e3b824/shenko-target/debug/examples/obelisk_acceptance')
EXPECTED_SHA='f043116ebbb25f98bda4a4619474e1e2b22a9b9e2ff2411052cecc394f985fdb'
EXPECTED_HEAD='012370b76735daaa97623bbea4401313b3d9be06'
def utc():return datetime.now(timezone.utc).isoformat()
def write(name,data):(ROOT/name).write_text(json.dumps(data,ensure_ascii=False,indent=2)+'\n')
fd=os.open(BINARY,os.O_RDONLY)
digest=hashlib.sha256()
while chunk:=os.read(fd,1024*1024):digest.update(chunk)
actual=digest.hexdigest();assert actual==EXPECTED_SHA,(actual,EXPECTED_SHA)
os.lseek(fd,0,os.SEEK_SET)
head=subprocess.check_output(['git','-C',str(REPO),'rev-parse','HEAD'],text=True).strip()
assert head==EXPECTED_HEAD,(head,EXPECTED_HEAD)
stat=os.fstat(fd)
environment=os.environ.copy()
environment.pop('SHENKO_LOCAL_MODEL_API_KEY',None)
environment.update({'SHENKO_USE_LOCAL_MODEL':'1','SHENKO_LOCAL_MODEL_URL':'http://127.0.0.1:18833','SHENKO_LOCAL_MODEL':'Qwen3-1.7B-Q8_0','SHENKO_OBELISK_CONFIG':str(ROOT/'configuration.json'),'SHENKO_OBELISK_ACCEPTANCE_ROOT':str(ROOT/'acceptance'),'SHENKO_OBELISK_ACCEPTANCE_DEADLINE_MS':'900000','SHENKO_OBELISK_ACCEPTANCE_CONTEXT_TOKENS':'16384'})
identity={'source_head':head,'source_repository':str(REPO),'executable_path':str(BINARY),'executable_sha256':actual,'executable_device':stat.st_dev,'executable_inode':stat.st_ino,'executable_bytes':stat.st_size,'execution_mode':'open fd via /proc/self/fd; prevents target-path replacement from selecting another executable','fd':fd,'started_at_utc':utc(),'upstream_source_head':subprocess.check_output(['git','-C','/workspace/scratch/549c30a35cef/obelisk-audit','rev-parse','HEAD'],text=True).strip(),'config_sha256':hashlib.sha256((ROOT/'configuration.json').read_bytes()).hexdigest(),'local_model_api_key_removed':True,'task_environment':{k:v for k,v in environment.items() if k in ['SHENKO_USE_LOCAL_MODEL','SHENKO_LOCAL_MODEL_URL','SHENKO_LOCAL_MODEL','SHENKO_OBELISK_CONFIG','SHENKO_OBELISK_ACCEPTANCE_ROOT','SHENKO_OBELISK_ACCEPTANCE_DEADLINE_MS','SHENKO_OBELISK_ACCEPTANCE_CONTEXT_TOKENS']},'relay_timing_caveat':'Whole-response buffering via Box connector; task elapsed time includes transport and cannot benchmark native provider latency'}
started=time.monotonic()
with (ROOT/'runner.stdout.log').open('wb') as out,(ROOT/'runner.stderr.log').open('wb') as err:
 child=subprocess.Popen(['/proc/self/fd/'+str(fd)],pass_fds=(fd,),cwd=REPO,env=environment,stdout=out,stderr=err)
 identity['pid']=child.pid;write('run-identity.json',identity)
 result=child.wait()
os.close(fd)
write('runner-exit.json',{'exit_code':result,'finished_at_utc':utc(),'elapsed_ms':(time.monotonic()-started)*1000,'executable_sha256':actual,'source_head':head})
