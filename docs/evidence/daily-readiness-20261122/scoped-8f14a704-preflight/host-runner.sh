#!/usr/bin/env bash
set -euo pipefail
root=/tank/shared/projects/shenko/build/daily-readiness-20261122
candidate=8f14a704c59ce723246286d72ea7cb724b2b3c0e
baseline=be9cfe1ba9ac99f34c69fe57eea4aa369a8371e3
prefix="$root/known-node-loss-8f14a704.host"
test ! -e "$prefix.started.txt"
test ! -e "$root/scoped-8f14a704"
date -u +%FT%TZ > "$prefix.started.txt"
finish() {
 local result=$?
 trap - EXIT
 printf '%s\n' "$result" > "$prefix.exit"
 date -u +%FT%TZ > "$prefix.finished.txt"
 exit "$result"
}
trap finish EXIT
python3 - <<'PY'
from pathlib import Path
import base64,hashlib
p=Path('/tank/shared/projects/shenko/build/known-node-loss-8f14a704.bundle')
b=base64.b64decode(p.with_suffix('.bundle.b64').read_text(),validate=True)
assert len(b)==798 and hashlib.sha256(b).hexdigest()=='36516d21f91c2c36e0c032ea03b21781372095ce27fd618e5418b65ea2951646'
if p.exists():assert p.read_bytes()==b
else:p.write_bytes(b)
print('Verified exact 8f14 fixture-only source bundle',flush=True)
PY
exec 8>/tank/shared/projects/shenko/build/cargo.lock
flock 8
date -u +%FT%TZ > "$prefix.admitted.txt"
test ! -e "$root/scoped-8f14a704"
cd /tank/shared/projects/shenko/shenko
git bundle verify /tank/shared/projects/shenko/build/known-node-loss-8f14a704.bundle
git fetch /tank/shared/projects/shenko/build/known-node-loss-8f14a704.bundle refs/heads/fix/recovery-stop-observation-order-20261122
cd /tank/shared/projects/shenko/worktrees/daily-readiness-20261122
test -z "$(git status --porcelain)"
test "$(git rev-parse HEAD)" = "$baseline"
git checkout --detach "$candidate"
test "$(git rev-parse HEAD)" = "$candidate"
task_image=$(ssh -n -T -o BatchMode=yes -o StrictHostKeyChecking=yes doescontainer 'docker inspect --format="{{.Image}}" shenko-build-r71')
test "$task_image" = sha256:28df77cc6beee4f88ee1f57922190dcb02414d78d284e8b0f91859e18728e565
printf 'Verified build container image: %s\n' "$task_image"
ssh -n -T -o BatchMode=yes -o StrictHostKeyChecking=yes doescontainer 'docker exec shenko-build-r71 bash /tank/shared/projects/shenko/build/daily-readiness-20261122-verify-known-node-loss-8f14a704.sh'
