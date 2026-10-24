#!/usr/bin/env python3
"""Two real isolated journeys, not physical two-machine or LLM evidence.
Run with python3 -I SCRIPT REPO NODE_BINARY SSH_DRIVER SSHD LIB_DIR NEW_ROOT.
Only creates/controls a new loopback SSH fixture and private per-node stores.
"""
import base64
import copy
import json
import hashlib
import os
from pathlib import Path
import runpy
import select
import socket
import sqlite3
import subprocess
import sys
import threading
import time
import uuid

repo, binary, ssh_driver, sshd, libdir, root = map(Path, sys.argv[1:7])
legacy_source = Path(sys.argv[7]) if len(sys.argv) == 8 else None
root.mkdir(mode=0o700)
d = runpy.run_path(str(repo / 'scripts/managed_node.py'))
node = lambda directory, request: d['call_binary'](binary, directory, request)
uid = lambda: str(uuid.uuid4())
identity = {'agent_id': uid(), 'branch_id': uid(), 'origin_proof': 'sha256:' + '1'*64}
old_self = {'agent_id': uid(), 'branch_id': uid(), 'origin_proof': 'sha256:' + '2'*64}
controller_id, worker_id = uid(), uid()
controller = root / 'controller'
node(controller, {'command':'initialize', 'node_id':controller_id, 'identity':identity})
for name in ['host_key', 'client_key']:
    subprocess.run(['ssh-keygen','-q','-t','ed25519','-N','','-f',str(root/name)],check=True)
(root/'authorized_keys').write_bytes((root/'client_key.pub').read_bytes())
def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1',0));return s.getsockname()[1]
server_port=port()
listener=socket.socket();listener.bind(('127.0.0.1',0));listener.listen()
proxy_port=listener.getsockname()[1]
config=root/'sshd_config'
config.write_text(f"Port {server_port}\nListenAddress 127.0.0.1\nHostKey {root}/host_key\nPidFile {root}/sshd.pid\nAuthorizedKeysFile {root}/authorized_keys\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPermitRootLogin prohibit-password\nStrictModes no\nUsePAM no\nAllowUsers root\nLogLevel ERROR\n")
public=(root/'host_key.pub').read_text().split()
(root/'known_hosts').write_text(f'[127.0.0.1]:{proxy_port} {public[0]} {public[1]}\n')
remote_root=root/'remote-worker'
# The proxy really closes an SSH stream after the managed install receipt is
# durable. Delay only that stream's packets so the observation cannot race past
# the receipt marker; subsequent receipt queries use a healthy new connection.
dropped=threading.Event();stopping=threading.Event()
marker=[None]
def relay(client):
    upstream=socket.create_connection(('127.0.0.1',server_port))
    try:
        while not stopping.is_set():
            readers,_,_=select.select([client,upstream],[],[],0.05)
            for src in readers:
                data=src.recv(262144)
                if not data:return
                if src is upstream and marker[0] is not None and not dropped.is_set():
                    time.sleep(0.04)
                    if marker[0].exists():dropped.set();return
                (upstream if src is client else client).sendall(data)
    except (OSError,ConnectionError):pass
    finally:client.close();upstream.close()
def serve():
    listener.settimeout(0.1)
    while not stopping.is_set():
        try:client,_=listener.accept()
        except socket.timeout:continue
        except OSError:return
        threading.Thread(target=relay,args=(client,),daemon=True).start()
