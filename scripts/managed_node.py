#!/usr/bin/env python3
"""Native connected-node driver over C07's existing trusted task/receipt API.

No network discovery, credential copying, shell-task replay or fake scheduler.
The input must be a currently admitted trusted parent TaskInvocation. The C07
example's fixture-init is for isolated experiments only, never used here.
"""
import argparse
import base64
import gzip
import hashlib
import json
import os
from pathlib import Path
import subprocess
import uuid

INSTALL = r"""
import gzip,hashlib,json,os,stat,sys
root,op,sha,*parts=sys.argv[1:]
assert os.path.isabs(root) and not os.path.islink(root)
st=os.stat(root)
assert stat.S_ISDIR(st.st_mode) and st.st_uid==os.getuid() and st.st_mode & 0o077==0
path=os.path.join(root,'artifact-'+op)
raw=gzip.decompress(b''.join(open(p,'rb').read() for p in parts))
assert hashlib.sha256(raw).hexdigest()==sha
try:
 fd=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o700)
 with os.fdopen(fd,'wb') as f:
  f.write(raw); f.flush(); os.fsync(f.fileno())
except FileExistsError:
 assert hashlib.sha256(open(path,'rb').read()).hexdigest()==sha
assert os.stat(path).st_mode & 0o077==0
receipt={'operation_id':op,'sha256':sha,'artifact':path,'state':'installed','service':'one_shot_no_daemon'}
receipt_path=os.path.join(root,'install-'+op+'.json')
try:
 fd=os.open(receipt_path,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
 with os.fdopen(fd,'w') as f:
  json.dump(receipt,f,sort_keys=True);f.flush();os.fsync(f.fileno())
except FileExistsError:
 assert json.load(open(receipt_path))==receipt
fd=os.open(root,os.O_RDONLY|os.O_DIRECTORY);os.fsync(fd);os.close(fd)
print(json.dumps(receipt))
"""
MKROOT = r"""
import os,stat,sys
p=sys.argv[1]
try:os.mkdir(p,0o700)
except FileExistsError:pass
s=os.lstat(p)
assert stat.S_ISDIR(s.st_mode) and s.st_uid==os.getuid() and s.st_mode & 0o077==0
print('{}')
"""

def dump(path, data):
    with open(path, 'x', encoding='utf-8') as f:
        json.dump(data, f, sort_keys=True)
        f.flush()
        os.fsync(f.fileno())


def call_binary(binary, directory, request):
    p = subprocess.run([str(binary), str(directory)], input=json.dumps(request).encode(), capture_output=True)
    if p.returncode:
        raise RuntimeError(f'node call exit {p.returncode}: {p.stderr.decode()[:2000]}')
    return json.loads(p.stdout)


def prepare(directory, config):
    # A new, caller-owned plan directory. Never regenerate an operation ID after
    # losing a response. Reopen this plan and query its existing operations.
    directory.mkdir(mode=0o700)
    config = json.loads(Path(config).read_text())
    binary = Path(config['artifact'])
    raw = binary.read_bytes()
    manifest = json.loads(subprocess.check_output([str(binary), '--manifest']))
    assert len(manifest['source_revision']) == 40, 'artifact must bind a clean product source commit'
    deployment_id = str(uuid.uuid4())
    blob = gzip.compress(raw, mtime=0)
    parts = []
    for offset in range(0, len(blob), 512 * 1024):
        name = f'chunk-{len(parts)}'
        (directory / name).write_bytes(blob[offset:offset + 512 * 1024])
        parts.append(name)
    trust = Path(config['parent']['target']['known_hosts_path']).read_bytes()
    public = {'target_id': config['parent']['target_id'], 'node_id': config['node_id'],
              'known_hosts_sha256': hashlib.sha256(trust).hexdigest(), 'root': config['remote_root']}
    binding = 'sha256:' + hashlib.sha256(json.dumps(public, sort_keys=True).encode()).hexdigest()
    controller = call_binary(config['controller_binary'], config['controller_data'], {'command': 'status'})
    assert controller['metadata']['identity'] == config['identity'], 'not the verified controller self'
    registered = call_binary(config['controller_binary'], config['controller_data'], {
        'command': 'register', 'node_id': config['node_id'], 'target_id': public['target_id'], 'binding_digest': binding})
    plan = dict(config, deployment_id=deployment_id, manifest=manifest, parts=parts,
                artifact_sha256=hashlib.sha256(raw).hexdigest(), known_hosts_sha256=public['known_hosts_sha256'], registered=registered,
                chunks_sha256={p: hashlib.sha256((directory / p).read_bytes()).hexdigest() for p in parts})
    dump(directory / 'plan.json', plan)
    print(json.dumps({'deployment_id': deployment_id, 'registered': registered, 'manifest': manifest}))


