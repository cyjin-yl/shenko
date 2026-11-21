import json,os,sqlite3,time,uuid
from pathlib import Path
tag="storage-sync-"+str(uuid.uuid4())
bases=[Path("/tank/shared/projects/shenko/build/daily-readiness-20261122"),Path("/var/idw-target/daily-readiness-storage-observation-20261122")]
report={"scope":"isolated new SQLite WAL/FULL commit latency; no product database or durability setting changed","tag":tag,"rounds":[]}
for round_no in range(2):
    for base in (bases if round_no==0 else list(reversed(bases))):
        base.mkdir(parents=True,exist_ok=True)
        directory=base/(tag+"-"+str(round_no))
        directory.mkdir()
        path=directory/"probe.db"
        start=time.monotonic()
        conn=sqlite3.connect(path)
        journal=conn.execute("PRAGMA journal_mode=WAL").fetchone()[0]
        conn.execute("PRAGMA synchronous=FULL")
        conn.execute("CREATE TABLE sample(i INTEGER PRIMARY KEY, value TEXT)")
        conn.commit()
        setup=time.monotonic()-start
        latencies=[]
        for index in range(20):
            start=time.monotonic()
            with conn:
                conn.execute("INSERT INTO sample(value) VALUES (?)",(str(index),))
            latencies.append(time.monotonic()-start)
        conn.close()
        stat=os.statvfs(directory)
        report["rounds"].append({"base":str(base),"directory":str(directory),"round":round_no,"journal_mode":journal,"synchronous":"FULL","setup_seconds":setup,"commit_seconds":latencies,"total_commit_seconds":sum(latencies),"filesystem_block_bytes":stat.f_bsize})
        print(json.dumps(report["rounds"][-1]),flush=True)
        Path("/tank/shared/projects/shenko/build/daily-readiness-20261122/storage-sync-probe.json").write_text(json.dumps(report,indent=2)+"\n")
