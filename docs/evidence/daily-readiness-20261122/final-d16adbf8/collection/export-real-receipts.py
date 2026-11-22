#!/usr/bin/env python3
"""Copy selected completed synthetic journey receipts without modifying originals."""
from pathlib import Path
import base64,datetime,gzip,hashlib,io,json,re,sys,tarfile
candidate=sys.argv[1]
assert re.fullmatch(r"[0-9a-f]{40}",candidate)
tag=candidate[:8]
root=Path('/tank/shared/projects/shenko/build/daily-readiness-20261122')
batch=root/f'final-{tag}'
assert (root/f'final-{tag}.finished.txt').exists()
assert (root/f'final-{tag}.wrapper.exit').exists()
report=json.loads((batch/'batch-summary.json').read_text())
out=root/f'final-{tag}-public'
out.mkdir(exist_ok=False)
index={'source_revision':candidate,'observed_at_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'scope':'selected original synthetic journey receipts; installation control files, databases and complete CAS stay on the authorized device; semantic selected CAS proof is a separate derived export','files':[]}
def retain(source,name,compressed=False):
 raw=source.read_bytes();b=gzip.compress(raw,mtime=0) if compressed else raw
 target=out/name;target.parent.mkdir(parents=True,exist_ok=True);target.write_bytes(b)
 assert (gzip.decompress(b) if compressed else b)==raw
 index['files'].append({'path':name,'source':str(source),'source_bytes':len(raw),'source_sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(b),'sha256':hashlib.sha256(b).hexdigest(),'encoding':'gzip-mtime0' if compressed else 'original'})
retain(batch/'batch-summary.json','batch-summary.json')
for name in ('model-pre.json','model-post.json','driver.exit','sampler.exit','wrapper.exit','model-post.exit','started.txt','finished.txt'):
 retain(root/(f'final-{tag}.'+name),name)
retain(root/f'final-{tag}.environment.ndjson','environment.ndjson.gz',True)
retain(root/f'driver-{tag}/manifest.json','driver-manifest.json')
retain(root/f'driver-{tag}-preparation.json','driver-preparation.json')
for source,name in [(root/f'release-{tag}/manifest.json','release-manifest.json'),(root/f'final-{tag}.driver.log','driver.log.gz')]:
 retain(source,name,name.endswith('.gz'))
for attempt in sorted(batch.glob('attempt-*')):
 for name in ('manifest.json','models.json','summary.json','first.input.json','first.startup.json','first.startup-shutdown.json','restart.input.json','restart.startup.json','restart.startup-shutdown.json','failure.txt'):
  if (attempt/name).exists():retain(attempt/name,attempt.name+'/'+name)
 for p in sorted(attempt.iterdir()):
  if p.is_file() and (p.name.endswith('.runtime.log') or p.name.endswith('.websocket.jsonl') or p.name.endswith('.ledger.json') or p.name=='runtime-controls.jsonl'):
   retain(p,attempt.name+'/'+p.name+'.gz',True)
 browser=attempt/'browser'
 if browser.exists():
  for p in sorted(browser.iterdir()):
   if p.is_file() and (p.name in ('summary.json','environment.json','served-build-manifest.json') or p.name.endswith(('.png','.aria.yml','.dom.json'))):
    retain(p,attempt.name+'/browser/'+p.name)
   elif p.is_file() and p.name=='events.jsonl':
    retain(p,attempt.name+'/browser/'+p.name+'.gz',True)
(out/'index.json').write_text(json.dumps(index,ensure_ascii=False,indent=2)+'\n')
stream=io.BytesIO()
with tarfile.open(fileobj=stream,mode='w') as archive:
 for p in sorted(out.rglob('*')):
  if p.is_file():archive.add(p,arcname=str(p.relative_to(out)))
packed=gzip.compress(stream.getvalue(),mtime=0)
archive=root/f'final-{tag}-public.tar.gz';archive.write_bytes(packed)
chunks=root/f'final-{tag}-transfer';chunks.mkdir(exist_ok=False)
for i,start in enumerate(range(0,len(packed),12288)):
 (chunks/f'{i:03d}.txt').write_text(base64.b64encode(packed[start:start+12288]).decode()+'\n')
print(json.dumps({'archive':str(archive),'bytes':len(packed),'sha256':hashlib.sha256(packed).hexdigest(),'files':len(index['files'])+1,'chunks':(len(packed)+12287)//12288,'chunk_root':str(chunks)},indent=2))
