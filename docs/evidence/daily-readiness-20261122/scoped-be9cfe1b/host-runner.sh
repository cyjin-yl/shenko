#!/usr/bin/env bash
set -euo pipefail
root=/tank/shared/projects/shenko/build/daily-readiness-20261122
candidate=be9cfe1ba9ac99f34c69fe57eea4aa369a8371e3
python3 - <<'PY'
from pathlib import Path
import base64,hashlib
p=Path('/tank/shared/projects/shenko/build/known-node-loss-be9cfe1b.bundle')
b=base64.b64decode(p.with_suffix('.bundle.b64').read_text(),validate=True)
assert len(b)==2690 and hashlib.sha256(b).hexdigest()=='b843d04a32c2010c2d7fbcde8483aa268dc5b80bf179538c3675ebfb567ddebe'
if p.exists():assert p.read_bytes()==b
else:p.write_bytes(b)
print('Verified exact known-Node-loss source bundle',flush=True)
PY
exec 8>/tank/shared/projects/shenko/build/cargo.lock
flock 8
test ! -e "$root/scoped-be9cfe1b"
cd /tank/shared/projects/shenko/shenko
git bundle verify /tank/shared/projects/shenko/build/known-node-loss-be9cfe1b.bundle
git fetch /tank/shared/projects/shenko/build/known-node-loss-be9cfe1b.bundle refs/heads/fix/recovery-stop-observation-order-20261122
cd /tank/shared/projects/shenko/worktrees/daily-readiness-20261122
test -z "$(git status --porcelain)"
git checkout --detach "$candidate"
test "$(git rev-parse HEAD)" = "$candidate"
task_image=$(ssh -n -T -o BatchMode=yes -o StrictHostKeyChecking=yes doescontainer 'docker inspect --format="{{.Image}}" shenko-build-r71')
test "$task_image" = sha256:28df77cc6beee4f88ee1f57922190dcb02414d78d284e8b0f91859e18728e565
printf 'Verified build container image: %s\n' "$task_image"
date -u +%FT%TZ > "$root/known-node-loss-be9cfe1b.host.started.txt"
set +e
ssh -n -T -o BatchMode=yes -o StrictHostKeyChecking=yes doescontainer 'docker exec shenko-build-r71 bash /tank/shared/projects/shenko/build/daily-readiness-20261122-verify-known-node-loss.sh'
status=$?
set -e
printf '%s\n' "$status" > "$root/known-node-loss-be9cfe1b.host.exit"
date -u +%FT%TZ > "$root/known-node-loss-be9cfe1b.host.finished.txt"
exit "$status"
