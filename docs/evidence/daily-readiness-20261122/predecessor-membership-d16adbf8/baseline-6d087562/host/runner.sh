#!/usr/bin/env bash
set -euo pipefail
candidate="$1"
expected_bytes="$2"
expected_sha="$3"
[[ "$candidate" =~ ^[0-9a-f]{40}$ ]]
[[ "$expected_bytes" =~ ^[0-9]+$ ]]
[[ "$expected_sha" =~ ^[0-9a-f]{64}$ ]]
tag="${candidate:0:8}"
root=/tank/shared/projects/shenko/build/daily-readiness-20261122
bundle="/tank/shared/projects/shenko/build/anchored-native-$tag.bundle"
python3 - "$candidate" "$expected_bytes" "$expected_sha" <<'PY'
from pathlib import Path
import base64, hashlib, sys
tag = sys.argv[1][:8]
target = Path('/tank/shared/projects/shenko/build') / ('anchored-native-' + tag + '.bundle')
files = sorted(Path(str(target) + '-parts').glob('*.txt'))
assert files and [f.stem for f in files] == [f'{n:04d}' for n in range(len(files))]
data = base64.b64decode(''.join(f.read_text().strip() for f in files), validate=True)
assert len(data) == int(sys.argv[2]) and hashlib.sha256(data).hexdigest() == sys.argv[3]
if target.exists():
    assert target.read_bytes() == data
else:
    target.write_bytes(data)
print('Verified exact predecessor source bundle bytes and SHA256', flush=True)
PY
exec 8>/tank/shared/projects/shenko/build/cargo.lock
flock 8
test ! -e "$root/scoped-predecessor-$tag"
cd /tank/shared/projects/shenko/shenko
git bundle verify "$bundle"
git fetch "$bundle" refs/heads/fix/frozen-predecessor-membership-20261122:refs/heads/fix/frozen-predecessor-membership-20261122
cd /tank/shared/projects/shenko/worktrees/daily-readiness-20261122
test -z "$(git status --porcelain)"
git checkout --detach "$candidate"
test "$(git rev-parse HEAD)" = "$candidate"
task_image=$(ssh -n -T -o BatchMode=yes -o StrictHostKeyChecking=yes doescontainer 'docker inspect --format="{{.Image}}" shenko-build-r71')
test "$task_image" = sha256:28df77cc6beee4f88ee1f57922190dcb02414d78d284e8b0f91859e18728e565
printf 'Verified build container image: %s\n' "$task_image"
date -u +%FT%TZ > "$root/predecessor-$tag.host.started.txt"
set +e
ssh -n -T -o BatchMode=yes -o StrictHostKeyChecking=yes doescontainer "docker exec shenko-build-r71 bash /tank/shared/projects/shenko/build/daily-readiness-20261122-verify-predecessor-fixture.sh '$candidate'"
status=$?
set -e
printf '%s\n' "$status" > "$root/predecessor-$tag.host.exit"
date -u +%FT%TZ > "$root/predecessor-$tag.host.finished.txt"
exit "$status"
