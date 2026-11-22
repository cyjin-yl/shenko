#!/usr/bin/env bash
set -euo pipefail
root=/tank/shared/projects/shenko/build/daily-readiness-20261122
driver="$root/driver-d16adbf8"
batch="$root/final-d16adbf8"
binary="$root/shenko-release-d16adbf8"
exec 9>/tmp/shenko-cargo.lock
flock 9
test ! -e "$batch"
test ! -e "$root/final-d16adbf8.environment.ndjson"
test -x "$binary"
export PATH="/var/idw-target/daily-readiness-node-v24.22.1/bin:$PATH"
export PLAYWRIGHT_BROWSERS_PATH=/var/idw-target/daily-readiness-browsers-20261122
export SHENKO_LOCAL_MODEL_REQUEST_TIMEOUT_MS=600000 PYTHONDONTWRITEBYTECODE=1
python3 - <<'PY'
from pathlib import Path
import hashlib,json
root=Path('/tank/shared/projects/shenko/build/daily-readiness-20261122')
manifest=json.loads((root/'release-d16adbf8/manifest.json').read_text())
assert manifest['source_revision']=='d16adbf89c9f79dfce8ef90d77843412bcda85bf'
p=root/'shenko-release-d16adbf8';b=p.read_bytes()
assert len(b)==manifest['bytes'] and hashlib.sha256(b).hexdigest()==manifest['sha256']
for row in json.loads((root/'driver-d16adbf8/manifest.json').read_text())['files']:
 b=Path(row['path']).read_bytes();assert len(b)==row['bytes'] and hashlib.sha256(b).hexdigest()==row['sha256']
print('frozen binary and four driver source files verified',flush=True)
PY
date -u +%FT%TZ > "$root/final-d16adbf8.started.txt"
/var/idw-target/daily-readiness-python-20261122/bin/python -B "$driver/real_single_node_journey.py" --binary "$binary" --source-revision d16adbf89c9f79dfce8ef90d77843412bcda85bf --model-url http://10.10.10.11:8000 --model swift-qwen3.8-27b-nvfp4 --evidence-dir "$batch" --port 17860 --browser --repeat 3 --timeout 900 --startup-timeout 300 --timing > "$root/final-d16adbf8.driver.log" 2>&1 &
journey_runner_pid=$!
printf '%s\n' "$journey_runner_pid" > "$root/final-d16adbf8.driver.pid"
python3 -B "$driver/sample-environment.py" --parent-pid "$journey_runner_pid" --interval 5 --max-duration 10800 --output "$root/final-d16adbf8.environment.ndjson" > "$root/final-d16adbf8.sampler.log" 2>&1 &
sampler_pid=$!
set +e
wait "$journey_runner_pid"
journey_status=$?
wait "$sampler_pid"
sampler_status=$?
set -e
printf '%s\n' "$journey_status" > "$root/final-d16adbf8.driver.exit"
printf '%s\n' "$sampler_status" > "$root/final-d16adbf8.sampler.exit"
date -u +%FT%TZ > "$root/final-d16adbf8.finished.txt"
printf 'Real browser batch exit=%s; environment sampler exit=%s\n' "$journey_status" "$sampler_status"
if [ "$journey_status" -ne 0 ]; then exit "$journey_status"; fi
exit "$sampler_status"
