#!/usr/bin/env bash
set -euo pipefail
root=/tank/shared/projects/shenko/build/daily-readiness-20261122
exec 9>/tank/shared/projects/shenko/build/cargo.lock
flock 9
exec 8>/tank/shared/projects/shenko/build/model-doescompute-8000.lock
flock 8
test "$(cat "$root/release-d16adbf8/exit.txt")" = 0
test ! -e "$root/final-d16adbf8"
test ! -e "$root/final-d16adbf8.model-pre.json"
observe_model() {
ssh -T -o BatchMode=yes -o StrictHostKeyChecking=yes doescompute 'python3 -' <<'PY'
from pathlib import Path
import datetime,hashlib,json
pid=164245
proc=Path('/proc')/str(pid)
cmd=(proc/'cmdline').read_bytes()
args=[s.decode() for s in cmd.split(b'\0') if s]
stat=(proc/'stat').read_text().rsplit(')',1)[1].split()
result={'schema_version':1,'observed_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),'model_server_pid':pid,'start_ticks':stat[19],'boot_id':Path('/proc/sys/kernel/random/boot_id').read_text().strip(),'cmdline_sha256':hashlib.sha256(cmd).hexdigest(),'active_files':[],'scope':'selected observed process identity and configured input files; not memory-resident tensor attestation'}
expected={'--chat-template':('scarlet-visible-answer.jinja','ee7b5403ac8d47ba15911a80931d518faa756254e7875214ce29c5fd0d9eae43'),'--hf-config-path':('config.json','1de5c7a7be5427a60cdb7375ce9a58a45ab2a1507d2a7b524a9749a3e0677264'),'--generation-config':('generation_config.json','cc62ca9bb8c38b8397bba4bedc6fd5f2d22e65596c285eb123d1b4e452de2333')}
for flag,(filename,digest) in expected.items():
 value=args[args.index(flag)+1]; path=Path(value); path=path/filename if path.is_dir() else path; b=path.read_bytes()
 result['active_files'].append({'flag':flag,'configured_value':value,'path':str(path),'exists':True,'bytes':len(b),'sha256':hashlib.sha256(b).hexdigest()})
assert result['start_ticks']=='12753921'
assert result['boot_id']=='dc52a324-ea43-4e54-aa48-8cf24bc51097'
assert result['cmdline_sha256']=='397b4c8cf506f8fe605af40c880535257aaf81f45b11c6c0acbf053f38e1b72f'
assert all(row['sha256']==expected[row['flag']][1] for row in result['active_files'])
print(json.dumps(result,ensure_ascii=False,indent=2))
PY
}
observe_model > "$root/final-d16adbf8.model-pre.json"
printf 'Verified current doescompute process and active configuration files\n'
set +e
ssh -n -T -o BatchMode=yes -o StrictHostKeyChecking=yes doescontainer 'docker exec shenko-build-r71 bash /tank/shared/projects/shenko/build/daily-readiness-20261122-real-final-d16adbf8.sh'
journey_status=$?
observe_model > "$root/final-d16adbf8.model-post.json"
identity_status=$?
set -e
printf '%s\n' "$journey_status" > "$root/final-d16adbf8.wrapper.exit"
printf '%s\n' "$identity_status" > "$root/final-d16adbf8.model-post.exit"
printf 'Real browser wrapper exit=%s; post model identity exit=%s\n' "$journey_status" "$identity_status"
if [ "$journey_status" -ne 0 ]; then exit "$journey_status"; fi
exit "$identity_status"
