// Runs inside functions.exec, whose tools provide the remote Box connector.
// options.maxRequests allows a one-GET infrastructure check before the runner.
const task = '/workspace/scratch/549c30a35cef/obelisk-qwen-real-20261116';
const relay = task + '/relay';
const remote = '/workspace/shenko-obelisk-qwen-20261116';
const started = Date.now();
let handled = 0;
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
function boxText(result) {
  if (result.isError) throw new Error(JSON.stringify(result));
  return result.content.filter(c => c.type === 'text').map(c => c.text).join('\n');
}
function boxJson(result) {
  const lines = boxText(result).split('\n');
  const value = lines.find(line => line.startsWith('BRIDGE_JSON='));
  if (!value) throw new Error('Box response missing JSON marker');
  return JSON.parse(value.slice('BRIDGE_JSON='.length));
}
async function local(command) {
  const result = await tools.exec_command({cmd:command,max_output_tokens:30000});
  if (result.exit_code !== 0) throw new Error(result.output);
  return result.output;
}
async function boxPython(source) {
  return boxJson(await tools.mcp__codex_apps__box_by_simular_shell({command:"( python3 - <<'PY'\n"+source+"\nPY\n)",timeout_s:20}));
}
async function forward(job) {
  const request = job.request;
  const identity = request.id;
  if (!/^[0-9]{4}-[a-f0-9]{32}$/.test(identity)) throw new Error('Invalid request id');
  const prefix = remote + '/requests/relay-' + identity;
  boxText(await tools.mcp__codex_apps__box_by_simular_write_file({path:prefix+'.request.body',content:job.body_utf8}));
  boxText(await tools.mcp__codex_apps__box_by_simular_write_file({path:prefix+'.transport.json',content:JSON.stringify(request,null,2)+'\n'}));
  boxText(await tools.mcp__codex_apps__box_by_simular_shell({command:"( set -eu; cd "+remote+"; nohup python3 relay-request.py "+identity+" > logs/relay-"+identity+".log 2>&1 < /dev/null & printf 'remote_worker_pid=%s\\n' \"$!\" )",timeout_s:15}));
  notify({event:'real_request_forwarded_once',id:identity,method:request.method,path:request.path,request_bytes:request.body_bytes,request_sha256:request.request_sha256});
  let remoteResult;
  const pollStart = Date.now();
  for (;;) {
    remoteResult = await boxPython("import json,base64\nfrom pathlib import Path\np=Path('"+prefix+"')\nr=Path(str(p)+'.receipt.json')\nready=Path(str(p)+'.response.ready').exists()\nif ready:\n receipt=json.loads(r.read_text())\n out={'ready':True,'receipt':receipt,'headers_base64':base64.b64encode(Path(str(p)+'.response.headers').read_bytes()).decode()}\nelse:\n cap=Path(str(p)+'.capacity.json')\n out={'ready':False,'capacity':json.loads(cap.read_text()) if cap.exists() else None}\nprint('BRIDGE_JSON='+json.dumps(out,separators=(',',':')))");
    if (remoteResult.ready) break;
    if (Date.now()-pollStart > 220000) throw new Error('One remote attempt did not finish within relay observation window; no retry');
    await delay(1500);
  }
  const chunks=[];
  for (let offset=0; offset<remoteResult.receipt.response_gzip_bytes; offset+=16384) {
    const chunk=await boxPython("import json,base64\nfrom pathlib import Path\np=Path('"+prefix+".response.body.gz')\nwith p.open('rb') as f:\n f.seek("+offset+")\n raw=f.read(16384)\nprint('BRIDGE_JSON='+json.dumps({'offset':"+offset+",'base64':base64.b64encode(raw).decode()},separators=(',',':')))");
    if (chunk.offset!==offset) throw new Error('Transfer offset mismatch');
    chunks.push(chunk.base64);
  }
  const envelope={receipt:remoteResult.receipt,headers_base64:remoteResult.headers_base64,gzip_base64_chunks:chunks};
  const envelopePath=relay+'/queue/'+identity+'/transfer-envelope.json';
  const payload=JSON.stringify(envelope,null,2)+'\n';
  await tools.apply_patch('*** Begin Patch\n*** Add File: '+envelopePath+'\n'+payload.split('\n').map(line=>'+'+line).join('\n')+'\n*** End Patch');
  const installed=await local('python3 '+relay+'/queue_bridge.py --root '+relay+' ingest '+envelopePath);
  notify({event:'real_response_verified_and_delivered',verification:JSON.parse(installed),capacity:remoteResult.receipt.capacity,remote_inference_ms:remoteResult.receipt.inference_elapsed_ms,curl_exit:remoteResult.receipt.curl_exit});
}
while (Date.now()-started<(options.maxMs || 1900000)) {
  const result=JSON.parse(await local('python3 '+relay+'/queue_bridge.py --root '+relay+' claim'));
  if (result.pending) {
    await forward(result);
    handled++;
    if (options.maxRequests && handled>=options.maxRequests) break;
  } else {
    const state=JSON.parse(await local("python3 - <<'PY'\nimport json,pathlib\np=pathlib.Path('"+task+"/runner-exit.json')\nprint(json.dumps({'runner_done':p.exists(),'result':json.loads(p.read_text()) if p.exists() else None}))\nPY"));
    if (state.runner_done) {text({event:'runner_finished',handled,result:state.result});break;}
    await delay(500);
  }
}
text({event:'bridge_finished',handled,elapsed_ms:Date.now()-started});