def invoke(directory, plan, label, action=None, query=False):
    assert len(label) <= 96 and label.replace('-', '').replace('_', '').isalnum(), 'stable simple operation label required'
    trust = Path(plan['parent']['target']['known_hosts_path']).read_bytes()
    assert hashlib.sha256(trust).hexdigest() == plan['known_hosts_sha256'], 'registered host trust changed'
    request = dict(plan['parent'])
    request['operation_id'] = str(uuid.uuid5(uuid.UUID(plan['deployment_id']), label))
    request['logical_key'] = 'node-' + plan['deployment_id'] + '-' + label
    request['action'] = action or {'kind': 'download', 'path': '/not-dispatched'}
    # This read is trusted task state, not a model declaration of higher epoch.
    request_file = directory / ('request-' + str(uuid.uuid4()) + '.json')
    dump(request_file, request)
    task = subprocess.run([plan['ssh_driver'], 'task', str(request_file)], capture_output=True)
    if task.returncode:
        raise RuntimeError(f'task read exit {task.returncode}: {task.stderr.decode()[:2000]}')
    record = json.loads(task.stdout)
    request['context'] = dict(request['context'], record_revision=record['record_revision'])
    # Fresh file, not overwrite a frozen prior invocation.
    request_file = directory / ('request-' + str(uuid.uuid4()) + '.json')
    dump(request_file, request)
    p = subprocess.run([plan['ssh_driver'], 'query' if query else 'execute', str(request_file)], capture_output=True)
    raw_file = directory / ('receipt-' + label + '-' + str(uuid.uuid4()) + '.json')
    raw_file.write_bytes(p.stdout)
    if p.returncode:
        raise RuntimeError(f'SSH driver exit {p.returncode}: {p.stderr.decode()[:2000]}')
    observation = json.loads(p.stdout)
    receipt = observation.get('receipt')
    if not receipt or receipt.get('state') != 'completed':
        raise RuntimeError(f'operation {request["operation_id"]} unknown; query same plan/label before any new decision')
    assert receipt['exit_code'] == 0, 'remote application failure; original receipt retained'
    assert not receipt['stdout_truncated'] and not receipt['stderr_truncated'], 'incomplete response'
    payload = base64.b64decode(receipt['stdout_b64'])
    return json.loads(payload) if payload else {'remote_receipt': receipt['state'], 'sha256': receipt.get('sha256')}


def action(argv, request=None):
    return {'kind': 'exec', 'argv': argv, 'stdin_b64': base64.b64encode(json.dumps(request).encode() if request is not None else b'').decode()}


def node_call(directory, plan, label, request):
    return invoke(directory, plan, label, action([
        plan['remote_root'] + '/artifact-' + plan['deployment_id'], plan['remote_root'] + '/data'], request))


def deploy(directory, plan):
    root = plan['remote_root']
    invoke(directory, plan, 'root', action(['python3', '-I', '-c', MKROOT, root]))
    for name in plan['parts']:
        data = (directory / name).read_bytes()
        assert hashlib.sha256(data).hexdigest() == plan['chunks_sha256'][name], 'artifact staging corruption'
        invoke_upload = {'kind': 'upload', 'path': root + '/' + name, 'data_b64': base64.b64encode(data).decode()}
        # Upload receipt stdout is intentionally empty; validate C07 receipt via
        # the same admission helper, but do not interpret empty bytes as JSON.
        invoke(directory, plan, name, invoke_upload)
    install = invoke(directory, plan, 'install', action(['python3', '-I', '-c', INSTALL, root,
        plan['deployment_id'], plan['artifact_sha256'], *[root + '/' + n for n in plan['parts']]]))
    node_call(directory, plan, 'initialize', {'command': 'initialize', 'node_id': plan['node_id'], 'identity': plan.get('initial_identity')})
    joined = node_call(directory, plan, 'join', {'command': 'join', 'operation_id': plan['deployment_id'],
        'expected_node': plan['node_id'], 'identity': plan['identity'], 'manifest': plan['manifest']})
    status = node_call(directory, plan, 'status', {'command': 'status', 'operation_id': plan['deployment_id']})
    assert status['node_id'] == plan['node_id'] and status['metadata']['identity'] == plan['identity']
    assert status['running_manifest'] == plan['manifest'], 'actual runtime differs from installation plan'
    print(json.dumps({'install': install, 'join': joined, 'status': status}))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('mode', choices=['prepare', 'deploy', 'call', 'query'])
    p.add_argument('directory', type=Path)
    p.add_argument('argument', nargs='?')
    p.add_argument('request', nargs='?')
    a = p.parse_args()
    if a.mode == 'prepare':
        prepare(a.directory, a.argument)
        return
    plan = json.loads((a.directory / 'plan.json').read_text())
    if a.mode == 'deploy':
        deploy(a.directory, plan)
    elif a.mode == 'query':
        print(json.dumps(invoke(a.directory, plan, a.argument, query=True)))
    else:
        print(json.dumps(node_call(a.directory, plan, a.argument, json.loads(Path(a.request).read_text()))))

if __name__ == '__main__':
    main()