log=open(root/'sshd.log','xb')
server=subprocess.Popen([str(sshd),'-D','-e','-f',str(config)],stdout=log,stderr=log,env=dict(os.environ,LD_LIBRARY_PATH=str(libdir)))
threading.Thread(target=serve,daemon=True).start()
try:
    for _ in range(100):
        if server.poll() is not None:raise RuntimeError('sshd failed; inspect private fixture log')
        try:
            with socket.create_connection(('127.0.0.1',server_port),timeout=0.1):break
        except OSError:time.sleep(0.02)
    ctx={'task_id':uid(),'task_revision':1,'record_revision':0,'invocation_id':uid(),'epoch':1,'worker_id':uid(),'node_id':controller_id}
    parent={'db':str(root/'effects.sqlite3'),'cas':str(root/'effects-objects'),'context':ctx,'operation_id':uid(),'logical_key':'fixture-only-parent',
        'target':{'host':'127.0.0.1','port':proxy_port,'user':'root','key_path':str(root/'client_key'),'known_hosts_path':str(root/'known_hosts'),'use_agent':False},
        'target_id':'c17-owned-loopback-node','receipt_root':str(root/'receipts'),'action':{'kind':'exec','argv':['true'],'stdin_b64':''},'timeout_secs':15}
    d['dump'](root/'parent.json',parent)
    init=subprocess.run([str(ssh_driver),'fixture-init',str(root/'parent.json')],capture_output=True)
    assert init.returncode==0,init.stderr
    config={'artifact':str(binary),'controller_binary':str(binary),'controller_data':str(controller),'node_id':worker_id,
        'remote_root':str(remote_root),'parent':parent,'identity':identity,'initial_identity':old_self,'ssh_driver':str(ssh_driver)}
    d['dump'](root/'config.json',config)
    plan_dir=root/'deployment'
    d['prepare'](plan_dir,root/'config.json')
    plan=json.loads((plan_dir/'plan.json').read_text())
    op=str(uuid.uuid5(uuid.UUID(plan['deployment_id']),'install'))
    marker[0]=remote_root/('install-'+plan['deployment_id']+'.json')
    # Journey 1 includes ownership refusal and the actual v1 -> v2 install
    # migration. The legacy source is read-only; all migration happens on backup.
    unrelated=root/'unrelated-data';unrelated.mkdir(mode=0o700)
    with sqlite3.connect(unrelated/'node.sqlite3') as db:
        db.execute('PRAGMA journal_mode=WAL')
        db.execute('CREATE TABLE unrelated(secret TEXT)')
        db.execute("INSERT INTO unrelated VALUES('preserve-me')")
    before={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in unrelated.iterdir()}
    rejected=subprocess.run([str(binary),str(unrelated)],input=b'{"command":"status"}',capture_output=True)
    assert rejected.returncode!=0 and b'unrecognized node installation' in rejected.stderr
    assert before=={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in unrelated.iterdir()}
    migration=None
    if legacy_source:
        migrated=root/'migrated-installation';migrated.mkdir(mode=0o700)
        with sqlite3.connect('file:'+str(legacy_source/'node.sqlite3')+'?mode=ro&immutable=1',uri=True) as old:
            assert old.execute('PRAGMA user_version').fetchone()[0]==1
            tables=[r[0] for r in old.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")]
            rows={t:old.execute('SELECT * FROM '+t+' ORDER BY rowid').fetchall() for t in tables}
            with sqlite3.connect(migrated/'node.sqlite3') as backup:old.backup(backup)
        migrated_status=node(migrated,{'command':'status'})
        with sqlite3.connect(migrated/'node.sqlite3') as db:
            assert db.execute('PRAGMA user_version').fetchone()[0]==2
            assert db.execute('PRAGMA application_id').fetchone()[0]==0x53484e4b
            for t,old_rows in rows.items():
                new_rows=db.execute('SELECT * FROM '+t+' ORDER BY rowid').fetchall()
                if t=='meta':new_rows=[r for r in new_rows if r[0]!='installation']
                assert new_rows==old_rows,t
        assert node(migrated,{'command':'status'})['metadata']==migrated_status['metadata']
        migration={'storage_schema':2,'old_tables_preserved':len(tables),'source_readonly':True}
    # Journey 1: real artifact upload/install -> lost SSH response -> original
    # receipt query -> resume/adopt existing self -> capability/status readback.
    try:d['deploy'](plan_dir,plan)
    except RuntimeError as error:assert 'unknown' in str(error),str(error)
    else:raise AssertionError('response-loss injection did not interrupt install')
    assert dropped.is_set() and marker[0].exists()
    recovered=d['invoke'](plan_dir,plan,'install',query=True)
    assert recovered['state']=='installed'
    d['deploy'](plan_dir,plan)
    # A full retry queries every old mutating operation, including join; status
    # is intentionally a new read-only observation.
    d['deploy'](plan_dir,plan)
    status=d['node_call'](plan_dir,plan,'status-readback',{'command':'status','operation_id':plan['deployment_id']})
    assert status['metadata']['retired_identities']==[old_self]
    assert status['metadata']['identity']==identity
    assert status['running_manifest']==plan['manifest']
    assert status['capabilities']['smart_node_evidence'] is False
    with sqlite3.connect(root/'effects.sqlite3') as effects:
        count=effects.execute('SELECT count(*) FROM attempts WHERE operation_id=?',[op]).fetchone()[0]
        assert count==1
    with sqlite3.connect(remote_root/'data/node.sqlite3') as db:
        assert db.execute('SELECT count(*) FROM commands').fetchone()[0]==1
    print(json.dumps({'journey':1,'result':'passed','install_operation':op,'attempts':count,'loss':'actual_closed_SSH_stream','identity_adopted':True,'unrelated_db_unchanged':True,'legacy_migration':migration,'service':'one_shot_no_daemon'}),flush=True)
    # Journey 2: a durable task leaves a result on old owner; query it, explicitly
    # transfer the pure contract, preserve late facts but fence old settlement,
    # execute on the new owner, accept exactly one terminal outcome after reopen.
    node(controller,{'command':'join','operation_id':uid(),'expected_node':controller_id,'identity':identity,'manifest':plan['manifest']})
    node(controller,{'command':'register','node_id':controller_id,'target_id':'c17-local-authority-worker','binding_digest':'sha256:'+'3'*64})
    # This authenticated receiver snapshot has the same registered source, but
    # has not yet received the task assignment. E1 must not depend on lookup.
    early_receiver=root/'early-receiver';early_receiver.mkdir(mode=0o700)
    with sqlite3.connect(controller/'node.sqlite3') as source:
        with sqlite3.connect(early_receiver/'node.sqlite3') as target:source.backup(target)
    payload=list(b'original task bytes\x00with stable identity')
    task_id=uid()
    first=node(controller,{'command':'create','task_id':task_id,'node_id':worker_id,'input':payload})['ticket']
    original=d['node_call'](plan_dir,plan,'first-task',{'command':'execute','ticket':first,'input':payload})['receipt']
    queried=d['node_call'](plan_dir,plan,'original-task-query',{'command':'query','task_id':task_id,'epoch':first['epoch']})
    assert queried['receipt']==original
    unassociated=node(early_receiver,{'command':'accept','from_node':worker_id,'receipt':original})
    assert unassociated['classification']=='unassociated' and not unassociated['accepted']
    early_export=node(early_receiver,{'command':'export_receipts','after':'0','limit':128})
    assert len(early_export['receipts'])==1 and early_export['receipts'][0]['receipt']==original
    assert early_export['receipts'][0]['reason']=='task_missing'
    # A trusted worker's contradictory outer result must be retained in full,
    # not lost with a transcript rejection, and cannot settle the running task.
    contradictory=copy.deepcopy(original);contradictory['output_digest']='blake3:'+'0'*64
    invalid=node(controller,{'command':'accept','from_node':worker_id,'receipt':contradictory})
    assert invalid['classification']=='invalid' and not invalid['accepted']
    invalid_export=node(controller,{'command':'export_receipts','after':'0','limit':128})
    assert invalid_export['receipts'][0]['receipt']==contradictory
    assert invalid_export['receipts'][0]['reason']=='result_transcript_mismatch'
    with sqlite3.connect(controller/'node.sqlite3') as db:
        assert db.execute('SELECT count(*) FROM results').fetchone()[0]==0
    second=node(controller,{'command':'transfer','task_id':task_id,'expected_revision':first['record_revision'],'expected_epoch':first['epoch'],'to_node':controller_id})['ticket']
    stale=node(controller,{'command':'accept','from_node':worker_id,'receipt':original})
    assert stale['status']=='evidence_only_stale_owner' and not stale['accepted']
    current=node(controller,{'command':'execute','ticket':second,'input':payload})['receipt']
    accepted=node(controller,{'command':'accept','from_node':controller_id,'receipt':current})
    assert accepted['accepted']
    duplicate=node(controller,{'command':'accept','from_node':controller_id,'receipt':current})
    assert duplicate['status']=='already_settled' and not duplicate['accepted']
    late=node(controller,{'command':'accept','from_node':worker_id,'receipt':original})
    assert late['status']=='evidence_only_terminal' and not late['accepted']
    restored=node(controller,{'command':'create','task_id':task_id,'node_id':worker_id,'input':payload})
    assert restored['terminal'] and restored['ticket']==second
    receipts=node(controller,{'command':'export_receipts','after':'0','limit':128})
    assert len(receipts['receipts'])==3
    assert receipts['receipts'][0]['receipt']==contradictory and receipts['receipts'][0]['status']=='invalid'
    assert {r['status'] for r in receipts['receipts']}=={'invalid','terminal','settled'}
    exported=node(controller,{'command':'export','after':'0','limit':128})
    assert {e['raw']['node_id'] for e in exported['events']}=={worker_id,controller_id}
    with sqlite3.connect(controller/'node.sqlite3') as db:
        assert db.execute('SELECT count(*) FROM tasks').fetchone()[0]==1
        assert db.execute('SELECT count(*) FROM assignments').fetchone()[0]==2
        assert db.execute('SELECT count(*) FROM results').fetchone()[0]==1
        assert db.execute('SELECT count(*) FROM events').fetchone()[0]==2
        task=json.loads(db.execute('SELECT data FROM tasks').fetchone()[0])
        assert task['owner']['id']==identity['branch_id'] and task['state']['status']=='terminal'
    print(json.dumps({'journey':2,'result':'passed','task_id':task_id,'epochs':[first['epoch'],second['epoch']],
        'stale_owner_fenced_at':'authoritative_result_store','terminal_results':1,'raw_source_events':2,'unassociated_result_retained':True,'invalid_full_receipt_retained':True,'receipt_rows':3,
        'boundary':'pure_no_external_effect_contract; old computation not claimed stopped'}),flush=True)
    d['dump'](root/'result.json',{'passed':True,'journeys':2,'physical_nodes':False,'smart_node_evidence':False,'manifest':plan['manifest'],
        'deployment_id':plan['deployment_id'],'install_operation':op,'task_id':task_id,'source_events':exported['events'],'receipts':receipts,'unassociated_receipts':early_export,'legacy_migration':migration,'unrelated_db_unchanged':True})
finally:
    stopping.set();listener.close();server.terminate()
    try:server.wait(timeout=5)
    except subprocess.TimeoutExpired:server.kill();server.wait()
    log.close()
