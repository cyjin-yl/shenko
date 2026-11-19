#!/usr/bin/env python3
"""Source/document observations only; deliberately does not compile products."""
import datetime, hashlib, json, os, signal, subprocess, time
from pathlib import Path
repo = Path('/tank/shared/projects/shenko/worktrees/continuation-pr327-20261117-1312')
base = Path('/tank/shared/projects/shenko/build/continuation-pr327-20261117-1312')
out = base / 'static-review-20261117-1420'
out.mkdir(exist_ok=False)
expected = '8978816295429b65308e6dd6c8499cca360f8552'
def now(): return datetime.datetime.now(datetime.timezone.utc).isoformat()
def digest(p): return hashlib.sha256(p.read_bytes()).hexdigest()
def save(name, data): (out/name).write_text(json.dumps(data, ensure_ascii=False, indent=2)+'\n')
records = []
def run(name, argv, seconds=90):
    rec = {'name':name, 'argv':argv, 'started_at':now(), 'deadline_seconds':seconds, 'timed_out':False}
    with (out/(name+'.stdout')).open('xb') as so, (out/(name+'.stderr')).open('xb') as se:
        p = subprocess.Popen(argv, cwd=repo, stdout=so, stderr=se, start_new_session=True)
        try: code = p.wait(timeout=seconds)
        except subprocess.TimeoutExpired:
            rec['timed_out'] = True
            os.killpg(p.pid, signal.SIGTERM)
            try: code = p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(p.pid, signal.SIGKILL)
                code = p.wait(timeout=10)
    rec.update(returncode=code, finished_at=now(), leader_reaped=p.returncode is not None)
    for stream in ('stdout','stderr'):
        path=out/(name+'.'+stream)
        rec[stream]={'file':path.name,'bytes':path.stat().st_size,'sha256':digest(path)}
    records.append(rec); save('commands.json',records)
    print(json.dumps(rec), flush=True)
    return code
try:
    run('head',['git','rev-parse','HEAD'])
    if (out/'head.stdout').read_text().strip() != expected: raise RuntimeError('unexpected source HEAD')
    if run('status',['git','status','--porcelain','--untracked-files=no']) != 0 or (out/'status.stdout').read_bytes():
        raise RuntimeError('source cleanliness not established')
    run('tree',['git','rev-parse','HEAD^{tree}'])
    paths=['crates/shenko-types/src/installation.rs','crates/shenko-store/src/ledger.rs',
           'crates/shenko-update/tests/rollback_migration.rs','crates/shenko-update/src/handoff_tests.rs',
           'docs/specs/self-update.md','Cargo.toml','Cargo.lock','rust-toolchain.toml']
    before={p:digest(repo/p) for p in paths}
    save('source-before.json',{'head':expected,'files':before,'observed_at':now(),'atomic_freeze':False})
    run('diff-check',['git','diff','--check','730c17ea6ea1098568e75bf67a81f00bf2f7bd17..HEAD'])
    run('rustfmt-version',['/home/ezra/.cargo/bin/rustfmt','+1.98.1','--version'],30)
    run('docs',['python3','scripts/check_docs.py'])
    run('migration',['python3','scripts/check_docs.py','--verify-migration'])
    after={p:digest(repo/p) for p in paths}
    save('source-after.json',{'files':after,'same_observed_inputs':before==after,'observed_at':now()})
    save('scope.json',{'rust_compilation':False,'rust_tests':False,'full_ship_work':False,
         'same_observed_inputs':before==after,'notes':['Host static observations, not the required devcontainer runtime validation.',
         'Prior driver-r1 failed at git status timeout before Rust execution.',
         'Device command policy rejected sudo; no privilege workaround attempted.']})
finally:
    save('inventory.json',[{'path':p.name,'bytes':p.stat().st_size,'sha256':digest(p)} for p in sorted(out.iterdir()) if p.is_file() and p.name!='inventory.json'])
